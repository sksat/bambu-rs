//! MQTT command envelopes — the JSON published to `device/{serial}/request`.
//!
//! Each command renders to `{ "<category>": { "sequence_id": .., "command": ..,
//! .. } }`. The printer echoes `sequence_id` back in its report, which is how we
//! match a command to its effect (verify-by-reread).
//!
//! Shapes here are derived from the OpenBambuAPI spec and **must be confirmed
//! against a real A1 mini**; where the device disagrees with the spec, the
//! device wins.

use serde_json::{Value, json};

/// Monotonic allocator for the `sequence_id` field. Owned by the session/client
/// — kept out of [`Command`] so commands stay pure, data-only values.
#[derive(Debug, Default)]
pub struct SequenceIds {
    next: u64,
}

impl SequenceIds {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next sequence id. Bambu's `sequence_id` is a string.
    pub fn next_id(&mut self) -> String {
        let id = self.next;
        self.next += 1;
        id.to_string()
    }
}

/// A control/query command sent to the printer.
///
/// Pure and data-only: rendering to JSON ([`Command::to_payload`]) takes the
/// caller-allocated `sequence_id`, so the value itself carries no mutable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Request a full state snapshot (`pushing.pushall`).
    PushAll,
    /// Request the module/firmware inventory (`info.get_version`). A read; the
    /// response comes back under `/info` with a `module[]` array.
    GetVersion,
    /// Pause the current print.
    Pause,
    /// Resume a paused print.
    Resume,
    /// Stop (cancel) the current print — irreversible.
    Stop,
    /// Send a single raw G-code line (`print.gcode_line`).
    GcodeLine(String),
    /// Print a raw G-code file already on the printer (`print.gcode_file`,
    /// single-material — no AMS mapping). The value is the on-printer path.
    GcodeFile(String),
    /// Start a print of a sliced 3MF on the printer (`print.project_file`).
    ProjectFile(ProjectFile),
    /// Turn the chamber light on/off (`system.ledctrl`).
    ChamberLight(bool),
    /// Reboot the printer (`system.reboot`). Undocumented in the spec but
    /// **accepted by the A1 mini** (observed). The connection drops and the
    /// printer restarts, so there is no ACK — send it fire-and-forget.
    Reboot,
    /// Run printer calibration (`print.calibration`, an `option` bitmask).
    /// (Lidar — bit 0 — is X1-only and intentionally not exposed here.)
    Calibration {
        /// Bed leveling (bit 1 = 2).
        bed_level: bool,
        /// Vibration compensation (bit 2 = 4).
        vibration: bool,
        /// Motor-noise calibration (bit 3 = 8).
        motor_noise: bool,
    },
}

/// Parameters for `print.project_file` — start a sliced `.gcode.3mf` that is
/// already on the printer's storage.
///
/// Field shapes are spec-derived (OpenBambuAPI) and must be confirmed on real
/// hardware (the device is the source of truth — start the print and verify it
/// reaches `RUNNING`). Calibration flags default to on, matching a normal slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFile {
    /// URL of the file on the printer, e.g. `ftp:///cache/foo.gcode.3mf`.
    pub url: String,
    /// Plate number; the gcode is read from `Metadata/plate_{plate}.gcode`.
    pub plate: u32,
    /// Job name shown on the printer.
    pub subtask_name: String,
    /// Lowercase-hex md5 of the plate gcode (empty to skip the check).
    pub md5: String,
    /// Build-plate type (`auto`, or a specific plate name).
    pub bed_type: String,
    /// Use the AMS, with a per-filament tray mapping (`-1` = external spool).
    pub use_ams: bool,
    pub ams_mapping: Vec<i32>,
    pub timelapse: bool,
    pub flow_cali: bool,
    pub bed_leveling: bool,
    pub vibration_cali: bool,
    pub layer_inspect: bool,
}

impl ProjectFile {
    /// A minimal project print: no AMS, `auto` bed type, calibrations on.
    pub fn new(url: impl Into<String>, plate: u32, subtask_name: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            plate,
            subtask_name: subtask_name.into(),
            md5: String::new(),
            bed_type: "auto".to_string(),
            use_ams: false,
            ams_mapping: Vec::new(),
            timelapse: false,
            flow_cali: true,
            bed_leveling: true,
            vibration_cali: true,
            layer_inspect: true,
        }
    }
}

