use std::ffi::{CStr, c_void};
use std::time::{Duration, Instant};

use glow::HasContext;

use crate::mpv::{Mpv, RenderContext};

const RESIZE_SETTLE_DELAY: Duration = Duration::from_millis(1);

#[derive(Clone, Copy)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Where mpv placed the picture in a buffer of size `buffer`, as margins from each edge.
#[derive(Clone, Copy)]
pub struct Picture {
    pub buffer: (i32, i32),
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Renderer {
    Direct3D11,
    OpenGl,
}

enum Target {
    Renderbuffer(glow::NativeRenderbuffer),
    #[cfg(windows)]
    Direct3D(crate::video_d3d11::D3d11Target),
}

pub struct VideoLayer {
    gl: glow::Context,
    framebuffer: glow::NativeFramebuffer,
    target: Target,
    size: (i32, i32),
    pending_size: Option<((i32, i32), Instant)>,
    render_context: RenderContext,
}

impl VideoLayer {
    pub fn new(
        mpv: &Mpv,
        renderer: Renderer,
        get_proc_address: &dyn Fn(&CStr) -> *const c_void,
        on_update: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let gl = unsafe { glow::Context::from_loader_function_cstr(|name| get_proc_address(name)) };
        let framebuffer = unsafe { gl.create_framebuffer() }?;
        let layer = |gl, target, render_context| Self {
            gl,
            framebuffer,
            target,
            size: (0, 0),
            pending_size: None,
            render_context,
        };

        #[cfg(windows)]
        if renderer == Renderer::Direct3D11 {
            match crate::video_d3d11::D3d11Target::new(get_proc_address) {
                Ok(target) => {
                    let render_context = mpv
                        .create_d3d11_render_context(target.device_ptr(), on_update)
                        .map_err(|err| err.to_string())?;
                    return Ok(layer(gl, Target::Direct3D(target), render_context));
                }
                Err(err) => eprintln!("Direct3D 11 video is not available, using OpenGL: {err}"),
            }
        }
        // Only Windows has a choice to make, everywhere else it is OpenGL.
        #[cfg(not(windows))]
        let _ = renderer;

        let renderbuffer = unsafe {
            let renderbuffer = gl.create_renderbuffer()?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(renderbuffer));
            gl.framebuffer_renderbuffer(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::RENDERBUFFER,
                Some(renderbuffer),
            );
            gl.bind_renderbuffer(glow::RENDERBUFFER, None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            renderbuffer
        };
        let render_context = mpv
            .create_render_context(get_proc_address, on_update)
            .map_err(|err| err.to_string())?;
        Ok(layer(gl, Target::Renderbuffer(renderbuffer), render_context))
    }

    pub fn resize_pending(&self) -> bool {
        self.pending_size.is_some()
    }

    /// `resize_now` skips the settle delay, for size changes that aren't a window being dragged.
    /// `hold` keeps showing the frame already in the buffer. mpv keeps the new frame pending until it's rendered.
    pub fn draw(
        &mut self,
        window: (i32, i32),
        area: Area,
        picture: Option<Picture>,
        resize_now: bool,
        hold: bool,
    ) -> Result<(), String> {
        if window.0 <= 0 || window.1 <= 0 {
            return Ok(());
        }
        if area.width <= 0 || area.height <= 0 {
            unsafe { self.clear_window(window) };
            return Ok(());
        }
        let resized = self.follow_area_size((area.width, area.height), Instant::now(), resize_now)?;
        if !hold && (self.render_context.has_new_frame() || resized) {
            let rendered = match &self.target {
                Target::Renderbuffer(_) => self.render_context.render(
                    self.framebuffer.0.get() as i32,
                    self.size.0,
                    self.size.1,
                    glow::RGBA8 as i32,
                ),
                #[cfg(windows)]
                Target::Direct3D(target) => match target.texture_ptr() {
                    Some(texture) => self.render_context.render_d3d11(texture),
                    None => Ok(()),
                },
            };
            rendered.map_err(|err| err.to_string())?;
        }
        let source = if self.size == (area.width, area.height) {
            self.whole_buffer()
        } else {
            self.picture_source(picture)
        };
        unsafe { self.present(window, area, source) };
        Ok(())
    }

    fn whole_buffer(&self) -> (i32, i32, i32, i32) {
        (0, 0, self.size.0, self.size.1)
    }

    // While a resize settles, only the picture is stretched, so mpv's black bars don't hold it at the old size.
    fn picture_source(&self, picture: Option<Picture>) -> (i32, i32, i32, i32) {
        let Some(picture) = picture.filter(|picture| picture.buffer == self.size) else {
            return self.whole_buffer();
        };
        let (left, right) = (picture.left, self.size.0 - picture.right);
        let (low, high) = if self.top_row_first() {
            (picture.top, self.size.1 - picture.bottom)
        } else {
            (picture.bottom, self.size.1 - picture.top)
        };
        if right <= left || high <= low {
            return self.whole_buffer();
        }
        (left, low, right, high)
    }

