// Without it Windows opens a console window next to Rona. Debug builds keep the console for eprintln output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio_info;
mod buffered;
mod capture;
mod chapter_skip;
mod extensions;
mod http_cache;
mod instance;
mod media;
mod mpv;
mod opener;
mod recorder;
mod resume;
mod settings;
mod shortcuts;
mod state;
mod updater;
mod video;
#[cfg(windows)]
mod video_d3d11;
mod volume;
#[cfg(target_os = "linux")]
mod wayland;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::error::Error;
use std::fmt::Display;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::winit_030::winit::error::ExternalError;
use slint::winit_030::winit::event::{ElementState, WindowEvent};
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::winit_030::winit::window::{ResizeDirection, WindowLevel};
use slint::{
    ComponentHandle, GraphicsAPI, Model, ModelRc, RenderingState, SharedString, Timer, TimerMode, VecModel,
};

use http_cache::ConnectionState;
use mpv::{Event, Mpv, Node, PropertyValue};
use capture::Capture;
use instance::Claim;
use settings::SharedConfig;
use state::SharedState;
use video::{Area, Picture, VideoLayer};

slint::include_modules!();

const TIME_POS: u64 = 1;
const DURATION: u64 = 2;
const PAUSE: u64 = 3;
const MEDIA_TITLE: u64 = 4;
const VOLUME: u64 = 5;
const MUTE: u64 = 6;
const SPEED: u64 = 7;
const HWDEC: u64 = 8;
const VIDEO_HEIGHT: u64 = 9;
const VIDEO_FORMAT: u64 = 10;
const AUDIO_CODEC: u64 = 11;
const AUDIO_CHANNELS: u64 = 12;
const TRACK_LIST: u64 = 13;
const PLAYLIST: u64 = 14;
const PAUSED_FOR_CACHE: u64 = 15;
const BUFFERING_STATE: u64 = 16;
const SEEKING: u64 = 17;
const PATH: u64 = 18;
const FILE_FORMAT: u64 = 19;
const VIDEO_BITRATE: u64 = 20;
const CHAPTER_LIST: u64 = 21;
const CACHE_STATE: u64 = 22;
const IDLE_ACTIVE: u64 = 23;
const EOF_REACHED: u64 = 24;
const METADATA: u64 = 25;
const OSD_DIMENSIONS: u64 = 26;
const CHAPTER: u64 = 27;
const SUB_VISIBILITY: u64 = 28;

#[cfg(target_os = "linux")]
const APP_ID: &str = "rona";

const DOWNLOAD_POLL_INTERVAL: Duration = Duration::from_millis(500);
const DOWNLOAD_SPEED_WINDOW: Duration = Duration::from_secs(2);
const SLOW_AFTER_SECS: u64 = 10;
const STALLED_AFTER_SECS: u64 = 10;

#[derive(Default)]
struct AudioInfo {
    codec: String,
    channels: i64,
}

#[derive(Default)]
struct EventState {
    audio: Mutex<AudioInfo>,
    track_refresh: Arc<AtomicU64>,
    playlist_refresh: Arc<AtomicU64>,
    chapter_refresh: Arc<AtomicU64>,
    tag_refresh: Arc<AtomicU64>,
    path: Mutex<String>,
    // The file whose pre-read info the audio view shows, so a file without it falls back to mpv's.
    pre_read_path: Mutex<String>,
    file_format: Mutex<String>,
    dump_results: Mutex<Vec<Option<String>>>,
    byte_times: Mutex<buffered::ByteTimeMap>,
    capture: Mutex<Option<Capture>>,
    video_bits_per_second: AtomicU64,
    picture: Mutex<Option<Picture>>,
    chapter_skip: Mutex<chapter_skip::ChapterSkip>,
    // When the current file started opening, and the download counter at that time.
    open_started: Mutex<Option<(Instant, u64)>>,
}

struct TrackLists {
    subtitles: Vec<TrackItem>,
    audio: Vec<TrackItem>,
    has_video: bool,
    has_cover: bool,
}

struct PlaylistRead {
    items: Vec<PlaylistItem>,
    next_title: String,
    // The current file and its neighbors, nearest first, whose audio info is read ahead.
    upcoming: Vec<String>,
    files: Vec<String>,
}

// Next is where playback usually goes, previous is one key press away.
const READ_AHEAD_NEXT: usize = 3;
const READ_AHEAD_PREVIOUS: usize = 1;

struct AudioTags {
    artist: String,
    album_line: String,
}

#[derive(Clone)]
struct Osd {
    app: slint::Weak<AppWindow>,
    timer: Rc<Timer>,
    config: SharedConfig,
}

impl Osd {
    fn new(app: &AppWindow, config: &SharedConfig) -> Self {
        Self { app: app.as_weak(), timer: Rc::new(Timer::default()), config: config.clone() }
    }

    fn show(&self, text: impl Into<SharedString>) {
        let Some(app) = self.app.upgrade() else { return };
        let player = app.global::<Player>();
        player.set_osd_text(text.into());
        player.set_osd_visible(true);
        let app_weak = self.app.clone();
        let duration = self.config.borrow().osd_duration();
        self.timer.start(TimerMode::SingleShot, duration, move || {
            if let Some(app) = app_weak.upgrade() {
                app.global::<Player>().set_osd_visible(false);
            }
        });
    }
}

// How long another launch waits for this window's UI thread to take its files.
const HANDOVER_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Arguments {
    version: bool,
    new_instance: bool,
    sources: Vec<String>,
}

// args_os, because args panics on a path that isn't valid Unicode.
fn parse_arguments() -> Arguments {
    let mut arguments = Arguments::default();
    for argument in std::env::args_os().skip(1) {
        let argument = argument.to_string_lossy().into_owned();
        match argument.as_str() {
            "--version" => arguments.version = true,
            "--new-instance" => arguments.new_instance = true,
            _ => arguments.sources.push(opener::absolute_argument(argument)),
        }
    }
    arguments
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = parse_arguments();
    // Also starts mpv, so the release workflow can tell that a package finds libmpv and what it needs.
    if arguments.version {
        attach_parent_console();
        println!("Rona {}", env!("CARGO_PKG_VERSION"));
        Mpv::new()?;
        println!("libmpv loaded");
        return Ok(());
    }

    let capture = Capture::from_env();
    // Claimed before anything slow, so files opened together all reach the same window.
    let server = if arguments.new_instance || capture.is_some() {
        None
    } else {
        match instance::claim(&arguments.sources) {
            Claim::Forwarded => return Ok(()),
            Claim::Primary(server) => Some(server),
            Claim::Alone => None,
        }
    };

    // Slint picks OpenGL ES on Windows by default, where mpv renders 10-bit hardware decoded frames washed out.
    let backend = slint::BackendSelector::new().require_opengl();
    // Wayland ignores window icons and finds the icon through the app id and rona.desktop instead.
    #[cfg(target_os = "linux")]
    let backend = backend.with_winit_window_attributes_hook(|attributes| {
        use slint::winit_030::winit::platform::{wayland::WindowAttributesExtWayland, x11::WindowAttributesExtX11};
        let attributes = WindowAttributesExtWayland::with_name(attributes, APP_ID, APP_ID);
        WindowAttributesExtX11::with_name(attributes, APP_ID, APP_ID)
    });
    if let Err(err) = backend.select() {
        eprintln!("desktop OpenGL is not available, falling back to the default renderer: {err}");
    }

    let config: SharedConfig = Rc::new(RefCell::new(settings::load()));
    let saved_state: SharedState = Rc::new(RefCell::new(state::load()));
    let app = AppWindow::new()?;
    if capture.is_none() && config.borrow().remember_window {
        state::restore_window(&app, &saved_state.borrow());
    }
    let mpv = Mpv::new()?;
    mpv.add_stream_protocol(http_cache::PROTOCOL, http_cache::open_stream)?;
    observe_properties(&mpv)?;
    settings::wire(&mpv, &app, &config);
    volume::wire(&mpv, &saved_state);
    resume::wire(&app, &mpv, &config);

    let state = Arc::new(EventState { capture: Mutex::new(capture), ..Default::default() });
    let osd = Osd::new(&app, &config);
    let loader = opener::Loader::default();
    let (read_app, read_state, read_mpv) = (app.as_weak(), state.clone(), mpv.clone());
    audio_info::start(move |read| {
        let (state, mpv) = (read_state.clone(), read_mpv.clone());
        let _ = read_app.upgrade_in_event_loop(move |app| {
            if let audio_info::Read::Info(path) = &read {
                show_pre_read(&app.global::<Player>(), &state, path);
            }
            // The playlist shows the titles that were just read.
            read_in_background(&mpv, &app, &state.playlist_refresh, read_playlist, apply_playlist);
        });
    });
    forward_mpv_events(&mpv, &app, &state)?;
    let renderer = config.borrow().renderer();
    attach_video(&mpv, &app, loader.clone(), state.clone(), renderer)
        .map_err(|err| format!("could not attach the video renderer: {err:?}"))?;

    opener::wire(&app, &mpv, &osd, loader);
    wire_playback(&mpv, &app, &osd, &config);
    wire_window_controls(&app, &osd, &config, &saved_state);
    let _download_watch = watch_download(&app, &state);
    let _recorder = recorder::wire(&app, &mpv, state);
    updater::wire(&app, &config);
    let listener = server.map(|server| {
        let app_weak = app.as_weak();
        instance::listen(server, move |launch| {
            let (taken, was_taken) = std::sync::mpsc::channel();
            // A hidden window is closing, so that launch opens its own window instead.
            let queued = app_weak.upgrade_in_event_loop(move |app| {
                let visible = app.window().is_visible();
                if visible {
                    bring_to_front(&app, launch.activation_token.as_deref());
                    opener::queue_arguments(launch.sources);
                }
                let _ = taken.send(visible);
            });
            queued.is_ok() && was_taken.recv_timeout(HANDOVER_TIMEOUT).unwrap_or(false)
        })
    });
    if listener.is_some() {
        opener::queue_arguments(arguments.sources);
    } else {
        opener::open_arguments(&arguments.sources);
    }

    app.run()?;
    if let Some(listener) = &listener {
        listener.stop();
    }
    updater::finish_pending_install();
    Ok(())
}

