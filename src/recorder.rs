use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::encoders::{self, Available, Codec, Vendor};
use crate::logging::{debug, info, warn};
use crate::mkv_cut;
use crate::mpv::{Event, Mpv, PropertyValue};
use crate::recorder_window;
use crate::{AppWindow, EventState, Panels, Player, Recorder as RecorderUi, format_time, http_cache, log_error};

pub const DUMP_COMMAND_ID: u64 = 1;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const MEGABYTE: u64 = 1024 * 1024;
const ENCODER_TIME_POS: u64 = 1;
// mpv keeps 50 MiB of played data by default, too little to dump a long recording afterwards.
const RECORDING_BACK_BUFFER: &str = "4GiB";
const DEFAULT_BACK_BUFFER: &str = "50MiB";
// The encoder runs behind playback while recording, paused once it catches up and resumed this far behind.
const ENCODER_RESUME_GAP: f32 = 2.0;
// How far past Stop the encoder may already be before the range is encoded again.
const ENCODER_OVERSHOOT: f32 = 0.25;

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
    Encode(Encode),
}

struct Encode {
    // Only when encoding during playback, otherwise the encoder starts at Stop.
    job: Option<EncodeJob>,
    source: String,
    settings: EncodeSettings,
    subtitle: Option<i32>,
    audio: Option<i32>,
    path: PathBuf,
}

struct Matroska {
    source: String,
    start_time: f64,
}

#[derive(Clone, Copy, PartialEq)]
enum RateControl {
    Quality,
    Average,
    Constant,
    Constrained,
}

struct EncodeSettings {
    extension: &'static str,
    codec: Codec,
    vendor: Vendor,
    rate_control: RateControl,
    crf: u32,
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
    // Where this encoder writes, which differs from `path` only for a range encoded again.
    output: PathBuf,
    path: PathBuf,
    // The first encoder, paused, when the range had to be encoded again. It writes to `path` until dropped.
    replaced: Option<Mpv>,
    progress: Arc<Progress>,
}

// Shared with the encoder's event handler.
struct Progress {
    start: f32,
    // The playback position while recording, the end after Stop. The encoder pauses once it gets there.
    limit: AtomicU32,
    time: AtomicU32,
    paused: AtomicBool,
    stopping: AtomicBool,
    stop_sent: AtomicBool,
    finished: AtomicBool,
}

impl Progress {
    fn limit(&self) -> f32 {
        f32::from_bits(self.limit.load(Ordering::Relaxed))
    }

    fn set_limit(&self, limit: f32) {
        self.limit.store(limit.to_bits(), Ordering::Relaxed);
    }

    fn time(&self) -> f32 {
        f32::from_bits(self.time.load(Ordering::Relaxed))
    }

    fn fraction(&self) -> f32 {
        let span = self.limit() - self.start;
        if span <= 0.0 { 0.0 } else { ((self.time() - self.start) / span).clamp(0.0, 1.0) }
    }
}

enum Probe {
    NotStarted,
    Running(Arc<Mutex<Option<Vec<Available>>>>),
    Done,
}

