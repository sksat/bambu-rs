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

use std::io::{Cursor, Read, Seek};

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
    /// `filament_ids` from `Metadata/plate_N.json`: for each entry of
    /// [`Self::filament_colors`], that filament's **0-based position in the
    /// PROJECT's filament list** — which is the index the wire `ams_mapping` is
    /// keyed by. A plate using only the project's 2nd filament has `[1]`, not
    /// `[0]`. Empty when the sidecar JSON is missing or predates the field.
    pub filament_ids: Vec<usize>,
    /// Whether the plate gcode INJECTS the slicer's per-layer timelapse block (the
    /// head-park + external-shutter moves) at layer changes — the precondition for a
    /// "clean", object-only timelapse. NOTE: even when present the block only RUNS if
    /// timelapse is armed at print start (it's wrapped in an `M622 J1` runtime
    /// conditional), so this is *capability*, not a guarantee the head will park. See
    /// [`injects_timelapse_blocks`] for how it's detected (a gcode scan — the
    /// `timelapse_type` metadata field is uniformly 0 and does NOT track this).
    pub has_timelapse_blocks: bool,
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
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(32), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Inspect one plate of a `.gcode.3mf` given its raw ZIP bytes.
pub fn inspect_plate(zip_bytes: &[u8], plate: u32) -> Result<PlateInspection, ProjectError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes))
        .map_err(|e| ProjectError::InvalidZip(e.to_string()))?;

    let gcode = read_entry(&mut archive, &format!("Metadata/plate_{plate}.gcode"))?
        .ok_or(ProjectError::PlateMissing(plate))?;
    let gcode_md5 = gcode_md5_hex(&gcode);

    // Optional sidecar checksum (cross-check only).
    let sidecar_md5 = match read_entry(&mut archive, &format!("Metadata/plate_{plate}.gcode.md5"))?
    {
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
    let (bed_type, filament_colors, filament_ids) =
        match read_entry(&mut archive, &format!("Metadata/plate_{plate}.json"))? {
            Some(raw) => parse_plate_json(&raw),
            None => (None, Vec::new(), Vec::new()),
        };

    let has_timelapse_blocks = injects_timelapse_blocks(&gcode);

    Ok(PlateInspection {
        plate,
        gcode_md5,
        sidecar_md5,
        sidecar_matches,
        bed_type,
        filament_colors,
        filament_ids,
        has_timelapse_blocks,
    })
}

/// Does the plate gcode INJECT the per-layer timelapse block (head-park + external
/// shutter), vs merely list it in the settings dump or omit it entirely?
///
/// Detected by the slicer's `SKIPTYPE: timelapse` marker. A file that injects the block at
/// each layer change has MANY (one per layer); a file that only echoes the `time_lapse_gcode`
/// template in its machine-settings dump has at most ONE; an older profile without the block
/// has NONE. So `> 1` separates "injected per layer" from "dump-only / absent" — conservative
/// on purpose: a single (dump-only) mention reads as "no", so we never falsely promise the
/// head will park. (`timelapse_type` in the 3mf metadata is uniformly 0 and useless here —
/// device-verified across real slices.)
fn injects_timelapse_blocks(gcode: &[u8]) -> bool {
    const MARKER: &[u8] = b"SKIPTYPE: timelapse";
    gcode
        .windows(MARKER.len())
        .filter(|w| *w == MARKER)
        .take(2) // early-out: two occurrences already proves per-layer injection
        .count()
        > 1
}

/// Does this `.3mf` carry an **authored** project — plate layout, per-object
/// orientation, per-object setting overrides — rather than bare geometry?
///
/// Slicing one of these from system profiles destroys the author's work: the
/// slicer is driven with `--arrange 1 --orient 1`, which re-packs and re-orients
/// every object. Observed cost: a part whose author set `brim_type = brim_ears`
/// per-object got a plain full-width brim instead — 48% more first-layer
/// extrusion, visibly not the designed part. So the server refuses these rather
/// than quietly ruining them.
///
/// Two signals, because authored layout arrives two ways:
///
/// 1. The settings blobs Bambu Studio / OrcaSlicer write into a project and
///    never into a plain geometry export.
/// 2. **More than one build item** in `3D/3dmodel.model`. Placement between
///    objects is authored work whatever tool wrote the file — a CAD assembly
///    exported straight to 3MF carries it in `<build>` and in no settings blob
///    at all — and `--arrange 1` re-packs it. One item is the ordinary export,
///    and arranging that is the point, so it stays bare.
///
/// Takes any seekable reader rather than bytes: a zip is answered from its
/// central directory, so this never needs the archive in memory — and the
/// caller's file can be 512 MiB, with several uploads in flight at once.
pub fn is_authored_project<R: Read + Seek>(zip: R) -> Result<bool, ProjectError> {
    is_authored_project_capped(zip, MAX_ENTRY_BYTES)
}

