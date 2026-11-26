use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::logging::{self, Level};
use crate::mpv::Mpv;

use super::Bridge;
use super::host;
use super::manifest::PluginManifest;
use super::package::sha256_hex;
use super::store::Store;

const MEMORY_PAGES: u32 = 16384;
const QUEUE_SIZE: usize = 64;
const LOG_LINES: usize = 300;
const LOG_LINE_CHARS: usize = 2000;
const WATCHDOG_TICK: Duration = Duration::from_millis(20);
pub const TIMEOUTS_BEFORE_PAUSE: u32 = 3;

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    pub time: String,
    pub level: &'static str,
    pub text: String,
}

/// The lines shown on the plugin's page. They also go to Rona's own log as `plugin/<id>`.
pub struct LogBuffer {
    plugin_id: String,
    lines: Mutex<VecDeque<LogLine>>,
    pub generation: AtomicU64,
}

impl LogBuffer {
    pub fn new(plugin_id: &str) -> Self {
        Self { plugin_id: plugin_id.to_owned(), lines: Mutex::new(VecDeque::new()), generation: AtomicU64::new(0) }
    }

    pub fn push(&self, level: &str, text: &str) {
        let (level, rona_level) = match level {
            "debug" => ("debug", Level::Debug),
            "warn" => ("warn", Level::Warn),
            "error" => ("error", Level::Error),
            _ => ("info", Level::Info),
        };
        let text: String = text.chars().take(LOG_LINE_CHARS).collect();
        logging::write_target(rona_level, &format!("plugin/{}", self.plugin_id), format_args!("{text}"));
        let mut lines = lock(&self.lines);
        if lines.len() == LOG_LINES {
            lines.pop_front();
        }
        lines.push_back(LogLine { time: chrono::Local::now().format("%H:%M:%S").to_string(), level, text });
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn lines(&self) -> Vec<LogLine> {
        lock(&self.lines).iter().cloned().collect()
    }
}

/// What the plugin's own mpv client last saw, so state reads don't wait on mpv's core.
#[derive(Debug, Clone, Default)]
pub struct PlayerSnap {
    pub paused: bool,
    pub idle: bool,
    pub opening: bool,
    pub position: f64,
    pub duration: f64,
    pub speed: f64,
    pub volume: f64,
    pub muted: bool,
    pub title: String,
    pub path: String,
    pub chapter: i64,
}

#[derive(Default)]
pub struct CallState {
    pub user_action: bool,
    pub response_body: Vec<u8>,
    pub process_stdout: Vec<u8>,
}

#[derive(Default)]
struct WatchState {
    done: bool,
    paused: bool,
    timed_out: bool,
}

#[derive(Default)]
pub struct Watch {
    state: Mutex<WatchState>,
}

impl Watch {
    pub fn set_paused(&self, paused: bool) {
        lock(&self.state).paused = paused;
    }

