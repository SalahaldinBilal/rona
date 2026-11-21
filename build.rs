use std::path::{Path, PathBuf};
use std::{env, fs};

const APP_ID: &str = "rona";
const ICON_SOURCE: &str = "assets/icon.svg";
// The window icon and the large taskbar icon, read by src/main.rs with include_bytes!.
const WINDOW_ICON_SIZES: [u32; 2] = [32, 256];
const ICO_SIZES: [u32; 6] = [16, 24, 32, 48, 64, 256];
const HICOLOR_SIZES: [u32; 8] = [16, 22, 24, 32, 48, 64, 128, 256];

fn main() {
    slint_build::compile("ui/app.slint").expect("failed to compile ui/app.slint");
    copy_libmpv();
    build_icons();
}

fn build_icons() {
    println!("cargo:rerun-if-changed={ICON_SOURCE}");
    let svg = fs::read(ICON_SOURCE).expect("failed to read assets/icon.svg");
    let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default())
        .expect("assets/icon.svg is not a valid SVG");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));

    for size in WINDOW_ICON_SIZES {
        fs::write(out_dir.join(format!("icon-{size}.rgba")), render(&tree, size))
            .expect("failed to write the window icon");
    }

    match env::var("CARGO_CFG_TARGET_OS").unwrap_or_default().as_str() {
        "windows" => embed_windows_icon(&tree, &out_dir),
        "linux" => write_linux_desktop_files(&tree, &svg, &out_dir),
        _ => {}
    }
}

fn embed_windows_icon(tree: &resvg::usvg::Tree, out_dir: &Path) {
    use image::codecs::ico::{IcoEncoder, IcoFrame};

    let frames: Vec<IcoFrame> = ICO_SIZES
        .iter()
        .map(|&size| {
            IcoFrame::as_png(&render(tree, size), size, size, image::ExtendedColorType::Rgba8)
                .expect("failed to encode an icon size")
        })
        .collect();
    let ico_path = out_dir.join("icon.ico");
    let file = fs::File::create(&ico_path).expect("failed to create icon.ico");
    IcoEncoder::new(file).encode_images(&frames).expect("failed to write icon.ico");

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(&ico_path.to_string_lossy());
    resource.compile().expect("failed to embed the icon, the Windows SDK resource compiler is needed");

    // The installer takes its icon from here, `node scripts/tasks.mts package` points at it.
    let profile_dir = profile_dir(out_dir).expect("OUT_DIR is not inside a cargo build folder");
    fs::copy(&ico_path, profile_dir.join("rona.ico")).expect("failed to copy icon.ico next to the exe");
}

// Laid out like /usr/share, so installing is copying `share` into ~/.local or /usr.
fn write_linux_desktop_files(tree: &resvg::usvg::Tree, svg: &[u8], out_dir: &Path) {
    let profile_dir = profile_dir(out_dir).expect("OUT_DIR is not inside a cargo build folder");
    let share = profile_dir.join("share");
    let icons = share.join("icons/hicolor");

    for size in HICOLOR_SIZES {
        let folder = icons.join(format!("{size}x{size}/apps"));
        fs::create_dir_all(&folder).expect("failed to create the icon folder");
        let path = folder.join(format!("{APP_ID}.png"));
        image::save_buffer(&path, &render(tree, size), size, size, image::ExtendedColorType::Rgba8)
            .expect("failed to write an icon PNG");
    }
    let scalable = icons.join("scalable/apps");
    fs::create_dir_all(&scalable).expect("failed to create the icon folder");
    fs::write(scalable.join(format!("{APP_ID}.svg")), svg).expect("failed to copy the SVG icon");

    let applications = share.join("applications");
    fs::create_dir_all(&applications).expect("failed to create the applications folder");
    fs::write(applications.join(format!("{APP_ID}.desktop")), desktop_entry())
        .expect("failed to write the .desktop file");
}

// StartupWMClass and the file name match the app id the window sets, so the desktop finds the icon.
fn desktop_entry() -> String {
    let mime_types = [
        "video/mp4", "video/x-matroska", "video/webm", "video/quicktime", "video/x-msvideo", "video/mpeg",
        "video/mp2t", "video/x-flv", "video/ogg", "audio/mpeg", "audio/flac", "audio/ogg", "audio/opus",
        "audio/mp4", "audio/x-wav", "audio/x-matroska",
    ];
    format!(
        "[Desktop Entry]\nType=Application\nName=Rona\nComment=Video player\nExec={APP_ID} %U\n\
         Icon={APP_ID}\nTerminal=false\nCategories=AudioVideo;Video;Player;\nStartupWMClass={APP_ID}\n\
         MimeType={};\n",
        mime_types.join(";")
    )
}

// Straight (not premultiplied) RGBA rows, which both winit and the .ico format expect.
fn render(tree: &resvg::usvg::Tree, size: u32) -> Vec<u8> {
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("icon size is not zero");
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    resvg::render(tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let color = pixel.demultiply();
            [color.red(), color.green(), color.blue(), color.alpha()]
        })
        .collect()
}

fn copy_libmpv() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os != "windows" || target_arch != "x86_64" {
        return;
    }

    let source = Path::new("vendor/mpv/windows-x86_64/libmpv-2.dll");
    println!("cargo:rerun-if-changed={}", source.display());
    if !source.exists() {
        println!("cargo:warning={} is missing, the player will not find libmpv", source.display());
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let profile_dir = profile_dir(&out_dir).expect("OUT_DIR is not inside a cargo build folder");
    fs::copy(source, profile_dir.join("libmpv-2.dll")).expect("failed to copy libmpv-2.dll");
}

// OUT_DIR sits under <profile>/build/, at a depth that differs between cargo versions.
fn profile_dir(out_dir: &Path) -> Option<&Path> {
    out_dir
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == "build"))
        .and_then(Path::parent)
}
