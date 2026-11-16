use crate::mpv::Node;

// Two readings further apart than this are not trusted to describe one stretch of the file.
const MAX_SAMPLE_STEP_BYTES: u64 = 64 * 1024 * 1024;
const MERGE_GAP_SECONDS: f64 = 0.5;

#[derive(Default)]
pub struct ByteTimeMap {
    samples: Vec<(u64, f64)>,
    last: Option<(u64, f64)>,
}

impl ByteTimeMap {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    // Only trusted when both moved forward together, so mpv jumping inside its cache is ignored.
    pub fn observe(&mut self, bytes: u64, time: f64) {
        if let Some((last_bytes, last_time)) = self.last {
            let moved_together =
                bytes > last_bytes && time > last_time && bytes - last_bytes <= MAX_SAMPLE_STEP_BYTES;
            if moved_together {
                self.insert(last_bytes, last_time);
                self.insert(bytes, time);
            }
        }
        self.last = Some((bytes, time));
    }

    pub fn to_time(&self, start: u64, end: u64) -> Vec<(f64, f64)> {
        self.samples
            .windows(2)
            .filter_map(|pair| {
                let ((first_bytes, first_time), (next_bytes, next_time)) = (pair[0], pair[1]);
                if next_bytes - first_bytes > MAX_SAMPLE_STEP_BYTES || next_time < first_time {
                    return None;
                }
                let (from, to) = (start.max(first_bytes), end.min(next_bytes));
                if from >= to {
                    return None;
                }
                let at = |bytes: u64| {
                    first_time
                        + (next_time - first_time) * (bytes - first_bytes) as f64 / (next_bytes - first_bytes) as f64
                };
                Some((at(from), at(to)))
            })
            .collect()
    }

    fn insert(&mut self, bytes: u64, time: f64) {
        match self.samples.binary_search_by_key(&bytes, |sample| sample.0) {
            Ok(index) => self.samples[index].1 = time,
            Err(index) => self.samples.insert(index, (bytes, time)),
        }
    }
}

pub fn mpv_ranges(cache_state: &Node) -> Vec<(f64, f64)> {
    let Some(ranges) = cache_state.get("seekable-ranges") else { return Vec::new() };
    ranges
        .items()
        .iter()
        .filter_map(|range| Some((range.get("start")?.number()?, range.get("end")?.number()?)))
        .collect()
}

pub fn cache_end(cache_state: &Node) -> Option<f64> {
    cache_state.get("cache-end")?.number()
}

pub fn merge(mut ranges: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    ranges.retain(|(start, end)| end > start);
    ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 + MERGE_GAP_SECONDS => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}
