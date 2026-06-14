---
name: bambu-slice
description: Slice a 3D model (STL/STEP/3MF) into a print-ready .gcode.3mf for the Bambu Lab A1 mini, and optionally upload + start the print. ALWAYS use this for any A1-mini slicing — driving OrcaSlicer's CLI directly silently produces WRONG output (it ignores Bambu's profile inheritance and reverts layer height/speeds/temps to defaults), and this skill flattens the profiles and verifies the result. Triggers on natural asks like "slice this for my A1", "get this STL ready to print", "make cube.stl printable on the a1", "print the benchy at 0.12mm on my a1 mini", or "I exported this from Fusion, need it on the bambu a1 mini in matte PLA" — including a freshly downloaded/exported STL or STEP, and choosing a layer height (0.08–0.28mm), nozzle, or filament; no slicer need be named. Do NOT use it when nothing needs slicing: controlling a live print (pause/resume/stop), calibration/bed-leveling, jogging axes, dashboard previews/thumbnails, or sending an already-sliced .gcode.3mf to the printer.
metadata:
  type: reference
---

# Slicing for the Bambu A1 mini (OrcaSlicer CLI) → print

Verified on a real A1 mini (OrcaSlicer 2.3.2 at `/usr/bin/orca-slicer`; system
profiles under `/opt/orca-slicer/resources/profiles/BBL/{machine,process,filament}/`).

## The one trap you must know

OrcaSlicer's CLI **does not resolve a profile's `inherits` chain.** Bambu's
system profiles are *diffs* — `0.12mm Fine @BBL A1M.json` doesn't contain
`layer_height` at all; it's in a grandparent. So `--load-settings "<leaf>.json"`
loads only the leaf's handful of keys and **everything else silently falls back
to Orca's built-in defaults** (layer_height → 0.2mm, plus default speeds/temps).
The result *looks* fine and even "works" for 0.2mm by coincidence, but any other
layer height (or any tuned setting) is quietly wrong. **Never trust the profile
name — flatten the chain and verify the output.**

## Use the bundled helper (does the flatten + verify for you)

```bash
scripts/slice.py <model.stl> <out.gcode.3mf> [--layer 0.20] \
    [--filament "Bambu PLA Basic @BBL A1M"] [--process "<process profile name>"]
```

It flattens the machine/process/filament `inherits` chains, slices, and **fails
loudly if the produced layer height doesn't match the request.** Examples:

```bash
scripts/slice.py /tmp/cube.stl /tmp/cube.gcode.3mf --layer 0.12
# -> OK /tmp/cube.gcode.3mf  layer_height=0.12mm  layers=166  filament='Bambu PLA Basic @BBL A1M'
scripts/slice.py /tmp/cube.stl /tmp/cube.gcode.3mf            # 0.20mm PLA default
```

- `--layer`: 0.08 / 0.12 / 0.16 / 0.20 / 0.24 / 0.28 (or any value → nearest
  profile with the height forced). Default 0.20.
- `--filament`: any `… @BBL A1M` name, e.g. `Bambu PETG Basic @BBL A1M`,
  `Generic PLA @BBL A1M`, `Bambu PLA Matte @BBL A1M`.
- `--process`: pass an exact process profile to honor its tuning (e.g.
  `0.20mm Strength @BBL A1M`) instead of `--layer`.
- `--machine`: defaults to `Bambu Lab A1 mini 0.4 nozzle` (use `… 0.6 nozzle`
  etc. for other nozzles; then pick matching `… 0.6 nozzle` profiles).

## Manual command (fallback — same trap applies)

If you slice by hand, you MUST resolve inheritance (the helper does this) and then
verify. The lazy `--load-settings "<leaf>.json"` form is only safe for the 0.2mm
default. Always confirm the real result:

```bash
unzip -p out.gcode.3mf Metadata/plate_1.gcode | grep -m1 '; layer_height'
unzip -l out.gcode.3mf | grep Metadata/plate_1.gcode   # proves it's sliced, not just a project 3mf
```

### Headless caveat (thumbnails)
With no display the slice succeeds but logs `init opengl failed! skip thumbnail
generating` — the gcode is fine, but the 3mf has **no `Metadata/plate_*.png`** (so
a dashboard preview is blank). For a thumbnail, run under `xvfb-run -a …`.

## Upload + print

Project CLI (loads `BAMBU_*` from `./.env`):
```bash
bambu file upload out.gcode.3mf --dest /
bambu job start --file /out.gcode.3mf --plate 1 --dry-run   # preview the plan first
bambu job start --file /out.gcode.3mf --plate 1 --confirm   # real print
bambu watch --exit-status
```

Or via `bambu serve` (HTTP, e.g. the dashboard):
```bash
B=http://HOST:8088
curl -X POST "$B/api/files/upload?dir=/&name=out.gcode.3mf" --data-binary @out.gcode.3mf
curl -X POST "$B/api/job/start" -H 'content-type: application/json' -d '{"file":"/out.gcode.3mf","plate":1,"dry_run":true}'
curl -X POST "$B/api/job/start" -H 'content-type: application/json' -d '{"file":"/out.gcode.3mf","plate":1,"confirm":true}'
```

## Gotchas (observed on the real A1 mini)
- **Clear the bed** and ensure the printer is idle (`gcode_state` ∈ IDLE/FINISH)
  — `job start` refuses (409) when busy. Always `--dry-run` before a real print.
- `--export-3mf` must be a **bare filename** when combined with `--outputdir`
  (Orca concatenates them, otherwise the path doubles and export fails).
- The real settings preset is `Bambu Lab A1 mini 0.4 nozzle.json`; the bare
  `Bambu Lab A1 mini.json` is just a model descriptor, not a usable preset.
- **AMS + single colour**: a slice made without AMS, started `use_ams=false` on an
  AMS-equipped A1, paused before the first layer with `print_error 0x03008015`
  (it expects an external spool). Either load an external spool, or assign the
  filament to an AMS slot and start with `use_ams` + an `ams_map` (e.g. `[0]`).
  The printer's screen shows the plain-language cause.
