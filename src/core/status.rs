//! A typed view over the merged report state.
//!
//! [`ReportState`](crate::core::report::ReportState) keeps the raw, untyped
//! JSON; [`PrinterStatus`] extracts the fields an agent actually cares about.
//! Every field is optional because a delta may not carry it and because we stay
//! tolerant of fields the device omits.
//!
//! Field names and shapes here are taken from a **real A1 mini capture** (see
//! `tests/fixtures/pushall-n1-idle.json`), not from spec guesses — e.g. fan
//! speeds arrive as strings and are parsed here.

use crate::core::capability::{ChamberTemperature, HardwareFeatures};
use crate::core::hms::{HmsEntry, Module, decode_report_hms};
use crate::core::stage::Stage;
use serde::Serialize;
use serde_json::Value;

/// The fields of a printer `print` report that matter for monitoring.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "ts-rs", derive(ts_rs::TS), ts(export))]
pub struct PrinterStatus {
    /// Coarse job state, e.g. `IDLE`, `RUNNING`, `PAUSE`, `FINISH`, `FAILED`.
    pub gcode_state: Option<String>,
    /// `print_error` code (0 = none).
    pub print_error: Option<i64>,
    /// Typed view of a non-zero `print_error` (a device-level fault, distinct
    /// from HMS — observed: a failing SD card surfaced `0x0500C010` here while
    /// `hms` was empty). `None` when there is no active error.
    pub error: Option<DeviceError>,
    /// Progress percentage (`mc_percent`).
    pub mc_percent: Option<i64>,
    /// Current layer / total layers.
    pub layer_num: Option<i64>,
    pub total_layer_num: Option<i64>,
    /// Remaining time in minutes (`mc_remaining_time`).
    pub remaining_time_min: Option<i64>,
    /// Current stage id (`stg_cur`). Read together with `gcode_state`: stage 0
    /// is the no-special-stage default and appears while idle too.
    pub stg_cur: Option<i64>,
    /// Decoded name of `stg_cur` (e.g. `auto_bed_leveling`), or `None` for an
    /// unknown/future stage id. See [`crate::core::stage`].
    pub stage: Option<&'static str>,
    /// `home_flag` bitfield (per-axis homed state); it changes during a home/move,
    /// so it's one of the few report signals that reflect ad-hoc motion.
    pub home_flag: Option<i64>,
    /// Nozzle / bed temperatures and their targets (°C).
    pub nozzle_temper: Option<f64>,
    pub nozzle_target: Option<f64>,
    pub bed_temper: Option<f64>,
    pub bed_target: Option<f64>,
    /// **Raw** `chamber_temper` value as reported. On A1/P1 this is emitted but
    /// is not a real sensor — call [`PrinterStatus::real_chamber_temperature`]
    /// for a value only when the model actually has a chamber sensor.
    pub chamber_temper_raw: Option<f64>,
    /// Part-cooling fan speed (`cooling_fan_speed`; arrives as a string).
    pub cooling_fan_speed: Option<i64>,
    /// Active print-speed level (`spd_lvl`): 1 silent, 2 standard, 3 sport,
    /// 4 ludicrous. How a `print_speed` command is verified.
    pub spd_lvl: Option<i64>,
    /// Name of the running subtask/job (empty when idle).
    pub subtask_name: Option<String>,
    /// The currently-loaded filament (the one the print uses), resolved from
    /// `ams.tray_now` → the matching AMS tray or the external spool. `None` when
    /// nothing is loaded or the report doesn't carry AMS data.
    pub filament: Option<Filament>,
    /// All `lights_report` entries (each `{node, mode}`), e.g.
    /// `chamber_light=off`. This is the printer's *actual* light state — distinct
    /// from a `ledctrl` ACK, which only confirms acceptance (observed: a faulty
    /// unit ACKs `ledctrl` but `lights_report` stays `off`). Look a node up with
    /// [`PrinterStatus::light_mode`].
    pub lights: Vec<LightReport>,
    /// Camera/timelapse settings from the `ipcam` report node. `None` when the
    /// report carries no `ipcam` object.
    pub ipcam: Option<Ipcam>,

    // ── Enriched fields ───────────────────────────────────────────────────
    // Mirror the device report's own shape: flat scalars where `print.*` is
    // flat, nested structs only where the report nests an object. All optional
    // and `skip_serializing_if`-elided so an idle frame stays compact.

    // Fans (besides the part-cooling fan above). A fan reading 0 while RUNNING
    // is a clog / heat-creep symptom worth surfacing.
    /// Aux/part fan #1 (`big_fan1_speed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub big_fan1_speed: Option<i64>,
    /// Chamber/second big fan (`big_fan2_speed`; 0 on the fanless-chamber A1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub big_fan2_speed: Option<i64>,
    /// Hotend/heatbreak fan (`heatbreak_fan_speed`). Dead → heat creep / jams.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heatbreak_fan_speed: Option<i64>,
    /// Packed per-fan gear bitfield (`fan_gear`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fan_gear: Option<i64>,

    // Finer progress / print-phase detail.
    /// Feed-rate override percent (`spd_mag`, 100 = nominal); distinct from the
    /// named `spd_lvl` tier and what actually moves the ETA.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spd_mag: Option<i64>,
    /// Filename currently loaded/printing (`gcode_file`); `None` when idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gcode_file: Option<String>,
    /// Pre-print file preparation/download progress (`gcode_file_prepare_percent`).
    /// Lets a viewer tell "preparing" from a stalled `mc_percent == 0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gcode_file_prepare_percent: Option<i64>,
    /// Coarse motion-controller phase (`mc_print_stage`), cross-checks `stg_cur`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mc_print_stage: Option<i64>,
    /// Finer sub-phase within `mc_print_stage` (`mc_print_sub_stage`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mc_print_sub_stage: Option<i64>,
    /// Current gcode line number (`mc_print_line_number`); increments within a
    /// layer, so it is a fine-grained liveness/stall signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mc_print_line_number: Option<i64>,
    /// Job source/type (`print_type`), e.g. `idle` / `local` / `cloud`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_type: Option<String>,
    /// Queue of upcoming stage ids (`stg`); `stg_cur` is the current one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stg: Vec<i64>,

