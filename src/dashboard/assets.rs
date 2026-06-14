//! The embedded SPA (`web/dist/`), served by the dashboard.
//!
//! `rust-embed` with `debug-embed` left **off**: debug builds read `web/dist/`
//! from disk at runtime (so `pnpm dev`/rebuilds show up live), release builds
//! embed the bytes into the binary. The folder must exist at compile time — a
//! committed `web/dist/.gitkeep` guarantees that even before `pnpm build` runs.

use axum::http::{Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct Assets;

/// Serve an embedded asset; fall back to `index.html` for unknown paths so the
/// SPA can client-route. Static assets are intentionally **not** token-gated
/// (the HTML/JS must load before the app can authenticate its API calls).
pub(crate) async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(content) => ([(header::CONTENT_TYPE, mime_for(path))], content.data).into_response(),
        // SPA fallback: serve index.html for client-side routes.
        None => match Assets::get("index.html") {
            Some(index) => ([(header::CONTENT_TYPE, "text/html")], index.data).into_response(),
            // The frontend hasn't been built (web/dist is empty) — serve a tiny
            // built-in page so the server still works (and tests don't depend on a
            // `pnpm build`). A real build replaces this with the SPA.
            None => (
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                FALLBACK_HTML,
            )
                .into_response(),
        },
    }
}

/// Shown when `web/dist` hasn't been built (rust-embed found no `index.html`).
const FALLBACK_HTML: &str = "<!doctype html><meta charset=utf-8><title>bambu dashboard</title>\
<body style=\"font-family:system-ui;padding:2rem\"><h1>bambu dashboard</h1>\
<p>The web UI isn't built yet. Run <code>pnpm -C web build</code> (or use a release \
binary). The API at <code>/api/*</code> is available.</p></body>";

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
