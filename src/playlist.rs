use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::images::{self, Size};
use crate::logging::{debug, info, warn};
use crate::mpv::Mpv;
use crate::playlist_file::{self, Entry, Item, Kind, Origin, PlaylistFile, RepeatMode};
use crate::state::{self, PlayModes, Repeat, SharedState};
use crate::{
    AppWindow, CoverState, Player, PlaylistDoc, PlaylistItem, audio_info, file_display_name, http_cache, log_error, media,
    opener, resume,
};

pub const HOOK_PRELOADED: u64 = 3;

const UNDO_SHOWN_FOR: Duration = Duration::from_secs(5);
const AUTOSAVE_DELAY: Duration = Duration::from_secs(1);
// Next is where playback usually goes, previous is one key press away.
const READ_AHEAD_NEXT: usize = 3;
const READ_AHEAD_PREVIOUS: usize = 1;
// Must match the panel's row and heading heights.
const ROW_HEIGHT: f32 = 46.0;
const HEADING_HEIGHT: f32 = 30.0;
const LIST_PADDING: f32 = 4.0;

/// One entry of mpv's playlist. mpv's playlist is kept in play order, the list as shown is kept here.
pub struct MpvEntry {
    id: i64,
    filename: String,
    title: String,
    current: bool,
    // The playlist's title, artist and cover from the file itself, once read.
    summary: Option<Arc<audio_info::Summary>>,
}

