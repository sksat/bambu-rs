//! The `core::capability` slicer row, checked against a real BBL bundle.
//!
//! The registry's own contract — and CLAUDE.md — say extending model support
//! means a captured fixture plus a row. The row for the A1 mini went in without
//! one, so every test that touched it restated the same profile names it was
//! meant to be checking, and a typo in either would have agreed with itself.
//!
//! Two halves, because the useful properties need different things:
//!
//! * [`the_row_matches_the_captured_bundle`] runs everywhere, including the
//!   lib-only CI job — it touches nothing but `core`, so it is not feature
//!   gated. It
//!   checks the names the row produces against
//!   `tests/fixtures/bbl-a1mini-manifest.json` — a provenance-recorded capture
//!   of a real bundle's *names*. Nothing here needs a slicer installed.
//! * [`capability_row_matches_the_installed_bundle`] is `#[ignore]`d and needs
//!   one. It re-checks the manifest against the bundle on this machine, so a
//!   stale manifest is discoverable rather than silently authoritative, and
//!   flattens the real `inherits` chains to prove the row actually slices.
//!
//! The manifest holds names, not vendor profile content: the profiles belong to
//! Bambu and ship with the slicer, and copying them into an MIT repository is
//! not a decision to make in passing. The `#[ignore]`d test is what covers the
//! content, on a machine that legitimately has it.

use bambu_rs::core::capability::default_registry;
use bambu_rs::core::model::Model;
use bambu_rs::core::slice::{ProfileFetch, ProfileKind, flatten, process_for_layer};
use serde_json::Value;

/// Reads profiles straight off the installed bundle.
///
/// A few lines here rather than reaching for the server's `FsProfileFetch`,
/// which is not public — widening the library's API so a test can borrow a
/// convenience is the wrong trade, and this needs none of that one's path
/// hardening: the names come from the manifest in this repository, not from a
/// request.
struct BundleFiles(std::path::PathBuf);

