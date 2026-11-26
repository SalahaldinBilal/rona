//! The UI thread's side: Settings > Plugins, the install dialog, plugin views and the control bar. Plugin
//! threads reach this through `AppBridge`, which only ever posts to the UI thread.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use slint::{ComponentHandle, Model, ModelRc, SharedPixelBuffer, SharedString, Timer, TimerMode, VecModel};

use crate::logging::{info, warn};
use crate::{
    AppWindow, Osd, Player, PluginAction, PluginControl, PluginControlPref, PluginGrant, PluginItem, PluginListItem,
    PluginMenuEntry, PluginRow, PluginsUi,
};

use super::manifest::{ControlKind, HOOK_UI_EVENT, HOOK_UI_RENDER};
use super::permissions::*;
use super::runtime::{Image, Shared};
use super::store::ControlPref;
use super::view::{self, Row};
use super::{BUDGET_UI, Bridge, Inspection, Loaded, Manager, Requester, host, manager, tree};

const SLOT_SETTINGS: usize = 0;
const SLOT_PANEL: usize = 1;
const SLOT_POPOVER: usize = 2;
const SLOT_DIALOG: usize = 3;
const SLOT_STATIC: usize = 4;
const SLOTS: usize = 5;
const MIN_REFRESH_MS: u64 = 500;
const LIVE_DELAY: Duration = Duration::from_millis(300);
const LOG_POLL: Duration = Duration::from_millis(500);
const LOG_SHOWN: usize = 80;
const DECLINE_MEMORY: Duration = Duration::from_secs(600);
const PANEL_CONTROL: &str = "panel";

type Answer = Sender<Result<&'static str, String>>;

struct ViewState {
    plugin: String,
    view: String,
    tree: Vec<Value>,
    rows: Vec<Row>,
    actions: Vec<(String, PluginAction)>,
    refresh: Timer,
}

struct Install {
    inspection: Option<Inspection>,
    grants: Vec<(String, PluginGrant)>,
    requester: Option<Requester>,
    answer: Option<Answer>,
}

struct Ui {
    app: slint::Weak<AppWindow>,
    osd: Osd,
    views: Vec<Option<ViewState>>,
    models: Vec<Rc<VecModel<PluginRow>>>,
    item_models: Vec<Vec<Rc<VecModel<PluginItem>>>>,
    images: HashMap<usize, slint::Image>,
    selected: Option<String>,
    detail_grants: Vec<String>,
    controls: Vec<(String, String)>,
    control_model: Rc<VecModel<PluginControl>>,
    text_widths: HashMap<SharedString, f32>,
    menu: Vec<(String, String)>,
    row_menu: Vec<(String, String)>,
    control_state: HashMap<(String, String), Value>,
    menu_state: HashMap<(String, String), Value>,
    prefs: Vec<ControlPref>,
    install: Option<Install>,
    queue: VecDeque<(PathBuf, Option<Requester>, Option<Answer>)>,
    declined: Vec<(String, String, Instant)>,
    live: Timer,
    log_timer: Timer,
    log_generation: u64,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn with_ui<T>(action: impl FnOnce(&mut Ui) -> T) -> Option<T> {
    UI.with(|slot| slot.borrow_mut().as_mut().map(action))
}

fn on_ui(action: impl FnOnce(&mut Ui) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        with_ui(action);
    });
}

pub struct AppBridge;

impl Bridge for AppBridge {
    fn status_changed(&self) {
        on_ui(|ui| ui.refresh_all());
    }

    fn osd(&self, text: String) {
        on_ui(move |ui| ui.osd.show(text));
    }

    fn view_tree(&self, plugin: &str, view: &str, tree: Vec<Value>) {
        let (plugin, view) = (plugin.to_owned(), view.to_owned());
        on_ui(move |ui| {
            if let Some(slot) = ui.slot_showing(&plugin, &view) {
                ui.set_tree(slot, tree);
            }
        });
    }

    fn view_patch(&self, plugin: &str, view: &str, patches: Vec<Value>) {
        let (plugin, view) = (plugin.to_owned(), view.to_owned());
        on_ui(move |ui| {
            let Some(slot) = ui.slot_showing(&plugin, &view) else { return };
            let Some(state) = ui.views[slot].as_mut() else { return };
            let mut tree = state.tree.clone();
            if !view::patch(&mut tree, &patches) {
                log(&plugin, "warn", "ui.patch named a node that isn't in the view");
            }
            match tree::validate_tree(&tree, tree::Mode::View) {
                Ok(()) => ui.set_tree(slot, tree),
                Err(err) => log(&plugin, "error", &format!("The patched view is not valid: {err}")),
            }
        });
    }

    fn images_changed(&self, plugin: &str) {
        let plugin = plugin.to_owned();
        on_ui(move |ui| {
            for slot in 0..SLOTS {
                if let Some(state) = ui.views[slot].as_ref().filter(|state| state.plugin == plugin) {
                    let tree = state.tree.clone();
                    ui.set_tree(slot, tree);
                }
            }
        });
    }

    fn open_dialog(&self, plugin: &str, dialog: &str) {
        let (plugin, dialog) = (plugin.to_owned(), dialog.to_owned());
        on_ui(move |ui| ui.open_dialog(&plugin, &dialog));
    }

    fn control_set(&self, plugin: &str, control: &str, state: Value) {
        let key = (plugin.to_owned(), control.to_owned());
        on_ui(move |ui| {
            let merged = ui.control_state.entry(key).or_insert_with(|| json!({}));
            if let (Some(merged), Some(state)) = (merged.as_object_mut(), state.as_object()) {
                merged.extend(state.clone());
            }
            ui.show_controls();
        });
    }

    fn menu_set(&self, plugin: &str, entry: &str, state: Value) {
        let key = (plugin.to_owned(), entry.to_owned());
        on_ui(move |ui| {
            let merged = ui.menu_state.entry(key).or_insert_with(|| json!({}));
            if let (Some(merged), Some(state)) = (merged.as_object_mut(), state.as_object()) {
                merged.extend(state.clone());
            }
            ui.show_controls();
        });
    }

    fn open_media(&self, sources: Vec<String>, append: bool) {
        let _ = slint::invoke_from_event_loop(move || {
            if append {
                crate::opener::add_arguments(&sources);
            } else {
                crate::opener::open_arguments(&sources);
            }
        });
    }

    fn player_control(&self, action: Value) {
        on_ui(move |ui| {
            let Some(app) = ui.app.upgrade() else { return };
            let player = app.global::<Player>();
            match action.get("action").and_then(Value::as_str).unwrap_or_default() {
                "volume" => {
                    if let Some(volume) = action.get("value").and_then(Value::as_f64).filter(|volume| volume.is_finite()) {
                        player.invoke_set_volume((volume.max(0.0) as f32).min(player.get_volume_limit()));
                    }
                }
                "mute" => {
                    if action.get("value").and_then(Value::as_bool) != Some(player.get_muted()) {
                        player.invoke_toggle_mute();
                    }
                }
                "next" => player.invoke_next(),
                "previous" => player.invoke_previous(),
                "stop" => player.invoke_close_file(),
                _ => {}
            }
        });
    }

