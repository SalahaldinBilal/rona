use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
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
const AMBIENT_PIXELS: u32 = 12;
// The playlist's covers are 34 px, so this stays sharp at 2x.
const THUMB_PIXELS: u32 = 72;
// The playlist is refreshed this often while summaries are read, so covers show up during a long read.
const SUMMARY_BATCH: usize = 25;
// The names mpv's cover-art-auto looks for next to the file.
const COVER_NAMES: [&str; 8] = ["albumart", "album", "cover", "front", "albumartsmall", "folder", ".folder", "thumb"];
const COVER_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// What the audio view shows for a file, read before mpv opens it.
pub struct AudioInfo {
    pub title: Option<String>,
    pub artist: String,
    // Album and year, or the file's type and size when it has neither artist nor album.
    pub album_line: String,
    pub cover: Option<SharedPixelBuffer<Rgba8Pixel>>,
    // The cover shrunk to a few pixels. Stretched over the audio view, it looks like a heavy blur, which Slint lacks.
    pub ambient: Option<SharedPixelBuffer<Rgba8Pixel>>,
    pub bit_depth: Option<u8>,
}

/// What the playlist shows for a file. Kept for every file, since it's small.
pub struct Summary {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub thumb: Option<SharedPixelBuffer<Rgba8Pixel>>,
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
    summaries: HashMap<String, Arc<Summary>>,
    // Read only when no full read waits.
    summary_queue: VecDeque<String>,
    // Decoded thumbnails by where they came from, so the tracks of an album share one.
    thumbs: HashMap<u64, Option<SharedPixelBuffer<Rgba8Pixel>>>,
}

pub enum Read {
    Info(String),
    // Sent every few summaries and when their queue runs empty.
    Summaries,
}

enum Job {
    Info(String),
    Summary(String),
}

#[derive(Hash)]
enum CoverSource {
    Embedded(Vec<u8>),
    File(PathBuf),
}

impl CoverSource {
    fn key(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }

    fn thumb(self) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
        match self {
            CoverSource::Embedded(bytes) => decode(&bytes, THUMB_PIXELS),
            CoverSource::File(path) => decode(&std::fs::read(path).ok()?, THUMB_PIXELS),
        }
    }
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

/// `on_read` is called on the reader's thread after each full read and after each batch of summaries.
pub fn start(on_read: impl Fn(Read) + Send + 'static) {
    let reader = READER.get_or_init(|| Reader { cache: Mutex::default(), wake: Condvar::new() });
    std::thread::spawn(move || {
        let mut summaries_read = 0;
        loop {
            let job = {
                let mut cache = lock(&reader.cache);
                loop {
                    if let Some(path) = cache.queue.pop_front() {
                        break Job::Info(path);
                    }
                    if let Some(path) = cache.summary_queue.pop_front() {
                        break Job::Summary(path);
                    }
                    cache = reader.wake.wait(cache).unwrap_or_else(PoisonError::into_inner);
                }
            };
            match job {
                Job::Info(path) => {
                    let info = read(Path::new(&path));
                    let summary = Summary {
                        title: info.as_ref().and_then(|info| info.title.clone()),
                        artist: info.as_ref().map(|info| info.artist.clone()).filter(|artist| !artist.is_empty()),
                        thumb: info.as_ref().and_then(|info| info.cover.as_ref()).and_then(|cover| fit(cover, THUMB_PIXELS)),
                    };
                    let mut cache = lock(&reader.cache);
                    cache.summaries.insert(path.clone(), Arc::new(summary));
                    let slot = match info {
                        Some(info) => Slot::Ready(Arc::new(info)),
                        None => Slot::Unreadable,
                    };
                    store(&mut cache, path.clone(), slot);
                    drop(cache);
                    on_read(Read::Info(path));
                }
                Job::Summary(path) => {
                    let (title, artist, source) = read_summary(Path::new(&path));
                    let thumb = source.and_then(|source| thumb_for(reader, source));
                    let mut cache = lock(&reader.cache);
                    cache.summaries.insert(path, Arc::new(Summary { title, artist, thumb }));
                    summaries_read += 1;
                    let refresh = cache.summary_queue.is_empty() || summaries_read % SUMMARY_BATCH == 0;
                    drop(cache);
                    if refresh {
                        on_read(Read::Summaries);
                    }
                }
            }
        }
    });
}

// Decodes a cover once per source, so a whole album costs one decode.
fn thumb_for(reader: &Reader, source: CoverSource) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let key = source.key();
    if let Some(known) = lock(&reader.cache).thumbs.get(&key) {
        return known.clone();
    }
    let thumb = source.thumb();
    lock(&reader.cache).thumbs.insert(key, thumb.clone());
    thumb
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

/// The playlist's title, artist and cover for a file, once read.
pub fn summary(path: &str) -> Option<Arc<Summary>> {
    let reader = READER.get()?;
    lock(&reader.cache).summaries.get(path).cloned()
}

