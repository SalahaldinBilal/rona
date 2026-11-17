use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use slint::{Timer, TimerMode};

use crate::media;
use crate::mpv::Mpv;
use crate::state::{self, SharedState};
use crate::log_error;

// Turning the wheel changes the volume many times a second, so the file is written once it stops.
const SAVE_DELAY: Duration = Duration::from_secs(1);

struct Volume {
    mpv: Mpv,
    state: SharedState,
    // Whether the playing file uses the audio level rather than the video one.
    audio: Cell<bool>,
    save_timer: Timer,
}

thread_local! {
    static VOLUME: RefCell<Option<Rc<Volume>>> = const { RefCell::new(None) };
}

fn with_volume(action: impl FnOnce(&Volume)) {
    let volume = VOLUME.with(|slot| slot.borrow().clone());
    if let Some(volume) = volume {
        action(&volume);
    }
}

pub fn wire(mpv: &Mpv, state: &SharedState) {
    let volume = Volume { mpv: mpv.clone(), state: state.clone(), audio: Cell::new(false), save_timer: Timer::default() };
    volume.apply();
    VOLUME.with(|slot| *slot.borrow_mut() = Some(Rc::new(volume)));
}

/// Runs in mpv's on_load hook, so the level is set before any sound plays. Tracks aren't known yet,
/// so the file name decides until `file_playing` knows better.
pub fn file_loading(path: &str) {
    with_volume(|volume| volume.use_level(media::is_audio_name(path)));
}

/// Corrects the guess from the file name once the tracks are known, like audio in a .webm.
pub fn file_playing(audio_only: bool) {
    with_volume(|volume| {
        if volume.audio.get() != audio_only {
            volume.use_level(audio_only);
        }
    });
}

/// The user set the volume in the player, so it becomes the level for this kind of file.
pub fn remember(percent: u32) {
    with_volume(|volume| {
        {
            let mut state = volume.state.borrow_mut();
            if volume.audio.get() {
                state.audio_volume = percent;
            } else {
                state.video_volume = percent;
            }
        }
        let state = volume.state.clone();
        volume.save_timer.start(TimerMode::SingleShot, SAVE_DELAY, move || state::save(&state.borrow()));
    });
}

/// For closing, when the delayed save might not get to run.
pub fn save_pending() {
    with_volume(|volume| {
        if volume.save_timer.running() {
            volume.save_timer.stop();
            state::save(&volume.state.borrow());
        }
    });
}

impl Volume {
    fn use_level(&self, audio: bool) {
        self.audio.set(audio);
        self.apply();
    }

    fn apply(&self) {
        let level = {
            let state = self.state.borrow();
            if self.audio.get() { state.audio_volume } else { state.video_volume }
        };
        log_error(self.mpv.set_property("volume", &level.to_string()));
    }
}
