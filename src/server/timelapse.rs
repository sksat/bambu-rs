//! Serve-internal per-layer timelapse capture. The dashboard's single MQTT
//! connection already streams [`PrinterStatus`] over a `watch` channel, so the
//! capture runs *inside* `bambu serve` off that feed — no second printer
//! connection (the A1 mini allows only one) and the lowest possible latency.
//!
//! It's driven by camera *id* (not an arbitrary command), so the control
//! endpoint is a normal gated write with no command-execution surface. The pure
//! [`CaptureSession`] decides when to grab; this owns the I/O (fetching the
//! frame and writing files) and the run lifecycle.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::camera::StreamOpen;
use super::stream_record::record_loop;
use crate::core::status::PrinterStatus;
use crate::core::timelapse::{ActivityAction, CaptureAction, CaptureSession, PrintActivitySession};

/// Grab a single JPEG frame (blocking). Resolved from a camera id at start time
/// and held for the run's duration, so later `/api/cameras/config` edits can't
/// repoint a running capture.
pub type FrameGrab = Arc<dyn Fn() -> Result<Vec<u8>, String> + Send + Sync>;

/// Disk cap for the raw-MJPEG fallback (no ffmpeg). Raw MJPEG is ~9 GB/hour, so
/// this bounds a runaway recording; the recorder stops cleanly at the cap.
const MAX_STREAM_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Backstop for the ffmpeg (live-mp4) path. There the raw bytes stream *through*
/// ffmpeg's stdin and are never stored — only the compact mp4 hits disk — so this
/// is just a runaway guard (~6h of stream); a normal print ends (cancel) first.
const MAX_STREAM_INPUT_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// How a camera contributes to a `plain` run: `Sample` grabs a JPEG every tick
/// (snapshot-only cameras); `Stream` records the camera's continuous MJPEG stream
/// to one file (cameras that expose a real `/stream` — the actual video, not
/// time-sampled frames).
pub enum PlainCapture {
    Sample { id: String, grab: FrameGrab },
    Stream { id: String, open: StreamOpen },
}

impl PlainCapture {
    fn id(&self) -> &str {
        match self {
            PlainCapture::Sample { id, .. } | PlainCapture::Stream { id, .. } => id,
        }
    }
}

/// Live capture status for one run (smooth or plain), surfaced by
/// `GET /api/timelapse`.
#[derive(Clone, Default)]
pub struct TimelapseStatus {
    pub running: bool,
    /// `"smooth"` (per-layer, park-synced) or `"plain"` (wall-time sampled).
    pub mode: &'static str,
    /// The cameras captured in this run (one frame each per trigger). Empty when
    /// idle. `camera` (singular) is kept in the JSON for the common one-cam case.
    pub cameras: Vec<String>,
    /// Smooth: capture every Nth layer. Plain: 0 (uses `interval_ms` instead).
    pub every: u64,
    /// Plain: sampling period in ms. Smooth: `None` (it's layer-driven).
    pub interval_ms: Option<u64>,
    /// Total frames written across all cameras (minus skips).
    pub frames: u64,
    pub failures: u64,
    pub current_layer: Option<i64>,
    pub out_dir: Option<String>,
    pub last_error: Option<String>,
}

impl TimelapseStatus {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "running": self.running,
            "mode": self.mode,
            "cameras": self.cameras,
            // Back-compat: surface the first camera as `camera` for the common
            // single-camera case so older readers keep working.
            "camera": self.cameras.first(),
            "every": self.every,
            "interval_ms": self.interval_ms,
            "frames": self.frames,
            "failures": self.failures,
            "current_layer": self.current_layer,
            "out_dir": self.out_dir,
            "last_error": self.last_error,
        })
    }
}

#[derive(Default)]
struct Inner {
    /// Shared with the running task, which updates it; replaced on each `start`.
    status: Arc<Mutex<TimelapseStatus>>,
    handle: Option<JoinHandle<()>>,
    /// Set on stop. The async run task is `abort`ed, but a `plain` run's blocking
    /// stream-recorder workers can't be aborted — they watch this flag and exit.
    cancel: Arc<AtomicBool>,
}

