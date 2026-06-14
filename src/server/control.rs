//! Control actions (the write side of the API) and the controller seam.
//!
//! [`Controller`] keeps the API testable without a printer: tests and `--fake`
//! use [`FakeController`]; live mode uses [`LiveController`], which drives the
//! real device with `send_and_verify` (a second MQTT connection alongside the
//! monitor — they coexist, see `docs/protocol.md`).

use std::time::Duration;

use crate::client::LanMqttClient;
use crate::config::ResolvedTarget;
use crate::core::command::{Command as ProtoCommand, LedNode, SpeedLevel};
use crate::core::session::CommandOutcome;

/// A control action the API can perform.
#[derive(Debug, Clone)]
pub enum ControlAction {
    Pause,
    Resume,
    Stop,
    Light { node: LedNode, on: bool },
    Speed(SpeedLevel),
    Gcode(String),
}

impl ControlAction {
    fn into_command(self) -> ProtoCommand {
        match self {
            ControlAction::Pause => ProtoCommand::Pause,
            ControlAction::Resume => ProtoCommand::Resume,
            ControlAction::Stop => ProtoCommand::Stop,
            ControlAction::Light { node, on } => ProtoCommand::Led { node, on },
            ControlAction::Speed(level) => ProtoCommand::PrintSpeed(level),
            ControlAction::Gcode(line) => ProtoCommand::GcodeLine(line),
        }
    }
}

/// Why a control action couldn't be carried out — distinct from the printer
/// *rejecting* it (that is a [`CommandOutcome::Rejected`]).
#[derive(Debug)]
pub enum ControlError {
    /// Couldn't reach or talk to the printer.
    Transport(String),
}

pub type ControlResult = Result<CommandOutcome, ControlError>;

/// Executes control actions. Blocking — call from `spawn_blocking`.
pub trait Controller: Send + Sync {
    fn execute(&self, action: ControlAction) -> ControlResult;
}

/// Drives a real printer over LAN MQTT.
pub struct LiveController {
    target: ResolvedTarget,
    timeout: Duration,
}

impl LiveController {
    pub fn new(target: ResolvedTarget) -> Self {
        Self {
            target,
            timeout: Duration::from_secs(15),
        }
    }
}

impl Controller for LiveController {
    fn execute(&self, action: ControlAction) -> ControlResult {
        let client = LanMqttClient::new(self.target.clone()).with_timeout(self.timeout);
        client
            .send_and_verify(&action.into_command())
            .map_err(|e| ControlError::Transport(e.to_string()))
    }
}

/// A canned controller for `--fake` mode and tests.
pub struct FakeController {
    outcome: Result<CommandOutcome, String>,
}

impl FakeController {
    /// Always reports the command verified (the `--fake` default).
    pub fn verified() -> Self {
        Self {
            outcome: Ok(CommandOutcome::Verified),
        }
    }

    #[cfg(test)]
    pub fn returning(outcome: CommandOutcome) -> Self {
        Self {
            outcome: Ok(outcome),
        }
    }

    #[cfg(test)]
    pub fn failing() -> Self {
        Self {
            outcome: Err("fake transport failure".to_string()),
        }
    }
}

impl Controller for FakeController {
    fn execute(&self, _action: ControlAction) -> ControlResult {
        match &self.outcome {
            Ok(o) => Ok(o.clone()),
            Err(e) => Err(ControlError::Transport(e.clone())),
        }
    }
}
