//! Printer report state and the pushall→delta merge engine.
//!
//! P1/A1-class printers (including the A1 mini) push only **deltas** after an
//! initial `pushall` snapshot, so the client must cache the state and merge each
//! incoming message into it. This module is the structural merge engine and the
//! cached-state container; the **typed** accessors (gcode_state, temperatures,
//! AMS, HMS, …) are added once we have real A1 mini captures to derive the exact
//! field names from — until then we keep the state as untyped JSON.
//!
//! ## Merge semantics (clean-room — confirm against the device)
//!
//! - **Objects** merge recursively (this is what makes a delta a delta: keys the
//!   delta doesn't mention are retained).
//! - **Everything else** (scalars, **arrays**, `null`) in the delta **replaces**
//!   the cached value. In particular `null` sets the value to `null`; it does
//!   *not* delete the key (this differs from RFC 7386 JSON Merge Patch).
//! - **Arrays are replaced wholesale**, not merged element-by-element.
//!
//! The wholesale-array rule was the least certain (some Bambu docs hint at partial
//! AMS-tray updates), so we **observed it**: over a 180 s capture during which an
//! AMS-Lite slot was physically pulled and reinserted, the A1 mini emitted **zero**
//! autonomous `ams` deltas — `ams` (the complete `ams.ams[].tray[]` structure)
//! appeared only in the full `pushall`. So on the A1 mini there are no partial
//! tray-array deltas and wholesale replacement is correct; a per-path array-merge
//! policy would only add a stale-entry risk for an unobserved case. Re-confirmed
//! during a real 2-colour print: a red→black swap surfaced only via the scalar
//! `tray_now`/`tray_pre`/`tray_tar` deltas; the `ams.ams[].tray[]` array still
//! came only in full pushalls, never partially. (Other models: not verified.)
//! The engine applies messages in **arrival order** (last-writer-wins); reordering
//! / dropping stale deltas is the transport layer's responsibility.

use serde_json::{Map, Value};

/// Whether a **single raw report message** is the full `pushall` response.
///
/// Observed on the A1 mini: the full snapshot carries `print.msg == 0` (~64
/// fields), while periodic deltas carry `print.msg == 1` (a few fields) — and
/// **both** set `command == "push_status"`, so the command alone isn't a reliable
/// full-vs-delta signal. Inspect the **raw incoming message** (not merged state):
/// `msg` is per-message, so a merged delta would overwrite a snapshot's `msg == 0`.
/// Missing `msg` falls back to the command check (older firmware may omit it).
pub fn is_full_snapshot_message(message: &Value) -> bool {
    let print = message.get("print");
    let is_push_status = print
        .and_then(|p| p.get("command"))
        .and_then(|v| v.as_str())
        == Some("push_status");
    let msg = print.and_then(|p| p.get("msg")).and_then(|v| v.as_i64());
    is_push_status && msg.is_none_or(|m| m == 0)
}

/// Recursively merge `delta` into `target` (see the module docs for semantics).
pub fn merge_into(target: &mut Value, delta: &Value) {
    if let (Value::Object(t), Value::Object(d)) = (&mut *target, delta) {
        for (key, value) in d {
            match t.get_mut(key) {
                Some(existing) => merge_into(existing, value),
                None => {
                    t.insert(key.clone(), value.clone());
                }
            }
        }
    } else {
        *target = delta.clone();
    }
}

/// The fields that identify a print job (`task_id` / `subtask_id` / `gcode_file`)
/// — stable for a job's life, so a change means a *different* print. Empty strings
/// read as absent, matching the typed accessors.
type PrintIdentity = (Option<String>, Option<String>, Option<String>);