    fn playlist_edit(&self, op: Value) {
        on_ui(move |ui| {
            let Some(app) = ui.app.upgrade() else { return };
            let player = app.global::<Player>();
            let number = |key: &str| op.get(key).and_then(Value::as_u64).map(|number| number as i32);
            let Some(index) = number("index") else { return };
            match op.get("op").and_then(Value::as_str) {
                Some("remove") => player.invoke_remove_entry(index),
                Some("play") => player.invoke_play_entry(index),
                Some("move") => {
                    if let Some(before) = number("before") {
                        player.invoke_move_entry(index, before);
                    }
                }
                _ => {}
            }
        });
    }
}

fn log(plugin: &str, level: &str, text: &str) {
    if let Some(found) = manager().and_then(|manager| manager.find(plugin)) {
        found.log.push(level, text);
    }
}

fn running(plugin: &str) -> Option<Arc<Shared>> {
    manager()?.find(plugin)?.running().cloned()
}

fn tone(name: &str) -> i32 {
    view::tone(Some(name))
}

/// Calls a view hook on a background thread and applies the answer on the UI thread.
fn call_view(slot: usize, plugin: String, view: String, hook: &'static str, input: Value, user_action: bool) {
    let Some(shared) = running(&plugin) else { return };
    std::thread::spawn(move || {
        let result = shared.call_and_wait(hook, input, BUDGET_UI, user_action);
        let response = result.and_then(|output| {
            if output.is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_slice::<Value>(&output).map_err(|err| format!("The plugin returned invalid JSON: {err}"))
        });
        on_ui(move |ui| match response {
            Ok(response) => ui.apply_response(slot, &plugin, &view, response, user_action),
            Err(err) => {
                if ui.slot_showing(&plugin, &view) == Some(slot) && ui.views[slot].as_ref().is_some_and(|state| state.tree.is_empty()) {
                    ui.set_tree(slot, vec![json!({ "type": "label", "tone": "danger", "text": format!("The plugin couldn't show this: {err}") })]);
                }
            }
        });
    });
}

fn render(slot: usize, plugin: String, view: String) {
    call_view(slot, plugin, view.clone(), HOOK_UI_RENDER, json!({ "view": view }), false);
}

fn to_slint_image(images: &mut HashMap<usize, slint::Image>, image: &Arc<Image>) -> slint::Image {
    let key = Arc::as_ptr(image) as usize;
    images
        .entry(key)
        .or_insert_with(|| {
            let buffer = SharedPixelBuffer::clone_from_slice(&image.rgba, image.width, image.height);
            slint::Image::from_rgba8(buffer)
        })
        .clone()
}

fn item_model(item: &view::Item, images: &mut HashMap<usize, slint::Image>) -> PluginItem {
    let picture = item.image.as_ref().map(|image| to_slint_image(images, image));
    PluginItem {
        kind: item.kind.as_str().into(),
        text: item.text.as_str().into(),
        tone: item.tone,
        value: item.value.as_str().into(),
        checked: item.checked,
        number: item.number,
        minimum: item.minimum,
        maximum: item.maximum,
        step: item.step,
        options: ModelRc::new(VecModel::from(item.options.iter().map(|option| SharedString::from(option.as_str())).collect::<Vec<_>>())),
        index: item.index,
        enabled: item.enabled,
        primary: item.primary,
        danger: item.danger,
        ghost: item.ghost,
        highlight: item.highlight,
        secret: item.secret,
        numeric: item.numeric,
        placeholder: item.placeholder.as_str().into(),
        has_picture: picture.is_some(),
        picture: picture.unwrap_or_default(),
        box_width: item.width,
        box_height: item.height,
        fraction: item.fraction,
        indeterminate: item.indeterminate,
        color: crate::color::parse(&item.value).unwrap_or_default(),
        stretch: matches!(item.kind.as_str(), "text" | "select" | "slider" | "color" | "progress" | "label" | "segmented"),
    }
}

impl Ui {
    fn ui(&self) -> Option<AppWindow> {
        self.app.upgrade()
    }

    fn slot_open(&self, slot: usize) -> bool {
        let Some(app) = self.ui() else { return false };
        let plugins = app.global::<PluginsUi>();
        match slot {
            SLOT_PANEL => plugins.get_panel_open(),
            SLOT_POPOVER => plugins.get_popover_open(),
            SLOT_DIALOG => plugins.get_dialog_open(),
            _ => true,
        }
    }

    fn slot_showing(&mut self, plugin: &str, view: &str) -> Option<usize> {
        let slot = (0..SLOTS).find(|slot| {
            self.views[*slot].as_ref().is_some_and(|state| state.plugin == plugin && state.view == view)
        })?;
        if !self.slot_open(slot) {
            self.views[slot] = None;
            return None;
        }
        Some(slot)
    }

    fn open_view(&mut self, slot: usize, plugin: &str, view: &str) {
        self.views[slot] = Some(ViewState {
            plugin: plugin.to_owned(),
            view: view.to_owned(),
            tree: Vec::new(),
            rows: Vec::new(),
            actions: Vec::new(),
            refresh: Timer::default(),
        });
        self.show_rows(slot, Vec::new());
        render(slot, plugin.to_owned(), view.to_owned());
    }

    fn close_view(&mut self, slot: usize) {
        self.views[slot] = None;
        self.show_rows(slot, Vec::new());
    }

    fn set_tree(&mut self, slot: usize, tree: Vec<Value>) {
        let Some(state) = self.views[slot].as_mut() else { return };
        let rows = match running(&state.plugin) {
            Some(shared) => host::render_rows(&shared, &tree),
            None => view::rows(&tree, &|_| None),
        };
        state.tree = tree;
        state.rows = rows.clone();
        self.show_rows(slot, rows);
    }