/// Owns up to two concurrent captures of the same print — a `smooth` one
/// (per-layer, synced to the printer's park) and a `plain` one (sampled on a
/// wall-time interval, head in shot). Each is started/stopped independently.
/// Lives in `AppState`.
#[derive(Default)]
pub struct TimelapseManager {
    smooth: Mutex<Inner>,
    plain: Mutex<Inner>,
}

impl TimelapseManager {
    /// Start the smooth (per-layer) capture: one frame per `every`-th layer from
    /// each camera, written to `out_dir/<camera-id>/`.
    pub fn start_smooth(
        &self,
        cameras: Vec<(String, FrameGrab)>,
        every: u64,
        rx: watch::Receiver<PrinterStatus>,
        out_dir: PathBuf,
    ) -> Result<(), String> {
        let every = every.max(1);
        let ids = cameras.iter().map(|(id, _)| id.clone()).collect();
        start_slot(
            &self.smooth,
            cameras,
            ids,
            out_dir,
            TimelapseStatus {
                mode: "smooth",
                every,
                ..Default::default()
            },
            // Smooth has no long-lived blocking workers, so it ignores `cancel`.
            move |status, cams, dir, _cancel| tokio::spawn(run(status, rx, cams, dir, every)),
        )
    }

    /// Start the plain (time-sampled) capture: one frame from each camera every
    /// `interval_ms`, while the print is active.
    pub fn start_plain(
        &self,
        cameras: Vec<PlainCapture>,
        interval_ms: u64,
        rx: watch::Receiver<PrinterStatus>,
        out_dir: PathBuf,
    ) -> Result<(), String> {
        let interval_ms = interval_ms.max(1);
        let ids = cameras.iter().map(|c| c.id().to_string()).collect();
        start_slot(
            &self.plain,
            cameras,
            ids,
            out_dir,
            TimelapseStatus {
                mode: "plain",
                interval_ms: Some(interval_ms),
                ..Default::default()
            },
            move |status, caps, dir, cancel| {
                tokio::spawn(run_plain(status, rx, caps, dir, interval_ms, cancel))
            },
        )
    }

    /// Stop the smooth capture (idempotent). Returns whether one was running.
    pub fn stop_smooth(&self) -> bool {
        stop_slot(&self.smooth)
    }
    /// Stop the plain capture (idempotent). Returns whether one was running.
    pub fn stop_plain(&self) -> bool {
        stop_slot(&self.plain)
    }

    pub fn status_smooth(&self) -> TimelapseStatus {
        self.smooth.lock().unwrap().status.lock().unwrap().clone()
    }
    pub fn status_plain(&self) -> TimelapseStatus {
        self.plain.lock().unwrap().status.lock().unwrap().clone()
    }
}

/// Shared start path for either slot: refuse if that slot is already running or
/// no cameras are given, create the per-camera dirs, install a fresh status, and
/// spawn the runner (`spawn` builds the right one — smooth or plain).
fn start_slot<C>(
    inner: &Mutex<Inner>,
    cameras: Vec<C>,
    ids: Vec<String>,
    out_dir: PathBuf,
    init: TimelapseStatus,
    spawn: impl FnOnce(Arc<Mutex<TimelapseStatus>>, Vec<C>, PathBuf, Arc<AtomicBool>) -> JoinHandle<()>,
) -> Result<(), String> {
    let mut g = inner.lock().unwrap();
    if g.status.lock().unwrap().running {
        return Err(format!("a {} timelapse is already running", init.mode));
    }
    if ids.is_empty() {
        return Err("no cameras to capture".to_string());
    }
    for id in &ids {
        let dir = out_dir.join(id);
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new(TimelapseStatus {
        running: true,
        cameras: ids,
        out_dir: Some(out_dir.display().to_string()),
        ..init
    }));
    let handle = spawn(status.clone(), cameras, out_dir, cancel.clone());
    g.status = status;
    g.handle = Some(handle);
    g.cancel = cancel;
    Ok(())
}

fn stop_slot(inner: &Mutex<Inner>) -> bool {
    let mut g = inner.lock().unwrap();
    // Signal blocking stream workers first (abort can't reach them), then abort
    // the async task (which drops any in-flight snapshot grab).
    g.cancel.store(true, Ordering::Relaxed);
    if let Some(h) = g.handle.take() {
        h.abort();
    }
    let mut s = g.status.lock().unwrap();
    let was = s.running;
    s.running = false;
    was
}

