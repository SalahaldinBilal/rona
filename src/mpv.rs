use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::ptr;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use libloading::Library;

use crate::logging::{debug, info};

const FORMAT_NONE: c_int = 0;
const FORMAT_STRING: c_int = 1;
const FORMAT_FLAG: c_int = 3;
const FORMAT_INT64: c_int = 4;
const FORMAT_DOUBLE: c_int = 5;
const FORMAT_NODE: c_int = 6;
const FORMAT_NODE_ARRAY: c_int = 7;
const FORMAT_NODE_MAP: c_int = 8;

const EVENT_NONE: c_int = 0;
const EVENT_SHUTDOWN: c_int = 1;
const EVENT_LOG_MESSAGE: c_int = 2;
const EVENT_COMMAND_REPLY: c_int = 5;
const EVENT_START_FILE: c_int = 6;
const EVENT_END_FILE: c_int = 7;
const EVENT_PLAYBACK_RESTART: c_int = 21;
const EVENT_PROPERTY_CHANGE: c_int = 22;
const EVENT_HOOK: c_int = 25;

const RENDER_PARAM_INVALID: c_int = 0;
const RENDER_PARAM_API_TYPE: c_int = 1;
const RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const RENDER_PARAM_OPENGL_FBO: c_int = 3;
const RENDER_PARAM_FLIP_Y: c_int = 4;
#[cfg(windows)]
const RENDER_PARAM_D3D11_INIT_PARAMS: c_int = 21;
#[cfg(windows)]
const RENDER_PARAM_D3D11_FBO: c_int = 22;

const RENDER_UPDATE_FRAME: u64 = 1 << 0;

#[cfg(target_os = "windows")]
const LIBRARY_NAMES: &[&str] = &["libmpv-2.dll", "mpv-2.dll"];
#[cfg(target_os = "macos")]
const LIBRARY_NAMES: &[&str] = &["libmpv.2.dylib", "libmpv.dylib"];
#[cfg(all(unix, not(target_os = "macos")))]
const LIBRARY_NAMES: &[&str] = &["libmpv.so.2", "libmpv.so"];

type Handle = c_void;
type RenderHandle = c_void;
type Callback = Box<dyn Fn() + Send + Sync>;
pub type StreamOpenFn = unsafe extern "C" fn(*mut c_void, *mut c_char, *mut c_void) -> c_int;
type ProcAddressLookup<'a> = &'a dyn Fn(&CStr) -> *const c_void;

#[repr(C)]
struct RawEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct RawEventEndFile {
    reason: c_int,
    error: c_int,
}

pub const END_FILE_REASON_ERROR: i32 = 4;

#[repr(C)]
struct RawEventLogMessage {
    prefix: *const c_char,
    level: *const c_char,
    text: *const c_char,
    _log_level: c_int,
}

#[repr(C)]
struct RawEventHook {
    _name: *const c_char,
    id: u64,
}

#[repr(C)]
struct RawEventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

#[repr(C)]
union RawNodeValue {
    int64: i64,
    double: f64,
    list: *mut RawNodeList,
}

#[repr(C)]
struct RawNode {
    value: RawNodeValue,
    format: c_int,
}

#[repr(C)]
struct RawNodeList {
    num: c_int,
    values: *mut RawNode,
    keys: *mut *mut c_char,
}

#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct OpenGlInitParams {
    get_proc_address: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>,
    get_proc_address_ctx: *mut c_void,
}

#[cfg(windows)]
#[repr(C)]
struct D3d11InitParams {
    device: *mut c_void,
}

#[cfg(windows)]
#[repr(C)]
struct D3d11Fbo {
    texture: *mut c_void,
}

#[repr(C)]
struct OpenGlFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub enum PropertyValue {
    Double(f64),
    Flag(bool),
    Text(String),
    Node(Node),
}

pub enum Node {
    Number(f64),
    Array(Vec<Node>),
    Map(Vec<(String, Node)>),
    Other,
}

impl Node {
    pub fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Map(entries) => entries.iter().find(|(name, _)| name == key).map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn number(&self) -> Option<f64> {
        match self {
            Node::Number(value) => Some(*value),
            _ => None,
        }
    }

    pub fn items(&self) -> &[Node] {
        match self {
            Node::Array(items) => items,
            _ => &[],
        }
    }
}

