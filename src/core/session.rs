//! Verify-by-reread **orchestration**, I/O-free.
//!
//! [`VerifySession`] is the brain of `send_and_verify`, extracted from the MQTT
//! client so it can be tested without a network: you publish a command, then feed
//! it the printer's report messages one at a time via [`VerifySession::observe`];
//! it returns a [`CommandOutcome`] the moment it can conclude, and
//! [`VerifySession::timed_out`] gives the verdict when no conclusive message
//! arrives in time. The real client is a thin async shell around this; tests
//! drive it with a [`crate::core::fake::FakePrinter`] message sequence.
//!
//! The order of operations matters and is the reason this is per-message:
//! - the ACK (echoed `sequence_id` + `result`/`reason` under the command's
//!   category) must be matched *as its message merges*, before a later
//!   `push_status` overwrites that category's `sequence_id` with the printer's
//!   own counter (the two-kinds-of-`sequence_id` hazard);
//! - the `print_error` baseline is captured from the first **full** snapshot so
//!   only a *new* fault is blamed on the command.

use serde::Serialize;
use serde_json::Value;

use crate::core::command::Command;
use crate::core::report::{ReportState, is_full_snapshot_message};
use crate::core::status::PrinterStatus;
use crate::core::verify::{self, EffectStatus};

/// Which stage of verification failed to confirm a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyStage {
    /// No usable ACK arrived (the printer never echoed our `sequence_id`).
    Ack,
    /// The ACK was `success`, but the command's *effect* was never observed in
    /// the report before the timeout (e.g. a print that never started).
    Effect,
}

/// The result of verifying a control command.
///
/// The ACK (`result == "success"`) is necessary but **not sufficient**: for
/// commands with an observable effect the effect is also confirmed from the
/// report, and a new `print_error` after the command is treated as a rejection
/// (observed: a failing SD card ACKed `project_file` then never printed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CommandOutcome {
    /// ACKed `success` **and**, for effectful commands, the effect was observed.
    Verified,
    /// The printer rejected the command (ACK `result != success`) or a new
    /// device error appeared right after it. `reason` is human-readable.
    Rejected { reason: String },
    /// Sent but not confirmed — never assume success. `stage` says whether we
    /// never saw an ACK ([`VerifyStage::Ack`]) or saw the ACK but never the
    /// effect ([`VerifyStage::Effect`]).
    Unverified { stage: VerifyStage },
}

/// Drives verify-by-reread for one command over a stream of report messages.
pub struct VerifySession {
    cmd: Command,
    seq: String,
    state: ReportState,
    acked: bool,
    baseline_error: Option<i64>,
}

impl VerifySession {
    /// Start verifying `cmd`, which was published with `seq` as its `sequence_id`.
    pub fn new(cmd: Command, seq: impl Into<String>) -> Self {
        Self {
            cmd,
            seq: seq.into(),
            state: ReportState::new(),
            acked: false,
            baseline_error: None,
        }
    }

    /// This message's ACK for our command, if it carries one: the echoed
    /// `sequence_id` **and** a `result`, both in the same message.
    ///
    /// `Ok` is success; `Err` carries the reason. A verdict, not a string —
    /// returning the reason for a failure and the result for a success makes
    /// the two indistinguishable to the caller, and a printer answering
    /// `{"result": "FAIL", "reason": "success"}` would then be read as success.
    fn ack_in(&self, message: &Value) -> Option<Result<(), String>> {
        let cat = self.cmd.category();
        let obj = message.pointer(&format!("/{cat}"))?;
        if obj.get("sequence_id").and_then(Value::as_str) != Some(self.seq.as_str()) {
            return None;
        }
        let result = obj.get("result").and_then(Value::as_str)?;
        if result.eq_ignore_ascii_case("success") {
            return Some(Ok(()));
        }
        // `reason` when the printer gives one, the bare result otherwise.
        Some(Err(obj
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or(result)
            .to_string()))
    }

