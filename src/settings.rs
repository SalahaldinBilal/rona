use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use slint::{Color, ComponentHandle, ModelRc, PhysicalPosition, PhysicalSize, SharedString, VecModel};

use crate::http_cache::{self, NetworkOptions};
use crate::mpv::Mpv;
use crate::shortcuts::{self, Action};
use crate::video::Renderer;
use crate::{AppWindow, Player, Settings as SettingsUi, ShortcutItem, WindowControls, log_error, update_window_level};

pub type SharedConfig = Rc<RefCell<Config>>;

const FILE_NAME: &str = "settings.conf";
const AUTO_AUDIO_DEVICE: &str = "auto";
const DEFAULT_SPEEDS: [f32; 6] = [1.0, 1.25, 1.5, 2.0, 0.5, 0.75];const END_ACTIONS: [&str; 3] = ["stop", "next", "loop"];
const SCALING_CHOICES: [&str; 3] = ["fast", "balanced", "high"];
const RENDERERS: [&str; 3] = ["auto", "d3d11", "opengl"];
const RENDERER_LABELS: [&str; 3] = ["Auto", "Direct3D 11", "OpenGL"];
// Positions in RENDERERS that this platform offers.
#[cfg(windows)]
const RENDERER_CHOICES: &[usize] = &[0, 1, 2];
#[cfg(not(windows))]
const RENDERER_CHOICES: &[usize] = &[0, 2];
#[cfg(windows)]
const RENDERER_HINT: &str = "Auto uses Direct3D 11.";
#[cfg(not(windows))]
const RENDERER_HINT: &str = "Auto uses OpenGL.";
const ASPECT_RATIOS: [&str; 5] = ["-1", "16:9", "4:3", "21:9", "2.35:1"];
const VOLUME_LIMITS: [&str; 3] = ["100", "150", "200"];
const DOUBLE_CLICK_CHOICES: [&str; 2] = ["fullscreen", "nothing"];
const SUBTITLE_SIZES: (u32, u32) = (16, 80);
const MAX_SUBTITLE_RAISE: u32 = 50;

#[derive(Clone, Copy, PartialEq)]
pub struct WindowGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    maximized: bool,
}

#[derive(Clone, PartialEq)]
pub struct Config {
    pub resume: bool,
    pub resume_days: u32,
    pub start_volume: u32,
    pub end_action: usize,
    pub seek_short: f64,
    pub seek_long: f64,
    pub speeds: Vec<f32>,
    pub skip_chapters: bool,
    pub skip_chapter_titles: String,

    pub renderer: usize,
    pub hardware_decoding: bool,
    pub scaling: usize,
    pub deband: bool,
    pub aspect: usize,

    pub audio_languages: String,
    pub volume_max: usize,
    pub normalize: bool,
    pub audio_device: String,

    pub subtitle_languages: String,
    pub subtitle_auto_load: bool,
    pub skip_matching_subtitles: bool,
    pub subtitle_font: String,
    pub subtitle_size: u32,
    pub subtitle_color: String,
    pub subtitle_border_color: String,
    pub subtitle_raise: u32,
    pub subtitle_override_styles: bool,

    pub cache_mib: u64,
    pub read_ahead_mib: u64,
    pub connect_timeout: f64,
    pub read_timeout: f64,
    pub user_agent: String,

    pub on_top_while_playing: bool,
    pub remember_window: bool,
    pub double_click: usize,
    pub hide_controls_after: f64,
    pub osd_duration: f64,

    pub shortcuts: Vec<String>,

    pub check_updates: bool,

    window: Option<WindowGeometry>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            resume: true,
            resume_days: 3,
            start_volume: 100,
            end_action: 1,
            seek_short: 5.0,
            seek_long: 60.0,
            speeds: DEFAULT_SPEEDS.to_vec(),
            skip_chapters: false,
            skip_chapter_titles: String::new(),

            renderer: 0,
            hardware_decoding: true,
            scaling: 1,
            deband: false,
            aspect: 0,

            audio_languages: String::new(),
            volume_max: 0,
            normalize: false,
            audio_device: AUTO_AUDIO_DEVICE.into(),

            subtitle_languages: String::new(),
            subtitle_auto_load: true,
            skip_matching_subtitles: false,
            subtitle_font: String::new(),
            subtitle_size: 38,
            subtitle_color: "#ffffff".into(),
            subtitle_border_color: "#000000".into(),
            subtitle_raise: 0,
            subtitle_override_styles: false,