unsafe fn convert_node(node: &RawNode) -> Node {
    unsafe {
        match node.format {
            FORMAT_INT64 => Node::Number(node.value.int64 as f64),
            FORMAT_DOUBLE => Node::Number(node.value.double),
            FORMAT_NODE_ARRAY | FORMAT_NODE_MAP => {
                let list = &*node.value.list;
                let count = list.num.max(0) as usize;
                let values: &[RawNode] =
                    if count == 0 { &[] } else { std::slice::from_raw_parts(list.values, count) };
                if node.format == FORMAT_NODE_ARRAY {
                    Node::Array(values.iter().map(|value| convert_node(value)).collect())
                } else {
                    let keys = std::slice::from_raw_parts(list.keys, count);
                    Node::Map(
                        keys.iter()
                            .zip(values)
                            .map(|(key, value)| {
                                (CStr::from_ptr(*key).to_string_lossy().into_owned(), convert_node(value))
                            })
                            .collect(),
                    )
                }
            }
            _ => Node::Other,
        }
    }
}

pub enum Event {
    Shutdown,
    CommandFailed(String),
    CommandDone { id: u64, error: Option<String> },
    StartFile,
    EndFile { reason: i32, error: Option<String> },
    PlaybackRestart,
    PropertyChange { id: u64, value: Option<PropertyValue> },
    /// `kind` is the id given to `add_hook`. mpv waits until `continue_hook` is called with `id`.
    Hook { kind: u64, id: u64 },
    Log { prefix: String, level: String, text: String },
    Other,
}

struct Api {
    create: unsafe extern "C" fn() -> *mut Handle,
    initialize: unsafe extern "C" fn(*mut Handle) -> c_int,
    set_option_string: unsafe extern "C" fn(*mut Handle, *const c_char, *const c_char) -> c_int,
    command_async: unsafe extern "C" fn(*mut Handle, u64, *mut *const c_char) -> c_int,
    observe_property: unsafe extern "C" fn(*mut Handle, u64, *const c_char, c_int) -> c_int,
    get_property_string: unsafe extern "C" fn(*mut Handle, *const c_char) -> *mut c_char,
    set_property_string: unsafe extern "C" fn(*mut Handle, *const c_char, *const c_char) -> c_int,
    hook_add: unsafe extern "C" fn(*mut Handle, u64, *const c_char, c_int) -> c_int,
    hook_continue: unsafe extern "C" fn(*mut Handle, u64) -> c_int,
    request_log_messages: unsafe extern "C" fn(*mut Handle, *const c_char) -> c_int,
    free: unsafe extern "C" fn(*mut c_void),
    stream_cb_add_ro: unsafe extern "C" fn(*mut Handle, *const c_char, *mut c_void, StreamOpenFn) -> c_int,
    wait_event: unsafe extern "C" fn(*mut Handle, f64) -> *mut RawEvent,
    set_wakeup_callback:
        unsafe extern "C" fn(*mut Handle, Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void),
    terminate_destroy: unsafe extern "C" fn(*mut Handle),
    error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    render_context_create:
        unsafe extern "C" fn(*mut *mut RenderHandle, *mut Handle, *mut RenderParam) -> c_int,
    render_context_set_update_callback: unsafe extern "C" fn(
        *mut RenderHandle,
        Option<unsafe extern "C" fn(*mut c_void)>,
        *mut c_void,
    ),
    render_context_update: unsafe extern "C" fn(*mut RenderHandle) -> u64,
    render_context_render: unsafe extern "C" fn(*mut RenderHandle, *mut RenderParam) -> c_int,
    render_context_free: unsafe extern "C" fn(*mut RenderHandle),
    _library: Library,
}

impl Api {
    fn load() -> Result<Self, Error> {
        let mut failures = Vec::new();
        for name in LIBRARY_NAMES {
            match unsafe { Library::new(*name) } {
                Ok(library) => {
                    info!("loaded {name}");
                    return unsafe { Self::from_library(library) };
                }
                Err(err) => match std::error::Error::source(&err) {
                    Some(cause) => failures.push(format!("{name}: {err}: {cause}")),
                    None => failures.push(format!("{name}: {err}")),
                },
            }
        }
        Err(Error(format!("could not load libmpv ({})", failures.join(", "))))
    }

