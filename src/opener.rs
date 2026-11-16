use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, SharedString, Timer, TimerMode};

use crate::http_cache::{self, ConnectionState};
use crate::media::{self, FileCheck, LinkCheck};
use crate::mpv::Mpv;
use crate::{AppWindow, DragState, FailureKind, Osd, Panels, Phase, Player, log_error};

#[derive(Clone)]
enum Source {
    File(PathBuf),
    Link(String),
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
                    let result = media::check_link(&url);
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
                    Checked::File(path, check)
                }
                Pending::Ready(Source::Link(url)) => Checked::Link(url, LinkCheck::Playable),
                Pending::Checking(check) => check.join().expect("the link check panicked"),
            })
            .collect()
    })
}

static FAILED: Mutex<Vec<String>> = Mutex::new(Vec::new());

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
}

impl Loader {
    pub fn render_ready(&self, mpv: &Mpv) {
        let pending = {
            let mut state = self.0.borrow_mut();
            state.ready = true;
            std::mem::take(&mut state.pending)
        };
        send_loadfile(mpv, &pending);
    }

    fn load(&self, mpv: &Mpv, paths: Vec<String>) {
        let mut state = self.0.borrow_mut();
        if state.ready {
            drop(state);
            send_loadfile(mpv, &paths);
        } else {
            state.pending = paths;
        }
    }
}

fn send_loadfile(mpv: &Mpv, paths: &[String]) {
    for (index, path) in paths.iter().enumerate() {
        log_error(mpv.command(&["loadfile", path, if index == 0 { "replace" } else { "append" }]));
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
    };
    OPENER.with(|slot| *slot.borrow_mut() = Some(Rc::new(RefCell::new(opener))));

    let player = app.global::<Player>();
    player.on_open_file(|| with_opener(Opener::pick_files));
    player.on_show_link_box(|| with_opener(Opener::show_link_box));
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
    wire_drops(app);
}

fn is_link_argument(argument: &str) -> bool {
    argument.contains("://")
}

/// Another launch hands its arguments over from its own working folder, so relative paths are resolved first.
pub fn absolute_argument(argument: String) -> String {
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
            if !is_link_argument(argument) {
                sources.push(Source::File(PathBuf::from(argument)));
                continue;
            }
            match media::parse_link(argument) {
                Ok(url) => sources.push(Source::Link(url)),
                Err(err) => {
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
        self.generation += 1;
        let generation = self.generation;
        self.last_request = sources.clone();
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
            return;
        }
        let playable: Vec<Source> = checks.iter().filter_map(Checked::playable).collect();
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
            let result = media::check_link(&url);
            let _ = slint::invoke_from_event_loop(move || {
                with_opener(|opener| opener.link_checked(generation, url, result));
            });
        });
    }

    fn link_checked(&mut self, generation: u64, url: String, result: LinkCheck) {
        if generation != self.generation {
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
        lock(&FAILED).push(path.to_string());
        let info = media::describe_source(path);
        let open_error = if info.is_link { http_cache::take_open_error() } else { None };
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

    fn load(&mut self, sources: &[Source]) {
        let Some(app) = self.app.upgrade() else { return };
        lock(&FAILED).clear();
        // Absolute, so a file opened from the command line has the same resume key as from the dialog.
        let paths: Vec<String> = sources
            .iter()
            .map(|source| match source {
                Source::File(path) => std::path::absolute(path).unwrap_or_else(|_| path.clone()).to_string_lossy().into_owned(),
                Source::Link(url) => http_cache::wrap_url(url),
            })
            .collect();
        let Some(first) = paths.first() else { return };
        let info = media::describe_source(first);
        let player = app.global::<Player>();
        player.set_opening_name(info.name.as_str().into());
        player.set_place(info.place.into());
        player.set_source_is_link(info.is_link);
        player.set_media_title(info.name.into());
        player.set_ended(false);
        player.set_phase(Phase::Opening);
        self.loader.load(&self.mpv, paths);
    }

    fn pick_files(&mut self) {
        // The native dialog blocks its thread, which must not be the UI thread that draws the video.
        std::thread::spawn(|| {
            let Some(files) = rfd::FileDialog::new()
                .add_filter("Video and audio", media::dialog_extensions().as_slice())
                .add_filter("All files", &["*"])
                .pick_files()
            else {
                return;
            };
            let sources: Vec<Source> = files.into_iter().map(Source::File).collect();
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
                if let Some(app) = self.app.upgrade() {
                    app.global::<Panels>().set_link_open(false);
                }
                self.open(vec![Source::Link(url)], false);
                String::new()
            }
            Err(err) => err.message().to_string(),
        }
    }

    // Anything that isn't a link is tried as a file path, and quietly ignored when it isn't a playable file.
    fn paste(&mut self) {
        let Some(text) = clipboard_text() else { return };
        if let Ok(url) = media::parse_link(&text) {
            self.open(vec![Source::Link(url)], false);
            return;
        }
        let text = media::clean_text(&text);
        if !text.is_empty() && !text.contains('\n') {
            self.open(vec![Source::File(PathBuf::from(text))], true);
        }
    }

    fn cancel(&mut self) {
        self.generation += 1;
        let Some(app) = self.app.upgrade() else { return };
        let player = app.global::<Player>();
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
            http_cache::retry_now();
            return;
        }
        let Some(app) = self.app.upgrade() else { return };
        let position = app.global::<Player>().get_position();
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
}

fn show_idle(player: &Player) {
    player.set_phase(Phase::Idle);
    player.set_media_title(SharedString::new());
    player.set_ended(false);
}

fn clipboard_text() -> Option<String> {
    arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()).ok()
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

// winit sends one event per file, all in one go, so each burst is handled once on the next turn of the loop.
fn wire_drops(app: &AppWindow) {
    let hovered: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    let dropped: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    let hover_generation = Arc::new(AtomicU64::new(0));
    let app_weak = app.as_weak();
    app.window().on_winit_window_event(move |_, event| {
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
                        let sources = paths.into_iter().map(Source::File).collect();
                        with_opener(|opener| opener.open(sources, true));
                    });
                }
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

fn check_hovered(paths: Vec<PathBuf>, app_weak: slint::Weak<AppWindow>, hover_generation: Arc<AtomicU64>) {
    let generation = hover_generation.load(Ordering::Relaxed);
    std::thread::spawn(move || {
        let playable: Vec<&PathBuf> =
            paths.iter().filter(|path| matches!(media::check_file(path), FileCheck::Playable)).collect();
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
