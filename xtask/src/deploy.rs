use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{changelog, read, require_env, signing, version, write};

pub const BUCKET: &str = "rona-files";
pub const DOWNLOAD_DOMAIN: &str = "rona.won.fyi";
const MANIFEST_PATH: &str = "updater/latest.json";

struct Artifact {
    label: &'static str,
    // Folder under bundles/ that the workflow downloads the build artifact into.
    bundle: &'static str,
    suffix: &'static str,
    published_suffix: &'static str,
    // Key in latest.json, `<os>-<arch>` as the app builds it.
    platform: &'static str,
}

const ARTIFACTS: [Artifact; 2] = [
    Artifact {
        label: "Windows",
        bundle: "bundle-windows",
        suffix: "-setup.exe",
        published_suffix: "x64-setup.exe",
        platform: "windows-x86_64",
    },
    Artifact {
        label: "Linux (AppImage)",
        bundle: "bundle-linux",
        suffix: ".AppImage",
        published_suffix: "amd64.AppImage",
        platform: "linux-x86_64",
    },
];

struct Staged {
    artifact: &'static Artifact,
    path: PathBuf,
    remote_path: String,
    signature: String,
}

pub fn run(root: &Path) -> Result<(), String> {
    let version = version::package_version(root)?;
    let tag = format!("v{version}");
    let notes = changelog::release_notes(&read(&root.join("CHANGELOG.md"))?, &version)?;
    let b2_key_id = require_env("B2_APPLICATION_KEY_ID")?;
    let b2_key = require_env("B2_APPLICATION_KEY")?;
    let sign_key = require_env("RONA_SIGN_KEY")?;
    let sign_password = require_env("RONA_SIGN_PASSWORD")?;

    let staging = root.join("release-files");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|err| format!("could not create {}: {err}", staging.display()))?;

    let mut staged = Vec::new();
    for artifact in &ARTIFACTS {
        let built = find_built_file(&root.join("bundles").join(artifact.bundle), artifact.suffix)?;
        let name = format!("rona_{version}_{}", artifact.published_suffix);
        let path = staging.join(&name);
        std::fs::copy(&built, &path).map_err(|err| format!("could not copy {}: {err}", built.display()))?;
        let bytes = std::fs::read(&path).map_err(|err| format!("could not read {}: {err}", path.display()))?;
        let signature = signing::sign(&bytes, &sign_key, &sign_password)?;
        write(&staging.join(format!("{name}.minisig")), &signature)?;
        staged.push(Staged { artifact, path, remote_path: format!("installer/{version}/{name}"), signature });
    }

    run_tool("b2", &["account", "authorize", &b2_key_id, &b2_key])?;
    for file in &staged {
        upload(&file.path, &file.remote_path)?;
        upload(&signature_path(&file.path), &format!("{}.minisig", file.remote_path))?;
    }

    let downloads: Vec<String> =
        staged.iter().map(|file| format!("- {}: {}", file.artifact.label, download_url(&file.remote_path))).collect();
    let release_notes = format!("{notes}\n\n### Downloads\n\n{}", downloads.join("\n"));
    let mut release_args = vec!["release", "create", &tag, "--title", &tag, "--notes", &release_notes];
    let attachments: Vec<String> = staged
        .iter()
        .flat_map(|file| [file.path.display().to_string(), signature_path(&file.path).display().to_string()])
        .collect();
    release_args.extend(attachments.iter().map(String::as_str));
    run_tool("gh", &release_args)?;

    for file in &staged {
        upload(&file.path, &format!("rona_latest_{}", file.artifact.published_suffix))?;
    }

    // Written last, so a client never sees a manifest that points at files that aren't uploaded yet.
    let manifest_path = staging.join("latest.json");
    write(&manifest_path, manifest(&tag, &notes, &chrono::Utc::now().to_rfc3339(), &staged))?;
    upload(&manifest_path, MANIFEST_PATH)?;

    println!("\nDeployed {tag}.");
    Ok(())
}

fn manifest(tag: &str, notes: &str, date: &str, staged: &[Staged]) -> String {
    let platforms: serde_json::Map<String, serde_json::Value> = staged
        .iter()
        .map(|file| {
            let entry = serde_json::json!({ "url": download_url(&file.remote_path), "signature": file.signature });
            (file.artifact.platform.to_string(), entry)
        })
        .collect();
    let manifest = serde_json::json!({ "version": tag, "notes": notes, "pub_date": date, "platforms": platforms });
    serde_json::to_string_pretty(&manifest).expect("a JSON value always serializes")
}

fn download_url(remote_path: &str) -> String {
    format!("https://{DOWNLOAD_DOMAIN}/{remote_path}")
}

fn signature_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".minisig");
    path.with_file_name(name)
}

fn upload(local: &Path, remote: &str) -> Result<(), String> {
    println!("Uploading {} -> b2://{BUCKET}/{remote}", local.display());
    let local = local.display().to_string();
    run_tool("b2", &["file", "upload", BUCKET, &local, remote])
}

fn run_tool(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program).args(args).status().map_err(|err| format!("could not run {program}: {err}"))?;
    if status.success() { Ok(()) } else { Err(format!("{program} {} failed with {status}", args[..2].join(" "))) }
}

// The packagers name files after the product and version, so they are found by suffix anywhere in the bundle.
fn find_built_file(dir: &Path, suffix: &str) -> Result<PathBuf, String> {
    let mut found = Vec::new();
    collect_files(dir, &mut found);
    let matches: Vec<&PathBuf> =
        found.iter().filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().ends_with(suffix))).collect();
    match matches.as_slice() {
        [single] => Ok(single.to_path_buf()),
        [] => Err(format!("no *{suffix} file in {}", dir.display())),
        _ => Err(format!("more than one *{suffix} file in {}", dir.display())),
    }
}

fn collect_files(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, found);
        } else {
            found.push(path);
        }
    }
}
