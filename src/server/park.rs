//! Live "parked frame per layer" preview for a stream camera, in-process in
//! `bambu serve`. The pure detector lives in [`crate::core::park`]; this owns the I/O:
//! one ffmpeg per stream camera tees a tiny gray rawvideo (read here, fed to the
//! detector) and full-resolution JPEGs to a ring dir, so the emitted preview is full-res
//! without decoding JPEGs in Rust. On each emitted park the chosen ring JPEG is copied
//! atomically to `latest_park.jpg` (what the dashboard shows) plus `park_NNNNNN.jpg`,
//! and a line is appended to `parks.jsonl`.
//!
//! ffmpeg is the only external tool (already a serve dependency for `plain.mp4`); there
//! is no python3 runtime dependency. The frame-reading + write-mapping logic is split
//! out so it's unit-tested with an in-memory stream and fake ring files — the ffmpeg
//! spawn itself is the thin, on-device-verified seam.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::core::park::Park;

/// Detection decode size (tiny grayscale): enough signal for the left-zone park, cheap
/// to score per frame. Matches the Python miner's default.
pub const DECODE_W: usize = 64;
pub const DECODE_H: usize = 36;

/// ffmpeg argv (after the program name) for the live park tee: one MJPEG stream in, two
/// frame-synced outputs — a tiny gray rawvideo to stdout (`pipe:1`, for detection) and
/// full-res JPEGs to `ring_dir/full_%09d.jpg` (index-aligned to the gray frames). Pure,
/// so the command shape is unit-tested without ffmpeg.
pub fn live_park_args(stream_url: &str, ring_dir: &Path, fps: f64, w: usize, h: usize) -> Vec<String> {
    let ring = ring_dir.join("full_%09d.jpg");
    vec![
        "-v".into(),
        "error".into(),
        "-f".into(),
        "mpjpeg".into(),
        "-i".into(),
        stream_url.into(),
        "-filter_complex".into(),
        format!("[0:v]fps={fps},split=2[full][det];[det]scale={w}:{h},format=gray[gray]"),
        "-map".into(),
        "[gray]".into(),
        "-f".into(),
        "rawvideo".into(),
        "pipe:1".into(),
        "-map".into(),
        "[full]".into(),
        "-start_number".into(),
        "0".into(),
        "-q:v".into(),
        "3".into(),
        ring.display().to_string(),
    ]
}

/// The full-res ring JPEG path ffmpeg writes for gray frame `idx`.
pub fn ring_jpeg_path(ring_dir: &Path, idx: u64) -> PathBuf {
    ring_dir.join(format!("full_{idx:09}.jpg"))
}

/// Read exactly `buf.len()` bytes (one gray frame) from a blocking reader. Returns false
/// on EOF or error (a short/partial read at stream end counts as EOF).
fn read_full(reader: &mut dyn Read, buf: &mut [u8]) -> bool {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => return false,
            Ok(n) => filled += n,
            Err(_) => return false,
        }
    }
    true
}

/// Writes emitted parks to disk. A `replace` park (a stronger close pair — same layer)
/// reuses the previous index and overwrites, so the timelapse keeps exactly one frame
/// per park; `latest_park.jpg` is updated atomically (temp + rename) so a polling
/// dashboard never reads a half-written file.
pub struct ParkWriter {
    out_dir: PathBuf,
    emitted: u64,
}

impl ParkWriter {
    pub fn new(out_dir: PathBuf) -> Self {
        Self { out_dir, emitted: 0 }
    }

    /// Number of distinct park frames written so far (a `replace` doesn't increment it).
    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// Copy `ring_jpeg` to `latest_park.jpg` (atomic) and `park_NNNNNN.jpg`, and append a
    /// line to `parks.jsonl`. Returns the index written.
    pub fn write(&mut self, park: &Park, ring_jpeg: &Path) -> std::io::Result<u64> {
        let replace = park.replace && self.emitted > 0;
        let n = if replace { self.emitted - 1 } else { self.emitted };

        let tmp = self.out_dir.join("latest_park.jpg.tmp");
        std::fs::copy(ring_jpeg, &tmp)?;
        std::fs::rename(&tmp, self.out_dir.join("latest_park.jpg"))?;
        std::fs::copy(ring_jpeg, self.out_dir.join(format!("park_{n:06}.jpg")))?;

        let line = serde_json::json!({
            "n": n, "idx": park.idx, "t": park.t, "left_mass": park.left_mass,
            "sharpness": park.sharpness, "confidence": park.confidence, "replace": replace,
        });
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.out_dir.join("parks.jsonl"))?;
        writeln!(f, "{line}")?;

