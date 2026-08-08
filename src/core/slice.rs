//! Pure slicing logic: profile flattening, slicer argv, and post-slice
//! verification. **No I/O** — profiles arrive through the [`ProfileFetch`] seam
//! and the produced gcode arrives as a `&str`, so every trap below is unit-tested
//! without a slicer installed.
//!
//! ## Why this module exists at all
//!
//! The slicer CLI (OrcaSlicer / Bambu Studio) does **not** resolve a profile's
//! `inherits` chain. Bambu's system profiles are *diffs*: `0.12mm Fine @BBL
//! A1M.json` does not contain `layer_height` at all — it lives in a grandparent.
//! `--load-settings <leaf>.json` therefore loads a handful of keys and lets
//! everything else fall back to the slicer's built-in defaults (layer_height →
//! 0.2mm, plus default speeds and temps). The output *looks* right and even
//! "works" at 0.2mm by coincidence. [`flatten`] resolves the chain;
//! [`verify_sliced`] proves the result against the request rather than trusting
//! the profile name.
//!
//! ## The second trap, which cost a failed print
//!
//! On Bambu Studio the A1 mini's long machine gcode blocks (`machine_start_gcode`
//! and friends) are **not on the inherits chain at all** — the chain carries only
//! a *generic fallback*. The real blocks sit in sibling `"<machine> template
//! <key>"` profiles that the GUI merges and a plain inherits-walk misses. The
//! generic start's prime line is `G1 X10.1 Y200.0 … E15 ;Draw the first line`,
//! and Y200 is 20mm past the A1 mini's 180mm bed: the head slams the Y limit and
//! the print never primes. [`flatten`] merges those siblings for
//! [`ProfileKind::Machine`], and [`verify_sliced`] fails loudly if the produced
//! start section extrudes off the bed anyway.

use std::collections::HashSet;
use std::path::Path;

use serde_json::{Map, Value};

/// A parsed slicer profile: the JSON object as-is.
pub type ProfileMap = Map<String, Value>;

/// Which of the slicer's three profile directories a name lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProfileKind {
    Machine,
    Process,
    Filament,
}

impl ProfileKind {
    pub fn subdir(&self) -> &'static str {
        match self {
            ProfileKind::Machine => "machine",
            ProfileKind::Process => "process",
            ProfileKind::Filament => "filament",
        }
    }
}

/// Looks a profile up by name — the seam that keeps [`flatten`] I/O-free.
///
/// `Ok(None)` means "no such profile" (a hard error to the caller, never a
/// default); `Err` means the profile exists but could not be read or parsed.
pub trait ProfileFetch {
    fn fetch(&self, kind: ProfileKind, name: &str) -> Result<Option<ProfileMap>, String>;
}

/// The long machine gcode blocks Bambu Studio keeps in sibling `"<machine>
/// template <key>"` profiles, off the inherits chain. See the module docs — miss
/// these and the slicer emits a generic start that drives off the bed.
pub const TEMPLATE_GCODE_KEYS: [&str; 5] = [
    "machine_start_gcode",
    "machine_end_gcode",
    "layer_change_gcode",
    "change_filament_gcode",
    "time_lapse_gcode",
];

/// Something went wrong resolving a profile chain.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SliceError {
    /// A named profile (leaf or ancestor) is not there. Deliberately fatal: a
    /// missing ancestor is exactly the silent-default trap this module exists to
    /// prevent, so there is no fallback.
    #[error("profile not found: {kind}/{name}")]
    ProfileNotFound { kind: &'static str, name: String },
    #[error("profile {0:?} is part of an `inherits` cycle")]
    InheritsCycle(String),
    #[error("unsafe profile name {0:?}")]
    UnsafeName(String),
    #[error("reading profile {name:?}: {error}")]
    Fetch { name: String, error: String },
}

/// Longest allowed profile name. Generous next to the ~30-character real BBL
/// names because [`flatten`] derives `"{leaf} template {key}"` from a machine
/// name and that sibling has to fit here too.
const MAX_PROFILE_NAME: usize = 160;

/// Is `s` usable as a profile name, i.e. safe to turn into `<root>/<kind>/<s>.json`?
///
/// Profile names reach us from HTTP query parameters, so this is a security
/// boundary, not a tidiness check: `../../../../etc/passwd` must not resolve. The
/// allowed set is what real BBL names need — letters, digits, space, and
/// `. - _ @ ( ) +` (e.g. `Bambu PLA Silk+ @BBL A1M`) — which excludes both
/// separators outright.
pub fn is_safe_profile_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_PROFILE_NAME
        && !s.starts_with('.')
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || " .-_@()+".contains(c))
}

