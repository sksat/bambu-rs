//! Parsing a multi-line G-code sequence into the individual lines to send.
//!
//! **No I/O.** The caller reads the file; this turns its text into vetted steps.
//!
//! A "sequence" is a plain `.gcode` file used as a *control* macro rather than a
//! print: park the head, drive an accessory, home an axis. That is how a
//! third-party plate changer is operated — its vendor ships the swap motion as
//! ordinary G-code, so the printer needs no product-specific support, just a way
//! to send those lines in order.
//!
//! Source line numbers are kept so a failure part-way through a sequence can say
//! *which* line stopped it — with hardware mid-motion, "step 12 of 32 failed" is
//! the difference between a safe recovery and a guess.

use serde::Serialize;

use crate::core::safety::{self, GcodeVerdict, TempLimits};
use crate::core::session::CommandOutcome;

/// One line to send, with the 1-based line number it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// 1-based line number in the source text (for diagnostics).
    pub line_no: usize,
    /// The G-code, comment-stripped and trimmed. Never empty.
    pub gcode: String,
}

/// A line that [`vet`] refuses to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsafe {
    pub step: Step,
    /// Why [`safety::check_gcode`] blocked it.
    pub reason: String,
}

/// Split sequence text into the lines to send.
///
/// Strips `;` comments (whole-line and trailing) and blank lines, keeping the
/// original line numbers. Bambu's own files put a literal `; \n` after many
/// commands, so trailing-comment stripping is required, not cosmetic.
pub fn parse(text: &str) -> Vec<Step> {
    text.lines()
        .enumerate()
        .filter_map(|(i, raw)| {
            let code = raw.split(';').next().unwrap_or("").trim();
            (!code.is_empty()).then(|| Step {
                line_no: i + 1,
                gcode: code.to_string(),
            })
        })
        .collect()
}

/// Vet every step against the same static safety rules as a single-line send,
/// returning **all** offending lines rather than just the first.
///
/// Reporting every one matters here: a sequence is confirmed as a whole, so the
/// operator should see the full list before deciding, not fix-and-retry one line
/// at a time while the machine sits mid-sequence.
pub fn vet(steps: &[Step], limits: &TempLimits) -> Vec<Unsafe> {
    steps
        .iter()
        .filter_map(|s| match safety::check_gcode(&s.gcode, limits) {
            GcodeVerdict::Block(reason) => Some(Unsafe {
                step: s.clone(),
                reason,
            }),
            GcodeVerdict::Allow => None,
        })
        .collect()
}

/// Name where a run stopped: `"step 12/32 (line 47: G0 Y150 F200)"`.
///
/// `index` is 0-based; the text is 1-based, because that is how the operator
/// counts and how the file is numbered. Every way a sequence can stop —
/// rejected, unverified, transport drop — reports through this, so the answer
/// to "the machine is mid-motion, which line was that?" never depends on which
/// failure happened.
pub fn step_context(index: usize, total: usize, step: &Step) -> String {
    format!(
        "step {}/{} (line {}: {})",
        index + 1,
        total,
        step.line_no,
        step.gcode
    )
}

/// One step's verdict in a [`Report`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StepReport {
    /// 1-based position in the sequence.
    pub step: usize,
    /// 1-based line number in the source file.
    pub line: usize,
    pub gcode: String,
    /// Flattened so a step reads exactly like a single command's verdict does
    /// (`"outcome": "rejected", "reason": …`).
    #[serde(flatten)]
    pub outcome: CommandOutcome,
}

/// The machine-readable result of running a whole sequence.
///
/// A sequence is many commands but **one** result: printing a verdict per step
/// would put several adjacent JSON objects on stdout, which no parser accepts.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// The `.gcode` file the steps came from.
    pub source: String,
    /// Steps in the sequence.
    pub total: usize,
    /// Steps the printer **acknowledged**.
    ///
    /// For a G-code line that is the whole verdict — `core::verify` classifies
    /// it as having no observable state effect, so there is nothing to confirm
    /// beyond the ACK. It does NOT mean the motion finished: the printer never
    /// reports that, and a sequence with `G4` dwells is still moving well after
    /// the last ACK. Reading it as "done" is the mistake this doc exists to
    /// prevent.
    pub verified: usize,
    /// Whether every step was confirmed — the one field to branch on.
    pub ok: bool,
    /// What `verified` covered. Always `"ack"` here: a G-code line has no
    /// observable state effect (see `core::verify`), so the printer's
    /// acknowledgement is the whole verdict and it never reports when the
    /// motion finishes. A sequence with `G4` dwells is still moving after the
    /// last ACK — reading `verified == total` as "done" is the mistake this
    /// field exists to prevent.
    pub confirms: &'static str,
    /// Per-step verdicts, in order. Ends at the step that stopped the run, so
    /// it is shorter than `total` unless the run finished.
    pub steps: Vec<StepReport>,
}

