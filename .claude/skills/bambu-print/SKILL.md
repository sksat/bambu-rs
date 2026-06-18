---
name: bambu-print
description: Start a print on the Bambu Lab A1 mini SAFELY — the part that goes wrong is the filament SOURCE. Use whenever the user wants to start/run a print from an already-sliced .gcode.3mf (upload + `job start`, via the `bambu` CLI or `bambu serve`), or when a print fails/pauses with "no source" / 0x03008015 / 0x0500C010, keeps stopping at layer 0, "isn't pulling filament", or they ask which AMS slot / use_ams / ams_map to print with. It maps the print to the source that actually holds the filament the slice needs (an AMS slot or the external spool), because the #1 footgun is starting from the EMPTY external spool while the filament sits in an AMS slot. Do NOT use it for slicing (bambu-slice) or recording a timelapse (bambu-timelapse).
metadata:
  type: reference
---

# Start a Bambu A1 mini print without no-sourcing

Slicing is **bambu-slice**'s job, timelapse is **bambu-timelapse**'s. This skill is
just: point the print at the right filament source, upload to the right place,
start, and recover from a no-source. All facts below are device-verified on a real
A1 mini (the `bambu` CLI / `bambu serve` in this repo).

The model can read a status JSON and reason fine — what trips people up is the
non-obvious stuff below, so check the source instead of assuming `use_ams=false`.

## Resolve the source (do this before every start)

Match the filament the **slice** needs (`filament_type` / `nozzle_temperature` in
the 3mf's `Metadata/project_settings.config`) to the loaded source that has it. Let
the bundled script do it — it encodes the gotchas:

```bash
bambu status --json | python3 scripts/resolve_source.py --want-3mf model.gcode.3mf
# → {use_ams, ams_map, source, ...}, or ok:false (exit 2) = nothing loaded has it; DON'T start
```

Gotchas it handles (and you must, if reading the JSON by hand):
- **Field name:** the typed status (`bambu status --json`, serve `/api/status`)
  exposes each tray's filament as **`material`** / `color` — NOT the raw report
  keys `tray_type` / `tray_color`, which return `None` and make every slot look
  empty. This is the trap that turned a 5-minute fix into hours.
- **Source ids:** `ams.active_tray` 255 = none, **254 = external spool**, 0..3 = an
  AMS tray id. `ams_map` uses the same: one entry per slice filament, value = AMS
  tray id `0..3` or **`-1`** for the external spool. AMS slots are labelled 1..4 on
  the hardware → protocol id is 0..3 (slot 3 → `ams_map=[2]`).
- **No match → refuse.** Don't invent a slot or load the wrong filament; either
  no-sources again or prints the wrong material.

## Upload + start

- Upload to the printer **root `/`**, not `/cache` (a print reading from `/cache`
  fails with `0x0500C010`). The repo's `--upload` / `upload-start` default to `/`.
- Dry-run, then confirm:
  ```bash
  bambu job start ./model.gcode.3mf --upload --use-ams --ams-map 2 --dry-run
  bambu job start ./model.gcode.3mf --upload --use-ams --ams-map 2 --confirm
  ```
- A `rejected` / `sent_unverified` outcome (with a `0x...` in the reason) means the
  print did **not** start — that's the truth, don't report success.

## If it no-sources (0x03008015)

The source mapping was wrong (or that slot ran out) — **re-resolve and remap**.
Do **not** reboot a *paused* print: power-loss recovery re-resumes it on boot, so it
re-pauses on the same no-source (the loop people get stuck in). Instead:
`bambu job stop --confirm`, then restart with the resolved `use_ams`/`ams_map`.
(`hw_switch_state = 0` in the status = nothing sensed at the selected source.)

## Error codes (so you don't misdiagnose)

| code | meaning | fix |
|---|---|---|
| `0x03008015` | no filament **source** (pointed at an empty/wrong slot or external) | re-resolve the source; map to where the filament is |
| `0x0500C010` | print file couldn't be read from storage | upload to `/`, not `/cache` |
| `0x12008015/16` | toolhead couldn't pull/push filament — a *real* feed problem | physical: check the extruder gear / path |
