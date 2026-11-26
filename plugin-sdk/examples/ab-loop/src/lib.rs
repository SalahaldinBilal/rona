use rona_plugin::host::{self, ControlState};
use rona_plugin::{Event, HookResult, export_hooks};

const STATE_KEY: &str = "state";

// 0 no loop, 1 start set, 2 looping. Kept in storage, since a plugin instance can be rebuilt between calls.
fn state() -> u8 {
    host::storage::get_text(STATE_KEY).ok().flatten().and_then(|text| text.parse().ok()).unwrap_or(0)
}

fn time_text(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn tell(text: &str) {
    let wanted = host::setting::<bool>("messages").ok().flatten().unwrap_or(true);
    // Without ui.osd the plugin still works, it just doesn't say anything.
    if let (true, Some(osd)) = (wanted, host::osd()) {
        let _ = osd.show(text);
    }
}

fn show_button(state: u8) {
    if let Some(controls) = host::controls() {
        let _ = controls.set("loop", &ControlState { active: Some(state > 0), dot: Some(state == 1), ..ControlState::default() });
    }
}

fn step() -> HookResult<()> {
    let (Some(playback), Some(mpv)) = (host::playback(), host::mpv_write()) else {
        host::log(host::LogLevel::Warn, "Needs playback.read and mpv.write to set a loop");
        tell("A-B loop needs its permissions back to work");
        return Ok(());
    };
    let position = playback.state()?.position;
    let next = match state() {
        0 => {
            mpv.set("ab-loop-a", &position.to_string())?;
            mpv.set("ab-loop-b", "no")?;
            tell(&format!("Loop from {}", time_text(position)));
            1
        }
        1 => {
            mpv.set("ab-loop-b", &position.to_string())?;
            tell(&format!("Looping up to {}", time_text(position)));
            2
        }
        _ => {
            mpv.set("ab-loop-a", "no")?;
            mpv.set("ab-loop-b", "no")?;
            tell("Loop off");
            0
        }
    };
    host::storage::set(STATE_KEY, next.to_string().as_bytes())?;
    show_button(next);
    Ok(())
}

fn event(event: Event) -> HookResult<()> {
    match event {
        Event::Control { id, button } if id == "loop" && button == "left" => step(),
        Event::Action { id } if id == "set-point" => step(),
        // A new file starts without a loop.
        Event::FileStarted { .. } | Event::Started => {
            host::storage::set(STATE_KEY, b"0")?;
            show_button(0);
            Ok(())
        }
        _ => Ok(()),
    }
}

export_hooks! {
    on_event => event,
}
