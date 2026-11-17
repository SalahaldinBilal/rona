use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::mpv::Mpv;
use crate::state::{self, PlayModes, Repeat, SharedState};
use crate::{AppWindow, CoverState, Player, PlaylistItem, audio_info, file_display_name, log_error, media, opener};

const UNDO_SHOWN_FOR: Duration = Duration::from_secs(5);
// Next is where playback usually goes, previous is one key press away.
const READ_AHEAD_NEXT: usize = 3;
const READ_AHEAD_PREVIOUS: usize = 1;

/// One entry of mpv's playlist. mpv's playlist is kept in play order, the list as shown is kept here.
pub struct Entry {
    id: i64,
    filename: String,
    title: String,
    current: bool,
    // The playlist's title, artist and cover from the file itself, once read.
    summary: Option<Arc<audio_info::Summary>>,
}

pub struct Read {
    entries: Vec<Entry>,
}

// Runs on a background thread, since each property read waits on mpv's core.
pub fn read(mpv: &Mpv) -> Read {
    let count = mpv
        .get_string("playlist/count")
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or(0);
    let entries = (0..count)
        .map(|index| {
            let field = |name: &str| mpv.get_string(&format!("playlist/{index}/{name}"));
            let filename = field("filename").unwrap_or_default();
            let summary = audio_info::summary(&filename);
            // mpv only knows a file's title once it has played it, so the pre-read one fills in before that.
            let title = field("title")
                .filter(|title| !title.is_empty())
                .or_else(|| summary.as_ref().and_then(|summary| summary.title.clone()))
                .unwrap_or_else(|| file_display_name(&filename));
            Entry {
                id: field("id").and_then(|id| id.parse().ok()).unwrap_or(-(index as i64) - 1),
                filename,
                title,
                current: field("current").as_deref() == Some("yes"),
                summary,
            }
        })
        .collect();
    Read { entries }
}

struct Removed {
    filename: String,
    list_index: usize,
    // Played entries go back among the played ones when shuffle is on.
    played: bool,
}

struct Playlist {
    app: slint::Weak<AppWindow>,
    mpv: Mpv,
    state: SharedState,
    // The kind of the open playlist, which picks the set of modes.
    audio: bool,
    modes: PlayModes,
    // mpv's playlist as last read, in play order. Updated right away for our own changes.
    play: Vec<Entry>,
    // The list's own order, as mpv entry ids.
    list: Vec<i64>,
    // A new playlist was opened, so the next read that shows it starts the list over.
    fresh: bool,
    // Entries put back by undo that mpv hasn't listed yet, with their place in the list.
    restoring: Vec<(String, usize)>,
    removed: Vec<Removed>,
    undo_timer: Timer,
    last_current: Option<usize>,
    random: Cell<u64>,
}

thread_local! {
    static PLAYLIST: RefCell<Option<Rc<RefCell<Playlist>>>> = const { RefCell::new(None) };
}

fn with_playlist<T>(action: impl FnOnce(&mut Playlist) -> T) -> Option<T> {
    let playlist = PLAYLIST.with(|slot| slot.borrow().clone())?;
    let mut playlist = playlist.borrow_mut();
    Some(action(&mut playlist))
}

pub fn wire(app: &AppWindow, mpv: &Mpv, state: &SharedState) {
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |since| since.as_nanos() as u64) | 1;
    let modes = state.borrow().video_modes;
    let playlist = Playlist {
        app: app.as_weak(),
        mpv: mpv.clone(),
        state: state.clone(),
        audio: false,
        modes,
        play: Vec::new(),
        list: Vec::new(),
        fresh: false,
        restoring: Vec::new(),
        removed: Vec::new(),
        undo_timer: Timer::default(),
        last_current: None,
        random: Cell::new(seed),
    };
    playlist.show_modes();
    PLAYLIST.with(|slot| *slot.borrow_mut() = Some(Rc::new(RefCell::new(playlist))));

    let player = app.global::<Player>();
    player.on_play_entry(|index| {
        with_playlist(|playlist| playlist.play_entry(index));
    });
    player.on_remove_entry(|index| {
        with_playlist(|playlist| playlist.remove_entry(index));
    });
    player.on_move_entry(|from, to| {
        with_playlist(|playlist| playlist.move_entry(from, to));
    });
    player.on_play_entry_next(|index| {
        with_playlist(|playlist| playlist.play_entry_next(index));
    });
    player.on_undo_remove(|| {
        with_playlist(Playlist::undo_remove);
    });
    player.on_toggle_shuffle(|| {
        with_playlist(Playlist::toggle_shuffle);
    });
    player.on_cycle_repeat(|| {
        with_playlist(Playlist::cycle_repeat);
    });
}

