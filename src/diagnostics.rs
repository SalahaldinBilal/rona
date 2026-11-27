use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slint::winit_030::winit::event::{ElementState, WindowEvent};
use slint::{Timer, TimerMode};

use crate::logging::{debug, info, warn};

const TICK: Duration = Duration::from_secs(1);
const STALL_AFTER: Duration = Duration::from_secs(3);

static START: OnceLock<Instant> = OnceLock::new();
static LAST_TICK_MS: AtomicU64 = AtomicU64::new(0);
static FRAMES: AtomicU64 = AtomicU64::new(0);
static FRAMES_AT_TICK: AtomicU64 = AtomicU64::new(0);
static STOPPED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static HEARTBEAT: Timer = Timer::default();
}

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// A UI thread timer and a thread that watches it, so a stuck UI thread shows in the log.
pub fn start() {
    LAST_TICK_MS.store(now_ms(), Ordering::Relaxed);
    HEARTBEAT.with(|timer| {
        timer.start(TimerMode::Repeated, TICK, || {
            FRAMES_AT_TICK.store(FRAMES.load(Ordering::Relaxed), Ordering::Relaxed);
            LAST_TICK_MS.store(now_ms(), Ordering::Relaxed);
        });
    });
    if let Err(err) = std::thread::Builder::new().name("diagnostics".into()).spawn(watch) {
        warn!("could not start the diagnostics thread: {err}");
    }
}

/// The event loop is ending, so its timers stop on purpose.
pub fn stop() {
    STOPPED.store(true, Ordering::Relaxed);
}

pub fn frame_drawn() {
    FRAMES.fetch_add(1, Ordering::Relaxed);
}

pub fn window_event(window: &str, event: &WindowEvent) {
    match event {
        WindowEvent::Focused(focused) => info!("{window} {}", if *focused { "focused" } else { "lost focus" }),
        WindowEvent::Occluded(occluded) => info!("{window} {}", if *occluded { "occluded" } else { "no longer occluded" }),
        WindowEvent::CursorEntered { .. } => debug!("pointer entered the {window}"),
        WindowEvent::CursorLeft { .. } => debug!("pointer left the {window}"),
        WindowEvent::MouseInput { state, button, .. } => debug!(
            "mouse {button:?} {} in the {window}",
            if *state == ElementState::Pressed { "pressed" } else { "released" }
        ),
        WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed && !event.repeat => {
            debug!("key down in the {window}");
        }
        _ => {}
    }
}

fn watch() {
    // When the timers stopped, and how many frames had been drawn by then.
    let mut stall: Option<(u64, u64)> = None;
    #[cfg(windows)]
    let mut desktop = desktop::Watch::default();
    while !STOPPED.load(Ordering::Relaxed) {
        std::thread::sleep(TICK);
        if STOPPED.load(Ordering::Relaxed) {
            break;
        }
        let (now, last) = (now_ms(), LAST_TICK_MS.load(Ordering::Relaxed));
        match stall {
            None if now.saturating_sub(last) >= STALL_AFTER.as_millis() as u64 => {
                let frames = FRAMES.load(Ordering::Relaxed) - FRAMES_AT_TICK.load(Ordering::Relaxed);
                warn!("UI thread timers stopped {:.1} s ago, {frames} frames drawn since", (now - last) as f64 / 1000.0);
                stall = Some((last, FRAMES_AT_TICK.load(Ordering::Relaxed)));
            }
            Some((since, frames_then)) if last > since => {
                let frames = FRAMES_AT_TICK.load(Ordering::Relaxed).saturating_sub(frames_then);
                info!(
                    "UI thread timers ran again after {:.1} s, {frames} frames drawn while they were stopped",
                    (last - since) as f64 / 1000.0
                );
                stall = None;
            }
            _ => {}
        }
        #[cfg(windows)]
        desktop.check();
    }
}

#[cfg(windows)]
pub use desktop::{set_main_window, set_recorder_window};

// Which window has focus, which one is under the pointer in fullscreen, and how the main window sits.
#[cfg(windows)]
mod desktop {
    use std::sync::atomic::{AtomicIsize, Ordering};

