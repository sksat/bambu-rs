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
//! The array rule is the least certain: Bambu is known to sometimes send partial
//! AMS-tray updates indexed by slot. If the A1 mini turns out to do that, we will
//! add an opt-in per-path array-merge policy — but only once observed on real
//! hardware. The engine applies messages in **arrival order** (last-writer-wins);
//! reordering / dropping stale deltas is the transport layer's responsibility.

use serde_json::{Map, Value};

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
    pub fn apply(&mut self, message: Value) {
        merge_into(&mut self.state, &message);
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
