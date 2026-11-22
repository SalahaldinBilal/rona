use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::http_cache::{self, LinkReader};
use crate::logging::{debug, info, warn};

type Result<T> = std::result::Result<T, String>;

const EBML: u32 = 0x1A45_DFA3;
const DOC_TYPE: u32 = 0x4282;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const MUXING_APP: u32 = 0x4D80;
const WRITING_APP: u32 = 0x5741;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const DEFAULT_DURATION: u32 = 0x23_E383;
const ATTACHMENTS: u32 = 0x1941_A469;
const CHAPTERS: u32 = 0x1043_A770;
const TAGS: u32 = 0x1254_C367;
const CUES: u32 = 0x1C53_BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;
const CUE_DURATION: u32 = 0xB2;
const CLUSTER: u32 = 0x1F43_B675;
const CLUSTER_TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const BLOCK_DURATION: u32 = 0x9B;
const REFERENCE_BLOCK: u32 = 0xFB;

// Info children that describe the whole source file, wrong for a cut from it.
const DROPPED_INFO: [u32; 13] = [
    0xBF, 0xEC, DURATION, MUXING_APP, WRITING_APP, 0x73A4, 0x7384, 0x3C_B923, 0x3C_83AB, 0x3E_B923, 0x3E_83BB, 0x4444,
    0x6924,
];
const TOP_LEVEL: [u32; 9] = [EBML, SEGMENT, SEEK_HEAD, INFO, TRACKS, ATTACHMENTS, CHAPTERS, TAGS, CUES];

const DEFAULT_TIMESTAMP_SCALE: u64 = 1_000_000;
const READ_SIZE: usize = 1024 * 1024;
const FLAG_KEYFRAME: u8 = 0x80;
const FLAG_INVISIBLE: u8 = 0x08;
const FLAG_DISCARDABLE: u8 = 0x01;

// mpv's dump-cache drops B-frames from Matroska (it has no decode timestamps, which ffmpeg's muxer requires).
pub fn cut(
    source: &str,
    start: f64,
    end: f64,
    split_seconds: Option<f64>,
    part_path: impl Fn(u32) -> PathBuf,
    progress: impl FnMut(f32),
) -> (Vec<PathBuf>, Option<String>) {
    let mut reader = match Source::open(source) {
        Ok(source) => Reader::new(source),
        Err(err) => return (Vec::new(), Some(err)),
    };
    let layout = match Layout::read(&mut reader) {
        Ok(layout) => layout,
        Err(err) => return (Vec::new(), Some(err)),
    };
    let ticks = |seconds: f64| (seconds * 1e9 / layout.scale as f64).round() as i64;
    let mut boundaries = VecDeque::new();
    if let Some(length) = split_seconds {
        let length = length.max(1.0);
        let mut at = start + length;
        while at < end {
            boundaries.push_back(ticks(at));
            at += length;
        }
    }

    let mut cutter = Cutter::new(&layout, ticks(end), boundaries, part_path);
    let result = find_start(&mut reader, &layout, ticks(start)).and_then(|(scan_from, first)| {
        cutter.first = first;
        info!("cutting from byte {scan_from}, first keyframe at or after tick {first}, until tick {}", cutter.end);
        copy_clusters(&mut reader, &layout, scan_from, &mut cutter, progress)?;
        cutter.finish()
    });
    match result {
        Ok(()) => (cutter.written, None),
        Err(err) => {
            cutter.discard();
            (cutter.written, Some(err))
        }
    }
}

enum Source {
    File(File, Option<u64>),
    Link(LinkReader),
}

impl Source {
    fn open(source: &str) -> Result<Self> {
        if source.starts_with(http_cache::PROTOCOL) {
            return LinkReader::open(source).map(Source::Link);
        }
        let file = File::open(source).map_err(|err| format!("Could not open the file: {err}"))?;
        let size = file.metadata().ok().map(|metadata| metadata.len());
        Ok(Source::File(file, size))
    }

    fn size(&self) -> Option<u64> {
        match self {
            Source::File(_, size) => *size,
            Source::Link(link) => link.size(),
        }
    }

    fn read_at(&mut self, position: u64, buffer: &mut [u8]) -> Result<usize> {
        match self {
            Source::File(file, _) => {
                file.seek(SeekFrom::Start(position)).map_err(|err| format!("Could not read the file: {err}"))?;
                file.read(buffer).map_err(|err| format!("Could not read the file: {err}"))
            }
            Source::Link(link) => link.read_at(position, buffer),
        }
    }
}

struct Reader {
    source: Source,
    buffer: Vec<u8>,
    buffer_start: u64,
    position: u64,
}

impl Reader {
    fn new(source: Source) -> Self {
        Self { source, buffer: Vec::new(), buffer_start: 0, position: 0 }
    }

    fn seek(&mut self, position: u64) {
        self.position = position;
    }

    // Fills the buffer at the read position, false at the end of the file.
    fn fill(&mut self) -> Result<bool> {
        let buffer_end = self.buffer_start + self.buffer.len() as u64;
        if self.position >= self.buffer_start && self.position < buffer_end {
            return Ok(true);
        }
        self.buffer.resize(READ_SIZE, 0);
        let count = self.source.read_at(self.position, &mut self.buffer)?;
        self.buffer.truncate(count);
        self.buffer_start = self.position;
        Ok(count > 0)
    }

    fn at_end(&mut self, position: u64) -> Result<bool> {
        self.seek(position);
        Ok(!self.fill()?)
    }

    fn read(&mut self, out: &mut [u8]) -> Result<()> {
        let mut filled = 0;
        while filled < out.len() {
            if !self.fill()? {
                return Err("The file ends early.".into());
            }
            let offset = (self.position - self.buffer_start) as usize;
            let count = (out.len() - filled).min(self.buffer.len() - offset);
            out[filled..filled + count].copy_from_slice(&self.buffer[offset..offset + count]);
            filled += count;
            self.position += count as u64;
        }
        Ok(())
    }

