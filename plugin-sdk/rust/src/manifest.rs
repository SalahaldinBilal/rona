use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::permissions::{self, IPC_CONNECT, NETWORK, SYSTEM_RUN};
use crate::tree;

pub use crate::API_VERSION;

pub const MANIFEST_FILE: &str = "plugin.json";
pub const WASM_FILE: &str = "plugin.wasm";
pub const ICON_FILE: &str = "icon.png";
pub const PACKAGE_EXTENSION: &str = "rp";

pub const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
pub const MAX_WASM_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_ICON_BYTES: u64 = 256 * 1024;

pub const HOOK_ON_EVENT: &str = "on_event";
pub const HOOK_UI_RENDER: &str = "ui_render";
pub const HOOK_UI_EVENT: &str = "ui_event";
pub const HOOK_RESOLVE_LINK: &str = "resolve_link";
pub const HOOK_IPC_ACCEPT: &str = "ipc_accept";

const KNOWN_HOOKS: &[&str] = &[HOOK_ON_EVENT, HOOK_UI_RENDER, HOOK_UI_EVENT, HOOK_RESOLVE_LINK, HOOK_IPC_ACCEPT];

pub const MAX_CONTROLS: usize = 3;
pub const MAX_MENU_ENTRIES: usize = 8;
pub const MAX_ACTIONS: usize = 16;
pub const MAX_DIALOGS: usize = 8;
pub const MAX_CONTROL_TEXT: usize = 8;
pub const MAX_LABEL: usize = 40;
pub const MAX_REASON: usize = 200;
pub const MAX_PATH_DATA: usize = 2000;

pub fn check_wasm(bytes: &[u8]) -> Result<(), String> {
    if bytes.starts_with(b"\0asm") { Ok(()) } else { Err(format!("{WASM_FILE} is not a WebAssembly module")) }
}

/// A permission as `plugin.json` asks for it: a reason, or a reason with the hosts or pipes it covers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PermissionRequest {
    Reason(String),
    Detailed {
        #[serde(default)]
        reason: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        hosts: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pipes: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        programs: Vec<String>,
    },
}

impl PermissionRequest {
    pub fn reason(&self) -> &str {
        match self {
            Self::Reason(reason) | Self::Detailed { reason, .. } => reason,
        }
    }

    fn hosts(&self) -> &[String] {
        match self {
            Self::Detailed { hosts, .. } => hosts,
            Self::Reason(_) => &[],
        }
    }

    fn pipes(&self) -> &[String] {
        match self {
            Self::Detailed { pipes, .. } => pipes,
            Self::Reason(_) => &[],
        }
    }