/// The above with the scan cap injectable, so a test can reach the
/// too-big-to-scan branch without a 256 MiB fixture.
fn is_authored_project_capped<R: Read + Seek>(zip: R, cap: u64) -> Result<bool, ProjectError> {
    let mut archive =
        zip::ZipArchive::new(zip).map_err(|e| ProjectError::InvalidZip(e.to_string()))?;
    if [
        "Metadata/project_settings.config",
        "Metadata/model_settings.config",
    ]
    .iter()
    .any(|name| archive.by_name(name).is_ok())
    {
        return Ok(true);
    }
    // `Unknown` means the build section could not be reached, and answering
    // "bare geometry" there is the answer that re-packs an authored layout. Say
    // authored instead: the cost is refusing an unusually large model, against
    // destroying one.
    Ok(match build_item_count(&mut archive, cap) {
        BuildItems::Count(n) => n > 1,
        BuildItems::Unknown => true,
    })
}

/// What the root model's `<build>` section says, when it can be reached.
#[derive(Debug, PartialEq, Eq)]
enum BuildItems {
    Count(usize),
    /// The entry could not be read to the end within the size cap, so the
    /// build section may never have been reached. In 3MF the `<build>` follows
    /// the mesh resources, so a model with more geometry than the cap puts its
    /// items *past* it — and a truncated prefix reads as zero items, i.e. as
    /// bare geometry, which is the layout-destroying answer.
    Unknown,
}

/// How many `<item>`s the root model's `<build>` lists.
///
/// Counted textually rather than parsed: `<item>` appears nowhere else in a
/// `.model` file (meshes use `<triangle>`/`<vertex>`, assemblies use
/// `<component>`), there is no XML dependency here, and the answer only has to
/// separate "one" from "more than one" — so it stops at two.
///
/// Streamed in chunks with a rolling overlap, never buffered: this reads an
/// attacker-supplied archive entry, and holding it costs as much memory as the
/// decompressed model is big.
fn build_item_count<R: Read + Seek>(archive: &mut zip::ZipArchive<R>, cap: u64) -> BuildItems {
    match archive.by_name("3D/3dmodel.model") {
        Ok(entry) => count_items(entry, cap),
        Err(_) => BuildItems::Count(0),
    }
}

