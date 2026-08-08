//! Waiting for a sequence's motion to physically finish.
//!
//! **No I/O.** This decides *what to send* and *what counts as finished*; the
//! caller does the sending and the polling.
//!
//! # Why this exists
//!
//! A `gcode_line` ACK means the printer **queued** the line, not that it ran —
//! see [`crate::core::sequence::Report::verified`]. Firing a print immediately
//! after a plate-changer sequence therefore starts the print *while the changer
//! is still moving*. Measured on a real A1 mini, with a 160 mm move at F200
//! (48 s of travel):
//!
//! | | when the line after the move took effect |
//! |---|---|
//! | last ACK | +1 s |
//! | no `M400` | **+34 s / +35 s** — mid-move |
//! | with `M400` | +53 s / +58 s / +60 s / +67 s — after the move |
//!
//! Two separate facts make this necessary:
//!
//! 1. **G-code lines are ordered but not drained.** A later line waits for
//!    earlier ones to be *parsed*, so a `G4` dwell does hold the queue — but a
//!    `G0`/`G1` only gets buffered into the motion planner, and the parser runs
//!    on ahead. `M400` is what blocks until the planner is empty.
//! 2. **A print start is not in that queue at all.** Sending `project_file`
//!    during a `G4 S30` dwell had the print at `RUNNING` 18 s in, well inside
//!    the dwell. So the printer will not serialise the two for us; the only
//!    thing that can is the caller, waiting.
//!
//! # How the wait terminates
//!
//! `M400` gives no report of its own, so it cannot be observed directly. The
//! trick is to follow it with a line whose effect *is* in the status report and
//! wait for that: because the queue is ordered, seeing the sentinel's effect
//! proves everything before it — including the `M400` — has run.
//!
//! `M1002 gcode_claim_action : N` sets `stg_cur`, the printer's displayed
//! activity, and nothing else: no motion, no heat, no fan. The printer echoes
//! **any** `N` verbatim, including values it never emits itself, which is what
//! this uses: vendor G-code across the whole BBL profile set claims values up
//! to 75 (plus the 255 "none" marker), so a sentinel at 200+ can only have come
//! from the line this module sent.
//!
//! Both commands are Bambu's own — `M400` and `M1002 gcode_claim_action` appear
//! in the vendor machine G-code of every model shipped in the BBL profiles (A1,
//! A1 mini, H2D, H2D Pro, H2S, P1P, P1S, P2S, X1, X1 Carbon, X1E, X2D). Only the
//! A1 mini was measured, so this is not gated on a model: the failure mode is a
//! wait that times out and **refuses to start the print**, never a print that
//! races the sequence.

use serde::Serialize;

use crate::core::status::GcodeState;

/// `stg_cur` value meaning "no activity" — where the printer is put back.
const NO_STAGE: i64 = 255;

/// Sentinel `stg_cur` values, in preference order.
///
/// Clear of every value vendor G-code is known to claim (up to 75) so a match
/// can only be the line this module sent. Several, because the value has to
/// avoid what the printer is *already* showing — a previous run that died
/// before restoring leaves its sentinel behind — and anything the sequence
/// being waited on claims for itself.
const SENTINELS: [i64; 6] = [200, 201, 202, 203, 204, 205];

/// A pending wait for the printer's G-code queue to drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settle {
    sentinel: i64,
    /// The fault code already present when the wait was set up, if any. Only a
    /// *different, nonzero* code counts as one this sequence caused.
    error_before: Option<i64>,
}

/// What one status report says about a [`Settle`] in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleStep {
    /// Nothing conclusive yet — keep reading reports.
    Waiting,
    /// The sentinel was observed: everything queued before it has run.
    Settled,
    /// Something else took the printer over. The caller must **not** proceed.
    Interrupted(&'static str),
}

