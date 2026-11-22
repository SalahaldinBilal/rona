use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use slint::{ComponentHandle, Model, SharedString, Timer, TimerMode};

use crate::logging::{debug, info, warn};
use crate::mkv_cut;
use crate::mpv::{Event, Mpv, PropertyValue};
use crate::recorder_window;
use crate::{AppWindow, EventState, Player, Recorder as RecorderUi, format_time, http_cache, log_error};

pub const DUMP_COMMAND_ID: u64 = 1;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const MEGABYTE: u64 = 1024 * 1024;
const ENCODER_TIME_POS: u64 = 1;
// mpv keeps 50 MiB of played data by default, too little to dump a long recording afterwards.
const RECORDING_BACK_BUFFER: &str = "4GiB";
const DEFAULT_BACK_BUFFER: &str = "50MiB";

enum Split {
    Off,
    Seconds(f32),
}

struct Session {
    start: f32,
    output: Output,
}

enum Output {
    Copy { base: PathBuf, extension: String, split: Split, matroska: Option<Matroska> },
    Encode { source: String, settings: EncodeSettings, subtitle: Option<i32>, audio: Option<i32>, path: PathBuf },
}

struct Matroska {
    source: String,
    start_time: f64,
}

struct EncodeSettings {
    extension: &'static str,
    video_codec: &'static str,
    video_kbps: Option<u32>,
    height: Option<u32>,
    audio_codec: &'static str,
    audio_bitrate: u32,
    burn_subtitles: bool,
}

struct Dump {
    pending: VecDeque<(f32, f32, PathBuf)>,
    current: Option<PathBuf>,
    total: usize,
    written: Vec<PathBuf>,
    failed: Option<String>,
}

struct EncodeJob {
    encoder: Mpv,
    output: PathBuf,
    progress: Arc<AtomicU32>,
    finished: Arc<AtomicBool>,
}

pub struct Controller {
    app: slint::Weak<AppWindow>,
    mpv: Mpv,
    state: Arc<EventState>,
    session: Option<Session>,
    dump: Option<Dump>,
    job: Option<EncodeJob>,
    poll: Timer,
}

// The recorder shows in the main window's sidebar or in its own window, and each has its own Recorder global.
// Status goes to both, fields are read from the one showing.
fn show(app: &AppWindow, set: impl Fn(&RecorderUi)) {
    set(&app.global::<RecorderUi>());
    recorder_window::with_ui(&set);
}

fn show_status(app: &AppWindow, text: impl Into<SharedString>) {
    let text = text.into();
    show(app, |ui| ui.set_status_text(text.clone()));
}

fn show_text(app: &AppWindow, text: impl Into<SharedString>, set: impl Fn(&RecorderUi, SharedString)) {
    let text = text.into();
    show(app, |ui| set(ui, text.clone()));
}

fn set_callbacks(ui: &RecorderUi, toggle: &Rc<dyn Fn()>, open: &Rc<dyn Fn()>, browse: &Rc<dyn Fn()>) {
    let (toggle, open, browse) = (toggle.clone(), open.clone(), browse.clone());
    ui.on_toggle_recording(move || toggle());
    ui.on_open_folder(move || open());
    ui.on_browse_folder(move || browse());
}

