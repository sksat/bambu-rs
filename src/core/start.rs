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
        pf.ams_mapping = params.ams_map.clone();
    }
    if let Some(insp) = inspection {
        pf.md5 = insp.gcode_md5.clone();
    }
    Command::ProjectFile(pf)
}

#[cfg(test)]
mod tests {
    use super::*;

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