            cache_mib: http_cache::DEFAULT_CACHE_MIB,
            read_ahead_mib: 150,
            connect_timeout: http_cache::DEFAULT_CONNECT_TIMEOUT.as_secs_f64(),
            read_timeout: http_cache::DEFAULT_READ_TIMEOUT.as_secs_f64(),
            user_agent: String::new(),

            on_top_while_playing: false,
            remember_window: true,
            double_click: 0,
            hide_controls_after: 2.5,
            osd_duration: 1.2,

            shortcuts: Action::ALL.iter().map(|action| action.default_key()).collect(),

            check_updates: true,

            window: None,
        }
    }
}

impl Config {
    pub fn pointer_hide_delay(&self) -> Duration {
        Duration::from_secs_f64(self.hide_controls_after)
    }

    pub fn osd_duration(&self) -> Duration {
        Duration::from_secs_f64(self.osd_duration)
    }

    /// Only read at startup, since switching mpv's render context means rebuilding the video output.
    pub fn renderer(&self) -> Renderer {
        match RENDERERS[self.renderer] {
            "d3d11" if cfg!(windows) => Renderer::Direct3D11,
            "opengl" => Renderer::OpenGl,
            _ if cfg!(windows) => Renderer::Direct3D11,
            _ => Renderer::OpenGl,
        }
    }

    fn network(&self) -> NetworkOptions {
        NetworkOptions {
            connect_timeout: Duration::from_secs_f64(self.connect_timeout),
            read_timeout: Duration::from_secs_f64(self.read_timeout),
            user_agent: self.user_agent.clone(),
        }
    }

    fn volume_limit(&self) -> f32 {
        VOLUME_LIMITS[self.volume_max].parse::<f32>().unwrap_or(100.0) / 100.0
    }

    fn reset_page(&mut self, page: i32) {
        let defaults = Config::default();
        match page {
            0 => {
                self.resume = defaults.resume;
                self.resume_days = defaults.resume_days;
                self.start_volume = defaults.start_volume;
                self.end_action = defaults.end_action;
                self.seek_short = defaults.seek_short;
                self.seek_long = defaults.seek_long;
                self.speeds = defaults.speeds;
                self.skip_chapters = defaults.skip_chapters;
                self.skip_chapter_titles = defaults.skip_chapter_titles;
            }
            1 => {
                self.renderer = defaults.renderer;
                self.hardware_decoding = defaults.hardware_decoding;
                self.scaling = defaults.scaling;
                self.deband = defaults.deband;
                self.aspect = defaults.aspect;
            }
            2 => {
                self.audio_languages = defaults.audio_languages;
                self.volume_max = defaults.volume_max;
                self.normalize = defaults.normalize;
                self.audio_device = defaults.audio_device;
            }
            3 => {
                self.subtitle_languages = defaults.subtitle_languages;
                self.subtitle_auto_load = defaults.subtitle_auto_load;
                self.skip_matching_subtitles = defaults.skip_matching_subtitles;
                self.subtitle_font = defaults.subtitle_font;
                self.subtitle_size = defaults.subtitle_size;
                self.subtitle_color = defaults.subtitle_color;
                self.subtitle_border_color = defaults.subtitle_border_color;
                self.subtitle_raise = defaults.subtitle_raise;
                self.subtitle_override_styles = defaults.subtitle_override_styles;
            }
            4 => {
                self.cache_mib = defaults.cache_mib;
                self.read_ahead_mib = defaults.read_ahead_mib;
                self.connect_timeout = defaults.connect_timeout;
                self.read_timeout = defaults.read_timeout;
                self.user_agent = defaults.user_agent;
            }
            5 => {
                self.on_top_while_playing = defaults.on_top_while_playing;
                self.remember_window = defaults.remember_window;
                self.double_click = defaults.double_click;
                self.hide_controls_after = defaults.hide_controls_after;
                self.osd_duration = defaults.osd_duration;
            }
            6 => self.shortcuts = defaults.shortcuts,
            7 => self.check_updates = defaults.check_updates,
            _ => {}
        }
    }