pub fn wire(app: &AppWindow, mpv: &Mpv, state: Arc<EventState>) -> Rc<RefCell<Controller>> {
    if app.global::<RecorderUi>().get_folder().is_empty() {
        show_text(app, default_folder().to_string_lossy().as_ref(), |ui, text| ui.set_folder(text));
    }

    let controller = Rc::new(RefCell::new(Controller {
        app: app.as_weak(),
        mpv: mpv.clone(),
        state,
        session: None,
        dump: None,
        job: None,
        poll: Timer::default(),
    }));

    let weak = Rc::downgrade(&controller);
    let toggle: Rc<dyn Fn()> = Rc::new(move || with_controller(&weak, Controller::toggle));
    let app_weak = app.as_weak();
    let open: Rc<dyn Fn()> = Rc::new(move || {
        if let Some(app) = app_weak.upgrade() {
            open_folder(Path::new(recorder_window::read(&app, |ui| ui.get_folder()).as_str()));
        }
    });
    let app_weak = app.as_weak();
    let browse: Rc<dyn Fn()> = Rc::new(move || {
        let Some(app) = app_weak.upgrade() else { return };
        let current = PathBuf::from(recorder_window::read(&app, |ui| ui.get_folder()).as_str());
        let app_weak = app_weak.clone();
        // The native dialog blocks its thread, which must not be the UI thread that draws the video.
        std::thread::spawn(move || {
            let Some(folder) = rfd::FileDialog::new().set_directory(&current).pick_folder() else { return };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = app_weak.upgrade() {
                    show_text(&app, folder.to_string_lossy().as_ref(), |ui, text| ui.set_folder(text));
                }
            });
        });
    });
    set_callbacks(&app.global::<RecorderUi>(), &toggle, &open, &browse);
    recorder_window::with_ui(|ui| set_callbacks(ui, &toggle, &open, &browse));

    let weak = Rc::downgrade(&controller);
    controller
        .borrow()
        .poll
        .start(TimerMode::Repeated, POLL_INTERVAL, move || with_controller(&weak, Controller::tick));
    controller
}

fn with_controller(weak: &Weak<RefCell<Controller>>, action: fn(&mut Controller)) {
    if let Some(controller) = weak.upgrade() {
        action(&mut controller.borrow_mut());
    }
}

impl Controller {
    fn toggle(&mut self) {
        if self.session.is_some() {
            self.stop();
        } else {
            self.start();
        }
    }

    fn start(&mut self) {
        let Some(app) = self.app.upgrade() else { return };
        let fields = recorder_window::read(&app, Fields::read);
        let player = app.global::<Player>();

        let source = self.state.path.lock().map(|path| path.clone()).unwrap_or_default();
        if source.is_empty() {
            show_status(&app, "Nothing is playing.");
            return;
        }
        let folder = PathBuf::from(fields.folder.as_str());
        if let Err(err) = std::fs::create_dir_all(&folder) {
            show_status(&app, format!("Could not create the folder: {err}"));
            return;
        }
        let base = folder.join(base_name(fields.prefix.as_str(), player.get_media_title().as_str()));

        let output = if fields.mode == 0 {
            let format = self.state.file_format.lock().map(|format| format.clone()).unwrap_or_default();
            let extension = copy_extension(&format).to_string();
            let split = match fields.split_mode {
                1 => Split::Seconds(parse_positive(fields.split_minutes.as_str()).unwrap_or(60) as f32 * 60.0),
                2 => {
                    let megabytes = parse_positive(fields.split_megabytes.as_str()).unwrap_or(500);
                    match average_bytes_per_second(&source, player.get_duration()) {
                        Some(rate) => Split::Seconds((megabytes * MEGABYTE) as f32 / rate),
                        None => Split::Off,
                    }
                }
                _ => Split::Off,
            };
            let matroska = if is_matroska(&format) {
                let Some(start_time) = self.state.start_time.lock().ok().and_then(|start_time| *start_time) else {
                    show_status(&app, "mpv didn't report where this file's timeline starts.");
                    return;
                };
                Some(Matroska { source: source.clone(), start_time })
            } else {
                log_error(self.mpv.command(&["set", "demuxer-max-back-bytes", RECORDING_BACK_BUFFER]));
                None
            };
            show_text(&app, file_name(&part_path(&base, &extension, 1, &split)), |ui, text| ui.set_current_file(text));
            Output::Copy { base, extension, split, matroska }
        } else {
            let settings = fields.encode;
            let path = with_extension(&base, settings.extension);
            show_text(&app, file_name(&path), |ui, text| ui.set_current_file(text));
            Output::Encode {
                source,
                subtitle: selected_track(&player.get_subtitle_tracks()),
                audio: selected_track(&player.get_audio_tracks()),
                settings,
                path,
            }
        };

        info!(
            "recording from {:.1} s, {}",
            player.get_position(),
            match &output {
                Output::Copy { extension, .. } => format!("copying to .{extension}"),
                Output::Encode { settings, .. } => format!("encoding with {} and {}", settings.video_codec, settings.audio_codec),
            }
        );
        self.session = Some(Session { start: player.get_position(), output });
        show(&app, |ui| ui.set_recording(true));
        show_text(&app, "0:00", |ui, text| ui.set_elapsed_text(text));
        show_text(&app, "", |ui, text| ui.set_size_text(text));
        show_status(&app, "Recording.");
    }