/// The capture task. The observe loop NEVER blocks on a frame grab: a slow or
/// offline camera would otherwise stall it, and since `watch::Receiver` only
/// keeps the latest value, intermediate layer updates would coalesce and be
/// skipped. So observation just enqueues capture jobs (non-blocking, bounded —
/// dropped + counted if the grabbing worker can't keep up, rather than lagging
/// the print or growing without bound); a worker grabs + writes off that path.
async fn run(
    status: Arc<Mutex<TimelapseStatus>>,
    mut rx: watch::Receiver<PrinterStatus>,
    cameras: Vec<(String, FrameGrab)>,
    out_dir: PathBuf,
    every: u64,
) {
    // wait=true: the capture may be started before the print is active; sit
    // through idle/finished until it runs, then stop when the print ends.
    let mut session = CaptureSession::new(every, true);
    // Each layer enqueues one job per camera, so scale the bound with the camera
    // count to keep the same per-camera backpressure headroom as the single case.
    let bound = (4 * cameras.len()).max(4);
    let (tx, mut jobs) = tokio::sync::mpsc::channel::<(FrameGrab, PathBuf)>(bound);

    let wstatus = status.clone();
    let worker = tokio::spawn(async move {
        while let Some((grab, path)) = jobs.recv().await {
            let res = tokio::task::spawn_blocking(move || grab()).await;
            let mut s = wstatus.lock().unwrap();
            match res {
                Ok(Ok(bytes)) => match std::fs::write(&path, &bytes) {
                    Ok(()) => s.frames += 1,
                    Err(e) => {
                        s.failures += 1;
                        s.last_error = Some(format!("write {}: {e}", path.display()));
                    }
                },
                Ok(Err(e)) => {
                    s.failures += 1;
                    s.last_error = Some(e);
                }
                Err(_) => {
                    s.failures += 1;
                    s.last_error = Some("frame grab task failed".to_string());
                }
            }
        }
    });

    loop {
        let snap = rx.borrow_and_update().clone();
        status.lock().unwrap().current_layer = snap.layer_num;
        match session.observe(&snap) {
            CaptureAction::Capture { frame_no, layer } => {
                let name = format!("frame_{frame_no:06}_layer_{layer:05}.jpg");
                for (id, grab) in &cameras {
                    let path = out_dir.join(id).join(&name);
                    if tx.try_send((grab.clone(), path)).is_err() {
                        // Worker busy/backlogged — skip this frame rather than
                        // block observation (which would coalesce later layers).
                        let mut s = status.lock().unwrap();
                        s.failures += 1;
                        s.last_error = Some("capture fell behind — frame skipped".to_string());
                    }
                }
            }
            CaptureAction::Stop => break,
            CaptureAction::Continue => {}
        }
        if rx.changed().await.is_err() {
            break; // the source (and the whole server) is gone
        }
    }
    drop(tx); // close the queue so the worker drains the backlog, then exits
    let _ = worker.await;
    status.lock().unwrap().running = false;
}

/// The plain capture: a frame from each camera every `interval_ms` while the
/// print is active (head wherever it is — the "watch it print" look), independent
/// of layers/park. Same non-blocking grab path as [`run`]; reacts to status
/// changes between ticks so it stops promptly when the print ends.
/// ffmpeg argv to encode the MJPEG stream (read from stdin as `mpjpeg`) into an
/// h264 mp4 at `out`. Pure, so the command shape is unit-tested without ffmpeg.
fn live_mp4_args(out: &std::path::Path) -> Vec<String> {
    vec![
        "-y".into(),
        "-f".into(),
        "mpjpeg".into(),
        "-i".into(),
        "-".into(),
        "-c:v".into(),
        "libx264".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-movflags".into(),
        "+faststart".into(),
        out.display().to_string(),
    ]
}