    // Machine configuration / peripherals.
    /// Installed nozzle diameter in mm (`nozzle_diameter`, e.g. `0.4`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_diameter: Option<String>,
    /// Nozzle material (`nozzle_type`, e.g. `stainless_steel` / `hardened`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_type: Option<String>,
    /// Whether an SD card is present/mounted (`sdcard`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdcard: Option<bool>,
    /// Top-level AMS state machine (`ams_status`): idle/loading/unloading code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ams_status: Option<i64>,
    /// Top-level AMS RFID read state (`ams_rfid_status`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ams_rfid_status: Option<i64>,
    /// Hardware switch / filament-presence / door sensor bits (`hw_switch_state`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hw_switch_state: Option<i64>,

    // Connectivity. (`net`/`net.ip` is deliberately NOT surfaced — it exposes
    // the device address; only the non-identifying RSSI is.)
    /// Wi-Fi signal strength (`wifi_signal`, e.g. `-50dBm`). The capture's
    /// `<redacted>` sentinel is filtered to `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wifi_signal: Option<String>,

    // Job/device identity (for correlating live status with a queued job).
    /// Cloud/print task id (`task_id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Subtask id (`subtask_id`), pairs with `subtask_name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtask_id: Option<String>,
    /// Project id (`project_id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Slicing/print profile id (`profile_id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// The printer's own report counter (`sequence_id`); detects dropped/reordered
    /// reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence_id: Option<String>,
    /// Device lifecycle/build channel (`lifecycle`, e.g. `product`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,

    // Nested objects — these mirror objects the report itself nests.
    /// Full AMS inventory (all units & trays), built from `ams` + `vt_tray`.
    /// `filament` above remains a convenience pointer to the loaded tray.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ams: Option<Ams>,
    /// Firmware-update availability/progress (`upgrade_state`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<Upgrade>,
    /// Peripheral online state (`online`: AHB / RFID bus presence).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub online: Option<Online>,
    /// In-progress file upload/transfer (`upload`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload: Option<Upload>,

    /// Decoded HMS alerts (the device's primary fault/warning channel). Empty
    /// when healthy; separate from `error`/`print_error`. See [`crate::core::hms`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hms: Vec<HmsAlert>,
}

/// One `lights_report` entry: an LED node and its mode (`on`/`off`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts-rs", derive(ts_rs::TS), ts(export))]
pub struct LightReport {
    pub node: String,
    pub mode: String,
}

/// Camera/timelapse settings from the `ipcam` report node (A1/P1: a JPEG-stream
/// camera). The `timelapse` field is the printer's *actual* timelapse setting,
/// which is how an `ipcam_timelapse` command is verified (the ACK alone only
/// says it was accepted — same caveat as the chamber light).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts-rs", derive(ts_rs::TS), ts(export))]
pub struct Ipcam {
    /// `timelapse` mode (`enable`/`disable`) — whether a timelapse is recorded
    /// during prints.
    pub timelapse: Option<String>,
    /// `ipcam_record` mode (`enable`/`disable`).
    pub record: Option<String>,
    /// Stream resolution, e.g. `1080p`.
    pub resolution: Option<String>,
}

/// The loaded filament a print draws from (resolved from `ams.tray_now`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts-rs", derive(ts_rs::TS), ts(export))]
pub struct Filament {
    /// `ams0`..`amsN` for an AMS tray, or `external` for the external spool.
    pub location: String,
    /// Material, e.g. `PLA` (`tray_type`).
    pub material: Option<String>,
    /// Display name, e.g. `PLA Matte` (`tray_sub_brands`).
    pub name: Option<String>,
    /// Colour as reported (`tray_color`), e.g. `000000FF` (RGBA hex).
    pub color: Option<String>,
}

/// A decoded HMS alert (the wire view of [`crate::core::hms::HmsEntry`]).
/// `severity` is the **raw** bits — label conventions conflict across sources,
/// so we expose the number and link to the wiki rather than bundling a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts-rs", derive(ts_rs::TS), ts(export))]
pub struct HmsAlert {
    /// Canonical underscore form, e.g. `HMS_0300_0100_0001_0007`.
    pub code: String,
    /// Hyphen form Bambu shows on-screen, e.g. `0300-0100-0001-0007`.
    pub code_hyphen: String,
    /// Raw severity bits (`code >> 16`); not mapped to a label.
    pub severity: u16,
    /// Originating subsystem: `motion_controller` / `mainboard` / `ams` /
    /// `toolhead` / `xcam` / `unknown:0xNN`.
    pub module: String,
    /// XCAM/micro-LiDAR code (only on X1-class hardware).
    pub is_lidar: bool,
    /// Deep link to Bambu's per-code troubleshooting page.
    pub wiki: String,
    /// Raw `attr` int (escape hatch for re-deriving fields).
    pub attr: u32,
    /// Raw `code` int.
    pub raw_code: u32,
}

impl HmsAlert {
    fn from_entry(e: HmsEntry) -> Self {
        HmsAlert {
            code: e.code_string(),
            code_hyphen: e.code_hyphen(),
            severity: e.severity_raw(),
            module: hms_module_str(e.module()),
            is_lidar: e.is_lidar(),
            wiki: e.wiki_url(),
            attr: e.attr,
            raw_code: e.code,
        }
    }
}

/// The full AMS picture: every unit and tray, the external spool, and the
/// active/target/previous tray pointers (the live colour-swap signal — the tray
/// *array* only arrives in full pushalls, the pointers in deltas).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "ts-rs", derive(ts_rs::TS), ts(export))]
pub struct Ams {
    /// Attached AMS units (`ams.ams[]`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<AmsUnit>,
    /// The external/virtual spool (`vt_tray`), surfaced even when not loaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external: Option<AmsTray>,
    /// Active tray id (`ams.tray_now`): `255` none, `254` external, else a tray id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_tray: Option<String>,
    /// Target tray during a swap (`ams.tray_tar`); `!= active_tray` ⇒ swapping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_tray: Option<String>,
    /// Previous tray (`ams.tray_pre`); reconstructs swap timelines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_tray: Option<String>,
    /// Hex bitfield of attached units (`ams.ams_exist_bits`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ams_exist_bits: Option<String>,
    /// Hex bitfield of occupied slots (`ams.tray_exist_bits`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tray_exist_bits: Option<String>,
    /// Hex bitfield of genuine-Bambu (RFID) trays (`ams.tray_is_bbl_bits`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tray_is_bbl_bits: Option<String>,
}

