mod changelog;
mod deploy;
#[cfg(windows)]
#[path = "../../src/extensions.rs"]
mod extensions;
mod mpv;
mod package;
mod signing;
mod version;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "Usage: cargo xtask <command>

Commands:
  fetch-mpv          Download libmpv for Windows into vendor/mpv/windows-x86_64/
  bump <version>     Set the version in Cargo.toml and date the Unreleased section of CHANGELOG.md
  keygen <password>  Create the update signing key pair, encrypted with this password
  package            Build in release mode and package for this platform (NSIS on Windows, AppImage on Linux)
  deploy             Upload a release from bundles/ (run by the release workflow)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = root_dir();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["fetch-mpv"] => mpv::fetch(&root),
        ["bump", version] => version::bump(&root, version),
        ["keygen", password] => signing::keygen(&root, password),
        ["package"] => package::run(&root),
        ["deploy"] => deploy::run(&root),
        _ => Err(USAGE.into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn root_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask sits inside the repo").to_path_buf()
}

pub fn require_env(name: &str) -> Result<String, String> {
    std::env::var(name).ok().filter(|value| !value.is_empty()).ok_or_else(|| format!("{name} is not set"))
}

pub fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|err| format!("could not read {}: {err}", path.display()))
}

pub fn write(path: &Path, contents: impl AsRef<[u8]>) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|err| format!("could not write {}: {err}", path.display()))
}
