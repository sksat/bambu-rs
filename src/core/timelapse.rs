//! Pure per-layer timelapse capture state machine, shared by the CLI
//! (`bambu timelapse capture`) and the server's serve-internal runner.
//!
//! Given a stream of [`PrinterStatus`] snapshots it decides — per snapshot —
//! whether to grab a frame (and its number + layer) or to end the watch. All
//! I/O (actually grabbing the frame, writing files, the MQTT/ws transport) is
//! the caller's; this stays free of it so the decision logic is exhaustively
//! unit-testable and identical across both call sites.

use crate::core::status::{GcodeState, PrinterStatus};

/// What the caller should do for one observed status snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureAction {
    /// Keep watching; nothing to do this tick.
    Continue,
    /// The watched print reached a terminal/abnormal state (or, without
    /// `wait`, was never active) — end the capture.
    Stop,
    /// A new layer crossed the `every` filter — grab frame `frame_no` (1-based).
    Capture { frame_no: u64, layer: i64 },
}

/// A print's identity. A changed identity means a *new* print, so a stale
/// `layer_num` carried over (e.g. across a reconnect, or one print ending and
/// another starting) can't suppress or mislabel the next print's first frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct PrintIdentity {
    task_id: Option<String>,
    subtask_id: Option<String>,
    gcode_file: Option<String>,
}

impl PrintIdentity {
    fn of(s: &PrinterStatus) -> Self {
        Self {
            task_id: s.task_id.clone(),
            subtask_id: s.subtask_id.clone(),
            gcode_file: s.gcode_file.clone(),
        }
    }
    /// An idle printer reports blank/`"0"` ids; those carry no signal, so don't
    /// treat the idle→idle transition as a "new print".
    fn is_meaningful(&self) -> bool {
        [&self.task_id, &self.subtask_id, &self.gcode_file]
            .into_iter()
            .any(|f| f.as_deref().is_some_and(|v| !v.is_empty() && v != "0"))
    }
}

/// Per-print capture state. One session captures one print: it watches for the
/// print to be active (optionally [`wait`](CaptureSession::new)ing for it),
/// emits a [`CaptureAction::Capture`] on each new layer that passes the `every`
/// filter, and [`Stop`](CaptureAction::Stop)s when the print reaches a terminal
/// state or errors.
pub struct CaptureSession {
    every: u64,
    wait: bool,
    last_layer: Option<i64>,
    frame_no: u64,
    seen_active: bool,
    identity: Option<PrintIdentity>,
}

impl CaptureSession {
    /// `every` = capture every Nth layer (clamped to >= 1). `wait` = sit through
    /// idle/finished/stale-error states until a print becomes active (so the
    /// session can be started before the print), rather than stopping at once.
    pub fn new(every: u64, wait: bool) -> Self {
        Self {
            every: every.max(1),
            wait,
            last_layer: None,
            frame_no: 0,
            seen_active: false,
            identity: None,
        }
    }

    /// Frames emitted so far (the last `Capture`'s `frame_no`).
    pub fn frames(&self) -> u64 {
        self.frame_no
    }

    /// Feed one status snapshot; get the action to take.
    pub fn observe(&mut self, s: &PrinterStatus) -> CaptureAction {
        let state = s.state();
        let active = is_active(state);
        if active {
            self.seen_active = true;
            // A new (meaningful) print identity resets per-print layer tracking,
            // so its first layer isn't suppressed by the previous print's.
            let id = PrintIdentity::of(s);
            if id.is_meaningful() && self.identity.as_ref() != Some(&id) {
                self.identity = Some(id);
                self.last_layer = None;
            }
        }

        // Stop takes priority over a coincident capture so termination is always
        // clean — the final layer was already captured on the last active tick.
        if self.should_stop(state, s.error.is_some()) {
            return CaptureAction::Stop;
        }

        if active
            && let Some(layer) = s.layer_num
            && self.last_layer != Some(layer)
        {
            self.last_layer = Some(layer);
            // Capture every Nth layer (layer 0 = the first reported layer).
            if layer >= 0 && (layer as u64).is_multiple_of(self.every) {
                self.frame_no += 1;
                return CaptureAction::Capture {
                    frame_no: self.frame_no,
                    layer,
                };
            }
        }
        CaptureAction::Continue
    }

    fn should_stop(&self, state: Option<GcodeState>, has_error: bool) -> bool {
        should_stop(self.wait, self.seen_active, state, has_error)
    }
}

