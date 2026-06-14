//! The axum router, app state, the printer-source seam + fake source, and the
//! HTTP API (reads + control).
//!
//! Auth model: **reads** (`/api/status`, `/api/ws`) are always open; **writes**
//! (control) are gated by an optional password (`None` = open). The token concept
//! is gone — there's nothing to put in a URL.
//!
//! `PrinterSource`/`Controller` are the seams that keep the API testable without a
//! real printer: tests and `--fake` use [`FakeSource`]/[`FakeController`]; live
//! mode uses [`super::LiveSource`]/[`super::control::LiveController`].

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{StatusCode, header::AUTHORIZATION};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

#[cfg(feature = "dashboard")]
use super::assets::static_handler;
#[cfg(test)]
use super::control::FakeController;
use super::control::{ControlAction, ControlError, Controller};
#[cfg(test)]
use super::files::FakeFiles;
use super::files::FileStore;
#[cfg(test)]
use super::start::FakeStarter;
use super::start::{StartRequest, Starter};
use crate::core::command::{LedNode, SpeedLevel};
use crate::core::safety::{GcodeVerdict, TempLimits, check_gcode};
use crate::core::session::CommandOutcome;
use crate::core::status::{Ams, AmsTray, AmsUnit, Filament, Online, PrinterStatus};

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

    /// A printer simulating a 2-colour print: nozzle/bed temps ramp toward
    /// target, fans spin up, progress advances one layer per `interval`, and a
    /// loaded AMS (4 trays) is reported — enough to exercise every dashboard
    /// card. Runs to 100% then reports `FINISH`. Spawns a task on the current
    /// tokio runtime.
    pub fn ramping(interval: Duration) -> Self {
        let initial = PrinterStatus {
            gcode_state: Some("RUNNING".to_string()),
            print_error: Some(0),
            subtask_name: Some("benchy_2c.3mf".to_string()),
            gcode_file: Some("benchy_2c.3mf".to_string()),
            print_type: Some("local".to_string()),
            nozzle_target: Some(220.0),
            bed_target: Some(60.0),
            nozzle_temper: Some(25.0),
            bed_temper: Some(25.0),
            mc_percent: Some(0),
            layer_num: Some(0),
            total_layer_num: Some(200),
            remaining_time_min: Some(72),
            spd_lvl: Some(2),
            spd_mag: Some(100),
            cooling_fan_speed: Some(0),
            big_fan1_speed: Some(0),
            heatbreak_fan_speed: Some(7000),
            nozzle_diameter: Some("0.4".to_string()),
            nozzle_type: Some("stainless_steel".to_string()),
            sdcard: Some(true),
            wifi_signal: Some("-58dBm".to_string()),
            online: Some(Online {
                ahb: Some(true),
                rfid: Some(true),
                version: Some(1),
            }),
            filament: Some(Filament {
                location: "ams0".to_string(),
                material: Some("PLA".to_string()),
                name: Some("PLA Matte".to_string()),
                color: Some("DE4343FF".to_string()),
            }),
            ams: Some(fake_ams()),
            ..Default::default()
        };
        let (tx, rx) = watch::channel(initial.clone());
        let task_tx = tx.clone();
        tokio::spawn(async move {
            let mut s = initial;
            let mut tick: i64 = 0;
            // Perpetual cycle so a left-open demo never goes stale: ~100 ticks
            // printing (heat + progress), then ~15 ticks FINISH/cool-down, then
            // a fresh print. The sparkline shows the resulting saw-tooth.
            const PRINT: i64 = 100;
            const CYCLE: i64 = 115;
            loop {
                tokio::time::sleep(interval).await;
                tick += 1;
                let p = tick % CYCLE;
                if p == 1 {
                    // A new print starts cold.
                    s.nozzle_temper = Some(25.0);
                    s.bed_temper = Some(25.0);
                }
                if (1..=PRINT).contains(&p) {
                    s.gcode_state = Some("RUNNING".to_string());
                    s.nozzle_temper = Some(approach(s.nozzle_temper.unwrap_or(25.0), 220.0, 8.0));
                    s.bed_temper = Some(approach(s.bed_temper.unwrap_or(25.0), 60.0, 4.0));
                    // Part-cooling fan spins up once the hotend is near temperature.
                    let hot = s.nozzle_temper.unwrap_or(0.0) >= 200.0;
                    s.cooling_fan_speed = Some(if hot { 100 } else { 0 });
                    s.mc_percent = Some(p);
                    s.layer_num = Some(p * 2); // 200 total layers
                    s.remaining_time_min = Some((PRINT - p) * 72 / 100);
                } else {
                    // Finished: hold at 100% and cool toward ambient.
                    s.gcode_state = Some("FINISH".to_string());
                    s.mc_percent = Some(100);
                    s.layer_num = Some(200);
                    s.remaining_time_min = Some(0);
                    s.cooling_fan_speed = Some(0);
                    s.nozzle_temper = Some(approach(s.nozzle_temper.unwrap_or(220.0), 30.0, 12.0));
                    s.bed_temper = Some(approach(s.bed_temper.unwrap_or(60.0), 30.0, 6.0));
                }
                if task_tx.send(s.clone()).is_err() {
                    break; // all receivers gone
                }
            }
        });
        Self { tx, _keepalive: rx }
    }
}

