//! Plugins: WebAssembly modules run by Extism, each with permissions the user can turn off one by one.
//! A plugin never gets a file path, the main mpv handle or a Slint callback. Everything goes through
//! `host.rs`, which checks the switches on every call.

mod host;
pub mod ipc;
mod mpv_allow;
mod net;
mod package;
mod process;
mod pump;
mod runtime;
mod store;
pub mod ui;
mod view;

// The package rules live in the SDK crate, so the packer and the app can't disagree.
pub use rona_plugin::{manifest, permissions, tree};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::logging::{info, warn};
use crate::mpv::Mpv;

use manifest::{HOOK_RESOLVE_LINK, PluginManifest, WASM_FILE};
use package::{Package, sha256_hex};
use runtime::{Job, LogBuffer, Shared, WasmSource, lock};
use store::{PluginRow, Store};

pub const BUDGET_EVENT: Duration = Duration::from_secs(2);
pub const BUDGET_UI: Duration = Duration::from_secs(2);
pub const BUDGET_RESOLVE: Duration = Duration::from_secs(20);

/// What a plugin's threads ask of the UI thread. Every call returns at once.
pub trait Bridge: Send + Sync {
    fn status_changed(&self);
    fn osd(&self, text: String);
    fn view_tree(&self, plugin: &str, view: &str, tree: Vec<Value>);
    fn view_patch(&self, plugin: &str, view: &str, patches: Vec<Value>);
    fn images_changed(&self, plugin: &str);
    fn open_dialog(&self, plugin: &str, dialog: &str);
    fn control_set(&self, plugin: &str, control: &str, state: Value);
    fn menu_set(&self, plugin: &str, entry: &str, state: Value);
    fn open_media(&self, sources: Vec<String>, append: bool);
    fn player_control(&self, action: Value);
    fn playlist_edit(&self, op: Value);
}

#[derive(Clone)]
pub struct PlaylistSnap {
    pub title: String,
    pub path: String,
    pub current: bool,
}

static PLAYLIST: Mutex<Vec<PlaylistSnap>> = Mutex::new(Vec::new());

/// The playlist as shown, set by the UI thread whenever it changes, so plugins read it without waiting on it.
pub fn set_playlist_snapshot(entries: Vec<PlaylistSnap>) {
    *lock(&PLAYLIST) = entries;
}

pub fn playlist_snapshot() -> Vec<PlaylistSnap> {
    lock(&PLAYLIST).clone()
}

pub fn random_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos() as u64);
    format!("{:x}{:x}", nanos ^ u64::from(std::process::id()), COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_millis() as i64)
}

/// Who asked for an install, shown in the install dialog.
#[derive(Clone, Debug)]
pub struct Requester {
    pub program: String,
}

pub struct Loaded {
    pub row: PluginRow,
    pub folder: PathBuf,
    pub shared: Option<Arc<Shared>>,
    pub problem: Option<String>,
    pub log: Arc<LogBuffer>,
}

impl Loaded {
    pub fn manifest(&self) -> &PluginManifest {
        &self.row.manifest
    }

    pub fn dev(&self) -> bool {
        self.row.dev_path.is_some()
    }

    pub fn status(&self) -> (&'static str, i32) {
        match (&self.problem, &self.shared) {
            (Some(_), _) => ("Can't run", 4),
            (None, None) => ("Off", 1),
            (None, Some(shared)) if shared.paused.load(Ordering::SeqCst) => ("Paused", 4),
            (None, Some(shared)) if lock(&shared.last_error).is_some() => ("Last call failed", 4),
            (None, Some(_)) => ("On", 3),
        }
    }

    pub fn running(&self) -> Option<&Arc<Shared>> {
        self.shared.as_ref().filter(|shared| shared.runs())
    }
}

pub struct Inspection {
    pub copy: PathBuf,
    pub manifest: PluginManifest,
    pub wasm_sha256: String,
    pub installed_version: Option<String>,
    /// Every switch with whether it starts on and whether this version adds it.
    pub grants: Vec<(manifest::Grant, bool, bool)>,
    pub icon: Option<Vec<u8>>,
}

