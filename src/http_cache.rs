use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::logging::{debug, info, warn};

pub const PROTOCOL: &str = "rpcache";

pub const DEFAULT_CACHE_MIB: u64 = 512;
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

const CHUNK_SIZE: u64 = 1024 * 1024;
const READ_BUFFER_SIZE: usize = 64 * 1024;
const RETRY_DELAYS: [Duration; 3] = [Duration::from_secs(3), Duration::from_secs(6), Duration::from_secs(12)];
const RETRY_POLL: Duration = Duration::from_millis(100);
const SLOW_CHUNK: Duration = Duration::from_secs(5);

#[derive(Clone, PartialEq)]
pub struct NetworkOptions {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub user_agent: String,
}

static MAX_CACHED_CHUNKS: AtomicUsize = AtomicUsize::new((DEFAULT_CACHE_MIB * 1024 * 1024 / CHUNK_SIZE) as usize);
static NETWORK: Mutex<NetworkOptions> = Mutex::new(NetworkOptions {
    connect_timeout: DEFAULT_CONNECT_TIMEOUT,
    read_timeout: DEFAULT_READ_TIMEOUT,
    user_agent: String::new(),
});

// The cache size applies right away, the network options from the next stream that opens.
pub fn configure(cache_mib: u64, network: NetworkOptions) {
    let chunks = (cache_mib * 1024 * 1024 / CHUNK_SIZE).max(1) as usize;
    MAX_CACHED_CHUNKS.store(chunks, Ordering::Relaxed);
    *lock(&NETWORK) = network;
}

pub type Response = ureq::http::Response<ureq::Body>;

// The read timeout applies to each read, not to the whole body, which for a film can take hours.
pub fn agent() -> ureq::Agent {
    let network = lock(&NETWORK);
    let builder = ureq::Agent::config_builder()
        .timeout_connect(Some(network.connect_timeout))
        .timeout_recv_response(Some(network.read_timeout))
        .timeout_per_read(Some(network.read_timeout));
    let builder = match network.user_agent.as_str() {
        "" => builder,
        user_agent => builder.user_agent(user_agent),
    };
    ureq::Agent::new_with_config(builder.build())
}

type HeadersByLink = HashMap<String, Vec<(String, String)>>;

// Headers a playlist asks for, per link. Cleared when a new playlist opens.
static HEADERS: Mutex<Option<HeadersByLink>> = Mutex::new(None);

pub fn add_headers(url: String, headers: Vec<(String, String)>) {
    lock(&HEADERS).get_or_insert_with(HashMap::new).insert(url, headers);
}

pub fn clear_headers() {
    *lock(&HEADERS) = None;
}

/// A GET for `url` with the headers its playlist gave, which win over the agent's own User-Agent.
pub fn request(agent: &ureq::Agent, url: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    let mut request = agent.get(url);
    if let Some(headers) = lock(&HEADERS).as_ref().and_then(|headers| headers.get(url)) {
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
    }
    request
}

pub fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response.headers().get(name)?.to_str().ok()
}

pub fn network_error_text(err: &ureq::Error, host: &str) -> String {
    match err {
        ureq::Error::StatusCode(code) => {
            let reason = ureq::http::StatusCode::from_u16(*code).ok().and_then(|status| status.canonical_reason());
            match reason {
                Some(reason) => format!("{code} {reason}"),
                None => code.to_string(),
            }
        }
        ureq::Error::HostNotFound => format!("Couldn't find {host}"),
        ureq::Error::TooManyRedirects => "Too many redirects".into(),
        ureq::Error::Timeout(ureq::Timeout::Connect | ureq::Timeout::Resolve) => {
            no_response_text(lock(&NETWORK).connect_timeout)
        }
        ureq::Error::Timeout(_) => no_response_text(lock(&NETWORK).read_timeout),
        ureq::Error::Io(io) if is_timeout(io) => no_response_text(lock(&NETWORK).read_timeout),
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) | ureq::Error::TlsRequired => "Secure connection failed".into(),
        _ => format!("Couldn't connect to {host}"),
    }
}

pub fn io_error_text(err: &std::io::Error, host: &str) -> String {
    if is_timeout(err) {
        no_response_text(lock(&NETWORK).read_timeout)
    } else {
        format!("Lost the connection to {host}")
    }
}

