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

use std::io::Read;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{
    StatusCode,
    header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE},
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
use super::camera::NoCamera;
use super::camera::{CameraSource, ExternalCamera, open_mjpeg_stream, url_stream_opener};
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
use super::timelapse::{
    DEFAULT_SMOOTH_BURST_MS, FrameGrab, PlainCapture, TimelapseManager, real_park_spawn,
};
use crate::core::command::{AmsControl, LedNode, SpeedLevel};
use crate::core::park::ParkTuning;
use crate::core::safety::{GcodeVerdict, TempLimits, check_extrude, check_gcode, check_jog};
use crate::core::session::CommandOutcome;
use crate::core::status::{Ams, AmsTray, AmsUnit, Filament, LightReport, Online, PrinterStatus};
use crate::park::ParkCapture;

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
    /// **External** IP cameras the server proxies (single-JPEG-per-GET). Held
    /// behind a lock so the dashboard can add/remove them at runtime
    /// (`/api/cameras/config`); seeded from `--camera-url` and in-memory only.
    /// Proxied server-side so a browser that can't reach the LAN cam (e.g. over
    /// Tailscale) still gets a live view.
    pub external_cameras: Arc<RwLock<Vec<ExternalCamera>>>,
    /// The **built-in** (printer chamber) camera, grabbed over TCP:6000 in live
    /// mode; [`NoCamera`](super::camera::NoCamera) in fake / no-target mode.
    pub internal_camera: Arc<dyn CameraSource>,
    /// Serve-internal per-layer timelapse capture, driven off `source`'s status
    /// feed and controlled at runtime by camera id. At most one runs at a time.
    pub timelapse: Arc<TimelapseManager>,
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
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
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
        .route("/api/files/mesh", get(file_mesh))
        .route("/api/cameras", get(cameras_list))
        .route("/api/cameras/{id}/snapshot", get(camera_snapshot))
        .route("/api/cameras/{id}/stream", get(camera_stream))
        .route("/api/cameras/{id}/park", get(camera_park))
        .route("/api/timelapse", get(timelapse_status));
    let writes = Router::new()
        .route("/api/job/pause", post(job_pause))
        .route("/api/job/resume", post(job_resume))
        .route("/api/job/stop", post(job_stop))
        .route("/api/job/clear-error", post(job_clear_error))
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
        .route("/api/ams/change", post(ams_change))
        .route("/api/reboot", post(reboot))
        .route("/api/steppers", post(steppers))
        .route(
            "/api/cameras/config",
            get(cameras_config_get).post(cameras_config_set),
        )
        .route("/api/timelapse/start", post(timelapse_start))
        .route("/api/timelapse/stop", post(timelapse_stop))
        // Uploads stream to a temp file, so the cap bounds disk, not memory.
        .route(
            "/api/files/upload",
            post(upload_file).layer(DefaultBodyLimit::max(512 * 1024 * 1024)),
        )
        .route(
            "/api/job/upload-start",
            post(job_upload_start).layer(DefaultBodyLimit::max(512 * 1024 * 1024)),
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
/// Dismiss a print error (`clean_print_error`) — narrow: it only acknowledges
/// the error popup (the way Studio clears one), it does not stop/resume the job.
/// Gated by confirm like the other job controls.
async fn job_clear_error(State(st): State<AppState>, body: Option<Json<ConfirmBody>>) -> Response {
    run_confirmed(st, ControlAction::ClearError, body).await
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

#[derive(Deserialize)]
struct AmsChangeBody {
    /// Tray to load (0..3), `254` (external spool), or `255` (unload).
    target: u32,
    /// Target nozzle temp for the new filament.
    tar_temp: i64,
    /// Temp to soften the *current* filament for retraction; defaults to
    /// `tar_temp` when omitted.
    curr_temp: Option<i64>,
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    dry_run: bool,
}

/// Change/unload filament via the AMS (`ams_change_filament`). This physically
/// moves filament, so it mirrors the CLI's `ams change`: nozzle temps are
/// clamped to the safe ceiling (no force bypass — an AMS change should never
/// command an unsafe temp), `dry_run` previews the resolved command without
/// sending, and a real send needs confirm (428) + idle (409).
async fn ams_change(State(st): State<AppState>, Json(b): Json<AmsChangeBody>) -> Response {
    // Only meaningful targets: AMS trays, the external spool, or unload.
    if !matches!(b.target, 0..=3 | 254 | 255) {
        return bad_request(format!(
            "target {} invalid (trays 0..3, 254 external spool, or 255 unload)",
            b.target
        ));
    }
    let curr = b.curr_temp.unwrap_or(b.tar_temp);
    let max = TempLimits::default().max_nozzle as i64;
    for (label, t) in [("tar_temp", b.tar_temp), ("curr_temp", curr)] {
        if !(0..=max).contains(&t) {
            return bad_request(format!("{label} {t}°C is out of range (0..={max})"));
        }
    }
    // dry_run previews the resolved command without sending — no confirm/idle
    // gate, so it works even on a busy printer.
    if b.dry_run {
        return Json(json!({ "plan": {
            "command": "ams_change_filament",
            "target": b.target,
            "curr_temp": curr,
            "tar_temp": b.tar_temp,
        }}))
        .into_response();
    }
    if let Some(unconfirmed) = need_confirm(b.confirm) {
        return unconfirmed;
    }
    if let Some(busy) = require_idle(&st) {
        return busy;
    }
    execute(
        st,
        ControlAction::AmsChange {
            target: b.target,
            curr_temp: curr,
            tar_temp: b.tar_temp,
        },
    )
    .await
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
    /// Arm the printer-side timelapse (needed for Smooth-mode's per-layer park +
    /// spiral Z-hop to actually run, not just to record the built-in camera).
    #[serde(default)]
    timelapse: bool,
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
        timelapse: b.timelapse,
        // The file is already on the printer here; we don't have its bytes to
        // inspect, so no md5 check (the upload-start path supplies one).
        inspection: None,
    };

    if b.dry_run {
        return Json(json!({ "plan": {
            "file": req.file,
            "plate": req.plate,
            "use_ams": req.use_ams,
            "ams_map": req.ams_map,
            "bed_type": req.bed_type,
            "timelapse": req.timelapse,
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

// ── Cameras ──────────────────────────────────────────────────────────────────
// The dashboard shows cameras as switchable tabs. Two kinds of source, listed
// together by /api/cameras: the **built-in** printer chamber camera (TCP:6000,
// often dead on the A1) and any number of **external** IP cameras the server
// proxies (e.g. ATOM Cams over LAN). Externals can be set at launch (--camera-url,
// repeatable) and edited at runtime via the gated config endpoint. IDs are
// positional: "internal" for the built-in, "ext-{i}" for the i-th external.

/// Cap a proxied camera frame to bound server memory (a single JPEG is well under
/// this; a misbehaving upstream can't OOM us).
const CAMERA_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Upstream fetch timeout — a stalled camera shouldn't hang the request.
const CAMERA_TIMEOUT: Duration = Duration::from_secs(8);

/// List the available cameras (open read) as `{id, kind, label}`. URLs are never
/// exposed here — only the proxied snapshot is reachable, by id.
async fn cameras_list(State(st): State<AppState>) -> Json<serde_json::Value> {
    let mut cameras = Vec::new();
    if st.internal_camera.configured() {
        cameras.push(json!({ "id": "internal", "kind": "internal", "label": "built-in camera" }));
    }
    for (i, c) in st.external_cameras.read().unwrap().iter().enumerate() {
        cameras.push(json!({
            "id": format!("ext-{i}"),
            "kind": "external",
            "label": c.label,
            // Whether a live MJPEG stream is proxiable for this camera (so the
            // frontend uses `/stream` instead of snapshot polling).
            "stream": c.stream_url.is_some(),
            // Whether this camera can run the live park preview: it needs both a
            // stream and a calibrated park_tuning (the dashboard shows a tile only then).
            "park": c.stream_url.is_some() && c.park_tuning.is_some(),
        }));
    }
    Json(json!({ "cameras": cameras }))
}

/// Proxy a single JPEG for one camera by id (open read). `internal` grabs the
/// built-in cam over TCP:6000; `ext-{i}` proxies that external camera's URL. 404
/// for an unknown id / unconfigured source; 502 when the grab fails.
async fn camera_snapshot(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if id == "internal" {
        if !st.internal_camera.configured() {
            return StatusCode::NOT_FOUND.into_response();
        }
        let cam = st.internal_camera.clone();
        return match tokio::task::spawn_blocking(move || cam.snapshot()).await {
            Ok(Ok(bytes)) => (
                [
                    (CONTENT_TYPE, "image/jpeg".to_string()),
                    (CACHE_CONTROL, "no-store".to_string()),
                ],
                bytes,
            )
                .into_response(),
            Ok(Err(e)) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
            Err(_) => server_error("camera task failed".to_string()),
        };
    }
    let url = id
        .strip_prefix("ext-")
        .and_then(|n| n.parse::<usize>().ok())
        .and_then(|i| {
            st.external_cameras
                .read()
                .unwrap()
                .get(i)
                .map(|c| c.url.clone())
        });
    let Some(url) = url else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::task::spawn_blocking(move || fetch_camera_frame(&url)).await {
        Ok(Ok((ctype, bytes))) => (
            [
                (CONTENT_TYPE, ctype),
                (CACHE_CONTROL, "no-store".to_string()),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
        Err(_) => server_error("camera task failed".to_string()),
    }
}

/// Resolve the live-stream URL for a camera id. Only `ext-{i}` cameras that have
/// a configured `stream_url` stream; `internal` and unknown ids yield `None` (the
/// built-in TCP:6000 cam has no MJPEG stream). Pure, so the routing is testable.
fn resolve_stream_url(id: &str, externals: &[ExternalCamera]) -> Option<String> {
    id.strip_prefix("ext-")
        .and_then(|n| n.parse::<usize>().ok())
        .and_then(|i| externals.get(i))
        .and_then(|c| c.stream_url.clone())
}

/// Reverse-proxy a camera's live MJPEG stream (open read). `ext-{i}` with a
/// configured stream URL only; otherwise 404. The endless upstream multipart body
/// is relayed chunk-by-chunk through a bounded channel, so a fast camera can't
/// outrun a slow client into unbounded memory (the reader blocks when the channel
/// is full; a dropped receiver — client gone — ends it). 502 if the connect fails.
async fn camera_stream(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(url) = resolve_stream_url(&id, &st.external_cameras.read().unwrap()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Connect first (blocking) to learn the upstream content-type — we need the
    // multipart boundary before we can set our own response headers.
    let opened = tokio::task::spawn_blocking(move || open_mjpeg_stream(&url)).await;
    let (ctype, reader) = match opened {
        Ok(Ok(s)) => (s.content_type, s.reader),
        Ok(Err(e)) => {
            return (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response();
        }
        Err(_) => return server_error("camera stream task failed".to_string()),
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    tokio::task::spawn_blocking(move || {
        let mut reader = reader;
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // blocking_send applies backpressure and fails once the client
                    // (receiver) is gone — either way we then stop reading upstream.
                    if tx
                        .blocking_send(Ok(Bytes::copy_from_slice(&buf[..n])))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    break;
                }
            }
        }
    });
    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    Response::builder()
        .header(CONTENT_TYPE, ctype)
        .header(CACHE_CONTROL, "no-store")
        .body(body)
        .unwrap()
}

// The MJPEG stream opener now lives in `super::camera` (open_mjpeg_stream), shared
// with the plain-timelapse stream recorder.

/// Serve the latest live-park preview JPEG for one camera (open read): the running
/// `park` timelapse run writes `<out>/<id>/latest_park.jpg` each layer. 404 before the
/// first park, when no park run is active, or for an id not in the run. The id is matched
/// against the run's own camera list (not joined blindly), so a crafted id can't traverse.
async fn camera_park(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let park = st.timelapse.status_park();
    // Only while a run is ACTIVE — a stopped/finished run keeps out_dir + cameras, so
    // without this it would keep serving the last frame as if still live.
    if !park.running {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(dir) = park.out_dir else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !park.cameras.iter().any(|c| c == &id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = std::path::Path::new(&dir).join(&id).join("latest_park.jpg");
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (CONTENT_TYPE, "image/jpeg".to_string()),
                (CACHE_CONTROL, "no-store".to_string()),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(), // no park captured yet
    }
}

/// Serialise the external list (with URLs) for the gated config endpoints.
fn external_json(st: &AppState) -> Vec<serde_json::Value> {
    st.external_cameras
        .read()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, c)| {
            json!({ "id": format!("ext-{i}"), "label": c.label, "url": c.url,
                    "stream_url": c.stream_url, "park_tuning": c.park_tuning })
        })
        .collect()
}

/// Current external-camera config (gated read) — includes URLs so the dashboard's
/// manage form can prefill. The built-in camera isn't configurable, so it's not
/// listed here.
async fn cameras_config_get(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "external": external_json(&st) }))
}

#[derive(Deserialize)]
struct ExternalCameraInput {
    label: Option<String>,
    url: String,
    /// Optional live MJPEG stream URL (reverse-proxied at `/stream`).
    #[serde(default)]
    stream_url: Option<String>,
    /// Optional per-camera live-park tuning. Deserialized as [`ParkTuning`], which has NO
    /// defaults, so a partial/garbled object is rejected (422) rather than running wrong.
    #[serde(default)]
    park_tuning: Option<ParkTuning>,
}

/// Both proxied URLs (snapshot + stream) must be `http://` — the proxy's `ureq`
/// is built without TLS (LAN IP cameras are plain HTTP), so `https://` would only
/// 502 at fetch time; rejecting it here also blocks `file:`/`gopher:` SSRF.
fn is_http_url(u: &str) -> bool {
    u.starts_with("http://")
}

#[derive(Deserialize)]
struct CamerasConfigBody {
    external: Vec<ExternalCameraInput>,
}

/// Replace the external-camera list (write). Each URL must be `http(s)` (the proxy
/// only speaks HTTP, and refusing other schemes blocks `file:`/`gopher:` SSRF). The
/// list is in-memory only — it resets on restart; `--camera-url` is the persistent
/// path. The built-in camera is untouched.
async fn cameras_config_set(
    State(st): State<AppState>,
    Json(b): Json<CamerasConfigBody>,
) -> Response {
    let mut next = Vec::with_capacity(b.external.len());
    for (i, e) in b.external.into_iter().enumerate() {
        let url = e.url.trim().to_string();
        if !is_http_url(&url) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "camera URL must start with http:// (the proxy is plain-HTTP; no TLS)" })),
            )
                .into_response();
        }
        // A stream URL, if given, is proxied too — apply the same scheme guard.
        let stream_url = e
            .stream_url
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(s) = &stream_url
            && !is_http_url(s)
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "camera stream URL must start with http:// (the proxy is plain-HTTP; no TLS)" })),
            )
                .into_response();
        }
        next.push(ExternalCamera::new(e.label, url, stream_url, i).with_park_tuning(e.park_tuning));
    }
    *st.external_cameras.write().unwrap() = next;
    Json(json!({ "external": external_json(&st) })).into_response()
}