    fn mpv_properties(&self, defaults: &MpvDefaults) -> Vec<(&'static str, String)> {
        let (scale, cscale, dscale) = match self.scaling {
            0 => ("bilinear".into(), "bilinear".into(), "bilinear".into()),
            2 => ("ewa_lanczossharp".into(), "ewa_lanczossharp".into(), "mitchell".into()),
            _ => (defaults.scale.clone(), defaults.cscale.clone(), defaults.dscale.clone()),
        };
        let font = if self.subtitle_font.trim().is_empty() { "sans-serif" } else { self.subtitle_font.trim() };
        vec![
            ("keep-open", if self.end_action == 0 { "always" } else { "yes" }.into()),
            ("loop-file", if self.end_action == 2 { "inf" } else { "no" }.into()),
            ("hwdec", if self.hardware_decoding { "auto-safe" } else { "no" }.into()),
            ("scale", scale),
            ("cscale", cscale),
            ("dscale", dscale),
            ("deband", yes_no(self.deband)),
            ("video-aspect-override", ASPECT_RATIOS[self.aspect].into()),
            ("alang", self.audio_languages.clone()),
            ("volume-max", VOLUME_LIMITS[self.volume_max].into()),
            ("af", if self.normalize { "lavfi=[dynaudnorm]" } else { "" }.into()),
            ("audio-device", self.audio_device.clone()),
            ("slang", self.subtitle_languages.clone()),
            ("sub-auto", if self.subtitle_auto_load { "fuzzy" } else { "no" }.into()),
            ("subs-with-matching-audio", yes_no(!self.skip_matching_subtitles)),
            ("sub-font", font.into()),
            ("sub-font-size", self.subtitle_size.to_string()),
            ("sub-color", self.subtitle_color.clone()),
            ("sub-border-color", self.subtitle_border_color.clone()),
            ("sub-pos", (100 - self.subtitle_raise).to_string()),
            ("sub-ass-override", if self.subtitle_override_styles { "force" } else { "scale" }.into()),
            ("demuxer-max-bytes", format!("{}MiB", self.read_ahead_mib)),
        ]
    }

