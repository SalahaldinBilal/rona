use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use slint::ComponentHandle;

use crate::logging::{info, warn};
use crate::settings::SharedConfig;
use crate::{AppWindow, Phase, Player, UpdatePhase, Updates, WindowControls};

const ENDPOINT: &str = "https://rona.won.fyi/updater/latest.json";
const DOWNLOAD_SITE: &str = "rona.won.fyi";
// Points the updater at a test manifest, and allows installing from a debug build.
const ENDPOINT_ENV: &str = "RONA_UPDATE_ENDPOINT";
const PUBLIC_KEY: &str = include_str!("../assets/update-key.pub");
const TIMEOUT: Duration = Duration::from_secs(30);
const PROGRESS_STEP_BYTES: u64 = 512 * 1024;

#[derive(Deserialize)]
struct Manifest {
    version: String,
    #[serde(default)]
    notes: String,
    platforms: HashMap<String, Download>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct Download {
    url: String,
    signature: String,
}

#[derive(Clone, Debug, PartialEq)]
struct Release {
    version: String,
    notes: String,
    download: Download,
}

#[derive(Default)]
struct State {
    release: Option<Release>,
    downloaded: Option<PathBuf>,
}

thread_local! {
    // Only touched on the UI thread.
    static STATE: RefCell<State> = RefCell::default();
}

// The verified update, installed once the window has closed and everything is saved.
static PENDING_INSTALL: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn wire(app: &AppWindow, config: &SharedConfig) {
    let updates = app.global::<Updates>();
    updates.set_current_version(env!("CARGO_PKG_VERSION").into());
    updates.set_self_update(can_self_update());
    updates.set_download_site(DOWNLOAD_SITE.into());

    let app_weak = app.as_weak();
    updates.on_check(move || {
        if let Some(app) = app_weak.upgrade() {
            check(&app, false);
        }
    });
    let app_weak = app.as_weak();
    updates.on_install(move || {
        if let Some(app) = app_weak.upgrade() {
            download(&app);
        }
    });
    let app_weak = app.as_weak();
    updates.on_restart(move || {
        if let Some(app) = app_weak.upgrade() {
            restart(&app);
        }
    });

    if config.borrow().check_updates {
        check(app, true);
    }
}

/// Runs after the event loop ends. Starts the installer on Windows, swaps the AppImage on Linux.
pub fn finish_pending_install() {
    let Some(path) = PENDING_INSTALL.lock().unwrap_or_else(|err| err.into_inner()).take() else { return };
    info!("installing the update from {}", path.display());
    if let Err(err) = hand_off(path) {
        crate::logging::error!("could not install the update: {err}");
    }
}

fn can_self_update() -> bool {
    let allowed_build = cfg!(not(debug_assertions)) || std::env::var_os(ENDPOINT_ENV).is_some();
    let installable = cfg!(windows) || appimage_path().is_some();
    cfg!(feature = "self-update") && !PUBLIC_KEY.trim().is_empty() && allowed_build && installable
}

fn appimage_path() -> Option<PathBuf> {
    std::env::var_os("APPIMAGE").map(PathBuf::from).filter(|path| path.is_file())
}

fn endpoint() -> String {
    std::env::var(ENDPOINT_ENV).unwrap_or_else(|_| ENDPOINT.into())
}

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(TIMEOUT))
        .timeout_recv_response(Some(TIMEOUT))
        .timeout_per_read(Some(TIMEOUT))
        .build();
    ureq::Agent::new_with_config(config)
}

fn check(app: &AppWindow, announce: bool) {
    let updates = app.global::<Updates>();
    if matches!(updates.get_phase(), UpdatePhase::Checking | UpdatePhase::Downloading | UpdatePhase::Ready) {
        return;
    }
    updates.set_phase(UpdatePhase::Checking);
    updates.set_error("".into());
    let app_weak = app.as_weak();
    std::thread::spawn(move || {
        info!("checking {} for updates", endpoint());
        let result = fetch_release(&endpoint());
        match &result {
            Ok(Some(release)) => info!("update {} available", release.version),
            Ok(None) => info!("up to date"),
            Err(err) => warn!("update check failed: {err}"),
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = app_weak.upgrade() {
                show_check_result(&app, result, announce);
            }
        });
    });
}