    unsafe fn from_library(library: Library) -> Result<Self, Error> {
        unsafe {
            Ok(Self {
                create: symbol(&library, b"mpv_create\0")?,
                initialize: symbol(&library, b"mpv_initialize\0")?,
                set_option_string: symbol(&library, b"mpv_set_option_string\0")?,
                command_async: symbol(&library, b"mpv_command_async\0")?,
                observe_property: symbol(&library, b"mpv_observe_property\0")?,
                get_property_string: symbol(&library, b"mpv_get_property_string\0")?,
                set_property_string: symbol(&library, b"mpv_set_property_string\0")?,
                hook_add: symbol(&library, b"mpv_hook_add\0")?,
                hook_continue: symbol(&library, b"mpv_hook_continue\0")?,
                request_log_messages: symbol(&library, b"mpv_request_log_messages\0")?,
                free: symbol(&library, b"mpv_free\0")?,
                stream_cb_add_ro: symbol(&library, b"mpv_stream_cb_add_ro\0")?,
                wait_event: symbol(&library, b"mpv_wait_event\0")?,
                set_wakeup_callback: symbol(&library, b"mpv_set_wakeup_callback\0")?,
                terminate_destroy: symbol(&library, b"mpv_terminate_destroy\0")?,
                error_string: symbol(&library, b"mpv_error_string\0")?,
                render_context_create: symbol(&library, b"mpv_render_context_create\0")?,
                render_context_set_update_callback: symbol(
                    &library,
                    b"mpv_render_context_set_update_callback\0",
                )?,
                render_context_update: symbol(&library, b"mpv_render_context_update\0")?,
                render_context_render: symbol(&library, b"mpv_render_context_render\0")?,
                render_context_free: symbol(&library, b"mpv_render_context_free\0")?,
                _library: library,
            })
        }
    }

    fn check(&self, code: c_int, what: &str) -> Result<(), Error> {
        if code >= 0 {
            return Ok(());
        }
        let message = unsafe { CStr::from_ptr((self.error_string)(code)) }.to_string_lossy();
        Err(Error(format!("{what}: {message}")))
    }
}

unsafe fn symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T, Error> {
    unsafe { library.get::<T>(name) }.map(|found| *found).map_err(|err| {
        let name = String::from_utf8_lossy(&name[..name.len() - 1]);
        Error(format!("libmpv is missing {name}: {err}"))
    })
}

fn cstring(value: &str) -> Result<CString, Error> {
    CString::new(value).map_err(|_| Error(format!("{value:?} contains a nul byte")))
}

unsafe extern "C" fn invoke_callback(data: *mut c_void) {
    let callback = unsafe { &*(data as *const Callback) };
    callback();
}

unsafe extern "C" fn lookup_proc_address(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    let lookup = unsafe { &*(ctx as *const ProcAddressLookup) };
    lookup(unsafe { CStr::from_ptr(name) }) as *mut c_void
}

// Ids for `command`, kept apart from the ids callers pass to `command_with_id`.
const UNNAMED_COMMAND_BIT: u64 = 1 << 63;

struct Inner {
    api: Api,
    handle: *mut Handle,
    wakeup: OnceLock<Callback>,
    next_command: AtomicU64,
    // What each queued command was, so a failure can say which one failed.
    pending_commands: Mutex<HashMap<u64, String>>,
}

// The mpv client API is thread safe, and the callbacks it stores are Send + Sync.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Inner {
    fn set_option(&self, name: &str, value: &str) -> Result<(), Error> {
        let (name_c, value_c) = (cstring(name)?, cstring(value)?);
        let code =
            unsafe { (self.api.set_option_string)(self.handle, name_c.as_ptr(), value_c.as_ptr()) };
        self.api.check(code, &format!("setting option {name}"))
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        unsafe { (self.api.terminate_destroy)(self.handle) };
    }
}

#[derive(Clone)]
pub struct Mpv(Arc<Inner>);

#[derive(Clone)]
pub struct WeakMpv(Weak<Inner>);

impl WeakMpv {
    pub fn upgrade(&self) -> Option<Mpv> {
        self.0.upgrade().map(Mpv)
    }
}

impl Mpv {
    pub fn new() -> Result<Self, Error> {
        Self::with_options(&[
            ("vo", "libmpv"),
            ("hwdec", "auto-safe"),
            ("keep-open", "yes"),
            ("cache", "yes"),
            // Resume positions are handled by resume.rs, through the on_load and on_unload hooks.
            ("resume-playback", "no"),
            ("save-position-on-quit", "no"),
            // Otherwise render calls block the UI thread for up to 50ms waiting for the frame's display time.
            ("video-timing-offset", "0"),
        ])
    }