#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn bring_to_front(app: &AppWindow, activation_token: Option<&str>) {
    app.window().set_minimized(false);
    app.window().with_winit_window(|window| {
        window.focus_window();
        #[cfg(target_os = "linux")]
        if let Some(token) = activation_token {
            activate_on_wayland(window, token);
        }
    });
}

#[cfg(target_os = "linux")]
fn activate_on_wayland(window: &slint::winit_030::winit::window::Window, token: &str) {
    use slint::winit_030::winit::raw_window_handle::{
        HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    };
    let (Ok(display), Ok(handle)) = (window.display_handle(), window.window_handle()) else { return };
    let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) = (display.as_raw(), handle.as_raw())
    else {
        return;
    };
    if let Err(err) = wayland::activate(display.display, surface.surface, token) {
        eprintln!("could not bring the window to the front: {err}");
    }
}

// A release build has no console of its own, so output goes to the terminal that started it.
#[cfg(windows)]
fn attach_parent_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

#[cfg(not(windows))]
fn attach_parent_console() {}

fn observe_properties(mpv: &Mpv) -> Result<(), mpv::Error> {
    mpv.observe_double(TIME_POS, "time-pos")?;
    mpv.observe_double(DURATION, "duration")?;
    mpv.observe_flag(PAUSE, "pause")?;
    mpv.observe_string(MEDIA_TITLE, "media-title")?;
    mpv.observe_double(VOLUME, "volume")?;
    mpv.observe_flag(MUTE, "mute")?;
    mpv.observe_flag(SUB_VISIBILITY, "sub-visibility")?;
    mpv.observe_double(SPEED, "speed")?;
    mpv.observe_string(HWDEC, "hwdec-current")?;
    mpv.observe_double(VIDEO_HEIGHT, "height")?;
    mpv.observe_string(VIDEO_FORMAT, "video-format")?;
    mpv.observe_string(AUDIO_CODEC, "audio-codec-name")?;
    mpv.observe_double(AUDIO_CHANNELS, "audio-params/channel-count")?;
    mpv.observe_changes(TRACK_LIST, "track-list")?;
    mpv.observe_changes(PLAYLIST, "playlist")?;
    mpv.observe_flag(PAUSED_FOR_CACHE, "paused-for-cache")?;
    mpv.observe_double(BUFFERING_STATE, "cache-buffering-state")?;
    mpv.observe_flag(SEEKING, "seeking")?;
    mpv.observe_string(PATH, "path")?;
    mpv.observe_string(FILE_FORMAT, "file-format")?;
    mpv.observe_double(VIDEO_BITRATE, "video-bitrate")?;
    mpv.observe_changes(CHAPTER_LIST, "chapter-list")?;
    mpv.observe_double(CHAPTER, "chapter")?;
    mpv.observe_flag(IDLE_ACTIVE, "idle-active")?;
    mpv.observe_flag(EOF_REACHED, "eof-reached")?;
    mpv.observe_changes(METADATA, "metadata")?;
    mpv.observe_node(CACHE_STATE, "demuxer-cache-state")?;
    mpv.observe_node(OSD_DIMENSIONS, "osd-dimensions")
}

fn forward_mpv_events(mpv: &Mpv, app: &AppWindow, state: &Arc<EventState>) -> Result<(), mpv::Error> {
    let mpv_weak = mpv.downgrade();
    let app_weak = app.as_weak();
    let state = state.clone();
    mpv.on_wakeup(move || {
        let mpv_weak = mpv_weak.clone();
        let app_weak = app_weak.clone();
        let state = state.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let (Some(mpv), Some(app)) = (mpv_weak.upgrade(), app_weak.upgrade()) {
                drain_events(&mpv, &app, &state);
            }
        });
    })
}