/// Blocking single-shot GET of the camera URL. Returns `(content_type, bytes)` or
/// an error string. The body is read with a hard byte cap so a bad upstream can't
/// exhaust memory.
fn fetch_camera_frame(url: &str) -> Result<(String, Vec<u8>), String> {
    // A snapshot CGI never legitimately redirects; disallowing redirects keeps
    // the server-side fetch from being bounced to an internal address (SSRF).
    let agent = ureq::AgentBuilder::new()
        .timeout(CAMERA_TIMEOUT)
        .redirects(0)
        .build();
    let resp = agent.get(url).call().map_err(|e| e.to_string())?;
    // Default to image/jpeg if the camera omits a content-type.
    let ctype = resp
        .header("content-type")
        .map(str::to_string)
        .unwrap_or_else(|| "image/jpeg".to_string());
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(CAMERA_MAX_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Err("camera returned an empty body".to_string());
    }
    Ok((ctype, bytes))
}

#[derive(Deserialize)]
struct TimelapseStartBody {
    /// Single camera id to capture from: `internal` or `ext-{i}`. Convenience
    /// for the common case; `cameras` takes precedence when both are given.
    #[serde(default)]
    camera: Option<String>,
    /// Capture several cameras at once (multi-angle) — each gets a frame per
    /// trigger under its own subdir. Falls back to `camera` when empty.
    #[serde(default)]
    cameras: Vec<String>,
    /// `"smooth"` (default): one frame per layer, synced to the printer's park.
    /// `"plain"`: one frame every `interval_ms`, head in shot. They're separate
    /// runs, so both can be on at once for the same print.
    #[serde(default)]
    mode: Option<String>,
    /// Smooth: capture every Nth layer.
    #[serde(default = "default_every")]
    every: u64,
    /// Plain: sampling period in ms (default 3000).
    #[serde(default)]
    interval_ms: Option<u64>,
    /// Smooth: per-layer park-capture burst, ms after the layer edge (default
    /// [`DEFAULT_SMOOTH_BURST_MS`]). The native park lands ~0.4–1.2 s after the
    /// `layer_num` increment, so a burst brackets the window; each frame is tagged
    /// with its offset. Exposed so the offsets can be calibrated without a rebuild.
    #[serde(default)]
    burst_offsets_ms: Option<Vec<u64>>,
}
fn default_every() -> u64 {
    1
}