pub struct Manager {
    store: Arc<Store>,
    dir: PathBuf,
    temp: PathBuf,
    mpv: Mpv,
    bridge: Arc<dyn Bridge>,
    plugins: RwLock<Vec<Arc<Loaded>>>,
    logs: Mutex<HashMap<String, Arc<LogBuffer>>>,
}

static MANAGER: OnceLock<Arc<Manager>> = OnceLock::new();

pub fn manager() -> Option<&'static Arc<Manager>> {
    MANAGER.get()
}

pub fn start(mpv: &Mpv, bridge: Arc<dyn Bridge>) -> Option<&'static Arc<Manager>> {
    let Some(config) = crate::settings::config_dir() else {
        warn!("no config folder, so plugins are off");
        return None;
    };
    let store = match Store::open(&config) {
        Ok(store) => Arc::new(store),
        Err(err) => {
            warn!("plugins are off: {err}");
            return None;
        }
    };
    let temp = std::env::temp_dir().join("rona-plugin-installs");
    let _ = std::fs::remove_dir_all(&temp);
    let manager = Arc::new(Manager {
        store,
        dir: config.join("plugins"),
        temp,
        mpv: mpv.clone(),
        bridge,
        plugins: RwLock::new(Vec::new()),
        logs: Mutex::new(HashMap::new()),
    });
    let _ = MANAGER.set(manager);
    let manager = MANAGER.get()?;
    manager.reload_all();
    Some(manager)
}

pub fn setting_value(store: &Store, plugin: &str, manifest: &PluginManifest, id: &str) -> Value {
    store
        .kv_get(plugin, &format!("{}{id}", store::SETTING_PREFIX))
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .or_else(|| manifest.setting_default(id))
        .unwrap_or(Value::Null)
}

/// A running plugin a program may connect to, for the IPC endpoint.
pub fn serving_plugin(id: &str) -> Result<Arc<Shared>, String> {
    let manager = manager().ok_or("Plugins are off")?;
    let plugin = manager.find(id).ok_or_else(|| format!("No plugin {id} is installed"))?;
    let shared = plugin.running().ok_or_else(|| format!("The plugin {id} is off"))?;
    if !shared.granted(permissions::IPC_SERVE) {
        return Err(format!("The plugin {id} doesn't take connections"));
    }
    Ok(shared.clone())
}

/// Shows the install dialog for a file another program handed over, and waits for the user's answer.
pub fn request_install(path: PathBuf, requester: Requester) -> Result<&'static str, String> {
    let (answer, answered) = std::sync::mpsc::channel();
    let queued = slint::invoke_from_event_loop(move || ui::queue_install(path, Some(requester), Some(answer)));
    queued.map_err(|_| "Rona is closing".to_string())?;
    answered.recv_timeout(Duration::from_secs(600)).unwrap_or(Err("Nobody answered in time".into()))
}

/// Gives the first plugin that turns `url` into a playable link the chance to. Blocks, so only for the
/// opener's background threads.
pub fn resolve_link(url: &str) -> Option<(String, BTreeMap<String, String>, Option<String>)> {
    let manager = manager()?;
    for plugin in manager.all() {
        let Some(shared) = plugin.running() else { continue };
        if !shared.granted(permissions::OPEN_RESOLVE) || !shared.manifest.has_hook(HOOK_RESOLVE_LINK) {
            continue;
        }
        let output = match shared.call_and_wait(HOOK_RESOLVE_LINK, json!({ "url": url }), BUDGET_RESOLVE, false) {
            Ok(output) => output,
            Err(_) => continue,
        };
        let Ok(Value::Object(resolved)) = serde_json::from_slice::<Value>(&output) else { continue };
        let Some(link) = resolved.get("url").and_then(Value::as_str) else { continue };
        if !(link.starts_with("https://") || link.starts_with("http://")) {
            shared.log.push("warn", "resolve_link returned a link that isn't http or https, so it was ignored");
            continue;
        }
        let headers = resolved
            .get("headers")
            .and_then(Value::as_object)
            .map(|headers| headers.iter().filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_owned()))).collect())
            .unwrap_or_default();
        let title = resolved.get("title").and_then(Value::as_str).map(str::to_owned);
        info!("plugin {} resolved {url}", shared.id);
        return Some((link.to_owned(), headers, title));
    }
    None
}