fn is_timeout(err: &std::io::Error) -> bool {
    matches!(err.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock)
}

fn no_response_text(timeout: Duration) -> String {
    format!("No response for {} s", timeout.as_secs())
}

static OPEN_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Why mpv's last open of a link failed, since mpv itself only reports a generic loading error.
pub fn take_open_error() -> Option<String> {
    lock(&OPEN_ERROR).take()
}

static LAST_PROGRESS_AT: AtomicU64 = AtomicU64::new(0);

fn clock_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

fn note_progress() {
    LAST_PROGRESS_AT.store(clock_ms(), Ordering::Relaxed);
}

/// Time spent waiting on the network, not counting time mpv read nothing because its buffer was full.
pub fn seconds_without_data() -> Option<u64> {
    match LAST_PROGRESS_AT.load(Ordering::Relaxed) {
        0 => None,
        at => Some(clock_ms().saturating_sub(at) / 1000),
    }
}

pub enum ConnectionState {
    Fine,
    Retrying { seconds_left: u64 },
}

const STATE_FINE: u8 = 0;
const STATE_RETRYING: u8 = 1;

static CONNECTION_STATE: AtomicU8 = AtomicU8::new(STATE_FINE);
static RETRY_AT: AtomicU64 = AtomicU64::new(0);
static RETRY_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn connection_state() -> ConnectionState {
    match CONNECTION_STATE.load(Ordering::Relaxed) {
        STATE_RETRYING => {
            let left = RETRY_AT.load(Ordering::Relaxed).saturating_sub(clock_ms());
            ConnectionState::Retrying { seconds_left: left.div_ceil(1000) }
        }
        _ => ConnectionState::Fine,
    }
}

pub fn retry_now() {
    RETRY_REQUESTED.store(true, Ordering::Relaxed);
}

// Waits out a retry delay, or until the user asks.
fn wait_for_retry(delay: Duration, cancelled: &AtomicBool, playback: bool) -> Result<(), String> {
    let until = clock_ms() + delay.as_millis() as u64;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            if playback {
                CONNECTION_STATE.store(STATE_FINE, Ordering::Relaxed);
            }
            return Err("cancelled".into());
        }
        if (playback && RETRY_REQUESTED.swap(false, Ordering::Relaxed)) || clock_ms() >= until {
            return Ok(());
        }
        std::thread::sleep(RETRY_POLL);
    }
}

const ERROR_LOADING_FAILED: c_int = -13;
const ERROR_UNSUPPORTED: i64 = -18;
const ERROR_GENERIC: i64 = -20;

#[repr(C)]
struct StreamInfo {
    cookie: *mut c_void,
    read_fn: Option<unsafe extern "C" fn(*mut c_void, *mut c_char, u64) -> i64>,
    seek_fn: Option<unsafe extern "C" fn(*mut c_void, i64) -> i64>,
    size_fn: Option<unsafe extern "C" fn(*mut c_void) -> i64>,
    close_fn: Option<unsafe extern "C" fn(*mut c_void)>,
    cancel_fn: Option<unsafe extern "C" fn(*mut c_void)>,
}

pub fn wrap_url(source: &str) -> String {
    if source.starts_with("http://") || source.starts_with("https://") {
        format!("{PROTOCOL}://{source}")
    } else {
        source.to_string()
    }
}

pub fn unwrap_url(source: &str) -> String {
    source.strip_prefix(PROTOCOL).and_then(|rest| rest.strip_prefix("://")).unwrap_or(source).to_string()
}

static DOWNLOADED_BYTES: AtomicU64 = AtomicU64::new(0);
static CURRENT: OnceLock<Mutex<Option<(String, Arc<Mutex<ByteCache>>)>>> = OnceLock::new();

pub fn downloaded_bytes() -> u64 {
    DOWNLOADED_BYTES.load(Ordering::Relaxed)
}

pub fn file_size(source: &str) -> Option<u64> {
    cache_of(source).and_then(|cache| lock(&cache).size)
}

pub fn read_position(source: &str) -> Option<u64> {
    cache_of(source).map(|cache| lock(&cache).read_head)
}