fn show_check_result(app: &AppWindow, result: Result<Option<Release>, String>, announce: bool) {
    let updates = app.global::<Updates>();
    match result {
        Ok(Some(release)) => {
            updates.set_new_version(release.version.as_str().into());
            updates.set_notes(plain_notes(&release.notes).into());
            updates.set_has_release(true);
            updates.set_phase(UpdatePhase::Available);
            // Never over a playing video, app.slint also closes it once something starts playing.
            if announce && app.global::<Player>().get_phase() != Phase::Playing {
                updates.set_banner_visible(true);
            }
            STATE.with_borrow_mut(|state| state.release = Some(release));
        }
        Ok(None) => updates.set_phase(UpdatePhase::UpToDate),
        // A failed check on startup stays quiet, the Updates page shows it.
        Err(err) => {
            updates.set_error(format!("Couldn't check for updates. {err}").into());
            updates.set_phase(UpdatePhase::Failed);
        }
    }
}

fn fetch_release(url: &str) -> Result<Option<Release>, String> {
    let response = agent().get(url).call().map_err(|err| err.to_string())?;
    let text = response.into_body().read_to_string().map_err(|err| err.to_string())?;
    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|err| format!("The update information is malformed: {err}"))?;
    newer_release(manifest, env!("CARGO_PKG_VERSION"), &platform())
}