    fn entries(&self) -> Vec<(String, String)> {
        let mut entries: Vec<(String, String)> = [
            ("resume", yes_no(self.resume)),
            ("resume-days", self.resume_days.to_string()),
            ("start-volume", self.start_volume.to_string()),
            ("end-action", END_ACTIONS[self.end_action].into()),
            ("seek-short", self.seek_short.to_string()),
            ("seek-long", self.seek_long.to_string()),
            ("speeds", speeds_text(&self.speeds)),
            ("skip-chapters", yes_no(self.skip_chapters)),
            ("skip-chapter-titles", self.skip_chapter_titles.clone()),
            ("renderer", RENDERERS[self.renderer].into()),
            ("hardware-decoding", yes_no(self.hardware_decoding)),
            ("scaling", SCALING_CHOICES[self.scaling].into()),
            ("deband", yes_no(self.deband)),
            ("aspect", ASPECT_RATIOS[self.aspect].into()),
            ("audio-languages", self.audio_languages.clone()),
            ("volume-max", VOLUME_LIMITS[self.volume_max].into()),
            ("normalize", yes_no(self.normalize)),
            ("audio-device", self.audio_device.clone()),
            ("subtitle-languages", self.subtitle_languages.clone()),
            ("subtitle-auto-load", yes_no(self.subtitle_auto_load)),
            ("skip-matching-subtitles", yes_no(self.skip_matching_subtitles)),
            ("subtitle-font", self.subtitle_font.clone()),
            ("subtitle-size", self.subtitle_size.to_string()),
            ("subtitle-color", self.subtitle_color.clone()),
            ("subtitle-border-color", self.subtitle_border_color.clone()),
            ("subtitle-raise", self.subtitle_raise.to_string()),
            ("subtitle-override-styles", yes_no(self.subtitle_override_styles)),
            ("cache-mib", self.cache_mib.to_string()),
            ("read-ahead-mib", self.read_ahead_mib.to_string()),
            ("connect-timeout", self.connect_timeout.to_string()),
            ("read-timeout", self.read_timeout.to_string()),
            ("user-agent", self.user_agent.clone()),
            ("on-top-while-playing", yes_no(self.on_top_while_playing)),
            ("remember-window", yes_no(self.remember_window)),
            ("double-click", DOUBLE_CLICK_CHOICES[self.double_click].into()),
            ("hide-controls-after", self.hide_controls_after.to_string()),
            ("osd-duration", self.osd_duration.to_string()),
            ("check-updates", yes_no(self.check_updates)),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
        for (action, key) in Action::ALL.iter().zip(&self.shortcuts) {
            let name = if key.is_empty() { String::new() } else { shortcuts::key_name(key) };
            entries.push((format!("shortcut.{}", action.config_name()), name));
        }
        if let Some(window) = self.window {
            let value = format!(
                "{} {} {} {} {}",
                window.x,
                window.y,
                window.width,
                window.height,
                yes_no(window.maximized)
            );
            entries.push(("window".into(), value));
        }
        entries
    }

    fn set(&mut self, name: &str, value: &str) {
        match name {
            "resume" => set_flag(&mut self.resume, value),
            "resume-days" => set_number(&mut self.resume_days, value, |days| *days > 0),
            "start-volume" => set_number(&mut self.start_volume, value, |volume| *volume <= 100),
            "end-action" => set_choice(&mut self.end_action, value, &END_ACTIONS),
            "seek-short" => set_number(&mut self.seek_short, value, is_positive),
            "seek-long" => set_number(&mut self.seek_long, value, is_positive),
            "speeds" => {
                if let Some(speeds) = parse_speeds(value) {
                    self.speeds = speeds;
                }
            }
            "skip-chapters" => set_flag(&mut self.skip_chapters, value),
            "skip-chapter-titles" => self.skip_chapter_titles = value.into(),
            "renderer" => set_choice(&mut self.renderer, value, &RENDERERS),
            "hardware-decoding" => set_flag(&mut self.hardware_decoding, value),
            "scaling" => set_choice(&mut self.scaling, value, &SCALING_CHOICES),
            "deband" => set_flag(&mut self.deband, value),
            "aspect" => set_choice(&mut self.aspect, value, &ASPECT_RATIOS),
            "audio-languages" => self.audio_languages = value.into(),
            "volume-max" => set_choice(&mut self.volume_max, value, &VOLUME_LIMITS),
            "normalize" => set_flag(&mut self.normalize, value),
            "audio-device" if !value.is_empty() => self.audio_device = value.into(),
            "subtitle-languages" => self.subtitle_languages = value.into(),
            "subtitle-auto-load" => set_flag(&mut self.subtitle_auto_load, value),
            "skip-matching-subtitles" => set_flag(&mut self.skip_matching_subtitles, value),
            "subtitle-font" => self.subtitle_font = value.into(),
            "subtitle-size" => set_number(&mut self.subtitle_size, value, |size| {
                (SUBTITLE_SIZES.0..=SUBTITLE_SIZES.1).contains(size)
            }),
            "subtitle-color" => set_color(&mut self.subtitle_color, value),
            "subtitle-border-color" => set_color(&mut self.subtitle_border_color, value),
            "subtitle-raise" => set_number(&mut self.subtitle_raise, value, |raise| *raise <= MAX_SUBTITLE_RAISE),
            "subtitle-override-styles" => set_flag(&mut self.subtitle_override_styles, value),
            "cache-mib" => set_number(&mut self.cache_mib, value, |size| *size > 0),
            "read-ahead-mib" => set_number(&mut self.read_ahead_mib, value, |size| *size > 0),
            "connect-timeout" => set_number(&mut self.connect_timeout, value, is_positive),
            "read-timeout" => set_number(&mut self.read_timeout, value, is_positive),
            "user-agent" => self.user_agent = value.into(),
            "on-top-while-playing" => set_flag(&mut self.on_top_while_playing, value),
            "remember-window" => set_flag(&mut self.remember_window, value),
            "double-click" => set_choice(&mut self.double_click, value, &DOUBLE_CLICK_CHOICES),
            "hide-controls-after" => set_number(&mut self.hide_controls_after, value, is_positive),
            "osd-duration" => set_number(&mut self.osd_duration, value, is_positive),
            "check-updates" => set_flag(&mut self.check_updates, value),
            "window" => self.window = parse_window(value).or(self.window),
            _ => {
                let Some(action) = name.strip_prefix("shortcut.") else { return };
                let Some(index) = Action::ALL.iter().position(|known| known.config_name() == action) else {
                    return;
                };
                if value.is_empty() {
                    self.shortcuts[index].clear();
                } else if let Some(key) = shortcuts::key_from_name(value) {
                    self.shortcuts[index] = key;
                }
            }
        }
    }
}

fn yes_no(flag: bool) -> String {
    if flag { "yes" } else { "no" }.into()
}

fn is_positive(value: &f64) -> bool {
    value.is_finite() && *value > 0.0
}

fn set_flag(field: &mut bool, value: &str) {
    match value {
        "yes" => *field = true,
        "no" => *field = false,
        _ => {}
    }
}

fn set_number<T: FromStr>(field: &mut T, value: &str, valid: impl Fn(&T) -> bool) {
    if let Some(number) = value.trim().parse::<T>().ok().filter(|number| valid(number)) {
        *field = number;
    }
}

fn set_choice(field: &mut usize, value: &str, choices: &[&str]) {
    if let Some(index) = choices.iter().position(|choice| *choice == value) {
        *field = index;
    }
}

fn set_color(field: &mut String, value: &str) {
    if let Some(color) = parse_color(value) {
        *field = color_hex(color);
    }
}

fn parse_color(text: &str) -> Option<Color> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let channel = |range: std::ops::Range<usize>| u8::from_str_radix(hex.get(range)?, 16).ok();
    Some(Color::from_rgb_u8(channel(0..2)?, channel(2..4)?, channel(4..6)?))
}

