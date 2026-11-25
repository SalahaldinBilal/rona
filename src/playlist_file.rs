use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::http_cache;
use crate::logging::{debug, info};

pub const EXTENSION: &str = crate::extensions::PLAYLIST_EXTENSIONS[0];
pub const VERSION: u32 = 1;
const MAX_DOWNLOAD: u64 = 16 * 1024 * 1024;
const MAX_INCLUDE_DEPTH: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Series,
    Music,
    #[default]
    #[serde(other)]
    Other,
}

impl Kind {
    fn is_other(&self) -> bool {
        *self == Kind::Other
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    Off,
    Once,
    Forever,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Series {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Playback {
    // None keeps what Rona remembered for this kind of playlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shuffle: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<RepeatMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<usize>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub autosave: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Playback {
    fn is_default(&self) -> bool {
        *self == Playback::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlaylistFile {
    pub version: u32,
    #[serde(rename = "type", default, skip_serializing_if = "Kind::is_other")]
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<Series>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    #[serde(default, skip_serializing_if = "Playback::is_default")]
    pub playback: Playback,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<Section>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Section {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<Entry>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Skip {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(serialize_with = "seconds")]
    pub from: f64,
    #[serde(serialize_with = "seconds")]
    pub to: f64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Subtitle {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub default: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tracks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitles: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Tracks {
    fn is_empty(&self) -> bool {
        *self == Tracks::default()
    }
}

/// Size and modification time (Unix seconds) of a local file when the playlist was saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub size: u64,
    pub modified: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absolute: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aired: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none", serialize_with = "optional_seconds")]
    pub duration: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", serialize_with = "optional_seconds")]
    pub start: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", serialize_with = "optional_seconds")]
    pub end: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip: Vec<Skip>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtitles: Vec<Subtitle>,
    #[serde(default, skip_serializing_if = "Tracks::is_empty")]
    pub tracks: Tracks,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_playlist_headers: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Check>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Entry {
    pub fn new(source: String) -> Self {
        Entry { source, ..Entry::default() }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

// Whole seconds are written as integers, so a hand-written `80` doesn't come back as `80.0`.
fn seconds<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64((value * 1000.0).round() / 1000.0)
    }
}

fn optional_seconds<S: Serializer>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => seconds(value, serializer),
        None => serializer.serialize_none(),
    }
}

/// Where a playlist came from, which decides where relative paths point and whether Save can write it back.
#[derive(Clone, Debug, PartialEq)]
pub enum Origin {
    File(PathBuf),
    Link(String),
    Text,
}

#[derive(Clone, Debug, PartialEq)]
enum Base {
    Folder(PathBuf),
    Url(String),
}

/// One entry in the order shown, with every path in it made absolute.
#[derive(Clone, Debug)]
pub struct Item {
    pub entry: Entry,
    pub section: Option<usize>,
    // From a section's `include`, so it isn't written back on save.
    pub included: bool,
    // The playlist's headers with the entry's own on top, for links.
    pub headers: BTreeMap<String, String>,
}

pub struct Loaded {
    // Top-level fields and sections. Entries live in `items`, sections keep only what isn't an entry.
    pub file: PlaylistFile,
    pub origin: Origin,
    pub items: Vec<Item>,
    // Local paths in a playlist from the web, which aren't followed.
    pub skipped_local: usize,
    pub warnings: Vec<String>,
}

pub fn is_playlist_name(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let name = name.split(['?', '#']).next().unwrap_or(name);
    name.rsplit_once('.').is_some_and(|(_, extension)| extension.eq_ignore_ascii_case(EXTENSION))
}

/// A command line argument holding a whole playlist rather than a path to one.
pub fn is_playlist_text(argument: &str) -> bool {
    argument.trim_start().starts_with('{')
}

/// Makes the paths in a playlist given as text absolute against `folder`, so another window can read it as it is.
/// Text that isn't a playlist is returned unchanged, and fails with a readable message when opened.
pub fn absolutize_text(text: &str, folder: &Path) -> String {
    let Ok(mut file) = parse(text) else { return text.to_string() };
    absolutize(&mut file, &Base::Folder(folder.to_path_buf()));
    serde_json::to_string(&file).unwrap_or_else(|_| text.to_string())
}

pub fn parse(text: &str) -> Result<PlaylistFile, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let file: PlaylistFile = serde_json::from_str(text).map_err(|err| format!("This isn't a valid playlist: {err}."))?;
    if file.version == 0 {
        return Err("The playlist has no version.".into());
    }
    Ok(file)
}

pub fn read_path(path: &Path) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("Couldn't read {}: {}", path.display(), err))?;
    let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();
    load(&text, Base::Folder(folder), Origin::File(path.to_path_buf()))
}

pub fn read_link(url: &str) -> Result<Loaded, String> {
    let text = download(url)?;
    load(&text, Base::Url(url.to_string()), Origin::Link(url.to_string()))
}

pub fn read_text(text: &str, folder: &Path) -> Result<Loaded, String> {
    load(text, Base::Folder(folder.to_path_buf()), Origin::Text)
}

fn download(url: &str) -> Result<String, String> {
    info!("downloading the playlist {url}");
    let host = crate::media::link_host(url);
    let response = http_cache::agent().get(url).call().map_err(|err| http_cache::network_error_text(&err, &host))?;
    response
        .into_body()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .read_to_string()
        .map_err(|err| format!("Couldn't read the playlist: {err}"))
}

fn load(text: &str, base: Base, origin: Origin) -> Result<Loaded, String> {
    let mut file = parse(text)?;
    if file.version > VERSION {
        info!("the playlist is version {}, newer than {VERSION}, reading what this version knows", file.version);
    }
    let mut skipped_local = absolutize(&mut file, &base);
    let mut warnings = Vec::new();
    let mut seen = vec![origin_key(&origin)];
    let mut items = Vec::new();
    let headers = file.headers.clone();
    push_items(&mut items, std::mem::take(&mut file.entries), None, false, &headers);
    for index in 0..file.sections.len() {
        let entries = std::mem::take(&mut file.sections[index].entries);
        push_items(&mut items, entries, Some(index), false, &headers);
        if let Some(include) = file.sections[index].include.clone() {
            match include_items(&include, &base, 1, &mut seen) {
                Ok((entries, skipped)) => {
                    skipped_local += skipped;
                    push_items(&mut items, entries, Some(index), true, &headers);
                }
                Err(err) => warnings.push(err),
            }
        }
    }
    debug!("playlist with {} entries, {} sections", items.len(), file.sections.len());
    Ok(Loaded { file, origin, items, skipped_local, warnings })
}

fn push_items(items: &mut Vec<Item>, entries: Vec<Entry>, section: Option<usize>, included: bool, headers: &BTreeMap<String, String>) {
    for entry in entries {
        let mut merged = if entry.use_playlist_headers == Some(false) { BTreeMap::new() } else { headers.clone() };
        merged.extend(entry.headers.clone());
        items.push(Item { entry, section, included, headers: merged });
    }
}

fn origin_key(origin: &Origin) -> String {
    match origin {
        Origin::File(path) => path_key(path),
        Origin::Link(url) => url.clone(),
        Origin::Text => String::new(),
    }
}

fn path_key(path: &Path) -> String {
    let text = normalize(path).to_string_lossy().into_owned();
    if cfg!(windows) { text.to_lowercase() } else { text }
}

// An included playlist's entries all go into the including section, its own sections included.
fn include_items(include: &str, parent: &Base, depth: usize, seen: &mut Vec<String>) -> Result<(Vec<Entry>, usize), String> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err(format!("{include} is included more than {MAX_INCLUDE_DEPTH} levels deep, so it was left out."));
    }
    let is_link = include.contains("://");
    if matches!(parent, Base::Url(_)) && !is_link {
        return Err(format!("A playlist from the web can't include the local file {include}."));
    }
    let key = if is_link { include.to_string() } else { path_key(Path::new(include)) };
    if seen.contains(&key) {
        return Err(format!("{include} includes itself, so it was left out."));
    }
    seen.push(key);
    let (text, base) = if is_link {
        (download(include)?, Base::Url(include.to_string()))
    } else {
        let path = Path::new(include);
        let text = std::fs::read_to_string(path).map_err(|err| format!("Couldn't read {include}: {err}"))?;
        (text, Base::Folder(path.parent().map(Path::to_path_buf).unwrap_or_default()))
    };
    let mut file = parse(&text).map_err(|err| format!("{include}: {err}"))?;
    let mut skipped = absolutize(&mut file, &base);
    let mut entries = std::mem::take(&mut file.entries);
    for section in &mut file.sections {
        entries.append(&mut section.entries);
        if let Some(nested) = &section.include {
            let (mut more, more_skipped) = include_items(nested, &base, depth + 1, seen)?;
            skipped += more_skipped;
            entries.append(&mut more);
        }
    }
    Ok((entries, skipped))
}

// Makes every path in the playlist absolute. Returns how many entries were dropped for pointing at local files
// from a playlist on the web.
fn absolutize(file: &mut PlaylistFile, base: &Base) -> usize {
    let resolve_option = |value: &mut Option<String>| {
        if let Some(text) = value.take() {
            *value = resolve(base, &text);
        }
    };
    resolve_option(&mut file.cover);
    resolve_option(&mut file.background);
    let mut skipped = absolutize_entries(&mut file.entries, base);
    for section in &mut file.sections {
        resolve_option(&mut section.cover);
        resolve_option(&mut section.include);
        skipped += absolutize_entries(&mut section.entries, base);
    }
    skipped
}

fn absolutize_entries(entries: &mut Vec<Entry>, base: &Base) -> usize {
    let before = entries.len();
    entries.retain_mut(|entry| {
        let Some(source) = resolve(base, &entry.source) else { return false };
        entry.source = source;
        entry.mirrors = entry.mirrors.iter().filter_map(|mirror| resolve(base, mirror)).collect();
        entry.thumbnail = entry.thumbnail.take().and_then(|thumbnail| resolve(base, &thumbnail));
        entry.audio = entry.audio.take().and_then(|audio| resolve(base, &audio));
        entry.subtitles.retain_mut(|subtitle| match resolve(base, &subtitle.source) {
            Some(source) => {
                subtitle.source = source;
                true
            }
            None => false,
        });
        true
    });
    before - entries.len()
}

fn is_link(text: &str) -> bool {
    text.contains("://")
}

fn is_local_absolute(text: &str) -> bool {
    let bytes = text.as_bytes();
    let drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/');
    drive || text.starts_with("\\\\")
}

// None for a local path in a playlist from the web, which isn't followed.
fn resolve(base: &Base, source: &str) -> Option<String> {
    let source = source.trim();
    if source.is_empty() {
        return None;
    }
    if is_link(source) {
        return Some(source.to_string());
    }
    match base {
        Base::Folder(folder) => {
            let path = Path::new(source);
            let full = if path.is_absolute() || is_local_absolute(source) { path.to_path_buf() } else { folder.join(path) };
            Some(normalize(&full).to_string_lossy().into_owned())
        }
        Base::Url(url) => {
            if is_local_absolute(source) {
                return None;
            }
            Some(join_url(url, source))
        }
    }
}

// Lexical, so a path that doesn't exist yet still resolves, and `..` doesn't follow links.
fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    result.push("..");
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn join_url(base: &str, relative: &str) -> String {
    let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    if let Some(network) = relative.strip_prefix("//") {
        return format!("{scheme}://{network}");
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let joined = if relative.starts_with('/') {
        relative.to_string()
    } else {
        let folder = path.rfind('/').map_or("/", |index| &path[..=index]);
        format!("{folder}{relative}")
    };
    let (joined_path, query) = match joined.find(['?', '#']) {
        Some(index) => joined.split_at(index),
        None => (joined.as_str(), ""),
    };
    let mut segments: Vec<&str> = Vec::new();
    for segment in joined_path.split('/').skip(1) {
        match segment {
            "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    format!("{scheme}://{authority}/{}{query}", segments.join("/"))
}

/// The playlist as JSON, with local paths inside `folder` written relative to it so the folder can move.
pub fn to_json(file: &PlaylistFile, folder: Option<&Path>) -> String {
    let mut file = file.clone();
    if let Some(folder) = folder {
        relativize(&mut file, folder);
    }
    let mut text = serde_json::to_string_pretty(&file).unwrap_or_default();
    text.push('\n');
    text
}

fn relativize(file: &mut PlaylistFile, folder: &Path) {
    let relative = |value: &mut Option<String>| {
        if let Some(text) = value.as_mut() {
            *text = relative_to(folder, text);
        }
    };
    relative(&mut file.cover);
    relative(&mut file.background);
    let entries = |entries: &mut Vec<Entry>| {
        for entry in entries {
            entry.source = relative_to(folder, &entry.source);
            for mirror in &mut entry.mirrors {
                *mirror = relative_to(folder, mirror);
            }
            if let Some(thumbnail) = entry.thumbnail.as_mut() {
                *thumbnail = relative_to(folder, thumbnail);
            }
            if let Some(audio) = entry.audio.as_mut() {
                *audio = relative_to(folder, audio);
            }
            for subtitle in &mut entry.subtitles {
                subtitle.source = relative_to(folder, &subtitle.source);
            }
        }
    };
    entries(&mut file.entries);
    for section in &mut file.sections {
        relative(&mut section.cover);
        relative(&mut section.include);
        entries(&mut section.entries);
    }
}

/// A path inside `folder` as a relative path with forward slashes, anything else as it is.
pub fn relative_to(folder: &Path, source: &str) -> String {
    if is_link(source) || folder.as_os_str().is_empty() {
        return source.to_string();
    }
    let folder = normalize(folder);
    let path = normalize(Path::new(source));
    let mut folder_parts = folder.components();
    let mut path_parts = path.components();
    loop {
        match folder_parts.next() {
            None => break,
            Some(part) => match path_parts.next() {
                Some(other) if same_component(part, other) => {}
                _ => return source.to_string(),
            },
        }
    }
    let rest: Vec<String> = path_parts.map(|part| part.as_os_str().to_string_lossy().into_owned()).collect();
    if rest.is_empty() { source.to_string() } else { rest.join("/") }
}

fn same_component(a: Component, b: Component) -> bool {
    if cfg!(windows) {
        a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// An `#EXTM3U` list of (title, duration, source), which most players read.
pub fn to_m3u(entries: &[(String, Option<f64>, String)]) -> String {
    let mut text = String::from("#EXTM3U\n");
    for (title, duration, source) in entries {
        let seconds = duration.map_or(-1, |duration| duration.round() as i64);
        let title = title.replace(['\r', '\n'], " ");
        text.push_str(&format!("#EXTINF:{seconds},{title}\n{source}\n"));
    }
    text
}

/// Reads `80`, `1:20`, `1:02:03` and fractions like `1:20.5` as seconds.
pub fn parse_time(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() > 3 || parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    let mut seconds = 0.0;
    for (index, part) in parts.iter().enumerate() {
        let last = index == parts.len() - 1;
        let value: f64 = if last {
            part.parse().ok().filter(|value: &f64| value.is_finite() && *value >= 0.0)?
        } else {
            f64::from(part.parse::<u32>().ok()?)
        };
        if index > 0 && value >= 60.0 {
            return None;
        }
        seconds = seconds * 60.0 + value;
    }
    Some(seconds)
}

pub fn time_text(seconds: f64) -> String {
    let whole = seconds.floor() as u64;
    let fraction = seconds - whole as f64;
    let (hours, minutes, rest) = (whole / 3600, whole / 60 % 60, whole % 60);
    let mut text =
        if hours > 0 { format!("{hours}:{minutes:02}:{rest:02}") } else { format!("{minutes}:{rest:02}") };
    if fraction >= 0.05 {
        text.push_str(&format!(".{}", (fraction * 10.0).round() as u32 % 10));
    }
    text
}

pub fn file_check(path: &str) -> Option<Check> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(Check { size: metadata.len(), modified })
}

pub fn now_text() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERIES: &str = r#"{
  "version": 1,
  "type": "series",
  "title": "Frieren",
  "cover": "frieren-poster.jpg",
  "series": { "name": "Frieren: Beyond Journey's End", "year": 2023 },
  "playback": { "repeat": "off", "start": 3 },
  "headers": { "Referer": "https://example.com/" },
  "made_by": "some other tool",
  "sections": [
    {
      "title": "Season 1",
      "season": 1,
      "entries": [
        {
          "source": "Season 1/Frieren - 05.mkv",
          "mirrors": ["https://media.example.com/frieren/05.mkv"],
          "title": "Phantoms of the Dead",
          "episode": 5,
          "duration": 1450,
          "skip": [{ "name": "Opening", "from": 80, "to": 170.5 }],
          "subtitles": [{ "source": "Season 1/Frieren - 05.en.ass", "language": "en", "default": true }],
          "headers": { "Cookie": "a=b" },
          "watched_on": "phone"
        }
      ]
    }
  ]
}"#;

    fn folder() -> PathBuf {
        if cfg!(windows) { PathBuf::from(r"D:\Shows\Frieren") } else { PathBuf::from("/media/shows/frieren") }
    }

    #[test]
    fn relative_sources_resolve_against_the_playlist_folder() {
        let loaded = read_text(SERIES, &folder()).unwrap();
        let entry = &loaded.items[0].entry;
        assert_eq!(Path::new(&entry.source), folder().join("Season 1").join("Frieren - 05.mkv"));
        assert_eq!(entry.mirrors, ["https://media.example.com/frieren/05.mkv"]);
        assert_eq!(Path::new(&entry.subtitles[0].source), folder().join("Season 1").join("Frieren - 05.en.ass"));
        assert_eq!(Path::new(loaded.file.cover.as_deref().unwrap()), folder().join("frieren-poster.jpg"));
        assert_eq!(loaded.items[0].section, Some(0));
    }

    #[test]
    fn entry_headers_add_to_the_playlist_headers() {
        let loaded = read_text(SERIES, &folder()).unwrap();
        let headers = &loaded.items[0].headers;
        assert_eq!(headers.get("Referer").map(String::as_str), Some("https://example.com/"));
        assert_eq!(headers.get("Cookie").map(String::as_str), Some("a=b"));
    }

    #[test]
    fn saving_keeps_keys_from_other_tools_and_writes_paths_relative_again() {
        let loaded = read_text(SERIES, &folder()).unwrap();
        let mut file = loaded.file.clone();
        file.sections[0].entries = loaded.items.iter().map(|item| item.entry.clone()).collect();
        let text = to_json(&file, Some(&folder()));
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["made_by"], "some other tool");
        let entry = &value["sections"][0]["entries"][0];
        assert_eq!(entry["watched_on"], "phone");
        assert_eq!(entry["source"], "Season 1/Frieren - 05.mkv");
        assert_eq!(value["cover"], "frieren-poster.jpg");
        assert_eq!(entry["duration"], 1450);
        assert_eq!(entry["skip"][0]["to"], 170.5);
    }

    #[test]
    fn paths_outside_the_folder_stay_absolute_on_save() {
        let elsewhere = if cfg!(windows) { r"E:\Other\movie.mkv" } else { "/mnt/other/movie.mkv" };
        assert_eq!(relative_to(&folder(), elsewhere), elsewhere);
        let parent_relative = folder().join("..").join("Other").join("a.mkv");
        assert_eq!(relative_to(&folder(), &parent_relative.to_string_lossy()), parent_relative.to_string_lossy());
    }

    #[test]
    fn a_playlist_from_the_web_does_not_follow_local_paths() {
        let text = r#"{ "version": 1, "entries": [
            { "source": "C:\\Users\\someone\\secret.mkv" },
            { "source": "episodes/01.mkv" },
            { "source": "/files/02.mkv?token=x" },
            { "source": "../03.mkv" }
        ] }"#;
        let mut file = parse(text).unwrap();
        let skipped = absolutize(&mut file, &Base::Url("https://cdn.example.com/show/list.rl?key=1".into()));
        assert_eq!(skipped, 1);
        let sources: Vec<&str> = file.entries.iter().map(|entry| entry.source.as_str()).collect();
        assert_eq!(
            sources,
            ["https://cdn.example.com/show/episodes/01.mkv", "https://cdn.example.com/files/02.mkv?token=x", "https://cdn.example.com/03.mkv"]
        );
    }