fn newer_release(manifest: Manifest, current: &str, platform: &str) -> Result<Option<Release>, String> {
    let latest = parse_version(&manifest.version)
        .ok_or_else(|| format!("The update information has an unreadable version \"{}\".", manifest.version))?;
    let current = parse_version(current).ok_or("This build has an unreadable version.")?;
    if latest <= current {
        return Ok(None);
    }
    let version = manifest.version.trim_start_matches('v').to_string();
    let download = manifest
        .platforms
        .get(platform)
        .cloned()
        .ok_or_else(|| format!("Version {version} has no download for this system ({platform})."))?;
    Ok(Some(Release { version, notes: manifest.notes, download }))
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.trim().trim_start_matches('v').split('.').map(|part| part.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

// Release notes come from the changelog in Markdown, the page shows plain text.
fn plain_notes(notes: &str) -> String {
    notes
        .lines()
        .map(|line| {
            let line = line.trim_end();
            if let Some(heading) = line.strip_prefix('#') {
                heading.trim_start_matches('#').trim().to_string()
            } else if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
                format!("• {item}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn download(app: &AppWindow) {
    let updates = app.global::<Updates>();
    if !can_self_update() || updates.get_phase() == UpdatePhase::Downloading {
        return;
    }
    let Some(release) = STATE.with_borrow(|state| state.release.clone()) else { return };
    updates.set_phase(UpdatePhase::Downloading);
    updates.set_progress(-1.0);
    updates.set_error("".into());

    let app_weak = app.as_weak();
    std::thread::spawn(move || {
        info!("downloading update {} from {}", release.version, release.download.url);
        let progress_weak = app_weak.clone();
        let result = download_verified(&release, PUBLIC_KEY, move |fraction| {
            let app_weak = progress_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = app_weak.upgrade() {
                    app.global::<Updates>().set_progress(fraction);
                }
            });
        });
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = app_weak.upgrade() else { return };
            let updates = app.global::<Updates>();
            match result {
                Ok(path) => {
                    info!("update verified and saved to {}", path.display());
                    STATE.with_borrow_mut(|state| state.downloaded = Some(path));
                    updates.set_phase(UpdatePhase::Ready);
                }
                Err(err) => {
                    warn!("update download failed: {err}");
                    updates.set_error(format!("The update couldn't be downloaded. {err}").into());
                    updates.set_phase(UpdatePhase::Failed);
                }
            }
        });
    });
}

fn download_verified(release: &Release, public_key: &str, on_progress: impl Fn(f32)) -> Result<PathBuf, String> {
    let key = minisign_verify::PublicKey::decode(public_key.trim())
        .map_err(|err| format!("This build's update key is unreadable: {err}"))?;
    let signature = minisign_verify::Signature::decode(&release.download.signature)
        .map_err(|err| format!("The update's signature is unreadable: {err}"))?;

    let response = agent().get(&release.download.url).call().map_err(|err| err.to_string())?;
    let total = response.body().content_length();
    let mut reader = response.into_body().into_reader();
    let mut bytes = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut reported = 0u64;
    loop {
        let read = reader.read(&mut chunk).map_err(|err| err.to_string())?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        let done = bytes.len() as u64;
        if let Some(total) = total.filter(|total| *total > 0 && done - reported >= PROGRESS_STEP_BYTES) {
            reported = done;
            on_progress((done as f32 / total as f32).min(1.0));
        }
    }
    key.verify(&bytes, &signature, false).map_err(|_| "The download doesn't match its signature.".to_string())?;

    let path = staging_path(&release.version)?;
    std::fs::write(&path, &bytes).map_err(|err| format!("Couldn't save it to {}: {err}", path.display()))?;
    Ok(path)
}

#[cfg(windows)]
fn staging_path(version: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("rona-update");
    std::fs::create_dir_all(&dir).map_err(|err| format!("Couldn't create {}: {err}", dir.display()))?;
    Ok(dir.join(format!("rona_{version}_x64-setup.exe")))
}

// Next to the AppImage, so swapping it in is a rename on the same file system.
#[cfg(not(windows))]
fn staging_path(_version: &str) -> Result<PathBuf, String> {
    let appimage = appimage_path().ok_or("Rona isn't running from an AppImage.")?;
    let mut name = appimage.file_name().unwrap_or_default().to_os_string();
    name.push(".update");
    Ok(appimage.with_file_name(name))
}

fn restart(app: &AppWindow) {
    let Some(path) = STATE.with_borrow_mut(|state| state.downloaded.take()) else { return };
    *PENDING_INSTALL.lock().unwrap_or_else(|err| err.into_inner()) = Some(path);
    app.global::<WindowControls>().invoke_close();
}

// The installer fails while any rona.exe lives, and Rona exits slowly with a short-lived child, so it waits (30 s at most).
#[cfg(windows)]
fn hand_off(installer: PathBuf) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let dir = installer.parent().map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let installer = installer.display().to_string().replace('\'', "''");
    let script = format!(
        r#"$deadline = (Get-Date).AddSeconds(30)
while ((Get-Process -Name rona -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {{ Start-Sleep -Milliseconds 250 }}
Start-Process -FilePath '{installer}' -ArgumentList '/P','/R'
"#
    );
    let script_path = dir.join("handoff.ps1");
    std::fs::write(&script_path, script).map_err(|err| err.to_string())?;
    let script_path = script_path.display().to_string();
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-File", &script_path])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|err| err.to_string())?;
    Ok(())
}

#[cfg(not(windows))]
fn hand_off(update: PathBuf) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let appimage = appimage_path().ok_or("Rona isn't running from an AppImage.")?;
    std::fs::set_permissions(&update, std::fs::Permissions::from_mode(0o755)).map_err(|err| err.to_string())?;
    std::fs::rename(&update, &appimage).map_err(|err| err.to_string())?;
    std::process::Command::new(&appimage).spawn().map_err(|err| err.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r##"{
  "version": "v0.2.0",
  "notes": "- Chapter skipping.",
  "pub_date": "2026-11-16T12:00:00+00:00",
  "platforms": {
    "windows-x86_64": {
      "url": "https://rona.won.fyi/installer/0.2.0/rona_0.2.0_x64-setup.exe",
      "signature": "untrusted comment: signature from minisign secret key\nRUQ...\n"
    },
    "linux-x86_64": {
      "url": "https://rona.won.fyi/installer/0.2.0/rona_0.2.0_amd64.AppImage",
      "signature": "untrusted comment: signature from minisign secret key\nRUQ...\n"
    }
  }
}"##;

    fn manifest() -> Manifest {
        serde_json::from_str(MANIFEST).unwrap()
    }

    #[test]
    fn a_newer_release_gives_this_platforms_download() {
        let release = newer_release(manifest(), "0.1.0", "linux-x86_64").unwrap().unwrap();
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.download.url, "https://rona.won.fyi/installer/0.2.0/rona_0.2.0_amd64.AppImage");
    }

    #[test]
    fn the_same_or_an_older_release_is_no_update() {
        assert_eq!(newer_release(manifest(), "0.2.0", "windows-x86_64").unwrap(), None);
        assert_eq!(newer_release(manifest(), "0.10.0", "windows-x86_64").unwrap(), None);
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(parse_version("0.10.0") > parse_version("0.9.0"));
        assert!(parse_version("v1.0.0") > parse_version("0.99.99"));
        assert_eq!(parse_version("0.2"), None);
        assert_eq!(parse_version("0.2.0-beta"), None);
    }

    #[test]
    fn a_release_without_this_platform_is_an_error() {
        assert!(newer_release(manifest(), "0.1.0", "linux-aarch64").is_err());
    }

    // Serves `files` by path, one request per connection, until the test ends.
    fn serve(files: Vec<(&'static str, Vec<u8>)>) -> String {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request_line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                reader.read_line(&mut request_line).unwrap();
                let mut header = String::new();
                while reader.read_line(&mut header).unwrap() > 2 {
                    header.clear();
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("");
                let reply = match files.iter().find(|(served, _)| *served == path) {
                    Some((_, body)) => {
                        let mut reply =
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                                .into_bytes();
                        reply.extend_from_slice(body);
                        reply
                    }
                    None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                };
                let _ = stream.write_all(&reply);
            }
        });
        address
    }

    fn signed_release(installer: &[u8], tampered: bool) -> (String, String) {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let public = pair.pk.to_box().unwrap().to_string();
        let signature =
            minisign::sign(None, &pair.sk, std::io::Cursor::new(installer), None, None).unwrap().to_string();
        let mut served = installer.to_vec();
        if tampered {
            served[0] ^= 1;
        }
        let base = serve(vec![("/rona_9.0.0_x64-setup.exe", served), ("/latest.json", Vec::new())]);
        let manifest = serde_json::json!({
            "version": "v9.0.0",
            "notes": "- Something new.",
            "pub_date": "2026-11-16T12:00:00+00:00",
            "platforms": {
                platform(): { "url": format!("{base}/rona_9.0.0_x64-setup.exe"), "signature": signature }
            }
        });
        let manifest_base = serve(vec![("/latest.json", manifest.to_string().into_bytes())]);
        (format!("{manifest_base}/latest.json"), public)
    }

    #[test]
    fn a_published_release_is_found_over_http() {
        let (manifest_url, _) = signed_release(b"installer", false);
        let release = fetch_release(&manifest_url).unwrap().unwrap();
        assert_eq!(release.version, "9.0.0");
        assert_eq!(release.notes, "- Something new.");
    }

    #[cfg(windows)]
    #[test]
    fn a_signed_download_is_saved_and_a_tampered_one_refused() {
        let installer = vec![7u8; 3 * 1024 * 1024];
        let (manifest_url, public) = signed_release(&installer, false);
        let release = fetch_release(&manifest_url).unwrap().unwrap();
        let path = download_verified(&release, &public, |_| {}).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), installer);
        std::fs::remove_file(path).unwrap();

        let (manifest_url, public) = signed_release(&installer, true);
        let release = fetch_release(&manifest_url).unwrap().unwrap();
        let err = download_verified(&release, &public, |_| {}).unwrap_err();
        assert!(err.contains("doesn't match its signature"), "{err}");
    }

    // Made by scripts/lib/minisign.mts, which signs releases, over the bytes "installer bytes".
    const SCRIPT_PUBLIC_KEY: &str = "untrusted comment: minisign public key: DE343C9638D875F6
