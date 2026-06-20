fn main() {
    // The `dashboard` feature embeds `web/dist/` via rust-embed, whose derive
    // requires the folder to EXIST at compile time (it errors otherwise) — even
    // before `pnpm build` populates it. Create it here so a fresh checkout
    // builds with no committed placeholder file. Runtime serves a built-in
    // fallback page when the folder is empty (see src/server/assets.rs).
    #[cfg(feature = "dashboard")]
    {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let _ = std::fs::create_dir_all(std::path::Path::new(&manifest).join("web/dist"));
    }

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
