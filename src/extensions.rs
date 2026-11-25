// Also read by scripts/lib/extensions.mts, which registers these with Windows in the installer. It expects each
// list in this exact shape, with its length matching the entries.

pub const VIDEO_EXTENSIONS: [&str; 24] = [
    "mkv", "mk3d", "webm", "mp4", "m4v", "mov", "3gp", "3g2", "avi", "wmv", "asf", "flv", "f4v", "ts", "m2ts", "mts",
    "mpg", "mpeg", "m2v", "vob", "ogv", "rm", "rmvb", "divx",
];
pub const AUDIO_EXTENSIONS: [&str; 17] = [
    "mp3", "m4a", "m4b", "aac", "flac", "wav", "ogg", "oga", "opus", "wma", "mka", "ape", "wv", "aiff", "aif", "ac3", "dts",
];
pub const PLAYLIST_EXTENSIONS: [&str; 1] = ["rl"];
