use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, Timer, TimerMode};

use crate::media;
use crate::mpv::Mpv;
use crate::settings::{self, SharedConfig};
use crate::{AppWindow, Phase, Player, http_cache, log_error, percent_decode};

pub const HOOK_LOAD: u64 = 1;
pub const HOOK_UNLOAD: u64 = 2;

const FILE_NAME: &str = "resume.conf";
const SAVE_INTERVAL: Duration = Duration::from_secs(10);
const MIN_POSITION: f64 = 10.0;
const END_MARGIN_SECONDS: f64 = 30.0;
const END_MARGIN_FRACTION: f64 = 0.05;
const MIN_DURATION: f64 = 120.0;
const MAX_ENTRIES: usize = 1000;
const AUDIO_MARK: &str = "audio";

struct Entry {
    key: String,
    // Unix seconds of the last save, used to forget entries after the configured number of days.
    written: u64,
    // Marks a saved position as an audio file's, so it's skipped while audio positions are off.
    audio: bool,
    options: Vec<(String, String)>,
}

// The file playing now, and its option values right after load, so only later changes count.
struct Current {
    key: String,
    baseline: Vec<(String, String)>,
}

struct Snapshot {
    key: String,
    position: Option<f64>,
    duration: Option<f64>,
    finished: bool,
    audio: bool,
    options: Vec<(String, String)>,
}

// The settings a save needs, copied so a save on another thread doesn't touch the config.
#[derive(Clone, Copy)]
struct Policy {
    max_age: Duration,
    videos: bool,
    audio: bool,
}

impl Policy {
    fn covers(&self, audio: bool) -> bool {
        if audio { self.audio } else { self.videos }
    }
}

#[derive(Default)]
struct Store {
    file: Option<PathBuf>,
    entries: Vec<Entry>,
    current: Option<Current>,
}

struct Resume {
    app: slint::Weak<AppWindow>,
    mpv: Mpv,
    config: SharedConfig,
    store: Arc<Mutex<Store>>,
    // The options mpv itself would save, read from its watch-later-options.
    option_names: Arc<Vec<String>>,
    saving: Arc<AtomicBool>,
    last_saved_position: Cell<f32>,
    _timer: Timer,
}

thread_local! {
    static RESUME: RefCell<Option<Rc<Resume>>> = const { RefCell::new(None) };
}

