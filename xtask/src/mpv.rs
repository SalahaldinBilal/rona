use std::io::{Cursor, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

const MPV_URL: &str =
    "https://github.com/mpv-player/mpv/releases/download/v0.42.0/libmpv-v0.42.0-x86_64-w64-mingw32-gpl.zip";
// Pinned so a changed or tampered download fails the build instead of being bundled.
const MPV_SHA256: &str = "b99e0d183024ae479d1b36a93fa262c0297dfb025b99715b98da377ffb6c2913";
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

pub fn fetch(root: &Path) -> Result<(), String> {
    println!("Downloading {MPV_URL}");
    let bytes = download(MPV_URL)?;
    let hash = sha256_hex(&bytes);
    if hash != MPV_SHA256 {
        return Err(format!("the download's SHA-256 is {hash}, expected {MPV_SHA256}"));
    }

    let target = root.join("vendor/mpv/windows-x86_64");
    std::fs::create_dir_all(&target).map_err(|err| format!("could not create {}: {err}", target.display()))?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|err| format!("not a zip file: {err}"))?;
    archive.extract(&target).map_err(|err| format!("could not extract into {}: {err}", target.display()))?;
    if !target.join("libmpv-2.dll").exists() {
        return Err(format!("the archive has no libmpv-2.dll at its top level, check {}", target.display()));
    }
    println!("Extracted to {}", target.display());
    Ok(())
}

pub fn download(url: &str) -> Result<Vec<u8>, String> {
    let response = ureq::get(url).call().map_err(|err| format!("could not download {url}: {err}"))?;
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(MAX_DOWNLOAD_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("could not download {url}: {err}"))?;
    Ok(bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}
