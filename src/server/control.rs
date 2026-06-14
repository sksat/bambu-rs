//! Control actions (the write side of the API) and the controller seam.
//!
//! [`Controller`] keeps the API testable without a printer: tests and `--fake`
//! use [`FakeController`]; live mode uses [`LiveController`], which drives the
//! real device with `send_and_verify` (a second MQTT connection alongside the
//! monitor — they coexist, see `docs/protocol.md`).

use std::time::Duration;

use crate::client::LanMqttClient;
use crate::config::ResolvedTarget;
use crate::core::command::{AmsControl, Command as ProtoCommand, LedNode, SpeedLevel};
use crate::core::session::{CommandOutcome, VerifyStage};

/// Which axes a homing move targets (`G28` with no arg homes all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeAxes {
    All,
    X,
    Y,
    Z,
}

/// A single jog axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
    Z,
}

impl Axis {
    /// The G-code axis letter.
    pub fn as_str(self) -> &'static str {
        match self {
            Axis::X => "X",
            Axis::Y => "Y",
            Axis::Z => "Z",
        }
    }
}

/// Which heater a temperature target applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempPart {
    Nozzle,
    Bed,
}

/// The `M104`/`M140` line that sets `part` to `celsius` (`0` = cooldown).
pub fn temp_line(part: TempPart, celsius: u32) -> String {
    match part {
        TempPart::Nozzle => format!("M104 S{celsius}"),
        TempPart::Bed => format!("M140 S{celsius}"),
    }
}

/// A control action the API can perform.
#[derive(Debug, Clone)]
pub enum ControlAction {
    Pause,
    Resume,
    Stop,
    Light {
        node: LedNode,
        on: bool,
    },
    Speed(SpeedLevel),
    Gcode(String),
    Home(HomeAxes),
    Move {
        axis: Axis,
        delta: f64,
        feedrate: u32,
    },
    Extrude {
        delta: f64,
        feedrate: u32,
    },
    SetTemp {
        part: TempPart,
        celsius: u32,
    },
    Calibrate {
        bed_level: bool,
        vibration: bool,
        motor_noise: bool,
    },
    Ams(AmsControl),
    Reboot,
    DisableSteppers,
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
            ControlAction::Home(axes) => ProtoCommand::GcodeLine(
                match axes {
                    HomeAxes::All => "G28",
                    HomeAxes::X => "G28 X",
                    HomeAxes::Y => "G28 Y",
                    HomeAxes::Z => "G28 Z",
                }
                .to_string(),
            ),
            // Relative move (G91), then back to absolute (G90) so a jog never
            // shifts the coordinate frame.
            ControlAction::Move {
                axis,
                delta,
                feedrate,
            } => ProtoCommand::GcodeLine(format!(
                "G91\nG1 {}{delta} F{feedrate}\nG90",
                axis.as_str()
            )),
            // Relative extrusion (M83), restoring absolute mode (M82) after.
            ControlAction::Extrude { delta, feedrate } => {
                ProtoCommand::GcodeLine(format!("M83\nG1 E{delta} F{feedrate}\nM82"))
            }
            ControlAction::SetTemp { part, celsius } => {
                ProtoCommand::GcodeLine(temp_line(part, celsius))
            }
            ControlAction::Calibrate {
                bed_level,
                vibration,
                motor_noise,
            } => ProtoCommand::Calibration {
                bed_level,
                vibration,
                motor_noise,
            },
            ControlAction::Ams(action) => ProtoCommand::AmsControl(action),
            ControlAction::Reboot => ProtoCommand::Reboot,
            ControlAction::DisableSteppers => ProtoCommand::GcodeLine("M84".to_string()),
        }
    }

    /// Whether this action's effect can be read back. Reboot tears down the
    /// connection (no ACK), so it is sent fire-and-forget instead.
    fn needs_verify(&self) -> bool {
        !matches!(self, ControlAction::Reboot)
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
        let needs_verify = action.needs_verify();
        let cmd = action.into_command();
        if needs_verify {
            client
                .send_and_verify(&cmd)
                .map_err(|e| ControlError::Transport(e.to_string()))
        } else {
            // Reboot: no ACK to await — report Unverified, never a false success.
            client
                .send_fire(&cmd)
                .map(|()| CommandOutcome::Unverified {
                    stage: VerifyStage::Ack,
                })
                .map_err(|e| ControlError::Transport(e.to_string()))
        }
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