/// Resolve a profile's whole `inherits` chain into one self-contained object.
///
/// Root-first merge so a child overrides its parents; `inherits` is blanked
/// afterwards (the chain is resolved — the slicer must not re-chase it) and
/// `instantiation` dropped. For [`ProfileKind::Machine`] the sibling template
/// gcodes are merged on top (module docs). `overrides` win over everything.
pub fn flatten(
    kind: ProfileKind,
    leaf: &str,
    fetch: &dyn ProfileFetch,
    overrides: &[(&str, Value)],
) -> Result<ProfileMap, SliceError> {
    let mut chain: Vec<ProfileMap> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut name = leaf.to_string();
    while !name.is_empty() {
        if !is_safe_profile_name(&name) {
            return Err(SliceError::UnsafeName(name));
        }
        // A profile that inherits (transitively) from itself would otherwise
        // loop forever building the chain.
        if !seen.insert(name.clone()) {
            return Err(SliceError::InheritsCycle(name));
        }
        let profile = fetch
            .fetch(kind, &name)
            .map_err(|error| SliceError::Fetch {
                name: name.clone(),
                error,
            })?
            .ok_or_else(|| SliceError::ProfileNotFound {
                kind: kind.subdir(),
                name: name.clone(),
            })?;
        let next = profile
            .get("inherits")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        chain.push(profile);
        name = next;
    }

    let mut merged = ProfileMap::new();
    for profile in chain.into_iter().rev() {
        for (k, v) in profile {
            merged.insert(k, v);
        }
    }
    merged.insert("inherits".to_string(), Value::String(String::new()));
    merged.remove("instantiation");

    if kind == ProfileKind::Machine {
        for key in TEMPLATE_GCODE_KEYS {
            let template = format!("{leaf} template {key}");
            // A machine name long enough to push its own sibling over the limit
            // is a loud error, never a quiet skip: skipping is precisely how the
            // generic off-bed start gcode gets emitted.
            if !is_safe_profile_name(&template) {
                return Err(SliceError::UnsafeName(template));
            }
            // Absent is normal (OrcaSlicer keeps these on the chain); only a read
            // failure is worth reporting.
            let found = fetch
                .fetch(ProfileKind::Machine, &template)
                .map_err(|error| SliceError::Fetch {
                    name: template.clone(),
                    error,
                })?;
            if let Some(t) = found
                && let Some(v) = t.get(key)
            {
                merged.insert(key.to_string(), v.clone());
            }
        }
    }

    for (k, v) in overrides {
        merged.insert((*k).to_string(), v.clone());
    }
    Ok(merged)
}

/// Which process profile to load for a requested layer height.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessChoice {
    pub name: String,
    /// The request is not one of the stock heights, so the nearest profile was
    /// picked and `layer_height` must be forced on top of it.
    pub force_layer: bool,
}

/// The stock A1-mini process profiles, by layer height.
const PROCESS_TABLE: [(f64, &str); 6] = [
    (0.08, "0.08mm Extra Fine"),
    (0.12, "0.12mm Fine"),
    (0.16, "0.16mm Optimal"),
    (0.20, "0.20mm Standard"),
    (0.24, "0.24mm Draft"),
    (0.28, "0.28mm Extra Draft"),
];

