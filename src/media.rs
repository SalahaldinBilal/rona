use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::Path;

use ureq::ResponseExt;

use crate::{http_cache, percent_decode};

use crate::extensions::{AUDIO_EXTENSIONS, VIDEO_EXTENSIONS};

const SNIFF_BYTES: u64 = 4096;

pub fn dialog_extensions() -> Vec<&'static str> {
    VIDEO_EXTENSIONS.iter().chain(AUDIO_EXTENSIONS.iter()).copied().collect()
}

pub fn is_media_name(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let Some((_, extension)) = name.rsplit_once('.') else { return false };
    let extension = extension.to_ascii_lowercase();
    VIDEO_EXTENSIONS.contains(&extension.as_str()) || AUDIO_EXTENSIONS.contains(&extension.as_str())
}

pub fn is_audio_name(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.rsplit_once('.').is_some_and(|(_, extension)| AUDIO_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

#[derive(Debug, PartialEq)]
pub enum LinkError {
    NotALink,
    MissingScheme,
    UnsupportedScheme,
}

impl LinkError {
    pub fn message(&self) -> &'static str {
        match self {
            LinkError::NotALink => "Not a link",
            LinkError::MissingScheme => "Must start with http:// or https://",
            LinkError::UnsupportedScheme => "Only http and https links are supported",
        }
    }
}

// Links copied from chats often come wrapped in quotes or angle brackets.
pub fn clean_text(text: &str) -> &str {
    let mut text = text.trim();
    loop {
        let inner = [('"', '"'), ('\'', '\''), ('<', '>')]
            .iter()
            .find_map(|(open, close)| text.strip_prefix(*open).and_then(|rest| rest.strip_suffix(*close)));
        match inner {
            Some(inner) => text = inner.trim(),
            None => return text,
        }
    }
}

pub fn parse_link(text: &str) -> Result<String, LinkError> {
    let text = clean_text(text);
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return Err(LinkError::NotALink);
    }
    let Some((scheme, rest)) = text.split_once("://") else {
        return Err(if looks_like_host(text) { LinkError::MissingScheme } else { LinkError::NotALink });
    };
    let valid_scheme = !scheme.is_empty()
        && scheme.chars().all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character));
    if !valid_scheme {
        return Err(LinkError::NotALink);
    }
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(LinkError::UnsupportedScheme);
    }
    if host_of(rest).is_empty() {
        return Err(LinkError::NotALink);
    }
    Ok(format!("{scheme}://{rest}"))
}

fn looks_like_host(text: &str) -> bool {
    let host = text.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(name, port)| {
        if port.chars().all(|character| character.is_ascii_digit()) { name } else { host }
    });
    let Some((_, top_level)) = host.rsplit_once('.') else { return false };
    top_level.len() >= 2
        && top_level.chars().all(|character| character.is_ascii_alphabetic())
        && host.chars().all(|character| character.is_ascii_alphanumeric() || ".-".contains(character))
}

fn host_of(rest: &str) -> &str {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    if authority.starts_with('[') {
        return authority.split_once(']').map_or(authority, |(host, _)| &host[1..]);
    }
    authority.split(':').next().unwrap_or("")
}

pub fn link_host(url: &str) -> String {
    host_of(url.split_once("://").map_or(url, |(_, rest)| rest)).to_string()
}

fn link_path(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    rest.find('/').map_or("", |index| &rest[index..])
}

pub fn link_has_media_extension(url: &str) -> bool {
    is_media_name(&percent_decode(link_path(url)))
}

pub fn without_scheme(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, rest)| rest)
}

pub struct SourceInfo {
    pub name: String,
    pub place: String,
    pub is_link: bool,
}

/// Name and folder or host for a path as mpv sees it, including `rpcache://` links.
pub fn describe_source(path: &str) -> SourceInfo {
    let path = path.strip_prefix(http_cache::PROTOCOL).and_then(|rest| rest.strip_prefix("://")).unwrap_or(path);
    if path.starts_with("http://") || path.starts_with("https://") {
        let host = link_host(path);
        let decoded = percent_decode(link_path(path));
        let name = decoded.rsplit('/').find(|segment| !segment.is_empty()).map(str::to_string);
        return SourceInfo { name: name.unwrap_or_else(|| host.clone()), place: host, is_link: true };
    }
    let as_path = Path::new(path);
    let name = as_path.file_name().map_or_else(|| path.to_string(), |name| name.to_string_lossy().into_owned());
    let place = as_path.parent().map(|folder| folder.display().to_string()).unwrap_or_default();
    SourceInfo { name, place, is_link: false }
}