    use windows::Win32::Foundation::{CloseHandle, HWND, POINT, RECT};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GA_ROOT, GWL_EXSTYLE, GWL_STYLE, GetAncestor, GetClassNameW, GetCursorPos, GetForegroundWindow,
        GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible, WS_CAPTION,
        WS_EX_TOPMOST, WindowFromPoint,
    };
    use windows::core::PWSTR;

    use crate::logging::{debug, info};

    static MAIN: AtomicIsize = AtomicIsize::new(0);
    static RECORDER: AtomicIsize = AtomicIsize::new(0);

    pub fn set_main_window(hwnd: HWND) {
        MAIN.store(hwnd.0 as isize, Ordering::Relaxed);
    }

    pub fn set_recorder_window(hwnd: HWND) {
        RECORDER.store(hwnd.0 as isize, Ordering::Relaxed);
    }

    #[derive(Default)]
    pub struct Watch {
        foreground: Option<isize>,
        pointer: Option<isize>,
        main: Option<String>,
    }

    impl Watch {
        pub fn check(&mut self) {
            let main = MAIN.load(Ordering::Relaxed);
            if main == 0 {
                return;
            }
            let main_hwnd = HWND(main as *mut _);

            let foreground = unsafe { GetForegroundWindow() }.0 as isize;
            if self.foreground != Some(foreground) {
                self.foreground = Some(foreground);
                info!("in front: {}", describe(foreground));
            }

            let state = main_state(main_hwnd);
            if self.main.as_ref() != Some(&state) {
                debug!("main window: {state}");
                self.main = Some(state);
            }

            // Only in fullscreen, where clicks landing on another program would mean the picture is stale.
            if !is_fullscreen(main_hwnd) {
                self.pointer = None;
                return;
            }
            let mut point = POINT::default();
            if unsafe { GetCursorPos(&mut point) }.is_err() {
                return;
            }
            let under = unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) }.0 as isize;
            if self.pointer != Some(under) {
                self.pointer = Some(under);
                info!("pointer over: {}", describe(under));
            }
        }
    }

    fn is_fullscreen(hwnd: HWND) -> bool {
        (unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32) & WS_CAPTION.0 == 0
    }

    fn main_state(hwnd: HWND) -> String {
        let mut rect = RECT::default();
        let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
        let topmost = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0 != 0;
        format!(
            "{}x{} at {},{}, fullscreen {}, on top {}, minimized {}, visible {}",
            rect.right - rect.left,
            rect.bottom - rect.top,
            rect.left,
            rect.top,
            is_fullscreen(hwnd),
            topmost,
            unsafe { IsIconic(hwnd) }.as_bool(),
            unsafe { IsWindowVisible(hwnd) }.as_bool(),
        )
    }

    // Program and window class only, since window titles can hold anything.
    fn describe(hwnd: isize) -> String {
        if hwnd == 0 {
            return "no window".into();
        }
        if hwnd == MAIN.load(Ordering::Relaxed) {
            return "Rona's main window".into();
        }
        if hwnd == RECORDER.load(Ordering::Relaxed) {
            return "Rona's recorder window".into();
        }
        let window = HWND(hwnd as *mut _);
        let mut class = [0u16; 128];
        let length = unsafe { GetClassNameW(window, &mut class) }.max(0) as usize;
        let class = String::from_utf16_lossy(&class[..length]);
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        if pid == std::process::id() {
            return format!("another Rona window ({class})");
        }
        format!("{} (process {pid}, {class})", program_name(pid).unwrap_or_else(|| "unknown program".into()))
    }

    fn program_name(pid: u32) -> Option<String> {
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let mut path = [0u16; 1024];
        let mut length = path.len() as u32;
        let result =
            unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut length) };
        let _ = unsafe { CloseHandle(process) };
        result.ok()?;
        let path = String::from_utf16_lossy(&path[..length as usize]);
        path.rsplit(['\\', '/']).next().map(str::to_string)
    }
}