#[derive(Deserialize, Default)]
struct TimelapseStopBody {
    /// Which run to stop: `"smooth"`, `"plain"`, or `"all"` (default).
    #[serde(default)]
    mode: Option<String>,
}

/// Combined status for both runs: a back-compat flat view mirroring the smooth
/// run (so older single-run readers keep working), plus nested `smooth`/`plain`.
/// Top-level `running` is true if *either* run is active.
fn timelapse_status_json(st: &AppState) -> serde_json::Value {
    let smooth = st.timelapse.status_smooth();
    let plain = st.timelapse.status_plain();
    let park = st.timelapse.status_park();
    let mut out = smooth.to_json();
    if let Some(o) = out.as_object_mut() {
        o.insert(
            "running".to_string(),
            json!(smooth.running || plain.running || park.running),
        );
        o.insert("smooth".to_string(), smooth.to_json());
        o.insert("plain".to_string(), plain.to_json());
        o.insert("park".to_string(), park.to_json());
    }
    out
}

/// Resolve a camera id to a blocking frame-grabber + a stable label, captured at
/// start so a later `/api/cameras/config` edit can't repoint a running capture.
fn resolve_grab(st: &AppState, camera: &str) -> Option<(String, FrameGrab)> {
    if camera == "internal" {
        if !st.internal_camera.configured() {
            return None;
        }
        let cam = st.internal_camera.clone();
        return Some((camera.to_string(), Arc::new(move || cam.snapshot())));
    }
    let idx = camera.strip_prefix("ext-")?.parse::<usize>().ok()?;
    let url = st.external_cameras.read().unwrap().get(idx)?.url.clone();
    Some((
        camera.to_string(),
        Arc::new(move || fetch_camera_frame(&url).map(|(_, bytes)| bytes)),
    ))
}