#[derive(Debug, PartialEq)]
pub enum Sniff {
    Media,
    Known(&'static str),
    Unknown,
}

pub fn sniff(bytes: &[u8]) -> Sniff {
    if is_heif_image(bytes) {
        return Sniff::Known("an image");
    }
    if is_media_bytes(bytes) {
        return Sniff::Media;
    }
    match known_kind(bytes) {
        Some(kind) => Sniff::Known(kind),
        None => Sniff::Unknown,
    }
}

fn starts(bytes: &[u8], offset: usize, signature: &[u8]) -> bool {
    bytes.get(offset..offset + signature.len()) == Some(signature)
}

fn is_media_bytes(bytes: &[u8]) -> bool {
    starts(bytes, 0, &[0x1A, 0x45, 0xDF, 0xA3])
        || [b"ftyp", b"moov", b"mdat", b"free", b"wide", b"skip"].iter().any(|kind| starts(bytes, 4, *kind))
        || (starts(bytes, 0, b"RIFF") && (starts(bytes, 8, b"AVI ") || starts(bytes, 8, b"WAVE")))
        || starts(bytes, 0, &[0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11])
        || starts(bytes, 0, b"FLV")
        || is_transport_stream(bytes)
        || starts(bytes, 0, &[0x00, 0x00, 0x01, 0xBA])
        || starts(bytes, 0, b"OggS")
        || starts(bytes, 0, b".RMF")
        || starts(bytes, 0, b"ID3")
        || starts(bytes, 0, b"fLaC")
        || (starts(bytes, 0, b"FORM") && (starts(bytes, 8, b"AIFF") || starts(bytes, 8, b"AIFC")))
        || starts(bytes, 0, b"MAC ")
        || starts(bytes, 0, b"wvpk")
        || starts(bytes, 0, &[0x0B, 0x77])
        || starts(bytes, 0, &[0x7F, 0xFE, 0x80, 0x01])
        || frames_repeat(bytes, mpeg_audio_frame_length)
        || frames_repeat(bytes, adts_frame_length)
}

// 188-byte packets, or 192-byte M2TS packets whose sync byte follows a 4-byte timestamp.
fn is_transport_stream(bytes: &[u8]) -> bool {
    [(0usize, 188usize), (4, 192)]
        .iter()
        .any(|&(start, step)| (0..3).all(|packet| bytes.get(start + packet * step) == Some(&0x47)))
}

// HEIC and AVIF images use the same ftyp box as MP4.
fn is_heif_image(bytes: &[u8]) -> bool {
    starts(bytes, 4, b"ftyp")
        && [b"heic", b"heix", b"heim", b"heis", b"mif1", b"msf1", b"avif", b"avis"]
            .iter()
            .any(|brand| starts(bytes, 8, *brand))
}

// Two bytes match a frame sync often enough by chance, so a second frame must follow where the first one ends.
fn frames_repeat(bytes: &[u8], frame_length: fn(&[u8]) -> Option<usize>) -> bool {
    frame_length(bytes)
        .and_then(|length| bytes.get(length..))
        .is_some_and(|rest| frame_length(rest).is_some())
}

fn mpeg_audio_frame_length(header: &[u8]) -> Option<usize> {
    const V1_L1: [usize; 15] = [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448];
    const V1_L2: [usize; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384];
    const V1_L3: [usize; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const V2_L1: [usize; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256];
    const V2_L23: [usize; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

    let &[first, second, third, _] = header.get(..4)? else { return None };
    if first != 0xFF || second & 0xE0 != 0xE0 {
        return None;
    }
    let version = (second >> 3) & 3;
    let layer = (second >> 1) & 3;
    let bitrate_index = (third >> 4) as usize;
    let rate_index = ((third >> 2) & 3) as usize;
    let padding = ((third >> 1) & 1) as usize;
    if version == 1 || layer == 0 || bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
        return None;
    }
    let mpeg1 = version == 3;
    let table = match (mpeg1, layer) {
        (true, 3) => V1_L1,
        (true, 2) => V1_L2,
        (true, _) => V1_L3,
        (false, 3) => V2_L1,
        (false, _) => V2_L23,
    };
    let bitrate = table[bitrate_index] * 1000;
    let halvings = match version {
        3 => 0,
        2 => 1,
        _ => 2,
    };
    let sample_rate: usize = [44100, 48000, 32000][rate_index] >> halvings;
    let length = match layer {
        3 => (12 * bitrate / sample_rate + padding) * 4,
        2 => 144 * bitrate / sample_rate + padding,
        _ if mpeg1 => 144 * bitrate / sample_rate + padding,
        _ => 72 * bitrate / sample_rate + padding,
    };
    Some(length)
}

fn adts_frame_length(header: &[u8]) -> Option<usize> {
    let &[first, second, third, fourth, fifth, sixth] = header.get(..6)? else { return None };
    if first != 0xFF || second & 0xF6 != 0xF0 || (third >> 2) & 0x0F >= 13 {
        return None;
    }
    let length = ((fourth as usize & 3) << 11) | ((fifth as usize) << 3) | (sixth as usize >> 5);
    (length >= 7).then_some(length)
}

fn known_kind(bytes: &[u8]) -> Option<&'static str> {
    let text = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let text = &text[text.iter().take_while(|byte| byte.is_ascii_whitespace()).count()..];
    let lowercase_start: Vec<u8> = text.iter().take(9).map(u8::to_ascii_lowercase).collect();
    if lowercase_start.starts_with(b"<!doctype") || lowercase_start.starts_with(b"<html") {
        return Some("a web page");
    }
    if starts(bytes, 0, b"\x89PNG")
        || starts(bytes, 0, &[0xFF, 0xD8, 0xFF])
        || starts(bytes, 0, b"GIF8")
        || (starts(bytes, 0, b"RIFF") && starts(bytes, 8, b"WEBP"))
    {
        return Some("an image");
    }
    if starts(bytes, 0, b"%PDF") {
        return Some("a PDF");
    }
    if starts(bytes, 0, b"PK\x03\x04") {
        return Some("a zip file");
    }
    match text.first() {
        Some(b'{') | Some(b'[') => Some("data"),
        _ => None,
    }
}

/// Content types that can never be video or audio, so the link is rejected before reading its body.
pub fn content_type_rejection(content_type: &str) -> Option<&'static str> {
    let media_type = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match media_type.as_str() {
        "text/html" | "application/xhtml+xml" => Some("a web page"),
        "application/pdf" => Some("a PDF"),
        "application/json" | "text/json" | "application/xml" | "text/xml" => Some("data"),
        "application/zip"
        | "application/gzip"
        | "application/x-7z-compressed"
        | "application/vnd.rar"
        | "application/x-rar-compressed" => Some("an archive"),
        "text/css" | "text/javascript" | "application/javascript" => Some("a web page file"),
        other if other.starts_with("image/") => Some("an image"),
        other if other.starts_with("font/") => Some("a web page file"),
        _ => None,
    }
}