/// One AMS unit (`ams.ams[]`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "ts-rs", derive(ts_rs::TS), ts(export))]
pub struct AmsUnit {
    /// Physical unit id (`id`).
    pub id: String,
    /// Coarse dryness bucket 1–5 (`humidity`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub humidity: Option<i64>,
    /// Finer raw humidity reading (`humidity_raw`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub humidity_raw: Option<i64>,
    /// Internal temperature °C (`temp`, drying-capable units).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp: Option<f64>,
    /// Remaining/active drying time (`dry_time`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_time: Option<i64>,
    /// The unit's trays/slots (`tray[]`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trays: Vec<AmsTray>,
}

/// One AMS tray/slot (`ams.ams[].tray[]` or `vt_tray`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "ts-rs", derive(ts_rs::TS), ts(export))]
pub struct AmsTray {
    /// Tray id (`id`); what `tray_now`/`tray_pre`/`tray_tar` point at.
    pub id: String,
    /// Material (`tray_type`, e.g. `PLA`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub material: Option<String>,
    /// Display name (`tray_sub_brands`, e.g. `PLA Matte`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Primary colour as RGBA hex (`tray_color`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// All colour segments (`cols`); single-colour spools mirror `color`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cols: Vec<String>,
    /// Remaining filament percent (`remain`); `0`/`-1` mean unknown on the A1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remain: Option<i64>,
    /// Per-tray RFID/load status code (`state`); raw (no verified label table).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<i64>,
    /// Bambu filament-preset id (`tray_info_idx`, e.g. `GFA01`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info_idx: Option<String>,
    /// Short SKU/colour code (`tray_id_name`, e.g. `A01-R1`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_name: Option<String>,
    /// Stable physical-spool id (`tray_uuid`); all-zero ⇒ `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Recommended min nozzle temp (`nozzle_temp_min`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_temp_min: Option<i64>,
    /// Recommended max nozzle temp (`nozzle_temp_max`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_temp_max: Option<i64>,
    /// `true` when this tray is the active one (`id == tray_now`).
    pub is_active: bool,
    /// `true` when this tray is the swap target (`id == tray_tar`).
    pub is_target: bool,
}

/// Firmware-update availability/progress (`upgrade_state`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts-rs", derive(ts_rs::TS), ts(export))]
pub struct Upgrade {
    /// Whether an update is available/pending (`new_version_state`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_version_state: Option<i64>,
    /// Non-zero ⇒ a flash is in progress (`cur_state_code`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cur_state_code: Option<i64>,
    /// Activity status (`status`, e.g. `IDLE`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Failed-update error code (`err_code`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err_code: Option<i64>,
}

/// Peripheral online state (`online`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts-rs", derive(ts_rs::TS), ts(export))]
pub struct Online {
    /// AHB bus present (`ahb`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ahb: Option<bool>,
    /// RFID reader present (`rfid`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rfid: Option<bool>,
    /// Reported version counter (`version`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
}