/// A print is "active" (and so its `layer_num` is meaningful) only in these
/// states; an idle printer's stale `layer_num` must not trigger a frame.
fn is_active(state: Option<GcodeState>) -> bool {
    matches!(
        state,
        Some(GcodeState::Running | GcodeState::Prepare | GcodeState::Pause)
    )
}

/// Whether a watch should end: shared by the layer-driven [`CaptureSession`] and
/// the time-driven [`PrintActivitySession`]. While `wait`ing for a print to start
/// (not yet `seen_active`) nothing ends it; an error ends it; otherwise a terminal
/// state (finish/failed/idle) does.
fn should_stop(wait: bool, seen_active: bool, state: Option<GcodeState>, has_error: bool) -> bool {
    if wait && !seen_active {
        return false;
    }
    if has_error {
        return true;
    }
    matches!(
        state,
        Some(GcodeState::Finish | GcodeState::Failed | GcodeState::Idle)
    )
}

/// What a time-sampled ("plain") runner should do on a tick — capture only while
/// the print is active, end when it finishes/errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityAction {
    /// The print is active — grab a frame now.
    Capture,
    /// Not active yet (idle/preparing-to-start while `wait`ing) — skip this tick.
    Idle,
    /// The print ended (or errored) — stop the runner.
    Stop,
}

/// Tracks only a print's *lifecycle* (active → ended), no layer logic — for a
/// runner that samples frames on its own clock (a wall-time interval) rather than
/// per layer. Shares the active/stop rules with [`CaptureSession`] so both agree
/// on when a print starts and ends.
pub struct PrintActivitySession {
    wait: bool,
    seen_active: bool,
}

impl PrintActivitySession {
    /// `wait` = sit through idle/finished until a print becomes active (so the
    /// runner can be armed before the print starts), rather than stopping at once.
    pub fn new(wait: bool) -> Self {
        Self {
            wait,
            seen_active: false,
        }
    }

