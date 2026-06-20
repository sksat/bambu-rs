//! Live "parked frame per layer" capture — the reusable I/O runner for the smooth
//! timelapse. The pure detector lives in [`crate::core::park`]; this owns the I/O and is
//! a first-class library module so the CLI, the server, and other consumers all drive it
//! the same way (not locked behind the `server` feature).
//!
//! One ffmpeg per stream camera opens the camera's MJPEG stream and tees a tiny gray
//! rawvideo (read here, fed to the detector) plus full-resolution JPEGs to a ring dir, so
//! the emitted preview is full-res without decoding JPEGs in Rust. On each emitted park
//! the chosen ring JPEG is copied atomically to `latest_park.jpg` (what a dashboard
//! shows) plus `park_NNNNNN.jpg`, and a line is appended to `parks.jsonl`.
//!
//! ffmpeg is the only external tool (already a dependency for the plain recorder); there
//! is no python3 runtime dependency. The frame-reading + write-mapping logic is injected
//! so it's unit-tested with an in-memory stream and fake ring files — the ffmpeg spawn
//! itself is the thin, on-device-verified seam, and it reports progress through a
//! callback rather than any server type, so every caller adapts it to its own output.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::core::park::{LiveParkDetector, Park, ParkTuning};

/// Detection decode size (tiny grayscale): enough signal for the left-zone park, cheap
/// to score per frame. Matches the Python miner's default.
pub const DECODE_W: usize = 64;
pub const DECODE_H: usize = 36;

/// ffmpeg argv (after the program name) for the live park tee: one MJPEG stream in, two
/// frame-synced outputs — a tiny gray rawvideo to stdout (`pipe:1`, for detection) and
/// full-res JPEGs to `ring_dir/full_%09d.jpg` (index-aligned to the gray frames). Pure,
/// so the command shape is unit-tested without ffmpeg.
pub fn live_park_args(
    stream_url: &str,
    ring_dir: &Path,
    fps: f64,
    w: usize,
    h: usize,
) -> Vec<String> {
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
        Self {
            out_dir,
            emitted: 0,
        }
    }

    /// Number of distinct park frames written so far (a `replace` doesn't increment it).
    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// Copy `ring_jpeg` to `latest_park.jpg` (atomic) and `park_NNNNNN.jpg`, and append a
    /// line to `parks.jsonl`. Returns the index written and whether it REPLACED the
    /// previous park (a stronger close pair, same layer — overwritten, not a new frame).
    pub fn write(&mut self, park: &Park, ring_jpeg: &Path) -> std::io::Result<ParkWritten> {
        let replaced = park.replace && self.emitted > 0;
        let index = if replaced {
            self.emitted - 1
        } else {
            self.emitted
        };

        let tmp = self.out_dir.join("latest_park.jpg.tmp");
        std::fs::copy(ring_jpeg, &tmp)?;
        std::fs::rename(&tmp, self.out_dir.join("latest_park.jpg"))?;
        std::fs::copy(ring_jpeg, self.out_dir.join(format!("park_{index:06}.jpg")))?;

        let line = serde_json::json!({
            "n": index, "idx": park.idx, "t": park.t, "left_mass": park.left_mass,
            "sharpness": park.sharpness, "confidence": park.confidence, "replace": replaced,
        });
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.out_dir.join("parks.jsonl"))?;
        writeln!(f, "{line}")?;

        if !replaced {
            self.emitted += 1;
        }
        Ok(ParkWritten { index, replaced })
    }
}

/// Result of [`ParkWriter::write`]: the index of the `park_NNNNNN.jpg` written, and
/// whether it overwrote the previous park (a same-layer supersession) vs. added a new one.
pub struct ParkWritten {
    pub index: u64,
    pub replaced: bool,
}

/// What happened to one emitted park — the live progress hook's event. A `Replaced` is
/// NOT a new layer: it overwrote the previous park with a stronger frame, so it must not
/// be counted as an additional park.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkEvent {
    /// A new distinct park frame was written (`park_NNNNNN.jpg` added).
    Written,
    /// A stronger close pair superseded the previous park (overwritten, same layer).
    Replaced,
    /// The park's ring JPEG never arrived / the write failed — the frame is lost.
    Dropped,
}

