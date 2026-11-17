use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};

use image::DynamicImage;
use lofty::config::ParseOptions;
use lofty::file::TaggedFile;
use lofty::picture::{Picture, PictureType};
use lofty::prelude::*;
use lofty::probe::Probe;
use slint::{Rgba8Pixel, SharedPixelBuffer};

use crate::media;

// Covers are kept decoded, so only the files around the current one stay cached.
const CACHE_SIZE: usize = 24;
// Bigger than the cover square at 2x scale, so the cover stays sharp.
const COVER_PIXELS: u32 = 640;
// The names mpv's cover-art-auto looks for next to the file.
const COVER_NAMES: [&str; 8] = ["albumart", "album", "cover", "front", "albumartsmall", "folder", ".folder", "thumb"];
const COVER_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// What the audio view shows for a file, read before mpv opens it.
pub struct AudioInfo {
    pub title: Option<String>,
    pub artist: String,
    pub album_line: String,
    pub cover: Option<SharedPixelBuffer<Rgba8Pixel>>,
}

enum Slot {
    Queued,
    Ready(Arc<AudioInfo>),
    Unreadable,
}

#[derive(Default)]
struct Cache {
    slots: HashMap<String, Slot>,
    // Read files in the order they finished, oldest first, for eviction.
    finished: VecDeque<String>,
    queue: VecDeque<String>,
    // Titles for the playlist, kept for every file since they're small. None when a file has no title tag.
    titles: HashMap<String, Option<String>>,
    // Read only when no full read waits.
    title_queue: VecDeque<String>,
}

pub enum Read {
    Info(String),
    // Sent once the title queue runs empty, so the playlist is refreshed once per batch.
    Titles,
}

enum Job {
    Info(String),
    Title(String),
}

struct Reader {
    cache: Mutex<Cache>,
    wake: Condvar,
}

static READER: OnceLock<Reader> = OnceLock::new();

fn lock(cache: &Mutex<Cache>) -> MutexGuard<'_, Cache> {
    cache.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Local audio files only. Links would need a download, and mpv reads those anyway.
pub fn can_read(path: &str) -> bool {
    !path.contains("://") && media::is_audio_name(path)
}

/// `on_read` is called on the reader's thread after each full read and after each batch of titles.
pub fn start(on_read: impl Fn(Read) + Send + 'static) {
    let reader = READER.get_or_init(|| Reader { cache: Mutex::default(), wake: Condvar::new() });
    std::thread::spawn(move || {
        loop {
            let job = {
                let mut cache = lock(&reader.cache);
                loop {
                    if let Some(path) = cache.queue.pop_front() {
                        break Job::Info(path);
                    }
                    if let Some(path) = cache.title_queue.pop_front() {
                        break Job::Title(path);
                    }
                    cache = reader.wake.wait(cache).unwrap_or_else(PoisonError::into_inner);
                }
            };
            match job {
                Job::Info(path) => {
                    let info = read(Path::new(&path));
                    let mut cache = lock(&reader.cache);
                    cache.titles.insert(path.clone(), info.as_ref().and_then(|info| info.title.clone()));
                    let slot = match info {
                        Some(info) => Slot::Ready(Arc::new(info)),
                        None => Slot::Unreadable,
                    };
                    store(&mut cache, path.clone(), slot);
                    drop(cache);
                    on_read(Read::Info(path));
                }
                Job::Title(path) => {
                    let title = read_title(Path::new(&path));
                    let mut cache = lock(&reader.cache);
                    cache.titles.insert(path, title);
                    let batch_done = cache.title_queue.is_empty();
                    drop(cache);
                    if batch_done {
                        on_read(Read::Titles);
                    }
                }
            }
        }
    });
}

fn store(cache: &mut Cache, path: String, slot: Slot) {
    cache.finished.retain(|finished| *finished != path);
    cache.finished.push_back(path.clone());
    cache.slots.insert(path, slot);
    while cache.finished.len() > CACHE_SIZE {
        if let Some(oldest) = cache.finished.pop_front() {
            cache.slots.remove(&oldest);
        }
    }
}

pub fn get(path: &str) -> Option<Arc<AudioInfo>> {
    let reader = READER.get()?;
    match lock(&reader.cache).slots.get(path) {
        Some(Slot::Ready(info)) => Some(info.clone()),
        _ => None,
    }
}

