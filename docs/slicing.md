# Slicing → printing

`bambu-rs` does **not** slice. Slicing is delegated to **Bambu Studio** or
**OrcaSlicer** (an open-source fork of Bambu Studio with a near-identical CLI),
which turn a model into a sliced **`.gcode.3mf`**. `bambu` then uploads that file
and starts the print. This keeps `bambu-rs` focused on the LAN
control/monitoring path and avoids re-implementing a slicer.

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

## Sources
- [Bambu Studio CLI Reference (Printago)](https://printago.io/blog/bambu-studio-cli-reference)
- [Using OrcaSlicer in CLI mode (OrcaSlicer discussion #8593)](https://github.com/OrcaSlicer/OrcaSlicer/discussions/8593)