    pub fn with_options(options: &[(&str, &str)]) -> Result<Self, Error> {
        let api = Api::load()?;
        let handle = unsafe { (api.create)() };
        if handle.is_null() {
            return Err(Error("mpv_create failed".into()));
        }
        let inner = Inner {
            api,
            handle,
            wakeup: OnceLock::new(),
            next_command: AtomicU64::new(1),
            pending_commands: Mutex::new(HashMap::new()),
        };
        for (name, value) in options {
            debug!("option {name} = {value}");
            inner.set_option(name, value)?;
        }
        inner.api.check(unsafe { (inner.api.initialize)(inner.handle) }, "mpv_initialize")?;
        Ok(Self(Arc::new(inner)))
    }

    pub fn downgrade(&self) -> WeakMpv {
        WeakMpv(Arc::downgrade(&self.0))
    }

    /// Queues the command without waiting for it, failures arrive later as `Event::CommandFailed`.
    pub fn command(&self, args: &[&str]) -> Result<(), Error> {
        let id = self.0.next_command.fetch_add(1, Ordering::Relaxed) | UNNAMED_COMMAND_BIT;
        self.pending_commands().insert(id, format!("{args:?}"));
        let result = self.command_with_id(id, args);
        if result.is_err() {
            self.pending_commands().remove(&id);
        }
        result
    }

    fn pending_commands(&self) -> std::sync::MutexGuard<'_, HashMap<u64, String>> {
        self.0.pending_commands.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Like `command`, but a non-zero id also reports completion as `Event::CommandDone`.
    pub fn command_with_id(&self, id: u64, args: &[&str]) -> Result<(), Error> {
        debug!("command {args:?}");
        let owned = args.iter().map(|arg| cstring(arg)).collect::<Result<Vec<_>, _>>()?;
        let mut pointers: Vec<*const c_char> =
            owned.iter().map(|arg| arg.as_ptr()).chain(std::iter::once(ptr::null())).collect();
        let code = unsafe { (self.0.api.command_async)(self.0.handle, id, pointers.as_mut_ptr()) };
        self.0.api.check(code, &format!("command {args:?}"))
    }

    pub fn add_stream_protocol(&self, protocol: &str, open: StreamOpenFn) -> Result<(), Error> {
        let protocol_c = cstring(protocol)?;
        let code = unsafe {
            (self.0.api.stream_cb_add_ro)(self.0.handle, protocol_c.as_ptr(), ptr::null_mut(), open)
        };
        self.0.api.check(code, &format!("registering protocol {protocol}"))
    }

    pub fn observe_double(&self, id: u64, name: &str) -> Result<(), Error> {
        self.observe(id, name, FORMAT_DOUBLE)
    }

    pub fn observe_flag(&self, id: u64, name: &str) -> Result<(), Error> {
        self.observe(id, name, FORMAT_FLAG)
    }

    pub fn observe_string(&self, id: u64, name: &str) -> Result<(), Error> {
        self.observe(id, name, FORMAT_STRING)
    }

    pub fn observe_node(&self, id: u64, name: &str) -> Result<(), Error> {
        self.observe(id, name, FORMAT_NODE)
    }

    pub fn observe_changes(&self, id: u64, name: &str) -> Result<(), Error> {
        self.observe(id, name, FORMAT_NONE)
    }

    pub fn get_string(&self, name: &str) -> Option<String> {
        let name_c = cstring(name).ok()?;
        let raw = unsafe { (self.0.api.get_property_string)(self.0.handle, name_c.as_ptr()) };
        if raw.is_null() {
            return None;
        }
        let value = unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned();
        unsafe { (self.0.api.free)(raw as *mut c_void) };
        Some(value)
    }

    /// Waits on mpv's core, unlike `command`. Used inside hooks, where mpv waits for us anyway.
    pub fn set_property(&self, name: &str, value: &str) -> Result<(), Error> {
        debug!("set {name} = {value}");
        let (name_c, value_c) = (cstring(name)?, cstring(value)?);
        let code = unsafe { (self.0.api.set_property_string)(self.0.handle, name_c.as_ptr(), value_c.as_ptr()) };
        self.0.api.check(code, &format!("setting {name}"))
    }

    pub fn add_hook(&self, kind: u64, name: &str) -> Result<(), Error> {
        let name_c = cstring(name)?;
        let code = unsafe { (self.0.api.hook_add)(self.0.handle, kind, name_c.as_ptr(), 0) };
        self.0.api.check(code, &format!("adding the {name} hook"))
    }

