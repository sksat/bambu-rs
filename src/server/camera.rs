//! The built-in (printer chamber) camera seam for the server — distinct from the
//! external IP-camera proxy, which is just a URL on [`super::AppState`]. Live mode
//! grabs a JPEG over TCP:6000 (see [`crate::camera`]); fake / no-target mode has
//! no built-in camera. Abstracted as a trait so the server stays testable without
//! a real printer.

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalCamera {
    pub label: String,
    pub url: String,
}

impl ExternalCamera {
    /// Build from an optional label + URL, filling a blank label with a stable
    /// `external N` (1-based `index`).
    pub fn new(label: Option<String>, url: String, index: usize) -> Self {
        let label = label
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| format!("external {}", index + 1));
        Self {
            label,
            url: url.trim().to_string(),
        }
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
        Some(Self::new(label, url, index))
    }
}