    fn stop(&mut self) {
        let Some(app) = self.app.upgrade() else { return };
        show(&app, |ui| ui.set_recording(false));
        let Some(Session { start, output, .. }) = self.session.take() else { return };
        let end = app.global::<Player>().get_position();
        info!("recording stopped at {end:.1} s, {:.1} s long", end - start);

        if end - start < 0.5 {
            show_status(&app, "The recorded part is too short.");
            if matches!(output, Output::Copy { matroska: None, .. }) {
                log_error(self.mpv.command(&["set", "demuxer-max-back-bytes", DEFAULT_BACK_BUFFER]));
            }
            return;
        }

        match output {
            Output::Copy { base, extension, split, matroska: Some(matroska) } => {
                cut_matroska(&app, matroska, start, end, base, extension, split);
            }
            Output::Copy { base, extension, split, matroska: None } => {
                let parts = split_range(start, end, &split)
                    .into_iter()
                    .enumerate()
                    .map(|(index, (from, to))| (from, to, part_path(&base, &extension, index as u32 + 1, &split)))
                    .collect::<VecDeque<_>>();
                let total = parts.len();
                self.dump = Some(Dump { pending: parts, current: None, total, written: Vec::new(), failed: None });
                show(&app, |ui| ui.set_encoding(true));
                self.dump_next();
            }
            Output::Encode { source, mut settings, subtitle, audio, path } => {
                if settings.video_kbps.is_none() {
                    let bits = self.state.video_bits_per_second.load(Ordering::Relaxed);
                    settings.video_kbps = (bits > 0).then(|| (bits as f64 / 1000.0).round() as u32);
                }
                match start_encoder(&source, start, end, &settings, subtitle, audio, &path) {
                    Ok(job) => {
                        info!("encoding to {}", job.output.display());
                        self.job = Some(job);
                        show(&app, |ui| ui.set_encoding(true));
                        show_status(&app, "Encoding 0%");
                    }
                    Err(err) => {
                        warn!("could not start encoding: {err}");
                        show_status(&app, format!("Could not start encoding: {err}"));
                    }
                }
            }
        }
    }

    // dump-cache commands run one at a time, a new one would cancel the one still writing.
    fn dump_next(&mut self) {
        let Some(app) = self.app.upgrade() else { return };
        let Some(dump) = &mut self.dump else { return };

        if let Some((from, to, path)) = dump.pending.pop_front() {
            let part = dump.total - dump.pending.len();
            let status = if dump.total > 1 { format!("Saving part {part} of {}.", dump.total) } else { "Saving.".into() };
            show_status(&app, status);
            show_text(&app, file_name(&path), |ui, text| ui.set_current_file(text));
            let (from_text, to_text) = (format!("{from:.3}"), format!("{to:.3}"));
            let path_text = path.to_string_lossy().into_owned();
            info!("saving part {part} of {}, {from_text} to {to_text} s, to {path_text}", dump.total);
            dump.current = Some(path);
            log_error(self.mpv.command_with_id(DUMP_COMMAND_ID, &["dump-cache", &from_text, &to_text, &path_text]));
            return;
        }

        finish_saving(&app, &dump.written, dump.failed.as_deref());
        self.dump = None;
        log_error(self.mpv.command(&["set", "demuxer-max-back-bytes", DEFAULT_BACK_BUFFER]));
    }

