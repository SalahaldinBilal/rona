use std::cell::RefCell;
use std::rc::Rc;

use slint::{ComponentHandle, PhysicalPosition, PhysicalSize};

use crate::settings::{self, set_number, yes_no};
use crate::{AppWindow, WindowControls};

pub type SharedState = Rc<RefCell<State>>;

const FILE_NAME: &str = "state.conf";

#[derive(Clone, Copy, Debug, PartialEq)]
struct WindowGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    maximized: bool,
}

/// What Rona remembers on its own, kept apart from the settings the user chooses.
#[derive(Debug, PartialEq)]
pub struct State {
    // The last volume used for each kind of file.
    pub video_volume: u32,
    pub audio_volume: u32,
    window: Option<WindowGeometry>,
}

impl Default for State {
    fn default() -> Self {
        Self { video_volume: 100, audio_volume: 100, window: None }
    }
}

impl State {
    fn entries(&self) -> Vec<(String, String)> {
        let mut entries = vec![
            ("video-volume".to_string(), self.video_volume.to_string()),
            ("audio-volume".to_string(), self.audio_volume.to_string()),
        ];
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
            // Written by older versions, it seeds both remembered levels.
            "start-volume" => {
                set_number(&mut self.video_volume, value, is_volume);
                set_number(&mut self.audio_volume, value, is_volume);
            }
            "video-volume" => set_number(&mut self.video_volume, value, is_volume),
            "audio-volume" => set_number(&mut self.audio_volume, value, is_volume),
            "window" => self.window = parse_window(value).or(self.window),
            _ => {}
        }
    }
}

// The highest volume limit is 200%.
fn is_volume(volume: &u32) -> bool {
    *volume <= 200
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

/// Versions up to 0.1.1 kept these values in settings.conf, so it is read first and state.conf wins.
pub fn load() -> State {
    let mut state = State::default();
    for file_name in [settings::FILE_NAME, FILE_NAME] {
        settings::read_file(file_name, |name, value| state.set(name, value));
    }
    state
}

pub fn save(state: &State) {
    settings::write_file(FILE_NAME, &state.entries());
}

pub fn restore_window(app: &AppWindow, state: &State) {
    let Some(window) = state.window else { return };
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

pub fn remember_window(app: &AppWindow, state: &SharedState) {
    if app.global::<WindowControls>().get_fullscreen() {
        return;
    }
    let mut state = state.borrow_mut();
    let window = app.window();
    let (position, size) = (window.position(), window.size());
    let current = WindowGeometry {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
        maximized: false,
    };
    state.window = Some(match (window.is_maximized(), state.window) {
        (true, Some(previous)) => WindowGeometry { maximized: true, ..previous },
        (true, None) => WindowGeometry { maximized: true, ..current },
        (false, _) => current,
    });
    save(&state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_entries_load_back_to_the_same_state() {
        let state = State {
            video_volume: 80,
            audio_volume: 135,
            window: Some(WindowGeometry { x: -8, y: 40, width: 1280, height: 720, maximized: true }),
        };
        let mut loaded = State::default();
        for (name, value) in state.entries() {
            loaded.set(&name, &value);
        }
        assert_eq!(loaded, state);
    }

    #[test]
    fn old_start_volume_seeds_both_levels() {
        let mut state = State::default();
        state.set("start-volume", "60");
        assert_eq!((state.video_volume, state.audio_volume), (60, 60));
    }
}
