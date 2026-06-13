//! Effect-verification predicates — "did the command actually take effect?"
//!
//! A command ACK (`result == "success"`) only says the printer *accepted* the
//! request. A real fault often surfaces **after** the ACK: either the expected
//! state transition never happens, or a new `print_error` appears. This was
//! observed on a real A1 mini — a `project_file` ACKed `success` but the print
//! never started (a failing SD card set `print_error = 0x0500C010` while
//! `gcode_state` stayed `IDLE` and `hms` was empty). Trusting the ACK alone is a
//! false positive.
//!
//! So for commands with an observable effect we read the report back and confirm
//! the effect, while watching for a **new** `print_error` that appeared after we
//! sent (the baseline error is captured before sending, so a pre-existing fault
//! is not blamed on this command).
//!
//! Note: `subtask_name` is deliberately **not** used as a "print started"
//! signal — in the SD-card failure it changed to the new job even though the
//! print never began. Only `gcode_state` transitions are trusted.

use crate::core::command::Command;
use crate::core::status::{GcodeState, PrinterStatus};

/// The observable effect status of a command, evaluated against one report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectStatus {
    /// The intended effect is visible in the report.
    Observed,
    /// A new device error (`print_error`) appeared after the command was sent.
    NewError(i64),
    /// No effect (and no new error) yet — the caller should keep waiting until
    /// its timeout, after which "pending" means *unverified*.
    Pending,
}

/// Whether a command produces an effect we can read back from the report. When
/// `false`, the ACK is the only available signal and is the final verdict.
pub fn has_observable_effect(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::ProjectFile(_)
            | Command::GcodeFile(_)
            | Command::Calibration { .. }
            | Command::Pause
            | Command::Resume
            | Command::Stop
            | Command::ChamberLight(_)
    )
}