/// Keys forced onto a flattened profile, each paired with its value.
pub type ProfileOverrides = Vec<(&'static str, serde_json::Value)>;

/// What a request asks the process profile to override, and the process
/// profile it applies to.
///
/// Pure, and here rather than beside the `Command` that spawns the slicer,
/// because the load-bearing part is a *decision*: every value is written as a
/// **string**. A bare JSON number is silently dropped back to the profile's
/// default (observed), which is the same class of quiet-wrong-output as the
/// unresolved `inherits` chain — and it cannot be caught by looking at the
/// produced gcode, since a dropped override just means the default was used.
/// Building the overrides in the I/O layer put that trap where no test could
/// reach it: the fake slicer ignores them, so CI never ran the real ones.
pub fn overrides_for(
    layer_mm: f64,
    preset_suffix: &str,
    bed_type: &str,
    brim_mm: Option<f64>,
) -> Result<(ProcessChoice, ProfileOverrides), String> {
    let choice = process_for_layer(layer_mm, preset_suffix)?;
    let mut overrides: ProfileOverrides = vec![("curr_bed_type", serde_json::json!(bed_type))];
    if choice.force_layer {
        overrides.push(("layer_height", serde_json::json!(fmt_mm(layer_mm))));
    }
    if let Some(brim) = brim_mm {
        overrides.push(("brim_type", serde_json::json!("outer_only")));
        overrides.push(("brim_width", serde_json::json!(fmt_mm(brim))));
    }
    Ok((choice, overrides))
}

/// A millimetre value as the slicer wants it: a string.
pub fn fmt_mm(v: f64) -> String {
    format!("{v}")
}

/// How far a requested height may sit from a stock profile before the pick
/// stops being a rounding and starts being a guess.
///
/// The stock heights are 0.04 apart, so half of that is the point where a
/// neighbour is no closer. Beyond it the profile's speeds, flow and cooling
/// were tuned for a height the print will not use — a guess, and this module
/// refuses those elsewhere.
const PROCESS_MAX_GAP_MM: f64 = 0.02;

/// Pick the process profile for `layer_mm`, named for the given preset suffix
/// (e.g. `@BBL A1M`). A non-stock height picks the nearest profile and asks the
/// caller to force `layer_height` on top of it; one too far from any stock
/// height is an error rather than a silent substitution.
pub fn process_for_layer(layer_mm: f64, preset_suffix: &str) -> Result<ProcessChoice, String> {
    let (best_h, best_name) = PROCESS_TABLE
        .iter()
        .min_by(|a, b| (a.0 - layer_mm).abs().total_cmp(&(b.0 - layer_mm).abs()))
        .expect("the process table is not empty");
    let gap = (best_h - layer_mm).abs();
    if gap > PROCESS_MAX_GAP_MM {
        let stock = PROCESS_TABLE
            .iter()
            .map(|(h, _)| format!("{h}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "layer={layer_mm} has no process profile near it (stock heights: {stock}); the \
             nearest is tuned for {best_h} mm and its speeds and flow would not be"
        ));
    }
    Ok(ProcessChoice {
        name: format!("{best_name} {preset_suffix}"),
        force_layer: gap > 1e-9,
    })
}

/// The slicer CLI arguments (after the program name) for one slice.
///
/// `out_name` is a **bare filename**: combined with `--outputdir` the slicer
/// concatenates the two, so a path here doubles up and the export fails
/// (observed).
pub fn slicer_args(
    machine_json: &Path,
    process_json: &Path,
    filament_json: &Path,
    out_dir: &Path,
    out_name: &str,
    input: &Path,
) -> Vec<String> {
    vec![
        "--load-settings".to_string(),
        format!("{};{}", machine_json.display(), process_json.display()),
        "--load-filaments".to_string(),
        filament_json.display().to_string(),
        "--arrange".to_string(),
        "1".to_string(),
        "--orient".to_string(),
        "1".to_string(),
        "--slice".to_string(),
        "0".to_string(),
        "--outputdir".to_string(),
        out_dir.display().to_string(),
        "--export-3mf".to_string(),
        out_name.to_string(),
        input.display().to_string(),
    ]
}

/// Wrap a full argv in `xvfb-run -a` when a virtual display is available.
///
/// Without one the slice still succeeds — the slicer just logs `init opengl
/// failed! skip thumbnail generating` and the 3mf ships with no preview PNG.
pub fn wrap_xvfb(argv: Vec<String>, use_xvfb: bool) -> Vec<String> {
    if !use_xvfb {
        return argv;
    }
    let mut wrapped = vec!["xvfb-run".to_string(), "-a".to_string()];
    wrapped.extend(argv);
    wrapped
}

/// The bed's maximum X and Y from a flattened machine profile's
/// `printable_area` polygon (`["0x0", "180x0", …]`). `None` when the profile
/// carries no usable polygon — the caller then skips the off-bed check rather
/// than inventing a bed size.
pub fn bed_bounds(machine: &ProfileMap) -> Option<(f64, f64)> {
    let points = machine.get("printable_area")?.as_array()?;
    let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for p in points {
        let Some((x, y)) = p.as_str().and_then(|s| s.split_once('x')) else {
            continue;
        };
        let (Ok(x), Ok(y)) = (x.trim().parse::<f64>(), y.trim().parse::<f64>()) else {
            continue;
        };
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    (max_x > 0.0 && max_y > 0.0).then_some((max_x, max_y))
}

/// What a verified slice turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub struct SliceMeta {
    pub layer_height_mm: f64,
    pub layers: Option<u32>,
    pub bed_temp_c: Option<u32>,
}

/// A produced slice that must not be printed.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum VerifyFault {
    #[error("requested {want}mm but the sliced gcode is {got}mm")]
    LayerHeightMismatch { want: f64, got: f64 },
    #[error("no `; layer_height =` in the plate gcode — this is not a sliced plate")]
    NoLayerHeight,
    #[error(
        "generic fallback start gcode ('Draw the first line') — the machine template gcodes were \
         not merged"
    )]
    GenericStartGcode,
    #[error("start gcode extrudes off the bed: {line:?} — the real machine_start_gcode is missing")]
    OffBedExtrusion { line: String },
}

/// Verify a produced plate gcode against what was asked for.
///
/// Two independent checks, both of which have caught a real bad slice:
/// the layer height actually in the gcode (the inherits trap), and the start
/// section not extruding off the bed (the template-gcode trap). `bed` is the
/// machine profile's own bounds; `None` skips the second check rather than
/// guessing a bed size.
pub fn verify_sliced(
    gcode: &str,
    want_layer_mm: f64,
    bed: Option<(f64, f64)>,
) -> Result<SliceMeta, VerifyFault> {
    let got = gcode
        .lines()
        .find_map(|l| l.strip_prefix("; layer_height = "))
        // `NaN` and `inf` parse as f64, and every comparison with NaN is false
        // — a slicer emitting `; layer_height = NaN` would satisfy the check
        // below for any requested height.
        .and_then(|v| v.trim().parse::<f64>().ok().filter(|f| f.is_finite()))
        .ok_or(VerifyFault::NoLayerHeight)?;
    if (got - want_layer_mm).abs() > 1e-6 {
        return Err(VerifyFault::LayerHeightMismatch {
            want: want_layer_mm,
            got,
        });
    }
    if let Some(fault) = start_gcode_fault(gcode, bed) {
        return Err(fault);
    }
    let layers = gcode
        .split_once("total layer number: ")
        .and_then(|(_, rest)| leading_u32(rest));
    let bed_temp_c = gcode
        .lines()
        .find_map(|l| l.strip_prefix("M190 S"))
        .and_then(leading_u32);
    Ok(SliceMeta {
        layer_height_mm: got,
        layers,
        bed_temp_c,
    })
}