fn print_identity(state: &Value) -> PrintIdentity {
    let field = |key: &str| {
        state
            .pointer(&format!("/print/{key}"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    (field("task_id"), field("subtask_id"), field("gcode_file"))
}

/// Whether an identity names an actual print (vs. the empty post-teardown state),
/// so the progress reset fires when a new job *starts*, not when one ends.
fn is_meaningful(id: &PrintIdentity) -> bool {
    id.0.is_some() || id.1.is_some() || id.2.is_some()
}

/// The cached, merged printer state.
///
/// Seed it with the `pushall` snapshot and feed every subsequent report message
/// through [`ReportState::apply`]; both go through the same merge so a snapshot
/// is just a delta that happens to contain everything.
#[derive(Debug, Clone)]
pub struct ReportState {
    state: Value,
}

impl Default for ReportState {
    fn default() -> Self {
        Self::new()
    }
}

impl ReportState {
    /// A fresh, empty state.
    pub fn new() -> Self {
        Self {
            state: Value::Object(Map::new()),
        }
    }

    /// Merge one report message (snapshot or delta) into the cached state.
    ///
    /// A new print inherits the just-finished job's `mc_percent`/`layer_num`:
    /// the printer reports the new `task_id` well before the first fresh percent,
    /// so a job in preheat/calibration would read 100% / the old layer count.
    /// When the merge reveals a new (different, non-empty) print identity, zero
    /// those carried-over fields — but only the ones this very message didn't set,
    /// so a report that brings its own fresh progress is trusted.
    pub fn apply(&mut self, message: Value) {
        let before = print_identity(&self.state);
        merge_into(&mut self.state, &message);
        let after = print_identity(&self.state);
        if after != before
            && is_meaningful(&after)
            && let Some(print) = self.state.get_mut("print").and_then(Value::as_object_mut)
        {
            for field in ["mc_percent", "layer_num"] {
                if message.pointer(&format!("/print/{field}")).is_none() {
                    print.insert(field.to_string(), Value::from(0));
                }
            }
        }
    }

    /// The full merged state as JSON.
    pub fn get(&self) -> &Value {
        &self.state
    }

    /// Look up a value by JSON Pointer (e.g. `"/print/gcode_state"`).
    pub fn pointer(&self, pointer: &str) -> Option<&Value> {
        self.state.pointer(pointer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn objects_merge_recursively_keeping_unmentioned_keys() {
        let mut state = json!({ "print": { "a": 1, "b": 2, "nested": { "x": 1 } } });
        merge_into(
            &mut state,
            &json!({ "print": { "b": 3, "c": 4, "nested": { "y": 2 } } }),
        );
        assert_eq!(
            state,
            json!({ "print": { "a": 1, "b": 3, "c": 4, "nested": { "x": 1, "y": 2 } } })
        );
    }

    #[test]
    fn scalars_are_replaced() {
        let mut state = json!({ "temp": 200 });
        merge_into(&mut state, &json!({ "temp": 215 }));
        assert_eq!(state, json!({ "temp": 215 }));
    }

    #[test]
    fn arrays_are_replaced_wholesale_not_element_merged() {
        let mut state = json!({ "trays": [1, 2, 3] });
        merge_into(&mut state, &json!({ "trays": [9] }));
        assert_eq!(state, json!({ "trays": [9] }));
    }

    #[test]
    fn null_replaces_value_but_keeps_the_key() {
        let mut state = json!({ "k": 5 });
        merge_into(&mut state, &json!({ "k": null }));
        assert_eq!(state, json!({ "k": null }));
        assert!(state.as_object().unwrap().contains_key("k"));
    }

    // ── new-print progress reset ──
    // A new print inherits the finished job's mc_percent/layer_num until the
    // printer pushes fresh values — it reports the new task_id well before the
    // first percent — so a just-started job reads 100%. Detect the new print by
    // its identity and zero the stale carryover.

    #[test]
    fn a_new_print_zeroes_progress_carried_over_from_the_last_job() {
        let mut rs = ReportState::new();
        rs.apply(json!({ "print": {
            "task_id": "A", "gcode_file": "a.3mf", "mc_percent": 100, "layer_num": 60,
        }}));
        // New print B arrives (preheat) with the new identity but no progress yet.
        rs.apply(json!({ "print": {
            "task_id": "B", "gcode_file": "b.3mf", "gcode_state": "PREPARE",
        }}));
        let p = rs.pointer("/print").unwrap();
        assert_eq!(p.get("mc_percent"), Some(&json!(0)), "stale 100% must reset on a new print");
        assert_eq!(p.get("layer_num"), Some(&json!(0)));
    }

    #[test]
    fn a_progress_delta_within_the_same_print_is_kept() {
        let mut rs = ReportState::new();
        rs.apply(json!({ "print": { "task_id": "A", "mc_percent": 30, "layer_num": 5 } }));
        rs.apply(json!({ "print": { "mc_percent": 31 } })); // same print, later delta
        let p = rs.pointer("/print").unwrap();
        assert_eq!(p.get("mc_percent"), Some(&json!(31)), "same-print progress must not be zeroed");
        assert_eq!(p.get("layer_num"), Some(&json!(5)));
    }

    #[test]
    fn a_new_print_that_brings_its_own_percent_keeps_it() {
        let mut rs = ReportState::new();
        rs.apply(json!({ "print": { "task_id": "A", "mc_percent": 100 } }));
        rs.apply(json!({ "print": { "task_id": "B", "mc_percent": 7 } }));
        assert_eq!(rs.pointer("/print/mc_percent"), Some(&json!(7)), "trust a fresh percent");
    }

    #[test]
    fn finishing_a_print_keeps_its_final_progress() {
        // Identity clears on teardown; the reset must only fire for a NEW print,
        // so a finished job still reads 100%.
        let mut rs = ReportState::new();
        rs.apply(json!({ "print": { "task_id": "A", "mc_percent": 100, "layer_num": 60 } }));
        rs.apply(json!({ "print": { "task_id": "", "gcode_state": "FINISH" } }));
        assert_eq!(rs.pointer("/print/mc_percent"), Some(&json!(100)), "a finished print keeps 100%");
    }

    #[test]
    fn new_keys_are_added() {
        let mut state = json!({ "a": 1 });
        merge_into(&mut state, &json!({ "b": 2 }));
        assert_eq!(state, json!({ "a": 1, "b": 2 }));
    }

    #[test]
    fn last_writer_wins_so_a_stale_delta_reverts_overlapping_fields() {
        // Documents that the engine has no staleness protection: applying an old
        // delta after a newer one reverts the overlapping field. Ordering is the
        // transport layer's job.
        let mut state = json!({ "layer": 0 });
        merge_into(&mut state, &json!({ "layer": 10 })); // newer
        merge_into(&mut state, &json!({ "layer": 5 })); // stale, re-applied late
        assert_eq!(state, json!({ "layer": 5 }));
    }

    #[test]
    fn report_state_seeds_then_merges_deltas() {
        let mut rs = ReportState::new();
        rs.apply(json!({ "print": { "gcode_state": "RUNNING", "layer_num": 1 } }));
        rs.apply(json!({ "print": { "layer_num": 2 } })); // delta
        assert_eq!(rs.pointer("/print/gcode_state"), Some(&json!("RUNNING")));
        assert_eq!(rs.pointer("/print/layer_num"), Some(&json!(2)));
    }

    #[test]
    fn full_snapshot_is_msg_zero_not_just_push_status() {
        // Full pushall response: push_status + msg 0.
        assert!(is_full_snapshot_message(
            &json!({ "print": { "command": "push_status", "msg": 0 } })
        ));
        // A delta also says push_status but msg == 1 -> NOT the full snapshot.
        assert!(!is_full_snapshot_message(
            &json!({ "print": { "command": "push_status", "msg": 1 } })
        ));
        // Older firmware without msg: fall back to the command check.
        assert!(is_full_snapshot_message(
            &json!({ "print": { "command": "push_status" } })
        ));
        // Not a push_status at all.
        assert!(!is_full_snapshot_message(
            &json!({ "print": { "command": "gcode_line" } })
        ));
    }

    // --- property-based tests -------------------------------------------------

    fn arb_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|n| json!(n)),
            "[a-z0-9]{0,5}".prop_map(Value::String),
        ];
        leaf.prop_recursive(4, 24, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
                prop::collection::vec(("[a-z]{1,4}", inner), 0..5)
                    .prop_map(|kvs| Value::Object(kvs.into_iter().collect())),
            ]
        })
    }

    fn arb_object() -> impl Strategy<Value = Value> {
        prop::collection::vec(("[a-z]{1,4}", arb_json()), 0..6)
            .prop_map(|kvs| Value::Object(kvs.into_iter().collect()))
    }

    fn prefix_keys(v: &Value, prefix: &str) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.iter()
                    .map(|(k, val)| (format!("{prefix}{k}"), val.clone()))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    proptest! {
        /// Re-applying the same delta never changes the result.
        #[test]
        fn merge_is_idempotent(base in arb_object(), delta in arb_object()) {
            let mut once = base.clone();
            merge_into(&mut once, &delta);
            let mut twice = once.clone();
            merge_into(&mut twice, &delta);
            prop_assert_eq!(once, twice);
        }

        /// Keys present in the base but absent from the delta are retained.
        #[test]
        fn base_keys_absent_from_delta_are_retained(base in arb_object(), delta in arb_object()) {
            let mut merged = base.clone();
            merge_into(&mut merged, &delta);
            if let (Value::Object(b), Value::Object(d), Value::Object(m)) = (&base, &delta, &merged) {
                for (k, v) in b {
                    if !d.contains_key(k) {
                        prop_assert_eq!(m.get(k), Some(v));
                    }
                }
            }
        }

        /// Two deltas touching disjoint top-level keys commute.
        #[test]
        fn disjoint_deltas_commute(base in arb_object(), a in arb_object(), b in arb_object()) {
            let a = prefix_keys(&a, "a_");
            let b = prefix_keys(&b, "b_");

            let mut ab = base.clone();
            merge_into(&mut ab, &a);
            merge_into(&mut ab, &b);

            let mut ba = base.clone();
            merge_into(&mut ba, &b);
            merge_into(&mut ba, &a);

            prop_assert_eq!(ab, ba);
        }
    }
}