fn drain_events(mpv: &Mpv, app: &AppWindow, state: &EventState) {
    let player = app.global::<Player>();
    let player = &player;
    while let Some(event) = mpv.next_event() {
        match event {
            Event::PropertyChange { id: TIME_POS, value } => {
                player.set_position(number(value).unwrap_or(0.0) as f32);
                update_time_text(player);
                state.chapter_skip.lock().unwrap().check(mpv, app);
            }
            Event::PropertyChange { id: CHAPTER, value } => {
                let mut chapter_skip = state.chapter_skip.lock().unwrap();
                chapter_skip.set_current(number(value).unwrap_or(-1.0));
                chapter_skip.check(mpv, app);
            }
            Event::PropertyChange { id: DURATION, value } => {
                player.set_duration(number(value).unwrap_or(0.0) as f32);
                update_time_text(player);
            }
            Event::PropertyChange { id: PAUSE, value: Some(PropertyValue::Flag(paused)) } => {
                player.set_paused(paused);
                update_window_level(app);
                if paused && player.get_phase() == Phase::Playing {
                    resume::save_soon();
                }
            }
            Event::Hook { kind, id } => {
                if kind == resume::HOOK_LOAD {
                    volume::file_loading(&mpv.get_string("stream-open-filename").unwrap_or_default());
                }
                resume::handle_hook(mpv, kind, id);
            }
            // While opening, the audio view keeps its title until the file's info switches in all at once.
            Event::PropertyChange { id: MEDIA_TITLE, value } => {
                let title: SharedString = readable_title(text(value)).into();
                if player.get_phase() == Phase::Playing {
                    player.set_audio_title(title.clone());
                }
                player.set_media_title(title);
            }
            Event::PropertyChange { id: VOLUME, value } => {
                player.set_volume((number(value).unwrap_or(0.0) / 100.0) as f32);
            }
            Event::PropertyChange { id: MUTE, value: Some(PropertyValue::Flag(muted)) } => {
                player.set_muted(muted);
            }
            Event::PropertyChange { id: SUB_VISIBILITY, value: Some(PropertyValue::Flag(visible)) } => {
                player.set_subtitles_hidden(!visible);
            }
            Event::PropertyChange { id: SPEED, value } => {
                let speed = number(value).unwrap_or(1.0) as f32;
                player.set_speed(speed);
                player.set_speed_text(speed_label(speed).into());
            }
            Event::PropertyChange { id: HWDEC, value } => {
                let decoder = text(value);
                player.set_hardware_decoding(!decoder.is_empty() && decoder != "no");
            }
            Event::PropertyChange { id: VIDEO_HEIGHT, value } => {
                let badge = match number(value) {
                    Some(height) if height > 0.0 => format!("{}p", height as i64),
                    _ => String::new(),
                };
                player.set_resolution_badge(badge.into());
            }
            Event::PropertyChange { id: VIDEO_FORMAT, value } => {
                player.set_video_badge(video_codec_label(&text(value)).into());
            }
            Event::PropertyChange { id: AUDIO_CODEC, value } => {
                if let Ok(mut info) = state.audio.lock() {
                    info.codec = text(value);
                    player.set_audio_badge(audio_badge(&info).into());
                }
            }
            Event::PropertyChange { id: AUDIO_CHANNELS, value } => {
                if let Ok(mut info) = state.audio.lock() {
                    info.channels = number(value).unwrap_or(0.0) as i64;
                    player.set_audio_badge(audio_badge(&info).into());
                }
            }
            Event::PropertyChange { id: TRACK_LIST, .. } => {
                read_in_background(mpv, app, &state.track_refresh, read_tracks, apply_tracks);
            }
            Event::PropertyChange { id: PLAYLIST, .. } => {
                read_in_background(mpv, app, &state.playlist_refresh, read_playlist, apply_playlist);
            }
            Event::PropertyChange { id: CACHE_STATE, value } => {
                let cache_state = match value {
                    Some(PropertyValue::Node(cache_state)) => Some(cache_state),
                    _ => None,
                };
                let ranges = buffered_ranges(cache_state.as_ref(), state, player.get_duration());
                if !same_ranges(&player.get_loaded_ranges(), &ranges) {
                    player.set_loaded_ranges(ModelRc::new(VecModel::from(ranges)));
                }
            }
            Event::PropertyChange { id: OSD_DIMENSIONS, value } => {
                let picture = match value {
                    Some(PropertyValue::Node(dimensions)) => picture_from_osd_dimensions(&dimensions),
                    _ => None,
                };
                *state.picture.lock().unwrap() = picture;
            }
            Event::PropertyChange { id: CHAPTER_LIST, .. } => {
                read_in_background(mpv, app, &state.chapter_refresh, read_chapters, apply_chapters);
            }
            Event::PropertyChange { id: PAUSED_FOR_CACHE, value: Some(PropertyValue::Flag(waiting)) } => {
                player.set_buffering(waiting);
            }
            Event::PropertyChange { id: BUFFERING_STATE, value } => {
                player.set_buffer_percent(number(value).unwrap_or(0.0) as i32);
            }
            Event::PropertyChange { id: SEEKING, value: Some(PropertyValue::Flag(seeking)) } => {
                player.set_seeking(seeking);
            }
            Event::PropertyChange { id: PATH, value } => {
                let path = text(value);
                if !path.is_empty() {
                    let info = media::describe_source(&path);
                    player.set_opening_name(info.name.into());
                    player.set_place(info.place.into());
                    player.set_source_is_link(info.is_link);
                }
                if let Ok(mut current) = state.path.lock() {
                    current.clone_from(&path);
                }
                if audio_info::can_read(&path) {
                    if audio_info::get(&path).is_some() {
                        show_pre_read(&player, state, &path);
                    } else {
                        audio_info::read_next(&path);
                    }
                }
            }
            Event::PropertyChange { id: IDLE_ACTIVE, value: Some(PropertyValue::Flag(true)) } => {
                if matches!(player.get_phase(), Phase::Opening | Phase::Playing) {
                    player.set_phase(Phase::Idle);
                    player.set_media_title(SharedString::new());
                    player.set_ended(false);
                    forget_audio_view(&player);
                }
            }
            Event::PropertyChange { id: EOF_REACHED, value: Some(PropertyValue::Flag(reached)) } => {
                player.set_ended(reached);
            }
            // mpv empties the tags while switching files, so they're only followed live once playing.
            Event::PropertyChange { id: METADATA, .. } => {
                if player.get_phase() == Phase::Playing {
                    read_in_background(mpv, app, &state.tag_refresh, read_tags, apply_tags);
                }
            }
            Event::PropertyChange { id: VIDEO_BITRATE, value } => {
                let bits = number(value).unwrap_or(0.0).max(0.0) as u64;
                state.video_bits_per_second.store(bits, Ordering::Relaxed);
            }
            Event::PropertyChange { id: FILE_FORMAT, value } => {
                if let Ok(mut format) = state.file_format.lock() {
                    *format = text(value);
                }
            }
            Event::StartFile => {
                // Going straight from one audio file to the next keeps the audio view and the old cover until
                // the new file's tracks and tags replace them, instead of blanking the view in between.
                let continuing_audio =
                    player.get_audio_only() && matches!(player.get_phase(), Phase::Playing | Phase::Opening);
                player.set_phase(Phase::Opening);
                player.set_ended(false);
                player.set_opening_stage("Connecting".into());
                // A read still running for the previous file must not mark this one's tracks as known.
                state.track_refresh.fetch_add(1, Ordering::SeqCst);
                player.set_tracks_known(false);
                if !continuing_audio {
                    forget_audio_view(&player);
                    player.set_audio_title(SharedString::new());
                    player.set_audio_artist(SharedString::new());
                    player.set_audio_album(SharedString::new());
                }
                http_cache::take_open_error();
                if let Ok(mut started) = state.open_started.lock() {
                    *started = Some((Instant::now(), http_cache::downloaded_bytes()));
                }
                if let Ok(mut byte_times) = state.byte_times.lock() {
                    byte_times.clear();
                }
                state.chapter_skip.lock().unwrap().reset();
            }
            Event::EndFile { reason } => {
                if reason == mpv::END_FILE_REASON_ERROR {
                    let path = state.path.lock().map(|path| path.clone()).unwrap_or_default();
                    opener::load_failed(&path, !player.get_next_title().is_empty());
                    read_in_background(mpv, app, &state.playlist_refresh, read_playlist, apply_playlist);
                }
            }
            Event::PlaybackRestart => {
                if player.get_phase() == Phase::Opening {
                    // Without pre-read info the audio view switches to mpv's now that the file plays.
                    let current = state.path.lock().map(|path| path.clone()).unwrap_or_default();
                    let pre_read = state.pre_read_path.lock().is_ok_and(|shown| *shown == current);
                    if !pre_read {
                        player.set_has_cover_image(false);
                        player.set_audio_title(player.get_media_title());
                    }
                    player.set_phase(Phase::Playing);
                    read_in_background(mpv, app, &state.tag_refresh, read_tags, apply_tags);
                    if player.get_tracks_known() {
                        volume::file_playing(player.get_audio_only());
                    }
                    opener::playback_restarted();
                } else if player.get_phase() == Phase::Playing {
                    resume::save_soon();
                }
                advance_capture(mpv, app, state);
            }
            Event::CommandFailed(message) => eprintln!("mpv command failed: {message}"),
            Event::CommandDone { id: recorder::DUMP_COMMAND_ID, error } => {
                if let Ok(mut results) = state.dump_results.lock() {
                    results.push(error);
                }
            }
            Event::Shutdown => {
                let _ = slint::quit_event_loop();
            }
            _ => {}
        }
    }
}

