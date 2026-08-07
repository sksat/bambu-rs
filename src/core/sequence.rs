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

use crate::core::safety::{self, GcodeVerdict, TempLimits};

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
                Step { line_no: 3, gcode: "G90".into() },
                Step { line_no: 4, gcode: "G28".into() },
                Step { line_no: 6, gcode: "G0 Z30 F5000".into() },
                Step { line_no: 8, gcode: "G0 X-10".into() },
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
        let steps = parse(
            "G90\nG28\nG0 Z30 F5000\nG0 X-10\nG0 Y-6 F2000\nG4 S3\nG0 Y186.5\nG0 Z186\n",
        );
        assert!(vet(&steps, &TempLimits::default()).is_empty());
    }
}
