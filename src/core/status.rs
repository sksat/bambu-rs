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
use crate::core::stage::Stage;
use serde::Serialize;
use serde_json::Value;

/// The fields of a printer `print` report that matter for monitoring.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
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
    /// Name of the running subtask/job (empty when idle).
    pub subtask_name: Option<String>,
    /// The currently-loaded filament (the one the print uses), resolved from
    /// `ams.tray_now` → the matching AMS tray or the external spool. `None` when
    /// nothing is loaded or the report doesn't carry AMS data.
    pub filament: Option<Filament>,
    /// Reported chamber-light mode (`on`/`off`) from `lights_report`. This is the
    /// printer's *actual* light state — distinct from a `ledctrl` ACK, which only
    /// confirms the command was accepted (observed: a faulty unit ACKs `ledctrl`
    /// but `lights_report` stays `off`).
    pub chamber_light: Option<String>,
}

/// The loaded filament a print draws from (resolved from `ams.tray_now`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
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
            subtask_name: get("subtask_name").and_then(as_string),
            filament: print.and_then(resolve_filament),
            chamber_light: get("lights_report")
                .and_then(Value::as_array)
                .and_then(|arr| {
                    arr.iter()
                        .find(|e| e.get("node").and_then(Value::as_str) == Some("chamber_light"))
                })
                .and_then(|e| e.get("mode").and_then(as_string)),
        }
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
/// We deliberately don't bundle a code→text table (sources conflict; same
/// rationale as [`crate::core::hms`]); the hex code is emitted for lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceError {
    /// Raw `print_error` value.
    pub code: i64,
    /// Conventional hex rendering, e.g. `0x0500C010`.
    pub hex: String,
    /// Link to Bambu's official error-code resolver for this code (we don't
    /// bundle a code→text table — sources conflict — so we point at the
    /// authority instead, the same way [`crate::core::hms`] links to the wiki).
    pub lookup_url: String,
}

impl DeviceError {
    /// Build from a raw `print_error`; `None` when the code is 0 (no error).
    pub fn from_code(code: i64) -> Option<Self> {
        (code != 0).then(|| DeviceError {
            code,
            hex: format!("0x{:08X}", code as u32),
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
}
