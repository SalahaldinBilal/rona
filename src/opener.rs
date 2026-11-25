use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use slint::winit_030::winit::event::WindowEvent;
use slint::{ComponentHandle, SharedString, Timer, TimerMode};

use crate::http_cache::{self, ConnectionState};
use crate::logging::{debug, info, warn};
use crate::media::{self, FileCheck, LinkCheck};
use crate::mpv::Mpv;
use crate::playlist::{Document, Meta, Opened, Problem};
use crate::playlist_file;
use crate::{
    AppWindow, DragState, FailureKind, Osd, Panels, Phase, Player, forget_audio_view, log_error, playlist, playlist_edit,
};

#[derive(Clone, Debug)]
enum Source {
    File(PathBuf),
    Link(String),
    Playlist(PlaylistSource),
}

#[derive(Clone, Debug)]
enum PlaylistSource {
    File(PathBuf),
    Link(String),
    // A whole playlist given as text, with its paths already absolute.
    Text(String),
}

fn file_source(path: PathBuf) -> Source {
    if playlist_file::is_playlist_name(&path.to_string_lossy()) {
        Source::Playlist(PlaylistSource::File(path))
    } else {
        Source::File(path)
    }
}

fn link_source(url: String) -> Source {
    if playlist_file::is_playlist_name(&url) { Source::Playlist(PlaylistSource::Link(url)) } else { Source::Link(url) }
}

// The file name mpv gets for a source.
fn mpv_name(source: &Source) -> String {
    match source {
        Source::File(path) => std::path::absolute(path).unwrap_or_else(|_| path.clone()).to_string_lossy().into_owned(),
        Source::Link(url) => http_cache::wrap_url(url),
        Source::Playlist(_) => String::new(),
    }
}

fn mpv_name_of(source: &str) -> String {
    if source.contains("://") { http_cache::wrap_url(source) } else { source.to_string() }
}

struct PlaylistRead {
    opened: Opened,
    start: usize,
    // Shown once the list opens, like local files left out of a playlist from the web.
    notes: Vec<String>,
}

// Blocks on disk and network, so it runs on its own thread. A single playlist keeps its file, anything mixed
// with it opens as a plain list.
fn read_playlists(sources: Vec<Source>) -> Result<PlaylistRead, String> {
    let single = sources.len() == 1;
    let mut metas = Vec::new();
    let mut notes = Vec::new();
    let mut document = None;
    let mut start = 0;
    let mut plain = Vec::new();
    let flush_plain = |plain: &mut Vec<Source>, metas: &mut Vec<(String, Meta)>| {
        for checked in check_sources(std::mem::take(plain)) {
            if let Some(source) = checked.playable() {
                let name = mpv_name(&source);
                let meta = Meta::plain(match &source {
                    Source::Link(url) => url.clone(),
                    _ => name.clone(),
                });
                metas.push((name, meta));
            }
        }
    };
    for source in sources {
        let Source::Playlist(playlist) = source else {
            plain.push(source);
            continue;
        };
        flush_plain(&mut plain, &mut metas);
        let loaded = match &playlist {
            PlaylistSource::File(path) => playlist_file::read_path(path),
            PlaylistSource::Link(url) => playlist_file::read_link(url),
            PlaylistSource::Text(text) => {
                playlist_file::read_text(text, &std::env::current_dir().unwrap_or_default())
            }
        }?;
        for warning in &loaded.warnings {
            warn!("{warning}");
        }
        notes.extend(loaded.warnings.iter().cloned());
        if loaded.skipped_local > 0 {
            notes.push(format!("Skipped {} local files in a playlist from the web", loaded.skipped_local));
        }
        let (enabled, disabled): (Vec<_>, Vec<_>) = loaded.items.into_iter().partition(|item| !item.entry.disabled);
        for item in enabled {
            let problem = problem_of(&item.entry);
            let name = mpv_name_of(&item.entry.source);
            metas.push((name, Meta::from_item(item, problem)));
        }
        if single {
            start = loaded.file.playback.start.unwrap_or(0);
            document = Some(Document { file: loaded.file, origin: loaded.origin, disabled, dirty: false });
        }
    }
    flush_plain(&mut plain, &mut metas);
    Ok(PlaylistRead { opened: Opened { document, metas }, start, notes })
}