/// Outcome of one detection run over a gray stream.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParkRunStats {
    /// Gray frames read from the stream.
    pub frames: u64,
    /// Distinct parks written (one `park_NNNNNN.jpg` each; a replace does NOT count here).
    pub parks: u64,
    /// Parks superseded by a stronger close pair (overwrote an existing frame).
    pub replaced: u64,
    /// Parks whose full-res ring JPEG never arrived / failed to write — dropped with a
    /// count rather than silently, so a missing layer is visible.
    pub dropped: u64,
}

/// Resolve one emitted park to disk: await its ring JPEG (briefly), write it, and
/// tally/report the outcome. `on_emit` fires per park so the caller can update live
/// progress without waiting for the run to end — distinguishing a new park from a
/// supersession so neither over-counts.
fn emit_park(
    park: &Park,
    ring_jpeg: &dyn Fn(u64) -> PathBuf,
    await_ring: &dyn Fn(&Path) -> bool,
    writer: &mut ParkWriter,
    on_emit: &mut dyn FnMut(ParkEvent),
    stats: &mut ParkRunStats,
) {
    let path = ring_jpeg(park.idx);
    let event = if await_ring(&path) {
        match writer.write(park, &path) {
            Ok(w) if w.replaced => ParkEvent::Replaced,
            Ok(_) => ParkEvent::Written,
            Err(_) => ParkEvent::Dropped,
        }
    } else {
        ParkEvent::Dropped
    };
    match event {
        ParkEvent::Written => stats.parks += 1,
        ParkEvent::Replaced => stats.replaced += 1,
        ParkEvent::Dropped => stats.dropped += 1,
    }
    on_emit(event);
}

/// Drive the detector over a gray rawvideo stream, writing each emitted park. Pure of
/// ffmpeg: the gray `reader`, the per-idx ring path, the "is the ring JPEG ready yet?"
/// wait, `cancel`, and the `on_emit` progress hook are all injected, so this is
/// unit-tested with an in-memory stream and pre-created fake ring files. `await_ring`
/// lets the real worker poll briefly for the JPEG (it can lag the gray by a tick) while
/// a test returns instantly; `on_emit` fires a [`ParkEvent`] per park for live status.
#[allow(clippy::too_many_arguments)]
pub fn detect_stream(
    reader: &mut dyn Read,
    detector: &mut LiveParkDetector,
    frame_size: usize,
    writer: &mut ParkWriter,
    ring_jpeg: &dyn Fn(u64) -> PathBuf,
    await_ring: &dyn Fn(&Path) -> bool,
    cancel: &dyn Fn() -> bool,
    on_emit: &mut dyn FnMut(ParkEvent),
) -> ParkRunStats {
    let mut stats = ParkRunStats::default();
    let mut buf = vec![0u8; frame_size];
    let mut idx: u64 = 0;
    loop {
        if cancel() || !read_full(reader, &mut buf) {
            break;
        }
        if let Some(park) = detector.push(&buf, idx) {
            emit_park(&park, ring_jpeg, await_ring, writer, on_emit, &mut stats);
        }
        idx += 1;
        stats.frames += 1;
    }
    if let Some(park) = detector.flush() {
        emit_park(&park, ring_jpeg, await_ring, writer, on_emit, &mut stats);
    }
    stats
}

/// One stream camera selected for live park detection: its id, the MJPEG stream URL
/// ffmpeg opens, and its per-camera tuning (framing is camera-specific, so the tuning is
/// too — there are no shared defaults).
#[derive(Clone)]
pub struct ParkCapture {
    pub id: String,
    pub stream_url: String,
    pub tuning: ParkTuning,
}