/// A new playlist is about to replace the open one. A playlist counts as audio when every entry is.
pub fn starting(paths: &[String]) {
    with_playlist(|playlist| {
        playlist.audio = !paths.is_empty() && paths.iter().all(|path| media::is_audio_name(path));
        playlist.modes = playlist.saved_modes();
        playlist.fresh = true;
        playlist.restoring.clear();
        playlist.removed.clear();
        playlist.hide_undo();
        playlist.send_repeat();
        playlist.show_modes();
    });
}

/// Whether mpv goes on after the current entry, also by starting the playlist over.
pub fn has_next() -> bool {
    with_playlist(|playlist| {
        let current = playlist.current();
        playlist.modes.repeat != Repeat::Off || current.is_some_and(|current| current + 1 < playlist.play.len())
    })
    .unwrap_or(false)
}

pub fn apply(player: &Player, read: Read) {
    with_playlist(|playlist| playlist.update(player, read.entries));
}

impl Playlist {
    fn saved_modes(&self) -> PlayModes {
        let state = self.state.borrow();
        if self.audio { state.audio_modes } else { state.video_modes }
    }

    fn save_modes(&self) {
        let mut state = self.state.borrow_mut();
        if self.audio {
            state.audio_modes = self.modes;
        } else {
            state.video_modes = self.modes;
        }
        state::save(&state);
    }

    fn show_modes(&self) {
        if let Some(app) = self.app.upgrade() {
            let player = app.global::<Player>();
            player.set_shuffle(self.modes.shuffle);
            player.set_repeat(self.modes.repeat.index());
        }
    }

    // mpv repeats the play order, which is already shuffled when shuffle is on.
    fn send_repeat(&self) {
        let value = match self.modes.repeat {
            Repeat::Off => "no",
            Repeat::Once => "2",
            Repeat::Forever => "inf",
        };
        log_error(self.mpv.command(&["set", "loop-playlist", value]));
    }

    fn current(&self) -> Option<usize> {
        self.play.iter().position(|entry| entry.current)
    }

    fn play_index(&self, id: i64) -> Option<usize> {
        self.play.iter().position(|entry| entry.id == id)
    }

    fn list_id(&self, index: i32) -> Option<i64> {
        usize::try_from(index).ok().and_then(|index| self.list.get(index).copied())
    }

    fn random_below(&self, bound: usize) -> usize {
        // xorshift, plenty for picking a play order.
        let mut value = self.random.get();
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.random.set(value);
        (value % bound.max(1) as u64) as usize
    }

    // mpv puts the entry before the one at `to`, or last when `to` is the length. Returns where it landed.
    fn move_in_mpv(&mut self, from: usize, to: usize) -> usize {
        if from == to || to == from + 1 || from >= self.play.len() {
            return from;
        }
        log_error(self.mpv.command(&["playlist-move", &from.to_string(), &to.to_string()]));
        let entry = self.play.remove(from);
        let landed = if to > from { to - 1 } else { to };
        self.play.insert(landed, entry);
        landed
    }

    fn arrange_mpv(&mut self, order: &[i64]) {
        for (target, id) in order.iter().enumerate() {
            if let Some(index) = self.play_index(*id) {
                self.move_in_mpv(index, target);
            }
        }
    }

    // The current entry first, so everything else is still to play, in a random order.
    fn shuffled_order(&self) -> Vec<i64> {
        let current = self.current().unwrap_or(0);
        let mut rest: Vec<i64> =
            self.play.iter().enumerate().filter(|(index, _)| *index != current).map(|(_, entry)| entry.id).collect();
        for index in (1..rest.len()).rev() {
            rest.swap(index, self.random_below(index + 1));
        }
        self.play.get(current).map(|entry| entry.id).into_iter().chain(rest).collect()
    }