    /// mpv's messages at `level` and above arrive as `Event::Log`.
    pub fn request_log_messages(&self, level: &str) -> Result<(), Error> {
        let level_c = cstring(level)?;
        let code = unsafe { (self.0.api.request_log_messages)(self.0.handle, level_c.as_ptr()) };
        self.0.api.check(code, &format!("requesting {level} log messages"))
    }

    pub fn continue_hook(&self, id: u64) -> Result<(), Error> {
        let code = unsafe { (self.0.api.hook_continue)(self.0.handle, id) };
        self.0.api.check(code, "continuing a hook")
    }

    fn observe(&self, id: u64, name: &str, format: c_int) -> Result<(), Error> {
        let name_c = cstring(name)?;
        let code = unsafe { (self.0.api.observe_property)(self.0.handle, id, name_c.as_ptr(), format) };
        self.0.api.check(code, &format!("observing {name}"))
    }

    pub fn on_wakeup(&self, callback: impl Fn() + Send + Sync + 'static) -> Result<(), Error> {
        self.0
            .wakeup
            .set(Box::new(callback))
            .map_err(|_| Error("the wakeup callback is already set".into()))?;
        if let Some(stored) = self.0.wakeup.get() {
            let data = stored as *const Callback as *mut c_void;
            unsafe { (self.0.api.set_wakeup_callback)(self.0.handle, Some(invoke_callback), data) };
        }
        Ok(())
    }

    fn error_text(&self, code: c_int) -> String {
        unsafe { CStr::from_ptr((self.0.api.error_string)(code)) }.to_string_lossy().into_owned()
    }

    pub fn next_event(&self) -> Option<Event> {
        let event = unsafe { &*(self.0.api.wait_event)(self.0.handle, 0.0) };
        match event.event_id {
            EVENT_NONE => None,
            EVENT_SHUTDOWN => Some(Event::Shutdown),
            EVENT_START_FILE => Some(Event::StartFile),
            EVENT_END_FILE => {
                let end = unsafe { &*(event.data as *const RawEventEndFile) };
                let error = (end.error < 0).then(|| self.error_text(end.error));
                Some(Event::EndFile { reason: end.reason, error })
            }
            EVENT_PLAYBACK_RESTART => Some(Event::PlaybackRestart),
            EVENT_LOG_MESSAGE => {
                let message = unsafe { &*(event.data as *const RawEventLogMessage) };
                let text = |pointer: *const c_char| unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned();
                Some(Event::Log {
                    prefix: text(message.prefix),
                    level: text(message.level),
                    text: text(message.text).trim_end().to_string(),
                })
            }
            EVENT_HOOK => {
                let hook = unsafe { &*(event.data as *const RawEventHook) };
                Some(Event::Hook { kind: event.reply_userdata, id: hook.id })
            }
            EVENT_COMMAND_REPLY => {
                let error = (event.error < 0).then(|| self.error_text(event.error));
                let id = event.reply_userdata;
                if id & UNNAMED_COMMAND_BIT == 0 {
                    return Some(Event::CommandDone { id, error });
                }
                let command = self.pending_commands().remove(&id).unwrap_or_default();
                match error {
                    None => Some(Event::Other),
                    Some(message) => Some(Event::CommandFailed(format!("{command}: {message}"))),
                }
            }
            EVENT_PROPERTY_CHANGE => {
                let property = unsafe { &*(event.data as *const RawEventProperty) };
                let value = match property.format {
                    FORMAT_DOUBLE => {
                        Some(PropertyValue::Double(unsafe { *(property.data as *const f64) }))
                    }
                    FORMAT_FLAG => {
                        Some(PropertyValue::Flag(unsafe { *(property.data as *const c_int) } != 0))
                    }
                    FORMAT_STRING => {
                        let text = unsafe { CStr::from_ptr(*(property.data as *const *const c_char)) };
                        Some(PropertyValue::Text(text.to_string_lossy().into_owned()))
                    }
                    FORMAT_NODE => {
                        Some(PropertyValue::Node(unsafe { convert_node(&*(property.data as *const RawNode)) }))
                    }
                    _ => None,
                };
                Some(Event::PropertyChange { id: event.reply_userdata, value })
            }
            _ => Some(Event::Other),
        }
    }

