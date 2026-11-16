use std::error::Error;
use std::ffi::c_void;
use std::ptr::NonNull;

use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::{self, XdgActivationV1};

struct Activation;

impl Dispatch<WlRegistry, GlobalListContents> for Activation {
    fn event(_: &mut Self, _: &WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<XdgActivationV1, ()> for Activation {
    fn event(_: &mut Self, _: &XdgActivationV1, _: xdg_activation_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

/// winit's focus_window does nothing on Wayland, so this asks the compositor itself, with the token from the launch.
pub fn activate(display: NonNull<c_void>, surface: NonNull<c_void>, token: &str) -> Result<(), Box<dyn Error>> {
    let connection = Connection::from_backend(unsafe { Backend::from_foreign_display(display.as_ptr().cast()) });
    let surface_id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.as_ptr().cast()) }?;
    let surface = WlSurface::from_id(&connection, surface_id)?;
    let (globals, queue) = registry_queue_init::<Activation>(&connection)?;
    let activation: XdgActivationV1 = globals.bind(&queue.handle(), 1..=1, ())?;
    activation.activate(token.to_string(), &surface);
    activation.destroy();
    connection.flush()?;
    Ok(())
}
