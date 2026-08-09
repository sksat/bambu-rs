//! Building the print-start command from resolved parameters — the one place the
//! CLI and the serve agree on how a `.3mf`/`.gcode` becomes a `project_file` /
//! `gcode_file`. Pure (no I/O): the caller resolves the on-printer path, parses
//! the AMS map, and (optionally) inspects the plate; this just renders the
//! command, folding in the plate-gcode md5 when an inspection is available so the
//! printer can verify the file it is about to print.

use std::path::Path;

use crate::core::command::{Command, ProjectFile};
use crate::core::project::PlateInspection;

/// Resolved parameters for a print start: the path is already an on-printer path
/// and the AMS map is already parsed. Render the wire command with
/// [`build_command`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintStartParams {
    /// On-printer path, e.g. `/cache/x.gcode.3mf` — becomes `ftp://<path>`.
    pub file: String,
    pub plate: u32,
    pub use_ams: bool,
    pub ams_map: Vec<i32>,
    pub bed_type: String,
    pub timelapse: bool,
}

impl PrintStartParams {
    /// Whether the target is a sliced `.3mf` project (vs a raw `.gcode`).
    pub fn is_3mf(&self) -> bool {
        self.file.to_ascii_lowercase().ends_with(".3mf")
    }
}

/// Expand a per-USED-filament AMS map into the positional array the printer
/// actually consumes.
///
/// The wire `ams_mapping` is keyed by a filament's **position in the PROJECT's
/// filament list**, not by the order this plate happens to use them.
/// `Metadata/plate_N.json`'s `filament_ids` gives that position for each entry of
/// `filament_colors`, so a plate using only the project's 2nd filament needs its
/// tray at index 1.
///
/// **Device-verified failure this prevents:** passing a bare `[2]` for such a
/// plate mapped the *first* filament instead, leaving the used one to fall back
/// to the gcode's baked-in `M620 S<n>A` — the print silently ran from tray 1
/// (PLA) instead of the requested tray 2 (PETG), with `print_error` staying 0.
///
/// Gaps (project filaments this plate never uses) are filled with the first
/// mapped tray rather than `-1`: they should never be consulted, but if they are,
/// landing on a loaded tray beats pulling from the external spool — which is how
/// the wrong-material print happened in the first place.
///
/// Returns `used` unchanged when `filament_ids` is unusable (absent in older
/// 3mfs, or mismatched length) — the caller validates lengths and this stays a
/// no-op rather than inventing a mapping.
///
/// Called from [`build_command`], so every frontend gets the corrected array.
pub fn expand_ams_map(used: &[i32], filament_ids: &[usize]) -> Vec<i32> {
    if used.is_empty() || filament_ids.len() != used.len() {
        return used.to_vec();
    }
    let len = filament_ids.iter().copied().max().unwrap_or(0) + 1;
    let mut out = vec![used[0]; len];
    for (slot, &idx) in filament_ids.iter().enumerate() {
        out[idx] = used[slot];
    }
    out
}

/// Whether `used` can be expanded onto this plate's `filament_ids`.
///
/// The one rule, in one place, because the CLI and the server each had their
/// own and they disagreed. [`expand_ams_map`] is keyed by **`filament_ids`** —
/// each used filament's index in the project — and a map it cannot expand is
/// passed through *unchanged*, so the printer falls back to whatever the gcode
/// baked in and prints the wrong material. That has happened on this hardware.
///
/// The CLI checked the length against `filament_colors` instead. Those are
/// normally the same length, but `filament_ids` is parsed all-or-nothing (a
/// malformed entry would otherwise shift every later index), so a plate with a
/// bad `filament_ids` keeps its colours and loses its ids — passing the check
/// and then not expanding.
///
/// With no ids to expand onto, exactly one tray is still unambiguous: a single
/// filament is index 0 whatever the metadata says. More than one is a guess
/// about ordering, and is refused.
///
/// The message is a clause, so a caller can name the thing it is about:
/// `--ams-map {why}` / `ams_map {why}`.
pub fn ams_map_fits(used: &[i32], filament_ids: &[usize]) -> Result<(), String> {
    // No mapping supplied at all. Expansion is a no-op and the plate's own
    // choice stands, so there is nothing here to be wrong — whether `use_ams`
    // without a mapping makes sense is the caller's question, not this one's.
    // Spelled out rather than folded into the `1` case below, which is about
    // something else entirely.
    if used.is_empty() {
        return Ok(());
    }
    if filament_ids.is_empty() {
        if used.len() == 1 {
            return Ok(());
        }
        return Err(format!(
            "has {} entries, but the plate does not say which filaments it uses, so they cannot \
             be resolved — pass a single tray, or re-slice the plate",
            used.len()
        ));
    }
    if used.len() != filament_ids.len() {
        return Err(format!(
            "has {} entr{} but the plate uses {} filament(s) — one tray per filament, in the \
             plate's own order",
            used.len(),
            if used.len() == 1 { "y" } else { "ies" },
            filament_ids.len()
        ));
    }
    Ok(())
}