    // Rows that keep their kind and item kinds are updated in place, so a field being typed in keeps its element.
    fn show_rows(&mut self, slot: usize, rows: Vec<Row>) {
        let model = self.models[slot].clone();
        let same_shape = model.row_count() == rows.len()
            && self.item_models[slot].len() == rows.len()
            && rows.iter().enumerate().all(|(index, row)| {
                let old = model.row_data(index).unwrap_or_default();
                let items = &self.item_models[slot][index];
                old.kind == row.kind
                    && items.row_count() == row.items.len()
                    && row.items.iter().enumerate().all(|(at, item)| items.row_data(at).is_some_and(|old| old.kind == item.kind.as_str()))
            });
        if !same_shape {
            self.item_models[slot] = rows.iter().map(|_| Rc::new(VecModel::default())).collect();
        }
        for (index, row) in rows.iter().enumerate() {
            let items = self.item_models[slot][index].clone();
            let new_items: Vec<PluginItem> = row.items.iter().map(|item| item_model(item, &mut self.images)).collect();
            if same_shape {
                for (at, item) in new_items.into_iter().enumerate() {
                    if items.row_data(at).as_ref() != Some(&item) {
                        items.set_row_data(at, item);
                    }
                }
            } else {
                items.set_vec(new_items);
            }
            let picture = row.image.as_ref().map(|image| to_slint_image(&mut self.images, image));
            let built = PluginRow {
                kind: row.kind.into(),
                label: row.label.as_str().into(),
                text: row.text.as_str().into(),
                tone: row.tone,
                items: ModelRc::from(items),
                stretches: row.items.iter().any(|item| {
                    matches!(item.kind.as_str(), "text" | "select" | "slider" | "color" | "progress" | "label" | "segmented")
                }),
                has_picture: picture.is_some(),
                picture: picture.unwrap_or_default(),
            };
            if same_shape {
                if model.row_data(index).as_ref() != Some(&built) {
                    model.set_row_data(index, built);
                }
            } else if index < model.row_count() {
                model.set_row_data(index, built);
            } else {
                model.push(built);
            }
        }
        while model.row_count() > rows.len() {
            model.remove(model.row_count() - 1);
        }
        // Drop decoded pictures no row uses any more.
        if self.images.len() > 64 {
            self.images.clear();
        }
    }

    fn apply_response(&mut self, slot: usize, plugin: &str, view: &str, response: Value, user_action: bool) {
        if self.slot_showing(plugin, view) != Some(slot) {
            return;
        }
        if let Some(tree) = response.get("tree") {
            match tree.as_array().map(|tree| (tree, tree::validate_tree(tree, tree::Mode::View))) {
                Some((tree, Ok(()))) => self.set_tree(slot, tree.clone()),
                Some((_, Err(err))) => log(plugin, "error", &format!("The view was dropped, it's not valid: {err}")),
                None if tree.is_null() => {}
                None => log(plugin, "error", "tree must be a list of nodes"),
            }
        }
        if let Some(actions) = response.get("actions").and_then(Value::as_array) {
            let actions: Vec<(String, PluginAction)> = actions
                .iter()
                .take(3)
                .filter_map(|action| {
                    let id = action.get("id")?.as_str()?.to_owned();
                    let label: String = action.get("label")?.as_str()?.chars().take(40).collect();
                    let flag = |key: &str| action.get(key).and_then(Value::as_bool).unwrap_or(false);
                    Some((id, PluginAction { label: label.into(), primary: flag("primary"), danger: flag("danger") }))
                })
                .collect();
            if let Some(state) = self.views[slot].as_mut() {
                state.actions = actions;
            }
            if slot == SLOT_DIALOG {
                self.show_dialog_actions();
            }
        }
        if let Some(text) = response.get("osd").and_then(Value::as_str)
            && let Some(shared) = running(plugin)
            && shared.granted(UI_OSD)
        {
            host::show_osd(&shared, text);
        }
        if response.get("close").and_then(Value::as_bool) == Some(true) {
            self.close_slot(slot);
        }
        if let Some(dialog) = response.get("open").and_then(Value::as_str) {
            if user_action {
                self.open_dialog(plugin, dialog);
            } else {
                log(plugin, "warn", "A dialog can only open in answer to something the user did");
            }
        }
        if let Some(ms) = response.get("refreshAfterMs").and_then(Value::as_u64)
            && let Some(state) = self.views[slot].as_ref()
        {
            let (plugin, view) = (plugin.to_owned(), view.to_owned());
            state.refresh.start(TimerMode::SingleShot, Duration::from_millis(ms.max(MIN_REFRESH_MS)), move || {
                let still = with_ui(|ui| ui.slot_showing(&plugin, &view) == Some(slot)).unwrap_or(false);
                if still {
                    render(slot, plugin.clone(), view.clone());
                }
            });
        }
    }

    fn close_slot(&mut self, slot: usize) {
        let Some(app) = self.ui() else { return };
        let plugins = app.global::<PluginsUi>();
        match slot {
            SLOT_POPOVER => plugins.set_popover_open(false),
            SLOT_DIALOG => plugins.set_dialog_open(false),
            SLOT_PANEL => plugins.set_panel_open(false),
            _ => {}
        }
        self.close_view(slot);
    }

    fn show_dialog_actions(&self) {
        let Some(app) = self.ui() else { return };
        let actions: Vec<PluginAction> =
            self.views[SLOT_DIALOG].as_ref().map(|state| state.actions.iter().map(|(_, action)| action.clone()).collect()).unwrap_or_default();
        app.global::<PluginsUi>().set_dialog_actions(ModelRc::new(VecModel::from(actions)));
    }

    fn open_dialog(&mut self, plugin: &str, dialog: &str) {
        let Some(found) = manager().and_then(|manager| manager.find(plugin)) else { return };
        let Some(declared) = found.manifest().dialog(dialog) else { return };
        let Some(app) = self.ui() else { return };
        let plugins = app.global::<PluginsUi>();
        plugins.set_dialog_title(declared.title.as_str().into());
        plugins.set_popover_open(false);
        plugins.set_dialog_open(true);
        self.open_view(SLOT_DIALOG, plugin, &format!("dialog:{dialog}"));
        self.show_dialog_actions();
    }

    fn view_event(&mut self, slot: usize, row: usize, item: Option<usize>, kind: &str, reported: &str) {
        let Some(state) = self.views[slot].as_ref() else { return };
        let Some(found_row) = state.rows.get(row).cloned() else { return };
        let (plugin, view_name) = (state.plugin.clone(), state.view.clone());
        let found_item = item.and_then(|index| found_row.items.get(index)).cloned();

        if let Some(link) = found_item.as_ref().filter(|item| item.kind == "link") {
            open_link(&link.url);
            return;
        }
        let (id, entry) = match (&found_item, found_row.kind) {
            (Some(item), "entry") => (item.id.clone(), Some(found_row.entry.clone())),
            (None, "entry") => (found_row.id.clone(), Some(found_row.entry.clone())),
            (Some(item), _) => (item.id.clone(), None),
            (None, _) => return,
        };
        let value = match &found_item {
            Some(item) => view::event_value(item, reported),
            None => Value::Null,
        };
        if id.is_empty() {
            return;
        }

        let kind = match kind {
            // Text and colors change on every key or drag step, so only live fields and colors send them,
            // and only once the user pauses.
            "input" if found_item.as_ref().is_some_and(|item| item.live || item.kind == "color") => {
                let (plugin, view_name, id, value) = (plugin.clone(), view_name.clone(), id.clone(), value.clone());
                self.live.start(TimerMode::SingleShot, LIVE_DELAY, move || {
                    with_ui(|ui| ui.send_event(slot, &plugin, &view_name, &id, "change", value.clone(), None));
                });
                return;
            }
            "input" => return,
            // A live field already sent its value while typing.
            "change" if found_item.as_ref().is_some_and(|item| item.live) => return,
            other => other,
        };
        self.send_event(slot, &plugin, &view_name, &id, kind, value, entry);
    }

