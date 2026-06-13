//! Pure inspection of a sliced `.gcode.3mf` (a ZIP), and verification of
//! caller-asserted expectations (`--expect-md5` / `--expect-plate`).
//!
//! **No I/O.** The caller reads the file's bytes (over FTPS, from the printer —
//! the source of truth for what will actually print) and hands them in; this
//! module opens the ZIP from memory and returns data or a typed error. Keeping
//! it pure makes the safety-critical logic unit-testable without a network.
//!
//! Layout is from a **real A1 mini** `.gcode.3mf` (`tools/` capture): each plate
//! has `Metadata/plate_N.gcode`, `Metadata/plate_N.gcode.md5` (the md5 hex of the
//! gcode, stored UPPERCASE), and `Metadata/plate_N.json` (`bed_type`,
//! `filament_colors`, …). We **compute** the md5 from the gcode bytes (the
//! authoritative "what will print"); the sidecar is only a cross-check.

use std::io::{Cursor, Read};

use serde::Serialize;

/// What we learned about one plate inside a `.gcode.3mf`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlateInspection {
    /// The plate number inspected.
    pub plate: u32,
    /// md5 of `Metadata/plate_N.gcode`, **lowercase hex** — computed from the
    /// gcode bytes (the value `--expect-md5` is checked against).
    pub gcode_md5: String,
    /// `Metadata/plate_N.gcode.md5` normalised to lowercase hex, or `None` if the
    /// sidecar is absent.
    pub sidecar_md5: Option<String>,
    /// Whether the sidecar matches the computed md5 (vacuously `true` with no
    /// sidecar). A `false` here means the file's own checksum disagrees with its
    /// bytes — surfaced as a warning, never trusted over the computed value.
    pub sidecar_matches: bool,
    /// `bed_type` from `Metadata/plate_N.json`, if present (e.g. `textured_plate`).
    pub bed_type: Option<String>,
    /// `filament_colors` from `Metadata/plate_N.json` (hex `#RRGGBB`), in order.
    pub filament_colors: Vec<String>,
}

/// A problem reading/parsing the `.3mf`.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("not a valid .3mf (zip): {0}")]
    InvalidZip(String),
    #[error("plate {0} is not in the .3mf (no Metadata/plate_{0}.gcode)")]
    PlateMissing(u32),
    #[error("sidecar Metadata/plate_{0}.gcode.md5 is not valid md5 hex")]
    InvalidSidecarMd5(u32),
    #[error(".3mf entry exceeds the {limit_mb} MB inspection cap")]
    TooLarge { limit_mb: u64 },
}

/// A caller-asserted expectation that the actual file did not meet.
/// Separate from [`ProjectError`] so the CLI can give distinct, agent-parseable
/// messages while mapping both to the validation exit code.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExpectError {
    #[error("expected md5 {expected} but plate {plate}'s gcode is {actual}")]
    Md5Mismatch {
        plate: u32,
        expected: String,
        actual: String,
    },
    #[error("expected plate {expected} but --plate is {requested}")]
    PlateMismatch { expected: u32, requested: u32 },
}

/// Max bytes read from any single ZIP entry during inspection (zip-bomb guard).
const MAX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

/// Lowercase-hex md5 of `bytes`.
pub fn gcode_md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    use std::fmt::Write;
    let mut hasher = Md5::new();
    hasher.update(bytes);
    // RustCrypto's digest output doesn't impl LowerHex, so hex-encode by hand.
    hasher.finalize().iter().fold(String::with_capacity(32), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Inspect one plate of a `.gcode.3mf` given its raw ZIP bytes.
pub fn inspect_plate(zip_bytes: &[u8], plate: u32) -> Result<PlateInspection, ProjectError> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(zip_bytes)).map_err(|e| ProjectError::InvalidZip(e.to_string()))?;

    let gcode = read_entry(&mut archive, &format!("Metadata/plate_{plate}.gcode"))?
        .ok_or(ProjectError::PlateMissing(plate))?;
    let gcode_md5 = gcode_md5_hex(&gcode);

    // Optional sidecar checksum (cross-check only).
    let sidecar_md5 = match read_entry(&mut archive, &format!("Metadata/plate_{plate}.gcode.md5"))? {
        Some(raw) => {
            let token = String::from_utf8_lossy(&raw)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if token.len() != 32 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(ProjectError::InvalidSidecarMd5(plate));
            }
            Some(token)
        }
        None => None,
    };
    let sidecar_matches = sidecar_md5.as_deref().is_none_or(|s| s == gcode_md5);

    // Best-effort plate metadata (never fatal).
    let (bed_type, filament_colors) =
        match read_entry(&mut archive, &format!("Metadata/plate_{plate}.json"))? {
            Some(raw) => parse_plate_json(&raw),
            None => (None, Vec::new()),
        };

    Ok(PlateInspection {
        plate,
        gcode_md5,
        sidecar_md5,
        sidecar_matches,
        bed_type,
        filament_colors,
    })
}