/// The scan itself, over any reader — separate so a test can feed it a reader
/// that returns one byte at a time and actually exercise the chunk overlap. A
/// zip entry's `read` returns whatever size it likes, so a fixture cannot be
/// relied on to straddle a boundary.
fn count_items<R: Read>(mut entry: R, cap: u64) -> BuildItems {
    /// Bytes held back between reads: one less than the longest marker, so a
    /// marker straddling a chunk boundary is seen whole on the next pass. A
    /// match is only acted on when it *starts* inside the decided region —
    /// otherwise it is left for next time, which is what stops it being
    /// half-seen at the edge.
    // Long enough to hold `</` + a namespace prefix + `build`; a marker that
    // straddles a boundary is seen whole next pass.
    const KEEP: usize = 64;
    /// A `<build>` bigger than this is not a file we need to weigh up: it is
    /// far past "more than one item" already.
    const BUILD_MAX: usize = 1 << 20;

    let mut chunk = vec![0u8; 64 * 1024];
    let mut carry: Vec<u8> = Vec::new();
    // `Some` once `<build` has been seen: the section's bytes, gathered whole
    // so items and their transforms are parsed with no chunk boundary running
    // through them. It is the tail of the document, and small.
    let mut build: Option<Vec<u8>> = None;
    let mut total = 0u64;

    loop {
        let n = match entry.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return BuildItems::Unknown,
        };
        total += n as u64;
        if total > cap {
            return BuildItems::Unknown;
        }
        let mut win = std::mem::take(&mut carry);
        win.extend_from_slice(&chunk[..n]);
        let usable = win.len().saturating_sub(KEEP);
        let mut pos = 0;
        while pos < usable {
            match build.as_mut() {
                None => match find_element(&win[pos..], b"build", false) {
                    Some((i, end)) if pos + i < usable => {
                        pos += end;
                        build = Some(Vec::new());
                    }
                    _ => break,
                },
                Some(buf) => match find_element(&win[pos..], b"build", true) {
                    Some((i, _)) if pos + i < usable => {
                        buf.extend_from_slice(&win[pos..pos + i]);
                        return decide_build(buf);
                    }
                    _ => {
                        buf.extend_from_slice(&win[pos..usable]);
                        if buf.len() > BUILD_MAX {
                            return BuildItems::Count(2); // long past deciding
                        }
                        pos = usable;
                    }
                },
            }
        }
        carry = win.split_off(usable);
    }
    // EOF: the carry is the tail nothing has decided yet — and for a short
    // document that can still be the whole `<build>` section, since nothing is
    // acted on until it is `KEEP` bytes from the end.
    let (mut buf, rest) = match build {
        Some(buf) => (buf, &carry[..]),
        None => match find_element(&carry, b"build", false) {
            Some((_, end)) => (Vec::new(), &carry[end..]),
            None => return BuildItems::Count(0),
        },
    };
    match find_element(rest, b"build", true) {
        Some((i, _)) => {
            buf.extend_from_slice(&rest[..i]);
            decide_build(&buf)
        }
        // A `<build>` that never closes is a truncated document; saying "no
        // items" about it is the answer that re-packs a layout.
        None => BuildItems::Unknown,
    }
}

/// What a gathered `<build>` section says: how many items, and whether any of
/// them carries an orientation.
fn decide_build(build: &[u8]) -> BuildItems {
    let mut items = 0usize;
    let mut at = 0usize;
    while let Some((_, end)) = find_element(&build[at..], b"item", false) {
        items += 1;
        at += end;
    }
    if items > 1 || has_oriented_item(&String::from_utf8_lossy(build)) {
        return BuildItems::Count(items.max(2));
    }
    BuildItems::Count(items)
}

/// Does any `transform=` here have a 3×3 block that is not the identity?
///
/// 3MF writes the matrix as twelve numbers, row-major, the last three being the
/// translation — so the first nine are rotation and scale. Anything else there
/// is an orientation someone chose, which `--orient 1` would re-decide.
///
/// A transform that cannot be parsed counts as oriented: this decides whether
/// to refuse, and "cannot tell" must not come out as "go ahead and re-orient".
fn has_oriented_item(text: &str) -> bool {
    text.match_indices("transform=").any(|(i, m)| {
        let rest = &text[i + m.len()..];
        // XML allows either quote character for an attribute value.
        let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            return true;
        };
        let rest = &rest[quote.len_utf8()..];
        let Some(end) = rest.find(quote) else {
            return true;
        };
        let nums: Vec<&str> = rest[..end].split_whitespace().collect();
        if nums.len() < 12 {
            return true;
        }
        const IDENTITY: [f64; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        nums.iter()
            .take(9)
            .zip(IDENTITY)
            .any(|(got, want)| got.parse::<f64>().map_or(true, |v| (v - want).abs() > 1e-6))
    })
}