    /// Feed one report message. Returns `Some(outcome)` once a verdict is
    /// reached; `None` means keep waiting (feed the next message, or call
    /// [`timed_out`](Self::timed_out) when the deadline passes).
    pub fn observe(&mut self, message: Value) -> Option<CommandOutcome> {
        let full = is_full_snapshot_message(&message);
        // Phase 1 reads THIS message; phase 2 reads the merged state. An ACK is
        // an event — it either is or isn't in the message that just arrived —
        // while an effect is a level that persists across reports.
        //
        // Matching the ACK against the merged state works only while this
        // session is the sole thing on the connection. Over a shared report
        // feed it doesn't: another client's ACK leaves `result` sitting in the
        // merged state, and the next `push_status` overwrites `sequence_id`
        // with the printer's own counter (a different thing entirely, see the
        // module docs) — a conjunction the command never produced.
        let ack = self.ack_in(&message);
        self.state.apply(message);

        // Baseline print_error from the first full snapshot, so we react only to
        // a NEW fault, not a pre-existing one.
        if self.baseline_error.is_none() && full {
            self.baseline_error = Some(
                PrinterStatus::from_state(self.state.get())
                    .print_error
                    .unwrap_or(0),
            );
        }

        // Phase 1 — the ACK echoes our sequence_id and carries result/reason.
        if !self.acked
            && let Some(verdict) = ack
        {
            if let Err(reason) = verdict {
                return Some(CommandOutcome::Rejected { reason });
            }
            self.acked = true;
            // For commands with no readable effect, the ACK is the verdict.
            if !verify::has_observable_effect(&self.cmd) {
                return Some(CommandOutcome::Verified);
            }
        }

        // Phase 2 — confirm the effect actually happened (and no new fault).
        if self.acked {
            let status = PrinterStatus::from_state(self.state.get());
            match verify::evaluate(&self.cmd, &status, self.baseline_error) {
                EffectStatus::Observed => return Some(CommandOutcome::Verified),
                EffectStatus::NewError(code) => {
                    return Some(CommandOutcome::Rejected {
                        reason: format!(
                            "device reported error 0x{code:08X} after the command; \
                             the effect was not observed (state unchanged)"
                        ),
                    });
                }
                EffectStatus::Pending => {}
            }
        }
        None
    }