fn problem_of(entry: &playlist_file::Entry) -> Problem {
    if entry.source.contains("://") {
        return Problem::None;
    }
    if !Path::new(&entry.source).exists() {
        return Problem::Missing;
    }
    match (entry.check, playlist_file::file_check(&entry.source)) {
        (Some(saved), Some(now)) if saved != now => Problem::Changed,
        _ => Problem::None,
    }
}

enum Checked {
    File(PathBuf, FileCheck),
    Link(String, LinkCheck),
}

impl Checked {
    fn playable(&self) -> Option<Source> {
        match self {
            Checked::File(path, FileCheck::Playable) => Some(Source::File(path.clone())),
            Checked::Link(url, LinkCheck::Playable) => Some(Source::Link(url.clone())),
            _ => None,
        }
    }
}

enum Pending<'scope> {
    Ready(Source),
    Checking(std::thread::ScopedJoinHandle<'scope, Checked>),
}

// Link checks wait on the network, so each gets its own thread while files are checked in order.
fn check_sources(sources: Vec<Source>) -> Vec<Checked> {
    std::thread::scope(|scope| {
        let pending: Vec<Pending> = sources
            .into_iter()
            .map(|source| match source {
                Source::Link(url) if !media::link_has_media_extension(&url) => Pending::Checking(scope.spawn(move || {
                    let result = timed_link_check(&url);
                    Checked::Link(url, result)
                })),
                source => Pending::Ready(source),
            })
            .collect();
        pending
            .into_iter()
            .map(|pending| match pending {
                Pending::Ready(Source::File(path)) => {
                    let check = media::check_file(&path);
                    if !matches!(check, FileCheck::Playable) {
                        info!("{}: {}", path.display(), file_check_text(&check));
                    }
                    Checked::File(path, check)
                }
                Pending::Ready(Source::Link(url)) => Checked::Link(url, LinkCheck::Playable),
                // Playlists are read before the media checks, so one here was given where only media fits.
                Pending::Ready(Source::Playlist(_)) => Checked::File(PathBuf::new(), FileCheck::NotMedia),
                Pending::Checking(check) => check.join().expect("the link check panicked"),
            })
            .collect()
    })
}

fn timed_link_check(url: &str) -> LinkCheck {
    info!("checking {url}");
    let started = std::time::Instant::now();
    let result = media::check_link(url);
    let outcome = match &result {
        LinkCheck::Playable => "playable".to_string(),
        LinkCheck::NotPlayable(detail) => format!("not playable: {detail}"),
        LinkCheck::Failed(detail) => format!("failed: {detail}"),
    };
    info!("link check took {} ms, {outcome}", started.elapsed().as_millis());
    result
}

fn file_check_text(check: &FileCheck) -> String {
    match check {
        FileCheck::Playable => "playable".into(),
        FileCheck::NotFound => "not found".into(),
        FileCheck::Unreadable(reason) => format!("unreadable: {reason}"),
        FileCheck::NotMedia => "not a video or audio file".into(),
    }
}

static FAILED: Mutex<Vec<String>> = Mutex::new(Vec::new());
// The link box was opened by "Add link", so its link joins the open list.
static ADDING_LINK: Mutex<bool> = Mutex::new(false);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Playlist entries mpv couldn't open, so the playlist can mark them.
pub fn has_failed(path: &str) -> bool {
    lock(&FAILED).iter().any(|failed| failed == path)
}

// mpv disables video for a file loaded before the render context exists, so loads wait for it.
#[derive(Clone, Default)]
pub struct Loader(Rc<RefCell<LoaderState>>);

#[derive(Default)]
struct LoaderState {
    ready: bool,
    pending: Vec<String>,
    start: usize,
}

impl Loader {
    pub fn render_ready(&self, mpv: &Mpv) {
        let (pending, start) = {
            let mut state = self.0.borrow_mut();
            state.ready = true;
            (std::mem::take(&mut state.pending), state.start)
        };
        if !pending.is_empty() {
            debug!("video is ready, loading what was waiting for it");
        }
        send_loadfile(mpv, &pending, start);
    }

    fn load(&self, mpv: &Mpv, paths: Vec<String>, start: usize) {
        let mut state = self.0.borrow_mut();
        if state.ready {
            drop(state);
            send_loadfile(mpv, &paths, start);
        } else {
            debug!("holding {} files until video is ready", paths.len());
            state.pending = paths;
            state.start = start;
        }
    }
}