pub enum LinkCheck {
    Playable,
    NotPlayable(String),
    Failed(String),
}

/// Blocks on the network, so it runs off the UI thread.
pub fn check_link(url: &str) -> LinkCheck {
    let host = link_host(url);
    let response = match http_cache::agent().get(url).header("Range", "bytes=0-4095").call() {
        Ok(response) => response,
        Err(err) => return LinkCheck::Failed(http_cache::network_error_text(&err, &host)),
    };
    let content_type = http_cache::header(&response, "Content-Type").map(str::to_string);
    if let Some(kind) = content_type.as_deref().and_then(content_type_rejection) {
        return LinkCheck::NotPlayable(format!("{host} sent {kind}"));
    }
    if link_has_media_extension(&response.get_uri().to_string()) {
        return LinkCheck::Playable;
    }
    let mut bytes = Vec::new();
    if let Err(err) = response.into_body().into_reader().take(SNIFF_BYTES).read_to_end(&mut bytes) {
        return LinkCheck::Failed(http_cache::io_error_text(&err, &host));
    }
    match sniff(&bytes) {
        Sniff::Media => LinkCheck::Playable,
        Sniff::Known(kind) => LinkCheck::NotPlayable(format!("{host} sent {kind}")),
        Sniff::Unknown if bytes.is_empty() => LinkCheck::NotPlayable(format!("{host} sent an empty file")),
        Sniff::Unknown => match content_type {
            Some(content_type) => {
                LinkCheck::NotPlayable(format!("{host} sent a file we don't recognise ({content_type})"))
            }
            None => LinkCheck::NotPlayable(format!("{host} sent a file we don't recognise")),
        },
    }
}

pub enum FileCheck {
    Playable,
    NotFound,
    Unreadable(String),
    NotMedia,
}

pub fn check_file(path: &Path) -> FileCheck {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return FileCheck::NotFound,
        Err(err) => return FileCheck::Unreadable(system_reason(&err)),
    };
    if file.metadata().is_ok_and(|metadata| metadata.is_dir()) {
        return FileCheck::NotMedia;
    }
    if is_media_name(&path.to_string_lossy()) {
        return FileCheck::Playable;
    }
    let mut bytes = Vec::new();
    match (&mut file).take(SNIFF_BYTES).read_to_end(&mut bytes) {
        Ok(_) if sniff(&bytes) == Sniff::Media => FileCheck::Playable,
        Ok(_) => FileCheck::NotMedia,
        Err(err) => FileCheck::Unreadable(system_reason(&err)),
    }
}