impl Report {
    /// Pair `steps` with the `outcomes` the printer gave them.
    ///
    /// `outcomes` is allowed to be shorter than `steps`: a run stops at the
    /// first step that isn't confirmed, and a transport drop can end it with no
    /// verdict at all.
    pub fn new(source: impl Into<String>, steps: &[Step], outcomes: &[CommandOutcome]) -> Self {
        let verified = outcomes
            .iter()
            .filter(|o| **o == CommandOutcome::Verified)
            .count();
        Self {
            source: source.into(),
            total: steps.len(),
            verified,
            confirms: "ack",
            ok: verified == steps.len(),
            steps: steps
                .iter()
                .zip(outcomes)
                .enumerate()
                .map(|(i, (s, o))| StepReport {
                    step: i + 1,
                    line: s.line_no,
                    gcode: s.gcode.clone(),
                    outcome: o.clone(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keeps_source_line_numbers_through_blanks_and_comments() {
        // Mirrors a real vendor sequence: a comment header, blank lines, and a
        // trailing `; \n` on every command (Bambu's own files look like this).
        let text = "\
; swap-s start plate load only - v05

G90;
G28;

G0 Z30 F5000; \\n
; a whole-line comment
G0 X-10; \\n
";
        let steps = parse(text);
        assert_eq!(
            steps,
            vec![
                Step {
                    line_no: 3,
                    gcode: "G90".into()
                },
                Step {
                    line_no: 4,
                    gcode: "G28".into()
                },
                Step {
                    line_no: 6,
                    gcode: "G0 Z30 F5000".into()
                },
                Step {
                    line_no: 8,
                    gcode: "G0 X-10".into()
                },
            ],
            "line numbers must survive so a mid-sequence failure can name the line"
        );
    }

    #[test]
    fn parse_drops_a_file_with_nothing_to_send() {
        assert!(parse("; only a comment\n\n   \n").is_empty());
    }

    #[test]
    fn vet_reports_every_unsafe_line_not_just_the_first() {
        let steps = parse("M104 S400\nG28\nM109 S500\n");
        let bad = vet(&steps, &TempLimits::default());
        assert_eq!(
            bad.iter().map(|u| u.step.line_no).collect::<Vec<_>>(),
            vec![1, 3],
            "the operator confirms the whole sequence, so show every problem at once"
        );
    }

    #[test]
    fn vet_passes_an_accessory_sequence_that_leaves_the_print_area() {
        // The plate-changer motion parks off the bed on purpose (X=-10, Y=-6,
        // Z above the print height). Those are legitimate positioning moves and
        // must not be mistaken for unsafe G-code.
        let steps =
            parse("G90\nG28\nG0 Z30 F5000\nG0 X-10\nG0 Y-6 F2000\nG4 S3\nG0 Y186.5\nG0 Z186\n");
        assert!(vet(&steps, &TempLimits::default()).is_empty());
    }

    #[test]
    fn step_context_names_the_position_and_the_source_line() {
        let steps = parse("G90\nG28\nG0 Y150 F200\n");
        assert_eq!(
            step_context(2, steps.len(), &steps[2]),
            "step 3/3 (line 3: G0 Y150 F200)"
        );
    }

    #[test]
    fn report_of_a_finished_run_is_ok() {
        let steps = parse("G90\nG28\n");
        let r = Report::new(
            "/tmp/seq.gcode",
            &steps,
            &[CommandOutcome::Verified, CommandOutcome::Verified],
        );
        assert!(r.ok);
        assert_eq!((r.total, r.verified), (2, 2));
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::json!({
                "source": "/tmp/seq.gcode",
                "total": 2,
                "verified": 2,
                "ok": true,
                // Says what "verified" covered — an agent reading `verified == total`
                // as "the motion finished" is the misread this guards against.
                "confirms": "ack",
                "steps": [
                    { "step": 1, "line": 1, "gcode": "G90", "outcome": "verified" },
                    { "step": 2, "line": 2, "gcode": "G28", "outcome": "verified" },
                ],
            }),
            "one object for the whole run: adjacent per-step objects are not JSON"
        );
    }

    #[test]
    fn report_of_a_run_that_stopped_keeps_the_steps_it_got_to() {
        // The run stops at the first step the printer doesn't confirm, so the
        // outcomes are SHORTER than the sequence — `total` still says how long
        // the sequence was, which is what "stopped 11 lines in" needs.
        let steps = parse("G90\nG28\nG0 Y150 F200\n");
        let r = Report::new(
            "/tmp/seq.gcode",
            &steps,
            &[
                CommandOutcome::Verified,
                CommandOutcome::Rejected {
                    reason: "busy".into(),
                },
            ],
        );
        assert!(!r.ok);
        assert_eq!((r.total, r.verified, r.steps.len()), (3, 1, 2));
        assert_eq!(
            serde_json::to_value(&r).unwrap()["steps"][1],
            serde_json::json!({
                "step": 2, "line": 2, "gcode": "G28",
                "outcome": "rejected", "reason": "busy",
            }),
            "the failing step carries the printer's reason inline"
        );
    }

    #[test]
    fn report_of_a_run_that_never_answered_is_not_ok() {
        // A transport drop before the first verdict: nothing is confirmed, and
        // that must not read as a clean run.
        let steps = parse("G90\nG28\n");
        let r = Report::new("/tmp/seq.gcode", &steps, &[]);
        assert!(!r.ok);
        assert_eq!((r.total, r.verified), (2, 0));
        assert!(r.steps.is_empty());
    }
}