/// In-progress file upload/transfer (`upload`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts-rs", derive(ts_rs::TS), ts(export))]
pub struct Upload {
    /// Transfer status (`status`, e.g. `idle`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Transfer progress percent (`progress`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<i64>,
    /// Status message (`message`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl PrinterStatus {
    /// Extract a [`PrinterStatus`] from the merged report state (the object that
    /// contains the `print` key). Missing fields become `None`.
    pub fn from_state(state: &Value) -> Self {
        let print = state.get("print");
        let get = |key: &str| print.and_then(|p| p.get(key));
        let stg_cur = get("stg_cur").and_then(as_i64_loose);
        let print_error = get("print_error").and_then(as_i64_loose);

        PrinterStatus {
            gcode_state: get("gcode_state").and_then(as_string),
            print_error,
            error: print_error.and_then(DeviceError::from_code),
            mc_percent: get("mc_percent").and_then(as_i64_loose),
            layer_num: get("layer_num").and_then(as_i64_loose),
            total_layer_num: get("total_layer_num").and_then(as_i64_loose),
            remaining_time_min: get("mc_remaining_time").and_then(as_i64_loose),
            stg_cur,
            stage: stg_cur.and_then(|id| Stage(id).name()),
            home_flag: get("home_flag").and_then(as_i64_loose),
            nozzle_temper: get("nozzle_temper").and_then(Value::as_f64),
            nozzle_target: get("nozzle_target_temper").and_then(Value::as_f64),
            bed_temper: get("bed_temper").and_then(Value::as_f64),
            bed_target: get("bed_target_temper").and_then(Value::as_f64),
            chamber_temper_raw: get("chamber_temper").and_then(Value::as_f64),
            cooling_fan_speed: get("cooling_fan_speed").and_then(as_i64_loose),
            spd_lvl: get("spd_lvl").and_then(as_i64_loose),
            subtask_name: get("subtask_name").and_then(as_string),
            filament: print.and_then(resolve_filament),
            lights: get("lights_report")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|e| {
                            Some(LightReport {
                                node: e.get("node").and_then(Value::as_str)?.to_string(),
                                mode: e.get("mode").and_then(Value::as_str)?.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            ipcam: get("ipcam").map(|ic| Ipcam {
                timelapse: ic.get("timelapse").and_then(as_string),
                record: ic.get("ipcam_record").and_then(as_string),
                resolution: ic.get("resolution").and_then(as_string),
            }),

            // ── Enriched fields ───────────────────────────────────────────
            big_fan1_speed: get("big_fan1_speed").and_then(as_i64_loose),
            big_fan2_speed: get("big_fan2_speed").and_then(as_i64_loose),
            heatbreak_fan_speed: get("heatbreak_fan_speed").and_then(as_i64_loose),
            fan_gear: get("fan_gear").and_then(as_i64_loose),

            spd_mag: get("spd_mag").and_then(as_i64_loose),
            gcode_file: get("gcode_file").and_then(as_nonempty_string),
            gcode_file_prepare_percent: get("gcode_file_prepare_percent").and_then(as_i64_loose),
            mc_print_stage: get("mc_print_stage").and_then(as_i64_loose),
            mc_print_sub_stage: get("mc_print_sub_stage").and_then(as_i64_loose),
            mc_print_line_number: get("mc_print_line_number").and_then(as_i64_loose),
            print_type: get("print_type").and_then(as_string),
            stg: get("stg")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(as_i64_loose).collect())
                .unwrap_or_default(),

            nozzle_diameter: get("nozzle_diameter").and_then(as_string),
            nozzle_type: get("nozzle_type").and_then(as_string),
            sdcard: get("sdcard").and_then(Value::as_bool),
            ams_status: get("ams_status").and_then(as_i64_loose),
            ams_rfid_status: get("ams_rfid_status").and_then(as_i64_loose),
            hw_switch_state: get("hw_switch_state").and_then(as_i64_loose),

            wifi_signal: get("wifi_signal")
                .and_then(as_nonempty_string)
                .filter(|s| s != "<redacted>"),

            task_id: get("task_id").and_then(as_nonempty_string),
            subtask_id: get("subtask_id").and_then(as_nonempty_string),
            project_id: get("project_id").and_then(as_nonempty_string),
            profile_id: get("profile_id").and_then(as_nonempty_string),
            sequence_id: get("sequence_id").and_then(as_string),
            lifecycle: get("lifecycle").and_then(as_string),

            ams: print.and_then(build_ams),
            upgrade: get("upgrade_state").map(|u| Upgrade {
                new_version_state: u.get("new_version_state").and_then(as_i64_loose),
                cur_state_code: u.get("cur_state_code").and_then(as_i64_loose),
                status: u.get("status").and_then(as_string),
                err_code: u.get("err_code").and_then(as_i64_loose),
            }),
            online: get("online").map(|o| Online {
                ahb: o.get("ahb").and_then(Value::as_bool),
                rfid: o.get("rfid").and_then(Value::as_bool),
                version: o.get("version").and_then(as_i64_loose),
            }),
            upload: get("upload").map(|u| Upload {
                status: u.get("status").and_then(as_string),
                progress: u.get("progress").and_then(as_i64_loose),
                message: u.get("message").and_then(as_nonempty_string),
            }),

            hms: decode_report_hms(state)
                .into_iter()
                .map(HmsAlert::from_entry)
                .collect(),
        }
    }

    /// The printer's current timelapse setting (`enable`/`disable`) from the
    /// `ipcam` report node, if present. Used to verify an `ipcam_timelapse`
    /// command took effect.
    pub fn timelapse_mode(&self) -> Option<&str> {
        self.ipcam.as_ref()?.timelapse.as_deref()
    }

    /// The mode (`on`/`off`) of a `lights_report` node (e.g. `chamber_light`),
    /// if reported. Used to verify a `ledctrl` command took effect.
    pub fn light_mode(&self, node: &str) -> Option<&str> {
        self.lights
            .iter()
            .find(|l| l.node == node)
            .map(|l| l.mode.as_str())
    }

    /// The chamber temperature **only if** the model has a real chamber sensor.
    /// Models that merely echo a synthetic `chamber_temper` (A1 / P1) get `None`.
    pub fn real_chamber_temperature(&self, hardware: &HardwareFeatures) -> Option<f64> {
        match hardware.chamber_temperature {
            ChamberTemperature::RealSensor => self.chamber_temper_raw,
            ChamberTemperature::ReportedSynthetic | ChamberTemperature::Unsupported => None,
        }
    }

    /// The parsed coarse job state, if a `gcode_state` was reported.
    pub fn state(&self) -> Option<GcodeState> {
        self.gcode_state.as_deref().map(GcodeState::parse)
    }
}

/// A device-level fault decoded from `print_error` (0 = no error). This is a
/// **separate channel from HMS** — on the A1 mini a failing SD card reported
/// `print_error = 0x0500C010` while `hms` stayed empty, so a status view must
/// surface `print_error` in its own right.
///
/// We don't bundle the full third-party code→text table (sources conflict; same
/// rationale as [`crate::core::hms`]) — but we DO attach a plain-language
/// [`message`](DeviceError::message) for the handful of codes verified on the
/// real device, and always emit the hex + a lookup link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts-rs", derive(ts_rs::TS), ts(export))]
pub struct DeviceError {
    /// Raw `print_error` value.
    pub code: i64,
    /// Conventional hex rendering, e.g. `0x0500C010`.
    pub hex: String,
    /// Plain-language cause — present ONLY for codes we've **verified on the real
    /// device** (the printer's own on-screen message). Unverified codes leave
    /// this `None` and rely on `lookup_url`, so we never surface a guessed cause.
    pub message: Option<String>,
    /// Link to Bambu's official error-code resolver for this code (we don't
    /// bundle the full code→text table — sources conflict — so we point at the
    /// authority instead, the same way [`crate::core::hms`] links to the wiki).
    pub lookup_url: String,
}

/// On-screen text for the `print_error` codes we've confirmed on the real A1
/// mini. Tiny and device-sourced on purpose; grow it only as codes are actually
/// verified (the printer's own wording), never from guesswork.
fn verified_error_message(code: i64) -> Option<&'static str> {
    match code {
        // Both verified on the A1 mini screen (2026-06-16) during AMS filament
        // operations (an end-of-print pullback and a tray-change, respectively).
        0x1200_8014 => Some("couldn't find the filament position in the toolhead"),
        0x1200_8015 => Some("couldn't pull the filament out of the toolhead"),
        _ => None,
    }
}

impl DeviceError {
    /// Build from a raw `print_error`; `None` when the code is 0 (no error).
    pub fn from_code(code: i64) -> Option<Self> {
        (code != 0).then(|| DeviceError {
            code,
            hex: format!("0x{:08X}", code as u32),
            message: verified_error_message(code).map(str::to_string),
            lookup_url: format!(
                "https://e.bambulab.com/query.php?lang=en&e={:08X}",
                code as u32
            ),
        })
    }
}

/// The coarse job state (`gcode_state`). The device sends uppercase tokens; an
/// unrecognised token maps to [`GcodeState::Unknown`] for forward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcodeState {
    Idle,
    Prepare,
    Running,
    Pause,
    Finish,
    Failed,
    Slicing,
    Init,
    Offline,
    Unknown,
}

