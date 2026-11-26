//! SDK for Rona plugins, plugin API 1.0.
//!
//! Inside a plugin (built for wasm): write one function per hook, list them in [`export_hooks!`],
//! and reach Rona through [`host`]. Every part of `host` that needs a permission is an `Option`,
//! `None` while the user has that permission turned off.
//!
//! On the build machine: `rona-plugin-pack` builds a plugin and writes its `.rp` file, using the
//! rules in [`manifest`] and [`tree`]. Rona checks packages with those same rules.

#[cfg(target_family = "wasm")]
pub mod host;
#[cfg(not(target_family = "wasm"))]
pub mod manifest;
#[cfg(all(feature = "pack", not(target_family = "wasm")))]
pub mod pack;
pub mod permissions;
#[cfg(not(target_family = "wasm"))]
pub mod tree;
pub mod ui;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
pub use serde_json::{Value, json};

/// The plugin API this SDK and the Rona built with it have. New host functions, events and nodes raise the
/// minor, anything that breaks existing plugins raises the major.
pub const API_VERSION: ApiVersion = ApiVersion { major: 1, minor: 0 };

/// `apiVersion` in `plugin.json`: `"1.0"`, or a bare `1` (read as `1.0`), which SDK 1.0.0 wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiVersion {
    pub major: u32,
    pub minor: u32,
}

impl ApiVersion {
    /// Whether a plugin that asks for `needed` runs on a host with this API.
    pub fn runs(self, needed: ApiVersion) -> bool {
        needed.major == self.major && needed.minor <= self.minor
    }
}

impl std::fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

impl std::str::FromStr for ApiVersion {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let invalid = || format!("apiVersion \"{text}\" is not valid. Use a major and minor version like \"1.0\"");
        let (major, minor) = text.split_once('.').ok_or_else(invalid)?;
        let number = |part: &str| {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) || (part.len() > 1 && part.starts_with('0')) {
                return Err(invalid());
            }
            part.parse::<u32>().map_err(|_| invalid())
        };
        Ok(ApiVersion { major: number(major)?, minor: number(minor)? })
    }
}

impl Serialize for ApiVersion {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ApiVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::Number(number) => number
                .as_u64()
                .and_then(|major| u32::try_from(major).ok())
                .map(|major| ApiVersion { major, minor: 0 })
                .ok_or_else(|| serde::de::Error::custom(format!("apiVersion {number} is not valid. Use a version like \"1.0\""))),
            Value::String(text) => text.parse().map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom("apiVersion must be a version like \"1.0\"")),
        }
    }
}

pub type HookResult<T> = Result<T, String>;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistEntry {
    pub index: usize,
    pub title: String,
    /// Only with `media.path`.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub current: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpcClient {
    pub connection: u32,
    pub pid: u32,
    /// The program's full path, empty when the system didn't say.
    pub program: String,
    pub channel: String,
}