        if !replace {
            self.emitted += 1;
        }
        Ok(n)
    }
}

/// Outcome of one detection run over a gray stream.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParkRunStats {
    /// Gray frames read from the stream.
    pub frames: u64,
    /// Parks written (distinct frames; a `replace` overwrites rather than adds).
    pub parks: u64,
    /// Parks whose full-res ring JPEG never arrived / failed to write — dropped with a
    /// count rather than silently, so a missing layer is visible.
    pub dropped: u64,
}

/// Drive the detector over a gray rawvideo stream, writing each emitted park. Pure of
/// ffmpeg: the gray `reader`, the per-idx ring path, the "is the ring JPEG ready yet?"
/// wait, and `cancel` are all injected, so this is unit-tested with an in-memory stream
/// and pre-created fake ring files. `await_ring` lets the real worker poll briefly for
/// the JPEG (it can lag the gray by a tick) while a test returns instantly.
pub fn detect_stream(
    reader: &mut dyn Read,
    detector: &mut crate::core::park::LiveParkDetector,
    frame_size: usize,
    writer: &mut ParkWriter,
    ring_jpeg: &dyn Fn(u64) -> PathBuf,
    await_ring: &dyn Fn(&Path) -> bool,
    cancel: &dyn Fn() -> bool,
) -> ParkRunStats {
    let mut stats = ParkRunStats::default();
    let mut buf = vec![0u8; frame_size];
    let mut idx: u64 = 0;
    let mut emit = |park: &Park, stats: &mut ParkRunStats| {
        let path = ring_jpeg(park.idx);
        if await_ring(&path) && writer.write(park, &path).is_ok() {
            stats.parks += 1;
        } else {
            stats.dropped += 1;
        }
    };
    loop {
        if cancel() || !read_full(reader, &mut buf) {
            break;
        }
        if let Some(park) = detector.push(&buf, idx) {
            emit(&park, &mut stats);
        }
        idx += 1;
        stats.frames += 1;
    }
    if let Some(park) = detector.flush() {
        emit(&park, &mut stats);
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::park::{LiveParkDetector, ParkTuning};
    use std::io::Cursor;

    #[test]
    fn live_park_args_tees_synced_gray_and_full_outputs() {
        let args = live_park_args("http://cam/stream", Path::new("/ring"), 4.0, 64, 36);
        let joined = args.join(" ");
        assert!(joined.contains("-f mpjpeg -i http://cam/stream"), "{joined}");
        assert!(joined.contains("split=2[full][det]"), "one input, two outputs: {joined}");
        assert!(joined.contains("scale=64:36,format=gray"), "detection stream: {joined}");
        assert!(joined.contains("rawvideo pipe:1"), "{joined}");
        assert!(joined.trim_end().ends_with("/ring/full_%09d.jpg"), "full-res ring: {joined}");
        // fps 4.0 renders without a trailing .0 (ffmpeg-friendly).
        assert!(joined.contains("fps=4,"), "{joined}");
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bambu-park-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn park(idx: u64, replace: bool) -> Park {
        Park { idx, t: idx as f64 / 4.0, left_mass: 9000.0, sharpness: 500.0, confidence: 0.9, replace }
    }

    #[test]
    fn writer_writes_latest_and_indexed_and_jsonl() {
        let dir = tmp("writer");
        let ring = dir.join("full_000000003.jpg");
        std::fs::write(&ring, b"JPEGA").unwrap();
        let mut w = ParkWriter::new(dir.clone());
        assert_eq!(w.write(&park(3, false), &ring).unwrap(), 0);
        assert_eq!(w.emitted(), 1);
        assert_eq!(std::fs::read(dir.join("latest_park.jpg")).unwrap(), b"JPEGA");
        assert_eq!(std::fs::read(dir.join("park_000000.jpg")).unwrap(), b"JPEGA");
        let jl = std::fs::read_to_string(dir.join("parks.jsonl")).unwrap();
        assert_eq!(jl.lines().count(), 1);
        assert!(jl.contains("\"n\":0"), "{jl}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_replace_park_overwrites_the_previous_index() {
        let dir = tmp("replace");
        let r1 = dir.join("full_000000003.jpg");
        let r2 = dir.join("full_000000005.jpg");
        std::fs::write(&r1, b"WEAK").unwrap();
        std::fs::write(&r2, b"STRONG").unwrap();
        let mut w = ParkWriter::new(dir.clone());
        w.write(&park(3, false), &r1).unwrap(); // n=0, emitted -> 1
        let n = w.write(&park(5, true), &r2).unwrap(); // replace: reuse n=0, emitted stays 1
        assert_eq!(n, 0, "replace reuses the previous index");
        assert_eq!(w.emitted(), 1, "replace does not add a frame");
        assert_eq!(std::fs::read(dir.join("park_000000.jpg")).unwrap(), b"STRONG", "overwritten");
        assert_eq!(std::fs::read(dir.join("latest_park.jpg")).unwrap(), b"STRONG");
        // parks.jsonl keeps both lines (the supersession is recorded, not erased).
        assert_eq!(std::fs::read_to_string(dir.join("parks.jsonl")).unwrap().lines().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── synthetic gray frames (mirrors the core detector's fixtures) ──
    const W: usize = 48;
    const H: usize = 24;

    fn cfg() -> ParkTuning {
        ParkTuning {
            fps: 3.0, left_frac: 0.33, ema_seconds: 6.0, abs_floor: 1500.0, mad_k: 6.0,
            merge_gap_s: 1.2, max_island_s: 3.0, min_sep_s: 3.0, candidate_frac: 0.75,
            warmup_s: 0.5, baseline_s: 20.0,
        }
    }

    fn cframe(head_x: usize) -> Vec<u8> {
        let (bg, fix, obj, head) = (200u8, 40u8, 110u8, 25u8);
        let mut img = vec![bg; W * H];
        for y in 0..H {
            let row = y * W;
            img[row] = fix;
            img[row + 1] = fix;
            for x in (W / 2 - 3)..(W / 2 + 3) {
                img[row + x] = obj;
            }
            for x in 0..W {
                if (x as i64 - head_x as i64).unsigned_abs() <= 4 {
                    img[row + x] = head;
                }
            }
        }
        img
    }

    #[test]
    fn detect_stream_writes_one_park_and_maps_the_ring_index() {
        // 8 center + 3 far-left (a park) + 10 center → exactly one park; its ring JPEG
        // (pre-created at the detector's chosen idx) is written to latest/park_000000.
        let dir = tmp("detect");
        let ring_dir = dir.join(".ring");
        std::fs::create_dir_all(&ring_dir).unwrap();
        for idx in 8..=10 {
            std::fs::write(ring_jpeg_path(&ring_dir, idx), format!("RING{idx}")).unwrap();
        }

        let mut bytes = Vec::new();
        for _ in 0..8 {
            bytes.extend_from_slice(&cframe(W / 2));
        }
        for _ in 0..3 {
            bytes.extend_from_slice(&cframe(6));
        }
        for _ in 0..10 {
            bytes.extend_from_slice(&cframe(W / 2));
        }

        let mut det = LiveParkDetector::new(W, H, &cfg());
        let mut writer = ParkWriter::new(dir.clone());
        let rd = ring_dir.clone();
        let stats = detect_stream(
            &mut Cursor::new(bytes),
            &mut det,
            W * H,
            &mut writer,
            &|idx| ring_jpeg_path(&rd, idx),
            &|p| p.exists(),
            &|| false,
        );
        assert_eq!(stats.parks, 1, "{stats:?}");
        assert_eq!(stats.dropped, 0);
        assert_eq!(stats.frames, 21, "all frames read");
        assert!(dir.join("latest_park.jpg").exists());
        assert!(dir.join("park_000000.jpg").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_stream_stops_promptly_on_cancel() {
        let dir = tmp("cancel");
        let mut det = LiveParkDetector::new(W, H, &cfg());
        let mut writer = ParkWriter::new(dir.clone());
        let bytes = cframe(W / 2).repeat(5);
        let stats = detect_stream(
            &mut Cursor::new(bytes),
            &mut det,
            W * H,
            &mut writer,
            &|idx| PathBuf::from(format!("/nope/{idx}")),
            &|_| true,
            &|| true, // already cancelled
        );
        assert_eq!(stats, ParkRunStats::default(), "cancel before reading any frame");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