    fn update(&mut self, player: &Player, entries: Vec<Entry>) {
        // A read from before the new playlist replaced the old one still shows only old entries.
        let fresh = self.fresh && entries.iter().any(|entry| !self.list.contains(&entry.id));
        if fresh {
            self.fresh = false;
            self.list.clear();
            self.last_current = None;
        }
        self.list.retain(|id| entries.iter().any(|entry| entry.id == *id));
        let mut added = Vec::new();
        for entry in &entries {
            if self.list.contains(&entry.id) {
                continue;
            }
            match self.restoring.iter().position(|(filename, _)| *filename == entry.filename) {
                Some(position) => {
                    let (_, at) = self.restoring.remove(position);
                    self.list.insert(at.min(self.list.len()), entry.id);
                }
                None => {
                    self.list.push(entry.id);
                    added.push(entry.id);
                }
            }
        }
        self.play = entries;

        let current = self.current();
        let wrapped = matches!((self.last_current, current), (Some(before), Some(0)) if before + 1 == self.play.len() && before > 0);
        if self.modes.shuffle {
            if fresh || wrapped {
                let order = self.shuffled_order();
                self.arrange_mpv(&order);
            } else {
                // Entries added later get a random place among the ones still to play.
                for id in added {
                    let (Some(index), Some(current)) = (self.play_index(id), self.current()) else { continue };
                    if index > current {
                        let to = current + 1 + self.random_below(self.play.len() - current);
                        self.move_in_mpv(index, to);
                    }
                }
            }
        }
        self.last_current = self.current();
        self.publish(player);
    }

    fn publish(&self, player: &Player) {
        let current = self.current();
        let next = match current {
            Some(current) if current + 1 < self.play.len() => Some(current + 1),
            // Without shuffle, repeating starts over at the top. With it, the next pass gets a new order.
            Some(_) if self.modes.repeat != Repeat::Off && !self.modes.shuffle && self.play.len() > 1 => Some(0),
            _ => None,
        };
        let items: Vec<PlaylistItem> = self
            .list
            .iter()
            .filter_map(|id| {
                let index = self.play_index(*id)?;
                let entry = &self.play[index];
                let thumb = entry.summary.as_ref().and_then(|summary| summary.thumb.clone());
                // Still being read when a readable file has no summary yet.
                let cover_state = match (&entry.summary, &thumb) {
                    (_, Some(_)) => CoverState::Shown,
                    (None, None) if audio_info::can_read(&entry.filename) => CoverState::Loading,
                    _ => CoverState::Missing,
                };
                Some(PlaylistItem {
                    is_audio: media::is_audio_name(&entry.filename),
                    artist: entry
                        .summary
                        .as_ref()
                        .and_then(|summary| summary.artist.as_deref())
                        .unwrap_or_default()
                        .into(),
                    cover_state,
                    cover: thumb.map(slint::Image::from_rgba8).unwrap_or_default(),
                    title: entry.title.as_str().into(),
                    current: entry.current,
                    failed: opener::has_failed(&entry.filename),
                    played: self.modes.shuffle && current.is_some_and(|current| index < current),
                    next: next == Some(index),
                })
            })
            .collect();
        let next_entry = next.and_then(|index| self.play.get(index));
        player.set_next_title(next_entry.map(|entry| entry.title.as_str()).unwrap_or_default().into());
        let next_cover = next_entry.and_then(|entry| audio_info::get(&entry.filename)).and_then(|info| info.cover.clone());
        player.set_has_next_cover(next_cover.is_some());
        if let Some(cover) = next_cover {
            player.set_next_cover(slint::Image::from_rgba8(cover));
        }
        let track = current.and_then(|current| self.list.iter().position(|id| *id == self.play[current].id));
        player.set_track_number(track.map_or(0, |track| track as i32 + 1));
        player.set_playlist_covers(self.play.iter().any(|entry| media::is_audio_name(&entry.filename)));
        player.set_playlist(ModelRc::new(VecModel::from(items)));

        let files: Vec<String> = self.play.iter().map(|entry| entry.filename.clone()).collect();
        let upcoming: Vec<String> = current
            .map(|current| {
                let next = (current..files.len()).take(READ_AHEAD_NEXT + 1);
                let previous = (0..current).rev().take(READ_AHEAD_PREVIOUS);
                next.chain(previous).map(|index| files[index].clone()).collect()
            })
            .unwrap_or_default();
        audio_info::prefetch(upcoming);
        audio_info::read_summaries(&files);
    }