    #[test]
    fn text_from_the_command_line_is_made_absolute_before_handing_over() {
        let text = r#"{"version":1,"entries":[{"source":"a.mkv"},{"source":"https://x.example/b.mkv"}]}"#;
        let absolute = absolutize_text(text, &folder());
        let file = parse(&absolute).unwrap();
        assert_eq!(Path::new(&file.entries[0].source), folder().join("a.mkv"));
        assert_eq!(file.entries[1].source, "https://x.example/b.mkv");
        assert!(is_playlist_text("  {\"version\":1}"));
        assert!(!is_playlist_text(r"C:\Videos\{draft}.mkv"));
    }

    #[test]
    fn broken_or_versionless_text_gives_a_message() {
        assert!(parse(r#"{"entries":[]}"#).is_err());
        assert!(parse(r#"{"version":1,"entries":[{"title":"no source"}]}"#).is_err());
        let err = parse("{\"version\":1,").unwrap_err();
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn a_file_saved_with_a_byte_order_mark_still_reads() {
        assert!(parse("\u{feff}{\"version\":1}").is_ok());
    }

    #[test]
    fn include_loops_are_refused() {
        let dir = std::env::temp_dir().join(format!("rona-playlist-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rl"), r#"{"version":1,"sections":[{"title":"B","include":"b.rl"}]}"#).unwrap();
        std::fs::write(dir.join("b.rl"), r#"{"version":1,"entries":[{"source":"b1.mkv"}],"sections":[{"include":"a.rl"}]}"#)
            .unwrap();
        let loaded = read_path(&dir.join("a.rl"));
        let _ = std::fs::remove_dir_all(&dir);
        let loaded = loaded.unwrap();
        assert_eq!(loaded.items.len(), 0);
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
    }

    #[test]
    fn included_entries_join_the_including_section() {
        let dir = std::env::temp_dir().join(format!("rona-playlist-include-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("s2")).unwrap();
        std::fs::write(
            dir.join("show.rl"),
            r#"{"version":1,"sections":[{"title":"Season 1","entries":[{"source":"s1/01.mkv"}]},{"title":"Season 2","include":"s2/list.rl"}]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("s2").join("list.rl"), r#"{"version":1,"entries":[{"source":"01.mkv"},{"source":"02.mkv"}]}"#)
            .unwrap();
        let loaded = read_path(&dir.join("show.rl"));
        let _ = std::fs::remove_dir_all(&dir);
        let loaded = loaded.unwrap();
        let second: Vec<&Item> = loaded.items.iter().filter(|item| item.section == Some(1)).collect();
        assert_eq!(second.len(), 2);
        assert!(second.iter().all(|item| item.included));
        assert!(Path::new(&second[0].entry.source).ends_with(Path::new("s2").join("01.mkv")));
    }

    #[test]
    fn times_read_like_people_write_them() {
        assert_eq!(parse_time("80"), Some(80.0));
        assert_eq!(parse_time("1:20"), Some(80.0));
        assert_eq!(parse_time("1:02:03"), Some(3723.0));
        assert_eq!(parse_time("22:00.5"), Some(1320.5));
        assert_eq!(parse_time("1:75"), None);
        assert_eq!(parse_time("abc"), None);
        assert_eq!(parse_time(""), None);
        assert_eq!(time_text(80.0), "1:20");
        assert_eq!(time_text(3723.0), "1:02:03");
        assert_eq!(time_text(1320.5), "22:00.5");
    }

    #[test]
    fn m3u_export_lists_titles_and_durations() {
        let text = to_m3u(&[
            ("Phantoms of the Dead".into(), Some(1450.4), r"D:\Shows\05.mkv".into()),
            ("Live".into(), None, "https://x.example/live.m3u8".into()),
        ]);
        assert_eq!(text, "#EXTM3U\n#EXTINF:1450,Phantoms of the Dead\nD:\\Shows\\05.mkv\n#EXTINF:-1,Live\nhttps://x.example/live.m3u8\n");
    }

    #[test]
    fn playlist_names_are_told_apart_from_media() {
        assert!(is_playlist_name(r"D:\Shows\Frieren.rl"));
        assert!(is_playlist_name("https://example.com/list.RL?token=1"));
        assert!(!is_playlist_name(r"D:\Shows\Frieren.mkv"));
    }
}