RWT2ddg4ljw03iURz1A/kNXu5XlG3EZK36xINMms1Bjckdc9XYH+Yqda
";
    const SCRIPT_SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RUT2ddg4ljw03tRgSBDawRHodGIEZLmwUJ8/JekLA7vNz50S5U+UNoarG9Tsgqzbfw63GSIdFnoWkOE1/+YjGMuxKmrrXMbkTAk=
trusted comment: timestamp:1795294747
3VEmqDWcS8zo1ANLBq89S6ovtEFDhAk9XlV+ZHrmEnWIIjIsVxgYAkE4odyT/KNBsKPMG+4h4Ipjr/DYnFbsBA==
";

    #[test]
    fn signatures_from_the_release_scripts_verify() {
        let key = minisign_verify::PublicKey::decode(SCRIPT_PUBLIC_KEY.trim()).unwrap();
        let signature = minisign_verify::Signature::decode(SCRIPT_SIGNATURE).unwrap();
        assert!(key.verify(b"installer bytes", &signature, false).is_ok());
        assert!(key.verify(b"installer byteS", &signature, false).is_err());
    }

    #[test]
    fn changelog_notes_read_as_plain_text() {
        let notes = "### Added\n- Chapter skipping.\n- A Renderer setting.\n\n### Fixed\n- Fullscreen from maximized.";
        assert_eq!(
            plain_notes(notes),
            "Added\n• Chapter skipping.\n• A Renderer setting.\n\nFixed\n• Fullscreen from maximized."
        );
    }
}
