//! Print-stage decoding (`stg_cur`).
//!
//! `stg_cur` is a small integer naming the printer's *current activity*. It is
//! the one report signal that tracks ad-hoc motion — homing, bed leveling,
//! calibration sweeps, filament changes — independently of the coarse
//! [`GcodeState`](crate::core::status::GcodeState).
//!
//! **Read it together with `gcode_state`, not alone.** Stage `0` ("printing")
//! is the *no-special-stage* default: a real A1 mini reports `stg_cur = 0` both
//! while laying down filament **and** while idle (confirmed by the idle
//! `pushall` fixture, where `gcode_state = IDLE` but `stg_cur = 0`). So `0`
//! means "nothing special is happening" — what's actually happening is told by
//! `gcode_state`.
//!
//! Provenance: the id→name table is transcribed from the OpenBambuAPI spec
//! (a *spec*, not an implementation). Stages annotated `[observed]` were seen
//! on a real A1 mini — e.g. a `bed_level + vibration` calibration walked
//! `stg_cur` through `14` (cleaning nozzle tip) → `3` (sweeping XY / vibration)
//! → `1` (auto bed leveling), matching the requested calibration. Names for
//! unobserved ids are the spec's claim, faithfully copied, not device-verified.

/// A printer activity stage (`stg_cur`).
///
/// A newtype over the raw id so that unknown / future-firmware stages
/// round-trip losslessly: [`Stage::name`] returns `None` for them while the
/// underlying integer is preserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stage(pub i64);

impl Stage {
    /// The spec name for this stage, or `None` for an id we have no name for
    /// (negative sentinels and unknown/future stages).
    pub fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            0 => "printing",          // no-special-stage default (also seen while idle)
            1 => "auto_bed_leveling", // [observed]
            2 => "heatbed_preheating",
            3 => "sweeping_xy_mech_mode", // [observed] (vibration sweep)
            4 => "changing_filament",
            5 => "m400_pause",
            6 => "paused_filament_runout",
            7 => "heating_hotend",
            8 => "calibrating_extrusion",
            9 => "scanning_bed_surface",
            10 => "inspecting_first_layer",
            11 => "identifying_build_plate_type",
            12 => "calibrating_micro_lidar",
            13 => "homing_toolhead",
            14 => "cleaning_nozzle_tip", // [observed]
            15 => "checking_extruder_temperature",
            16 => "paused_user",
            17 => "paused_front_cover_falling",
            18 => "calibrating_lidar",
            19 => "calibrating_extrusion_flow",
            20 => "paused_nozzle_temperature_malfunction",
            21 => "paused_heatbed_temperature_malfunction",
            22 => "filament_unloading",
            23 => "paused_skipped_step",
            24 => "filament_loading",
            25 => "calibrating_motor_noise",
            26 => "paused_ams_lost",
            27 => "paused_low_fan_speed_heat_break",
            28 => "paused_chamber_temperature_control_error",
            29 => "cooling_chamber",
            30 => "paused_user_gcode",
            31 => "motor_noise_showoff",
            32 => "paused_nozzle_filament_covered_detected",
            33 => "paused_cutter_error",
            34 => "paused_first_layer_error",
            35 => "paused_nozzle_clog",
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_stages_observed_during_calibration() {
        // Walked on a real A1 mini during a bed_level + vibration calibration.
        assert_eq!(Stage(1).name(), Some("auto_bed_leveling"));
        assert_eq!(Stage(3).name(), Some("sweeping_xy_mech_mode"));
        assert_eq!(Stage(14).name(), Some("cleaning_nozzle_tip"));
    }

    #[test]
    fn stage_zero_is_the_no_special_stage_default() {
        // 0 is reported both while printing and while idle — it is named, but
        // callers must read it together with gcode_state.
        assert_eq!(Stage(0).name(), Some("printing"));
    }

    #[test]
    fn names_other_spec_stages() {
        assert_eq!(Stage(2).name(), Some("heatbed_preheating"));
        assert_eq!(Stage(7).name(), Some("heating_hotend"));
        assert_eq!(Stage(13).name(), Some("homing_toolhead"));
        assert_eq!(Stage(25).name(), Some("calibrating_motor_noise"));
    }

    #[test]
    fn unknown_and_sentinel_stages_have_no_name_but_keep_their_id() {
        // Negative sentinels and future-firmware stages stay decodable as raw
        // ids; we just don't invent a name for them.
        assert_eq!(Stage(-1).name(), None);
        assert_eq!(Stage(9999).name(), None);
        assert_eq!(Stage(9999).0, 9999);
    }
}