    fn finish(&self) -> bool {
        let mut state = lock(&self.state);
        state.done = true;
        state.timed_out
    }
}

fn watchdog(watch: Arc<Watch>, budget: Duration, cancel: extism::CancelHandle) {
    let mut used = Duration::ZERO;
    loop {
        std::thread::sleep(WATCHDOG_TICK);
        let mut state = lock(&watch.state);
        if state.done {
            return;
        }
        if state.paused {
            continue;
        }
        used += WATCHDOG_TICK;
        if used >= budget {
            state.timed_out = true;
            let _ = cancel.cancel();
            return;
        }
    }
}

/// An update that came too soon after the last one, sent when the interval is up. Newer ones replace or join it.
pub enum Pending {
    Control(String, Value),
    Tree(String, Vec<Value>),
    Patch(String, Vec<Value>),
}

#[derive(Default)]
pub struct Limits {
    pub last_osd: Option<Instant>,
    pub last_sent: HashMap<String, Instant>,
    pub pending: HashMap<String, Pending>,
}

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct PipeConn {
    pub name: String,
    pub writer: Sender<Vec<u8>>,
    pub received: Arc<Mutex<Vec<u8>>>,
    /// Set while a `pipe-data` event is queued that the plugin hasn't read for yet.
    pub told: Arc<AtomicBool>,
}

/// Everything a plugin's host functions reach, shared by its worker, its mpv client thread and its timers.
pub struct Shared {
    pub id: String,
    pub manifest: PluginManifest,
    pub store: Arc<Store>,
    pub log: Arc<LogBuffer>,
    pub bridge: Arc<dyn Bridge>,
    /// The plugin's own mpv client. Let go when the plugin stops, since mpv only shuts down once every client is gone.
    pub mpv: RwLock<Option<Mpv>>,
    pub granted: RwLock<HashSet<String>>,
    pub sender: SyncSender<Job>,
    pub call: Mutex<CallState>,
    pub watch: Mutex<Option<Arc<Watch>>>,
    pub agent: ureq::Agent,
    pub player: Mutex<PlayerSnap>,
    pub position_interval_ms: AtomicU64,
    pub timers: Mutex<HashMap<String, Arc<AtomicBool>>>,
    pub observed: Mutex<HashMap<String, u64>>,
    pub limits: Mutex<Limits>,
    pub images: Mutex<HashMap<String, Arc<Image>>>,
    pub image_bytes: AtomicU64,
    pub pipes: Mutex<HashMap<u32, PipeConn>>,
    pub processes: Mutex<HashMap<u32, super::process::Running>>,
    pub overlays: Mutex<HashSet<u32>>,
    pub stopped: AtomicBool,
    pub paused: AtomicBool,
    pub timeouts: AtomicU32,
    pub last_error: Mutex<Option<String>>,
}

impl Shared {
    /// `None` once the plugin stopped.
    pub fn client(&self) -> Option<Mpv> {
        self.mpv.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn release_client(&self) {
        self.mpv.write().unwrap_or_else(PoisonError::into_inner).take();
    }

    pub fn granted(&self, permission: &str) -> bool {
        self.granted.read().unwrap_or_else(PoisonError::into_inner).contains(permission)
    }

    pub fn granted_list(&self) -> Vec<String> {
        let granted = self.granted.read().unwrap_or_else(PoisonError::into_inner);
        let mut list: Vec<String> = granted.iter().cloned().collect();
        list.sort();
        list
    }

    pub fn granted_hosts(&self) -> Vec<String> {
        self.granted
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|grant| grant.strip_prefix("network:").map(str::to_owned))
            .collect()
    }

    pub fn runs(&self) -> bool {
        !self.stopped.load(Ordering::SeqCst) && !self.paused.load(Ordering::SeqCst)
    }

    /// Queues an `on_event` call. Events are dropped while the queue is full, so a slow plugin can't
    /// hold up the player.
    /// False when the event didn't reach the plugin's queue.
    pub fn post_event(&self, event: Value) -> bool {
        self.post_event_from_user(event, false)
    }