fn stop(shared: &Arc<Shared>) {
    if shared.stopped.swap(true, Ordering::SeqCst) {
        return;
    }
    shared.post_stopping();
    for cancelled in lock(&shared.timers).drain().map(|(_, cancelled)| cancelled) {
        cancelled.store(true, Ordering::SeqCst);
    }
    ipc::close_all(&shared.id);
    lock(&shared.pipes).clear();
    lock(&shared.processes).clear();
    for id in lock(&shared.overlays).drain() {
        host::clear_overlay(shared, id);
    }
    pump::stop(shared);
    shared.release_client();
    let sender = shared.sender.clone();
    std::thread::spawn(move || {
        let _ = sender.send(Job::Stop);
    });
}

impl Shared {
    fn post_stopping(&self) {
        if !self.manifest.has_hook(manifest::HOOK_ON_EVENT) {
            return;
        }
        let _ = self.sender.try_send(Job::Call(runtime::Call {
            hook: manifest::HOOK_ON_EVENT,
            input: json!({ "event": { "type": "stopping" } }),
            budget: BUDGET_EVENT,
            user_action: false,
            reply: None,
        }));
    }
}

impl Manager {
    fn app_version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    fn log_for(&self, id: &str) -> Arc<LogBuffer> {
        lock(&self.logs).entry(id.to_owned()).or_insert_with(|| Arc::new(LogBuffer::new(id))).clone()
    }

