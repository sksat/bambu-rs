# Slicing → printing

`bambu-rs` does **not** implement a slicer. Slicing is delegated to **Bambu
Studio** or **OrcaSlicer** (an open-source fork of Bambu Studio with a
near-identical CLI), which turn a model into a sliced **`.gcode.3mf`**; `bambu`
then uploads that file and starts the print. This keeps `bambu-rs` focused on
the LAN control/monitoring path.

`bambu serve` can *drive* that slicer for you over HTTP — see
[Slicing through `bambu serve`](#slicing-through-bambu-serve) — which is where
the two traps below are handled and verified for you rather than left as
gotchas to reproduce by hand.

## 1. Slice to `.gcode.3mf` (Bambu Studio / OrcaSlicer CLI)

The slicer CLI is headless. It needs three kinds of config JSON — **machine**,
**process**, and **filament** — which you export from the slicer GUI (or take
from its profile directory):

```bash
bambu-studio \
  --slice 1 \
  --load-settings "machine.json;process.json" \
  --load-filaments "filament.json" \
  --allow-newer-file \
  --export-3mf out.gcode.3mf \
  model.3mf
```

Key flags (from the Bambu Studio CLI reference):

| Flag | Purpose |
|------|---------|
| `--slice <n>` | Slice plates; `0` = all plates, `i` = plate number `i`. |
| `--load-settings "machine.json;process.json"` | Machine first, then process. Semicolon-separated. |
| `--load-filaments "a.json;b.json"` | One filament file per slot (multi-material). |
| `--export-3mf out.gcode.3mf` | The sliced output — a `.gcode.3mf` (g-code embedded in a 3MF archive), **not** a raw `.gcode`. |
| `--allow-newer-file` | Allow a 3MF made by a newer Studio version. |
| `--outputdir <dir>` / `--arrange` / `--orient` | Output dir; auto-arrange / auto-orient. |

`OrcaSlicer` takes the same flags (substitute the `orca-slicer` binary).

> **Gotcha — flatten inherited profiles for headless CLI use.** The *bundled
> system* profiles (`<install>/resources/profiles/BBL/...`) use `"inherits"`
> chains (e.g. a PETG filament → `…@base` → `fdm_filament_pet` → common). The
> OrcaSlicer 2.3.2 CLI, given such a file by path, applies only the **leaf**'s
> keys and does **not** resolve the chain — so you silently get wrong values
> (observed: `nozzle_temperature` correct but `filament_type=PLA` and bed temp
> 45 °C instead of PETG's 70 °C, which then fails validation as
> "Cool Plate does not support filament"). Either export a *resolved* preset
> from the GUI, or flatten the chain yourself (walk `inherits`, merge
> parent→leaf) before passing it. Also set `curr_bed_type` explicitly (e.g.
> `Textured PEI Plate`) — the CLI defaults it to `Cool Plate`, which PETG can't
> use. Headless slicing also needs a display: run under `xvfb-run -a`.

> The slicer embeds, in the `.gcode.3mf`, exactly what `bambu job start` needs:
> `Metadata/plate_N.gcode`, its `.md5`, plate metadata, and the filament the
> plate was sliced for (material + temps). Print PETG with a PETG slice — a
> PLA-temp slice will not print PETG correctly.

## Slicing through `bambu serve`

`bambu serve` exposes the same slicer over HTTP, with the two traps above
handled: it flattens the `inherits` chains, merges the machine template gcodes,
and then **verifies the produced gcode** — the layer height must match the
request, and the start section must not extrude off the bed.

The slicer binary and its profile bundle are auto-detected (`orca-slicer` on
`$PATH`, else `/opt/bambustudio-bin`); override with `--slicer-bin` and
`--slicer-profiles`, and cap a run with `--slice-timeout-secs` (default 600). A
host with no slicer still serves — `GET /api/slice` just reports
`"available": false` with the reason.

```bash
B=http://HOST:8088
# Is slicing available, and what is the slot doing?
curl -s "$B/api/slice"

# Slice a model. layer / filament / bed_type are REQUIRED: they depend on your
# intent and on the spool and plate actually fitted, so the server never guesses
# (a defaulted bed_type means a 35 °C Cool Plate and a print that lifts).
curl -s -X POST --data-binary @cube.stl -H "authorization: Bearer $PW" \
  "$B/api/slice?name=cube.stl&layer=0.12&filament=Bambu%20PLA%20Basic%20@BBL%20A1M&bed_type=Textured%20PEI%20Plate"

# Poll until state is "done" (or "failed", with the reason in .job.error).
# This one is an open read, like every other status — no token needed.
curl -s "$B/api/slice" | jq .job

# Then either download it… (password-gated like the slice itself: it hands back
# a file made from geometry you uploaded, not printer state)
curl -s -o cube.gcode.3mf -H "authorization: Bearer $PW" "$B/api/slice/result"
# …or print it straight off the server, without a round trip through a browser:
# The plan reports `expect`: the id of the slice it describes. Confirming has to
# echo it back, so a slice that replaced this one between the two calls is a 409
# rather than a print of a file you never saw a plan for.
ID=$(curl -s -X POST "$B/api/slice/print" -H "authorization: Bearer $PW" \
       -H 'content-type: application/json' -d '{"dry_run":true}' | jq .plan.expect)
curl -s -X POST "$B/api/slice/print" -H "authorization: Bearer $PW" \
  -H 'content-type: application/json' \
  -d "{\"confirm\":true,\"expect\":$ID,\"use_ams\":true,\"ams_map\":[0]}"
```

`layer` is judged against the nozzle the printer reports, not a fixed window: a
0.2 nozzle cannot lay 0.4 mm and a 0.8 can, so the accepted range is derived
from the diameter. A height with no process profile near it is refused rather
than sliced on a neighbour's speeds and flow.

The **nozzle itself** must be one we have profiles for, which today means 0.4.
The BBL bundle ships a separate process set per nozzle — the unsuffixed presets
declare `"compatible_printers": ["Bambu Lab A1 mini 0.4 nozzle"]` — so a 0.6 or
0.8 is refused rather than sliced with 0.4 speeds and flow. Models the slicer
cannot read are refused up front too (`.stl`, `.obj`, `.amf` and bare-geometry
`.3mf` are in; STEP is not, and needs converting first). An **authored** 3mf —
one with slicer settings blobs, or more than one object placed in `<build>` — is
refused as well: this path slices with `--arrange 1 --orient 1`, which would
re-pack the layout its author chose.

Limits, deliberately: one slice at a time per printer (a second is `409`), one
filament, no timelapse mode, and no queue or persistence across a restart.
Only models we have **verified** a profile mapping for can be sliced — today
that is the A1 mini; any other model reports `"available": false` rather than
slicing against some other machine's bed. An **authored project `.3mf`** (one
with its own plates and per-object settings) is refused with a message: slicing
it here would re-arrange and re-orient the author's layout.

