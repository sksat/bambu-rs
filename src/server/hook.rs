//! Running a printer's `pre_print` sequence, immediately before a print starts.
//!
//! Behind a seam like [`Starter`](super::start::Starter), so the API is testable:
//! tests and `--fake` use [`NoHook`]/`RecordingHook`; live mode uses
//! [`LiveHook`].
//!
//! Why the server needs its own: `hooks.pre_print` is a *profile* field, and
//! nothing under `src/server/` sees a `Profile` — every module here takes a
//! [`ResolvedTarget`] and nothing else. So the CLI resolves the hook (name →
//! this profile's sequence → path) and hands over a [`PrePrint`]; this module
//! only runs it.
//!
//! Returning means the motion has been **observed to finish**, not merely
//! acknowledged. That is the whole point: measured on an A1 mini, the last ACK
//! lands ~1 s in while the machine keeps moving for another minute, and a print
//! start does not queue behind it — see [`crate::core::settle`].

use std::path::PathBuf;
use std::time::Duration;

use crate::client::LanMqttClient;
use crate::config::ResolvedTarget;
use crate::core::command::Command as ProtoCommand;
use crate::core::safety::TempLimits;
use crate::core::sequence;
use crate::core::session::CommandOutcome;
use crate::core::settle::SettleOutcome;

/// Why a pre-print hook did not complete.
///
/// Separate from [`ControlError`](super::control::ControlError) because the
/// interesting failures here are not transport ones: a sequence that is missing
/// or unsafe is the operator's configuration, and a motion that never finished
/// is the printer's state. Telling those apart is what lets the response say
/// which one to go and fix.
#[derive(Debug)]
pub enum HookError {
    /// The sequence is missing, empty, or contains G-code that is refused.
    Config(String),
    /// The printer refused a line, or the motion was never observed to finish.
    Printer(String),
    /// Couldn't reach or talk to the printer.
    Transport(String),
}

impl std::fmt::Display for HookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookError::Config(m) | HookError::Printer(m) | HookError::Transport(m) => {
                f.write_str(m)
            }
        }
    }
}

/// A resolved `pre_print` hook: which sequence, where it lives, how long its
/// motion may take.
#[derive(Debug, Clone)]
pub struct PrePrint {
    /// The sequence's name in the profile — what to call it in a message.
    pub name: String,
    /// The `.gcode` file, already resolved against the config directory.
    pub path: PathBuf,
    /// How long to wait for the motion before refusing to start the print.
    pub settle: Duration,
}

/// Runs a printer's pre-print sequence. Blocking — call from `spawn_blocking`.
pub trait PrePrintHook: Send + Sync {
    /// The sequence that would run, or `None` when this printer has no hook.
    /// Used by `dry_run` to disclose motion the preview would otherwise hide.
    fn describe(&self) -> Option<&str> {
        None
    }

    /// Run it, returning only once the motion has finished.
    fn run(&self) -> Result<(), HookError> {
        Ok(())
    }
}

/// No hook configured — the common case, and what `--fake` uses.
pub struct NoHook;
impl PrePrintHook for NoHook {}

/// Drives a real printer over LAN MQTT.
///
/// # Its own connection, and what that costs under `--emulate`
///
/// This opens a `LanMqttClient` for the length of the sequence *and* its settle
/// wait — minutes, for a plate changer. Every other live implementation here
/// does the same ([`LiveController`](super::control::LiveController),
/// [`LiveStarter`](super::start::LiveStarter), `LiveFiles`, `LiveCamera`), but
/// theirs last about a second, and duration is exactly what makes a second
/// connection likely to be noticed.
///
/// With `--emulate`, [`LivePrinterLink`](super::relay::LivePrinterLink) already
/// holds the printer's one long-lived connection and this is a second one. On a
/// printer that tolerates only one, the two disconnect each other. The failure
/// is loud and lands on the safe side — the sequence fails to verify, so the
/// hook reports failure and **the print is not started** — but the relay's
/// status goes stale while it reconnects.
///
/// Routing through the shared link is the right fix, and it belongs to all five
/// live implementations rather than to this one: `Upstream` already exposes
/// exactly what that needs (`subscribe` for reports, `send` for requests), but
/// the ACK/settle loop lives inside `LanMqttClient` and would have to be
/// expressed against that trait first. Doing it for the hook alone would leave
/// the other four opening connections behind the relay's back and make the rule
/// harder to see, not easier.
pub struct LiveHook {
    target: ResolvedTarget,
    spec: PrePrint,
}

impl LiveHook {
    pub fn new(target: ResolvedTarget, spec: PrePrint) -> Self {
        Self { target, spec }
    }

    /// Read and vet the sequence.
    ///
    /// Read at fire time rather than cached at startup, so editing the macro
    /// takes effect without restarting the server — a plate changer's
    /// coordinates get tuned by trial, and each trial ending in a restart is
    /// how a knob stops being used. Startup still checks the file (see
    /// [`PrePrint::vet_now`]), so a missing one is caught long before a print.
    fn steps(&self) -> Result<Vec<sequence::Step>, String> {
        let text = std::fs::read_to_string(&self.spec.path)
            .map_err(|e| format!("reading {}: {e}", self.spec.path.display()))?;
        vet_text(&text, &self.spec.path.display().to_string())
    }
}