fn system_reason(err: &std::io::Error) -> String {
    let text = err.to_string();
    match text.find(" (os error") {
        Some(index) => text[..index].to_string(),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings_and_fragments_do_not_count_as_the_extension() {
        assert!(link_has_media_extension("https://cdn.example.com/films/movie.mkv?token=abc.html"));
        assert!(!link_has_media_extension("https://example.com/watch?v=movie.mp4"));
        assert!(!link_has_media_extension("https://example.com/page#clip.webm"));
    }

    #[test]
    fn encoded_and_uppercase_extensions_are_recognised() {
        assert!(link_has_media_extension("https://example.com/My%20Holiday.MKV"));
        assert!(link_has_media_extension("https://example.com/a/b/song.FLAC"));
        assert!(!link_has_media_extension("https://example.mp4.com/"));
    }

    #[test]
    fn pasted_links_lose_chat_wrapping() {
        assert_eq!(parse_link("  <https://a.example/v.mp4>  "), Ok("https://a.example/v.mp4".into()));
        assert_eq!(parse_link("\"https://a.example/v.mp4\""), Ok("https://a.example/v.mp4".into()));
        assert_eq!(parse_link("HTTPS://a.example/v.mp4"), Ok("https://a.example/v.mp4".into()));
    }

    #[test]
    fn bad_links_get_the_matching_message() {
        assert_eq!(parse_link("www.example.com/video"), Err(LinkError::MissingScheme));
        assert_eq!(parse_link("ftp://files.example.com/a.mkv"), Err(LinkError::UnsupportedScheme));
        assert_eq!(parse_link("rtmp://live.example.com/stream"), Err(LinkError::UnsupportedScheme));
        assert_eq!(parse_link("just some words"), Err(LinkError::NotALink));
        assert_eq!(parse_link("https:///path"), Err(LinkError::NotALink));
    }

    #[test]
    fn host_ignores_credentials_and_port() {
        assert_eq!(link_host("https://user:pass@cdn.example.com:8443/a.mp4"), "cdn.example.com");
    }

    fn mp3_frame() -> Vec<u8> {
        // MPEG-1 Layer III, 128 kbps, 44.1 kHz, no padding: 417 bytes.
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        frame
    }

    #[test]
    fn mp3_without_id3_needs_two_frames() {
        let mut two = mp3_frame();
        two.extend(mp3_frame());
        assert_eq!(sniff(&two), Sniff::Media);
        let mut one = mp3_frame();
        one.extend(vec![0u8; 500]);
        assert_eq!(sniff(&one), Sniff::Unknown);
    }

    #[test]
    fn jpeg_is_not_mistaken_for_mpeg_audio() {
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
        jpeg.resize(4096, 0);
        assert_eq!(sniff(&jpeg), Sniff::Known("an image"));
    }

    #[test]
    fn heic_is_an_image_even_though_it_has_an_ftyp_box() {
        let heic = b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00mif1heic";
        assert_eq!(sniff(heic), Sniff::Known("an image"));
        let mp4 = b"\x00\x00\x00\x20ftypisom\x00\x00\x02\x00isomiso2avc1mp41";
        assert_eq!(sniff(mp4), Sniff::Media);
    }

    #[test]
    fn transport_streams_with_both_packet_sizes() {
        let mut ts = vec![0u8; 188 * 3];
        for packet in 0..3 {
            ts[packet * 188] = 0x47;
        }
        assert_eq!(sniff(&ts), Sniff::Media);
        let mut m2ts = vec![0u8; 192 * 3];
        for packet in 0..3 {
            m2ts[packet * 192 + 4] = 0x47;
        }
        assert_eq!(sniff(&m2ts), Sniff::Media);
    }

    #[test]
    fn web_pages_are_named_as_such() {
        assert_eq!(sniff(b"\xEF\xBB\xBF\n  <!DOCTYPE html><html>"), Sniff::Known("a web page"));
        assert_eq!(sniff(b"<HTML><head>"), Sniff::Known("a web page"));
    }

    #[test]
    fn content_type_only_rules_out() {
        assert_eq!(content_type_rejection("text/html; charset=UTF-8"), Some("a web page"));
        assert_eq!(content_type_rejection("image/webp"), Some("an image"));
        assert_eq!(content_type_rejection("application/octet-stream"), None);
        assert_eq!(content_type_rejection("text/plain"), None);
        assert_eq!(content_type_rejection("audio/webm"), None);
        assert_eq!(content_type_rejection("video/mp4"), None);
    }
}