// The entry to start at replaces what plays, and the others go around it, so mpv never starts the first one.
fn send_loadfile(mpv: &Mpv, paths: &[String], start: usize) {
    let Some(first) = paths.get(start).or(paths.first()) else { return };
    let start = if start < paths.len() { start } else { 0 };
    info!("loading {} files, starting at {} ({first})", paths.len(), start + 1);
    log_error(mpv.command(&["loadfile", first, "replace"]));
    for (index, path) in paths.iter().enumerate().take(start) {
        log_error(mpv.command(&["loadfile", path, "insert-at", &index.to_string()]));
    }
    for path in paths.iter().skip(start + 1) {
        log_error(mpv.command(&["loadfile", path, "append"]));
    }
}

struct Opener {
    app: slint::Weak<AppWindow>,
    mpv: Mpv,
    osd: Osd,
    loader: Loader,
    // Bumped on every new request, so results of older checks are dropped.
    generation: u64,
    last_request: Vec<Source>,
    restore_start: bool,
    // An open held back while the user decides whether to save the edited playlist it would replace.
    held: Option<(Vec<Source>, bool, usize)>,
}

thread_local! {
    static OPENER: RefCell<Option<Rc<RefCell<Opener>>>> = const { RefCell::new(None) };
}

fn with_opener(action: impl FnOnce(&mut Opener)) {
    let opener = OPENER.with(|slot| slot.borrow().clone());
    if let Some(opener) = opener {
        action(&mut opener.borrow_mut());
    }
}

pub fn wire(app: &AppWindow, mpv: &Mpv, osd: &Osd, loader: Loader) {
    let opener = Opener {
        app: app.as_weak(),
        mpv: mpv.clone(),
        osd: osd.clone(),
        loader,
        generation: 0,
        last_request: Vec::new(),
        restore_start: false,
        held: None,
    };
    OPENER.with(|slot| *slot.borrow_mut() = Some(Rc::new(RefCell::new(opener))));

    let player = app.global::<Player>();
    player.on_open_file(|| with_opener(Opener::pick_files));
    player.on_show_link_box(|| {
        *lock(&ADDING_LINK) = false;
        with_opener(Opener::show_link_box);
    });
    player.on_paste_link(|| with_opener(Opener::paste));
    player.on_open_link(|text| {
        let mut error = String::new();
        with_opener(|opener| error = opener.open_link(&text));
        SharedString::from(error)
    });
    player.on_cancel_loading(|| with_opener(Opener::cancel));
    player.on_retry_loading(|| with_opener(Opener::retry));
    player.on_close_file(|| with_opener(Opener::close));
    player.on_replay(|| with_opener(Opener::replay));
    player.on_retry_connection(|| with_opener(Opener::retry_connection));
}

fn is_link_argument(argument: &str) -> bool {
    argument.contains("://") && !playlist_file::is_playlist_text(argument)
}

/// Another launch hands its arguments over from its own working folder, so relative paths are resolved first,
/// also the ones inside a playlist given as text.
pub fn absolute_argument(argument: String) -> String {
    if playlist_file::is_playlist_text(&argument) {
        return playlist_file::absolutize_text(&argument, &std::env::current_dir().unwrap_or_default());
    }
    if is_link_argument(&argument) {
        return argument;
    }
    match std::path::absolute(&argument) {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(_) => argument,
    }
}

// Explorer can start one process per selected file, so arguments that arrive close together open as one playlist.
const ARGUMENT_BURST: Duration = Duration::from_millis(150);

thread_local! {
    static QUEUED_ARGUMENTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static ARGUMENT_TIMER: Timer = Timer::default();
}

pub fn queue_arguments(arguments: Vec<String>) {
    debug!("queued arguments {arguments:?}");
    QUEUED_ARGUMENTS.with(|queued| queued.borrow_mut().extend(arguments));
    ARGUMENT_TIMER.with(|timer| {
        timer.start(TimerMode::SingleShot, ARGUMENT_BURST, || {
            let arguments = QUEUED_ARGUMENTS.with(|queued| std::mem::take(&mut *queued.borrow_mut()));
            open_arguments(&arguments);
        });
    });
}