/// The generic-start-gcode detector, ported from the slicing skill's helper.
///
/// The real A1 mini start *does* move outside the bed (a Y185 wipe) — but with no
/// extrusion. So "an **extruding** move outside the printable area" is a clean
/// signal, and both bounds matter: a negative prime (`G1 X-5 E1`) is as much a
/// crash as one past Y200. 1mm of slack absorbs the profile's own edge-of-bed
/// rounding.
fn start_gcode_fault(gcode: &str, bed: Option<(f64, f64)>) -> Option<VerifyFault> {
    let start = gcode.split("; CHANGE_LAYER").next().unwrap_or(gcode);
    if start.contains("Draw the first line") {
        return Some(VerifyFault::GenericStartGcode);
    }
    let (max_x, max_y) = bed?;
    for line in start.lines() {
        let line = line.trim();
        if !(line.starts_with("G0 ") || line.starts_with("G1 ")) {
            continue;
        }
        // Only extruding moves matter: the real start wipes past the bed edge
        // without extruding, and that is fine.
        if gcode_param(line, 'E').filter(|e| *e > 0.0).is_none() {
            continue;
        }
        // Only the coordinates NAMED ON THIS LINE. Carrying the head's position
        // across lines looks stricter and is wrong here: the real A1 mini start
        // parks off the bed on purpose and purges there with bare `G1 E50 F200`
        // lines, which a position-carrying check reports as extruding off the
        // bed. Device-verified — the change broke a slice that had just been
        // proven good.
        //
        // So this detects a move that *claims* an off-bed coordinate while
        // extruding, which is exactly the shape of the generic start's prime
        // (`G1 X10.1 Y200.0 E15`). It is a detector for that known-bad output,
        // not a general proof that nothing extrudes off the bed.
        let off = |v: Option<f64>, max: f64| v.is_some_and(|v| !(-1.0..=max + 1.0).contains(&v));
        if off(gcode_param(line, 'X'), max_x) || off(gcode_param(line, 'Y'), max_y) {
            return Some(VerifyFault::OffBedExtrusion {
                line: line.chars().take(60).collect(),
            });
        }
    }
    None
}

/// The numeric argument of a gcode word (`X10.1` → `10.1`), or `None`.
///
/// The comment is cut off first: `;Draw the first line` contains words that would
/// otherwise parse as parameters.
fn gcode_param(line: &str, letter: char) -> Option<f64> {
    line.split(';')
        .next()
        .unwrap_or("")
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix(letter)?.parse::<f64>().ok())
}

/// The leading run of digits in `s`, parsed.
fn leading_u32(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The BBL profile names that slice for one printer model. Lives here rather
/// than in the capability registry so the profile-name knowledge stays with the
/// slicing logic; the registry just holds the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlicerNames {
    /// e.g. `Bambu Lab A1 mini` — a nozzle size makes it a usable machine
    /// preset. The bare name on its own is only a model descriptor.
    pub machine_base: &'static str,
    /// e.g. `@BBL A1M` — the process/filament preset suffix.
    pub preset_suffix: &'static str,
}

impl SlicerNames {
    /// The full machine preset name for an installed nozzle, e.g.
    /// `Bambu Lab A1 mini 0.4 nozzle`.
    pub fn machine_profile(&self, nozzle: &str) -> String {
        format!("{} {} nozzle", self.machine_base, nozzle.trim())
    }
}

/// Whether a layer height is printable with the fitted nozzle.
///
/// Derived, not a fixed window: the usable range is a fraction of the nozzle
/// diameter, so a constant 0.04–0.4 accepts 0.4 mm on a 0.2 nozzle (which
/// cannot lay it down) and refuses 0.5 mm on a 0.8 (which can) — while naming a
/// nozzle the machine may not have. The bounds themselves are the slicer's own:
/// Bambu's presets bottom out at 0.2× and top out at 0.75× the diameter.
pub fn layer_fits_nozzle(layer_mm: f64, nozzle: &str) -> Result<(), String> {
    let Ok(d) = nozzle.trim().parse::<f64>() else {
        return Err(format!("nozzle {nozzle:?} is not a diameter in mm"));
    };
    if d <= 0.0 {
        return Err(format!("nozzle {nozzle:?} is not a diameter in mm"));
    }
    let (lo, hi) = (d * 0.2, d * 0.75);
    // Compared with slack, because the bounds are products: 0.4 * 0.2 is
    // 0.08000000000000002, and 0.08 mm on a 0.4 nozzle is a STOCK Bambu
    // profile. An exact comparison refuses the very heights the presets ship.
    const SLACK: f64 = 1e-9;
    if layer_mm >= lo - SLACK && layer_mm <= hi + SLACK {
        return Ok(());
    }
    Err(format!(
        "layer={layer_mm} is outside the {lo:.2}–{hi:.2} mm a {d} mm nozzle can print"
    ))
}