    #[allow(clippy::too_many_arguments)]
    fn send_event(&mut self, slot: usize, plugin: &str, view_name: &str, id: &str, kind: &str, value: Value, entry: Option<String>) {
        if slot == SLOT_STATIC {
            if kind == "change"
                && let Some(manager) = manager()
            {
                if let Err(err) = manager.set_setting(plugin, id, &value) {
                    warn!("could not save a plugin setting: {err}");
                }
                self.show_static_settings();
            }
            return;
        }
        let input = json!({ "view": view_name, "id": id, "type": kind, "value": value, "entry": entry });
        call_view(slot, plugin.to_owned(), view_name.to_owned(), HOOK_UI_EVENT, input, true);
    }

    fn refresh_all(&mut self) {
        self.show_list();
        self.show_detail();
        self.show_controls();
        crate::settings::refresh_shortcuts();
    }

    fn show_list(&self) {
        let Some(app) = self.ui() else { return };
        let Some(manager) = manager() else { return };
        let items: Vec<PluginListItem> = manager
            .all()
            .iter()
            .map(|plugin| {
                let (status, status_tone) = plugin.status();
                let icon = manager.icon(plugin).and_then(|bytes| decode_icon(&bytes));
                PluginListItem {
                    name: plugin.manifest().name.as_str().into(),
                    version: plugin.manifest().version.as_str().into(),
                    status: status.into(),
                    status_tone,
                    enabled: plugin.row.enabled,
                    can_enable: plugin.problem.is_none() || plugin.row.enabled,
                    dev: plugin.dev(),
                    has_icon: icon.is_some(),
                    icon: icon.unwrap_or_default(),
                }
            })
            .collect();
        app.global::<PluginsUi>().set_plugins(ModelRc::new(VecModel::from(items)));
    }

    fn selected(&self) -> Option<Arc<Loaded>> {
        manager()?.find(self.selected.as_ref()?)
    }

    fn show_detail(&mut self) {
        let Some(app) = self.ui() else { return };
        let plugins = app.global::<PluginsUi>();
        let Some(plugin) = self.selected() else {
            if self.selected.take().is_some() {
                plugins.set_selected(-1);
                self.close_view(SLOT_SETTINGS);
                self.close_view(SLOT_STATIC);
            }
            return;
        };
        let manager = manager().expect("a selected plugin means the manager runs");
        let index = manager.all().iter().position(|other| other.row.id == plugin.row.id).unwrap_or(0);
        plugins.set_selected(index as i32);
        let manifest = plugin.manifest();
        let (status, status_tone) = plugin.status();
        let error = plugin.shared.as_ref().and_then(|shared| super::runtime::lock(&shared.last_error).clone());
        plugins.set_d_name(manifest.name.as_str().into());
        plugins.set_d_version(manifest.version.as_str().into());
        plugins.set_d_author(manifest.author.clone().unwrap_or_default().into());
        plugins.set_d_description(manifest.description.clone().unwrap_or_default().into());
        plugins.set_d_homepage(manifest.homepage.clone().unwrap_or_default().into());
        plugins.set_d_status(status.into());
        plugins.set_d_status_tone(status_tone);
        plugins.set_d_problem(plugin.problem.clone().or(error).unwrap_or_default().into());
        plugins.set_d_enabled(plugin.row.enabled);
        plugins.set_d_can_enable(plugin.problem.is_none() || plugin.row.enabled);
        plugins.set_d_dev(plugin.dev());
        let icon = manager.icon(&plugin).and_then(|bytes| decode_icon(&bytes));
        plugins.set_d_has_icon(icon.is_some());
        plugins.set_d_icon(icon.unwrap_or_default());

        let stored = manager.grants(&plugin.row.id);
        let (keys, grants) = grant_models(&manifest.grants(), |key| stored.get(key).copied().unwrap_or(false), |_| false);
        self.detail_grants = keys;
        plugins.set_d_grants(ModelRc::new(VecModel::from(grants)));
        self.show_log(true);

        let has_view = manifest.has_hook(HOOK_UI_RENDER) && plugin.running().is_some();
        plugins.set_d_has_view(has_view);
        let showing = self.views[SLOT_SETTINGS].as_ref().is_some_and(|state| state.plugin == plugin.row.id);
        if has_view && !showing {
            self.open_view(SLOT_SETTINGS, &plugin.row.id, "settings");
        } else if !has_view {
            self.close_view(SLOT_SETTINGS);
        }
        self.show_static_settings();
    }

    fn show_static_settings(&mut self) {
        let Some(plugin) = self.selected() else {
            self.close_view(SLOT_STATIC);
            return;
        };
        let manager = manager().expect("a selected plugin means the manager runs");
        let manifest = plugin.manifest().clone();
        let id = plugin.row.id.clone();
        let tree = view::with_values(&manifest.settings, &|setting| {
            Some(super::setting_value(manager.store(), &id, &manifest, setting)).filter(|value| !value.is_null())
        });
        if self.views[SLOT_STATIC].as_ref().is_none_or(|state| state.plugin != id) {
            self.views[SLOT_STATIC] = Some(ViewState {
                plugin: id,
                view: "manifest-settings".into(),
                tree: Vec::new(),
                rows: Vec::new(),
                actions: Vec::new(),
                refresh: Timer::default(),
            });
        }
        self.set_tree(SLOT_STATIC, tree);
    }

    fn show_log(&mut self, force: bool) {
        let Some(app) = self.ui() else { return };
        let Some(plugin) = self.selected() else { return };
        let generation = plugin.log.generation.load(std::sync::atomic::Ordering::Relaxed);
        if !force && generation == self.log_generation {
            return;
        }
        self.log_generation = generation;
        let lines = plugin.log.lines();
        let text: Vec<String> = lines
            .iter()
            .skip(lines.len().saturating_sub(LOG_SHOWN))
            .map(|line| format!("{} {:5} {}", line.time, line.level, line.text))
            .collect();
        app.global::<PluginsUi>().set_d_log(text.join("\n").into());
    }

    fn select(&mut self, index: i32) {
        self.close_view(SLOT_SETTINGS);
        self.close_view(SLOT_STATIC);
        self.selected = usize::try_from(index)
            .ok()
            .and_then(|index| manager().and_then(|manager| manager.all().get(index).map(|plugin| plugin.row.id.clone())));
        if self.selected.is_none()
            && let Some(app) = self.ui()
        {
            app.global::<PluginsUi>().set_selected(-1);
        }
        self.show_detail();
    }