impl Command {
    /// The top-level message category — the single JSON key this command nests
    /// under, and the key its ACK comes back under (`print` commands are ACKed
    /// at `/print/...`, `system` commands at `/system/...`).
    pub fn category(&self) -> &'static str {
        match self {
            Command::PushAll => "pushing",
            Command::GetVersion => "info",
            Command::Pause
            | Command::Resume
            | Command::Stop
            | Command::GcodeLine(_)
            | Command::GcodeFile(_)
            | Command::ProjectFile(_)
            | Command::Calibration { .. } => "print",
            Command::ChamberLight(_) | Command::Reboot => "system",
        }
    }

    /// Render this command to its request-payload JSON, stamping `sequence_id`.
    pub fn to_payload(&self, sequence_id: &str) -> Value {
        match self {
            Command::PushAll => json!({
                "pushing": { "sequence_id": sequence_id, "command": "pushall" }
            }),
            Command::GetVersion => json!({
                "info": { "sequence_id": sequence_id, "command": "get_version" }
            }),
            Command::Pause => print_command(sequence_id, "pause", ""),
            Command::Resume => print_command(sequence_id, "resume", ""),
            Command::Stop => print_command(sequence_id, "stop", ""),
            Command::GcodeLine(line) => print_command(sequence_id, "gcode_line", line),
            Command::GcodeFile(path) => print_command(sequence_id, "gcode_file", path),
            Command::ProjectFile(p) => json!({
                "print": {
                    "sequence_id": sequence_id,
                    "command": "project_file",
                    "param": format!("Metadata/plate_{}.gcode", p.plate),
                    "url": p.url,
                    "subtask_name": p.subtask_name,
                    "md5": p.md5,
                    "bed_type": p.bed_type,
                    "timelapse": p.timelapse,
                    "flow_cali": p.flow_cali,
                    "bed_leveling": p.bed_leveling,
                    "vibration_cali": p.vibration_cali,
                    "layer_inspect": p.layer_inspect,
                    "use_ams": p.use_ams,
                    "ams_mapping": p.ams_mapping,
                    "project_id": "0",
                    "profile_id": "0",
                    "task_id": "0",
                    "subtask_id": "0",
                }
            }),
            Command::Calibration {
                bed_level,
                vibration,
                motor_noise,
            } => {
                let option = i64::from(*bed_level) * 2
                    + i64::from(*vibration) * 4
                    + i64::from(*motor_noise) * 8;
                json!({
                    "print": { "sequence_id": sequence_id, "command": "calibration", "option": option }
                })
            }
            Command::ChamberLight(on) => json!({
                "system": {
                    "sequence_id": sequence_id,
                    "command": "ledctrl",
                    "led_node": "chamber_light",
                    "led_mode": if *on { "on" } else { "off" },
                    "led_on_time": 500,
                    "led_off_time": 500,
                    "loop_times": 0,
                    "interval_time": 0,
                }
            }),
            Command::Reboot => json!({
                "system": { "sequence_id": sequence_id, "command": "reboot" }
            }),
        }
    }
}

