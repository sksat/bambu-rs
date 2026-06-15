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
use crate::core::timelapse::{CaptureAction, CaptureSession};

/// Grab a single JPEG frame (blocking). Resolved from a camera id at start time
/// and held for the run's duration, so later `/api/cameras/config` edits can't
/// repoint a running capture.
pub type FrameGrab = Arc<dyn Fn() -> Result<Vec<u8>, String> + Send + Sync>;

/// Live capture status, surfaced by `GET /api/timelapse`.
#[derive(Clone, Default)]
pub struct TimelapseStatus {
    pub running: bool,
    pub camera: Option<String>,
    pub every: u64,
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
            "camera": self.camera,
            "every": self.every,
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

/// Owns at most one running capture. Lives in `AppState`.
#[derive(Default)]
pub struct TimelapseManager {
    inner: Mutex<Inner>,
}

impl TimelapseManager {
    /// Start a capture. `out_dir` is created. Errors if one is already running.
    pub fn start(
        &self,
        camera: String,
        every: u64,
        rx: watch::Receiver<PrinterStatus>,
        grab: FrameGrab,
        out_dir: PathBuf,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.status.lock().unwrap().running {
            return Err("a timelapse capture is already running".to_string());
        }
        std::fs::create_dir_all(&out_dir).map_err(|e| format!("create {}: {e}", out_dir.display()))?;
        let status = Arc::new(Mutex::new(TimelapseStatus {
            running: true,
            camera: Some(camera),
            every: every.max(1),
            out_dir: Some(out_dir.display().to_string()),
            ..Default::default()
        }));
        let handle = tokio::spawn(run(status.clone(), rx, grab, out_dir, every.max(1)));
        inner.status = status;
        inner.handle = Some(handle);
        Ok(())
    }

    /// Stop a running capture (idempotent). Returns whether one was running.
    pub fn stop(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if let Some(h) = inner.handle.take() {
            h.abort(); // cancels the watch await; any in-flight blocking grab is dropped
        }
        let mut s = inner.status.lock().unwrap();
        let was = s.running;
        s.running = false;
        was
    }

    pub fn status(&self) -> TimelapseStatus {
        self.inner.lock().unwrap().status.lock().unwrap().clone()
    }
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
    grab: FrameGrab,
    out_dir: PathBuf,
    every: u64,
) {
    // wait=true: the capture may be started before the print is active; sit
    // through idle/finished until it runs, then stop when the print ends.
    let mut session = CaptureSession::new(every, true);
    let (tx, mut jobs) = tokio::sync::mpsc::channel::<(u64, PathBuf)>(4);

    let wstatus = status.clone();
    let worker = tokio::spawn(async move {
        while let Some((_frame_no, path)) = jobs.recv().await {
            let g = grab.clone();
            let res = tokio::task::spawn_blocking(move || g()).await;
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
                let path = out_dir.join(format!("frame_{frame_no:06}_layer_{layer:05}.jpg"));
                if tx.try_send((frame_no, path)).is_err() {
                    // Worker busy/backlogged — skip this layer rather than block
                    // observation (which would coalesce later layers too).
                    let mut s = status.lock().unwrap();
                    s.failures += 1;
                    s.last_error = Some("capture fell behind — frame skipped".to_string());
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

    // A capture driven by a fake status channel + a fake in-memory camera, end to
    // end through the manager — no MQTT, no real camera, no network.
    #[tokio::test]
    async fn runs_a_capture_from_a_watch_feed_writing_one_frame_per_layer() {
        let dir = std::env::temp_dir().join(format!("bambu-tl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = watch::channel(st("IDLE", None));
        let grab: FrameGrab = Arc::new(|| Ok(vec![0xff, 0xd8, 0xff, 0x42]));
        let mgr = TimelapseManager::default();
        mgr.start("ext-0".into(), 1, rx, grab, dir.clone()).unwrap();

        // Drive: print starts and advances three layers, then finishes.
        for s in [st("RUNNING", Some(1)), st("RUNNING", Some(2)), st("RUNNING", Some(3)), st("FINISH", Some(3))] {
            tx.send(s).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let s = mgr.status();
        assert!(!s.running, "capture should auto-stop when the print finishes");
        assert_eq!(s.frames, 3, "one frame per advancing layer");
        assert_eq!(s.failures, 0);
        let n = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(n, 3, "three JPEG files written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn start_twice_is_rejected_until_stopped() {
        let dir = std::env::temp_dir().join(format!("bambu-tl-test2-{}", std::process::id()));
        let (_tx, rx) = watch::channel(st("RUNNING", Some(0)));
        let grab: FrameGrab = Arc::new(|| Ok(vec![0xff, 0xd8, 0xff]));
        let mgr = TimelapseManager::default();
        mgr.start("ext-0".into(), 1, rx.clone(), grab.clone(), dir.clone()).unwrap();
        assert!(mgr.start("ext-1".into(), 1, rx, grab, dir.clone()).is_err());
        assert!(mgr.stop());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failing_grab_counts_failures_and_keeps_going() {
        let dir = std::env::temp_dir().join(format!("bambu-tl-test3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = watch::channel(st("RUNNING", Some(0)));
        let grab: FrameGrab = Arc::new(|| Err("camera offline".to_string()));
        let mgr = TimelapseManager::default();
        mgr.start("ext-0".into(), 1, rx, grab, dir.clone()).unwrap();
        for s in [st("RUNNING", Some(1)), st("RUNNING", Some(2)), st("FINISH", Some(2))] {
            tx.send(s).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let s = mgr.status();
        assert!(s.failures >= 2, "grab failures are counted");
        assert_eq!(s.frames, 0, "no files on failure");
        assert!(s.last_error.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
