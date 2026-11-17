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
preinstall-section = '''
{}'''
",
        player_registration()
    )
}

// cargo-packager's own file-associations take over each extension's default, leave the exe path in the open
// command unquoted and never remove anything on uninstall, so the registration is written here instead.
// SHCTX is HKCU in a per-user install. A `un.` section runs in the uninstaller, before cargo-packager's own.
#[cfg(windows)]
fn player_registration() -> String {
    use crate::extensions::{AUDIO_EXTENSIONS, VIDEO_EXTENSIONS};

    const EXE: &str = r#"$\"$INSTDIR\${MAINBINARYNAME}.exe$\""#;
    const APPLICATION: &str = r"Software\Classes\Applications\${MAINBINARYNAME}.exe";
    const CAPABILITIES: &str = r"Software\Clients\Media\Rona\Capabilities";
    let icon = format!("{EXE},0");
    let command = format!(r#"{EXE} $\"%1$\""#);
    let kinds = VIDEO_EXTENSIONS.iter().map(|ext| (ext, "video")).chain(AUDIO_EXTENSIONS.iter().map(|ext| (ext, "audio")));

    let mut install = vec![
        format!(r#"WriteRegStr SHCTX "{APPLICATION}" "FriendlyAppName" "${{PRODUCTNAME}}""#),
        format!(r#"WriteRegStr SHCTX "{APPLICATION}\DefaultIcon" "" "{icon}""#),
        format!(r#"WriteRegStr SHCTX "{APPLICATION}\shell\open\command" "" "{command}""#),
        format!(r#"WriteRegStr SHCTX "{CAPABILITIES}" "ApplicationName" "${{PRODUCTNAME}}""#),
        format!(r#"WriteRegStr SHCTX "{CAPABILITIES}" "ApplicationDescription" "Video and audio player""#),
        format!(r#"WriteRegStr SHCTX "{CAPABILITIES}" "ApplicationIcon" "{icon}""#),
        format!(r#"WriteRegStr SHCTX "Software\RegisteredApplications" "${{PRODUCTNAME}}" "{CAPABILITIES}""#),
    ];
    let mut uninstall = Vec::new();
    for (ext, kind) in kinds {
        let prog_id = format!("Rona.{ext}");
        let class = format!(r"Software\Classes\{prog_id}");
        install.extend([
            format!(r#"WriteRegStr SHCTX "{class}" "" "{} {kind}""#, ext.to_uppercase()),
            format!(r#"WriteRegStr SHCTX "{class}\DefaultIcon" "" "{icon}""#),
            format!(r#"WriteRegStr SHCTX "{class}\shell\open\command" "" "{command}""#),
            // Offers Rona under Open with without taking the default from the app that has it.
            format!(r#"WriteRegStr SHCTX "Software\Classes\.{ext}\OpenWithProgids" "{prog_id}" """#),
            format!(r#"WriteRegStr SHCTX "{APPLICATION}\SupportedTypes" ".{ext}" """#),
            format!(r#"WriteRegStr SHCTX "{CAPABILITIES}\FileAssociations" ".{ext}" "{prog_id}""#),
        ]);
        uninstall.extend([
            format!(r#"DeleteRegKey SHCTX "{class}""#),
            format!(r#"DeleteRegValue SHCTX "Software\Classes\.{ext}\OpenWithProgids" "{prog_id}""#),
        ]);
    }
    uninstall.extend([
        format!(r#"DeleteRegKey SHCTX "{APPLICATION}""#),
        r#"DeleteRegKey SHCTX "Software\Clients\Media\Rona""#.to_string(),
        r#"DeleteRegValue SHCTX "Software\RegisteredApplications" "${PRODUCTNAME}""#.to_string(),
    ]);

    let section = |name: &str, lines: &[String]| {
        let body: String = lines.iter().map(|line| format!("  {line}\n")).collect();
        format!("Section {name}\n{body}  !insertmacro UPDATEFILEASSOC\nSectionEnd\n")
    };
    format!("{}\n{}", section("RegisterPlayer", &install), section("un.UnregisterPlayer", &uninstall))
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn sections() -> (String, String) {
        let text = player_registration();
        let (install, uninstall) = text.split_once("Section un.").expect("an uninstall section");
        (install.to_string(), uninstall.to_string())
    }

    // An unquoted exe path breaks for a user name with a space, and an unquoted %1 for any file with one.
    #[test]
    fn open_commands_quote_the_exe_and_the_file() {
        let (install, _) = sections();
        let commands: Vec<&str> = install.lines().filter(|line| line.contains(r"\shell\open\command")).collect();
        assert!(!commands.is_empty());
        for command in commands {
            assert!(command.ends_with(r#""$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\"""#), "{command}");
        }
    }

    #[test]
    fn uninstalling_removes_every_key_and_value_the_install_adds() {
        let (install, uninstall) = sections();
        for line in install.lines().filter(|line| line.trim_start().starts_with("WriteRegStr")) {
            let fields: Vec<&str> = line.split('"').collect();
            let (key, value) = (fields[1], fields[3]);
            let removed = uninstall.lines().any(|undo| {
                let undo = undo.trim_start();
                (undo.starts_with("DeleteRegKey") && key.starts_with(undo.split('"').nth(1).unwrap_or("\u{0}")))
                    || (undo.starts_with("DeleteRegValue") && undo.contains(&format!(r#""{key}" "{value}""#)))
            });
            assert!(removed, "nothing removes {line}");
        }
    }

    // Deleting a shared key would take other apps' entries with it.
    #[test]
    fn uninstalling_never_deletes_an_extension_key() {
        let (_, uninstall) = sections();
        for line in uninstall.lines().filter(|line| line.contains("DeleteRegKey")) {
            assert!(!line.contains(r"Software\Classes\."), "{line}");
        }
    }
}
