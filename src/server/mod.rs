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

use std::sync::Arc;
use std::time::Duration;

use crate::config::ResolvedTarget;
pub use api::{AppState, FakeSource, PrinterSource};
pub use control::{Controller, FakeController, LiveController};
pub use files::{FakeFiles, FileStore, LiveFiles};
pub use live::LiveSource;

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
}

/// Run the server (blocking; owns its own multi-thread runtime).
pub fn serve(target: Option<ResolvedTarget>, opts: ServeOpts) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        // Live mode bridges the real MQTT monitor (and controls the real device);
        // otherwise serve a ramping fake so the UI still has moving data.
        let (source, controller, files): (
            Arc<dyn PrinterSource>,
            Arc<dyn Controller>,
            Arc<dyn FileStore>,
        ) = match target {
            Some(t) if !opts.fake => {
                eprintln!("connecting to the printer over LAN…");
                (
                    Arc::new(LiveSource::connect(t.clone(), opts.interval)),
                    Arc::new(LiveController::new(t.clone())),
                    Arc::new(LiveFiles::new(t)),
                )
            }
            _ => {
                if !opts.fake {
                    eprintln!(
                        "note: no printer configured; serving fake data (pass --fake to silence)"
                    );
                }
                let tick = opts.interval.unwrap_or(Duration::from_secs(1));
                (
                    Arc::new(FakeSource::ramping(tick)),
                    Arc::new(FakeController::verified()),
                    Arc::new(FakeFiles),
                )
            }
        };
        let addr = format!("{}:{}", opts.host, opts.port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?;
        let loopback =
            opts.host.starts_with("127.") || opts.host == "localhost" || opts.host == "::1";
        if !loopback {
            match &opts.password {
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
        let state = AppState {
            source,
            controller,
            files,
            password: opts.password,
        };
        axum::serve(listener, api::router(state))
            .await
            .map_err(|e| anyhow::anyhow!("serving: {e}"))
    })
}
