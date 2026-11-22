use slint::ComponentHandle;
use slint::platform::Key;

use crate::mpv::Mpv;
use crate::settings::SharedConfig;
use crate::{AppWindow, Player, Recorder, WindowControls, log_error};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    PlayPause,
    SeekBack,
    SeekForward,
    VolumeUp,
    VolumeDown,
    Mute,
    Fullscreen,
    LeaveFullscreen,
    FrameBack,
    FrameForward,
    Next,
    Previous,
    HideSubtitles,
    Record,
}

impl Action {
    pub const ALL: [Action; 14] = [
        Action::PlayPause,
        Action::SeekBack,
        Action::SeekForward,
        Action::VolumeUp,
        Action::VolumeDown,
        Action::Mute,
        Action::Fullscreen,
        Action::LeaveFullscreen,
        Action::FrameBack,
        Action::FrameForward,
        Action::Next,
        Action::Previous,
        Action::HideSubtitles,
        Action::Record,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Action::PlayPause => "Play or pause",
            Action::SeekBack => "Seek back",
            Action::SeekForward => "Seek forward",
            Action::VolumeUp => "Volume up",
            Action::VolumeDown => "Volume down",
            Action::Mute => "Mute",
            Action::Fullscreen => "Fullscreen",
            Action::LeaveFullscreen => "Leave fullscreen",
            Action::FrameBack => "Previous frame",
            Action::FrameForward => "Next frame",
            Action::Next => "Next file",
            Action::Previous => "Previous file",
            Action::HideSubtitles => "Hide or show subtitles",
            Action::Record => "Start or stop recording",
        }
    }

    pub fn config_name(self) -> &'static str {
        match self {
            Action::PlayPause => "play-pause",
            Action::SeekBack => "seek-back",
            Action::SeekForward => "seek-forward",
            Action::VolumeUp => "volume-up",
            Action::VolumeDown => "volume-down",
            Action::Mute => "mute",
            Action::Fullscreen => "fullscreen",
            Action::LeaveFullscreen => "leave-fullscreen",
            Action::FrameBack => "frame-back",
            Action::FrameForward => "frame-forward",
            Action::Next => "next",
            Action::Previous => "previous",
            Action::HideSubtitles => "hide-subtitles",
            Action::Record => "record",
        }
    }

    pub fn default_key(self) -> String {
        match self {
            Action::PlayPause => key_char(Key::Space),
            Action::SeekBack => key_char(Key::LeftArrow),
            Action::SeekForward => key_char(Key::RightArrow),
            Action::VolumeUp => key_char(Key::UpArrow),
            Action::VolumeDown => key_char(Key::DownArrow),
            Action::Mute => "m".into(),
            Action::Fullscreen => "f".into(),
            Action::LeaveFullscreen => key_char(Key::Escape),
            Action::FrameBack => ",".into(),
            Action::FrameForward => ".".into(),
            Action::Next => "n".into(),
            Action::Previous => "p".into(),
            Action::HideSubtitles => "v".into(),
            Action::Record => "r".into(),
        }
    }
}

fn key_char(key: Key) -> String {
    char::from(key).to_string()
}

const NAMED_KEYS: [(&str, Key); 27] = [
    ("Space", Key::Space),
    ("Left", Key::LeftArrow),
    ("Right", Key::RightArrow),
    ("Up", Key::UpArrow),
    ("Down", Key::DownArrow),
    ("Esc", Key::Escape),
    ("Enter", Key::Return),
    ("Tab", Key::Tab),
    ("Backspace", Key::Backspace),
    ("Delete", Key::Delete),
    ("Insert", Key::Insert),
    ("Home", Key::Home),
    ("End", Key::End),
    ("PageUp", Key::PageUp),
    ("PageDown", Key::PageDown),
    ("F1", Key::F1),
    ("F2", Key::F2),
    ("F3", Key::F3),
    ("F4", Key::F4),
    ("F5", Key::F5),
    ("F6", Key::F6),
    ("F7", Key::F7),
    ("F8", Key::F8),
    ("F9", Key::F9),
    ("F10", Key::F10),
    ("F11", Key::F11),
    ("F12", Key::F12),
];

const MODIFIERS: [Key; 9] = [
    Key::Shift,
    Key::ShiftR,
    Key::Control,
    Key::ControlR,
    Key::Alt,
    Key::AltGr,
    Key::Meta,
    Key::MetaR,
    Key::CapsLock,
];

/// Shift turns letters uppercase, so letters are stored and compared in lowercase.
pub fn normalize(text: &str) -> String {
    text.to_lowercase()
}

pub fn is_bindable(text: &str) -> bool {
    !text.is_empty() && !MODIFIERS.iter().any(|modifier| key_char(*modifier) == text)
}

/// The name shown in the UI and written to the settings file.
pub fn key_name(text: &str) -> String {
    NAMED_KEYS
        .iter()
        .find(|(_, key)| key_char(*key) == text)
        .map(|(name, _)| name.to_string())
        .unwrap_or_else(|| text.to_uppercase())
}

pub fn key_from_name(name: &str) -> Option<String> {
    if let Some((_, key)) = NAMED_KEYS.iter().find(|(known, _)| known.eq_ignore_ascii_case(name)) {
        return Some(key_char(*key));
    }
    let mut characters = name.chars();
    match (characters.next(), characters.next()) {
        (Some(character), None) => Some(normalize(&character.to_string())),
        _ => None,
    }
}

pub fn handle(text: &str, shift: bool, mpv: &Mpv, app: &AppWindow, config: &SharedConfig) -> bool {
    let pressed = normalize(text);
    let (action, seek_step, seek_mode) = {
        let config = config.borrow();
        let Some(index) = config.shortcuts.iter().position(|key| !key.is_empty() && *key == pressed) else {
            return false;
        };
        let step = if shift { config.seek_long } else { config.seek_short };
        let mode = if config.seek_keyframes { "relative+keyframes" } else { "relative+exact" };
        (Action::ALL[index], step, mode)
    };

    crate::logging::debug!("key {pressed:?}{} -> {action:?}", if shift { " with shift" } else { "" });
    let player = app.global::<Player>();
    let controls = app.global::<WindowControls>();
    match action {
        Action::PlayPause => player.invoke_toggle_pause(),
        Action::SeekBack => log_error(mpv.command(&["seek", &(-seek_step).to_string(), seek_mode])),
        Action::SeekForward => log_error(mpv.command(&["seek", &seek_step.to_string(), seek_mode])),
        Action::VolumeUp => player.invoke_change_volume(0.05),
        Action::VolumeDown => player.invoke_change_volume(-0.05),
        Action::Mute => player.invoke_toggle_mute(),
        Action::Fullscreen => controls.invoke_toggle_fullscreen(),
        Action::LeaveFullscreen => {
            if !controls.get_fullscreen() {
                return false;
            }
            controls.invoke_toggle_fullscreen();
        }
        Action::FrameBack => log_error(mpv.command(&["frame-back-step"])),
        Action::FrameForward => log_error(mpv.command(&["frame-step"])),
        Action::Next => player.invoke_next(),
        Action::Previous => player.invoke_previous(),
        Action::HideSubtitles => player.invoke_toggle_subtitles(),
        Action::Record => app.global::<Recorder>().invoke_toggle_recording(),
    }
    true
}