/// Sanitise a print name into a filesystem-safe run-dir suffix.
fn sanitize_hint(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let trimmed = cleaned.trim_matches('_');
    if trimmed.is_empty() {
        "print".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `captures/<epoch>_<print-hint>_<mode>/` — the per-run output dir (per-mode so a
/// concurrent smooth/plain/park run never mixes frames).
fn run_out_dir(st: &AppState, mode: &str) -> std::path::PathBuf {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hint = sanitize_hint(
        st.source
            .current()
            .subtask_name
            .as_deref()
            .unwrap_or("print"),
    );
    std::path::PathBuf::from("captures").join(format!("{epoch}_{hint}_{mode}"))
}

/// Resolve the park-capable cameras among `ids` and start the live park slot. A camera is
/// capable iff it's an external camera with BOTH a stream and a calibrated `park_tuning`;
/// non-capable requested cameras are skipped (reported in `skipped`), and it's a 400 if
/// none qualify. Each emits `<out>/<id>/latest_park.jpg` per layer, served (open) by
/// `/api/cameras/{id}/park`.
fn start_park_run(
    st: &AppState,
    ids: &[String],
    out_dir: std::path::PathBuf,
    rx: watch::Receiver<PrinterStatus>,
) -> Response {
    let externals = st.external_cameras.read().unwrap();
    let mut caps = Vec::new();
    let mut skipped = Vec::new();
    for id in ids {
        let cap = id
            .strip_prefix("ext-")
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|i| externals.get(i))
            .and_then(|c| match (&c.stream_url, &c.park_tuning) {
                (Some(url), Some(t)) => Some(ParkCapture {
                    id: id.clone(),
                    stream_url: url.clone(),
                    tuning: t.clone(),
                }),
                _ => None,
            });
        match cap {
            Some(c) => caps.push(c),
            None => skipped.push(id.clone()),
        }
    }
    drop(externals);
    if caps.is_empty() {
        return bad_request(format!(
            "no park-capable cameras among {ids:?} — each needs a stream_url and a park_tuning"
        ));
    }
    match st
        .timelapse
        .start_park(caps, rx, out_dir, real_park_spawn())
    {
        Ok(()) => {
            let mut body = timelapse_status_json(st);
            if let Some(o) = body.as_object_mut() {
                o.insert("skipped".to_string(), json!(skipped));
            }
            Json(body).into_response()
        }
        Err(e) => (StatusCode::CONFLICT, Json(json!({ "error": e }))).into_response(),
    }
}

/// Start a per-layer timelapse capture from a configured camera (gated write).
/// 409 if one is already running; 404 for an unknown/unconfigured camera. Frames
/// land in `./captures/<epoch>_<print-hint>/`.
async fn timelapse_start(
    State(st): State<AppState>,
    Json(b): Json<TimelapseStartBody>,
) -> Response {
    // Validate the mode's cadence up front (before resolving cameras), so a bad
    // `every`/`interval_ms` is a clean 400 regardless of camera config.
    let mode = b.mode.as_deref().unwrap_or("smooth");
    let interval_ms = b.interval_ms.unwrap_or(3000);
    let burst_offsets = b
        .burst_offsets_ms
        .clone()
        .unwrap_or_else(|| DEFAULT_SMOOTH_BURST_MS.to_vec());
    match mode {
        "smooth" => {
            if b.every < 1 {
                return bad_request("every must be >= 1".to_string());
            }
            if burst_offsets.is_empty() {
                return bad_request("burst_offsets_ms must have at least one offset".to_string());
            }
            if burst_offsets.len() > 16 {
                return bad_request("burst_offsets_ms: at most 16 offsets".to_string());
            }
            if let Some(&o) = burst_offsets.iter().find(|&&o| o > 10_000) {
                return bad_request(format!("burst_offsets_ms: {o} ms exceeds the 10000 ms cap"));
            }
        }
        "plain" => {
            if interval_ms < 100 {
                return bad_request("interval_ms must be >= 100".to_string());
            }
        }
        // Park has no cadence knobs; its requirement (a stream + park_tuning per camera)
        // is enforced when the cameras resolve below.
        "park" => {}
        other => {
            return bad_request(format!(
                "unknown mode {other:?} (use smooth, plain, or park)"
            ));
        }
    }
    // `cameras` wins; fall back to the single `camera`. De-dupe but keep order.
    let mut ids: Vec<String> = if !b.cameras.is_empty() {
        b.cameras.clone()
    } else {
        b.camera.clone().into_iter().collect()
    };
    ids.dedup();
    if ids.is_empty() {
        return bad_request("specify a camera or cameras to capture".to_string());
    }
    // Park reads the camera stream (not snapshot grabs) and needs per-camera tuning, so it
    // resolves cameras differently — branch before the grab resolution the others need.
    if mode == "park" {
        let out_dir = run_out_dir(&st, mode);
        let rx = st.source.subscribe();
        return start_park_run(&st, &ids, out_dir, rx);
    }
    let mut grabs = Vec::with_capacity(ids.len());
    for id in &ids {
        let Some(resolved) = resolve_grab(&st, id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("unknown or unconfigured camera: {id}") })),
            )
                .into_response();
        };
        grabs.push(resolved);
    }
    // Per-mode dir so a concurrent smooth + plain run never mix their frames.
    let out_dir = run_out_dir(&st, mode);
    let rx = st.source.subscribe();
    let res = match mode {
        "plain" => {
            // A camera with a configured stream URL records its real MJPEG stream;
            // a snapshot-only camera keeps time-sampling. Resolved once, at start.
            let externals = st.external_cameras.read().unwrap();
            let caps: Vec<PlainCapture> = ids
                .iter()
                .zip(grabs)
                .map(
                    |(id, (gid, grab))| match resolve_stream_url(id, &externals) {
                        Some(url) => PlainCapture::Stream {
                            id: gid,
                            open: url_stream_opener(url),
                        },
                        None => PlainCapture::Sample { id: gid, grab },
                    },
                )
                .collect();
            drop(externals);
            st.timelapse.start_plain(caps, interval_ms, rx, out_dir)
        }
        _ => st
            .timelapse
            .start_smooth(grabs, b.every, burst_offsets, rx, out_dir),
    };
    match res {
        Ok(()) => Json(timelapse_status_json(&st)).into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(json!({ "error": e }))).into_response(),
    }
}