    pub fn create_render_context(
        &self,
        get_proc_address: ProcAddressLookup,
        on_update: impl Fn() + Send + Sync + 'static,
    ) -> Result<RenderContext, Error> {
        let mut lookup = get_proc_address;
        let mut init = OpenGlInitParams {
            get_proc_address: Some(lookup_proc_address),
            get_proc_address_ctx: &mut lookup as *mut ProcAddressLookup as *mut c_void,
        };
        let mut params = [
            RenderParam { kind: RENDER_PARAM_API_TYPE, data: c"opengl".as_ptr() as *mut c_void },
            RenderParam {
                kind: RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: &mut init as *mut OpenGlInitParams as *mut c_void,
            },
            RenderParam { kind: RENDER_PARAM_INVALID, data: ptr::null_mut() },
        ];
        self.create_render_context_with(&mut params, on_update)
    }

    /// `device` is an `ID3D11Device*` created without the single-threaded flag, so `d3d11va` can decode on it.
    #[cfg(windows)]
    pub fn create_d3d11_render_context(
        &self,
        device: *mut c_void,
        on_update: impl Fn() + Send + Sync + 'static,
    ) -> Result<RenderContext, Error> {
        let mut init = D3d11InitParams { device };
        let mut params = [
            RenderParam { kind: RENDER_PARAM_API_TYPE, data: c"d3d11".as_ptr() as *mut c_void },
            RenderParam {
                kind: RENDER_PARAM_D3D11_INIT_PARAMS,
                data: &mut init as *mut D3d11InitParams as *mut c_void,
            },
            RenderParam { kind: RENDER_PARAM_INVALID, data: ptr::null_mut() },
        ];
        self.create_render_context_with(&mut params, on_update)
    }

    fn create_render_context_with(
        &self,
        params: &mut [RenderParam],
        on_update: impl Fn() + Send + Sync + 'static,
    ) -> Result<RenderContext, Error> {
        let mut handle = ptr::null_mut();
        let code = unsafe {
            (self.0.api.render_context_create)(&mut handle, self.0.handle, params.as_mut_ptr())
        };
        self.0.api.check(code, "mpv_render_context_create")?;

        let on_update: Box<Callback> = Box::new(Box::new(on_update));
        let data = &*on_update as *const Callback as *mut c_void;
        unsafe {
            (self.0.api.render_context_set_update_callback)(handle, Some(invoke_callback), data)
        };
        Ok(RenderContext { mpv: self.0.clone(), handle, _on_update: on_update })
    }
}

pub struct RenderContext {
    mpv: Arc<Inner>,
    handle: *mut RenderHandle,
    _on_update: Box<Callback>,
}

impl RenderContext {
    pub fn has_new_frame(&self) -> bool {
        let flags = unsafe { (self.mpv.api.render_context_update)(self.handle) };
        flags & RENDER_UPDATE_FRAME != 0
    }

    pub fn render(&self, fbo: i32, width: i32, height: i32, internal_format: i32) -> Result<(), Error> {
        let mut fbo = OpenGlFbo { fbo, w: width, h: height, internal_format };
        let mut flip_y: c_int = 1;
        let mut params = [
            RenderParam {
                kind: RENDER_PARAM_OPENGL_FBO,
                data: &mut fbo as *mut OpenGlFbo as *mut c_void,
            },
            RenderParam { kind: RENDER_PARAM_FLIP_Y, data: &mut flip_y as *mut c_int as *mut c_void },
            RenderParam { kind: RENDER_PARAM_INVALID, data: ptr::null_mut() },
        ];
        let code = unsafe { (self.mpv.api.render_context_render)(self.handle, params.as_mut_ptr()) };
        self.mpv.api.check(code, "mpv_render_context_render")
    }

    /// `texture` is an `ID3D11Texture2D*` on the device the render context was created with.
    #[cfg(windows)]
    pub fn render_d3d11(&self, texture: *mut c_void) -> Result<(), Error> {
        let mut fbo = D3d11Fbo { texture };
        let mut params = [
            RenderParam { kind: RENDER_PARAM_D3D11_FBO, data: &mut fbo as *mut D3d11Fbo as *mut c_void },
            RenderParam { kind: RENDER_PARAM_INVALID, data: ptr::null_mut() },
        ];
        let code = unsafe { (self.mpv.api.render_context_render)(self.handle, params.as_mut_ptr()) };
        self.mpv.api.check(code, "mpv_render_context_render")
    }
}

impl Drop for RenderContext {
    fn drop(&mut self) {
        unsafe {
            (self.mpv.api.render_context_set_update_callback)(self.handle, None, ptr::null_mut());
            (self.mpv.api.render_context_free)(self.handle);
        }
    }
}
