//! The axum router, app state, the printer-source abstraction, and a fake source.
//!
//! `PrinterSource` is the seam that keeps the dashboard testable without a real
//! printer: tests and `--fake` mode use [`FakeSource`]; the live source (wrapping
//! `LanMqttClient::monitor`) arrives in a later phase.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header::AUTHORIZATION};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use tokio::sync::watch;

use super::assets::static_handler;
use crate::core::status::PrinterStatus;

/// Something that can provide the printer's current status and a live stream of
/// updates. Abstracted so the server is testable without a network: tests and
/// `--fake` mode use [`FakeSource`], the live source (P2) wraps the MQTT monitor.
pub trait PrinterSource: Send + Sync {
    /// The latest known status.
    fn current(&self) -> PrinterStatus;
    /// Subscribe to status updates. The receiver's *current* value is whatever
    /// the source last held; callers should send it first (via `borrow_and_update`)
    /// and then await `changed()` for each subsequent update.
    fn subscribe(&self) -> watch::Receiver<PrinterStatus>;
}

/// A fake source for `--fake` mode and tests, backed by a [`watch`] channel so it
/// can stream like the real one. [`FakeSource::idle`] is static; [`FakeSource::ramping`]
/// simulates a running print (temps climb toward target, progress advances) so
/// the live charts have moving data to draw.
pub struct FakeSource {
    tx: watch::Sender<PrinterStatus>,
    // Held only to keep the channel's receiver count ≥ 1, so a ramping task's
    // `send` never sees "no receivers" and stops early when no client is attached.
    _keepalive: watch::Receiver<PrinterStatus>,
}

impl FakeSource {
    /// An idle, fault-free printer. Static — never emits an update.
    pub fn idle() -> Self {
        let (tx, rx) = watch::channel(PrinterStatus {
            gcode_state: Some("IDLE".to_string()),
            print_error: Some(0),
            ..Default::default()
        });
        Self { tx, _keepalive: rx }
    }

    /// A printer simulating a running print: nozzle/bed temps ramp toward their
    /// targets and progress advances one layer per `interval`, until 100% (then
    /// it reports `FINISH`). Spawns a task on the current tokio runtime.
    pub fn ramping(interval: Duration) -> Self {
        let initial = PrinterStatus {
            gcode_state: Some("RUNNING".to_string()),
            print_error: Some(0),
            nozzle_target: Some(220.0),
            bed_target: Some(60.0),
            nozzle_temper: Some(25.0),
            bed_temper: Some(25.0),
            mc_percent: Some(0),
            layer_num: Some(0),
            total_layer_num: Some(100),
            ..Default::default()
        };
        let (tx, rx) = watch::channel(initial.clone());
        let task_tx = tx.clone();
        tokio::spawn(async move {
            let mut s = initial;
            let mut tick: i64 = 0;
            loop {
                tokio::time::sleep(interval).await;
                tick += 1;
                s.nozzle_temper = Some(approach(s.nozzle_temper.unwrap_or(25.0), 220.0, 8.0));
                s.bed_temper = Some(approach(s.bed_temper.unwrap_or(25.0), 60.0, 4.0));
                let pct = tick.min(100);
                s.mc_percent = Some(pct);
                s.layer_num = Some(pct);
                if pct >= 100 {
                    s.gcode_state = Some("FINISH".to_string());
                }
                if task_tx.send(s.clone()).is_err() || pct >= 100 {
                    break;
                }
            }
        });
        Self { tx, _keepalive: rx }
    }
}

/// Move `current` toward `target` by at most `step` (a simple ramp for the fake).
fn approach(current: f64, target: f64, step: f64) -> f64 {
    if current < target {
        (current + step).min(target)
    } else {
        (current - step).max(target)
    }
}

impl PrinterSource for FakeSource {
    fn current(&self) -> PrinterStatus {
        self.tx.borrow().clone()
    }
    fn subscribe(&self) -> watch::Receiver<PrinterStatus> {
        self.tx.subscribe()
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
        .route("/api/ws", get(status_ws))
        .layer(middleware::from_fn_with_state(state.clone(), require_token));
    api.fallback(static_handler).with_state(state)
}

async fn status(State(st): State<AppState>) -> Json<PrinterStatus> {
    Json(st.source.current())
}

/// Upgrade to a WebSocket that pushes a `PrinterStatus` JSON frame on connect and
/// on every subsequent change.
async fn status_ws(State(st): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| stream_status(socket, st.source.clone()))
}

async fn stream_status(mut socket: WebSocket, source: Arc<dyn PrinterSource>) {
    let mut rx = source.subscribe();
    loop {
        // Send the current snapshot, marking it seen so `changed()` waits for the
        // *next* update regardless of the receiver's initial seen-state.
        let snapshot = rx.borrow_and_update().clone();
        let Ok(json) = serde_json::to_string(&snapshot) else {
            break;
        };
        if socket.send(Message::Text(json.into())).await.is_err() {
            break; // client gone
        }
        if rx.changed().await.is_err() {
            break; // source dropped
        }
    }
}

/// Reject `/api/*` requests without a matching token. The token may be supplied
/// as `Authorization: Bearer <token>` (used by `fetch`) **or** as a `?token=`
/// query parameter (used by the WebSocket, which can't set request headers in the
/// browser). Tokens are base64url, so no percent-decoding is needed in the query.
async fn require_token(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if authorized(&req, &st.token) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

fn authorized(req: &Request, token: &str) -> bool {
    let header_ok = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        == Some(token);
    let query_ok = req.uri().query().is_some_and(|q| {
        q.split('&')
            .any(|kv| kv.strip_prefix("token=") == Some(token))
    });
    header_ok || query_ok
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

    // WebSocket tests need the real HTTP transport (the mocked one can't upgrade).
    fn ws_server(state: AppState) -> TestServer {
        TestServer::builder().http_transport().build(router(state))
    }

    #[tokio::test]
    async fn ws_pushes_initial_status_with_query_token() {
        // The browser WebSocket can't set headers, so the token rides in ?token=.
        let mut ws = ws_server(AppState::fake())
            .get_websocket("/api/ws?token=testtoken")
            .await
            .into_websocket()
            .await;
        let msg: serde_json::Value = ws.receive_json().await;
        assert_eq!(msg["gcode_state"], "IDLE");
        assert_eq!(msg["print_error"], 0);
    }

    #[tokio::test]
    async fn ws_rejects_without_token() {
        let res = ws_server(AppState::fake()).get_websocket("/api/ws").await;
        res.assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ws_streams_subsequent_updates_from_a_ramping_source() {
        let state = AppState {
            source: Arc::new(FakeSource::ramping(Duration::from_millis(5))),
            token: "testtoken".to_string(),
        };
        let mut ws = ws_server(state)
            .get_websocket("/api/ws?token=testtoken")
            .await
            .into_websocket()
            .await;
        // First frame is the initial snapshot at 25 °C; a later frame must be hotter.
        let first: serde_json::Value = ws.receive_json().await;
        assert_eq!(first["gcode_state"], "RUNNING");
        let start = first["nozzle_temper"].as_f64().unwrap_or(0.0);
        let mut hotter = false;
        for _ in 0..5 {
            let next: serde_json::Value = ws.receive_json().await;
            if next["nozzle_temper"].as_f64().unwrap_or(0.0) > start {
                hotter = true;
                break;
            }
        }
        assert!(hotter, "ramping source should push rising nozzle temps");
    }
}