/// Spawn one blocking recorder per stream camera. Each copies its MJPEG stream,
/// reconnecting on drops (interruptible backoff), until `cancel` is set. When
/// ffmpeg is on PATH it pipes the stream straight into ffmpeg → a compact h264
/// `<id>/plain.mp4` (the whole print fits — the raw bytes are never stored); with
/// no ffmpeg it falls back to the raw `<id>/plain.mjpeg` bounded by a disk cap.
fn spawn_stream_recorders(
    streams: Vec<(String, StreamOpen)>,
    out_dir: &std::path::Path,
    status: &Arc<Mutex<TimelapseStatus>>,
    cancel: &Arc<AtomicBool>,
) -> Vec<JoinHandle<()>> {
    streams
        .into_iter()
        .map(|(id, open)| {
            let dir = out_dir.join(&id);
            let mp4 = dir.join("plain.mp4");
            let mjpeg = dir.join("plain.mjpeg");
            let cancel = cancel.clone();
            let wstatus = status.clone();
            tokio::task::spawn_blocking(move || {
                let cancel_fn = || cancel.load(Ordering::Relaxed);
                // Interruptible reconnect backoff: sleep in small chunks so a stop
                // is noticed within ~50ms rather than after the full (up to 5s) wait.
                let backoff = |attempt: u32| {
                    let total_ms = (500u64 * u64::from(attempt)).min(5_000);
                    let mut slept = 0u64;
                    while slept < total_ms && !cancel.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(50));
                        slept += 50;
                    }
                };

                // Live mp4 (pipe through ffmpeg) when available; else raw mjpeg.
                let ffmpeg = std::process::Command::new("ffmpeg")
                    .args(live_mp4_args(&mp4))
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                // (stats, output path, encode_ok). For the raw path encode_ok is
                // vacuously true (write errors are already counted by record_loop);
                // for the ffmpeg path it's ffmpeg's exit status.
                let (stats, target, encode_ok) = match ffmpeg {
                    Ok(mut child) => {
                        let mut stdin = child.stdin.take().expect("piped stdin");
                        let stats = record_loop(
                            &open,
                            &mut stdin,
                            &cancel_fn,
                            MAX_STREAM_INPUT_BYTES,
                            32 * 1024,
                            &backoff,
                        );
                        drop(stdin); // EOF → ffmpeg finalizes the mp4
                        // If ffmpeg exits non-zero (no libx264, unsupported stream,
                        // disk error) the mp4 is missing/corrupt — surface it rather
                        // than report a silent success. The streamed bytes are gone,
                        // so we can't retroactively fall back to raw .mjpeg.
                        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
                        (stats, mp4, ok)
                    }
                    Err(_) => {
                        let file = match std::fs::File::create(&mjpeg) {
                            Ok(f) => f,
                            Err(e) => {
                                let mut s = wstatus.lock().unwrap();
                                s.failures += 1;
                                s.last_error = Some(format!("create {}: {e}", mjpeg.display()));
                                return;
                            }
                        };
                        let mut sink = std::io::BufWriter::new(file);
                        let stats = record_loop(
                            &open,
                            &mut sink,
                            &cancel_fn,
                            MAX_STREAM_BYTES,
                            32 * 1024,
                            &backoff,
                        );
                        let _ = sink.flush();
                        (stats, mjpeg, true)
                    }
                };
                let mut s = wstatus.lock().unwrap();
                s.failures += u64::from(stats.failures);
                if stats.bytes == 0 {
                    s.last_error =
                        Some(format!("stream {id}: no data recorded ({})", target.display()));
                } else if !encode_ok {
                    s.failures += 1;
                    s.last_error = Some(format!(
                        "stream {id}: ffmpeg failed to encode {} (missing libx264, or bad stream)",
                        target.display()
                    ));
                }
            })
        })
        .collect()
}