/// Where an element tag named `name` starts and ends in `hay`, allowing any
/// namespace prefix: `<build`, `<c:build`, `</build`, `</c:build`.
///
/// XML lets a document bind the 3MF core namespace to a prefix and write
/// `<c:build><c:item/></c:build>`, which means exactly what the unprefixed form
/// means. Matching the literal bytes `<build` misses it — and missing it here
/// reads an authored assembly as bare geometry, which is the answer that lets
/// `--arrange 1` re-pack it.
///
/// Returns `(start, end)`: the index of the `<`, and the index just past the
/// element name.
fn find_element(hay: &[u8], name: &[u8], closing: bool) -> Option<(usize, usize)> {
    fn is_name_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
    }
    let mut at = 0;
    while let Some(rel) = hay[at..]
        .windows(name.len())
        .position(|w| w.eq_ignore_ascii_case(name))
    {
        let i = at + rel;
        at = i + 1;
        // An element name ends at whitespace, `/` or `>` — otherwise this is a
        // longer name that merely starts the same way (`<buildplate`).
        match hay.get(i + name.len()) {
            Some(b) if b.is_ascii_whitespace() || matches!(b, b'/' | b'>') => {}
            _ => continue,
        }
        // Walk back over an optional `prefix:`, then the `/` of a closing tag,
        // then require the `<`.
        let mut j = i;
        if j > 0 && hay[j - 1] == b':' {
            j -= 1;
            while j > 0 && is_name_byte(hay[j - 1]) {
                j -= 1;
            }
            if j == 0 {
                continue;
            }
        }
        if closing {
            if j == 0 || hay[j - 1] != b'/' {
                continue;
            }
            j -= 1;
        }
        if j > 0 && hay[j - 1] == b'<' {
            return Some((j - 1, i + name.len()));
        }
    }
    None
}

/// One plate's gcode as text, or `None` when the `.3mf` has no such plate
/// (i.e. it is not sliced). Size-capped like every other entry read.
///
/// Lossy UTF-8: gcode is ASCII, and a stray byte in a comment must not turn a
/// verifiable slice into an error.
pub fn plate_gcode(zip_bytes: &[u8], plate: u32) -> Result<Option<String>, ProjectError> {
    plate_gcode_from(Cursor::new(zip_bytes), plate)
}