    pub fn all(&self) -> Vec<Arc<Loaded>> {
        self.plugins.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    pub fn find(&self, id: &str) -> Option<Arc<Loaded>> {
        self.all().into_iter().find(|plugin| plugin.row.id == id)
    }

    fn reload_all(&self) {
        let rows = match self.store.list() {
            Ok(rows) => rows,
            Err(err) => {
                warn!("could not read the plugins: {err}");
                return;
            }
        };
        let old = std::mem::take(&mut *self.plugins.write().unwrap_or_else(std::sync::PoisonError::into_inner));
        for plugin in &old {
            if let Some(shared) = &plugin.shared {
                stop(shared);
            }
        }
        let loaded: Vec<Arc<Loaded>> = rows.into_iter().map(|row| Arc::new(self.load(row))).collect();
        *self.plugins.write().unwrap_or_else(std::sync::PoisonError::into_inner) = loaded;
        self.bridge.status_changed();
    }

    /// Restarts one plugin from the database, keeping the others running.
    fn reload_one(&self, id: &str) {
        let row = self.store.list().ok().and_then(|rows| rows.into_iter().find(|row| row.id == id));
        let mut plugins = self.plugins.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = plugins.iter().position(|plugin| plugin.row.id == id);
        if let Some(shared) = index.and_then(|index| plugins[index].shared.clone()) {
            stop(&shared);
        }
        match (index, row) {
            (Some(index), Some(row)) => plugins[index] = Arc::new(self.load(row)),
            (None, Some(row)) => plugins.push(Arc::new(self.load(row))),
            (Some(index), None) => {
                plugins.remove(index);
            }
            (None, None) => {}
        }
        drop(plugins);
        self.bridge.status_changed();
    }

    fn load(&self, mut row: PluginRow) -> Loaded {
        let folder = row.dev_path.clone().unwrap_or_else(|| self.dir.join(&row.id));
        let log = self.log_for(&row.id);
        let problem = match &row.dev_path {
            Some(dev) => match package::read_unpacked(dev, self.app_version()) {
                Ok(package) if package.manifest.id != row.id => Some("The folder now holds a different plugin".to_string()),
                Ok(package) => {
                    row.wasm_sha256 = package.wasm_sha256();
                    row.manifest = package.manifest;
                    if let Err(err) = self.store.refresh_dev(&row.manifest, &row.wasm_sha256) {
                        warn!("could not refresh plugin {}: {err}", row.id);
                    }
                    None
                }
                Err(err) => Some(err),
            },
            None => match row.manifest.validate(self.app_version()) {
                Err(err) => Some(err),
                Ok(()) => match std::fs::read(folder.join(WASM_FILE)) {
                    Ok(wasm) if sha256_hex(&wasm) == row.wasm_sha256 => None,
                    Ok(_) => Some("plugin.wasm changed on disk since it was installed".to_string()),
                    Err(err) => Some(format!("Couldn't read plugin.wasm: {err}")),
                },
            },
        };
        if let Some(problem) = &problem {
            log.push("error", problem);
        }
        let shared = (row.enabled && problem.is_none()).then(|| self.spawn(&row, &folder, &log)).flatten();
        Loaded { row, folder, shared, problem, log }
    }

    fn spawn(&self, row: &PluginRow, folder: &Path, log: &Arc<LogBuffer>) -> Option<Arc<Shared>> {
        let client = match self.mpv.create_client(&format!("plugin-{}", row.id)) {
            Ok(client) => client,
            Err(err) => {
                log.push("error", &format!("Couldn't start: {err}"));
                return None;
            }
        };
        let known: HashSet<String> = row.manifest.grant_keys().into_iter().collect();
        let granted: HashSet<String> = self
            .store
            .grants(&row.id)
            .unwrap_or_default()
            .into_iter()
            .filter(|(key, on)| *on && known.contains(key))
            .map(|(key, _)| key)
            .collect();
        let (sender, receiver) = runtime::channel();
        let shared = Arc::new(Shared {
            id: row.id.clone(),
            manifest: row.manifest.clone(),
            store: self.store.clone(),
            log: log.clone(),
            bridge: self.bridge.clone(),
            mpv: RwLock::new(Some(client)),
            granted: RwLock::new(granted),
            sender,
            call: Mutex::default(),
            watch: Mutex::default(),
            agent: net::agent(),
            player: Mutex::default(),
            position_interval_ms: AtomicU64::new(0),
            timers: Mutex::default(),
            observed: Mutex::default(),
            limits: Mutex::default(),
            images: Mutex::default(),
            image_bytes: AtomicU64::new(0),
            pipes: Mutex::default(),
            processes: Mutex::default(),
            overlays: Mutex::default(),
            stopped: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            timeouts: AtomicU32::new(0),
            last_error: Mutex::default(),
        });
        let source = WasmSource {
            path: folder.join(WASM_FILE),
            expected_sha256: row.dev_path.is_none().then(|| row.wasm_sha256.clone()),
        };
        runtime::spawn_worker(shared.clone(), source, receiver);
        pump::start(shared.clone());
        shared.post_event(json!({ "type": "started" }));
        Some(shared)
    }

    pub fn icon_path(&self, plugin: &Loaded) -> Option<PathBuf> {
        package::icon_path(&plugin.folder)
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        self.find(id).ok_or("The plugin is not installed")?;
        self.store.set_enabled(id, enabled)?;
        info!("plugin {id} turned {}", if enabled { "on" } else { "off" });
        self.reload_one(id);
        Ok(())
    }

    /// Turns one switch on or off while the plugin runs. It sees the change on its next call and gets
    /// a `permissions-changed` event.
    pub fn set_grant(&self, id: &str, key: &str, on: bool) -> Result<(), String> {
        let plugin = self.find(id).ok_or("The plugin is not installed")?;
        if !plugin.manifest().grant_keys().iter().any(|known| known == key) {
            return Err(format!("The plugin doesn't ask for {key}"));
        }
        self.store.set_grant(id, key, on)?;
        info!("plugin {id}: {key} turned {}", if on { "on" } else { "off" });
        let Some(shared) = &plugin.shared else { return Ok(()) };
        {
            let mut granted = shared.granted.write().unwrap_or_else(std::sync::PoisonError::into_inner);
            if on {
                granted.insert(key.to_owned());
            } else {
                granted.remove(key);
            }
        }
        if !on {
            match key {
                permissions::IPC_SERVE => ipc::close_all(id),
                permissions::UI_OVERLAY => {
                    for overlay in lock(&shared.overlays).drain() {
                        host::clear_overlay(shared, overlay);
                    }
                }
                permissions::PLAYBACK_READ => shared.position_interval_ms.store(0, Ordering::SeqCst),
                _ => {}
            }
            if let Some(pipe) = key.strip_prefix("ipc.connect:") {
                lock(&shared.pipes).retain(|_, open| open.name != pipe);
            }
            if let Some(program) = key.strip_prefix("system.run:") {
                lock(&shared.processes).retain(|_, running| running.program != program);
            }
        }
        shared.post_event(json!({ "type": "permissions-changed", "granted": shared.granted_list() }));
        self.bridge.status_changed();
        Ok(())
    }

    pub fn grants(&self, id: &str) -> BTreeMap<String, bool> {
        self.store.grants(id).unwrap_or_default()
    }

    /// Reads a package the user is about to see. The file is copied first, and the copy is what gets
    /// installed, so the program that handed it over can't swap it after the user looked.
    pub fn inspect(&self, path: &Path) -> Result<Inspection, String> {
        std::fs::create_dir_all(&self.temp).map_err(|err| err.to_string())?;
        let copy = self.temp.join(format!("{}.rp", random_id()));
        std::fs::copy(path, &copy).map_err(|err| format!("Couldn't read the plugin file: {err}"))?;
        let package = package::read_package(&copy, self.app_version())?;
        let installed = self.find(&package.manifest.id);
        if let Some(installed) = &installed
            && installed.dev()
        {
            return Err("A plugin with this id is loaded from a folder. Remove it first".into());
        }
        let known = installed.as_ref().map(|installed| self.grants(&installed.row.id));
        let grants = package
            .manifest
            .grants()
            .into_iter()
            .map(|grant| {
                let previous = known.as_ref().and_then(|known| known.get(&grant.key).copied());
                let is_new = known.is_some() && previous.is_none();
                (grant, previous.unwrap_or(true), is_new)
            })
            .collect();
        Ok(Inspection {
            copy,
            wasm_sha256: package.wasm_sha256(),
            installed_version: installed.map(|installed| installed.manifest().version.clone()),
            manifest: package.manifest,
            grants,
            icon: package.icon.map(|(_, bytes)| bytes),
        })
    }

    pub fn install(&self, inspection: &Inspection, grants: &BTreeMap<String, bool>) -> Result<(), String> {
        let package = package::read_package(&inspection.copy, self.app_version())?;
        if package.wasm_sha256() != inspection.wasm_sha256 {
            return Err("The plugin file changed since you reviewed it. Try again.".into());
        }
        self.write_and_record(&package, grants)?;
        let _ = std::fs::remove_file(&inspection.copy);
        Ok(())
    }

    fn write_and_record(&self, package: &Package, grants: &BTreeMap<String, bool>) -> Result<(), String> {
        let id = &package.manifest.id;
        // A running plugin holds its wasm open, so it stops before its files are replaced.
        if let Some(shared) = self.find(id).and_then(|plugin| plugin.shared.clone()) {
            stop(&shared);
        }
        package.write_to(&self.dir.join(id)).map_err(|err| format!("Couldn't write the plugin's files: {err}"))?;
        self.store.upsert(&package.manifest, &package.wasm_sha256(), None, grants, now_ms())?;
        info!("installed plugin {id} {}", package.manifest.version);
        self.reload_one(id);
        Ok(())
    }

    pub fn load_unpacked(&self, folder: &Path) -> Result<String, String> {
        let package = package::read_unpacked(folder, self.app_version())?;
        let id = package.manifest.id.clone();
        if let Some(existing) = self.find(&id)
            && !existing.dev()
        {
            return Err("A plugin with this id is installed. Uninstall it first".into());
        }
        let grants: BTreeMap<String, bool> = package.manifest.grant_keys().into_iter().map(|key| (key, true)).collect();
        self.store.upsert(&package.manifest, &package.wasm_sha256(), Some(folder), &grants, now_ms())?;
        info!("loaded plugin {id} from {}", folder.display());
        self.reload_one(&id);
        Ok(id)
    }

    pub fn reload_dev(&self, id: &str) {
        self.reload_one(id);
    }

    pub fn uninstall(&self, id: &str, delete_data: bool) -> Result<(), String> {
        let plugin = self.find(id).ok_or("The plugin is not installed")?;
        if let Some(shared) = &plugin.shared {
            stop(shared);
        }
        self.store.delete(id, delete_data)?;
        if !plugin.dev() {
            match std::fs::remove_dir_all(&plugin.folder) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => warn!("could not remove the folder of plugin {id}: {err}"),
                _ => {}
            }
        }
        lock(&self.logs).remove(id);
        info!("uninstalled plugin {id}{}", if delete_data { " and its data" } else { "" });
        self.reload_one(id);
        Ok(())
    }