/// A loaded AMS for the fake: 1 unit, 4 spools, red (tray 0) active.
fn fake_ams() -> Ams {
    let tray = |id: &str, material: &str, name: &str, color: &str, active: bool| AmsTray {
        id: id.to_string(),
        material: Some(material.to_string()),
        name: Some(name.to_string()),
        color: Some(color.to_string()),
        cols: vec![color.to_string()],
        remain: Some(-1), // A1 spools don't report a usable remaining %
        state: Some(3),
        nozzle_temp_min: Some(if material == "PETG" { 230 } else { 190 }),
        nozzle_temp_max: Some(if material == "PETG" { 260 } else { 230 }),
        is_active: active,
        is_target: active,
        ..Default::default()
    };
    Ams {
        units: vec![AmsUnit {
            id: "0".to_string(),
            humidity: Some(5),
            humidity_raw: Some(28),
            temp: Some(0.0),
            dry_time: None,
            trays: vec![
                tray("0", "PLA", "PLA Matte Red", "DE4343FF", true),
                tray("1", "PLA", "PLA Basic Black", "000000FF", false),
                tray("2", "PETG", "PETG Translucent", "D6ABFF80", false),
                tray("3", "PLA", "PLA Wood", "918669FF", false),
            ],
        }],
        external: None,
        active_tray: Some("0".to_string()),
        target_tray: Some("0".to_string()),
        previous_tray: Some("255".to_string()),
        ams_exist_bits: Some("1".to_string()),
        tray_exist_bits: Some("f".to_string()),
        tray_is_bbl_bits: Some("f".to_string()),
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
    pub controller: Arc<dyn Controller>,
    pub files: Arc<dyn FileStore>,
    pub starter: Arc<dyn Starter>,
    /// Optional password gating **write** (control) requests; `None` = control is
    /// open. Reads are always unauthenticated.
    pub password: Option<String>,
}

impl AppState {
    #[cfg(test)]
    pub fn fake() -> Self {
        Self {
            source: Arc::new(FakeSource::idle()),
            controller: Arc::new(FakeController::verified()),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: None,
        }
    }
}

/// Build the API router: open reads, password-gated writes, and — when the
/// `dashboard` feature is on — the embedded SPA as the fallback.
pub fn router(state: AppState) -> Router {
    let reads = Router::new()
        .route("/api/status", get(status))
        .route("/api/ws", get(status_ws))
        .route("/api/files", get(list_files));
    let writes = Router::new()
        .route("/api/job/pause", post(job_pause))
        .route("/api/job/resume", post(job_resume))
        .route("/api/job/stop", post(job_stop))
        .route("/api/job/start", post(job_start))
        .route("/api/light", post(light))
        .route("/api/speed", post(speed))
        .route("/api/gcode", post(gcode))
        // Uploads can be large (sliced 3mf); raise the body cap from the 2 MB default.
        .route(
            "/api/files/upload",
            post(upload_file).layer(DefaultBodyLimit::max(256 * 1024 * 1024)),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_password,
        ));
    let app = reads.merge(writes);
    #[cfg(feature = "dashboard")]
    let app = app.fallback(static_handler);
    app.with_state(state)
}

async fn status(State(st): State<AppState>) -> Json<PrinterStatus> {
    Json(st.source.current())
}

// ── Control (write) endpoints ──────────────────────────────────────────────

/// Body for a destructive job action — requires explicit `{"confirm": true}`,
/// mirroring the CLI's `--confirm` (an absent/empty body is "not confirmed").
#[derive(Deserialize, Default)]
struct ConfirmBody {
    #[serde(default)]
    confirm: bool,
}