/// Delete all but the newest `keep` ring JPEGs (`full_<idx>.jpg`) to bound disk during a
/// long print — ffmpeg writes one full-res JPEG per gray frame. The newest are kept, so
/// an in-flight park (its idx is within a few frames of the latest) is never pruned out
/// from under [`detect_stream`]'s `await_ring`. Returns how many were removed.
pub fn prune_ring(ring_dir: &Path, keep: usize) -> usize {
    let mut entries: Vec<(u64, PathBuf)> = match std::fs::read_dir(ring_dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let idx = p
                    .file_name()
                    .and_then(|f| f.to_str())
                    .and_then(|f| f.strip_prefix("full_"))
                    .and_then(|f| f.strip_suffix(".jpg"))
                    .and_then(|f| f.parse::<u64>().ok())?;
                Some((idx, p))
            })
            .collect(),
        Err(_) => return 0,
    };
    if entries.len() <= keep {
        return 0;
    }
    entries.sort_by_key(|(idx, _)| *idx);
    let remove = entries.len() - keep;
    entries
        .into_iter()
        .take(remove)
        .filter(|(_, p)| std::fs::remove_file(p).is_ok())
        .count()
}

/// Seconds of full-res ring JPEGs to retain (sliding window) — well above the few-frame
/// park lag, so an in-flight park's JPEG is always still present when it's written out.
const RING_KEEP_SECONDS: f64 = 60.0;

/// Run live park detection for ONE stream camera until the stream ends or `cancel` is
/// set, writing its output into `cam_dir` (`latest_park.jpg` + `park_NNNNNN.jpg` +
/// `parks.jsonl`, with a transient `.ring` subdir). The caller picks the dir — the server
/// uses one per camera under the run dir; the CLI passes its `--out` directly. Spawns one
/// ffmpeg ([`live_park_args`]) that opens the MJPEG stream and tees a tiny gray rawvideo
/// (read here → the detector) plus full-res JPEGs to the ring; on each park the chosen
/// JPEG is written to `latest_park.jpg`. An aux thread KILLS ffmpeg the instant `cancel`
/// is set — the main read blocks until bytes arrive, so it can't observe `cancel` itself —
/// and prunes the ring on a sliding window.
///
/// Reports each park live via `on_park` ([`ParkEvent`]); returns the run's
/// [`ParkRunStats`], or an error string if it couldn't even start (no server/CLI type
/// leaks in, so any caller adapts it). `stats.frames == 0` on return means the stream
/// produced nothing — the caller decides how to surface that. This is the thin,
/// on-device-verified I/O seam; the detector, argv, writer, and prune are the unit-tested
/// pure pieces.
pub fn run_park_camera(
    cap: &ParkCapture,
    cam_dir: &Path,
    w: usize,
    h: usize,
    cancel: &Arc<AtomicBool>,
    on_park: &mut dyn FnMut(ParkEvent),
) -> Result<ParkRunStats, String> {
    let ring = cam_dir.join(".ring");
    std::fs::create_dir_all(&ring)
        .map_err(|e| format!("park {}: create {}: {e}", cap.id, ring.display()))?;

    let fps = cap.tuning.fps;
    let mut child = std::process::Command::new("ffmpeg")
        .args(live_park_args(&cap.stream_url, &ring, fps, w, h))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("park {}: ffmpeg spawn failed: {e}", cap.id))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let child = Arc::new(Mutex::new(child));
    let done = Arc::new(AtomicBool::new(false));

    // Aux thread: kill ffmpeg the instant cancel is set (the main read blocks until bytes
    // arrive, so it can't see cancel itself), and prune the ring on a sliding window.
    let aux = {
        let (cancel, done, child, ring) =
            (cancel.clone(), done.clone(), child.clone(), ring.clone());
        let keep = (RING_KEEP_SECONDS * fps).max(40.0) as usize;
        std::thread::spawn(move || {
            let mut ticks = 0u32;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    let _ = child.lock().unwrap().kill();
                    return;
                }
                if done.load(Ordering::Relaxed) {
                    return;
                }
                ticks += 1;
                if ticks.is_multiple_of(4) {
                    prune_ring(&ring, keep);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        })
    };

    let mut det = LiveParkDetector::new(w, h, &cap.tuning);
    let mut writer = ParkWriter::new(cam_dir.to_path_buf());
    let ring_path = ring.clone();
    let read_cancel = cancel.clone();
    let wait_cancel = cancel.clone();
    let stats = detect_stream(
        &mut stdout,
        &mut det,
        w * h,
        &mut writer,
        &|idx| ring_jpeg_path(&ring_path, idx),
        &|p| await_ring(p, &wait_cancel),
        &|| read_cancel.load(Ordering::Relaxed),
        on_park,
    );

    done.store(true, Ordering::Relaxed);
    let _ = aux.join();
    let _ = child.lock().unwrap().wait();
    let _ = std::fs::remove_dir_all(&ring);
    Ok(stats)
}