    fn programs(&self) -> &[String] {
        match self {
            Self::Detailed { programs, .. } => programs,
            Self::Reason(_) => &[],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelDecl {
    pub title: String,
    #[serde(default)]
    pub fill: String,
    #[serde(default)]
    pub stroke: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ControlKind {
    #[default]
    Button,
    Text,
    Badge,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ControlDecl {
    pub id: String,
    pub kind: ControlKind,
    pub label: String,
    pub fill: String,
    pub stroke: String,
    pub text: String,
    pub tone: String,
    pub active: bool,
    pub dot: bool,
    pub highlight: bool,
    pub popover: bool,
    pub always: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MenuDecl {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub checked: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DialogDecl {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UiDecl {
    pub panel: Option<PanelDecl>,
    pub controls: Vec<ControlDecl>,
    pub menu: Vec<MenuDecl>,
    pub row_menu: Vec<MenuDecl>,
    pub dialogs: Vec<DialogDecl>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionDecl {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    pub api_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub min_app_version: Option<String>,
    #[serde(default)]
    pub permissions: BTreeMap<String, PermissionRequest>,
    #[serde(default)]
    pub hooks: Vec<String>,
    #[serde(default)]
    pub settings: Vec<Value>,
    #[serde(default)]
    pub ui: UiDecl,
    #[serde(default)]
    pub actions: Vec<ActionDecl>,
}

/// One switch the user sees: a permission, one network host, or one pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub key: String,
    pub category: &'static str,
    pub label: String,
    pub reason: String,
}

impl PluginManifest {
    pub fn parse(raw: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(raw).map_err(|err| format!("plugin.json is not valid: {err}"))
    }

    pub fn package_file_name(&self) -> String {
        format!("{}-{}.{PACKAGE_EXTENSION}", self.id, self.version)
    }

    pub fn has_hook(&self, hook: &str) -> bool {
        self.hooks.iter().any(|name| name == hook)
    }

    pub fn asks_for(&self, permission: &str) -> bool {
        self.permissions.contains_key(permission)
    }

    /// Every switch this plugin asks for, in the order of the permission table.
    pub fn grants(&self) -> Vec<Grant> {
        let mut grants = Vec::new();
        for permission in permissions::PERMISSIONS {
            let Some(request) = self.permissions.get(permission.name) else { continue };
            let reason = request.reason().to_owned();
            match permission.name {
                NETWORK => grants.extend(request.hosts().iter().map(|host| Grant {
                    key: permissions::network_grant(host),
                    category: permission.category,
                    label: format!("{} {host}", permission.label),
                    reason: reason.clone(),
                })),
                IPC_CONNECT => grants.extend(request.pipes().iter().map(|pipe| Grant {
                    key: permissions::pipe_grant(pipe),
                    category: permission.category,
                    label: format!("{} {pipe}", permission.label),
                    reason: reason.clone(),
                })),
                SYSTEM_RUN => grants.extend(request.programs().iter().map(|program| Grant {
                    key: permissions::program_grant(program),
                    category: permission.category,
                    label: format!("{} {program}", permission.label),
                    reason: reason.clone(),
                })),
                _ => grants.push(Grant {
                    key: permission.name.to_owned(),
                    category: permission.category,
                    label: permission.label.to_owned(),
                    reason,
                }),
            }
        }
        grants
    }

    pub fn grant_keys(&self) -> Vec<String> {
        self.grants().into_iter().map(|grant| grant.key).collect()
    }

    pub fn network_hosts(&self) -> &[String] {
        self.permissions.get(NETWORK).map_or(&[], PermissionRequest::hosts)
    }

    pub fn pipes(&self) -> &[String] {
        self.permissions.get(IPC_CONNECT).map_or(&[], PermissionRequest::pipes)
    }

    pub fn programs(&self) -> &[String] {
        self.permissions.get(SYSTEM_RUN).map_or(&[], PermissionRequest::programs)
    }

    pub fn setting_default(&self, id: &str) -> Option<Value> {
        tree::find_node(&self.settings, id).and_then(|node| node.get("value").cloned())
    }

    pub fn has_setting(&self, id: &str) -> bool {
        tree::find_node(&self.settings, id).is_some()
    }

    pub fn control(&self, id: &str) -> Option<&ControlDecl> {
        self.ui.controls.iter().find(|control| control.id == id)
    }

    pub fn dialog(&self, id: &str) -> Option<&DialogDecl> {
        self.ui.dialogs.iter().find(|dialog| dialog.id == id)
    }

    /// Every rule, plus `minAppVersion` against the app that is about to load the plugin.
    pub fn validate(&self, app_version: &str) -> Result<(), String> {
        self.validate_shape()?;
        if let Some(min) = &self.min_app_version {
            let min = semver::Version::parse(min).map_err(|_| format!("minAppVersion \"{min}\" is not a valid version"))?;
            let app = semver::Version::parse(app_version)
                .map_err(|_| format!("App version \"{app_version}\" is not a valid version"))?;
            if app < min {
                return Err(format!("Needs Rona {min} or newer"));
            }
        }
        Ok(())
    }

    /// The rules that don't depend on which app loads the plugin.
    pub fn validate_shape(&self) -> Result<(), String> {
        if self.api_version != API_VERSION {
            return Err(format!(
                "This plugin is made for plugin API {}, this version of Rona supports API {API_VERSION}",
                self.api_version
            ));
        }
        if !is_valid_id(&self.id) {
            return Err(format!(
                "Plugin id \"{}\" is not valid. Use 3 to 64 lowercase letters, digits, dots and dashes",
                self.id
            ));
        }
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return Err("The plugin name must be 1 to 64 characters".into());
        }
        semver::Version::parse(&self.version)
            .map_err(|_| format!("Plugin version \"{}\" is not a valid version", self.version))?;
        if let Some(min) = &self.min_app_version {
            semver::Version::parse(min).map_err(|_| format!("minAppVersion \"{min}\" is not a valid version"))?;
        }
        if let Some(homepage) = &self.homepage
            && !homepage.starts_with("https://")
        {
            return Err("homepage must be an https link".into());
        }

        for hook in &self.hooks {
            if !KNOWN_HOOKS.contains(&hook.as_str()) {
                return Err(format!("Unknown hook \"{hook}\""));
            }
        }
        self.validate_permissions()?;
        self.validate_ui()?;
        tree::validate_tree(&self.settings, tree::Mode::Settings).map_err(|err| format!("settings: {err}"))
    }

    fn validate_permissions(&self) -> Result<(), String> {
        for (name, request) in &self.permissions {
            if permissions::find(name).is_none() {
                return Err(format!("Unknown permission \"{name}\""));
            }
            if request.reason().chars().count() > MAX_REASON {
                return Err(format!("The reason for {name} is longer than {MAX_REASON} characters"));
            }
            let (hosts, pipes, programs) = (request.hosts(), request.pipes(), request.programs());
            let lists = [(NETWORK, hosts.len()), (IPC_CONNECT, pipes.len()), (SYSTEM_RUN, programs.len())];
            for (owner, count) in lists {
                if name == owner && count == 0 {
                    let what = match owner {
                        NETWORK => "hosts",
                        IPC_CONNECT => "pipes",
                        _ => "programs",
                    };
                    return Err(format!("{name} needs a list of {what}"));
                }
                if name != owner && count > 0 {
                    return Err(format!("Only {owner} takes that list, not {name}"));
                }
            }
            for host in hosts {
                validate_host(host)?;
            }
            for pipe in pipes {
                validate_pipe(pipe)?;
            }
            for program in programs {
                validate_program(program)?;
            }
        }
        if self.asks_for(permissions::OPEN_RESOLVE) && !self.has_hook(HOOK_RESOLVE_LINK) {
            return Err("open.resolve needs the resolve_link hook".into());
        }
        if self.has_hook(HOOK_RESOLVE_LINK) && !self.asks_for(permissions::OPEN_RESOLVE) {
            return Err("The resolve_link hook needs the open.resolve permission".into());
        }
        if self.has_hook(HOOK_IPC_ACCEPT) && !self.asks_for(permissions::IPC_SERVE) {
            return Err("The ipc_accept hook needs the ipc.serve permission".into());
        }
        let needs_events = self.asks_for(permissions::IPC_SERVE) || self.asks_for(IPC_CONNECT);
        if needs_events && !self.has_hook(HOOK_ON_EVENT) {
            return Err("A plugin that talks to other programs needs the on_event hook".into());
        }
        Ok(())
    }

    fn validate_ui(&self) -> Result<(), String> {
        let ui = &self.ui;
        let renders = self.has_hook(HOOK_UI_RENDER);
        let mut ids = HashSet::new();

        if let Some(panel) = &ui.panel {
            if !self.asks_for(permissions::UI_PANEL) {
                return Err("A panel needs the ui.panel permission".into());
            }
            if !renders {
                return Err("A panel needs the ui_render hook".into());
            }
            validate_label("The panel title", &panel.title)?;
            validate_path(&panel.fill)?;
            validate_path(&panel.stroke)?;
        } else if self.asks_for(permissions::UI_PANEL) {
            return Err("ui.panel is asked for but ui.panel is not described".into());
        }

        if !ui.controls.is_empty() && !self.asks_for(permissions::UI_CONTROLS) {
            return Err("Controls need the ui.controls permission".into());
        }
        if ui.controls.len() + usize::from(ui.panel.is_some()) > MAX_CONTROLS {
            return Err(format!("A plugin can have at most {MAX_CONTROLS} controls, counting the panel button"));
        }
        for control in &ui.controls {
            validate_item_id("Control", &control.id, &mut ids)?;
            validate_label("A control label", &control.label)?;
            validate_path(&control.fill)?;
            validate_path(&control.stroke)?;
            validate_tone(&control.tone)?;
            if control.text.chars().count() > MAX_CONTROL_TEXT {
                return Err(format!("Control text can be at most {MAX_CONTROL_TEXT} characters"));
            }
            if control.kind == ControlKind::Button && control.fill.is_empty() && control.stroke.is_empty() {
                return Err(format!("The button \"{}\" has no icon (fill or stroke)", control.id));
            }
            if control.popover && control.kind == ControlKind::Badge {
                return Err("A badge can't open a popover".into());
            }
            if control.popover && !renders {
                return Err("A control with a popover needs the ui_render hook".into());
            }
        }

        let menus = ui.menu.len() + ui.row_menu.len();
        if menus > 0 && !self.asks_for(permissions::UI_MENU) {
            return Err("Menu entries need the ui.menu permission".into());
        }
        if ui.menu.len() > MAX_MENU_ENTRIES || ui.row_menu.len() > MAX_MENU_ENTRIES {
            return Err(format!("A menu can have at most {MAX_MENU_ENTRIES} entries"));
        }
        for entry in ui.menu.iter().chain(&ui.row_menu) {
            validate_item_id("Menu entry", &entry.id, &mut ids)?;
            validate_label("A menu label", &entry.label)?;
        }

        if !self.actions.is_empty() && !self.asks_for(permissions::INPUT_ACTIONS) {
            return Err("Actions need the input.actions permission".into());
        }
        if self.actions.len() > MAX_ACTIONS {
            return Err(format!("A plugin can have at most {MAX_ACTIONS} actions"));
        }
        for action in &self.actions {
            validate_item_id("Action", &action.id, &mut ids)?;
            validate_label("An action label", &action.label)?;
        }

        if ui.dialogs.len() > MAX_DIALOGS {
            return Err(format!("A plugin can have at most {MAX_DIALOGS} dialogs"));
        }
        if !ui.dialogs.is_empty() && !renders {
            return Err("Dialogs need the ui_render hook".into());
        }
        for dialog in &ui.dialogs {
            validate_item_id("Dialog", &dialog.id, &mut ids)?;
            validate_label("A dialog title", &dialog.title)?;
        }

        let clickable = !ui.controls.is_empty() || menus > 0 || !self.actions.is_empty();
        if clickable && !self.has_hook(HOOK_ON_EVENT) {
            return Err("Controls, menu entries and actions need the on_event hook".into());
        }
        if renders != self.has_hook(HOOK_UI_EVENT) {
            return Err("ui_render and ui_event go together".into());
        }
        Ok(())
    }
}

fn is_valid_id(id: &str) -> bool {
    (3..=64).contains(&id.len())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        && !id.starts_with(['.', '-'])
        && !id.ends_with(['.', '-'])
        && !id.contains("..")
}

pub fn is_valid_item_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn validate_item_id(what: &str, id: &str, seen: &mut HashSet<String>) -> Result<(), String> {
    if !is_valid_item_id(id) {
        return Err(format!("{what} id \"{id}\" is not valid. Use letters, digits, dashes and underscores"));
    }
    if !seen.insert(id.to_owned()) {
        return Err(format!("The id \"{id}\" is used twice"));
    }
    Ok(())
}

fn validate_label(what: &str, label: &str) -> Result<(), String> {
    let length = label.trim().chars().count();
    if (1..=MAX_LABEL).contains(&length) { Ok(()) } else { Err(format!("{what} must be 1 to {MAX_LABEL} characters")) }
}

pub fn validate_tone(tone: &str) -> Result<(), String> {
    match tone {
        "" | "normal" | "muted" | "accent" | "success" | "danger" => Ok(()),
        _ => Err(format!("Unknown tone \"{tone}\". Use normal, muted, accent, success or danger")),
    }
}

/// Icons are SVG path data on an 18 px box, drawn by Rona in the theme's colors.
pub fn validate_path(data: &str) -> Result<(), String> {
    if data.chars().count() > MAX_PATH_DATA {
        return Err(format!("Icon path data can be at most {MAX_PATH_DATA} characters"));
    }
    let allowed = |c: char| "MmLlHhVvCcSsQqTtAaZz".contains(c) || c.is_ascii_digit() || " ,.-\n".contains(c);
    if data.chars().all(allowed) { Ok(()) } else { Err("Icon path data may only hold path commands and numbers".into()) }
}

fn validate_host(entry: &str) -> Result<(), String> {
    let host = entry.strip_prefix("*.").unwrap_or(entry);
    let labels_ok = !host.is_empty()
        && host.len() <= 253
        && host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        });
    let is_ip = host.parse::<std::net::Ipv4Addr>().is_ok();
    if labels_ok && !is_ip {
        Ok(())
    } else {
        Err(format!("Network host \"{entry}\" is not valid. Use a lowercase hostname like api.example.com or *.example.com"))
    }
}

/// A pipe name is the last part of `\\.\pipe\<name>` on Windows and a socket name on Linux and macOS.
pub fn validate_pipe(pipe: &str) -> Result<(), String> {
    let valid = (1..=64).contains(&pipe.len())
        && pipe.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        && !pipe.starts_with('.')
        && !pipe.to_ascii_lowercase().starts_with("rona");
    if valid {
        Ok(())
    } else {
        Err(format!("Pipe \"{pipe}\" is not valid. Use up to 64 letters, digits, dots, dashes and underscores"))
    }
}

/// A program is a bare name found on PATH, like `yt-dlp`, so a plugin can't run a file it put somewhere itself.
pub fn validate_program(program: &str) -> Result<(), String> {
    let valid = (1..=64).contains(&program.len())
        && program.chars().all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c))
        && !program.starts_with(['.', '-']);
    if valid {
        Ok(())
    } else {
        Err(format!("Program \"{program}\" is not valid. Use the name of a program on PATH, like yt-dlp, without a folder"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(extra: Value) -> PluginManifest {
        let mut base = json!({ "apiVersion": 1, "id": "com.example.test", "name": "Test", "version": "1.0.0" });
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn the_example_plugins_are_accepted() {
        for raw in [include_str!("../../examples/ab-loop/plugin.json"), include_str!("../../examples/remote-control/plugin.json")] {
            PluginManifest::parse(raw.as_bytes()).unwrap().validate("0.2.0").unwrap();
        }
    }

    #[test]
    fn ids_that_could_escape_the_plugins_folder_are_refused() {
        for id in ["../evil", "a/b", "a\\b", "..", "Com.Example", "ab", ".hidden", "a..b"] {
            assert!(manifest(json!({ "id": id })).validate("0.2.0").is_err(), "{id} should be refused");
        }
    }

    #[test]
    fn unknown_permissions_are_refused() {
        let result = manifest(json!({ "permissions": { "files.write": "Saves lyrics" } })).validate("0.2.0");
        assert_eq!(result.unwrap_err(), "Unknown permission \"files.write\"");
    }

    #[test]
    fn each_host_and_pipe_is_its_own_switch() {
        let manifest = manifest(json!({
            "hooks": ["on_event"],
            "permissions": {
                "playback.read": "Shows what plays",
                "network": { "reason": "Covers", "hosts": ["a.example.com", "b.example.com"] },
                "ipc.connect": { "reason": "Discord", "pipes": ["discord-ipc-0"] },
            },
        }));
        manifest.validate("0.2.0").unwrap();
        assert_eq!(
            manifest.grant_keys(),
            ["playback.read", "network:a.example.com", "network:b.example.com", "ipc.connect:discord-ipc-0"]
        );
    }

    #[test]
    fn network_entries_must_be_bare_hostnames() {
        for host in ["https://api.example.com", "api.example.com/x", "api.example.com:8443", "127.0.0.1", "localhost", "*"] {
            let result = manifest(json!({ "permissions": { "network": { "hosts": [host] } } })).validate("0.2.0");
            assert!(result.is_err(), "{host} should be refused");
        }
    }

    #[test]
    fn pipes_cant_reach_ronas_own_endpoint_or_other_folders() {
        for pipe in ["rona-plugins-1", "Rona", "..\\other", "a/b", ""] {
            let result = manifest(json!({
                "hooks": ["on_event"],
                "permissions": { "ipc.connect": { "pipes": [pipe] } },
            }))
            .validate("0.2.0");
            assert!(result.is_err(), "{pipe} should be refused");
        }
    }

    #[test]
    fn programs_are_bare_names_and_each_is_a_switch() {
        let ok = manifest(json!({ "permissions": { "system.run": { "reason": "Reads links", "programs": ["yt-dlp", "ffprobe"] } } }));
        ok.validate("0.2.0").unwrap();
        assert_eq!(ok.grant_keys(), ["system.run:yt-dlp", "system.run:ffprobe"]);
        for program in ["C:\\Windows\\System32\\cmd.exe", "/bin/sh", "../tool", "sub/tool", "-rf", "a b", ""] {
            let result = manifest(json!({ "permissions": { "system.run": { "programs": [program] } } })).validate("0.2.0");
            assert!(result.is_err(), "{program:?} should be refused");
        }
        assert!(manifest(json!({ "permissions": { "system.run": "No list" } })).validate("0.2.0").is_err());
        assert!(manifest(json!({ "permissions": { "playback.read": { "programs": ["yt-dlp"] } } })).validate("0.2.0").is_err());
    }

    #[test]
    fn a_resolver_needs_its_hook_and_permission() {
        assert!(manifest(json!({ "permissions": { "open.resolve": "" } })).validate("0.2.0").is_err());
        assert!(manifest(json!({ "hooks": ["resolve_link"] })).validate("0.2.0").is_err());
    }

    #[test]
    fn controls_are_limited_and_counted_with_the_panel() {
        let control = |id: &str| json!({ "id": id, "label": "Button", "stroke": "M 3 9 H 15" });
        let result = manifest(json!({
            "hooks": ["on_event", "ui_render", "ui_event"],
            "permissions": { "ui.controls": "", "ui.panel": "" },
            "ui": { "panel": { "title": "Stats", "stroke": "M 3 3 H 15" }, "controls": [control("a"), control("b"), control("c")] },
        }))
        .validate("0.2.0");
        assert!(result.is_err());
    }

    #[test]
    fn icon_paths_hold_only_path_data() {
        assert!(validate_path("M 3 9 H 15 M 3 6 V 12 Z").is_ok());
        assert!(validate_path("M 3 9\"/><script>").is_err());
        assert!(validate_path("url(#x)").is_err());
    }

    #[test]
    fn a_plugin_for_a_newer_app_is_refused() {
        let result = manifest(json!({ "minAppVersion": "9.0.0" })).validate("0.2.0");
        assert_eq!(result.unwrap_err(), "Needs Rona 9.0.0 or newer");
    }
}