// mpv property reads wait on its core, which stalls video drawing when done on the UI thread.
fn read_in_background<T: Send + 'static>(
    mpv: &Mpv,
    app: &AppWindow,
    counter: &Arc<AtomicU64>,
    read: fn(&Mpv) -> T,
    apply: fn(&Player, T),
) {
    let generation = counter.fetch_add(1, Ordering::SeqCst) + 1;
    let (mpv, app_weak, counter) = (mpv.clone(), app.as_weak(), counter.clone());
    std::thread::spawn(move || {
        let result = read(&mpv);
        let _ = slint::invoke_from_event_loop(move || {
            if counter.load(Ordering::SeqCst) != generation {
                return;
            }
            if let Some(app) = app_weak.upgrade() {
                apply(&app.global::<Player>(), result);
            }
        });
    });
}

fn read_tracks(mpv: &Mpv) -> TrackLists {
    let count = mpv
        .get_string("track-list/count")
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or(0);
    let mut subtitles = Vec::new();
    let mut audio = Vec::new();
    let (mut has_video, mut has_cover) = (false, false);
    for index in 0..count {
        let field = |name: &str| mpv.get_string(&format!("track-list/{index}/{name}"));
        let Some(id) = field("id").and_then(|id| id.parse::<i32>().ok()) else { continue };
        let title = field("title").unwrap_or_default();
        let language = field("lang").unwrap_or_default();
        let codec = field("codec").unwrap_or_default();
        let selected = field("selected").as_deref() == Some("yes");

        let (label, sublabel) = match language_name(&language) {
            Some(name) => (name, title),
            None if !title.is_empty() => (title, language.to_uppercase()),
            None if !language.is_empty() => (language.to_uppercase(), String::new()),
            None => (format!("Track {id}"), String::new()),
        };

        match field("type").as_deref() {
            Some("sub") => {
                let detail = subtitle_codec_label(&codec);
                subtitles.push(track_item(id, label, sublabel, detail, selected));
            }
            Some("audio") => {
                let channels = field("demux-channel-count")
                    .and_then(|count| count.parse::<i64>().ok())
                    .unwrap_or(0);
                let detail = audio_label(&codec, channels);
                audio.push(track_item(id, label, sublabel, detail, selected));
            }
            // mpv lists embedded cover art as a video track marked as album art.
            Some("video") if field("albumart").as_deref() == Some("yes") => has_cover = true,
            Some("video") => has_video = true,
            _ => {}
        }
    }
    TrackLists { subtitles, audio, has_video, has_cover }
}

/// Once nothing plays, the next file must not pick up this one's audio view and cover.
fn forget_audio_view(player: &Player) {
    player.set_audio_only(false);
    player.set_has_cover(false);
    player.set_has_cover_image(false);
}

// Shows a file's pre-read info as soon as it's cached, all at once and usually while mpv is still opening it.
fn show_pre_read(player: &Player, state: &EventState, path: &str) {
    let current = state.path.lock().map(|current| current.clone()).unwrap_or_default();
    if current != path || !matches!(player.get_phase(), Phase::Opening | Phase::Playing) {
        return;
    }
    let Some(info) = audio_info::get(path) else { return };
    player.set_audio_only(true);
    player.set_has_cover_image(info.cover.is_some());
    if let Some(cover) = &info.cover {
        player.set_cover_image(slint::Image::from_rgba8(cover.clone()));
    }
    player.set_audio_title(info.title.clone().unwrap_or_else(|| file_display_name(path)).into());
    player.set_audio_artist(info.artist.as_str().into());
    player.set_audio_album(info.album_line.as_str().into());
    if let Ok(mut shown) = state.pre_read_path.lock() {
        *shown = path.to_string();
    }
}

fn apply_tracks(player: &Player, tracks: TrackLists) {
    // mpv empties the list while switching files. That says nothing about the new file, and must not end the
    // audio view that stays up between two audio files.
    let known = tracks.has_video || !tracks.audio.is_empty();
    player.set_tracks_known(known);
    if known {
        player.set_audio_only(!tracks.has_video);
        player.set_has_cover(tracks.has_cover && !tracks.has_video);
    }
    player.set_subtitles_off(!tracks.subtitles.iter().any(|track| track.selected));
    player.set_subtitle_tracks(ModelRc::new(VecModel::from(tracks.subtitles)));
    player.set_audio_tracks(ModelRc::new(VecModel::from(tracks.audio)));
}

fn track_item(id: i32, label: String, sublabel: String, detail: String, selected: bool) -> TrackItem {
    TrackItem { id, label: label.into(), sublabel: sublabel.into(), detail: detail.into(), selected }
}

fn language_name(code: &str) -> Option<String> {
    let mut parts = code.split(['-', '_']);
    let base = parts.next()?.to_lowercase();
    let name = match base.as_str() {
        "en" | "eng" => "English",
        "ja" | "jpn" => "Japanese",
        "pt" | "por" => "Portuguese",
        "es" | "spa" => "Spanish",
        "ar" | "ara" => "Arabic",
        "fr" | "fre" | "fra" => "French",
        "de" | "ger" | "deu" => "German",
        "it" | "ita" => "Italian",
        "ru" | "rus" => "Russian",
        "zh" | "chi" | "zho" => "Chinese",
        "ko" | "kor" => "Korean",
        "id" | "ind" => "Indonesian",
        "ms" | "may" | "msa" => "Malay",
        "th" | "tha" => "Thai",
        "vi" | "vie" => "Vietnamese",
        "tr" | "tur" => "Turkish",
        "pl" | "pol" => "Polish",
        "nl" | "dut" | "nld" => "Dutch",
        "sv" | "swe" => "Swedish",
        "no" | "nor" | "nb" | "nob" => "Norwegian",
        "da" | "dan" => "Danish",
        "fi" | "fin" => "Finnish",
        "cs" | "cze" | "ces" => "Czech",
        "sk" | "slo" | "slk" => "Slovak",
        "hu" | "hun" => "Hungarian",
        "ro" | "rum" | "ron" => "Romanian",
        "bg" | "bul" => "Bulgarian",
        "hr" | "hrv" => "Croatian",
        "sr" | "srp" => "Serbian",
        "uk" | "ukr" => "Ukrainian",
        "el" | "gre" | "ell" => "Greek",
        "he" | "heb" => "Hebrew",
        "fa" | "per" | "fas" => "Persian",
        "hi" | "hin" => "Hindi",
        "bn" | "ben" => "Bengali",
        "ta" | "tam" => "Tamil",
        "te" | "tel" => "Telugu",
        "ca" | "cat" => "Catalan",
        "fil" | "tl" | "tgl" => "Filipino",
        _ => return None,
    };
    match parts.next() {
        Some(region) => Some(format!("{name} ({})", region_name(region))),
        None => Some(name.to_string()),
    }
}

fn region_name(code: &str) -> String {
    let name = match code.to_uppercase().as_str() {
        "BR" => "Brazil",
        "PT" => "Portugal",
        "419" => "Latin America",
        "ES" => "Spain",
        "MX" => "Mexico",
        "US" => "US",
        "GB" => "UK",
        "CA" => "Canada",
        "FR" => "France",
        "AU" => "Australia",
        "IN" => "India",
        "CN" => "China",
        "TW" => "Taiwan",
        "HK" => "Hong Kong",
        "HANS" => "Simplified",
        "HANT" => "Traditional",
        other => return other.to_string(),
    };
    name.to_string()
}

