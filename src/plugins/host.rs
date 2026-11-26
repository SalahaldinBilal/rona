use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use extism::{CurrentPlugin, Function, PTR, UserData, Val};
use serde::Deserialize;
use serde_json::{Value, json};

use super::manifest::{HOOK_UI_RENDER, validate_tone};
use super::mpv_allow::{self, Readable};
use super::permissions::*;
use super::runtime::{Image, Pending, Shared, lock};
use super::{ipc, net, tree, view};

const MAX_FILE_READ: u64 = 4 * 1024 * 1024;
const MAX_TIMERS: usize = 16;
const MIN_TIMER_MS: u64 = 250;
const MAX_OVERLAY_ID: u32 = 16;
const MAX_OVERLAY_BYTES: usize = 64 * 1024;
const MAX_OSD_CHARS: usize = 200;
const OSD_INTERVAL: Duration = Duration::from_secs(1);
const VIEW_UPDATE_INTERVAL: Duration = Duration::from_millis(100);
const MAX_IMAGE_SIDE: u32 = 1024;
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IMAGES: usize = 200;
const MAX_SUBTITLE_BYTES: usize = 16 * 1024 * 1024;
const MAX_OBSERVED: usize = 32;
const MAX_PIPES: usize = 4;
const MAX_PIPE_BUFFER: usize = 4 * 1024 * 1024;
const MAX_PROCESS_ARGS: usize = 256;
const MAX_PROCESS_OUTPUT: usize = 32 * 1024 * 1024;
const DEFAULT_PROCESS_MS: u64 = 60_000;
const MAX_PROCESS_MS: u64 = 10 * 60_000;
const MAX_PROCESSES: usize = 4;
static NEXT_PROCESS: AtomicU32 = AtomicU32::new(1);
// Plugin-observed properties get ids from here, below are the client thread's own.
pub const OBSERVE_ID_BASE: u64 = 1000;