/// How a wait ended.
///
/// Only [`Settled`](Self::Settled) means the motion has stopped; on everything
/// else the machine may still be moving, so a caller that was going to start a
/// print must not. Serialised tagged, so a machine consumer gets the reason and
/// not just a bare `false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SettleOutcome {
    /// The sentinel was observed: everything sent has physically run.
    Settled,
    /// No sentinel within the budget. The motion may still be running.
    TimedOut { after_secs: u64 },
    /// A job took the printer over mid-wait.
    Interrupted { reason: String },
    /// The printer **rejected** one of the wait's own lines, so there was
    /// nothing to observe.
    NotSent { gcode: String },
    /// One of the wait's own lines was never acknowledged. Distinct from a
    /// rejection: the printer said nothing, so the line may yet run — the wait
    /// simply has no way to find out.
    NotConfirmed { gcode: String },
    /// The run stopped before the wait could start. Distinct from "no wait was
    /// asked for", which is the absence of a [`SettleOutcome`] altogether.
    NotReached,
    /// Every sentinel collided with what the printer was showing or what the
    /// sequence claims for itself, so there was no value whose appearance would
    /// have proved anything.
    NoSentinel { claimed: Vec<i64> },
}

impl SettleOutcome {
    /// Why this is not a drained queue — `None` when it is one.
    pub fn failure(&self) -> Option<String> {
        match self {
            SettleOutcome::Settled => None,
            SettleOutcome::TimedOut { after_secs } => Some(format!(
                "the printer was still running the sequence after {after_secs}s \
                 (waiting for its G-code queue to drain)"
            )),
            SettleOutcome::Interrupted { reason } => Some(reason.clone()),
            SettleOutcome::NotSent { gcode } => {
                Some(format!("the printer would not accept {gcode:?}"))
            }
            SettleOutcome::NotConfirmed { gcode } => Some(format!(
                "the printer never acknowledged {gcode:?}, so the motion could not be observed"
            )),
            SettleOutcome::NotReached => {
                Some("the sequence stopped before the wait could start".to_string())
            }
            SettleOutcome::NoSentinel { claimed } => Some(format!(
                "the sequence claims every stage this could have waited on ({claimed:?}), \
                 so there is no way to tell when its motion finished"
            )),
        }
    }
}

impl Settle {
    /// Prepare a wait.
    ///
    /// `stage_now` must be an **authoritative** reading of `stg_cur` — freshly
    /// pushed, not whatever happened to have merged. The sentinel is only
    /// meaningful because the printer is not already showing it: matching a
    /// value that was there all along ends the wait instantly, which is exactly
    /// the failure this exists to prevent. Taking it by value rather than as an
    /// `Option` is deliberate — "we don't know the current stage" is not a
    /// state a caller may settle from.
    ///
    /// `claimed` is every stage the sequence itself sets (see
    /// [`claimed_stages`]): those lines are still queued when the wait starts,
    /// so one of them landing later would look just like the sentinel.
    ///
    /// `error_before` is the fault code the printer was already reporting, so a
    /// pre-existing one is not blamed on this sequence. A profile is allowed to
    /// start from `FAILED` — that is how an A1 sits after a cancelled print —
    /// and the stale code clearing part-way through is the machine getting
    /// *better*, not a new fault.
    ///
    /// `None` when every candidate collides — there is then no value whose
    /// appearance would prove anything, and the caller must refuse rather than
    /// wait on a compromised one. Settling for a *claimed* value would end the
    /// wait when the sequence's own line lands, mid-motion, which is precisely
    /// the failure this module exists to prevent; a relaxed invariant here
    /// would be worse than no wait at all, because it reports success.
    pub fn after(stage_now: i64, claimed: &[i64], error_before: Option<i64>) -> Option<Self> {
        SENTINELS
            .iter()
            .copied()
            .find(|s| *s != stage_now && !claimed.contains(s))
            .map(|sentinel| Self {
                sentinel,
                error_before,
            })
    }

