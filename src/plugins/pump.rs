//! Reads a plugin's own mpv client, keeps what it last saw for state reads, and turns it into the plugin's events.
//! Each event is only sent while the permission it needs is on.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use serde_json::{Value, json};

use crate::mpv::{Event, Mpv, PropertyValue};

use super::host::OBSERVE_ID_BASE;
use super::permissions::*;
use super::runtime::{Shared, lock};

const PAUSE: u64 = 1;
const TIME_POS: u64 = 2;
const DURATION: u64 = 3;
const PATH: u64 = 4;
const TITLE: u64 = 5;
const VOLUME: u64 = 6;
const MUTE: u64 = 7;
const SPEED: u64 = 8;
const TRACKS: u64 = 9;
const PLAYLIST: u64 = 10;
const CHAPTER: u64 = 11;
const IDLE: u64 = 12;

fn observe(mpv: &Mpv) -> Result<(), crate::mpv::Error> {
    mpv.observe_flag(PAUSE, "pause")?;
    mpv.observe_double(TIME_POS, "time-pos")?;
    mpv.observe_double(DURATION, "duration")?;
    mpv.observe_string(PATH, "path")?;
    mpv.observe_string(TITLE, "media-title")?;
    mpv.observe_double(VOLUME, "volume")?;
    mpv.observe_flag(MUTE, "mute")?;
    mpv.observe_double(SPEED, "speed")?;
    mpv.observe_changes(TRACKS, "track-list")?;
    mpv.observe_changes(PLAYLIST, "playlist")?;
    mpv.observe_double(CHAPTER, "chapter")?;
    mpv.observe_flag(IDLE, "idle-active")
}

fn number(value: Option<PropertyValue>) -> Option<f64> {
    match value {
        Some(PropertyValue::Double(number)) => Some(number),
        _ => None,
    }
}

fn text(value: Option<PropertyValue>) -> String {
    match value {
        Some(PropertyValue::Text(text)) => text,
        _ => String::new(),
    }
}

fn flag(value: Option<PropertyValue>) -> Option<bool> {
    match value {
        Some(PropertyValue::Flag(flag)) => Some(flag),
        _ => None,
    }
}

pub fn start(shared: Arc<Shared>) {
    let Some(mpv) = shared.client() else { return };
    if let Err(err) = observe(&mpv) {
        shared.log.push("error", &format!("Couldn't follow playback: {err}"));
        return;
    }
    let name = format!("plugin {} events", shared.id);
    let spawned = std::thread::Builder::new().name(name).spawn(move || run(shared, mpv));
    if let Err(err) = spawned {
        crate::logging::warn!("could not start a plugin's mpv thread: {err}");
    }
}

/// Makes the thread notice `stopped` and end, which lets go of its copy of the mpv client.
pub fn stop(shared: &Shared) {
    if let Some(mpv) = shared.client() {
        mpv.wake();
    }
}

fn path_for_plugin(shared: &Shared, path: &str) -> Value {
    if shared.granted(MEDIA_PATH) && !path.is_empty() { json!(crate::http_cache::unwrap_url(path)) } else { Value::Null }
}