fn read_playlist(mpv: &Mpv) -> PlaylistRead {
    let count = mpv
        .get_string("playlist/count")
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or(0);
    let (items, filenames): (Vec<PlaylistItem>, Vec<String>) = (0..count)
        .map(|index| {
            let field = |name: &str| mpv.get_string(&format!("playlist/{index}/{name}"));
            let filename = field("filename").unwrap_or_default();
            // mpv only knows a file's title once it has played it, so the pre-read one fills in before that.
            let title = field("title")
                .filter(|title| !title.is_empty())
                .or_else(|| audio_info::title(&filename).flatten())
                .unwrap_or_else(|| file_display_name(&filename));
            let item = PlaylistItem {
                title: title.into(),
                current: field("current").as_deref() == Some("yes"),
                failed: opener::has_failed(&filename),
            };
            (item, filename)
        })
        .unzip();
    let current = items.iter().position(|item| item.current);
    let next_title = current
        .and_then(|current| items.get(current + 1))
        .map(|item| item.title.to_string())
        .unwrap_or_default();
    let upcoming = current
        .map(|current| {
            let next = (current..filenames.len()).take(READ_AHEAD_NEXT + 1);
            let previous = (0..current).rev().take(READ_AHEAD_PREVIOUS);
            next.chain(previous).map(|index| filenames[index].clone()).collect()
        })
        .unwrap_or_default();
    PlaylistRead { items, next_title, upcoming, files: filenames }
}

fn apply_playlist(player: &Player, playlist: PlaylistRead) {
    audio_info::prefetch(playlist.upcoming);
    audio_info::read_titles(&playlist.files);
    player.set_next_title(playlist.next_title.into());
    player.set_playlist(ModelRc::new(VecModel::from(playlist.items)));
}

// mpv matches metadata keys without case, and files mix "title", "Title" and "TITLE".
fn read_tags(mpv: &Mpv) -> AudioTags {
    let tag = |key: &str| {
        mpv.get_string(&format!("metadata/by-key/{key}"))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let artist = tag("artist").or_else(|| tag("album_artist")).unwrap_or_default();
    let year = tag("date").or_else(|| tag("year")).map(|date| date.chars().take(4).collect::<String>());
    let album_line = match (tag("album"), year) {
        (Some(album), Some(year)) => format!("{album} · {year}"),
        (Some(album), None) => album,
        (None, Some(year)) => year,
        (None, None) => String::new(),
    };
    AudioTags { artist, album_line }
}

fn apply_tags(player: &Player, tags: AudioTags) {
    if player.get_phase() != Phase::Playing {
        return;
    }
    player.set_audio_artist(tags.artist.into());
    player.set_audio_album(tags.album_line.into());
}

fn read_chapters(mpv: &Mpv) -> Vec<ChapterItem> {
    let count = mpv
        .get_string("chapter-list/count")
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or(0);
    (0..count)
        .filter_map(|index| {
            let field = |name: &str| mpv.get_string(&format!("chapter-list/{index}/{name}"));
            let time = field("time")?.parse::<f32>().ok()?;
            let title = field("title").filter(|title| !is_placeholder_chapter_title(title)).unwrap_or_default();
            Some(ChapterItem { time, title: title.into() })
        })
        .collect()
}

// Releases often fill unnamed chapters with "Chapter" or "Chapter 3", which says nothing about the content.
fn is_placeholder_chapter_title(title: &str) -> bool {
    let title = title.trim();
    let Some(rest) = title.get(..7).filter(|start| start.eq_ignore_ascii_case("chapter")).map(|_| &title[7..])
    else {
        return false;
    };
    rest.trim().chars().all(|character| character.is_ascii_digit())
}

fn apply_chapters(player: &Player, chapters: Vec<ChapterItem>) {
    player.set_chapters(ModelRc::new(VecModel::from(chapters)));
}

fn chapter_title_at(player: &Player, seconds: f32) -> SharedString {
    player
        .get_chapters()
        .iter()
        .filter(|chapter| chapter.time <= seconds)
        .max_by(|a, b| a.time.total_cmp(&b.time))
        .map(|chapter| chapter.title)
        .unwrap_or_default()
}

// A title taken from a URL file name still has %20 and friends, a real title has spaces.
fn readable_title(title: String) -> String {
    if title.contains('%') && !title.contains(' ') {
        percent_decode(&title)
    } else {
        title
    }
}

fn file_display_name(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    percent_decode(name)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                decoded.push(byte);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn subtitle_codec_label(codec: &str) -> String {
    match codec {
        "ass" | "ssa" => "ASS".into(),
        "subrip" => "SRT".into(),
        "webvtt" => "VTT".into(),
        "hdmv_pgs_subtitle" => "PGS".into(),
        "dvd_subtitle" => "VobSub".into(),
        other => other.to_uppercase(),
    }
}

// mpv drops old ranges our byte cache still holds, so the bar shows both.
fn buffered_ranges(cache_state: Option<&Node>, state: &EventState, duration: f32) -> Vec<LoadedRange> {
    if duration <= 0.0 {
        return Vec::new();
    }
    let path = state.path.lock().map(|path| path.clone()).unwrap_or_default();
    let mut seconds = cache_state.map(buffered::mpv_ranges).unwrap_or_default();
    if let Ok(mut byte_times) = state.byte_times.lock() {
        let reading = cache_state.and_then(buffered::cache_end).zip(http_cache::read_position(&path));
        if let Some((time, bytes)) = reading {
            byte_times.observe(bytes, time);
        }
        for (start, end) in http_cache::cached_byte_ranges(&path) {
            seconds.extend(byte_times.to_time(start, end));
        }
    }
    buffered::merge(seconds)
        .into_iter()
        .map(|(start, end)| {
            let duration = duration as f64;
            LoadedRange {
                start: (start / duration).clamp(0.0, 1.0) as f32,
                end: (end / duration).clamp(0.0, 1.0) as f32,
            }
        })
        .collect()
}

fn same_ranges(current: &ModelRc<LoadedRange>, ranges: &[LoadedRange]) -> bool {
    current.row_count() == ranges.len() && current.iter().zip(ranges).all(|(a, b)| a == *b)
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

fn update_time_text(player: &Player) {
    let text = format!("{} / {}", format_time(player.get_position()), format_time(player.get_duration()));
    player.set_time_text(text.into());
}

fn format_time(seconds: f32) -> String {
    let total = seconds.max(0.0) as u64;
    let (hours, minutes, secs) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

fn speed_label(speed: f32) -> String {
    format!("{speed}x")
}

fn video_codec_label(format: &str) -> String {
    match format {
        "" => String::new(),
        "h264" => "AVC".into(),
        "hevc" => "HEVC".into(),
        "av1" => "AV1".into(),
        "vp9" => "VP9".into(),
        "vp8" => "VP8".into(),
        "mpeg2video" => "MPEG-2".into(),
        other => other.to_uppercase(),
    }
}

fn audio_badge(info: &AudioInfo) -> String {
    audio_label(&info.codec, info.channels)
}

fn audio_label(codec: &str, channels: i64) -> String {
    let codec = match codec {
        "" => return String::new(),
        "aac" => "AAC".to_string(),
        "ac3" => "AC-3".to_string(),
        "eac3" => "E-AC-3".to_string(),
        "truehd" => "TrueHD".to_string(),
        "opus" => "Opus".to_string(),
        "vorbis" => "Vorbis".to_string(),
        other => other.to_uppercase(),
    };
    let layout = match channels {
        0 => return codec,
        1 => "1.0".to_string(),
        2 => "2.0".to_string(),
        6 => "5.1".to_string(),
        8 => "7.1".to_string(),
        count => format!("{count}ch"),
    };
    format!("{codec} {layout}")
}

// Capture mode: once the file plays, turn subtitles off, pause and seek, then let the frame settle.
fn advance_capture(mpv: &Mpv, app: &AppWindow, state: &EventState) {
    let Ok(mut capture) = state.capture.lock() else { return };
    let Some(capture) = capture.as_mut() else { return };
    match capture.stage {
        capture::Stage::WaitingForFile => {
            log_error(mpv.command(&["set", "sid", "no"]));
            log_error(mpv.command(&["set", "pause", "yes"]));
            log_error(mpv.command(&["seek", &capture.at.to_string(), "absolute+exact"]));
            capture.stage = capture::Stage::Seeking;
        }
        capture::Stage::Seeking => {
            if capture.size.is_none() {
                let dimension = |name: &str| mpv.get_string(name).and_then(|value| value.parse::<i32>().ok());
                capture.size = dimension("video-params/dw").zip(dimension("video-params/dh"));
            }
            capture.stage = capture::Stage::Resizing(0);
            schedule_video_redraw(&app.as_weak());
        }
        _ => {}
    }
}

fn capture_shown_frame(video: &VideoLayer, window: (i32, i32), area: Area, state: &EventState, app_weak: &slint::Weak<AppWindow>) {
    let Ok(mut capture) = state.capture.lock() else { return };
    let Some(capture) = capture.as_mut() else { return };

    if let capture::Stage::Resizing(attempts) = capture.stage {
        let wanted = capture.size.unwrap_or((area.width, area.height));
        let reached = (area.width, area.height) == wanted;
        if reached || attempts >= capture::RESIZE_ATTEMPTS {
            if !reached {
                eprintln!("the video area stayed {}x{} instead of {}x{}", area.width, area.height, wanted.0, wanted.1);
            }
            capture.stage = capture::Stage::Settling(0);
        } else {
            if let Some(app) = app_weak.upgrade() {
                let width = (window.0 + wanted.0 - area.width).max(1) as u32;
                let height = (window.1 + wanted.1 - area.height).max(1) as u32;
                app.window().set_size(slint::PhysicalSize::new(width, height));
            }
            capture.stage = capture::Stage::Resizing(attempts + 1);
        }
        schedule_video_redraw(app_weak);
        return;
    }

    let capture::Stage::Settling(frames) = capture.stage else { return };
    if frames < capture::SETTLE_FRAMES {
        capture.stage = capture::Stage::Settling(frames + 1);
        schedule_video_redraw(app_weak);
        return;
    }
    match video.read_shown_video(window, area) {
        Some((width, height, pixels)) => match capture::save_png(&capture.path, width, height, &pixels) {
            Ok(()) => eprintln!("captured {width}x{height} to {}", capture.path.display()),
            Err(err) => eprintln!("could not save the capture: {err}"),
        },
        None => eprintln!("no video frame to capture"),
    }
    capture.stage = capture::Stage::Done;
    let _ = slint::quit_event_loop();
}

fn attach_video(
    mpv: &Mpv,
    app: &AppWindow,
    loader: opener::Loader,
    event_state: Arc<EventState>,
    renderer: video::Renderer,
) -> Result<(), slint::SetRenderingNotifierError> {
    let mpv = mpv.clone();
    let app_weak = app.as_weak();
    let mut video: Option<VideoLayer> = None;
    let mut showed_cover = false;

    app.window().set_rendering_notifier(move |state, graphics_api| match state {
        RenderingState::RenderingSetup => {
            let GraphicsAPI::NativeOpenGL { get_proc_address } = graphics_api else {
                eprintln!("video playback needs an OpenGL renderer");
                return;
            };
            let redraw_weak = app_weak.clone();
            match VideoLayer::new(&mpv, renderer, *get_proc_address, move || schedule_video_redraw(&redraw_weak)) {
                Ok(layer) => video = Some(layer),
                Err(err) => {
                    eprintln!("{err}");
                    return;
                }
            }
            loader.render_ready(&mpv);
            let shadow_weak = app_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = shadow_weak.upgrade() {
                    enable_window_shadow(&app);
                    fill_monitor_when_fullscreen(&app);
                    set_window_icons(&app);
                }
            });
        }
        RenderingState::BeforeRendering => {
            if let (Some(video), Some(app)) = (&mut video, app_weak.upgrade()) {
                let window = app.window();
                let size = window.size();
                let scale = window.scale_factor();
                let player = app.global::<Player>();
                // Not tied to the phase, so mpv already renders the cover at its size behind the load screen.
                // Between two audio files these still describe the previous one, so its cover keeps its square.
                let showing_cover = player.get_audio_only() && player.get_has_cover() && !player.get_has_cover_image();
                // Until the new file's tracks are known its first frame could be anything, and until playback
                // starts mpv may have no frame yet and would render black, so the old frame stays.
                let hold_frame = !player.get_tracks_known() || player.get_phase() == Phase::Opening;
                // Switching between the video area and the cover is not a window resize, so it doesn't wait to settle.
                let switched_area = showing_cover != showed_cover;
                showed_cover = showing_cover;
                let backdrop =
                    if showing_cover { app.global::<Theme>().get_backdrop() } else { slint::Color::from_rgb_u8(0, 0, 0) };
                // mpv fills its buffer around a cover that isn't square with this color.
                if switched_area {
                    let hex = format!("#{:02x}{:02x}{:02x}", backdrop.red(), backdrop.green(), backdrop.blue());
                    log_error(mpv.command(&["set", "background-color", &hex]));
                }
                let area = if showing_cover {
                    let size = (app.get_cover_size() * scale).round() as i32;
                    Area {
                        x: (app.get_cover_x() * scale).round() as i32,
                        y: (app.get_cover_y() * scale).round() as i32,
                        width: size,
                        height: size,
                    }
                } else {
                    Area {
                        x: 0,
                        y: (app.get_video_y() * scale).round() as i32,
                        width: (app.get_video_width() * scale).round() as i32,
                        height: (app.get_video_height() * scale).round() as i32,
                    }
                };
                let window_size = (size.width as i32, size.height as i32);
                let picture = *event_state.picture.lock().unwrap();
                let clear = [backdrop.red(), backdrop.green(), backdrop.blue()].map(|channel| f32::from(channel) / 255.0);
                log_error(video.draw(window_size, area, picture, switched_area, hold_frame, clear));
                capture_shown_frame(video, window_size, area, &event_state, &app_weak);
                if video.resize_pending() {
                    schedule_video_redraw(&app_weak);
                }
            }
        }
        RenderingState::RenderingTeardown => video = None,
        _ => {}
    })
}

fn picture_from_osd_dimensions(dimensions: &Node) -> Option<Picture> {
    let value = |key: &str| dimensions.get(key).and_then(Node::number).map(|number| number.round() as i32);
    Some(Picture {
        buffer: (value("w")?, value("h")?),
        left: value("ml")?,
        top: value("mt")?,
        right: value("mr")?,
        bottom: value("mb")?,
    })
}

fn schedule_video_redraw(app_weak: &slint::Weak<AppWindow>) {
    let app_weak = app_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = app_weak.upgrade() {
            request_video_redraw(&app);
        }
    });
}

