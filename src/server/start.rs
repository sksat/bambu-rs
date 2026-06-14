//! Starting a print (the write side, separate from simple control because it
//! carries file/plate/AMS parameters and needs a fresh MQTT connection). Behind
//! a seam so the API is testable: tests/`--fake` use [`FakeStarter`]; live mode
//! uses [`LiveStarter`] (`project_file`/`gcode_file` + verify).
//!
//! Safety lives in the HTTP handler (confirm gate, idle check, AMS-map range);
//! this just builds the command and verifies it.

use std::path::Path;
use std::time::Duration;

use super::control::ControlError;
use crate::client::LanMqttClient;
use crate::config::ResolvedTarget;
use crate::core::command::{Command as ProtoCommand, ProjectFile};
use crate::core::session::CommandOutcome;

/// A resolved print-start request.
pub struct StartRequest {
    pub file: String,
    pub plate: u32,
    pub use_ams: bool,
    pub ams_map: Vec<i32>,
    pub bed_type: String,
}

impl StartRequest {
    /// Build the MQTT command: `project_file` for a `.3mf`, `gcode_file` for raw
    /// `.gcode`. The file path becomes `ftp://<path>` on the printer.
    pub fn to_command(&self) -> ProtoCommand {
        let name = Path::new(&self.file)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.file)
            .to_string();
        if self.file.to_ascii_lowercase().ends_with(".3mf") {
            let mut pf = ProjectFile::new(format!("ftp://{}", self.file), self.plate, name);
            pf.bed_type = self.bed_type.clone();
            if self.use_ams {
                pf.use_ams = true;
                pf.ams_mapping = self.ams_map.clone();
            }
            ProtoCommand::ProjectFile(pf)
        } else {
            ProtoCommand::GcodeFile(self.file.clone())
        }
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
        };
        match req.to_command() {
            ProtoCommand::ProjectFile(pf) => {
                assert_eq!(pf.url, "ftp:///cache/coin.gcode.3mf");
                assert_eq!(pf.plate, 2);
                assert_eq!(pf.subtask_name, "coin.gcode.3mf");
                assert!(pf.use_ams);
                assert_eq!(pf.ams_mapping, vec![0, 3]);
            }
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
        };
        assert!(matches!(req.to_command(), ProtoCommand::GcodeFile(f) if f == "/test.gcode"));
    }
}