    fn read_bytes(&mut self, count: u64) -> Result<Vec<u8>> {
        if self.source.size().is_some_and(|size| self.position.saturating_add(count) > size) {
            return Err("The file ends early.".into());
        }
        let mut bytes = vec![0; count as usize];
        self.read(&mut bytes)?;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8> {
        let mut byte = [0];
        self.read(&mut byte)?;
        Ok(byte[0])
    }

    fn header_at(&mut self, position: u64) -> Result<Header> {
        self.seek(position);
        let first = self.byte()?;
        let length = first.leading_zeros() as usize + 1;
        if length > 4 {
            return Err(format!("Broken element at byte {position}."));
        }
        let mut id = first as u32;
        for _ in 1..length {
            id = id << 8 | self.byte()? as u32;
        }
        let first = self.byte()?;
        let length = first.leading_zeros() as usize + 1;
        if length > 8 {
            return Err(format!("Broken element size at byte {position}."));
        }
        let mut size = (first as u64) & (0xFF >> length);
        for _ in 1..length {
            size = size << 8 | self.byte()? as u64;
        }
        let unknown = size == (1u64 << (7 * length)) - 1;
        Ok(Header { id, size: (!unknown).then_some(size), start: position, data: self.position })
    }

    // The whole element, header included.
    fn raw_at(&mut self, position: u64) -> Result<(Header, Vec<u8>)> {
        let header = self.header_at(position)?;
        let size = header.size.ok_or_else(|| format!("Element at byte {position} has no size."))?;
        self.seek(header.start);
        let raw = self.read_bytes(header.data - header.start + size)?;
        Ok((header, raw))
    }

    fn data_at(&mut self, position: u64) -> Result<(Header, Vec<u8>)> {
        let (header, mut raw) = self.raw_at(position)?;
        raw.drain(..(header.data - header.start) as usize);
        Ok((header, raw))
    }
}

struct Header {
    id: u32,
    size: Option<u64>,
    start: u64,
    data: u64,
}

impl Header {
    fn end(&self) -> Option<u64> {
        self.size.map(|size| self.data + size)
    }
}

// Children of an element already in memory, as (id, data, whole element).
fn children(data: &[u8]) -> Result<Vec<(u32, &[u8], &[u8])>> {
    let mut list = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let (id, id_length) = vint(data, at, true)?;
        let (size, size_length) = vint(data, at + id_length, false)?;
        let start = at + id_length + size_length;
        let end = start.checked_add(size as usize).filter(|end| *end <= data.len()).ok_or("Broken element.")?;
        list.push((id as u32, &data[start..end], &data[at..end]));
        at = end;
    }
    Ok(list)
}

fn vint(data: &[u8], at: usize, keep_marker: bool) -> Result<(u64, usize)> {
    let first = *data.get(at).ok_or("Broken element.")?;
    let length = first.leading_zeros() as usize + 1;
    if length > 8 || at + length > data.len() {
        return Err("Broken element.".into());
    }
    let mut value = if keep_marker { first as u64 } else { (first as u64) & (0xFF >> length) };
    for byte in &data[at + 1..at + length] {
        value = value << 8 | *byte as u64;
    }
    Ok((value, length))
}

fn uint(data: &[u8]) -> u64 {
    data.iter().fold(0, |value, byte| value << 8 | *byte as u64)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Video,
    Audio,
    Subtitle,
    Other,
}

struct Track {
    kind: Kind,
    default_duration: Option<i64>,
}

struct Cue {
    time: i64,
    track: u64,
    cluster: u64,
    duration: Option<i64>,
}

struct Layout {
    ebml: Vec<u8>,
    segment_end: u64,
    scale: u64,
    info: Vec<u8>,
    tracks_raw: Vec<u8>,
    attachments: Option<Vec<u8>>,
    tracks: HashMap<u64, Track>,
    key_track: u64,
    cues: Vec<Cue>,
    first_cluster: Option<u64>,
}

