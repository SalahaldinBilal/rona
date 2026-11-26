//! What a plugin can ask Rona to do.
//!
//! Every part that needs a permission comes from a function returning `Option`, which is `None`
//! while the user has that permission turned off. The user can turn permissions off at any time,
//! so check each time instead of keeping the handle. A call that is denied anyway (the user turned
//! it off during the hook) returns [`HostError::Denied`] and the hook goes on.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;

use extism_pdk::Memory;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::permissions::*;

mod raw {
    #[link(wasm_import_module = "extism:host/user")]
    unsafe extern "C" {
        pub fn log(args: u64) -> u64;
        pub fn kv_get(key: u64) -> u64;
        pub fn kv_set(key: u64, value: u64) -> u64;
        pub fn kv_delete(key: u64) -> u64;
        pub fn kv_keys(prefix: u64) -> u64;
        pub fn setting_get(id: u64) -> u64;
        pub fn player_state() -> u64;
        pub fn player_control(args: u64) -> u64;
        pub fn media_info() -> u64;
        pub fn tracks_get() -> u64;
        pub fn tracks_select(args: u64) -> u64;
        pub fn subtitle_add(args: u64, bytes: u64) -> u64;
        pub fn file_size() -> u64;
        pub fn file_read(args: u64) -> u64;
        pub fn open_media(args: u64) -> u64;
        pub fn playlist_get() -> u64;
        pub fn playlist_edit(args: u64) -> u64;
        pub fn mpv_get(name: u64) -> u64;
        pub fn mpv_observe(name: u64) -> u64;
        pub fn mpv_set(name: u64, value: u64) -> u64;
        pub fn mpv_command(args: u64) -> u64;
        pub fn osd_show(text: u64) -> u64;
        pub fn overlay_set(args: u64, ass: u64) -> u64;
        pub fn overlay_clear(args: u64) -> u64;
        pub fn ui_update(view: u64, tree: u64) -> u64;
        pub fn ui_patch(view: u64, patches: u64) -> u64;
        pub fn ui_image(args: u64, bytes: u64) -> u64;
        pub fn ui_open(dialog: u64) -> u64;
        pub fn controls_set(id: u64, state: u64) -> u64;
        pub fn menu_set(id: u64, state: u64) -> u64;
        pub fn clipboard_read() -> u64;
        pub fn clipboard_write(text: u64) -> u64;
        pub fn http_request(request: u64, bytes: u64) -> u64;
        pub fn http_response_body() -> u64;
        pub fn timer_set(args: u64) -> u64;
        pub fn timer_clear(id: u64) -> u64;
        pub fn position_events(args: u64) -> u64;
        pub fn ipc_send(connection: u64, message: u64) -> u64;
        pub fn ipc_close(connection: u64) -> u64;
        pub fn pipe_connect(pipe: u64) -> u64;
        pub fn pipe_write(connection: u64, bytes: u64) -> u64;
        pub fn pipe_read(connection: u64) -> u64;
        pub fn pipe_close(connection: u64) -> u64;
        pub fn process_run(args: u64, stdin: u64) -> u64;
        pub fn process_stdout() -> u64;
        pub fn process_start(args: u64) -> u64;
        pub fn process_write(process: u64, bytes: u64) -> u64;
        pub fn process_read(process: u64, stream: u64) -> u64;
        pub fn process_end_input(process: u64) -> u64;
        pub fn process_close(process: u64) -> u64;
    }
}