/// Parse and safety-check a sequence, naming its source in any error.
fn vet_text(text: &str, source: &str) -> Result<Vec<sequence::Step>, String> {
    let steps = sequence::parse(text);
    if steps.is_empty() {
        return Err(format!(
            "{source} has no G-code to send (only comments/blank lines)"
        ));
    }
    let bad = sequence::vet(&steps, &TempLimits::default());
    if !bad.is_empty() {
        // Every offending line, not just the first: the operator confirms a
        // sequence as a whole, and there is no `--force` on this path.
        let detail = bad
            .iter()
            .map(|u| format!("line {}: {} ({})", u.step.line_no, u.step.gcode, u.reason))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("{source} contains unsafe G-code: {detail}"));
    }
    Ok(steps)
}

impl PrePrintHook for LiveHook {
    fn describe(&self) -> Option<&str> {
        Some(&self.spec.name)
    }

    fn run(&self) -> Result<(), HookError> {
        let name = &self.spec.name;
        let steps = self.steps().map_err(HookError::Config)?;
        let total = steps.len();
        let commands = steps
            .iter()
            .map(|s| ProtoCommand::GcodeLine(s.gcode.clone()))
            .collect::<Vec<_>>();

        let run = LanMqttClient::new(self.target.clone())
            .with_timeout(Duration::from_secs(30))
            .send_sequence(&commands, |_| {}, Some(self.spec.settle))
            .map_err(|e| HookError::Transport(e.to_string()))?;

        // Name the line it stopped on: the machine is left mid-motion, and
        // "which line was that?" is the first thing the operator needs.
        if let Some((i, o)) = run
            .outcomes
            .iter()
            .enumerate()
            .find(|(_, o)| **o != CommandOutcome::Verified)
        {
            return Err(HookError::Printer(format!(
                "pre-print sequence {name:?} stopped at {}: {}",
                sequence::step_context(i, total, &steps[i]),
                describe_outcome(o),
            )));
        }
        match run.settled {
            Some(SettleOutcome::Settled) => Ok(()),
            // Asked for and not achieved: the machine may still be moving, so
            // starting the print now is exactly the collision this exists to
            // prevent.
            Some(o) => Err(HookError::Printer(format!(
                "pre-print sequence {name:?} ran, but {}",
                o.failure().unwrap_or_else(|| "it did not finish".into()),
            ))),
            None => Err(HookError::Printer(format!(
                "pre-print sequence {name:?} was not waited for"
            ))),
        }
    }
}

fn describe_outcome(o: &CommandOutcome) -> String {
    match o {
        CommandOutcome::Verified => "acknowledged".to_string(),
        CommandOutcome::Rejected { reason } => format!("the printer refused it ({reason})"),
        CommandOutcome::Unverified { .. } => "the printer never acknowledged it".to_string(),
    }
}

impl PrePrint {
    /// Check the sequence at startup, so a hook pointing at a missing or unsafe
    /// file is a configuration error rather than a surprise at print time.
    pub fn vet_now(&self) -> Result<(), String> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("reading {}: {e}", self.path.display()))?;
        vet_text(&text, &self.path.display().to_string()).map(|_| ())
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts how many times it ran, and can be told to fail — the two things a
    /// handler test needs to know about a hook.
    pub struct RecordingHook {
        pub name: String,
        pub runs: AtomicUsize,
        pub fail: Option<HookError>,
    }

    impl RecordingHook {
        pub fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                runs: AtomicUsize::new(0),
                fail: None,
            }
        }

        pub fn failing(name: &str, why: &str) -> Self {
            Self::erroring(name, HookError::Printer(why.to_string()))
        }

        /// Fail with a specific error class, so each response mapping is
        /// exercised rather than just the one.
        pub fn erroring(name: &str, err: HookError) -> Self {
            Self {
                fail: Some(err),
                ..Self::new(name)
            }
        }

        pub fn runs(&self) -> usize {
            self.runs.load(Ordering::SeqCst)
        }
    }

    impl PrePrintHook for RecordingHook {
        fn describe(&self) -> Option<&str> {
            Some(&self.name)
        }
        fn run(&self) -> Result<(), HookError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            match self.fail.as_ref() {
                Some(HookError::Config(m)) => Err(HookError::Config(m.clone())),
                Some(HookError::Printer(m)) => Err(HookError::Printer(m.clone())),
                Some(HookError::Transport(m)) => Err(HookError::Transport(m.clone())),
                None => Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_printer_without_a_hook_discloses_nothing_and_does_nothing() {
        assert_eq!(NoHook.describe(), None);
        assert!(NoHook.run().is_ok());
    }

    #[test]
    fn a_sequence_of_only_comments_is_a_configuration_error() {
        // Silently doing nothing would be worse than refusing: the operator
        // configured a swap and would get a print onto whatever is on the bed.
        let err = vet_text("; just a comment\n\n", "swap.gcode").unwrap_err();
        assert!(err.contains("no G-code"), "{err}");
    }

    #[test]
    fn unsafe_lines_are_all_reported_before_anything_is_sent() {
        // There is no --force on this path, so the operator needs the whole
        // list to fix the file, not one line at a time with a print waiting.
        let err = vet_text("M104 S400\nG28\nM109 S500\n", "swap.gcode").unwrap_err();
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("line 3"), "{err}");
    }

    #[test]
    fn an_accessory_sequence_that_parks_off_the_bed_is_accepted() {
        // The plate-changer motion leaves the print area on purpose; those are
        // positioning moves, not unsafe G-code.
        let steps = vet_text(
            "G90\nG28\nG0 Z30 F5000\nG0 X-10\nG0 Y-6 F2000\nG4 S3\n",
            "swap.gcode",
        )
        .unwrap();
        assert_eq!(steps.len(), 6);
    }
}