pub fn cached_byte_ranges(source: &str) -> Vec<(u64, u64)> {
    let Some(cache) = cache_of(source) else { return Vec::new() };
    let cache = lock(&cache);
    let mut indices: Vec<u64> = cache.chunks.keys().copied().collect();
    indices.sort_unstable();
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for index in indices {
        let length = cache.chunks.get(&index).map_or(0, |chunk| chunk.len() as u64);
        let (start, end) = (index * CHUNK_SIZE, index * CHUNK_SIZE + length);
        match ranges.last_mut() {
            Some(last) if last.1 == start => last.1 = end,
            _ => ranges.push((start, end)),
        }
    }
    ranges
}

/// Random access to a link for a recording: cached chunks come from memory, missing ones are downloaded.
pub struct LinkReader {
    stream: HttpStream,
    cancelled: AtomicBool,
}

impl LinkReader {
    // Uses the player's cache while the link is still the one playing, and a cache of its own otherwise.
    pub fn open(source: &str) -> Result<Self, String> {
        let url = source
            .strip_prefix(PROTOCOL)
            .and_then(|rest| rest.strip_prefix("://"))
            .ok_or_else(|| format!("{source} is not a link"))?;
        let id = NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed);
        info!("stream #{id}: opening {url} for a recording");
        let shared = cache_of(source);
        let stream = HttpStream::open(id, url, false, || shared.unwrap_or_default())
            .map_err(|err| network_error_text(&err, &crate::media::link_host(url)))?;
        Ok(Self { stream, cancelled: AtomicBool::new(false) })
    }

    pub fn size(&self) -> Option<u64> {
        self.stream.size
    }

    pub fn read_at(&mut self, position: u64, buffer: &mut [u8]) -> Result<usize, String> {
        self.stream.position = position;
        self.stream.read(buffer, &self.cancelled)
    }
}

fn cache_of(source: &str) -> Option<Arc<Mutex<ByteCache>>> {
    let url = source.strip_prefix(PROTOCOL)?.strip_prefix("://")?;
    match current().as_ref() {
        Some((cached_url, cache)) if cached_url == url => Some(cache.clone()),
        _ => None,
    }
}

fn current() -> MutexGuard<'static, Option<(String, Arc<Mutex<ByteCache>>)>> {
    lock(CURRENT.get_or_init(|| Mutex::new(None)))
}

#[derive(Default)]
struct ByteCache {
    chunks: HashMap<u64, Arc<[u8]>>,
    insertion_order: VecDeque<u64>,
    size: Option<u64>,
    read_head: u64,
    evicting: bool,
}

impl ByteCache {
    fn get(&self, index: u64) -> Option<Arc<[u8]>> {
        self.chunks.get(&index).cloned()
    }