pub enum Reply {
    Ok(Vec<u8>),
    Denied(&'static str),
    Failed(String),
}

impl Reply {
    fn encode(self) -> Vec<u8> {
        let (status, payload) = match self {
            Reply::Ok(bytes) => (0, bytes),
            Reply::Denied(permission) => (1, permission.as_bytes().to_vec()),
            Reply::Failed(message) => (2, message.into_bytes()),
        };
        let mut output = Vec::with_capacity(payload.len() + 1);
        output.push(status);
        output.extend_from_slice(&payload);
        output
    }
}

type SharedRef = Arc<Shared>;
type HostFn = fn(&SharedRef, Vec<Vec<u8>>) -> Result<Vec<u8>, Reply>;

// Keep in step with the raw imports in plugin-sdk/rust/src/host.rs, plugin-sdk/ts/src/index.ts and
// HOST_FUNCTIONS in plugin-sdk/ts/src/build.mts.
pub const FUNCTIONS: &[(&str, usize, HostFn)] = &[
    ("log", 1, log),
    ("kv_get", 1, kv_get),
    ("kv_set", 2, kv_set),
    ("kv_delete", 1, kv_delete),
    ("kv_keys", 1, kv_keys),
    ("setting_get", 1, setting_get),
    ("player_state", 0, player_state),
    ("player_control", 1, player_control),
    ("media_info", 0, media_info),
    ("tracks_get", 0, tracks_get),
    ("tracks_select", 1, tracks_select),
    ("subtitle_add", 2, subtitle_add),
    ("file_size", 0, file_size),
    ("file_read", 1, file_read),
    ("open_media", 1, open_media),
    ("playlist_get", 0, playlist_get),
    ("playlist_edit", 1, playlist_edit),
    ("mpv_get", 1, mpv_get),
    ("mpv_observe", 1, mpv_observe),
    ("mpv_set", 2, mpv_set),
    ("mpv_command", 1, mpv_command),
    ("osd_show", 1, osd_show),
    ("overlay_set", 2, overlay_set),
    ("overlay_clear", 1, overlay_clear),
    ("ui_update", 2, ui_update),
    ("ui_patch", 2, ui_patch),
    ("ui_image", 2, ui_image),
    ("ui_open", 1, ui_open),
    ("controls_set", 2, controls_set),
    ("menu_set", 2, menu_set),
    ("clipboard_read", 0, clipboard_read),
    ("clipboard_write", 1, clipboard_write),
    ("http_request", 2, http_request),
    ("http_response_body", 0, http_response_body),
    ("timer_set", 1, timer_set),
    ("timer_clear", 1, timer_clear),
    ("position_events", 1, position_events),
    ("ipc_send", 2, ipc_send),
    ("ipc_close", 1, ipc_close),
    ("pipe_connect", 1, pipe_connect),
    ("pipe_write", 2, pipe_write),
    ("pipe_read", 1, pipe_read),
    ("pipe_close", 1, pipe_close),
    ("process_run", 2, process_run),
    ("process_stdout", 0, process_stdout),
    ("process_start", 1, process_start),
    ("process_write", 2, process_write),
    ("process_read", 2, process_read),
    ("process_end_input", 1, process_end_input),
    ("process_close", 1, process_close),
];

pub fn functions(shared: &SharedRef) -> Vec<Function> {
    FUNCTIONS
        .iter()
        .map(|&(name, params, body)| {
            Function::new(
                name,
                vec![PTR; params],
                vec![PTR],
                UserData::new(shared.clone()),
                move |plugin: &mut CurrentPlugin, inputs: &[Val], outputs: &mut [Val], data: UserData<SharedRef>| {
                    let shared = data.get()?.lock().map_err(|_| extism::Error::msg("plugin host state is unavailable"))?.clone();
                    let mut args = Vec::with_capacity(inputs.len());
                    // Offset 0 stands for an empty block in both directions, since Extism has no empty allocation.
                    for input in inputs {
                        args.push(match input {
                            Val::I64(0) => Vec::new(),
                            _ => plugin.memory_get_val::<Vec<u8>>(input)?,
                        });
                    }
                    let reply = match body(&shared, args) {
                        Ok(bytes) => Reply::Ok(bytes),
                        Err(reply) => reply,
                    };
                    plugin.memory_set_val(&mut outputs[0], reply.encode())
                },
            )
        })
        .collect()
}

fn require(shared: &Shared, permission: &'static str) -> Result<(), Reply> {
    if shared.granted(permission) { Ok(()) } else { Err(Reply::Denied(permission)) }
}

fn fail(message: impl Into<String>) -> Reply {
    Reply::Failed(message.into())
}

fn text(bytes: Vec<u8>) -> Result<String, Reply> {
    String::from_utf8(bytes).map_err(|_| fail("Expected UTF-8 text"))
}

fn parse<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, Reply> {
    serde_json::from_slice(bytes).map_err(|err| fail(format!("Invalid JSON argument: {err}")))
}

fn to_json(value: &impl serde::Serialize) -> Result<Vec<u8>, Reply> {
    serde_json::to_vec(value).map_err(|err| fail(err.to_string()))
}

fn none() -> Result<Vec<u8>, Reply> {
    Ok(Vec::new())
}

fn client(shared: &Shared) -> Result<crate::mpv::Mpv, Reply> {
    shared.client().ok_or_else(|| fail("The plugin is stopping"))
}

fn mpv_result(result: Result<(), crate::mpv::Error>) -> Result<Vec<u8>, Reply> {
    result.map_err(|err| fail(err.to_string()))?;
    none()
}

#[derive(Deserialize)]
struct LogArgs {
    #[serde(default)]
    level: String,
    text: String,
}

fn log(shared: &SharedRef, args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let args: LogArgs = parse(&args[0])?;
    shared.log.push(&args.level, &args.text);
    none()
}

// A leading 1 means the value follows, 0 means there is no such key.
fn kv_get(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let key = text(args.remove(0))?;
    Ok(match shared.store.kv_get(&shared.id, &key).map_err(fail)? {
        Some(value) => [&[1u8][..], &value].concat(),
        None => vec![0],
    })
}

fn kv_set(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let value = args.remove(1);
    let key = text(args.remove(0))?;
    shared.store.kv_set(&shared.id, &key, &value).map_err(fail)?;
    none()
}

fn kv_delete(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let key = text(args.remove(0))?;
    shared.store.kv_delete(&shared.id, &key).map_err(fail)?;
    none()
}

fn kv_keys(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let prefix = text(args.remove(0))?;
    to_json(&shared.store.kv_keys(&shared.id, &prefix).map_err(fail)?)
}

fn setting_get(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let id = text(args.remove(0))?;
    to_json(&super::setting_value(&shared.store, &shared.id, &shared.manifest, &id))
}

fn player_state(shared: &SharedRef, _args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, PLAYBACK_READ)?;
    let player = lock(&shared.player).clone();
    let state = if player.idle {
        "idle"
    } else if player.opening {
        "opening"
    } else if player.paused {
        "paused"
    } else {
        "playing"
    };
    let mut reply = json!({
        "state": state,
        "position": player.position,
        "duration": player.duration,
        "speed": player.speed,
        "volume": player.volume / 100.0,
        "muted": player.muted,
    });
    if shared.granted(MEDIA_METADATA) && !player.idle {
        reply["title"] = json!(player.title);
    }
    if shared.granted(MEDIA_PATH) && !player.idle {
        reply["path"] = json!(crate::http_cache::unwrap_url(&player.path));
    }
    to_json(&reply)
}

