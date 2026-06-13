//! `bambu` — the command-line consumer of the `bambu_rs` library.
//!
//! Thin noun-verb wrapper: argument parsing, JSON/human output formatting,
//! exit codes, confirmation flags and the local exclusive lock live here; all
//! protocol behaviour lives in the reusable [`bambu_rs`] library.
//!
//! All behaviour lives in [`bambu_rs::cli`]; this just returns its exit code.

fn main() -> std::process::ExitCode {
    bambu_rs::cli::run()
}