    fn insert(&mut self, index: u64, chunk: Arc<[u8]>) {
        if self.chunks.insert(index, chunk).is_none() {
            self.insertion_order.push_back(index);
        }
        while self.chunks.len() > MAX_CACHED_CHUNKS.load(Ordering::Relaxed) {
            let Some(oldest) = self.insertion_order.pop_front() else { break };
            if !self.evicting {
                self.evicting = true;
                info!("byte cache full at {} MiB, dropping the oldest chunks from now on", self.chunks.len());
            }
            self.chunks.remove(&oldest);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// Only the most recently opened URL keeps its cache, so memory stays bounded.
fn cache_for(url: &str) -> Arc<Mutex<ByteCache>> {
    let mut current = current();
    if let Some((cached_url, cache)) = current.as_ref() {
        if cached_url == url {
            return cache.clone();
        }
    }
    if let Some((cached_url, cache)) = current.as_ref() {
        info!("dropping the byte cache of {cached_url} ({} MiB)", lock(cache).chunks.len());
    }
    let cache = Arc::new(Mutex::new(ByteCache::default()));
    *current = Some((url.to_string(), cache.clone()));
    cache
}

struct Connection {
    reader: Box<dyn Read + Send + Sync>,
    position: u64,
}

impl Connection {
    fn new(response: Response, position: u64) -> Self {
        Self { reader: Box::new(response.into_body().into_reader()), position }
    }
}

static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

struct HttpStream {
    // Numbers each open in the log, since mpv can still hold the previous file's stream while opening the next.
    id: u64,
    downloaded: u64,
    download_time: Duration,
    url: String,
    agent: ureq::Agent,
    cache: Arc<Mutex<ByteCache>>,
    position: u64,
    size: Option<u64>,
    supports_ranges: bool,
    connection: Option<Connection>,
    // Only mpv's own stream moves the read head and the connection banner, not a recording reading the same link.
    playback: bool,
}

impl HttpStream {
    fn open(
        id: u64,
        url: &str,
        playback: bool,
        cache: impl FnOnce() -> Arc<Mutex<ByteCache>>,
    ) -> Result<Self, ureq::Error> {
        let agent = agent();
        let started = Instant::now();
        let response = range_request(&agent, url, 0)?;
        let (supports_ranges, size) = if response.status().as_u16() == 206 {
            (true, header(&response, "Content-Range").and_then(total_size))
        } else {
            let accepts = header(&response, "Accept-Ranges").is_some_and(|value| value.eq_ignore_ascii_case("bytes"));
            (accepts, header(&response, "Content-Length").and_then(|value| value.parse().ok()))
        };
        info!(
            "stream #{id}: {} after {} ms, size {}, ranges {}",
            response.status().as_u16(),
            started.elapsed().as_millis(),
            size.map_or("unknown".to_string(), |size| format!("{} MiB", size / CHUNK_SIZE)),
            if supports_ranges { "supported" } else { "not supported" }
        );
        let cache = cache();
        lock(&cache).size = size;
        Ok(Self {
            id,
            downloaded: 0,
            download_time: Duration::ZERO,
            url: url.to_string(),
            agent,
            cache,
            position: 0,
            size,
            supports_ranges,
            connection: Some(Connection::new(response, 0)),
            playback,
        })
    }

    fn read(&mut self, buffer: &mut [u8], cancelled: &AtomicBool) -> Result<usize, String> {
        let index = self.position / CHUNK_SIZE;
        let chunk = match self.chunk(index, cancelled) {
            Ok(chunk) => chunk,
            Err(err) if cancelled.load(Ordering::Relaxed) => return Err(err),
            Err(err) => {
                warn!("stream #{}: read at byte {} failed, reconnecting: {err}", self.id, self.position);
                self.reconnect(index, cancelled)?
            }
        };
        let Some(chunk) = chunk else { return Ok(0) };
        let offset = (self.position % CHUNK_SIZE) as usize;
        if offset >= chunk.len() {
            return Ok(0);
        }
        let count = buffer.len().min(chunk.len() - offset);
        buffer[..count].copy_from_slice(&chunk[offset..offset + count]);
        self.position += count as u64;
        if self.playback {
            lock(&self.cache).read_head = self.position;
        }
        Ok(count)
    }

    // The usual failure is a connection the server closed while it sat idle, because mpv stops reading once its
    // buffer is full. So the first attempt is immediate, then it keeps trying with the last delay.
    fn reconnect(&mut self, index: u64, cancelled: &AtomicBool) -> Result<Option<Arc<[u8]>>, String> {
        let mut delay: Option<Duration> = None;
        let mut attempt = 0;
        loop {
            if let Some(delay) = delay {
                if self.playback {
                    RETRY_AT.store(clock_ms() + delay.as_millis() as u64, Ordering::Relaxed);
                    CONNECTION_STATE.store(STATE_RETRYING, Ordering::Relaxed);
                }
                wait_for_retry(delay, cancelled, self.playback)?;
            }
            self.connection = None;
            match self.chunk(index, cancelled) {
                Ok(chunk) => {
                    info!("stream #{}: reconnected", self.id);
                    if self.playback {
                        CONNECTION_STATE.store(STATE_FINE, Ordering::Relaxed);
                    }
                    return Ok(chunk);
                }
                Err(err) if cancelled.load(Ordering::Relaxed) => {
                    if self.playback {
                        CONNECTION_STATE.store(STATE_FINE, Ordering::Relaxed);
                    }
                    return Err(err);
                }
                Err(err) => {
                    warn!("stream #{}: reconnecting failed, trying again in {} s: {err}", self.id, RETRY_DELAYS[attempt].as_secs());
                    delay = Some(RETRY_DELAYS[attempt]);
                    attempt = (attempt + 1).min(RETRY_DELAYS.len() - 1);
                }
            }
        }
    }

    fn seek(&mut self, offset: i64) -> i64 {
        if offset < 0 || self.size.is_some_and(|size| offset as u64 > size) {
            warn!("stream #{}: seek to byte {offset} is outside the file", self.id);
            return ERROR_GENERIC;
        }
        debug!("stream #{}: seek from byte {} to {offset}", self.id, self.position);
        self.position = offset as u64;
        offset
    }

    fn chunk(&mut self, index: u64, cancelled: &AtomicBool) -> Result<Option<Arc<[u8]>>, String> {
        if let Some(chunk) = lock(&self.cache).get(index) {
            return Ok(Some(chunk));
        }
        let start = index * CHUNK_SIZE;
        if self.size.is_some_and(|size| start >= size) {
            return Ok(None);
        }
        note_progress();
        self.connect_at(start, cancelled)?;
        self.download_next_chunk(cancelled)
    }

    fn connect_at(&mut self, start: u64, cancelled: &AtomicBool) -> Result<(), String> {
        if self.connection.as_ref().is_some_and(|connection| connection.position == start) {
            return Ok(());
        }
        if self.supports_ranges {
            let started = Instant::now();
            let response = range_request(&self.agent, &self.url, start).map_err(|err| err.to_string())?;
            let expected = format!("bytes {start}-");
            let status = response.status().as_u16();
            let starts_here =
                status == 206 && header(&response, "Content-Range").is_some_and(|range| range.starts_with(&expected));
            debug!("stream #{}: request from byte {start}: {status} after {} ms", self.id, started.elapsed().as_millis());
            if starts_here {
                self.connection = Some(Connection::new(response, start));
                return Ok(());
            }
            warn!("stream #{}: the server ignored the range, reading forward from here on", self.id);
            self.supports_ranges = false;
            self.connection = (status == 200).then(|| Connection::new(response, 0));
        }

        if self.connection.as_ref().is_none_or(|connection| connection.position > start) {
            info!("stream #{}: downloading again from the start to reach byte {start}", self.id);
            let response = request(&self.agent, &self.url).call().map_err(|err| err.to_string())?;
            self.connection = Some(Connection::new(response, 0));
        }
        while self.connection.as_ref().is_some_and(|connection| connection.position < start) {
            if self.download_next_chunk(cancelled)?.is_none() {
                break;
            }
        }
        Ok(())
    }

    fn download_next_chunk(&mut self, cancelled: &AtomicBool) -> Result<Option<Arc<[u8]>>, String> {
        let mut connection = self.connection.take().ok_or("no open connection")?;
        let started = Instant::now();
        let mut data = Vec::with_capacity(CHUNK_SIZE as usize);
        let mut buffer = vec![0u8; READ_BUFFER_SIZE];
        while (data.len() as u64) < CHUNK_SIZE {
            if cancelled.load(Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            let wanted = (CHUNK_SIZE as usize - data.len()).min(buffer.len());
            let read = connection.reader.read(&mut buffer[..wanted]).map_err(|err| err.to_string())?;
            if read == 0 {
                // Cached as is, a chunk cut short would end the file early for mpv.
                if self.size.is_some_and(|size| connection.position + (data.len() as u64) < size) {
                    return Err("the server closed the connection before the end".into());
                }
                debug!("stream #{}: downloaded to the end of the file", self.id);
                break;
            }
            DOWNLOADED_BYTES.fetch_add(read as u64, Ordering::Relaxed);
            note_progress();
            data.extend_from_slice(&buffer[..read]);
        }
        if data.is_empty() {
            return Ok(None);
        }

        let index = connection.position / CHUNK_SIZE;
        let elapsed = started.elapsed();
        if self.downloaded == 0 {
            info!("stream #{}: first chunk ({index}) took {} ms", self.id, elapsed.as_millis());
        } else if elapsed >= SLOW_CHUNK {
            warn!("stream #{}: chunk {index} took {} s", self.id, elapsed.as_secs());
        }
        self.downloaded += data.len() as u64;
        self.download_time += elapsed;
        connection.position += data.len() as u64;
        self.connection = Some(connection);
        let chunk: Arc<[u8]> = data.into();
        lock(&self.cache).insert(index, chunk.clone());
        Ok(Some(chunk))
    }
}

fn range_request(agent: &ureq::Agent, url: &str, start: u64) -> Result<Response, ureq::Error> {
    request(agent, url).header("Range", format!("bytes={start}-")).call()
}

fn total_size(content_range: &str) -> Option<u64> {
    content_range.rsplit('/').next()?.trim().parse().ok()
}

struct Cookie {
    id: u64,
    cancelled: AtomicBool,
    stream: Mutex<HttpStream>,
}

pub unsafe extern "C" fn open_stream(_user_data: *mut c_void, uri: *mut c_char, info: *mut c_void) -> c_int {
    let uri = unsafe { CStr::from_ptr(uri) }.to_string_lossy().into_owned();
    let Some(url) = uri.strip_prefix(PROTOCOL).and_then(|rest| rest.strip_prefix("://")) else {
        return ERROR_LOADING_FAILED;
    };
    let id = NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed);
    info!("stream #{id}: opening {url}");
    CONNECTION_STATE.store(STATE_FINE, Ordering::Relaxed);
    RETRY_REQUESTED.store(false, Ordering::Relaxed);
    note_progress();
    match HttpStream::open(id, url, true, || cache_for(url)) {
        Ok(stream) => {
            let cookie = Box::new(Cookie { id, cancelled: AtomicBool::new(false), stream: Mutex::new(stream) });
            let info = unsafe { &mut *(info as *mut StreamInfo) };
            info.cookie = Box::into_raw(cookie) as *mut c_void;
            info.read_fn = Some(read_stream);
            info.seek_fn = Some(seek_stream);
            info.size_fn = Some(stream_size);
            info.close_fn = Some(close_stream);
            info.cancel_fn = Some(cancel_stream);
            0
        }
        Err(err) => {
            warn!("stream #{id}: could not open: {err}");
            let host = crate::media::link_host(url);
            *lock(&OPEN_ERROR) = Some(network_error_text(&err, &host));
            ERROR_LOADING_FAILED
        }
    }
}

unsafe extern "C" fn read_stream(cookie: *mut c_void, buffer: *mut c_char, size: u64) -> i64 {
    let cookie = unsafe { &*(cookie as *const Cookie) };
    let buffer = unsafe { std::slice::from_raw_parts_mut(buffer as *mut u8, size as usize) };
    match lock(&cookie.stream).read(buffer, &cookie.cancelled) {
        Ok(count) => count as i64,
        Err(err) => {
            if cookie.cancelled.load(Ordering::Relaxed) {
                debug!("stream #{}: read stopped by the cancel", cookie.id);
            } else {
                warn!("stream #{}: read failed: {err}", cookie.id);
            }
            -1
        }
    }
}

unsafe extern "C" fn seek_stream(cookie: *mut c_void, offset: i64) -> i64 {
    let cookie = unsafe { &*(cookie as *const Cookie) };
    lock(&cookie.stream).seek(offset)
}

unsafe extern "C" fn stream_size(cookie: *mut c_void) -> i64 {
    let cookie = unsafe { &*(cookie as *const Cookie) };
    lock(&cookie.stream).size.map_or(ERROR_UNSUPPORTED, |size| size as i64)
}

unsafe extern "C" fn close_stream(cookie: *mut c_void) {
    let cookie = unsafe { Box::from_raw(cookie as *mut Cookie) };
    let stream = lock(&cookie.stream);
    info!(
        "stream #{}: closed at byte {}, {} KiB downloaded at {:.0} KiB/s",
        stream.id,
        stream.position,
        stream.downloaded / 1024,
        stream.downloaded as f64 / 1024.0 / stream.download_time.as_secs_f64().max(0.001)
    );
}

// mpv calls this from another thread while a read may be waiting on the network, so it only sets the flag.
unsafe extern "C" fn cancel_stream(cookie: *mut c_void) {
    let cookie = unsafe { &*(cookie as *const Cookie) };
    cookie.cancelled.store(true, Ordering::Relaxed);
    info!("stream #{}: cancelled", cookie.id);
}
