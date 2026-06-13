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

/// Statically vet a raw G-code payload against `limits`.
///
/// A payload may contain more than one line (a `\n`/`\r` could otherwise hide a
/// second command past the first), so every line is vetted and the first block
/// wins.
pub fn check_gcode(line: &str, limits: &TempLimits) -> GcodeVerdict {
    line.split(['\n', '\r'])
        .map(|l| check_one_line(l, limits))
        .find(GcodeVerdict::is_blocked)
        .unwrap_or(GcodeVerdict::Allow)
}

/// Vet a single G-code line. Tolerant of the forms a real sender might use —
/// line numbers (`N5 …`), trailing comments (`… ; warmup`), no space between a
/// word and its number (`M104S999`), and a space inside a parameter
/// (`M104 S 999`) — so a dangerous command can't slip past on formatting.
fn check_one_line(line: &str, limits: &TempLimits) -> GcodeVerdict {
    // Drop a trailing comment and uppercase for case-insensitive matching.
    let body = line.split(';').next().unwrap_or("").to_ascii_uppercase();
    let Some(code) = command_code(&body) else {
        return GcodeVerdict::Allow;
    };
    let (max, what) = match code.as_str() {
        // Cold-extrusion enable: lets the extruder push unmelted filament, which
        // can grind the gears / strip the drive.
        "M302" => return GcodeVerdict::Block("M302 (cold extrusion) is blocked".to_string()),
        "M104" | "M109" => (limits.max_nozzle, "nozzle"),
        "M140" | "M190" => (limits.max_bed, "bed"),
        _ => return GcodeVerdict::Allow,
    };
    // Both S (target) and R (set-and-wait target) carry the setpoint.
    match max_setpoint(&body) {
        Some(t) if t > max => GcodeVerdict::Block(format!(
            "{what} target {t:.0}°C exceeds the {max:.0}°C safety limit (use --force to override)"
        )),
        _ => GcodeVerdict::Allow,
    }
}

/// The command word (e.g. `M104`) of an uppercased line: the first
/// letter+number word, skipping a leading `N<line-number>`.
fn command_code(body: &str) -> Option<String> {
    let chars: Vec<char> = body.chars().collect();
    let read_word = |start: usize| -> Option<(String, usize)> {
        let mut i = start;
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
        let letter = *chars.get(i)?;
        if !letter.is_ascii_alphabetic() {
            return None;
        }
        let mut j = i + 1;
        while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == '.') {
            j += 1;
        }
        let digits: String = chars[i + 1..j].iter().collect();
        Some((format!("{letter}{digits}"), j))
    };
    let (first, next) = read_word(0)?;
    if first.starts_with('N') {
        // A line-number prefix; the real command is the next word.
        read_word(next).map(|(w, _)| w)
    } else {
        Some(first)
    }
}

/// The largest `S`/`R` numeric setpoint on the line, allowing a space between the
/// letter and its number (`S 999`).
fn max_setpoint(body: &str) -> Option<f64> {
    let chars: Vec<char> = body.chars().collect();
    let mut max: Option<f64> = None;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == 'S' || chars[i] == 'R' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            let start = j;
            while j < chars.len()
                && (chars[j].is_ascii_digit() || matches!(chars[j], '.' | '-' | '+'))
            {
                j += 1;
            }
            if let Ok(v) = chars[start..j].iter().collect::<String>().parse::<f64>() {
                max = Some(max.map_or(v, |m: f64| m.max(v)));
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    max
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

    #[test]
    fn parser_is_not_bypassed_by_formatting_variants() {
        // No space between command and param.
        assert!(check_gcode("M104S999", &limits()).is_blocked());
        // Space *inside* the parameter.
        assert!(check_gcode("M104 S 999", &limits()).is_blocked());
        // R (set-and-wait) setpoint, not just S.
        assert!(check_gcode("M109 R999", &limits()).is_blocked());
        assert!(check_gcode("M190 R150", &limits()).is_blocked());
        // Leading line number.
        assert!(check_gcode("N5 M104 S999", &limits()).is_blocked());
        assert!(check_gcode("N5 M302", &limits()).is_blocked());
        // M302 with a stuck param.
        assert!(check_gcode("M302P1", &limits()).is_blocked());
        // Trailing comment doesn't hide the setpoint.
        assert!(check_gcode("M104 S999 ; warmup", &limits()).is_blocked());
        // A comment that merely mentions a high number is not a setpoint.
        assert_eq!(
            check_gcode("M104 S210 ; was S999", &limits()),
            GcodeVerdict::Allow
        );
    }

    #[test]
    fn embedded_newline_cannot_smuggle_a_second_command() {
        // Only the first line is "safe"; the hidden second line must still block.
        assert!(check_gcode("G28\nM104 S999", &limits()).is_blocked());
        assert!(check_gcode("G28\r\nM302", &limits()).is_blocked());
    }
}