/// [`plate_gcode`] over any seekable reader, so a caller holding a file on disk
/// does not have to read the whole archive into memory first: a sliced plate's
/// archive has no size bound of its own, and the entry read below is already
/// capped.
pub fn plate_gcode_from<R: Read + Seek>(
    zip: R,
    plate: u32,
) -> Result<Option<String>, ProjectError> {
    let mut archive =
        zip::ZipArchive::new(zip).map_err(|e| ProjectError::InvalidZip(e.to_string()))?;
    Ok(
        read_entry(&mut archive, &format!("Metadata/plate_{plate}.gcode"))?
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
    )
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
fn read_entry<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
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

/// Extract `bed_type` + `filament_colors` + `filament_ids` from a `plate_N.json`
/// (best-effort).
fn parse_plate_json(raw: &[u8]) -> (Option<String>, Vec<String>, Vec<usize>) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return (None, Vec::new(), Vec::new());
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
    // Parallel to `colors`: each used filament's 0-based index in the project's
    // filament list.
    //
    // ALL-OR-NOTHING on purpose. Skipping a malformed entry would compact the
    // array and shift every later index left — `[null, 1]` would become `[1]`,
    // silently re-pointing the second filament's tray at the first. These
    // indices choose which material feeds, so a plausible-but-shifted array is
    // worse than none: an empty vec makes `expand_ams_map` a no-op and the
    // caller's map goes out as written, which is the pre-existing behaviour for
    // 3mfs that lack the field entirely.
    let ids = v
        .get("filament_ids")
        .and_then(|c| c.as_array())
        .filter(|arr| arr.len() <= 64)
        .and_then(|arr| {
            arr.iter()
                .map(|c| c.as_u64().filter(|n| *n < 64).map(|n| n as usize))
                .collect::<Option<Vec<_>>>()
        })
        .unwrap_or_default();
    (bed_type, colors, ids)
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
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            for (name, bytes) in entries {
                zip.start_file(*name, opts).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn one_object_rotated_on_purpose_is_authored_but_merely_moved_is_not() {
        // `--orient 1` re-decides orientation, so a part someone laid down at
        // an angle loses it. A translation is different: putting one object on
        // the bed is exactly what `--arrange` is for, and refusing every 3mf
        // carrying an offset would refuse the ordinary STL→3MF conversion.
        let with = |t: &str| {
            make_3mf(&[(
                "3D/3dmodel.model",
                format!(r#"<model><build><item objectid="1" transform="{t}"/></build></model>"#)
                    .as_bytes(),
            )])
        };
        let authored = |t: &str| is_authored_project(Cursor::new(&with(t))).unwrap();

        // Identity, and identity with a translation: bare.
        assert!(!authored("1 0 0 0 1 0 0 0 1 0 0 0"));
        assert!(!authored("1 0 0 0 1 0 0 0 1 90 90 0"));
        // Rotated 90° about Z, and scaled: authored.
        assert!(authored("0 -1 0 1 0 0 0 0 1 0 0 0"), "rotation");
        assert!(authored("2 0 0 0 2 0 0 0 2 0 0 0"), "scale");
        // Unparseable or short: cannot tell, so do not re-orient it.
        assert!(authored("1 0 0 0 1 0 0 0"), "too few numbers");
        assert!(authored("1 0 0 0 1 0 0 0 x 0 0 0"), "not a number");
        // And no transform at all is the plainest bare case.
        let plain = make_3mf(&[(
            "3D/3dmodel.model",
            br#"<model><build><item objectid="1"/></build></model>"#,
        )]);
        assert!(!is_authored_project(Cursor::new(&plain)).unwrap());
    }

    #[test]
    fn a_namespace_prefixed_document_is_read_the_same_as_a_plain_one() {
        // `<c:build><c:item/></c:build>` with `c` bound to the 3MF core
        // namespace means exactly what the unprefixed form means. Matching the
        // literal bytes `<build` misses it — and missing it reads an authored
        // assembly as bare geometry, the answer that lets `--arrange` re-pack.
        let prefixed = make_3mf(&[(
            "3D/3dmodel.model",
            br#"<c:model xmlns:c="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
                  <c:build><c:item objectid="1"/><c:item objectid="2"/></c:build></c:model>"#,
        )]);
        assert!(is_authored_project(Cursor::new(&prefixed)).unwrap());

        // One prefixed item, rotated: still authored, via the transform.
        let one = make_3mf(&[(
            "3D/3dmodel.model",
            br#"<c:model><c:build><c:item objectid="1"
                  transform='0 -1 0 1 0 0 0 0 1 0 0 0'/></c:build></c:model>"#,
        )]);
        assert!(is_authored_project(Cursor::new(&one)).unwrap());

        // And one prefixed item merely moved is still bare — single-quoted, to
        // pin that both attribute quote styles are read.
        let moved = make_3mf(&[(
            "3D/3dmodel.model",
            br#"<c:model><c:build><c:item objectid="1"
                  transform='1 0 0 0 1 0 0 0 1 40 40 0'/></c:build></c:model>"#,
        )]);
        assert!(!is_authored_project(Cursor::new(&moved)).unwrap());
    }

    #[test]
    fn an_element_whose_name_merely_starts_the_same_is_not_a_match() {
        // `<buildplate>` is not `<build>`, and `<items>` is not `<item>`.
        assert_eq!(find_element(b"<buildplate>", b"build", false), None);
        assert_eq!(find_element(b"<items/>", b"item", false), None);
        // A closing tag is not an opening one, and vice versa.
        assert_eq!(find_element(b"</build>", b"build", false), None);
        assert_eq!(find_element(b"<build>", b"build", true), None);
        // The plain forms still match, prefix or not.
        assert_eq!(find_element(b"<build>", b"build", false), Some((0, 6)));
        assert_eq!(find_element(b"<c:build>", b"build", false), Some((0, 8)));
        assert_eq!(find_element(b"</c:build>", b"build", true), Some((0, 9)));
    }

    #[test]
    fn an_unterminated_build_section_is_not_read_as_having_no_items() {
        // A truncated document: saying "zero items" about it is the answer that
        // hands an authored layout to `--arrange`.
        let cut = make_3mf(&[(
            "3D/3dmodel.model",
            br#"<model><build><item objectid="1"/><item objectid="2"/>"#,
        )]);
        assert!(is_authored_project(Cursor::new(&cut)).unwrap());
    }

    #[test]
    fn a_model_too_big_to_scan_is_authored_rather_than_assumed_bare() {
        // 3MF puts <build> AFTER the mesh resources, so a model with more
        // geometry than the scan cap has its items past the cutoff. Reading the
        // prefix and calling that "zero items" is the answer that hands an
        // authored layout to `--arrange 1`.
        let big = make_3mf(&[(
            "3D/3dmodel.model",
            format!(
                "<model><resources>{}</resources><build>\
                 <item objectid=\"1\"/><item objectid=\"2\"/></build></model>",
                "<vertex x=\"0\" y=\"0\" z=\"0\"/>".repeat(400)
            )
            .as_bytes(),
        )]);
        let mut archive = zip::ZipArchive::new(Cursor::new(&big)).unwrap();
        // Reachable: both items counted.
        assert_eq!(
            build_item_count(&mut archive, 1 << 20),
            BuildItems::Count(2)
        );
        // Cut short before <build>: unknown, never "zero".
        assert_eq!(build_item_count(&mut archive, 64), BuildItems::Unknown);
        // And unknown resolves to authored, so the layout is not re-packed.
        assert!(is_authored_project_capped(Cursor::new(&big), 64).unwrap());
    }

    #[test]
    fn a_build_item_split_across_read_chunks_is_still_counted() {
        // The scan reads in chunks and carries an overlap between them; without
        // it a `<item` straddling a boundary vanishes and an assembly reads as
        // bare geometry. Driven through a reader that hands over ONE byte per
        // call, so every needle is split — a zip entry's `read` returns
        // whatever size it likes, and a fixture cannot force the case.
        struct Dribble<'a>(&'a [u8]);
        impl std::io::Read for Dribble<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() || out.is_empty() {
                    return Ok(0);
                }
                out[0] = self.0[0];
                self.0 = &self.0[1..];
                Ok(1)
            }
        }
        let xml = br#"<model><build><item objectid="1"/><item objectid="2"/></build></model>"#;
        assert_eq!(count_items(Dribble(xml), 1 << 20), BuildItems::Count(2));
        // One item is still one, however it is chopped up.
        let one = br#"<model><build><item objectid="1"/></build></model>"#;
        assert_eq!(count_items(Dribble(one), 1 << 20), BuildItems::Count(1));
        // A namespace prefix lengthens every marker, so it is the case most
        // likely to straddle a boundary.
        let pfx = br#"<c:model><c:build><c:item objectid="1"/><c:item objectid="2"/></c:build></c:model>"#;
        assert_eq!(count_items(Dribble(pfx), 1 << 20), BuildItems::Count(2));
    }

    #[test]
    fn a_multi_object_3mf_is_authored_even_without_a_slicer_settings_blob() {
        // Placement between objects is authored work whatever wrote the file:
        // the slicer runs with `--arrange 1`, which re-packs them. A 3mf from
        // a CAD tool carries that layout in <build>, not in a Bambu/Orca
        // settings blob, so keying only on those blobs re-arranges an assembly
        // and calls it bare geometry.
        let assembly = make_3mf(&[(
            "3D/3dmodel.model",
            br#"<model><build>
                  <item objectid="1" transform="1 0 0 0 1 0 0 0 1 0 0 0"/>
                  <item objectid="2" transform="1 0 0 0 1 0 0 0 1 40 0 0"/>
                </build></model>"#,
        )]);
        assert!(is_authored_project(Cursor::new(&assembly)).unwrap());

        // One object is the ordinary export, and arranging it is the point.
        let single = make_3mf(&[(
            "3D/3dmodel.model",
            br#"<model><build><item objectid="1"/></build></model>"#,
        )]);
        assert!(!is_authored_project(Cursor::new(&single)).unwrap());
    }

    #[test]
    fn an_authored_project_is_told_apart_from_bare_geometry() {
        let authored = make_3mf(&[
            ("3D/3dmodel.model", b"<model/>"),
            (
                "Metadata/project_settings.config",
                b"{\"layer_height\":\"0.2\"}",
            ),
        ]);
        assert!(is_authored_project(Cursor::new(&authored)).unwrap());

        let per_object = make_3mf(&[
            ("3D/3dmodel.model", b"<model/>"),
            ("Metadata/model_settings.config", b"<config/>"),
        ]);
        assert!(is_authored_project(Cursor::new(&per_object)).unwrap());

        // A plain geometry export — the only kind we may re-arrange and slice.
        let bare = make_3mf(&[("3D/3dmodel.model", b"<model/>")]);
        assert!(!is_authored_project(Cursor::new(&bare)).unwrap());

        assert!(is_authored_project(Cursor::new(b"not a zip")).is_err());
    }

    #[test]
    fn plate_gcode_reads_the_plate_or_says_it_is_unsliced() {
        let sliced = make_3mf(&[("Metadata/plate_1.gcode", b"; layer_height = 0.12\n")]);
        assert_eq!(
            plate_gcode(&sliced, 1).unwrap().as_deref(),
            Some("; layer_height = 0.12\n")
        );
        assert_eq!(plate_gcode(&sliced, 2).unwrap(), None);
        let unsliced = make_3mf(&[("3D/3dmodel.model", b"<model/>")]);
        assert_eq!(plate_gcode(&unsliced, 1).unwrap(), None);
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
                br##"{"bed_type":"textured_plate","filament_colors":["#F2754E","#000000"],
                      "filament_ids":[1,3]}"##,
            ),
        ]);
        let got = inspect_plate(&zip, 1).unwrap();
        assert_eq!(got.gcode_md5, md5);
        assert_eq!(got.sidecar_md5.as_deref(), Some(md5.as_str())); // normalised lowercase
        assert!(got.sidecar_matches);
        assert_eq!(got.bed_type.as_deref(), Some("textured_plate"));
        assert_eq!(got.filament_colors, vec!["#F2754E", "#000000"]);
        // Parsed from the same sidecar as the colours. Asserted here (not just
        // fed to the start tests by hand) so a field-name, JSON-type, or
        // indexing regression can't silently disable the AMS-mapping fix.
        assert_eq!(got.filament_ids, vec![1, 3]);
        assert!(!got.has_timelapse_blocks); // this gcode has no timelapse markers
    }

    #[test]
    fn a_malformed_filament_id_discards_the_whole_array() {
        // Compacting would turn [null, 1] into [1] and point the SECOND
        // filament's tray at the FIRST — a wrong-material print that looks
        // well-formed. Better to have no indices and leave the caller's map
        // as written.
        for bad in [
            br##"{"filament_ids":[null,1]}"##.as_slice(),
            br##"{"filament_ids":[0,-1]}"##.as_slice(),
            br##"{"filament_ids":[0,"1"]}"##.as_slice(),
            br##"{"filament_ids":[0,64]}"##.as_slice(),
        ] {
            let zip = make_3mf(&[
                ("Metadata/plate_1.gcode", b"G28\n"),
                ("Metadata/plate_1.json", bad),
            ]);
            assert!(
                inspect_plate(&zip, 1).unwrap().filament_ids.is_empty(),
                "malformed entry must void the array, not shift it: {}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn detects_per_layer_timelapse_block_injection() {
        // Injected at layer changes (the settings dump + several layers) → many markers.
        let injected = b"; time_lapse_gcode = ;SKIPTYPE: timelapse template\n\
                         G1 Z1\n; SKIPTYPE: timelapse\nM1004 S5 P1\n\
                         G1 Z2\n; SKIPTYPE: timelapse\nM1004 S5 P1\n";
        let zip = make_3mf(&[("Metadata/plate_1.gcode", injected)]);
        assert!(inspect_plate(&zip, 1).unwrap().has_timelapse_blocks);

        // Only the machine-settings dump mentions it once (not injected per layer) → no.
        let dump_only = b"; time_lapse_gcode = ;SKIPTYPE: timelapse template\nG28\nG1 X1\n";
        let zip = make_3mf(&[("Metadata/plate_1.gcode", dump_only)]);
        assert!(!inspect_plate(&zip, 1).unwrap().has_timelapse_blocks);

        // Older profile with no timelapse gcode at all → no.
        let none = b"G28\nG1 X1 Y1\nG1 Z0.2\n";
        let zip = make_3mf(&[("Metadata/plate_1.gcode", none)]);
        assert!(!inspect_plate(&zip, 1).unwrap().has_timelapse_blocks);
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
            (
                "Metadata/plate_1.gcode.md5",
                b"00000000000000000000000000000000",
            ),
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
            filament_ids: vec![],
            has_timelapse_blocks: false,
        };
        // Uppercase + whitespace asserted value still matches.
        assert!(
            verify_expectations(
                &inspection,
                1,
                Some("  F4DC55FD36F79D26ACA4003E36B48D4F "),
                None
            )
            .is_ok()
        );
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
            filament_ids: vec![],
            has_timelapse_blocks: false,
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