    // The renderbuffer holds the bottom row first like OpenGL expects, the Direct3D texture the top row first.
    fn top_row_first(&self) -> bool {
        !matches!(self.target, Target::Renderbuffer(_))
    }

    fn follow_area_size(&mut self, target: (i32, i32), now: Instant, resize_now: bool) -> Result<bool, String> {
        if target == self.size {
            self.pending_size = None;
            return Ok(false);
        }
        let since = match self.pending_size {
            Some((size, since)) if size == target => since,
            _ => {
                self.pending_size = Some((target, now));
                now
            }
        };
        let first_frame = self.size == (0, 0);
        if !first_frame && !resize_now && now - since < RESIZE_SETTLE_DELAY {
            return Ok(false);
        }
        unsafe { self.resize_target(target) }?;
        self.pending_size = None;
        Ok(true)
    }

    unsafe fn resize_target(&mut self, size: (i32, i32)) -> Result<(), String> {
        let gl = &self.gl;
        let renderbuffer = match &mut self.target {
            Target::Renderbuffer(renderbuffer) => *renderbuffer,
            #[cfg(windows)]
            Target::Direct3D(target) => {
                unsafe { target.resize(gl, self.framebuffer, size) }?;
                self.size = size;
                return Ok(());
            }
        };
        unsafe {
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(renderbuffer));
            gl.renderbuffer_storage(glow::RENDERBUFFER, glow::RGBA8, size.0, size.1);
            gl.bind_renderbuffer(glow::RENDERBUFFER, None);

            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            gl.disable(glow::SCISSOR_TEST);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            if status != glow::FRAMEBUFFER_COMPLETE {
                return Err(format!("video framebuffer is incomplete (status {status:#x})"));
            }
        }
        self.size = size;
        Ok(())
    }

    unsafe fn clear_window(&self, window: (i32, i32)) {
        let gl = &self.gl;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.disable(glow::SCISSOR_TEST);
            gl.viewport(0, 0, window.0, window.1);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
    }

    // Where the letterboxed video sits in the window, in GL coordinates (y counted from the bottom).
    fn video_rect(&self, window: (i32, i32), area: Area, source: (i32, i32)) -> (i32, i32, i32, i32) {
        let scale = f64::min(area.width as f64 / source.0 as f64, area.height as f64 / source.1 as f64);
        let width = (source.0 as f64 * scale).round() as i32;
        let height = (source.1 as f64 * scale).round() as i32;
        let x = area.x + (area.width - width) / 2;
        let top = area.y + (area.height - height) / 2;
        (x, window.1 - (top + height), width, height)
    }

    /// Reads back the video as shown in the window, as top-down RGBA rows.
    pub fn read_shown_video(&self, window: (i32, i32), area: Area) -> Option<(u32, u32, Vec<u8>)> {
        if self.size.0 <= 0 || self.size.1 <= 0 {
            return None;
        }
        let (x, y, width, height) = self.video_rect(window, area, self.size);
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        unsafe {
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            self.gl.read_pixels(
                x,
                y,
                width,
                height,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
        }
        let row = width as usize * 4;
        let mut top_down: Vec<u8> = pixels.chunks(row).rev().flatten().copied().collect();
        for alpha in top_down.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }
        Some((width as u32, height as u32, top_down))
    }

    unsafe fn present(&self, window: (i32, i32), area: Area, source: (i32, i32, i32, i32)) {
        let (source_left, source_low, source_right, source_high) = source;
        let (x, y, width, height) =
            self.video_rect(window, area, (source_right - source_left, source_high - source_low));

        let gl = &self.gl;
        unsafe {
            self.clear_window(window);
            #[cfg(windows)]
            if let Target::Direct3D(target) = &self.target
                && !target.lock()
            {
                return;
            }
            let (destination_top, destination_bottom) =
                if self.top_row_first() { (y + height, y) } else { (y, y + height) };
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(self.framebuffer));
            gl.blit_framebuffer(
                source_left,
                source_low,
                source_right,
                source_high,
                x,
                destination_top,
                x + width,
                destination_bottom,
                glow::COLOR_BUFFER_BIT,
                glow::LINEAR,
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            #[cfg(windows)]
            if let Target::Direct3D(target) = &self.target {
                target.unlock();
            }
        }
    }
}

impl Drop for VideoLayer {
    fn drop(&mut self) {
        unsafe {
            match &mut self.target {
                Target::Renderbuffer(renderbuffer) => self.gl.delete_renderbuffer(*renderbuffer),
                #[cfg(windows)]
                Target::Direct3D(target) => target.destroy(&self.gl),
            }
            self.gl.delete_framebuffer(self.framebuffer);
        }
    }
}