/// Everything that happens, delivered to the `on_event` hook. Events that need a permission
/// only arrive while it is on.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Event {
    Started,
    Stopping,
    PermissionsChanged {
        granted: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    FileStarted {
        #[serde(default)]
        path: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    FileLoaded {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        duration: Option<f64>,
    },
    FileEnded {
        reason: String,
    },
    Idle,
    Pause {
        paused: bool,
    },
    Seek {
        position: f64,
    },
    Position {
        position: f64,
        duration: f64,
    },
    Volume {
        volume: f64,
        muted: bool,
    },
    Speed {
        speed: f64,
    },
    PlaylistChanged,
    TracksChanged,
    ChapterChanged {
        chapter: i64,
        #[serde(default)]
        title: Option<String>,
    },
    Action {
        id: String,
    },
    Menu {
        id: String,
        #[serde(default)]
        entry: Option<PlaylistEntry>,
    },
    Control {
        id: String,
        /// `left` or `right`.
        button: String,
    },
    Timer {
        id: String,
    },
    IpcConnected {
        connection: u32,
        client: IpcClient,
    },
    IpcMessage {
        connection: u32,
        message: Value,
    },
    IpcClosed {
        connection: u32,
    },
    /// Bytes arrived on a pipe opened with `host::pipes`. Read them with `Pipes::read`.
    PipeData {
        connection: u32,
    },
    PipeClosed {
        connection: u32,
    },
    /// A program started with `Programs::start` printed something. Read both of its streams.
    ProcessOutput {
        process: u32,
    },
    /// It ended. `status` is `None` when it was stopped. What it printed last can still be read.
    ProcessExited {
        process: u32,
        #[serde(default)]
        status: Option<i32>,
    },
    Property {
        name: String,
        value: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiRender {
    /// `settings`, `panel`, `popover:<control id>` or `dialog:<dialog id>`.
    pub view: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiEvent {
    pub view: String,
    pub id: String,
    /// `click`, `change` or `submit`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub value: Value,
    /// The list entry that was clicked, for list nodes.
    #[serde(default)]
    pub entry: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DialogAction {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub primary: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub danger: bool,
}

impl DialogAction {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into(), ..Self::default() }
    }

    pub fn primary(mut self) -> Self {
        self.primary = true;
        self
    }

    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree: Option<Vec<Value>>,
    /// Buttons at the bottom of a dialog, after Rona's own Close.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<DialogAction>>,
    /// An OSD message. Needs `ui.osd`, dropped without it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub osd: Option<String>,
    /// Closes the dialog or popover the event came from.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub close: bool,
    /// Opens one of the manifest's dialogs. Only works in answer to something the user did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open: Option<String>,
    /// Asks for `ui_render` again after this long while the view stays open. 500 ms at least.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_after_ms: Option<u64>,
}

impl UiResponse {
    pub fn tree(tree: Vec<Value>) -> Self {
        Self { tree: Some(tree), ..Self::default() }
    }

    pub fn none() -> Self {
        Self::default()
    }

    pub fn close() -> Self {
        Self { close: true, ..Self::default() }
    }

    pub fn open(dialog: impl Into<String>) -> Self {
        Self { open: Some(dialog.into()), ..Self::default() }
    }

    pub fn with_actions(mut self, actions: Vec<DialogAction>) -> Self {
        self.actions = Some(actions);
        self
    }

    pub fn with_osd(mut self, text: impl Into<String>) -> Self {
        self.osd = Some(text.into());
        self
    }

    pub fn refresh_after(mut self, ms: u64) -> Self {
        self.refresh_after_ms = Some(ms);
        self
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveRequest {
    pub url: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Resolved {
    /// An http(s) link mpv can play.
    pub url: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Exports hooks to Rona. Each name must also be listed under `hooks` in `plugin.json`.
///
/// ```ignore
/// export_hooks! {
///     on_event => event,
///     ui_render => render,
///     ui_event => clicked,
/// }
/// ```
#[macro_export]
macro_rules! export_hooks {
    ($($hook:ident => $handler:path),* $(,)?) => {
        $(
            #[unsafe(no_mangle)]
            pub extern "C" fn $hook() -> i32 {
                $crate::__private::$hook($handler)
            }
        )*
    };
}

#[cfg(target_family = "wasm")]
#[doc(hidden)]
pub mod __private {
    use extism_pdk::Memory;
    use serde::Serialize;
    use serde::de::DeserializeOwned;

    use super::*;

    fn fail(message: &str) -> i32 {
        if let Ok(memory) = Memory::from_bytes(message) {
            unsafe { extism_pdk::extism::error_set(memory.offset()) };
        }
        1
    }

    // Every input carries `granted`, which the host API reads to know what is there.
    fn run<I: DeserializeOwned, O: Serialize>(field: Option<&str>, handler: fn(I) -> HookResult<O>) -> i32 {
        let result = extism_pdk::input::<Vec<u8>>()
            .map_err(|err| err.to_string())
            .and_then(|input| serde_json::from_slice::<Value>(&input).map_err(|err| err.to_string()))
            .and_then(|mut input| {
                let granted = input
                    .get("granted")
                    .and_then(Value::as_array)
                    .map(|list| list.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                    .unwrap_or_default();
                crate::host::set_granted(granted);
                let payload = match field {
                    Some(field) => input.get_mut(field).map(Value::take).unwrap_or(Value::Null),
                    None => input,
                };
                serde_json::from_value::<I>(payload).map_err(|err| format!("Unexpected hook input: {err}"))
            })
            .and_then(handler)
            .and_then(|output| serde_json::to_vec(&output).map_err(|err| err.to_string()))
            .and_then(|output| Memory::from_bytes(output).map_err(|err| err.to_string()));

        match result {
            Ok(memory) => {
                memory.set_output();
                0
            }
            Err(message) => fail(&message),
        }
    }

    pub fn on_event(handler: fn(Event) -> HookResult<()>) -> i32 {
        run(Some("event"), handler)
    }

    pub fn ui_render(handler: fn(UiRender) -> HookResult<UiResponse>) -> i32 {
        run(None, handler)
    }

    pub fn ui_event(handler: fn(UiEvent) -> HookResult<UiResponse>) -> i32 {
        run(None, handler)
    }

    pub fn resolve_link(handler: fn(ResolveRequest) -> HookResult<Option<Resolved>>) -> i32 {
        run(None, handler)
    }

    pub fn ipc_accept(handler: fn(IpcClient) -> HookResult<bool>) -> i32 {
        run(Some("client"), handler)
    }
}
