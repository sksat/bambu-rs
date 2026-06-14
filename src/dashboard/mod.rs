//! The embedded web dashboard — an HTTP server (axum) that serves a React SPA
//! (embedded via `rust-embed`) and an API for real-time monitoring + control.
//! Behind the `dashboard` cargo feature; another consumer of the library, like
//! the CLI. The LAN access code stays server-side and never reaches the browser.

pub mod assets;
pub mod server;

use crate::config::ResolvedTarget;
pub use server::{AppState, FakeSource, PrinterSource};

/// Options for [`serve`].
pub struct DashboardOpts {
    /// Bind host (default `127.0.0.1`). A non-loopback host serves over the
    /// network — for that, a token is required and a warning is printed.
    pub host: String,
    pub port: u16,
    /// Bearer token for `/api/*`; generated (and printed once to stderr) if `None`.
    pub token: Option<String>,
    /// Serve deterministic fake data instead of talking to a printer.
    pub fake: bool,
    pub interval: Option<std::time::Duration>,
    pub camera_rtsp: Option<String>,
}

/// Run the dashboard server (blocking; owns its own multi-thread runtime).
pub fn serve(_target: Option<ResolvedTarget>, opts: DashboardOpts) -> anyhow::Result<()> {
    let token = opts.token.clone().unwrap_or_else(generate_token);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        // The live source (MQTT monitor bridge) is wired in P2; until then every
        // mode serves a ramping fake so the UI and charts have moving data.
        let tick = opts.interval.unwrap_or(std::time::Duration::from_secs(1));
        let source: std::sync::Arc<dyn PrinterSource> =
            std::sync::Arc::new(FakeSource::ramping(tick));
        if !opts.fake {
            eprintln!(
                "note: live mode isn't wired yet (serving fake data); pass --fake to silence this"
            );
        }
        let addr = format!("{}:{}", opts.host, opts.port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?;
        let loopback =
            opts.host.starts_with("127.") || opts.host == "localhost" || opts.host == "::1";
        if !loopback {
            eprintln!(
                "warning: binding to non-loopback {addr} — the dashboard can drive the printer; \
                 the bearer token is required for every control request."
            );
        }
        // The token is a secret like the access code: printed once to stderr, never logged again.
        eprintln!("bambu dashboard: http://{addr}/   (bearer token: {token})");
        let state = AppState {
            source,
            token: token.clone(),
        };
        axum::serve(listener, server::router(state))
            .await
            .map_err(|e| anyhow::anyhow!("serving: {e}"))
    })
}

/// A random URL-safe bearer token (32 bytes, base64url, no padding).
fn generate_token() -> String {
    use base64::Engine;
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
