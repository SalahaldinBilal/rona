use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use std::time::Duration;

use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::color;
use crate::settings::{self, SharedConfig};
use crate::{AppWindow, Theme, ThemeItem, ThemeProfiles};

const FOLDER: &str = "themes";
const EXTENSION: &str = "conf";
const DEFAULT_NAME: &str = "Default";
const MAX_NAME_CHARS: usize = 60;
const MAX_LENGTH: f32 = 400.0;
const MAX_DURATION_MS: i64 = 5000;
// Dragging in the color picker changes a value many times a second, so the file is written once it stops.
const SAVE_DELAY: Duration = Duration::from_millis(400);
const HEADINGS: [&str; 3] = ["COLORS", "SIZES", "TIMING"];
const UNITS: [&str; 3] = ["", "px", "ms"];

#[derive(Clone, Copy, PartialEq)]
enum Value {
    Color(Color),
    Length(f32),
    Duration(i64),
}

impl Value {
    fn kind(self) -> usize {
        match self {
            Value::Color(_) => 0,
            Value::Length(_) => 1,
            Value::Duration(_) => 2,
        }
    }

    fn text(self) -> String {
        match self {
            Value::Color(value) => color::format(value),
            Value::Length(length) => length.to_string(),
            Value::Duration(milliseconds) => milliseconds.to_string(),
        }
    }

    fn parse_like(self, text: &str) -> Option<Value> {
        let text = text.trim();
        match self {
            Value::Color(_) => color::parse(text).map(Value::Color),
            Value::Length(_) => text
                .parse::<f32>()
                .ok()
                .filter(|length| length.is_finite() && (0.0..=MAX_LENGTH).contains(length))
                .map(Value::Length),
            Value::Duration(_) => text
                .parse::<i64>()
                .ok()
                .filter(|milliseconds| (0..=MAX_DURATION_MS).contains(milliseconds))
                .map(Value::Duration),
        }
    }
}

struct Key {
    name: &'static str,
    get: fn(&Theme<'_>) -> Value,
    set: fn(&Theme<'_>, Value),
}

// Slint globals have no lookup by name, so each property of the Theme global in ui/theme.slint is listed here.
macro_rules! keys {
    ($($kind:ident $name:literal $get:ident $set:ident,)*) => {
        static KEYS: &[Key] = &[$(Key {
            name: $name,
            get: |theme| Value::$kind(theme.$get()),
            set: |theme, value| {
                if let Value::$kind(value) = value {
                    theme.$set(value);
                }
            },
        }),*];
    };
}

keys! {
    Color "chrome" get_chrome set_chrome,
    Color "panel" get_panel set_panel,
    Color "backdrop" get_backdrop set_backdrop,
    Color "line" get_line set_line,
    Color "line-hover" get_line_hover set_line_hover,
    Color "hover" get_hover set_hover,
    Color "pressed" get_pressed set_pressed,
    Color "text" get_text set_text,
    Color "muted" get_muted set_muted,
    Color "accent" get_accent set_accent,
    Color "accent-hover" get_accent_hover set_accent_hover,
    Color "accent-soft" get_accent_soft set_accent_soft,
    Color "danger" get_danger set_danger,
    Color "warning" get_warning set_warning,
    Color "on-warning" get_on_warning set_on_warning,
    Length "title-height" get_title_height set_title_height,
    Length "controls-height" get_controls_height set_controls_height,
    Duration "hover-duration" get_hover_duration set_hover_duration,
    Duration "fade-duration" get_fade_duration set_fade_duration,
}

fn label(name: &str) -> String {
    let words = name.replace('-', " ");
    let mut characters = words.chars();
    characters.next().map(|first| first.to_uppercase().chain(characters).collect()).unwrap_or_default()
}

fn folder() -> Option<PathBuf> {
    settings::config_dir().map(|dir| dir.join(FOLDER))
}

fn file_name(profile: &str) -> String {
    format!("{profile}.{EXTENSION}")
}

// Relative to the config folder, as settings::read_file and write_file expect.
fn relative_file(profile: &str) -> PathBuf {
    Path::new(FOLDER).join(file_name(profile))
}

// Profile names are file names, so anything Windows doesn't allow in one is ruled out everywhere.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name == name.trim()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.ends_with('.')
        && !name.chars().any(|character| character.is_control() || r#"\/:*?"<>|"#.contains(character))
        && name.to_lowercase() != DEFAULT_NAME.to_lowercase()
}

fn sort_names(names: &mut [String]) {
    names.sort_by_key(|name| name.to_lowercase());
}

fn saved_names() -> Vec<String> {
    let Some(entries) = folder().and_then(|folder| std::fs::read_dir(folder).ok()) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == EXTENSION))
        .filter_map(|path| path.file_stem()?.to_str().map(str::to_string))
        .filter(|name| is_valid_name(name))
        .collect();
    sort_names(&mut names);
    names
}

