//! Every permission a plugin can ask for. Each one is a switch the user can turn off at any time.

pub struct Permission {
    pub name: &'static str,
    pub category: &'static str,
    pub label: &'static str,
}

pub const PLAYBACK_READ: &str = "playback.read";
pub const PLAYBACK_CONTROL: &str = "playback.control";
pub const MEDIA_METADATA: &str = "media.metadata";
pub const MEDIA_PATH: &str = "media.path";
pub const MEDIA_TRACKS: &str = "media.tracks";
pub const MEDIA_SUBTITLES: &str = "media.subtitles";
pub const MEDIA_FILE: &str = "media.file";
pub const OPEN_MEDIA: &str = "open.media";
pub const OPEN_RESOLVE: &str = "open.resolve";
pub const PLAYLIST_READ: &str = "playlist.read";
pub const PLAYLIST_EDIT: &str = "playlist.edit";
pub const MPV_READ: &str = "mpv.read";
pub const MPV_WRITE: &str = "mpv.write";
pub const MPV_COMMAND: &str = "mpv.command";
pub const UI_OSD: &str = "ui.osd";
pub const UI_OVERLAY: &str = "ui.overlay";
pub const UI_PANEL: &str = "ui.panel";
pub const UI_CONTROLS: &str = "ui.controls";
pub const UI_MENU: &str = "ui.menu";
pub const INPUT_ACTIONS: &str = "input.actions";
pub const CLIPBOARD_READ: &str = "clipboard.read";
pub const CLIPBOARD_WRITE: &str = "clipboard.write";
pub const IPC_SERVE: &str = "ipc.serve";
/// Asked for with a list of `hosts`. Each host is its own switch, `network:<host>`.
pub const NETWORK: &str = "network";
/// Asked for with a list of `pipes`. Each pipe is its own switch, `ipc.connect:<pipe>`.
pub const IPC_CONNECT: &str = "ipc.connect";
/// Asked for with a list of `programs`. Each program is its own switch, `system.run:<program>`.
pub const SYSTEM_RUN: &str = "system.run";

pub const PERMISSIONS: &[Permission] = &[
    Permission { name: PLAYBACK_READ, category: "Playback", label: "See what plays and where it is" },
    Permission { name: PLAYBACK_CONTROL, category: "Playback", label: "Play, pause, seek and change the volume" },
    Permission { name: MEDIA_METADATA, category: "Media", label: "Read titles, tags, codecs and chapters" },
    Permission { name: MEDIA_PATH, category: "Media", label: "See file paths and links" },
    Permission { name: MEDIA_TRACKS, category: "Media", label: "See and switch audio and subtitle tracks" },
    Permission { name: MEDIA_SUBTITLES, category: "Media", label: "Add subtitles" },
    Permission { name: MEDIA_FILE, category: "Media", label: "Read parts of the playing file" },
    Permission { name: OPEN_MEDIA, category: "Opening", label: "Open files and links" },
    Permission { name: OPEN_RESOLVE, category: "Opening", label: "Turn links into playable ones" },
    Permission { name: PLAYLIST_READ, category: "Playlist", label: "See the playlist" },
    Permission { name: PLAYLIST_EDIT, category: "Playlist", label: "Add, remove and move playlist entries" },
    Permission { name: MPV_READ, category: "mpv", label: "Read mpv properties" },
    Permission { name: MPV_WRITE, category: "mpv", label: "Change mpv properties" },
    Permission { name: MPV_COMMAND, category: "mpv", label: "Run mpv commands" },
    Permission { name: UI_OSD, category: "Interface", label: "Show messages on the video" },
    Permission { name: UI_OVERLAY, category: "Interface", label: "Draw text on the video" },
    Permission { name: UI_PANEL, category: "Interface", label: "Show a side panel" },
    Permission { name: UI_CONTROLS, category: "Interface", label: "Add controls to the control bar" },
    Permission { name: UI_MENU, category: "Interface", label: "Add menu entries" },
    Permission { name: INPUT_ACTIONS, category: "Interface", label: "Add actions you can bind to keys" },
    Permission { name: CLIPBOARD_READ, category: "Clipboard", label: "Read text from the clipboard" },
    Permission { name: CLIPBOARD_WRITE, category: "Clipboard", label: "Copy text to the clipboard" },
    Permission { name: NETWORK, category: "Network", label: "Connect to" },
    Permission { name: IPC_SERVE, category: "Other programs", label: "Let other programs on this computer talk to it" },
    Permission { name: IPC_CONNECT, category: "Other programs", label: "Talk to" },
    Permission { name: SYSTEM_RUN, category: "Other programs", label: "Run" },
];

pub fn find(name: &str) -> Option<&'static Permission> {
    PERMISSIONS.iter().find(|permission| permission.name == name)
}

pub fn network_grant(host: &str) -> String {
    format!("{NETWORK}:{host}")
}

pub fn pipe_grant(pipe: &str) -> String {
    format!("{IPC_CONNECT}:{pipe}")
}

pub fn program_grant(program: &str) -> String {
    format!("{SYSTEM_RUN}:{program}")
}

/// The permission a grant belongs to: `network` for `network:example.com`.
pub fn base_of(grant: &str) -> &str {
    grant.split_once(':').map_or(grant, |(base, _)| base)
}
