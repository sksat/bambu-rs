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

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{
    StatusCode,
    header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE},
};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
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
use super::hook::NoHook;
use super::hook::{HookError, PrePrintHook};
#[cfg(test)]
use super::slice::FakeSlicer;
use super::slice::{SliceManager, SliceParams, Slicer, SlicerInfo};
#[cfg(test)]
use super::start::FakeStarter;
use super::start::{StartRequest, Starter};
use super::timelapse::{
    DEFAULT_SMOOTH_BURST_MS, FrameGrab, PlainCapture, TimelapseManager, real_park_spawn,
    real_segment_spawn,
};
use crate::core::command::{AmsControl, LedNode, SpeedLevel};
use crate::core::park::ParkTuning;
use crate::core::safety::{GcodeVerdict, TempLimits, check_extrude, check_gcode, check_jog};
use crate::core::session::CommandOutcome;
use crate::core::status::{Ams, AmsTray, AmsUnit, Filament, LightReport, Online, PrinterStatus};
use crate::park::{ParkCapture, SegmentCapture};

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
                // The A1 mini (AMS lite) reports ahb/rfid false regardless of the reader
                // actually working — these are X1/P1 "AMS hub / RFID bus" flags. The fake
                // mirrors that so the dashboard's read-derived RFID indicator is exercised
                // against a false online.rfid (the old false-alarm source).
                ahb: Some(false),
                rfid: Some(false),
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
        // A genuine (Bambu) spool carries an RFID tag, so a successful read fills in a
        // non-empty uuid + SKU id_name. The fake sets these — combined with the A1's
        // `online.rfid: false` below — so the dashboard exercises the real scenario: the
        // reader works (tags read) even though the online flag is a meaningless placeholder.
        id_name: Some(format!("A01-R{id}")),
        uuid: Some(format!("FACADE0000000000000000000000000{id}")),
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

/// Everything the server knows, across every printer it serves.
///
/// Only the printer map is here; each printer's own machinery lives in its
/// [`PrinterState`]. The split is what makes two printers independent: a
/// `job/start` on one no longer takes a lock the other is waiting on, and a
/// timelapse on one no longer occupies the only slot.
#[derive(Clone)]
pub struct ServerState {
    /// Configured printers by name. Ordered so `/api/printers` is stable.
    pub printers: Arc<BTreeMap<String, PrinterState>>,
    /// The [`PrinterState::id`] of the one that also answers on the unprefixed
    /// `/api/...` paths.
    pub default: String,
}

/// Safe to use as **both** one URL path segment and one directory name.
///
/// An identifier is trusted by two parsers at once, and their failure modes are
/// unrelated: `..` and `/tmp/x` escape the captures directory, while `{name}`
/// and `*` are axum route *syntax* — a printer identified as `{name}` would
/// quietly capture every mistyped printer URL, and other spellings make router
/// construction panic. Hence an allow-list.
pub fn is_safe_printer_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('.')
        && !s.ends_with('.')
        && !is_windows_device_name(s)
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Whether Windows reserves this as a device rather than a name.
///
/// `CON`, `NUL`, `COM1` and friends cannot be directories on Windows — and the
/// reservation applies to the stem, so `CON.txt` is taken too. This crate ships
/// a Windows build, so a profile innocently named `con` would otherwise pass
/// every check here and then fail to create `captures/con` on every timelapse,
/// with nothing pointing at the name as the cause.
fn is_windows_device_name(s: &str) -> bool {
    const RESERVED: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "COM", "LPT"];
    let stem = s.split('.').next().unwrap_or(s).to_ascii_uppercase();
    RESERVED.contains(&stem.as_str())
        || (stem.len() == 4
            && matches!(&stem[..3], "COM" | "LPT")
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

/// The identifier a printer is addressed by, derived from its profile name.
///
/// Profile names have always been free-form — `config add` stores whatever you
/// type, and `"Shop A"` is a perfectly reasonable thing to have typed. Those
/// configs predate this file caring about the name at all, so requiring them to
/// be renamed would break `bambu serve` for setups that worked yesterday.
/// Instead the name stays whatever it is and the *identifier* is derived:
/// ordinary names pass through unchanged, so there is nothing to learn unless
/// you used something exotic, and `/api/printers` reports both.
///
/// The result is safe because it is **checked**, not because the trimming below
/// happens to be in the right order. Trimming only leading dots first let
/// `"-.."` come out as `".."` — still a traversal, and one an enumerated test
/// missed. A predicate applied to the answer cannot miss a spelling nobody
/// thought of.
pub fn printer_id(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Both ends, both characters: a leading dot hides the directory (and `..`
    // escapes it), and a trailing dot is silently dropped by Windows, which
    // would make two ids name one directory.
    let trim = |s: &str| s.trim_matches(|c| c == '-' || c == '.').to_string();
    // Pure ASCII by construction, so truncating bytes cannot split a character.
    let mut id = trim(&mapped);
    id.truncate(64);
    let id = trim(&id);
    if is_safe_printer_id(&id) {
        return id;
    }
    // A device name is otherwise a perfectly good identifier; an underscore
    // takes it out of Windows' reserved set while keeping it recognisable, and
    // keeps two such names distinct instead of collapsing both to the fallback.
    // Only for that reason, though — appending to whatever else failed turns
    // `".."` into `"_"`, which is safe but tells the operator nothing.
    if is_windows_device_name(&id) {
        let escaped = format!("{id}_");
        if is_safe_printer_id(&escaped) {
            return escaped;
        }
    }
    // Nothing usable survived. Callers reject collisions, so falling back
    // cannot silently merge two printers.
    "printer".to_string()
}

/// The key two identifiers must not share.
///
/// Case-folded because this crate ships macOS and Windows builds, where
/// `captures/A1` and `captures/a1` are one directory: distinct routes, mixed
/// recordings. Comparing ids byte-for-byte would let that through.
pub fn printer_id_key(id: &str) -> String {
    id.to_ascii_lowercase()
}

/// One printer's share of the server.
#[derive(Clone)]
pub struct PrinterState {
    /// Profile name, as configured — free-form, shown to people.
    pub name: String,
    /// What this printer is addressed by: its route segment and its capture
    /// directory. Equal to `name` unless the name needed sanitising (see
    /// [`printer_id`]). The status report carries no identity of its own, so
    /// with two printers this is the only way to tell whose reading you have.
    pub id: String,
    /// Canonical model name, when known.
    pub model: Option<String>,
    /// Whether the runs recorded before captures were namespaced belong to this
    /// printer. True for exactly one — otherwise every printer would list, and
    /// serve, the same legacy footage as if it were its own.
    pub legacy_captures: bool,
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
    /// (`/api/camera/config`); seeded from `--camera-url` and in-memory only.
    /// Proxied server-side so a browser that can't reach the LAN cam (e.g. over
    /// Tailscale) still gets a live view.
    pub external_cameras: Arc<RwLock<Vec<ExternalCamera>>>,
    /// The **built-in** (printer chamber) camera, grabbed over TCP:6000 in live
    /// mode; [`NoCamera`](super::camera::NoCamera) in fake / no-target mode.
    pub internal_camera: Arc<dyn CameraSource>,
    /// Serve-internal per-layer timelapse capture, driven off `source`'s status
    /// feed and controlled at runtime by camera id. At most one runs at a time.
    pub timelapse: Arc<TimelapseManager>,
    /// Set while the `pre_print` hook is driving the machine, so the endpoints
    /// that move something can refuse rather than interleave with a plate
    /// changer mid-swing. `start_lock` does not cover them: it excludes other
    /// starts, and a jog is not a start.
    pub hook_running: Arc<std::sync::atomic::AtomicBool>,
    /// This printer's `pre_print` sequence, run immediately before a print
    /// starts — a plate changer's swap, typically. [`super::hook::NoHook`] when the profile
    /// configures none, which is the common case.
    pub hook: Arc<dyn PrePrintHook>,
    /// The slicer, **shared by every printer this server serves** — it is one
    /// heavyweight binary on this host, and `LiveSlicer` serialises the actual
    /// processes across them.
    pub slicer: Arc<dyn Slicer>,
    /// This printer's slice slot: one job at a time, no queue.
    pub slice_jobs: Arc<SliceManager>,
    /// Which BBL profiles slice for this printer's model, resolved once at
    /// startup from the capability registry. `None` = no verified mapping, so
    /// slicing is refused rather than aimed at some other machine's bed.
    pub slicer_names: Option<crate::core::slice::SlicerNames>,
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

impl PrinterState {
    #[cfg(test)]
    pub fn fake() -> Self {
        Self {
            name: "fake".to_string(),
            id: "fake".to_string(),
            model: None,
            legacy_captures: true,
            source: Arc::new(FakeSource::idle()),
            controller: Arc::new(FakeController::verified()),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: None,
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
            hook_running: Default::default(),
            hook: Arc::new(NoHook),
            slicer: Arc::new(FakeSlicer),
            slice_jobs: Default::default(),
            slicer_names: crate::core::capability::default_registry()
                .slicer_names(&crate::core::model::Model::A1Mini)
                .copied(),
        }
    }
}

/// One printer's API, with every path built under `prefix`.
///
/// A prefix rather than a path parameter: the printer set is fixed when the
/// server starts, so each one gets its own literal routes. That keeps every
/// handler's extractors exactly as they were — a `{name}` segment would hand
/// the nested handlers a second `Path` component and force all of them to be
/// rewritten — and it leaves the `/api/{*rest}` catch-all below still able to
/// tell a typo'd endpoint from a real one.
fn printer_routes(prefix: &str, state: PrinterState) -> Router {
    let p = |s: &str| format!("{prefix}{s}");

    let reads = Router::new()
        .route(&p("/status"), get(status))
        .route(&p("/ws"), get(status_ws))
        .route(&p("/file"), get(list_files))
        .route(&p("/file/thumbnail"), get(file_thumbnail))
        .route(&p("/file/raw"), get(file_raw))
        .route(&p("/file/gcode"), get(file_gcode))
        .route(&p("/file/inspect"), get(file_inspect))
        .route(&p("/file/mesh"), get(file_mesh))
        .route(&p("/camera"), get(cameras_list))
        .route(&p("/camera/{id}/snapshot"), get(camera_snapshot))
        .route(&p("/camera/{id}/stream"), get(camera_stream))
        .route(&p("/camera/{id}/park"), get(park_index))
        .route(&p("/camera/{id}/park/{n}"), get(camera_park_frame))
        .route(&p("/timelapse"), get(timelapse_status))
        .route(&p("/slice"), get(slice_info))
        .route(&p("/capture"), get(captures_list))
        .route(&p("/capture/{run}/{cam}/video.mp4"), get(capture_video))
        .route(&p("/capture/{run}/{cam}/thumb.jpg"), get(capture_thumb));
    let writes = Router::new()
        .route(&p("/job/pause"), post(job_pause))
        .route(&p("/job/resume"), post(job_resume))
        .route(&p("/job/stop"), post(job_stop))
        .route(&p("/job/clear-error"), post(job_clear_error))
        .route(&p("/job/start"), post(job_start))
        .route(&p("/light"), post(light))
        .route(&p("/speed"), post(speed))
        .route(&p("/gcode"), post(gcode))
        .route(&p("/home"), post(home))
        .route(&p("/move"), post(move_axis))
        .route(&p("/extrude"), post(extrude))
        .route(&p("/temp"), post(temp))
        .route(&p("/calibrate"), post(calibrate))
        .route(&p("/ams"), post(ams))
        .route(&p("/ams/change"), post(ams_change))
        .route(&p("/reboot"), post(reboot))
        .route(&p("/steppers"), post(steppers))
        .route(
            &p("/camera/config"),
            get(cameras_config_get).post(cameras_config_set),
        )
        .route(&p("/timelapse/start"), post(timelapse_start))
        .route(&p("/timelapse/stop"), post(timelapse_stop))
        // Uploads stream to a temp file, so the cap bounds disk, not memory.
        .route(
            &p("/file/upload"),
            post(upload_file).layer(DefaultBodyLimit::max(512 * 1024 * 1024)),
        )
        .route(
            &p("/job/upload-start"),
            post(job_upload_start).layer(DefaultBodyLimit::max(512 * 1024 * 1024)),
        )
        // The model to slice streams to the job's temp dir, same cap as an upload.
        .route(
            &p("/slice"),
            post(slice_start).layer(DefaultBodyLimit::max(512 * 1024 * 1024)),
        )
        .route(&p("/slice/print"), post(slice_print))
        // Gated with the writes, deliberately. Open reads are about the
        // PRINTER's state; this hands back a file derived from geometry the
        // caller uploaded moments ago, and letting that fall out of router
        // placement rather than a decision is how it would stay open by
        // accident.
        .route(&p("/slice/result"), get(slice_result))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_password,
        ));
    reads.merge(writes).with_state(state)
}

/// Build the whole API: every configured printer, plus the default one mounted
/// unprefixed so existing callers keep working.
pub fn router(server: ServerState) -> Router {
    let default = server
        .printers
        .get(&server.default)
        .expect("the default printer is one of the configured ones")
        .clone();
    // The unprefixed mount is permanent, not a deprecation: one printer and no
    // prefix stays the common case, and every script, skill and `--via-serve`
    // call already written targets it.
    let mut app = printer_routes("/api", default);
    for (name, st) in server.printers.iter() {
        app = app.merge(printer_routes(&format!("/api/printers/{name}"), st.clone()));
    }
    let app = app
        .merge(
            Router::new()
                .route("/api/printers", get(printers_list))
                .with_state(server),
        )
        // Unknown `/api/*` paths 404 as JSON (a typo'd endpoint shouldn't fall through to the
        // SPA and get HTML 200). Specific routes above are more specific than this catch-all,
        // so they still win; only unmatched API paths land here.
        .route("/api/{*rest}", any(api_not_found));
    #[cfg(feature = "dashboard")]
    let app = app.fallback(static_handler);
    app
}

/// The printers this server serves, in configured order, each with its current
/// status.
///
/// The status is included rather than left to N follow-up requests because an
/// overview of every machine is the whole reason to run one server for several:
/// asking per printer would make the common view cost a round trip per printer
/// and show them at different instants. Each reading is the source's cached
/// latest, so this costs no printer traffic at all.
///
/// `default` marks the one the unprefixed `/api/...` paths reach.
async fn printers_list(State(st): State<ServerState>) -> Response {
    let list = st
        .printers
        .values()
        .map(|p| {
            json!({
                "name": p.name,
                "id": p.id,
                "model": p.model,
                "default": p.id == st.default,
                "status": p.source.current(),
            })
        })
        .collect::<Vec<_>>();
    Json(json!({ "printers": list, "default": st.default })).into_response()
}

/// 404 for an unmatched `/api/*` path — JSON, not the SPA fallback's HTML.
async fn api_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "unknown API endpoint" })),
    )
        .into_response()
}

