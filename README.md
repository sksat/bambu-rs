# bambu-rs

Clean-room, agent-friendly **library and CLI** for monitoring and driving
[Bambu Lab](https://bambulab.com/) 3D printers over the LAN — built so AI agents
can run prints safely.

> Status: early development. The reusable library (`bambu_rs`) is the first-class
> artifact; the `bambu` command is its first consumer. Target hardware for the
> MVP is the **A1 mini** in **LAN-only + Developer Mode**.

## Design highlights

- **Library first.** `cargo install bambu-rs` gives the `bambu` CLI; depending on
  `bambu-rs` gives the reusable library (`use bambu_rs::...`). A future MCP server
  is just another consumer.
- **Clean-room.** Implemented from the protocol documentation
  ([OpenBambuAPI](https://github.com/Doridian/OpenBambuAPI)) and direct
  observation of real hardware — no dependency on, or reference to, existing
  Bambu library implementations. The observed protocol facts (transport, access
  modes, model codes, report shape, command verification, HMS) are written up in
  [docs/protocol.md](docs/protocol.md).
- **Agent-friendly & safe.** Machine-readable JSON with `--json` (human-readable
  by default), a semantic exit-code scheme,
  `--confirm`/`--dry-run` gates on every physical action, and *verify-by-reread*
  (success is confirmed from the printer's own report, never from publish
  success).
- **Firmware-aware.** A capability/quirk registry keyed on `(model, firmware)`
  absorbs per-firmware API differences in one place.

## Slice → print

`bambu-rs` doesn't slice — it **delegates to Bambu Studio / OrcaSlicer's CLI**
to produce a sliced `.gcode.3mf`, then uploads and prints it. End to end:

```bash
# 1. Slice a model to .gcode.3mf (Bambu Studio / OrcaSlicer CLI)
bambu-studio --slice 1 \
  --load-settings "machine.json;process.json" \
  --load-filaments "filament.json" \
  --allow-newer-file \
  --export-3mf out.gcode.3mf  model.3mf

# 2. Upload it to the printer (FTPS)
bambu file upload out.gcode.3mf --dest /cache

# 3. Start the print (plate 1, AMS tray 0) and watch to completion
bambu job start /cache/out.gcode.3mf --plate 1 --ams-map 0 --confirm --watch
```

Full details (flags, AMS mapping, external spool, `--dry-run`) in
[docs/slicing.md](docs/slicing.md).

## Status / roadmap

LAN MVP: `status` (one-shot or `--watch` to monitor) / `info` (firmware +
resolved capabilities) / `hms`, file transfer (`file upload`/`download`/`ls`/`rm`
over FTPS), and printing a pre-sliced `.gcode.3mf` (FTPS upload → MQTT
`project_file`) with safety guards.

**Timelapse** is supported three ways: a printer-side toggle
(`bambu timelapse enable/disable`, and `job start --timelapse`), fetching
recorded videos (`bambu timelapse get`), and — for printers whose built-in
camera is missing or broken — driving an **external** camera from the print's
own layer events:

```bash
# Watch the active print; grab one frame per layer with any capture tool
bambu timelapse capture --out-dir ./tl --on-layer-cmd 'fswebcam -r 1280x720 {frame}'
```

`capture` runs the command as argv (no shell; `{frame}`/`{layer}`/`{outdir}` are
substituted), skips a failed grab and continues, and suggests an `ffmpeg` line at
the end.

Other control: `bambu speed <silent|standard|sport|ludicrous>` (verified via
`spd_lvl`), `bambu light on|off [--node chamber|work]`, `bambu gcode <line>`
(with a static safety guard — over-limit temps / cold extrusion are refused
unless `--force`), and `bambu ams <resume|reset|pause|change|set-filament|settings>`
(spec-derived, `--confirm`/`--dry-run` gated). Deeper slicer integration and the
MCP server come later.

Control requires the printer to be in **LAN-only + Developer Mode** (since the
Jan-2025 Authorization Control System).

## License

[MIT](LICENSE)
