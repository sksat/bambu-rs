//! Starting a print (the write side, separate from simple control because it
//! carries file/plate/AMS parameters and needs a fresh MQTT connection). Behind
//! a seam so the API is testable: tests/`--fake` use [`FakeStarter`]; live mode
//! uses [`LiveStarter`] (`project_file`/`gcode_file` + verify).
//!
//! Safety lives in the HTTP handler (confirm gate, idle check, AMS-map range);
//! this just builds the command and verifies it.

use std::time::Duration;

use super::control::ControlError;
use crate::client::LanMqttClient;
use crate::config::ResolvedTarget;
use crate::core::command::Command as ProtoCommand;
use crate::core::project::PlateInspection;
use crate::core::session::CommandOutcome;
use crate::core::start::{self, PrintStartParams};

/// A resolved print-start request.
pub struct StartRequest {
    pub file: String,
    pub plate: u32,
    pub use_ams: bool,
    pub ams_map: Vec<i32>,
    pub bed_type: String,
    /// Enable the printer-side timelapse flag. Beyond recording the built-in
    /// camera, this arms the sliced timelapse gcode — on Smooth mode that's the
    /// per-layer park with a spiral Z-hop + prime-tower wipe, which is skipped
    /// entirely when the flag is off (and the head then scrapes the print).
    pub timelapse: bool,
    /// Plate inspection (when we have the file's bytes, e.g. an upload-then-start):
    /// its plate-gcode md5 is stamped into the `project_file` so the printer
    /// verifies the file. `None` for "file already on the printer" starts.
    pub inspection: Option<PlateInspection>,
}

impl StartRequest {
    /// Build the MQTT command via the shared [`core::start`](crate::core::start)
    /// builder: `project_file` for a `.3mf`, `gcode_file` for raw `.gcode`, with
    /// the plate-gcode md5 folded in when an inspection is present.
    pub fn to_command(&self) -> ProtoCommand {
        let params = PrintStartParams {
            file: self.file.clone(),
            plate: self.plate,
            use_ams: self.use_ams,
            ams_map: self.ams_map.clone(),
            bed_type: self.bed_type.clone(),
            timelapse: self.timelapse,
        };
        start::build_command(&params, self.inspection.as_ref())
    }
}

/// Starts prints. Blocking — call from `spawn_blocking`.
pub trait Starter: Send + Sync {
    fn start(&self, req: &StartRequest) -> Result<CommandOutcome, ControlError>;
}

/// Drives a real printer over LAN MQTT.
pub struct LiveStarter {
    target: ResolvedTarget,
}

impl LiveStarter {
    pub fn new(target: ResolvedTarget) -> Self {
        Self { target }
    }
}

impl Starter for LiveStarter {
    fn start(&self, req: &StartRequest) -> Result<CommandOutcome, ControlError> {
        LanMqttClient::new(self.target.clone())
            .with_timeout(Duration::from_secs(30))
            .send_and_verify(&req.to_command())
            .map_err(|e| ControlError::Transport(e.to_string()))
    }
}

/// A canned starter for `--fake` mode and tests.
pub struct FakeStarter;

impl Starter for FakeStarter {
    fn start(&self, _req: &StartRequest) -> Result<CommandOutcome, ControlError> {
        Ok(CommandOutcome::Verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_mf_builds_a_project_file_with_ftp_url_and_ams() {
        let req = StartRequest {
            file: "/cache/coin.gcode.3mf".to_string(),
            plate: 2,
            use_ams: true,
            ams_map: vec![0, 3],
            bed_type: "auto".to_string(),
            timelapse: false,
            inspection: None,
        };
        match req.to_command() {
            ProtoCommand::ProjectFile(pf) => {
                assert_eq!(pf.url, "ftp:///cache/coin.gcode.3mf");
                assert_eq!(pf.plate, 2);
                assert_eq!(pf.subtask_name, "coin.gcode.3mf");
                assert!(pf.use_ams);
                assert_eq!(pf.ams_mapping, vec![0, 3]);
                assert!(!pf.timelapse, "timelapse off unless requested");
            }
            other => panic!("expected ProjectFile, got {other:?}"),
        }
    }

    #[test]
    fn timelapse_flag_flows_into_the_project_file() {
        let req = StartRequest {
            file: "/cube.gcode.3mf".to_string(),
            plate: 1,
            use_ams: false,
            ams_map: vec![],
            bed_type: "auto".to_string(),
            timelapse: true,
            inspection: None,
        };
        match req.to_command() {
            ProtoCommand::ProjectFile(pf) => assert!(pf.timelapse, "arms the sliced timelapse gcode"),
            other => panic!("expected ProjectFile, got {other:?}"),
        }
    }

    #[test]
    fn raw_gcode_builds_a_gcode_file() {
        let req = StartRequest {
            file: "/test.gcode".to_string(),
            plate: 1,
            use_ams: false,
            ams_map: vec![],
            bed_type: "auto".to_string(),
            timelapse: false,
            inspection: None,
        };
        assert!(matches!(req.to_command(), ProtoCommand::GcodeFile(f) if f == "/test.gcode"));
    }
}
