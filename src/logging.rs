use std::fmt::Arguments;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use chrono::{Local, NaiveDate};

const KEEP_DAYS: u64 = 7;
const FILE_PREFIX: &str = "rona-";
const FILE_SUFFIX: &str = ".log";
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }
}

enum Message {
    Line(NaiveDate, String),
    Flush(Sender<()>),
}

static SINK: OnceLock<Sender<Message>> = OnceLock::new();

macro_rules! debug {
    ($($arg:tt)*) => { $crate::logging::write($crate::logging::Level::Debug, module_path!(), format_args!($($arg)*)) };
}
macro_rules! info {
    ($($arg:tt)*) => { $crate::logging::write($crate::logging::Level::Info, module_path!(), format_args!($($arg)*)) };
}
// Named apart, since a bare `warn` in a `use` clashes with the built-in #[warn] attribute.
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::logging::write($crate::logging::Level::Warn, module_path!(), format_args!($($arg)*)) };
}
macro_rules! error {
    ($($arg:tt)*) => { $crate::logging::write($crate::logging::Level::Error, module_path!(), format_args!($($arg)*)) };
}
pub(crate) use {debug, error, info, log_warn as warn};

/// Starts writing to a file per day in the log folder, and removes files older than a week.
/// Before this runs, or if the folder can't be used, lines only go to stderr.
pub fn init() {
    let Some(dir) = log_dir() else { return };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        eprintln!("could not create the log folder {}: {err}", dir.display());
        return;
    }
    let (sender, receiver) = channel();
    if SINK.set(sender).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("log writer".into())
        .spawn(move || write_lines(&dir, &receiver))
        .ok();
    install_panic_hook();
}

pub fn write(level: Level, module: &str, arguments: Arguments) {
    let target = module.strip_prefix("rona::").unwrap_or(module);
    write_target(level, target, arguments);
}

pub fn write_target(level: Level, target: &str, arguments: Arguments) {
    let now = Local::now();
    let line = format!("{} {} {target}: {arguments}\n", now.format("%Y-%m-%d %H:%M:%S%.3f"), level.label());
    if level >= Level::Warn || SINK.get().is_none() {
        eprint!("{line}");
    }
    if let Some(sink) = SINK.get() {
        let _ = sink.send(Message::Line(now.date_naive(), line));
    }
}

/// Waits until every line sent so far is on disk, for the moments before the process ends.
pub fn flush() {
    let Some(sink) = SINK.get() else { return };
    let (done, finished) = channel();
    if sink.send(Message::Flush(done)).is_ok() {
        let _ = finished.recv_timeout(FLUSH_TIMEOUT);
    }
}

pub fn log_dir() -> Option<PathBuf> {
    platform_log_dir().map(|dir| dir.join("logs"))
}

#[cfg(windows)]
fn platform_log_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("rona"))
}

#[cfg(target_os = "macos")]
fn platform_log_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Logs/rona"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_log_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
    Some(base.join("rona"))
}

fn write_lines(dir: &Path, receiver: &Receiver<Message>) {
    let mut current: Option<(NaiveDate, File)> = None;
    for message in receiver {
        match message {
            Message::Line(date, line) => {
                if current.as_ref().is_none_or(|(open_date, _)| *open_date != date) {
                    remove_old_files(dir, date);
                    current = open_file(dir, date).map(|file| (date, file));
                }
                if let Some((_, file)) = current.as_mut() {
                    let _ = file.write_all(line.as_bytes());
                }
            }
            Message::Flush(done) => {
                if let Some((_, file)) = current.as_mut() {
                    let _ = file.flush();
                }
                let _ = done.send(());
            }
        }
    }
}

// Opened for appending, so another Rona process writing the same day's file doesn't overwrite it.
fn open_file(dir: &Path, date: NaiveDate) -> Option<File> {
    let path = dir.join(file_name(date));
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| eprintln!("could not open the log file {}: {err}", path.display()))
        .ok()
}

fn file_name(date: NaiveDate) -> String {
    format!("{FILE_PREFIX}{}{FILE_SUFFIX}", date.format("%Y-%m-%d"))
}

fn file_date(name: &str) -> Option<NaiveDate> {
    let date = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

fn remove_old_files(dir: &Path, today: NaiveDate) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(date) = file_date(&name.to_string_lossy()) else { continue };
        if is_expired(date, today) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn is_expired(date: NaiveDate, today: NaiveDate) -> bool {
    today.signed_duration_since(date).num_days() >= KEEP_DAYS as i64
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed");
        let location = panic.location().map(|location| format!(" at {location}")).unwrap_or_default();
        let message = panic
            .payload()
            .downcast_ref::<&str>()
            .map(|text| text.to_string())
            .or_else(|| panic.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        write_target(Level::Error, "panic", format_args!("thread {thread} panicked{location}: {message}"));
        flush();
        default_hook(panic);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn a_week_of_files_is_kept_and_older_ones_go() {
        let today = date("2026-11-21");
        assert!(!is_expired(date("2026-11-21"), today));
        assert!(!is_expired(date("2026-11-15"), today));
        assert!(is_expired(date("2026-11-14"), today));
    }

    #[test]
    fn only_rona_log_files_are_recognized() {
        assert_eq!(file_date("rona-2026-11-21.log"), Some(date("2026-11-21")));
        assert_eq!(file_date(&file_name(date("2026-01-05"))), Some(date("2026-01-05")));
        assert_eq!(file_date("rona-2026-11-21.log.tmp"), None);
        assert_eq!(file_date("notes.log"), None);
    }
}
