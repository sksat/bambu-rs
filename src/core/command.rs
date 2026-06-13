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
    /// Pause the current print.
    Pause,
    /// Resume a paused print.
    Resume,
    /// Stop (cancel) the current print — irreversible.
    Stop,
    /// Send a single raw G-code line (`print.gcode_line`).
    GcodeLine(String),
    /// Turn the chamber light on/off (`system.ledctrl`).
    ChamberLight(bool),
}

impl Command {
    /// The top-level message category — the single JSON key this command nests
    /// under, and the key its ACK comes back under (`print` commands are ACKed
    /// at `/print/...`, `system` commands at `/system/...`).
    pub fn category(&self) -> &'static str {
        match self {
            Command::PushAll => "pushing",
            Command::Pause | Command::Resume | Command::Stop | Command::GcodeLine(_) => "print",
            Command::ChamberLight(_) => "system",
        }
    }

    /// Render this command to its request-payload JSON, stamping `sequence_id`.
    pub fn to_payload(&self, sequence_id: &str) -> Value {
        match self {
            Command::PushAll => json!({
                "pushing": { "sequence_id": sequence_id, "command": "pushall" }
            }),
            Command::Pause => print_command(sequence_id, "pause", ""),
            Command::Resume => print_command(sequence_id, "resume", ""),
            Command::Stop => print_command(sequence_id, "stop", ""),
            Command::GcodeLine(line) => print_command(sequence_id, "gcode_line", line),
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
        assert_eq!(Command::ChamberLight(true).category(), "system");
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