fn player_control(shared: &SharedRef, args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, PLAYBACK_CONTROL)?;
    let args: Value = parse(&args[0])?;
    let number = |key: &str| args.get(key).and_then(Value::as_f64).filter(|value| value.is_finite());
    let mpv = &client(shared)?;
    match args.get("action").and_then(Value::as_str).unwrap_or_default() {
        "play" => mpv_result(mpv.command(&["set", "pause", "no"])),
        "pause" => mpv_result(mpv.command(&["set", "pause", "yes"])),
        "toggle" => mpv_result(mpv.command(&["cycle", "pause"])),
        "seek" => match (number("to"), number("by")) {
            (Some(to), _) => mpv_result(mpv.command(&["seek", &to.max(0.0).to_string(), "absolute+exact"])),
            (None, Some(by)) => mpv_result(mpv.command(&["seek", &by.to_string(), "relative+exact"])),
            _ => Err(fail("seek needs to or by")),
        },
        "speed" => {
            let speed = number("value").filter(|speed| (0.01..=100.0).contains(speed)).ok_or_else(|| fail("Speeds go from 0.01 to 100"))?;
            mpv_result(mpv.command(&["set", "speed", &speed.to_string()]))
        }
        "volume" | "mute" | "next" | "previous" | "stop" => {
            shared.bridge.player_control(args);
            none()
        }
        other => Err(fail(format!("Unknown player action \"{other}\""))),
    }
}

fn json_property(shared: &Shared, name: &str) -> Value {
    shared.client().and_then(|mpv| mpv.get_string(name)).and_then(|text| serde_json::from_str(&text).ok()).unwrap_or(Value::Null)
}

fn media_info(shared: &SharedRef, _args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, MEDIA_METADATA)?;
    let mpv = &client(shared)?;
    let tags: serde_json::Map<String, Value> = json_property(shared, "metadata").as_object().cloned().unwrap_or_default();
    let tag = |names: &[&str]| {
        tags.iter().find(|(key, _)| names.iter().any(|name| key.eq_ignore_ascii_case(name))).and_then(|(_, value)| value.as_str().map(str::to_owned))
    };
    let chapters: Vec<Value> = json_property(shared, "chapter-list")
        .as_array()
        .map(|list| {
            list.iter()
                .map(|chapter| json!({ "time": chapter.get("time").cloned().unwrap_or(json!(0)), "title": chapter.get("title").cloned().unwrap_or(json!("")) }))
                .collect()
        })
        .unwrap_or_default();
    let number = |name: &str| mpv.get_string(name).and_then(|value| value.parse::<u32>().ok());
    let has_video = mpv.get_string("current-tracks/video/id").is_some();
    let cover_only = mpv.get_string("current-tracks/video/albumart").as_deref() == Some("yes");
    to_json(&json!({
        "title": mpv.get_string("media-title").unwrap_or_default(),
        "artist": tag(&["artist", "album_artist"]),
        "album": tag(&["album"]),
        "videoCodec": mpv.get_string("video-codec"),
        "audioCodec": mpv.get_string("audio-codec-name"),
        "width": number("width"),
        "height": number("height"),
        "audioOnly": !has_video || cover_only,
        "chapters": chapters,
        "tags": tags,
    }))
}

fn tracks_get(shared: &SharedRef, _args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, MEDIA_TRACKS)?;
    let tracks: Vec<Value> = json_property(shared, "track-list")
        .as_array()
        .map(|list| {
            list.iter()
                .map(|track| {
                    json!({
                        "id": track.get("id").cloned().unwrap_or(json!(0)),
                        "kind": track.get("type").cloned().unwrap_or(json!("")),
                        "title": track.get("title"),
                        "language": track.get("lang"),
                        "codec": track.get("codec"),
                        "selected": track.get("selected").and_then(Value::as_bool).unwrap_or(false),
                        "external": track.get("external").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    to_json(&tracks)
}

fn tracks_select(shared: &SharedRef, args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, MEDIA_TRACKS)?;
    let args: Value = parse(&args[0])?;
    let property = match args.get("kind").and_then(Value::as_str) {
        Some("audio") => "aid",
        Some("sub") => "sid",
        _ => return Err(fail("kind is audio or sub")),
    };
    let id = match args.get("id").and_then(Value::as_i64) {
        Some(id) => id.to_string(),
        None => "no".into(),
    };
    mpv_result(client(shared)?.command(&["set", property, &id]))
}

#[derive(Deserialize)]
struct SubtitleArgs {
    name: String,
    #[serde(default)]
    language: Option<String>,
    extension: String,
}

fn subtitle_add(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, MEDIA_SUBTITLES)?;
    let bytes = args.remove(1);
    let args: SubtitleArgs = parse(&args[0])?;
    if bytes.is_empty() || bytes.len() > MAX_SUBTITLE_BYTES {
        return Err(fail("A subtitle must be 1 byte to 16 MB"));
    }
    let extension = args.extension.to_ascii_lowercase();
    if !matches!(extension.as_str(), "srt" | "ass" | "ssa" | "vtt" | "sub" | "sup" | "idx" | "smi" | "lrc") {
        return Err(fail(format!("Subtitles of type {extension} aren't supported")));
    }
    let folder = std::env::temp_dir().join("rona-plugin-subtitles");
    std::fs::create_dir_all(&folder).map_err(|err| fail(err.to_string()))?;
    let file = folder.join(format!("{}-{}.{extension}", shared.id, super::random_id()));
    std::fs::write(&file, &bytes).map_err(|err| fail(format!("Couldn't write the subtitle: {err}")))?;
    let path = file.to_string_lossy().into_owned();
    let title: String = args.name.chars().take(100).collect();
    let language = args.language.unwrap_or_default();
    mpv_result(client(shared)?.command(&["sub-add", &path, "select", &title, &language]))
}

fn local_path(shared: &Shared) -> Result<std::path::PathBuf, Reply> {
    let path = lock(&shared.player).path.clone();
    if path.is_empty() || path.contains("://") {
        return Err(fail("Only local files can be read"));
    }
    Ok(std::path::PathBuf::from(path))
}

fn file_size(shared: &SharedRef, _args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, MEDIA_FILE)?;
    let size = std::fs::metadata(local_path(shared)?).map_err(|err| fail(err.to_string()))?.len();
    to_json(&size)
}

#[derive(Deserialize)]
struct ReadArgs {
    offset: u64,
    length: u64,
}

fn file_read(shared: &SharedRef, args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, MEDIA_FILE)?;
    let args: ReadArgs = parse(&args[0])?;
    let mut file = std::fs::File::open(local_path(shared)?).map_err(|err| fail(err.to_string()))?;
    file.seek(SeekFrom::Start(args.offset)).map_err(|err| fail(err.to_string()))?;
    let mut bytes = Vec::new();
    file.take(args.length.min(MAX_FILE_READ)).read_to_end(&mut bytes).map_err(|err| fail(err.to_string()))?;
    Ok(bytes)
}

#[derive(Deserialize)]
struct OpenArgs {
    sources: Vec<String>,
    #[serde(default)]
    append: bool,
}

fn open_media(shared: &SharedRef, args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, OPEN_MEDIA)?;
    let args: OpenArgs = parse(&args[0])?;
    if args.sources.is_empty() || args.sources.len() > 1000 {
        return Err(fail("Open 1 to 1000 sources at a time"));
    }
    shared.log.push("info", &format!("opens {} sources", args.sources.len()));
    shared.bridge.open_media(args.sources, args.append);
    none()
}

