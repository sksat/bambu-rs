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

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{
    StatusCode,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

#[cfg(feature = "dashboard")]
use super::assets::static_handler;
#[cfg(test)]
use super::control::FakeController;
use super::control::{
    Axis, ControlAction, ControlError, Controller, HomeAxes, TempPart, temp_line,
};
#[cfg(test)]
use super::files::FakeFiles;
use super::files::FileStore;
#[cfg(test)]
use super::start::FakeStarter;
use super::start::{StartRequest, Starter};
use crate::core::command::{AmsControl, LedNode, SpeedLevel};
use crate::core::safety::{GcodeVerdict, TempLimits, check_extrude, check_gcode, check_jog};
use crate::core::session::CommandOutcome;
use crate::core::status::{Ams, AmsTray, AmsUnit, Filament, LightReport, Online, PrinterStatus};

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
            lights: vec![LightReport {
                node: "chamber_light".to_string(),
                mode: "off".to_string(),
            }],
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
    /// Held for the duration of a `job/start` so two concurrent starts can't both
    /// pass the idle check.
    pub start_lock: Arc<tokio::sync::Mutex<()>>,
}

/// A safe absolute path on the printer: starts with `/`, no traversal or scheme.
fn is_safe_remote_path(p: &str) -> bool {
    p.starts_with('/')
        && p.len() > 1
        && !p.contains("..")
        && !p.contains("//")
        && !p.contains('\\')
        && !p.contains(':')
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
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

/// Build the API router: open reads, password-gated writes, and — when the
/// `dashboard` feature is on — the embedded SPA as the fallback.
pub fn router(state: AppState) -> Router {
    let reads = Router::new()
        .route("/api/status", get(status))
        .route("/api/ws", get(status_ws))
        .route("/api/files", get(list_files))
        .route("/api/files/thumbnail", get(file_thumbnail))
        .route("/api/files/raw", get(file_raw))
        .route("/api/files/gcode", get(file_gcode))
        .route("/api/files/mesh", get(file_mesh));
    let writes = Router::new()
        .route("/api/job/pause", post(job_pause))
        .route("/api/job/resume", post(job_resume))
        .route("/api/job/stop", post(job_stop))
        .route("/api/job/start", post(job_start))
        .route("/api/light", post(light))
        .route("/api/speed", post(speed))
        .route("/api/gcode", post(gcode))
        .route("/api/home", post(home))
        .route("/api/move", post(move_axis))
        .route("/api/extrude", post(extrude))
        .route("/api/temp", post(temp))
        .route("/api/calibrate", post(calibrate))
        .route("/api/ams", post(ams))
        .route("/api/reboot", post(reboot))
        .route("/api/steppers", post(steppers))
        // Uploads stream to a temp file, so the cap bounds disk, not memory.
        .route(
            "/api/files/upload",
            post(upload_file).layer(DefaultBodyLimit::max(512 * 1024 * 1024)),
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

// ── Shared control gates ─────────────────────────────────────────────────────

/// Refuse a control action while the printer is busy (409). The predicate
/// mirrors `job_start`'s idle guard exactly: any of RUNNING/PAUSE/PREPARE/SLICING
/// (case-insensitive) is "busy". `None` ⇒ idle, run the action.
fn require_idle(st: &AppState) -> Option<Response> {
    let state = st
        .source
        .current()
        .gcode_state
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(state.as_str(), "RUNNING" | "PAUSE" | "PREPARE" | "SLICING") {
        return Some(
            (
                StatusCode::CONFLICT,
                Json(json!({ "error": format!("printer is busy ({state}); operation refused") })),
            )
                .into_response(),
        );
    }
    None
}

/// Require explicit `{"confirm": true}` before a destructive action (428 if not).
/// `None` ⇒ confirmed, proceed.
fn need_confirm(confirm: bool) -> Option<Response> {
    if confirm {
        return None;
    }
    Some(
        (
            StatusCode::PRECONDITION_REQUIRED,
            Json(json!({ "error": "confirm required: POST {\"confirm\": true}" })),
        )
            .into_response(),
    )
}

// ── Machine control (write) endpoints ────────────────────────────────────────

#[derive(Deserialize)]
struct HomeBody {
    #[serde(default = "default_axes")]
    axes: String,
}

fn default_axes() -> String {
    "all".to_string()
}

/// Home one or all axes (`G28`). Idle-gated (no confirm).
async fn home(State(st): State<AppState>, Json(b): Json<HomeBody>) -> Response {
    let axes = match b.axes.as_str() {
        "all" => HomeAxes::All,
        "x" => HomeAxes::X,
        "y" => HomeAxes::Y,
        "z" => HomeAxes::Z,
        other => return bad_request(format!("unknown axes {other:?}")),
    };
    if let Some(busy) = require_idle(&st) {
        return busy;
    }
    execute(st, ControlAction::Home(axes)).await
}

#[derive(Deserialize)]
struct MoveBody {
    axis: String,
    delta: f64,
    #[serde(default = "default_move_feedrate")]
    feedrate: u32,
}

fn default_move_feedrate() -> u32 {
    3000
}

/// Jog a single axis a relative distance (`G91; G1; G90`). Idle-gated, no
/// confirm; the distance and feedrate are bounds-checked.
async fn move_axis(State(st): State<AppState>, Json(b): Json<MoveBody>) -> Response {
    let axis = match b.axis.as_str() {
        "x" => Axis::X,
        "y" => Axis::Y,
        "z" => Axis::Z,
        other => return bad_request(format!("unknown axis {other:?}")),
    };
    if let GcodeVerdict::Block(reason) = check_jog(b.delta) {
        return bad_request(reason);
    }
    if !(60..=6000).contains(&b.feedrate) {
        return bad_request(format!("feedrate {} out of range (60..=6000)", b.feedrate));
    }
    if let Some(busy) = require_idle(&st) {
        return busy;
    }
    execute(
        st,
        ControlAction::Move {
            axis,
            delta: b.delta,
            feedrate: b.feedrate,
        },
    )
    .await
}

#[derive(Deserialize)]
struct ExtrudeBody {
    delta: f64,
    #[serde(default = "default_extrude_feedrate")]
    feedrate: u32,
}

fn default_extrude_feedrate() -> u32 {
    300
}

/// Extrude or retract filament (`M83; G1 E; M82`). Idle-gated, no confirm. The
/// cold-extrusion guard reads the live nozzle temperature and has **no** force
/// bypass.
async fn extrude(State(st): State<AppState>, Json(b): Json<ExtrudeBody>) -> Response {
    let nozzle_temper = st.source.current().nozzle_temper;
    if let GcodeVerdict::Block(reason) = check_extrude(b.delta, nozzle_temper) {
        return bad_request(reason);
    }
    if !(60..=6000).contains(&b.feedrate) {
        return bad_request(format!("feedrate {} out of range (60..=6000)", b.feedrate));
    }
    if let Some(busy) = require_idle(&st) {
        return busy;
    }
    execute(
        st,
        ControlAction::Extrude {
            delta: b.delta,
            feedrate: b.feedrate,
        },
    )
    .await
}

#[derive(Deserialize)]
struct TempBody {
    part: String,
    celsius: u32,
    #[serde(default)]
    confirm: bool,
    /// Override the temperature ceiling (over-limit setpoint).
    #[serde(default)]
    force: bool,
}

/// Set a heater target (`M104`/`M140`). Not idle-gated — a cooldown (`celsius:
/// 0`) is the abort valve and is always allowed without confirm. A non-zero
/// setpoint needs confirm (428) and must clear the safety ceiling (400) unless
/// `force` overrides it, exactly like `/api/gcode`.
async fn temp(State(st): State<AppState>, Json(b): Json<TempBody>) -> Response {
    let part = match b.part.as_str() {
        "nozzle" => TempPart::Nozzle,
        "bed" => TempPart::Bed,
        other => return bad_request(format!("unknown part {other:?}")),
    };
    let line = temp_line(part, b.celsius);
    if !b.force
        && let GcodeVerdict::Block(reason) = check_gcode(&line, &TempLimits::default())
    {
        return bad_request(format!(
            "unsafe temperature (use force to override): {reason}"
        ));
    }
    // A cooldown (0 °C) is always allowed — it's the panic "turn it off" valve.
    if b.celsius > 0
        && let Some(unconfirmed) = need_confirm(b.confirm)
    {
        return unconfirmed;
    }
    execute(
        st,
        ControlAction::SetTemp {
            part,
            celsius: b.celsius,
        },
    )
    .await
}

#[derive(Deserialize)]
struct CalibrateBody {
    #[serde(default)]
    bed_level: bool,
    #[serde(default)]
    vibration: bool,
    #[serde(default)]
    motor_noise: bool,
    #[serde(default)]
    confirm: bool,
}

/// Run one or more calibrations. Requires at least one flag (400), confirm
/// (428), and an idle printer (409).
async fn calibrate(State(st): State<AppState>, Json(b): Json<CalibrateBody>) -> Response {
    if !(b.bed_level || b.vibration || b.motor_noise) {
        return bad_request(
            "select at least one calibration (bed_level/vibration/motor_noise)".to_string(),
        );
    }
    if let Some(unconfirmed) = need_confirm(b.confirm) {
        return unconfirmed;
    }
    if let Some(busy) = require_idle(&st) {
        return busy;
    }
    execute(
        st,
        ControlAction::Calibrate {
            bed_level: b.bed_level,
            vibration: b.vibration,
            motor_noise: b.motor_noise,
        },
    )
    .await
}

#[derive(Deserialize)]
struct AmsBody {
    action: String,
    #[serde(default)]
    confirm: bool,
}

/// AMS control. `resume` clears a pause and is allowed any time (no confirm,
/// no idle gate); `reset`/`pause` are destructive — confirm (428) + idle (409).
async fn ams(State(st): State<AppState>, Json(b): Json<AmsBody>) -> Response {
    let action = match b.action.as_str() {
        "resume" => AmsControl::Resume,
        "reset" => AmsControl::Reset,
        "pause" => AmsControl::Pause,
        other => return bad_request(format!("unknown ams action {other:?}")),
    };
    // resume is the "carry on" action; reset/pause change AMS state, so gate them.
    if !matches!(action, AmsControl::Resume) {
        if let Some(unconfirmed) = need_confirm(b.confirm) {
            return unconfirmed;
        }
        if let Some(busy) = require_idle(&st) {
            return busy;
        }
    }
    execute(st, ControlAction::Ams(action)).await
}

/// Reboot the printer (`system.reboot`). Confirm (428) + idle (409). Fire-and-
/// forget — there's no ACK to read back, so a success is 202 (Unverified).
async fn reboot(State(st): State<AppState>, body: Option<Json<ConfirmBody>>) -> Response {
    if let Some(unconfirmed) = need_confirm(body.map(|b| b.confirm).unwrap_or(false)) {
        return unconfirmed;
    }
    if let Some(busy) = require_idle(&st) {
        return busy;
    }
    execute(st, ControlAction::Reboot).await
}

/// Disable the stepper motors (`M84`). Confirm (428) + idle (409).
async fn steppers(State(st): State<AppState>, body: Option<Json<ConfirmBody>>) -> Response {
    if let Some(unconfirmed) = need_confirm(body.map(|b| b.confirm).unwrap_or(false)) {
        return unconfirmed;
    }
    if let Some(busy) = require_idle(&st) {
        return busy;
    }
    execute(st, ControlAction::DisableSteppers).await
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
    // Must be an absolute on-printer path — a relative one like `host/x.3mf`
    // would become `ftp://host/x.3mf` and escape the printer's namespace.
    if !is_safe_remote_path(&b.file) {
        return bad_request(format!(
            "file must be an absolute printer path: {:?}",
            b.file
        ));
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
    // Serialize starts so two concurrent requests can't both pass the idle check.
    let Ok(_guard) = st.start_lock.try_lock() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "a print start is already in progress" })),
        )
            .into_response();
    };
    // Idle guard: refuse to start over an active job.
    if let Some(busy) = require_idle(&st) {
        return busy;
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
struct ThumbQuery {
    name: String,
    #[serde(default = "default_plate")]
    plate: u32,
}

/// Serve the embedded plate preview PNG for a `.3mf` (open read). 404 if absent.
async fn file_thumbnail(State(st): State<AppState>, Query(q): Query<ThumbQuery>) -> Response {
    let remote = if q.name.starts_with('/') {
        q.name.clone()
    } else {
        format!("/{}", q.name)
    };
    // Restrict the open thumbnail read to .3mf at a safe absolute path — it
    // downloads the whole file, so don't let it pull arbitrary large files.
    if !is_safe_remote_path(&remote) || !remote.to_ascii_lowercase().ends_with(".3mf") {
        return bad_request(format!("thumbnail needs a .3mf printer path: {:?}", q.name));
    }
    if !(1..=64).contains(&q.plate) {
        return bad_request("plate out of range (1..64)".to_string());
    }
    let files = st.files.clone();
    let plate = q.plate;
    match tokio::task::spawn_blocking(move || files.thumbnail(&remote, plate)).await {
        Ok(Ok(Some(png))) => ([(CONTENT_TYPE, "image/png")], png).into_response(),
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "thumbnail task failed" })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct RawQuery {
    name: String,
}

/// Serve a `.3mf`/`.gcode`'s raw bytes for the 3D viewer (open read). Restricted
/// to those extensions at a safe path; size-capped in [`FileStore::fetch`].
async fn file_raw(State(st): State<AppState>, Query(q): Query<RawQuery>) -> Response {
    let remote = if q.name.starts_with('/') {
        q.name.clone()
    } else {
        format!("/{}", q.name)
    };
    let lower = remote.to_ascii_lowercase();
    if !is_safe_remote_path(&remote) || !(lower.ends_with(".3mf") || lower.ends_with(".gcode")) {
        return bad_request(format!("viewer needs a .3mf/.gcode path: {:?}", q.name));
    }
    let ctype = if lower.ends_with(".gcode") {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    };
    let files = st.files.clone();
    match tokio::task::spawn_blocking(move || files.fetch(&remote)).await {
        Ok(Ok(bytes)) => ([(CONTENT_TYPE, ctype)], bytes).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
        Err(_) => server_error("fetch task failed".to_string()),
    }
}

#[derive(Deserialize)]
struct GcodeFileQuery {
    name: String,
    #[serde(default = "default_plate")]
    plate: u32,
}

/// Serve a sliced `.3mf`'s plate gcode (`Metadata/plate_N.gcode`) as plain text
/// for the 3D viewer's toolpath render (open read). 404 if the plate has none.
///
/// Why a dedicated endpoint instead of `raw`: three's `3MFLoader` doesn't follow
/// Bambu's external-component mesh refs (`3D/Objects/*.model`), so a sliced
/// `.gcode.3mf` renders empty. The embedded gcode toolpath always renders.
async fn file_gcode(State(st): State<AppState>, Query(q): Query<GcodeFileQuery>) -> Response {
    let remote = if q.name.starts_with('/') {
        q.name.clone()
    } else {
        format!("/{}", q.name)
    };
    // Like the thumbnail read: .3mf at a safe path, bounded plate — it downloads
    // the whole file, so don't let it pull arbitrary files.
    if !is_safe_remote_path(&remote) || !remote.to_ascii_lowercase().ends_with(".3mf") {
        return bad_request(format!("gcode needs a .3mf printer path: {:?}", q.name));
    }
    if !(1..=64).contains(&q.plate) {
        return bad_request("plate out of range (1..64)".to_string());
    }
    let files = st.files.clone();
    let plate = q.plate;
    match tokio::task::spawn_blocking(move || files.gcode(&remote, plate)).await {
        Ok(Ok(Some(gcode))) => {
            ([(CONTENT_TYPE, "text/plain; charset=utf-8")], gcode).into_response()
        }
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
        Err(_) => server_error("gcode task failed".to_string()),
    }
}

#[derive(Deserialize)]
struct MeshQuery {
    name: String,
}

/// Serve a `.3mf`'s embedded object meshes as `{ "models": [<3MF model XML>, …] }`
/// for the 3D viewer's solid-mesh render (open read). The viewer parses the mesh
/// XML itself because three's `3MFLoader` won't follow Bambu's external-component
/// refs. Empty `models` when the file embeds no mesh.
async fn file_mesh(State(st): State<AppState>, Query(q): Query<MeshQuery>) -> Response {
    let remote = if q.name.starts_with('/') {
        q.name.clone()
    } else {
        format!("/{}", q.name)
    };
    if !is_safe_remote_path(&remote) || !remote.to_ascii_lowercase().ends_with(".3mf") {
        return bad_request(format!("mesh needs a .3mf printer path: {:?}", q.name));
    }
    let files = st.files.clone();
    match tokio::task::spawn_blocking(move || files.models(&remote)).await {
        Ok(Ok(models)) => Json(json!({ "models": models })).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
        Err(_) => server_error("mesh task failed".to_string()),
    }
}

#[derive(Deserialize)]
struct UploadQuery {
    dir: Option<String>,
    name: String,
}

/// Upload a file to the printer (write). The body is streamed straight to a temp
/// file (not buffered in memory), then handed to the FTPS upload. `?name=` is the
/// filename and `?dir=` the destination (default `/`).
async fn upload_file(
    State(st): State<AppState>,
    Query(q): Query<UploadQuery>,
    body: Body,
) -> Response {
    // Reject path-traversal / nested names — `name` is a single filename.
    if q.name.is_empty() || q.name.contains('/') || q.name.contains('\\') || q.name.contains("..") {
        return bad_request(format!("invalid filename {:?}", q.name));
    }
    let dir = q.dir.unwrap_or_else(|| "/".to_string());
    // Validate the destination dir too (root is allowed; otherwise a safe path).
    if dir != "/" && !is_safe_remote_path(&dir) {
        return bad_request(format!("invalid dir {dir:?}"));
    }
    let remote = format!("{}/{}", dir.trim_end_matches('/'), q.name);

    // Stream the request body to a temp file.
    let tmp = match tempfile::Builder::new().prefix("bambu-upload-").tempfile() {
        Ok(t) => t,
        Err(e) => return server_error(e.to_string()),
    };
    {
        let mut file = match tokio::fs::File::create(tmp.path()).await {
            Ok(f) => f,
            Err(e) => return server_error(e.to_string()),
        };
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(_) => return bad_request("upload stream error".to_string()),
            };
            if file.write_all(&chunk).await.is_err() {
                return server_error("writing upload".to_string());
            }
        }
        if file.flush().await.is_err() {
            return server_error("flushing upload".to_string());
        }
    }

    let name = q.name.clone();
    let path = tmp.path().to_path_buf();
    let files = st.files.clone();
    let res = tokio::task::spawn_blocking(move || files.upload(&remote, &path)).await;
    drop(tmp); // remove the staged file after the upload completes
    match res {
        Ok(Ok(())) => Json(json!({ "uploaded": name })).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
        Err(_) => server_error("upload task failed".to_string()),
    }
}