async fn run_plain(
    status: Arc<Mutex<TimelapseStatus>>,
    mut rx: watch::Receiver<PrinterStatus>,
    cameras: Vec<PlainCapture>,
    out_dir: PathBuf,
    interval_ms: u64,
    cancel: Arc<AtomicBool>,
) {
    // Split by strategy: snapshot cameras tick on the interval; stream cameras get
    // a long-lived blocking recorder each (the actual video, not samples).
    let mut samples: Vec<(String, FrameGrab)> = Vec::new();
    let mut streams: Vec<(String, StreamOpen)> = Vec::new();
    for cap in cameras {
        match cap {
            PlainCapture::Sample { id, grab } => samples.push((id, grab)),
            PlainCapture::Stream { id, open } => streams.push((id, open)),
        }
    }

    // Stream recorders are spawned LAZILY — only once the print is actually active
    // (the first `Capture`), like the sampled cameras — so we never record idle /
    // pre-print video (or burn the byte cap on a print that never starts). Each
    // then runs until `cancel` (print-end here, or stop_slot). They're blocking, so
    // they watch the flag — they can't be `abort`ed like the async task.
    let mut streams = streams;
    let mut stream_workers: Vec<JoinHandle<()>> = Vec::new();

    let mut activity = PrintActivitySession::new(true);
    let bound = (4 * samples.len()).max(4);
    let (tx, mut jobs) = tokio::sync::mpsc::channel::<(FrameGrab, PathBuf)>(bound);

    let wstatus = status.clone();
    let worker = tokio::spawn(async move {
        while let Some((grab, path)) = jobs.recv().await {
            let res = tokio::task::spawn_blocking(move || grab()).await;
            let mut s = wstatus.lock().unwrap();
            match res {
                Ok(Ok(bytes)) => match std::fs::write(&path, &bytes) {
                    Ok(()) => s.frames += 1,
                    Err(e) => {
                        s.failures += 1;
                        s.last_error = Some(format!("write {}: {e}", path.display()));
                    }
                },
                Ok(Err(e)) => {
                    s.failures += 1;
                    s.last_error = Some(e);
                }
                Err(_) => {
                    s.failures += 1;
                    s.last_error = Some("frame grab task failed".to_string());
                }
            }
        }
    });

    let mut frame_no: u64 = 0;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
    // A slow grab batch shouldn't make the next ticks fire back-to-back to catch
    // up; just resume the cadence from now.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let snap = rx.borrow().clone();
                status.lock().unwrap().current_layer = snap.layer_num;
                match activity.observe(&snap) {
                    ActivityAction::Capture => {
                        // First active tick → start the stream recorders (lazily, so
                        // pre-print idle isn't recorded). `take` empties `streams`, so
                        // this runs exactly once.
                        if !streams.is_empty() {
                            stream_workers = spawn_stream_recorders(
                                std::mem::take(&mut streams),
                                &out_dir,
                                &status,
                                &cancel,
                            );
                        }
                        frame_no += 1;
                        let name = format!("frame_{frame_no:06}.jpg");
                        for (id, grab) in &samples {
                            let path = out_dir.join(id).join(&name);
                            if tx.try_send((grab.clone(), path)).is_err() {
                                let mut s = status.lock().unwrap();
                                s.failures += 1;
                                s.last_error = Some("capture fell behind — frame skipped".to_string());
                            }
                        }
                    }
                    ActivityAction::Idle => {}
                    ActivityAction::Stop => break,
                }
            }
            // Between ticks, notice the print ending (or the source going away) so
            // we don't keep capturing a finished print for up to one interval.
            changed = rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let snap = rx.borrow().clone();
                if activity.observe(&snap) == ActivityAction::Stop {
                    break;
                }
            }
        }
    }
    drop(tx);
    let _ = worker.await;
    // Tell the stream recorders to stop (print ended), then let them flush + exit.
    cancel.store(true, Ordering::Relaxed);
    for w in stream_workers {
        let _ = w.await;
    }
    status.lock().unwrap().running = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::status::PrinterStatus;

    fn st(state: &str, layer: Option<i64>) -> PrinterStatus {
        PrinterStatus {
            gcode_state: Some(state.to_string()),
            layer_num: layer,
            ..Default::default()
        }
    }

    fn one(id: &str, grab: FrameGrab) -> Vec<(String, FrameGrab)> {
        vec![(id.to_string(), grab)]
    }

    /// One snapshot-only camera for a plain run.
    fn sample(id: &str, grab: FrameGrab) -> Vec<PlainCapture> {
        vec![PlainCapture::Sample {
            id: id.to_string(),
            grab,
        }]
    }

    // A capture driven by a fake status channel + a fake in-memory camera, end to
    // end through the manager — no MQTT, no real camera, no network.
    #[tokio::test]
    async fn runs_a_capture_from_a_watch_feed_writing_one_frame_per_layer() {
        let dir = std::env::temp_dir().join(format!("bambu-tl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = watch::channel(st("IDLE", None));
        let grab: FrameGrab = Arc::new(|| Ok(vec![0xff, 0xd8, 0xff, 0x42]));
        let mgr = TimelapseManager::default();
        mgr.start_smooth(one("ext-0", grab), 1, rx, dir.clone()).unwrap();

        // Drive: print starts and advances three layers, then finishes.
        for s in [st("RUNNING", Some(1)), st("RUNNING", Some(2)), st("RUNNING", Some(3)), st("FINISH", Some(3))] {
            tx.send(s).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let s = mgr.status_smooth();
        assert!(!s.running, "capture should auto-stop when the print finishes");
        assert_eq!(s.frames, 3, "one frame per advancing layer");
        assert_eq!(s.failures, 0);
        let n = std::fs::read_dir(dir.join("ext-0")).unwrap().count();
        assert_eq!(n, 3, "three JPEG files written under the camera's subdir");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn captures_every_camera_once_per_layer_into_per_camera_subdirs() {
        let dir = std::env::temp_dir().join(format!("bambu-tl-multi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = watch::channel(st("IDLE", None));
        let g: FrameGrab = Arc::new(|| Ok(vec![0xff, 0xd8, 0xff, 0x01]));
        let mgr = TimelapseManager::default();
        mgr.start_smooth(
            vec![("ext-0".into(), g.clone()), ("ext-1".into(), g)],
            1,
            rx,
            dir.clone(),
        )
        .unwrap();
        for s in [st("RUNNING", Some(1)), st("RUNNING", Some(2)), st("FINISH", Some(2))] {
            tx.send(s).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let s = mgr.status_smooth();
        assert_eq!(s.cameras, vec!["ext-0".to_string(), "ext-1".to_string()]);
        assert_eq!(s.frames, 4, "2 layers × 2 cameras");
        assert_eq!(s.failures, 0);
        assert_eq!(std::fs::read_dir(dir.join("ext-0")).unwrap().count(), 2);
        assert_eq!(std::fs::read_dir(dir.join("ext-1")).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn start_twice_is_rejected_until_stopped() {
        let dir = std::env::temp_dir().join(format!("bambu-tl-test2-{}", std::process::id()));
        let (_tx, rx) = watch::channel(st("RUNNING", Some(0)));
        let grab: FrameGrab = Arc::new(|| Ok(vec![0xff, 0xd8, 0xff]));
        let mgr = TimelapseManager::default();
        mgr.start_smooth(one("ext-0", grab.clone()), 1, rx.clone(), dir.clone()).unwrap();
        assert!(mgr.start_smooth(one("ext-1", grab), 1, rx, dir.clone()).is_err());
        assert!(mgr.stop_smooth());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn start_with_no_cameras_is_rejected() {
        let dir = std::env::temp_dir().join(format!("bambu-tl-empty-{}", std::process::id()));
        let (_tx, rx) = watch::channel(st("RUNNING", Some(0)));
        let mgr = TimelapseManager::default();
        assert!(mgr.start_smooth(vec![], 1, rx, dir).is_err(), "need at least one camera");
    }

    #[tokio::test]
    async fn a_failing_grab_counts_failures_and_keeps_going() {
        let dir = std::env::temp_dir().join(format!("bambu-tl-test3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = watch::channel(st("RUNNING", Some(0)));
        let grab: FrameGrab = Arc::new(|| Err("camera offline".to_string()));
        let mgr = TimelapseManager::default();
        mgr.start_smooth(one("ext-0", grab), 1, rx, dir.clone()).unwrap();
        for s in [st("RUNNING", Some(1)), st("RUNNING", Some(2)), st("FINISH", Some(2))] {
            tx.send(s).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let s = mgr.status_smooth();
        assert!(s.failures >= 2, "grab failures are counted");
        assert_eq!(s.frames, 0, "no files on failure");
        assert!(s.last_error.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── plain (time-sampled) capture ──
    #[tokio::test]
    async fn plain_samples_frames_on_an_interval_while_printing() {
        let dir = std::env::temp_dir().join(format!("bambu-tl-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = watch::channel(st("IDLE", None));
        let grab: FrameGrab = Arc::new(|| Ok(vec![0xff, 0xd8, 0xff, 0x09]));
        let mgr = TimelapseManager::default();
        mgr.start_plain(sample("ext-0", grab), 20, rx, dir.clone()).unwrap();

        // Idle → nothing is sampled (it waits for the print like the smooth one).
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert_eq!(mgr.status_plain().frames, 0, "no sampling before the print is active");

        // Printing → frames accumulate on the ~20ms clock, NOT per layer (the
        // layer never changes here, yet several frames land).
        tx.send(st("RUNNING", Some(1))).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let mid = mgr.status_plain().frames;
        assert!(mid >= 2, "plain samples on its own clock while printing (got {mid})");

        // Finishing stops it promptly (the changed-feed path, not a whole interval).
        tx.send(st("FINISH", Some(1))).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(!mgr.status_plain().running, "plain stops when the print finishes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn plain_stream_recorder_starts_and_stops_with_the_print() {
        // The recorder is a blocking worker driven by `cancel`; verify the
        // lifecycle (spawns once active, exits cleanly when the print finishes).
        // The output is ffmpeg-mp4 when ffmpeg is present, raw .mjpeg otherwise, so
        // this asserts the cancellation, not the bytes (record_loop's copy is
        // unit-tested in stream_record; the real encode is verified on-device).
        use crate::server::camera::{OpenedCameraStream, StreamOpen};
        let dir = std::env::temp_dir().join(format!("bambu-tl-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = watch::channel(st("RUNNING", Some(1)));
        let open: StreamOpen = Arc::new(|| {
            Ok(OpenedCameraStream {
                content_type: "multipart/x-mixed-replace".to_string(),
                reader: Box::new(std::io::Cursor::new(b"JPEGDATA".to_vec())),
            })
        });
        let caps = vec![PlainCapture::Stream {
            id: "ext-1".to_string(),
            open,
        }];
        let mgr = TimelapseManager::default();
        mgr.start_plain(caps, 20, rx, dir.clone()).unwrap();
        assert!(dir.join("ext-1").is_dir(), "per-camera dir created");

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        tx.send(st("FINISH", Some(1))).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert!(
            !mgr.status_plain().running,
            "stream recorder stops cleanly when the print finishes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_mp4_args_pipe_mpjpeg_stdin_to_h264() {
        let args = super::live_mp4_args(std::path::Path::new("/cap/ext-1/plain.mp4"));
        let joined = args.join(" ");
        assert!(joined.contains("-f mpjpeg"), "{joined}");
        assert!(joined.contains("-i -"), "reads the stream from stdin: {joined}");
        assert!(joined.contains("libx264"));
        assert!(joined.trim_end().ends_with("/cap/ext-1/plain.mp4"));
    }

    #[tokio::test]
    async fn smooth_and_plain_run_concurrently_and_stop_independently() {
        let dir = std::env::temp_dir().join(format!("bambu-tl-both-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = watch::channel(st("IDLE", None));
        let g: FrameGrab = Arc::new(|| Ok(vec![0xff, 0xd8, 0xff, 0x01]));
        let mgr = TimelapseManager::default();
        // Different slots → neither rejects the other (unlike start-twice).
        mgr.start_smooth(one("ext-0", g.clone()), 1, rx.clone(), dir.join("smooth")).unwrap();
        mgr.start_plain(sample("ext-0", g), 20, rx, dir.join("plain")).unwrap();
        assert!(mgr.status_smooth().running && mgr.status_plain().running);

        tx.send(st("RUNNING", Some(1))).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        tx.send(st("RUNNING", Some(2))).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert!(mgr.status_smooth().frames >= 1, "smooth captured layers");
        assert!(mgr.status_plain().frames >= 2, "plain sampled its interval");

        // Stopping one leaves the other running.
        assert!(mgr.stop_smooth());
        assert!(!mgr.status_smooth().running);
        assert!(mgr.status_plain().running, "plain keeps running after smooth stops");
        assert!(mgr.stop_plain());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