// A bad link only gets the failure screen when nothing else was given.
pub fn open_arguments(arguments: &[String]) {
    with_opener(|opener| {
        let mut sources = Vec::new();
        let mut bad_link = None;
        let mut skipped = 0;
        for argument in arguments {
            if playlist_file::is_playlist_text(argument) {
                sources.push(Source::Playlist(PlaylistSource::Text(argument.clone())));
                continue;
            }
            if !is_link_argument(argument) {
                sources.push(file_source(PathBuf::from(argument)));
                continue;
            }
            match media::parse_link(argument) {
                Ok(url) => sources.push(link_source(url)),
                Err(err) => {
                    info!("skipping argument {argument}: {}", err.message());
                    skipped += 1;
                    bad_link.get_or_insert_with(|| (argument.clone(), err.message().to_string()));
                }
            }
        }
        if sources.is_empty() {
            if let Some((argument, detail)) = bad_link {
                opener.fail(FailureKind::NotPlayable, "Can't play this link", &detail, &argument, true);
            }
            return;
        }
        opener.open_counting(sources, false, skipped);
    });
}

/// The save prompt was answered. An open waiting on it goes ahead unless it was cancelled.
pub fn release_held(proceed: bool) {
    with_opener(|opener| opener.release_held(proceed));
}

pub fn add_files() {
    // The native dialog blocks its thread, which must not be the UI thread that draws the video.
    std::thread::spawn(|| {
        let Some(files) = rfd::FileDialog::new()
            .set_title("Add to the playlist")
            .add_filter("Video and audio", media::dialog_extensions().as_slice())
            .add_filter("All files", &["*"])
            .pick_files()
        else {
            return;
        };
        let sources: Vec<Source> = files.into_iter().map(Source::File).collect();
        let _ = slint::invoke_from_event_loop(move || with_opener(|opener| opener.add(sources)));
    });
}

/// Opens the link box for a link that joins the open list.
pub fn add_link() {
    *lock(&ADDING_LINK) = true;
    with_opener(Opener::show_link_box);
}

/// mpv ended a file with an error. With more entries after it, mpv moves on by itself.
pub fn load_failed(path: &str, has_next: bool) {
    with_opener(|opener| opener.load_failed(path, has_next));
}

pub fn playback_restarted() {
    with_opener(|opener| {
        if std::mem::take(&mut opener.restore_start) {
            log_error(opener.mpv.command(&["set", "start", "none"]));
        }
    });
}

impl Opener {
    fn open(&mut self, sources: Vec<Source>, quiet: bool) {
        self.open_counting(sources, quiet, 0);
    }