fn server_error(msg: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": msg })),
    )
        .into_response()
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
    // Accept any-case `Bearer <pw>`; compare in constant time.
    let given = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, tok)| tok.trim());
    if given.is_some_and(|tok| constant_time_eq(tok.as_bytes(), pw.as_bytes())) {
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

/// Length-independent byte equality, to avoid leaking the password via timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
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
        let files = body["files"].as_array().unwrap();
        assert!(
            files
                .iter()
                .any(|f| f["name"] == "coin2c.gcode.3mf" && f["is_dir"] == false)
        );
        assert!(
            files
                .iter()
                .any(|f| f["name"] == "cache" && f["is_dir"] == true)
        );
    }

    #[tokio::test]
    async fn thumbnail_returns_png() {
        let res = app(None, FakeController::verified())
            .get("/api/files/thumbnail?name=coin2c.gcode.3mf")
            .await;
        res.assert_status_ok();
        assert_eq!(res.header("content-type"), "image/png");
    }

    #[tokio::test]
    async fn raw_serves_3mf_bytes() {
        let res = app(None, FakeController::verified())
            .get("/api/files/raw?name=/cache/coin.gcode.3mf")
            .await;
        res.assert_status_ok();
        assert_eq!(res.header("content-type"), "application/octet-stream");
    }

    #[tokio::test]
    async fn raw_rejects_other_extensions() {
        app(None, FakeController::verified())
            .get("/api/files/raw?name=/secret.txt")
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn gcode_file_serves_plate_toolpath() {
        let res = app(None, FakeController::verified())
            .get("/api/files/gcode?name=/coin2c.gcode.3mf&plate=1")
            .await;
        res.assert_status_ok();
        assert!(
            res.header("content-type")
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
        assert!(res.text().contains("G1"));
    }

    #[tokio::test]
    async fn gcode_file_rejects_non_3mf() {
        app(None, FakeController::verified())
            .get("/api/files/gcode?name=/raw.gcode")
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mesh_file_serves_object_models() {
        let res = app(None, FakeController::verified())
            .get("/api/files/mesh?name=/coin2c.gcode.3mf")
            .await;
        res.assert_status_ok();
        let body: serde_json::Value = res.json();
        let models = body["models"].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert!(models[0].as_str().unwrap().contains("<triangle "));
    }

    #[tokio::test]
    async fn mesh_file_rejects_non_3mf() {
        app(None, FakeController::verified())
            .get("/api/files/mesh?name=/raw.gcode")
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn thumbnail_rejects_non_3mf() {
        app(None, FakeController::verified())
            .get("/api/files/thumbnail?name=/timelapse/video.mp4")
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn start_rejects_relative_path() {
        // A non-absolute path would become ftp://host/x and escape the printer.
        app(None, FakeController::verified())
            .post("/api/job/start")
            .json(&json!({ "file": "host/evil.3mf", "confirm": true }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn upload_rejects_traversal_dir() {
        app(None, FakeController::verified())
            .post("/api/files/upload?dir=../etc&name=a.3mf")
            .bytes(b"data".to_vec().into())
            .await
            .assert_status(StatusCode::BAD_REQUEST);
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
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        TestServer::new(router(state))
            .post("/api/job/start")
            .json(&json!({ "file": "/c.3mf", "confirm": true }))
            .await
            .assert_status(StatusCode::CONFLICT);
    }

    // ── machine control: helpers ──

    /// A test server whose source is RUNNING (busy), to exercise the idle guard.
    fn busy_app(controller: impl Controller + 'static) -> TestServer {
        let state = AppState {
            source: Arc::new(FakeSource::ramping(Duration::from_millis(50))),
            controller: Arc::new(controller),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: None,
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        TestServer::new(router(state))
    }

    /// An idle source reporting a hot nozzle, so the cold-extrude guard passes.
    struct HotSource(watch::Sender<PrinterStatus>);
    impl HotSource {
        fn new() -> Self {
            let (tx, _rx) = watch::channel(PrinterStatus {
                gcode_state: Some("IDLE".to_string()),
                print_error: Some(0),
                nozzle_temper: Some(220.0),
                ..Default::default()
            });
            Self(tx)
        }
    }
    impl PrinterSource for HotSource {
        fn current(&self) -> PrinterStatus {
            self.0.borrow().clone()
        }
        fn subscribe(&self) -> watch::Receiver<PrinterStatus> {
            self.0.subscribe()
        }
    }

    /// A test server with an idle, hot-nozzle source (for extrude success).
    fn hot_app(controller: impl Controller + 'static) -> TestServer {
        let state = AppState {
            source: Arc::new(HotSource::new()),
            controller: Arc::new(controller),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: None,
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        TestServer::new(router(state))
    }

    // ── machine control: home ──
    #[tokio::test]
    async fn home_all_on_idle_runs() {
        app(None, FakeController::verified())
            .post("/api/home")
            .json(&json!({}))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn home_does_not_require_confirm() {
        app(None, FakeController::verified())
            .post("/api/home")
            .json(&json!({ "axes": "z" }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn home_on_busy_printer_is_409() {
        busy_app(FakeController::verified())
            .post("/api/home")
            .json(&json!({ "axes": "all" }))
            .await
            .assert_status(StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn home_unknown_axes_is_400() {
        app(None, FakeController::verified())
            .post("/api/home")
            .json(&json!({ "axes": "w" }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    // ── machine control: move (jog) ──
    #[tokio::test]
    async fn move_in_range_on_idle_runs_without_confirm() {
        app(None, FakeController::verified())
            .post("/api/move")
            .json(&json!({ "axis": "x", "delta": 10.0 }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn move_over_bound_is_400() {
        app(None, FakeController::verified())
            .post("/api/move")
            .json(&json!({ "axis": "x", "delta": 999.0 }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn move_zero_delta_is_400() {
        app(None, FakeController::verified())
            .post("/api/move")
            .json(&json!({ "axis": "y", "delta": 0.0 }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn move_out_of_range_feedrate_is_400() {
        app(None, FakeController::verified())
            .post("/api/move")
            .json(&json!({ "axis": "x", "delta": 5.0, "feedrate": 1 }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn move_on_busy_printer_is_409() {
        busy_app(FakeController::verified())
            .post("/api/move")
            .json(&json!({ "axis": "x", "delta": 5.0 }))
            .await
            .assert_status(StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn move_unknown_axis_is_400() {
        app(None, FakeController::verified())
            .post("/api/move")
            .json(&json!({ "axis": "w", "delta": 5.0 }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    // ── machine control: extrude ──
    #[tokio::test]
    async fn extrude_on_cold_nozzle_is_400() {
        // idle source has nozzle_temper = None (cold) → refused.
        app(None, FakeController::verified())
            .post("/api/extrude")
            .json(&json!({ "delta": 5.0 }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn extrude_cold_guard_has_no_force_bypass() {
        // The cold guard takes no force field, but even an over-limit-style
        // attempt can't bypass it: a cold nozzle stays a 400.
        app(None, FakeController::verified())
            .post("/api/extrude")
            .json(&json!({ "delta": 5.0, "force": true }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn extrude_on_hot_idle_nozzle_runs() {
        hot_app(FakeController::verified())
            .post("/api/extrude")
            .json(&json!({ "delta": 5.0 }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn extrude_over_bound_is_400() {
        hot_app(FakeController::verified())
            .post("/api/extrude")
            .json(&json!({ "delta": 999.0 }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn extrude_zero_delta_is_400() {
        hot_app(FakeController::verified())
            .post("/api/extrude")
            .json(&json!({ "delta": 0.0 }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    // ── machine control: temp ──
    #[tokio::test]
    async fn temp_setpoint_needs_confirm() {
        app(None, FakeController::verified())
            .post("/api/temp")
            .json(&json!({ "part": "nozzle", "celsius": 210 }))
            .await
            .assert_status(StatusCode::PRECONDITION_REQUIRED);
    }

    #[tokio::test]
    async fn temp_setpoint_confirmed_runs() {
        app(None, FakeController::verified())
            .post("/api/temp")
            .json(&json!({ "part": "nozzle", "celsius": 210, "confirm": true }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn temp_cooldown_is_allowed_without_confirm() {
        // celsius:0 is the abort valve — no confirm, and allowed even while busy.
        busy_app(FakeController::verified())
            .post("/api/temp")
            .json(&json!({ "part": "nozzle", "celsius": 0 }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn temp_over_limit_is_400_unless_forced() {
        let s = app(None, FakeController::verified());
        s.post("/api/temp")
            .json(&json!({ "part": "nozzle", "celsius": 999, "confirm": true }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        // force overrides the ceiling, exactly like /api/gcode.
        s.post("/api/temp")
            .json(&json!({ "part": "nozzle", "celsius": 999, "confirm": true, "force": true }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn temp_unknown_part_is_400() {
        app(None, FakeController::verified())
            .post("/api/temp")
            .json(&json!({ "part": "chamber", "celsius": 50 }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn temp_is_not_idle_gated_for_a_setpoint() {
        // A non-zero setpoint with confirm runs even on a busy printer.
        busy_app(FakeController::verified())
            .post("/api/temp")
            .json(&json!({ "part": "bed", "celsius": 60, "confirm": true }))
            .await
            .assert_status_ok();
    }

    // ── machine control: calibrate ──
    #[tokio::test]
    async fn calibrate_needs_confirm() {
        app(None, FakeController::verified())
            .post("/api/calibrate")
            .json(&json!({ "bed_level": true }))
            .await
            .assert_status(StatusCode::PRECONDITION_REQUIRED);
    }

    #[tokio::test]
    async fn calibrate_with_no_flags_is_400() {
        app(None, FakeController::verified())
            .post("/api/calibrate")
            .json(&json!({ "confirm": true }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn calibrate_confirmed_on_idle_runs() {
        app(None, FakeController::verified())
            .post("/api/calibrate")
            .json(&json!({ "bed_level": true, "confirm": true }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn calibrate_on_busy_printer_is_409() {
        busy_app(FakeController::verified())
            .post("/api/calibrate")
            .json(&json!({ "vibration": true, "confirm": true }))
            .await
            .assert_status(StatusCode::CONFLICT);
    }

    // ── machine control: ams ──
    #[tokio::test]
    async fn ams_reset_needs_confirm() {
        app(None, FakeController::verified())
            .post("/api/ams")
            .json(&json!({ "action": "reset" }))
            .await
            .assert_status(StatusCode::PRECONDITION_REQUIRED);
    }

    #[tokio::test]
    async fn ams_reset_confirmed_on_idle_runs() {
        app(None, FakeController::verified())
            .post("/api/ams")
            .json(&json!({ "action": "reset", "confirm": true }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn ams_reset_on_busy_printer_is_409() {
        busy_app(FakeController::verified())
            .post("/api/ams")
            .json(&json!({ "action": "reset", "confirm": true }))
            .await
            .assert_status(StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn ams_resume_is_allowed_without_confirm_even_when_busy() {
        // resume clears a pause — no confirm, no idle gate.
        busy_app(FakeController::verified())
            .post("/api/ams")
            .json(&json!({ "action": "resume" }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn ams_unknown_action_is_400() {
        app(None, FakeController::verified())
            .post("/api/ams")
            .json(&json!({ "action": "eject" }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    // ── machine control: reboot ──
    #[tokio::test]
    async fn reboot_needs_confirm() {
        app(None, FakeController::verified())
            .post("/api/reboot")
            .await
            .assert_status(StatusCode::PRECONDITION_REQUIRED);
    }

    #[tokio::test]
    async fn reboot_confirmed_on_idle_is_202() {
        // Reboot is fire-and-forget: a success is Unverified → 202.
        let c = FakeController::returning(CommandOutcome::Unverified {
            stage: VerifyStage::Ack,
        });
        app(None, c)
            .post("/api/reboot")
            .json(&json!({ "confirm": true }))
            .await
            .assert_status(StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn reboot_on_busy_printer_is_409() {
        busy_app(FakeController::verified())
            .post("/api/reboot")
            .json(&json!({ "confirm": true }))
            .await
            .assert_status(StatusCode::CONFLICT);
    }

    // ── machine control: steppers ──
    #[tokio::test]
    async fn steppers_needs_confirm() {
        app(None, FakeController::verified())
            .post("/api/steppers")
            .await
            .assert_status(StatusCode::PRECONDITION_REQUIRED);
    }

    #[tokio::test]
    async fn steppers_confirmed_on_idle_runs() {
        app(None, FakeController::verified())
            .post("/api/steppers")
            .json(&json!({ "confirm": true }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn steppers_on_busy_printer_is_409() {
        busy_app(FakeController::verified())
            .post("/api/steppers")
            .json(&json!({ "confirm": true }))
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
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
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
