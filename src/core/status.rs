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
use serde::Serialize;
use serde_json::Value;

/// The fields of a printer `print` report that matter for monitoring.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PrinterStatus {
    /// Coarse job state, e.g. `IDLE`, `RUNNING`, `PAUSE`, `FINISH`, `FAILED`.
    pub gcode_state: Option<String>,
    /// `print_error` code (0 = none).
    pub print_error: Option<i64>,
    /// Progress percentage (`mc_percent`).
    pub mc_percent: Option<i64>,
    /// Current layer / total layers.
    pub layer_num: Option<i64>,
    pub total_layer_num: Option<i64>,
    /// Remaining time in minutes (`mc_remaining_time`).
    pub remaining_time_min: Option<i64>,
    /// Current stage id (`stg_cur`); decoded to a name elsewhere.
    pub stg_cur: Option<i64>,
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
}

impl PrinterStatus {
    /// Extract a [`PrinterStatus`] from the merged report state (the object that
    /// contains the `print` key). Missing fields become `None`.
    pub fn from_state(state: &Value) -> Self {
        let print = state.get("print");
        let get = |key: &str| print.and_then(|p| p.get(key));

        PrinterStatus {
            gcode_state: get("gcode_state").and_then(as_string),
            print_error: get("print_error").and_then(as_i64_loose),
            mc_percent: get("mc_percent").and_then(as_i64_loose),
            layer_num: get("layer_num").and_then(as_i64_loose),
            total_layer_num: get("total_layer_num").and_then(as_i64_loose),
            remaining_time_min: get("mc_remaining_time").and_then(as_i64_loose),
            stg_cur: get("stg_cur").and_then(as_i64_loose),
            nozzle_temper: get("nozzle_temper").and_then(Value::as_f64),
            nozzle_target: get("nozzle_target_temper").and_then(Value::as_f64),
            bed_temper: get("bed_temper").and_then(Value::as_f64),
            bed_target: get("bed_target_temper").and_then(Value::as_f64),
            chamber_temper_raw: get("chamber_temper").and_then(Value::as_f64),
            cooling_fan_speed: get("cooling_fan_speed").and_then(as_i64_loose),
            subtask_name: get("subtask_name").and_then(as_string),
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