/// Poll for a ring JPEG to appear (it can lag its gray frame by a tick), up to ~500ms,
/// bailing early if cancelled. Bounded — a JPEG that never arrives is dropped, never
/// waited on forever (which would stall draining ffmpeg's stdout).
fn await_ring(path: &Path, cancel: &Arc<AtomicBool>) -> bool {
    for _ in 0..10 {
        if path.exists() {
            return true;
        }
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn live_park_args_tees_synced_gray_and_full_outputs() {
        let args = live_park_args("http://cam/stream", Path::new("/ring"), 4.0, 64, 36);
        let joined = args.join(" ");
        assert!(
            joined.contains("-f mpjpeg -i http://cam/stream"),
            "{joined}"
        );
        assert!(
            joined.contains("split=2[full][det]"),
            "one input, two outputs: {joined}"
        );
        assert!(
            joined.contains("scale=64:36,format=gray"),
            "detection stream: {joined}"
        );
        assert!(joined.contains("rawvideo pipe:1"), "{joined}");
        assert!(
            joined.trim_end().ends_with("/ring/full_%09d.jpg"),
            "full-res ring: {joined}"
        );
        assert!(
            joined.contains("fps=4,"),
            "fps renders ffmpeg-friendly: {joined}"
        );
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bambu-park-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn park(idx: u64, replace: bool) -> Park {
        Park {
            idx,
            t: idx as f64 / 4.0,
            left_mass: 9000.0,
            sharpness: 500.0,
            confidence: 0.9,
            replace,
        }
    }

    #[test]
    fn writer_writes_latest_and_indexed_and_jsonl() {
        let dir = tmp("writer");
        let ring = dir.join("full_000000003.jpg");
        std::fs::write(&ring, b"JPEGA").unwrap();
        let mut w = ParkWriter::new(dir.clone());
        let wr = w.write(&park(3, false), &ring).unwrap();
        assert_eq!(wr.index, 0);
        assert!(!wr.replaced);
        assert_eq!(w.emitted(), 1);
        assert_eq!(
            std::fs::read(dir.join("latest_park.jpg")).unwrap(),
            b"JPEGA"
        );
        assert_eq!(
            std::fs::read(dir.join("park_000000.jpg")).unwrap(),
            b"JPEGA"
        );
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
        let wr = w.write(&park(5, true), &r2).unwrap(); // replace: reuse n=0, emitted stays 1
        assert_eq!(wr.index, 0, "replace reuses the previous index");
        assert!(wr.replaced, "flagged as a replacement");
        assert_eq!(w.emitted(), 1, "replace does not add a frame");
        assert_eq!(
            std::fs::read(dir.join("park_000000.jpg")).unwrap(),
            b"STRONG",
            "overwritten"
        );
        assert_eq!(
            std::fs::read(dir.join("latest_park.jpg")).unwrap(),
            b"STRONG"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("parks.jsonl"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── synthetic gray frames (mirrors the core detector's fixtures) ──
    const W: usize = 48;
    const H: usize = 24;

    fn cfg() -> ParkTuning {
        ParkTuning {
            fps: 3.0,
            left_frac: 0.33,
            ema_seconds: 6.0,
            abs_floor: 1500.0,
            mad_k: 6.0,
            merge_gap_s: 1.2,
            max_island_s: 3.0,
            min_sep_s: 3.0,
            candidate_frac: 0.75,
            warmup_s: 0.5,
            baseline_s: 20.0,
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

    /// Like [`cframe`] but the head is drawn only on the top half of rows → a fainter
    /// island (less left-mass), e.g. a blurred travel move that a real park supersedes.
    fn cframe_weak(head_x: usize) -> Vec<u8> {
        let (bg, fix, obj, head) = (200u8, 40u8, 110u8, 25u8);
        let mut img = vec![bg; W * H];
        for y in 0..H {
            let row = y * W;
            img[row] = fix;
            img[row + 1] = fix;
            for x in (W / 2 - 3)..(W / 2 + 3) {
                img[row + x] = obj;
            }
            if y >= H / 2 {
                continue;
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
        let mut emitted = 0u32;
        let stats = detect_stream(
            &mut Cursor::new(bytes),
            &mut det,
            W * H,
            &mut writer,
            &|idx| ring_jpeg_path(&rd, idx),
            &|p| p.exists(),
            &|| false,
            &mut |ev| {
                if ev == ParkEvent::Written {
                    emitted += 1;
                }
            },
        );
        assert_eq!(stats.parks, 1, "{stats:?}");
        assert_eq!(stats.dropped, 0);
        assert_eq!(emitted, 1, "on_emit fired for the written park");
        assert_eq!(stats.frames, 21, "all frames read");
        assert!(dir.join("latest_park.jpg").exists());
        assert!(dir.join("park_000000.jpg").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_stream_counts_a_replacement_as_replaced_not_a_new_park() {
        // a weak island then a stronger one <min_sep later (the core replace scenario):
        // the second SUPERSEDES the first, so it must count as Replaced, not a 2nd park.
        let dir = tmp("replace-stream");
        let ring_dir = dir.join(".ring");
        std::fs::create_dir_all(&ring_dir).unwrap();
        for idx in 0..30 {
            std::fs::write(ring_jpeg_path(&ring_dir, idx), b"R").unwrap();
        }
        let mut bytes = Vec::new();
        for _ in 0..8 {
            bytes.extend_from_slice(&cframe(W / 2));
        }
        for _ in 0..2 {
            bytes.extend_from_slice(&cframe_weak(6));
        }
        for _ in 0..5 {
            bytes.extend_from_slice(&cframe(W / 2));
        }
        for _ in 0..2 {
            bytes.extend_from_slice(&cframe(6));
        }
        for _ in 0..8 {
            bytes.extend_from_slice(&cframe(W / 2));
        }

        let mut det = LiveParkDetector::new(W, H, &cfg());
        let mut writer = ParkWriter::new(dir.clone());
        let rd = ring_dir.clone();
        let (mut written, mut replaced) = (0u32, 0u32);
        let stats = detect_stream(
            &mut Cursor::new(bytes),
            &mut det,
            W * H,
            &mut writer,
            &|idx| ring_jpeg_path(&rd, idx),
            &|p| p.exists(),
            &|| false,
            &mut |ev| match ev {
                ParkEvent::Written => written += 1,
                ParkEvent::Replaced => replaced += 1,
                ParkEvent::Dropped => {}
            },
        );
        assert_eq!(stats.parks, 1, "one distinct park on disk: {stats:?}");
        assert_eq!(
            stats.replaced, 1,
            "the stronger pair superseded it: {stats:?}"
        );
        assert_eq!(written, 1, "live count: one new park, not two");
        assert_eq!(replaced, 1);
        assert_eq!(writer.emitted(), 1, "one park_*.jpg on disk");
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
            &mut |_| {},
        );
        assert_eq!(
            stats,
            ParkRunStats::default(),
            "cancel before reading any frame"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_ring_keeps_only_the_newest_jpegs() {
        let dir = tmp("prune");
        for idx in 0..10u64 {
            std::fs::write(ring_jpeg_path(&dir, idx), b"x").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), b"x").unwrap(); // a non-ring file
        assert_eq!(prune_ring(&dir, 3), 7);
        for idx in 0..7u64 {
            assert!(!ring_jpeg_path(&dir, idx).exists(), "old {idx} pruned");
        }
        for idx in 7..10u64 {
            assert!(ring_jpeg_path(&dir, idx).exists(), "newest {idx} kept");
        }
        assert!(dir.join("notes.txt").exists(), "non-ring file untouched");
        assert_eq!(
            prune_ring(&dir, 10),
            0,
            "nothing to prune when under the cap"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