/// Queues files to read in the background, in the order given, behind anything already queued.
pub fn prefetch(paths: impl IntoIterator<Item = String>) {
    let Some(reader) = READER.get() else { return };
    let mut cache = lock(&reader.cache);
    for path in paths.into_iter().filter(|path| can_read(path)) {
        if !cache.slots.contains_key(&path) {
            cache.slots.insert(path.clone(), Slot::Queued);
            cache.queue.push_back(path);
        }
    }
    reader.wake.notify_one();
}

/// The title tag, once read. Outer None means not read yet.
pub fn title(path: &str) -> Option<Option<String>> {
    let reader = READER.get()?;
    lock(&reader.cache).titles.get(path).cloned()
}

/// Queues title reads for the playlist, behind every full read.
pub fn read_titles(paths: &[String]) {
    let Some(reader) = READER.get() else { return };
    let mut cache = lock(&reader.cache);
    let mut queued = false;
    for path in paths.iter().filter(|path| can_read(path)) {
        if !cache.titles.contains_key(path) && !cache.title_queue.contains(path) {
            cache.title_queue.push_back(path.clone());
            queued = true;
        }
    }
    if queued {
        reader.wake.notify_one();
    }
}

/// Moves a file to the front of the queue, for the file that is starting right now.
pub fn read_next(path: &str) {
    let Some(reader) = READER.get() else { return };
    if !can_read(path) {
        return;
    }
    let mut cache = lock(&reader.cache);
    if matches!(cache.slots.get(path), Some(Slot::Ready(_) | Slot::Unreadable)) {
        return;
    }
    cache.queue.retain(|queued| queued != path);
    cache.queue.push_front(path.to_string());
    cache.slots.insert(path.to_string(), Slot::Queued);
    reader.wake.notify_one();
}

fn open(path: &Path, cover_art: bool) -> Option<TaggedFile> {
    let options = ParseOptions::new().read_properties(false).read_cover_art(cover_art);
    Probe::open(path).ok()?.options(options).guess_file_type().ok()?.read().ok()
}

fn tag_text(file: &TaggedFile, key: ItemKey) -> Option<String> {
    file.primary_tag()
        .or_else(|| file.first_tag())
        .and_then(|tag| tag.get_string(key))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn read_title(path: &Path) -> Option<String> {
    tag_text(&open(path, false)?, ItemKey::TrackTitle)
}

// The same keys mpv's metadata gives the audio view: artist, else album artist, and the year of the date.
fn read(path: &Path) -> Option<AudioInfo> {
    let file = open(path, true)?;
    let text = |key: ItemKey| tag_text(&file, key);
    let artist = text(ItemKey::TrackArtist).or_else(|| text(ItemKey::AlbumArtist)).unwrap_or_default();
    let year = text(ItemKey::RecordingDate)
        .or_else(|| text(ItemKey::Year))
        .map(|date| date.chars().take(4).collect::<String>());
    let album_line = match (text(ItemKey::AlbumTitle), year) {
        (Some(album), Some(year)) => format!("{album} · {year}"),
        (Some(album), None) => album,
        (None, Some(year)) => year,
        (None, None) => String::new(),
    };
    let pictures: Vec<&Picture> = file.tags().iter().flat_map(|tag| tag.pictures()).collect();
    let front = pictures.iter().find(|picture| picture.pic_type() == PictureType::CoverFront);
    let embedded = front.or(pictures.first()).and_then(|picture| decode(picture.data()));
    let cover = embedded.or_else(|| cover_next_to(path));
    Some(AudioInfo { title: text(ItemKey::TrackTitle), artist, album_line, cover })
}

fn cover_next_to(path: &Path) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let entries = std::fs::read_dir(path.parent()?).ok()?;
    let mut candidates: Vec<(usize, std::path::PathBuf)> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter_map(|candidate| {
            let name = candidate.file_stem()?.to_string_lossy().to_lowercase();
            let extension = candidate.extension()?.to_string_lossy().to_lowercase();
            let rank = COVER_NAMES.iter().position(|cover| *cover == name)?;
            COVER_EXTENSIONS.contains(&extension.as_str()).then_some((rank, candidate))
        })
        .collect();
    candidates.sort();
    candidates.into_iter().find_map(|(_, candidate)| decode(&std::fs::read(candidate).ok()?))
}

fn decode(bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let image: DynamicImage = image::load_from_memory(bytes).ok()?;
    let image = if image.width() > COVER_PIXELS || image.height() > COVER_PIXELS {
        image.thumbnail(COVER_PIXELS, COVER_PIXELS)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()))
}
