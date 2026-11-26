use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::manifest::{
    ICON_FILES, MANIFEST_FILE, MAX_ICON_BYTES, MAX_MANIFEST_BYTES, MAX_WASM_BYTES, PluginManifest, WASM_FILE, check_icon,
    check_wasm,
};

pub struct Package {
    pub manifest: PluginManifest,
    pub manifest_raw: Vec<u8>,
    pub wasm: Vec<u8>,
    /// One of `ICON_FILES` and its bytes.
    pub icon: Option<(&'static str, Vec<u8>)>,
}

/// The icon in an installed plugin's folder or a plugin author's build folder.
pub fn icon_path(dir: &Path) -> Option<std::path::PathBuf> {
    ICON_FILES.iter().map(|name| dir.join(name)).find(|path| path.is_file())
}

impl Package {
    pub fn wasm_sha256(&self) -> String {
        sha256_hex(&self.wasm)
    }

    /// Writes the known files into `dir`. Names are fixed, never taken from the package. An icon from the
    /// version before, under another name, is removed.
    pub fn write_to(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join(MANIFEST_FILE), &self.manifest_raw)?;
        std::fs::write(dir.join(WASM_FILE), &self.wasm)?;
        for name in ICON_FILES {
            match &self.icon {
                Some((icon_name, bytes)) if icon_name == name => std::fs::write(dir.join(name), bytes)?,
                _ => match std::fs::remove_file(dir.join(name)) {
                    Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err),
                    _ => {}
                },
            }
        }
        Ok(())
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn read_package(path: &Path, app_version: &str) -> Result<Package, String> {
    let file = std::fs::File::open(path).map_err(|err| format!("Couldn't open the plugin file: {err}"))?;
    read_package_from(file, app_version)
}

pub fn read_package_from<R: Read + std::io::Seek>(reader: R, app_version: &str) -> Result<Package, String> {
    let mut archive = zip::ZipArchive::new(reader).map_err(|_| "This is not a Rona plugin file".to_string())?;
    let manifest_raw =
        read_entry(&mut archive, MANIFEST_FILE, MAX_MANIFEST_BYTES)?.ok_or("The plugin file has no plugin.json")?;
    let wasm = read_entry(&mut archive, WASM_FILE, MAX_WASM_BYTES)?.ok_or("The plugin file has no plugin.wasm")?;
    let mut icon = None;
    for name in ICON_FILES {
        if let Some(bytes) = read_entry(&mut archive, name, MAX_ICON_BYTES)? {
            icon = Some((*name, bytes));
            break;
        }
    }
    finish(manifest_raw, wasm, icon, app_version)
}

/// A plugin author's build folder, loaded in place.
pub fn read_unpacked(dir: &Path, app_version: &str) -> Result<Package, String> {
    let manifest_raw =
        read_file(&dir.join(MANIFEST_FILE), MAX_MANIFEST_BYTES)?.ok_or("The folder has no plugin.json")?;
    let wasm = read_file(&dir.join(WASM_FILE), MAX_WASM_BYTES)?.ok_or("The folder has no plugin.wasm")?;
    let mut icon = None;
    for name in ICON_FILES {
        if let Some(bytes) = read_file(&dir.join(name), MAX_ICON_BYTES)? {
            icon = Some((*name, bytes));
            break;
        }
    }
    finish(manifest_raw, wasm, icon, app_version)
}

fn finish(
    manifest_raw: Vec<u8>,
    wasm: Vec<u8>,
    icon: Option<(&'static str, Vec<u8>)>,
    app_version: &str,
) -> Result<Package, String> {
    let manifest = PluginManifest::parse(&manifest_raw)?;
    manifest.validate(app_version)?;
    check_wasm(&wasm)?;
    if let Some((name, bytes)) = &icon {
        check_icon(name, bytes)?;
    }
    Ok(Package { manifest, manifest_raw, wasm, icon })
}

fn read_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
    limit: u64,
) -> Result<Option<Vec<u8>>, String> {
    let entry = match archive.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(err) => return Err(format!("Couldn't read {name} from the plugin file: {err}")),
    };
    read_limited(entry, name, limit).map(Some)
}

fn read_file(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default().to_owned();
    match std::fs::File::open(path) {
        Ok(file) => read_limited(file, &name, limit).map(Some),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("Couldn't read {name}: {err}")),
    }
}

