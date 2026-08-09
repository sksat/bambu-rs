//! The player's state, read as a printer's temperatures.
//!
//! The half of [`super`] that faces the client the other way. The game comes
//! back not only as pictures: the player's health is reported as the nozzle
//! temperature, so a client that knows nothing about any of this still shows
//! the game state on the readout it puts in the largest type on the screen.

use serde_json::Value;

use crate::core::report::is_status_report;
use crate::core::safety::TempLimits;

// ---- the game, reported as a printer ------------------------------------

/// How the player is doing, as the engine last said.
///
/// `None` means the engine has not said — it is at the title screen, or it is
/// some other program that only draws pictures. Not the same as zero, which is
/// a dead player, and the difference decides whether the relay touches the
/// report at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Vitals {
    pub health: Option<i16>,
    pub armour: Option<i16>,
}

/// The nozzle temperature that stands for full health.
///
/// 220 °C is what the captured A1 mini reports as its target for PLA, so a
/// client showing "220" is showing a number it would show during a real print.
/// The point of putting health here is that it needs no explanation: a printer
/// client already renders the nozzle in the largest type on the screen.
const PRINTING_C: f64 = 220.0;

/// The nozzle temperature that stands for none.
///
/// Room temperature, not zero: a machine that has been off all night reports
/// about this, and 0 °C is a reading no printer in a heated room ever gives. A
/// dead player is a cold nozzle, which is both the joke and a plausible number.
const AMBIENT_C: f64 = 25.0;

/// The bed temperature that stands for full armour, and none.
///
/// 60 °C is the captured machine's PLA bed target. Armour goes on the bed
/// because it is the other number a client shows next to the nozzle, and
/// because a second bar wants a second readout. Ammo deliberately goes
/// nowhere: there is no field on a printer's face where a count of bullets
/// reads as anything but a wrong number, and the game already draws it.
const BED_C: f64 = 60.0;

/// Full health and full armour, in DOOM's own units. Both go to 200 with a
/// sphere; 100 is what the game calls full and what the status bar shows as
/// `100%`.
const FULL: f64 = 100.0;

/// The nozzle temperature for `health`.
///
/// Linear from [`AMBIENT_C`] at zero to [`PRINTING_C`] at 100. A soulsphere
/// takes health past 100 and the nozzle past its printing temperature — the
/// player is running hot, which is exactly right — but not past the ceiling
/// [`crate::core::safety`] puts on a nozzle, because a client should never be
/// shown a temperature this crate would refuse to command.
pub fn nozzle_for_health(health: i16) -> f64 {
    let ceiling = TempLimits::default().max_nozzle;
    (AMBIENT_C + f64::from(health.max(0)) / FULL * (PRINTING_C - AMBIENT_C)).min(ceiling)
}

/// The bed temperature for `armour`, on the same scale and with the same cap.
pub fn bed_for_armour(armour: i16) -> f64 {
    let ceiling = TempLimits::default().max_bed;
    (AMBIENT_C + f64::from(armour.max(0)) / FULL * (BED_C - AMBIENT_C)).min(ceiling)
}