    fn show_controls(&mut self) {
        let Some(app) = self.ui() else { return };
        let plugins_ui = app.global::<PluginsUi>();
        let Some(manager) = manager() else { return };
        self.prefs = manager.store().control_prefs().unwrap_or_default();

        let mut buttons: Vec<(i64, String, String, PluginControl)> = Vec::new();
        let mut badges = Vec::new();
        let mut prefs = Vec::new();
        let mut menu = Vec::new();
        let mut row_menu = Vec::new();
        let (mut menu_keys, mut row_keys) = (Vec::new(), Vec::new());
        for plugin in manager.all() {
            let Some(shared) = plugin.running() else { continue };
            let manifest = &shared.manifest;
            if shared.granted(UI_CONTROLS) || shared.granted(UI_PANEL) {
                let mut declared: Vec<(String, PluginControl)> = Vec::new();
                if let Some(panel) = manifest.ui.panel.as_ref().filter(|_| shared.granted(UI_PANEL)) {
                    declared.push((PANEL_CONTROL.into(), PluginControl {
                        kind: 0,
                        label: panel.title.as_str().into(),
                        fill: panel.fill.as_str().into(),
                        stroke: panel.stroke.as_str().into(),
                        enabled: true,
                        always: true,
                        panel_of: plugin.row.id.as_str().into(),
                        ..PluginControl::default()
                    }));
                }
                if shared.granted(UI_CONTROLS) {
                    for control in &manifest.ui.controls {
                        let state = self.control_state.get(&(plugin.row.id.clone(), control.id.clone())).cloned().unwrap_or(json!({}));
                        let text = |key: &str, default: &str| state.get(key).and_then(Value::as_str).unwrap_or(default).to_owned();
                        let flag = |key: &str, default: bool| state.get(key).and_then(Value::as_bool).unwrap_or(default);
                        if !flag("visible", true) {
                            continue;
                        }
                        let built = PluginControl {
                            kind: match control.kind {
                                ControlKind::Button => 0,
                                ControlKind::Text => 1,
                                ControlKind::Badge => 2,
                            },
                            label: control.label.as_str().into(),
                            fill: text("fill", &control.fill).into(),
                            stroke: text("stroke", &control.stroke).into(),
                            text: text("text", &control.text).into(),
                            tone: tone(&text("tone", &control.tone)),
                            active: flag("active", control.active),
                            dot: flag("dot", control.dot),
                            highlight: flag("highlight", control.highlight),
                            enabled: flag("enabled", true),
                            always: control.always,
                            panel_of: Default::default(),
                            end: 0.0,
                        };
                        if control.kind == ControlKind::Badge {
                            badges.push(built);
                        } else {
                            declared.push((control.id.clone(), built));
                        }
                    }
                }
                for (id, control) in declared {
                    let pref = self.prefs.iter().find(|pref| pref.plugin_id == plugin.row.id && pref.control_id == id);
                    let position = pref.map_or(i64::MAX, |pref| pref.position);
                    let shown = pref.is_none_or(|pref| !pref.hidden);
                    prefs.push((position, plugin.row.id.clone(), id.clone(), format!("{}: {}", manifest.name, control.label), shown));
                    if shown {
                        buttons.push((position, plugin.row.id.clone(), id, control));
                    }
                }
            }
            if shared.granted(UI_MENU) {
                let entry = |entry: &super::manifest::MenuDecl| {
                    let state = self.menu_state.get(&(plugin.row.id.clone(), entry.id.clone())).cloned().unwrap_or(json!({}));
                    if state.get("visible").and_then(Value::as_bool) == Some(false) {
                        return None;
                    }
                    let checked = state.get("checked").and_then(Value::as_bool).or(entry.checked);
                    Some(PluginMenuEntry {
                        label: state.get("label").and_then(Value::as_str).unwrap_or(&entry.label).into(),
                        plugin: manifest.name.as_str().into(),
                        has_check: checked.is_some(),
                        checked: checked.unwrap_or(false),
                    })
                };
                for declared in &manifest.ui.menu {
                    if let Some(built) = entry(declared) {
                        menu.push(built);
                        menu_keys.push((plugin.row.id.clone(), declared.id.clone()));
                    }
                }
                for declared in &manifest.ui.row_menu {
                    if let Some(built) = entry(declared) {
                        row_menu.push(built);
                        row_keys.push((plugin.row.id.clone(), declared.id.clone()));
                    }
                }
            }
        }
        buttons.sort_by_key(|(position, ..)| *position);
        prefs.sort_by_key(|(position, ..)| *position);
        self.controls = buttons.iter().map(|(_, plugin, id, _)| (plugin.clone(), id.clone())).collect();
        self.menu = menu_keys;
        self.row_menu = row_keys;
        // The full order is kept, hidden ones too, so a control comes back where it was.
        self.prefs = prefs
            .iter()
            .enumerate()
            .map(|(index, (_, plugin, id, _, shown))| ControlPref { plugin_id: plugin.clone(), control_id: id.clone(), hidden: !shown, position: index as i64 })
            .collect();
        self.control_model.set_vec(buttons.into_iter().map(|(.., control)| control).collect::<Vec<_>>());
        self.place_controls();
        plugins_ui.set_badges(ModelRc::new(VecModel::from(badges)));
        plugins_ui.set_menu(ModelRc::new(VecModel::from(menu)));
        plugins_ui.set_row_menu(ModelRc::new(VecModel::from(row_menu)));
        let pref_items: Vec<PluginControlPref> =
            prefs.into_iter().map(|(.., label, shown)| PluginControlPref { label: label.into(), shown }).collect();
        plugins_ui.set_control_prefs(ModelRc::new(VecModel::from(pref_items)));

        // A panel whose plugin stopped or lost the permission closes.
        if let Some(state) = self.views[SLOT_PANEL].as_ref()
            && running(&state.plugin).is_none_or(|shared| !shared.granted(UI_PANEL))
        {
            self.close_slot(SLOT_PANEL);
        }
        if let Some(state) = self.views[SLOT_POPOVER].as_ref()
            && running(&state.plugin).is_none_or(|shared| !shared.granted(UI_CONTROLS))
        {
            self.close_slot(SLOT_POPOVER);
        }
    }

    /// The widths here match the ones `PluginControlSlot` draws.
    fn place_controls(&self) {
        let mut end = 0.0;
        for index in 0..self.control_model.row_count() {
            let Some(mut control) = self.control_model.row_data(index) else { continue };
            let width = if control.kind == 1 {
                (self.text_widths.get(&control.text).copied().unwrap_or(0.0) + 14.0).max(36.0)
            } else {
                34.0
            };
            end += width + if index > 0 { 2.0 } else { 0.0 };
            if control.end != end {
                control.end = end;
                self.control_model.set_row_data(index, control);
            }
        }
        if let Some(app) = self.ui() {
            app.global::<PluginsUi>().set_controls_width(end);
        }
    }

    fn save_prefs(&self) {
        if let Some(manager) = manager()
            && let Err(err) = manager.store().set_control_prefs(&self.prefs)
        {
            warn!("could not save the plugin control order: {err}");
        }
    }

    fn control_clicked(&mut self, index: usize, right: bool) {
        let Some((plugin, control)) = self.controls.get(index).cloned() else { return };
        self.click_control(&plugin, &control, right);
    }