    fn tick(&mut self) {
        let Some(app) = self.app.upgrade() else { return };

        if let Some(session) = &self.session {
            let span = app.global::<Player>().get_position() - session.start;
            show_text(&app, format_time(span.max(0.0)), |ui, text| ui.set_elapsed_text(text));
        }

        let results: Vec<Option<String>> =
            self.state.dump_results.lock().map(|mut results| results.drain(..).collect()).unwrap_or_default();
        for result in results {
            let Some(dump) = &mut self.dump else { break };
            let Some(path) = dump.current.take() else { continue };
            match result {
                None => dump.written.push(path),
                Some(err) => {
                    dump.failed = Some(err);
                    dump.pending.clear();
                }
            }
            self.dump_next();
        }

        if let Some(job) = &self.job {
            if job.finished.load(Ordering::Relaxed) {
                if let Some(job) = self.job.take() {
                    finish_encoder(job, self.app.clone());
                }
            } else {
                let progress = f32::from_bits(job.progress.load(Ordering::Relaxed));
                show_status(&app, format!("Encoding {:.0}%", progress * 100.0));
            }
        }
    }
}

fn finish_saving(app: &AppWindow, written: &[PathBuf], failed: Option<&str>) {
    let status = match (failed, written.len()) {
        (Some(err), _) => format!("Saving failed: {err}"),
        (None, 1) => "Saved.".to_string(),
        (None, count) => format!("Saved {count} files."),
    };
    let size: u64 = written.iter().map(|path| file_size(path)).sum();
    match failed {
        Some(err) => warn!("saving the recording failed: {err}"),
        None => info!("recording saved, {} files, {}", written.len(), bytes_text(size)),
    }
    if let Some(last) = written.last() {
        show_text(app, file_name(last), |ui, text| ui.set_current_file(text));
    }
    show_status(app, status);
    show_text(app, bytes_text(size), |ui, text| ui.set_size_text(text));
    show(app, |ui| ui.set_encoding(false));
}

// Reads the source itself instead of mpv's cache, so it runs off the UI thread.
fn cut_matroska(app: &AppWindow, matroska: Matroska, start: f32, end: f32, base: PathBuf, extension: String, split: Split) {
    show(app, |ui| ui.set_encoding(true));
    show_status(app, "Saving.");
    let app_weak = app.as_weak();
    std::thread::spawn(move || {
        let split_seconds = match split {
            Split::Seconds(seconds) => Some(seconds as f64),
            Split::Off => None,
        };
        let progress_app = app_weak.clone();
        let (written, failed) = mkv_cut::cut(
            &matroska.source,
            start as f64 + matroska.start_time,
            end as f64 + matroska.start_time,
            split_seconds,
            |part| part_path(&base, &extension, part, &split),
            |fraction| {
                let app_weak = progress_app.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = app_weak.upgrade() {
                        show_status(&app, format!("Saving {:.0}%", fraction * 100.0));
                    }
                });
            },
        );
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = app_weak.upgrade() {
                finish_saving(&app, &written, failed.as_deref());
            }
        });
    });
}

fn split_range(start: f32, end: f32, split: &Split) -> Vec<(f32, f32)> {
    let Split::Seconds(length) = split else { return vec![(start, end)] };
    let length = length.max(1.0);
    let mut parts = Vec::new();
    let mut from = start;
    while from < end {
        let to = (from + length).min(end);
        parts.push((from, to));
        from = to;
    }
    parts
}

fn average_bytes_per_second(source: &str, duration: f32) -> Option<f32> {
    if duration <= 0.0 {
        return None;
    }
    let size = http_cache::file_size(source).or_else(|| std::fs::metadata(source).ok().map(|meta| meta.len()))?;
    Some(size as f32 / duration)
}