/// Build a `print.<command>` envelope carrying a `param` field.
fn print_command(sequence_id: &str, command: &str, param: &str) -> Value {
    json!({
        "print": { "sequence_id": sequence_id, "command": command, "param": param }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sequence_ids_are_monotonic_strings_from_zero() {
        let mut ids = SequenceIds::new();
        assert_eq!(ids.next_id(), "0");
        assert_eq!(ids.next_id(), "1");
        assert_eq!(ids.next_id(), "2");
    }

    #[test]
    fn categories_match_the_envelope_key() {
        assert_eq!(Command::PushAll.category(), "pushing");
        assert_eq!(Command::Pause.category(), "print");
        assert_eq!(Command::GcodeLine("G28".into()).category(), "print");
        assert_eq!(Command::GcodeFile("/x".into()).category(), "print");
        assert_eq!(
            Command::ProjectFile(ProjectFile::new("u", 1, "n")).category(),
            "print"
        );
        assert_eq!(Command::ChamberLight(true).category(), "system");
    }

    #[test]
    fn calibration_option_is_a_bitmask() {
        let v = Command::Calibration {
            bed_level: true,
            vibration: true,
            motor_noise: false,
        }
        .to_payload("1");
        assert_eq!(v["print"]["command"], "calibration");
        assert_eq!(v["print"]["option"], 6); // 2 (bed) | 4 (vibration)
        assert_eq!(
            Command::Calibration {
                bed_level: false,
                vibration: false,
                motor_noise: true,
            }
            .to_payload("1")["print"]["option"],
            8
        );
    }

    #[test]
    fn gcode_file_payload() {
        assert_eq!(
            Command::GcodeFile("/cache/foo.gcode".into()).to_payload("2"),
            json!({ "print": { "sequence_id": "2", "command": "gcode_file", "param": "/cache/foo.gcode" } })
        );
    }

    #[test]
    fn project_file_payload_has_plate_and_lan_ids() {
        let pf = ProjectFile::new("ftp:///cache/x.gcode.3mf", 2, "x job");
        let v = Command::ProjectFile(pf).to_payload("3");
        let p = &v["print"];
        assert_eq!(p["command"], "project_file");
        assert_eq!(p["sequence_id"], "3");
        assert_eq!(p["param"], "Metadata/plate_2.gcode");
        assert_eq!(p["url"], "ftp:///cache/x.gcode.3mf");
        assert_eq!(p["subtask_name"], "x job");
        assert_eq!(p["use_ams"], false);
        assert_eq!(p["task_id"], "0"); // LAN SD print uses "0" ids
        assert!(p["ams_mapping"].is_array());
    }

    #[test]
    fn get_version_is_an_info_read() {
        assert_eq!(Command::GetVersion.category(), "info");
        assert_eq!(
            Command::GetVersion.to_payload("1"),
            json!({ "info": { "sequence_id": "1", "command": "get_version" } })
        );
    }

    #[test]
    fn pushall_payload() {
        assert_eq!(
            Command::PushAll.to_payload("0"),
            json!({ "pushing": { "sequence_id": "0", "command": "pushall" } })
        );
    }

    #[test]
    fn pause_resume_stop_payloads() {
        assert_eq!(
            Command::Pause.to_payload("3"),
            json!({ "print": { "sequence_id": "3", "command": "pause", "param": "" } })
        );
        assert_eq!(
            Command::Resume.to_payload("4"),
            json!({ "print": { "sequence_id": "4", "command": "resume", "param": "" } })
        );
        assert_eq!(
            Command::Stop.to_payload("5"),
            json!({ "print": { "sequence_id": "5", "command": "stop", "param": "" } })
        );
    }

    #[test]
    fn gcode_line_payload_carries_the_line_in_param() {
        assert_eq!(
            Command::GcodeLine("M104 S210".to_string()).to_payload("7"),
            json!({ "print": { "sequence_id": "7", "command": "gcode_line", "param": "M104 S210" } })
        );
    }

    #[test]
    fn chamber_light_on_and_off_payloads() {
        let on = Command::ChamberLight(true).to_payload("8");
        assert_eq!(on["system"]["command"], "ledctrl");
        assert_eq!(on["system"]["led_node"], "chamber_light");
        assert_eq!(on["system"]["led_mode"], "on");
        assert_eq!(on["system"]["sequence_id"], "8");

        let off = Command::ChamberLight(false).to_payload("9");
        assert_eq!(off["system"]["led_mode"], "off");
    }

    #[test]
    fn reboot_is_a_system_command() {
        assert_eq!(Command::Reboot.category(), "system");
        assert_eq!(
            Command::Reboot.to_payload("3"),
            json!({ "system": { "sequence_id": "3", "command": "reboot" } })
        );
    }

    #[test]
    fn sequence_id_is_serialised_as_a_string_not_a_number() {
        let payload = Command::PushAll.to_payload("42");
        assert!(payload["pushing"]["sequence_id"].is_string());
    }

    #[test]
    fn rendering_does_not_consume_or_mutate_the_command() {
        let cmd = Command::GcodeLine("G28".to_string());
        let _ = cmd.to_payload("0");
        // Still usable / unchanged afterwards.
        assert_eq!(cmd, Command::GcodeLine("G28".to_string()));
    }
}
