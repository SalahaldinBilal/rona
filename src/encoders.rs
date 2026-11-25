use std::path::Path;
use std::time::{Duration, Instant};

use crate::logging::{debug, info, warn};
use crate::mpv::{Event, Mpv};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vendor {
    Software,
    Nvidia,
    Amd,
    Intel,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    H264,
    H265,
}

#[derive(Clone, Copy, Debug)]
pub struct Available {
    pub vendor: Vendor,
    pub h264: bool,
    pub h265: bool,
}

impl Available {
    pub fn supports(&self, codec: Codec) -> bool {
        match codec {
            Codec::H264 => self.h264,
            Codec::H265 => self.h265,
        }
    }
}

const HARDWARE: [Vendor; 3] = [Vendor::Nvidia, Vendor::Amd, Vendor::Intel];
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_SOURCE: &str = "av://lavfi:color=c=black:s=320x240:r=25:d=0.4";

impl Vendor {
    pub fn label(self) -> &'static str {
        match self {
            Vendor::Software => "Software",
            Vendor::Nvidia => "NVIDIA",
            Vendor::Amd => "AMD",
            Vendor::Intel => "Intel",
        }
    }

    pub fn encoder(self, codec: Codec) -> &'static str {
        match (self, codec) {
            (Vendor::Software, Codec::H264) => "libx264",
            (Vendor::Software, Codec::H265) => "libx265",
            (Vendor::Nvidia, Codec::H264) => "h264_nvenc",
            (Vendor::Nvidia, Codec::H265) => "hevc_nvenc",
            (Vendor::Amd, Codec::H264) => "h264_amf",
            (Vendor::Amd, Codec::H265) => "hevc_amf",
            (Vendor::Intel, Codec::H264) => "h264_qsv",
            (Vendor::Intel, Codec::H265) => "hevc_qsv",
        }
    }
}

/// Tries each hardware encoder on a few blank frames, since only opening one shows whether the GPU and driver have it.
/// Blocks for a second or more, so it runs on its own thread.
pub fn probe() -> Vec<Available> {
    let folder = std::env::temp_dir();
    if !encodes(Vendor::Software.encoder(Codec::H264), &folder) {
        warn!("the encoder check failed with libx264 too, so no hardware encoder is offered");
        return Vec::new();
    }
    let found: Vec<Available> = HARDWARE
        .iter()
        .map(|&vendor| Available {
            vendor,
            h264: encodes(vendor.encoder(Codec::H264), &folder),
            h265: encodes(vendor.encoder(Codec::H265), &folder),
        })
        .filter(|available| available.h264 || available.h265)
        .collect();
    info!("hardware encoders: {found:?}");
    found
}

fn encodes(encoder: &str, folder: &Path) -> bool {
    let output = folder.join(format!("rona-encoder-check-{}-{encoder}.mkv", std::process::id()));
    let output_text = output.to_string_lossy().into_owned();
    let options = [("o", output_text.as_str()), ("ovc", encoder), ("aid", "no"), ("sid", "no")];
    let Ok(mpv) = Mpv::with_options(&options) else { return false };
    let mut failed = mpv.request_log_messages("error").is_err();
    failed |= mpv.command(&["loadfile", PROBE_SOURCE]).is_err();

    let deadline = Instant::now() + PROBE_TIMEOUT;
    while !failed {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            warn!("checking {encoder} took longer than {} s", PROBE_TIMEOUT.as_secs());
            failed = true;
            break;
        }
        match mpv.wait_event(left.as_secs_f64()) {
            Some(Event::Log { prefix, text, .. }) => {
                debug!("checking {encoder}: {prefix}: {text}");
                failed = true;
            }
            Some(Event::EndFile { error, .. }) => {
                failed = error.is_some();
                break;
            }
            Some(Event::Shutdown) => break,
            _ => {}
        }
    }
    drop(mpv);

    let written = std::fs::metadata(&output).map(|meta| meta.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&output);
    let works = !failed && written > 0;
    debug!("{encoder} {}", if works { "works" } else { "is not available" });
    works
}