fn start_encoder(
    source: &str,
    start: f32,
    end: f32,
    settings: &EncodeSettings,
    subtitle: Option<i32>,
    audio: Option<i32>,
    output: &Path,
) -> Result<EncodeJob, String> {
    let output_text = output.to_string_lossy().into_owned();
    let start_text = format!("{start:.3}");
    let end_text = format!("{end:.3}");
    let video_options = settings.video_kbps.map(|kbps| format!("b={kbps}k"));
    let audio_options = format!("b={}k", settings.audio_bitrate);
    let subtitle_text = match (settings.burn_subtitles, subtitle) {
        (true, Some(id)) => id.to_string(),
        _ => "no".to_string(),
    };
    let audio_text = audio.map_or("auto".to_string(), |id| id.to_string());
    let scale = settings.height.map(|height| format!("scale=-2:{height}"));

    let mut options: Vec<(&str, &str)> = vec![
        ("o", output_text.as_str()),
        ("start", start_text.as_str()),
        ("end", end_text.as_str()),
        ("ovc", settings.video_codec),
        ("oac", settings.audio_codec),
        ("oacopts", audio_options.as_str()),
        ("sid", subtitle_text.as_str()),
        ("aid", audio_text.as_str()),
        ("cache", "yes"),
    ];
    // Without a bitrate the codec falls back to its own quality-based default.
    if let Some(video_options) = &video_options {
        options.push(("ovcopts", video_options.as_str()));
    }
    if let Some(scale) = &scale {
        options.push(("vf", scale.as_str()));
    }

    debug!("encoder options {options:?}");
    let encoder = Mpv::with_options(&options).map_err(|err| err.to_string())?;
    crate::log_error(encoder.request_log_messages("info"));
    encoder
        .add_stream_protocol(http_cache::PROTOCOL, http_cache::open_stream)
        .map_err(|err| err.to_string())?;
    encoder.observe_double(ENCODER_TIME_POS, "time-pos").map_err(|err| err.to_string())?;

    let progress = Arc::new(AtomicU32::new(0f32.to_bits()));
    let finished = Arc::new(AtomicBool::new(false));
    let (encoder_weak, event_progress, event_finished) = (encoder.downgrade(), progress.clone(), finished.clone());
    encoder
        .on_wakeup(move || {
            let (encoder_weak, progress, finished) =
                (encoder_weak.clone(), event_progress.clone(), event_finished.clone());
            let _ = slint::invoke_from_event_loop(move || {
                let Some(encoder) = encoder_weak.upgrade() else { return };
                while let Some(event) = encoder.next_event() {
                    match event {
                        Event::PropertyChange { id: ENCODER_TIME_POS, value: Some(PropertyValue::Double(time)) } => {
                            let fraction = ((time as f32 - start) / (end - start)).clamp(0.0, 1.0);
                            progress.store(fraction.to_bits(), Ordering::Relaxed);
                        }
                        Event::EndFile { error, .. } => {
                            if let Some(error) = error {
                                warn!("encoding ended with an error: {error}");
                            }
                            finished.store(true, Ordering::Relaxed);
                        }
                        Event::Shutdown => finished.store(true, Ordering::Relaxed),
                        Event::Log { prefix, level, text } => {
                            crate::log_mpv_message(&format!("encoder/{prefix}"), &level, &text);
                        }
                        Event::CommandFailed(message) => warn!("encoder command failed: {message}"),
                        _ => {}
                    }
                }
            });
        })
        .map_err(|err| err.to_string())?;
    encoder.command(&["loadfile", source]).map_err(|err| err.to_string())?;

    Ok(EncodeJob { encoder, output: output.to_path_buf(), progress, finished })
}

// mpv only finishes writing the output file when the instance is destroyed, which can take a moment.
fn finish_encoder(job: EncodeJob, app: slint::Weak<AppWindow>) {
    std::thread::spawn(move || {
        let EncodeJob { encoder, output, .. } = job;
        drop(encoder);
        let size = file_size(&output);
        if size > 0 {
            info!("encoding finished, {} in {}", bytes_text(size), output.display());
        } else {
            warn!("encoding produced nothing at {}", output.display());
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = app.upgrade() else { return };
            show(&app, |ui| ui.set_encoding(false));
            show_text(&app, bytes_text(size), |ui, text| ui.set_size_text(text));
            show_status(&app, if size > 0 { "Saved." } else { "Encoding failed." });
        });
    });
}

// What the recorder's form holds when Start is pressed.
struct Fields {
    folder: SharedString,
    prefix: SharedString,
    mode: i32,
    split_mode: i32,
    split_minutes: SharedString,
    split_megabytes: SharedString,
    encode: EncodeSettings,
}