/// Stop a capture run (gated write; idempotent). `{"mode":"smooth"|"plain"|"park"}`
/// stops just that one; no body / `"all"` stops every slot. An unrecognized mode is a
/// 400 rather than a silent "all" — a typo must not abort a run the caller meant to keep
/// going (the slots are independently controlled).
async fn timelapse_stop(
    State(st): State<AppState>,
    body: Option<Json<TimelapseStopBody>>,
) -> Response {
    let mode = body
        .and_then(|b| b.0.mode)
        .unwrap_or_else(|| "all".to_string());
    match mode.as_str() {
        "smooth" => {
            st.timelapse.stop_smooth();
        }
        "plain" => {
            st.timelapse.stop_plain();
        }
        "park" => {
            st.timelapse.stop_park();
        }
        "all" => {
            st.timelapse.stop_smooth();
            st.timelapse.stop_plain();
            st.timelapse.stop_park();
        }
        other => {
            return bad_request(format!(
                "unknown mode {other:?} (use smooth, plain, park, or all)"
            ));
        }
    }
    Json(timelapse_status_json(&st)).into_response()
}

/// Current capture status (open read).
async fn timelapse_status(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(timelapse_status_json(&st))
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

/// Cap for a streamed upload body (DefaultBodyLimit can't bound a raw `Body`).
const MAX_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Deserialize)]
struct UploadStartQuery {
    name: String,
    dir: Option<String>,
    #[serde(default = "default_plate")]
    plate: u32,
    #[serde(default)]
    timelapse: bool,
    bed_type: Option<String>,
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    overwrite: bool,
}