fn playlist_get(shared: &SharedRef, _args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, PLAYLIST_READ)?;
    let paths = shared.granted(MEDIA_PATH);
    let entries: Vec<Value> = super::playlist_snapshot()
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut value = json!({ "index": index, "title": entry.title, "current": entry.current });
            if paths {
                value["path"] = json!(entry.path);
            }
            value
        })
        .collect();
    to_json(&entries)
}

fn playlist_edit(shared: &SharedRef, args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, PLAYLIST_EDIT)?;
    let op: Value = parse(&args[0])?;
    let count = super::playlist_snapshot().len() as u64;
    let index = op.get("index").and_then(Value::as_u64).ok_or_else(|| fail("index is missing"))?;
    if index >= count {
        return Err(fail(format!("There is no entry {index}")));
    }
    match op.get("op").and_then(Value::as_str) {
        Some("remove" | "play") => {}
        Some("move") if op.get("before").and_then(Value::as_u64).is_some_and(|before| before <= count) => {}
        Some("move") => return Err(fail("before is missing or past the end")),
        _ => return Err(fail("op is remove, move or play")),
    }
    shared.bridge.playlist_edit(op);
    none()
}

fn mpv_get(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, MPV_READ)?;
    let name = text(args.remove(0))?;
    match mpv_allow::readable(&name) {
        Readable::No => return Err(fail(format!("The property {name} can't be read by plugins"))),
        Readable::WithPaths => require(shared, MEDIA_PATH)?,
        Readable::Yes => {}
    }
    let value = client(shared)?.get_string(&name).map(|text| serde_json::from_str(&text).unwrap_or(Value::String(text)));
    to_json(&value.unwrap_or(Value::Null))
}

fn mpv_observe(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, MPV_READ)?;
    let name = text(args.remove(0))?;
    match mpv_allow::readable(&name) {
        Readable::No => return Err(fail(format!("The property {name} can't be read by plugins"))),
        Readable::WithPaths => require(shared, MEDIA_PATH)?,
        Readable::Yes => {}
    }
    let mut observed = lock(&shared.observed);
    if observed.contains_key(&name) {
        return none();
    }
    if observed.len() >= MAX_OBSERVED {
        return Err(fail(format!("A plugin can observe at most {MAX_OBSERVED} properties")));
    }
    let id = OBSERVE_ID_BASE + observed.len() as u64;
    client(shared)?.observe_string(id, &name).map_err(|err| fail(err.to_string()))?;
    observed.insert(name, id);
    none()
}

fn mpv_set(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, MPV_WRITE)?;
    let value = text(args.remove(1))?;
    let name = text(args.remove(0))?;
    if !mpv_allow::writable(&name) {
        return Err(fail(format!("The property {name} can't be changed by plugins")));
    }
    mpv_result(client(shared)?.command(&["set", &name, &value]))
}

fn mpv_command(shared: &SharedRef, args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, MPV_COMMAND)?;
    let args: Vec<String> = parse(&args[0])?;
    if let Some(reason) = mpv_allow::refuse_command(&args) {
        return Err(fail(reason));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    mpv_result(client(shared)?.command(&args))
}

