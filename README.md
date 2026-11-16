# Rona

A video player for Windows and Linux. It uses [mpv](https://mpv.io) for playback and [Slint](https://slint.dev) for the interface, and it's written in Rust.

## What it does

- Plays local files and http(s) links. Open them from the file dialog, by pasting a link, or by dragging them onto the window.
- Keeps a cache of what it has downloaded from a link, so seeking back or switching subtitle and audio tracks doesn't download the same part again. If the connection drops, it tries to reconnect a few times before asking.
- Remembers where you stopped in each file, plus the track, volume and similar choices you made for it.
- Can skip chapters by name, for example ones titled "Opening" or "Preview".
- Records a part of what you're watching. It can copy it as is or re-encode it.
- Lets you rebind every shortcut.
- Updates itself. It checks when it starts and from Settings > Updates, and installs on restart.

## Download

- Windows: [rona_latest_x64-setup.exe](https://rona.won.fyi/rona_latest_x64-setup.exe)
- Linux: [rona_latest_amd64.AppImage](https://rona.won.fyi/rona_latest_amd64.AppImage)

Older versions are on the [releases page](../../releases).

The Windows installer isn't signed with a code signing certificate, so SmartScreen will warn you the first time you run it. Click "More info", then "Run anyway". Updates are checked against our own signature before they install.

The AppImage needs glibc 2.39 or newer, so Ubuntu 24.04, Fedora 40 or later. It won't run on Ubuntu 22.04 or Debian 12. Make it executable with `chmod +x` and run it.

## Building

You need a recent stable Rust.

On Windows, download libmpv first:

```
cargo xtask fetch-mpv
cargo run --release
```

On Linux, install libmpv and the libraries the window needs. On Ubuntu or Debian:

```
sudo apt install libmpv-dev libfontconfig1-dev libxkbcommon-dev libgtk-3-dev libwayland-dev libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev
cargo run --release
```

You can pass a file or a link to open it right away: `cargo run --release -- video.mkv`.

Debug builds work, but some parts of the interface are slow in them. Use `--release` if you want to see how it really feels.

macOS isn't supported yet.

## License

GPL-3.0-or-later.
