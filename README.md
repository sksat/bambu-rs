# bambu-rs

[![crates.io](https://img.shields.io/crates/v/bambu-rs.svg)](https://crates.io/crates/bambu-rs)
[![docs.rs](https://img.shields.io/docsrs/bambu-rs)](https://docs.rs/bambu-rs)
[![CI](https://github.com/sksat/bambu-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/sksat/bambu-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A command-line tool — with a web dashboard and a reusable Rust library — for
monitoring and driving [Bambu Lab](https://bambulab.com/) 3D printers over the LAN,
agent-friendly and safe, so a human or an AI agent can watch and run prints without
the cloud.

It's a **clean-room** implementation: built from the protocol documentation
([OpenBambuAPI](https://github.com/Doridian/OpenBambuAPI)) and direct observation of
real hardware, with no dependency on — or reference to — existing Bambu libraries
(the observed protocol facts are written up in [docs/protocol.md](docs/protocol.md)).
What makes it safe to automate: machine-readable JSON (`--json`), a semantic
exit-code scheme, `--confirm`/`--dry-run` gates on every physical action, and
*verify-by-reread* — success is confirmed from the printer's own report, never from
publish success. Per-firmware API differences are absorbed by a `(model, firmware)`
capability registry. The CLI is the main consumer; the same crate is usable as a
library (`use bambu_rs::...`).

> ⚠️ Early development, and **only tested against my own A1 mini** on my home LAN.
> Other models / firmware are unverified — treat them as best-effort. Control needs
> the printer in **LAN-only + Developer Mode** (the Jan-2025 Authorization Control
> System); reads work without it.

## Usage

```bash
# one-time: register a printer (the 8-digit LAN access code is on the printer's screen)
bambu config add --printer a1 --ip 192.168.1.50 --serial <SERIAL> \
  --access-code <CODE> --model a1mini

# read state — one-shot JSON, or --watch to follow the active print to completion
bambu status --json
bambu status --watch
bambu info                          # firmware + the resolved capabilities for this printer
bambu hms                           # decode any health/maintenance (HMS) alerts

# files on the printer (FTPS): ls / upload / download / rm
bambu file ls
bambu file upload model.gcode.3mf --dest /cache

# start a print: preview the resolved plan, then run it with guards and watch it
bambu job start /cache/model.gcode.3mf --plate 1 --dry-run        # → md5 / plate / AMS map
bambu job start /cache/model.gcode.3mf --plate 1 --ams-map 0 \
  --expect-md5 <md5> --expect-plate 1 --confirm --watch
```

Reads are a single connect → snapshot. Physical actions
(`job start/pause/resume/stop`, `temp`, `light`, `gcode`, `ams`, `calibrate`) require
`--confirm` and check the printer's own state first (idle, no errors, the expected
file/plate). Under `--json` the output is machine-readable, and the exit code
distinguishes success / unverified / rejected / busy so scripts and agents can branch
on it.

## Dashboard

`bambu serve` runs a small local server with an embedded web dashboard for live
monitoring from a phone or browser — printer status, temperatures, AMS, the live
camera, one-click clean-timelapse capture, and the usual controls — all over the
same single LAN connection (reads are open; control is gated behind an optional
password). No cloud, no second app.

<p align="center">
  <img src="assets/dashboard-demo.gif" alt="bambu serve web dashboard" width="600">
</p>

```bash
# the web UI needs the `dashboard` feature (release binaries already include it):
#   cargo install bambu-rs --features dashboard
bambu serve            # serves the dashboard on the LAN and prints its URL
```

## Slicing

`bambu-rs` doesn't slice — it **delegates to Bambu Studio / OrcaSlicer's CLI** to
produce a sliced `.gcode.3mf`, then uploads and prints it:

```bash
# 1. Slice a model to .gcode.3mf (Bambu Studio / OrcaSlicer CLI)
bambu-studio --slice 1 \
  --load-settings "machine.json;process.json" \
  --load-filaments "filament.json" \
  --allow-newer-file \
  --export-3mf out.gcode.3mf  model.3mf

# 2. Upload, preview the plan, then print it (asserting it's exactly what you inspected)
bambu file upload out.gcode.3mf --dest /cache
bambu job start /cache/out.gcode.3mf --plate 1 --dry-run          # → inspection.gcode_md5
bambu job start /cache/out.gcode.3mf --plate 1 --ams-map 0 \
  --expect-md5 <that-md5> --expect-plate 1 --confirm --watch
```

Full details (flags, AMS mapping, external spool, `--dry-run`) in
[docs/slicing.md](docs/slicing.md).

## Timelapse

Three ways: a printer-side toggle (`bambu timelapse enable/disable`, and
`job start --timelapse`), fetching the recorded video (`bambu timelapse get`), and —
for printers whose built-in camera is missing or broken — driving an **external**
camera from the print's own layer events:

```bash
# Grab one frame per layer with any capture tool; the command goes after `--`
# (so its own flags are fine), with {frame}/{layer}/{outdir} substituted in:
bambu timelapse capture --out-dir ./tl -- fswebcam -r 1280x720 {frame}

# An IP camera (e.g. an ATOM Cam running atomcam_tools) over plain HTTP:
bambu timelapse capture --out-dir ./tl -- \
  curl -s -m 15 -o {frame} "http://$ATOMCAM_HOST/cgi-bin/get_jpeg.cgi"
```

It runs the command as argv (**no shell**), skips a failed grab and continues, and
prints a suggested `ffmpeg` line to stitch the frames. The `serve` dashboard wraps
this into one-click capture, with on-device parked-frame selection so each frame
shows the object with the head out of the way.

## More commands

`bambu speed <silent|standard|sport|ludicrous>`,
`bambu light on|off [--node chamber|work]`, `bambu gcode <line>` (with a static
safety guard — over-limit temps / cold extrusion are refused unless `--force`), and
`bambu ams <resume|reset|pause|change|set-filament|settings>` (all need `--confirm`;
`change`/`set-filament` also support `--dry-run`). Deeper slicer integration and an
MCP server come later.