fn color_hex(color: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", color.red(), color.green(), color.blue())
}

fn parse_speeds(text: &str) -> Option<Vec<f32>> {
    let speeds = text
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<f32>().ok().filter(|speed| speed.is_finite() && *speed > 0.0))
        .collect::<Option<Vec<f32>>>()?;
    (!speeds.is_empty()).then_some(speeds)
}

fn speeds_text(speeds: &[f32]) -> String {
    speeds.iter().map(f32::to_string).collect::<Vec<_>>().join(", ")
}

fn parse_window(text: &str) -> Option<WindowGeometry> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [x, y, width, height, maximized] = parts.as_slice() else { return None };
    Some(WindowGeometry {
        x: x.parse().ok()?,
        y: y.parse().ok()?,
        width: width.parse().ok().filter(|width| *width > 0)?,
        height: height.parse().ok().filter(|height| *height > 0)?,
        maximized: *maximized == "yes",
    })
}

#[cfg(windows)]
pub fn config_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("rona"))
}

#[cfg(target_os = "macos")]
pub fn config_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support/rona"))
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("rona"))
}

pub fn load() -> Config {
    let mut config = Config::default();
    let Some(path) = config_dir().map(|dir| dir.join(FILE_NAME)) else { return config };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return config,
        Err(err) => {
            eprintln!("could not read {}: {err}", path.display());
            return config;
        }
    };
    for line in text.lines() {
        if let Some((name, value)) = line.split_once('=') {
            config.set(name.trim(), value.trim());
        }
    }
    config
}

fn save(config: &Config) {
    let Some(dir) = config_dir() else { return };
    let text: String = config.entries().iter().map(|(name, value)| format!("{name} = {value}\n")).collect();
    let result = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(dir.join(FILE_NAME), text));
    if let Err(err) = result {
        eprintln!("could not save the settings to {}: {err}", dir.display());
    }
}

struct MpvDefaults {
    scale: String,
    cscale: String,
    dscale: String,
}

impl MpvDefaults {
    fn read(mpv: &Mpv) -> Self {
        let read = |name: &str| mpv.get_string(name).unwrap_or_default();
        Self { scale: read("scale"), cscale: read("cscale"), dscale: read("dscale") }
    }
}

struct AudioDevice {
    name: String,
    description: String,
}

type AudioDevices = Arc<Mutex<Vec<AudioDevice>>>;

fn read_audio_devices(mpv: &Mpv) -> Vec<AudioDevice> {
    let count = mpv
        .get_string("audio-device-list/count")
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or(0);
    (0..count)
        .filter_map(|index| {
            let name = mpv.get_string(&format!("audio-device-list/{index}/name"))?;
            let description = mpv.get_string(&format!("audio-device-list/{index}/description")).unwrap_or_default();
            Some(AudioDevice { name, description })
        })
        .collect()
}

fn ensure_audio_device(devices: &mut Vec<AudioDevice>, name: &str) {
    if !devices.iter().any(|device| device.name == name) {
        devices.push(AudioDevice { name: name.into(), description: String::new() });
    }
}

fn audio_device_label(device: &AudioDevice) -> SharedString {
    if device.name == AUTO_AUDIO_DEVICE {
        "System default".into()
    } else if device.description.is_empty() {
        device.name.as_str().into()
    } else {
        device.description.as_str().into()
    }
}