    /// Feed the current status (call on each sampling tick). `Stop` takes priority
    /// over `Capture` so a finishing print ends cleanly instead of grabbing once
    /// more.
    pub fn observe(&mut self, s: &PrinterStatus) -> ActivityAction {
        let state = s.state();
        let active = is_active(state);
        if active {
            self.seen_active = true;
        }
        if should_stop(self.wait, self.seen_active, state, s.error.is_some()) {
            return ActivityAction::Stop;
        }
        if active {
            ActivityAction::Capture
        } else {
            ActivityAction::Idle
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::status::DeviceError;

    fn st(state: &str, layer: Option<i64>) -> PrinterStatus {
        PrinterStatus {
            gcode_state: Some(state.to_string()),
            layer_num: layer,
            ..Default::default()
        }
    }

    fn cap(a: CaptureAction) -> Option<(u64, i64)> {
        match a {
            CaptureAction::Capture { frame_no, layer } => Some((frame_no, layer)),
            _ => None,
        }
    }

    #[test]
    fn idle_does_not_capture_a_stale_layer() {
        let mut s = CaptureSession::new(1, true);
        // Idle with a leftover layer_num must not fire (and with --wait, must not stop).
        assert_eq!(s.observe(&st("IDLE", Some(42))), CaptureAction::Continue);
        assert_eq!(s.frames(), 0);
    }

    #[test]
    fn first_active_layer_captures_once_then_no_recapture_on_same_layer() {
        let mut s = CaptureSession::new(1, false);
        assert_eq!(cap(s.observe(&st("RUNNING", Some(1)))), Some((1, 1)));
        // Same layer again -> no recapture.
        assert_eq!(s.observe(&st("RUNNING", Some(1))), CaptureAction::Continue);
        // Next layer -> capture.
        assert_eq!(cap(s.observe(&st("RUNNING", Some(2)))), Some((2, 2)));
    }

    #[test]
    fn every_n_filters_layers_including_layer_zero() {
        let mut s = CaptureSession::new(2, false);
        assert_eq!(cap(s.observe(&st("RUNNING", Some(0)))), Some((1, 0))); // 0 % 2 == 0
        assert_eq!(s.observe(&st("RUNNING", Some(1))), CaptureAction::Continue); // odd skipped
        assert_eq!(cap(s.observe(&st("RUNNING", Some(2)))), Some((2, 2)));
        assert_eq!(s.observe(&st("RUNNING", Some(3))), CaptureAction::Continue);
    }

    #[test]
    fn negative_or_missing_layer_does_nothing() {
        let mut s = CaptureSession::new(1, false);
        assert_eq!(s.observe(&st("RUNNING", Some(-1))), CaptureAction::Continue);
        assert_eq!(s.observe(&st("RUNNING", None)), CaptureAction::Continue);
        assert_eq!(s.frames(), 0);
    }

    #[test]
    fn without_wait_idle_or_finish_stops_immediately() {
        assert_eq!(
            CaptureSession::new(1, false).observe(&st("IDLE", None)),
            CaptureAction::Stop
        );
        assert_eq!(
            CaptureSession::new(1, false).observe(&st("FINISH", Some(100))),
            CaptureAction::Stop
        );
    }

    #[test]
    fn with_wait_sits_through_idle_finish_and_stale_error_until_active() {
        let mut s = CaptureSession::new(1, true);
        assert_eq!(s.observe(&st("IDLE", None)), CaptureAction::Continue);
        assert_eq!(s.observe(&st("FINISH", Some(100))), CaptureAction::Continue);
        // A stale print_error from the previous print is ignored while waiting.
        let mut errd = st("FINISH", Some(100));
        errd.error = DeviceError::from_code(0x05004003);
        assert_eq!(s.observe(&errd), CaptureAction::Continue);
        // Then the new print starts and captures.
        assert_eq!(cap(s.observe(&st("RUNNING", Some(1)))), Some((1, 1)));
    }

    #[test]
    fn after_active_a_terminal_or_error_stops() {
        let mut s = CaptureSession::new(1, true);
        assert!(cap(s.observe(&st("RUNNING", Some(1)))).is_some());
        assert_eq!(s.observe(&st("FINISH", Some(1))), CaptureAction::Stop);

        let mut s = CaptureSession::new(1, true);
        assert!(cap(s.observe(&st("RUNNING", Some(1)))).is_some());
        let mut errd = st("RUNNING", Some(1));
        errd.error = DeviceError::from_code(0x1234);
        assert_eq!(s.observe(&errd), CaptureAction::Stop);
    }

    #[test]
    fn pause_counts_as_active() {
        let mut s = CaptureSession::new(1, false);
        assert_eq!(cap(s.observe(&st("PAUSE", Some(5)))), Some((1, 5)));
    }

    #[test]
    fn a_new_print_identity_resets_layer_tracking() {
        let mut s = CaptureSession::new(1, true);
        let mut a = st("RUNNING", Some(10));
        a.task_id = Some("task-A".into());
        assert!(cap(s.observe(&a)).is_some()); // captures layer 10 of print A
        // Print B starts; its layer_num restarts at 0 < 10. Without the identity
        // reset, last_layer=10 would let 0 through anyway (10 != 0), but a print
        // that begins again at the SAME layer must still fire — assert the reset
        // makes a repeated layer number of a new print capture.
        let mut b = st("RUNNING", Some(10));
        b.task_id = Some("task-B".into());
        assert!(
            cap(s.observe(&b)).is_some(),
            "a new print at the same layer number must capture (identity reset)"
        );
    }

    // ── PrintActivitySession (time-sampled "plain" lifecycle) ──

    #[test]
    fn plain_waits_through_idle_then_captures_while_active() {
        let mut a = PrintActivitySession::new(true);
        assert_eq!(a.observe(&st("IDLE", None)), ActivityAction::Idle); // armed before print
        assert_eq!(a.observe(&st("RUNNING", None)), ActivityAction::Capture);
        assert_eq!(a.observe(&st("PAUSE", None)), ActivityAction::Capture); // pause still active
    }

    #[test]
    fn plain_stops_when_the_print_finishes() {
        let mut a = PrintActivitySession::new(true);
        assert_eq!(a.observe(&st("RUNNING", None)), ActivityAction::Capture);
        assert_eq!(a.observe(&st("FINISH", None)), ActivityAction::Stop);
    }

    #[test]
    fn plain_stops_on_a_device_error() {
        let mut a = PrintActivitySession::new(true);
        a.observe(&st("RUNNING", None));
        let mut e = st("RUNNING", None);
        e.error = Some(DeviceError::from_code(0x1200_8016).unwrap());
        assert_eq!(a.observe(&e), ActivityAction::Stop);
    }

    #[test]
    fn plain_without_wait_stops_if_never_active() {
        let mut a = PrintActivitySession::new(false);
        assert_eq!(a.observe(&st("IDLE", None)), ActivityAction::Stop);
    }
}