// Slint's request_redraw waits on a timer that never fires inside the Windows move/resize loop.
fn request_video_redraw(app: &AppWindow) {
    app.window().with_winit_window(|window| window.request_redraw());
}

fn wire_playback(mpv: &Mpv, app: &AppWindow, osd: &Osd, config: &SharedConfig) {
    let player = app.global::<Player>();

    player.on_format_time(|seconds| format_time(seconds).into());

    let app_weak = app.as_weak();
    player.on_chapter_title_at(move |seconds| {
        app_weak.upgrade().map(|app| chapter_title_at(&app.global::<Player>(), seconds)).unwrap_or_default()
    });

    let (pause_mpv, app_weak) = (mpv.clone(), app.as_weak());
    player.on_toggle_pause(move || {
        if app_weak.upgrade().is_some_and(|app| app.global::<Player>().get_ended()) {
            log_error(pause_mpv.command(&["seek", "0", "absolute"]));
            log_error(pause_mpv.command(&["set", "pause", "no"]));
        } else {
            log_error(pause_mpv.command(&["cycle", "pause"]));
        }
    });

    let seek_mpv = mpv.clone();
    player.on_seek(move |fraction| {
        let percent = (fraction * 100.0).to_string();
        log_error(seek_mpv.command(&["seek", &percent, "absolute-percent+exact"]));
    });

    let preview_mpv = mpv.clone();
    player.on_seek_preview(move |fraction| {
        let percent = (fraction * 100.0).to_string();
        log_error(preview_mpv.command(&["seek", &percent, "absolute-percent+keyframes"]));
    });

    let previous_mpv = mpv.clone();
    player.on_previous(move || log_error(previous_mpv.command(&["playlist-prev"])));

    let next_mpv = mpv.clone();
    player.on_next(move || log_error(next_mpv.command(&["playlist-next"])));

    let subtitle_mpv = mpv.clone();
    player.on_select_subtitle(move |id| {
        let id = if id == 0 { "no".to_string() } else { id.to_string() };
        log_error(subtitle_mpv.command(&["set", "sid", &id]));
    });

    let (hide_mpv, hide_osd, app_weak) = (mpv.clone(), osd.clone(), app.as_weak());
    player.on_toggle_subtitles(move || {
        let Some(app) = app_weak.upgrade() else { return };
        let player = app.global::<Player>();
        let hidden = !player.get_subtitles_hidden();
        log_error(hide_mpv.command(&["set", "sub-visibility", if hidden { "no" } else { "yes" }]));
        player.set_subtitles_hidden(hidden);
        hide_osd.show(if hidden { "Subtitles hidden" } else { "Subtitles shown" });
    });

    let audio_mpv = mpv.clone();
    player.on_select_audio(move |id| log_error(audio_mpv.command(&["set", "aid", &id.to_string()])));

    let entry_mpv = mpv.clone();
    player.on_play_entry(move |index| log_error(entry_mpv.command(&["playlist-play-index", &index.to_string()])));

    let (mute_mpv, mute_osd, app_weak) = (mpv.clone(), osd.clone(), app.as_weak());
    player.on_toggle_mute(move || {
        let Some(app) = app_weak.upgrade() else { return };
        let player = app.global::<Player>();
        let muted = !player.get_muted();
        log_error(mute_mpv.command(&["set", "mute", if muted { "yes" } else { "no" }]));
        player.set_muted(muted);
        if muted {
            mute_osd.show("Muted");
        } else {
            mute_osd.show(format!("Volume {}%", (player.get_volume() * 100.0).round()));
        }
    });

    let (volume_mpv, volume_osd, app_weak) = (mpv.clone(), osd.clone(), app.as_weak());
    player.on_set_volume(move |level| {
        if let Some(app) = app_weak.upgrade() {
            set_volume(&volume_mpv, &app.global::<Player>(), &volume_osd, level);
        }
    });

    let (step_mpv, step_osd, app_weak) = (mpv.clone(), osd.clone(), app.as_weak());
    player.on_change_volume(move |delta| {
        if let Some(app) = app_weak.upgrade() {
            let player = app.global::<Player>();
            let level = player.get_volume() + delta;
            set_volume(&step_mpv, &player, &step_osd, level);
        }
    });

    let (speed_mpv, speed_osd, app_weak, speed_config) = (mpv.clone(), osd.clone(), app.as_weak(), config.clone());
    player.on_cycle_speed(move || {
        let Some(app) = app_weak.upgrade() else { return };
        let current = app.global::<Player>().get_speed();
        let next = {
            let speeds = &speed_config.borrow().speeds;
            speeds
                .iter()
                .position(|speed| (speed - current).abs() < 0.01)
                .map_or(speeds[0], |index| speeds[(index + 1) % speeds.len()])
        };
        log_error(speed_mpv.command(&["set", "speed", &next.to_string()]));
        speed_osd.show(format!("Speed {}", speed_label(next)));
    });

    let (key_mpv, app_weak, key_config) = (mpv.clone(), app.as_weak(), config.clone());
    player.on_key_pressed(move |text, shift| {
        let Some(app) = app_weak.upgrade() else { return false };
        shortcuts::handle(&text, shift, &key_mpv, &app, &key_config)
    });
}

