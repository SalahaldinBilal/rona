use std::ffi::{CStr, c_void};

use glow::HasContext;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::Interface;

const WGL_ACCESS_READ_ONLY_NV: u32 = 0;

type OpenDevice = unsafe extern "system" fn(*mut c_void) -> *mut c_void;
type CloseDevice = unsafe extern "system" fn(*mut c_void) -> i32;
type RegisterObject = unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u32, u32) -> *mut c_void;
type UnregisterObject = unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32;
type LockObjects = unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> i32;

/// WGL_NV_DX_interop2, which lets OpenGL read a Direct3D 11 texture without copying it.
struct Interop {
    device: *mut c_void,
    close_device: CloseDevice,
    register_object: RegisterObject,
    unregister_object: UnregisterObject,
    lock_objects: LockObjects,
    unlock_objects: LockObjects,
}

struct SharedTexture {
    d3d: ID3D11Texture2D,
    gl: glow::NativeTexture,
    handle: *mut c_void,
}

pub struct D3d11Target {
    device: ID3D11Device,
    interop: Interop,
    shared: Option<SharedTexture>,
}

impl D3d11Target {
    pub fn new(get_proc_address: &dyn Fn(&CStr) -> *const c_void) -> Result<Self, String> {
        let load = |name: &CStr| {
            let function = get_proc_address(name);
            if function.is_null() {
                Err(format!("{} is not available", name.to_string_lossy()))
            } else {
                Ok(function)
            }
        };
        let open_device: OpenDevice = unsafe { std::mem::transmute(load(c"wglDXOpenDeviceNV")?) };
        let close_device: CloseDevice = unsafe { std::mem::transmute(load(c"wglDXCloseDeviceNV")?) };
        let register_object: RegisterObject = unsafe { std::mem::transmute(load(c"wglDXRegisterObjectNV")?) };
        let unregister_object: UnregisterObject =
            unsafe { std::mem::transmute(load(c"wglDXUnregisterObjectNV")?) };
        let lock_objects: LockObjects = unsafe { std::mem::transmute(load(c"wglDXLockObjectsNV")?) };
        let unlock_objects: LockObjects = unsafe { std::mem::transmute(load(c"wglDXUnlockObjectsNV")?) };

        let mut device = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }
        .map_err(|err| format!("creating the Direct3D 11 device failed: {err}"))?;
        let device = device.ok_or("creating the Direct3D 11 device returned nothing")?;

        let interop_device = unsafe { open_device(device.as_raw()) };
        if interop_device.is_null() {
            return Err("wglDXOpenDeviceNV failed".into());
        }
        let interop = Interop {
            device: interop_device,
            close_device,
            register_object,
            unregister_object,
            lock_objects,
            unlock_objects,
        };
        Ok(Self { device, interop, shared: None })
    }

    pub fn device_ptr(&self) -> *mut c_void {
        self.device.as_raw()
    }

    pub fn texture_ptr(&self) -> Option<*mut c_void> {
        self.shared.as_ref().map(|shared| shared.d3d.as_raw())
    }

    pub unsafe fn resize(
        &mut self,
        gl: &glow::Context,
        framebuffer: glow::NativeFramebuffer,
        size: (i32, i32),
    ) -> Result<(), String> {
        unsafe { self.release(gl) };

        let description = D3D11_TEXTURE2D_DESC {
            Width: size.0 as u32,
            Height: size.1 as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe { self.device.CreateTexture2D(&description, None, Some(&mut texture)) }
            .map_err(|err| format!("creating the video texture failed: {err}"))?;
        let d3d = texture.ok_or("creating the video texture returned nothing")?;

        let gl_texture = unsafe { gl.create_texture() }?;
        let handle = unsafe {
            (self.interop.register_object)(
                self.interop.device,
                d3d.as_raw(),
                gl_texture.0.get(),
                glow::TEXTURE_2D,
                WGL_ACCESS_READ_ONLY_NV,
            )
        };
        if handle.is_null() {
            unsafe { gl.delete_texture(gl_texture) };
            return Err("wglDXRegisterObjectNV failed".into());
        }
        self.shared = Some(SharedTexture { d3d, gl: gl_texture, handle });

        if !self.lock() {
            return Err("wglDXLockObjectsNV failed".into());
        }
        let status = unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(gl_texture),
                0,
            );
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            status
        };
        self.unlock();
        if status != glow::FRAMEBUFFER_COMPLETE {
            return Err(format!("video framebuffer is incomplete (status {status:#x})"));
        }
        Ok(())
    }

    // OpenGL may only read the shared texture while it is locked, which also waits for Direct3D to finish.
    pub fn lock(&self) -> bool {
        let Some(shared) = &self.shared else { return false };
        let mut handle = shared.handle;
        unsafe { (self.interop.lock_objects)(self.interop.device, 1, &mut handle) != 0 }
    }

    pub fn unlock(&self) {
        let Some(shared) = &self.shared else { return };
        let mut handle = shared.handle;
        unsafe { (self.interop.unlock_objects)(self.interop.device, 1, &mut handle) };
    }

    unsafe fn release(&mut self, gl: &glow::Context) {
        if let Some(shared) = self.shared.take() {
            unsafe {
                (self.interop.unregister_object)(self.interop.device, shared.handle);
                gl.delete_texture(shared.gl);
            }
        }
    }

    pub unsafe fn destroy(&mut self, gl: &glow::Context) {
        unsafe {
            self.release(gl);
            (self.interop.close_device)(self.interop.device);
        }
    }
}