    /// The lines to send after the sequence, in order.
    ///
    /// `M400` first — without it the sentinel is parsed while the moves are
    /// still draining from the planner, and the wait ends early (measured: 34 s
    /// into a 51 s motion).
    pub fn gcode(&self) -> [String; 2] {
        [
            "M400".to_string(),
            format!("M1002 gcode_claim_action : {}", self.sentinel),
        ]
    }

    /// The line that puts the printer's displayed activity back to "none".
    ///
    /// The sentinel is deliberately a value no decoder knows, so leaving it on
    /// the screen would misreport the machine to the next reader.
    pub fn restore_gcode() -> String {
        format!("M1002 gcode_claim_action : {NO_STAGE}")
    }

    /// Classify one status report.
    ///
    /// Takeovers and faults are checked first: a print that started from the
    /// screen, or a fault the sequence itself caused, makes the whole plan void
    /// and deserves its own message rather than ten minutes of silence followed
    /// by a timeout.
    pub fn observe(
        &self,
        stg_cur: Option<i64>,
        state: Option<GcodeState>,
        print_error: Option<i64>,
    ) -> SettleStep {
        // A *different, nonzero* code — not merely "different from the
        // baseline". A stale error clearing reads as a change too, and aborting
        // on it would refuse the print because the printer stopped complaining.
        if matches!(print_error, Some(e) if Some(e) != self.error_before) {
            return SettleStep::Interrupted("the printer reported an error during the sequence");
        }
        if let Some(why) = takeover(state) {
            return SettleStep::Interrupted(why);
        }
        match stg_cur {
            Some(s) if s == self.sentinel => SettleStep::Settled,
            _ => SettleStep::Waiting,
        }
    }

    /// The sentinel value, for diagnostics.
    pub fn sentinel(&self) -> i64 {
        self.sentinel
    }
}

/// Every `stg_cur` value a sequence sets for itself.
///
/// Those lines are queued behind the sequence's motion, so one of them arriving
/// mid-wait is indistinguishable from the sentinel unless the sentinel avoids
/// them. Parsing is deliberately loose — `M1002 gcode_claim_action : 14` and
/// `M1002 gcode_claim_action:54` both appear in Bambu's own output.
/// Matched case-insensitively, like [`crate::core::safety`] does: the printer
/// accepts `m1002` as readily as `M1002`, so a parser that only knows the
/// upper-case spelling would send such a line and leave it out of `claimed` —
/// and a claim missing from that list is exactly what can be mistaken for the
/// sentinel.
pub fn claimed_stages(lines: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<i64> {
    lines
        .into_iter()
        .filter_map(|l| {
            let upper = l.as_ref().to_ascii_uppercase();
            let rest = upper.strip_prefix("M1002")?.trim_start();
            let rest = rest.strip_prefix("GCODE_CLAIM_ACTION")?;
            rest.trim_start()
                .strip_prefix(':')?
                .trim()
                .parse::<i64>()
                .ok()
        })
        .collect()
}

/// Why the printer is no longer ours to drive, if it isn't.
///
/// Only states that indicate *someone started a job* count. `FINISH`/`FAILED`
/// are how an idle A1 sits after its last print, and `IDLE` is the fresh-boot
/// value — none of them are a takeover.
///
/// Public because "did someone else take over?" is also the question that
/// decides whether to put the displayed stage back: after a takeover it belongs
/// to their job, but after a fault the machine is still ours and still showing
/// our sentinel. Asking the printer's state answers both; asking why the wait
/// ended conflates them.
/// Why this printer must not be driven at all, if it must not.
///
/// Stricter than [`takeover`], and deliberately so — the two answer different
/// questions. Before the first line is published, the bar is "known idle": the
/// only states a print may be started from (`ensure_idle`'s set) are `IDLE`,
/// `FINISH` and `FAILED`. Anything else — booting, offline, or a state name
/// this build doesn't recognise — is *not known* to be idle, and refusing costs
/// only a retry where guessing costs a plate-changer swing into a moving
/// machine. A missing reading is refused for the same reason.
///
/// Once the sequence is running the bar moves, and `takeover` is what applies:
/// aborting mid-motion has its own cost, so only evidence that a job actually
/// started is worth stopping for.
pub fn blocks_start(state: Option<GcodeState>) -> Option<&'static str> {
    match state {
        Some(GcodeState::Idle | GcodeState::Finish | GcodeState::Failed) => None,
        Some(other) => takeover(Some(other)).or(Some(
            "the printer is not idle (it is starting up or unreachable)",
        )),
        None => Some("the printer did not report a state, so it is not known to be idle"),
    }
}