## 2. Upload and print with `bambu`

```bash
# Upload the sliced file to the printer (FTPS)
bambu file upload out.gcode.3mf --dest /cache

# Start it (plate 1), drawing filament from AMS tray 0, and watch to completion
bambu job start /cache/out.gcode.3mf --plate 1 --ams-map 0 --confirm --watch
```

- Omit `--ams-map` to print from the **external spool** instead of the AMS.
- `--ams-map` is a forward index: one tray per filament, e.g. `0,2` (filament 0
  → AMS tray 0, filament 1 → tray 2); `-1` = external spool.
- `--dry-run` (without `--confirm`) prints the resolved `project_file` payload so
  you can review plate/url/AMS mapping before committing.
- `--watch` reports live progress (state, stage, **nozzle/bed temps**) and exits
  non-zero on a device error or FAILED end.

## Multi-filament (AMS colour-switching) prints

Slice a 2-colour model with **one filament preset per AMS slot** (`;`-separated,
in filament-index order):

```bash
xvfb-run -a orca-slicer --slice 1 \
  --load-settings "machine.json;process.json" \
  --load-filaments "filament-a.json;filament-b.json" \
  --allow-newer-file --export-3mf out.gcode.3mf  model_2color.3mf
```

The plate's `Metadata/plate_N.json` then has 2 `filament_colors`; `--ams-map`
needs **one tray per filament, in that order**:

```bash
bambu file upload out.gcode.3mf --dest /cache
# Preview the mapping first — shows filament[k] (colour) → tray and warns on a
# count mismatch (downloads + inspects the on-printer 3mf):
bambu job start /cache/out.gcode.3mf --plate 1 --ams-map 0,1 --dry-run
# Then print: a wrong-length --ams-map is refused (exit 3) before anything moves.
bambu job start /cache/out.gcode.3mf --plate 1 --ams-map 0,1 --confirm --watch
```

`--ams-map` is validated — tray range `0..3`/`-1` always (fails fast, offline
too), and length == filament count once inspected; `-1` = external spool.

**Keep a test print short** — to *see* a colour switch fast, slice a tiny model
(~1 cm, few layers) with the colour change at an early layer + draft settings
(thick layers, low/0 infill) and run `bambu speed sport`; heat + a few layers +
one swap ≈ 5–10 min. Capture it with the external camera (this unit's built-in
camera is dead):

```bash
bambu timelapse capture --out-dir ./tl -- \
  curl -s -m15 -o {frame} "http://$ATOMCAM_HOST/cgi-bin/get_jpeg.cgi"
```

`timelapse capture` is layer-synced (one frame per `layer_num`), so run it **with
the print** either way — each opens its own MQTT connection (unique `client_id`),
and this A1 mini happily holds several at once:

- **Sequential:** `job start … --confirm` (returns once the print is verified
  started), then `timelapse capture …`.
- **Concurrent:** `job start … --confirm --watch` in one shell and
  `timelapse capture …` in another — both run at the same time.

## Sources
- [Bambu Studio CLI Reference (Printago)](https://printago.io/blog/bambu-studio-cli-reference)
- [Using OrcaSlicer in CLI mode (OrcaSlicer discussion #8593)](https://github.com/OrcaSlicer/OrcaSlicer/discussions/8593)
