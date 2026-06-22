fn main() {
    // The `dashboard` feature embeds `web/dist/` (the built SPA) via rust-embed. Make sure
    // it's available at compile time — see [`dashboard::ensure`].
    #[cfg(feature = "dashboard")]
    dashboard::ensure();

    // License notices are embedded only for release distribution, behind the
    // `license-notice` feature, which keeps the build-time license gathering out of a normal
    // `cargo build` / `cargo test` / `cargo install`. With it, `notalawyer_build::build()`
    // resolves the dependency graph and gathers each crate's license into $OUT_DIR/notalawyer,
    // reading `about.toml` (the SPDX accept-list) and possibly fetching license texts over the
    // network at build time.
    #[cfg(feature = "license-notice")]
    {
        println!("cargo:rerun-if-changed=about.toml");
        println!("cargo:rerun-if-changed=Cargo.toml");
        println!("cargo:rerun-if-changed=Cargo.lock");
        notalawyer_build::build();
    }
}

#[cfg(feature = "dashboard")]
mod dashboard {
    use std::env;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    /// Make `web/dist/` (the SPA `rust-embed`'s `#[folder]` embeds) available at compile time.
    ///
    /// The folder must EXIST for the derive to compile (it errors otherwise), and a non-empty
    /// one is what makes `bambu serve` actually serve the dashboard rather than the built-in
    /// fallback page (see `src/server/assets.rs`). Resolution, best-first:
    ///
    /// 1. **Already built** — the release matrix downloads a pre-built `web/dist` before
    ///    compiling, and a dev keeps one from `pnpm -C web build`. If `web/dist/index.html`
    ///    exists we use it and stop (no toolchain needed).
    /// 2. **Build it** — otherwise (a crates.io tarball or a fresh git checkout — neither
    ///    ships `web/dist`, only the frontend source) run a best-effort
    ///    `pnpm install --frozen-lockfile && pnpm build`.
    /// 3. **Fallback** — any constrained environment (docs.rs has no network; an offline or
    ///    `BAMBU_SKIP_WEB_BUILD=1` build; a host without node/pnpm; a failed build) keeps the
    ///    empty dir and the runtime fallback page rather than failing the whole crate build.
    ///    Set `BAMBU_WEB_BUILD_STRICT=1` to turn a failed/again-skipped build into a hard
    ///    error instead — used by release/CI verification, where the SPA must be present.
    pub fn ensure() {
        let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
        let dist = manifest.join("web/dist");
        // rust-embed's derive needs the folder to exist even when it ends up empty.
        let _ = std::fs::create_dir_all(&dist);

        // Rebuild the SPA only when its real inputs change — never on node_modules/dist or
        // Playwright (test) churn, which must not retrigger the Rust build.
        for p in [
            "web/package.json",
            "web/pnpm-lock.yaml",
            "web/pnpm-workspace.yaml",
            "web/index.html",
            "web/vite.config.ts",
            "web/tsconfig.json",
            "web/src",
        ] {
            println!("cargo:rerun-if-changed={p}");
        }
        for e in [
            "DOCS_RS",
            "CARGO_NET_OFFLINE",
            "BAMBU_SKIP_WEB_BUILD",
            "BAMBU_WEB_BUILD_STRICT",
        ] {
            println!("cargo:rerun-if-env-changed={e}");
        }

        // 1. A pre-built SPA wins — no toolchain needed (the common crates.io / release path).
        if dist.join("index.html").exists() {
            return;
        }

        let strict = env::var("BAMBU_WEB_BUILD_STRICT").as_deref() == Ok("1");

        // 3a. Don't shell out to a package manager in constrained environments.
        if env::var_os("DOCS_RS").is_some()
            || env::var("CARGO_NET_OFFLINE").as_deref() == Ok("true")
            || env::var("BAMBU_SKIP_WEB_BUILD").as_deref() == Ok("1")
        {
            give_up(strict, "skipping the SPA build (docs.rs/offline/opt-out)");
            return;
        }
        if !have("node") || !have("pnpm") {
            give_up(
                strict,
                "node/pnpm not found (install them, or use a release binary, for the full dashboard)",
            );
            return;
        }

        // 2. Best-effort build.
        let built = run(
            "pnpm",
            &["-C", "web", "install", "--frozen-lockfile"],
            &manifest,
        ) && run("pnpm", &["-C", "web", "build"], &manifest)
            && dist.join("index.html").exists();
        if !built {
            give_up(strict, "SPA build failed");
        }
    }

    /// Either hard-fail (strict, for release/CI) or warn and let the runtime fallback page
    /// stand in. The message explains which path was taken.
    fn give_up(strict: bool, why: &str) {
        if strict {
            panic!("bambu dashboard: {why} and BAMBU_WEB_BUILD_STRICT=1");
        }
        println!("cargo:warning=bambu dashboard: {why}; embedding the fallback page");
    }

    fn have(cmd: &str) -> bool {
        Command::new(cmd)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn run(cmd: &str, args: &[&str], dir: &Path) -> bool {
        Command::new(cmd)
            .args(args)
            .current_dir(dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}