// The size in a zip header can lie, so the limit is enforced on what is actually read.
fn read_limited(reader: impl Read, name: &str, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes).map_err(|err| format!("Couldn't read {name}: {err}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("{name} is larger than {} MB", limit.div_ceil(1024 * 1024)));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    const MANIFEST: &str = r#"{"apiVersion":1,"id":"com.example.test","name":"Test","version":"1.0.0"}"#;
    const WASM: &[u8] = b"\0asm\x01\0\0\0";

    fn zip_of(entries: &[(&str, &[u8])]) -> Cursor<Vec<u8>> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            writer.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rona-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn entries_with_other_names_never_reach_disk() {
        let dir = scratch("package-escape");
        let package = read_package_from(
            zip_of(&[
                (MANIFEST_FILE, MANIFEST.as_bytes()),
                (WASM_FILE, WASM),
                ("../escaped.txt", b"nope"),
                ("nested/extra.dll", b"nope"),
            ]),
            "0.2.0",
        )
        .unwrap();
        package.write_to(&dir.join("plugin")).unwrap();
        let written = std::fs::read_dir(dir.join("plugin")).unwrap().count();
        assert_eq!(written, 2);
        assert!(!dir.join("escaped.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_package_that_is_not_a_plugin_is_refused() {
        assert!(read_package_from(zip_of(&[(MANIFEST_FILE, MANIFEST.as_bytes())]), "0.2.0").is_err());
        assert!(read_package_from(zip_of(&[(MANIFEST_FILE, MANIFEST.as_bytes()), (WASM_FILE, b"MZ")]), "0.2.0").is_err());
        assert!(read_package_from(Cursor::new(b"not a zip".to_vec()), "0.2.0").is_err());
    }

    #[test]
    fn an_oversized_manifest_is_refused_whatever_the_zip_header_says() {
        let huge = vec![b' '; MAX_MANIFEST_BYTES as usize + 1];
        assert!(read_package_from(zip_of(&[(MANIFEST_FILE, &huge), (WASM_FILE, WASM)]), "0.2.0").is_err());
    }

    // The start of a real file of each format, which is all the checks read.
    const WEBP: &[u8] = b"RIFF\x1a\0\0\0WEBPVP8L\x0d\0\0\0";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR";

    #[test]
    fn what_the_packer_writes_is_what_the_app_reads() {
        let dir = scratch("pack");
        std::fs::write(dir.join(MANIFEST_FILE), MANIFEST).unwrap();
        std::fs::write(dir.join(WASM_FILE), WASM).unwrap();
        std::fs::write(dir.join("icon.webp"), WEBP).unwrap();

        let packed = rona_plugin::pack::pack_plugin(&dir, None).unwrap();
        assert_eq!(packed.file_name().unwrap(), "com.example.test-1.0.0.rp");

        let package = read_package(&packed, "0.2.0").unwrap();
        assert_eq!(package.manifest_raw, MANIFEST.as_bytes());
        assert_eq!(package.wasm, WASM);
        assert_eq!(package.icon, Some(("icon.webp", WEBP.to_vec())));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_packer_refuses_two_icons_and_icons_in_the_wrong_format() {
        let dir = scratch("pack-icons");
        std::fs::write(dir.join(MANIFEST_FILE), MANIFEST).unwrap();
        std::fs::write(dir.join(WASM_FILE), WASM).unwrap();
        std::fs::write(dir.join("icon.webp"), WEBP).unwrap();
        std::fs::write(dir.join("icon.png"), PNG).unwrap();
        assert!(rona_plugin::pack::pack_plugin(&dir, None).unwrap_err().contains("Keep one icon"));

        std::fs::remove_file(dir.join("icon.webp")).unwrap();
        std::fs::write(dir.join("icon.png"), WEBP).unwrap();
        assert!(rona_plugin::pack::pack_plugin(&dir, None).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_update_with_a_new_icon_format_leaves_no_old_icon_behind() {
        let dir = scratch("package-icon-update");
        let version = |icon: (&str, &[u8])| {
            read_package_from(zip_of(&[(MANIFEST_FILE, MANIFEST.as_bytes()), (WASM_FILE, WASM), icon]), "0.2.0").unwrap()
        };
        version(("icon.png", PNG)).write_to(&dir).unwrap();
        version(("icon.webp", WEBP)).write_to(&dir).unwrap();
        assert_eq!(icon_path(&dir), Some(dir.join("icon.webp")));
        assert!(!dir.join("icon.png").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