/// Evaluate a command's effect against the current `status`, given the
/// `baseline_print_error` captured **before** the command was sent (so only a
/// *new* error is reported, not a pre-existing one).
pub fn evaluate(
    cmd: &Command,
    status: &PrinterStatus,
    baseline_print_error: Option<i64>,
) -> EffectStatus {
    let state = status.state();

    // Stop/abort is special: success == reaching a terminal state. Aborting a
    // paused or already-errored job can transiently raise a `print_error` before
    // it settles to FAILED, so a "new" error here must NOT mask the stop
    // succeeding (observed: stop -> 0x0300400C -> then FAILED, print_error 0).
    if matches!(cmd, Command::Stop) {
        return if matches!(
            state,
            Some(GcodeState::Idle | GcodeState::Finish | GcodeState::Failed)
        ) {
            EffectStatus::Observed
        } else {
            EffectStatus::Pending
        };
    }

    // For every other command, a new non-zero print_error means it did not
    // cleanly take effect.
    let current = status.print_error.unwrap_or(0);
    let baseline = baseline_print_error.unwrap_or(0);
    if current != 0 && current != baseline {
        return EffectStatus::NewError(current);
    }

    let observed = match cmd {
        // A print start has "taken effect" once the job is being prepared or
        // run. (subtask_name is not trusted — see module docs.)
        Command::ProjectFile(_) | Command::GcodeFile(_) | Command::Calibration { .. } => {
            matches!(
                state,
                Some(GcodeState::Prepare | GcodeState::Running | GcodeState::Slicing)
            )
        }
        Command::Pause => state == Some(GcodeState::Pause),
        Command::Resume => state == Some(GcodeState::Running),
        // The light is "set" only once `lights_report` actually shows the
        // commanded mode — the `ledctrl` ACK alone is not enough (a faulty unit
        // ACKs but `lights_report` stays unchanged).
        Command::ChamberLight(on) => {
            let want = if *on { "on" } else { "off" };
            status.chamber_light.as_deref() == Some(want)
        }
        // Stop is handled above (terminal-state check, error-tolerant).
        Command::Stop => unreachable!("Stop handled before the new-error check"),
        // No observable state effect — caller should not use evaluate() for these.
        Command::PushAll | Command::GcodeLine(_) | Command::Reboot => false,
    };

    if observed {
        EffectStatus::Observed
    } else {
        EffectStatus::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::command::ProjectFile;

    fn status(gcode_state: &str, print_error: i64) -> PrinterStatus {
        PrinterStatus {
            gcode_state: Some(gcode_state.to_string()),
            print_error: Some(print_error),
            ..Default::default()
        }
    }

    fn project() -> Command {
        Command::ProjectFile(ProjectFile::new("ftp:///model/x.gcode.3mf", 1, "x"))
    }

    #[test]
    fn which_commands_have_an_observable_effect() {
        assert!(has_observable_effect(&project()));
        assert!(has_observable_effect(&Command::Pause));
        assert!(has_observable_effect(&Command::Calibration {
            bed_level: true,
            vibration: false,
            motor_noise: false
        }));
        // The light is effectful — verified via lights_report, not just the ACK.
        assert!(has_observable_effect(&Command::ChamberLight(true)));
        // ACK-only commands:
        assert!(!has_observable_effect(&Command::GcodeLine("G28".into())));
        assert!(!has_observable_effect(&Command::PushAll));
        assert!(!has_observable_effect(&Command::Reboot));
    }

    #[test]
    fn chamber_light_effect_reads_lights_report_not_the_ack() {
        let lit = |mode: &str| PrinterStatus {
            chamber_light: Some(mode.to_string()),
            ..Default::default()
        };
        // light on: observed only when lights_report actually shows "on".
        assert_eq!(
            evaluate(&Command::ChamberLight(true), &lit("on"), Some(0)),
            EffectStatus::Observed
        );
        // Faulty unit: ledctrl ACKed but lights_report stays "off" -> pending
        // (→ unverified on timeout), never a false "verified".
        assert_eq!(
            evaluate(&Command::ChamberLight(true), &lit("off"), Some(0)),
            EffectStatus::Pending
        );
        assert_eq!(
            evaluate(&Command::ChamberLight(false), &lit("off"), Some(0)),
            EffectStatus::Observed
        );
    }

    #[test]
    fn project_file_running_is_observed() {
        assert_eq!(
            evaluate(&project(), &status("RUNNING", 0), Some(0)),
            EffectStatus::Observed
        );
        assert_eq!(
            evaluate(&project(), &status("PREPARE", 0), Some(0)),
            EffectStatus::Observed
        );
    }

    #[test]
    fn project_file_idle_with_no_error_is_pending_not_observed() {
        // The exact SD-card failure window *before* print_error appears: ACK was
        // success, but gcode_state is still IDLE — must NOT read as Observed.
        assert_eq!(
            evaluate(&project(), &status("IDLE", 0), Some(0)),
            EffectStatus::Pending
        );
    }

    #[test]
    fn new_print_error_after_send_is_reported() {
        // The real SD-card fault: print_error becomes 0x0500C010 while IDLE.
        assert_eq!(
            evaluate(&project(), &status("IDLE", 0x0500C010), Some(0)),
            EffectStatus::NewError(0x0500C010)
        );
    }

    #[test]
    fn preexisting_error_is_not_blamed_on_this_command() {
        // Same error present before sending (baseline) -> not a new fault; with
        // no state transition yet it's simply pending.
        assert_eq!(
            evaluate(&project(), &status("IDLE", 0x0500C010), Some(0x0500C010)),
            EffectStatus::Pending
        );
    }

    #[test]
    fn pause_resume_stop_effects() {
        assert_eq!(
            evaluate(&Command::Pause, &status("PAUSE", 0), Some(0)),
            EffectStatus::Observed
        );
        assert_eq!(
            evaluate(&Command::Resume, &status("RUNNING", 0), Some(0)),
            EffectStatus::Observed
        );
        assert_eq!(
            evaluate(&Command::Stop, &status("FINISH", 0), Some(0)),
            EffectStatus::Observed
        );
        // Wrong state -> still pending.
        assert_eq!(
            evaluate(&Command::Pause, &status("RUNNING", 0), Some(0)),
            EffectStatus::Pending
        );
    }

    #[test]
    fn stop_tolerates_a_transient_abort_error() {
        // Aborting a paused/errored job transiently raises a new print_error
        // (0x0300400C) before it settles to FAILED. Reaching a terminal state is
        // success — the transient error must NOT be reported as a failure.
        assert_eq!(
            evaluate(&Command::Stop, &status("FAILED", 0x0300400C), Some(0)),
            EffectStatus::Observed
        );
        // Not terminal yet -> pending (keep waiting), still not NewError.
        assert_eq!(
            evaluate(&Command::Stop, &status("PAUSE", 0x0300400C), Some(0)),
            EffectStatus::Pending
        );
    }

    #[test]
    fn a_new_error_beats_an_otherwise_observed_effect() {
        // Even if the state looks right, a fresh fault means it did not cleanly
        // take effect.
        assert_eq!(
            evaluate(&Command::Resume, &status("RUNNING", 0x0500C010), Some(0)),
            EffectStatus::NewError(0x0500C010)
        );
    }
}