pub struct Controller {
    app: slint::Weak<AppWindow>,
    mpv: Mpv,
    state: Arc<EventState>,
    session: Option<Session>,
    dump: Option<Dump>,
    job: Option<EncodeJob>,
    hardware: Vec<Available>,
    probe: Probe,
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
        hardware: Vec::new(),
        probe: Probe::NotStarted,
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
        let fields = recorder_window::read(&app, |ui| Fields::read(ui, &self.hardware));
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
            let mut settings = fields.encode;
            if settings.vendor != Vendor::Software
                && !self.hardware.iter().any(|found| found.vendor == settings.vendor && found.supports(settings.codec))
            {
                let codec = if settings.codec == Codec::H265 { "H.265" } else { "H.264" };
                show_status(&app, format!("This {} GPU can't encode {codec}.", settings.vendor.label()));
                return;
            }
            if settings.vendor == Vendor::Amd && settings.rate_control == RateControl::Constrained {
                show_status(&app, "AMD's encoder can't do Constrained. Pick another rate control.");
                return;
            }
            if settings.rate_control != RateControl::Quality && settings.video_kbps.is_none() {
                let bits = self.state.video_bits_per_second.load(Ordering::Relaxed);
                settings.video_kbps = (bits > 0).then(|| (bits as f64 / 1000.0).round() as u32);
                if settings.video_kbps.is_none() {
                    warn!("source bitrate unknown, encoding at constant quality instead");
                }
            }
            let path = with_extension(&base, settings.extension);
            let subtitle = selected_track(&player.get_subtitle_tracks());
            let audio = selected_track(&player.get_audio_tracks());
            let job = if fields.encode_live {
                match start_encoder(&source, player.get_position(), None, &settings, subtitle, audio, &path) {
                    Ok(job) => Some(job),
                    Err(err) => {
                        warn!("could not start encoding: {err}");
                        show_status(&app, format!("Could not start encoding: {err}"));
                        return;
                    }
                }
            } else {
                None
            };
            show_text(&app, file_name(&path), |ui, text| ui.set_current_file(text));
            Output::Encode(Encode { job, source, settings, subtitle, audio, path })
        };

        info!(
            "recording from {:.1} s, {}",
            player.get_position(),
            match &output {
                Output::Copy { extension, .. } => format!("copying to .{extension}"),
                Output::Encode(encode) => format!(
                    "encoding with {} and {} to {}",
                    encode.settings.vendor.encoder(encode.settings.codec),
                    encode.settings.audio_codec,
                    encode.path.display()
                ),
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
            match output {
                Output::Copy { matroska: None, .. } => {
                    log_error(self.mpv.command(&["set", "demuxer-max-back-bytes", DEFAULT_BACK_BUFFER]));
                }
                Output::Encode(Encode { job: Some(job), .. }) => discard_encoder(job),
                Output::Encode(_) => {}
                Output::Copy { .. } => {}
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
            Output::Encode(encode) => self.finish_encoding(&app, encode, start, end),
        }
    }

    fn finish_encoding(&mut self, app: &AppWindow, encode: Encode, start: f32, end: f32) {
        let Encode { job, source, settings, subtitle, audio, path } = encode;
        let Some(job) = job else {
            match start_encoder(&source, start, Some(end), &settings, subtitle, audio, &path) {
                Ok(job) => {
                    self.job = Some(job);
                    show(app, |ui| ui.set_encoding(true));
                    show_status(app, "Encoding 0%");
                }
                Err(err) => {
                    warn!("could not start encoding: {err}");
                    show_status(app, format!("Could not start encoding: {err}"));
                }
            }
            return;
        };
        let time = job.progress.time();
        if time <= end + ENCODER_OVERSHOOT {
            job.progress.set_limit(end);
            job.progress.stopping.store(true, Ordering::Relaxed);
            log_error(job.encoder.command(&["set", "end", &format!("{end:.3}")]));
            job.progress.paused.store(false, Ordering::Relaxed);
            log_error(job.encoder.command(&["set", "pause", "no"]));
            let fraction = job.progress.fraction();
            self.job = Some(job);
            show(app, |ui| ui.set_encoding(true));
            show_status(app, format!("Encoding {:.0}%", fraction * 100.0));
            return;
        }

        // A seek back while recording left the encoder past the end, so the range is encoded again.
        info!("the encoder is at {time:.1} s, past the end at {end:.1} s, encoding the range again");
        log_error(job.encoder.command(&["set", "pause", "yes"]));
        let retry = path.with_extension(format!("part.{}", settings.extension));
        match start_encoder(&source, start, Some(end), &settings, subtitle, audio, &retry) {
            Ok(mut retry_job) => {
                retry_job.path = path;
                retry_job.replaced = Some(job.encoder);
                self.job = Some(retry_job);
                show(app, |ui| ui.set_encoding(true));
                show_status(app, "Encoding 0%");
            }
            Err(err) => {
                warn!("could not start encoding again: {err}");
                discard_encoder(job);
                show_status(app, format!("Could not start encoding: {err}"));
            }
        }
    }

    fn check_probe(&mut self, app: &AppWindow) {
        match &self.probe {
            Probe::NotStarted => {
                if !app.global::<Panels>().get_recorder_open() {
                    return;
                }
                let result = Arc::new(Mutex::new(None));
                let slot = result.clone();
                std::thread::spawn(move || {
                    let found = encoders::probe();
                    if let Ok(mut slot) = slot.lock() {
                        *slot = Some(found);
                    }
                });
                self.probe = Probe::Running(result);
            }
            Probe::Running(result) => {
                let Some(found) = result.lock().ok().and_then(|mut slot| slot.take()) else { return };
                let labels: Vec<SharedString> = std::iter::once(Vendor::Software)
                    .chain(found.iter().map(|available| available.vendor))
                    .map(|vendor| vendor.label().into())
                    .collect();
                let model = ModelRc::new(VecModel::from(labels));
                show(app, |ui| ui.set_encoders(model.clone()));
                self.hardware = found;
                self.probe = Probe::Done;
            }
            Probe::Done => {}
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
        self.check_probe(&app);

        if let Some(session) = &self.session {
            let position = app.global::<Player>().get_position();
            show_text(&app, format_time((position - session.start).max(0.0)), |ui, text| ui.set_elapsed_text(text));
            if let Output::Encode(Encode { job: Some(job), .. }) = &session.output {
                let progress = &job.progress;
                progress.set_limit(position);
                if progress.paused.load(Ordering::Relaxed) && position - progress.time() > ENCODER_RESUME_GAP {
                    progress.paused.store(false, Ordering::Relaxed);
                    log_error(job.encoder.command(&["set", "pause", "no"]));
                }
            }
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
            if job.progress.finished.load(Ordering::Relaxed) {
                if let Some(job) = self.job.take() {
                    finish_encoder(job, self.app.clone());
                }
            } else {
                show_status(&app, format!("Encoding {:.0}%", job.progress.fraction() * 100.0));
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
    end: Option<f32>,
    settings: &EncodeSettings,
    subtitle: Option<i32>,
    audio: Option<i32>,
    output: &Path,
) -> Result<EncodeJob, String> {
    let output_text = output.to_string_lossy().into_owned();
    let start_text = format!("{start:.3}");
    let end_text = end.map(|end| format!("{end:.3}"));
    let video_options = video_options(settings);
    let audio_options = audio_options(settings);
    let subtitle_text = match (settings.burn_subtitles, subtitle) {
        (true, Some(id)) => id.to_string(),
        _ => "no".to_string(),
    };
    let audio_text = audio.map_or("auto".to_string(), |id| id.to_string());
    let scale = settings.height.map(|height| format!("scale=-2:{height}"));

    let mut options: Vec<(&str, &str)> = vec![
        ("o", output_text.as_str()),
        ("start", start_text.as_str()),
        ("ovc", settings.vendor.encoder(settings.codec)),
        ("ovcopts", video_options.as_str()),
        ("oac", settings.audio_codec),
        ("oacopts", audio_options.as_str()),
        ("sid", subtitle_text.as_str()),
        ("aid", audio_text.as_str()),
        ("cache", "yes"),
    ];
    if let Some(scale) = &scale {
        options.push(("vf", scale.as_str()));
    }
    if let Some(end_text) = &end_text {
        options.push(("end", end_text.as_str()));
    }

    debug!("encoder options {options:?}");
    let encoder = Mpv::with_options(&options).map_err(|err| err.to_string())?;
    crate::log_error(encoder.request_log_messages("info"));
    encoder
        .add_stream_protocol(http_cache::PROTOCOL, http_cache::open_stream)
        .map_err(|err| err.to_string())?;
    encoder.observe_double(ENCODER_TIME_POS, "time-pos").map_err(|err| err.to_string())?;

    let progress = Arc::new(Progress {
        start,
        limit: AtomicU32::new(end.unwrap_or(start).to_bits()),
        time: AtomicU32::new(start.to_bits()),
        paused: AtomicBool::new(false),
        stopping: AtomicBool::new(end.is_some()),
        stop_sent: AtomicBool::new(false),
        finished: AtomicBool::new(false),
    });
    let (encoder_weak, event_progress) = (encoder.downgrade(), progress.clone());
    encoder
        .on_wakeup(move || {
            let (encoder_weak, progress) = (encoder_weak.clone(), event_progress.clone());
            let _ = slint::invoke_from_event_loop(move || {
                let Some(encoder) = encoder_weak.upgrade() else { return };
                while let Some(event) = encoder.next_event() {
                    match event {
                        Event::PropertyChange { id: ENCODER_TIME_POS, value: Some(PropertyValue::Double(time)) } => {
                            let time = time as f32;
                            progress.time.store(time.to_bits(), Ordering::Relaxed);
                            if time < progress.limit() {
                                continue;
                            }
                            if !progress.stopping.load(Ordering::Relaxed) {
                                if !progress.paused.swap(true, Ordering::Relaxed) {
                                    log_error(encoder.command(&["set", "pause", "yes"]));
                                }
                            } else if !progress.stop_sent.swap(true, Ordering::Relaxed) {
                                // Normally `end` ends the file first, this is in case mpv missed it.
                                log_error(encoder.command(&["stop"]));
                            }
                        }
                        Event::EndFile { error, .. } => {
                            if let Some(error) = error {
                                warn!("encoding ended with an error: {error}");
                            }
                            progress.finished.store(true, Ordering::Relaxed);
                        }
                        Event::Shutdown => progress.finished.store(true, Ordering::Relaxed),
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

    Ok(EncodeJob { encoder, output: output.to_path_buf(), path: output.to_path_buf(), replaced: None, progress })
}

// Each encoder family picks its rate control from different options, see ffmpeg's nvenc.c, amfenc_*.c and qsvenc.c.
fn video_options(settings: &EncodeSettings) -> String {
    let crf = settings.crf;
    // The hardware encoders treat 0 as "not set".
    let level = crf.max(1);
    let rate = if settings.video_kbps.is_some() { settings.rate_control } else { RateControl::Quality };
    let kbps = settings.video_kbps.unwrap_or(0);
    let fixed = format!("b={kbps}k,maxrate={kbps}k,bufsize={kbps}k");
    match (settings.vendor, rate) {
        (Vendor::Software, RateControl::Quality) => format!("crf={crf}"),
        (Vendor::Software, RateControl::Average) => format!("b={kbps}k"),
        (Vendor::Software, RateControl::Constant) => {
            // libx264 holds a fixed bitrate with minrate and maxrate alone, libx265 needs strict-cbr.
            let options = format!("{fixed},minrate={kbps}k");
            if settings.codec == Codec::H265 { format!("{options},x265-params=strict-cbr=1") } else { options }
        }
        (Vendor::Software, RateControl::Constrained) => format!("crf={crf},maxrate={kbps}k,bufsize={}k", kbps * 2),
        (Vendor::Nvidia, RateControl::Quality) => format!("rc=vbr,cq={level},b=0"),
        (Vendor::Nvidia, RateControl::Average) => format!("rc=vbr,b={kbps}k"),
        (Vendor::Nvidia, RateControl::Constant) => format!("rc=cbr,{fixed}"),
        (Vendor::Nvidia, RateControl::Constrained) => format!("rc=vbr,cq={level},maxrate={kbps}k"),
        // hevc_amf has no qp_b.
        (Vendor::Amd, RateControl::Quality) if settings.codec == Codec::H265 => format!("rc=cqp,qp_i={crf},qp_p={crf}"),
        (Vendor::Amd, RateControl::Quality) => format!("rc=cqp,qp_i={crf},qp_p={crf},qp_b={crf}"),
        (Vendor::Amd, RateControl::Average) => format!("rc=vbr_peak,b={kbps}k,maxrate={}k", kbps * 2),
        (Vendor::Amd, RateControl::Constant) => format!("rc=cbr,{fixed}"),
        // Refused in `start`, AMF's quality modes have no bitrate cap.
        (Vendor::Amd, RateControl::Constrained) => format!("rc=vbr_peak,b={kbps}k,maxrate={kbps}k"),
        // QSV picks ICQ for a quality without maxrate, CBR when maxrate equals b, and QVBR for a quality with both.
        (Vendor::Intel, RateControl::Quality) => format!("global_quality={level}"),
        (Vendor::Intel, RateControl::Average) => format!("b={kbps}k"),
        (Vendor::Intel, RateControl::Constant) => fixed,
        (Vendor::Intel, RateControl::Constrained) => {
            format!("global_quality={level},b={}k,maxrate={kbps}k", (kbps * 3 / 4).max(1))
        }
    }
}

fn discard_encoder(job: EncodeJob) {
    std::thread::spawn(move || {
        let EncodeJob { encoder, output, replaced, .. } = job;
        drop(encoder);
        drop(replaced);
        let _ = std::fs::remove_file(&output);
        info!("discarded the encoding to {}", output.display());
    });
}

fn audio_options(settings: &EncodeSettings) -> String {
    let bitrate = format!("b={}k", settings.audio_bitrate);
    if settings.audio_codec != "libopus" {
        return bitrate;
    }
    let vbr = match settings.rate_control {
        RateControl::Quality | RateControl::Average => "on",
        RateControl::Constant => "off",
        RateControl::Constrained => "constrained",
    };
    format!("{bitrate},vbr={vbr}")
}

// mpv only finishes writing the output file when the instance is destroyed, which can take a moment.
fn finish_encoder(job: EncodeJob, app: slint::Weak<AppWindow>) {
    std::thread::spawn(move || {
        let EncodeJob { encoder, output: written, path: output, replaced, .. } = job;
        drop(encoder);
        if let Some(replaced) = replaced {
            drop(replaced);
            let _ = std::fs::remove_file(&output);
            if let Err(err) = std::fs::rename(&written, &output) {
                warn!("could not rename {} to {}: {err}", written.display(), output.display());
            }
        }
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
    encode_live: bool,
}

impl Fields {
    fn read(ui: &RecorderUi, hardware: &[Available]) -> Self {
        Self {
            folder: ui.get_folder(),
            prefix: ui.get_prefix(),
            mode: ui.get_mode(),
            split_mode: ui.get_split_mode(),
            split_minutes: ui.get_split_minutes(),
            split_megabytes: ui.get_split_megabytes(),
            encode: encode_settings(ui, hardware),
            encode_live: ui.get_encode_live(),
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
    to.set_encoder(from.get_encoder());
    to.set_rate_control(from.get_rate_control());
    to.set_quality(from.get_quality());
    to.set_crf(from.get_crf());
    to.set_video_bitrate(from.get_video_bitrate());
    to.set_size(from.get_size());
    to.set_audio_codec(from.get_audio_codec());
    to.set_audio_bitrate(from.get_audio_bitrate());
    to.set_burn_subtitles(from.get_burn_subtitles());
    to.set_encode_live(from.get_encode_live());
}

fn encode_settings(ui: &RecorderUi, hardware: &[Available]) -> EncodeSettings {
    let codec = if ui.get_video_codec() == 1 { Codec::H265 } else { Codec::H264 };
    // The Encoder list is Software followed by the hardware found.
    let vendor = usize::try_from(ui.get_encoder() - 1)
        .ok()
        .and_then(|index| hardware.get(index))
        .map_or(Vendor::Software, |available| available.vendor);
    // High, Medium and Low per codec. H.265 needs a higher CRF than H.264 for the same quality.
    let presets = if codec == Codec::H265 { [22, 28, 32] } else { [18, 23, 28] };
    let crf = match ui.get_quality() {
        0 => presets[0],
        2 => presets[2],
        3 => ui.get_crf().trim().parse::<u32>().unwrap_or(presets[1]),
        _ => presets[1],
    };
    EncodeSettings {
        extension: if ui.get_container() == 1 { "mkv" } else { "mp4" },
        codec,
        vendor,
        rate_control: match ui.get_rate_control() {
            1 => RateControl::Average,
            2 => RateControl::Constant,
            3 => RateControl::Constrained,
            _ => RateControl::Quality,
        },
        crf,
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