/// One-shot **upload + start**: stream the body to a temp file, (for a `.3mf`)
/// inspect it for the plate-gcode md5, then FTPS-upload it and start the print —
/// the dashboard's single request instead of `/files/upload` then `/job/start`.
/// Reuses the upload guards (filename traversal, safe dir) and the start guards
/// (confirm, idle, the held `start_lock`); the command is built by the shared
/// `core::start` builder, with the md5 stamped in so the printer verifies the file.
async fn job_upload_start(
    State(st): State<AppState>,
    Query(q): Query<UploadStartQuery>,
    body: Body,
) -> Response {
    // Same filename/dir guards as the plain upload (single filename, safe dir).
    if q.name.is_empty() || q.name.contains('/') || q.name.contains('\\') || q.name.contains("..") {
        return bad_request(format!("invalid filename {:?}", q.name));
    }
    // Default to the printer root: the A1 mini prints from `/`, and a print start
    // that reads an uploaded file from `/cache` fails with 0x0500C010 (verified).
    let dir = q.dir.clone().unwrap_or_else(|| "/".to_string());
    if dir != "/" && !is_safe_remote_path(&dir) {
        return bad_request(format!("invalid dir {dir:?}"));
    }
    let remote = format!("{}/{}", dir.trim_end_matches('/'), q.name);
    let is_3mf = q.name.to_ascii_lowercase().ends_with(".3mf");

    // Reject before reading the (possibly huge) body: an unconfirmed, non-dry-run
    // request can't do anything, so don't stream it to disk first.
    if !q.confirm && !q.dry_run {
        return (
            StatusCode::PRECONDITION_REQUIRED,
            Json(
                json!({ "error": "confirm required: add &confirm=true (try &dry_run=true first)" }),
            ),
        )
            .into_response();
    }

    // Stream the body to a temp file (never buffered in memory). DefaultBodyLimit
    // does NOT bound a raw `Body` we consume ourselves, so count bytes and cap.
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
        let mut written: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(_) => return bad_request("upload stream error".to_string()),
            };
            written += chunk.len() as u64;
            if written > MAX_UPLOAD_BYTES {
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({ "error": "upload exceeds the 512 MiB limit" })),
                )
                    .into_response();
            }
            if file.write_all(&chunk).await.is_err() {
                return server_error("writing upload".to_string());
            }
        }
        if file.flush().await.is_err() {
            return server_error("flushing upload".to_string());
        }
    }

    // For a .3mf, read the plate-gcode md5 from the bytes we just staged.
    let inspection = if is_3mf {
        match std::fs::read(tmp.path())
            .map_err(|e| e.to_string())
            .and_then(|b| {
                crate::core::project::inspect_plate(&b, q.plate).map_err(|e| e.to_string())
            }) {
            Ok(insp) => Some(insp),
            Err(e) => return bad_request(format!("3mf inspection: {e}")),
        }
    } else {
        None
    };
    let bed_type = q.bed_type.clone().unwrap_or_else(|| "auto".to_string());
    let md5 = inspection.as_ref().map(|i| i.gcode_md5.clone());

    if q.dry_run {
        return Json(json!({ "plan": {
            "file": remote,
            "plate": q.plate,
            "use_ams": false,
            "bed_type": bed_type,
            "timelapse": q.timelapse,
            "md5": md5,
            "overwrite": q.overwrite,
        }}))
        .into_response();
    }
    // (confirm is guaranteed here — the early gate rejected !confirm && !dry_run,
    // and dry_run returned above.)

    // Hold the start lock across upload+start so two requests can't both pass idle.
    let Ok(_guard) = st.start_lock.try_lock() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "a print start is already in progress" })),
        )
            .into_response();
    };
    if let Some(busy) = require_idle(&st) {
        return busy;
    }

    // Conservative overwrite guard (list the dir; a listing error doesn't block).
    if !q.overwrite {
        let files = st.files.clone();
        let dir_for_check = dir.clone();
        let name = q.name.clone();
        if let Ok(Ok(entries)) =
            tokio::task::spawn_blocking(move || files.list(&dir_for_check)).await
            && entries.iter().any(|e| e.name == name)
        {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": format!("{remote} already exists (add &overwrite=true to replace it)") })),
            )
                .into_response();
        }
    }

    // Upload the staged file, then start from its on-printer path.
    let files = st.files.clone();
    let path = tmp.path().to_path_buf();
    let remote_for_upload = remote.clone();
    let up = tokio::task::spawn_blocking(move || files.upload(&remote_for_upload, &path)).await;
    drop(tmp); // remove the staged file once uploaded (or on error)
    match up {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response();
        }
        Err(_) => return server_error("upload task failed".to_string()),
    }

    let req = StartRequest {
        file: remote,
        plate: q.plate,
        use_ams: false,
        ams_map: Vec::new(),
        bed_type,
        timelapse: q.timelapse,
        inspection,
    };
    let starter = st.starter.clone();
    let res = tokio::task::spawn_blocking(move || starter.start(&req)).await;
    verify_response(res)
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
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
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

    #[tokio::test]
    async fn job_clear_error_needs_confirmation() {
        app(None, FakeController::verified())
            .post("/api/job/clear-error")
            .await
            .assert_status(StatusCode::PRECONDITION_REQUIRED);
    }

    #[tokio::test]
    async fn job_clear_error_confirmed_returns_verified() {
        let res = app(None, FakeController::verified())
            .post("/api/job/clear-error")
            .json(&json!({ "confirm": true }))
            .await;
        res.assert_status_ok();
        assert_eq!(res.json::<serde_json::Value>()["outcome"], "verified");
    }

    // ── upload-then-start (one-shot) ──
    #[tokio::test]
    async fn upload_start_needs_confirmation() {
        app(None, FakeController::verified())
            .post("/api/job/upload-start?name=x.gcode")
            .bytes(b"G28\n".to_vec().into())
            .await
            .assert_status(StatusCode::PRECONDITION_REQUIRED);
    }

    #[tokio::test]
    async fn upload_start_confirmed_uploads_then_starts() {
        // A raw .gcode skips 3mf inspection, so the fake files+starter carry it
        // end to end: upload succeeds, the print verifies.
        let res = app(None, FakeController::verified())
            .post("/api/job/upload-start?name=x.gcode&confirm=true")
            .bytes(b"G28\n".to_vec().into())
            .await;
        res.assert_status_ok();
        assert_eq!(res.json::<serde_json::Value>()["outcome"], "verified");
    }

    #[tokio::test]
    async fn upload_start_dry_run_plans_without_starting() {
        let res = app(None, FakeController::verified())
            .post("/api/job/upload-start?name=x.gcode&dry_run=true")
            .bytes(b"G28\n".to_vec().into())
            .await;
        res.assert_status_ok();
        let v = res.json::<serde_json::Value>();
        // Default destination is the printer root — the A1 mini prints from `/`,
        // and reading an uploaded file from `/cache` fails with 0x0500C010.
        assert_eq!(v["plan"]["file"], "/x.gcode");
    }

    #[tokio::test]
    async fn upload_start_rejects_a_traversal_name() {
        app(None, FakeController::verified())
            .post("/api/job/upload-start?name=../evil.gcode&confirm=true")
            .bytes(b"x".to_vec().into())
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn upload_start_is_gated_by_password() {
        app(Some("hunter2"), FakeController::verified())
            .post("/api/job/upload-start?name=x.gcode&confirm=true")
            .bytes(b"G28\n".to_vec().into())
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
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

    // ── cameras (built-in + external proxies, listed as switchable sources) ──
    #[tokio::test]
    async fn cameras_list_is_empty_without_built_in_or_external() {
        // Fake/test mode has no built-in camera and no external URLs.
        let res = app(None, FakeController::verified())
            .get("/api/cameras")
            .await;
        res.assert_status_ok();
        assert_eq!(
            res.json::<serde_json::Value>()["cameras"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn camera_snapshot_is_404_for_unknown_id() {
        let server = app(None, FakeController::verified());
        for id in ["internal", "ext-0", "bogus"] {
            server
                .get(&format!("/api/cameras/{id}/snapshot"))
                .await
                .assert_status(StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn external_cameras_can_be_set_then_listed_and_cleared() {
        let server = app(None, FakeController::verified());
        // Configure two external cameras (one labelled, one auto-labelled).
        let res = server
            .post("/api/cameras/config")
            .json(&json!({
                "external": [
                    { "label": "front", "url": "http://cam.local/a.jpg" },
                    { "url": "http://cam.local/b.jpg" }
                ]
            }))
            .await;
        res.assert_status_ok();
        // The open listing now shows both, with ids and labels but no URLs.
        let list = server.get("/api/cameras").await.json::<serde_json::Value>();
        let cams = list["cameras"].as_array().unwrap();
        assert_eq!(cams.len(), 2);
        assert_eq!(cams[0]["id"], "ext-0");
        assert_eq!(cams[0]["label"], "front");
        assert_eq!(cams[0]["kind"], "external");
        assert_eq!(cams[1]["label"], "external 2"); // auto-labelled
        assert!(cams[0].get("url").is_none()); // URL never exposed on the open list
        // The gated config read echoes URLs back for the manage form.
        let cfg = server
            .get("/api/cameras/config")
            .await
            .json::<serde_json::Value>();
        assert_eq!(cfg["external"][0]["url"], "http://cam.local/a.jpg");
        // Replacing with an empty list clears them.
        server
            .post("/api/cameras/config")
            .json(&json!({ "external": [] }))
            .await
            .assert_status_ok();
        let list = server.get("/api/cameras").await.json::<serde_json::Value>();
        assert_eq!(list["cameras"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn camera_config_rejects_non_http_url() {
        let server = app(None, FakeController::verified());
        server
            .post("/api/cameras/config")
            .json(&json!({ "external": [{ "url": "file:///etc/passwd" }] }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        // The proxy's ureq is built without TLS, so an https camera would only
        // fail later with a 502 — reject it up front rather than advertise it.
        server
            .post("/api/cameras/config")
            .json(&json!({ "external": [{ "url": "https://cam.local/a.jpg" }] }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn external_camera_stream_url_round_trips_and_flags_the_list() {
        let server = app(None, FakeController::verified());
        server
            .post("/api/cameras/config")
            .json(&json!({
                "external": [
                    { "label": "front", "url": "http://cam.local/snapshot",
                      "stream_url": "http://cam.local/stream" },
                    { "url": "http://cam.local/b.jpg" }
                ]
            }))
            .await
            .assert_status_ok();
        // The open list flags whether a live MJPEG stream is available, so the
        // frontend can pick stream vs snapshot-poll — still without leaking URLs.
        let list = server.get("/api/cameras").await.json::<serde_json::Value>();
        let cams = list["cameras"].as_array().unwrap();
        assert_eq!(cams[0]["stream"], true);
        assert_eq!(cams[1]["stream"], false);
        assert!(cams[0].get("url").is_none());
        // The gated config read echoes the stream URL for the manage form.
        let cfg = server
            .get("/api/cameras/config")
            .await
            .json::<serde_json::Value>();
        assert_eq!(cfg["external"][0]["stream_url"], "http://cam.local/stream");
        assert!(cfg["external"][1]["stream_url"].is_null());
    }

    #[tokio::test]
    async fn park_tuning_round_trips_and_flags_capability() {
        let server = app(None, FakeController::verified());
        let tuning = json!({ "fps": 4, "left_frac": 0.33, "ema_seconds": 30, "abs_floor": 1500,
            "mad_k": 6, "merge_gap_s": 1.2, "max_island_s": 3, "min_sep_s": 3,
            "candidate_frac": 0.75, "warmup_s": 4, "baseline_s": 90 });
        server
            .post("/api/cameras/config")
            .json(&json!({ "external": [
                { "label": "front", "url": "http://cam.local/snap",
                  "stream_url": "http://cam.local/stream", "park_tuning": tuning },
                // a stream camera WITHOUT tuning — not park-capable
                { "url": "http://cam.local/b.jpg", "stream_url": "http://cam.local/bstream" },
            ]}))
            .await
            .assert_status_ok();
        // park-capability needs BOTH a stream and a tuning.
        let list = server.get("/api/cameras").await.json::<serde_json::Value>();
        let cams = list["cameras"].as_array().unwrap();
        assert_eq!(cams[0]["park"], true);
        assert_eq!(
            cams[1]["park"], false,
            "stream but no tuning → not park-capable"
        );
        // The gated config read echoes the tuning so the manage form can prefill.
        let cfg = server
            .get("/api/cameras/config")
            .await
            .json::<serde_json::Value>();
        assert!(cfg["external"][0]["park_tuning"].is_object());
        assert_eq!(cfg["external"][0]["park_tuning"]["fps"], json!(4.0));
        assert!(cfg["external"][1]["park_tuning"].is_null());
    }

    #[tokio::test]
    async fn camera_config_rejects_a_partial_park_tuning() {
        // No baked defaults: a park_tuning missing a knob (abs_floor) must be rejected,
        // not run with a wrong value.
        let server = app(None, FakeController::verified());
        let res = server
            .post("/api/cameras/config")
            .json(&json!({ "external": [
                { "url": "http://cam.local/a.jpg", "stream_url": "http://cam.local/s",
                  "park_tuning": { "fps": 4, "left_frac": 0.33 } },
            ]}))
            .await;
        assert!(
            !res.status_code().is_success(),
            "partial tuning must be rejected"
        );
    }

    #[tokio::test]
    async fn timelapse_start_park_rejects_without_a_capable_camera() {
        let server = app(None, FakeController::verified());
        server
            .post("/api/cameras/config")
            .json(&json!({ "external": [ { "url": "http://cam.local/a.jpg" } ] }))
            .await
            .assert_status_ok();
        server
            .post("/api/timelapse/start")
            .json(&json!({ "mode": "park", "camera": "ext-0" }))
            .await
            .assert_status_bad_request();
    }

    #[tokio::test]
    async fn camera_park_is_404_without_a_running_park() {
        let server = app(None, FakeController::verified());
        server
            .get("/api/cameras/ext-0/park")
            .await
            .assert_status_not_found();
    }

    #[tokio::test]
    async fn camera_config_rejects_non_http_stream_url() {
        let server = app(None, FakeController::verified());
        server
            .post("/api/cameras/config")
            .json(&json!({
                "external": [
                    { "url": "http://cam.local/a.jpg", "stream_url": "file:///etc/passwd" }
                ]
            }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        // Same TLS-less reason as the snapshot URL: no https streams.
        server
            .post("/api/cameras/config")
            .json(&json!({
                "external": [
                    { "url": "http://cam.local/a.jpg", "stream_url": "https://cam.local/stream" }
                ]
            }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[test]
    fn resolve_stream_url_only_for_ext_with_a_stream() {
        use super::{ExternalCamera, resolve_stream_url};
        let cams = vec![
            ExternalCamera::new(
                Some("a".into()),
                "http://x/snap".into(),
                Some("http://x/stream".into()),
                0,
            ),
            ExternalCamera::new(None, "http://y/snap".into(), None, 1),
        ];
        assert_eq!(
            resolve_stream_url("ext-0", &cams).as_deref(),
            Some("http://x/stream")
        );
        assert_eq!(resolve_stream_url("ext-1", &cams), None); // snapshot-only
        assert_eq!(resolve_stream_url("ext-9", &cams), None); // out of range
        assert_eq!(resolve_stream_url("internal", &cams), None);
        assert_eq!(resolve_stream_url("bogus", &cams), None);
    }

    #[tokio::test]
    async fn camera_stream_relays_the_upstream_multipart_body() {
        use std::io::{Read as _, Write as _};
        // Throwaway upstream: answer one request with a short multipart MJPEG body
        // (including a non-UTF8 JPEG start marker), then close so the relayed
        // stream ends and the test can read it in full.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let upstream = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf); // drain the request line/headers
                let mut body = Vec::new();
                body.extend_from_slice(b"--FRAME\r\nContent-Type: image/jpeg\r\n\r\n");
                body.extend_from_slice(&[0xff, 0xd8, 0xff, b'D', b'A', b'T', b'A']);
                body.extend_from_slice(b"\r\n--FRAME--\r\n");
                let head = "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; \
                            boundary=FRAME\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(head.as_bytes());
                let _ = sock.write_all(&body);
            }
        });
        let server = app(None, FakeController::verified());
        server
            .post("/api/cameras/config")
            .json(&json!({ "external": [
                { "url": format!("http://{addr}/snap"),
                  "stream_url": format!("http://{addr}/stream") }
            ] }))
            .await
            .assert_status_ok();
        let res = server.get("/api/cameras/ext-0/stream").await;
        res.assert_status_ok();
        assert!(
            res.header("content-type")
                .to_str()
                .unwrap()
                .starts_with("multipart/x-mixed-replace")
        );
        // The upstream body is relayed through verbatim (incl. the binary marker).
        let bytes = res.as_bytes();
        assert!(bytes.windows(7).any(|w| w == b"--FRAME"));
        assert!(bytes.windows(4).any(|w| w == b"DATA"));
        upstream.join().unwrap();
    }

    #[tokio::test]
    async fn timelapse_status_is_open_and_initially_idle() {
        let res = app(None, FakeController::verified())
            .get("/api/timelapse")
            .await;
        res.assert_status_ok();
        assert_eq!(res.json::<serde_json::Value>()["running"], false);
    }

    #[tokio::test]
    async fn timelapse_start_rejects_unknown_camera() {
        app(None, FakeController::verified())
            .post("/api/timelapse/start")
            .json(&json!({ "camera": "ext-9" }))
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn timelapse_start_rejects_every_zero() {
        app(None, FakeController::verified())
            .post("/api/timelapse/start")
            .json(&json!({ "camera": "ext-0", "every": 0 }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn timelapse_start_rejects_unknown_mode() {
        app(None, FakeController::verified())
            .post("/api/timelapse/start")
            .json(&json!({ "camera": "ext-0", "mode": "fancy" }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn timelapse_plain_rejects_too_fast_interval() {
        // The cadence is validated before camera resolution, so this is a 400 even
        // though ext-0 isn't configured in the fake app.
        app(None, FakeController::verified())
            .post("/api/timelapse/start")
            .json(&json!({ "camera": "ext-0", "mode": "plain", "interval_ms": 10 }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn timelapse_smooth_rejects_an_out_of_range_burst_offset() {
        // Burst offsets are validated up front (before camera resolution), like the
        // cadence — so an offset past the 10s cap is a 400 even with no camera.
        app(None, FakeController::verified())
            .post("/api/timelapse/start")
            .json(&json!({ "camera": "ext-0", "burst_offsets_ms": [800, 99999] }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn timelapse_smooth_rejects_an_empty_burst() {
        app(None, FakeController::verified())
            .post("/api/timelapse/start")
            .json(&json!({ "camera": "ext-0", "burst_offsets_ms": [] }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn timelapse_stop_rejects_unknown_mode() {
        // A typo like "plian" must NOT silently fall through to stopping both runs
        // — that would abort the other capture the caller meant to keep going.
        app(None, FakeController::verified())
            .post("/api/timelapse/stop")
            .json(&json!({ "mode": "plian" }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn timelapse_stop_without_a_mode_is_ok() {
        // No body (or an explicit "all") stops both and is the documented default.
        app(None, FakeController::verified())
            .post("/api/timelapse/stop")
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn timelapse_start_stop_are_gated_by_password() {
        let server = app(Some("hunter2"), FakeController::verified());
        server
            .post("/api/timelapse/start")
            .json(&json!({ "camera": "ext-0" }))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        server
            .post("/api/timelapse/stop")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        // ...but status stays an open read.
        server.get("/api/timelapse").await.assert_status_ok();
    }

    #[tokio::test]
    async fn camera_config_is_gated_by_password() {
        app(Some("hunter2"), FakeController::verified())
            .post("/api/cameras/config")
            .json(&json!({ "external": [{ "url": "http://cam.local/a.jpg" }] }))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
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
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
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
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
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
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
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

    // ── machine control: ams change/unload ──
    #[tokio::test]
    async fn ams_change_needs_confirm() {
        // Moving filament is physical — an unconfirmed request is a 428.
        app(None, FakeController::verified())
            .post("/api/ams/change")
            .json(&json!({ "target": 255, "tar_temp": 220 }))
            .await
            .assert_status(StatusCode::PRECONDITION_REQUIRED);
    }

    #[tokio::test]
    async fn ams_change_confirmed_on_idle_runs() {
        app(None, FakeController::verified())
            .post("/api/ams/change")
            .json(&json!({ "target": 1, "tar_temp": 220, "confirm": true }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn ams_unload_target_255_confirmed_runs() {
        // 255 is the unload sentinel — the whole reason this endpoint exists.
        app(None, FakeController::verified())
            .post("/api/ams/change")
            .json(&json!({ "target": 255, "tar_temp": 250, "confirm": true }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn ams_change_on_busy_printer_is_409() {
        busy_app(FakeController::verified())
            .post("/api/ams/change")
            .json(&json!({ "target": 0, "tar_temp": 220, "confirm": true }))
            .await
            .assert_status(StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn ams_change_over_limit_temp_is_400() {
        // An AMS change must not command an unsafe nozzle temp (no force here).
        app(None, FakeController::verified())
            .post("/api/ams/change")
            .json(&json!({ "target": 1, "tar_temp": 999, "confirm": true }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ams_change_curr_temp_is_also_clamped() {
        app(None, FakeController::verified())
            .post("/api/ams/change")
            .json(&json!({ "target": 1, "tar_temp": 220, "curr_temp": 999, "confirm": true }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ams_change_unknown_target_is_400() {
        // Trays 0..3, 254 (external spool), 255 (unload) are meaningful; 7 isn't.
        app(None, FakeController::verified())
            .post("/api/ams/change")
            .json(&json!({ "target": 7, "tar_temp": 220, "confirm": true }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ams_change_dry_run_previews_without_confirm_or_idle() {
        // dry_run echoes the resolved command without sending — usable even on a
        // busy printer and with no confirm, mirroring job_start's preview.
        let res = busy_app(FakeController::verified())
            .post("/api/ams/change")
            .json(&json!({ "target": 255, "tar_temp": 250, "dry_run": true }))
            .await;
        res.assert_status_ok();
        let body: serde_json::Value = res.json();
        assert_eq!(body["plan"]["command"], "ams_change_filament");
        assert_eq!(body["plan"]["target"], 255);
        assert_eq!(body["plan"]["tar_temp"], 250);
        // curr_temp defaults to tar_temp when omitted.
        assert_eq!(body["plan"]["curr_temp"], 250);
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
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
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
