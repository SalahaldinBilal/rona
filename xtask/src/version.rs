use std::path::Path;

use crate::{changelog, read, write};

pub fn bump(root: &Path, version: &str) -> Result<(), String> {
    if !is_version(version) {
        return Err("Usage: cargo xtask bump <version>  (for example 0.2.0)".into());
    }
    let cargo_path = root.join("Cargo.toml");
    let cargo = read(&cargo_path)?;
    let updated = set_package_version(&cargo, version).ok_or("no `version = \"...\"` line in Cargo.toml")?;
    write(&cargo_path, updated)?;
    println!("Updated Cargo.toml");

    let changelog_path = root.join("CHANGELOG.md");
    let (updated, empty) = changelog::cut_release(&read(&changelog_path)?, version, &today())?;
    write(&changelog_path, updated)?;
    if empty {
        eprintln!("Warning: the Unreleased section of CHANGELOG.md is empty, so this release has no notes.");
    }
    println!("Updated CHANGELOG.md\n\nBumped to {version}. Cargo.lock follows on the next build.");
    Ok(())
}

pub fn package_version(root: &Path) -> Result<String, String> {
    let cargo = read(&root.join("Cargo.toml"))?;
    cargo
        .lines()
        .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"').map(str::to_string))
        .ok_or_else(|| "no `version = \"...\"` line in Cargo.toml".into())
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn is_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 3 && parts.iter().all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

// Only a line that starts with `version =` is the package's, dependency versions sit inside `{ ... }`.
fn set_package_version(cargo: &str, version: &str) -> Option<String> {
    let mut replaced = false;
    let lines: Vec<String> = cargo
        .lines()
        .map(|line| {
            if !replaced && line.starts_with("version = \"") {
                replaced = true;
                format!("version = \"{version}\"")
            } else {
                line.to_string()
            }
        })
        .collect();
    replaced.then(|| lines.join("\n") + if cargo.ends_with('\n') { "\n" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARGO: &str = "[package]
name = \"rona\"
version = \"0.1.0\"
edition = \"2024\"

[dependencies]
glow = \"0.18\"
ureq = { version = \"3.5.2\", default-features = false }
";

    #[test]
    fn sets_only_the_package_version() {
        let updated = set_package_version(CARGO, "0.2.0").unwrap();
        assert!(updated.contains("\nversion = \"0.2.0\"\n"));
        assert!(updated.contains("ureq = { version = \"3.5.2\", default-features = false }"));
        assert!(!updated.contains("0.1.0"));
        assert!(updated.ends_with('\n'));
    }

    #[test]
    fn rejects_versions_that_are_not_three_numbers() {
        assert!(is_version("0.2.0"));
        assert!(is_version("1.10.3"));
        assert!(!is_version("v0.2.0"));
        assert!(!is_version("0.2"));
        assert!(!is_version("0.2.0-beta"));
        assert!(!is_version("0..1"));
    }
}
