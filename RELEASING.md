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
  [`arkedge/notalawyer`](https://github.com/arkedge/notalawyer), which gathers the
  third-party notice (per `about.toml`) and embeds it, exposed by `bambu --license-notice`.
  The feature is **off by default**, so normal builds and `cargo install` skip the
  build-time license gathering.
- **assets** per target: `bambu-rs-<target>-v<version>.tar.gz` (`.zip` on
  Windows) containing `bambu`(`.exe`) + README + LICENSE, plus a standalone
  `bambu-<target>` binary, each with a `.sha256` sidecar. The Unix tarballs are
  reproducible (sorted, zeroed owner/mtime, `gzip -n`); the Windows `.zip` is not
  bit-identical across runs (PowerShell `Compress-Archive` deflate), though its
  checksum is always consistent with the produced file.
- **cargo-binstall**: `[package.metadata.binstall]` in `Cargo.toml` matches the
  asset names, so `cargo binstall bambu-rs` works once a release exists.

## Verification status

The host-side of the pipeline is verified locally (x86_64-linux, 2026-06-15);
the Linux **cross** link step needs Docker and hasn't been run.

- ⚠️ **native-tls / OpenSSL under `cross`** — the one unverified step (highest
  risk). The crate uses `suppaftp` with `native-tls` (OpenSSL) for FTPS; linking
  OpenSSL inside the old gnu/musl `cross` images can fail. If it does, the fix is
  vendored OpenSSL or a rustls FTPS backend. **Needs Docker** — exercise it from
  the Actions tab via `workflow_dispatch` (runs the whole matrix, no tag), and
  test the musl targets first.
- ✅ **`about.toml` accept-list** — verified (no cargo-about install needed):
  `cargo build --no-default-features --features license-notice --bin bambu` builds clean —
  every dependency license is accepted; no error. Re-run after a dependency bump and add any
  reported licenses (`BAMBU_WEB_BUILD_STRICT` is unrelated; a missing license errors on its own).
- ✅ **license notice** — verified: `bambu --license-notice` prints the embedded
  notice (~7.6k lines, all deps).
- ✅ **self-contained SPA** — verified: a release `--features dashboard` binary
  run from a directory with no `web/dist` still serves the real dashboard, so the
  SPA is embedded via rust-embed in release (not read from disk).