fn write_audio_devices(ui: &SettingsUi, devices: &[AudioDevice], current: &str) {
    let labels: Vec<SharedString> = devices.iter().map(audio_device_label).collect();
    ui.set_audio_devices(ModelRc::new(VecModel::from(labels)));
    let index = devices.iter().position(|device| device.name == current).unwrap_or(0);
    ui.set_audio_device(index as i32);
}

fn shortcut_model(config: &Config) -> ModelRc<ShortcutItem> {
    let items: Vec<ShortcutItem> = Action::ALL
        .iter()
        .zip(&config.shortcuts)
        .map(|(action, key)| ShortcutItem {
            action: action.label().into(),
            key: if key.is_empty() { SharedString::new() } else { shortcuts::key_name(key).into() },
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

// A renderer saved on another platform that this one lacks shows as Auto.
fn renderer_index(renderer: usize) -> i32 {
    RENDERER_CHOICES.iter().position(|choice| *choice == renderer).unwrap_or(0) as i32
}

fn write_ui(ui: &SettingsUi, config: &Config, devices: &[AudioDevice]) {
    ui.set_resume(config.resume);
    ui.set_resume_days(config.resume_days.to_string().into());
    ui.set_start_volume(config.start_volume as f32);
    ui.set_end_action(config.end_action as i32);
    ui.set_seek_short(config.seek_short.to_string().into());
    ui.set_seek_long(config.seek_long.to_string().into());
    ui.set_speed_steps(speeds_text(&config.speeds).into());
    ui.set_skip_chapters(config.skip_chapters);
    ui.set_skip_chapter_titles(config.skip_chapter_titles.as_str().into());

    ui.set_renderer(renderer_index(config.renderer));
    ui.set_hardware_decoding(if config.hardware_decoding { 0 } else { 1 });
    ui.set_scaling(config.scaling as i32);
    ui.set_deband(config.deband);
    ui.set_aspect(config.aspect as i32);

    ui.set_audio_languages(config.audio_languages.as_str().into());
    ui.set_volume_max(config.volume_max as i32);
    ui.set_normalize(config.normalize);
    write_audio_devices(ui, devices, &config.audio_device);

    ui.set_subtitle_languages(config.subtitle_languages.as_str().into());
    ui.set_subtitle_auto_load(config.subtitle_auto_load);
    ui.set_skip_matching_subtitles(config.skip_matching_subtitles);
    ui.set_subtitle_font(config.subtitle_font.as_str().into());
    ui.set_subtitle_size(config.subtitle_size as f32);
    ui.set_subtitle_color(parse_color(&config.subtitle_color).unwrap_or_default());
    ui.set_subtitle_border_color(parse_color(&config.subtitle_border_color).unwrap_or_default());
    ui.set_subtitle_raise(config.subtitle_raise as f32);
    ui.set_subtitle_override_styles(config.subtitle_override_styles);

    ui.set_cache_size(config.cache_mib.to_string().into());
    ui.set_read_ahead(config.read_ahead_mib.to_string().into());
    ui.set_connect_timeout(config.connect_timeout.to_string().into());
    ui.set_read_timeout(config.read_timeout.to_string().into());
    ui.set_user_agent(config.user_agent.as_str().into());

    ui.set_on_top_while_playing(config.on_top_while_playing);
    ui.set_remember_window(config.remember_window);
    ui.set_double_click(config.double_click as i32);
    ui.set_hide_controls_after(config.hide_controls_after.to_string().into());
    ui.set_osd_duration(config.osd_duration.to_string().into());

    ui.set_shortcuts(shortcut_model(config));

    ui.set_check_updates(config.check_updates);
}

// Text fields that don't parse keep the last good value, so typing "1." on the way to "1.5" changes nothing.
fn read_ui(ui: &SettingsUi, config: &mut Config, devices: &[AudioDevice]) {
    let index = |value: i32, count: usize| (value.max(0) as usize).min(count - 1);

    config.resume = ui.get_resume();
    set_number(&mut config.resume_days, &ui.get_resume_days(), |days| *days > 0);
    config.start_volume = ui.get_start_volume().round().clamp(0.0, 100.0) as u32;
    config.end_action = index(ui.get_end_action(), END_ACTIONS.len());
    set_number(&mut config.seek_short, &ui.get_seek_short(), is_positive);
    set_number(&mut config.seek_long, &ui.get_seek_long(), is_positive);
    if let Some(speeds) = parse_speeds(&ui.get_speed_steps()) {
        config.speeds = speeds;
    }
    config.skip_chapters = ui.get_skip_chapters();
    config.skip_chapter_titles = ui.get_skip_chapter_titles().trim().into();

    config.renderer = RENDERER_CHOICES[index(ui.get_renderer(), RENDERER_CHOICES.len())];
    config.hardware_decoding = ui.get_hardware_decoding() == 0;
    config.scaling = index(ui.get_scaling(), SCALING_CHOICES.len());
    config.deband = ui.get_deband();
    config.aspect = index(ui.get_aspect(), ASPECT_RATIOS.len());

    config.audio_languages = ui.get_audio_languages().trim().into();
    config.volume_max = index(ui.get_volume_max(), VOLUME_LIMITS.len());
    config.normalize = ui.get_normalize();
    if let Some(device) = devices.get(ui.get_audio_device().max(0) as usize) {
        config.audio_device = device.name.clone();
    }

    config.subtitle_languages = ui.get_subtitle_languages().trim().into();
    config.subtitle_auto_load = ui.get_subtitle_auto_load();
    config.skip_matching_subtitles = ui.get_skip_matching_subtitles();
    config.subtitle_font = ui.get_subtitle_font().trim().into();
    config.subtitle_size =
        (ui.get_subtitle_size().round() as u32).clamp(SUBTITLE_SIZES.0, SUBTITLE_SIZES.1);
    config.subtitle_color = color_hex(ui.get_subtitle_color());
    config.subtitle_border_color = color_hex(ui.get_subtitle_border_color());
    config.subtitle_raise = (ui.get_subtitle_raise().round().max(0.0) as u32).min(MAX_SUBTITLE_RAISE);
    config.subtitle_override_styles = ui.get_subtitle_override_styles();

    set_number(&mut config.cache_mib, &ui.get_cache_size(), |size| *size > 0);
    set_number(&mut config.read_ahead_mib, &ui.get_read_ahead(), |size| *size > 0);
    set_number(&mut config.connect_timeout, &ui.get_connect_timeout(), is_positive);
    set_number(&mut config.read_timeout, &ui.get_read_timeout(), is_positive);
    config.user_agent = ui.get_user_agent().trim().into();

    config.on_top_while_playing = ui.get_on_top_while_playing();
    config.remember_window = ui.get_remember_window();
    config.double_click = index(ui.get_double_click(), DOUBLE_CLICK_CHOICES.len());
    set_number(&mut config.hide_controls_after, &ui.get_hide_controls_after(), is_positive);
    set_number(&mut config.osd_duration, &ui.get_osd_duration(), is_positive);

    config.check_updates = ui.get_check_updates();
}

fn apply(mpv: &Mpv, app: &AppWindow, old: Option<&Config>, new: &Config, defaults: &MpvDefaults) {
    let old_properties = old.map(|old| old.mpv_properties(defaults));
    for (index, (name, value)) in new.mpv_properties(defaults).iter().enumerate() {
        if old_properties.as_ref().is_some_and(|old| old[index].1 == *value) {
            continue;
        }
        log_error(mpv.command(&["set", name, value]));
    }
    if old.is_none_or(|old| old.cache_mib != new.cache_mib || old.network() != new.network()) {
        http_cache::configure(new.cache_mib, new.network());
    }
    app.global::<Player>().set_volume_limit(new.volume_limit());
    update_window_level(app);
}

fn lock_devices(devices: &AudioDevices) -> std::sync::MutexGuard<'_, Vec<AudioDevice>> {
    devices.lock().unwrap_or_else(PoisonError::into_inner)
}

// Reading the device list waits on mpv's core, so it happens off the UI thread.
fn refresh_audio_devices(mpv: &Mpv, app: &AppWindow, devices: &AudioDevices) {
    let (mpv, app_weak, devices) = (mpv.clone(), app.as_weak(), devices.clone());
    std::thread::spawn(move || {
        let found = read_audio_devices(&mpv);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = app_weak.upgrade() else { return };
            let ui = app.global::<SettingsUi>();
            let mut devices = lock_devices(&devices);
            let current = devices
                .get(ui.get_audio_device().max(0) as usize)
                .map(|device| device.name.clone())
                .unwrap_or_else(|| AUTO_AUDIO_DEVICE.into());
            *devices = found;
            ensure_audio_device(&mut devices, &current);
            write_audio_devices(&ui, &devices, &current);
        });
    });
}