    fn click_control(&mut self, plugin: &str, control: &str, right: bool) {
        let Some(shared) = running(plugin) else { return };
        let Some(app) = self.ui() else { return };
        let plugins = app.global::<PluginsUi>();
        if control == PANEL_CONTROL && !right {
            let open_here = plugins.get_panel_open() && self.views[SLOT_PANEL].as_ref().is_some_and(|state| state.plugin == plugin);
            if open_here {
                self.close_slot(SLOT_PANEL);
            } else if let Some(panel) = &shared.manifest.ui.panel {
                plugins.set_panel_title(panel.title.as_str().into());
                plugins.set_panel_plugin(plugin.into());
                plugins.set_panel_open(true);
                self.open_view(SLOT_PANEL, plugin, "panel");
            }
            return;
        }
        let Some(declared) = shared.manifest.control(control) else { return };
        if declared.popover && !right {
            let view = format!("popover:{control}");
            let open_here = plugins.get_popover_open() && self.views[SLOT_POPOVER].as_ref().is_some_and(|state| state.view == view && state.plugin == plugin);
            if open_here {
                self.close_slot(SLOT_POPOVER);
            } else {
                app.global::<crate::Panels>().set_tracks_open(false);
                plugins.set_popover_title(declared.label.to_uppercase().into());
                plugins.set_popover_open(true);
                self.open_view(SLOT_POPOVER, plugin, &view);
            }
            return;
        }
        shared.post_event_from_user(json!({ "type": "control", "id": control, "button": if right { "right" } else { "left" } }), true);
    }

    fn menu_clicked(&self, index: usize) {
        let Some((plugin, entry)) = self.menu.get(index) else { return };
        if let Some(shared) = running(plugin) {
            shared.post_event_from_user(json!({ "type": "menu", "id": entry }), true);
        }
    }

    fn row_menu_clicked(&self, index: usize, list_index: usize) {
        let Some((plugin, entry)) = self.row_menu.get(index) else { return };
        let Some(shared) = running(plugin) else { return };
        let snapshot = super::playlist_snapshot();
        let Some(row) = snapshot.get(list_index) else { return };
        let mut details = json!({ "index": list_index, "title": row.title, "current": row.current });
        if shared.granted(MEDIA_PATH) {
            details["path"] = json!(row.path);
        }
        shared.post_event_from_user(json!({ "type": "menu", "id": entry, "entry": details }), true);
    }

    fn show_install(&mut self) {
        let Some(app) = self.ui() else { return };
        let plugins = app.global::<PluginsUi>();
        let Some(install) = self.install.as_ref() else {
            plugins.set_install_open(false);
            return;
        };
        let grants: Vec<PluginGrant> = install.grants.iter().map(|(_, grant)| grant.clone()).collect();
        plugins.set_i_grants(ModelRc::new(VecModel::from(grants)));
        plugins.set_i_requester(install.requester.as_ref().map(|requester| requester.program.clone()).unwrap_or_default().into());
        match &install.inspection {
            Some(inspection) => {
                let manifest = &inspection.manifest;
                let update = inspection.installed_version.is_some();
                plugins.set_i_title(if update { "Update plugin" } else { "Install plugin" }.into());
                plugins.set_i_name(manifest.name.as_str().into());
                plugins.set_i_version(manifest.version.as_str().into());
                plugins.set_i_author(manifest.author.clone().unwrap_or_default().into());
                plugins.set_i_description(manifest.description.clone().unwrap_or_default().into());
                let note = match &inspection.installed_version {
                    Some(version) => format!("Version {version} is installed. Your switches stay as you set them, and new permissions are marked."),
                    None if inspection.grants.is_empty() => "It asks for no permissions.".into(),
                    None => String::new(),
                };
                plugins.set_i_note(note.into());
                plugins.set_i_ready(true);
            }
            None => {
                plugins.set_i_title("Install plugin".into());
                plugins.set_i_name(SharedString::new());
                plugins.set_i_version(SharedString::new());
                plugins.set_i_author(SharedString::new());
                plugins.set_i_description(SharedString::new());
                plugins.set_i_note(SharedString::new());
                plugins.set_i_ready(false);
            }
        }
        plugins.set_install_open(true);
    }

    fn next_install(&mut self) {
        if self.install.is_some() {
            return;
        }
        while let Some((path, requester, answer)) = self.queue.pop_front() {
            let Some(manager) = manager() else {
                if let Some(answer) = answer {
                    let _ = answer.send(Err("Plugins are off".into()));
                }
                continue;
            };
            let inspection = manager.inspect(&path);
            self.declined.retain(|(_, _, at)| at.elapsed() < DECLINE_MEMORY);
            if let (Ok(inspection), Some(requester)) = (&inspection, &requester)
                && self.declined.iter().any(|(program, id, _)| *program == requester.program && *id == inspection.manifest.id)
            {
                info!("{} asked again to install {} after it was declined", requester.program, inspection.manifest.id);
                let _ = std::fs::remove_file(&inspection.copy);
                if let Some(answer) = answer {
                    let _ = answer.send(Ok("declined"));
                }
                continue;
            }
            let error = inspection.as_ref().err().cloned();
            if let (Some(error), Some(answer)) = (&error, &answer) {
                // A program gets the reason back. The user is only shown it when they asked themselves.
                warn!("could not read the plugin {}: {error}", path.display());
                let _ = answer.send(Err(error.clone()));
                continue;
            }
            let inspection = inspection.ok();
            let grants = inspection
                .as_ref()
                .map(|inspection| {
                    let grants: Vec<_> = inspection.grants.iter().map(|(grant, _, _)| grant.clone()).collect();
                    let start = |key: &str| inspection.grants.iter().find(|(grant, ..)| grant.key == key).is_some_and(|(_, on, _)| *on);
                    let new = |key: &str| inspection.grants.iter().find(|(grant, ..)| grant.key == key).is_some_and(|(_, _, new)| *new);
                    let (keys, models) = grant_models(&grants, start, new);
                    keys.into_iter().zip(models).collect()
                })
                .unwrap_or_default();
            self.install = Some(Install { inspection, grants, requester, answer });
            self.show_install();
            if let (Some(error), Some(app)) = (error, self.ui()) {
                app.global::<PluginsUi>().set_i_error(error.into());
            } else if let Some(app) = self.ui() {
                app.global::<PluginsUi>().set_i_error(SharedString::new());
            }
            return;
        }
    }

