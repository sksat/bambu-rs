//! The embedded HTTP server — a monitoring + control API (axum) and, when the
//! `dashboard` feature is on, the React SPA (embedded via `rust-embed`). Behind
//! the `server` feature; another consumer of the library, like the CLI.
//!
//! Auth: reads are always open; writes (control) are gated by an optional
//! password (`None` = open). The LAN access code stays server-side and never
//! reaches a client.

pub mod api;
#[cfg(feature = "dashboard")]
pub mod assets;
pub mod control;
pub mod files;
pub mod live;
pub mod start;

use std::sync::Arc;
use std::time::Duration;

use crate::config::ResolvedTarget;
pub use api::{AppState, FakeSource, PrinterSource};
pub use control::{Controller, FakeController, LiveController};
pub use files::{FakeFiles, FileStore, LiveFiles};
pub use live::LiveSource;
pub use start::{FakeStarter, LiveStarter, Starter};

/// Options for [`serve`].
pub struct ServeOpts {
    /// Bind host (default `127.0.0.1`). A non-loopback host serves over the
    /// network; without a password, control is open — a warning is printed.
    pub host: String,
    pub port: u16,
    /// Optional password gating **write** (control) requests. `None` = control is
    /// open. Reads are always unauthenticated.
    pub password: Option<String>,
    /// Serve deterministic fake data instead of talking to a printer.
    pub fake: bool,
    pub interval: Option<Duration>,
    pub camera_rtsp: Option<String>,
    /// External IP-camera snapshot URL (single-JPEG-per-GET). When set, the
    /// server proxies it via `/api/camera/snapshot` so a browser that can't reach
    /// the LAN cam (e.g. over Tailscale) still gets a live view.
    pub camera_url: Option<String>,
}

/// Run the server (blocking; owns its own multi-thread runtime).
pub fn serve(target: Option<ResolvedTarget>, opts: ServeOpts) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let ServeOpts {
        host,
        port,
        password,
        fake,
        interval,
        camera_rtsp: _,
        camera_url,
    } = opts;
    rt.block_on(async move {
        // Live mode bridges the real MQTT monitor (and controls the real device);
        // otherwise serve a ramping fake so the UI still has moving data.
        let state = match target {
            Some(t) if !fake => {
                eprintln!("connecting to the printer over LAN…");
                AppState {
                    source: Arc::new(LiveSource::connect(t.clone(), interval)),
                    controller: Arc::new(LiveController::new(t.clone())),
                    files: Arc::new(LiveFiles::new(t.clone())),
                    starter: Arc::new(LiveStarter::new(t)),
                    password,
                    start_lock: Arc::new(tokio::sync::Mutex::new(())),
                    camera_url,
                }
            }
            _ => {
                if !fake {
                    eprintln!(
                        "note: no printer configured; serving fake data (pass --fake to silence)"
                    );
                }
                let tick = interval.unwrap_or(Duration::from_secs(1));
                AppState {
                    source: Arc::new(FakeSource::ramping(tick)),
                    controller: Arc::new(FakeController::verified()),
                    files: Arc::new(FakeFiles),
                    starter: Arc::new(FakeStarter),
                    password,
                    start_lock: Arc::new(tokio::sync::Mutex::new(())),
                    camera_url,
                }
            }
        };
        let addr = format!("{host}:{port}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?;
        let loopback = host.starts_with("127.") || host == "localhost" || host == "::1";
        if !loopback {
            match &state.password {
                Some(_) => eprintln!(
                    "warning: serving on non-loopback {addr}; control requires the password, \
                     reads are open."
                ),
                None => eprintln!(
                    "warning: serving on non-loopback {addr} with no --password — control \
                     (pause/stop/light/speed) is OPEN to anyone who can reach this address."
                ),
            }
        }
        eprintln!("bambu serve: http://{addr}/");
        axum::serve(listener, api::router(state))
            .await
            .map_err(|e| anyhow::anyhow!("serving: {e}"))
    })
}