impl Layout {
    fn read(reader: &mut Reader) -> Result<Self> {
        let (header, ebml) = reader.raw_at(0)?;
        if header.id != EBML {
            return Err("Not a Matroska file.".into());
        }
        let doc_type = children(&ebml[(header.data - header.start) as usize..])?
            .into_iter()
            .find(|(id, ..)| *id == DOC_TYPE)
            .map(|(_, data, _)| String::from_utf8_lossy(data).trim_end_matches('\0').to_string())
            .unwrap_or_default();
        if doc_type != "matroska" && doc_type != "webm" {
            return Err(format!("Not a Matroska file ({doc_type})."));
        }

        let mut position = header.end().unwrap_or(0);
        let segment = loop {
            let header = reader.header_at(position)?;
            if header.id == SEGMENT {
                break header;
            }
            position = header.end().ok_or("Broken file start.")?;
        };
        let segment_data = segment.data;
        let segment_end = segment.end().or(reader.source.size()).unwrap_or(u64::MAX);

        // Everything before the first cluster, then whatever the seek heads point to past it.
        let mut found: HashMap<u32, u64> = HashMap::new();
        let mut first_cluster = None;
        let mut seek_heads = Vec::new();
        let mut position = segment_data;
        while position < segment_end && !reader.at_end(position)? {
            let header = reader.header_at(position)?;
            match header.id {
                CLUSTER => {
                    first_cluster = Some(position);
                    break;
                }
                SEEK_HEAD => seek_heads.push(position),
                INFO | TRACKS | ATTACHMENTS | CUES => {
                    found.entry(header.id).or_insert(position);
                }
                _ => {}
            }
            let Some(end) = header.end() else { break };
            position = end;
        }
        let mut visited = HashSet::new();
        while let Some(position) = seek_heads.pop() {
            if !visited.insert(position) {
                continue;
            }
            let (_, data) = reader.data_at(position)?;
            for (id, seek, _) in children(&data)? {
                if id != SEEK {
                    continue;
                }
                let fields = children(seek)?;
                let target = fields.iter().find(|(id, ..)| *id == SEEK_ID).map(|(_, data, _)| uint(data) as u32);
                let at = fields.iter().find(|(id, ..)| *id == SEEK_POSITION).map(|(_, data, _)| uint(data));
                let (Some(target), Some(at)) = (target, at) else { continue };
                if target == SEEK_HEAD {
                    seek_heads.push(segment_data + at);
                } else {
                    found.entry(target).or_insert(segment_data + at);
                }
            }
        }

        let info_at = *found.get(&INFO).ok_or("The file has no segment info.")?;
        let (_, info_data) = reader.data_at(info_at)?;
        let mut scale = DEFAULT_TIMESTAMP_SCALE;
        let mut info = Vec::new();
        for (id, data, raw) in children(&info_data)? {
            if id == TIMESTAMP_SCALE {
                scale = uint(data).max(1);
            }
            if !DROPPED_INFO.contains(&id) {
                info.extend_from_slice(raw);
            }
        }

        let tracks_at = *found.get(&TRACKS).ok_or("The file has no track list.")?;
        let (header, tracks_raw) = reader.raw_at(tracks_at)?;
        let mut tracks = HashMap::new();
        let mut order = Vec::new();
        for (id, entry, _) in children(&tracks_raw[(header.data - header.start) as usize..])? {
            if id != TRACK_ENTRY {
                continue;
            }
            let fields = children(entry)?;
            let field = |wanted: u32| fields.iter().find(|(id, ..)| *id == wanted).map(|(_, data, _)| uint(data));
            let Some(number) = field(TRACK_NUMBER) else { continue };
            let kind = match field(TRACK_TYPE) {
                Some(1) => Kind::Video,
                Some(2) => Kind::Audio,
                Some(0x11) => Kind::Subtitle,
                _ => Kind::Other,
            };
            let default_duration = field(DEFAULT_DURATION).map(|ns| (ns / scale) as i64);
            tracks.insert(number, Track { kind, default_duration });
            order.push((number, kind));
        }
        let key_track = order
            .iter()
            .find(|(_, kind)| *kind == Kind::Video)
            .or_else(|| order.iter().find(|(_, kind)| *kind == Kind::Audio))
            .map(|(number, _)| *number)
            .ok_or("The file has no video or audio track.")?;

        let attachments = match found.get(&ATTACHMENTS) {
            Some(at) => Some(reader.raw_at(*at)?.1),
            None => None,
        };

        let mut cues = Vec::new();
        if let Some(at) = found.get(&CUES) {
            let (_, data) = reader.data_at(*at)?;
            for (id, point, _) in children(&data)? {
                if id != CUE_POINT {
                    continue;
                }
                let fields = children(point)?;
                let Some(time) = fields.iter().find(|(id, ..)| *id == CUE_TIME).map(|(_, data, _)| uint(data) as i64)
                else {
                    continue;
                };
                for (id, positions, _) in &fields {
                    if *id != CUE_TRACK_POSITIONS {
                        continue;
                    }
                    let fields = children(positions)?;
                    let field = |wanted: u32| fields.iter().find(|(id, ..)| *id == wanted).map(|(_, data, _)| uint(data));
                    let (Some(track), Some(cluster)) = (field(CUE_TRACK), field(CUE_CLUSTER_POSITION)) else { continue };
                    let duration = field(CUE_DURATION).map(|duration| duration as i64);
                    cues.push(Cue { time, track, cluster: segment_data + cluster, duration });
                }
            }
        }
        debug!(
            "matroska layout: {} tracks, key track {key_track}, timestamp scale {scale}, {} cues, attachments {}",
            tracks.len(),
            cues.len(),
            attachments.is_some()
        );

        Ok(Self {
            ebml,
            segment_end,
            scale,
            info,
            tracks_raw,
            attachments,
            tracks,
            key_track,
            cues,
            first_cluster,
        })
    }

    fn kind(&self, track: u64) -> Kind {
        self.tracks.get(&track).map_or(Kind::Other, |track| track.kind)
    }

    fn key_is_video(&self) -> bool {
        self.kind(self.key_track) == Kind::Video
    }

    fn end_of(&self, block: &Block) -> Option<i64> {
        let duration = block.duration.or_else(|| self.tracks.get(&block.track)?.default_duration)?;
        Some(block.pts + duration)
    }
}

#[derive(Clone)]
struct Block {
    track: u64,
    pts: i64,
    keyframe: bool,
    duration: Option<i64>,
    // The SimpleBlock or Block payload: track number, relative timestamp, flags, frames.
    frame: Vec<u8>,
    simple: bool,
    // BlockGroup children other than the Block and its BlockDuration, kept as they are.
    extra: Vec<u8>,
}

impl Block {
    fn parse(id: u32, data: Vec<u8>, cluster_time: u64) -> Result<Self> {
        let (frame, simple, duration, referenced, extra) = if id == SIMPLE_BLOCK {
            (data, true, None, false, Vec::new())
        } else {
            let mut frame = None;
            let (mut duration, mut referenced, mut extra) = (None, false, Vec::new());
            for (id, child, raw) in children(&data)? {
                match id {
                    BLOCK => frame = Some(child.to_vec()),
                    BLOCK_DURATION => duration = Some(uint(child) as i64),
                    _ => {
                        referenced |= id == REFERENCE_BLOCK;
                        extra.extend_from_slice(raw);
                    }
                }
            }
            (frame.ok_or("A block group has no block.")?, false, duration, referenced, extra)
        };
        let (track, offset) = vint(&frame, 0, false)?;
        if frame.len() < offset + 3 {
            return Err("Broken block.".into());
        }
        let relative = i16::from_be_bytes([frame[offset], frame[offset + 1]]);
        let keyframe = if simple { frame[offset + 2] & FLAG_KEYFRAME != 0 } else { !referenced };
        Ok(Self { track, pts: cluster_time as i64 + relative as i64, keyframe, duration, frame, simple, extra })
    }
}

// The blocks of the cluster at `header`, and where the next element starts.
fn read_cluster(reader: &mut Reader, header: &Header, layout: &Layout) -> Result<(Vec<Block>, u64)> {
    let end = header.end().map_or(layout.segment_end, |end| end.min(layout.segment_end));
    let mut position = header.data;
    let mut time = None;
    let mut blocks = Vec::new();
    while position < end && !reader.at_end(position)? {
        let child = reader.header_at(position)?;
        // A cluster of unknown size ends where the next top-level element starts.
        if header.size.is_none() && (child.id == CLUSTER || TOP_LEVEL.contains(&child.id)) {
            return Ok((blocks, position));
        }
        let size = child.size.ok_or_else(|| format!("Element at byte {position} has no size."))?;
        match child.id {
            CLUSTER_TIMESTAMP => time = Some(uint(&reader.read_bytes(size)?)),
            SIMPLE_BLOCK | BLOCK_GROUP => {
                let data = reader.read_bytes(size)?;
                let time = time.ok_or_else(|| format!("The cluster at byte {} has no timestamp.", header.start))?;
                blocks.push(Block::parse(child.id, data, time)?);
            }
            _ => {}
        }
        position = child.data + size;
    }
    Ok((blocks, header.end().unwrap_or(position)))
}

