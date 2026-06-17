//! Serve-internal per-layer timelapse capture. The dashboard's single MQTT
//! connection already streams [`PrinterStatus`] over a `watch` channel, so the
//! capture runs *inside* `bambu serve` off that feed — no second printer
//! connection (the A1 mini allows only one) and the lowest possible latency.
//!
//! It's driven by camera *id* (not an arbitrary command), so the control
//! endpoint is a normal gated write with no command-execution surface. The pure
//! [`CaptureSession`] decides when to grab; this owns the I/O (fetching the
//! frame and writing files) and the run lifecycle.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::core::status::PrinterStatus;
use crate::core::timelapse::{ActivityAction, CaptureAction, CaptureSession, PrintActivitySession};

/// Grab a single JPEG frame (blocking). Resolved from a camera id at start time
/// and held for the run's duration, so later `/api/cameras/config` edits can't
/// repoint a running capture.
pub type FrameGrab = Arc<dyn Fn() -> Result<Vec<u8>, String> + Send + Sync>;

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
        start_slot(
            &self.smooth,
            cameras,
            out_dir,
            TimelapseStatus {
                mode: "smooth",
                every,
                ..Default::default()
            },
            move |status, cams, dir| tokio::spawn(run(status, rx, cams, dir, every)),
        )
    }

    /// Start the plain (time-sampled) capture: one frame from each camera every
    /// `interval_ms`, while the print is active.
    pub fn start_plain(
        &self,
        cameras: Vec<(String, FrameGrab)>,
        interval_ms: u64,
        rx: watch::Receiver<PrinterStatus>,
        out_dir: PathBuf,
    ) -> Result<(), String> {
        let interval_ms = interval_ms.max(1);
        start_slot(
            &self.plain,
            cameras,
            out_dir,
            TimelapseStatus {
                mode: "plain",
                interval_ms: Some(interval_ms),
                ..Default::default()
            },
            move |status, cams, dir| tokio::spawn(run_plain(status, rx, cams, dir, interval_ms)),
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
fn start_slot(
    inner: &Mutex<Inner>,
    cameras: Vec<(String, FrameGrab)>,
    out_dir: PathBuf,
    init: TimelapseStatus,
    spawn: impl FnOnce(Arc<Mutex<TimelapseStatus>>, Vec<(String, FrameGrab)>, PathBuf) -> JoinHandle<()>,
) -> Result<(), String> {
    let mut g = inner.lock().unwrap();
    if g.status.lock().unwrap().running {
        return Err(format!("a {} timelapse is already running", init.mode));
    }
    if cameras.is_empty() {
        return Err("no cameras to capture".to_string());
    }
    for (id, _) in &cameras {
        let dir = out_dir.join(id);
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let status = Arc::new(Mutex::new(TimelapseStatus {
        running: true,
        cameras: cameras.iter().map(|(id, _)| id.clone()).collect(),
        out_dir: Some(out_dir.display().to_string()),
        ..init
    }));
    let handle = spawn(status.clone(), cameras, out_dir);
    g.status = status;
    g.handle = Some(handle);
    Ok(())
}

fn stop_slot(inner: &Mutex<Inner>) -> bool {
    let mut g = inner.lock().unwrap();
    if let Some(h) = g.handle.take() {
        h.abort(); // cancels the await; any in-flight blocking grab is dropped
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
async fn run_plain(
    status: Arc<Mutex<TimelapseStatus>>,
    mut rx: watch::Receiver<PrinterStatus>,
    cameras: Vec<(String, FrameGrab)>,
    out_dir: PathBuf,
    interval_ms: u64,
) {
    let mut activity = PrintActivitySession::new(true);
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
                        frame_no += 1;
                        let name = format!("frame_{frame_no:06}.jpg");
                        for (id, grab) in &cameras {
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
        mgr.start_plain(one("ext-0", grab), 20, rx, dir.clone()).unwrap();

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
    async fn smooth_and_plain_run_concurrently_and_stop_independently() {
        let dir = std::env::temp_dir().join(format!("bambu-tl-both-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = watch::channel(st("IDLE", None));
        let g: FrameGrab = Arc::new(|| Ok(vec![0xff, 0xd8, 0xff, 0x01]));
        let mgr = TimelapseManager::default();
        // Different slots → neither rejects the other (unlike start-twice).
        mgr.start_smooth(one("ext-0", g.clone()), 1, rx.clone(), dir.join("smooth")).unwrap();
        mgr.start_plain(one("ext-0", g), 20, rx, dir.join("plain")).unwrap();
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