impl GcodeState {
    /// Parse a `gcode_state` token (case-insensitive).
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_uppercase().as_str() {
            "IDLE" => GcodeState::Idle,
            "PREPARE" => GcodeState::Prepare,
            "RUNNING" => GcodeState::Running,
            "PAUSE" => GcodeState::Pause,
            "FINISH" => GcodeState::Finish,
            "FAILED" => GcodeState::Failed,
            "SLICING" => GcodeState::Slicing,
            "INIT" => GcodeState::Init,
            "OFFLINE" => GcodeState::Offline,
            _ => GcodeState::Unknown,
        }
    }

    /// Whether the print has reached a terminal state (finished or failed).
    pub fn is_terminal(&self) -> bool {
        matches!(self, GcodeState::Finish | GcodeState::Failed)
    }
}

fn as_string(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

/// A string field, but `None` for empty (the device uses `""` for "unset").
fn as_nonempty_string(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Resolve the loaded filament from the `print` object: `ams.tray_now` names the
/// active tray — `254` is the external spool (`vt_tray`), otherwise it matches a
/// tray `id` inside `ams.ams[].tray[]`. Returns `None` when nothing is loaded
/// (`tray_now == 255`) or the report carries no AMS data.
fn resolve_filament(print: &Value) -> Option<Filament> {
    let ams = print.get("ams")?;
    let tray_now = ams.get("tray_now").and_then(Value::as_str)?;

    if tray_now == "254" {
        let vt = print.get("vt_tray")?;
        return Some(Filament {
            location: "external".to_string(),
            material: vt.get("tray_type").and_then(as_nonempty_string),
            name: vt.get("tray_sub_brands").and_then(as_nonempty_string),
            color: vt.get("tray_color").and_then(as_nonempty_string),
        });
    }

    for unit in ams.get("ams").and_then(Value::as_array)?.iter() {
        let Some(trays) = unit.get("tray").and_then(Value::as_array) else {
            continue;
        };
        for tray in trays {
            if tray.get("id").and_then(Value::as_str) == Some(tray_now) {
                return Some(Filament {
                    location: format!("ams{tray_now}"),
                    material: tray.get("tray_type").and_then(as_nonempty_string),
                    name: tray.get("tray_sub_brands").and_then(as_nonempty_string),
                    color: tray.get("tray_color").and_then(as_nonempty_string),
                });
            }
        }
    }
    None
}

/// Accept either a JSON number or a numeric string (the device sends some
/// integer-valued fields, e.g. fan speeds, as strings).
fn as_i64_loose(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Like [`as_i64_loose`] but for floats — the device sends some real-valued
/// fields (e.g. AMS unit temperature, `"0.0"`) as strings. Non-finite values
/// (`NaN`/`inf`) are rejected so they can't poison JSON serialization.
fn as_f64_loose(v: &Value) -> Option<f64> {
    let n = match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    n.is_finite().then_some(n)
}

/// Wire string for an HMS [`Module`]; unknown ids keep their hex so nothing is
/// silently mislabelled (the enum's Rust variant names stay off the wire).
fn hms_module_str(m: Module) -> String {
    match m {
        Module::MotionController => "motion_controller".to_string(),
        Module::Mainboard => "mainboard".to_string(),
        Module::Ams => "ams".to_string(),
        Module::Toolhead => "toolhead".to_string(),
        Module::Xcam => "xcam".to_string(),
        Module::Unknown(n) => format!("unknown:0x{n:02X}"),
    }
}

/// Build the full [`Ams`] view from the `print` object (`ams` + `vt_tray`).
/// `None` only when the report carries neither.
fn build_ams(print: &Value) -> Option<Ams> {
    let ams = print.get("ams");
    let vt = print.get("vt_tray");
    if ams.is_none() && vt.is_none() {
        return None;
    }
    let active = ams.and_then(|a| a.get("tray_now")).and_then(as_string);
    let target = ams.and_then(|a| a.get("tray_tar")).and_then(as_string);
    let previous = ams.and_then(|a| a.get("tray_pre")).and_then(as_string);
    let (act, tar) = (active.as_deref(), target.as_deref());

    let units = ams
        .and_then(|a| a.get("ams"))
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(|u| build_unit(u, act, tar)).collect())
        .unwrap_or_default();

    // Keep the external spool only when it actually carries filament info.
    let external = vt.map(|v| build_tray(v, act, tar)).filter(|t| {
        t.material.is_some() || t.color.is_some() || t.name.is_some() || !t.cols.is_empty()
    });

    let bits = |k: &str| ams.and_then(|a| a.get(k)).and_then(as_nonempty_string);
    Some(Ams {
        units,
        external,
        active_tray: active,
        target_tray: target,
        previous_tray: previous,
        ams_exist_bits: bits("ams_exist_bits"),
        tray_exist_bits: bits("tray_exist_bits"),
        tray_is_bbl_bits: bits("tray_is_bbl_bits"),
    })
}

fn build_unit(u: &Value, active: Option<&str>, target: Option<&str>) -> AmsUnit {
    AmsUnit {
        id: u.get("id").and_then(as_string).unwrap_or_default(),
        humidity: u.get("humidity").and_then(as_i64_loose),
        humidity_raw: u.get("humidity_raw").and_then(as_i64_loose),
        temp: u.get("temp").and_then(as_f64_loose),
        dry_time: u.get("dry_time").and_then(as_i64_loose),
        trays: u
            .get("tray")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().map(|t| build_tray(t, active, target)).collect())
            .unwrap_or_default(),
    }
}

fn build_tray(t: &Value, active: Option<&str>, target: Option<&str>) -> AmsTray {
    let id = t.get("id").and_then(as_string).unwrap_or_default();
    let matches = |p: Option<&str>| !id.is_empty() && p == Some(id.as_str());
    AmsTray {
        is_active: matches(active),
        is_target: matches(target),
        material: t.get("tray_type").and_then(as_nonempty_string),
        name: t.get("tray_sub_brands").and_then(as_nonempty_string),
        color: t.get("tray_color").and_then(as_nonempty_string),
        cols: t
            .get("cols")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(as_nonempty_string).collect())
            .unwrap_or_default(),
        remain: t.get("remain").and_then(as_i64_loose),
        state: t.get("state").and_then(as_i64_loose),
        info_idx: t.get("tray_info_idx").and_then(as_nonempty_string),
        id_name: t.get("tray_id_name").and_then(as_nonempty_string),
        // A `tray_uuid` of all-zeros means "no spool tag" → None.
        uuid: t
            .get("tray_uuid")
            .and_then(as_nonempty_string)
            .filter(|s| s.bytes().any(|b| b != b'0')),
        nozzle_temp_min: t.get("nozzle_temp_min").and_then(as_i64_loose),
        nozzle_temp_max: t.get("nozzle_temp_max").and_then(as_i64_loose),
        id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::report::ReportState;
    use serde_json::json;

    #[test]
    fn filament_resolves_from_ams_tray_now() {
        // tray_now points at AMS tray 3 (PLA Matte black).
        let state = json!({ "print": {
            "ams": {
                "tray_now": "3",
                "ams": [{ "id": "0", "tray": [
                    { "id": "0", "tray_type": "PLA", "tray_sub_brands": "PLA Matte", "tray_color": "DE4343FF" },
                    { "id": "3", "tray_type": "PLA", "tray_sub_brands": "PLA Matte", "tray_color": "000000FF" }
                ]}]
            }
        }});
        let f = PrinterStatus::from_state(&state).filament.unwrap();
        assert_eq!(f.location, "ams3");
        assert_eq!(f.material.as_deref(), Some("PLA"));
        assert_eq!(f.name.as_deref(), Some("PLA Matte"));
        assert_eq!(f.color.as_deref(), Some("000000FF"));
    }

    #[test]
    fn filament_resolves_external_spool() {
        let state = json!({ "print": {
            "ams": { "tray_now": "254", "ams": [] },
            "vt_tray": { "tray_type": "PLA", "tray_sub_brands": "", "tray_color": "161616FF" }
        }});
        let f = PrinterStatus::from_state(&state).filament.unwrap();
        assert_eq!(f.location, "external");
        assert_eq!(f.material.as_deref(), Some("PLA"));
        assert_eq!(f.name, None); // empty sub_brands -> None
        assert_eq!(f.color.as_deref(), Some("161616FF"));
    }

    #[test]
    fn filament_none_when_nothing_loaded() {
        let state = json!({ "print": { "ams": { "tray_now": "255", "ams": [] } } });
        assert_eq!(PrinterStatus::from_state(&state).filament, None);
        // No AMS data at all.
        let bare = json!({ "print": { "gcode_state": "IDLE" } });
        assert_eq!(PrinterStatus::from_state(&bare).filament, None);
    }

    #[test]
    fn device_error_decodes_nonzero_print_error_to_hex() {
        // The real SD-card fault value.
        let e = DeviceError::from_code(0x0500C010).unwrap();
        assert_eq!(e.code, 0x0500C010);
        assert_eq!(e.hex, "0x0500C010");
        assert_eq!(
            e.lookup_url,
            "https://e.bambulab.com/query.php?lang=en&e=0500C010"
        );
        // Zero is "no error".
        assert_eq!(DeviceError::from_code(0), None);
    }

    #[test]
    fn device_error_attaches_a_message_only_for_device_verified_codes() {
        // Verified on the real A1 mini screen (filament/toolhead faults).
        assert_eq!(
            DeviceError::from_code(0x1200_8015).unwrap().message.as_deref(),
            Some("couldn't pull the filament out of the toolhead")
        );
        assert_eq!(
            DeviceError::from_code(0x1200_8014).unwrap().message.as_deref(),
            Some("couldn't find the filament position in the toolhead")
        );
        // An unverified code carries no fabricated message — just hex + the link.
        let u = DeviceError::from_code(0x0500_C010).unwrap();
        assert!(u.message.is_none());
        assert_eq!(u.hex, "0x0500C010");
    }

    #[test]
    fn status_surfaces_a_nonzero_print_error_as_a_typed_error() {
        let state = json!({ "print": { "print_error": 83935248, "gcode_state": "IDLE" } });
        let st = PrinterStatus::from_state(&state);
        assert_eq!(st.print_error, Some(83935248));
        assert_eq!(st.error.as_ref().unwrap().hex, "0x0500C010");
    }

    #[test]
    fn parses_the_real_a1mini_idle_pushall_fixture() {
        let raw = include_str!("../../tests/fixtures/pushall-n1-idle.json");
        let fixture: Value = serde_json::from_str(raw).expect("valid fixture json");
        let mut rs = ReportState::new();
        rs.apply(fixture["message"].clone());

        let st = PrinterStatus::from_state(rs.get());
        assert_eq!(st.gcode_state.as_deref(), Some("IDLE"));
        assert_eq!(st.print_error, Some(0));
        assert_eq!(st.mc_percent, Some(0));
        assert_eq!(st.layer_num, Some(0));
        assert_eq!(st.total_layer_num, Some(0));
        assert_eq!(st.stg_cur, Some(0));
        assert_eq!(st.subtask_name.as_deref(), Some(""));
        // Fan speed arrives as the string "0" and is parsed to a number.
        assert_eq!(st.cooling_fan_speed, Some(0));
        // Real float temperatures from the device.
        assert!((st.bed_temper.unwrap() - 26.53125).abs() < 1e-9);
        assert!((st.nozzle_temper.unwrap() - 27.21875).abs() < 1e-9);
        assert_eq!(st.bed_target, Some(0.0));
        // Raw chamber value is present (5.0) but the A1 mini has no real sensor.
        assert_eq!(st.chamber_temper_raw, Some(5.0));

        // ── Enriched scalars (all string "0"s parse to numbers) ──
        assert_eq!(st.big_fan1_speed, Some(0));
        assert_eq!(st.big_fan2_speed, Some(0));
        assert_eq!(st.heatbreak_fan_speed, Some(0));
        assert_eq!(st.fan_gear, Some(0));
        assert_eq!(st.spd_mag, Some(100));
        assert_eq!(st.mc_print_stage, Some(1)); // string "1"
        assert_eq!(st.print_type.as_deref(), Some("idle"));
        assert_eq!(st.gcode_file, None); // empty "" -> None
        assert_eq!(st.nozzle_diameter.as_deref(), Some("0.4"));
        assert_eq!(st.nozzle_type.as_deref(), Some("stainless_steel"));
        assert_eq!(st.sdcard, Some(true));
        assert_eq!(st.sequence_id.as_deref(), Some("5"));
        assert_eq!(st.lifecycle.as_deref(), Some("product"));
        // wifi_signal is "<redacted>" in the scrubbed fixture -> filtered to None.
        assert_eq!(st.wifi_signal, None);
        assert!(st.hms.is_empty()); // healthy idle device

        // ── Nested objects ──
        let online = st.online.as_ref().unwrap();
        assert_eq!(online.ahb, Some(false));
        assert_eq!(online.rfid, Some(false));
        assert_eq!(online.version, Some(816539411));
        let up = st.upgrade.as_ref().unwrap();
        assert_eq!(up.new_version_state, Some(2));
        assert_eq!(up.status.as_deref(), Some("IDLE"));
        let upload = st.upload.as_ref().unwrap();
        assert_eq!(upload.status.as_deref(), Some("idle"));
        assert_eq!(upload.message, None); // empty -> None

        // ── Full AMS inventory: 1 unit, 4 trays, idle (nothing loaded) ──
        let ams = st.ams.as_ref().unwrap();
        assert_eq!(ams.active_tray.as_deref(), Some("255")); // none loaded
        assert_eq!(ams.tray_exist_bits.as_deref(), Some("f")); // all 4 slots full
        assert_eq!(ams.units.len(), 1);
        let unit = &ams.units[0];
        assert_eq!(unit.id, "0");
        assert_eq!(unit.humidity, Some(5)); // string "5"
        assert_eq!(unit.temp, Some(0.0)); // string "0.0" via as_f64_loose
        assert_eq!(unit.trays.len(), 4);
        let t0 = &unit.trays[0];
        assert_eq!(t0.id, "0");
        assert_eq!(t0.material.as_deref(), Some("PLA"));
        assert_eq!(t0.name.as_deref(), Some("PLA Matte"));
        assert_eq!(t0.color.as_deref(), Some("DE4343FF"));
        assert_eq!(t0.cols, vec!["DE4343FF".to_string()]);
        assert_eq!(t0.info_idx.as_deref(), Some("GFA01"));
        assert_eq!(t0.nozzle_temp_min, Some(190));
        assert_eq!(t0.nozzle_temp_max, Some(230));
        assert!(t0.uuid.is_some()); // real spool tag present
        assert!(!t0.is_active); // nothing loaded (tray_now == 255)
        // The PETG tray keeps its distinct temps.
        assert_eq!(unit.trays[2].material.as_deref(), Some("PETG"));
        assert_eq!(unit.trays[2].nozzle_temp_max, Some(260));
        // External spool surfaced from vt_tray; empty sub_brands & all-zero uuid -> None.
        let ext = ams.external.as_ref().unwrap();
        assert_eq!(ext.id, "254");
        assert_eq!(ext.material.as_deref(), Some("PLA"));
        assert_eq!(ext.name, None);
        assert_eq!(ext.uuid, None);

        // The single-filament convenience pointer is None while idle (tray_now 255).
        assert_eq!(st.filament, None);
    }

    #[test]
    fn real_chamber_temperature_respects_hardware() {
        use crate::core::capability::{ChamberTemperature, HardwareFeatures};
        let st = PrinterStatus::from_state(&json!({ "print": { "chamber_temper": 5.0 } }));
        let a1 = HardwareFeatures {
            lidar: false,
            chamber_temperature: ChamberTemperature::ReportedSynthetic,
            aux_fan: false,
            chamber_fan: false,
        };
        let x1 = HardwareFeatures {
            lidar: true,
            chamber_temperature: ChamberTemperature::RealSensor,
            aux_fan: true,
            chamber_fan: true,
        };
        assert_eq!(st.real_chamber_temperature(&a1), None); // synthetic -> hidden
        assert_eq!(st.real_chamber_temperature(&x1), Some(5.0)); // real sensor -> exposed
    }

    #[test]
    fn lights_report_parses_and_is_looked_up_by_node() {
        let st = PrinterStatus::from_state(&json!({ "print": { "lights_report": [
            { "node": "chamber_light", "mode": "off" },
            { "node": "work_light", "mode": "on" }
        ]}}));
        assert_eq!(st.light_mode("chamber_light"), Some("off"));
        assert_eq!(st.light_mode("work_light"), Some("on"));
        // Unknown node -> None.
        assert_eq!(st.light_mode("logo_light"), None);
        // No lights_report -> empty, no panic.
        let bare = PrinterStatus::from_state(&json!({ "print": {} }));
        assert!(bare.lights.is_empty());
        assert_eq!(bare.light_mode("chamber_light"), None);
    }

    #[test]
    fn ipcam_node_parses_timelapse_and_record() {
        let st = PrinterStatus::from_state(&json!({ "print": { "ipcam": {
            "timelapse": "disable", "ipcam_record": "enable", "resolution": "1080p"
        }}}));
        let ic = st.ipcam.as_ref().unwrap();
        assert_eq!(ic.timelapse.as_deref(), Some("disable"));
        assert_eq!(ic.record.as_deref(), Some("enable"));
        assert_eq!(ic.resolution.as_deref(), Some("1080p"));
        assert_eq!(st.timelapse_mode(), Some("disable"));
        // No ipcam node -> None.
        assert_eq!(
            PrinterStatus::from_state(&json!({ "print": {} })).ipcam,
            None
        );
        assert_eq!(
            PrinterStatus::from_state(&json!({ "print": {} })).timelapse_mode(),
            None
        );
    }

    #[test]
    fn missing_fields_become_none() {
        let st = PrinterStatus::from_state(&json!({ "print": { "gcode_state": "RUNNING" } }));
        assert_eq!(st.gcode_state.as_deref(), Some("RUNNING"));
        assert_eq!(st.bed_temper, None);
        assert_eq!(st.mc_percent, None);
    }

    #[test]
    fn empty_or_unrelated_state_is_all_none() {
        let st = PrinterStatus::from_state(&json!({}));
        assert_eq!(st, PrinterStatus::default());
    }

    #[test]
    fn numeric_strings_and_numbers_both_parse() {
        let st = PrinterStatus::from_state(&json!({
            "print": { "cooling_fan_speed": "85", "mc_percent": 42 }
        }));
        assert_eq!(st.cooling_fan_speed, Some(85)); // from string
        assert_eq!(st.mc_percent, Some(42)); // from number
    }

    #[test]
    fn gcode_state_parses_and_classifies_terminality() {
        assert_eq!(GcodeState::parse("IDLE"), GcodeState::Idle);
        assert_eq!(GcodeState::parse("running"), GcodeState::Running);
        assert_eq!(GcodeState::parse("WAT"), GcodeState::Unknown);
        assert!(GcodeState::parse("FINISH").is_terminal());
        assert!(GcodeState::parse("FAILED").is_terminal());
        assert!(!GcodeState::parse("RUNNING").is_terminal());
    }

    #[test]
    fn printer_status_exposes_typed_state() {
        let st = PrinterStatus::from_state(&json!({ "print": { "gcode_state": "RUNNING" } }));
        assert_eq!(st.state(), Some(GcodeState::Running));
        assert_eq!(PrinterStatus::from_state(&json!({})).state(), None);
    }

    #[test]
    fn status_reflects_merged_deltas() {
        let mut rs = ReportState::new();
        rs.apply(json!({ "print": { "gcode_state": "RUNNING", "mc_percent": 10 } }));
        rs.apply(json!({ "print": { "mc_percent": 55 } })); // delta
        let st = PrinterStatus::from_state(rs.get());
        assert_eq!(st.gcode_state.as_deref(), Some("RUNNING"));
        assert_eq!(st.mc_percent, Some(55));
    }

    #[test]
    fn hms_alerts_are_decoded_into_the_status() {
        // attr=0x03000100, code=0x00010007 -> the wiki's heatbed-temp-abnormal code.
        let st = PrinterStatus::from_state(&json!({ "print": { "hms": [
            { "attr": 50331904, "code": 65543 },
            { "attr": 0, "code": 0 } // zero padding -> skipped
        ]}}));
        assert_eq!(st.hms.len(), 1);
        let a = &st.hms[0];
        assert_eq!(a.code, "HMS_0300_0100_0001_0007");
        assert_eq!(a.code_hyphen, "0300-0100-0001-0007");
        assert_eq!(a.module, "motion_controller");
        assert_eq!(a.severity, 1); // 0x0001
        assert!(!a.is_lidar);
        assert!(a.wiki.contains("0300_0100_0001_0007"));
        assert_eq!(a.attr, 50331904);
        // Healthy device -> empty list, and it is elided from the JSON.
        let healthy = PrinterStatus::from_state(&json!({ "print": { "hms": [] } }));
        assert!(healthy.hms.is_empty());
        let v = serde_json::to_value(&healthy).unwrap();
        assert!(v.get("hms").is_none(), "empty hms should be skipped");
    }

    #[test]
    fn hms_unknown_module_keeps_its_hex() {
        // attr module byte 0x11 is community-only / unverified -> unknown:0x11.
        let st = PrinterStatus::from_state(&json!({ "print": { "hms": [
            { "attr": 0x1100_0000u32, "code": 0x0002_0001u32 }
        ]}}));
        assert_eq!(st.hms[0].module, "unknown:0x11");
    }

    #[test]
    fn ams_derives_active_and_target_during_a_swap() {
        // tray_now=1, tray_tar=3 -> a colour swap from tray 1 to tray 3.
        let st = PrinterStatus::from_state(&json!({ "print": { "ams": {
            "tray_now": "1", "tray_tar": "3", "tray_pre": "1",
            "ams": [{ "id": "0", "tray": [
                { "id": "0", "tray_type": "PLA", "tray_color": "DE4343FF" },
                { "id": "1", "tray_type": "PLA", "tray_color": "000000FF" },
                { "id": "3", "tray_type": "PETG", "tray_color": "D6ABFF80" }
            ]}]
        }}}));
        let ams = st.ams.as_ref().unwrap();
        assert_eq!(ams.active_tray.as_deref(), Some("1"));
        assert_eq!(ams.target_tray.as_deref(), Some("3"));
        let trays = &ams.units[0].trays;
        assert!(trays[1].is_active && !trays[1].is_target); // currently printing
        assert!(trays[2].is_target && !trays[2].is_active); // swapping to it
        assert!(!trays[0].is_active && !trays[0].is_target);
        // The convenience pointer still resolves the loaded tray.
        assert_eq!(st.filament.as_ref().unwrap().location, "ams1");
    }

    #[test]
    fn ams_is_none_without_ams_or_vt_tray() {
        let st = PrinterStatus::from_state(&json!({ "print": { "gcode_state": "IDLE" } }));
        assert_eq!(st.ams, None);
    }

    #[test]
    fn wifi_signal_real_value_passes_through_but_sentinel_is_dropped() {
        let real = PrinterStatus::from_state(&json!({ "print": { "wifi_signal": "-50dBm" } }));
        assert_eq!(real.wifi_signal.as_deref(), Some("-50dBm"));
        let scrubbed =
            PrinterStatus::from_state(&json!({ "print": { "wifi_signal": "<redacted>" } }));
        assert_eq!(scrubbed.wifi_signal, None);
    }

    #[test]
    fn loose_float_parsing_accepts_string_and_number() {
        assert_eq!(as_f64_loose(&json!("0.0")), Some(0.0));
        assert_eq!(as_f64_loose(&json!(28.5)), Some(28.5));
        assert_eq!(as_f64_loose(&json!(true)), None);
    }

    #[test]
    fn enriched_fields_are_elided_from_an_idle_payload() {
        // A bare RUNNING status should not carry a wall of nulls for the new
        // optional fields (skip_serializing_if keeps WS frames compact).
        let st = PrinterStatus::from_state(&json!({ "print": { "gcode_state": "RUNNING" } }));
        let v = serde_json::to_value(&st).unwrap();
        for absent in [
            "ams",
            "upgrade",
            "online",
            "upload",
            "wifi_signal",
            "big_fan1_speed",
        ] {
            assert!(
                v.get(absent).is_none(),
                "{absent} should be skipped when absent"
            );
        }
    }
}