#[derive(Deserialize)]
struct LightBody {
    node: String,
    on: bool,
}

#[derive(Deserialize)]
struct SpeedBody {
    level: String,
}

async fn job_pause(State(st): State<AppState>, body: Option<Json<ConfirmBody>>) -> Response {
    run_confirmed(st, ControlAction::Pause, body).await
}
async fn job_resume(State(st): State<AppState>, body: Option<Json<ConfirmBody>>) -> Response {
    run_confirmed(st, ControlAction::Resume, body).await
}
async fn job_stop(State(st): State<AppState>, body: Option<Json<ConfirmBody>>) -> Response {
    run_confirmed(st, ControlAction::Stop, body).await
}

async fn light(State(st): State<AppState>, Json(b): Json<LightBody>) -> Response {
    let node = match b.node.as_str() {
        "chamber" => LedNode::ChamberLight,
        "work" => LedNode::WorkLight,
        other => return bad_request(format!("unknown light node {other:?}")),
    };
    execute(st, ControlAction::Light { node, on: b.on }).await
}

async fn speed(State(st): State<AppState>, Json(b): Json<SpeedBody>) -> Response {
    let level = match b.level.as_str() {
        "silent" => SpeedLevel::Silent,
        "standard" => SpeedLevel::Standard,
        "sport" => SpeedLevel::Sport,
        "ludicrous" => SpeedLevel::Ludicrous,
        other => return bad_request(format!("unknown speed level {other:?}")),
    };
    execute(st, ControlAction::Speed(level)).await
}

#[derive(Deserialize)]
struct GcodeBody {
    line: String,
    #[serde(default)]
    confirm: bool,
    /// Override the safety blocklist (over-limit temps / cold extrusion).
    #[serde(default)]
    force: bool,
}

/// Send a raw gcode line. Mirrors the CLI `gcode`: requires confirm (428), and
/// the safety blocklist refuses dangerous lines (400) unless `force`.
async fn gcode(State(st): State<AppState>, Json(b): Json<GcodeBody>) -> Response {
    if b.line.trim().is_empty() {
        return bad_request("empty gcode line".to_string());
    }
    if !b.confirm {
        return (
            StatusCode::PRECONDITION_REQUIRED,
            Json(json!({ "error": "confirm required: POST {\"confirm\": true}" })),
        )
            .into_response();
    }
    if !b.force
        && let GcodeVerdict::Block(reason) = check_gcode(&b.line, &TempLimits::default())
    {
        return bad_request(format!("unsafe gcode (use force to override): {reason}"));
    }
    execute(st, ControlAction::Gcode(b.line)).await
}

/// Require `{"confirm": true}` before running a destructive action (428 if not).
async fn run_confirmed(
    st: AppState,
    action: ControlAction,
    body: Option<Json<ConfirmBody>>,
) -> Response {
    if !body.map(|b| b.confirm).unwrap_or(false) {
        return (
            StatusCode::PRECONDITION_REQUIRED,
            Json(json!({ "error": "confirm required: POST {\"confirm\": true}" })),
        )
            .into_response();
    }
    execute(st, action).await
}

/// Run a control action on the blocking pool and map the verify outcome to HTTP.
async fn execute(st: AppState, action: ControlAction) -> Response {
    let controller = st.controller.clone();
    let res = tokio::task::spawn_blocking(move || controller.execute(action)).await;
    verify_response(res)
}

/// Result of running a verify on the blocking pool: the verdict (or transport
/// error), wrapped in the `spawn_blocking` join result.
type VerifyJoin = Result<Result<CommandOutcome, ControlError>, tokio::task::JoinError>;

/// Map a `spawn_blocking` verify result to HTTP: verified → 200, unverified →
/// 202, rejected → 409, transport error → 502, join error → 500.
fn verify_response(res: VerifyJoin) -> Response {
    match res {
        Ok(Ok(outcome)) => {
            let code = match &outcome {
                CommandOutcome::Verified => StatusCode::OK,
                CommandOutcome::Unverified { .. } => StatusCode::ACCEPTED,
                CommandOutcome::Rejected { .. } => StatusCode::CONFLICT,
            };
            (code, Json(outcome)).into_response()
        }
        Ok(Err(ControlError::Transport(e))) => {
            (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "control task failed" })),
        )
            .into_response(),
    }
}