/// Render the print-start command: `project_file` for a `.3mf`, `gcode_file` for
/// raw `.gcode`. When `inspection` is present (for a `.3mf`), its plate-gcode md5
/// is stamped into the `project_file` so the printer checks the file matches its
/// bytes before printing; without one the md5 is left empty (the check is
/// skipped), exactly as the builders did before this was shared.
pub fn build_command(params: &PrintStartParams, inspection: Option<&PlateInspection>) -> Command {
    if !params.is_3mf() {
        return Command::GcodeFile(params.file.clone());
    }
    let name = Path::new(&params.file)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&params.file)
        .to_string();
    let mut pf = ProjectFile::new(format!("ftp://{}", params.file), params.plate, name);
    pf.bed_type = params.bed_type.clone();
    pf.timelapse = params.timelapse;
    if params.use_ams {
        pf.use_ams = true;
        // Expand HERE, not in each frontend: the CLI and the dashboard both
        // reach the wire through this builder, and a mapping that is only
        // corrected on one of them is a wrong-material print waiting to happen
        // on the other.
        pf.ams_mapping = match inspection {
            Some(insp) => expand_ams_map(&params.ams_map, &insp.filament_ids),
            None => params.ams_map.clone(),
        };
    }
    if let Some(insp) = inspection {
        pf.md5 = insp.gcode_md5.clone();
    }
    Command::ProjectFile(pf)
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_mapping_is_judged_against_the_ids_it_will_be_expanded_onto() {
        // The pairing that matters: whatever `ams_map_fits` accepts must be
        // something `expand_ams_map` actually expands. Anything it passes
        // through unchanged reaches the printer keyed wrongly.
        for (used, ids) in [
            (vec![0], vec![0]),
            (vec![3, 1], vec![0, 2]),
            (vec![1, 3], vec![2, 0]),
        ] {
            assert!(ams_map_fits(&used, &ids).is_ok(), "{used:?} / {ids:?}");
            // Expanded means each filament's tray lands at that filament's own
            // index — the property the wire array is read by. (For a single
            // filament at index 0 that is the identity, which is why "the
            // output differs from the input" is not the test.)
            let out = expand_ams_map(&used, &ids);
            assert_eq!(
                out.len(),
                ids.iter().max().unwrap() + 1,
                "{used:?} / {ids:?}"
            );
            for (slot, &idx) in ids.iter().enumerate() {
                assert_eq!(out[idx], used[slot], "{used:?} / {ids:?} at {idx}");
            }
        }
    }

    #[test]
    fn a_length_that_cannot_be_expanded_is_refused() {
        // This is the wrong-material bug: `expand_ams_map` returns a
        // mismatched-length map UNCHANGED, so it goes to the printer keyed by
        // nothing and the gcode's baked-in choice wins.
        let err = ams_map_fits(&[0], &[0, 1]).unwrap_err();
        assert!(err.contains("2 filament"), "{err}");
        assert_eq!(
            expand_ams_map(&[0], &[0, 1]),
            vec![0],
            "unexpanded, as feared"
        );
        assert!(ams_map_fits(&[0, 1, 2], &[0, 1]).is_err());
    }

    #[test]
    fn no_mapping_at_all_is_not_a_one_entry_mapping() {
        // `used.is_empty()` means the caller supplied no mapping — expansion is
        // a no-op and the plate's own choice stands. Folding that in with "one
        // tray" (as `<= 1` did) would also let `use_ams` through with an empty
        // wire array, which is a different thing entirely.
        assert!(ams_map_fits(&[], &[]).is_ok());
        assert!(ams_map_fits(&[], &[0, 1]).is_ok());
        assert_eq!(expand_ams_map(&[], &[0, 1]), Vec::<i32>::new());
    }

    #[test]
    fn with_no_ids_one_tray_is_still_unambiguous_and_several_are_not() {
        // `filament_ids` is parsed all-or-nothing, so a plate with a malformed
        // one keeps its colours and loses its ids — which is exactly when the
        // CLI's old check (against `filament_colors`) waved a bad map through.
        assert!(ams_map_fits(&[2], &[]).is_ok(), "one filament is index 0");
        let err = ams_map_fits(&[2, 3], &[]).unwrap_err();
        assert!(err.contains("does not say which filaments"), "{err}");
    }
    use super::*;

    #[test]
    fn ams_map_lands_on_the_projects_filament_index_not_the_used_order() {
        // Plate 1 uses the project's FIRST filament: identity, and the historic
        // single-entry form keeps working.
        assert_eq!(expand_ams_map(&[0], &[0]), vec![0]);
        // Plate 5 uses the project's SECOND filament (PETG). A bare [2] used to
        // go out as-is and map filament 1 — the printer then fell back to the
        // gcode's `M620 S1A` and printed PLA from tray 1. The tray must sit at
        // index 1; index 0 is a gap, filled with a loaded tray (never -1).
        assert_eq!(expand_ams_map(&[2], &[1]), vec![2, 2]);
    }

    #[test]
    fn ams_map_places_each_filament_and_fills_gaps() {
        // Uses project filaments 0 and 2 -> index 1 is a gap.
        assert_eq!(expand_ams_map(&[3, 1], &[0, 2]), vec![3, 3, 1]);
        // Order follows filament_ids, not the argument order.
        assert_eq!(expand_ams_map(&[1, 3], &[2, 0]), vec![3, 1, 1]);
    }

    #[test]
    fn ams_map_is_left_alone_when_filament_ids_are_unusable() {
        // Older 3mf with no `filament_ids`, or a length mismatch: don't invent
        // a mapping — hand back exactly what the caller asked for.
        assert_eq!(expand_ams_map(&[2], &[]), vec![2]);
        assert_eq!(expand_ams_map(&[2], &[0, 1]), vec![2]);
        assert_eq!(expand_ams_map(&[], &[0]), Vec::<i32>::new());
    }

    fn params(file: &str) -> PrintStartParams {
        PrintStartParams {
            file: file.to_string(),
            plate: 1,
            use_ams: false,
            ams_map: vec![],
            bed_type: "auto".to_string(),
            timelapse: false,
        }
    }

    fn inspection(md5: &str) -> PlateInspection {
        PlateInspection {
            plate: 1,
            gcode_md5: md5.to_string(),
            sidecar_md5: None,
            sidecar_matches: true,
            bed_type: None,
            filament_colors: vec![],
            filament_ids: vec![],
            has_timelapse_blocks: false,
        }
    }

    #[test]
    fn three_mf_builds_a_project_file_with_ftp_url() {
        let mut p = params("/cache/coin.gcode.3mf");
        p.plate = 2;
        p.use_ams = true;
        p.ams_map = vec![0, 3];
        p.timelapse = true;
        match build_command(&p, None) {
            Command::ProjectFile(pf) => {
                assert_eq!(pf.url, "ftp:///cache/coin.gcode.3mf");
                assert_eq!(pf.plate, 2);
                assert_eq!(pf.subtask_name, "coin.gcode.3mf");
                assert!(pf.use_ams);
                assert_eq!(pf.ams_mapping, vec![0, 3]);
                assert!(pf.timelapse);
                assert!(pf.md5.is_empty(), "no inspection ⇒ no md5 check");
            }
            other => panic!("expected ProjectFile, got {other:?}"),
        }
    }

    #[test]
    fn the_builder_expands_the_map_so_every_frontend_sends_the_same_wire_array() {
        // The regression that motivated this: a plate using the project's SECOND
        // filament. Expansion used to live in the CLI, so the dashboard still
        // emitted the un-expanded array and printed the wrong material.
        let mut p = params("/x.gcode.3mf");
        p.use_ams = true;
        p.ams_map = vec![2];
        let mut insp = inspection("abc");
        insp.filament_ids = vec![1];
        match build_command(&p, Some(&insp)) {
            Command::ProjectFile(pf) => assert_eq!(pf.ams_mapping, vec![2, 2]),
            other => panic!("expected ProjectFile, got {other:?}"),
        }
    }

    #[test]
    fn without_an_inspection_the_map_goes_out_as_given() {
        // Nothing to expand against — don't invent a mapping.
        let mut p = params("/x.gcode.3mf");
        p.use_ams = true;
        p.ams_map = vec![2];
        match build_command(&p, None) {
            Command::ProjectFile(pf) => assert_eq!(pf.ams_mapping, vec![2]),
            other => panic!("expected ProjectFile, got {other:?}"),
        }
    }

    #[test]
    fn an_inspection_stamps_the_plate_gcode_md5() {
        match build_command(&params("/x.gcode.3mf"), Some(&inspection("abc123"))) {
            Command::ProjectFile(pf) => assert_eq!(pf.md5, "abc123"),
            other => panic!("expected ProjectFile, got {other:?}"),
        }
    }

    #[test]
    fn raw_gcode_builds_a_gcode_file_and_ignores_inspection() {
        // A raw .gcode has no plate metadata, so an inspection can't apply.
        assert!(matches!(
            build_command(&params("/test.gcode"), Some(&inspection("abc"))),
            Command::GcodeFile(f) if f == "/test.gcode"
        ));
    }
}