    // `skipped` counts sources ruled out before this, so one message covers everything left out.
    fn open_counting(&mut self, sources: Vec<Source>, quiet: bool, skipped: usize) {
        if sources.is_empty() {
            return;
        }
        if playlist::has_unsaved_changes() {
            info!("holding an open request until the edited playlist is saved or let go");
            self.held = Some((sources, quiet, skipped));
            playlist_edit::ask_to_save(playlist_edit::Then::Open);
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        info!("open request {generation}: {sources:?}{}", if quiet { ", quiet" } else { "" });
        self.last_request = sources.clone();
        if sources.iter().any(|source| matches!(source, Source::Playlist(_))) {
            if let Some(Source::Playlist(PlaylistSource::Link(url))) = sources.first() {
                self.show_checking(url);
            }
            std::thread::spawn(move || {
                let read = read_playlists(sources);
                let _ = slint::invoke_from_event_loop(move || {
                    with_opener(|opener| opener.playlists_read(generation, read, quiet));
                });
            });
            return;
        }
        if let [Source::Link(url)] = sources.as_slice() {
            self.show_skipped(skipped, quiet);
            if media::link_has_media_extension(url) {
                self.load(&sources);
            } else {
                self.check_link(url.clone(), generation);
            }
            return;
        }
        let link_to_check = sources.iter().find_map(|source| match source {
            Source::Link(url) if !media::link_has_media_extension(url) => Some(url.clone()),
            _ => None,
        });
        if let Some(url) = link_to_check {
            self.show_checking(&url);
        }
        std::thread::spawn(move || {
            let checks = check_sources(sources);
            let _ = slint::invoke_from_event_loop(move || {
                with_opener(|opener| opener.sources_checked(generation, checks, quiet, skipped));
            });
        });
    }

    fn sources_checked(&mut self, generation: u64, checks: Vec<Checked>, quiet: bool, skipped: usize) {
        if generation != self.generation {
            debug!("dropping the checks of open request {generation}, a newer one replaced it");
            return;
        }
        let playable: Vec<Source> = checks.iter().filter_map(Checked::playable).collect();
        debug!("open request {generation}: {} of {} playable", playable.len(), checks.len());
        if !playable.is_empty() {
            self.show_skipped(skipped + checks.len() - playable.len(), quiet);
            self.load(&playable);
            return;
        }
        if quiet {
            return;
        }
        match checks.into_iter().next() {
            Some(Checked::File(path, check)) => self.file_failed(&path, check),
            Some(Checked::Link(url, result)) => self.link_failed(&url, result),
            None => {}
        }
    }

    fn playlists_read(&mut self, generation: u64, read: Result<PlaylistRead, String>, quiet: bool) {
        if generation != self.generation {
            debug!("dropping the playlist read of open request {generation}, a newer one replaced it");
            return;
        }
        let is_link = matches!(self.last_request.first(), Some(Source::Playlist(PlaylistSource::Link(_))));
        let name = match self.last_request.first() {
            Some(Source::Playlist(PlaylistSource::File(path))) => file_name(path),
            Some(Source::Playlist(PlaylistSource::Link(url))) => media::link_host(url),
            _ => "Playlist".into(),
        };
        match read {
            Err(detail) => {
                if !quiet {
                    self.fail(FailureKind::Unsupported, "Can't open this playlist", &detail, &name, is_link);
                }
            }
            Ok(read) if read.opened.metas.is_empty() => {
                if !quiet {
                    let detail = "There's nothing in it that can be played.";
                    self.fail(FailureKind::Unsupported, "This playlist is empty", detail, &name, is_link);
                }
            }
            Ok(read) => {
                if let Some(note) = read.notes.first() {
                    self.osd.show(note.clone());
                }
                self.load_opened(read.opened, read.start);
            }
        }
    }

    /// Lets go of an open held for the save prompt, opening it unless the user cancelled.
    fn release_held(&mut self, proceed: bool) {
        let Some((sources, quiet, skipped)) = self.held.take() else { return };
        if proceed {
            playlist::forget_changes();
            self.open_counting(sources, quiet, skipped);
        }
    }

    // Adds to the open list instead of replacing it. With nothing open, it opens.
    fn add(&mut self, sources: Vec<Source>) {
        if sources.is_empty() {
            return;
        }
        let Some(app) = self.app.upgrade() else { return };
        if matches!(app.global::<Player>().get_phase(), Phase::Idle | Phase::Failed) || playlist::is_empty() {
            self.open(sources, false);
            return;
        }
        info!("adding to the playlist: {sources:?}");
        std::thread::spawn(move || {
            let checks = check_sources(sources);
            let _ = slint::invoke_from_event_loop(move || {
                with_opener(|opener| {
                    let playable: Vec<Source> = checks.iter().filter_map(Checked::playable).collect();
                    let left_out = checks.len() - playable.len();
                    if left_out > 0 {
                        opener.osd.show(format!("Skipped {left_out} that can't be played"));
                    }
                    for source in &playable {
                        log_error(opener.mpv.command(&["loadfile", &mpv_name(source), "append"]));
                    }
                    if !playable.is_empty() {
                        opener.osd.show(if playable.len() == 1 { "Added 1 entry".into() } else { format!("Added {} entries", playable.len()) });
                    }
                });
            });
        });
    }

    fn show_skipped(&self, skipped: usize, quiet: bool) {
        if skipped > 0 && !quiet {
            self.osd.show(format!("Skipped {skipped} that can't be played"));
        }
    }

    fn file_failed(&self, path: &Path, check: FileCheck) {
        let name = file_name(path);
        match check {
            FileCheck::NotFound => {
                self.fail(FailureKind::NotFound, "File not found", &path.display().to_string(), &name, false)
            }
            FileCheck::Unreadable(reason) => {
                self.fail(FailureKind::NotFound, "Can't read this file", &reason, &name, false)
            }
            FileCheck::NotMedia => {
                let detail = format!("{name} isn't a video or audio file.");
                self.fail(FailureKind::Unsupported, "Can't play this file", &detail, &name, false)
            }
            FileCheck::Playable => {}
        }
    }

    fn show_checking(&self, url: &str) {
        let Some(app) = self.app.upgrade() else { return };
        let player = app.global::<Player>();
        if player.get_phase() == Phase::Playing {
            self.osd.show("Checking link");
            return;
        }
        if player.get_phase() == Phase::Opening {
            log_error(self.mpv.command(&["stop"]));
        }
        player.set_place(media::without_scheme(url).into());
        player.set_media_title(media::link_host(url).into());
        player.set_phase(Phase::Checking);
    }

    fn check_link(&mut self, url: String, generation: u64) {
        self.show_checking(&url);
        std::thread::spawn(move || {
            let result = timed_link_check(&url);
            let _ = slint::invoke_from_event_loop(move || {
                with_opener(|opener| opener.link_checked(generation, url, result));
            });
        });
    }

    fn link_checked(&mut self, generation: u64, url: String, result: LinkCheck) {
        if generation != self.generation {
            debug!("dropping the link check of open request {generation}, a newer one replaced it");
            return;
        }
        match result {
            LinkCheck::Playable => self.load(&[Source::Link(url)]),
            failure => self.link_failed(&url, failure),
        }
    }

    fn link_failed(&self, url: &str, result: LinkCheck) {
        let host = media::link_host(url);
        match result {
            LinkCheck::Playable => {}
            LinkCheck::NotPlayable(detail) => {
                let detail = format!("{detail}.");
                self.fail(FailureKind::NotPlayable, "Can't play this link", &detail, &host, true);
            }
            LinkCheck::Failed(detail) => {
                self.fail(FailureKind::Network, "Couldn't load the link", &detail, &host, true);
            }
        }
    }

    // While something plays, a failed open must not stop or cover it.
    fn fail(&self, kind: FailureKind, title: &str, detail: &str, window_title: &str, is_link: bool) {
        let Some(app) = self.app.upgrade() else { return };
        let player = app.global::<Player>();
        warn!("{title}: {detail} ({window_title}), while {:?}", player.get_phase());
        match player.get_phase() {
            Phase::Playing => {
                self.osd.show(format!("{title} · {}", detail.trim_end_matches('.')));
                return;
            }
            Phase::Opening => log_error(self.mpv.command(&["stop"])),
            _ => {}
        }
        show_failure(&player, kind, title, detail, window_title, is_link);
    }

    fn load_failed(&mut self, path: &str, has_next: bool) {
        if playlist::try_mirror(path) {
            self.osd.show("Couldn't play it, trying a mirror");
            return;
        }
        lock(&FAILED).push(path.to_string());
        let info = media::describe_source(path);
        let open_error = if info.is_link { http_cache::take_open_error() } else { None };
        warn!(
            "mpv couldn't play {path}{}{}",
            open_error.as_deref().map(|error| format!(": {error}")).unwrap_or_default(),
            if has_next { ", moving on to the next entry" } else { "" }
        );
        if has_next {
            self.osd.show(format!("Couldn't play {}", info.name));
            return;
        }
        let Some(app) = self.app.upgrade() else { return };
        let player = app.global::<Player>();
        match open_error {
            Some(detail) => {
                show_failure(&player, FailureKind::Network, "Couldn't load the link", &detail, &info.place, true)
            }
            None => {
                let detail = format!("No playable video or audio in {}.", info.name);
                show_failure(&player, FailureKind::Unsupported, "Can't play this file", &detail, &info.name, info.is_link)
            }
        }
    }

    // Absolute, so a file opened from the command line has the same resume key as from the dialog.
    fn load(&mut self, sources: &[Source]) {
        let metas = sources
            .iter()
            .filter(|source| !matches!(source, Source::Playlist(_)))
            .map(|source| {
                let meta = Meta::plain(match source {
                    Source::Link(url) => url.clone(),
                    _ => mpv_name(source),
                });
                (mpv_name(source), meta)
            })
            .collect();
        self.load_opened(Opened { document: None, metas }, 0);
    }

    fn load_opened(&mut self, opened: Opened, start: usize) {
        let Some(app) = self.app.upgrade() else { return };
        lock(&FAILED).clear();
        http_cache::clear_headers();
        for (name, meta) in &opened.metas {
            playlist::add_headers(name, &meta.headers);
        }
        let paths: Vec<String> = opened.metas.iter().map(|(name, _)| name.clone()).collect();
        let start = if start < paths.len() { start } else { 0 };
        let Some(first) = paths.get(start) else { return };
        let info = media::describe_source(first);
        let player = app.global::<Player>();
        player.set_opening_name(info.name.as_str().into());
        player.set_place(info.place.into());
        player.set_source_is_link(info.is_link);
        player.set_media_title(info.name.into());
        player.set_ended(false);
        player.set_phase(Phase::Opening);
        log_error(self.mpv.command(&["set", "pause", "no"]));
        playlist::starting(&paths, opened);
        self.loader.load(&self.mpv, paths, start);
    }

    fn pick_files(&mut self) {
        // The native dialog blocks its thread, which must not be the UI thread that draws the video.
        std::thread::spawn(|| {
            debug!("file dialog opened");
            let mut extensions = media::dialog_extensions();
            extensions.push(playlist_file::EXTENSION);
            let Some(files) = rfd::FileDialog::new()
                .add_filter("Video, audio and playlists", extensions.as_slice())
                .add_filter("Playlists", &[playlist_file::EXTENSION])
                .add_filter("All files", &["*"])
                .pick_files()
            else {
                debug!("file dialog cancelled");
                return;
            };
            info!("picked {} files in the dialog", files.len());
            let sources: Vec<Source> = files.into_iter().map(file_source).collect();
            let _ = slint::invoke_from_event_loop(move || with_opener(|opener| opener.open(sources, false)));
        });
    }

    fn show_link_box(&mut self) {
        let Some(app) = self.app.upgrade() else { return };
        let panels = app.global::<Panels>();
        if let Some(link) = clipboard_text().and_then(|text| media::parse_link(&text).ok()) {
            panels.set_link_text(link.into());
        }
        panels.set_link_error(SharedString::new());
        panels.set_link_open(true);
    }

    fn open_link(&mut self, text: &str) -> String {
        match media::parse_link(text) {
            Ok(url) => {
                info!("link box: {url}");
                let adding = self.app.upgrade().is_some_and(|app| {
                    let panels = app.global::<Panels>();
                    panels.set_link_open(false);
                    std::mem::take(&mut *lock(&ADDING_LINK))
                });
                if adding {
                    self.add(vec![Source::Link(url)]);
                } else {
                    self.open(vec![link_source(url)], false);
                }
                String::new()
            }
            Err(err) => {
                debug!("link box refused {text:?}: {}", err.message());
                err.message().to_string()
            }
        }
    }

    // Files copied in a file manager come as a file list. Text that isn't a link is tried as a file path,
    // and quietly ignored when it isn't a playable file.
    fn paste(&mut self) {
        let mut clipboard = match arboard::Clipboard::new() {
            Ok(clipboard) => clipboard,
            Err(err) => {
                warn!("could not read the clipboard: {err}");
                return;
            }
        };
        if let Ok(files) = clipboard.get().file_list()
            && !files.is_empty()
        {
            info!("pasted {} files", files.len());
            self.open(files.into_iter().map(file_source).collect(), false);
            return;
        }
        let Ok(text) = clipboard.get_text() else {
            debug!("paste: nothing usable on the clipboard");
            return;
        };
        if playlist_file::is_playlist_text(&text) {
            info!("pasted a playlist");
            let text = playlist_file::absolutize_text(&text, &std::env::current_dir().unwrap_or_default());
            self.open(vec![Source::Playlist(PlaylistSource::Text(text))], false);
            return;
        }
        if let Ok(url) = media::parse_link(&text) {
            info!("pasted a link");
            self.open(vec![link_source(url)], false);
            return;
        }
        let text = media::clean_text(&text);
        if !text.is_empty() && !text.contains('\n') {
            info!("pasted text, trying it as a file path");
            self.open(vec![file_source(PathBuf::from(text))], true);
        } else {
            debug!("paste: the clipboard text is neither a link nor a path");
        }
    }

    fn cancel(&mut self) {
        self.generation += 1;
        let Some(app) = self.app.upgrade() else { return };
        let player = app.global::<Player>();
        info!("loading cancelled while {:?}", player.get_phase());
        match player.get_phase() {
            Phase::Checking => show_idle(&player),
            Phase::Opening => {
                log_error(self.mpv.command(&["stop"]));
                show_idle(&player);
            }
            _ => {}
        }
    }

    fn retry(&mut self) {
        let Some(app) = self.app.upgrade() else { return };
        info!("retry while {:?}", app.global::<Player>().get_phase());
        match app.global::<Player>().get_phase() {
            Phase::Failed => {
                let request = self.last_request.clone();
                self.open(request, false);
            }
            Phase::Opening => log_error(self.mpv.command(&["playlist-play-index", "current"])),
            _ => {}
        }
    }

    fn close(&mut self) {
        info!("closing the file");
        self.generation += 1;
        log_error(self.mpv.command(&["stop"]));
        if let Some(app) = self.app.upgrade() {
            show_idle(&app.global::<Player>());
        }
    }

    fn replay(&mut self) {
        log_error(self.mpv.command(&["seek", "0", "absolute"]));
        log_error(self.mpv.command(&["set", "pause", "no"]));
    }

    // A read blocked on a silent server can't be interrupted, so that case reloads the file where it was.
    fn retry_connection(&mut self) {
        if !matches!(http_cache::connection_state(), ConnectionState::Fine) {
            info!("retrying the connection now");
            http_cache::retry_now();
            return;
        }
        let Some(app) = self.app.upgrade() else { return };
        let position = app.global::<Player>().get_position();
        info!("reloading the stalled file at {position:.1} s");
        log_error(self.mpv.command(&["set", "start", &format!("{position:.3}")]));
        log_error(self.mpv.command(&["playlist-play-index", "current"]));
        self.restore_start = true;
    }
}

fn show_failure(player: &Player, kind: FailureKind, title: &str, detail: &str, window_title: &str, is_link: bool) {
    player.set_failure_kind(kind);
    player.set_failure_title(title.into());
    player.set_failure_detail(detail.into());
    player.set_media_title(window_title.into());
    player.set_source_is_link(is_link);
    player.set_ended(false);
    player.set_phase(Phase::Failed);
    forget_audio_view(player);
}

fn show_idle(player: &Player) {
    player.set_phase(Phase::Idle);
    player.set_media_title(SharedString::new());
    player.set_ended(false);
    forget_audio_view(player);
}

fn clipboard_text() -> Option<String> {
    arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()).ok()
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// Handles file drags and drops. Slint keeps one winit event callback per window, so the window's single hook calls this.
// winit sends one event per file, all in one go, so each burst is handled once on the next turn of the loop.
pub fn drop_handler(app: &AppWindow) -> impl FnMut(&WindowEvent) + 'static {
    let hovered: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    let dropped: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    let hover_generation = Arc::new(AtomicU64::new(0));
    let app_weak = app.as_weak();
    move |event| {
        match event {
            WindowEvent::HoveredFile(path) => {
                let first = hovered.borrow().is_empty();
                hovered.borrow_mut().push(path.clone());
                if first {
                    let (hovered, app_weak, hover_generation) =
                        (hovered.clone(), app_weak.clone(), hover_generation.clone());
                    Timer::single_shot(Duration::ZERO, move || {
                        let paths = hovered.borrow().clone();
                        check_hovered(paths, app_weak, hover_generation);
                    });
                }
            }
            WindowEvent::HoveredFileCancelled => {
                hovered.borrow_mut().clear();
                hover_generation.fetch_add(1, Ordering::Relaxed);
                set_drag(&app_weak, DragState::None, String::new());
            }
            WindowEvent::DroppedFile(path) => {
                hovered.borrow_mut().clear();
                hover_generation.fetch_add(1, Ordering::Relaxed);
                set_drag(&app_weak, DragState::None, String::new());
                let first = dropped.borrow().is_empty();
                dropped.borrow_mut().push(path.clone());
                if first {
                    let dropped = dropped.clone();
                    Timer::single_shot(Duration::ZERO, move || {
                        let paths = std::mem::take(&mut *dropped.borrow_mut());
                        info!("dropped {} files", paths.len());
                        let sources = paths.into_iter().map(file_source).collect();
                        with_opener(|opener| opener.open(sources, true));
                    });
                }
            }
            _ => {}
        }
    }
}

