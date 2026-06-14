fn main() {
    // License notices are embedded only for release distribution, behind the
    // `license-notice` feature. Without it we never touch cargo-about, so normal
    // `cargo build` / `cargo test` / `cargo install` have no extra toolchain
    // requirement. With it, `notalawyer_build::build()` shells out to
    // `cargo about generate` (using notalawyer-build's bundled about.hbs) and
    // writes the notice to $OUT_DIR/notalawyer — and PANICS if cargo-about is
    // missing, which is exactly why this is gated.
    #[cfg(feature = "license-notice")]
    {
        println!("cargo:rerun-if-changed=about.toml");
        println!("cargo:rerun-if-changed=Cargo.toml");
        println!("cargo:rerun-if-changed=Cargo.lock");
        notalawyer_build::build();
    }
}
