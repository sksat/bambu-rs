# Releasing

`bambu` is distributed as prebuilt binaries via GitHub Releases, built by
[`.github/workflows/release.yml`](.github/workflows/release.yml).

## Cutting a release

1. Bump `version` in `Cargo.toml` (and update `Cargo.lock`: `cargo update -p bambu-rs`).
2. Commit, then tag **`vX.Y.Z`** (the `v` prefix matters — assets and
   `cargo-binstall` resolve `v{version}` paths) and push the tag.
3. The workflow builds every target and opens a **draft** release. Review it,
   then publish.

## What the workflow does

- **dashboard** job: builds the web SPA once (`pnpm -C web build`) and uploads
  it; every target build downloads it into `web/dist/` so `rust-embed` embeds
  the same SPA. The release binary is built `--features dashboard,license-notice`.
- **build** matrix (7 targets):

  | Target | Runner | How |
  |---|---|---|
  | `x86_64-unknown-linux-gnu` | ubuntu | `cross`, pinned old-glibc image (glibc 2.23) |
  | `x86_64-unknown-linux-musl` | ubuntu | `cross`, static |
  | `aarch64-unknown-linux-gnu` | ubuntu | `cross`, pinned old-glibc image |
  | `aarch64-unknown-linux-musl` | ubuntu | `cross`, static |
  | `x86_64-apple-darwin` | macos-15-intel | native |
  | `aarch64-apple-darwin` | macos-14 | native |
  | `x86_64-pc-windows-msvc` | windows | native (`.zip`) |

  The Linux glibc floor is pinned via `Cross.toml` (images `:0.2.5`) so the
  binaries run on old systems.
- **license notices** (`license-notice` feature): `build.rs` runs
  [`arkedge/notalawyer`](https://github.com/arkedge/notalawyer) → `cargo-about`
  (per `about.toml`) and embeds the third-party notice, exposed by `bambu
  --license-notice`. The feature is **off by default**, so normal builds and
  `cargo install` never need `cargo-about`. `cargo-about` is installed on native
  runners and inside the `cross` containers (`Cross.toml` `pre-build`).
- **assets** per target: `bambu-rs-<target>-v<version>.tar.gz` (`.zip` on
  Windows) containing `bambu`(`.exe`) + README + LICENSE, plus a standalone
  `bambu-<target>` binary, each with a `.sha256` sidecar. The Unix tarballs are
  reproducible (sorted, zeroed owner/mtime, `gzip -n`); the Windows `.zip` is not
  bit-identical across runs (PowerShell `Compress-Archive` deflate), though its
  checksum is always consistent with the produced file.
- **cargo-binstall**: `[package.metadata.binstall]` in `Cargo.toml` matches the
  asset names, so `cargo binstall bambu-rs` works once a release exists.

## Not yet verified on a real tag run

This pipeline is authored but **has not been run end-to-end** yet. Watch the
first real tag for:

- **native-tls / OpenSSL under `cross`** (highest risk): the crate uses
  `suppaftp` with `native-tls` (OpenSSL) for FTPS. Linking OpenSSL inside the
  old gnu/musl `cross` images can fail; if it does, the fix is vendored OpenSSL
  or moving FTPS to a rustls backend. Test the musl targets first.
- **`about.toml` accept-list**: `cargo-about` errors on any dependency license
  not in the list. Run `cargo install --locked cargo-about@0.9.0 && cargo build
  --features dashboard,license-notice` once locally and add any reported
  licenses before tagging, or the build panics.
- **Empty SPA**: the build asserts `web/dist/index.html` exists, but confirm the
  published binary serves the real dashboard (not the fallback page).