/// Verify caller-asserted expectations against an inspected plate.
/// `expect_plate`, when given, must equal the `--plate` actually requested.
pub fn verify_expectations(
    inspection: &PlateInspection,
    requested_plate: u32,
    expect_md5: Option<&str>,
    expect_plate: Option<u32>,
) -> Result<(), ExpectError> {
    // Defensive: the inspection must be of the plate we're verifying. The CLI
    // always inspects `requested_plate`, but enforcing the invariant here keeps a
    // future caller from accidentally verifying md5 against the wrong plate.
    if inspection.plate != requested_plate {
        return Err(ExpectError::PlateMismatch {
            expected: inspection.plate,
            requested: requested_plate,
        });
    }
    if let Some(want) = expect_plate
        && want != requested_plate
    {
        return Err(ExpectError::PlateMismatch {
            expected: want,
            requested: requested_plate,
        });
    }
    if let Some(want) = expect_md5 {
        let want = want.trim().to_ascii_lowercase();
        if want != inspection.gcode_md5 {
            return Err(ExpectError::Md5Mismatch {
                plate: inspection.plate,
                expected: want,
                actual: inspection.gcode_md5.clone(),
            });
        }
    }
    Ok(())
}

/// Read a ZIP entry fully (capped), `Ok(None)` if the entry isn't present.
fn read_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> Result<Option<Vec<u8>>, ProjectError> {
    let file = match archive.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(ProjectError::InvalidZip(e.to_string())),
    };
    if file.size() > MAX_ENTRY_BYTES {
        return Err(ProjectError::TooLarge {
            limit_mb: MAX_ENTRY_BYTES / (1024 * 1024),
        });
    }
    let mut buf = Vec::new();
    file.take(MAX_ENTRY_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| ProjectError::InvalidZip(format!("reading {name}: {e}")))?;
    Ok(Some(buf))
}

