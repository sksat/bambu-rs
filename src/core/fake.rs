//! `FakePrinter` — a tiny state-machine test double that emits the report
//! messages a real A1 mini would, so verify-by-reread orchestration
//! ([`crate::core::session::VerifySession`]) can be tested without a network.
//!
//! It models just enough state (`gcode_state`, `print_error`) to drive the
//! verify predicates, and produces the messages that matter: the full `pushall`
//! snapshot, a command ACK (success/fail, under the command's category), and the
//! effect delta each effectful command would cause (so `verify::evaluate`
//! observes the change). Test-only.

use serde_json::{Map, Value, json};

use crate::core::command::Command;

/// A simulated printer in a known state.
pub struct FakePrinter {
    gcode_state: String,
    print_error: i64,
}

impl FakePrinter {
    /// An idle, fault-free printer.
    pub fn idle() -> Self {
        Self {
            gcode_state: "IDLE".to_string(),
            print_error: 0,
        }
    }

    /// An idle printer that already has a `print_error` in its baseline snapshot.
    pub fn with_error(code: i64) -> Self {
        Self {
            gcode_state: "IDLE".to_string(),
            print_error: code,
        }
    }

    /// The full `pushall` response (`push_status`, `msg == 0`).
    pub fn snapshot(&self) -> Value {
        json!({ "print": {
            "command": "push_status",
            "msg": 0,
            "gcode_state": self.gcode_state,
            "print_error": self.print_error,
        }})
    }

    /// A command ACK under the command's own category, echoing `seq`.
    pub fn ack(&self, cmd: &Command, seq: &str, success: bool, reason: &str) -> Value {
        let mut m = Map::new();
        m.insert(
            cmd.category().to_string(),
            json!({
                "sequence_id": seq,
                "result": if success { "success" } else { "fail" },
                "reason": reason,
            }),
        );
        Value::Object(m)
    }

    /// The effect delta this command would cause — mutates the simulated state so
    /// the resulting report makes `verify::evaluate` observe the effect.
    pub fn effect_delta(&mut self, cmd: &Command) -> Value {
        match cmd {
            Command::ProjectFile(_) | Command::GcodeFile(_) | Command::Calibration { .. } => {
                self.gcode_state = "RUNNING".to_string();
                self.print_delta(json!({ "gcode_state": "RUNNING" }))
            }
            Command::Pause => {
                self.gcode_state = "PAUSE".to_string();
                self.print_delta(json!({ "gcode_state": "PAUSE" }))
            }
            Command::Resume => {
                self.gcode_state = "RUNNING".to_string();
                self.print_delta(json!({ "gcode_state": "RUNNING" }))
            }
            Command::Stop => {
                self.gcode_state = "FINISH".to_string();
                self.print_delta(json!({ "gcode_state": "FINISH" }))
            }
            Command::PrintSpeed(level) => self.print_delta(json!({ "spd_lvl": level.level() })),
            Command::Led { node, on } => self.print_delta(json!({
                "lights_report": [ { "node": node.as_str(), "mode": if *on { "on" } else { "off" } } ]
            })),
            Command::IpcamTimelapse(control) => {
                self.print_delta(json!({ "ipcam": { "timelapse": control.as_str() } }))
            }
            // ACK-only commands have no readable effect.
            _ => self.print_delta(json!({})),
        }
    }

    /// A delta that introduces a new `print_error` (e.g. the SD-card fault) while
    /// the job stays idle — the "ACKed then faulted" scenario.
    pub fn new_error_delta(&mut self, code: i64) -> Value {
        self.print_error = code;
        self.print_delta(json!({ "print_error": code }))
    }

    /// Wrap changed `print` fields as a delta message (`msg == 1`).
    fn print_delta(&self, fields: Value) -> Value {
        let mut print = fields.as_object().cloned().unwrap_or_default();
        print.insert("command".to_string(), json!("push_status"));
        print.insert("msg".to_string(), json!(1));
        json!({ "print": Value::Object(print) })
    }
}
