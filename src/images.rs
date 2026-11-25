use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError};

use image::DynamicImage;
use slint::{Rgba8Pixel, SharedPixelBuffer};

use crate::http_cache;
use crate::logging::{debug, warn};

const MAX_DOWNLOAD: u64 = 20 * 1024 * 1024;
const MAX_CACHED: usize = 600;

/// The box an image is cropped to, at twice the size it's drawn so it stays sharp on high density screens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Size {
    Wide,
    Square,
    Poster,
    Card,
    Large,
}

impl Size {
    fn pixels(self) -> (u32, u32) {
        match self {
            Size::Wide => (120, 68),
            Size::Square => (68, 68),
            Size::Poster => (92, 132),
            Size::Card => (116, 116),
            Size::Large => (640, 640),
        }
    }
}

#[derive(Clone)]
pub enum State {
    Loading,
    Ready(SharedPixelBuffer<Rgba8Pixel>),
    Failed,
}

type Key = (String, Size);

struct Loader {
    cache: Mutex<Cache>,
    wake: Condvar,
}

#[derive(Default)]
struct Cache {
    done: HashMap<Key, Option<SharedPixelBuffer<Rgba8Pixel>>>,
    order: VecDeque<Key>,
    queue: VecDeque<Key>,
    listener: Option<Box<dyn Fn() + Send>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn loader() -> &'static Loader {
    static LOADER: OnceLock<Loader> = OnceLock::new();
    LOADER.get_or_init(|| {
        std::thread::spawn(work);
        Loader { cache: Mutex::new(Cache::default()), wake: Condvar::new() }
    })
}

/// Called on the loader's thread after each image is ready, so the UI can show it.
pub fn on_loaded(listener: impl Fn() + Send + 'static) {
    lock(&loader().cache).listener = Some(Box::new(listener));
}

/// The image if it's ready, queuing it the first time it's asked for.
pub fn get(source: &str, size: Size) -> State {
    let key = (source.to_string(), size);
    let loader = loader();
    let mut cache = lock(&loader.cache);
    match cache.done.get(&key) {
        Some(Some(image)) => State::Ready(image.clone()),
        Some(None) => State::Failed,
        None => {
            if !cache.queue.contains(&key) {
                cache.queue.push_back(key);
                loader.wake.notify_one();
            }
            State::Loading
        }
    }
}

pub fn ready(source: &str, size: Size) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    match get(source, size) {
        State::Ready(image) => Some(image),
        _ => None,
    }
}

/// Drops a source from the cache, for an image changed on disk while Rona runs.
pub fn forget(source: &str) {
    let mut cache = lock(&loader().cache);
    cache.done.retain(|(cached, _), _| cached != source);
    cache.order.retain(|(cached, _)| cached != source);
}

fn work() {
    let loader = loader();
    loop {
        let key = {
            let mut cache = lock(&loader.cache);
            loop {
                if let Some(key) = cache.queue.pop_front() {
                    break key;
                }
                cache = loader.wake.wait(cache).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let image = load(&key.0, key.1);
        let mut cache = lock(&loader.cache);
        cache.done.insert(key.clone(), image);
        cache.order.push_back(key);
        while cache.order.len() > MAX_CACHED {
            if let Some(oldest) = cache.order.pop_front() {
                cache.done.remove(&oldest);
            }
        }
        if let Some(listener) = &cache.listener {
            listener();
        }
    }
}

fn load(source: &str, size: Size) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let bytes = match read(source) {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!("could not read the image {source}: {err}");
            return None;
        }
    };
    let image: DynamicImage = match image::load_from_memory(&bytes) {
        Ok(image) => image,
        Err(err) => {
            warn!("could not decode the image {source}: {err}");
            return None;
        }
    };
    let (width, height) = size.pixels();
    let image = if size == Size::Large {
        if image.width() > width || image.height() > height { image.thumbnail(width, height) } else { image }
    } else {
        image.resize_to_fill(width, height, image::imageops::FilterType::Triangle)
    };
    debug!("image {source} ready at {}x{}", image.width(), image.height());
    let rgba = image.to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()))
}

fn read(source: &str) -> Result<Vec<u8>, String> {
    if !source.contains("://") {
        return std::fs::read(source).map_err(|err| err.to_string());
    }
    let response = http_cache::request(&http_cache::agent(), source).call().map_err(|err| err.to_string())?;
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(MAX_DOWNLOAD)
        .read_to_end(&mut bytes)
        .map_err(|err| err.to_string())?;
    Ok(bytes)
}