fn watch_download(app: &AppWindow, state: &Arc<EventState>) -> Timer {
    let app_weak = app.as_weak();
    let state = state.clone();
    let mut samples: VecDeque<(Instant, u64)> = VecDeque::new();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, DOWNLOAD_POLL_INTERVAL, move || {
        let Some(app) = app_weak.upgrade() else { return };
        let player = app.global::<Player>();

        let now = Instant::now();
        samples.push_back((now, http_cache::downloaded_bytes()));
        while samples.front().is_some_and(|(time, _)| now - *time > DOWNLOAD_SPEED_WINDOW) {
            samples.pop_front();
        }
        let speed = match (samples.front(), samples.back()) {
            (Some((first_time, first_bytes)), Some((last_time, last_bytes))) if last_time > first_time => {
                (last_bytes - first_bytes) as f64 / (*last_time - *first_time).as_secs_f64()
            }
            _ => 0.0,
        };
        player.set_download_speed(if speed > 0.0 { speed_text(speed) } else { String::new() }.into());
        update_loading_status(&player, &state);
    });
    timer
}

fn update_loading_status(player: &Player, state: &EventState) {
    let phase = player.get_phase();
    let link = player.get_source_is_link();
    let started = state.open_started.lock().ok().and_then(|started| *started);
    let (stage, slow) = match started {
        Some((at, bytes_at_start)) if phase == Phase::Opening && link => {
            let received = http_cache::downloaded_bytes().saturating_sub(bytes_at_start);
            let waited = at.elapsed().as_secs();
            match received {
                0 => ("Connecting".to_string(), if waited >= SLOW_AFTER_SECS { waited } else { 0 }),
                _ => (format!("Reading file info · {}", byte_count_text(received)), 0),
            }
        }
        _ => (String::new(), 0),
    };
    player.set_opening_stage(stage.into());
    player.set_slow_seconds(slow as i32);

    // Reconnecting stays silent while playback goes on from mpv's buffer.
    let waiting_on_link = phase == Phase::Playing && link && player.get_buffering();
    let stalled = if waiting_on_link {
        http_cache::seconds_without_data().filter(|seconds| *seconds >= STALLED_AFTER_SECS).unwrap_or(0)
    } else {
        0
    };
    player.set_stalled_seconds(stalled as i32);
    let (lost, retry_in) = match http_cache::connection_state() {
        _ if !waiting_on_link => (false, 0),
        ConnectionState::Fine => (false, 0),
        ConnectionState::Retrying { seconds_left } => (true, seconds_left),
    };
    player.set_connection_lost(lost);
    player.set_retry_in(retry_in as i32);
}