// Where to start reading, and the time of the keyframe the cut starts at (the first one at or after it).
fn find_start(reader: &mut Reader, layout: &Layout, start: i64) -> Result<(u64, i64)> {
    let first_cluster = layout.first_cluster;
    let key_cues: Vec<&Cue> = layout.cues.iter().filter(|cue| cue.track == layout.key_track).collect();
    if !key_cues.is_empty() {
        let Some(cue) = key_cues.iter().filter(|cue| cue.time <= start).max_by_key(|cue| cue.time) else {
            return Ok((first_cluster.unwrap_or(key_cues[0].cluster), i64::MIN));
        };
        // Subtitle lines still on screen at the keyframe can sit in earlier clusters.
        let earliest = layout
            .cues
            .iter()
            .filter(|other| layout.kind(other.track) == Kind::Subtitle)
            .filter(|other| other.time <= cue.time && other.duration.is_some_and(|duration| other.time + duration > cue.time))
            .map(|other| other.cluster)
            .fold(cue.cluster, u64::min);
        return Ok((earliest, cue.time));
    }

    // Without an index, walk the cluster headers and look back from the cluster the start falls in.
    let first_cluster = first_cluster.ok_or("The file has no clusters.")?;
    warn!("the file has no index, reading every cluster header to find the start");
    let mut clusters = Vec::new();
    let mut position = first_cluster;
    while position < layout.segment_end && !reader.at_end(position)? {
        let header = reader.header_at(position)?;
        if header.id != CLUSTER {
            let Some(end) = header.end() else { break };
            position = end;
            continue;
        }
        let time = cluster_time(reader, &header)?;
        if time.is_some_and(|time| time as i64 > start) {
            break;
        }
        clusters.push(position);
        position = match header.end() {
            Some(end) => end,
            None => read_cluster(reader, &header, layout)?.1,
        };
    }
    for position in clusters.iter().rev() {
        let header = reader.header_at(*position)?;
        let (blocks, _) = read_cluster(reader, &header, layout)?;
        let keyframe = blocks
            .iter()
            .filter(|block| block.track == layout.key_track && block.keyframe && block.pts <= start)
            .map(|block| block.pts)
            .max();
        if let Some(keyframe) = keyframe {
            return Ok((*position, keyframe));
        }
    }
    Ok((first_cluster, i64::MIN))
}

fn cluster_time(reader: &mut Reader, header: &Header) -> Result<Option<u64>> {
    let mut position = header.data;
    while header.end().is_none_or(|end| position < end) {
        let child = reader.header_at(position)?;
        let Some(size) = child.size else { return Ok(None) };
        if child.id == CLUSTER_TIMESTAMP {
            return Ok(Some(uint(&reader.read_bytes(size)?)));
        }
        if child.id == SIMPLE_BLOCK || child.id == BLOCK_GROUP || TOP_LEVEL.contains(&child.id) || child.id == CLUSTER {
            return Ok(None);
        }
        position = child.data + size;
    }
    Ok(None)
}

fn copy_clusters<P: Fn(u32) -> PathBuf>(
    reader: &mut Reader,
    layout: &Layout,
    from: u64,
    cutter: &mut Cutter<'_, P>,
    mut progress: impl FnMut(f32),
) -> Result<()> {
    let mut position = from;
    let mut shown = -1;
    while !cutter.done() && position < layout.segment_end && !reader.at_end(position)? {
        let header = reader.header_at(position)?;
        if header.id != CLUSTER {
            let Some(end) = header.end() else { break };
            position = end;
            continue;
        }
        let (blocks, next) = read_cluster(reader, &header, layout)?;
        for block in blocks {
            cutter.feed(block)?;
            if cutter.done() {
                break;
            }
        }
        let percent = (cutter.progress() * 100.0) as i32;
        if percent != shown {
            shown = percent;
            progress(cutter.progress());
        }
        position = next;
    }
    Ok(())
}

#[derive(PartialEq)]
enum State {
    Before,
    Running,
    // Past the end, holding the key track's blocks until it's clear which ones frames before the end need.
    Tail,
    Done,
}

struct Previous {
    part: Part,
    // Audio interleaved just after the next part's keyframe can still be shown before it.
    waiting_on: HashSet<u64>,
}

struct Cutter<'a, P: Fn(u32) -> PathBuf> {
    layout: &'a Layout,
    first: i64,
    end: i64,
    boundaries: VecDeque<i64>,
    part_path: P,
    state: State,
    early: Vec<Block>,
    subtitles: Vec<Block>,
    tail: Vec<Block>,
    current: Option<Part>,
    previous: Option<Previous>,
    started: u32,
    written: Vec<PathBuf>,
    progress_from: i64,
    progress_at: i64,
}

impl<'a, P: Fn(u32) -> PathBuf> Cutter<'a, P> {
    fn new(layout: &'a Layout, end: i64, boundaries: VecDeque<i64>, part_path: P) -> Self {
        Self {
            layout,
            first: i64::MIN,
            end,
            boundaries,
            part_path,
            state: State::Before,
            early: Vec::new(),
            subtitles: Vec::new(),
            tail: Vec::new(),
            current: None,
            previous: None,
            started: 0,
            written: Vec::new(),
            progress_from: 0,
            progress_at: 0,
        }
    }

    fn done(&self) -> bool {
        self.state == State::Done
    }

    fn progress(&self) -> f32 {
        if self.state == State::Before || self.end <= self.progress_from {
            return 0.0;
        }
        ((self.progress_at - self.progress_from) as f32 / (self.end - self.progress_from) as f32).clamp(0.0, 1.0)
    }

