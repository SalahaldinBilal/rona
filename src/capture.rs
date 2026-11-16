use std::path::{Path, PathBuf};

// Redraws to wait after the seek lands, so the frame shown is the settled target frame.
pub const SETTLE_FRAMES: u32 = 10;
// The window may not reach the requested size (for example on a smaller screen), so give up after this many tries.
pub const RESIZE_ATTEMPTS: u32 = 60;

pub enum Stage {
    WaitingForFile,
    Seeking,
    Resizing(u32),
    Settling(u32),
    Done,
}

pub struct Capture {
    pub at: f64,
    pub path: PathBuf,
    pub size: Option<(i32, i32)>,
    pub stage: Stage,
}

impl Capture {
    /// `RP_CAPTURE=<time>|<png path>[|<width>x<height>]`, for example `8:15|C:\shots\frame.png|1920x1080`.
    pub fn from_env() -> Option<Self> {
        let value = std::env::var("RP_CAPTURE").ok()?;
        let mut parts = value.split('|');
        let at = parse_time(parts.next()?)?;
        let path = PathBuf::from(parts.next()?.trim());
        let size = parts.next().and_then(parse_size);
        Some(Self { at, path, size, stage: Stage::WaitingForFile })
    }
}

fn parse_size(text: &str) -> Option<(i32, i32)> {
    let (width, height) = text.trim().split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

fn parse_time(text: &str) -> Option<f64> {
    text.trim().split(':').try_fold(0.0, |total, part| Some(total * 60.0 + part.parse::<f64>().ok()?))
}

pub fn save_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder).map_err(|err| err.to_string())?;
    }
    image::save_buffer_with_format(path, rgba, width, height, image::ExtendedColorType::Rgba8, image::ImageFormat::Png)
        .map_err(|err| err.to_string())
}