/// The output filename for an input model: `cube.stl` → `cube.gcode.3mf`.
///
/// Path separators are stripped and anything outside `[A-Za-z0-9._-]` is folded
/// to `_`: the result names a file on disk and rides in a `Content-Disposition`
/// header, so it must not carry a separator, a quote, or a newline.
pub fn sanitize_output_name(input_name: &str) -> String {
    let base = input_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(input_name)
        .trim();
    let lower = base.to_ascii_lowercase();
    let mut stem = base;
    for suffix in [".gcode.3mf", ".3mf", ".stl", ".step", ".stp"] {
        if lower.len() > suffix.len() && lower.ends_with(suffix) {
            stem = &base[..base.len() - suffix.len()];
            break;
        }
    }
    let cleaned: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('.');
    if cleaned.is_empty() {
        return "model.gcode.3mf".to_string();
    }
    format!("{cleaned}.gcode.3mf")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// An in-memory [`ProfileFetch`], recording what was asked for.
    struct MapFetch {
        profiles: HashMap<(ProfileKind, String), Value>,
        asked: RefCell<Vec<String>>,
    }

    impl MapFetch {
        fn new(entries: &[(ProfileKind, &str, Value)]) -> Self {
            Self {
                profiles: entries
                    .iter()
                    .map(|(k, n, v)| ((*k, (*n).to_string()), v.clone()))
                    .collect(),
                asked: RefCell::new(Vec::new()),
            }
        }
    }

    impl ProfileFetch for MapFetch {
        fn fetch(&self, kind: ProfileKind, name: &str) -> Result<Option<ProfileMap>, String> {
            self.asked.borrow_mut().push(name.to_string());
            Ok(self
                .profiles
                .get(&(kind, name.to_string()))
                .map(|v| v.as_object().expect("fixture is an object").clone()))
        }
    }

    // ── TRAP 1: the inherits chain ──────────────────────────────────────────

    #[test]
    fn flatten_pulls_layer_height_from_the_inherits_grandparent() {
        let fetch = MapFetch::new(&[
            (
                ProfileKind::Process,
                "0.12mm Fine @BBL A1M",
                json!({"inherits": "fdm_process_bbl_0.12", "instantiation": "true", "brim_width": "0"}),
            ),
            (
                ProfileKind::Process,
                "fdm_process_bbl_0.12",
                json!({"inherits": "fdm_process_common", "sparse_infill_speed": "270"}),
            ),
            (
                ProfileKind::Process,
                "fdm_process_common",
                json!({"layer_height": "0.12", "brim_width": "5", "sparse_infill_speed": "100"}),
            ),
        ]);
        let merged = flatten(ProfileKind::Process, "0.12mm Fine @BBL A1M", &fetch, &[]).unwrap();

        // The whole reason this module exists: the leaf does not carry it.
        assert_eq!(merged["layer_height"], json!("0.12"));
        // Child overrides parent, at every depth.
        assert_eq!(merged["brim_width"], json!("0"));
        assert_eq!(merged["sparse_infill_speed"], json!("270"));
        // The chain is resolved — the slicer must not re-chase it.
        assert_eq!(merged["inherits"], json!(""));
        assert!(!merged.contains_key("instantiation"));
    }

    #[test]
    fn the_leaf_alone_has_no_layer_height() {
        // Documents *why* a naive `--load-settings <leaf>.json` is quietly wrong:
        // the slicer would silently fall back to its 0.2mm default.
        let leaf = json!({"inherits": "fdm_process_bbl_0.12", "brim_width": "0"});
        assert!(!leaf.as_object().unwrap().contains_key("layer_height"));
    }

    // ── TRAP 2 (merge side): sibling machine template gcodes ────────────────

    #[test]
    fn machine_flatten_merges_sibling_template_gcodes() {
        let machine = "Bambu Lab A1 mini 0.4 nozzle";
        let fetch = MapFetch::new(&[
            (
                ProfileKind::Machine,
                machine,
                json!({"inherits": "fdm_bbl_3dp_001_common", "printable_area": ["0x0", "180x0", "180x180", "0x180"]}),
            ),
            (
                ProfileKind::Machine,
                "fdm_bbl_3dp_001_common",
                json!({"machine_start_gcode": "G1 X10.1 Y200.0 E15 ;Draw the first line"}),
            ),
            (
                ProfileKind::Machine,
                &format!("{machine} template machine_start_gcode"),
                json!({"machine_start_gcode": "M190 S65\nG29 ; real bed mesh"}),
            ),
        ]);
        let merged = flatten(ProfileKind::Machine, machine, &fetch, &[]).unwrap();

        // The generic fallback from the chain must have been replaced.
        assert_eq!(
            merged["machine_start_gcode"],
            json!("M190 S65\nG29 ; real bed mesh")
        );
        // All five are attempted — a missing one is normal (OrcaSlicer keeps
        // them on the chain), but none may be skipped.
        let asked = fetch.asked.borrow();
        for key in TEMPLATE_GCODE_KEYS {
            let want = format!("{machine} template {key}");
            assert!(asked.contains(&want), "never looked for {want:?}");
        }
    }

    // ── TRAP 2 (output side): the produced start gcode ──────────────────────

    #[test]
    fn start_gcode_fault_detects_the_generic_off_bed_prime() {
        let bed = Some((180.0, 180.0));
        let head = "; layer_height = 0.2\n";

        let generic = format!("{head}G1 X10.1 Y200.0 E15 ;Draw the first line\n; CHANGE_LAYER\n");
        assert_eq!(
            verify_sliced(&generic, 0.2, bed),
            Err(VerifyFault::GenericStartGcode)
        );

        // The real start's out-of-bed wipe carries NO extrusion — the clean signal.
        let real = format!("{head}G1 X10 Y185 F9000\nG1 X10 Y10 E5 F1200\n; CHANGE_LAYER\n");
        assert!(verify_sliced(&real, 0.2, bed).is_ok());

        // Off the bed is off the bed even without the marker string.
        let off_y = format!("{head}G1 X10.1 Y200.0 E15 F1200\n; CHANGE_LAYER\n");
        assert!(matches!(
            verify_sliced(&off_y, 0.2, bed),
            Err(VerifyFault::OffBedExtrusion { line }) if line.contains("Y200.0")
        ));

        // A negative prime is as much a crash as one past Y200.
        let neg_x = format!("{head}G1 X-5 Y10 E1 F1200\n; CHANGE_LAYER\n");
        assert!(matches!(
            verify_sliced(&neg_x, 0.2, bed),
            Err(VerifyFault::OffBedExtrusion { .. })
        ));

        // Only the *start* section is scanned: the print itself lives past the
        // first layer change and legitimately moves everywhere.
        let later = format!("{head}; CHANGE_LAYER\nG1 X10.1 Y200.0 E15 F1200\n");
        assert!(verify_sliced(&later, 0.2, bed).is_ok());

        // With no known bed there is nothing to compare against — we do not
        // invent one.
        assert!(verify_sliced(&off_y, 0.2, None).is_ok());
    }

    #[test]
    fn the_real_starts_bare_purge_lines_are_not_off_bed_extrusion() {
        // Taken from an actual A1 mini slice: the start parks off the bed and
        // purges there with lines carrying no coordinate at all, then lays its
        // prime inside the bed naming X each time. Tracking the head's position
        // across lines — which looks stricter — flags those purges and fails a
        // slice that had just been proven good on the machine.
        let bed = Some((180.0, 180.0));
        let real = concat!(
            "; layer_height = 0.2\n",
            "G1 X10 Y185 F9000\n",
            "G1 E1.2 F500\n",
            "G1 E50 F200\n",
            "G0 X88 E10 F904\n",
            "G0 X93 E.3742 F1508\n",
            "; CHANGE_LAYER\n"
        );
        assert!(
            verify_sliced(real, 0.2, bed).is_ok(),
            "the real start purges off the bed on purpose"
        );
        // What it DOES detect is a move that claims an off-bed coordinate while
        // extruding — the shape of the generic start's prime line.
        let generic = "; layer_height = 0.2\nG1 X10.1 Y200.0 E15 F1200\n; CHANGE_LAYER\n";
        assert!(matches!(
            verify_sliced(generic, 0.2, bed),
            Err(VerifyFault::OffBedExtrusion { .. })
        ));
    }

    #[test]
    fn a_layer_height_that_is_not_a_number_is_not_a_match() {
        // NaN parses as f64 and every comparison with it is false, so
        // `(got - want).abs() > 1e-6` is false too: the check passes for ANY
        // requested height.
        let bed = Some((180.0, 180.0));
        for bad in ["NaN", "nan", "inf", "-inf"] {
            assert_eq!(
                verify_sliced(&format!("; layer_height = {bad}\n"), 0.12, bed),
                Err(VerifyFault::NoLayerHeight),
                "{bad} is not a layer height"
            );
        }
    }

    #[test]
    fn layer_height_mismatch_fails_loudly() {
        let bed = Some((180.0, 180.0));
        assert_eq!(
            verify_sliced("; layer_height = 0.2\n", 0.12, bed),
            Err(VerifyFault::LayerHeightMismatch {
                want: 0.12,
                got: 0.2
            })
        );
        assert_eq!(
            verify_sliced("; nothing useful here\n", 0.12, bed),
            Err(VerifyFault::NoLayerHeight)
        );
        let good = "; total layer number: 166\n; layer_height = 0.12\nM190 S65\n";
        assert_eq!(
            verify_sliced(good, 0.12, bed).unwrap(),
            SliceMeta {
                layer_height_mm: 0.12,
                layers: Some(166),
                bed_temp_c: Some(65),
            }
        );
    }

    #[test]
    fn process_for_layer_maps_stock_heights_and_forces_the_rest() {
        for (h, want) in [
            (0.08, "0.08mm Extra Fine @BBL A1M"),
            (0.12, "0.12mm Fine @BBL A1M"),
            (0.16, "0.16mm Optimal @BBL A1M"),
            (0.20, "0.20mm Standard @BBL A1M"),
            (0.24, "0.24mm Draft @BBL A1M"),
            (0.28, "0.28mm Extra Draft @BBL A1M"),
        ] {
            let c = process_for_layer(h, "@BBL A1M").unwrap();
            assert_eq!(c.name, want);
            assert!(!c.force_layer, "{h} is a stock height");
        }
        let c = process_for_layer(0.15, "@BBL A1M").unwrap();
        assert_eq!(c.name, "0.16mm Optimal @BBL A1M");
        assert!(c.force_layer, "a non-stock height must be forced on top");
    }

    #[test]
    fn a_height_far_from_every_stock_profile_is_refused_rather_than_substituted() {
        // The nearest profile's speeds, flow and cooling were tuned for ITS
        // height. Forcing `layer_height` on top of a profile 0.12 mm away is a
        // guess dressed as a pick, and this module refuses guesses elsewhere.
        let err = process_for_layer(0.40, "@BBL A1M").unwrap_err();
        assert!(err.contains("no process profile near it"), "{err}");
        assert!(err.contains("0.28"), "names the nearest: {err}");
        // Within half a step is a rounding, not a guess.
        assert!(process_for_layer(0.22, "@BBL A1M").is_ok());
        assert!(process_for_layer(0.30, "@BBL A1M").is_ok());
        assert!(process_for_layer(0.31, "@BBL A1M").is_err());
    }

    #[test]
    fn every_override_a_request_produces_is_a_string() {
        // A bare JSON number is silently dropped back to the profile's default
        // (observed), and a dropped override is invisible in the output — the
        // gcode just shows the default. This is the production mapping, not a
        // hand-built one: building it in the I/O layer is what let the trap sit
        // where no test could reach it.
        let (choice, ov) =
            overrides_for(0.15, "@BBL A1M", "Textured PEI Plate", Some(5.0)).unwrap();
        assert!(choice.force_layer);
        for (k, v) in &ov {
            assert!(v.is_string(), "{k} must be a string, got {v}");
        }
        let get = |k: &str| {
            ov.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.as_str().unwrap().to_string())
        };
        assert_eq!(get("curr_bed_type").as_deref(), Some("Textured PEI Plate"));
        assert_eq!(get("layer_height").as_deref(), Some("0.15"));
        assert_eq!(get("brim_type").as_deref(), Some("outer_only"));
        assert_eq!(get("brim_width").as_deref(), Some("5"));
        // A stock height needs no forcing, so it must not be overridden at all.
        let (_, ov) = overrides_for(0.20, "@BBL A1M", "Cool Plate", None).unwrap();
        assert!(ov.iter().all(|(k, _)| *k != "layer_height"));
    }

    #[test]
    fn a_layer_height_is_judged_against_the_nozzle_actually_fitted() {
        // A fixed window accepts what a small nozzle cannot lay down and
        // refuses what a large one can — while naming a nozzle the machine may
        // not have.
        assert!(layer_fits_nozzle(0.2, "0.4").is_ok());
        // Every stock height for a 0.4 nozzle must pass — the bounds are
        // products, and an exact comparison rejects 0.08 because 0.4 * 0.2 is
        // 0.08000000000000002.
        for h in [0.08, 0.12, 0.16, 0.20, 0.24, 0.28] {
            assert!(layer_fits_nozzle(h, "0.4").is_ok(), "{h} is a stock height");
        }
        assert!(layer_fits_nozzle(0.4, "0.2").is_err(), "0.2 can't lay 0.4");
        assert!(layer_fits_nozzle(0.5, "0.8").is_ok(), "0.8 can");
        assert!(layer_fits_nozzle(0.02, "0.4").is_err());
        let err = layer_fits_nozzle(0.4, "0.2").unwrap_err();
        assert!(
            err.contains("0.2 mm nozzle"),
            "names the real nozzle: {err}"
        );
        assert!(layer_fits_nozzle(0.2, "wat").is_err());
        assert!(layer_fits_nozzle(0.2, "0").is_err());
    }

    #[test]
    fn slicer_args_use_a_bare_export_name_and_xvfb_wraps_the_whole_argv() {
        let args = slicer_args(
            Path::new("/w/m.json"),
            Path::new("/w/p.json"),
            Path::new("/w/f.json"),
            Path::new("/w"),
            "cube.gcode.3mf",
            Path::new("/w/in.stl"),
        );
        let at = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .map(|i| args[i + 1].clone())
                .unwrap_or_else(|| panic!("{flag} missing"))
        };
        assert_eq!(at("--load-settings"), "/w/m.json;/w/p.json");
        assert_eq!(at("--load-filaments"), "/w/f.json");
        assert_eq!(at("--arrange"), "1");
        assert_eq!(at("--orient"), "1");
        assert_eq!(at("--slice"), "0");
        assert_eq!(at("--outputdir"), "/w");
        // A bare filename: with --outputdir the slicer concatenates the two, so a
        // path here doubles up and the export fails.
        assert_eq!(at("--export-3mf"), "cube.gcode.3mf");
        assert_eq!(args.last().unwrap(), "/w/in.stl");

        let argv = wrap_xvfb(vec!["slicer".into(), "--slice".into()], true);
        assert_eq!(argv[..2], ["xvfb-run".to_string(), "-a".to_string()]);
        assert_eq!(argv[2], "slicer");
        assert_eq!(wrap_xvfb(vec!["slicer".into()], false), vec!["slicer"]);
    }

    #[test]
    fn profile_names_that_would_escape_the_profiles_dir_are_rejected() {
        for bad in [
            "../x",
            "a/b",
            "a\\b",
            ".hidden",
            "",
            "sub/../../etc/passwd",
            "name\nwith newline",
            &"x".repeat(161),
        ] {
            assert!(!is_safe_profile_name(bad), "{bad:?} must be rejected");
        }
        for good in [
            "Bambu PLA Basic @BBL A1M",
            "0.12mm Fine @BBL A1M",
            "Bambu Lab A1 mini 0.4 nozzle",
            "Bambu PLA Silk+ @BBL A1M",
            &"x".repeat(160),
        ] {
            assert!(is_safe_profile_name(good), "{good:?} must be accepted");
        }
    }

    #[test]
    fn flatten_refuses_an_inherits_cycle_instead_of_looping() {
        let fetch = MapFetch::new(&[
            (ProfileKind::Process, "a", json!({"inherits": "b"})),
            (ProfileKind::Process, "b", json!({"inherits": "a"})),
        ]);
        assert_eq!(
            flatten(ProfileKind::Process, "a", &fetch, &[]),
            Err(SliceError::InheritsCycle("a".to_string()))
        );
    }

    #[test]
    fn a_missing_ancestor_is_a_hard_error_not_a_silent_skip() {
        let fetch = MapFetch::new(&[(ProfileKind::Process, "leaf", json!({"inherits": "gone"}))]);
        assert_eq!(
            flatten(ProfileKind::Process, "leaf", &fetch, &[]),
            Err(SliceError::ProfileNotFound {
                kind: "process",
                name: "gone".to_string()
            })
        );
        assert_eq!(
            flatten(ProfileKind::Process, "nope", &fetch, &[]),
            Err(SliceError::ProfileNotFound {
                kind: "process",
                name: "nope".to_string()
            })
        );
    }

    #[test]
    fn bed_bounds_reads_the_printable_area_polygon() {
        let m = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(
            bed_bounds(&m(
                json!({"printable_area": ["0x0", "180x0", "180x180", "0x180"]})
            )),
            Some((180.0, 180.0))
        );
        // Garbage entries are skipped, not fatal.
        assert_eq!(
            bed_bounds(&m(json!({"printable_area": ["0x0", "nope", "256x256", 7]}))),
            Some((256.0, 256.0))
        );
        assert_eq!(bed_bounds(&m(json!({}))), None);
        assert_eq!(bed_bounds(&m(json!({"printable_area": ["bad"]}))), None);
    }

    #[test]
    fn overrides_land_as_strings_because_a_bare_number_is_dropped() {
        // Slicer config values are strings; a bare JSON number is silently
        // dropped back to the default (observed).
        let fetch = MapFetch::new(&[(
            ProfileKind::Process,
            "p",
            json!({"layer_height": "0.16", "curr_bed_type": "Cool Plate"}),
        )]);
        let merged = flatten(
            ProfileKind::Process,
            "p",
            &fetch,
            &[
                ("curr_bed_type", json!("Textured PEI Plate")),
                ("layer_height", json!("0.15")),
                ("brim_type", json!("outer_only")),
                ("brim_width", json!("5")),
            ],
        )
        .unwrap();
        assert_eq!(merged["curr_bed_type"], json!("Textured PEI Plate"));
        assert_eq!(merged["layer_height"], json!("0.15"));
        assert_eq!(merged["brim_width"], json!("5"));
        for key in ["curr_bed_type", "layer_height", "brim_width"] {
            assert!(merged[key].is_string(), "{key} must be a JSON string");
        }
    }

    #[test]
    fn sanitize_output_name_strips_paths_and_extensions() {
        assert_eq!(sanitize_output_name("cube.stl"), "cube.gcode.3mf");
        assert_eq!(sanitize_output_name("Part A.STEP"), "Part_A.gcode.3mf");
        assert_eq!(sanitize_output_name("bracket.3mf"), "bracket.gcode.3mf");
        assert_eq!(sanitize_output_name("x.gcode.3mf"), "x.gcode.3mf");
        assert_eq!(sanitize_output_name("../../etc/passwd"), "passwd.gcode.3mf");
        assert_eq!(sanitize_output_name("a\\b\\c.stl"), "c.gcode.3mf");
        assert_eq!(sanitize_output_name(".."), "model.gcode.3mf");
        assert_eq!(sanitize_output_name("\"evil\n.stl"), "_evil_.gcode.3mf");
    }

    #[test]
    fn machine_profile_name_needs_the_nozzle() {
        let names = SlicerNames {
            machine_base: "Bambu Lab A1 mini",
            preset_suffix: "@BBL A1M",
        };
        // The bare base name is only a model descriptor, not a usable preset.
        assert_eq!(names.machine_profile("0.4"), "Bambu Lab A1 mini 0.4 nozzle");
    }
}