/// Put the player's state into a report on its way to a client.
///
/// The targets are set as well as the readings, and to the *full-health*
/// values rather than to anything the game is doing: a client draws current
/// against target, so "148 / 220" is a health bar without the client having
/// been told it is one.
///
/// Only a `push_status` is touched. An ACK wears the same envelope, and a
/// temperature spliced into a command's answer would be a reading in a place
/// nothing reads — the same rule [`crate::core::camerad::claim_camera`] follows,
/// for the same reason.
///
/// Returns whether anything changed.
pub fn report_vitals(message: &mut Value, vitals: Vitals) -> bool {
    if vitals == Vitals::default() {
        return false;
    }
    // `is_status_report` also passes a `get_version` inventory, which has no
    // `print` object to write into.
    if message.get("print").is_none() || !is_status_report(message) {
        return false;
    }
    let Some(print) = message.get_mut("print").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    // Written into every report, delta or snapshot, whether or not the printer
    // mentioned temperature: a health bar that only moved when the machine
    // happened to talk about its nozzle would lag the game by seconds.
    for (key, value) in [
        ("nozzle_temper", vitals.health.map(nozzle_for_health)),
        ("nozzle_target_temper", vitals.health.map(|_| PRINTING_C)),
        ("bed_temper", vitals.armour.map(bed_for_armour)),
        ("bed_target_temper", vitals.armour.map(|_| BED_C)),
    ] {
        let Some(value) = value else { continue };
        let value = Value::from((value * 10.0).round() / 10.0);
        if print.insert(key.to_string(), value.clone()) != Some(value) {
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A report shaped like the printer's own, with the fields the overlay
    /// writes into already present and wrong.
    fn a_report() -> Value {
        json!({"print": {
            "command": "push_status", "msg": 0,
            "nozzle_temper": 24.0, "nozzle_target_temper": 0.0,
            "bed_temper": 24.0, "bed_target_temper": 0.0,
            "gcode_state": "IDLE",
        }})
    }

    #[test]
    fn full_health_is_a_temperature_a_printer_really_prints_at() {
        // The whole joke depends on the number being unremarkable: someone
        // glancing at Studio should see a printer at 220 °C, not a fault.
        assert_eq!(nozzle_for_health(100), 220.0);
        // And nothing is a cold machine, not a broken sensor.
        assert_eq!(nozzle_for_health(0), 25.0);
        // Halfway is halfway.
        assert!((nozzle_for_health(50) - 122.5).abs() < 0.01);
        assert_eq!(bed_for_armour(100), 60.0);
        assert_eq!(bed_for_armour(0), 25.0);
    }

    #[test]
    fn health_only_ever_goes_up_with_health() {
        let mut last = f64::MIN;
        for health in 0..=200 {
            let now = nozzle_for_health(health);
            assert!(now >= last, "{health} went backwards: {now} after {last}");
            last = now;
        }
    }

    #[test]
    fn a_soulsphere_runs_hot_but_never_past_what_this_crate_would_command() {
        // Health goes to 200, and 200 on this scale is 415 °C — a number no
        // printer shows and this crate's own safety layer would refuse to send.
        // Above full health the nozzle should read hot, not broken.
        let limits = TempLimits::default();
        assert!(nozzle_for_health(150) > 220.0, "a sphere should run hot");
        assert_eq!(nozzle_for_health(200), limits.max_nozzle);
        assert_eq!(nozzle_for_health(i16::MAX), limits.max_nozzle);
        assert!(bed_for_armour(i16::MAX) <= limits.max_bed);
        // A negative reading cannot come out of `parse_vitals`, but the mapping
        // must not produce a nonsense temperature if one ever does.
        assert_eq!(nozzle_for_health(-50), 25.0);
    }

    #[test]
    fn a_report_carries_the_players_health_as_the_nozzle() {
        let mut report = a_report();
        assert!(report_vitals(
            &mut report,
            Vitals {
                health: Some(100),
                armour: Some(50),
            }
        ));
        assert_eq!(report["print"]["nozzle_temper"], 220.0);
        assert_eq!(report["print"]["bed_temper"], 42.5);
        // The targets are the FULL-health values, not the game's: a client
        // draws current against target, so this is a health bar it was never
        // told about.
        assert_eq!(report["print"]["nozzle_target_temper"], 220.0);
        assert_eq!(report["print"]["bed_target_temper"], 60.0);
        // Everything else is the printer's to say.
        assert_eq!(report["print"]["gcode_state"], "IDLE");
    }

    #[test]
    fn a_delta_that_never_mentioned_temperature_carries_it_anyway() {
        // Health has to move when the player is hit, not when the printer next
        // happens to talk about its nozzle.
        let mut delta = json!({"print": {"command": "push_status", "msg": 1, "mc_percent": 4}});
        assert!(report_vitals(
            &mut delta,
            Vitals {
                health: Some(75),
                armour: None,
            }
        ));
        assert_eq!(delta["print"]["nozzle_temper"], 171.3);
        assert!(
            delta["print"].get("bed_temper").is_none(),
            "armour was unknown; the bed is the printer's own"
        );
    }

    #[test]
    fn nothing_is_written_when_the_game_has_not_said() {
        // An engine at the title screen, or one that is not a game at all. The
        // printer's own readings must survive that.
        let mut report = a_report();
        assert!(!report_vitals(&mut report, Vitals::default()));
        assert_eq!(report, a_report());
    }

    #[test]
    fn an_ack_is_not_a_place_to_report_a_temperature() {
        // An ACK wears the same envelope as a report. A nozzle reading spliced
        // into a command's answer is a number where nothing reads one — and it
        // would travel into the cache and out again with every snapshot.
        for mut message in [
            json!({"print": {"sequence_id": "1", "param": "G28", "result": "success"}}),
            json!({"system": {"sequence_id": "1", "result": "success"}}),
            json!({"info": {"command": "get_version", "module": []}}),
        ] {
            let before = message.clone();
            assert!(
                !report_vitals(
                    &mut message,
                    Vitals {
                        health: Some(100),
                        armour: Some(100)
                    }
                ),
                "{before}"
            );
            assert_eq!(message, before);
        }
    }

    #[test]
    fn writing_the_same_reading_twice_changes_nothing() {
        // The caller re-encodes a message it was told changed; a health bar
        // that has not moved should not make work.
        let vitals = Vitals {
            health: Some(80),
            armour: Some(20),
        };
        let mut report = a_report();
        assert!(report_vitals(&mut report, vitals));
        assert!(!report_vitals(&mut report, vitals));
    }
}