struct Profiles {
    app: slint::Weak<AppWindow>,
    config: SharedConfig,
    // The values ui/theme.slint gives, read before anything is overridden.
    defaults: Vec<Value>,
    names: Vec<String>,
    // Index into `names`, None for the default theme.
    current: Option<usize>,
    // Only values that differ from the default are kept and saved.
    overrides: Vec<Option<Value>>,
    items: Rc<VecModel<ThemeItem>>,
    save_timer: Timer,
    pending_save: Rc<RefCell<Option<(PathBuf, Vec<(String, String)>)>>>,
}

impl Drop for Profiles {
    fn drop(&mut self) {
        self.flush();
    }
}

impl Profiles {
    fn current_name(&self) -> Option<&str> {
        self.current.map(|index| self.names[index].as_str())
    }

    fn value(&self, index: usize) -> Value {
        self.overrides[index].unwrap_or(self.defaults[index])
    }

    fn name_available(&self, name: &str, except: Option<usize>) -> bool {
        let lowercase = name.to_lowercase();
        is_valid_name(name)
            && !self.names.iter().enumerate().any(|(index, other)| Some(index) != except && other.to_lowercase() == lowercase)
    }

    fn read_overrides(&self, profile: &str) -> Vec<Option<Value>> {
        let mut overrides = vec![None; KEYS.len()];
        settings::read_file(relative_file(profile), |name, text| {
            if let Some(index) = KEYS.iter().position(|key| key.name == name) {
                let default = self.defaults[index];
                overrides[index] = default.parse_like(text).filter(|value| *value != default);
            }
        });
        overrides
    }

    fn load_current(&mut self) {
        let overrides = match self.current_name() {
            Some(profile) => self.read_overrides(profile),
            None => vec![None; KEYS.len()],
        };
        self.overrides = overrides;
    }

    fn entries(&self) -> Vec<(String, String)> {
        KEYS.iter()
            .zip(&self.overrides)
            .filter_map(|(key, value)| value.map(|value| (key.name.to_string(), value.text())))
            .collect()
    }

    fn write_current(&self) {
        self.save_timer.stop();
        self.pending_save.borrow_mut().take();
        if let Some(profile) = self.current_name() {
            settings::write_file(relative_file(profile), &self.entries());
        }
    }

    fn write_current_later(&self) {
        let Some(profile) = self.current_name() else { return };
        *self.pending_save.borrow_mut() = Some((relative_file(profile), self.entries()));
        let pending = self.pending_save.clone();
        self.save_timer.start(TimerMode::SingleShot, SAVE_DELAY, move || {
            if let Some((file, entries)) = pending.borrow_mut().take() {
                settings::write_file(file, &entries);
            }
        });
    }

    // Runs before switching, renaming or deleting, so a write still waiting goes to the right file.
    fn flush(&self) {
        self.save_timer.stop();
        if let Some((file, entries)) = self.pending_save.borrow_mut().take() {
            settings::write_file(file, &entries);
        }
    }

    fn remember_choice(&self) {
        let mut config = self.config.borrow_mut();
        config.theme = self.current_name().unwrap_or_default().to_string();
        settings::save(&config);
    }

    fn item(&self, index: usize) -> ThemeItem {
        let value = self.value(index);
        let kind = value.kind();
        let starts_group = index == 0 || self.defaults[index - 1].kind() != kind;
        ThemeItem {
            label: label(KEYS[index].name).into(),
            heading: if starts_group { HEADINGS[kind] } else { "" }.into(),
            unit: UNITS[kind].into(),
            value: value.text().into(),
            is_color: matches!(value, Value::Color(_)),
            swatch: match value {
                Value::Color(color) => color,
                _ => Color::default(),
            },
            overridden: self.overrides[index].is_some(),
        }
    }

    fn show_names(&self, ui: &ThemeProfiles) {
        let names: Vec<SharedString> =
            std::iter::once(DEFAULT_NAME).chain(self.names.iter().map(String::as_str)).map(SharedString::from).collect();
        ui.set_names(ModelRc::new(VecModel::from(names)));
        ui.set_current(self.current.map_or(0, |index| index as i32 + 1));
        ui.set_current_name(self.current_name().unwrap_or(DEFAULT_NAME).into());
    }

    fn show_all(&self) {
        let Some(app) = self.app.upgrade() else { return };
        self.show_names(&app.global::<ThemeProfiles>());
        self.items.set_vec((0..KEYS.len()).map(|index| self.item(index)).collect::<Vec<_>>());
        let theme = app.global::<Theme>();
        for (index, key) in KEYS.iter().enumerate() {
            (key.set)(&theme, self.value(index));
        }
    }

    fn show_value(&self, index: usize) {
        self.items.set_row_data(index, self.item(index));
        if let Some(app) = self.app.upgrade() {
            (KEYS[index].set)(&app.global::<Theme>(), self.value(index));
        }
    }

    fn select(&mut self, index: i32) {
        self.flush();
        self.current = usize::try_from(index - 1).ok().filter(|index| *index < self.names.len());
        self.load_current();
        self.remember_choice();
        self.show_all();
    }