fn bad_request(msg: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
}

// ── Print start ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StartBody {
    file: String,
    #[serde(default = "default_plate")]
    plate: u32,
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    use_ams: bool,
    #[serde(default)]
    ams_map: Vec<i32>,
    bed_type: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

fn default_plate() -> u32 {
    1
}

/// Start a print. Safety mirrors the CLI: file/AMS-map validation, a `dry_run`
/// that returns the resolved plan without sending, a `confirm` gate (428), and
/// an idle check against the live status (409 if the printer is busy).
async fn job_start(State(st): State<AppState>, Json(b): Json<StartBody>) -> Response {
    let lower = b.file.to_ascii_lowercase();
    if b.file.is_empty() || b.file.contains("..") {
        return bad_request(format!("invalid file {:?}", b.file));
    }
    if !(lower.ends_with(".3mf") || lower.ends_with(".gcode")) {
        return bad_request("file must be a .3mf or .gcode".to_string());
    }
    if b.use_ams {
        for (i, v) in b.ams_map.iter().enumerate() {
            if !(-1..=3).contains(v) {
                return bad_request(format!(
                    "ams_map[{i}]={v} out of range (trays 0..3, or -1 external)"
                ));
            }
        }
    }
    let req = StartRequest {
        file: b.file.clone(),
        plate: b.plate,
        use_ams: b.use_ams,
        ams_map: b.ams_map.clone(),
        bed_type: b.bed_type.clone().unwrap_or_else(|| "auto".to_string()),
    };

    if b.dry_run {
        return Json(json!({ "plan": {
            "file": req.file,
            "plate": req.plate,
            "use_ams": req.use_ams,
            "ams_map": req.ams_map,
            "bed_type": req.bed_type,
        }}))
        .into_response();
    }
    if !b.confirm {
        return (
            StatusCode::PRECONDITION_REQUIRED,
            Json(json!({ "error": "confirm required: POST {\"confirm\": true} (try dry_run first)" })),
        )
            .into_response();
    }
    // Idle guard: refuse to start over an active job.
    let state = st
        .source
        .current()
        .gcode_state
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(state.as_str(), "RUNNING" | "PAUSE" | "PREPARE" | "SLICING") {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("printer is busy ({state}); cannot start a print") })),
        )
            .into_response();
    }
    let starter = st.starter.clone();
    let res = tokio::task::spawn_blocking(move || starter.start(&req)).await;
    verify_response(res)
}

// ── File endpoints ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ListQuery {
    dir: Option<String>,
}

/// List files on the printer (open read). `?dir=` defaults to `/`.
async fn list_files(State(st): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let dir = q.dir.unwrap_or_else(|| "/".to_string());
    let files = st.files.clone();
    match tokio::task::spawn_blocking(move || files.list(&dir)).await {
        Ok(Ok(names)) => Json(json!({ "files": names })).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "file task failed" })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct UploadQuery {
    dir: Option<String>,
    name: String,
}

/// Upload a file to the printer (write). The body is the raw file bytes;
/// `?name=` is the filename and `?dir=` the destination (default `/`).
async fn upload_file(
    State(st): State<AppState>,
    Query(q): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    // Reject path-traversal / nested names — `name` is a single filename.
    if q.name.is_empty() || q.name.contains('/') || q.name.contains('\\') || q.name.contains("..") {
        return bad_request(format!("invalid filename {:?}", q.name));
    }
    let dir = q.dir.unwrap_or_else(|| "/".to_string());
    let remote = format!("{}/{}", dir.trim_end_matches('/'), q.name);
    let name = q.name.clone();
    let files = st.files.clone();
    let bytes = body.to_vec();
    match tokio::task::spawn_blocking(move || files.upload(&remote, bytes)).await {
        Ok(Ok(())) => Json(json!({ "uploaded": name })).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "upload task failed" })),
        )
            .into_response(),
    }
}

