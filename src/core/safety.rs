//! Static safety checks for raw G-code before it is sent.
//!
//! `bambu gcode <line>` lets a caller (an AI agent included) send an arbitrary
//! G-code line. This module is a **guard rail**, not a full parser: it blocks
//! only what it *positively recognises* as unsafe (a clearly-dangerous command,
//! or a temperature setpoint past a ceiling), and allows everything else. A
//! caller can still override a block explicitly (`--force`).
//!
//! The point is to stop an agent from, say, commanding `M104 S999` (thermal
//! runaway) or `M302` (cold extrusion) by accident — not to sandbox a
//! determined operator.

/// Temperature ceilings (°C) a raw G-code line may not exceed.
#[derive(Debug, Clone, Copy)]
pub struct TempLimits {
    pub max_nozzle: f64,
    pub max_bed: f64,
}

impl Default for TempLimits {
    fn default() -> Self {
        // Conservative caps: high enough to clear any real Bambu print, low
        // enough to block absurd values (e.g. `M104 S999`). Tighter per-model
        // limits can be sourced from the capability registry later.
        Self {
            max_nozzle: 300.0,
            max_bed: 100.0,
        }
    }
}

/// The verdict of a static G-code safety check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcodeVerdict {
    /// Nothing recognised as unsafe — fine to send.
    Allow,
    /// Recognised as unsafe; `reason` explains why.
    Block(String),
}

impl GcodeVerdict {
    pub fn is_blocked(&self) -> bool {
        matches!(self, GcodeVerdict::Block(_))
    }
}

/// Statically vet one raw G-code line against `limits`.
pub fn check_gcode(line: &str, limits: &TempLimits) -> GcodeVerdict {
    let code = line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    match code.as_str() {
        // Cold-extrusion enable: lets the extruder try to push unmelted
        // filament, which can grind the gears / strip the drive.
        "M302" => GcodeVerdict::Block("M302 (cold extrusion) is blocked".to_string()),
        // Nozzle temperature setpoints (set / set-and-wait).
        "M104" | "M109" => check_temp(line, limits.max_nozzle, "nozzle"),
        // Bed temperature setpoints (set / set-and-wait).
        "M140" | "M190" => check_temp(line, limits.max_bed, "bed"),
        _ => GcodeVerdict::Allow,
    }
}

/// Block a temperature command whose `S` setpoint exceeds `max`.
fn check_temp(line: &str, max: f64, what: &str) -> GcodeVerdict {
    match parse_s_param(line) {
        Some(t) if t > max => GcodeVerdict::Block(format!(
            "{what} target {t:.0}°C exceeds the {max:.0}°C safety limit (use --force to override)"
        )),
        _ => GcodeVerdict::Allow,
    }
}

/// The numeric `S` parameter of a G-code line, if present (e.g. `M104 S210`).
fn parse_s_param(line: &str) -> Option<f64> {
    line.split_whitespace().find_map(|tok| {
        let rest = tok.strip_prefix(['S', 's'])?;
        rest.parse::<f64>().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> TempLimits {
        TempLimits::default()
    }

    #[test]
    fn ordinary_gcode_is_allowed() {
        assert_eq!(check_gcode("G28", &limits()), GcodeVerdict::Allow);
        assert_eq!(check_gcode("M104 S210", &limits()), GcodeVerdict::Allow);
        assert_eq!(check_gcode("M140 S60", &limits()), GcodeVerdict::Allow);
        // Unknown command: not positively unsafe -> allowed.
        assert_eq!(check_gcode("M9999 X1", &limits()), GcodeVerdict::Allow);
    }

    #[test]
    fn over_limit_nozzle_temp_is_blocked() {
        assert!(check_gcode("M104 S999", &limits()).is_blocked());
        assert!(check_gcode("M109 S350", &limits()).is_blocked());
        // At the limit is allowed; one over is blocked.
        assert_eq!(check_gcode("M104 S300", &limits()), GcodeVerdict::Allow);
        assert!(check_gcode("M104 S301", &limits()).is_blocked());
    }

    #[test]
    fn over_limit_bed_temp_is_blocked() {
        assert!(check_gcode("M140 S150", &limits()).is_blocked());
        assert!(check_gcode("M190 S120", &limits()).is_blocked());
        assert_eq!(check_gcode("M140 S100", &limits()), GcodeVerdict::Allow);
    }

    #[test]
    fn cold_extrusion_is_blocked() {
        assert!(check_gcode("M302", &limits()).is_blocked());
        assert!(check_gcode("M302 P1", &limits()).is_blocked());
        // Case/whitespace tolerant.
        assert!(check_gcode("  m302  ", &limits()).is_blocked());
    }

    #[test]
    fn temp_parsing_is_case_insensitive_and_handles_no_setpoint() {
        assert!(check_gcode("m104 s999", &limits()).is_blocked());
        // M104 with no S (a query / turn-off form) has no setpoint -> allowed.
        assert_eq!(check_gcode("M104", &limits()), GcodeVerdict::Allow);
    }
}