    // A new profile starts as a copy of the one shown.
    fn create(&mut self) {
        self.flush();
        let Some(name) = (1..).map(|number| format!("Custom {number}")).find(|name| self.name_available(name, None))
        else {
            return;
        };
        self.names.push(name.clone());
        sort_names(&mut self.names);
        self.current = self.names.iter().position(|other| *other == name);
        self.write_current();
        self.remember_choice();
        self.show_all();
    }

    fn delete(&mut self) {
        let Some(index) = self.current else { return };
        self.save_timer.stop();
        self.pending_save.borrow_mut().take();
        if let Some(path) = folder().map(|folder| folder.join(file_name(&self.names[index])))
            && let Err(err) = std::fs::remove_file(&path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("could not delete {}: {err}", path.display());
            return;
        }
        self.names.remove(index);
        self.current = None;
        self.load_current();
        self.remember_choice();
        self.show_all();
    }

    fn rename(&mut self, name: &str) {
        let Some(index) = self.current else { return };
        if self.names[index] == name || !self.name_available(name, Some(index)) {
            return;
        }
        self.flush();
        if let Some(folder) = folder() {
            let (from, to) = (folder.join(file_name(&self.names[index])), folder.join(file_name(name)));
            if let Err(err) = std::fs::rename(&from, &to) {
                eprintln!("could not rename {} to {}: {err}", from.display(), to.display());
                if let Some(app) = self.app.upgrade() {
                    self.show_names(&app.global::<ThemeProfiles>());
                }
                return;
            }
        }
        self.names[index] = name.to_string();
        sort_names(&mut self.names);
        self.current = self.names.iter().position(|other| other == name);
        self.remember_choice();
        if let Some(app) = self.app.upgrade() {
            self.show_names(&app.global::<ThemeProfiles>());
        }
    }

    fn set_value(&mut self, index: usize, text: &str) {
        if self.current.is_none() || index >= KEYS.len() {
            return;
        }
        let default = self.defaults[index];
        let Some(value) = default.parse_like(text) else { return };
        self.overrides[index] = (value != default).then_some(value);
        self.write_current_later();
        self.show_value(index);
    }

    fn revert(&mut self, index: usize) {
        if self.current.is_none() || index >= KEYS.len() {
            return;
        }
        self.overrides[index] = None;
        self.write_current();
        self.show_value(index);
    }

    fn revert_all(&mut self) {
        if self.current.is_none() {
            return;
        }
        self.overrides = vec![None; KEYS.len()];
        self.write_current();
        self.show_all();
    }
}

/// Applies the chosen profile, so it should run before the window is shown.
pub fn wire(app: &AppWindow, config: &SharedConfig) {
    let theme = app.global::<Theme>();
    let defaults: Vec<Value> = KEYS.iter().map(|key| (key.get)(&theme)).collect();
    let names = saved_names();
    let current = {
        let chosen = config.borrow().theme.to_lowercase();
        names.iter().position(|name| !chosen.is_empty() && name.to_lowercase() == chosen)
    };
    let items = Rc::new(VecModel::default());
    let ui = app.global::<ThemeProfiles>();
    ui.set_items(ModelRc::from(items.clone()));

    let mut profiles = Profiles {
        app: app.as_weak(),
        config: config.clone(),
        defaults,
        names,
        current,
        overrides: Vec::new(),
        items,
        save_timer: Timer::default(),
        pending_save: Rc::default(),
    };
    profiles.load_current();
    profiles.show_all();
    let profiles = Rc::new(RefCell::new(profiles));

    let shared = profiles.clone();
    ui.on_select(move |index| shared.borrow_mut().select(index));
    let shared = profiles.clone();
    ui.on_create(move || shared.borrow_mut().create());
    let shared = profiles.clone();
    ui.on_delete(move || shared.borrow_mut().delete());
    let shared = profiles.clone();
    ui.on_rename(move |name| shared.borrow_mut().rename(&name));
    let shared = profiles.clone();
    ui.on_valid_name(move |name| {
        let profiles = shared.borrow();
        profiles.current.is_none_or(|index| profiles.name_available(&name, Some(index)))
    });
    let shared = profiles.clone();
    ui.on_valid_value(move |index, text| {
        let profiles = shared.borrow();
        usize::try_from(index)
            .ok()
            .and_then(|index| profiles.defaults.get(index))
            .is_some_and(|default| default.parse_like(&text).is_some())
    });
    let shared = profiles.clone();
    ui.on_set_value(move |index, text| {
        if let Ok(index) = usize::try_from(index) {
            shared.borrow_mut().set_value(index, &text);
        }
    });
    let shared = profiles.clone();
    ui.on_revert(move |index| {
        if let Ok(index) = usize::try_from(index) {
            shared.borrow_mut().revert(index);
        }
    });
    ui.on_revert_all(move || profiles.borrow_mut().revert_all());
}
