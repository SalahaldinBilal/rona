use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use slint::{Timer, TimerMode};

use crate::media;
use crate::mpv::Mpv;
use crate::settings::{self, SharedConfig};
use crate::log_error;

// Turning the wheel changes the volume many times a second, so the file is written once it stops.
const SAVE_DELAY: Duration = Duration::from_secs(1);

struct Volume {
    mpv: Mpv,
    config: SharedConfig,
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

pub fn wire(mpv: &Mpv, config: &SharedConfig) {
    let volume = Volume { mpv: mpv.clone(), config: config.clone(), audio: Cell::new(false), save_timer: Timer::default() };
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
            let mut config = volume.config.borrow_mut();
            if volume.audio.get() {
                config.audio_volume = percent;
            } else {
                config.video_volume = percent;
            }
        }
        let config = volume.config.clone();
        volume.save_timer.start(TimerMode::SingleShot, SAVE_DELAY, move || settings::save(&config.borrow()));
    });
}

/// For closing, when the delayed save might not get to run.
pub fn save_pending() {
    with_volume(|volume| {
        if volume.save_timer.running() {
            volume.save_timer.stop();
            settings::save(&volume.config.borrow());
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
            let config = self.config.borrow();
            if self.audio.get() { config.audio_volume } else { config.video_volume }
        };
        log_error(self.mpv.set_property("volume", &level.to_string()));
    }
}