fn osd_show(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, UI_OSD)?;
    let text = text(args.remove(0))?;
    show_osd(shared, &text);
    none()
}

pub fn show_osd(shared: &Shared, text: &str) {
    let mut limits = lock(&shared.limits);
    if limits.last_osd.is_some_and(|at| at.elapsed() < OSD_INTERVAL) {
        shared.log.push("debug", &format!("OSD message dropped (too many): {text}"));
        return;
    }
    limits.last_osd = Some(Instant::now());
    drop(limits);
    shared.bridge.osd(text.chars().take(MAX_OSD_CHARS).collect());
}

#[derive(Deserialize)]
struct OverlayArgs {
    id: u32,
}

// Overlays belong to the plugin's own mpv client, so they go away with it.
fn overlay_set(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, UI_OVERLAY)?;
    let ass = text(args.remove(1))?;
    let args: OverlayArgs = parse(&args[0])?;
    if !(1..=MAX_OVERLAY_ID).contains(&args.id) {
        return Err(fail(format!("Overlay ids are 1 to {MAX_OVERLAY_ID}")));
    }
    if ass.len() > MAX_OVERLAY_BYTES {
        return Err(fail("An overlay can be at most 64 KB"));
    }
    lock(&shared.overlays).insert(args.id);
    let data = format!("data={ass}");
    mpv_result(client(shared)?.command(&["osd-overlay", &format!("id={}", args.id), "format=ass-events", &data, "res_x=1280", "res_y=720"]))
}

fn overlay_clear(shared: &SharedRef, args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, UI_OVERLAY)?;
    let args: OverlayArgs = parse(&args[0])?;
    lock(&shared.overlays).remove(&args.id);
    clear_overlay(shared, args.id);
    none()
}

pub fn clear_overlay(shared: &Shared, id: u32) {
    if let Some(mpv) = shared.client() {
        let _ = mpv.command(&["osd-overlay", &format!("id={id}"), "format=none", "data="]);
    }
}

fn view_allowed(shared: &Shared, view: &str) -> Result<(), Reply> {
    let manifest = &shared.manifest;
    if !manifest.has_hook(HOOK_UI_RENDER) {
        return Err(fail("The plugin has no views"));
    }
    let known = match view {
        "settings" => true,
        "panel" => manifest.ui.panel.is_some(),
        _ => match view.split_once(':') {
            Some(("popover", id)) => manifest.control(id).is_some_and(|control| control.popover),
            Some(("dialog", id)) => manifest.dialog(id).is_some(),
            _ => false,
        },
    };
    if !known {
        return Err(fail(format!("The plugin has no view {view}")));
    }
    if view == "panel" {
        require(shared, UI_PANEL)?;
    }
    if view.starts_with("popover:") {
        require(shared, UI_CONTROLS)?;
    }
    Ok(())
}

fn send_now(shared: &Shared, update: Pending) {
    match update {
        Pending::Control(id, state) => shared.bridge.control_set(&shared.id, &id, state),
        Pending::Tree(view, tree) => shared.bridge.view_tree(&shared.id, &view, tree),
        Pending::Patch(view, patches) => shared.bridge.view_patch(&shared.id, &view, patches),
    }
}

// At most 10 updates a second per view or control. One that comes sooner waits for the rest of the interval,
// and newer ones replace it (trees) or join it (control states, patches), so the last one always arrives.
fn deliver(shared: &SharedRef, key: String, update: Pending) {
    let mut limits = lock(&shared.limits);
    let waited = limits.last_sent.get(&key).map(Instant::elapsed);
    if waited.is_none_or(|waited| waited >= VIEW_UPDATE_INTERVAL) && !limits.pending.contains_key(&key) {
        limits.last_sent.insert(key, Instant::now());
        drop(limits);
        send_now(shared, update);
        return;
    }
    let scheduled = limits.pending.contains_key(&key);
    let merged = match (limits.pending.remove(&key), update) {
        (Some(Pending::Control(id, mut old)), Pending::Control(_, new)) => {
            if let (Some(old), Some(new)) = (old.as_object_mut(), new.as_object()) {
                old.extend(new.clone());
            }
            Pending::Control(id, old)
        }
        (Some(Pending::Patch(view, mut old)), Pending::Patch(_, new)) => {
            old.extend(new);
            Pending::Patch(view, old)
        }
        (_, update) => update,
    };
    limits.pending.insert(key.clone(), merged);
    drop(limits);
    if scheduled {
        return;
    }
    let wait = VIEW_UPDATE_INTERVAL.saturating_sub(waited.unwrap_or_default());
    let weak = Arc::downgrade(shared);
    std::thread::spawn(move || {
        std::thread::sleep(wait);
        let Some(shared) = weak.upgrade() else { return };
        let update = {
            let mut limits = lock(&shared.limits);
            limits.last_sent.insert(key.clone(), Instant::now());
            limits.pending.remove(&key)
        };
        if let Some(update) = update
            && !shared.stopped.load(Ordering::SeqCst)
        {
            send_now(&shared, update);
        }
    });
}