fn with_resume(action: impl FnOnce(&Resume)) {
    let resume = RESUME.with(|slot| slot.borrow().clone());
    if let Some(resume) = resume {
        action(&resume);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn wire(app: &AppWindow, mpv: &Mpv, config: &SharedConfig) {
    let file = settings::config_dir().map(|dir| dir.join(FILE_NAME));
    let mut entries = file.as_ref().map(|file| load_entries(file)).unwrap_or_default();
    let before = entries.len();
    forget_old(&mut entries, max_age(config));
    if let Some(file) = file.as_ref().filter(|_| entries.len() != before) {
        write_entries(file, &entries);
    }
    let option_names: Vec<String> = mpv
        .get_string("watch-later-options")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != "start")
        .map(str::to_string)
        .collect();

    log_error(mpv.add_hook(HOOK_LOAD, "on_load"));
    log_error(mpv.add_hook(HOOK_UNLOAD, "on_unload"));

    let timer = Timer::default();
    timer.start(TimerMode::Repeated, SAVE_INTERVAL, || with_resume(Resume::save_if_moved));

    let resume = Resume {
        app: app.as_weak(),
        mpv: mpv.clone(),
        config: config.clone(),
        store: Arc::new(Mutex::new(Store { file, entries, current: None })),
        option_names: Arc::new(option_names),
        saving: Arc::new(AtomicBool::new(false)),
        last_saved_position: Cell::new(-1.0),
        _timer: timer,
    };
    RESUME.with(|slot| *slot.borrow_mut() = Some(Rc::new(resume)));
}

/// mpv waits until the hook is continued, so this always continues it, even when resume is off.
pub fn handle_hook(mpv: &Mpv, kind: u64, id: u64) {
    with_resume(|resume| match kind {
        HOOK_LOAD => resume.file_loading(),
        HOOK_UNLOAD => resume.save_now(),
        _ => {}
    });
    log_error(mpv.continue_hook(id));
}

pub fn save_now() {
    with_resume(Resume::save_now);
}

/// For moments that matter but don't need to block, like pausing or a finished seek.
pub fn save_soon() {
    with_resume(Resume::save_in_background);
}

impl Resume {
    fn enabled(&self) -> bool {
        let config = self.config.borrow();
        config.resume || config.resume_audio
    }

    // Tracks aren't known yet while loading, so audio is told apart by the entry's mark or the file name.
    fn file_loading(&self) {
        let key = key_of(&self.mpv.get_string("stream-open-filename").unwrap_or_default());
        let policy = policy(&self.config);
        let saved = lock(&self.store)
            .entries
            .iter()
            .find(|entry| entry.key == key && policy.covers(entry.audio || media::is_audio_name(&key)))
            .map(|entry| entry.options.clone());
        for (name, value) in saved.unwrap_or_default() {
            log_error(self.mpv.set_property(&format!("file-local-options/{name}"), &value));
        }
        let baseline = read_options(&self.mpv, &self.option_names);
        lock(&self.store).current = Some(Current { key, baseline });
        self.last_saved_position.set(-1.0);
    }

    // Runs on the UI thread, used where mpv is about to drop the file or the app is closing.
    fn save_now(&self) {
        if !self.enabled() {
            return;
        }
        let snapshot = read_snapshot(&self.mpv, &self.option_names);
        record_and_write(&self.store, snapshot, policy(&self.config));
    }

    fn save_in_background(&self) {
        if !self.enabled() || self.saving.swap(true, Ordering::SeqCst) {
            return;
        }
        let policy = policy(&self.config);
        let (mpv, store, names, saving) =
            (self.mpv.clone(), self.store.clone(), self.option_names.clone(), self.saving.clone());
        std::thread::spawn(move || {
            let snapshot = read_snapshot(&mpv, &names);
            record_and_write(&store, snapshot, policy);
            saving.store(false, Ordering::SeqCst);
        });
    }

    fn save_if_moved(&self) {
        let Some(app) = self.app.upgrade() else { return };
        let player = app.global::<Player>();
        let position = player.get_position();
        if player.get_phase() != Phase::Playing || (position - self.last_saved_position.get()).abs() < 0.5 {
            return;
        }
        self.last_saved_position.set(position);
        self.save_in_background();
    }
}

/// Local paths are compared as absolute paths, and without case on Windows. Links keep their full URL.
pub fn key_of(path: &str) -> String {
    let path = path.strip_prefix(http_cache::PROTOCOL).and_then(|rest| rest.strip_prefix("://")).unwrap_or(path);
    if path.contains("://") {
        return path.to_string();
    }
    let absolute = std::path::absolute(path).map_or_else(|_| path.to_string(), |full| full.to_string_lossy().into_owned());
    if cfg!(windows) { absolute.to_lowercase() } else { absolute }
}

fn read_options(mpv: &Mpv, names: &[String]) -> Vec<(String, String)> {
    names
        .iter()
        .filter_map(|name| mpv.get_string(&format!("options/{name}")).map(|value| (name.clone(), value)))
        .collect()
}

// Reads the path before and after, so a snapshot that straddles a file switch can be thrown away.
fn read_snapshot(mpv: &Mpv, names: &[String]) -> Option<Snapshot> {
    let before = mpv.get_string("path")?;
    let number = |name: &str| mpv.get_string(name).and_then(|value| value.parse::<f64>().ok());
    let position = number("time-pos");
    let duration = number("duration");
    let finished = mpv.get_string("eof-reached").as_deref() == Some("yes");
    // No video track, or only cover art, is an audio file.
    let audio = mpv.get_string("current-tracks/video/albumart").is_none_or(|albumart| albumart == "yes");
    let options = read_options(mpv, names);
    let after = mpv.get_string("path")?;
    (before == after).then(|| Snapshot { key: key_of(&before), position, duration, finished, audio, options })
}

fn max_age(config: &SharedConfig) -> Duration {
    Duration::from_secs(u64::from(config.borrow().resume_days) * 24 * 60 * 60)
}

fn policy(config: &SharedConfig) -> Policy {
    let settings = config.borrow();
    Policy { max_age: max_age(config), videos: settings.resume, audio: settings.resume_audio }
}

fn now_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

fn forget_old(entries: &mut Vec<Entry>, max_age: Duration) {
    let oldest_kept = now_seconds().saturating_sub(max_age.as_secs());
    entries.retain(|entry| entry.written >= oldest_kept);
}

fn record_and_write(store: &Mutex<Store>, snapshot: Option<Snapshot>, policy: Policy) {
    let Some(snapshot) = snapshot.filter(|snapshot| policy.covers(snapshot.audio)) else { return };
    let mut store = lock(store);
    let Some(current) = store.current.as_ref().filter(|current| current.key == snapshot.key) else { return };

    let changed: Vec<(String, String)> = snapshot
        .options
        .iter()
        .filter(|(name, value)| !current.baseline.iter().any(|(base_name, base)| base_name == name && base == value))
        .cloned()
        .collect();
    let previous = store.entries.iter().position(|entry| entry.key == snapshot.key);
    let mut options: Vec<(String, String)> = previous
        .map(|index| store.entries[index].options.iter().filter(|(name, _)| name != "start").cloned().collect())
        .unwrap_or_default();
    for (name, value) in changed {
        match options.iter_mut().find(|(existing, _)| *existing == name) {
            Some(existing) => existing.1 = value,
            None => options.push((name, value)),
        }
    }
    if let Some(start) = resume_position(&snapshot) {
        options.insert(0, ("start".to_string(), format!("{start:.3}")));
    }

    if let Some(index) = previous {
        store.entries.remove(index);
    }
    if !options.is_empty() {
        store.entries.push(Entry { key: snapshot.key, written: now_seconds(), audio: snapshot.audio, options });
    }
    forget_old(&mut store.entries, policy.max_age);
    let overflow = store.entries.len().saturating_sub(MAX_ENTRIES);
    store.entries.drain(..overflow);
    if let Some(file) = store.file.clone() {
        write_entries(&file, &store.entries);
    }
}

// No position near the start, near the end, or for short files, so those start over next time.
fn resume_position(snapshot: &Snapshot) -> Option<f64> {
    let position = snapshot.position?;
    let duration = snapshot.duration.unwrap_or(0.0);
    if snapshot.finished || position < MIN_POSITION {
        return None;
    }
    if duration > 0.0 {
        let margin = END_MARGIN_SECONDS.max(duration * END_MARGIN_FRACTION);
        if duration < MIN_DURATION || position > duration - margin {
            return None;
        }
    }
    Some(position)
}

// One entry per line: key, time last written, then name=value pairs, all tab separated. A bare `audio` field
// marks an audio file.
fn escape(text: &str) -> String {
    text.replace('%', "%25").replace('\t', "%09").replace('\n', "%0A").replace('\r', "%0D")
}

fn load_entries(file: &PathBuf) -> Vec<Entry> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            eprintln!("could not read {}: {err}", file.display());
            return Vec::new();
        }
    };
    let mut entries: Vec<Entry> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let key = percent_decode(fields.next().filter(|key| !key.is_empty())?);
            let written = fields.next()?.parse().ok()?;
            let mut audio = false;
            let options = fields
                .filter_map(|field| {
                    audio |= field == AUDIO_MARK;
                    field.split_once('=')
                })
                .map(|(name, value)| (percent_decode(name), percent_decode(value)))
                .collect();
            Some(Entry { key, written, audio, options })
        })
        .collect();
    entries.sort_by_key(|entry| entry.written);
    entries
}

