use std::path::Path;
use std::process::Command;

use crate::{version, write};

pub fn run(root: &Path) -> Result<(), String> {
    let version = version::package_version(root)?;
    cargo(root, &["build", "--release"])?;
    let config_path = root.join("target/packager.toml");
    write(&config_path, config(&slash_path(root), &version))?;
    let config = config_path.display().to_string();
    cargo(root, &["packager", "--release", "--config", &config])?;
    println!("\nPackaged Rona {version} into target/packager/");
    Ok(())
}

fn cargo(root: &Path, args: &[&str]) -> Result<(), String> {
    let status = Command::new("cargo")
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|err| format!("could not run cargo: {err}"))?;
    if status.success() { Ok(()) } else { Err(format!("cargo {} failed with {status}", args[0])) }
}

// TOML strings treat backslashes as escapes, and every tool here accepts forward slashes on Windows.
fn slash_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn config(root: &str, version: &str) -> String {
    let common = format!(
        "name = \"rona\"
product-name = \"Rona\"
version = \"{version}\"
identifier = \"fyi.won.rona\"
description = \"Video player\"
out-dir = \"{root}/target/packager\"
binaries-dir = \"{root}/target/release\"
"
    );
    format!("{common}{}", platform_config(root))
}

// build.rs puts libmpv-2.dll and rona.ico next to the release exe.
#[cfg(windows)]
fn platform_config(root: &str) -> String {
    format!(
        "formats = [\"nsis\"]
icons = [\"{root}/target/release/rona.ico\"]
resources = [\"{root}/target/release/libmpv-2.dll\"]

[[binaries]]
path = \"rona\"
main = true

[nsis]
installer-mode = \"currentUser\"
installer-icon = \"{root}/target/release/rona.ico\"
"
    )
}

// build.rs writes the hicolor icons under target/release/share. libmpv comes from the build machine's packages.
#[cfg(not(windows))]
fn platform_config(root: &str) -> String {
    format!(
        "formats = [\"appimage\"]
icons = [\"{root}/target/release/share/icons/hicolor/*/apps/rona.png\"]

[[binaries]]
path = \"rona\"
main = true

[appimage]
libs = [\"libmpv.so.2\"]
"
    )
}