pub fn wire(mpv: &Mpv, app: &AppWindow, config: &SharedConfig) {
    let ui = app.global::<SettingsUi>();
    let defaults = Rc::new(MpvDefaults::read(mpv));
    let devices: AudioDevices = Arc::new(Mutex::new(Vec::new()));

    {
        let config = config.borrow();
        let labels: Vec<SharedString> = RENDERER_CHOICES.iter().map(|choice| RENDERER_LABELS[*choice].into()).collect();
        ui.set_renderers(ModelRc::new(VecModel::from(labels)));
        ui.set_renderer_hint(RENDERER_HINT.into());
        ui.set_renderer_at_start(renderer_index(config.renderer));
        ensure_audio_device(&mut lock_devices(&devices), &config.audio_device);
        write_ui(&ui, &config, &lock_devices(&devices));
        log_error(mpv.command(&["set", "volume", &config.start_volume.to_string()]));
        apply(mpv, app, None, &config, &defaults);
    }
    refresh_audio_devices(mpv, app, &devices);

    let (changed_mpv, app_weak, changed_config, changed_defaults, changed_devices) =
        (mpv.clone(), app.as_weak(), config.clone(), defaults.clone(), devices.clone());
    ui.on_changed(move || {
        let Some(app) = app_weak.upgrade() else { return };
        let old = changed_config.borrow().clone();
        let mut new = old.clone();
        read_ui(&app.global::<SettingsUi>(), &mut new, &lock_devices(&changed_devices));
        if new == old {
            return;
        }
        apply(&changed_mpv, &app, Some(&old), &new, &changed_defaults);
        save(&new);
        *changed_config.borrow_mut() = new;
    });

    let (reset_mpv, app_weak, reset_config, reset_defaults, reset_devices) =
        (mpv.clone(), app.as_weak(), config.clone(), defaults.clone(), devices.clone());
    ui.on_reset_page(move |page| {
        let Some(app) = app_weak.upgrade() else { return };
        let old = reset_config.borrow().clone();
        let mut new = old.clone();
        new.reset_page(page);
        let mut devices = lock_devices(&reset_devices);
        ensure_audio_device(&mut devices, &new.audio_device);
        write_ui(&app.global::<SettingsUi>(), &new, &devices);
        apply(&reset_mpv, &app, Some(&old), &new, &reset_defaults);
        save(&new);
        *reset_config.borrow_mut() = new;
    });

    ui.on_valid_speeds(|text| parse_speeds(&text).is_some());

    let (app_weak, bind_config) = (app.as_weak(), config.clone());
    ui.on_bind_key(move |index, text| {
        if !shortcuts::is_bindable(&text) {
            return false;
        }
        let key = shortcuts::normalize(&text);
        let mut config = bind_config.borrow_mut();
        let Some(index) = usize::try_from(index).ok().filter(|index| *index < config.shortcuts.len()) else {
            return true;
        };
        for existing in config.shortcuts.iter_mut().filter(|existing| **existing == key) {
            existing.clear();
        }
        config.shortcuts[index] = key;
        save(&config);
        if let Some(app) = app_weak.upgrade() {
            app.global::<SettingsUi>().set_shortcuts(shortcut_model(&config));
        }
        true
    });

    let (refresh_mpv, app_weak) = (mpv.clone(), app.as_weak());
    ui.on_refresh_audio_devices(move || {
        if let Some(app) = app_weak.upgrade() {
            refresh_audio_devices(&refresh_mpv, &app, &devices);
        }
    });
}

pub fn restore_window(app: &AppWindow, config: &Config) {
    let Some(window) = config.window.filter(|_| config.remember_window) else { return };
    app.window().set_size(PhysicalSize::new(window.width, window.height));
    app.window().set_position(PhysicalPosition::new(window.x, window.y));
    if window.maximized {
        let app_weak = app.as_weak();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = app_weak.upgrade() {
                app.window().set_maximized(true);
            }
        });
    }
}

pub fn remember_window(app: &AppWindow, config: &SharedConfig) {
    let mut config = config.borrow_mut();
    if !config.remember_window || app.global::<WindowControls>().get_fullscreen() {
        return;
    }
    let window = app.window();
    let (position, size) = (window.position(), window.size());
    let current = WindowGeometry {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
        maximized: false,
    };
    config.window = Some(match (window.is_maximized(), config.window) {
        (true, Some(previous)) => WindowGeometry { maximized: true, ..previous },
        (true, None) => WindowGeometry { maximized: true, ..current },
        (false, _) => current,
    });
    save(&config);
}