// Written to a temporary file and renamed over the old one, so a crash mid-write can't leave half a file.
fn write_entries(file: &PathBuf, entries: &[Entry]) {
    let mut text = String::new();
    for entry in entries {
        text.push_str(&escape(&entry.key));
        text.push('\t');
        text.push_str(&entry.written.to_string());
        if entry.audio {
            text.push('\t');
            text.push_str(AUDIO_MARK);
        }
        for (name, value) in &entry.options {
            text.push('\t');
            text.push_str(&escape(name));
            text.push('=');
            text.push_str(&escape(value));
        }
        text.push('\n');
    }
    let temporary = file.with_extension("conf.tmp");
    let result = file
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&temporary, text))
        .and_then(|()| std::fs::rename(&temporary, file));
    if let Err(err) = result {
        eprintln!("could not save resume positions to {}: {err}", file.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(position: f64, duration: f64, finished: bool) -> Snapshot {
        Snapshot {
            key: String::new(),
            position: Some(position),
            duration: Some(duration),
            finished,
            audio: false,
            options: Vec::new(),
        }
    }

    fn saved_start(audio: bool, videos_on: bool, audio_on: bool) -> Option<String> {
        let key = r"c:\users\someone\music\album\07 - long track.flac".to_string();
        let store = Mutex::new(Store {
            file: None,
            entries: Vec::new(),
            current: Some(Current { key: key.clone(), baseline: Vec::new() }),
        });
        let snapshot = Snapshot { key, audio, ..snapshot(400.0, 1500.0, false) };
        let policy = Policy { max_age: Duration::from_secs(3 * 24 * 3600), videos: videos_on, audio: audio_on };
        record_and_write(&store, Some(snapshot), policy);
        let store = lock(&store);
        let entry = store.entries.first()?;
        entry.options.iter().find(|(name, _)| name == "start").map(|(_, value)| value.clone())
    }

    #[test]
    fn audio_and_videos_each_follow_their_own_option() {
        assert_eq!(saved_start(true, true, false), None);
        assert_eq!(saved_start(true, false, true).as_deref(), Some("400.000"));
        assert_eq!(saved_start(false, true, false).as_deref(), Some("400.000"));
        assert_eq!(saved_start(false, false, true), None);
    }

    #[test]
    fn position_is_kept_only_in_the_middle_of_a_long_file() {
        assert_eq!(resume_position(&snapshot(1800.0, 5640.0, false)), Some(1800.0));
        assert_eq!(resume_position(&snapshot(6.0, 5640.0, false)), None);
        assert_eq!(resume_position(&snapshot(5400.0, 5640.0, false)), None);
        assert_eq!(resume_position(&snapshot(1800.0, 5640.0, true)), None);
        assert_eq!(resume_position(&snapshot(60.0, 95.0, false)), None);
    }

    #[test]
    fn short_end_margin_still_covers_the_credits_of_a_long_file() {
        // 5% of 3 hours is 9 minutes, more than the 30 s floor.
        assert_eq!(resume_position(&snapshot(10_400.0, 10_800.0, false)), None);
        assert_eq!(resume_position(&snapshot(10_000.0, 10_800.0, false)), Some(10_000.0));
    }

    #[test]
    fn values_with_tabs_and_percent_signs_survive_a_round_trip() {
        let dir = std::env::temp_dir().join(format!("rona-resume-test-{}", std::process::id()));
        let file = dir.join(FILE_NAME);
        let entries = vec![Entry {
            key: "https://cdn.example.com/a%20b.mkv?token=x\ty".into(),
            written: 42,
            audio: true,
            options: vec![("start".into(), "61.500".into()), ("af".into(), "lavfi=[dynaudnorm]\t%".into())],
        }];
        write_entries(&file, &entries);
        let loaded = load_entries(&file);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].key, entries[0].key);
        assert!(loaded[0].audio);
        assert_eq!(loaded[0].options, entries[0].options);
    }

    #[test]
    fn entries_older_than_the_limit_are_forgotten() {
        let now = now_seconds();
        let entry = |key: &str, age_hours: u64| Entry {
            key: key.into(),
            written: now - age_hours * 3600,
            audio: false,
            options: Vec::new(),
        };
        let mut entries = vec![entry("old", 80), entry("recent", 70), entry("today", 1)];
        forget_old(&mut entries, Duration::from_secs(3 * 24 * 3600));
        let kept: Vec<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
        assert_eq!(kept, ["recent", "today"]);
    }

    #[test]
    fn links_keep_the_whole_url_including_the_query() {
        assert_eq!(
            key_of("rpcache://https://cdn.example.com/v.mkv?token=abc"),
            "https://cdn.example.com/v.mkv?token=abc"
        );
    }
}