fn byte_count_text(bytes: u64) -> String {
    const MEGABYTE: u64 = 1024 * 1024;
    if bytes >= MEGABYTE {
        format!("{:.1} MB", bytes as f64 / MEGABYTE as f64)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

fn speed_text(bytes_per_second: f64) -> String {
    const MEGABYTE: f64 = 1024.0 * 1024.0;
    if bytes_per_second >= MEGABYTE {
        format!("{:.1} MB/s", bytes_per_second / MEGABYTE)
    } else {
        format!("{:.0} KB/s", bytes_per_second / 1024.0)
    }
}

fn set_volume(mpv: &Mpv, player: &Player, osd: &Osd, level: f32) {
    let level = level.clamp(0.0, player.get_volume_limit());
    let percent = (level * 100.0).round();
    log_error(mpv.command(&["set", "volume", &percent.to_string()]));
    log_error(mpv.command(&["set", "mute", "no"]));
    volume::remember(percent as u32);
    player.set_volume(level);
    player.set_muted(false);
    osd.show(format!("Volume {percent}%"));
}

fn toggle_fullscreen(app: &AppWindow) {
    let controls = app.global::<WindowControls>();
    let fullscreen = !controls.get_fullscreen();
    app.window().set_fullscreen(fullscreen);
    controls.set_fullscreen(fullscreen);
    controls.invoke_activity();
}

// Pinned keeps the window on top, the "while playing" setting only while a video plays.
fn update_window_level(app: &AppWindow) {
    let pinned = app.global::<WindowControls>().get_pinned();
    let while_playing = app.global::<Settings>().get_on_top_while_playing() && !app.global::<Player>().get_paused();
    let level = if pinned || while_playing { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal };
    app.window().with_winit_window(|window| window.set_window_level(level));
}

fn wire_window_controls(app: &AppWindow, osd: &Osd, config: &SharedConfig, saved_state: &SharedState) {
    let controls = app.global::<WindowControls>();

    // Slint keeps only one winit event callback per window, so every handler goes through this one.
    let app_weak = app.as_weak();
    let mut handle_drop = opener::drop_handler(app);
    app.window().on_winit_window_event(move |_, event| {
        if let WindowEvent::MouseInput { state: ElementState::Pressed, .. } = event
            && let Some(app) = app_weak.upgrade()
        {
            app.invoke_release_text_focus();
        }
        handle_drop(event);
        EventResult::Propagate
    });

    let (pointer_timer, app_weak, pointer_config) = (Timer::default(), app.as_weak(), config.clone());
    controls.on_activity(move || {
        let Some(app) = app_weak.upgrade() else { return };
        let controls = app.global::<WindowControls>();
        controls.set_pointer_active(true);
        if !controls.get_fullscreen() {
            pointer_timer.stop();
            return;
        }
        let hide_weak = app_weak.clone();
        let delay = pointer_config.borrow().pointer_hide_delay();
        pointer_timer.start(TimerMode::SingleShot, delay, move || {
            if let Some(app) = hide_weak.upgrade() {
                let controls = app.global::<WindowControls>();
                if controls.get_fullscreen() {
                    controls.set_pointer_active(false);
                }
            }
        });
    });

    let app_weak = app.as_weak();
    controls.on_toggle_fullscreen(move || {
        if let Some(app) = app_weak.upgrade() {
            toggle_fullscreen(&app);
        }
    });

    let app_weak = app.as_weak();
    controls.on_start_drag(move || {
        if let Some(app) = app_weak.upgrade() {
            log_window_error(app.window().with_winit_window(|window| window.drag_window()));
        }
    });

    let app_weak = app.as_weak();
    controls.on_start_resize(move |edge| {
        if let Some(app) = app_weak.upgrade() {
            let direction = resize_direction(edge);
            log_window_error(
                app.window().with_winit_window(|window| window.drag_resize_window(direction)),
            );
        }
    });

    let app_weak = app.as_weak();
    controls.on_minimize(move || {
        if let Some(app) = app_weak.upgrade() {
            app.window().set_minimized(true);
        }
    });

    let app_weak = app.as_weak();
    controls.on_toggle_maximize(move || {
        let Some(app) = app_weak.upgrade() else { return };
        if app.global::<WindowControls>().get_fullscreen() {
            toggle_fullscreen(&app);
            return;
        }
        let window = app.window();
        window.set_maximized(!window.is_maximized());
    });

    let (app_weak, close_config, close_state) = (app.as_weak(), config.clone(), saved_state.clone());
    controls.on_close(move || {
        if let Some(app) = app_weak.upgrade() {
            resume::save_now();
            volume::save_pending();
            if close_config.borrow().remember_window {
                state::remember_window(&app, &close_state);
            }
            log_error(app.window().hide());
        }
    });

    let app_weak = app.as_weak();
    controls.on_state_changed(move || {
        let Some(app) = app_weak.upgrade() else { return };
        let controls = app.global::<WindowControls>();
        controls.set_maximized(app.window().is_maximized());
        let fullscreen = app.window().is_fullscreen();
        if controls.get_fullscreen() != fullscreen {
            controls.set_fullscreen(fullscreen);
            controls.invoke_activity();
        }
    });

    let (pin_osd, app_weak) = (osd.clone(), app.as_weak());
    controls.on_toggle_pin(move || {
        let Some(app) = app_weak.upgrade() else { return };
        let controls = app.global::<WindowControls>();
        let pinned = !controls.get_pinned();
        controls.set_pinned(pinned);
        update_window_level(&app);
        pin_osd.show(if pinned { "Keep on top: on" } else { "Keep on top: off" });
    });
}

fn resize_direction(edge: ResizeEdge) -> ResizeDirection {
    match edge {
        ResizeEdge::North => ResizeDirection::North,
        ResizeEdge::South => ResizeDirection::South,
        ResizeEdge::East => ResizeDirection::East,
        ResizeEdge::West => ResizeDirection::West,
        ResizeEdge::NorthEast => ResizeDirection::NorthEast,
        ResizeEdge::NorthWest => ResizeDirection::NorthWest,
        ResizeEdge::SouthEast => ResizeDirection::SouthEast,
        ResizeEdge::SouthWest => ResizeDirection::SouthWest,
    }
}

#[cfg(target_os = "windows")]
fn enable_window_shadow(app: &AppWindow) {
    use slint::winit_030::winit::platform::windows::WindowExtWindows;
    app.window().with_winit_window(|window| window.set_undecorated_shadow(true));
}

#[cfg(not(target_os = "windows"))]
fn enable_window_shadow(_app: &AppWindow) {}

// winit clamps a maximized frameless window to the work area even in fullscreen (winit#4756).
#[cfg(target_os = "windows")]
fn fill_monitor_when_fullscreen(app: &AppWindow) {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::SetWindowSubclass;

    app.window().with_winit_window(|window| {
        let Ok(handle) = window.window_handle() else { return };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
        let hwnd = HWND(handle.hwnd.get() as *mut std::ffi::c_void);
        if !unsafe { SetWindowSubclass(hwnd, Some(fullscreen_client_area), 1, 0) }.as_bool() {
            eprintln!("could not hook the window for fullscreen sizing");
        }
    });
}

#[cfg(not(target_os = "windows"))]
fn fill_monitor_when_fullscreen(_app: &AppWindow) {}

#[cfg(target_os = "windows")]
unsafe extern "system" fn fullscreen_client_area(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    _subclass_id: usize,
    _data: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect};
    use windows::Win32::UI::Shell::DefSubclassProc;
    use windows::Win32::UI::WindowsAndMessaging::{GWL_STYLE, GetWindowLongW, NCCALCSIZE_PARAMS, WM_NCCALCSIZE, WS_CAPTION};

    // winit removes WS_CAPTION only while fullscreen.
    let fullscreen = || unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32 & WS_CAPTION.0 == 0;
    if message == WM_NCCALCSIZE && wparam.0 != 0 && fullscreen() {
        let params = unsafe { &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS) };
        let monitor = unsafe { MonitorFromRect(&params.rgrc[0], MONITOR_DEFAULTTONEAREST) };
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            params.rgrc[0] = info.rcMonitor;
            return LRESULT(0);
        }
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

// Rendered from assets/icon.svg by build.rs, as straight RGBA.
const WINDOW_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/icon-32.rgba"));
#[cfg(target_os = "windows")]
const TASKBAR_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/icon-256.rgba"));

fn window_icon(rgba: &[u8], size: u32) -> Option<slint::winit_030::winit::window::Icon> {
    slint::winit_030::winit::window::Icon::from_rgba(rgba.to_vec(), size, size)
        .map_err(|err| eprintln!("could not use the window icon: {err}"))
        .ok()
}

// Windows takes the large taskbar icon separately from the small window icon.
fn set_window_icons(app: &AppWindow) {
    app.window().with_winit_window(|window| {
        window.set_window_icon(window_icon(WINDOW_ICON, 32));
        #[cfg(target_os = "windows")]
        {
            use slint::winit_030::winit::platform::windows::WindowExtWindows;
            window.set_taskbar_icon(window_icon(TASKBAR_ICON, 256));
        }
    });
}

fn log_window_error(result: Option<Result<(), ExternalError>>) {
    if let Some(Err(err)) = result {
        eprintln!("{err}");
    }
}

fn log_error(result: Result<(), impl Display>) {
    if let Err(err) = result {
        eprintln!("{err}");
    }
}