    fn refresh(&self) {
        if let Some(app) = self.app.upgrade() {
            self.publish(&app.global::<Player>());
        }
    }

    // In shuffle, a clicked entry is moved right after the current one first, so it counts as played.
    fn play_entry(&mut self, index: i32) {
        let Some(mut target) = self.list_id(index).and_then(|id| self.play_index(id)) else { return };
        if self.modes.shuffle
            && let Some(current) = self.current()
            && target != current
        {
            target = self.move_in_mpv(target, current + 1);
        }
        log_error(self.mpv.command(&["playlist-play-index", &target.to_string()]));
    }

    fn play_entry_next(&mut self, index: i32) {
        let (Some(id), Some(current)) = (self.list_id(index), self.current()) else { return };
        let Some(from) = self.play_index(id).filter(|from| *from != current) else { return };
        if self.modes.shuffle {
            self.move_in_mpv(from, current + 1);
        } else {
            let current_id = self.play[current].id;
            let after_current = self.list.iter().position(|id| *id == current_id).map_or(0, |position| position + 1);
            self.move_entry(index, after_current as i32);
            return;
        }
        self.refresh();
    }

    // Moves within the list. mpv follows only while the play order is the list order.
    fn move_entry(&mut self, from: i32, to: i32) {
        let (Ok(from), Ok(to)) = (usize::try_from(from), usize::try_from(to)) else { return };
        if from >= self.list.len() || to > self.list.len() || to == from || to == from + 1 {
            return;
        }
        let id = self.list[from];
        if !self.modes.shuffle {
            let target = match self.list.get(to) {
                Some(before) => self.play_index(*before),
                None => Some(self.play.len()),
            };
            if let (Some(index), Some(target)) = (self.play_index(id), target) {
                self.move_in_mpv(index, target);
            }
        }
        self.list.remove(from);
        self.list.insert(if to > from { to - 1 } else { to }, id);
        self.refresh();
    }

    fn remove_entry(&mut self, index: i32) {
        let Ok(list_index) = usize::try_from(index) else { return };
        let Some(id) = self.list.get(list_index).copied() else { return };
        let Some(play_index) = self.play_index(id) else { return };
        let played = self.current().is_some_and(|current| play_index <= current);
        log_error(self.mpv.command(&["playlist-remove", &play_index.to_string()]));
        let entry = self.play.remove(play_index);
        self.list.remove(list_index);
        if let Some(app) = self.app.upgrade() {
            app.global::<Player>().set_undo_text(format!("Removed {}", entry.title).into());
        }
        let app_weak = self.app.clone();
        self.undo_timer.start(TimerMode::SingleShot, UNDO_SHOWN_FOR, move || {
            if let Some(app) = app_weak.upgrade() {
                app.global::<Player>().set_undo_text(SharedString::new());
            }
        });
        self.removed.push(Removed { filename: entry.filename, list_index, played });
        self.refresh();
    }

    fn hide_undo(&self) {
        self.undo_timer.stop();
        if let Some(app) = self.app.upgrade() {
            app.global::<Player>().set_undo_text(SharedString::new());
        }
    }

    // mpv gives the entry back a new id, so it's matched by file name when mpv lists it.
    fn undo_remove(&mut self) {
        let Some(removed) = self.removed.pop() else { return };
        self.hide_undo();
        let current = self.current();
        let index = match current {
            Some(current) if self.modes.shuffle && removed.played => current,
            Some(current) if self.modes.shuffle => current + 1 + self.random_below(self.play.len() - current),
            _ => match self.list.get(removed.list_index) {
                Some(id) => self.play_index(*id).unwrap_or(self.play.len()),
                None => self.play.len(),
            },
        };
        log_error(self.mpv.command(&["loadfile", &removed.filename, "insert-at", &index.to_string()]));
        self.restoring.push((removed.filename, removed.list_index));
    }

    fn toggle_shuffle(&mut self) {
        self.modes.shuffle = !self.modes.shuffle;
        self.save_modes();
        self.show_modes();
        let order = if self.modes.shuffle { self.shuffled_order() } else { self.list.clone() };
        self.arrange_mpv(&order);
        self.last_current = self.current();
        self.refresh();
    }

    fn cycle_repeat(&mut self) {
        self.modes.repeat = self.modes.repeat.next();
        self.save_modes();
        self.show_modes();
        self.send_repeat();
        self.refresh();
    }
}