fn ui_update(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let tree: Vec<Value> = parse(&args.remove(1))?;
    let view = text(args.remove(0))?;
    view_allowed(shared, &view)?;
    tree::validate_tree(&tree, tree::Mode::View).map_err(|err| fail(format!("The tree is not valid: {err}")))?;
    deliver(shared, format!("view:{view}"), Pending::Tree(view, tree));
    none()
}

fn ui_patch(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let patches: Vec<Value> = parse(&args.remove(1))?;
    let view = text(args.remove(0))?;
    view_allowed(shared, &view)?;
    if patches.iter().any(|patch| patch.get("id").and_then(Value::as_str).is_none()) {
        return Err(fail("Every patch needs the id of the node it changes"));
    }
    deliver(shared, format!("patch:{view}"), Pending::Patch(view, patches));
    none()
}

#[derive(Deserialize)]
struct ImageArgs {
    id: String,
}

fn ui_image(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let bytes = args.remove(1);
    let args: ImageArgs = parse(&args[0])?;
    if !super::manifest::is_valid_item_id(&args.id) {
        return Err(fail("Image ids are 1 to 64 letters, digits, dashes and underscores"));
    }
    let decoded = image::load_from_memory(&bytes).map_err(|_| fail("Images must be PNG, JPEG or WebP"))?;
    let decoded = if decoded.width().max(decoded.height()) > MAX_IMAGE_SIDE {
        decoded.thumbnail(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE)
    } else {
        decoded
    };
    let rgba = decoded.into_rgba8();
    let image = Image { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() };
    let size = image.rgba.len() as u64;
    let mut images = lock(&shared.images);
    let replaced = images.get(&args.id).map_or(0, |old| old.rgba.len() as u64);
    let total = shared.image_bytes.load(Ordering::SeqCst) - replaced + size;
    if total > MAX_IMAGE_BYTES || (replaced == 0 && images.len() >= MAX_IMAGES) {
        return Err(fail("Plugin images can't take more than 16 MB or 200 images"));
    }
    images.insert(args.id, Arc::new(image));
    shared.image_bytes.store(total, Ordering::SeqCst);
    drop(images);
    shared.bridge.images_changed(&shared.id);
    none()
}

fn ui_open(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let dialog = text(args.remove(0))?;
    if shared.manifest.dialog(&dialog).is_none() {
        return Err(fail(format!("The plugin has no dialog {dialog}")));
    }
    if !lock(&shared.call).user_action {
        return Err(fail("Dialogs only open in answer to something the user did"));
    }
    shared.bridge.open_dialog(&shared.id, &dialog);
    none()
}

fn controls_set(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, UI_CONTROLS)?;
    let state: Value = parse(&args.remove(1))?;
    let id = text(args.remove(0))?;
    if shared.manifest.control(&id).is_none() {
        return Err(fail(format!("The plugin has no control {id}")));
    }
    let field = |key: &str| state.get(key).and_then(Value::as_str);
    for key in ["fill", "stroke"] {
        if let Some(path) = field(key) {
            super::manifest::validate_path(path).map_err(fail)?;
        }
    }
    if let Some(tone) = field("tone") {
        validate_tone(tone).map_err(fail)?;
    }
    if field("text").is_some_and(|text| text.chars().count() > super::manifest::MAX_CONTROL_TEXT) {
        return Err(fail("Control text can be at most 8 characters"));
    }
    deliver(shared, format!("control:{id}"), Pending::Control(id, state));
    none()
}

fn menu_set(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, UI_MENU)?;
    let state: Value = parse(&args.remove(1))?;
    let id = text(args.remove(0))?;
    let ui = &shared.manifest.ui;
    if !ui.menu.iter().chain(&ui.row_menu).any(|entry| entry.id == id) {
        return Err(fail(format!("The plugin has no menu entry {id}")));
    }
    if state.get("label").and_then(Value::as_str).is_some_and(|label| label.trim().is_empty() || label.chars().count() > super::manifest::MAX_LABEL) {
        return Err(fail("A menu label must be 1 to 40 characters"));
    }
    shared.bridge.menu_set(&shared.id, &id, state);
    none()
}

fn clipboard_read(shared: &SharedRef, _args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, CLIPBOARD_READ)?;
    let mut clipboard = arboard::Clipboard::new().map_err(|err| fail(err.to_string()))?;
    Ok(clipboard.get_text().unwrap_or_default().into_bytes())
}

fn clipboard_write(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, CLIPBOARD_WRITE)?;
    let text = text(args.remove(0))?;
    let mut clipboard = arboard::Clipboard::new().map_err(|err| fail(err.to_string()))?;
    clipboard.set_text(text).map_err(|err| fail(err.to_string()))?;
    none()
}