    fn feed(&mut self, block: Block) -> Result<()> {
        let is_key = block.track == self.layout.key_track;
        if is_key && block.keyframe {
            let layout = self.layout;
            self.subtitles.retain(|sub| layout.end_of(sub).is_some_and(|end| end > block.pts));
        }
        if self.layout.kind(block.track) == Kind::Subtitle && self.layout.end_of(&block).is_some() {
            self.subtitles.push(block.clone());
        }
        if is_key {
            self.progress_at = block.pts;
        }

        match self.state {
            State::Done => {}
            State::Before => {
                if is_key && block.keyframe && block.pts >= self.first && block.pts < self.end {
                    self.progress_from = block.pts;
                    self.boundaries.retain(|boundary| *boundary > block.pts);
                    self.start_part(block)?;
                    self.state = State::Running;
                } else if is_key && block.keyframe && block.pts >= self.end {
                    self.state = State::Done;
                } else if !is_key && block.pts >= self.first {
                    self.early.push(block);
                }
            }
            State::Running => {
                if is_key && block.pts >= self.end {
                    if block.keyframe {
                        self.state = State::Done;
                    } else {
                        self.state = State::Tail;
                        self.tail.push(block);
                    }
                } else if is_key && block.keyframe && self.boundaries.front().is_some_and(|at| block.pts >= *at) {
                    while self.boundaries.front().is_some_and(|at| block.pts >= *at) {
                        self.boundaries.pop_front();
                    }
                    self.start_part(block)?;
                } else {
                    self.route(block, false)?;
                }
            }
            State::Tail => {
                if is_key && block.keyframe {
                    self.flush_tail()?;
                    self.state = State::Done;
                } else {
                    self.tail.push(block);
                }
            }
        }
        Ok(())
    }

    fn start_part(&mut self, keyframe: Block) -> Result<()> {
        let start = keyframe.pts;
        if let Some(previous) = self.previous.take() {
            self.close(previous.part)?;
        }
        if let Some(part) = self.current.take() {
            let waiting_on: HashSet<u64> = self
                .layout
                .tracks
                .iter()
                .filter(|(number, track)| track.kind == Kind::Audio && **number != self.layout.key_track)
                .map(|(number, _)| *number)
                .collect();
            if waiting_on.is_empty() {
                self.close(part)?;
            } else {
                self.previous = Some(Previous { part, waiting_on });
            }
        }

        self.started += 1;
        let path = (self.part_path)(self.started);
        info!("starting part {} at tick {start}: {}", self.started, path.display());
        let mut part = Part::create(path, self.layout, start)?;
        part.write(&keyframe, self.layout, false, None)?;
        for sub in &self.subtitles {
            if sub.pts < start && self.layout.end_of(sub).is_some_and(|end| end > start) {
                part.write(sub, self.layout, false, Some(start))?;
            }
        }
        self.current = Some(part);
        for block in std::mem::take(&mut self.early) {
            self.route(block, false)?;
        }
        Ok(())
    }

    fn route(&mut self, block: Block, invisible: bool) -> Result<()> {
        let is_key = block.track == self.layout.key_track;
        let Some(part) = self.current.as_mut() else { return Ok(()) };
        if block.pts >= part.start {
            if !is_key && block.pts >= self.end {
                return Ok(());
            }
            part.write(&block, self.layout, invisible, None)?;
            if let Some(previous) = self.previous.as_mut() {
                previous.waiting_on.remove(&block.track);
                if previous.waiting_on.is_empty() {
                    let previous = self.previous.take().expect("checked above");
                    self.close(previous.part)?;
                }
            }
        } else if is_key {
            // Decoded after the part's keyframe but shown before it, so it refers to frames this part doesn't have.
            debug!("dropping a leading frame at tick {} before part start {}", block.pts, part.start);
        } else if let Some(previous) = self.previous.as_mut().filter(|previous| block.pts >= previous.part.start) {
            previous.part.write(&block, self.layout, false, None)?;
        }
        Ok(())
    }