pub fn takeover(state: Option<GcodeState>) -> Option<&'static str> {
    match state? {
        GcodeState::Prepare | GcodeState::Running | GcodeState::Slicing => {
            Some("a print started on the printer while waiting")
        }
        GcodeState::Pause => Some("the printer entered a paused job while waiting"),
        // `Unknown` is a state name this build doesn't recognise, which a
        // future firmware could use for something running. Not treated as a
        // takeover — the sentinel still has to appear before anything proceeds,
        // so an unrecognised busy state times out rather than starting a print.
        GcodeState::Idle
        | GcodeState::Finish
        | GcodeState::Failed
        | GcodeState::Init
        | GcodeState::Offline
        | GcodeState::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_barrier_puts_m400_before_the_sentinel() {
        // Order is the whole point: the sentinel observed without a preceding
        // M400 is reached while the moves are still draining.
        let s = Settle::after(255, &[], None).unwrap();
        assert_eq!(
            s.gcode(),
            [
                "M400".to_string(),
                "M1002 gcode_claim_action : 200".to_string()
            ]
        );
    }

    #[test]
    fn the_sentinel_is_never_the_value_already_showing() {
        // A previous run that died before restoring leaves its sentinel behind;
        // reusing it would "observe" success without the printer doing anything.
        assert_eq!(Settle::after(200, &[], None).unwrap().sentinel(), 201);
        assert_eq!(Settle::after(255, &[], None).unwrap().sentinel(), 200);
        assert_eq!(Settle::after(14, &[], None).unwrap().sentinel(), 200);
    }

    #[test]
    fn the_sentinel_dodges_stages_the_sequence_sets_for_itself() {
        // Those lines are still queued when the wait starts, so one landing
        // later is indistinguishable from the sentinel.
        assert_eq!(Settle::after(255, &[200], None).unwrap().sentinel(), 201);
        assert_eq!(
            Settle::after(255, &[200, 201, 202], None)
                .unwrap()
                .sentinel(),
            203
        );
        assert_eq!(
            Settle::after(201, &[200, 202], None).unwrap().sentinel(),
            203
        );
    }

    #[test]
    fn sentinels_sit_clear_of_every_stage_vendor_gcode_claims() {
        // Bambu's own machine G-code claims values up to 75, plus 255. The
        // decoder in `core::stage` only names 0..=35, which is NOT the same
        // thing — anything the firmware can emit could be mistaken for our line.
        const HIGHEST_VENDOR_CLAIM: i64 = 75;
        for s in SENTINELS {
            assert!(
                s > HIGHEST_VENDOR_CLAIM && s != NO_STAGE,
                "{s} could be a stage the printer sets by itself"
            );
        }
    }

    #[test]
    fn claimed_stages_reads_both_spacings_bambu_itself_emits() {
        let lines = [
            "G28",
            "M1002 gcode_claim_action : 14",
            "M1002 gcode_claim_action:54",
            "M1002 judge_flag 1",
            "M1002 gcode_claim_action : notanumber",
        ];
        assert_eq!(claimed_stages(lines), vec![14, 54]);
    }

    #[test]
    fn claimed_stages_is_case_insensitive_because_the_printer_is() {
        // A lower-case claim is sent to the printer just the same. Missing it
        // here would let that queued line land during the wait and pass for the
        // sentinel — settled reported while the motion is still draining.
        assert_eq!(
            claimed_stages(["m1002 gcode_claim_action : 200"]),
            vec![200]
        );
        assert_eq!(claimed_stages(["M1002 GCODE_CLAIM_ACTION:201"]), vec![201]);
        assert_eq!(
            Settle::after(
                255,
                &claimed_stages(["m1002 gcode_claim_action : 200"]),
                None
            )
            .unwrap()
            .sentinel(),
            201,
            "the sentinel must dodge a claim however it was spelled"
        );
    }

    #[test]
    fn a_printer_not_known_to_be_idle_is_not_driven_at_all() {
        // Before anything is published the bar is "known idle" — the states a
        // print may be started from. Booting, unreachable, or a state name this
        // build doesn't know are all *unknown*, and a plate-changer swing into
        // one of those is not worth saving a retry.
        for ok in [GcodeState::Idle, GcodeState::Finish, GcodeState::Failed] {
            assert!(blocks_start(Some(ok)).is_none(), "{ok:?} is a normal start");
        }
        for no in [
            GcodeState::Running,
            GcodeState::Prepare,
            GcodeState::Pause,
            GcodeState::Slicing,
            GcodeState::Init,
            GcodeState::Offline,
            GcodeState::Unknown,
        ] {
            assert!(blocks_start(Some(no)).is_some(), "{no:?} must refuse");
        }
        assert!(
            blocks_start(None).is_some(),
            "no reading at all is not evidence of idleness"
        );
        // …but mid-motion the bar is different: only an actual job is worth
        // aborting a running sequence for.
        assert!(takeover(Some(GcodeState::Init)).is_none());
        assert!(takeover(Some(GcodeState::Running)).is_some());
    }

    #[test]
    fn the_wait_ends_only_on_our_own_sentinel() {
        let s = Settle::after(255, &[], None).unwrap();
        assert_eq!(s.observe(Some(255), None, None), SettleStep::Waiting);
        assert_eq!(s.observe(Some(14), None, None), SettleStep::Waiting);
        assert_eq!(s.observe(Some(201), None, None), SettleStep::Waiting); // another sentinel
        assert_eq!(s.observe(None, None, None), SettleStep::Waiting);
        assert_eq!(s.observe(Some(200), None, None), SettleStep::Settled);
    }

    #[test]
    fn a_job_taking_the_printer_over_stops_the_wait_before_the_sentinel_is_read() {
        let s = Settle::after(255, &[], None).unwrap();
        // Even with the sentinel present, a running job wins: proceeding to
        // start a print would be wrong, and "timed out" would misdescribe it.
        assert!(matches!(
            s.observe(Some(200), Some(GcodeState::Running), None),
            SettleStep::Interrupted(_)
        ));
        assert!(matches!(
            s.observe(None, Some(GcodeState::Prepare), None),
            SettleStep::Interrupted(_)
        ));
    }

    #[test]
    fn a_fault_during_the_sequence_ends_the_wait_instead_of_running_out_the_clock() {
        // A crashed toolhead halts the queue, so the sentinel never arrives.
        // Timing out eventually says the right thing far too late.
        let s = Settle::after(255, &[], None).unwrap();
        assert!(matches!(
            s.observe(None, Some(GcodeState::Idle), Some(0x0300_800B)),
            SettleStep::Interrupted(_)
        ));
    }

    #[test]
    fn a_stale_fault_clearing_is_the_machine_getting_better_not_a_new_one() {
        // A profile may be started from FAILED — that is how an A1 sits after a
        // cancelled print. The old code cleared to 0 part-way through would
        // differ from the baseline and, read as "changed", refuse the print
        // because the printer had stopped complaining.
        let s = Settle::after(255, &[], Some(0x0300_400C)).unwrap();
        assert_eq!(
            s.observe(Some(200), None, None),
            SettleStep::Settled,
            "the stale code clearing must not abort the wait"
        );
        assert_eq!(
            s.observe(None, None, Some(0x0300_400C)),
            SettleStep::Waiting,
            "the SAME code still present is the one we started with"
        );
        assert!(
            matches!(
                s.observe(None, None, Some(0x0300_800B)),
                SettleStep::Interrupted(_)
            ),
            "a different, nonzero code is a fault this sequence caused"
        );
    }

    #[test]
    fn the_states_an_idle_printer_actually_reports_are_not_a_takeover() {
        // A real A1 mini sits at FINISH after a good print and FAILED after a
        // cancelled one; both are the normal state to run a hook from.
        let s = Settle::after(255, &[], None).unwrap();
        for st in [GcodeState::Idle, GcodeState::Finish, GcodeState::Failed] {
            assert_eq!(s.observe(None, Some(st), None), SettleStep::Waiting);
            assert_eq!(s.observe(Some(200), Some(st), None), SettleStep::Settled);
        }
    }

    #[test]
    fn running_out_of_uncollided_sentinels_is_a_refusal_not_a_compromise() {
        // Settling for a value the sequence also claims would end the wait when
        // the sequence's own line lands — mid-motion — and report success. That
        // is worse than not waiting at all, so there is no fallback.
        let all: Vec<i64> = SENTINELS.to_vec();
        assert_eq!(Settle::after(255, &all, None), None);
        // One survivor is enough.
        let all_but_last: Vec<i64> = SENTINELS[..SENTINELS.len() - 1].to_vec();
        assert_eq!(
            Settle::after(255, &all_but_last, None).unwrap().sentinel(),
            SENTINELS[SENTINELS.len() - 1]
        );
        // The one the printer is showing counts as taken too.
        let mut all_but_first = SENTINELS.to_vec();
        all_but_first.remove(0);
        assert_eq!(Settle::after(SENTINELS[0], &all_but_first, None), None);
    }

    #[test]
    fn a_fault_is_not_a_takeover_so_the_sentinel_still_gets_cleaned_up() {
        // Both end the wait, but only one means the printer stopped being ours.
        // Deciding restore from the verdict would leave our out-of-band sentinel
        // on the display after every fault.
        assert!(takeover(Some(GcodeState::Running)).is_some());
        assert!(takeover(Some(GcodeState::Idle)).is_none());
        assert!(takeover(None).is_none());
        let s = Settle::after(255, &[], None).unwrap();
        assert!(
            matches!(
                s.observe(None, Some(GcodeState::Idle), Some(0x0300_800B)),
                SettleStep::Interrupted(_)
            ),
            "a fault interrupts the wait…"
        );
        assert!(
            takeover(Some(GcodeState::Idle)).is_none(),
            "…but the machine is still ours to tidy up"
        );
    }

    #[test]
    fn restore_returns_the_printer_to_no_stage() {
        assert_eq!(Settle::restore_gcode(), "M1002 gcode_claim_action : 255");
    }

    #[test]
    fn a_failed_wait_serialises_its_reason_not_just_a_flag() {
        // "settled: false" alone leaves an agent guessing between "still
        // moving", "someone else started a print" and "the line was refused".
        assert_eq!(
            serde_json::to_value(SettleOutcome::TimedOut { after_secs: 600 }).unwrap(),
            serde_json::json!({ "outcome": "timed_out", "after_secs": 600 })
        );
        assert_eq!(
            serde_json::to_value(SettleOutcome::Settled).unwrap(),
            serde_json::json!({ "outcome": "settled" })
        );
        assert!(SettleOutcome::Settled.failure().is_none());
        for o in [
            SettleOutcome::TimedOut { after_secs: 1 },
            SettleOutcome::Interrupted { reason: "x".into() },
            SettleOutcome::NotSent {
                gcode: "M400".into(),
            },
            SettleOutcome::NotReached,
        ] {
            assert!(o.failure().is_some(), "{o:?} must explain itself");
        }
    }
}
