//! The built-in (printer chamber) camera seam for the server — distinct from the
//! external IP-camera proxy, which is just a URL on [`super::AppState`]. Live mode
//! grabs a JPEG over TCP:6000 (see [`crate::camera`]); fake / no-target mode has
//! no built-in camera. Abstracted as a trait so the server stays testable without
//! a real printer.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use crate::camera::CameraClient;
use crate::config::ResolvedTarget;

/// Per-request timeout for a built-in-camera grab. Shorter than the CLI's default
/// because a stalled grab shouldn't tie up an HTTP request (or a blocking-pool
/// thread) for long: the A1 camera is often off, in which case the connect
/// succeeds but no frame arrives, so the grab only ends on this timeout. The
/// dashboard pairs this with a poll back-off so a dead camera isn't hammered.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(4);

/// Source of built-in-camera frames.
pub trait CameraSource: Send + Sync {
    /// Whether a built-in camera could be reached at all (live mode with a
    /// target). The A1 camera is intermittently off, so `true` does **not**
    /// guarantee [`snapshot`](CameraSource::snapshot) will return a frame.
    fn configured(&self) -> bool;
    /// Grab a single JPEG frame, or an error message. Messages never include the
    /// access code (the underlying [`crate::camera::CameraError`] is careful too).
    fn snapshot(&self) -> Result<Vec<u8>, String>;
}

/// Live built-in camera: a one-shot TCP:6000 grab against the printer.
pub struct LiveCamera {
    target: ResolvedTarget,
}

impl LiveCamera {
    pub fn new(target: ResolvedTarget) -> Self {
        Self { target }
    }
}

impl CameraSource for LiveCamera {
    fn configured(&self) -> bool {
        true
    }
    fn snapshot(&self) -> Result<Vec<u8>, String> {
        CameraClient::new(self.target.clone())
            .with_timeout(SNAPSHOT_TIMEOUT)
            .snapshot()
            .map_err(|e| e.to_string())
    }
}

/// No built-in camera — fake mode, or `bambu serve` without a printer target.
pub struct NoCamera;

impl CameraSource for NoCamera {
    fn configured(&self) -> bool {
        false
    }
    fn snapshot(&self) -> Result<Vec<u8>, String> {
        Err("no built-in camera".to_string())
    }
}

/// One configured **external** IP camera: a human label plus the snapshot URL the
/// server proxies. The URL is an internal LAN address kept server-side — it's only
/// exposed on the gated config endpoint, never on the open camera listing.
// No `Eq`: `park_tuning` carries f64 knobs (ParkTuning is `PartialEq` only).
#[derive(Clone, Debug, PartialEq)]
pub struct ExternalCamera {
    pub label: String,
    pub url: String,
    /// Optional live MJPEG stream URL (e.g. an MJPEG `/stream` endpoint). When
    /// present the server can reverse-proxy a continuous multipart stream instead
    /// of polling `url` for single JPEGs. `None` = snapshot-only.
    pub stream_url: Option<String>,
    /// Optional per-camera live-park detection tuning. Present (with a `stream_url`)
    /// makes the camera eligible for the `park` timelapse slot; the values are
    /// camera-specific (framing), so there are no shared defaults. `None` = no live
    /// park preview for this camera.
    pub park_tuning: Option<crate::core::park::ParkTuning>,
    /// Optional per-camera burst-SELECTION tuning (the `select_smooth` knobs:
    /// min_outlier/min_left_density/min_confidence/select_candidate_frac + left_frac). When
    /// present, the serve assembles a smooth recording into a CLEAN one-frame-per-layer
    /// timelapse (pick the parked frame per burst); absent → the raw all-frames assemble.
    pub select_tuning: Option<crate::core::park::SelectTuning>,
}

impl ExternalCamera {
    /// Build from an optional label + snapshot URL + optional stream URL, filling
    /// a blank label with a stable `external N` (1-based `index`). A blank stream
    /// URL normalises to `None`. `park_tuning` starts `None`; set it with
    /// [`with_park_tuning`](Self::with_park_tuning).
    pub fn new(
        label: Option<String>,
        url: String,
        stream_url: Option<String>,
        index: usize,
    ) -> Self {
        let label = label
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| format!("external {}", index + 1));
        let stream_url = stream_url
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Self {
            label,
            url: url.trim().to_string(),
            stream_url,
            park_tuning: None,
            select_tuning: None,
        }
    }

    /// Attach (or clear) the per-camera live-park tuning. Chained after [`new`](Self::new).
    pub fn with_park_tuning(mut self, tuning: Option<crate::core::park::ParkTuning>) -> Self {
        self.park_tuning = tuning;
        self
    }

    /// Attach (or clear) the per-camera smooth-selection tuning. Chained after [`new`](Self::new).
    pub fn with_select_tuning(mut self, tuning: Option<crate::core::park::SelectTuning>) -> Self {
        self.select_tuning = tuning;
        self
    }

    /// Parse a CLI/env entry: `label=url`, or a bare `url` (auto-labelled). The
    /// leading `http(s)://` is detected so an `=` inside a URL query isn't taken
    /// as a label separator.
    pub fn parse(entry: &str, index: usize) -> Option<Self> {
        let entry = entry.trim();
        if entry.is_empty() {
            return None;
        }
        let (label, url) = if entry.starts_with("http://") || entry.starts_with("https://") {
            (None, entry.to_string())
        } else if let Some((l, u)) = entry.split_once('=') {
            (Some(l.to_string()), u.to_string())
        } else {
            (None, entry.to_string())
        };
        Some(Self::new(label, url, None, index))
    }
}

/// Connect timeout for opening a camera's MJPEG stream — bounds the handshake;
/// the body itself is meant to be endless (a per-read timeout ends a stall).
const STREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// An opened MJPEG stream: the upstream `content-type` (which carries the
/// multipart boundary) plus a blocking reader over the long-lived body.
pub struct OpenedCameraStream {
    pub content_type: String,
    pub reader: Box<dyn Read + Send + 'static>,
}

/// Opens a camera's MJPEG stream on demand. A closure, not a URL, so a recorder
/// can reconnect (re-open) without knowing the source and tests can inject a fake
/// that yields canned readers.
pub type StreamOpen = Arc<dyn Fn() -> Result<OpenedCameraStream, String> + Send + Sync>;

/// Open `url`'s MJPEG stream (blocking): connect and return the content-type plus
/// a reader over the endless multipart body. A connect timeout bounds the
/// handshake and a per-read timeout ends a stalled stream, but there is
/// deliberately no overall timeout — the stream is meant to be endless. Shared by
/// the HTTP reverse-proxy and the plain-timelapse stream recorder.
pub fn open_mjpeg_stream(url: &str) -> Result<OpenedCameraStream, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(STREAM_CONNECT_TIMEOUT)
        .timeout_read(Duration::from_secs(30))
        .redirects(0)
        .build();
    let resp = agent.get(url).call().map_err(|e| e.to_string())?;
    let content_type = resp
        .header("content-type")
        .map(str::to_string)
        .unwrap_or_else(|| "multipart/x-mixed-replace".to_string());
    Ok(OpenedCameraStream {
        content_type,
        reader: resp.into_reader(),
    })
}

/// A [`StreamOpen`] that re-opens `url` on each call.
pub fn url_stream_opener(url: String) -> StreamOpen {
    Arc::new(move || open_mjpeg_stream(&url))
}