    fn answer_install(&mut self, accepted: bool) {
        let Some(install) = self.install.take() else { return };
        let result = match (&install.inspection, accepted) {
            (Some(inspection), true) => {
                let grants: BTreeMap<String, bool> = install.grants.iter().map(|(key, grant)| (key.clone(), grant.on)).collect();
                match manager().ok_or_else(|| "Plugins are off".to_string()).and_then(|manager| manager.install(inspection, &grants)) {
                    Ok(()) => Ok("installed"),
                    Err(err) => {
                        // Shown in the same dialog, so the user sees why.
                        let error = err.clone();
                        self.install = Some(Install { inspection: None, grants: Vec::new(), requester: install.requester.clone(), answer: None });
                        self.show_install();
                        if let Some(app) = self.ui() {
                            app.global::<PluginsUi>().set_i_error(error.into());
                        }
                        Err(err)
                    }
                }
            }
            (Some(inspection), false) => {
                if let Some(requester) = &install.requester {
                    self.declined.push((requester.program.clone(), inspection.manifest.id.clone(), Instant::now()));
                }
                let _ = std::fs::remove_file(&inspection.copy);
                Ok("declined")
            }
            (None, _) => Ok("declined"),
        };
        if let Some(answer) = install.answer {
            let _ = answer.send(result);
        }
        if self.install.is_none() {
            self.show_install();
            self.next_install();
        }
    }
}

fn decode_icon(bytes: &[u8]) -> Option<slint::Image> {
    let image = image::load_from_memory(bytes).ok()?.thumbnail(64, 64).into_rgba8();
    let buffer = SharedPixelBuffer::clone_from_slice(image.as_raw(), image.width(), image.height());
    Some(slint::Image::from_rgba8(buffer))
}

fn grant_models(
    grants: &[super::manifest::Grant],
    on: impl Fn(&str) -> bool,
    is_new: impl Fn(&str) -> bool,
) -> (Vec<String>, Vec<PluginGrant>) {
    let mut last_category = "";
    let mut keys = Vec::new();
    let mut models = Vec::new();
    for grant in grants {
        let heading = if grant.category != last_category { grant.category } else { "" };
        last_category = grant.category;
        keys.push(grant.key.clone());
        models.push(PluginGrant {
            heading: heading.into(),
            label: grant.label.as_str().into(),
            reason: grant.reason.as_str().into(),
            on: on(&grant.key),
            is_new: is_new(&grant.key),
        });
    }
    (keys, models)
}