/// Extract `bed_type` + `filament_colors` from a `plate_N.json` (best-effort).
fn parse_plate_json(raw: &[u8]) -> (Option<String>, Vec<String>) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return (None, Vec::new());
    };
    let bed_type = v
        .get("bed_type")
        .and_then(|b| b.as_str())
        .map(str::to_owned);
    // Bound the colours defensively (a malicious .3mf could pack a huge array);
    // a real plate has a handful. Cap count and per-entry length.
    let colors = v
        .get("filament_colors")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.as_str())
                .filter(|s| s.len() <= 32)
                .take(64)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    (bed_type, colors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// Build a `.3mf` (ZIP) in memory from (name, bytes) entries — deflate, so it
    /// exercises the same path real Bambu files use. No committed binary fixture.
    fn make_3mf(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            for (name, bytes) in entries {
                zip.start_file(*name, opts).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn md5_is_lowercase_hex_of_the_bytes() {
        // Known vectors.
        assert_eq!(gcode_md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(gcode_md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn inspects_a_plate_with_sidecar_and_json() {
        let gcode = b"G28\nG1 X1 Y1\n";
        let md5 = gcode_md5_hex(gcode);
        let zip = make_3mf(&[
            ("Metadata/plate_1.gcode", gcode),
            // sidecar stored UPPERCASE, like the real device.
            ("Metadata/plate_1.gcode.md5", md5.to_uppercase().as_bytes()),
            (
                "Metadata/plate_1.json",
                br##"{"bed_type":"textured_plate","filament_colors":["#F2754E","#000000"]}"##,
            ),
        ]);
        let got = inspect_plate(&zip, 1).unwrap();
        assert_eq!(got.gcode_md5, md5);
        assert_eq!(got.sidecar_md5.as_deref(), Some(md5.as_str())); // normalised lowercase
        assert!(got.sidecar_matches);
        assert_eq!(got.bed_type.as_deref(), Some("textured_plate"));
        assert_eq!(got.filament_colors, vec!["#F2754E", "#000000"]);
    }

    #[test]
    fn missing_plate_is_an_error() {
        let zip = make_3mf(&[("Metadata/plate_1.gcode", b"G28")]);
        assert!(matches!(
            inspect_plate(&zip, 2),
            Err(ProjectError::PlateMissing(2))
        ));
    }

    #[test]
    fn absent_sidecar_and_json_are_not_fatal() {
        let zip = make_3mf(&[("Metadata/plate_1.gcode", b"G28")]);
        let got = inspect_plate(&zip, 1).unwrap();
        assert_eq!(got.sidecar_md5, None);
        assert!(got.sidecar_matches); // vacuously
        assert_eq!(got.bed_type, None);
        assert!(got.filament_colors.is_empty());
    }

    #[test]
    fn mismatched_sidecar_is_flagged_not_fatal_and_computed_value_wins() {
        let zip = make_3mf(&[
            ("Metadata/plate_1.gcode", b"G28"),
            ("Metadata/plate_1.gcode.md5", b"00000000000000000000000000000000"),
        ]);
        let got = inspect_plate(&zip, 1).unwrap();
        assert!(!got.sidecar_matches);
        assert_eq!(got.gcode_md5, gcode_md5_hex(b"G28")); // computed, not the sidecar
    }

    #[test]
    fn malformed_sidecar_is_rejected() {
        let zip = make_3mf(&[
            ("Metadata/plate_1.gcode", b"G28"),
            ("Metadata/plate_1.gcode.md5", b"not-a-real-md5"),
        ]);
        assert!(matches!(
            inspect_plate(&zip, 1),
            Err(ProjectError::InvalidSidecarMd5(1))
        ));
    }

    #[test]
    fn not_a_zip_is_an_error() {
        assert!(matches!(
            inspect_plate(b"this is not a zip", 1),
            Err(ProjectError::InvalidZip(_))
        ));
    }

    #[test]
    fn verify_expectations_matches_case_insensitively() {
        let inspection = PlateInspection {
            plate: 1,
            gcode_md5: "f4dc55fd36f79d26aca4003e36b48d4f".to_string(),
            sidecar_md5: None,
            sidecar_matches: true,
            bed_type: None,
            filament_colors: vec![],
        };
        // Uppercase + whitespace asserted value still matches.
        assert!(verify_expectations(&inspection, 1, Some("  F4DC55FD36F79D26ACA4003E36B48D4F "), None).is_ok());
        // Wrong md5.
        assert!(matches!(
            verify_expectations(&inspection, 1, Some("deadbeef"), None),
            Err(ExpectError::Md5Mismatch { .. })
        ));
        // expect-plate must equal the requested plate.
        assert!(verify_expectations(&inspection, 1, None, Some(1)).is_ok());
        assert_eq!(
            verify_expectations(&inspection, 1, None, Some(2)),
            Err(ExpectError::PlateMismatch {
                expected: 2,
                requested: 1
            })
        );
        // No expectations -> ok.
        assert!(verify_expectations(&inspection, 1, None, None).is_ok());
    }

    #[test]
    fn verify_expectations_rejects_an_inspection_of_the_wrong_plate() {
        // Defensive invariant: inspecting plate 1 but verifying for plate 2 must
        // fail even with no caller expectations (guards against a future bug).
        let inspection = PlateInspection {
            plate: 1,
            gcode_md5: "f4dc55fd36f79d26aca4003e36b48d4f".to_string(),
            sidecar_md5: None,
            sidecar_matches: true,
            bed_type: None,
            filament_colors: vec![],
        };
        assert_eq!(
            verify_expectations(&inspection, 2, None, None),
            Err(ExpectError::PlateMismatch {
                expected: 1,
                requested: 2
            })
        );
    }
}