    /// The verdict when no conclusive message arrived before the timeout:
    /// distinguishes "never ACKed" from "ACKed but the effect never showed".
    pub fn timed_out(&self) -> CommandOutcome {
        CommandOutcome::Unverified {
            stage: if self.acked {
                VerifyStage::Effect
            } else {
                VerifyStage::Ack
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::command::{Command, ProjectFile, SpeedLevel};
    use crate::core::fake::FakePrinter;

    fn project() -> Command {
        Command::ProjectFile(ProjectFile::new("ftp:///model/x.gcode.3mf", 1, "x"))
    }

    /// Two report messages that carry an ACK's halves separately — another
    /// client's verdict, then a status push whose `sequence_id` is the
    /// printer's own counter and happens to be ours.
    fn split_ack(cat: &str, seq: &str) -> Vec<Value> {
        vec![
            // Somebody else's command was rejected. Nothing here is ours.
            serde_json::json!({ cat: { "sequence_id": "20000042", "result": "FAIL",
                                       "reason": "not ours" } }),
            // A status push. Its `sequence_id` is the printer's counter, a
            // different thing from an echo (see the module docs) — and it
            // carries no result.
            serde_json::json!({ cat: { "command": "push_status", "msg": 1,
                                       "sequence_id": seq } }),
        ]
    }

    #[test]
    fn another_clients_verdict_plus_our_number_is_not_our_ack() {
        // Over a shared report feed the two halves arrive in different
        // messages, and merging them invents a verdict the printer never gave.
        // Matching per message is what makes one connection shareable.
        let s = run(project(), "300000007", split_ack("print", "300000007"));
        assert_eq!(
            s,
            CommandOutcome::Unverified {
                stage: VerifyStage::Ack
            },
            "no message contained both our id and a result"
        );
    }

    #[test]
    fn a_failure_whose_reason_reads_like_success_is_still_a_failure() {
        // The result is the verdict and the reason is prose. Collapsing them
        // into one string — the reason for a failure, the result for a success
        // — makes a printer answering {"result":"FAIL","reason":"success"}
        // indistinguishable from one that succeeded.
        let s = run(
            Command::PrintSpeed(SpeedLevel::Silent),
            "300000009",
            vec![serde_json::json!({ "print": { "sequence_id": "300000009",
                                                "result": "FAIL", "reason": "success" } })],
        );
        assert_eq!(
            s,
            CommandOutcome::Rejected {
                reason: "success".to_string()
            },
            "FAIL is a rejection whatever the reason says"
        );
    }

    #[test]
    fn an_ack_in_one_message_still_concludes() {
        // The halves together, as a real ACK has them.
        let s = run(
            Command::PrintSpeed(SpeedLevel::Silent),
            "300000008",
            vec![serde_json::json!({ "print": { "sequence_id": "300000008",
                                                "result": "FAIL", "reason": "busy" } })],
        );
        assert_eq!(
            s,
            CommandOutcome::Rejected {
                reason: "busy".to_string()
            }
        );
    }

    /// Feed a whole message sequence; return the first concluded outcome (or the
    /// timeout verdict if the sequence is exhausted without one).
    fn run(cmd: Command, seq: &str, msgs: Vec<Value>) -> CommandOutcome {
        let mut s = VerifySession::new(cmd, seq);
        for m in msgs {
            if let Some(o) = s.observe(m) {
                return o;
            }
        }
        s.timed_out()
    }

    #[test]
    fn print_start_acked_and_running_is_verified() {
        let mut p = FakePrinter::idle();
        let msgs = vec![
            p.snapshot(),
            p.ack(&project(), "1", true, "success"),
            p.effect_delta(&project()), // -> RUNNING
        ];
        assert_eq!(run(project(), "1", msgs), CommandOutcome::Verified);
    }

    #[test]
    fn rejected_ack_is_rejected_with_reason() {
        let p = FakePrinter::idle();
        let msgs = vec![
            p.snapshot(),
            p.ack(&project(), "1", false, "print_id error"),
        ];
        assert_eq!(
            run(project(), "1", msgs),
            CommandOutcome::Rejected {
                reason: "print_id error".to_string()
            }
        );
    }

    #[test]
    fn acked_but_no_effect_times_out_as_unverified_effect() {
        let p = FakePrinter::idle();
        // ACK success, but the printer never leaves IDLE (the SD-card scenario).
        let msgs = vec![p.snapshot(), p.ack(&project(), "1", true, "success")];
        assert_eq!(
            run(project(), "1", msgs),
            CommandOutcome::Unverified {
                stage: VerifyStage::Effect
            }
        );
    }

    #[test]
    fn never_acked_times_out_as_unverified_ack() {
        let p = FakePrinter::idle();
        // Only the snapshot, no ACK ever.
        let msgs = vec![p.snapshot()];
        assert_eq!(
            run(project(), "1", msgs),
            CommandOutcome::Unverified {
                stage: VerifyStage::Ack
            }
        );
    }

    #[test]
    fn new_print_error_after_command_is_rejected() {
        let mut p = FakePrinter::idle();
        let msgs = vec![
            p.snapshot(),
            p.ack(&project(), "1", true, "success"),
            p.new_error_delta(0x0500C010), // fault appears after the ACK
        ];
        assert!(matches!(
            run(project(), "1", msgs),
            CommandOutcome::Rejected { .. }
        ));
    }

    #[test]
    fn preexisting_error_is_not_blamed_on_the_command() {
        // The fault is already present in the baseline snapshot; a no-effect ACK
        // must read as Unverified(Effect), NOT Rejected (we didn't cause it).
        let p = FakePrinter::with_error(0x0500C010);
        let msgs = vec![p.snapshot(), p.ack(&project(), "1", true, "success")];
        assert_eq!(
            run(project(), "1", msgs),
            CommandOutcome::Unverified {
                stage: VerifyStage::Effect
            }
        );
    }

    #[test]
    fn ack_only_command_is_verified_on_ack_without_effect() {
        // gcode_line has no observable effect -> the success ACK is the verdict.
        let cmd = Command::GcodeLine("G28".to_string());
        let p = FakePrinter::idle();
        let msgs = vec![p.snapshot(), p.ack(&cmd, "1", true, "success")];
        assert_eq!(run(cmd, "1", msgs), CommandOutcome::Verified);
    }

    #[test]
    fn effect_verified_via_spd_lvl_for_print_speed() {
        let cmd = Command::PrintSpeed(SpeedLevel::Sport);
        let mut p = FakePrinter::idle();
        let msgs = vec![
            p.snapshot(),
            p.ack(&cmd, "1", true, "success"),
            p.effect_delta(&cmd), // -> spd_lvl 3
        ];
        assert_eq!(run(cmd, "1", msgs), CommandOutcome::Verified);
    }
}