/// Queues summary reads for the playlist, behind every full read.
pub fn read_summaries(paths: &[String]) {
    let Some(reader) = READER.get() else { return };
    let mut cache = lock(&reader.cache);
    let mut queued = false;
    for path in paths.iter().filter(|path| can_read(path)) {
        if !cache.summaries.contains_key(path) && !cache.summary_queue.contains(path) {
            cache.summary_queue.push_back(path.clone());
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

// Only a full read needs the properties, for the bit depth.
fn open(path: &Path, properties: bool) -> Option<TaggedFile> {
    let options = ParseOptions::new().read_properties(properties).read_cover_art(true);
    Probe::open(path).ok()?.options(options).guess_file_type().ok()?.read().ok()
}

fn cover_picture(file: &TaggedFile) -> Option<&Picture> {
    let pictures: Vec<&Picture> = file.tags().iter().flat_map(|tag| tag.pictures()).collect();
    let front = pictures.iter().find(|picture| picture.pic_type() == PictureType::CoverFront);
    front.or(pictures.first()).copied()
}

fn artist_of(file: &TaggedFile) -> Option<String> {
    tag_text(file, ItemKey::TrackArtist).or_else(|| tag_text(file, ItemKey::AlbumArtist))
}

fn tag_text(file: &TaggedFile, key: ItemKey) -> Option<String> {
    file.primary_tag()
        .or_else(|| file.first_tag())
        .and_then(|tag| tag.get_string(key))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

// The cover is only located here. It's decoded later, once per source.
fn read_summary(path: &Path) -> (Option<String>, Option<String>, Option<CoverSource>) {
    let next_to = || covers_next_to(path).into_iter().next().map(CoverSource::File);
    let Some(file) = open(path, false) else { return (None, None, next_to()) };
    let embedded = cover_picture(&file).map(|picture| CoverSource::Embedded(picture.data().to_vec()));
    (tag_text(&file, ItemKey::TrackTitle), artist_of(&file), embedded.or_else(next_to))
}

// The same keys mpv's metadata gives the audio view: artist, else album artist, and the year of the date.
fn read(path: &Path) -> Option<AudioInfo> {
    let file = open(path, true)?;
    let text = |key: ItemKey| tag_text(&file, key);
    let artist = artist_of(&file).unwrap_or_default();
    let year = text(ItemKey::RecordingDate)
        .or_else(|| text(ItemKey::Year))
        .map(|date| date.chars().take(4).collect::<String>());
    let album_line = match (text(ItemKey::AlbumTitle), year) {
        (Some(album), Some(year)) => format!("{album} · {year}"),
        (Some(album), None) => album,
        (None, Some(year)) => year,
        (None, None) if artist.is_empty() => file_facts(path),
        (None, None) => String::new(),
    };
    let embedded = cover_picture(&file).and_then(|picture| decode(picture.data(), COVER_PIXELS));
    let cover = embedded.or_else(|| {
        covers_next_to(path).into_iter().find_map(|candidate| decode(&std::fs::read(candidate).ok()?, COVER_PIXELS))
    });
    let ambient = cover.as_ref().and_then(shrink);
    let bit_depth = file.properties().bit_depth();
    Some(AudioInfo { title: text(ItemKey::TrackTitle), artist, album_line, cover, ambient, bit_depth })
}

// For a file without tags, like "M4A · 3.1 MB".
fn file_facts(path: &Path) -> String {
    let extension = path.extension().map(|extension| extension.to_string_lossy().to_uppercase()).unwrap_or_default();
    let size = std::fs::metadata(path).map(|metadata| metadata.len()).ok().map(|bytes| {
        const MEGABYTE: f64 = 1024.0 * 1024.0;
        if bytes as f64 >= MEGABYTE {
            format!("{:.1} MB", bytes as f64 / MEGABYTE)
        } else {
            format!("{} KB", bytes.div_ceil(1024))
        }
    });
    [Some(extension).filter(|extension| !extension.is_empty()), size].into_iter().flatten().collect::<Vec<_>>().join(" · ")
}

fn shrink(cover: &SharedPixelBuffer<Rgba8Pixel>) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let image = image::RgbaImage::from_raw(cover.width(), cover.height(), cover.as_bytes().to_vec())?;
    let small = image::imageops::resize(&image, AMBIENT_PIXELS, AMBIENT_PIXELS, image::imageops::FilterType::Triangle);
    Some(SharedPixelBuffer::clone_from_slice(small.as_raw(), small.width(), small.height()))
}

// Best match first, in the order mpv's cover-art-auto prefers them.
fn covers_next_to(path: &Path) -> Vec<PathBuf> {
    let Some(entries) = path.parent().and_then(|folder| std::fs::read_dir(folder).ok()) else { return Vec::new() };
    let mut candidates: Vec<(usize, PathBuf)> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter_map(|candidate| {
            let name = candidate.file_stem()?.to_string_lossy().to_lowercase();
            let extension = candidate.extension()?.to_string_lossy().to_lowercase();
            let rank = COVER_NAMES.iter().position(|cover| *cover == name)?;
            COVER_EXTENSIONS.contains(&extension.as_str()).then_some((rank, candidate))
        })
        .collect();
    candidates.sort();
    candidates.into_iter().map(|(_, candidate)| candidate).collect()
}

fn decode(bytes: &[u8], max_pixels: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let image: DynamicImage = image::load_from_memory(bytes).ok()?;
    Some(to_buffer(scale_down(image, max_pixels)))
}

// Keeps the aspect ratio, and never scales up.
fn scale_down(image: DynamicImage, max_pixels: u32) -> DynamicImage {
    if image.width() > max_pixels || image.height() > max_pixels { image.thumbnail(max_pixels, max_pixels) } else { image }
}

fn to_buffer(image: DynamicImage) -> SharedPixelBuffer<Rgba8Pixel> {
    let rgba = image.to_rgba8();
    SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height())
}

fn fit(cover: &SharedPixelBuffer<Rgba8Pixel>, max_pixels: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let image = image::RgbaImage::from_raw(cover.width(), cover.height(), cover.as_bytes().to_vec())?;
    Some(to_buffer(scale_down(DynamicImage::ImageRgba8(image), max_pixels)))
}