    // Frames shown after the end are kept, invisible, when a frame shown before it is decoded from them.
    fn flush_tail(&mut self) -> Result<()> {
        let key_track = self.layout.key_track;
        let last = self.tail.iter().rposition(|block| block.track == key_track && block.pts < self.end);
        for (index, block) in std::mem::take(&mut self.tail).into_iter().enumerate() {
            if block.track != key_track {
                self.route(block, false)?;
            } else if last.is_some_and(|last| index <= last) {
                let invisible = block.pts >= self.end;
                self.route(block, invisible)?;
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.state == State::Tail {
            self.flush_tail()?;
        }
        self.state = State::Done;
        if let Some(previous) = self.previous.take() {
            self.close(previous.part)?;
        }
        if let Some(part) = self.current.take() {
            self.close(part)?;
        }
        if self.started == 0 {
            return Err("No keyframe in the recorded range.".into());
        }
        Ok(())
    }

    fn close(&mut self, part: Part) -> Result<()> {
        let path = part.finish(self.layout)?;
        self.written.push(path);
        Ok(())
    }

    // A part left half written would look like a finished file.
    fn discard(&mut self) {
        let parts = self.previous.take().map(|previous| previous.part).into_iter().chain(self.current.take());
        for part in parts {
            let path = part.path.clone();
            drop(part);
            if let Err(err) = std::fs::remove_file(&path) {
                warn!("could not remove the unfinished {}: {err}", path.display());
            }
        }
    }
}

struct Cluster {
    time: i64,
    body: Vec<u8>,
}

struct Part {
    path: PathBuf,
    file: BufWriter<File>,
    written: u64,
    segment_size_at: u64,
    segment_data: u64,
    seek_entries: Vec<(u32, u64)>,
    element_at: HashMap<u32, u64>,
    duration_at: u64,
    // The source time shown at 0 in this part.
    start: i64,
    cluster: Option<Cluster>,
    cues: Vec<(i64, u64)>,
    last_end: i64,
}

impl Part {
    fn create(path: PathBuf, layout: &Layout, start: i64) -> Result<Self> {
        let file = File::create(&path).map_err(|err| format!("Could not create {}: {err}", path.display()))?;
        let mut part = Self {
            path,
            file: BufWriter::new(file),
            written: 0,
            segment_size_at: 0,
            segment_data: 0,
            seek_entries: Vec::new(),
            element_at: HashMap::new(),
            duration_at: 0,
            start,
            cluster: None,
            cues: Vec::new(),
            last_end: 0,
        };

        part.put(&layout.ebml)?;
        part.put(&id_bytes(SEGMENT))?;
        part.segment_size_at = part.written;
        // Unknown size until the end, so a file cut short by a crash still plays.
        part.put(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])?;
        part.segment_data = part.written;

        let mut targets = vec![INFO, TRACKS];
        if layout.attachments.is_some() {
            targets.push(ATTACHMENTS);
        }
        targets.push(CUES);
        let mut body = Vec::new();
        let mut value_offsets = Vec::new();
        for target in &targets {
            let mut seek = element(SEEK_ID, &id_bytes(*target));
            seek.extend_from_slice(&id_bytes(SEEK_POSITION));
            seek.push(0x88);
            let value_at = seek.len();
            seek.extend_from_slice(&[0; 8]);
            let mut entry = id_bytes(SEEK);
            entry.extend_from_slice(&size_bytes(seek.len() as u64));
            value_offsets.push(body.len() + entry.len() + value_at);
            entry.extend_from_slice(&seek);
            body.extend_from_slice(&entry);
        }
        let mut seek_head = id_bytes(SEEK_HEAD);
        seek_head.extend_from_slice(&size_bytes(body.len() as u64));
        let body_at = part.written + seek_head.len() as u64;
        seek_head.extend_from_slice(&body);
        part.seek_entries = targets.iter().zip(value_offsets).map(|(id, offset)| (*id, body_at + offset as u64)).collect();
        part.put(&seek_head)?;

        let mut info = layout.info.clone();
        info.extend_from_slice(&element(MUXING_APP, b"Rona"));
        info.extend_from_slice(&element(WRITING_APP, format!("Rona {}", env!("CARGO_PKG_VERSION")).as_bytes()));
        info.extend_from_slice(&id_bytes(DURATION));
        info.push(0x88);
        let duration_offset = info.len();
        info.extend_from_slice(&[0; 8]);
        let mut info_element = id_bytes(INFO);
        info_element.extend_from_slice(&size_bytes(info.len() as u64));
        part.element_at.insert(INFO, part.written - part.segment_data);
        part.duration_at = part.written + info_element.len() as u64 + duration_offset as u64;
        info_element.extend_from_slice(&info);
        part.put(&info_element)?;

        part.element_at.insert(TRACKS, part.written - part.segment_data);
        part.put(&layout.tracks_raw)?;
        if let Some(attachments) = &layout.attachments {
            part.element_at.insert(ATTACHMENTS, part.written - part.segment_data);
            part.put(attachments)?;
        }
        Ok(part)
    }

    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.file.write_all(bytes).map_err(|err| format!("Could not write {}: {err}", self.path.display()))?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    // `clamp_to` moves a block that started before this part to its start, shortening it to end at the same time.
    fn write(&mut self, block: &Block, layout: &Layout, invisible: bool, clamp_to: Option<i64>) -> Result<()> {
        let (pts, duration) = match clamp_to {
            Some(start) => (start, layout.end_of(block).map(|end| end - start)),
            None => (block.pts, block.duration),
        };
        let time = pts - self.start;
        let starts_cluster = block.track == layout.key_track && block.keyframe;
        let fits = |cluster: &Cluster| (i16::MIN as i64..=i16::MAX as i64).contains(&(time - cluster.time));
        let new_cluster = match &self.cluster {
            None => true,
            Some(cluster) => (starts_cluster && layout.key_is_video()) || !fits(cluster),
        };
        if new_cluster {
            self.flush_cluster()?;
            if starts_cluster {
                self.cues.push((time, self.written - self.segment_data));
            }
            self.cluster = Some(Cluster { time, body: Vec::new() });
        }
        let cluster = self.cluster.as_mut().expect("set above");

        let (_, offset) = vint(&block.frame, 0, false)?;
        let mut frame = block.frame.clone();
        frame[offset..offset + 2].copy_from_slice(&((time - cluster.time) as i16).to_be_bytes());
        if invisible {
            frame[offset + 2] |= FLAG_INVISIBLE;
        }
        if block.simple && clamp_to.is_none() {
            cluster.body.extend_from_slice(&element(SIMPLE_BLOCK, &frame));
        } else {
            if block.simple {
                frame[offset + 2] &= !(FLAG_KEYFRAME | FLAG_DISCARDABLE);
            }
            let mut group = element(BLOCK, &frame);
            if let Some(duration) = duration {
                group.extend_from_slice(&element(BLOCK_DURATION, &uint_bytes(duration.max(0) as u64)));
            }
            group.extend_from_slice(&block.extra);
            cluster.body.extend_from_slice(&element(BLOCK_GROUP, &group));
        }

        let length = duration.or_else(|| layout.tracks.get(&block.track)?.default_duration).unwrap_or(0);
        self.last_end = self.last_end.max(time + length);
        Ok(())
    }

    fn flush_cluster(&mut self) -> Result<()> {
        let Some(cluster) = self.cluster.take() else { return Ok(()) };
        let mut body = element(CLUSTER_TIMESTAMP, &uint_bytes(cluster.time.max(0) as u64));
        body.extend_from_slice(&cluster.body);
        self.put(&element(CLUSTER, &body))
    }

    fn finish(mut self, layout: &Layout) -> Result<PathBuf> {
        self.flush_cluster()?;
        let mut points = Vec::new();
        for (time, position) in &self.cues {
            let mut positions = element(CUE_TRACK, &uint_bytes(layout.key_track));
            positions.extend_from_slice(&element(CUE_CLUSTER_POSITION, &uint_bytes(*position)));
            let mut point = element(CUE_TIME, &uint_bytes((*time).max(0) as u64));
            point.extend_from_slice(&element(CUE_TRACK_POSITIONS, &positions));
            points.extend_from_slice(&element(CUE_POINT, &point));
        }
        self.element_at.insert(CUES, self.written - self.segment_data);
        self.put(&element(CUES, &points))?;

        let segment_size = self.written - self.segment_data;
        let error = |err: std::io::Error| format!("Could not write {}: {err}", self.path.display());
        let mut file = self.file.into_inner().map_err(|err| err.into_error()).map_err(error)?;
        let mut patch = |at: u64, bytes: &[u8]| -> std::io::Result<()> {
            file.seek(SeekFrom::Start(at))?;
            file.write_all(bytes)
        };
        let mut size = segment_size.to_be_bytes();
        size[0] = 0x01;
        patch(self.segment_size_at, &size).map_err(error)?;
        for (id, at) in &self.seek_entries {
            let position = self.element_at.get(id).copied().unwrap_or(0);
            patch(*at, &position.to_be_bytes()).map_err(error)?;
        }
        patch(self.duration_at, &(self.last_end as f64).to_be_bytes()).map_err(error)?;
        file.sync_all().map_err(error)?;
        info!("wrote {}, {} bytes, {} ticks", self.path.display(), self.written, self.last_end);
        Ok(self.path)
    }
}

fn id_bytes(id: u32) -> Vec<u8> {
    let bytes = id.to_be_bytes();
    let skip = bytes.iter().take_while(|byte| **byte == 0).count().min(3);
    bytes[skip..].to_vec()
}

fn size_bytes(size: u64) -> Vec<u8> {
    let length = (1..=8).find(|length| size < (1u64 << (7 * length)) - 1).unwrap_or(8);
    let value = size | (1u64 << (7 * length));
    value.to_be_bytes()[8 - length..].to_vec()
}

fn uint_bytes(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let skip = bytes.iter().take_while(|byte| **byte == 0).count().min(7);
    bytes[skip..].to_vec()
}

fn element(id: u32, data: &[u8]) -> Vec<u8> {
    let mut bytes = id_bytes(id);
    bytes.extend_from_slice(&size_bytes(data.len() as u64));
    bytes.extend_from_slice(data);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    const FRAME: i64 = 40;
    const GOP: i64 = 12;
    const GOPS: i64 = 10;
    const AUDIO: i64 = 20;
    // Frame indices of one GOP in decode order, as an encoder using B-frames stores them.
    const DECODE_ORDER: [i64; 12] = [0, 3, 1, 2, 6, 4, 5, 9, 7, 8, 11, 10];
    const SUBTITLES: [(i64, i64); 2] = [(900, 1000), (1500, 300)];

    fn frame(track: u64, relative: i64, flags: u8, marker: i64) -> Vec<u8> {
        let mut frame = vec![0x80 | track as u8];
        frame.extend_from_slice(&(relative as i16).to_be_bytes());
        frame.push(flags);
        frame.extend_from_slice(&marker.to_be_bytes());
        frame
    }

    fn seek_head(entries: &[(u32, u64)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (id, position) in entries {
            let mut seek = element(SEEK_ID, &id_bytes(*id));
            seek.extend_from_slice(&element(SEEK_POSITION, &position.to_be_bytes()));
            body.extend_from_slice(&element(SEEK, &seek));
        }
        element(SEEK_HEAD, &body)
    }

    fn track(number: u64, kind: u64, default_duration: Option<u64>) -> Vec<u8> {
        let mut entry = element(TRACK_NUMBER, &uint_bytes(number));
        entry.extend_from_slice(&element(TRACK_TYPE, &uint_bytes(kind)));
        if let Some(duration) = default_duration {
            entry.extend_from_slice(&element(DEFAULT_DURATION, &uint_bytes(duration)));
        }
        element(TRACK_ENTRY, &entry)
    }

    // Laid out like an mkvmerge file: one cluster per GOP, audio interleaved by decode time, index at the end.
    fn write_source(path: &Path, with_index: bool) {
        let mut info = element(TIMESTAMP_SCALE, &uint_bytes(1_000_000));
        info.extend_from_slice(&element(DURATION, &((GOP * GOPS * FRAME) as f64).to_be_bytes()));
        let info = element(INFO, &info);
        let mut tracks = track(1, 1, Some(40_000_000));
        tracks.extend_from_slice(&track(2, 2, Some(20_000_000)));
        tracks.extend_from_slice(&track(3, 0x11, None));
        let tracks = element(TRACKS, &tracks);

        let mut targets = vec![INFO, TRACKS];
        if with_index {
            targets.push(CUES);
        }
        let head_length = seek_head(&targets.iter().map(|id| (*id, 0)).collect::<Vec<_>>()).len() as u64;
        let info_at = head_length;
        let tracks_at = info_at + info.len() as u64;
        let mut position = tracks_at + tracks.len() as u64;
        let mut clusters = Vec::new();
        let mut cues: Vec<(i64, u64, u64, Option<i64>)> = Vec::new();
        for gop in 0..GOPS {
            let base = gop * GOP * FRAME;
            let mut body = element(CLUSTER_TIMESTAMP, &uint_bytes(base as u64));
            cues.push((base, 1, position, None));
            for (step, index) in DECODE_ORDER.iter().enumerate() {
                let pts = base + index * FRAME;
                let flags = if *index == 0 { FLAG_KEYFRAME } else { 0 };
                body.extend_from_slice(&element(SIMPLE_BLOCK, &frame(1, pts - base, flags, pts)));
                let from = base + step as i64 * FRAME;
                for audio in (from..from + FRAME).step_by(AUDIO as usize) {
                    body.extend_from_slice(&element(SIMPLE_BLOCK, &frame(2, audio - base, FLAG_KEYFRAME, audio)));
                }
                for (start, duration) in SUBTITLES.iter().filter(|(start, _)| (from..from + FRAME).contains(start)) {
                    let mut group = element(BLOCK, &frame(3, start - base, 0, *start));
                    group.extend_from_slice(&element(BLOCK_DURATION, &uint_bytes(*duration as u64)));
                    body.extend_from_slice(&element(BLOCK_GROUP, &group));
                    cues.push((*start, 3, position, Some(*duration)));
                }
            }
            let cluster = element(CLUSTER, &body);
            position += cluster.len() as u64;
            clusters.extend_from_slice(&cluster);
        }
        cues.sort_by_key(|cue| cue.0);
        let mut points = Vec::new();
        for (time, track, cluster, duration) in cues {
            let mut positions = element(CUE_TRACK, &uint_bytes(track));
            positions.extend_from_slice(&element(CUE_CLUSTER_POSITION, &uint_bytes(cluster)));
            if let Some(duration) = duration {
                positions.extend_from_slice(&element(CUE_DURATION, &uint_bytes(duration as u64)));
            }
            let mut point = element(CUE_TIME, &uint_bytes(time as u64));
            point.extend_from_slice(&element(CUE_TRACK_POSITIONS, &positions));
            points.extend_from_slice(&element(CUE_POINT, &point));
        }

        let mut entries = vec![(INFO, info_at), (TRACKS, tracks_at)];
        if with_index {
            entries.push((CUES, position));
        }
        let mut segment = seek_head(&entries);
        segment.extend_from_slice(&info);
        segment.extend_from_slice(&tracks);
        segment.extend_from_slice(&clusters);
        if with_index {
            segment.extend_from_slice(&element(CUES, &points));
        }
        let mut file = element(EBML, &element(DOC_TYPE, b"matroska"));
        file.extend_from_slice(&element(SEGMENT, &segment));
        std::fs::write(path, file).unwrap();
    }

    #[derive(Debug)]
    struct Written {
        track: u64,
        time: i64,
        marker: i64,
        keyframe: bool,
        invisible: bool,
        duration: Option<i64>,
    }

    fn read_output(path: &Path) -> Vec<Written> {
        let mut reader = Reader::new(Source::open(path.to_str().unwrap()).unwrap());
        let layout = Layout::read(&mut reader).unwrap();
        let mut position = layout.first_cluster.unwrap();
        let mut written = Vec::new();
        while position < layout.segment_end && !reader.at_end(position).unwrap() {
            let header = reader.header_at(position).unwrap();
            if header.id != CLUSTER {
                position = header.end().unwrap();
                continue;
            }
            let (blocks, next) = read_cluster(&mut reader, &header, &layout).unwrap();
            for block in blocks {
                let (_, offset) = vint(&block.frame, 0, false).unwrap();
                let marker = i64::from_be_bytes(block.frame[offset + 3..offset + 11].try_into().unwrap());
                written.push(Written {
                    track: block.track,
                    time: block.pts,
                    marker,
                    keyframe: block.keyframe,
                    invisible: block.frame[offset + 2] & FLAG_INVISIBLE != 0,
                    duration: block.duration,
                });
            }
            position = next;
        }
        written
    }

    fn folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("rona-mkv-cut-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn markers(written: &[Written], track: u64, invisible: bool) -> BTreeSet<i64> {
        written.iter().filter(|block| block.track == track && block.invisible == invisible).map(|block| block.marker).collect()
    }

    fn check_cut_mid_gop(with_index: bool) {
        let folder = folder(if with_index { "indexed" } else { "unindexed" });
        let source = folder.join("source.mkv");
        write_source(&source, with_index);

        // From 1.0 s, inside the GOP starting at 960, to 2.0 s, inside the GOP starting at 1920.
        let (written, failed) = cut(source.to_str().unwrap(), 1.0, 2.0, None, |_| folder.join("cut.mkv"), |_| {});
        assert_eq!(failed, None);
        assert_eq!(written.len(), 1);
        let blocks = read_output(&written[0]);

        let first = &blocks[0];
        assert!(first.track == 1 && first.keyframe && first.time == 0 && first.marker == 960);
        for block in &blocks {
            if block.track != 3 {
                assert_eq!(block.time + 960, block.marker, "a block moved in time: {block:?}");
            }
        }
        let shown: BTreeSet<i64> = (960..2000).step_by(FRAME as usize).collect();
        assert_eq!(markers(&blocks, 1, false), shown);
        // 1960 is decoded from the P-frame at 2040, which is only there to decode from.
        assert_eq!(markers(&blocks, 1, true), BTreeSet::from([2040]));
        let audio: BTreeSet<i64> = (960..2000).step_by(AUDIO as usize).collect();
        assert_eq!(markers(&blocks, 2, false), audio);

        let subtitles: Vec<(i64, Option<i64>)> =
            blocks.iter().filter(|block| block.track == 3).map(|block| (block.time, block.duration)).collect();
        if with_index {
            // The line from 900 to 1900 is on screen at the cut's start, so it shows from 0 and ends at the same moment.
            assert_eq!(subtitles, vec![(0, Some(940)), (540, Some(300))]);
        } else {
            // Only the index says how long earlier lines last.
            assert_eq!(subtitles, vec![(540, Some(300))]);
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn cut_inside_gops_keeps_every_frame_shown_in_the_range() {
        check_cut_mid_gop(true);
    }

    #[test]
    fn cut_without_an_index_finds_the_same_keyframe() {
        check_cut_mid_gop(false);
    }

    #[test]
    fn split_parts_start_on_keyframes_and_lose_nothing() {
        let folder = folder("split");
        let source = folder.join("source.mkv");
        write_source(&source, true);

        let end = (GOP * GOPS * FRAME) as f64 / 1000.0;
        let (written, failed) =
            cut(source.to_str().unwrap(), 0.0, end, Some(1.0), |part| folder.join(format!("part {part}.mkv")), |_| {});
        assert_eq!(failed, None);
        assert_eq!(written.len(), 5);

        let mut video = Vec::new();
        let mut audio = Vec::new();
        for path in &written {
            let blocks = read_output(path);
            let start = blocks[0].marker;
            assert!(blocks[0].track == 1 && blocks[0].keyframe && blocks[0].time == 0);
            for block in blocks.iter().filter(|block| block.track != 3) {
                assert!(!block.invisible);
                assert_eq!(block.time + start, block.marker);
            }
            video.extend(blocks.iter().filter(|block| block.track == 1).map(|block| block.marker));
            audio.extend(blocks.iter().filter(|block| block.track == 2).map(|block| block.marker));
        }
        video.sort_unstable();
        audio.sort_unstable();
        assert_eq!(video, (0..GOP * GOPS * FRAME).step_by(FRAME as usize).collect::<Vec<_>>());
        assert_eq!(audio, (0..GOP * GOPS * FRAME).step_by(AUDIO as usize).collect::<Vec<_>>());
        std::fs::remove_dir_all(folder).unwrap();
    }
}