pub fn open_link(url: &str) {
    if !url.starts_with("https://") {
        return;
    }
    let result = if cfg!(target_os = "windows") {
        std::process::Command::new("explorer").arg(url).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(err) = result {
        warn!("could not open {url}: {err}");
    }
}

/// Shows the install dialog for `path`, after any that are already waiting.
pub fn queue_install(path: PathBuf, requester: Option<Requester>, answer: Option<Answer>) {
    let queued = with_ui(|ui| {
        ui.queue.push_back((path, requester, answer));
        ui.next_install();
    });
    if queued.is_none() {
        warn!("an install request came before the window was ready");
    }
}

/// Plugin actions and controls, as rows for the Shortcuts page after Rona's own: label, bound key.
pub fn shortcut_rows() -> Vec<(String, String)> {
    shortcut_targets().into_iter().map(|(_, _, label, key)| (label, key)).collect()
}

// (plugin, target, label, key) for every action and control the user can bind right now.
fn shortcut_targets() -> Vec<(String, String, String, String)> {
    let Some(manager) = manager() else { return Vec::new() };
    let bindings = manager.store().key_bindings().unwrap_or_default();
    let key_of = |plugin: &str, target: &str| {
        bindings.iter().find(|(owner, bound, _)| owner == plugin && bound == target).map(|(.., key)| key.clone()).unwrap_or_default()
    };
    let mut targets = Vec::new();
    for plugin in manager.all() {
        let Some(shared) = plugin.running() else { continue };
        let name = &shared.manifest.name;
        if shared.granted(INPUT_ACTIONS) {
            for action in &shared.manifest.actions {
                let target = format!("action:{}", action.id);
                targets.push((plugin.row.id.clone(), target.clone(), format!("{name}: {}", action.label), key_of(&plugin.row.id, &target)));
            }
        }
        if shared.granted(UI_CONTROLS) {
            for control in shared.manifest.ui.controls.iter().filter(|control| control.kind != ControlKind::Badge) {
                let target = format!("control:{}", control.id);
                targets.push((plugin.row.id.clone(), target.clone(), format!("{name}: {}", control.label), key_of(&plugin.row.id, &target)));
            }
        }
    }
    targets
}

/// Binds a key to the plugin shortcut at `index` of `shortcut_rows`. Any other binding of the key is cleared.
pub fn bind_key(index: usize, key: &str) {
    let Some(manager) = manager() else { return };
    let Some((plugin, target, ..)) = shortcut_targets().into_iter().nth(index) else { return };
    if let Err(err) = manager.store().bind_key(&plugin, &target, key) {
        warn!("could not bind a plugin shortcut: {err}");
    }
}

/// A key Rona's own shortcuts just took, so no plugin keeps it.
pub fn release_key(key: &str) {
    if let Some(manager) = manager() {
        let _ = manager.store().clear_key(key);
    }
}

/// A key no Rona shortcut has. Returns whether a plugin had it.
pub fn handle_key(key: &str) -> bool {
    let Some(manager) = manager() else { return false };
    let Some((plugin, target, _)) = manager.store().key_bindings().unwrap_or_default().into_iter().find(|(.., bound)| bound == key) else {
        return false;
    };
    let Some(shared) = running(&plugin) else { return false };
    if let Some(action) = target.strip_prefix("action:") {
        if !shared.granted(INPUT_ACTIONS) {
            return false;
        }
        shared.post_event_from_user(json!({ "type": "action", "id": action }), true);
        return true;
    }
    if let Some(control) = target.strip_prefix("control:") {
        if !shared.granted(UI_CONTROLS) {
            return false;
        }
        let control = control.to_owned();
        with_ui(|ui| ui.click_control(&plugin, &control, false));
        return true;
    }
    false
}

pub fn wire(app: &AppWindow, mpv: &crate::mpv::Mpv, osd: &Osd) {
    let models: Vec<Rc<VecModel<PluginRow>>> = (0..SLOTS).map(|_| Rc::new(VecModel::default())).collect();
    let plugins = app.global::<PluginsUi>();
    plugins.set_d_rows(ModelRc::from(models[SLOT_SETTINGS].clone()));
    plugins.set_panel_rows(ModelRc::from(models[SLOT_PANEL].clone()));
    plugins.set_popover_rows(ModelRc::from(models[SLOT_POPOVER].clone()));
    plugins.set_dialog_rows(ModelRc::from(models[SLOT_DIALOG].clone()));
    plugins.set_d_static_rows(ModelRc::from(models[SLOT_STATIC].clone()));
    let control_model: Rc<VecModel<PluginControl>> = Rc::new(VecModel::default());
    plugins.set_controls(ModelRc::from(control_model.clone()));

    let ui = Ui {
        app: app.as_weak(),
        osd: osd.clone(),
        views: (0..SLOTS).map(|_| None).collect(),
        models,
        item_models: (0..SLOTS).map(|_| Vec::new()).collect(),
        images: HashMap::new(),
        selected: None,
        detail_grants: Vec::new(),
        controls: Vec::new(),
        control_model,
        text_widths: HashMap::new(),
        menu: Vec::new(),
        row_menu: Vec::new(),
        control_state: HashMap::new(),
        menu_state: HashMap::new(),
        prefs: Vec::new(),
        install: None,
        queue: VecDeque::new(),
        declined: Vec::new(),
        live: Timer::default(),
        log_timer: Timer::default(),
        log_generation: 0,
    };
    UI.with(|slot| *slot.borrow_mut() = Some(ui));

    plugins.on_select(|index| {
        with_ui(|ui| ui.select(index));
    });
    plugins.on_set_enabled(|index, on| {
        let Some(manager) = manager() else { return };
        let Some(plugin) = usize::try_from(index).ok().and_then(|index| manager.all().get(index).cloned()) else { return };
        if let Err(err) = manager.set_enabled(&plugin.row.id, on) {
            warn!("could not turn plugin {} {}: {err}", plugin.row.id, if on { "on" } else { "off" });
        }
    });
    plugins.on_set_grant(|index, on| {
        with_ui(|ui| {
            let (Some(plugin), Some(key)) = (ui.selected.clone(), usize::try_from(index).ok().and_then(|index| ui.detail_grants.get(index).cloned()))
            else {
                return;
            };
            if let Some(manager) = manager()
                && let Err(err) = manager.set_grant(&plugin, &key, on)
            {
                warn!("could not change {key} for {plugin}: {err}");
            }
        });
    });
    plugins.on_uninstall(|delete_data| {
        let Some(plugin) = with_ui(|ui| ui.selected.clone()).flatten() else { return };
        with_ui(|ui| ui.select(-1));
        if let Some(manager) = manager()
            && let Err(err) = manager.uninstall(&plugin, delete_data)
        {
            warn!("could not uninstall {plugin}: {err}");
        }
    });
    plugins.on_reload_dev(|| {
        let Some(plugin) = with_ui(|ui| ui.selected.clone()).flatten() else { return };
        with_ui(|ui| {
            ui.close_view(SLOT_SETTINGS);
        });
        if let Some(manager) = manager() {
            manager.reload_dev(&plugin);
        }
    });
    let app_weak = app.as_weak();
    plugins.on_install_file(move || {
        let app_weak = app_weak.clone();
        // The native dialog blocks its thread, which must not be the UI thread that draws the video.
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new().set_title("Install a plugin").add_filter("Rona plugin", &["rp"]).pick_file();
            if let Some(path) = picked {
                let _ = app_weak.upgrade_in_event_loop(move |_| queue_install(path, None, None));
            }
        });
    });
    let app_weak = app.as_weak();
    plugins.on_load_folder(move || {
        let app_weak = app_weak.clone();
        std::thread::spawn(move || {
            let Some(folder) = rfd::FileDialog::new().set_title("Load a plugin from its build folder").pick_folder() else { return };
            let _ = app_weak.upgrade_in_event_loop(move |app| {
                let message = match manager().map(|manager| manager.load_unpacked(&folder)) {
                    Some(Ok(_)) => String::new(),
                    Some(Err(err)) => err,
                    None => "Plugins are off".into(),
                };
                app.global::<PluginsUi>().set_list_message(message.into());
            });
        });
    });
    plugins.on_open_link(|url| open_link(&url));
    plugins.on_view_event(|slot, row, item, kind, value| {
        let (Ok(slot), Ok(row)) = (usize::try_from(slot), usize::try_from(row)) else { return };
        if slot >= SLOTS {
            return;
        }
        with_ui(|ui| ui.view_event(slot, row, usize::try_from(item).ok(), &kind, &value));
    });
    plugins.on_text_measured(|text, width| {
        with_ui(|ui| {
            if ui.text_widths.get(&text) != Some(&width) {
                if ui.text_widths.len() >= 256 {
                    let shown: Vec<SharedString> = ui.control_model.iter().map(|control| control.text).collect();
                    ui.text_widths.retain(|text, _| shown.contains(text));
                }
                ui.text_widths.insert(text, width);
                ui.place_controls();
            }
        });
    });
    plugins.on_control_clicked(|index, right| {
        with_ui(|ui| ui.control_clicked(index.max(0) as usize, right));
    });
    plugins.on_menu_clicked(|index| {
        with_ui(|ui| ui.menu_clicked(index.max(0) as usize));
    });
    plugins.on_row_menu_clicked(|index, list_index| {
        with_ui(|ui| ui.row_menu_clicked(index.max(0) as usize, list_index.max(0) as usize));
    });
    plugins.on_close_panel(|| {
        with_ui(|ui| ui.close_slot(SLOT_PANEL));
    });
    plugins.on_dialog_close(|| {
        with_ui(|ui| ui.close_slot(SLOT_DIALOG));
    });
    plugins.on_dialog_action(|index| {
        with_ui(|ui| {
            let Some(state) = ui.views[SLOT_DIALOG].as_ref() else { return };
            let Some((id, _)) = usize::try_from(index).ok().and_then(|index| state.actions.get(index)).cloned() else { return };
            let (plugin, view_name) = (state.plugin.clone(), state.view.clone());
            ui.send_event(SLOT_DIALOG, &plugin, &view_name, &id, "click", Value::Null, None);
        });
    });
    plugins.on_i_set_grant(|index, on| {
        with_ui(|ui| {
            if let Some((_, grant)) = ui.install.as_mut().and_then(|install| install.grants.get_mut(index.max(0) as usize)) {
                grant.on = on;
            }
        });
    });
    plugins.on_install_answer(|accepted| {
        with_ui(|ui| ui.answer_install(accepted));
    });
    plugins.on_control_pref_shown(|index, shown| {
        with_ui(|ui| {
            if let Some(pref) = ui.prefs.get_mut(index.max(0) as usize) {
                pref.hidden = !shown;
                ui.save_prefs();
                ui.show_controls();
            }
        });
    });
    plugins.on_control_pref_move(|from, to| {
        with_ui(|ui| {
            let (from, to) = (from.max(0) as usize, to.max(0) as usize);
            if from >= ui.prefs.len() || to >= ui.prefs.len() {
                return;
            }
            let pref = ui.prefs.remove(from);
            ui.prefs.insert(to, pref);
            for (index, pref) in ui.prefs.iter_mut().enumerate() {
                pref.position = index as i64;
            }
            ui.save_prefs();
            ui.show_controls();
        });
    });

    // The log only updates while its page is open, and only when a line was added.
    let app_weak = app.as_weak();
    with_ui(|ui| {
        ui.log_timer.start(TimerMode::Repeated, LOG_POLL, move || {
            let Some(app) = app_weak.upgrade() else { return };
            if app.global::<crate::Panels>().get_settings_open() && app.global::<PluginsUi>().get_selected() >= 0 {
                with_ui(|ui| ui.show_log(false));
            }
        });
    });

    super::start(mpv, Arc::new(AppBridge));
    super::ipc::serve();
    with_ui(Ui::refresh_all);
}

/// Stops every plugin, when the window closes.
pub fn shutdown() {
    if let Some(manager) = manager() {
        Manager::shutdown(manager);
    }
}