impl Fields {
    fn read(ui: &RecorderUi) -> Self {
        Self {
            folder: ui.get_folder(),
            prefix: ui.get_prefix(),
            mode: ui.get_mode(),
            split_mode: ui.get_split_mode(),
            split_minutes: ui.get_split_minutes(),
            split_megabytes: ui.get_split_megabytes(),
            encode: encode_settings(ui),
        }
    }
}

/// Copies what the user can edit from one place the recorder shows to the other.
pub fn copy_fields(from: &RecorderUi, to: &RecorderUi) {
    to.set_folder(from.get_folder());
    to.set_prefix(from.get_prefix());
    to.set_mode(from.get_mode());
    to.set_split_mode(from.get_split_mode());
    to.set_split_minutes(from.get_split_minutes());
    to.set_split_megabytes(from.get_split_megabytes());
    to.set_container(from.get_container());
    to.set_video_codec(from.get_video_codec());
    to.set_video_bitrate(from.get_video_bitrate());
    to.set_size(from.get_size());
    to.set_audio_codec(from.get_audio_codec());
    to.set_audio_bitrate(from.get_audio_bitrate());
    to.set_burn_subtitles(from.get_burn_subtitles());
}

fn encode_settings(ui: &RecorderUi) -> EncodeSettings {
    EncodeSettings {
        extension: if ui.get_container() == 1 { "mkv" } else { "mp4" },
        video_codec: if ui.get_video_codec() == 1 { "libx265" } else { "libx264" },
        video_kbps: parse_positive(ui.get_video_bitrate().as_str()).map(|kbps| kbps as u32),
        height: match ui.get_size() {
            1 => Some(1080),
            2 => Some(720),
            3 => Some(480),
            _ => None,
        },
        audio_codec: if ui.get_audio_codec() == 1 { "libopus" } else { "aac" },
        audio_bitrate: match ui.get_audio_bitrate() {
            0 => 128,
            2 => 320,
            _ => 192,
        },
        burn_subtitles: ui.get_burn_subtitles(),
    }
}

fn selected_track(tracks: &slint::ModelRc<crate::TrackItem>) -> Option<i32> {
    tracks.iter().find(|track| track.selected).map(|track| track.id)
}

fn is_matroska(format: &str) -> bool {
    ["mkv", "matroska", "webm"].iter().any(|name| format.contains(name))
}

fn copy_extension(format: &str) -> &'static str {
    if format.contains("mp4") || format.contains("mov") {
        "mp4"
    } else if format.contains("webm") && !format.contains("matroska") {
        "webm"
    } else {
        "mkv"
    }
}

fn base_name(prefix: &str, title: &str) -> String {
    let title = title.rsplit_once('.').map_or(title, |(stem, _)| stem);
    let title: String = title
        .chars()
        .map(|character| if "<>:\"/\\|?*".contains(character) || character.is_control() { '_' } else { character })
        .take(120)
        .collect();
    let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
    format!("{prefix}{} {stamp}", title.trim())
}

fn part_path(base: &Path, extension: &str, part: u32, split: &Split) -> PathBuf {
    match split {
        Split::Off => with_extension(base, extension),
        Split::Seconds(_) => {
            let mut name = base.as_os_str().to_owned();
            name.push(format!(" part {part}.{extension}"));
            PathBuf::from(name)
        }
    }
}

fn with_extension(base: &Path, extension: &str) -> PathBuf {
    let mut name = base.as_os_str().to_owned();
    name.push(format!(".{extension}"));
    PathBuf::from(name)
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|metadata| metadata.len()).unwrap_or(0)
}

fn bytes_text(bytes: u64) -> String {
    let bytes = bytes as f64;
    let megabyte = MEGABYTE as f64;
    if bytes >= 1024.0 * megabyte {
        format!("{:.2} GB", bytes / (1024.0 * megabyte))
    } else {
        format!("{:.1} MB", bytes / megabyte)
    }
}

fn parse_positive(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok().filter(|value| *value > 0)
}

fn default_folder() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    home.unwrap_or_default().join("Videos").join("rona")
}

fn open_folder(folder: &Path) {
    let opener = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(err) = std::process::Command::new(opener).arg(folder).spawn() {
        warn!("could not open {}: {err}", folder.display());
    }
}
