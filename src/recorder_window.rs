use std::cell::{Cell, RefCell};

use slint::winit_030::winit::event::{ElementState, WindowEvent};
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{CloseRequestResponse, ComponentHandle, PhysicalPosition, RenderingState};

use crate::logging::{error, info};
use crate::mpv::Mpv;
use crate::settings::SharedConfig;
use crate::{
    AppWindow, Panels, Recorder as RecorderUi, RecorderWindow, WindowControls, log_error, log_window_error, recorder,
    shortcuts, theme,
};

// The width ui/recorder-window.slint fixes the window to.
const WIDTH: f32 = 320.0;
const GAP: i32 = 8;

thread_local! {
    static WINDOW: RefCell<Option<RecorderWindow>> = const { RefCell::new(None) };
    static OPEN: Cell<bool> = const { Cell::new(false) };
    // Where the window was when it was last closed, so it opens there again.
    static PLACE: Cell<Option<PhysicalPosition>> = const { Cell::new(None) };
}

/// Created hidden at startup. It shows while the recorder is open and the Recorder setting is Window.
pub fn create(app: &AppWindow, mpv: &Mpv, config: &SharedConfig) -> Result<(), slint::PlatformError> {
    let window = RecorderWindow::new()?;
    theme::mirror_to(&window, app);

    let app_weak = app.as_weak();
    window.on_dismiss(move || close_from_window(&app_weak));
    let app_weak = app.as_weak();
    window.window().on_close_requested(move || {
        close_from_window(&app_weak);
        CloseRequestResponse::KeepWindowShown
    });

    let (app_weak, key_mpv, key_config) = (app.as_weak(), mpv.clone(), config.clone());
    window.on_key_pressed(move |text, shift| {
        let Some(app) = app_weak.upgrade() else { return false };
        shortcuts::handle(&text, shift, &key_mpv, &app, &key_config)
    });

    let controls = window.global::<WindowControls>();
    let window_weak = window.as_weak();
    controls.on_start_drag(move || {
        if let Some(window) = window_weak.upgrade() {
            log_window_error(window.window().with_winit_window(|winit| winit.drag_window()));
        }
    });

    // As in the main window, any click takes focus from a text field.
    let window_weak = window.as_weak();
    window.window().on_winit_window_event(move |_, event| {
        crate::diagnostics::window_event("recorder window", event);
        if let WindowEvent::MouseInput { state: ElementState::Pressed, .. } = event
            && let Some(window) = window_weak.upgrade()
        {
            window.invoke_release_text_focus();
        }
        EventResult::Propagate
    });

    // The native window only exists once shown, and is set up each time it's created.
    let (window_weak, app_weak) = (window.as_weak(), app.as_weak());
    let notifier = window.window().set_rendering_notifier(move |state, _| {
        if !matches!(state, RenderingState::RenderingSetup) {
            return;
        }
        let (window_weak, app_weak) = (window_weak.clone(), app_weak.clone());
        let _ = slint::invoke_from_event_loop(move || {
            if let (Some(window), Some(app)) = (window_weak.upgrade(), app_weak.upgrade()) {
                attach_to_main_window(&window, &app);
            }
        });
    });
    if let Err(err) = notifier {
        error!("could not watch the recorder window: {err:?}");
    }

    let app_weak = app.as_weak();
    app.global::<Panels>().on_recorder_window_changed(move |open| {
        if let Some(app) = app_weak.upgrade() {
            if open { show(&app) } else { hide(&app) }
        }
    });

    WINDOW.with_borrow_mut(|slot| *slot = Some(window));
    Ok(())
}

/// Runs `set` on the recorder window's Recorder global, shown or not.
pub fn with_ui(set: impl Fn(&RecorderUi)) {
    WINDOW.with_borrow(|window| {
        if let Some(window) = window {
            set(&window.global::<RecorderUi>());
        }
    });
}

/// Reads from the recorder window while it's open, else from the main window's sidebar.
pub fn read<T>(app: &AppWindow, get: impl FnOnce(&RecorderUi) -> T) -> T {
    WINDOW.with_borrow(|window| match window.as_ref().filter(|_| OPEN.get()) {
        Some(window) => get(&window.global::<RecorderUi>()),
        None => get(&app.global::<RecorderUi>()),
    })
}

/// The main window is closing, and the event loop only ends once no window is shown.
pub fn close_for_exit() {
    WINDOW.with_borrow(|window| {
        if let Some(window) = window {
            log_error(window.hide());
        }
    });
    OPEN.set(false);
}

// Closing goes through the main window's flag, so the recorder button there stays in step.
fn close_from_window(app_weak: &slint::Weak<AppWindow>) {
    if let Some(app) = app_weak.upgrade() {
        hide(&app);
        app.global::<Panels>().set_recorder_open(false);
    }
}

fn show(app: &AppWindow) {
    if OPEN.get() {
        return;
    }
    WINDOW.with_borrow(|window| {
        let Some(window) = window else { return };
        recorder::copy_fields(&app.global::<RecorderUi>(), &window.global::<RecorderUi>());
        match PLACE.get() {
            Some(place) => window.window().set_position(place),
            None => place_beside(window, app),
        }
        log_error(window.show());
    });
    OPEN.set(true);
    info!("recorder window shown");
}

// The window's fields go back to the sidebar's, which hold them while the window is closed.
fn hide(app: &AppWindow) {
    if !OPEN.get() {
        return;
    }
    WINDOW.with_borrow(|window| {
        let Some(window) = window else { return };
        PLACE.set(Some(window.window().position()));
        recorder::copy_fields(&window.global::<RecorderUi>(), &app.global::<RecorderUi>());
        log_error(window.hide());
    });
    OPEN.set(false);
    info!("recorder window closed");
}

// The first time, right of the main window, or inside its right edge when the screen ends there.
fn place_beside(window: &RecorderWindow, app: &AppWindow) {
    let main = app.window();
    let (position, size) = (main.position(), main.size());
    let width = (WIDTH * main.scale_factor()).round() as i32;
    let main_right = position.x + size.width as i32;
    let screen_right = main
        .with_winit_window(|winit| winit.current_monitor())
        .flatten()
        .map(|monitor| monitor.position().x + monitor.size().width as i32);
    let x = match screen_right {
        Some(right) if main_right + GAP + width > right => main_right - width - GAP,
        _ => main_right + GAP,
    };
    window.window().set_position(PhysicalPosition::new(x, position.y));
}

// Owned by the main window, it stays above it, minimizes with it and has no taskbar button of its own.
#[cfg(windows)]
fn attach_to_main_window(window: &RecorderWindow, app: &AppWindow) {
    use slint::winit_030::winit::platform::windows::WindowExtWindows;
    use windows::Win32::UI::WindowsAndMessaging::{GWLP_HWNDPARENT, SetWindowLongPtrW};

    crate::enable_window_shadow(window.window());
    window.window().with_winit_window(|winit| winit.set_skip_taskbar(true));
    if let (Some(owned), Some(owner)) = (crate::window_hwnd(window.window()), crate::window_hwnd(app.window())) {
        crate::diagnostics::set_recorder_window(owned);
        crate::hook_recorder_window(owned);
        unsafe { SetWindowLongPtrW(owned, GWLP_HWNDPARENT, owner.0 as isize) };
    }
}

#[cfg(not(windows))]
fn attach_to_main_window(_window: &RecorderWindow, _app: &AppWindow) {}