// Network failures and refused hosts come back as failures the plugin can handle.
fn http_request(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let hosts = shared.granted_hosts();
    if hosts.is_empty() {
        return Err(Reply::Denied(NETWORK));
    }
    let bytes = args.remove(1);
    let request: net::HttpRequest = parse(&args[0])?;
    let watch = lock(&shared.watch).clone();
    if let Some(watch) = &watch {
        watch.set_paused(true);
    }
    let outcome = net::execute(&shared.agent, &hosts, request, bytes);
    if let Some(watch) = &watch {
        watch.set_paused(false);
    }
    let (reply, body) = outcome.map_err(fail)?;
    lock(&shared.call).response_body = body;
    to_json(&reply)
}

fn http_response_body(shared: &SharedRef, _args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    Ok(std::mem::take(&mut lock(&shared.call).response_body))
}

#[derive(Deserialize)]
struct TimerArgs {
    id: String,
    ms: u64,
    #[serde(default)]
    repeat: bool,
}

fn timer_set(shared: &SharedRef, args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let args: TimerArgs = parse(&args[0])?;
    if !super::manifest::is_valid_item_id(&args.id) {
        return Err(fail("Timer ids are 1 to 64 letters, digits, dashes and underscores"));
    }
    let mut timers = lock(&shared.timers);
    if let Some(old) = timers.remove(&args.id) {
        old.store(true, Ordering::SeqCst);
    }
    if timers.len() >= MAX_TIMERS {
        return Err(fail(format!("A plugin can have at most {MAX_TIMERS} timers")));
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    timers.insert(args.id.clone(), cancelled.clone());
    drop(timers);

    let weak = Arc::downgrade(shared);
    let interval = Duration::from_millis(args.ms.max(MIN_TIMER_MS));
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(interval);
            let Some(shared) = weak.upgrade() else { return };
            if cancelled.load(Ordering::SeqCst) || shared.stopped.load(Ordering::SeqCst) {
                return;
            }
            shared.post_event(json!({ "type": "timer", "id": args.id }));
            if !args.repeat {
                lock(&shared.timers).remove(&args.id);
                return;
            }
        }
    });
    none()
}

fn timer_clear(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let id = text(args.remove(0))?;
    if let Some(cancelled) = lock(&shared.timers).remove(&id) {
        cancelled.store(true, Ordering::SeqCst);
    }
    none()
}

fn position_events(shared: &SharedRef, args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, PLAYBACK_READ)?;
    let args: Value = parse(&args[0])?;
    let interval = args.get("intervalMs").and_then(Value::as_u64).unwrap_or(0);
    let interval = if interval == 0 { 0 } else { interval.max(MIN_TIMER_MS) };
    shared.position_interval_ms.store(interval, Ordering::SeqCst);
    none()
}

fn connection_id(bytes: Vec<u8>) -> Result<u32, Reply> {
    text(bytes)?.trim().parse().map_err(|_| fail("Not a valid id"))
}

fn ipc_send(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, IPC_SERVE)?;
    let message = args.remove(1);
    let connection = connection_id(args.remove(0))?;
    serde_json::from_slice::<Value>(&message).map_err(|_| fail("Messages must be JSON"))?;
    ipc::send(&shared.id, connection, message).map_err(fail)?;
    none()
}

fn ipc_close(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    require(shared, IPC_SERVE)?;
    ipc::close(&shared.id, connection_id(args.remove(0))?);
    none()
}

fn pipe_connect(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let name = text(args.remove(0))?;
    if !shared.granted(&super::permissions::pipe_grant(&name)) {
        return Err(Reply::Denied(IPC_CONNECT));
    }
    if lock(&shared.pipes).len() >= MAX_PIPES {
        return Err(fail(format!("A plugin can have at most {MAX_PIPES} pipes open")));
    }
    let watch = lock(&shared.watch).clone();
    if let Some(watch) = &watch {
        watch.set_paused(true);
    }
    let opened = ipc::connect_pipe(shared, &name, MAX_PIPE_BUFFER);
    if let Some(watch) = &watch {
        watch.set_paused(false);
    }
    to_json(&opened.map_err(fail)?)
}

fn pipe_write(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let bytes = args.remove(1);
    let connection = connection_id(args.remove(0))?;
    let pipes = lock(&shared.pipes);
    let pipe = pipes.get(&connection).ok_or_else(|| fail("That pipe is not open"))?;
    if !shared.granted(&super::permissions::pipe_grant(&pipe.name)) {
        return Err(Reply::Denied(IPC_CONNECT));
    }
    pipe.writer.send(bytes).map_err(|_| fail("The pipe was closed"))?;
    none()
}

fn pipe_read(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let connection = connection_id(args.remove(0))?;
    let pipes = lock(&shared.pipes);
    let pipe = pipes.get(&connection).ok_or_else(|| fail("That pipe is not open"))?;
    pipe.told.store(false, Ordering::SeqCst);
    Ok(std::mem::take(&mut *lock(&pipe.received)))
}

