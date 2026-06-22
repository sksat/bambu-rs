# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`bambu-rs` is an AI-agent-friendly Bambu Lab 3D printer CLI & library, with an embedded React
dashboard.

## Working process

- **TDD.** Failing test first, minimal code to pass, then refactor — `core/` is kept I/O-free so
  logic can be driven this way.
- **Design with a second opinion first.** For a non-trivial design, consult `smart-friend`
  (`codex exec --sandbox read-only -m gpt-5.5 …`) and weigh its take against your own before
  committing to an approach.
- **Review substantial changes** with `code-review-gpt` (`codex review -c model=gpt-5.5 --base main`)
  before finalizing; engage with the feedback rather than rubber-stamping it.

## Working in this codebase

- **Pure logic vs I/O is the load-bearing split.** `src/core/` is protocol logic with no I/O — the
  test surface; inject `core::fake::FakePrinter` instead of mocking. I/O lives in `client.rs`
  (MQTT), `ftp.rs`, `park.rs`/`camera.rs`, and `server/`. Keep CLI concerns out of the library API.
- **No baked heuristic defaults.** Tuning constants (e.g. park-frame selection) are config-driven
  with no in-code defaults — a missing knob is a hard error, because the right value depends on the
  physical setup.
- **Extending model/firmware support** means a captured fixture plus a row in the `core::capability`
  registry, not an inline version branch.
- **One MQTT connection at a time** — a second concurrent client (e.g. the CLI while `serve` runs)
  makes the A1 mutually disconnect.
- **Local checks:** a full build/test is `--features dashboard,ts-rs`; CI also builds a smaller
  feature matrix (`.github/workflows/ci.yml`), so keep the lib-only and dashboard-less builds
  green. After changing a `ts-rs`-exported type, run `pnpm -C web gen:types` and commit, or the
  frontend CI job fails.