/// Upgrade to a WebSocket that pushes a `PrinterStatus` JSON frame on connect and
/// on every subsequent change.
async fn status_ws(State(st): State<AppState>, ws: WebSocketUpgrade) -> Response {
    eprintln!("ws: client upgrade accepted");
    ws.on_upgrade(move |socket| async move {
        stream_status(socket, st.source.clone()).await;
        eprintln!("ws: client disconnected");
    })
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

/// Gate **write** requests on the optional password. `None` ⇒ control is open
/// (the default). When set, the password must arrive as `Authorization: Bearer
/// <password>`. Reads never reach this middleware.
async fn require_password(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let Some(pw) = st.password.as_deref() else {
        return next.run(req).await; // no password configured: control is open
    };
    let given = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if given == Some(pw) {
        next.run(req).await
    } else {
        eprintln!("auth: rejected write {} {}", req.method(), req.uri().path());
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "password required" })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::session::VerifyStage;
    use axum_test::TestServer;

    /// Build a test server with a chosen password + controller (idle source).
    fn app(password: Option<&str>, controller: impl Controller + 'static) -> TestServer {
        let state = AppState {
            source: Arc::new(FakeSource::idle()),
            controller: Arc::new(controller),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: password.map(str::to_owned),
        };
        TestServer::new(router(state))
    }

    // ── reads are always open ──
    #[tokio::test]
    async fn status_is_open_and_returns_printer_status_json() {
        let res = app(None, FakeController::verified())
            .get("/api/status")
            .await;
        res.assert_status_ok();
        let body: serde_json::Value = res.json();
        assert_eq!(body["gcode_state"], "IDLE");
        assert_eq!(body["print_error"], 0);
    }

    #[tokio::test]
    async fn status_is_open_even_when_a_password_is_set() {
        // A password gates writes only — reads stay open.
        app(Some("secret"), FakeController::verified())
            .get("/api/status")
            .await
            .assert_status_ok();
    }

    // ── control: confirm gating ──
    #[tokio::test]
    async fn job_stop_needs_confirmation() {
        app(None, FakeController::verified())
            .post("/api/job/stop")
            .await
            .assert_status(StatusCode::PRECONDITION_REQUIRED);
    }

    #[tokio::test]
    async fn job_pause_confirmed_returns_verified() {
        let res = app(None, FakeController::verified())
            .post("/api/job/pause")
            .json(&json!({ "confirm": true }))
            .await;
        res.assert_status_ok();
        assert_eq!(res.json::<serde_json::Value>()["outcome"], "verified");
    }

    // ── control: outcome → HTTP status ──
    #[tokio::test]
    async fn rejected_outcome_maps_to_409() {
        let c = FakeController::returning(CommandOutcome::Rejected {
            reason: "busy".into(),
        });
        app(None, c)
            .post("/api/job/stop")
            .json(&json!({ "confirm": true }))
            .await
            .assert_status(StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn unverified_outcome_maps_to_202() {
        let c = FakeController::returning(CommandOutcome::Unverified {
            stage: VerifyStage::Effect,
        });
        app(None, c)
            .post("/api/light")
            .json(&json!({ "node": "chamber", "on": true }))
            .await
            .assert_status(StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn transport_failure_maps_to_502() {
        app(None, FakeController::failing())
            .post("/api/light")
            .json(&json!({ "node": "chamber", "on": false }))
            .await
            .assert_status(StatusCode::BAD_GATEWAY);
    }

    // ── control: input validation ──
    #[tokio::test]
    async fn unknown_light_node_is_400() {
        app(None, FakeController::verified())
            .post("/api/light")
            .json(&json!({ "node": "kitchen", "on": true }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn speed_level_sets_ok() {
        app(None, FakeController::verified())
            .post("/api/speed")
            .json(&json!({ "level": "standard" }))
            .await
            .assert_status_ok();
    }

    // ── control: password gating ──
    #[tokio::test]
    async fn write_without_password_is_401_when_one_is_set() {
        app(Some("secret"), FakeController::verified())
            .post("/api/light")
            .json(&json!({ "node": "chamber", "on": true }))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn write_with_correct_password_is_allowed() {
        app(Some("secret"), FakeController::verified())
            .post("/api/light")
            .authorization_bearer("secret")
            .json(&json!({ "node": "chamber", "on": true }))
            .await
            .assert_status_ok();
    }

    // ── gcode ──
    #[tokio::test]
    async fn gcode_needs_confirmation() {
        app(None, FakeController::verified())
            .post("/api/gcode")
            .json(&json!({ "line": "G28" }))
            .await
            .assert_status(StatusCode::PRECONDITION_REQUIRED);
    }

    #[tokio::test]
    async fn gcode_safe_line_runs() {
        app(None, FakeController::verified())
            .post("/api/gcode")
            .json(&json!({ "line": "G28", "confirm": true }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn gcode_unsafe_line_is_blocked_unless_forced() {
        let s = app(None, FakeController::verified());
        // An over-limit nozzle temp is on the blocklist.
        s.post("/api/gcode")
            .json(&json!({ "line": "M104 S999", "confirm": true }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        // force overrides it.
        s.post("/api/gcode")
            .json(&json!({ "line": "M104 S999", "confirm": true, "force": true }))
            .await
            .assert_status_ok();
    }

    // ── files ──
    #[tokio::test]
    async fn list_files_is_open() {
        let res = app(Some("secret"), FakeController::verified())
            .get("/api/files")
            .await;
        res.assert_status_ok();
        let body: serde_json::Value = res.json();
        assert!(
            body["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f == "coin2c.gcode.3mf")
        );
    }

    #[tokio::test]
    async fn upload_open_when_no_password() {
        app(None, FakeController::verified())
            .post("/api/files/upload?name=part.gcode.3mf")
            .bytes(b"PK\x03\x04 fake 3mf".to_vec().into())
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn upload_needs_password_when_set() {
        app(Some("secret"), FakeController::verified())
            .post("/api/files/upload?name=part.gcode.3mf")
            .bytes(b"data".to_vec().into())
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn upload_rejects_path_traversal() {
        app(None, FakeController::verified())
            .post("/api/files/upload?name=../etc/passwd")
            .bytes(b"data".to_vec().into())
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    // ── print start ──
    #[tokio::test]
    async fn start_dry_run_returns_plan_without_confirm() {
        let res = app(None, FakeController::verified())
            .post("/api/job/start")
            .json(&json!({ "file": "/coin.gcode.3mf", "plate": 2, "dry_run": true }))
            .await;
        res.assert_status_ok();
        let body: serde_json::Value = res.json();
        assert_eq!(body["plan"]["plate"], 2);
        assert_eq!(body["plan"]["bed_type"], "auto");
    }

    #[tokio::test]
    async fn start_needs_confirmation() {
        app(None, FakeController::verified())
            .post("/api/job/start")
            .json(&json!({ "file": "/coin.gcode.3mf" }))
            .await
            .assert_status(StatusCode::PRECONDITION_REQUIRED);
    }

    #[tokio::test]
    async fn start_confirmed_on_idle_printer_verifies() {
        // AppState::fake() source is IDLE, so the idle guard passes.
        app(None, FakeController::verified())
            .post("/api/job/start")
            .json(&json!({ "file": "/coin.gcode.3mf", "confirm": true }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn start_rejects_bad_filetype_and_traversal() {
        let s = app(None, FakeController::verified());
        s.post("/api/job/start")
            .json(&json!({ "file": "/notes.txt", "confirm": true }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        s.post("/api/job/start")
            .json(&json!({ "file": "../secret.3mf", "confirm": true }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn start_rejects_out_of_range_ams_map() {
        app(None, FakeController::verified())
            .post("/api/job/start")
            .json(&json!({ "file": "/c.3mf", "confirm": true, "use_ams": true, "ams_map": [0, 9] }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn start_on_busy_printer_is_409() {
        // A RUNNING source → idle guard refuses.
        let state = AppState {
            source: Arc::new(FakeSource::ramping(Duration::from_millis(50))),
            controller: Arc::new(FakeController::verified()),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: None,
        };
        TestServer::new(router(state))
            .post("/api/job/start")
            .json(&json!({ "file": "/c.3mf", "confirm": true }))
            .await
            .assert_status(StatusCode::CONFLICT);
    }

    // WebSocket tests need the real HTTP transport (the mocked one can't upgrade).
    fn ws_server(state: AppState) -> TestServer {
        TestServer::builder().http_transport().build(router(state))
    }

    #[tokio::test]
    async fn ws_is_open_and_pushes_initial_status() {
        let mut ws = ws_server(AppState::fake())
            .get_websocket("/api/ws")
            .await
            .into_websocket()
            .await;
        let msg: serde_json::Value = ws.receive_json().await;
        assert_eq!(msg["gcode_state"], "IDLE");
        assert_eq!(msg["print_error"], 0);
    }

    #[tokio::test]
    async fn ws_streams_subsequent_updates_from_a_ramping_source() {
        let state = AppState {
            source: Arc::new(FakeSource::ramping(Duration::from_millis(5))),
            controller: Arc::new(FakeController::verified()),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: None,
        };
        let mut ws = ws_server(state)
            .get_websocket("/api/ws")
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