thread_local! {
    static GRANTED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

#[doc(hidden)]
pub fn set_granted(granted: Vec<String>) {
    GRANTED.with(|slot| *slot.borrow_mut() = granted);
}

/// Whether the user has this permission (or `network:<host>`, `ipc.connect:<pipe>`) on right now.
pub fn granted(permission: &str) -> bool {
    GRANTED.with(|slot| slot.borrow().iter().any(|grant| grant == permission))
}

/// Every permission that is on right now.
pub fn granted_list() -> Vec<String> {
    GRANTED.with(|slot| slot.borrow().clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// The user turned this permission off.
    Denied(String),
    Failed(String),
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied(permission) => write!(f, "The {permission} permission is turned off"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

impl From<HostError> for String {
    fn from(error: HostError) -> Self {
        error.to_string()
    }
}

pub type HostResult<T> = Result<T, HostError>;

// Offset 0 stands for an empty block in both directions.
struct Block(Option<Memory>);

impl Block {
    fn bytes(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return Self(None);
        }
        Self(Memory::from_bytes(bytes).ok())
    }

    fn json(value: &impl Serialize) -> Self {
        Self::bytes(&serde_json::to_vec(value).unwrap_or_default())
    }

    fn offset(&self) -> u64 {
        self.0.as_ref().map_or(0, Memory::offset)
    }

    fn free(self) {
        if let Some(memory) = self.0 {
            memory.free();
        }
    }
}

fn take(offset: u64) -> Vec<u8> {
    if offset == 0 {
        return Vec::new();
    }
    match Memory::find(offset) {
        Some(memory) => {
            let bytes = memory.to_vec();
            memory.free();
            bytes
        }
        None => Vec::new(),
    }
}

// Every reply starts with a status byte: 0 ok, 1 denied (the permission follows), 2 failed (the message follows).
fn decode(mut reply: Vec<u8>) -> HostResult<Vec<u8>> {
    if reply.is_empty() {
        return Err(HostError::Failed("Rona sent no reply".into()));
    }
    let status = reply.remove(0);
    let text = || String::from_utf8_lossy(&reply).into_owned();
    match status {
        0 => Ok(reply),
        1 => Err(HostError::Denied(text())),
        _ => Err(HostError::Failed(text())),
    }
}

fn call0(function: unsafe extern "C" fn() -> u64) -> HostResult<Vec<u8>> {
    decode(take(unsafe { function() }))
}

fn call1(function: unsafe extern "C" fn(u64) -> u64, first: Block) -> HostResult<Vec<u8>> {
    let reply = take(unsafe { function(first.offset()) });
    first.free();
    decode(reply)
}

fn call2(function: unsafe extern "C" fn(u64, u64) -> u64, first: Block, second: Block) -> HostResult<Vec<u8>> {
    let reply = take(unsafe { function(first.offset(), second.offset()) });
    first.free();
    second.free();
    decode(reply)
}

fn parse<T: DeserializeOwned>(bytes: HostResult<Vec<u8>>) -> HostResult<T> {
    serde_json::from_slice(&bytes?).map_err(|err| HostError::Failed(format!("Unexpected reply from Rona: {err}")))
}

fn done(bytes: HostResult<Vec<u8>>) -> HostResult<()> {
    bytes.map(|_| ())
}

fn when<T>(permission: &str, value: T) -> Option<T> {
    granted(permission).then_some(value)
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// Lines show in the plugin's page in Settings > Plugins and in Rona's log file.
pub fn log(level: LogLevel, text: &str) {
    let _ = call1(raw::log, Block::json(&json!({ "level": level, "text": text })));
}

/// The plugin's own storage. Private to it, so it needs no permission.
pub mod storage {
    use super::*;

    pub fn get(key: &str) -> HostResult<Option<Vec<u8>>> {
        let mut reply = call1(raw::kv_get, Block::bytes(key.as_bytes()))?;
        if reply.first() != Some(&1) {
            return Ok(None);
        }
        reply.remove(0);
        Ok(Some(reply))
    }

    pub fn get_text(key: &str) -> HostResult<Option<String>> {
        Ok(get(key)?.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    pub fn set(key: &str, value: &[u8]) -> HostResult<()> {
        done(call2(raw::kv_set, Block::bytes(key.as_bytes()), Block::bytes(value)))
    }

    pub fn delete(key: &str) -> HostResult<()> {
        done(call1(raw::kv_delete, Block::bytes(key.as_bytes())))
    }

    pub fn keys(prefix: &str) -> HostResult<Vec<String>> {
        parse(call1(raw::kv_keys, Block::bytes(prefix.as_bytes())))
    }
}

/// The value of a static setting from `plugin.json`, or its declared default.
pub fn setting<T: DeserializeOwned>(id: &str) -> HostResult<Option<T>> {
    parse(call1(raw::setting_get, Block::bytes(id.as_bytes())))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerState {
    /// `idle`, `opening`, `playing` or `paused`.
    pub state: String,
    pub position: f64,
    pub duration: f64,
    pub speed: f64,
    pub volume: f64,
    pub muted: bool,
    /// Only with `media.metadata`.
    #[serde(default)]
    pub title: Option<String>,
    /// Only with `media.path`.
    #[serde(default)]
    pub path: Option<String>,
}

pub struct Playback(());

/// `playback.read`
pub fn playback() -> Option<Playback> {
    when(PLAYBACK_READ, Playback(()))
}

impl Playback {
    pub fn state(&self) -> HostResult<PlayerState> {
        parse(call0(raw::player_state))
    }

    /// Sends `position` events at most this often while something plays. 0 stops them, the least is 250 ms.
    pub fn position_events(&self, interval_ms: u64) -> HostResult<()> {
        done(call1(raw::position_events, Block::json(&json!({ "intervalMs": interval_ms }))))
    }
}

pub struct PlaybackControl(());

/// `playback.control`
pub fn playback_control() -> Option<PlaybackControl> {
    when(PLAYBACK_CONTROL, PlaybackControl(()))
}

impl PlaybackControl {
    fn send(&self, args: Value) -> HostResult<()> {
        done(call1(raw::player_control, Block::json(&args)))
    }

    pub fn play(&self) -> HostResult<()> {
        self.send(json!({ "action": "play" }))
    }

    pub fn pause(&self) -> HostResult<()> {
        self.send(json!({ "action": "pause" }))
    }

    pub fn toggle_pause(&self) -> HostResult<()> {
        self.send(json!({ "action": "toggle" }))
    }

    pub fn seek_to(&self, seconds: f64) -> HostResult<()> {
        self.send(json!({ "action": "seek", "to": seconds }))
    }

    pub fn seek_by(&self, seconds: f64) -> HostResult<()> {
        self.send(json!({ "action": "seek", "by": seconds }))
    }

    pub fn set_speed(&self, speed: f64) -> HostResult<()> {
        self.send(json!({ "action": "speed", "value": speed }))
    }

    /// 0 to 1, or more when the user allows a louder maximum.
    pub fn set_volume(&self, volume: f64) -> HostResult<()> {
        self.send(json!({ "action": "volume", "value": volume }))
    }

    pub fn set_muted(&self, muted: bool) -> HostResult<()> {
        self.send(json!({ "action": "mute", "value": muted }))
    }

    pub fn next(&self) -> HostResult<()> {
        self.send(json!({ "action": "next" }))
    }

    pub fn previous(&self) -> HostResult<()> {
        self.send(json!({ "action": "previous" }))
    }

    pub fn stop(&self) -> HostResult<()> {
        self.send(json!({ "action": "stop" }))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chapter {
    pub time: f64,
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub video_codec: Option<String>,
    #[serde(default)]
    pub audio_codec: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    pub audio_only: bool,
    #[serde(default)]
    pub chapters: Vec<Chapter>,
    /// Every tag the file has, as mpv reads them.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

pub struct Media(());

/// `media.metadata`
pub fn media() -> Option<Media> {
    when(MEDIA_METADATA, Media(()))
}

impl Media {
    pub fn info(&self) -> HostResult<MediaInfo> {
        parse(call0(raw::media_info))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub id: i64,
    /// `audio`, `sub` or `video`.
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub codec: Option<String>,
    pub selected: bool,
    pub external: bool,
}

pub struct Tracks(());

/// `media.tracks`
pub fn tracks() -> Option<Tracks> {
    when(MEDIA_TRACKS, Tracks(()))
}

impl Tracks {
    pub fn list(&self) -> HostResult<Vec<Track>> {
        parse(call0(raw::tracks_get))
    }

    /// `kind` is `audio` or `sub`. `None` turns that kind off.
    pub fn select(&self, kind: &str, id: Option<i64>) -> HostResult<()> {
        done(call1(raw::tracks_select, Block::json(&json!({ "kind": kind, "id": id }))))
    }
}

pub struct Subtitles(());

/// `media.subtitles`
pub fn subtitles() -> Option<Subtitles> {
    when(MEDIA_SUBTITLES, Subtitles(()))
}

impl Subtitles {
    /// Adds a subtitle file's contents to what plays and selects it. `extension` is like `srt` or `ass`.
    pub fn add(&self, name: &str, language: Option<&str>, extension: &str, bytes: &[u8]) -> HostResult<()> {
        let args = json!({ "name": name, "language": language, "extension": extension });
        done(call2(raw::subtitle_add, Block::json(&args), Block::bytes(bytes)))
    }
}

pub struct CurrentFile(());

/// `media.file`. Only local files can be read.
pub fn current_file() -> Option<CurrentFile> {
    when(MEDIA_FILE, CurrentFile(()))
}

impl CurrentFile {
    pub fn size(&self) -> HostResult<u64> {
        parse(call0(raw::file_size))
    }

    /// Up to 4 MiB from `offset`. Shorter at the end of the file.
    pub fn read(&self, offset: u64, length: u64) -> HostResult<Vec<u8>> {
        call1(raw::file_read, Block::json(&json!({ "offset": offset, "length": length })))
    }
}

pub struct Opener(());

/// `open.media`
pub fn opener() -> Option<Opener> {
    when(OPEN_MEDIA, Opener(()))
}

impl Opener {
    /// Replaces what plays with these paths or links, like opening them from the file dialog.
    pub fn open(&self, sources: &[&str]) -> HostResult<()> {
        done(call1(raw::open_media, Block::json(&json!({ "sources": sources, "append": false }))))
    }

    /// Adds them to the end of the playlist.
    pub fn append(&self, sources: &[&str]) -> HostResult<()> {
        done(call1(raw::open_media, Block::json(&json!({ "sources": sources, "append": true }))))
    }
}

pub struct Playlist(());

/// `playlist.read`
pub fn playlist() -> Option<Playlist> {
    when(PLAYLIST_READ, Playlist(()))
}

impl Playlist {
    /// The list as the user sees it. Paths only with `media.path`.
    pub fn entries(&self) -> HostResult<Vec<crate::PlaylistEntry>> {
        parse(call0(raw::playlist_get))
    }
}

pub struct PlaylistEdit(());

/// `playlist.edit`
pub fn playlist_edit() -> Option<PlaylistEdit> {
    when(PLAYLIST_EDIT, PlaylistEdit(()))
}

impl PlaylistEdit {
    /// Removed entries can be put back by the user with Undo, like any other removal.
    pub fn remove(&self, index: usize) -> HostResult<()> {
        done(call1(raw::playlist_edit, Block::json(&json!({ "op": "remove", "index": index }))))
    }

    /// Moves the entry so it lands before the one at `before`, or at the end when that is the length.
    pub fn move_entry(&self, index: usize, before: usize) -> HostResult<()> {
        done(call1(raw::playlist_edit, Block::json(&json!({ "op": "move", "index": index, "before": before }))))
    }

    pub fn play(&self, index: usize) -> HostResult<()> {
        done(call1(raw::playlist_edit, Block::json(&json!({ "op": "play", "index": index }))))
    }
}

pub struct MpvRead(());

/// `mpv.read`. Properties that hold paths also need `media.path`.
pub fn mpv_read() -> Option<MpvRead> {
    when(MPV_READ, MpvRead(()))
}

impl MpvRead {
    /// A property's value, parsed as JSON when mpv gives JSON, else as text.
    pub fn get(&self, name: &str) -> HostResult<Value> {
        parse(call1(raw::mpv_get, Block::bytes(name.as_bytes())))
    }

    /// Sends a `property` event each time it changes, and once right away.
    pub fn observe(&self, name: &str) -> HostResult<()> {
        done(call1(raw::mpv_observe, Block::bytes(name.as_bytes())))
    }
}

pub struct MpvWrite(());

/// `mpv.write`. Options that could reach outside the plugin (scripts, files, sockets) are refused.
pub fn mpv_write() -> Option<MpvWrite> {
    when(MPV_WRITE, MpvWrite(()))
}

impl MpvWrite {
    pub fn set(&self, name: &str, value: &str) -> HostResult<()> {
        done(call2(raw::mpv_set, Block::bytes(name.as_bytes()), Block::bytes(value.as_bytes())))
    }
}

pub struct MpvCommand(());

/// `mpv.command`. Only commands that stay inside the player are allowed.
pub fn mpv_command() -> Option<MpvCommand> {
    when(MPV_COMMAND, MpvCommand(()))
}

impl MpvCommand {
    pub fn run(&self, args: &[&str]) -> HostResult<()> {
        done(call1(raw::mpv_command, Block::json(&args)))
    }
}

pub struct Osd(());

/// `ui.osd`
pub fn osd() -> Option<Osd> {
    when(UI_OSD, Osd(()))
}

impl Osd {
    /// At most one message a second gets through.
    pub fn show(&self, text: &str) -> HostResult<()> {
        done(call1(raw::osd_show, Block::bytes(text.as_bytes())))
    }
}

pub struct Overlay(());

/// `ui.overlay`
pub fn overlay() -> Option<Overlay> {
    when(UI_OVERLAY, Overlay(()))
}

impl Overlay {
    /// ASS events drawn on the video, on a 1280 by 720 canvas scaled to the picture. Ids are 1 to 16.
    pub fn set(&self, id: u32, ass: &str) -> HostResult<()> {
        done(call2(raw::overlay_set, Block::json(&json!({ "id": id })), Block::bytes(ass.as_bytes())))
    }

    pub fn clear(&self, id: u32) -> HostResult<()> {
        done(call1(raw::overlay_clear, Block::json(&json!({ "id": id }))))
    }
}

/// The plugin's own views. They only exist when the manifest describes them.
pub mod ui {
    use super::*;

    /// Replaces the tree of an open view (`settings`, `panel`, `popover:<id>`, `dialog:<id>`).
    /// Dropped while that view isn't open. At most 10 updates a second get through.
    pub fn update(view: &str, tree: &[Value]) -> HostResult<()> {
        done(call2(raw::ui_update, Block::bytes(view.as_bytes()), Block::json(&tree)))
    }

    /// Changes props of nodes by id, like `[{ "id": "elapsed", "text": "1:20" }]`.
    pub fn patch(view: &str, patches: &[Value]) -> HostResult<()> {
        done(call2(raw::ui_patch, Block::bytes(view.as_bytes()), Block::json(&patches)))
    }

    /// Stores a PNG, JPEG or WebP for `image` nodes and list entries under `id`.
    pub fn image(id: &str, bytes: &[u8]) -> HostResult<()> {
        done(call2(raw::ui_image, Block::json(&json!({ "id": id })), Block::bytes(bytes)))
    }

    /// Opens one of the manifest's dialogs. Only while handling something the user did: a `control`,
    /// `menu` or `action` event. From `ui_event`, return `UiResponse::open` instead.
    pub fn open_dialog(dialog: &str) -> HostResult<()> {
        done(call1(raw::ui_open, Block::bytes(dialog.as_bytes())))
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dot: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

pub struct Controls(());

/// `ui.controls`
pub fn controls() -> Option<Controls> {
    when(UI_CONTROLS, Controls(()))
}

impl Controls {
    /// Changes a control from the manifest. Fields left `None` keep their value.
    pub fn set(&self, id: &str, state: &ControlState) -> HostResult<()> {
        done(call2(raw::controls_set, Block::bytes(id.as_bytes()), Block::json(state)))
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MenuState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
}

pub struct Menu(());

/// `ui.menu`
pub fn menu() -> Option<Menu> {
    when(UI_MENU, Menu(()))
}

impl Menu {
    pub fn set(&self, id: &str, state: &MenuState) -> HostResult<()> {
        done(call2(raw::menu_set, Block::bytes(id.as_bytes()), Block::json(state)))
    }
}

pub struct ClipboardRead(());

/// `clipboard.read`
pub fn clipboard_read() -> Option<ClipboardRead> {
    when(CLIPBOARD_READ, ClipboardRead(()))
}

impl ClipboardRead {
    pub fn text(&self) -> HostResult<String> {
        call0(raw::clipboard_read).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
}

pub struct ClipboardWrite(());

/// `clipboard.write`
pub fn clipboard_write() -> Option<ClipboardWrite> {
    when(CLIPBOARD_WRITE, ClipboardWrite(()))
}

impl ClipboardWrite {
    pub fn text(&self, text: &str) -> HostResult<()> {
        done(call1(raw::clipboard_write, Block::bytes(text.as_bytes())))
    }
}

/// Timers send `timer` events, since a plugin has no thread of its own. They stop with the plugin.
pub mod timers {
    use super::*;

    /// At least 250 ms. Setting an id again restarts it.
    pub fn set(id: &str, ms: u64, repeat: bool) -> HostResult<()> {
        done(call1(raw::timer_set, Block::json(&json!({ "id": id, "ms": ms, "repeat": repeat }))))
    }

    pub fn clear(id: &str) -> HostResult<()> {
        done(call1(raw::timer_clear, Block::bytes(id.as_bytes())))
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Body {
    #[default]
    None,
    Text {
        text: String,
    },
    Json {
        json: Value,
    },
    Bytes,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Body,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip)]
    pub bytes: Vec<u8>,
}

impl HttpRequest {
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self { method: method.into(), url: url.into(), headers: Vec::new(), body: Body::None, timeout_ms: None, bytes: Vec::new() }
    }

    pub fn get(url: impl Into<String>) -> Self {
        Self::new("GET", url)
    }

    pub fn post(url: impl Into<String>) -> Self {
        Self::new("POST", url)
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.body = Body::Text { text: text.into() };
        self
    }

    pub fn json(mut self, json: Value) -> Self {
        self.body = Body::Json { json };
        self
    }

    pub fn bytes(mut self, bytes: Vec<u8>) -> Self {
        self.body = Body::Bytes;
        self.bytes = bytes;
        self
    }

    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json<T: DeserializeOwned>(&self) -> HostResult<T> {
        serde_json::from_slice(&self.body).map_err(|err| HostError::Failed(format!("The response is not the expected JSON: {err}")))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }
}

#[derive(Deserialize)]
struct Reply {
    status: u16,
    #[serde(default)]
    headers: Vec<(String, String)>,
}

pub struct Network(());

/// Any `network:<host>` on. Each request is checked against the hosts that are on.
pub fn network() -> Option<Network> {
    GRANTED.with(|slot| slot.borrow().iter().any(|grant| base_of(grant) == NETWORK)).then_some(Network(()))
}

impl Network {
    /// `Err` is a network failure or a host that is off. An HTTP error status is still `Ok`.
    pub fn send(&self, request: HttpRequest) -> HostResult<HttpResponse> {
        let reply: Reply = parse(call2(raw::http_request, Block::json(&request), Block::bytes(&request.bytes)))?;
        Ok(HttpResponse { status: reply.status, headers: reply.headers, body: call0(raw::http_response_body)? })
    }
}

pub struct IpcServe(());

/// `ipc.serve`. Programs connect to Rona's plugin pipe and name this plugin. Each one arrives as an
/// `ipc-connected` event once the `ipc_accept` hook (if any) said yes.
pub fn ipc_serve() -> Option<IpcServe> {
    when(IPC_SERVE, IpcServe(()))
}

impl IpcServe {
    pub fn send(&self, connection: u32, message: &Value) -> HostResult<()> {
        done(call2(raw::ipc_send, Block::bytes(connection.to_string().as_bytes()), Block::json(message)))
    }

    pub fn close(&self, connection: u32) -> HostResult<()> {
        done(call1(raw::ipc_close, Block::bytes(connection.to_string().as_bytes())))
    }
}

pub struct Pipes(());

/// Any `ipc.connect:<pipe>` on. Each connect is checked against the pipes that are on.
pub fn pipes() -> Option<Pipes> {
    GRANTED.with(|slot| slot.borrow().iter().any(|grant| base_of(grant) == IPC_CONNECT)).then_some(Pipes(()))
}

impl Pipes {
    /// Opens `\\.\pipe\<name>` on Windows, or `<name>` in the runtime folder elsewhere. Bytes arriving
    /// on it send `pipe-data` events.
    pub fn connect(&self, pipe: &str) -> HostResult<u32> {
        parse(call1(raw::pipe_connect, Block::bytes(pipe.as_bytes())))
    }

    pub fn write(&self, connection: u32, bytes: &[u8]) -> HostResult<()> {
        done(call2(raw::pipe_write, Block::bytes(connection.to_string().as_bytes()), Block::bytes(bytes)))
    }

    /// Everything that arrived since the last read.
    pub fn read(&self, connection: u32) -> HostResult<Vec<u8>> {
        call1(raw::pipe_read, Block::bytes(connection.to_string().as_bytes()))
    }

    pub fn close(&self, connection: u32) -> HostResult<()> {
        done(call1(raw::pipe_close, Block::bytes(connection.to_string().as_bytes())))
    }
}

#[derive(Debug, Clone)]
pub struct RunOutput {
    /// `None` when the program was stopped, by the timeout or otherwise.
    pub status: Option<i32>,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl RunOutput {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunReply {
    status: Option<i32>,
    timed_out: bool,
    stderr: String,
}

pub struct Programs(());

/// Any `system.run:<program>` on. Each run is checked against the programs that are on.
pub fn programs() -> Option<Programs> {
    GRANTED.with(|slot| slot.borrow().iter().any(|grant| base_of(grant) == SYSTEM_RUN)).then_some(Programs(()))
}

impl Programs {
    /// Runs one of the manifest's programs, found on PATH, without a shell, and waits for it. `timeout_ms` is
    /// 60 s by default and 10 minutes at most. Output is kept up to 32 MB each.
    pub fn run(&self, program: &str, args: &[&str], stdin: &[u8], timeout_ms: Option<u64>) -> HostResult<RunOutput> {
        let request = json!({ "program": program, "args": args, "timeoutMs": timeout_ms });
        let reply: RunReply = parse(call2(raw::process_run, Block::json(&request), Block::bytes(stdin)))?;
        Ok(RunOutput { status: reply.status, timed_out: reply.timed_out, stderr: reply.stderr, stdout: call0(raw::process_stdout)? })
    }

    /// Starts one of the manifest's programs and returns at once. It runs until it exits, is closed or the
    /// plugin stops, with no time limit. Output sends `process-output` events and the end `process-exited`.
    /// At most 4 at a time, and ones that ended count until closed.
    pub fn start(&self, program: &str, args: &[&str]) -> HostResult<u32> {
        parse(call1(raw::process_start, Block::json(&json!({ "program": program, "args": args }))))
    }

    /// Sends bytes to the program's input.
    pub fn write(&self, process: u32, bytes: &[u8]) -> HostResult<()> {
        done(call2(raw::process_write, Block::bytes(process.to_string().as_bytes()), Block::bytes(bytes)))
    }

    /// Everything it printed on stdout since the last read.
    pub fn read_stdout(&self, process: u32) -> HostResult<Vec<u8>> {
        call2(raw::process_read, Block::bytes(process.to_string().as_bytes()), Block::bytes(b"stdout"))
    }

    pub fn read_stderr(&self, process: u32) -> HostResult<Vec<u8>> {
        call2(raw::process_read, Block::bytes(process.to_string().as_bytes()), Block::bytes(b"stderr"))
    }

    /// Closes its input, which tells many programs to finish.
    pub fn end_input(&self, process: u32) -> HostResult<()> {
        done(call1(raw::process_end_input, Block::bytes(process.to_string().as_bytes())))
    }

    /// Stops it if it still runs, along with what it started, and forgets it.
    pub fn close(&self, process: u32) -> HostResult<()> {
        done(call1(raw::process_close, Block::bytes(process.to_string().as_bytes())))
    }
}