fn pipe_close(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let connection = connection_id(args.remove(0))?;
    lock(&shared.pipes).remove(&connection);
    none()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunArgs {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn check_program(shared: &Shared, run: &RunArgs) -> Result<(), Reply> {
    if !shared.granted(&super::permissions::program_grant(&run.program)) {
        return Err(Reply::Denied(SYSTEM_RUN));
    }
    super::manifest::validate_program(&run.program).map_err(fail)?;
    if run.args.len() > MAX_PROCESS_ARGS || run.args.iter().any(|arg| arg.contains('\0') || arg.len() > 32 * 1024) {
        return Err(fail("Too many or too long arguments"));
    }
    Ok(())
}

fn process_run(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let stdin = args.remove(1);
    let run: RunArgs = parse(&args[0])?;
    check_program(shared, &run)?;
    if stdin.len() > MAX_PROCESS_OUTPUT {
        return Err(fail("The input can be at most 32 MB"));
    }
    let timeout = Duration::from_millis(run.timeout_ms.unwrap_or(DEFAULT_PROCESS_MS).clamp(1, MAX_PROCESS_MS));
    shared.log.push("info", &format!("runs {} with {} arguments", run.program, run.args.len()));

    let watch = lock(&shared.watch).clone();
    if let Some(watch) = &watch {
        watch.set_paused(true);
    }
    let outcome = super::process::run(&run.program, &run.args, stdin, timeout, MAX_PROCESS_OUTPUT);
    if let Some(watch) = &watch {
        watch.set_paused(false);
    }
    let output = outcome.map_err(|err| fail(format!("Couldn't run {}: {err}", run.program)))?;
    lock(&shared.call).process_stdout = output.stdout;
    to_json(&json!({
        "status": output.status,
        "timedOut": output.timed_out,
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}

fn process_stdout(shared: &SharedRef, _args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    Ok(std::mem::take(&mut lock(&shared.call).process_stdout))
}

fn process_start(shared: &SharedRef, args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let run: RunArgs = parse(&args[0])?;
    check_program(shared, &run)?;
    if !shared.manifest.has_hook(super::manifest::HOOK_ON_EVENT) {
        return Err(fail("Starting a program needs the on_event hook, which hears its output"));
    }
    let mut processes = lock(&shared.processes);
    if processes.len() >= MAX_PROCESSES {
        // Programs that already ended make room, oldest first.
        let ended = processes.iter().filter(|(_, running)| running.status().is_some()).map(|(id, _)| *id).min();
        processes.remove(&ended.ok_or_else(|| fail(format!("A plugin can have at most {MAX_PROCESSES} programs running")))?);
    }
    let id = NEXT_PROCESS.fetch_add(1, Ordering::Relaxed);
    let weak = Arc::downgrade(shared);
    let running = super::process::Running::start(&run.program, &run.args, MAX_PIPE_BUFFER, move |event| {
        let Some(shared) = weak.upgrade() else { return false };
        match event {
            super::process::Event::Output => shared.post_event(json!({ "type": "process-output", "process": id })),
            super::process::Event::Exited(status) => {
                lock(&shared.processes).contains_key(&id)
                    && shared.post_event(json!({ "type": "process-exited", "process": id, "status": status }))
            }
            super::process::Event::Overflowed => {
                shared.log.push("warn", "A program printed more than the plugin read, so it was stopped");
                true
            }
        }
    })
    .map_err(|err| fail(format!("Couldn't start {}: {err}", run.program)))?;
    shared.log.push("info", &format!("started {} with {} arguments", run.program, run.args.len()));
    processes.insert(id, running);
    to_json(&id)
}

fn with_process<T>(shared: &Shared, id: Vec<u8>, action: impl FnOnce(&super::process::Running) -> Result<T, Reply>) -> Result<T, Reply> {
    let id = connection_id(id)?;
    let processes = lock(&shared.processes);
    let running = processes.get(&id).ok_or_else(|| fail("No such program is running"))?;
    if !shared.granted(&super::permissions::program_grant(&running.program)) {
        return Err(Reply::Denied(SYSTEM_RUN));
    }
    action(running)
}

fn process_write(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let bytes = args.remove(1);
    if bytes.len() > MAX_PIPE_BUFFER {
        return Err(fail("Write at most 4 MB at a time"));
    }
    with_process(shared, args.remove(0), |running| running.write(bytes).map_err(fail))?;
    none()
}

fn process_read(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let stream = match text(args.remove(1))?.as_str() {
        "stdout" => super::process::Stream::Stdout,
        "stderr" => super::process::Stream::Stderr,
        _ => return Err(fail("The stream is stdout or stderr")),
    };
    with_process(shared, args.remove(0), |running| Ok(running.take(stream)))
}

fn process_end_input(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    with_process(shared, args.remove(0), |running| {
        running.end_input();
        Ok(())
    })?;
    none()
}

fn process_close(shared: &SharedRef, mut args: Vec<Vec<u8>>) -> Result<Vec<u8>, Reply> {
    let id = connection_id(args.remove(0))?;
    lock(&shared.processes).remove(&id);
    none()
}

/// Turns an image id of the plugin into what a view draws.
pub fn image(shared: &Shared, id: &str) -> Option<Arc<Image>> {
    lock(&shared.images).get(id).cloned()
}

pub fn render_rows(shared: &Shared, tree: &[Value]) -> Vec<view::Row> {
    view::rows(tree, &|id| image(shared, id))
}