fn run(shared: Arc<Shared>, mpv: Mpv) {
    let mut last_position_sent: Option<Instant> = None;
    let mut loaded = false;
    // mpv reports volume and speed once right after observing, which isn't a change the user made.
    let mut volume_known = false;
    let mut speed_known = false;
    loop {
        if shared.stopped.load(Ordering::SeqCst) {
            return;
        }
        let Some(event) = mpv.wait_event(-1.0) else { continue };
        let reads = shared.granted(PLAYBACK_READ);
        match event {
            Event::Shutdown => {
                shared.release_client();
                return;
            }
            Event::StartFile => {
                loaded = false;
                lock(&shared.player).opening = true;
            }
            Event::PlaybackRestart => {
                let player = lock(&shared.player).clone();
                if !loaded {
                    loaded = true;
                    lock(&shared.player).opening = false;
                    if reads {
                        let title = if shared.granted(MEDIA_METADATA) { json!(player.title) } else { Value::Null };
                        let path = path_for_plugin(&shared, &player.path);
                        shared.post_event(json!({ "type": "file-loaded", "path": path, "title": title, "duration": player.duration }));
                    }
                } else if reads {
                    shared.post_event(json!({ "type": "seek", "position": player.position }));
                }
            }
            Event::EndFile { reason, error } => {
                let reason = match reason {
                    0 => "eof",
                    2 => "stop",
                    3 => "quit",
                    crate::mpv::END_FILE_REASON_ERROR => "error",
                    _ => "other",
                };
                if reads {
                    shared.post_event(json!({ "type": "file-ended", "reason": reason }));
                }
                if let Some(error) = error {
                    shared.log.push("debug", &format!("the file ended with an error: {error}"));
                }
            }
            Event::CommandFailed(message) => shared.log.push("warn", &format!("mpv: {message}")),
            Event::PropertyChange { id, value } => {
                match id {
                    PAUSE => {
                        if let Some(paused) = flag(value) {
                            lock(&shared.player).paused = paused;
                            if reads {
                                shared.post_event(json!({ "type": "pause", "paused": paused }));
                            }
                        }
                    }
                    TIME_POS => {
                        let position = number(value).unwrap_or(0.0);
                        let (duration, paused) = {
                            let mut player = lock(&shared.player);
                            player.position = position;
                            (player.duration, player.paused)
                        };
                        let interval = shared.position_interval_ms.load(Ordering::SeqCst);
                        let due = last_position_sent.is_none_or(|at| at.elapsed().as_millis() as u64 >= interval);
                        if reads && interval > 0 && due && !paused {
                            last_position_sent = Some(Instant::now());
                            shared.post_event(json!({ "type": "position", "position": position, "duration": duration }));
                        }
                    }
                    DURATION => lock(&shared.player).duration = number(value).unwrap_or(0.0),
                    PATH => {
                        let path = text(value);
                        if !path.is_empty() && reads {
                            shared.post_event(json!({ "type": "file-started", "path": path_for_plugin(&shared, &path) }));
                        }
                        lock(&shared.player).path = path;
                    }
                    TITLE => lock(&shared.player).title = text(value),
                    VOLUME => {
                        let volume = number(value).unwrap_or(0.0);
                        let muted = {
                            let mut player = lock(&shared.player);
                            player.volume = volume;
                            player.muted
                        };
                        if reads && std::mem::replace(&mut volume_known, true) {
                            shared.post_event(json!({ "type": "volume", "volume": volume / 100.0, "muted": muted }));
                        }
                    }
                    MUTE => {
                        if let Some(muted) = flag(value) {
                            let volume = {
                                let mut player = lock(&shared.player);
                                player.muted = muted;
                                player.volume
                            };
                            if reads && volume_known {
                                shared.post_event(json!({ "type": "volume", "volume": volume / 100.0, "muted": muted }));
                            }
                        }
                    }
                    SPEED => {
                        let speed = number(value).unwrap_or(1.0);
                        lock(&shared.player).speed = speed;
                        if reads && std::mem::replace(&mut speed_known, true) {
                            shared.post_event(json!({ "type": "speed", "speed": speed }));
                        }
                    }
                    TRACKS => {
                        if shared.granted(MEDIA_TRACKS) {
                            shared.post_event(json!({ "type": "tracks-changed" }));
                        }
                    }
                    PLAYLIST => {
                        if shared.granted(PLAYLIST_READ) {
                            shared.post_event(json!({ "type": "playlist-changed" }));
                        }
                    }
                    CHAPTER => {
                        let chapter = number(value).map_or(-1, |chapter| chapter as i64);
                        lock(&shared.player).chapter = chapter;
                        if shared.granted(MEDIA_METADATA) && chapter >= 0 {
                            let title = mpv.get_string(&format!("chapter-list/{chapter}/title"));
                            shared.post_event(json!({ "type": "chapter-changed", "chapter": chapter, "title": title }));
                        }
                    }
                    IDLE => {
                        if let Some(idle) = flag(value) {
                            lock(&shared.player).idle = idle;
                            if idle && reads {
                                shared.post_event(json!({ "type": "idle" }));
                            }
                        }
                    }
                    id if id >= OBSERVE_ID_BASE && shared.granted(MPV_READ) => {
                        let name = lock(&shared.observed).iter().find(|(_, known)| **known == id).map(|(name, _)| name.clone());
                        let paths_allowed = |name: &str| {
                            super::mpv_allow::readable(name) != super::mpv_allow::Readable::WithPaths || shared.granted(MEDIA_PATH)
                        };
                        if let Some(name) = name.filter(|name| paths_allowed(name)) {
                            let raw = text(value);
                            let value = serde_json::from_str(&raw).unwrap_or(Value::String(raw));
                            shared.post_event(json!({ "type": "property", "name": name, "value": value }));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