    pub fn post_event_from_user(&self, event: Value, user_action: bool) -> bool {
        if !self.runs() || !self.manifest.has_hook(super::manifest::HOOK_ON_EVENT) {
            return false;
        }
        let kind = event.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
        let job = Job::Call(Call {
            hook: super::manifest::HOOK_ON_EVENT,
            input: json!({ "event": event }),
            budget: super::BUDGET_EVENT,
            user_action,
            reply: None,
        });
        match self.sender.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) if kind == "position" => false,
            Err(TrySendError::Full(_)) => {
                self.log.push("warn", &format!("A {kind} event was dropped, the plugin is busy"));
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Runs a hook and waits for its output. Never called on the UI thread.
    pub fn call_and_wait(&self, hook: &'static str, input: Value, budget: Duration, user_action: bool) -> Result<Vec<u8>, String> {
        if !self.runs() {
            return Err("The plugin is not running".into());
        }
        if !self.manifest.has_hook(hook) {
            return Err(format!("The plugin has no {hook} hook"));
        }
        let (reply, answer) = std::sync::mpsc::channel();
        let job = Job::Call(Call { hook, input, budget, user_action, reply: Some(reply) });
        if self.sender.try_send(job).is_err() {
            return Err("The plugin is busy".into());
        }
        answer.recv().unwrap_or_else(|_| Err("The plugin stopped before it answered".into()))
    }
}

pub struct Call {
    pub hook: &'static str,
    pub input: Value,
    pub budget: Duration,
    pub user_action: bool,
    pub reply: Option<Sender<Result<Vec<u8>, String>>>,
}

pub enum Job {
    Call(Call),
    Stop,
}

pub struct WasmSource {
    pub path: PathBuf,
    /// `None` for plugins loaded from a folder, which have no hash lock.
    pub expected_sha256: Option<String>,
}

pub fn channel() -> (SyncSender<Job>, Receiver<Job>) {
    sync_channel(QUEUE_SIZE)
}

pub fn spawn_worker(shared: Arc<Shared>, source: WasmSource, receiver: Receiver<Job>) {
    let name = format!("plugin {}", shared.id);
    let spawned = std::thread::Builder::new().name(name).spawn(move || run(shared, source, receiver));
    if let Err(err) = spawned {
        crate::logging::warn!("could not start a plugin worker: {err}");
    }
}

fn run(shared: Arc<Shared>, source: WasmSource, receiver: Receiver<Job>) {
    let mut compiled: Option<extism::CompiledPlugin> = None;
    let mut instance: Option<extism::Plugin> = None;
    while let Ok(job) = receiver.recv() {
        let Job::Call(call) = job else { break };
        let hook = call.hook;
        let reply = call.reply.clone();
        let result = if shared.paused.load(Ordering::SeqCst) {
            Err("The plugin is paused".to_string())
        } else {
            run_call(&shared, &source, &mut compiled, &mut instance, call)
        };
        if let Err(message) = &result {
            shared.log.push("error", &format!("{hook}: {message}"));
        }
        let error = result.as_ref().err().cloned();
        let changed = {
            let mut last = lock(&shared.last_error);
            let changed = *last != error;
            *last = error;
            changed
        };
        if changed {
            shared.bridge.status_changed();
        }
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    }
}

fn compile(shared: &Arc<Shared>, source: &WasmSource) -> Result<extism::CompiledPlugin, String> {
    let wasm = std::fs::read(&source.path).map_err(|err| format!("Couldn't read plugin.wasm: {err}"))?;
    if let Some(expected) = &source.expected_sha256
        && sha256_hex(&wasm) != *expected
    {
        return Err("plugin.wasm changed on disk since it was installed".into());
    }
    let manifest = extism::Manifest::new([extism::Wasm::data(wasm)]).with_memory_max(MEMORY_PAGES);
    // WASI with no folders and no environment, which the JavaScript runtime needs for its clock and random numbers.
    extism::PluginBuilder::new(manifest)
        .with_wasi(true)
        .with_functions(host::functions(shared))
        .compile()
        .map_err(|err| format!("Couldn't load the plugin: {err}"))
}

fn run_call(
    shared: &Arc<Shared>,
    source: &WasmSource,
    compiled: &mut Option<extism::CompiledPlugin>,
    instance: &mut Option<extism::Plugin>,
    call: Call,
) -> Result<Vec<u8>, String> {
    if instance.is_none() {
        if compiled.is_none() {
            *compiled = Some(compile(shared, source)?);
        }
        let built = extism::Plugin::new_from_compiled(compiled.as_ref().expect("compiled above"))
            .map_err(|err| format!("Couldn't start the plugin: {err}"))?;
        *instance = Some(built);
    }
    let plugin = instance.as_mut().expect("instance built above");
    if !plugin.function_exists(call.hook) {
        return Err(format!("The plugin lists {} but doesn't export it", call.hook));
    }

    let mut input = call.input;
    if let Some(fields) = input.as_object_mut() {
        fields.insert("granted".into(), json!(shared.granted_list()));
    }
    let input = input.to_string().into_bytes();

    *lock(&shared.call) = CallState { user_action: call.user_action, ..CallState::default() };
    let watch = Arc::new(Watch::default());
    *lock(&shared.watch) = Some(watch.clone());
    let cancel = plugin.cancel_handle();
    let watchdog_watch = watch.clone();
    let budget = call.budget;
    std::thread::spawn(move || watchdog(watchdog_watch, budget, cancel));

    let result = plugin.call::<&[u8], Vec<u8>>(call.hook, &input);

    let timed_out = watch.finish();
    *lock(&shared.watch) = None;
    *lock(&shared.call) = CallState::default();

    let too_long = || format!("Took longer than {} s", budget.as_secs_f32());
    match result {
        Ok(output) if !timed_out => {
            shared.timeouts.store(0, Ordering::SeqCst);
            Ok(output)
        }
        Ok(_) => {
            *instance = None;
            count_timeout(shared);
            Err(too_long())
        }
        Err(err) => {
            *instance = None;
            if timed_out {
                count_timeout(shared);
                Err(too_long())
            } else {
                Err(err.to_string())
            }
        }
    }
}

fn count_timeout(shared: &Shared) {
    if shared.timeouts.fetch_add(1, Ordering::SeqCst) + 1 < TIMEOUTS_BEFORE_PAUSE {
        return;
    }
    shared.paused.store(true, Ordering::SeqCst);
    shared.log.push("error", "Paused: ran out of time 3 times in a row. Turn it off and on again to resume.");
    shared.bridge.status_changed();
}
