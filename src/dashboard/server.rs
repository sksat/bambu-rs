//! The axum router, app state, the printer-source abstraction, and a fake source.
//!
//! `PrinterSource` is the seam that keeps the dashboard testable without a real
//! printer: tests and `--fake` mode use [`FakeSource`]; the live source (wrapping
//! `LanMqttClient::monitor`) arrives in a later phase.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header::AUTHORIZATION};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use super::assets::static_handler;
use crate::core::status::PrinterStatus;

/// Something that can provide the printer's current status (and, later, a live
/// stream). Abstracted so the server is testable without a network.
pub trait PrinterSource: Send + Sync {
    /// The latest known status.
    fn current(&self) -> PrinterStatus;
}

/// A static fake source for `--fake` mode and tests.
pub struct FakeSource {
    status: PrinterStatus,
}

impl FakeSource {
    /// An idle, fault-free printer.
    pub fn idle() -> Self {
        Self {
            status: PrinterStatus {
                gcode_state: Some("IDLE".to_string()),
                print_error: Some(0),
                ..Default::default()
            },
        }
    }
}

impl PrinterSource for FakeSource {
    fn current(&self) -> PrinterStatus {
        self.status.clone()
    }
}

/// Shared server state.
#[derive(Clone)]
pub struct AppState {
    pub source: Arc<dyn PrinterSource>,
    /// Bearer token required on all `/api/*` routes.
    pub token: String,
}

impl AppState {
    #[cfg(test)]
    pub fn fake() -> Self {
        Self {
            source: Arc::new(FakeSource::idle()),
            token: "testtoken".to_string(),
        }
    }
}

/// Build the dashboard router: token-gated `/api/*`, with the embedded SPA served
/// (un-gated) as the fallback so the page can load before authenticating.
pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/status", get(status))
        .layer(middleware::from_fn_with_state(state.clone(), require_token));
    api.fallback(static_handler).with_state(state)
}

async fn status(State(st): State<AppState>) -> Json<PrinterStatus> {
    Json(st.source.current())
}

/// Reject `/api/*` requests without a matching `Authorization: Bearer <token>`.
async fn require_token(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let want = format!("Bearer {}", st.token);
    let ok = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        == Some(want.as_str());
    if ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::TestServer;

    #[tokio::test]
    async fn status_endpoint_returns_printer_status_json() {
        let server = TestServer::new(router(AppState::fake()));
        let res = server
            .get("/api/status")
            .authorization_bearer("testtoken")
            .await;
        res.assert_status_ok();
        let body: serde_json::Value = res.json();
        assert_eq!(body["gcode_state"], "IDLE");
        assert_eq!(body["print_error"], 0);
    }

    #[tokio::test]
    async fn status_endpoint_rejects_without_token() {
        let server = TestServer::new(router(AppState::fake()));
        server
            .get("/api/status")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn status_endpoint_rejects_wrong_token() {
        let server = TestServer::new(router(AppState::fake()));
        server
            .get("/api/status")
            .authorization_bearer("nope")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_path_falls_back_to_index_html_without_a_token() {
        // Static assets must load without auth (so the SPA can then authenticate).
        let server = TestServer::new(router(AppState::fake()));
        let res = server.get("/some/spa/route").await;
        res.assert_status_ok();
    }
}
