# Rona

A video and audio player for Windows and Linux. It uses [mpv](https://mpv.io) for playback and [Slint](https://slint.dev) for the interface, and it's written in Rust.

## What it does

- Plays local files and http(s) links. Open them from the file dialog, by dragging them onto the window, or with Ctrl+V, which takes a copied link, a copied path or files copied in Explorer or a file manager.
- Opening several files at once puts them all in the playlist.
- Opening a file while Rona is already open plays it in that window instead of starting a second one. Start Rona with `--new-instance` if you want a separate window.
- Shows audio files with their cover, title, artist and album. These are read ahead for the next files in the playlist, so switching songs is instant.
- Keeps a cache of what it has downloaded from a link, so seeking back or switching subtitle and audio tracks doesn't download the same part again. If the connection drops, it reconnects by itself after 3, 6 and 12 seconds. If that fails too, it waits for you to press Retry.
- Remembers where you stopped in each file, along with the audio and subtitle tracks you picked. This is on by default for videos and off by default for audio, and each can be turned on or off in Settings.
- Remembers the volume you last set, separately for videos and audio files.
- Can skip chapters by name, for example ones titled "Opening" or "Preview".
- Records a part of what you're watching. It can copy it as is or re-encode it.
- Lets you rebind every shortcut.
- Updates itself. It checks when it starts and from Settings > Updates, and installs on restart.

## Download

- Windows: [rona_latest_x64-setup.exe](https://rona.won.fyi/rona_latest_x64-setup.exe)
- Linux: [rona_latest_amd64.AppImage](https://rona.won.fyi/rona_latest_amd64.AppImage)

Older versions are on the [releases page](../../releases).

The Windows installer isn't signed with a code signing certificate, so SmartScreen will warn you the first time you run it. Click "More info", then "Run anyway". Updates are checked against our own signature before they install.

The installer adds Rona to "Open with" for video and audio files, but it doesn't take any file type from the player you use now. To make Rona the default, open Settings > Apps > Default apps, pick Rona and set the file types you want.

The AppImage needs glibc 2.39 or newer, so Ubuntu 24.04, Fedora 40 or later. It won't run on Ubuntu 22.04 or Debian 12. Make it executable with `chmod +x` and run it.

## Building

You need a recent stable Rust, and Node.js 24 or newer for the helper scripts in `scripts/`.

On Windows, download libmpv first:

```
node scripts/tasks.mts fetch-mpv
cargo run --release
```

On Linux, install libmpv and the libraries the window needs. On Ubuntu or Debian:

```
sudo apt install libmpv-dev libfontconfig1-dev libxkbcommon-dev libgtk-3-dev libwayland-dev libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev
cargo run --release
```

You can pass files or links to open them right away: `cargo run --release -- video.mkv`. If Rona is already running, they open in that window. Add `--new-instance` to start a separate one.

Debug builds work, but some parts of the interface are slow in them. Use `--release` if you want to see how it really feels.

`node scripts/tasks.mts package` builds the installer on Windows and the AppImage on Linux.

macOS isn't supported yet.

## License

GPL-3.0-or-later.