    pub fn set_setting(&self, id: &str, setting: &str, value: &Value) -> Result<(), String> {
        let plugin = self.find(id).ok_or("The plugin is not installed")?;
        if !plugin.manifest().has_setting(setting) {
            return Err(format!("The plugin has no setting {setting}"));
        }
        self.store.kv_set(id, &format!("{}{setting}", store::SETTING_PREFIX), value.to_string().as_bytes())
    }

    /// Stops every plugin, when the window closes.
    pub fn shutdown(&self) {
        for plugin in self.all() {
            if let Some(shared) = &plugin.shared {
                stop(shared);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[derive(Default)]
    struct Recorded {
        controls: Mutex<Vec<(String, Value)>>,
        osd: Mutex<Vec<String>>,
    }

    struct TestBridge(Arc<Recorded>);

    impl Bridge for TestBridge {
        fn status_changed(&self) {}
        fn osd(&self, text: String) {
            lock(&self.0.osd).push(text);
        }
        fn view_tree(&self, _: &str, _: &str, _: Vec<Value>) {}
        fn view_patch(&self, _: &str, _: &str, _: Vec<Value>) {}
        fn images_changed(&self, _: &str) {}
        fn open_dialog(&self, _: &str, _: &str) {}
        fn control_set(&self, _: &str, control: &str, state: Value) {
            lock(&self.0.controls).push((control.to_owned(), state));
        }
        fn menu_set(&self, _: &str, _: &str, _: Value) {}
        fn open_media(&self, _: Vec<String>, _: bool) {}
        fn player_control(&self, _: Value) {}
        fn playlist_edit(&self, _: Value) {}
    }

    fn mpv() -> Mpv {
        #[cfg(windows)]
        {
            use windows::Win32::System::LibraryLoader::SetDllDirectoryW;
            let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/mpv/windows-x86_64");
            let wide: Vec<u16> = folder.to_string_lossy().encode_utf16().chain([0]).collect();
            unsafe { SetDllDirectoryW(windows::core::PCWSTR(wide.as_ptr())) }.unwrap();
        }
        Mpv::with_options(&[("vo", "null"), ("ao", "null"), ("idle", "yes")]).unwrap()
    }

    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn run_ab_loop(granted: &[&str]) -> (Arc<Shared>, Mpv, Arc<Recorded>, PathBuf) {
        let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin-sdk/examples/ab-loop");
        let package = package::read_unpacked(&folder, "0.2.0").expect("build the example first with rona-plugin-pack");
        let dir = std::env::temp_dir().join(format!("rona-plugin-e2e-{}", random_id()));
        let store = Arc::new(Store::open(&dir).unwrap());
        let grants = granted.iter().map(|key| (key.to_string(), true)).collect();
        store.upsert(&package.manifest, &package.wasm_sha256(), Some(&folder), &grants, 0).unwrap();
        let recorded = Arc::new(Recorded::default());
        let mpv = mpv();
        let manager = Manager {
            store,
            dir: dir.join("plugins"),
            temp: dir.join("temp"),
            mpv: mpv.clone(),
            bridge: Arc::new(TestBridge(recorded.clone())),
            plugins: RwLock::new(Vec::new()),
            logs: Mutex::new(HashMap::new()),
        };
        let row = manager.store.list().unwrap().remove(0);
        let log = manager.log_for(&row.id);
        let shared = manager.spawn(&row, &folder, &log).unwrap();
        (shared, mpv, recorded, dir)
    }

    #[test]
    #[ignore = "needs plugin-sdk/examples/remote-control/plugin.wasm, built with rona-plugin-pack"]
    fn a_program_talks_to_a_typescript_plugin_through_the_endpoint() {
        let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin-sdk/examples/remote-control");
        let package = package::read_unpacked(&folder, "0.2.0").expect("build the example first with rona-plugin-pack");
        let dir = std::env::temp_dir().join(format!("rona-plugin-e2e-{}", random_id()));
        let store = Arc::new(Store::open(&dir).unwrap());
        let grants = package.manifest.grant_keys().into_iter().map(|key| (key, true)).collect();
        store.upsert(&package.manifest, &package.wasm_sha256(), Some(&folder), &grants, 0).unwrap();
        let built = Arc::new(Manager {
            store,
            dir: dir.join("plugins"),
            temp: dir.join("temp"),
            mpv: mpv(),
            bridge: Arc::new(TestBridge(Arc::default())),
            plugins: RwLock::new(Vec::new()),
            logs: Mutex::new(HashMap::new()),
        });
        assert!(MANAGER.set(built).is_ok(), "only this test sets the manager");
        let manager = manager().unwrap();
        manager.reload_all();
        let id = package.manifest.id.clone();

        let base = format!("rona-test-e2e-{}", std::process::id());
        assert!(ipc::tests_serve(&base));
        let stream = ipc::tests_connect(&base);
        let hello = |stream: &ipc::Stream, value: Value| {
            ipc::write_frame(stream, value.to_string().as_bytes()).unwrap();
            serde_json::from_slice::<Value>(&ipc::read_frame(stream).unwrap().unwrap()).unwrap()
        };
        let reply = hello(&stream, json!({ "rona": 1, "plugin": id, "channel": "test" }));
        assert_eq!(reply["ok"], json!(true), "{reply}");
        let answer = hello(&stream, json!({ "cmd": "state" }));
        assert_eq!(answer["state"], json!("idle"), "{answer}");
        let answer = hello(&stream, json!({ "cmd": "fly" }));
        assert!(answer["error"].as_str().is_some_and(|error| error.contains("fly")), "{answer}");

        manager.set_grant(&id, permissions::IPC_SERVE, false).unwrap();
        let again = ipc::tests_connect(&base);
        let refused = hello(&again, json!({ "rona": 1, "plugin": id }));
        assert_eq!(refused["ok"], json!(false), "{refused}");
        manager.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    fn click(shared: &Shared) {
        shared.post_event_from_user(json!({ "type": "control", "id": "loop", "button": "left" }), true);
    }

    #[test]
    #[ignore = "needs plugin-sdk/examples/ab-loop/plugin.wasm, built with rona-plugin-pack"]
    fn a_plugin_runs_its_hooks_and_reaches_mpv_through_its_permissions() {
        let (shared, mpv, recorded, dir) =
            run_ab_loop(&["playback.read", "mpv.write", "ui.controls", "ui.osd", "input.actions"]);
        click(&shared);
        wait_for("the loop start", || mpv.get_string("ab-loop-a").is_some_and(|value| value != "no"));
        wait_for("the button", || lock(&recorded.controls).iter().any(|(id, state)| id == "loop" && state["active"] == json!(true)));
        assert!(lock(&recorded.osd).iter().any(|text| text.starts_with("Loop from")));
        assert_eq!(shared.store.kv_get(&shared.id, "state").unwrap().unwrap(), b"1");
        assert!(lock(&shared.last_error).is_none());
        stop(&shared);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    #[ignore = "needs plugin-sdk/examples/ab-loop/plugin.wasm, built with rona-plugin-pack"]
    fn a_plugin_keeps_running_when_the_user_turns_a_permission_off() {
        let (shared, mpv, recorded, dir) = run_ab_loop(&["playback.read", "ui.controls", "ui.osd"]);
        click(&shared);
        wait_for("the plugin's answer", || shared.log.lines().iter().any(|line| line.text.contains("Needs playback.read and mpv.write")));
        assert!(lock(&recorded.osd).iter().any(|text| text.contains("permissions back")));
        assert_eq!(mpv.get_string("ab-loop-a").as_deref(), Some("no"));
        assert!(lock(&shared.last_error).is_none(), "the hook should still succeed: {:?}", lock(&shared.last_error));
        stop(&shared);
        let _ = std::fs::remove_dir_all(dir);
    }
}