pub struct Read {
    entries: Vec<MpvEntry>,
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
            MpvEntry {
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Problem {
    None,
    Missing,
    Changed,
}

/// What the playlist knows about one of its entries, kept with the mpv entry it was loaded as.
#[derive(Clone)]
pub struct Meta {
    pub entry: Entry,
    pub section: Option<usize>,
    pub included: bool,
    pub headers: BTreeMap<String, String>,
    pub problem: Problem,
    // Learned while playing, and written into the file on save when it has none.
    pub seen_duration: Option<f64>,
    mirrors_tried: usize,
    // Place in the playlist as loaded, so entries mpv lists out of order still land in the right place.
    order: Option<usize>,
}

impl Meta {
    pub fn from_item(item: Item, problem: Problem) -> Self {
        Meta {
            entry: item.entry,
            section: item.section,
            included: item.included,
            headers: item.headers,
            problem,
            seen_duration: None,
            mirrors_tried: 0,
            order: None,
        }
    }

    pub fn plain(source: String) -> Self {
        Meta {
            entry: Entry::new(source),
            section: None,
            included: false,
            headers: BTreeMap::new(),
            problem: Problem::None,
            seen_duration: None,
            mirrors_tried: 0,
            order: None,
        }
    }

    fn duration(&self) -> Option<f64> {
        self.entry.duration.or(self.seen_duration)
    }
}

/// An opened `.rl` playlist. A list of files opened any other way has none.
pub struct Document {
    // Everything but the entries, which live with the list.
    pub file: PlaylistFile,
    pub origin: Origin,
    // Not loaded, written back to their section on save.
    pub disabled: Vec<Item>,
    pub dirty: bool,
}

pub struct Opened {
    pub document: Option<Document>,
    // In load order, keyed by the file name given to mpv.
    pub metas: Vec<(String, Meta)>,
}

struct Removed {
    filename: String,
    list_index: usize,
    // Played entries go back among the played ones when shuffle is on.
    played: bool,
    meta: Option<Meta>,
}

#[derive(Clone, Copy)]
enum RowKind {
    Heading,
    Entry(usize),
}

#[derive(Clone, Copy)]
struct Row {
    kind: RowKind,
    top: f32,
    height: f32,
    // The list index this row starts at, so a drop before it lands there.
    list_at: usize,
    section: Option<usize>,
}

pub struct Playlist {
    app: slint::Weak<AppWindow>,
    mpv: Mpv,
    state: SharedState,
    // The kind of the open playlist, which picks the set of modes.
    audio: bool,
    modes: PlayModes,
    // mpv's playlist as last read, in play order. Updated right away for our own changes.
    play: Vec<MpvEntry>,
    // The list's own order, as mpv entry ids.
    list: Vec<i64>,
    metas: HashMap<i64, Meta>,
    // Loaded entries mpv hasn't listed yet.
    pending: Vec<(String, Meta)>,
    document: Option<Document>,
    // A new playlist was opened, so the next read that shows it starts the list over.
    fresh: bool,
    // Entries put back or replaced that mpv hasn't listed yet, with their place in the list.
    restoring: Vec<(String, usize, Option<Meta>)>,
    removed: Vec<Removed>,
    undo_timer: Timer,
    autosave_timer: Timer,
    last_current: Option<usize>,
    random: Cell<u64>,
    rows: Vec<Row>,
    collapsed: HashSet<usize>,
    // Skip ranges already used in the file playing now, so going back into one plays it.
    skips_done: (Option<i64>, HashSet<usize>),
}

thread_local! {
    static PLAYLIST: RefCell<Option<Rc<RefCell<Playlist>>>> = const { RefCell::new(None) };
}

pub fn with_playlist<T>(action: impl FnOnce(&mut Playlist) -> T) -> Option<T> {
    let playlist = PLAYLIST.with(|slot| slot.borrow().clone())?;
    let mut playlist = playlist.try_borrow_mut().ok()?;
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
        metas: HashMap::new(),
        pending: Vec::new(),
        document: None,
        fresh: false,
        restoring: Vec::new(),
        removed: Vec::new(),
        undo_timer: Timer::default(),
        autosave_timer: Timer::default(),
        last_current: None,
        random: Cell::new(seed),
        rows: Vec::new(),
        collapsed: HashSet::new(),
        skips_done: (None, HashSet::new()),
    };
    playlist.show_modes();
    PLAYLIST.with(|slot| *slot.borrow_mut() = Some(Rc::new(RefCell::new(playlist))));
    log_error(mpv.add_hook(HOOK_PRELOADED, "on_preloaded"));

    let app_weak = app.as_weak();
    images::on_loaded(move || {
        let _ = app_weak.upgrade_in_event_loop(|_| {
            with_playlist(|playlist| playlist.refresh());
            crate::playlist_edit::refresh_previews();
        });
    });

    let player = app.global::<Player>();
    player.on_play_entry(|index| {
        with_playlist(|playlist| playlist.play_entry(index));
    });
    player.on_remove_entry(|index| {
        with_playlist(|playlist| playlist.remove_entry(index));
    });
    player.on_move_entry(|from, to| {
        with_playlist(|playlist| playlist.move_entry(from, to, None));
    });
    player.on_move_row(|from, to| {
        with_playlist(|playlist| playlist.move_row(from, to));
    });
    player.on_row_at(|y| with_playlist(|playlist| playlist.row_at(y)).unwrap_or(0));
    player.on_step_row(|row, step| with_playlist(|playlist| playlist.step_row(row, step)).unwrap_or(row));
    player.on_toggle_section(|row| {
        with_playlist(|playlist| playlist.toggle_section(row));
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
pub fn starting(paths: &[String], opened: Opened) {
    with_playlist(|playlist| {
        let music = opened.document.as_ref().is_some_and(|document| document.file.kind == Kind::Music);
        playlist.audio = music || (!paths.is_empty() && paths.iter().all(|path| media::is_audio_name(path)));
        playlist.modes = playlist.saved_modes();
        if let Some(document) = &opened.document {
            let playback = &document.file.playback;
            if let Some(shuffle) = playback.shuffle {
                playlist.modes.shuffle = shuffle;
            }
            if let Some(repeat) = playback.repeat {
                playlist.modes.repeat = match repeat {
                    RepeatMode::Off => Repeat::Off,
                    RepeatMode::Once => Repeat::Once,
                    RepeatMode::Forever => Repeat::Forever,
                };
            }
        }
        info!(
            "new {} playlist of {}{}, shuffle {}, repeat {:?}",
            if playlist.audio { "audio" } else { "video" },
            paths.len(),
            if opened.document.is_some() { " from a playlist file" } else { "" },
            playlist.modes.shuffle,
            playlist.modes.repeat
        );
        playlist.fresh = true;
        playlist.pending = opened
            .metas
            .into_iter()
            .enumerate()
            .map(|(order, (filename, mut meta))| {
                meta.order = Some(order);
                (filename, meta)
            })
            .collect();
        playlist.document = opened.document;
        playlist.collapsed.clear();
        playlist.autosave_timer.stop();
        playlist.restoring.clear();
        playlist.removed.clear();
        playlist.hide_undo();
        playlist.send_repeat();
        playlist.show_modes();
        playlist.show_document();
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

/// mpv's on_load hook. The file's options from the playlist go in before resume adds its own, so a saved position
/// wins over the entry's start.
pub fn file_loading(mpv: &Mpv) {
    let filename = mpv.get_string("stream-open-filename").unwrap_or_default();
    with_playlist(|playlist| {
        let Some(meta) = playlist.meta_by_filename(&filename) else { return };
        let entry = &meta.entry;
        let set = |name: &str, value: &str| log_error(mpv.set_property(&format!("file-local-options/{name}"), value));
        if let Some(start) = entry.start {
            set("start", &format!("{start:.3}"));
        }
        if let Some(end) = entry.end {
            set("end", &format!("{end:.3}"));
        }
        if let Some(language) = entry.tracks.audio.as_deref().filter(|language| !language.is_empty()) {
            set("alang", language);
        }
        if let Some(language) = entry.tracks.subtitles.as_deref().filter(|language| !language.is_empty()) {
            set("slang", language);
        }
        if let Some(volume) = entry.volume.filter(|volume| *volume != 0.0) {
            set("volume-gain", &format!("{volume}"));
        }
        if let Some(title) = playlist.media_title(meta) {
            set("force-media-title", &title);
        }
    });
}

/// mpv's on_preloaded hook, where a file's tracks exist and extra ones can be added.
pub fn file_preloaded(mpv: &Mpv) {
    let filename = mpv.get_string("stream-open-filename").unwrap_or_default();
    with_playlist(|playlist| {
        let Some(meta) = playlist.meta_by_filename(&filename) else { return };
        for subtitle in &meta.entry.subtitles {
            let flag = if subtitle.default { "select" } else { "auto" };
            let language = subtitle.language.as_deref().unwrap_or("");
            log_error(mpv.command(&["sub-add", &subtitle.source, flag, "", language]));
        }
        if let Some(audio) = &meta.entry.audio {
            log_error(mpv.command(&["audio-add", audio, "auto"]));
        }
    });
}

/// Jumps over the entry's skip ranges. Each range is used once per file, so seeking back into one plays it.
pub fn check_skip(mpv: &Mpv, position: f64) {
    with_playlist(|playlist| {
        let Some(current) = playlist.current() else { return };
        let id = playlist.play[current].id;
        if playlist.skips_done.0 != Some(id) {
            playlist.skips_done = (Some(id), HashSet::new());
        }
        let Some(meta) = playlist.metas.get(&id) else { return };
        let hit = meta
            .entry
            .skip
            .iter()
            .enumerate()
            .find(|(index, skip)| !playlist.skips_done.1.contains(index) && position >= skip.from && position < skip.to - 0.5);
        if let Some((index, skip)) = hit {
            info!("skipping {} from {:.1} to {:.1} s", skip.name.as_deref().unwrap_or("a range"), skip.from, skip.to);
            log_error(mpv.command(&["seek", &format!("{:.3}", skip.to), "absolute"]));
            playlist.skips_done.1.insert(index);
        }
    });
}

pub fn duration_known(duration: f64) {
    if duration <= 0.0 {
        return;
    }
    with_playlist(|playlist| {
        let Some(current) = playlist.current() else { return };
        let id = playlist.play[current].id;
        if let Some(meta) = playlist.metas.get_mut(&id)
            && meta.seen_duration.is_none()
        {
            meta.seen_duration = Some(duration);
            playlist.refresh();
        }
    });
}

/// What a playlist file says about an audio entry, which wins over the file's own tags in the audio view.
#[derive(Default)]
pub struct AudioOverrides {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_line: Option<String>,
}

pub fn audio_overrides(filename: &str) -> AudioOverrides {
    with_playlist(|playlist| playlist.meta_by_filename(filename).map(|meta| playlist.overrides_of(meta)))
        .flatten()
        .unwrap_or_default()
}

pub fn current_audio_overrides() -> AudioOverrides {
    with_playlist(|playlist| {
        let current = playlist.current()?;
        let meta = playlist.metas.get(&playlist.play[current].id)?;
        Some(playlist.overrides_of(meta))
    })
    .flatten()
    .unwrap_or_default()
}

/// A failed entry with mirrors left is put back in its place with the next one. Returns whether that happened.
pub fn try_mirror(filename: &str) -> bool {
    with_playlist(|playlist| playlist.try_mirror(filename)).unwrap_or(false)
}

/// An edited playlist that isn't saved on its own would lose its changes.
pub fn has_unsaved_changes() -> bool {
    with_playlist(|playlist| {
        playlist
            .document
            .as_ref()
            .is_some_and(|document| document.dirty && !(document.file.playback.autosave && can_save_in_place(document)))
    })
    .unwrap_or(false)
}

/// The user chose not to save, so the edits no longer hold anything up.
pub fn forget_changes() {
    with_playlist(|playlist| {
        if let Some(document) = playlist.document.as_mut() {
            document.dirty = false;
        }
        playlist.autosave_timer.stop();
    });
}

fn can_save_in_place(document: &Document) -> bool {
    matches!(document.origin, Origin::File(_))
}

pub fn document_path() -> Option<PathBuf> {
    with_playlist(|playlist| match playlist.document.as_ref().map(|document| &document.origin) {
        Some(Origin::File(path)) => Some(path.clone()),
        _ => None,
    })
    .flatten()
}

/// Writes the playlist to `target`, or back to its own file. A list without a file needs a target.
pub fn save(target: Option<PathBuf>) -> Result<PathBuf, String> {
    with_playlist(|playlist| playlist.save(target)).unwrap_or_else(|| Err("The playlist isn't ready.".into()))
}

/// The open list as M3U, with absolute paths.
pub fn m3u() -> String {
    with_playlist(|playlist| {
        let entries: Vec<(String, Option<f64>, String)> = playlist
            .list
            .iter()
            .filter_map(|id| {
                let meta = playlist.metas.get(id)?;
                let entry = playlist.play.iter().find(|entry| entry.id == *id)?;
                Some((playlist.title_of(entry, meta), meta.duration(), meta.entry.source.clone()))
            })
            .collect();
        playlist_file::to_m3u(&entries)
    })
    .unwrap_or_default()
}

pub fn suggested_name() -> String {
    with_playlist(|playlist| {
        let title = playlist.document.as_ref().and_then(|document| document.file.title.clone());
        let title = title.or_else(|| {
            let first = playlist.list.first().and_then(|id| playlist.metas.get(id))?;
            let path = Path::new(&first.entry.source);
            path.parent().and_then(Path::file_name).map(|name| name.to_string_lossy().into_owned())
        });
        title.unwrap_or_else(|| "Playlist".into())
    })
    .unwrap_or_else(|| "Playlist".into())
}

/// Whether there is anything to save.
pub fn is_empty() -> bool {
    with_playlist(|playlist| playlist.list.is_empty()).unwrap_or(true)
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

    // mpv can open the first file before the playlist read that matches it to its entry, so the entries still
    // waiting for that count too.
    fn meta_by_filename(&self, filename: &str) -> Option<&Meta> {
        let listed = self.play.iter().find(|entry| entry.filename == filename).and_then(|entry| self.metas.get(&entry.id));
        listed.or_else(|| self.pending.iter().find(|(pending, _)| pending == filename).map(|(_, meta)| meta))
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

    // Where a newly listed entry from the loaded playlist goes, after the ones loaded before it.
    fn place_for_order(&self, order: usize) -> usize {
        self.list
            .iter()
            .position(|id| self.metas.get(id).and_then(|meta| meta.order).is_some_and(|other| other > order))
            .unwrap_or(self.list.len())
    }

    fn update(&mut self, player: &Player, entries: Vec<MpvEntry>) {
        // A read from before the new playlist replaced the old one still shows only old entries.
        let fresh = self.fresh && entries.iter().any(|entry| !self.list.contains(&entry.id));
        if fresh {
            self.fresh = false;
            self.list.clear();
            self.metas.clear();
            self.last_current = None;
        }
        self.list.retain(|id| entries.iter().any(|entry| entry.id == *id));
        self.metas.retain(|id, _| entries.iter().any(|entry| entry.id == *id));
        let mut added = Vec::new();
        let mut added_by_user = false;
        for entry in &entries {
            if self.list.contains(&entry.id) {
                continue;
            }
            if let Some(position) = self.restoring.iter().position(|(filename, _, _)| *filename == entry.filename) {
                let (_, at, meta) = self.restoring.remove(position);
                self.list.insert(at.min(self.list.len()), entry.id);
                self.metas.insert(entry.id, meta.unwrap_or_else(|| Meta::plain(source_of(&entry.filename))));
                continue;
            }
            if let Some(position) = self.pending.iter().position(|(filename, _)| *filename == entry.filename) {
                let (_, meta) = self.pending.remove(position);
                let at = meta.order.map_or(self.list.len(), |order| self.place_for_order(order));
                self.list.insert(at, entry.id);
                self.metas.insert(entry.id, meta);
            } else {
                self.list.push(entry.id);
                self.metas.insert(entry.id, Meta::plain(source_of(&entry.filename)));
                added_by_user = !fresh;
            }
            added.push(entry.id);
        }
        self.play = entries;
        if added_by_user {
            self.mark_dirty();
        }

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

    fn kind(&self) -> Kind {
        self.document.as_ref().map_or(Kind::Other, |document| document.file.kind)
    }

    fn section_season(&self, meta: &Meta) -> Option<u32> {
        meta.entry.season.or_else(|| {
            let section = meta.section?;
            self.document.as_ref()?.file.sections.get(section)?.season
        })
    }

    fn title_of(&self, entry: &MpvEntry, meta: &Meta) -> String {
        meta.entry.title.clone().filter(|title| !title.is_empty()).unwrap_or_else(|| entry.title.clone())
    }

    fn episode_label(&self, meta: &Meta) -> Option<String> {
        let episode = meta.entry.episode?;
        Some(match self.section_season(meta) {
            Some(season) => format!("S{season} E{episode}"),
            None => format!("E{episode}"),
        })
    }

    // The window title for an entry, when the playlist says more than the file does.
    fn media_title(&self, meta: &Meta) -> Option<String> {
        let title = meta.entry.title.clone().filter(|title| !title.is_empty());
        if self.kind() != Kind::Series {
            return title;
        }
        let series = self.document.as_ref().and_then(|document| {
            let file = &document.file;
            file.title.clone().or_else(|| file.series.as_ref().and_then(|series| series.name.clone()))
        });
        let parts: Vec<String> = [series, self.episode_label(meta), title].into_iter().flatten().collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    fn overrides_of(&self, meta: &Meta) -> AudioOverrides {
        let file = self.document.as_ref().map(|document| &document.file);
        let entry = &meta.entry;
        let album = entry.album.clone().or_else(|| file.and_then(|file| file.album.clone()));
        let year = entry.year.or_else(|| file.and_then(|file| file.year));
        let album_line = match (album, year) {
            (Some(album), Some(year)) => Some(format!("{album} · {year}")),
            (Some(album), None) => Some(album),
            (None, _) => None,
        };
        AudioOverrides {
            title: entry.title.clone().filter(|title| !title.is_empty()),
            artist: entry.artist.clone().or_else(|| file.and_then(|file| file.artist.clone())),
            album_line,
        }
    }

    fn section_title(&self, section: usize) -> String {
        let file = self.document.as_ref().map(|document| &document.file);
        let found = file.and_then(|file| file.sections.get(section));
        match found {
            Some(found) => found.title.clone().filter(|title| !title.is_empty()).unwrap_or_else(|| match (found.season, found.disc) {
                (Some(season), _) => format!("Season {season}"),
                (_, Some(disc)) => format!("Disc {disc}"),
                _ => format!("Section {}", section + 1),
            }),
            None => format!("Section {}", section + 1),
        }
    }

    fn publish(&mut self, player: &Player) {
        let current = self.current();
        let next = match current {
            Some(current) if current + 1 < self.play.len() => Some(current + 1),
            // Without shuffle, repeating starts over at the top. With it, the next pass gets a new order.
            Some(_) if self.modes.repeat != Repeat::Off && !self.modes.shuffle && self.play.len() > 1 => Some(0),
            _ => None,
        };
        let kind = self.kind();
        let wide = self.metas.values().any(|meta| meta.entry.thumbnail.is_some());
        let mut rows = Vec::new();
        let mut items = Vec::new();
        let mut top = LIST_PADDING;
        let mut previous_section: Option<Option<usize>> = None;
        for (list_index, id) in self.list.iter().enumerate() {
            let Some(index) = self.play_index(*id) else { continue };
            let entry = &self.play[index];
            let Some(meta) = self.metas.get(id) else { continue };
            if previous_section != Some(meta.section) {
                previous_section = Some(meta.section);
                if let Some(section) = meta.section {
                    let count = self.list[list_index..]
                        .iter()
                        .take_while(|other| self.metas.get(other).is_some_and(|other| other.section == Some(section)))
                        .count();
                    let collapsed = self.collapsed.contains(&section);
                    rows.push(Row { kind: RowKind::Heading, top, height: HEADING_HEIGHT, list_at: list_index, section: Some(section) });
                    items.push(PlaylistItem {
                        heading: true,
                        heading_title: self.section_title(section).to_uppercase().into(),
                        heading_count: count.to_string().into(),
                        collapsed,
                        entry: -1,
                        top,
                        ..PlaylistItem::default()
                    });
                    top += HEADING_HEIGHT;
                }
            }
            if meta.section.is_some_and(|section| self.collapsed.contains(&section)) {
                continue;
            }
            let is_audio = media::is_audio_name(&entry.filename) || kind == Kind::Music;
            let (cover_state, cover) = self.row_cover(entry, meta, is_audio);
            let number = match kind {
                Kind::Series => meta.entry.episode.map(|episode| episode.to_string()),
                Kind::Music => meta.entry.track.map(|track| track.to_string()),
                Kind::Other => None,
            };
            let artist = meta
                .entry
                .artist
                .clone()
                .or_else(|| entry.summary.as_ref().and_then(|summary| summary.artist.clone()))
                .unwrap_or_default();
            let detail = match kind {
                Kind::Series => self.episode_label(meta).unwrap_or_default(),
                _ if is_audio => artist,
                _ => String::new(),
            };
            let progress = meta
                .duration()
                .and_then(|duration| resume::saved_start(&entry.filename).map(|start| (start / duration) as f32))
                .unwrap_or(0.0);
            let problem = match meta.problem {
                Problem::Missing => "File not found",
                Problem::Changed => "File changed since saved",
                Problem::None => "",
            };
            rows.push(Row { kind: RowKind::Entry(list_index), top, height: ROW_HEIGHT, list_at: list_index, section: meta.section });
            items.push(PlaylistItem {
                heading: false,
                entry: list_index as i32,
                top,
                number: number.unwrap_or_else(|| (list_index + 1).to_string()).into(),
                title: self.title_of(entry, meta).into(),
                detail: detail.into(),
                duration: meta.duration().map(playlist_file::time_text).unwrap_or_default().into(),
                is_audio,
                wide: wide && !is_audio,
                cover_state,
                cover,
                progress: progress.clamp(0.0, 1.0),
                problem: problem.into(),
                problem_warning: meta.problem == Problem::Changed,
                current: entry.current,
                failed: opener::has_failed(&entry.filename),
                played: self.modes.shuffle && current.is_some_and(|current| index < current),
                next: next == Some(index),
                ..PlaylistItem::default()
            });
            top += ROW_HEIGHT;
        }
        self.rows = rows;

        let next_entry = next.and_then(|index| self.play.get(index));
        let next_title = next_entry
            .map(|entry| match self.metas.get(&entry.id) {
                Some(meta) => self.title_of(entry, meta),
                None => entry.title.clone(),
            })
            .unwrap_or_default();
        player.set_next_title(next_title.into());
        let next_cover = next_entry.and_then(|entry| audio_info::get(&entry.filename)).and_then(|info| info.cover.clone());
        player.set_has_next_cover(next_cover.is_some());
        if let Some(cover) = next_cover {
            player.set_next_cover(slint::Image::from_rgba8(cover));
        }
        let track = current.and_then(|current| self.list.iter().position(|id| *id == self.play[current].id));
        player.set_track_number(track.map_or(0, |track| track as i32 + 1));
        player.set_playlist_covers(wide || kind == Kind::Music || self.play.iter().any(|entry| media::is_audio_name(&entry.filename)));
        player.set_playlist(ModelRc::new(VecModel::from(items)));
        player.set_playlist_count(self.list.len() as i32);

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

    // The entry's own thumbnail wins, then the file's embedded cover for audio.
    fn row_cover(&self, entry: &MpvEntry, meta: &Meta, is_audio: bool) -> (CoverState, slint::Image) {
        if let Some(thumbnail) = &meta.entry.thumbnail {
            let size = if is_audio { Size::Square } else { Size::Wide };
            return match images::get(thumbnail, size) {
                images::State::Ready(image) => (CoverState::Shown, slint::Image::from_rgba8(image)),
                images::State::Loading => (CoverState::Loading, slint::Image::default()),
                images::State::Failed => (CoverState::Missing, slint::Image::default()),
            };
        }
        let thumb = entry.summary.as_ref().and_then(|summary| summary.thumb.clone());
        // Still being read when a readable file has no summary yet.
        let state = match (&entry.summary, &thumb) {
            (_, Some(_)) => CoverState::Shown,
            (None, None) if audio_info::can_read(&entry.filename) => CoverState::Loading,
            _ => CoverState::Missing,
        };
        (state, thumb.map(slint::Image::from_rgba8).unwrap_or_default())
    }

    fn refresh(&mut self) {
        if let Some(app) = self.app.upgrade() {
            self.publish(&app.global::<Player>());
            self.show_document();
        }
    }

    /// The panel's header and card.
    fn show_document(&self) {
        let Some(app) = self.app.upgrade() else { return };
        let doc = app.global::<PlaylistDoc>();
        let Some(document) = &self.document else {
            doc.set_has_document(false);
            doc.set_title(SharedString::new());
            doc.set_dirty(false);
            return;
        };
        let file = &document.file;
        doc.set_has_document(true);
        let series_name = file.series.as_ref().and_then(|series| series.name.clone());
        let title = file.title.clone().or_else(|| series_name.clone()).unwrap_or_else(|| match &document.origin {
            Origin::File(path) => path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default(),
            _ => "Playlist".into(),
        });
        doc.set_title(title.as_str().into());
        doc.set_card_title(series_name.filter(|name| *name != title).unwrap_or(title).into());
        let sections = self.document.as_ref().map_or(0, |document| document.file.sections.len());
        let mut parts: Vec<String> = Vec::new();
        match file.kind {
            Kind::Series => {
                parts.push("Series".into());
                if let Some(year) = file.series.as_ref().and_then(|series| series.year).or(file.year) {
                    parts.push(year.to_string());
                }
                if sections > 1 {
                    parts.push(format!("{sections} seasons"));
                }
            }
            Kind::Music => {
                parts.extend(file.artist.clone());
                parts.extend(file.year.map(|year| year.to_string()));
                if sections > 1 {
                    parts.push(format!("{sections} discs"));
                }
            }
            Kind::Other => parts.extend(file.description.clone().map(|text| text.lines().next().unwrap_or("").to_string())),
        }
        doc.set_card_subtitle(parts.join(" · ").into());
        let origin_text = match &document.origin {
            Origin::File(path) => path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
            Origin::Link(url) => media::link_host(url),
            Origin::Text => "Not saved yet".into(),
        };
        doc.set_file_name(origin_text.into());
        doc.set_dirty(document.dirty);
        doc.set_can_save_in_place(can_save_in_place(document));
        doc.set_autosave(document.file.playback.autosave);
        doc.set_square_cover(file.kind == Kind::Music);
        let size = if file.kind == Kind::Music { Size::Card } else { Size::Poster };
        match file.cover.as_deref().and_then(|cover| images::ready(cover, size)) {
            Some(image) => {
                doc.set_has_cover(true);
                doc.set_cover(slint::Image::from_rgba8(image));
            }
            None => doc.set_has_cover(false),
        }
    }

    pub fn mark_dirty(&mut self) {
        let Some(document) = self.document.as_mut() else { return };
        document.dirty = true;
        if document.file.playback.autosave && can_save_in_place(document) {
            self.autosave_timer.start(TimerMode::SingleShot, AUTOSAVE_DELAY, || {
                with_playlist(|playlist| {
                    if let Err(err) = playlist.save(None) {
                        warn!("autosave failed: {err}");
                    }
                });
            });
        }
        self.show_document();
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
        info!("playing playlist entry {index} ({})", self.play.get(target).map_or("", |entry| entry.filename.as_str()));
        log_error(self.mpv.command(&["playlist-play-index", &target.to_string()]));
    }

    fn play_entry_next(&mut self, index: i32) {
        let (Some(id), Some(current)) = (self.list_id(index), self.current()) else { return };
        let Some(from) = self.play_index(id).filter(|from| *from != current) else { return };
        info!("playing playlist entry {index} next");
        if self.modes.shuffle {
            self.move_in_mpv(from, current + 1);
        } else {
            let current_id = self.play[current].id;
            let after_current = self.list.iter().position(|id| *id == current_id).map_or(0, |position| position + 1);
            let section = self.metas.get(&current_id).and_then(|meta| meta.section);
            self.move_entry(index, after_current as i32, Some(section));
            return;
        }
        self.refresh();
    }

    // Moves within the list. mpv follows only while the play order is the list order. `section` is the section
    // the entry joins, when the move changes it.
    fn move_entry(&mut self, from: i32, to: i32, section: Option<Option<usize>>) {
        let (Ok(from), Ok(to)) = (usize::try_from(from), usize::try_from(to)) else { return };
        if from >= self.list.len() || to > self.list.len() {
            return;
        }
        let id = self.list[from];
        let changes_section = section.is_some_and(|section| self.metas.get(&id).is_some_and(|meta| meta.section != section));
        if (to == from || to == from + 1) && !changes_section {
            return;
        }
        debug!("moving playlist entry {from} to {to}");
        if !self.modes.shuffle && to != from && to != from + 1 {
            let target = match self.list.get(to) {
                Some(before) => self.play_index(*before),
                None => Some(self.play.len()),
            };
            if let (Some(index), Some(target)) = (self.play_index(id), target) {
                self.move_in_mpv(index, target);
            }
        }
        if to != from && to != from + 1 {
            self.list.remove(from);
            self.list.insert(if to > from { to - 1 } else { to }, id);
        }
        if let (Some(section), Some(meta)) = (section, self.metas.get_mut(&id))
            && !meta.included
        {
            meta.section = section;
        }
        self.mark_dirty();
        self.refresh();
    }

    // A drop before `to` lands at that row's place in the list and joins the section of the row above it.
    fn move_row(&mut self, from: i32, to: i32) {
        let (Ok(from), Ok(to)) = (usize::try_from(from), usize::try_from(to)) else { return };
        let Some(RowKind::Entry(entry)) = self.rows.get(from).map(|row| row.kind) else { return };
        let list_to = self.rows.get(to).map_or(self.list.len(), |row| row.list_at);
        let above = (0..to).rev().find(|index| *index != from).and_then(|index| self.rows.get(index));
        let section = above.and_then(|row| row.section);
        self.move_entry(entry as i32, list_to as i32, Some(section));
    }

    fn row_at(&self, y: f32) -> i32 {
        self.rows.iter().position(|row| y < row.top + row.height / 2.0).unwrap_or(self.rows.len()) as i32
    }

    // The next row in that direction that is an entry, for moving the selection with the arrow keys.
    fn step_row(&self, row: i32, step: i32) -> i32 {
        let mut index = row;
        loop {
            index += step;
            match usize::try_from(index).ok().and_then(|index| self.rows.get(index)) {
                Some(found) if matches!(found.kind, RowKind::Entry(_)) => return index,
                Some(_) => {}
                None => return row,
            }
        }
    }

    fn toggle_section(&mut self, row: i32) {
        let Some(section) = usize::try_from(row).ok().and_then(|row| self.rows.get(row)).and_then(|row| row.section) else {
            return;
        };
        if !self.collapsed.remove(&section) {
            self.collapsed.insert(section);
        }
        self.refresh();
    }

    fn remove_entry(&mut self, index: i32) {
        let Ok(list_index) = usize::try_from(index) else { return };
        let Some(id) = self.list.get(list_index).copied() else { return };
        let Some(play_index) = self.play_index(id) else { return };
        let played = self.current().is_some_and(|current| play_index <= current);
        log_error(self.mpv.command(&["playlist-remove", &play_index.to_string()]));
        let entry = self.play.remove(play_index);
        let meta = self.metas.remove(&id);
        info!("removed playlist entry {list_index} ({})", entry.filename);
        self.list.remove(list_index);
        let title = meta.as_ref().map_or_else(|| entry.title.clone(), |meta| self.title_of(&entry, meta));
        if let Some(app) = self.app.upgrade() {
            app.global::<Player>().set_undo_text(format!("Removed {title}").into());
        }
        let app_weak = self.app.clone();
        self.undo_timer.start(TimerMode::SingleShot, UNDO_SHOWN_FOR, move || {
            if let Some(app) = app_weak.upgrade() {
                app.global::<Player>().set_undo_text(SharedString::new());
            }
        });
        self.removed.push(Removed { filename: entry.filename, list_index, played, meta });
        self.mark_dirty();
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
        info!("putting back {} at {index}", removed.filename);
        log_error(self.mpv.command(&["loadfile", &removed.filename, "insert-at", &index.to_string()]));
        self.restoring.push((removed.filename, removed.list_index, removed.meta));
        self.mark_dirty();
    }

    // Puts `filename` where the entry `id` is, in mpv and in the list, and drops the old one.
    fn replace(&mut self, id: i64, filename: String, meta: Meta, play: bool) {
        let (Some(play_index), Some(list_index)) = (self.play_index(id), self.list.iter().position(|other| *other == id))
        else {
            return;
        };
        let how = if play { "insert-at-play" } else { "insert-at" };
        log_error(self.mpv.command(&["loadfile", &filename, how, &play_index.to_string()]));
        log_error(self.mpv.command(&["playlist-remove", &(play_index + 1).to_string()]));
        self.play.remove(play_index);
        self.list.remove(list_index);
        self.metas.remove(&id);
        self.restoring.push((filename, list_index, Some(meta)));
    }

    fn try_mirror(&mut self, filename: &str) -> bool {
        let Some(entry) = self.play.iter().find(|entry| entry.filename == filename) else { return false };
        let id = entry.id;
        let Some(meta) = self.metas.get(&id) else { return false };
        let Some(mirror) = meta.entry.mirrors.get(meta.mirrors_tried).cloned() else { return false };
        let mut meta = meta.clone();
        meta.mirrors_tried += 1;
        info!("{filename} failed, trying mirror {} of {}: {mirror}", meta.mirrors_tried, meta.entry.mirrors.len());
        let target = if mirror.contains("://") { http_cache::wrap_url(&mirror) } else { mirror };
        add_headers(&target, &meta.headers);
        self.replace(id, target, meta, true);
        true
    }

    /// Points a missing entry at `path`. Returns the folder it was in before, to look for others there.
    pub fn relink(&mut self, list_index: usize, path: &Path) -> Option<PathBuf> {
        let id = *self.list.get(list_index)?;
        let old_folder = Path::new(&self.metas.get(&id)?.entry.source).parent().map(Path::to_path_buf);
        self.change_source(list_index, path.to_string_lossy().into_owned());
        old_folder
    }

    /// Gives an entry a new file or link, replacing it in mpv.
    pub fn change_source(&mut self, list_index: usize, source: String) {
        let Some(id) = self.list.get(list_index).copied() else { return };
        let Some(mut meta) = self.metas.get(&id).cloned() else { return };
        info!("playlist entry {list_index} now points at {source}");
        meta.entry.source = source.clone();
        meta.entry.check = None;
        meta.mirrors_tried = 0;
        meta.problem = if source.contains("://") || Path::new(&source).exists() { Problem::None } else { Problem::Missing };
        let filename = if source.contains("://") { http_cache::wrap_url(&source) } else { source };
        add_headers(&filename, &meta.headers);
        self.replace(id, filename, meta, false);
        self.mark_dirty();
    }

    /// The sections were rearranged in the details dialog. `origins` holds each new section's old index.
    pub fn remap_sections(&mut self, origins: &[Option<usize>]) {
        let new_index = |old: Option<usize>| old.and_then(|old| origins.iter().position(|origin| *origin == Some(old)));
        for meta in self.metas.values_mut() {
            meta.section = new_index(meta.section);
        }
        for (_, meta) in &mut self.pending {
            meta.section = new_index(meta.section);
        }
        if let Some(document) = self.document.as_mut() {
            for item in &mut document.disabled {
                item.section = new_index(item.section);
            }
        }
        self.collapsed.clear();
    }

    /// Missing entries that were in `old_folder`, with the file of the same name in `new_folder` if it exists.
    pub fn missing_in(&self, old_folder: &Path, new_folder: &Path) -> Vec<(usize, PathBuf)> {
        self.list
            .iter()
            .enumerate()
            .filter_map(|(list_index, id)| {
                let meta = self.metas.get(id).filter(|meta| meta.problem == Problem::Missing)?;
                let source = Path::new(&meta.entry.source);
                (source.parent()? == old_folder).then_some(())?;
                let candidate = new_folder.join(source.file_name()?);
                candidate.is_file().then_some((list_index, candidate))
            })
            .collect()
    }

    fn toggle_shuffle(&mut self) {
        self.modes.shuffle = !self.modes.shuffle;
        info!("shuffle {}", if self.modes.shuffle { "on" } else { "off" });
        self.save_modes();
        self.show_modes();
        let order = if self.modes.shuffle { self.shuffled_order() } else { self.list.clone() };
        self.arrange_mpv(&order);
        self.last_current = self.current();
        self.refresh();
    }

    fn cycle_repeat(&mut self) {
        self.modes.repeat = self.modes.repeat.next();
        info!("repeat {:?}", self.modes.repeat);
        self.save_modes();
        self.show_modes();
        self.send_repeat();
        self.refresh();
    }

    pub fn document(&self) -> Option<&Document> {
        self.document.as_ref()
    }

    /// The open list's own document, made for a list that was opened without one.
    pub fn document_mut(&mut self) -> &mut Document {
        let audio = self.audio;
        self.document.get_or_insert_with(|| Document {
            file: PlaylistFile {
                version: playlist_file::VERSION,
                kind: if audio { Kind::Music } else { Kind::Other },
                ..PlaylistFile::default()
            },
            origin: Origin::Text,
            disabled: Vec::new(),
            dirty: false,
        })
    }

    pub fn set_file(&mut self, file: PlaylistFile) {
        self.document_mut().file = file;
        self.mark_dirty();
        self.refresh();
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn meta_at(&self, list_index: usize) -> Option<&Meta> {
        self.metas.get(self.list.get(list_index)?)
    }

    pub fn set_meta(&mut self, list_index: usize, entry: Entry, section: Option<usize>) {
        let Some(id) = self.list.get(list_index).copied() else { return };
        let Some(meta) = self.metas.get_mut(&id) else { return };
        if let Some(thumbnail) = &entry.thumbnail {
            images::forget(thumbnail);
        }
        meta.entry = entry;
        if !meta.included {
            meta.section = section;
        }
        self.mark_dirty();
        self.refresh();
    }

    /// Entries in `section` other than the one at `list_index`, for copying skip ranges to a whole season.
    pub fn same_section(&self, list_index: usize) -> Vec<usize> {
        let Some(section) = self.meta_at(list_index).map(|meta| meta.section) else { return Vec::new() };
        (0..self.list.len()).filter(|index| *index != list_index && self.meta_at(*index).is_some_and(|meta| meta.section == section)).collect()
    }

    pub fn copy_skips(&mut self, list_index: usize) {
        let Some(skip) = self.meta_at(list_index).map(|meta| meta.entry.skip.clone()) else { return };
        for index in self.same_section(list_index) {
            if let Some(meta) = self.list.get(index).and_then(|id| self.metas.get_mut(id))
                && !meta.included
            {
                meta.entry.skip = skip.clone();
            }
        }
        self.mark_dirty();
    }

    fn save(&mut self, target: Option<PathBuf>) -> Result<PathBuf, String> {
        let target = match target {
            Some(target) => target,
            None => match self.document.as_ref().map(|document| &document.origin) {
                Some(Origin::File(path)) => path.clone(),
                _ => return Err("This playlist has no file yet.".into()),
            },
        };
        let folder = target.parent().map(Path::to_path_buf).unwrap_or_default();
        let file = self.build_file();
        let text = playlist_file::to_json(&file, Some(&folder));
        let temporary = target.with_extension("rl.tmp");
        std::fs::write(&temporary, text)
            .and_then(|()| std::fs::rename(&temporary, &target))
            .map_err(|err| format!("Couldn't save {}: {err}", target.display()))?;
        info!("saved the playlist to {}", target.display());
        for meta in self.metas.values_mut() {
            if meta.problem == Problem::Changed {
                meta.problem = Problem::None;
            }
        }
        let document = self.document_mut();
        document.file = PlaylistFile { entries: Vec::new(), ..file };
        for section in &mut document.file.sections {
            section.entries.clear();
        }
        document.origin = Origin::File(target.clone());
        document.dirty = false;
        self.autosave_timer.stop();
        self.refresh();
        Ok(target)
    }

    // The document's fields with the entries in list order, each in its section, and disabled ones back in place.
    fn build_file(&mut self) -> PlaylistFile {
        let now = playlist_file::now_text();
        let document = self.document_mut();
        let mut file = document.file.clone();
        let disabled = document.disabled.clone();
        file.version = file.version.max(playlist_file::VERSION);
        file.created.get_or_insert_with(|| now.clone());
        file.updated = Some(now);
        file.entries.clear();
        for section in &mut file.sections {
            section.entries.clear();
        }
        for id in &self.list {
            let Some(meta) = self.metas.get(id) else { continue };
            if meta.included {
                continue;
            }
            let mut entry = meta.entry.clone();
            if entry.duration.is_none() {
                entry.duration = meta.seen_duration.map(|duration| duration.round());
            }
            if !entry.source.contains("://") {
                entry.check = playlist_file::file_check(&entry.source);
            }
            match meta.section.and_then(|section| file.sections.get_mut(section)) {
                Some(section) => section.entries.push(entry),
                None => file.entries.push(entry),
            }
        }
        for item in disabled {
            match item.section.and_then(|section| file.sections.get_mut(section)) {
                Some(section) if section.include.is_none() => section.entries.push(item.entry),
                Some(_) => {}
                None => file.entries.push(item.entry),
            }
        }
        file
    }

    pub fn section_names(&self) -> Vec<String> {
        let count = self.document.as_ref().map_or(0, |document| document.file.sections.len());
        (0..count).map(|section| self.section_title(section)).collect()
    }
}

fn source_of(filename: &str) -> String {
    filename
        .strip_prefix(http_cache::PROTOCOL)
        .and_then(|rest| rest.strip_prefix("://"))
        .unwrap_or(filename)
        .to_string()
}

/// Registers the headers a playlist gives a link, before mpv opens it.
pub fn add_headers(filename: &str, headers: &BTreeMap<String, String>) {
    if headers.is_empty() {
        return;
    }
    let url = source_of(filename);
    http_cache::add_headers(url, headers.iter().map(|(name, value)| (name.clone(), value.clone())).collect());
}