async fn status(State(st): State<PrinterState>) -> Json<PrinterStatus> {
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

async fn job_pause(State(st): State<PrinterState>, body: Option<Json<ConfirmBody>>) -> Response {
    run_confirmed(st, ControlAction::Pause, body).await
}
async fn job_resume(State(st): State<PrinterState>, body: Option<Json<ConfirmBody>>) -> Response {
    run_confirmed(st, ControlAction::Resume, body).await
}
async fn job_stop(State(st): State<PrinterState>, body: Option<Json<ConfirmBody>>) -> Response {
    run_confirmed(st, ControlAction::Stop, body).await
}
/// Dismiss a print error (`clean_print_error`) — narrow: it only acknowledges
/// the error popup (the way Studio clears one), it does not stop/resume the job.
/// Gated by confirm like the other job controls.
async fn job_clear_error(
    State(st): State<PrinterState>,
    body: Option<Json<ConfirmBody>>,
) -> Response {
    run_confirmed(st, ControlAction::ClearError, body).await
}

async fn light(State(st): State<PrinterState>, Json(b): Json<LightBody>) -> Response {
    let node = match b.node.as_str() {
        "chamber" => LedNode::ChamberLight,
        "work" => LedNode::WorkLight,
        other => return bad_request(format!("unknown light node {other:?}")),
    };
    execute(st, ControlAction::Light { node, on: b.on }).await
}

async fn speed(State(st): State<PrinterState>, Json(b): Json<SpeedBody>) -> Response {
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
async fn gcode(State(st): State<PrinterState>, Json(b): Json<GcodeBody>) -> Response {
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
    if let Some(hooked) = require_no_hook(&st) {
        return hooked;
    }
    execute(st, ControlAction::Gcode(b.line)).await
}

/// Require `{"confirm": true}` before running a destructive action (428 if not).
async fn run_confirmed(
    st: PrinterState,
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
async fn execute(st: PrinterState, action: ControlAction) -> Response {
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

/// Refuse a control action while the pre-print hook is driving the machine.
///
/// The hook runs a plate changer for minutes, and `start_lock` only excludes
/// other *starts* — a jog, a home or a raw G-code line still sees an idle
/// printer and interleaves with the swap, invalidating the positions it
/// assumed. So the endpoints that MOVE something honour this too.
///
/// Deliberately not applied to `pause`/`resume`/`stop`/`clear-error`/`reboot`:
/// stopping has to work at exactly the moment something is going wrong, and a
/// gate that makes the emergency controls unavailable during the one operation
/// most likely to need them is worse than the interleaving it prevents.
fn require_no_hook(st: &PrinterState) -> Option<Response> {
    if !st.hook_running.load(std::sync::atomic::Ordering::SeqCst) {
        return None;
    }
    Some(
        (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "the pre-print sequence is running; this would move the machine \
                          mid-sequence (pause/stop remain available)"
            })),
        )
            .into_response(),
    )
}

/// Refuse a control action while the printer is busy (409). The predicate
/// mirrors `job_start`'s idle guard exactly: any of RUNNING/PAUSE/PREPARE/SLICING
/// (case-insensitive) is "busy". `None` ⇒ idle, run the action.
fn require_idle(st: &PrinterState) -> Option<Response> {
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
async fn home(State(st): State<PrinterState>, Json(b): Json<HomeBody>) -> Response {
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
    if let Some(hooked) = require_no_hook(&st) {
        return hooked;
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
async fn move_axis(State(st): State<PrinterState>, Json(b): Json<MoveBody>) -> Response {
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
    if let Some(hooked) = require_no_hook(&st) {
        return hooked;
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
async fn extrude(State(st): State<PrinterState>, Json(b): Json<ExtrudeBody>) -> Response {
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
    if let Some(hooked) = require_no_hook(&st) {
        return hooked;
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
async fn temp(State(st): State<PrinterState>, Json(b): Json<TempBody>) -> Response {
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
async fn calibrate(State(st): State<PrinterState>, Json(b): Json<CalibrateBody>) -> Response {
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
    if let Some(hooked) = require_no_hook(&st) {
        return hooked;
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
async fn ams(State(st): State<PrinterState>, Json(b): Json<AmsBody>) -> Response {
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
    if let Some(hooked) = require_no_hook(&st) {
        return hooked;
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
async fn ams_change(State(st): State<PrinterState>, Json(b): Json<AmsChangeBody>) -> Response {
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
    if let Some(hooked) = require_no_hook(&st) {
        return hooked;
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
async fn reboot(State(st): State<PrinterState>, body: Option<Json<ConfirmBody>>) -> Response {
    if let Some(unconfirmed) = need_confirm(body.map(|b| b.confirm).unwrap_or(false)) {
        return unconfirmed;
    }
    if let Some(busy) = require_idle(&st) {
        return busy;
    }
    if let Some(hooked) = require_no_hook(&st) {
        return hooked;
    }
    execute(st, ControlAction::Reboot).await
}

/// Disable the stepper motors (`M84`). Confirm (428) + idle (409).
async fn steppers(State(st): State<PrinterState>, body: Option<Json<ConfirmBody>>) -> Response {
    if let Some(unconfirmed) = need_confirm(body.map(|b| b.confirm).unwrap_or(false)) {
        return unconfirmed;
    }
    if let Some(busy) = require_idle(&st) {
        return busy;
    }
    if let Some(hooked) = require_no_hook(&st) {
        return hooked;
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
async fn job_start(State(st): State<PrinterState>, Json(b): Json<StartBody>) -> Response {
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
    // With an AMS mapping, inspecting is MANDATORY, not a nicety: the wire array
    // is keyed by each filament's index in the project, and only the plate's
    // `filament_ids` says which those are. Starting without it sends the map
    // un-expanded, which on a plate that doesn't use the project's first
    // filament silently prints the wrong material (device-verified). Without a
    // mapping this stays `None` — no AMS, nothing to get wrong.
    let inspection = if b.use_ams && lower.ends_with(".3mf") {
        let (files, file, plate) = (st.files.clone(), b.file.clone(), b.plate);
        match tokio::task::spawn_blocking(move || {
            files.fetch(&file).and_then(|bytes| {
                crate::core::project::inspect_plate(&bytes, plate).map_err(|e| e.to_string())
            })
        })
        .await
        {
            Ok(Ok(insp)) => Some(insp),
            Ok(Err(e)) => {
                return bad_request(format!(
                    "cannot inspect {} to resolve the AMS mapping: {e}",
                    b.file
                ));
            }
            Err(e) => {
                return bad_request(format!("inspection task failed: {e}"));
            }
        }
    } else {
        None
    };
    // Length matters as much as range, and only the plate says what it should
    // be: the wire array is keyed by each filament's index in the project, so
    // `expand_ams_map` leaves a wrong-length map UNEXPANDED and the printer
    // uses whatever the gcode baked in. That is how a plate got printed in the
    // wrong material once already — device-verified — so a map that cannot be
    // expanded is refused here rather than sent and hoped over.
    if let Some(insp) = inspection.as_ref()
        && let Err(why) = crate::core::start::ams_map_fits(&b.ams_map, &insp.filament_ids)
    {
        return bad_request(format!("ams_map {why} ({} plate {})", b.file, b.plate));
    }
    let req = StartRequest {
        file: b.file.clone(),
        plate: b.plate,
        use_ams: b.use_ams,
        ams_map: b.ams_map.clone(),
        bed_type: b.bed_type.clone().unwrap_or_else(|| "auto".to_string()),
        timelapse: b.timelapse,
        inspection,
    };

    if b.dry_run {
        // Best-effort: download + inspect the on-printer .3mf so the plan can say whether
        // this plate supports a clean per-layer timelapse (the head-park blocks). Never
        // fatal — a failed download/inspect just leaves it `null` ("unknown"), like the
        // CLI's best-effort dry-run. Mirrors `PlateInspection::has_timelapse_blocks`.
        let has_timelapse_blocks = if req.file.to_ascii_lowercase().ends_with(".3mf") {
            let (files, file, plate) = (st.files.clone(), req.file.clone(), req.plate);
            match tokio::task::spawn_blocking(move || {
                files.fetch(&file).and_then(|bytes| {
                    crate::core::project::inspect_plate(&bytes, plate).map_err(|e| e.to_string())
                })
            })
            .await
            {
                Ok(Ok(insp)) => Some(insp.has_timelapse_blocks),
                _ => None,
            }
        } else {
            None
        };
        return Json(json!({ "plan": {
            "file": req.file,
            "plate": req.plate,
            "use_ams": req.use_ams,
            "ams_map": req.ams_map,
            "bed_type": req.bed_type,
            "timelapse": req.timelapse,
            "has_timelapse_blocks": has_timelapse_blocks,
            // A preview that omits hardware motion under-reports what a
            // confirmed start does. `null` when this printer has no hook.
            "pre_print": st.hook.describe(),
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
    if let Some(failed) = run_pre_print_hook(&st).await {
        return failed;
    }
    let starter = st.starter.clone();
    let res = tokio::task::spawn_blocking(move || starter.start(&req)).await;
    verify_response(res)
}

/// Run the printer's `pre_print` hook, if it has one. `Some(response)` means the
/// print must NOT be started.
///
/// Fired after everything that can refuse the print, and immediately before the
/// start: a swap that ejects the last plate must not happen for a print that
/// then fails to start. It returns only once the motion has been observed to
/// finish — an ACK is not enough, because a print start does not queue behind
/// the sequence (see `core::settle`).
async fn run_pre_print_hook(st: &PrinterState) -> Option<Response> {
    st.hook.describe()?;
    let hook = st.hook.clone();
    let running = st.hook_running.clone();
    running.store(true, std::sync::atomic::Ordering::SeqCst);
    let outcome = tokio::task::spawn_blocking(move || hook.run()).await;
    running.store(false, std::sync::atomic::Ordering::SeqCst);
    match outcome {
        Ok(Ok(())) => None,
        Ok(Err(e)) => {
            // Which one to go and fix: a bad sequence file is the operator's
            // configuration, a refusal or an unfinished motion is the machine.
            let code = match e {
                HookError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
                HookError::Printer(_) => StatusCode::CONFLICT,
                HookError::Transport(_) => StatusCode::BAD_GATEWAY,
            };
            Some(
                (
                    code,
                    Json(json!({ "error": format!("{e}; print not started") })),
                )
                    .into_response(),
            )
        }
        Err(_) => Some(server_error(
            "the pre-print sequence task failed; print not started".to_string(),
        )),
    }
}

// ── File endpoints ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ListQuery {
    dir: Option<String>,
}

/// List files on the printer (open read). `?dir=` defaults to `/`.
async fn list_files(State(st): State<PrinterState>, Query(q): Query<ListQuery>) -> Response {
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
async fn file_thumbnail(State(st): State<PrinterState>, Query(q): Query<ThumbQuery>) -> Response {
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
async fn file_raw(State(st): State<PrinterState>, Query(q): Query<RawQuery>) -> Response {
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
async fn file_gcode(State(st): State<PrinterState>, Query(q): Query<GcodeFileQuery>) -> Response {
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
struct InspectQuery {
    name: String,
    #[serde(default = "default_plate")]
    plate: u32,
}

/// Inspect an on-printer `.3mf` plate (open read): download + parse it, reporting whether
/// the sliced gcode supports a clean per-layer timelapse (`has_timelapse_blocks`) plus the
/// md5 / bed / filament metadata. Lets the start dialog show a file's timelapse capability
/// the moment it opens — without a write-gated dry-run. Best-effort: a non-3mf or
/// unreadable file returns `{ "inspected": false, ... }` rather than an error status, so
/// the dialog degrades to "unknown" cleanly.
async fn file_inspect(State(st): State<PrinterState>, Query(q): Query<InspectQuery>) -> Response {
    let remote = if q.name.starts_with('/') {
        q.name.clone()
    } else {
        format!("/{}", q.name)
    };
    if !is_safe_remote_path(&remote) || !remote.to_ascii_lowercase().ends_with(".3mf") {
        return Json(json!({ "inspected": false, "error": "not a .3mf printer path" }))
            .into_response();
    }
    if !(1..=64).contains(&q.plate) {
        return bad_request("plate out of range (1..64)".to_string());
    }
    let (files, plate) = (st.files.clone(), q.plate);
    match tokio::task::spawn_blocking(move || {
        files
            .fetch(&remote)
            .and_then(|b| crate::core::project::inspect_plate(&b, plate).map_err(|e| e.to_string()))
    })
    .await
    {
        Ok(Ok(i)) => Json(json!({
            "inspected": true,
            "plate": i.plate,
            "has_timelapse_blocks": i.has_timelapse_blocks,
            "gcode_md5": i.gcode_md5,
            "bed_type": i.bed_type,
            "filament_colors": i.filament_colors,
        }))
        .into_response(),
        Ok(Err(e)) => Json(json!({ "inspected": false, "error": e })).into_response(),
        Err(_) => server_error("inspect task failed".to_string()),
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
async fn file_mesh(State(st): State<PrinterState>, Query(q): Query<MeshQuery>) -> Response {
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
// together by /api/camera: the **built-in** printer chamber camera (TCP:6000,
// often dead on the A1) and any number of **external** IP cameras the server
// proxies (e.g. ATOM Cams over LAN). Externals can be set at launch (--camera-url,
// repeatable) and edited at runtime via the gated config endpoint. IDs are
// positional: "internal" for the built-in, "ext-{i}" for the i-th external.

/// List the available cameras (open read) as `{id, kind, label}`. URLs are never
/// exposed here — only the proxied snapshot is reachable, by id.
async fn cameras_list(State(st): State<PrinterState>) -> Json<serde_json::Value> {
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
            // Whether it's ready for the robust dense-stream `segment` capture: a stream,
            // a park_tuning (for the capture fps), AND a select_tuning (the median-subtract
            // knobs). The dashboard prefers this over `park` when present.
            "segment": c.stream_url.is_some()
                && c.park_tuning.is_some()
                && c.select_tuning.is_some(),
        }));
    }
    Json(json!({ "cameras": cameras }))
}

/// Proxy a single JPEG for one camera by id (open read). `internal` grabs the
/// built-in cam over TCP:6000; `ext-{i}` proxies that external camera's URL. 404
/// for an unknown id / unconfigured source; 502 when the grab fails.
async fn camera_snapshot(State(st): State<PrinterState>, Path(id): Path<String>) -> Response {
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
    match tokio::task::spawn_blocking(move || crate::server::camera::fetch_camera_frame(&url)).await
    {
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
async fn camera_stream(State(st): State<PrinterState>, Path(id): Path<String>) -> Response {
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

/// Parse a park run's `parks.jsonl` (one line per write) into a per-frame index for the
/// player: one entry per distinct frame `n` (a `replace` line re-uses an `n`, so the last
/// one — the stronger frame — wins), sorted by n, with malformed/blank lines skipped. Each
/// entry is `{ n, t, confidence }` — enough for a scrubber with a timestamp readout. Pure,
/// so it's unit-tested without the filesystem.
fn parse_parks_index(contents: &str) -> Vec<serde_json::Value> {
    let mut by_n: std::collections::BTreeMap<u64, serde_json::Value> =
        std::collections::BTreeMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(n) = v.get("n").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        by_n.insert(
            n,
            json!({
                "n": n,
                "t": v.get("t").and_then(serde_json::Value::as_f64),
                "confidence": v.get("confidence").and_then(serde_json::Value::as_f64),
            }),
        );
    }
    by_n.into_values().collect()
}

/// The park index `/api/camera/{id}/park`: a camera's captured park frames for the
/// dashboard player (open read), `{ running, count, parks: [{n, t, confidence}, …] }` from
/// `<out>/<id>/parks.jsonl`. The individual frames are `…/park/{n}` ([`camera_park_frame`]).
/// Available while the run is active AND after it stops (until the next run replaces the
/// status's out_dir), so the whole filmstrip stays reviewable. 404 when no park run owns
/// `id`. The id is matched against the run's own camera list, never joined blindly.
/// The source dir for the live park preview: the `park` run if one owns it, else the
/// `smooth` run (its live per-layer selection publishes the same `park_*.jpg`/`parks.jsonl`
/// into its dir). Returns `(out_dir, cameras, running)`.
fn live_park_source(st: &PrinterState) -> Option<(String, Vec<String>, bool)> {
    // The park preview reads `latest_park.jpg`/`parks.jsonl`, which three slots can produce:
    // `segment` (the dense-stream robust path), `park` (the image-change miner), and a
    // `smooth` run with live selection. Prefer a RUNNING one (so the preview follows the
    // active capture when several have been started this session), else the most recent to
    // have a dir. Within each tier order segment → park → smooth.
    let sources = [
        st.timelapse.status_segment(),
        st.timelapse.status_park(),
        st.timelapse.status_smooth(),
    ];
    // Prefer a RUNNING run so the preview follows the active capture (segment → park →
    // smooth on a tie, but they're rarely all live at once).
    if let Some(s) = sources.iter().find(|s| s.running && s.out_dir.is_some()) {
        return Some((s.out_dir.clone().unwrap(), s.cameras.clone(), s.running));
    }
    // Otherwise the most RECENT completed run, not a fixed slot order — the run dir is
    // `captures/<epoch>_<hint>_<mode>`, so its leading epoch orders them by recency with no
    // extra bookkeeping (else a stale segment dir would shadow a newer park/smooth one).
    sources
        .into_iter()
        .filter(|s| s.out_dir.is_some())
        .max_by_key(|s| run_dir_epoch(s.out_dir.as_deref().unwrap_or("")))
        .map(|s| (s.out_dir.unwrap(), s.cameras, s.running))
}

/// Recency key for a run dir (`captures/<epoch>_<hint>_<mode>`): its leading epoch. An
/// unparseable dir sorts oldest, so a real run always wins over a malformed one.
fn run_dir_epoch(dir: &str) -> u64 {
    std::path::Path::new(dir)
        .file_name()
        .and_then(|f| f.to_str())
        .and_then(|f| f.split('_').next())
        .and_then(|e| e.parse::<u64>().ok())
        .unwrap_or(0)
}

async fn park_index(State(st): State<PrinterState>, Path(id): Path<String>) -> Response {
    let Some((dir, cameras, running)) = live_park_source(&st) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !cameras.iter().any(|c| c == &id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let jsonl = std::path::Path::new(&dir).join(&id).join("parks.jsonl");
    let parks = match tokio::fs::read_to_string(&jsonl).await {
        Ok(s) => parse_parks_index(&s),
        Err(_) => Vec::new(), // run started, no park written yet → an empty filmstrip
    };
    Json(json!({ "running": running, "count": parks.len(), "parks": parks })).into_response()
}

/// Serve one indexed park frame `/api/camera/{id}/park/{n}` (`park_NNNNNN.jpg`) for a
/// camera (open read). Same gating and lifetime as the [`park_index`] it belongs to. `n`
/// is numeric, so it can't traverse; an index with no file (out of range / pruned) 404s.
async fn camera_park_frame(
    State(st): State<PrinterState>,
    Path((id, n)): Path<(String, u64)>,
) -> Response {
    let Some((dir, cameras, _)) = live_park_source(&st) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !cameras.iter().any(|c| c == &id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = std::path::Path::new(&dir)
        .join(&id)
        .join(format!("park_{n:06}.jpg"));
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (CONTENT_TYPE, "image/jpeg".to_string()),
                (CACHE_CONTROL, "no-store".to_string()),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The tuning echo for the manage form: the park knobs MERGED with the select knobs into
/// one object, mirroring the single combined object the form posts. Without the merge a
/// re-save would drop `select_tuning` (it's stored separately but lives in the same posted
/// object). `null` when the camera has no tuning at all.
fn tuning_json(c: &ExternalCamera) -> serde_json::Value {
    let Some(park) = &c.park_tuning else {
        return serde_json::Value::Null;
    };
    let mut v = serde_json::to_value(park).unwrap_or_else(|_| json!({}));
    if let (Some(obj), Some(sel)) = (v.as_object_mut(), c.select_tuning)
        && let Ok(serde_json::Value::Object(sobj)) = serde_json::to_value(sel)
    {
        // Add the select-only knobs; shared keys (e.g. left_frac) keep the park value.
        for (k, val) in sobj {
            obj.entry(k).or_insert(val);
        }
    }
    v
}

/// Serialise the external list (with URLs) for the gated config endpoints.
fn external_json(st: &PrinterState) -> Vec<serde_json::Value> {
    st.external_cameras
        .read()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, c)| {
            json!({ "id": format!("ext-{i}"), "label": c.label, "url": c.url,
                    "stream_url": c.stream_url, "park_tuning": tuning_json(c) })
        })
        .collect()
}

/// Current external-camera config (gated read) — includes URLs so the dashboard's
/// manage form can prefill. The built-in camera isn't configurable, so it's not
/// listed here.
async fn cameras_config_get(State(st): State<PrinterState>) -> Json<serde_json::Value> {
    Json(json!({ "external": external_json(&st) }))
}

#[derive(Deserialize)]
struct ExternalCameraInput {
    label: Option<String>,
    url: String,
    /// Optional live MJPEG stream URL (reverse-proxied at `/stream`).
    #[serde(default)]
    stream_url: Option<String>,
    /// Optional per-camera tuning, a raw JSON object (same shape as the CLI
    /// `--cameras-config`): parsed below into [`ParkTuning`] (STRICT — no baked defaults, a
    /// partial object is rejected) AND `SelectTuning` (best-effort — the select knobs live
    /// in the same object), so an HTTP-configured camera is `park`/`segment`-capable exactly
    /// like a CLI-seeded one.
    #[serde(default)]
    park_tuning: Option<serde_json::Value>,
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
    State(st): State<PrinterState>,
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
        // One raw tuning object → ParkTuning (strict; a partial object is a 400, no baked
        // defaults) AND SelectTuning (best-effort — the select knobs share the object), the
        // same split the CLI `--cameras-config` does, so both paths stay consistent.
        let (park, select) = match e.park_tuning {
            Some(v) => {
                let park: ParkTuning = match serde_json::from_value(v.clone()) {
                    Ok(p) => p,
                    Err(err) => return bad_request(format!("invalid park_tuning: {err}")),
                };
                (Some(park), serde_json::from_value(v).ok())
            }
            None => (None, None),
        };
        next.push(
            ExternalCamera::new(e.label, url, stream_url, i)
                .with_park_tuning(park)
                .with_select_tuning(select),
        );
    }
    *st.external_cameras.write().unwrap() = next;
    Json(json!({ "external": external_json(&st) })).into_response()
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
    /// Segment: per-layer accumulation SAFETY CAP in ms (default 120000). The native park is
    /// a layer-change event, so the segment spans the WHOLE layer (finalized by the next
    /// layer edge) and median-subtract selection finds the park wherever it lands; this cap
    /// only forces a selection if `layer_num` stalls, so it sits well above any layer time.
    #[serde(default)]
    window_ms: Option<u64>,
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
fn timelapse_status_json(st: &PrinterState) -> serde_json::Value {
    let smooth = st.timelapse.status_smooth();
    let plain = st.timelapse.status_plain();
    let park = st.timelapse.status_park();
    let segment = st.timelapse.status_segment();
    let mut out = smooth.to_json();
    if let Some(o) = out.as_object_mut() {
        o.insert(
            "running".to_string(),
            json!(smooth.running || plain.running || park.running || segment.running),
        );
        o.insert("smooth".to_string(), smooth.to_json());
        o.insert("plain".to_string(), plain.to_json());
        o.insert("park".to_string(), park.to_json());
        o.insert("segment".to_string(), segment.to_json());
    }
    out
}

/// Resolve a camera id to a blocking frame-grabber + a stable label, captured at
/// start so a later `/api/camera/config` edit can't repoint a running capture.
fn resolve_grab(st: &PrinterState, camera: &str) -> Option<(String, FrameGrab)> {
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
        Arc::new(move || crate::server::camera::fetch_camera_frame(&url).map(|(_, bytes)| bytes)),
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

/// The root all capture runs are written under (relative to the serve's CWD). One place,
/// so the listing endpoint and the writers agree.
///
/// Namespaced by printer: two machines recording at the same second would
/// otherwise write into the same run directory, and each one's dashboard would
/// list the other's footage.
fn captures_root(st: &PrinterState) -> std::path::PathBuf {
    debug_assert!(is_safe_printer_id(&st.id), "ids are safe by construction");
    std::path::PathBuf::from("captures").join(&st.id)
}

/// Locate one run: this printer's namespace first, then the pre-namespacing
/// root, so a run recorded before the split is still playable.
fn capture_run_dir(st: &PrinterState, run: &str) -> std::path::PathBuf {
    let dir = captures_root(st).join(run);
    if dir.is_dir() || !st.legacy_captures {
        return dir;
    }
    legacy_captures_root().join(run)
}

/// Where runs were written before captures were namespaced by printer.
///
/// Still *listed* for the default printer, so a server that has been recording
/// for months doesn't appear to have lost everything the day it learns about a
/// second machine. Nothing new is ever written here.
fn legacy_captures_root() -> std::path::PathBuf {
    std::path::PathBuf::from("captures")
}

/// `captures/<epoch>_<print-hint>_<mode>/` — the per-run output dir (per-mode so a
/// concurrent smooth/plain/park run never mixes frames).
fn run_out_dir(st: &PrinterState, mode: &str) -> std::path::PathBuf {
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
    captures_root(st).join(format!("{epoch}_{hint}_{mode}"))
}

/// List finished/in-progress capture runs on disk (open read): each run's recordings, so
/// the dashboard can review and download them. Reads `captures/` lazily off the blocking
/// pool. An absent root → an empty list, never an error.
async fn captures_list(State(st): State<PrinterState>) -> Response {
    let root = captures_root(&st);
    let legacy = st.legacy_captures;
    let runs = tokio::task::spawn_blocking(move || {
        let mut runs = crate::captures::list_captures(&root);
        // Pre-namespacing runs live directly under `captures/` and belong to one
        // printer, not to all of them.
        if legacy {
            runs.extend(crate::captures::list_captures(&legacy_captures_root()));
        }
        runs
    })
    .await
    .unwrap_or_default();
    Json(json!({ "captures": runs })).into_response()
}

/// A single path segment safe to join under the captures root: no traversal, no separators,
/// no leading dot, bounded length.
fn is_safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && !s.contains('/')
        && !s.contains('\\')
        && s != ".."
}

#[derive(Deserialize)]
struct CaptureVideoQuery {
    /// Playback fps for an assembled image-sequence timelapse.
    #[serde(default = "default_fps")]
    fps: u32,
}
fn default_fps() -> u32 {
    10
}

/// One capture's mp4 (open read, playable/downloadable): a Video streams its `plain.mp4`; a
/// Park/Smooth is assembled from its frames on demand (→ `timelapse.mp4`) and streamed.
/// `run`/`cam` are validated as plain dir segments (no traversal); a missing `cam` subdir
/// maps back to the run dir (old single-dir layout). 404 when there's nothing to serve.
async fn capture_video(
    State(st): State<PrinterState>,
    Path((run, cam)): Path<(String, String)>,
    Query(q): Query<CaptureVideoQuery>,
) -> Response {
    if !is_safe_segment(&run) || !is_safe_segment(&cam) {
        return bad_request("invalid capture path".to_string());
    }
    let fps = q.fps.clamp(1, 60);
    // A smooth recording is a per-layer BURST; if this camera has select tuning, assemble a
    // CLEAN one-frame-per-layer timelapse (pick the parked frame). `ext-N` → external_cameras[N].
    let select_tuning = cam
        .strip_prefix("ext-")
        .and_then(|n| n.parse::<usize>().ok())
        .and_then(|i| {
            st.external_cameras
                .read()
                .unwrap()
                .get(i)
                .and_then(|c| c.select_tuning)
        });
    let run_dir = capture_run_dir(&st, &run);
    let sub = run_dir.join(&cam);
    let cam_dir = if sub.is_dir() { sub } else { run_dir };
    let path = tokio::task::spawn_blocking(move || -> Result<std::path::PathBuf, String> {
        use crate::captures::{CaptureKind, assemble_mp4, classify};
        let files: Vec<String> = std::fs::read_dir(&cam_dir)
            .map_err(|e| e.to_string())?
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        match classify(&files).ok_or("no recording")?.kind {
            CaptureKind::Video => {
                let mp4 = cam_dir.join("plain.mp4");
                if mp4.is_file() {
                    return Ok(mp4);
                }
                // ffmpeg was absent at capture → only a raw plain.mjpeg. Transcode it now.
                let mjpeg = cam_dir.join("plain.mjpeg");
                if mjpeg.is_file() {
                    crate::captures::transcode_mp4(&mjpeg, &mp4)?;
                    return Ok(mp4);
                }
                Err("video not available".to_string())
            }
            CaptureKind::Smooth => {
                let out = cam_dir.join("timelapse.mp4");
                // Clean per-layer selection when tuned; otherwise the raw all-frames assemble
                // (a burst-y timelapse, but better than nothing for an untuned camera).
                let selected = select_tuning.is_some_and(|sel| {
                    crate::captures::assemble_smooth_selected_mp4(&cam_dir, &sel, &out, fps).is_ok()
                });
                if !selected {
                    assemble_mp4(&cam_dir, CaptureKind::Smooth, &out, fps)?;
                }
                Ok(out)
            }
            kind => {
                let out = cam_dir.join("timelapse.mp4");
                assemble_mp4(&cam_dir, kind, &out, fps)?;
                Ok(out)
            }
        }
    })
    .await;
    let p = match path {
        Ok(Ok(p)) => p,
        Ok(Err(_)) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return server_error("assemble task failed".to_string()),
    };
    // Stream the file rather than buffering it — a full-print video or a long timelapse can
    // be large, and several may download at once.
    let Ok(file) = tokio::fs::File::open(&p).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let stream = futures_util::stream::unfold(Some(file), |st| async move {
        let mut f = st?;
        let mut buf = vec![0u8; 64 * 1024];
        match tokio::io::AsyncReadExt::read(&mut f, &mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<Bytes, std::io::Error>(Bytes::from(buf)), Some(f)))
            }
            Err(e) => Some((Err(e), None)),
        }
    });
    (
        [
            (CONTENT_TYPE, "video/mp4".to_string()),
            (CACHE_CONTROL, "no-store".to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

/// One capture's thumbnail (open read): a representative still for the recordings list. A
/// Park/Smooth serves its last frame straight off disk (no transcode); a Video extracts a
/// poster with ffmpeg, cached as `thumb.jpg`. Path-safe like [`capture_video`]; 404 when
/// there's nothing to show (incl. ffmpeg missing for a Video).
async fn capture_thumb(
    State(st): State<PrinterState>,
    Path((run, cam)): Path<(String, String)>,
) -> Response {
    if !is_safe_segment(&run) || !is_safe_segment(&cam) {
        return bad_request("invalid capture path".to_string());
    }
    let run_dir = capture_run_dir(&st, &run);
    let sub = run_dir.join(&cam);
    let cam_dir = if sub.is_dir() { sub } else { run_dir };
    let path = tokio::task::spawn_blocking(move || -> Result<std::path::PathBuf, String> {
        use crate::captures::{CaptureKind, classify, extract_video_thumb, thumb_frame};
        let files: Vec<String> = std::fs::read_dir(&cam_dir)
            .map_err(|e| e.to_string())?
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        let kind = classify(&files).ok_or("no recording")?.kind;
        match kind {
            CaptureKind::Video => {
                let thumb = cam_dir.join("thumb.jpg");
                if !thumb.is_file() {
                    let mp4 = cam_dir.join("plain.mp4");
                    let src = if mp4.is_file() {
                        mp4
                    } else {
                        cam_dir.join("plain.mjpeg")
                    };
                    if !src.is_file() {
                        return Err("no video".to_string());
                    }
                    extract_video_thumb(&src, &thumb)?;
                }
                Ok(thumb)
            }
            kind => Ok(cam_dir.join(thumb_frame(&files, kind).ok_or("no frame")?)),
        }
    })
    .await;
    let p = match path {
        Ok(Ok(p)) => p,
        Ok(Err(_)) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return server_error("thumb task failed".to_string()),
    };
    match tokio::fs::read(&p).await {
        Ok(bytes) => (
            [
                (CONTENT_TYPE, "image/jpeg".to_string()),
                // The /thumb.jpg URL tracks the run's *latest* frame, which moves while a run
                // is live — don't let a stale poster stick. (Finished runs re-read a tiny jpeg.)
                (CACHE_CONTROL, "no-store".to_string()),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Resolve the park-capable cameras among `ids` and start the live park slot. A camera is
/// capable iff it's an external camera with BOTH a stream and a calibrated `park_tuning`;
/// non-capable requested cameras are skipped (reported in `skipped`), and it's a 400 if
/// none qualify. Each emits `<out>/<id>/latest_park.jpg` per layer, served (open) by
/// `/api/camera/{id}/park`.
fn start_park_run(
    st: &PrinterState,
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

/// Resolve the segment-capable cameras among `ids` and start the dense-stream segmented
/// slot. A camera qualifies iff it's an external camera with a stream, a `park_tuning` (for
/// the capture fps), AND a `select_tuning` (the median-subtract knobs) — i.e. fully
/// calibrated for park capture. Non-capable requested cameras are skipped; it's a 400 if
/// none qualify. Output is the same `latest_park.jpg`/`parks.jsonl` layout `park` produces,
/// served by `/api/camera/{id}/park`.
fn start_segment_run(
    st: &PrinterState,
    ids: &[String],
    window_ms: u64,
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
            .and_then(|c| match (&c.stream_url, &c.park_tuning, c.select_tuning) {
                (Some(url), Some(park), Some(select)) => Some(SegmentCapture {
                    id: id.clone(),
                    stream_url: url.clone(),
                    fps: park.fps,
                    window_ms,
                    select_tuning: select,
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
            "no segment-capable cameras among {ids:?} — each needs a stream_url, a park_tuning, and a select_tuning"
        ));
    }
    match st
        .timelapse
        .start_segment(caps, rx, out_dir, real_segment_spawn())
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
    State(st): State<PrinterState>,
    Json(b): Json<TimelapseStartBody>,
) -> Response {
    // Validate the mode's cadence up front (before resolving cameras), so a bad
    // `every`/`interval_ms` is a clean 400 regardless of camera config.
    let mode = b.mode.as_deref().unwrap_or("smooth");
    let interval_ms = b.interval_ms.unwrap_or(3000);
    // Default: a full-layer safety cap (well above any real layer time). The native park is
    // a layer-change event, so the segment must span the WHOLE layer to contain it — the
    // next layer edge finalizes first; this only bites if layer_num stalls.
    let window_ms = b.window_ms.unwrap_or(120_000);
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
        "segment" => {
            // window_ms is the per-layer accumulation SAFETY CAP, not a gate — it must
            // comfortably exceed a layer's print time so the next layer edge finalizes first.
            if !(5_000..=600_000).contains(&window_ms) {
                return bad_request("window_ms must be between 5000 and 600000".to_string());
            }
        }
        other => {
            return bad_request(format!(
                "unknown mode {other:?} (use smooth, plain, park, or segment)"
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
    // Segment likewise reads the stream; it additionally needs select tuning + a window.
    if mode == "segment" {
        let out_dir = run_out_dir(&st, mode);
        let rx = st.source.subscribe();
        return start_segment_run(&st, &ids, window_ms, out_dir, rx);
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
        _ => {
            // Per-camera select tuning (index-aligned with `grabs`/`ids`): when present, the
            // smooth run live-selects the parked frame per layer so the dashboard's live park
            // preview advances during the capture and the finished run reads back clean.
            let externals = st.external_cameras.read().unwrap();
            let selects: Vec<Option<crate::core::park::SelectTuning>> = ids
                .iter()
                .map(|id| {
                    id.strip_prefix("ext-")
                        .and_then(|n| n.parse::<usize>().ok())
                        .and_then(|i| externals.get(i))
                        .and_then(|c| c.select_tuning)
                })
                .collect();
            drop(externals);
            st.timelapse.start_smooth_with_select(
                grabs,
                b.every,
                burst_offsets,
                rx,
                out_dir,
                selects,
            )
        }
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
    State(st): State<PrinterState>,
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
        "segment" => {
            st.timelapse.stop_segment();
        }
        "all" => {
            st.timelapse.stop_smooth();
            st.timelapse.stop_plain();
            st.timelapse.stop_park();
            st.timelapse.stop_segment();
        }
        other => {
            return bad_request(format!(
                "unknown mode {other:?} (use smooth, plain, park, segment, or all)"
            ));
        }
    }
    Json(timelapse_status_json(&st)).into_response()
}

/// Current capture status (open read).
async fn timelapse_status(State(st): State<PrinterState>) -> Json<serde_json::Value> {
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
    State(st): State<PrinterState>,
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
    State(st): State<PrinterState>,
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
    if let Err(resp) = stream_body_to(&tmp, body).await {
        return resp;
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
            "has_timelapse_blocks": inspection.as_ref().map(|i| i.has_timelapse_blocks),
            "overwrite": q.overwrite,
            "pre_print": st.hook.describe(),
        }}))
        .into_response();
    }
    // (confirm is guaranteed here — the early gate rejected !confirm && !dry_run,
    // and dry_run returned above.)

    let res = upload_and_start(
        &st,
        tmp.path(),
        UploadStart {
            dir: &dir,
            name: &q.name,
            remote: &remote,
            overwrite: q.overwrite,
            overwrite_hint: "add &overwrite=true",
            plate: q.plate,
            use_ams: false,
            ams_map: Vec::new(),
            bed_type,
            timelapse: q.timelapse,
            inspection,
        },
    )
    .await;
    drop(tmp); // remove the staged file once uploaded (or on error)
    res
}

/// Where a staged local file is going, and how to print it once it lands.
struct UploadStart<'a> {
    dir: &'a str,
    /// Bare filename on the printer, for the overwrite check.
    name: &'a str,
    /// `dir` + `name`: the absolute on-printer path.
    remote: &'a str,
    overwrite: bool,
    /// How *this* endpoint spells the overwrite flag, so the refusal tells the
    /// caller something it can actually act on.
    overwrite_hint: &'static str,
    plate: u32,
    use_ams: bool,
    ams_map: Vec<i32>,
    bed_type: String,
    timelapse: bool,
    inspection: Option<crate::core::project::PlateInspection>,
}

/// The device-verified tail of "put this local file on the printer and print
/// it": hold the start lock across both halves, refuse while busy, refuse to
/// clobber, FTPS-upload, then start.
///
/// Shared by `job/upload-start` and `slice/print` rather than reimplemented — so
/// a freshly sliced file never has to round-trip through the browser to reach
/// the printer, and every one of these guards applies to it verbatim.
async fn upload_and_start(
    st: &PrinterState,
    local: &std::path::Path,
    p: UploadStart<'_>,
) -> Response {
    // Hold the start lock across upload+start so two requests can't both pass idle.
    let Ok(_guard) = st.start_lock.try_lock() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "a print start is already in progress" })),
        )
            .into_response();
    };
    if let Some(busy) = require_idle(st) {
        return busy;
    }

    // Conservative overwrite guard (list the dir; a listing error doesn't block).
    if !p.overwrite {
        let files = st.files.clone();
        let dir_for_check = p.dir.to_string();
        let name = p.name.to_string();
        if let Ok(Ok(entries)) =
            tokio::task::spawn_blocking(move || files.list(&dir_for_check)).await
            && entries.iter().any(|e| e.name == name)
        {
            let (remote, hint) = (p.remote, p.overwrite_hint);
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": format!("{remote} already exists ({hint} to replace it)") })),
            )
                .into_response();
        }
    }

    // Upload the staged file, then start from its on-printer path.
    let files = st.files.clone();
    let path = local.to_path_buf();
    let remote_for_upload = p.remote.to_string();
    let up = tokio::task::spawn_blocking(move || files.upload(&remote_for_upload, &path)).await;
    match up {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response();
        }
        Err(_) => return server_error("upload task failed".to_string()),
    }

    // The upload above can take minutes. A job started from the printer's screen
    // in that window must not be driven into by a plate swap, so idle is checked
    // again before the hook rather than only before the upload.
    if let Some(busy) = require_idle(st) {
        return busy;
    }
    if let Some(failed) = run_pre_print_hook(st).await {
        return failed;
    }

    let req = StartRequest {
        file: p.remote.to_string(),
        plate: p.plate,
        use_ams: p.use_ams,
        ams_map: p.ams_map,
        bed_type: p.bed_type,
        timelapse: p.timelapse,
        inspection: p.inspection,
    };
    let starter = st.starter.clone();
    let res = tokio::task::spawn_blocking(move || starter.start(&req)).await;
    verify_response(res)
}

// ── Slicing ────────────────────────────────────────────────────────────────

/// Can this printer be sliced for right now? `Err(reason)` is handed to the
/// caller verbatim — it names the thing to install or the model we have no
/// mapping for.
fn slice_available(st: &PrinterState) -> Result<SlicerInfo, String> {
    // The machine profile decides the bed, the start gcode and the flow
    // calibration. Slicing an unmapped model against some other machine's
    // profile is exactly the failure this feature exists to prevent, so an
    // unmapped model is unavailable, never an A1-mini fallback.
    if st.slicer_names.is_none() {
        return Err(format!(
            "no verified slicer profile mapping for model {}",
            st.model.as_deref().unwrap_or("unknown")
        ));
    }
    st.slicer.info()
}

/// Whether slicing is available here, and what the slice slot is doing.
async fn slice_info(State(st): State<PrinterState>) -> Response {
    let (slicer, reason) = match slice_available(&st) {
        Ok(info) => (
            Some(json!({ "kind": info.kind, "thumbnails": info.thumbnails })),
            None,
        ),
        Err(reason) => (None, Some(reason)),
    };
    Json(json!({
        "available": slicer.is_some(),
        "slicer": slicer,
        "reason": reason,
        "machine": st.slicer_names.map(|n| n.machine_base),
        "job": st.slice_jobs.status().to_json(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct SliceQuery {
    name: Option<String>,
    layer: Option<f64>,
    filament: Option<String>,
    bed_type: Option<String>,
    brim: Option<f64>,
    nozzle: Option<String>,
}

/// Slice a model (write). The body is the raw model bytes, streamed to a temp
/// file exactly like an upload; the answer is 202 + the job, which is then
/// polled at `GET /slice`.
///
/// `layer`, `filament` and `bed_type` are **required with no server default**:
/// the first two depend on the user's intent and the spool actually loaded, and
/// defaulting the third is precisely the Cool-Plate-at-35°C silent wrong guess.
/// The slicer binary and profile paths, by contrast, are discoverable facts, so
/// those are auto-detected.
async fn slice_start(
    State(st): State<PrinterState>,
    Query(q): Query<SliceQuery>,
    body: Body,
) -> Response {
    let name = q.name.unwrap_or_default();
    // Same filename guard as an upload — `name` is a single filename.
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return bad_request(format!("invalid filename {name:?}"));
    }
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".gcode.3mf") {
        return bad_request(
            "that is already a sliced .gcode.3mf — send it to the printer with /job/upload-start"
                .to_string(),
        );
    }
    // Exactly what the slicer reads. It rejects anything else before opening
    // the file — "Unknown file format. Input file must have .stl, .obj,
    // .amf(.xml) extension" — so accepting `.step` here would return 202 and
    // then fail in the background, which is a worse answer than refusing.
    // (Verified against the installed OrcaSlicer; `.3mf` gets past the format
    // check on its own path.)
    if !crate::core::slice::MODEL_EXTENSIONS
        .iter()
        .any(|e| lower.ends_with(e))
    {
        return bad_request(format!(
            "{name:?} is not a model file this slicer reads: expected .stl, .obj, .amf or .3mf \
             (STEP is not supported — convert it first)"
        ));
    }

    let Some(layer) = q.layer else {
        return bad_request(
            "layer required: the layer height in mm, e.g. &layer=0.2 (there is no default — it \
             is your call, not the server's)"
                .to_string(),
        );
    };
    let Some(filament) = q.filament.filter(|f| !f.trim().is_empty()) else {
        return bad_request(
            "filament required: a full profile name, e.g. &filament=Bambu PLA Basic @BBL A1M"
                .to_string(),
        );
    };
    if !crate::core::slice::is_safe_profile_name(&filament) {
        return bad_request(format!("invalid filament profile name {filament:?}"));
    }
    let Some(bed_type) = q.bed_type.filter(|b| !b.trim().is_empty()) else {
        return bad_request(
            "bed_type required: the plate actually on the printer, e.g. &bed_type=Textured PEI \
             Plate — it sets the bed temperature, and guessing it means a cold plate and a print \
             that lifts"
                .to_string(),
        );
    };
    if !crate::core::slice::is_safe_profile_name(&bed_type) {
        return bad_request(format!("invalid bed_type {bed_type:?}"));
    }
    if let Some(brim) = q.brim
        && !(0.0..=20.0).contains(&brim)
    {
        return bad_request(format!("brim={brim} is outside 0–20 mm"));
    }

    if let Err(reason) = slice_available(&st) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": reason })),
        )
            .into_response();
    }
    let names = st
        .slicer_names
        .expect("availability already proved there is a mapping");

    // The nozzle picks the machine preset. Prefer what the printer reports over
    // what the caller assumed; refuse rather than assume 0.4.
    let Some(nozzle) = st
        .source
        .current()
        .nozzle_diameter
        .or_else(|| q.nozzle.filter(|n| !n.trim().is_empty()))
    else {
        return bad_request("nozzle unknown — pass &nozzle=0.4".to_string());
    };
    let machine_profile = names.machine_profile(&nozzle);
    if !crate::core::slice::is_safe_profile_name(&machine_profile) {
        return bad_request(format!("invalid nozzle {nozzle:?}"));
    }
    // The process presets are per-nozzle, and only the 0.4 set is mapped. A
    // 0.6 machine would otherwise slice with 0.4 speeds and flow: a success
    // that is wrong, which is worse than this refusal.
    let preset_suffix = match names.process_suffix(&nozzle) {
        Ok(s) => s,
        Err(why) => return bad_request(why),
    };
    // Judged against the nozzle actually fitted, and only once that is known: a
    // fixed 0.04–0.4 window accepts 0.4 mm on a 0.2 nozzle and refuses legal
    // heights on a 0.6, while naming a nozzle the machine doesn't have.
    if let Err(why) = crate::core::slice::layer_fits_nozzle(layer, &nozzle) {
        return bad_request(why);
    }

    // Claimed BEFORE the body is streamed, not merely checked. A check would
    // let every concurrent caller through to write its own copy of a body worth
    // up to 512 MiB and race for the slot afterwards, so one-slice-at-a-time
    // would bound the slicer and not the disk. The guard releases on drop,
    // including when the client disconnects mid-upload and axum drops this
    // future.
    let Some(slot) = st.slice_jobs.reserve() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "a slice is already running" })),
        )
            .into_response();
    };

    // The suffix is not cosmetic: libslic3r picks its model reader from the
    // extension, so an extensionless staged file is rejected before it is even
    // read — "Unknown file format. Input file must have .stl, .obj, .amf(.xml)
    // extension" — and every real slice fails. Verified against the installed
    // slicer with byte-identical files that differed only in name.
    let suffix = std::path::Path::new(&name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_ascii_lowercase()))
        .unwrap_or_default();
    let tmp = match tempfile::Builder::new()
        .prefix("bambu-model-")
        .suffix(&suffix)
        .tempfile()
    {
        Ok(t) => t,
        Err(e) => return server_error(e.to_string()),
    };
    let written = match stream_body_to(&tmp, body).await {
        Ok(n) => n,
        Err(resp) => return resp,
    };
    if written == 0 {
        return bad_request("empty body: POST the model file's bytes".to_string());
    }

    // An authored project 3mf carries its own plates, orientations and
    // per-object overrides, and the slicer is driven with `--arrange 1 --orient
    // 1`, which re-packs all of it. Refuse instead of quietly ruining it.
    if lower.ends_with(".3mf") {
        // Off the async workers: walking a zip is blocking. The file is handed
        // over rather than its bytes — a zip answers this from its central
        // directory, and slurping a 512 MiB upload into RAM (times however many
        // arrive before one wins the slot) is how the server gets OOM-killed.
        let staged = tmp.path().to_path_buf();
        match tokio::task::spawn_blocking(move || {
            std::fs::File::open(&staged)
                .map_err(|e| e.to_string())
                .and_then(|f| {
                    crate::core::project::is_authored_project(std::io::BufReader::new(f))
                        .map_err(|e| e.to_string())
                })
        })
        .await
        .unwrap_or_else(|e| Err(format!("3mf inspection task failed: {e}")))
        {
            Ok(true) => {
                return bad_request(
                    "this is an authored project 3mf; slicing it here would re-arrange and \
                     re-orient its plates — slice it with its own settings instead"
                        .to_string(),
                );
            }
            Ok(false) => {}
            Err(e) => return bad_request(format!("3mf inspection: {e}")),
        }
    }

    let workdir = match tempfile::Builder::new().prefix("bambu-slice-").tempdir() {
        Ok(d) => d,
        Err(e) => return server_error(e.to_string()),
    };
    let params = SliceParams {
        input: tmp.path().to_path_buf(),
        input_name: name.clone(),
        out_dir: workdir.path().to_path_buf(),
        out_name: crate::core::slice::sanitize_output_name(&name),
        machine_profile,
        preset_suffix: preset_suffix.to_string(),
        layer_mm: layer,
        filament,
        bed_type,
        brim_mm: q.brim,
    };
    match st.slice_jobs.start(st.slicer.clone(), params, tmp, workdir) {
        Ok(()) => {
            // The running status keeps the next caller out from here on, so the
            // claim is spent rather than released.
            slot.commit();
            (
                StatusCode::ACCEPTED,
                Json(json!({ "job": st.slice_jobs.status().to_json() })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::CONFLICT, Json(json!({ "error": e }))).into_response(),
    }
}

/// Stream a request body into an already-created temp file, returning the byte
/// count. `DefaultBodyLimit` does not bound a raw `Body` consumed by hand, so
/// the cap is counted here.
async fn stream_body_to(tmp: &tempfile::NamedTempFile, body: Body) -> Result<u64, Response> {
    let mut file = tokio::fs::File::create(tmp.path())
        .await
        .map_err(|e| server_error(e.to_string()))?;
    let mut stream = body.into_data_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| bad_request("upload stream error".to_string()))?;
        written += chunk.len() as u64;
        if written > MAX_UPLOAD_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({ "error": "upload exceeds the 512 MiB limit" })),
            )
                .into_response());
        }
        file.write_all(&chunk)
            .await
            .map_err(|_| server_error("writing upload".to_string()))?;
    }
    file.flush()
        .await
        .map_err(|_| server_error("flushing upload".to_string()))?;
    Ok(written)
}

/// Download the finished `.gcode.3mf`.
async fn slice_result(State(st): State<PrinterState>) -> Response {
    let status = st.slice_jobs.status();
    if status.state == "running" {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "slice still running" })),
        )
            .into_response();
    }
    let Some(done) = st.slice_jobs.result() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no finished slice", "job": status.to_json() })),
        )
            .into_response();
    };
    // Streamed, not buffered: a plate's gcode for a big print is tens of MB,
    // and the POST that made it accepts up to 512 MiB. Same shape as
    // `capture_video`, and for the same reason.
    let Ok(file) = tokio::fs::File::open(&done.path).await else {
        return server_error("the sliced file has gone".to_string());
    };
    // `out_name` came from `sanitize_output_name`, so it carries no quote or
    // newline to break out of this header.
    let name = done
        .status
        .out_name
        .clone()
        .unwrap_or_else(|| "slice.gcode.3mf".to_string());
    // The lease rides along in the stream state, so the work directory outlives
    // the download rather than this function. Dropping it here would be
    // invisible on Linux (an open fd keeps reading through an unlink) and a
    // leak on Windows, where `TempDir` cannot remove a directory holding an
    // open file and does not retry — the bounded-disk claim has to hold on
    // both.
    let stream = futures_util::stream::unfold(Some((file, done)), |st| async move {
        let (mut f, lease) = st?;
        let mut buf = vec![0u8; 64 * 1024];
        match tokio::io::AsyncReadExt::read(&mut f, &mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((
                    Ok::<Bytes, std::io::Error>(Bytes::from(buf)),
                    Some((f, lease)),
                ))
            }
            Err(e) => Some((Err(e), None)),
        }
    });
    (
        [
            (CONTENT_TYPE, "application/octet-stream".to_string()),
            // Same URL, different file after the next slice — and the file is
            // the caller's own geometry. A cached copy would hand back the
            // previous job's output, or keep this one on disk after it is gone.
            (CACHE_CONTROL, "no-store".to_string()),
            (
                CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

#[derive(Deserialize)]
struct SlicePrintBody {
    #[serde(default = "default_plate")]
    plate: u32,
    #[serde(default)]
    timelapse: bool,
    bed_type: Option<String>,
    #[serde(default)]
    use_ams: bool,
    #[serde(default)]
    ams_map: Vec<i32>,
    dir: Option<String>,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    dry_run: bool,
    /// Which slice this print is for — the `id` the dry run reported.
    ///
    /// Required to confirm, because the slot holds exactly one result: another
    /// caller can slice between the plan and the confirmation, and without this
    /// the confirmation would silently print the replacement. Same shape as the
    /// CLI's `--expect-md5`, and the reason is the same one.
    expect: Option<u64>,
}

impl Default for SlicePrintBody {
    fn default() -> Self {
        Self {
            plate: default_plate(),
            timelapse: false,
            bed_type: None,
            use_ams: false,
            ams_map: Vec::new(),
            dir: None,
            overwrite: false,
            confirm: false,
            dry_run: false,
            expect: None,
        }
    }
}

/// Print the slice that is sitting in the slot: upload it over FTPS and start,
/// through the same [`upload_and_start`] the dashboard's upload path uses.
async fn slice_print(
    State(st): State<PrinterState>,
    body: Option<Json<SlicePrintBody>>,
) -> Response {
    let b = body.map_or_else(SlicePrintBody::default, |Json(b)| b);
    if st.slice_jobs.status().state == "running" {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "slice still running" })),
        )
            .into_response();
    }
    // `path` lives in the slot's working directory, which a NEW slice would drop
    // out from under this upload. That costs at most a 502 on this request: the
    // replacement job gets a fresh directory, so this path can only ever name
    // this slice's own output — never someone else's file.
    let Some(done) = st.slice_jobs.result() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no finished slice to print — POST /api/slice first" })),
        )
            .into_response();
    };
    if b.use_ams {
        for (i, v) in b.ams_map.iter().enumerate() {
            if !(-1..=3).contains(v) {
                return bad_request(format!(
                    "ams_map[{i}]={v} out of range (trays 0..3, or -1 external)"
                ));
            }
        }
        // Length matters as much as range. This path slices ONE filament, and
        // the mapping is expanded onto the project's filament indices — a
        // mismatched length is left unexpanded, so the printer falls back to
        // whatever the gcode baked in. That is how a plate got printed in the
        // wrong material once already; it is a refusal now, not a surprise.
        if b.ams_map.len() != 1 {
            return bad_request(format!(
                "ams_map has {} entries but this slice uses 1 filament — pass exactly one tray",
                b.ams_map.len()
            ));
        }
    }
    // The A1 mini prints from `/`; a start that reads an uploaded file out of
    // `/cache` fails with 0x0500C010 (verified).
    let dir = b.dir.clone().unwrap_or_else(|| "/".to_string());
    if dir != "/" && !is_safe_remote_path(&dir) {
        return bad_request(format!("invalid dir {dir:?}"));
    }
    let name = done
        .status
        .out_name
        .clone()
        .unwrap_or_else(|| "slice.gcode.3mf".to_string());
    let remote = format!("{}/{}", dir.trim_end_matches('/'), name);

    // Inspect the local result so the plate-gcode md5 is stamped into the start
    // command and the printer verifies the file it is about to run. Off the
    // async workers: a sliced plate is tens of MB and this both reads it and
    // walks a zip, which would block a runtime thread for the whole of it.
    let path_for_inspect = done.path.clone();
    let plate = b.plate;
    let inspection = match tokio::task::spawn_blocking(move || {
        // Opened, not read: the slicer's output has no archive-size bound, and
        // several print requests reach here before any of them takes the start
        // lock. The entries this pulls out are individually capped.
        std::fs::File::open(&path_for_inspect)
            .map_err(|e| e.to_string())
            .and_then(|f| {
                crate::core::project::inspect_plate_from(std::io::BufReader::new(f), plate)
                    .map_err(|e| e.to_string())
            })
    })
    .await
    {
        Ok(Ok(insp)) => insp,
        Ok(Err(e)) => return bad_request(format!("3mf inspection: {e}")),
        Err(_) => return server_error("3mf inspection task failed".to_string()),
    };
    let bed_type = b.bed_type.clone().unwrap_or_else(|| "auto".to_string());

    if b.dry_run {
        return Json(json!({ "plan": {
            "file": remote,
            "plate": b.plate,
            "use_ams": b.use_ams,
            "ams_map": b.ams_map,
            "bed_type": bed_type,
            "timelapse": b.timelapse,
            "md5": inspection.gcode_md5,
            "has_timelapse_blocks": inspection.has_timelapse_blocks,
            "overwrite": b.overwrite,
            // What to echo back to print exactly this slice.
            "expect": done.status.id,
        }}))
        .into_response();
    }
    if let Some(unconfirmed) = need_confirm(b.confirm) {
        return unconfirmed;
    }
    // The plan above described one particular slice, and the slot holds one at a
    // time. Between that plan and this confirmation another caller can slice —
    // legally, the slot was free — and everything named here (`remote`, `name`,
    // the md5 just computed) would then describe the replacement. Confirming a
    // print of a file nobody looked at is the same class of mistake as the AMS
    // mapping ones, so it is proven rather than assumed.
    match b.expect {
        Some(want) if Some(want) == done.status.id => {}
        Some(want) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "slice {want} is no longer the one loaded (the slot now holds {}) — \
                         dry-run again and confirm against that",
                        done.status.id.map_or_else(|| "none".to_string(), |v| v.to_string())
                    ),
                })),
            )
                .into_response();
        }
        None => {
            return bad_request(format!(
                "confirming a print needs \"expect\": <the dry run's slice id> — this slice is {}",
                done.status
                    .id
                    .map_or_else(|| "unknown".to_string(), |v| v.to_string())
            ));
        }
    }

    upload_and_start(
        &st,
        &done.path,
        UploadStart {
            dir: &dir,
            name: &name,
            remote: &remote,
            overwrite: b.overwrite,
            overwrite_hint: "pass \"overwrite\": true",
            plate: b.plate,
            use_ams: b.use_ams,
            ams_map: b.ams_map.clone(),
            bed_type,
            timelapse: b.timelapse,
            inspection: Some(inspection),
        },
    )
    .await
}

/// Upgrade to a WebSocket that pushes a `PrinterStatus` JSON frame on connect and
/// on every subsequent change.
async fn status_ws(State(st): State<PrinterState>, ws: WebSocketUpgrade) -> Response {
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
async fn require_password(State(st): State<PrinterState>, req: Request, next: Next) -> Response {
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
    use super::super::hook::fake::RecordingHook;
    use super::*;
    use crate::core::session::VerifyStage;
    use axum_test::TestServer;

    /// Serve one printer as the whole server. It is the default, so the
    /// unprefixed `/api/...` paths these tests use reach it — which is also the
    /// back-compat guarantee the router makes to everything already written.
    fn one(state: PrinterState) -> Router {
        let id = state.id.clone();
        router(ServerState {
            printers: Arc::new(BTreeMap::from([(id.clone(), state)])),
            default: id,
        })
    }

    /// Build a test server with a chosen password + controller (idle source).
    fn app(password: Option<&str>, controller: impl Controller + 'static) -> TestServer {
        let state = PrinterState {
            name: "test".to_string(),
            id: "test".to_string(),
            model: None,
            legacy_captures: true,
            source: Arc::new(FakeSource::idle()),
            controller: Arc::new(controller),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: password.map(str::to_owned),
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
            hook_running: Default::default(),
            hook: Arc::new(NoHook),
            slicer: Arc::new(FakeSlicer),
            slice_jobs: Default::default(),
            slicer_names: crate::core::capability::default_registry()
                .slicer_names(&crate::core::model::Model::A1Mini)
                .copied(),
        };
        TestServer::new(one(state))
    }

    /// A named printer with its own everything, for the multi-printer tests.
    fn printer(name: &str) -> PrinterState {
        PrinterState {
            name: name.to_string(),
            id: name.to_string(),
            model: None,
            legacy_captures: false,
            source: Arc::new(FakeSource::idle()),
            controller: Arc::new(FakeController::verified()),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: None,
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
            hook_running: Default::default(),
            hook: Arc::new(NoHook),
            slicer: Arc::new(FakeSlicer),
            slice_jobs: Default::default(),
            slicer_names: crate::core::capability::default_registry()
                .slicer_names(&crate::core::model::Model::A1Mini)
                .copied(),
        }
    }

    /// Serve several printers; the first is the default.
    fn serve_printers(states: Vec<PrinterState>) -> TestServer {
        let default = states[0].id.clone();
        let map = states
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect::<BTreeMap<_, _>>();
        TestServer::new(router(ServerState {
            printers: Arc::new(map),
            default,
        }))
    }

    // ── several printers at once ──
    #[tokio::test]
    async fn every_printer_answers_on_its_own_path_and_the_default_also_answers_unprefixed() {
        let server = serve_printers(vec![printer("a1mini"), printer("x1c")]);
        for path in [
            "/api/status",
            "/api/printers/a1mini/status",
            "/api/printers/x1c/status",
        ] {
            server.get(path).await.assert_status_ok();
        }
        // The unprefixed paths are what every existing script, skill and
        // `--via-serve` call already targets; they must keep working, and keep
        // reaching the SAME machine they always did.
        server
            .get("/api/printers/nope/status")
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_printer_list_carries_each_ones_status_so_an_overview_is_one_request() {
        let server = serve_printers(vec![printer("a1mini"), printer("x1c")]);
        let body: serde_json::Value = server.get("/api/printers").await.json();
        assert_eq!(body["default"], "a1mini");
        let list = body["printers"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["name"], "a1mini");
        assert_eq!(list[0]["default"], json!(true));
        assert_eq!(list[1]["default"], json!(false));
        // Watching several machines is the reason to run one server for them;
        // a status per printer here is what keeps that a single request taken
        // at a single instant, instead of N round trips at N different ones.
        assert!(
            list.iter().all(|p| p["status"].is_object()),
            "each entry carries its printer's current status: {list:?}"
        );
    }

    #[tokio::test]
    async fn a_timelapse_on_one_printer_does_not_occupy_another_printers_slot() {
        // The manager allows one run per mode, and it used to be one per SERVER
        // — so a second printer could not record while the first was recording.
        // Started through the manager rather than the HTTP route because every
        // route-level rejection (cadence, unknown camera) happens BEFORE the
        // manager is touched, and would pass just as well with one shared slot.
        let (a, b) = (printer("a1mini"), printer("x1c"));
        let a_tl = a.timelapse.clone();
        assert!(
            !Arc::ptr_eq(&a.timelapse, &b.timelapse),
            "each printer owns its manager"
        );
        let server = serve_printers(vec![a, b]);

        let dir = std::env::temp_dir().join(format!("bambu-mp-tl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let grab: crate::server::timelapse::FrameGrab =
            Arc::new(|| Ok(vec![0xff, 0xd8, 0xff, 0x42]));
        let (_tx, rx) = tokio::sync::watch::channel(PrinterStatus::default());
        a_tl.start_plain(
            vec![crate::server::timelapse::PlainCapture::Sample {
                id: "ext-0".to_string(),
                grab,
            }],
            50,
            rx,
            dir.clone(),
        )
        .unwrap();

        let a_running = server
            .get("/api/printers/a1mini/timelapse")
            .await
            .json::<serde_json::Value>()["running"]
            .clone();
        let b_running = server
            .get("/api/printers/x1c/timelapse")
            .await
            .json::<serde_json::Value>()["running"]
            .clone();
        assert_eq!(a_running, json!(true), "a1mini is recording");
        assert_eq!(
            b_running,
            json!(false),
            "x1c's slot is its own and still free"
        );
        a_tl.stop_plain();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_print_start_in_flight_on_one_printer_does_not_block_another() {
        // `start_lock` used to be process-wide, so starting a print on one
        // printer answered "a print start is already in progress" for every
        // other one.
        let (a, b) = (printer("a1mini"), printer("x1c"));
        let a_lock = a.start_lock.clone();
        let server = serve_printers(vec![a, b]);
        let body = json!({ "file": "/cache/x.gcode.3mf", "confirm": true });

        // Hold a1mini's lock the way a start in flight would.
        let held = a_lock.lock().await;
        server
            .post("/api/printers/a1mini/job/start")
            .json(&body)
            .await
            .assert_status(StatusCode::CONFLICT);
        server
            .post("/api/printers/x1c/job/start")
            .json(&body)
            .await
            .assert_status_ok();
        drop(held);
        // …and a1mini is startable again once its own start finishes.
        server
            .post("/api/printers/a1mini/job/start")
            .json(&body)
            .await
            .assert_status_ok();
    }

    #[test]
    fn an_ordinary_profile_name_is_its_own_identifier() {
        // Nothing to learn in the common case: the URL says what the config says.
        for name in ["a1mini", "x1-carbon", "shop_2", "p1s.spare", "A1"] {
            assert_eq!(printer_id(name), name);
        }
    }

    #[test]
    fn a_free_form_profile_name_still_yields_a_safe_identifier() {
        // `config add` has always stored whatever was typed, so configs like
        // "Shop A" exist and must keep serving — sanitised, not rejected.
        assert_eq!(printer_id("Shop A"), "Shop-A");
        assert_eq!(printer_id("a/b"), "a-b");
        assert_eq!(printer_id("{name}"), "name");
        assert_eq!(printer_id(".hidden"), "hidden");
        // Windows reserves these as devices, stem and all, so `captures/con`
        // cannot exist there. An underscore keeps the name recognisable and
        // keeps two such names distinct.
        for dev in ["con", "CON", "nul", "com1", "LPT9", "aux", "con.txt"] {
            let id = printer_id(dev);
            assert!(is_safe_printer_id(&id), "{dev:?} -> {id:?}");
        }
        assert_eq!(printer_id("con"), "con_");
        assert_eq!(printer_id("nul"), "nul_");
        assert_ne!(printer_id("con"), printer_id("nul"), "kept distinct");
        assert_eq!(printer_id("console"), "console", "only the exact stem");
        assert_eq!(printer_id("com0"), "com0", "COM0 is not reserved");
        for empty in ["..", "プリンタ", "", "-..", "..-", "---", "."] {
            assert_eq!(printer_id(empty), "printer", "{empty:?}");
        }
    }

    #[test]
    fn no_profile_name_can_produce_an_unsafe_identifier() {
        // The property, not a list of spellings: an enumerated test passed while
        // `printer_id("-..")` returned "..", because the leading-dot trim ran
        // before the hyphen trim and nothing checked the answer. `captures/..`
        // then writes outside the printer's namespace.
        let alphabet = ['-', '.', 'c', 'o', 'n', '/', '{', ' '];
        let mut checked = 0;
        for a in alphabet {
            for b in alphabet {
                for c in alphabet {
                    for d in alphabet {
                        let name: String = [a, b, c, d].iter().collect();
                        let id = printer_id(&name);
                        assert!(
                            is_safe_printer_id(&id),
                            "{name:?} produced an unsafe id {id:?}"
                        );
                        // The two shapes that actually escape or hide.
                        assert!(!id.starts_with('.'), "{name:?} -> {id:?}");
                        assert!(!id.ends_with('.'), "{name:?} -> {id:?} (Windows drops it)");
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, alphabet.len().pow(4));
    }

    #[test]
    fn identifiers_that_differ_only_in_case_are_treated_as_one() {
        // This crate ships macOS and Windows builds, where `captures/A1` and
        // `captures/a1` are the same directory: two printers' recordings would
        // land together despite the per-printer split.
        assert_eq!(
            printer_id_key(&printer_id("A1")),
            printer_id_key(&printer_id("a1"))
        );
        assert_ne!(printer_id_key("a1mini"), printer_id_key("x1c"));
    }

    #[test]
    fn only_the_default_printer_inherits_the_pre_namespacing_captures() {
        // Every printer writes under its own name; the runs from before the
        // split belong to one of them, not to all.
        let (mut d, other) = (printer("a1mini"), printer("x1c"));
        d.legacy_captures = true;
        assert_eq!(captures_root(&d), std::path::Path::new("captures/a1mini"));
        assert_eq!(captures_root(&other), std::path::Path::new("captures/x1c"));
        // `no-such-run` exists under neither, so this exercises the fallback
        // branch rather than the on-disk one.
        assert_eq!(
            capture_run_dir(&d, "no-such-run"),
            std::path::Path::new("captures/no-such-run"),
            "the default printer can still play a run recorded before the split"
        );
        assert_eq!(
            capture_run_dir(&other, "no-such-run"),
            std::path::Path::new("captures/x1c/no-such-run"),
            "another printer must never resolve into the legacy root"
        );
    }

    // ── the pre-print hook ──
    fn with_hook(hook: Arc<dyn PrePrintHook>) -> (TestServer, Arc<dyn PrePrintHook>) {
        let mut st = printer("a1mini");
        st.hook = hook.clone();
        (TestServer::new(one(st)), hook)
    }

    fn start_body() -> serde_json::Value {
        json!({ "file": "/cache/x.gcode.3mf", "confirm": true })
    }

    #[tokio::test]
    async fn a_confirmed_start_runs_the_hook_first() {
        let hook = Arc::new(RecordingHook::new("swap"));
        let (server, _) = with_hook(hook.clone());
        server
            .post("/api/job/start")
            .json(&start_body())
            .await
            .assert_status_ok();
        assert_eq!(hook.runs(), 1, "the swap ran before the print started");
    }

    #[tokio::test]
    async fn a_hook_that_fails_stops_the_print() {
        // The whole point: a swap that didn't finish means the machine may
        // still be moving, and starting into it is the collision this prevents.
        let hook = Arc::new(RecordingHook::failing("swap", "the motion never finished"));
        let (server, _) = with_hook(hook.clone());
        let res = server.post("/api/job/start").json(&start_body()).await;
        res.assert_status(StatusCode::CONFLICT);
        assert!(
            res.text().contains("print not started"),
            "the response says the print did NOT start: {}",
            res.text()
        );
        assert_eq!(hook.runs(), 1);
    }

    #[tokio::test]
    async fn a_dry_run_discloses_the_hook_and_runs_nothing() {
        // Motion the preview hides is motion the operator finds out about by
        // watching it happen.
        let hook = Arc::new(RecordingHook::new("swap"));
        let (server, _) = with_hook(hook.clone());
        let body: serde_json::Value = server
            .post("/api/job/start")
            .json(&json!({ "file": "/cache/x.gcode.3mf", "dry_run": true }))
            .await
            .json();
        assert_eq!(body["plan"]["pre_print"], json!("swap"));
        assert_eq!(hook.runs(), 0, "a dry run sends nothing");
    }

    #[tokio::test]
    async fn a_printer_without_a_hook_starts_exactly_as_before() {
        let server = serve_printers(vec![printer("a1mini")]);
        let body: serde_json::Value = server
            .post("/api/job/start")
            .json(&json!({ "file": "/cache/x.gcode.3mf", "dry_run": true }))
            .await
            .json();
        assert_eq!(body["plan"]["pre_print"], json!(null));
        server
            .post("/api/job/start")
            .json(&start_body())
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn the_upload_path_runs_the_hook_too() {
        // Its own ordering and its own failure handling, so its own tests: this
        // branch can regress independently of the plain start.
        let hook = Arc::new(RecordingHook::new("swap"));
        let (server, _) = with_hook(hook.clone());
        server
            .post("/api/job/upload-start?name=x.gcode&confirm=true")
            .bytes(b"G28\n".to_vec().into())
            .await
            .assert_status_ok();
        assert_eq!(hook.runs(), 1, "the swap ran before the print started");
    }

    #[tokio::test]
    async fn a_failed_hook_stops_the_upload_path_after_the_upload() {
        // The file is already on the printer by then — that is fine and
        // deliberate — but the PRINT must not start.
        let hook = Arc::new(RecordingHook::failing("swap", "the motion never finished"));
        let (server, _) = with_hook(hook.clone());
        let res = server
            .post("/api/job/upload-start?name=x.gcode&confirm=true")
            .bytes(b"G28\n".to_vec().into())
            .await;
        res.assert_status(StatusCode::CONFLICT);
        assert!(res.text().contains("print not started"), "{}", res.text());
        assert_eq!(hook.runs(), 1);
    }

    #[tokio::test]
    async fn the_upload_paths_dry_run_discloses_the_hook_and_runs_nothing() {
        let hook = Arc::new(RecordingHook::new("swap"));
        let (server, _) = with_hook(hook.clone());
        let v: serde_json::Value = server
            .post("/api/job/upload-start?name=x.gcode&dry_run=true")
            .bytes(b"G28\n".to_vec().into())
            .await
            .json();
        assert_eq!(v["plan"]["pre_print"], json!("swap"));
        assert_eq!(hook.runs(), 0);
    }

    #[tokio::test]
    async fn a_hooks_own_failures_are_classified_for_the_operator() {
        // Which one to go and fix differs: a bad sequence file is the
        // operator's configuration, a refusal or an unfinished motion is the
        // machine, and an unreachable printer is neither.
        for (err, want) in [
            (
                HookError::Config("swap.gcode has no G-code".to_string()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                HookError::Printer("the motion never finished".to_string()),
                StatusCode::CONFLICT,
            ),
            (
                HookError::Transport("connection refused".to_string()),
                StatusCode::BAD_GATEWAY,
            ),
        ] {
            let (server, _) = with_hook(Arc::new(RecordingHook::erroring("swap", err)));
            let res = server.post("/api/job/start").json(&start_body()).await;
            res.assert_status(want);
            assert!(res.text().contains("print not started"), "{}", res.text());
        }
    }

    #[tokio::test]
    async fn a_running_hook_refuses_a_jog_but_never_a_stop() {
        // `start_lock` excludes other STARTS. A jog is not a start, so without
        // this the dashboard could drive the head into a plate changer
        // mid-swing. Stopping stays available on purpose: a gate that removes
        // the emergency controls during the one operation most likely to need
        // them is worse than the interleaving it prevents.
        let st = printer("a1mini");
        let running = st.hook_running.clone();
        let server = serve_printers(vec![st]);
        running.store(true, std::sync::atomic::Ordering::SeqCst);

        for (path, body) in [
            ("/api/home", json!({ "axes": "all" })),
            ("/api/move", json!({ "axis": "z", "delta": 1.0 })),
            ("/api/gcode", json!({ "line": "G28", "confirm": true })),
            // (`/extrude` is gated too, but its cold-nozzle validation answers
            // first — this codebase validates before it gates, everywhere.)
            (
                "/api/calibrate",
                json!({ "bed_level": true, "confirm": true }),
            ),
            ("/api/steppers", json!({ "confirm": true })),
        ] {
            let res = server.post(path).json(&body).await;
            res.assert_status(StatusCode::CONFLICT);
            assert!(
                res.text().contains("pre-print sequence is running"),
                "{path}: {}",
                res.text()
            );
        }
        // …and the ones that make a runaway stoppable are untouched.
        for path in ["/api/job/pause", "/api/job/stop", "/api/job/clear-error"] {
            server
                .post(path)
                .json(&json!({ "confirm": true }))
                .await
                .assert_status_ok();
        }
        running.store(false, std::sync::atomic::Ordering::SeqCst);
        server
            .post("/api/home")
            .json(&json!({ "axes": "all" }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn each_printer_runs_only_its_own_hook() {
        // One machine has a plate changer and the other doesn't; starting on
        // the plain one must not swing the other one's changer.
        let (swap, plain) = (
            Arc::new(RecordingHook::new("swap")),
            Arc::new(RecordingHook::new("other")),
        );
        let (mut a, mut b) = (printer("a1mini"), printer("x1c"));
        a.hook = swap.clone();
        b.hook = plain.clone();
        let server = serve_printers(vec![a, b]);
        server
            .post("/api/printers/x1c/job/start")
            .json(&start_body())
            .await
            .assert_status_ok();
        assert_eq!((swap.runs(), plain.runs()), (0, 1));
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
            .get("/api/file")
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
            .get("/api/file/thumbnail?name=coin2c.gcode.3mf")
            .await;
        res.assert_status_ok();
        assert_eq!(res.header("content-type"), "image/png");
    }

    #[tokio::test]
    async fn raw_serves_3mf_bytes() {
        let res = app(None, FakeController::verified())
            .get("/api/file/raw?name=/cache/coin.gcode.3mf")
            .await;
        res.assert_status_ok();
        assert_eq!(res.header("content-type"), "application/octet-stream");
    }

    #[tokio::test]
    async fn raw_rejects_other_extensions() {
        app(None, FakeController::verified())
            .get("/api/file/raw?name=/secret.txt")
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn gcode_file_serves_plate_toolpath() {
        let res = app(None, FakeController::verified())
            .get("/api/file/gcode?name=/coin2c.gcode.3mf&plate=1")
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
            .get("/api/file/gcode?name=/raw.gcode")
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mesh_file_serves_object_models() {
        let res = app(None, FakeController::verified())
            .get("/api/file/mesh?name=/coin2c.gcode.3mf")
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
            .get("/api/file/mesh?name=/raw.gcode")
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    // ── cameras (built-in + external proxies, listed as switchable sources) ──
    #[tokio::test]
    async fn cameras_list_is_empty_without_built_in_or_external() {
        // Fake/test mode has no built-in camera and no external URLs.
        let res = app(None, FakeController::verified())
            .get("/api/camera")
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
                .get(&format!("/api/camera/{id}/snapshot"))
                .await
                .assert_status(StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn external_cameras_can_be_set_then_listed_and_cleared() {
        let server = app(None, FakeController::verified());
        // Configure two external cameras (one labelled, one auto-labelled).
        let res = server
            .post("/api/camera/config")
            .json(&json!({
                "external": [
                    { "label": "front", "url": "http://cam.local/a.jpg" },
                    { "url": "http://cam.local/b.jpg" }
                ]
            }))
            .await;
        res.assert_status_ok();
        // The open listing now shows both, with ids and labels but no URLs.
        let list = server.get("/api/camera").await.json::<serde_json::Value>();
        let cams = list["cameras"].as_array().unwrap();
        assert_eq!(cams.len(), 2);
        assert_eq!(cams[0]["id"], "ext-0");
        assert_eq!(cams[0]["label"], "front");
        assert_eq!(cams[0]["kind"], "external");
        assert_eq!(cams[1]["label"], "external 2"); // auto-labelled
        assert!(cams[0].get("url").is_none()); // URL never exposed on the open list
        // The gated config read echoes URLs back for the manage form.
        let cfg = server
            .get("/api/camera/config")
            .await
            .json::<serde_json::Value>();
        assert_eq!(cfg["external"][0]["url"], "http://cam.local/a.jpg");
        // Replacing with an empty list clears them.
        server
            .post("/api/camera/config")
            .json(&json!({ "external": [] }))
            .await
            .assert_status_ok();
        let list = server.get("/api/camera").await.json::<serde_json::Value>();
        assert_eq!(list["cameras"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn camera_config_rejects_non_http_url() {
        let server = app(None, FakeController::verified());
        server
            .post("/api/camera/config")
            .json(&json!({ "external": [{ "url": "file:///etc/passwd" }] }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        // The proxy's ureq is built without TLS, so an https camera would only
        // fail later with a 502 — reject it up front rather than advertise it.
        server
            .post("/api/camera/config")
            .json(&json!({ "external": [{ "url": "https://cam.local/a.jpg" }] }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn external_camera_stream_url_round_trips_and_flags_the_list() {
        let server = app(None, FakeController::verified());
        server
            .post("/api/camera/config")
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
        let list = server.get("/api/camera").await.json::<serde_json::Value>();
        let cams = list["cameras"].as_array().unwrap();
        assert_eq!(cams[0]["stream"], true);
        assert_eq!(cams[1]["stream"], false);
        assert!(cams[0].get("url").is_none());
        // The gated config read echoes the stream URL for the manage form.
        let cfg = server
            .get("/api/camera/config")
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
            .post("/api/camera/config")
            .json(&json!({ "external": [
                { "label": "front", "url": "http://cam.local/snap",
                  "stream_url": "http://cam.local/stream", "park_tuning": tuning },
                // a stream camera WITHOUT tuning — not park-capable
                { "url": "http://cam.local/b.jpg", "stream_url": "http://cam.local/bstream" },
            ]}))
            .await
            .assert_status_ok();
        // park-capability needs BOTH a stream and a tuning.
        let list = server.get("/api/camera").await.json::<serde_json::Value>();
        let cams = list["cameras"].as_array().unwrap();
        assert_eq!(cams[0]["park"], true);
        assert_eq!(
            cams[1]["park"], false,
            "stream but no tuning → not park-capable"
        );
        // The gated config read echoes the tuning so the manage form can prefill.
        let cfg = server
            .get("/api/camera/config")
            .await
            .json::<serde_json::Value>();
        assert!(cfg["external"][0]["park_tuning"].is_object());
        assert_eq!(cfg["external"][0]["park_tuning"]["fps"], json!(4.0));
        assert!(cfg["external"][1]["park_tuning"].is_null());
    }

    #[tokio::test]
    async fn select_tuning_round_trips_and_flags_segment_capability() {
        let server = app(None, FakeController::verified());
        // ONE combined tuning object (park + select knobs), the shape the CLI seeds and the
        // manage form posts — parsed into ParkTuning AND SelectTuning.
        let full = json!({ "fps": 15, "left_frac": 0.33, "ema_seconds": 30, "abs_floor": 150,
            "mad_k": 3, "merge_gap_s": 1.2, "max_island_s": 3, "min_sep_s": 3,
            "candidate_frac": 0.75, "warmup_s": 4, "baseline_s": 90,
            "min_outlier": 2.5, "min_left_density": 3.0, "min_confidence": 0.4,
            "select_candidate_frac": 0.6 });
        // park knobs only — park-capable but NOT segment-capable (no select knobs).
        let park_only = json!({ "fps": 4, "left_frac": 0.33, "ema_seconds": 30, "abs_floor": 1500,
            "mad_k": 6, "merge_gap_s": 1.2, "max_island_s": 3, "min_sep_s": 3,
            "candidate_frac": 0.75, "warmup_s": 4, "baseline_s": 90 });
        server
            .post("/api/camera/config")
            .json(&json!({ "external": [
                { "url": "http://cam.local/s", "stream_url": "http://cam.local/stream", "park_tuning": full },
                { "url": "http://cam.local/b", "stream_url": "http://cam.local/bstream", "park_tuning": park_only },
            ]}))
            .await
            .assert_status_ok();
        // segment-capability needs a stream + park_tuning + select_tuning; park needs the first two.
        let list = server.get("/api/camera").await.json::<serde_json::Value>();
        let cams = list["cameras"].as_array().unwrap();
        assert_eq!(
            cams[0]["segment"], true,
            "stream + park + select → segment-capable"
        );
        assert_eq!(cams[0]["park"], true);
        assert_eq!(
            cams[1]["segment"], false,
            "no select knobs → not segment-capable"
        );
        assert_eq!(cams[1]["park"], true, "still park-capable");
        // The echo MERGES the select knobs back into the tuning object, so a manage-form
        // re-save round-trips them (they'd otherwise be dropped, breaking segment).
        let cfg = server
            .get("/api/camera/config")
            .await
            .json::<serde_json::Value>();
        assert_eq!(cfg["external"][0]["park_tuning"]["min_outlier"], json!(2.5));
        assert_eq!(cfg["external"][0]["park_tuning"]["fps"], json!(15.0));
        assert!(
            cfg["external"][1]["park_tuning"]
                .get("min_outlier")
                .is_none(),
            "park-only camera's echo carries no select knobs"
        );
    }

    #[tokio::test]
    async fn camera_config_rejects_a_partial_park_tuning() {
        // No baked defaults: a park_tuning missing a knob (abs_floor) must be rejected,
        // not run with a wrong value.
        let server = app(None, FakeController::verified());
        let res = server
            .post("/api/camera/config")
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
            .post("/api/camera/config")
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
    async fn timelapse_start_segment_rejects_without_a_capable_camera() {
        // Segment needs a stream + park_tuning + select_tuning; a stream-only camera
        // (no tuning) isn't capable → 400, like park.
        let server = app(None, FakeController::verified());
        server
            .post("/api/camera/config")
            .json(&json!({ "external": [
                { "url": "http://cam.local/a.jpg", "stream_url": "http://cam.local/s" },
            ]}))
            .await
            .assert_status_ok();
        server
            .post("/api/timelapse/start")
            .json(&json!({ "mode": "segment", "camera": "ext-0" }))
            .await
            .assert_status_bad_request();
    }

    #[test]
    fn run_dir_epoch_orders_runs_by_recency() {
        // The live park preview falls back to the most recent completed run; the dir's
        // leading epoch is the recency key (mode suffix and hint underscores don't matter).
        assert_eq!(
            super::run_dir_epoch("captures/1718900000_benchy_segment"),
            1718900000
        );
        assert!(
            super::run_dir_epoch("captures/1718900500_x_park")
                > super::run_dir_epoch("captures/1718900000_x_segment"),
            "a later epoch is more recent regardless of slot"
        );
        assert_eq!(
            super::run_dir_epoch("captures/not-a-run"),
            0,
            "unparseable sorts oldest"
        );
    }

    #[tokio::test]
    async fn timelapse_start_segment_rejects_a_bad_window() {
        // window_ms is validated up front (before camera resolution), so an out-of-range
        // value is a clean 400 regardless of camera config.
        app(None, FakeController::verified())
            .post("/api/timelapse/start")
            .json(&json!({ "mode": "segment", "camera": "ext-0", "window_ms": 50 }))
            .await
            .assert_status_bad_request();
    }

    #[test]
    fn parse_parks_index_dedupes_by_n_keeps_the_replace_and_sorts() {
        // parks.jsonl has one line per WRITE: a `replace` re-uses the prior `n` with a
        // stronger frame, so the index must keep one entry per distinct n (the last/
        // stronger metadata wins), sorted by n, and skip malformed/blank lines.
        let jsonl = concat!(
            "{\"n\":1,\"idx\":20,\"t\":5.0,\"confidence\":0.70,\"replace\":false}\n",
            "{\"n\":0,\"idx\":10,\"t\":2.5,\"confidence\":0.80,\"replace\":false}\n",
            "{\"n\":1,\"idx\":22,\"t\":5.6,\"confidence\":0.95,\"replace\":true}\n",
            "not json\n",
            "\n",
        );
        let idx = parse_parks_index(jsonl);
        assert_eq!(idx.len(), 2, "two distinct frames: {idx:?}");
        assert_eq!(idx[0]["n"], 0, "sorted by n");
        assert_eq!(idx[1]["n"], 1);
        assert_eq!(
            idx[1]["confidence"], 0.95,
            "the replace's stronger metadata wins"
        );
        assert_eq!(idx[1]["t"], 5.6);
    }

    /// Build a test server that shares its [`TimelapseManager`] handle, so a test can
    /// install a park run (out_dir + cameras) and seed its on-disk frames.
    fn app_with_timelapse(
        controller: impl Controller + 'static,
    ) -> (TestServer, Arc<TimelapseManager>) {
        let tl: Arc<TimelapseManager> = Default::default();
        let state = PrinterState {
            name: "test".to_string(),
            id: "test".to_string(),
            model: None,
            legacy_captures: true,
            source: Arc::new(FakeSource::idle()),
            controller: Arc::new(controller),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: None,
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: tl.clone(),
            hook_running: Default::default(),
            hook: Arc::new(NoHook),
            slicer: Arc::new(FakeSlicer),
            slice_jobs: Default::default(),
            slicer_names: crate::core::capability::default_registry()
                .slicer_names(&crate::core::model::Model::A1Mini)
                .copied(),
        };
        (TestServer::new(one(state)), tl)
    }

    fn test_tuning() -> ParkTuning {
        ParkTuning {
            fps: 4.0,
            left_frac: 0.33,
            ema_seconds: 6.0,
            abs_floor: 1500.0,
            mad_k: 6.0,
            merge_gap_s: 1.2,
            max_island_s: 3.0,
            min_sep_s: 3.0,
            candidate_frac: 0.75,
            warmup_s: 0.5,
            baseline_s: 20.0,
        }
    }

    /// Install a park run for `id` writing into `out`, with a no-op worker (the test seeds
    /// the frame files itself). Returns the status `tx` to keep the run's channel alive.
    fn install_park_run(
        tl: &Arc<TimelapseManager>,
        out: &std::path::Path,
        id: &str,
    ) -> watch::Sender<PrinterStatus> {
        let (tx, rx) = watch::channel(PrinterStatus::default());
        let noop: crate::server::timelapse::ParkSpawn =
            Arc::new(|_, _, _, _| tokio::task::spawn_blocking(|| {}));
        tl.start_park(
            vec![ParkCapture {
                id: id.to_string(),
                stream_url: "http://cam/stream".into(),
                tuning: test_tuning(),
            }],
            rx,
            out.to_path_buf(),
            noop,
        )
        .unwrap();
        tx
    }

    #[tokio::test]
    async fn parks_index_and_indexed_frames_serve_during_and_after_a_run() {
        let dir = std::env::temp_dir().join(format!("bambu-api-parks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cam = dir.join("ext-0");
        std::fs::create_dir_all(&cam).unwrap();

        let (server, tl) = app_with_timelapse(FakeController::verified());
        let _tx = install_park_run(&tl, &dir, "ext-0");

        std::fs::write(cam.join("park_000000.jpg"), b"FRAME0").unwrap();
        std::fs::write(cam.join("park_000001.jpg"), b"FRAME1").unwrap();
        std::fs::write(
            cam.join("parks.jsonl"),
            "{\"n\":0,\"t\":1.0,\"confidence\":0.8}\n{\"n\":1,\"t\":2.0,\"confidence\":0.9}\n",
        )
        .unwrap();

        // The index lists both frames, sorted, with a count.
        let res = server.get("/api/camera/ext-0/park").await;
        res.assert_status_ok();
        let body: serde_json::Value = res.json();
        assert_eq!(body["count"], 2);
        assert_eq!(body["parks"][0]["n"], 0);
        assert_eq!(body["parks"][1]["n"], 1);

        // An indexed frame serves its exact JPEG bytes.
        let f1 = server.get("/api/camera/ext-0/park/1").await;
        f1.assert_status_ok();
        assert_eq!(f1.as_bytes().as_ref(), b"FRAME1");

        // Out-of-range index → 404; a camera not in the run → 404 (no traversal join).
        server
            .get("/api/camera/ext-0/park/9")
            .await
            .assert_status_not_found();
        server
            .get("/api/camera/ext-1/park")
            .await
            .assert_status_not_found();
        server
            .get("/api/camera/ext-1/park/0")
            .await
            .assert_status_not_found();

        // After the run STOPS, the filmstrip stays reviewable (until the next run).
        tl.stop_park();
        let after = server.get("/api/camera/ext-0/park").await;
        after.assert_status_ok();
        assert_eq!(after.json::<serde_json::Value>()["count"], 2);
        server
            .get("/api/camera/ext-0/park/0")
            .await
            .assert_status_ok();

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A files store that returns one chosen `.3mf` for `fetch()`, to drive the dry-run's
    /// best-effort timelapse-block inspection. The other methods aren't exercised here.
    struct OneFile(Vec<u8>);
    impl crate::server::files::FileStore for OneFile {
        fn list(&self, _: &str) -> Result<Vec<crate::ftp::FileEntry>, String> {
            Ok(vec![])
        }
        fn upload(&self, _: &str, _: &std::path::Path) -> Result<(), String> {
            Ok(())
        }
        fn thumbnail(&self, _: &str, _: u32) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
        fn fetch(&self, _: &str) -> Result<Vec<u8>, String> {
            Ok(self.0.clone())
        }
        fn gcode(&self, _: &str, _: u32) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
        fn models(&self, _: &str) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }

    /// A minimal `.3mf` whose plate gcode injects `markers` per-layer timelapse blocks.
    fn three_mf_with_timelapse(markers: usize) -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut gcode = String::from("; time_lapse_gcode = ;SKIPTYPE: timelapse template\n");
        for i in 0..markers {
            gcode.push_str(&format!("G1 Z{i}\n; SKIPTYPE: timelapse\nM1004 S5 P1\n"));
        }
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("Metadata/plate_1.gcode", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(gcode.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        buf
    }

    /// A `.3mf` whose plate 1 declares these filament ids — the positions
    /// `expand_ams_map` keys the wire array by.
    fn three_mf_with_filaments(ids: &[usize]) -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let colors = vec!["\"#FFFFFF\""; ids.len()].join(",");
        let list = ids
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(
            r#"{{"filament_colors":[{colors}],"filament_ids":[{list}],"bed_type":"textured_plate"}}"#
        );
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("Metadata/plate_1.gcode", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"G1 X1\n").unwrap();
            zip.start_file("Metadata/plate_1.json", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(json.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        buf
    }

    /// A server whose only file is `bytes`.
    fn app_serving(bytes: Vec<u8>) -> TestServer {
        let mut state = printer("test");
        state.files = Arc::new(OneFile(bytes));
        TestServer::new(one(state))
    }

    #[tokio::test]
    async fn an_ams_map_must_have_one_entry_per_filament_the_plate_uses() {
        // The wire array is keyed by each filament's index in the project, and
        // `expand_ams_map` leaves a wrong-length map UNEXPANDED — the printer
        // then uses whatever the gcode baked in. That is how a plate got
        // printed in the wrong material once already, so a length that cannot
        // be expanded is refused rather than sent.
        let app = app_serving(three_mf_with_filaments(&[0, 1]));
        let start = |map: serde_json::Value| {
            app.post("/api/job/start")
                .json(&json!({ "file": "/two.3mf", "plate": 1, "confirm": true,
                               "use_ams": true, "ams_map": map }))
        };
        for bad in [json!([0]), json!([0, 1, 2])] {
            let n = bad.as_array().unwrap().len();
            let res = start(bad).await;
            res.assert_status(StatusCode::BAD_REQUEST);
            let t = res.text();
            assert!(t.contains("ams_map"), "{n}: {t}");
            assert!(t.contains('2'), "names how many the plate uses: {t}");
        }
        // The matching length is exactly what a two-colour print needs, and
        // must NOT be refused.
        start(json!([0, 1])).await.assert_status_ok();
    }

    #[tokio::test]
    async fn file_inspect_reports_timelapse_capability_open() {
        let state = PrinterState {
            name: "test".to_string(),
            id: "test".to_string(),
            model: None,
            legacy_captures: true,
            source: Arc::new(FakeSource::idle()),
            controller: Arc::new(FakeController::verified()),
            files: Arc::new(OneFile(three_mf_with_timelapse(3))),
            starter: Arc::new(FakeStarter),
            // A password is set, to prove the inspect read stays OPEN (no auth needed).
            password: Some("secret".to_string()),
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
            hook_running: Default::default(),
            hook: Arc::new(NoHook),
            slicer: Arc::new(FakeSlicer),
            slice_jobs: Default::default(),
            slicer_names: crate::core::capability::default_registry()
                .slicer_names(&crate::core::model::Model::A1Mini)
                .copied(),
        };
        let server = TestServer::new(one(state));
        let res = server
            .get("/api/file/inspect?name=/cube.gcode.3mf&plate=1")
            .await;
        res.assert_status_ok();
        let body: serde_json::Value = res.json();
        assert_eq!(body["inspected"], true);
        assert_eq!(body["has_timelapse_blocks"], true);
    }

    #[tokio::test]
    async fn file_inspect_degrades_to_not_inspected() {
        // FakeFiles.fetch returns junk (not a zip) → inspected:false, never an error status.
        let res = app(None, FakeController::verified())
            .get("/api/file/inspect?name=/x.gcode.3mf&plate=1")
            .await;
        res.assert_status_ok();
        assert_eq!(res.json::<serde_json::Value>()["inspected"], false);
        // A non-3mf is also a clean "not inspected", not a 4xx.
        let res2 = app(None, FakeController::verified())
            .get("/api/file/inspect?name=/notes.txt")
            .await;
        res2.assert_status_ok();
        assert_eq!(res2.json::<serde_json::Value>()["inspected"], false);
    }

    #[tokio::test]
    async fn job_start_dry_run_reports_timelapse_block_capability() {
        let state = PrinterState {
            name: "test".to_string(),
            id: "test".to_string(),
            model: None,
            legacy_captures: true,
            source: Arc::new(FakeSource::idle()),
            controller: Arc::new(FakeController::verified()),
            files: Arc::new(OneFile(three_mf_with_timelapse(3))),
            starter: Arc::new(FakeStarter),
            password: None,
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
            hook_running: Default::default(),
            hook: Arc::new(NoHook),
            slicer: Arc::new(FakeSlicer),
            slice_jobs: Default::default(),
            slicer_names: crate::core::capability::default_registry()
                .slicer_names(&crate::core::model::Model::A1Mini)
                .copied(),
        };
        let server = TestServer::new(one(state));
        let res = server
            .post("/api/job/start")
            .json(&json!({ "file": "/cube.gcode.3mf", "plate": 1, "dry_run": true, "timelapse": true }))
            .await;
        res.assert_status_ok();
        // The on-printer file was downloaded + scanned: it has the per-layer park blocks.
        assert_eq!(
            res.json::<serde_json::Value>()["plan"]["has_timelapse_blocks"],
            true
        );
    }

    #[tokio::test]
    async fn job_start_dry_run_timelapse_blocks_null_when_uninspectable() {
        // FakeFiles.fetch returns junk (not a zip) → inspection fails → null, not an error.
        let res = app(None, FakeController::verified())
            .post("/api/job/start")
            .json(&json!({ "file": "/x.gcode.3mf", "plate": 1, "dry_run": true }))
            .await;
        res.assert_status_ok();
        assert!(
            res.json::<serde_json::Value>()["plan"]["has_timelapse_blocks"].is_null(),
            "uninspectable file → unknown (null), gracefully"
        );
    }

    #[tokio::test]
    async fn captures_list_is_open_and_returns_an_array() {
        // Open read; shape is `{ captures: [...] }` (contents depend on ./captures, which
        // may be empty). The listing logic itself is unit-tested in `crate::captures`.
        let res = app(Some("secret"), FakeController::verified())
            .get("/api/capture")
            .await;
        res.assert_status_ok();
        assert!(res.json::<serde_json::Value>()["captures"].is_array());
    }

    #[tokio::test]
    async fn unknown_api_path_404s_as_json() {
        let server = app(None, FakeController::verified());
        // A typo'd / unknown API path → JSON 404, not the SPA's HTML 200.
        let res = server.get("/api/nope").await;
        res.assert_status_not_found();
        assert!(res.json::<serde_json::Value>()["error"].is_string());
        // A deeper unmatched path under a real prefix, too.
        server
            .get("/api/camera/x/bogus")
            .await
            .assert_status_not_found();
        // A real route is NOT shadowed by the catch-all.
        server.get("/api/status").await.assert_status_ok();
    }

    #[test]
    fn is_safe_segment_blocks_traversal() {
        assert!(is_safe_segment("1781634785_cube_smooth"));
        assert!(is_safe_segment("ext-0"));
        assert!(is_safe_segment("default"));
        assert!(!is_safe_segment(""));
        assert!(!is_safe_segment(".."));
        assert!(!is_safe_segment(".hidden"));
        assert!(!is_safe_segment("a/b"));
        assert!(!is_safe_segment("a\\b"));
    }

    #[tokio::test]
    async fn capture_video_rejects_unsafe_and_unknown() {
        let server = app(None, FakeController::verified());
        // A leading-dot (traversal-ish) segment is refused outright.
        server
            .get("/api/capture/.evil/cam/video.mp4")
            .await
            .assert_status_bad_request();
        // A safe-but-nonexistent run → nothing to serve.
        server
            .get("/api/capture/no_such_run_zzz/cam/video.mp4")
            .await
            .assert_status_not_found();
    }

    #[tokio::test]
    async fn capture_thumb_rejects_unsafe_and_unknown() {
        let server = app(None, FakeController::verified());
        // Same path-safety + missing-run handling as the video endpoint.
        server
            .get("/api/capture/.evil/cam/thumb.jpg")
            .await
            .assert_status_bad_request();
        server
            .get("/api/capture/no_such_run_zzz/cam/thumb.jpg")
            .await
            .assert_status_not_found();
    }

    #[tokio::test]
    async fn parks_index_and_frame_are_404_without_a_run() {
        let server = app(None, FakeController::verified());
        server
            .get("/api/camera/ext-0/park")
            .await
            .assert_status_not_found();
        server
            .get("/api/camera/ext-0/park/0")
            .await
            .assert_status_not_found();
    }

    #[tokio::test]
    async fn camera_config_rejects_non_http_stream_url() {
        let server = app(None, FakeController::verified());
        server
            .post("/api/camera/config")
            .json(&json!({
                "external": [
                    { "url": "http://cam.local/a.jpg", "stream_url": "file:///etc/passwd" }
                ]
            }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        // Same TLS-less reason as the snapshot URL: no https streams.
        server
            .post("/api/camera/config")
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
            .post("/api/camera/config")
            .json(&json!({ "external": [
                { "url": format!("http://{addr}/snap"),
                  "stream_url": format!("http://{addr}/stream") }
            ] }))
            .await
            .assert_status_ok();
        let res = server.get("/api/camera/ext-0/stream").await;
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
            .post("/api/camera/config")
            .json(&json!({ "external": [{ "url": "http://cam.local/a.jpg" }] }))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn thumbnail_rejects_non_3mf() {
        app(None, FakeController::verified())
            .get("/api/file/thumbnail?name=/timelapse/video.mp4")
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
            .post("/api/file/upload?dir=../etc&name=a.3mf")
            .bytes(b"data".to_vec().into())
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn upload_open_when_no_password() {
        app(None, FakeController::verified())
            .post("/api/file/upload?name=part.gcode.3mf")
            .bytes(b"PK\x03\x04 fake 3mf".to_vec().into())
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn upload_needs_password_when_set() {
        app(Some("secret"), FakeController::verified())
            .post("/api/file/upload?name=part.gcode.3mf")
            .bytes(b"data".to_vec().into())
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn upload_rejects_path_traversal() {
        app(None, FakeController::verified())
            .post("/api/file/upload?name=../etc/passwd")
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
        // PrinterState::fake() source is IDLE, so the idle guard passes.
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
        let state = PrinterState {
            name: "test".to_string(),
            id: "test".to_string(),
            model: None,
            legacy_captures: true,
            source: Arc::new(FakeSource::ramping(Duration::from_millis(50))),
            controller: Arc::new(FakeController::verified()),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: None,
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
            hook_running: Default::default(),
            hook: Arc::new(NoHook),
            slicer: Arc::new(FakeSlicer),
            slice_jobs: Default::default(),
            slicer_names: crate::core::capability::default_registry()
                .slicer_names(&crate::core::model::Model::A1Mini)
                .copied(),
        };
        TestServer::new(one(state))
            .post("/api/job/start")
            .json(&json!({ "file": "/c.3mf", "confirm": true }))
            .await
            .assert_status(StatusCode::CONFLICT);
    }

    // ── machine control: helpers ──

    /// A test server whose source is RUNNING (busy), to exercise the idle guard.
    fn busy_app(controller: impl Controller + 'static) -> TestServer {
        let state = PrinterState {
            name: "test".to_string(),
            id: "test".to_string(),
            model: None,
            legacy_captures: true,
            source: Arc::new(FakeSource::ramping(Duration::from_millis(50))),
            controller: Arc::new(controller),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: None,
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
            hook_running: Default::default(),
            hook: Arc::new(NoHook),
            slicer: Arc::new(FakeSlicer),
            slice_jobs: Default::default(),
            slicer_names: crate::core::capability::default_registry()
                .slicer_names(&crate::core::model::Model::A1Mini)
                .copied(),
        };
        TestServer::new(one(state))
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
        let state = PrinterState {
            name: "test".to_string(),
            id: "test".to_string(),
            model: None,
            legacy_captures: true,
            source: Arc::new(HotSource::new()),
            controller: Arc::new(controller),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: None,
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
            hook_running: Default::default(),
            hook: Arc::new(NoHook),
            slicer: Arc::new(FakeSlicer),
            slice_jobs: Default::default(),
            slicer_names: crate::core::capability::default_registry()
                .slicer_names(&crate::core::model::Model::A1Mini)
                .copied(),
        };
        TestServer::new(one(state))
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
    fn ws_server(state: PrinterState) -> TestServer {
        TestServer::builder().http_transport().build(one(state))
    }

    #[tokio::test]
    async fn ws_is_open_and_pushes_initial_status() {
        let mut ws = ws_server(PrinterState::fake())
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
        let state = PrinterState {
            name: "test".to_string(),
            id: "test".to_string(),
            model: None,
            legacy_captures: true,
            source: Arc::new(FakeSource::ramping(Duration::from_millis(5))),
            controller: Arc::new(FakeController::verified()),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: None,
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
            hook_running: Default::default(),
            hook: Arc::new(NoHook),
            slicer: Arc::new(FakeSlicer),
            slice_jobs: Default::default(),
            slicer_names: crate::core::capability::default_registry()
                .slicer_names(&crate::core::model::Model::A1Mini)
                .copied(),
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

    // ── slicing ──────────────────────────────────────────────────────────

    /// A slicing-capable test server, with the slicer and the model mapping
    /// chosen by the test.
    fn slice_app(
        slicer: Arc<dyn Slicer>,
        model: Option<crate::core::model::Model>,
    ) -> (TestServer, PrinterState) {
        let state = PrinterState {
            name: "test".to_string(),
            id: "test".to_string(),
            model: model.as_ref().map(|m| m.as_str().to_string()),
            legacy_captures: true,
            source: Arc::new(FakeSource::idle()),
            controller: Arc::new(FakeController::verified()),
            files: Arc::new(FakeFiles),
            starter: Arc::new(FakeStarter),
            password: None,
            start_lock: Arc::new(tokio::sync::Mutex::new(())),
            external_cameras: Arc::new(RwLock::new(Vec::new())),
            internal_camera: Arc::new(NoCamera),
            timelapse: Default::default(),
            hook_running: Default::default(),
            hook: Arc::new(NoHook),
            slicer,
            slice_jobs: Default::default(),
            slicer_names: model.and_then(|m| {
                crate::core::capability::default_registry()
                    .slicer_names(&m)
                    .copied()
            }),
        };
        (TestServer::new(one(state.clone())), state)
    }

    /// The usual case: a fake slicer standing in for an A1 mini.
    fn fake_slice_app() -> TestServer {
        slice_app(
            Arc::new(FakeSlicer),
            Some(crate::core::model::Model::A1Mini),
        )
        .0
    }

    /// The valid half of a slice request (percent-encoded, as a client sends
    /// it), so each test can vary one thing.
    const SLICE_Q: &str = "name=cube.stl&layer=0.12&filament=Bambu%20PLA%20Basic%20@BBL%20A1M\
         &bed_type=Textured%20PEI%20Plate&nozzle=0.4";
    const FILAMENT_Q: &str = "filament=Bambu%20PLA%20Basic%20@BBL%20A1M";

    /// The id of the slice currently in the slot — what a caller echoes back to
    /// confirm a print of exactly this one.
    async fn slice_id(app: &TestServer) -> u64 {
        app.get("/api/slice").await.json::<serde_json::Value>()["job"]["id"]
            .as_u64()
            .expect("a finished slice has an id")
    }

    /// Poll the slot the way the dashboard does, until the job settles.
    async fn await_slice(app: &TestServer) -> serde_json::Value {
        for _ in 0..2000 {
            let job = app.get("/api/slice").await.json::<serde_json::Value>()["job"].clone();
            if job["state"] != "running" {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the slice never finished");
    }

    #[tokio::test]
    async fn slice_availability_is_advertised_with_the_machine_it_would_slice_for() {
        let body = fake_slice_app()
            .get("/api/slice")
            .await
            .json::<serde_json::Value>();
        assert_eq!(body["available"], true);
        assert_eq!(body["slicer"]["kind"], "fake");
        assert_eq!(body["machine"], "Bambu Lab A1 mini");
        assert_eq!(body["job"]["state"], "idle");
        assert!(body["reason"].is_null());
    }

    #[tokio::test]
    async fn with_no_slicer_installed_the_api_says_so_instead_of_failing() {
        let (app, _) = slice_app(
            Arc::new(super::super::slice::NoSlicer(
                "no slicer found: install X".to_string(),
            )),
            Some(crate::core::model::Model::A1Mini),
        );
        let body = app.get("/api/slice").await.json::<serde_json::Value>();
        assert_eq!(body["available"], false);
        assert_eq!(body["reason"], "no slicer found: install X");

        let res = app
            .post(&format!("/api/slice?{SLICE_Q}"))
            .bytes(b"solid cube".to_vec().into())
            .await;
        res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            res.json::<serde_json::Value>()["error"],
            "no slicer found: install X"
        );
    }

    #[tokio::test]
    async fn a_model_with_no_verified_profile_mapping_is_not_sliced_for() {
        // A P1S is a printer we happily control — and one we have never verified
        // a slicing profile for. Slicing it against the A1 mini's machine
        // profile would emit gcode for a different bed.
        let (app, _) = slice_app(Arc::new(FakeSlicer), Some(crate::core::model::Model::P1S));
        let body = app.get("/api/slice").await.json::<serde_json::Value>();
        assert_eq!(body["available"], false);
        assert!(
            body["reason"].as_str().unwrap().contains("p1s"),
            "{}",
            body["reason"]
        );
        assert!(body["machine"].is_null());
        app.post(&format!("/api/slice?{SLICE_Q}"))
            .bytes(b"solid cube".to_vec().into())
            .await
            .assert_status(StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn slice_refuses_to_guess_the_things_only_the_user_knows() {
        let app = fake_slice_app();
        let body = b"solid cube".to_vec();
        for (q, want) in [
            (
                "name=cube.stl&filament=Bambu%20PLA%20Basic%20@BBL%20A1M&bed_type=Textured%20PEI%20Plate",
                "layer",
            ),
            (
                "name=cube.stl&layer=0.12&bed_type=Textured%20PEI%20Plate",
                "filament",
            ),
            (
                "name=cube.stl&layer=0.12&filament=Bambu%20PLA%20Basic%20@BBL%20A1M",
                "bed_type",
            ),
        ] {
            let res = app
                .post(&format!("/api/slice?{q}"))
                .bytes(body.clone().into())
                .await;
            res.assert_status(StatusCode::BAD_REQUEST);
            let err = res.json::<serde_json::Value>()["error"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(err.contains(want), "expected {want:?} in {err:?}");
        }
        // The nozzle picks the machine preset; an idle fake reports none.
        let res = app
            .post(
                "/api/slice?name=cube.stl&layer=0.12&filament=Bambu%20PLA%20Basic%20@BBL%20A1M\
                 &bed_type=Textured%20PEI%20Plate",
            )
            .bytes(body.clone().into())
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert!(
            res.json::<serde_json::Value>()["error"]
                .as_str()
                .unwrap()
                .contains("nozzle")
        );
    }

    #[tokio::test]
    async fn slice_rejects_bad_names_extensions_and_layer_heights() {
        let app = fake_slice_app();
        let body = b"solid cube".to_vec();
        let post = async |q: String| {
            app.post(&format!("/api/slice?{q}"))
                .bytes(body.clone().into())
                .await
        };

        for name in [
            "..%2F..%2Fetc%2Fpasswd",
            "sub%2Fcube.stl",
            "cube%5Cx.stl",
            "",
        ] {
            let q = SLICE_Q.replace("name=cube.stl", &format!("name={name}"));
            post(q).await.assert_status(StatusCode::BAD_REQUEST);
        }
        // Not a model.
        post(SLICE_Q.replace("cube.stl", "notes.txt"))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        // Already sliced — that goes to /job/upload-start, not here.
        let res = post(SLICE_Q.replace("cube.stl", "cube.gcode.3mf")).await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert!(
            res.json::<serde_json::Value>()["error"]
                .as_str()
                .unwrap()
                .contains("already a sliced")
        );
        // Outside what a nozzle can lay down.
        post(SLICE_Q.replace("layer=0.12", "layer=1.5"))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        // A filament name that would walk out of the profiles directory.
        post(SLICE_Q.replace(FILAMENT_Q, "filament=..%2F..%2F..%2F..%2Fetc%2Fpasswd"))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        // Nothing to slice.
        app.post(&format!("/api/slice?{SLICE_Q}"))
            .bytes(Vec::new().into())
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_authored_project_3mf_is_refused_rather_than_re_arranged() {
        let app = fake_slice_app();
        let mut buf = Vec::new();
        {
            use std::io::Write as _;
            use zip::write::SimpleFileOptions;
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            for name in ["3D/3dmodel.model", "Metadata/project_settings.config"] {
                zip.start_file(name, SimpleFileOptions::default()).unwrap();
                zip.write_all(b"{}").unwrap();
            }
            zip.finish().unwrap();
        }
        let res = app
            .post(&format!(
                "/api/slice?{}",
                SLICE_Q.replace("cube.stl", "part.3mf")
            ))
            .bytes(buf.clone().into())
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        let err = res.json::<serde_json::Value>()["error"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(err.contains("project"), "{err}");

        // Bare geometry in a 3mf is fine — that is the case the helper is for.
        let mut bare = Vec::new();
        {
            use std::io::Write as _;
            use zip::write::SimpleFileOptions;
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut bare));
            zip.start_file("3D/3dmodel.model", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"<model/>").unwrap();
            zip.finish().unwrap();
        }
        app.post(&format!(
            "/api/slice?{}",
            SLICE_Q.replace("cube.stl", "part.3mf")
        ))
        .bytes(bare.into())
        .await
        .assert_status(StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn the_finished_slice_is_password_gated_while_its_status_stays_open() {
        // An unusual pairing — an authenticated GET — so it needs a test of its
        // own. Everything else here builds a state with no password, which
        // means moving this route back into `reads` would go unnoticed.
        let (app, st) = slice_app(
            Arc::new(FakeSlicer),
            Some(crate::core::model::Model::A1Mini),
        );
        let guarded = TestServer::new(one(PrinterState {
            password: Some("hunter2".to_string()),
            ..st
        }));
        drop(app);
        // Status is printer-shaped and stays open…
        guarded.get("/api/slice").await.assert_status_ok();
        // …the file made from the caller's own geometry is not.
        guarded
            .get("/api/slice/result")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        guarded
            .get("/api/slice/result")
            .add_header("authorization", "Bearer hunter2")
            // No slice has run, so this is the honest 404 rather than a 401.
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_slot_is_claimed_before_the_upload_not_after_it() {
        // Otherwise "one slice at a time" bounds the slicer and not the disk:
        // every concurrent caller sees an idle slot, writes its own copy of a
        // body worth up to 512 MiB, and only then races to start.
        let (app, st) = slice_app(
            Arc::new(FakeSlicer),
            Some(crate::core::model::Model::A1Mini),
        );

        let held = st.slice_jobs.reserve().expect("the slot starts free");
        // While someone is staging, the slot reads as busy and a second upload
        // is refused before it can write anything.
        assert_eq!(st.slice_jobs.status().state, "running");
        let res = app
            .post(&format!("/api/slice?{SLICE_Q}"))
            .bytes(b"solid cube".to_vec().into())
            .await;
        res.assert_status(StatusCode::CONFLICT);
        assert!(res.text().contains("already running"), "{}", res.text());

        // Releasing it — which is what a failed or abandoned upload does, since
        // the guard drops — leaves the slot usable rather than wedged.
        drop(held);
        assert_eq!(st.slice_jobs.status().state, "idle");
        app.post(&format!("/api/slice?{SLICE_Q}"))
            .bytes(b"solid cube".to_vec().into())
            .await
            .assert_status(StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn a_rejected_upload_does_not_wedge_the_slot() {
        // Every early return between the claim and the start drops the guard.
        // If one did not, a single bad request would take the printer's slicing
        // out of service until a restart.
        let app = fake_slice_app();
        app.post(&format!(
            "/api/slice?{}",
            SLICE_Q.replace("cube.stl", "notes.txt")
        ))
        .bytes(b"nope".to_vec().into())
        .await
        .assert_status(StatusCode::BAD_REQUEST);
        // Still usable.
        app.post(&format!("/api/slice?{SLICE_Q}"))
            .bytes(b"solid cube".to_vec().into())
            .await
            .assert_status(StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn a_nozzle_without_verified_profiles_is_refused_rather_than_sliced_at_0_4() {
        // The bundle ships a process set per nozzle; only the 0.4 set is
        // mapped. Slicing a 0.6 with 0.4 speeds and flow would succeed and be
        // wrong — the failure mode this whole module is arranged against.
        let app = fake_slice_app();
        for n in ["0.2", "0.6", "0.8"] {
            let res = app
                .post(&format!(
                    "/api/slice?{}",
                    SLICE_Q.replace("nozzle=0.4", &format!("nozzle={n}"))
                ))
                .bytes(b"solid cube".to_vec().into())
                .await;
            res.assert_status(StatusCode::BAD_REQUEST);
            assert!(
                res.text().contains("no verified slicing profiles"),
                "{n}: {}",
                res.text()
            );
            assert!(res.text().contains(n), "names the nozzle: {}", res.text());
        }
    }

    #[tokio::test]
    async fn a_format_the_slicer_cannot_read_is_refused_up_front() {
        // The slicer rejects anything but .stl/.obj/.amf (and .3mf on its own
        // path) before it opens the file. Accepting `.step` here would answer
        // 202 and fail in the background — a worse answer than refusing.
        let (app, _) = slice_app(
            Arc::new(FakeSlicer),
            Some(crate::core::model::Model::A1Mini),
        );
        for bad in ["part.step", "part.stp"] {
            let res = app
                .post(&format!("/api/slice?{}", SLICE_Q.replace("cube.stl", bad)))
                .bytes(b"whatever".to_vec().into())
                .await;
            res.assert_status(StatusCode::BAD_REQUEST);
            assert!(
                res.text().contains("STEP is not supported"),
                "{}",
                res.text()
            );
        }
    }

    #[tokio::test]
    async fn printing_a_slice_needs_exactly_one_tray() {
        // The slice uses one filament, and a mismatched map is left unexpanded
        // — the printer then falls back to whatever the gcode baked in, which
        // is how a plate got printed in the wrong material once already.
        let app = fake_slice_app();
        app.post(&format!("/api/slice?{SLICE_Q}"))
            .bytes(b"solid cube".to_vec().into())
            .await
            .assert_status(StatusCode::ACCEPTED);
        await_slice(&app).await;
        for map in [json!([]), json!([0, 3])] {
            let res = app
                .post("/api/slice/print")
                .json(&json!({ "use_ams": true, "ams_map": map, "dry_run": true }))
                .await;
            res.assert_status(StatusCode::BAD_REQUEST);
            assert!(res.text().contains("exactly one tray"), "{}", res.text());
        }
        // One is fine.
        app.post("/api/slice/print")
            .json(&json!({ "use_ams": true, "ams_map": [0], "dry_run": true }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn the_staged_model_keeps_the_extension_the_slicer_dispatches_on() {
        // libslic3r picks its model reader from the extension and refuses an
        // extensionless path outright — verified against the installed slicer,
        // where byte-identical files differing only in name gave "Unknown file
        // format" and a load. Staging without a suffix therefore made EVERY
        // real slice fail, and no test noticed because every double ignored
        // `p.input`. This one looks at it.
        struct Inspects(Arc<std::sync::Mutex<Option<String>>>);
        impl Slicer for Inspects {
            fn info(&self) -> Result<SlicerInfo, String> {
                Ok(SlicerInfo {
                    kind: "inspects".to_string(),
                    thumbnails: false,
                })
            }
            fn slice(&self, p: &SliceParams) -> Result<super::super::slice::SliceOutput, String> {
                *self.0.lock().unwrap() = Some(p.input.to_string_lossy().into_owned());
                Err("stop here; the path is what this test is about".to_string())
            }
        }
        let seen = Arc::new(std::sync::Mutex::new(None));
        let (app, _) = slice_app(
            Arc::new(Inspects(Arc::clone(&seen))),
            Some(crate::core::model::Model::A1Mini),
        );
        app.post(&format!("/api/slice?{SLICE_Q}"))
            .bytes(b"solid cube".to_vec().into())
            .await
            .assert_status(StatusCode::ACCEPTED);
        await_slice(&app).await;
        let path = seen.lock().unwrap().clone().expect("the slicer saw a path");
        assert!(
            path.ends_with(".stl"),
            "the staged model must carry the uploaded file's extension, got {path:?}"
        );
    }

    #[tokio::test]
    async fn a_second_slice_while_one_runs_is_refused() {
        // A slicer that only finishes when the test lets it, so the slot is
        // provably still occupied at the second request.
        struct Gated(Arc<std::sync::atomic::AtomicBool>);
        impl Slicer for Gated {
            fn info(&self) -> Result<SlicerInfo, String> {
                Ok(SlicerInfo {
                    kind: "gated".to_string(),
                    thumbnails: false,
                })
            }
            fn slice(&self, _p: &SliceParams) -> Result<super::super::slice::SliceOutput, String> {
                while !self.0.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err("released".to_string())
            }
        }
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (app, _) = slice_app(
            Arc::new(Gated(Arc::clone(&gate))),
            Some(crate::core::model::Model::A1Mini),
        );
        app.post(&format!("/api/slice?{SLICE_Q}"))
            .bytes(b"solid cube".to_vec().into())
            .await
            .assert_status(StatusCode::ACCEPTED);
        app.post(&format!("/api/slice?{SLICE_Q}"))
            .bytes(b"solid cube".to_vec().into())
            .await
            .assert_status(StatusCode::CONFLICT);
        // And the finished file cannot be fetched mid-run.
        app.get("/api/slice/result")
            .await
            .assert_status(StatusCode::CONFLICT);

        gate.store(true, std::sync::atomic::Ordering::SeqCst);
        let job = await_slice(&app).await;
        assert_eq!(job["state"], "failed");
        assert_eq!(job["error"], "released");
    }

    #[tokio::test]
    async fn slice_then_download_then_print_it() {
        let app = fake_slice_app();
        // Nothing sliced yet.
        app.get("/api/slice/result")
            .await
            .assert_status(StatusCode::NOT_FOUND);
        app.post("/api/slice/print")
            .json(&json!({ "confirm": true }))
            .await
            .assert_status(StatusCode::NOT_FOUND);

        app.post(&format!("/api/slice?{SLICE_Q}"))
            .bytes(b"solid cube".to_vec().into())
            .await
            .assert_status(StatusCode::ACCEPTED);
        let job = await_slice(&app).await;
        assert_eq!(job["state"], "done", "{job}");
        assert_eq!(job["out_name"], "cube.gcode.3mf");
        // Read back out of the verified gcode, not echoed from the request.
        assert_eq!(job["layers"], 42);
        assert_eq!(job["bed_temp_c"], 65);

        let res = app.get("/api/slice/result").await;
        res.assert_status_ok();
        assert!(
            res.headers()[CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .contains("cube.gcode.3mf")
        );
        assert!(crate::core::project::inspect_plate(res.as_bytes(), 1).is_ok());

        // Printing it is gated exactly like any other start.
        app.post("/api/slice/print")
            .json(&json!({}))
            .await
            .assert_status(StatusCode::PRECONDITION_REQUIRED);
        let plan = app
            .post("/api/slice/print")
            .json(&json!({ "dry_run": true }))
            .await
            .json::<serde_json::Value>();
        assert_eq!(plan["plan"]["file"], "/cube.gcode.3mf");
        assert_eq!(plan["plan"]["plate"], 1);
        // The md5 the printer will verify the file against is stamped from the
        // local bytes, not taken on trust.
        assert_eq!(plan["plan"]["md5"].as_str().unwrap().len(), 32);

        // The plan names the slice it is for, and confirming echoes it back.
        let expect = plan["plan"]["expect"].as_u64().unwrap();
        assert_eq!(expect, slice_id(&app).await);
        app.post("/api/slice/print")
            .json(&json!({ "confirm": true, "expect": expect }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn slice_print_validates_the_ams_map_like_every_other_start() {
        let app = fake_slice_app();
        app.post(&format!("/api/slice?{SLICE_Q}"))
            .bytes(b"solid cube".to_vec().into())
            .await
            .assert_status(StatusCode::ACCEPTED);
        await_slice(&app).await;
        app.post("/api/slice/print")
            .json(&json!({ "confirm": true, "use_ams": true, "ams_map": [9] }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn confirming_a_print_must_name_the_slice_the_plan_described() {
        // One slot, several callers: a second slice between the plan and the
        // confirmation replaces the result, and the confirmation would then
        // print a file whose plan nobody saw. Proven, not assumed.
        let app = fake_slice_app();
        let slice = |name: &str| {
            app.post(&format!("/api/slice?{}", SLICE_Q.replace("cube.stl", name)))
                .bytes(b"solid cube".to_vec().into())
        };
        slice("first.stl").await.assert_status(StatusCode::ACCEPTED);
        await_slice(&app).await;

        let plan = app
            .post("/api/slice/print")
            .json(&json!({ "dry_run": true }))
            .await;
        plan.assert_status_ok();
        let first: u64 = plan.json::<serde_json::Value>()["plan"]["expect"]
            .as_u64()
            .expect("the plan says which slice it is for");

        // Someone else slices. The slot was free, so this is allowed.
        slice("second.stl")
            .await
            .assert_status(StatusCode::ACCEPTED);
        await_slice(&app).await;

        // Confirming against the plan we were shown must not print the other one.
        let stale = app
            .post("/api/slice/print")
            .json(&json!({ "confirm": true, "expect": first }))
            .await;
        stale.assert_status(StatusCode::CONFLICT);
        assert!(
            stale.text().contains("no longer the one loaded"),
            "{}",
            stale.text()
        );

        // Confirming with no id at all is refused too — that is the same race
        // with the check simply omitted.
        app.post("/api/slice/print")
            .json(&json!({ "confirm": true }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);

        // Re-planning shows the current slice, and confirming that one works.
        let plan2 = app
            .post("/api/slice/print")
            .json(&json!({ "dry_run": true }))
            .await;
        let second = plan2.json::<serde_json::Value>()["plan"]["expect"]
            .as_u64()
            .unwrap();
        assert_ne!(first, second, "a new slice gets a new id");
        app.post("/api/slice/print")
            .json(&json!({ "confirm": true, "expect": second }))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn slice_print_refuses_while_the_printer_is_busy() {
        let (app, st) = slice_app(
            Arc::new(FakeSlicer),
            Some(crate::core::model::Model::A1Mini),
        );
        app.post(&format!("/api/slice?{SLICE_Q}"))
            .bytes(b"solid cube".to_vec().into())
            .await
            .assert_status(StatusCode::ACCEPTED);
        await_slice(&app).await;
        // Re-serve the SAME finished slot behind a busy printer: the refusal has
        // to come from the idle guard in the shared upload-and-start helper, not
        // from there being nothing to print.
        let busy = PrinterStatus {
            gcode_state: Some("RUNNING".to_string()),
            ..Default::default()
        };
        let (tx, rx) = watch::channel(busy);
        let busy_state = PrinterState {
            source: Arc::new(FakeSource { tx, _keepalive: rx }),
            slice_jobs: st.slice_jobs.clone(),
            ..st.clone()
        };
        let id = slice_id(&app).await;
        TestServer::new(one(busy_state))
            .post("/api/slice/print")
            .json(&json!({ "confirm": true, "expect": id }))
            .await
            .assert_status(StatusCode::CONFLICT);
    }
}