impl ProfileFetch for BundleFiles {
    fn fetch(
        &self,
        kind: ProfileKind,
        name: &str,
    ) -> Result<Option<bambu_rs::core::slice::ProfileMap>, String> {
        let path = self.0.join(kind.subdir()).join(format!("{name}.json"));
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

const MANIFEST: &str = include_str!("fixtures/bbl-a1mini-manifest.json");

fn manifest() -> Value {
    serde_json::from_str(MANIFEST).expect("the manifest is valid JSON")
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .unwrap_or_else(|| panic!("manifest has no array {key:?}"))
        .iter()
        .map(|s| s.as_str().expect("string").to_string())
        .collect()
}

/// The names the registry row generates must be names a real bundle has.
#[test]
fn the_row_matches_the_captured_bundle() {
    let m = manifest();
    let names = default_registry()
        .slicer_names(&Model::A1Mini)
        .copied()
        .expect("the A1 mini has a slicer row");

    let nozzle = m["nozzle"].as_str().unwrap();
    assert_eq!(
        names.machine_profile(nozzle),
        m["machine_preset"].as_str().unwrap(),
        "the machine preset this row builds is not the one the bundle ships"
    );

    // Every process name the slicing table can ask for must exist. This is the
    // check that a synthetic fixture cannot make: the suffix, the spacing and
    // the wording all have to match a real file name exactly.
    let suffix = names
        .process_suffix(nozzle)
        .expect("0.4 is the mapped nozzle");
    let have = strings(&m, "process_presets");
    for layer in [0.08, 0.12, 0.16, 0.20, 0.24, 0.28] {
        let choice = process_for_layer(layer, suffix)
            .unwrap_or_else(|e| panic!("{layer}mm has no process: {e}"));
        assert!(
            have.contains(&choice.name),
            "{layer}mm resolves to {:?}, which the bundle does not have.\nIt ships: {have:#?}",
            choice.name
        );
    }

    // And the filament naming, from the other end: the example the manifest
    // recorded must carry this row's suffix.
    let filament = m["filament_preset_example"].as_str().unwrap();
    assert!(
        filament.ends_with(suffix),
        "{filament:?} does not end with this row's preset suffix {suffix:?}"
    );
}

/// Everything above is only as true as the manifest. This re-derives it from
/// the bundle installed here and reports what has moved.
///
/// `cargo test -- --ignored`
#[test]
#[ignore = "needs a BBL profile bundle installed"]
fn capability_row_matches_the_installed_bundle() {
    let m = manifest();
    // The manifest's path is provenance — where this capture came from — not a
    // requirement. A bundle lives somewhere else on Bambu Studio, on macOS and
    // Windows, and wherever `--slicer-profiles` points, and refusing to check
    // those would make this test useful on exactly one machine.
    let recorded = std::path::PathBuf::from(m["captured_from"]["bundle"].as_str().unwrap());
    // An explicit choice that does not exist is an error, not a reason to go
    // and check some other bundle: passing then would tell you your install is
    // fine when it was never looked at.
    if let Some(chosen) = std::env::var_os("BAMBU_BBL_PROFILES") {
        let chosen = std::path::PathBuf::from(chosen);
        assert!(
            chosen.is_dir(),
            "BAMBU_BBL_PROFILES={} is not a directory",
            chosen.display()
        );
    }
    let candidates = [
        std::env::var_os("BAMBU_BBL_PROFILES").map(std::path::PathBuf::from),
        Some(recorded.clone()),
        Some("/opt/bambustudio-bin/resources/profiles/BBL".into()),
    ];
    let Some(root) = candidates.into_iter().flatten().find(|p| p.is_dir()) else {
        panic!(
            "no BBL bundle found (looked at $BAMBU_BBL_PROFILES, {}, and the Bambu Studio \
             install) — point BAMBU_BBL_PROFILES at yours",
            recorded.display()
        );
    };
    let fetch = BundleFiles(root.clone());

    // The recorded chains are the load-bearing part: `flatten` walks them, and
    // a chain that has changed shape upstream is exactly the "bundle-layout
    // mismatch" this whole file exists to catch.
    for (kind, key, leaf_key) in [
        (ProfileKind::Machine, "machine_chain", "machine_preset"),
        (
            ProfileKind::Process,
            "process_chain_example",
            "process_chain_example",
        ),
        (
            ProfileKind::Filament,
            "filament_chain_example",
            "filament_preset_example",
        ),
    ] {
        let recorded = strings(&m, key);
        let leaf = match m[leaf_key].as_str() {
            Some(s) => s.to_string(),
            None => recorded[0].clone(),
        };
        let mut actual = Vec::new();
        let mut name = leaf.clone();
        while !name.is_empty() {
            actual.push(name.clone());
            let p = fetch
                .fetch(kind, &name)
                .unwrap_or_else(|e| panic!("{kind:?} {name:?}: {e}"))
                .unwrap_or_else(|| panic!("{kind:?} {name:?} is not in the bundle"));
            name = p
                .get("inherits")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
        }
        assert_eq!(
            actual, recorded,
            "the {kind:?} inherits chain has moved since the manifest was captured — \
             re-capture it and check the row still holds"
        );
    }

    // Flattening the real chain has to produce a usable profile, not just
    // resolve. These are the keys a slice would be wrong without.
    let machine = flatten(
        ProfileKind::Machine,
        m["machine_preset"].as_str().unwrap(),
        &fetch,
        &[],
    )
    .expect("the machine profile flattens");
    for key in ["printable_area", "machine_start_gcode", "nozzle_diameter"] {
        assert!(
            machine.contains_key(key),
            "flattened machine has no {key:?}"
        );
    }

    // EVERY height the row can ask for, not just the one the manifest recorded
    // a chain for. A bundle that renamed or broke `0.08mm Extra Fine` would
    // otherwise leave this test green while that height fails to slice.
    let names = default_registry()
        .slicer_names(&Model::A1Mini)
        .copied()
        .expect("the A1 mini has a slicer row");
    let suffix = names
        .process_suffix(m["nozzle"].as_str().unwrap())
        .expect("0.4 is the mapped nozzle");
    for layer in [0.08, 0.12, 0.16, 0.20, 0.24, 0.28] {
        let choice = process_for_layer(layer, suffix).expect("a stock height has a process");
        let flat = flatten(ProfileKind::Process, &choice.name, &fetch, &[])
            .unwrap_or_else(|e| panic!("{:?} does not flatten: {e}", choice.name));
        // The first trap this module's docs describe: the leaf carries no
        // `layer_height` at all, so an unresolved chain silently slices at
        // Orca's 0.2mm default — which looks right for 0.20 by coincidence and
        // is wrong for every other height. Checking all six is what makes that
        // coincidence visible.
        let got = flat
            .get("layer_height")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{:?} flattened without a layer_height", choice.name));
        assert!(
            (got.parse::<f64>().expect("a number") - layer).abs() < 1e-9,
            "{:?} flattened to layer_height {got}, not {layer}",
            choice.name
        );
    }
}
