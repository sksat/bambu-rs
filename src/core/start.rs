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
