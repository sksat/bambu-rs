//! `bambu` — the command-line consumer of the `bambu_rs` library.
//!
//! Thin noun-verb wrapper: argument parsing, JSON/human output formatting,
//! exit codes, confirmation flags and the local exclusive lock live here; all
//! protocol behaviour lives in the reusable [`bambu_rs`] library.
//!
//! The real CLI is built incrementally (see the plan, phases P0–P3). For now
//! this is a placeholder so the `bambu` binary target compiles.

fn main() {
    eprintln!(
        "bambu {} — work in progress (see plan P0–P3)",
        env!("CARGO_PKG_VERSION")
    );
}