fn check_hovered(paths: Vec<PathBuf>, app_weak: slint::Weak<AppWindow>, hover_generation: Arc<AtomicU64>) {
    let generation = hover_generation.load(Ordering::Relaxed);
    std::thread::spawn(move || {
        let playable: Vec<&PathBuf> = paths
            .iter()
            .filter(|path| {
                playlist_file::is_playlist_name(&path.to_string_lossy()) || matches!(media::check_file(path), FileCheck::Playable)
            })
            .collect();
        let (state, name) = match playable.as_slice() {
            [] => (DragState::NotPlayable, paths.first().map(|path| file_name(path)).unwrap_or_default()),
            [only] => (DragState::Playable, file_name(only)),
            several => (DragState::Playable, format!("{} files", several.len())),
        };
        let _ = app_weak.upgrade_in_event_loop(move |app| {
            if hover_generation.load(Ordering::Relaxed) == generation {
                let player = app.global::<Player>();
                player.set_drag_state(state);
                player.set_drag_name(name.into());
            }
        });
    });
}

fn set_drag(app_weak: &slint::Weak<AppWindow>, state: DragState, name: String) {
    if let Some(app) = app_weak.upgrade() {
        let player = app.global::<Player>();
        player.set_drag_state(state);
        player.set_drag_name(name.into());
    }
}
