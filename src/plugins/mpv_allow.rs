//! Allowlists, not blocklists: several mpv commands and options run programs, load scripts, open sockets or
//! write files anywhere (`run`, `subprocess`, `load-script`, `input-ipc-server`, `screenshot-to-file`, an
//! `af` with lavfi's `amovie`), and mpv adds new ones over time.

const READABLE: &[&str] = &[
    "ab-loop-a", "ab-loop-b", "af", "aid", "audio-bitrate", "audio-codec", "audio-codec-name", "audio-delay",
    "audio-params", "avsync", "brightness", "cache-buffering-state", "chapter", "chapter-list", "chapter-metadata",
    "chapters", "container-fps", "contrast", "current-tracks", "decoder-frame-drop-count", "deinterlace",
    "demuxer-cache-duration", "demuxer-cache-state", "display-fps", "duration", "edition", "editions",
    "eof-reached", "estimated-vf-fps", "ffmpeg-version", "file-format", "file-size", "filtered-metadata", "fps",
    "frame-drop-count", "gamma", "height", "hue", "hwdec-current", "idle-active", "loop-file", "loop-playlist",
    "media-title", "metadata", "mpv-version", "mute", "osd-dimensions", "panscan", "pause", "paused-for-cache",
    "percent-pos", "playlist-count", "playlist-pos", "playtime-remaining", "saturation", "secondary-sid",
    "secondary-sub-text", "seeking", "sid", "speed", "sub-delay", "sub-end", "sub-pos", "sub-scale", "sub-start",
    "sub-text", "sub-visibility", "time-pos", "time-remaining", "vf", "vid", "video-aspect-override",
    "video-bitrate", "video-codec", "video-format", "video-out-params", "video-pan-x", "video-pan-y", "video-params",
    "video-rotate", "video-zoom", "volume", "width",
];

// These hold file paths or links, so they also need media.path.
const READABLE_WITH_PATHS: &[&str] = &["path", "filename", "stream-open-filename", "stream-path", "playlist", "track-list"];

const WRITABLE: &[&str] = &[
    "ab-loop-a", "ab-loop-b", "aid", "audio-delay", "brightness", "chapter", "contrast", "deinterlace", "gamma", "hue",
    "loop-file", "mute", "panscan", "pause", "percent-pos", "saturation", "secondary-sid", "secondary-sub-visibility",
    "sid", "speed", "sub-delay", "sub-pos", "sub-scale", "sub-visibility", "time-pos", "vid", "video-aspect-override",
    "video-pan-x", "video-pan-y", "video-rotate", "video-zoom", "volume",
];

const COMMANDS: &[&str] = &[
    "ab-loop", "frame-back-step", "frame-step", "revert-seek", "seek", "show-progress", "show-text", "sub-seek",
    "sub-step",
];

// Commands that change a property, which then has to be writable.
const PROPERTY_COMMANDS: &[&str] = &["add", "cycle", "cycle-values", "multiply", "set"];

#[derive(Debug, PartialEq, Eq)]
pub enum Readable {
    Yes,
    WithPaths,
    No,
}

// `track-list/0/title` is checked as `track-list`. `options/...` and `user-data/...` are never readable.
fn base(name: &str) -> &str {
    name.split('/').next().unwrap_or(name)
}

pub fn readable(name: &str) -> Readable {
    let base = base(name);
    if READABLE_WITH_PATHS.contains(&base) {
        // A track's or entry's count, title or language holds no path, but its filename does.
        let safe_part = name.ends_with("/count") || name.ends_with("/title") || name.ends_with("/lang");
        return if safe_part && base != "path" { Readable::Yes } else { Readable::WithPaths };
    }
    if READABLE.contains(&base) { Readable::Yes } else { Readable::No }
}

pub fn writable(name: &str) -> bool {
    WRITABLE.contains(&name)
}

/// Why a command is refused, or `None` when it may run.
pub fn refuse_command(args: &[String]) -> Option<String> {
    let Some(name) = args.first() else { return Some("The command is empty".into()) };
    // A prefix like `no-osd` or `async` comes before the command itself.
    let mut parts = args.iter().map(String::as_str).skip_while(|part| {
        matches!(*part, "no-osd" | "osd-auto" | "osd-bar" | "osd-msg" | "osd-msg-bar" | "raw" | "expand-properties" | "repeatable" | "async" | "sync")
    });
    let Some(command) = parts.next() else { return Some(format!("{name} is not a command")) };
    if COMMANDS.contains(&command) {
        return None;
    }
    if PROPERTY_COMMANDS.contains(&command) {
        let property = parts.next().unwrap_or_default();
        return if writable(property) { None } else { Some(format!("The property {property} can't be changed by plugins")) };
    }
    Some(format!("The mpv command {command} isn't allowed for plugins"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn commands_that_reach_outside_the_player_are_refused() {
        for command in [
            "run calc.exe",
            "subprocess cmd",
            "load-script C:/evil.lua",
            "loadfile C:/Users/someone/secret.txt",
            "screenshot-to-file C:/Windows/x.png",
            "screenshot",
            "dump-cache 0 10 C:/out.mkv",
            "write-watch-later-config",
            "keypress q",
            "osd-overlay id=1 format=ass-events data=x",
            "set input-ipc-server \\\\.\\pipe\\open",
            "set scripts C:/evil.lua",
            "set screenshot-directory C:/",
            "set o C:/out.mkv",
            "set stream-record C:/out.mkv",
            "set af lavfi=[amovie=C:/secret.wav]",
            "set script-opts a=b",
            "set include C:/mpv.conf",
            "set log-file C:/log.txt",
            "no-osd run calc.exe",
            "async subprocess cmd",
            "cycle-values input-ipc-server a b",
        ] {
            assert!(refuse_command(&args(command)).is_some(), "{command} should be refused");
        }
    }

    #[test]
    fn everyday_player_commands_run() {
        for command in ["seek 10 relative", "no-osd seek 0 absolute", "cycle pause", "add volume 5", "set sub-delay 0.5", "frame-step", "show-text hi"] {
            assert_eq!(refuse_command(&args(command)), None, "{command} should run");
        }
    }

    #[test]
    fn options_and_other_clients_data_are_never_readable() {
        assert_eq!(readable("options/screenshot-directory"), Readable::No);
        assert_eq!(readable("user-data/secret"), Readable::No);
        assert_eq!(readable("time-pos"), Readable::Yes);
        assert_eq!(readable("path"), Readable::WithPaths);
        assert_eq!(readable("track-list/2/external-filename"), Readable::WithPaths);
        assert_eq!(readable("track-list/2/title"), Readable::Yes);
        assert_eq!(readable("playlist/0/filename"), Readable::WithPaths);
    }
}
