---
name: bambu-slice
description: Slice a 3D model (STL/STEP/3MF) into a print-ready .gcode.3mf for the Bambu Lab A1 mini, and optionally upload + start the print. ALWAYS use this for any A1-mini slicing — driving OrcaSlicer's CLI directly silently produces WRONG output (it ignores Bambu's profile inheritance and reverts layer height/speeds/temps to defaults), and this skill flattens the profiles and verifies the result. Triggers on natural asks like "slice this for my A1", "get this STL ready to print", "make cube.stl printable on the a1", "print the benchy at 0.12mm on my a1 mini", or "I exported this from Fusion, need it on the bambu a1 mini in matte PLA" — including a freshly downloaded/exported STL or STEP, and choosing a layer height (0.08–0.28mm), nozzle, or filament; no slicer need be named. Also covers slicing an EXISTING project .3mf (a purchased or Bambu-Studio-authored file with its own plates and per-object settings) — that path deliberately does NOT use the bundled helper, which would re-arrange and re-orient the author's layout. Do NOT use it when nothing needs slicing: controlling a live print (pause/resume/stop), calibration/bed-leveling, jogging axes, dashboard previews/thumbnails, or sending an already-sliced .gcode.3mf to the printer.
metadata:
  type: reference
---

# Slicing for the Bambu A1 mini (OrcaSlicer / Bambu Studio CLI) → print

Verified on a real A1 mini (OrcaSlicer 2.3.2 at `/usr/bin/orca-slicer`; system
profiles under `/opt/orca-slicer/resources/profiles/BBL/{machine,process,filament}/`).

**Slicer auto-detect:** the helper prefers OrcaSlicer, and falls back to **Bambu
Studio** (`/opt/bambustudio-bin`) when OrcaSlicer isn't installed — same BBL
profiles + CLI flags, but it needs its bundled libs on `LD_LIBRARY_PATH` and
`LC_ALL=C` (the helper sets both).

## The one trap you must know

OrcaSlicer's CLI **does not resolve a profile's `inherits` chain.** Bambu's
system profiles are *diffs* — `0.12mm Fine @BBL A1M.json` doesn't contain
`layer_height` at all; it's in a grandparent. So `--load-settings "<leaf>.json"`
loads only the leaf's handful of keys and **everything else silently falls back
to Orca's built-in defaults** (layer_height → 0.2mm, plus default speeds/temps).
The result *looks* fine and even "works" for 0.2mm by coincidence, but any other
layer height (or any tuned setting) is quietly wrong. **Never trust the profile
name — flatten the chain and verify the output.**

## The second trap (cost a failed print): machine start gcode isn't on the inherits chain

On **Bambu Studio**, the A1 mini's long machine gcode blocks — `machine_start_gcode`
(≈10 KB of heat/wipe/**bed-mesh**/flow-cali/**nozzle-load**), plus `machine_end_gcode`,
`layer_change_gcode`, `change_filament_gcode`, `time_lapse_gcode` — do **not** live on
the machine profile's `inherits` chain (`… 0.4 nozzle` → `fdm_bbl_3dp_001_common` →
`fdm_machine_common`). The chain carries only a **generic fallback** start gcode. The
real blocks sit in sibling files `<machine> template <key>.json` (e.g. `Bambu Lab A1
mini 0.4 nozzle template machine_start_gcode.json`), which the GUI merges but a plain
inherits-walk misses.

Miss them and the slicer emits the generic start, whose prime line is
`G1 X10.1 Y200.0 … E15 ;Draw the first line` — **Y200 is 20 mm past the 180 mm bed**,
so the head slams the Y limit (loud thud, lost steps, "weird from the start") and the
generic prime **skips Bambu's real nozzle-load + flow-cali → under-extrusion, "filament
won't come out."** Symptom on the real A1: thudding + no filament from layer 0, print
manually stopped. `print_error` stays 0 (a manual stop, not a firmware fault).

The helper now **merges those template gcodes into the machine profile** and, after
slicing, **fails loudly if the start section extrudes past the printable area** (the
real start's out-of-bed wipe moves carry no extrusion, so this is a clean signal). It
also sets `curr_bed_type` (default **Textured PEI Plate**) — otherwise the merge
defaults to **Cool Plate → bed 35 °C**, far too cold for good adhesion (PLA wants 65 °C,
PETG 70 °C on textured PEI).

## Use the bundled helper (does the flatten + verify for you)

**For a bare model — STL/STEP, or a 3mf that is just geometry.** If the 3mf is a
*project* with its own plates and per-object settings, skip to
[Slicing an EXISTING project 3mf](#slicing-an-existing-project-3mf--do-not-use-the-helper);
the helper re-arranges it.

```bash
scripts/slice.py <model.stl> <out.gcode.3mf> [--layer 0.20] \
    [--filament "Bambu PLA Basic @BBL A1M"] [--process "<process profile name>"]
```

It flattens the machine/process/filament `inherits` chains, merges the machine
template gcodes, slices, and **fails loudly if the produced layer height doesn't
match the request, or if the start gcode extrudes off the bed** (the second trap).
Examples:

```bash
scripts/slice.py /tmp/cube.stl /tmp/cube.gcode.3mf --layer 0.12
# -> OK /tmp/cube.gcode.3mf  layer_height=0.12mm  layers=166  filament='…'  bed=65C  slicer=bambu-studio
scripts/slice.py /tmp/cube.stl /tmp/cube.gcode.3mf            # 0.20mm PLA default
```

- `--layer`: 0.08 / 0.12 / 0.16 / 0.20 / 0.24 / 0.28 (or any value → nearest
  profile with the height forced). Default 0.20.
- `--filament`: any `… @BBL A1M` name, e.g. `Bambu PETG Basic @BBL A1M`,
  `Bambu PETG Translucent @BBL A1M`, `Generic PLA @BBL A1M`, `Bambu PLA Matte @BBL A1M`.
- `--process`: pass an exact process profile to honor its tuning (e.g.
  `0.20mm Strength @BBL A1M`) instead of `--layer`.
- `--machine`: defaults to `Bambu Lab A1 mini 0.4 nozzle` (use `… 0.6 nozzle`
  etc. for other nozzles; then pick matching `… 0.6 nozzle` profiles).
- `--bed-type`: default `Textured PEI Plate` (A1 mini stock). Drives the bed temp
  from the filament profile — leave it unless you actually swapped plates.
- `--brim`: force an outer brim of N mm (e.g. `--brim 5`) for thin/tall-and-narrow
  parts prone to lifting. PLA benefits; PETG adheres hard enough that a brim is
  usually unneeded and harder to peel.
- `--support` / `--support-type`: generate support. Anything resting on a point
  or with a real overhang needs it — a sphere printed without it has nothing to
  build its first layers on. The A1M presets already choose `tree(auto)` and a
  30° threshold, so `--support` alone is usually the whole decision;
  `--support-type` overrides the style. **Support material counts toward the
  weight the slicer reports**, so re-check the weight after enabling it.
- `--infill`: sparse infill density in percent. The knob for **how much filament
  the part uses** — a solid part is the size's upper bound and nothing else moves
  weight nearly as much. Read the result back rather than predicting it:

  ```bash
  unzip -p out.gcode.3mf Metadata/slice_info.config \
    | grep -oE 'key="(weight|prediction)" value="[^"]+"'
  ```

  Weight is close to linear in density, so two slices bracket any target. A 46mm
  ball came out 28.15 g at 40% and 61.11 g at 100%; the interpolated 82% gave
  49.38 g, one slice later.

For a headless thumbnail run the helper under `xvfb-run -a …` (see the caveat below;
on some boxes GL still fails and the 3mf ships without a preview — the gcode is fine).

## Manual command (fallback — BOTH traps apply)

**Use the helper.** Slicing by hand means reproducing both traps yourself: flatten
the `inherits` chain *and* merge the `<machine> template <key>.json` gcodes. Neither
is optional, and neither depends on the layer height.

An earlier version of this section said `--load-settings "<leaf>.json"` was "safe for
the 0.2mm default". **That is wrong** — it only ever addressed the first trap. The
leaf also lacks the machine gcode templates, so on Bambu Studio that shortcut still
emits the generic start whose prime line drives 20mm off the bed. A 0.2mm slice made
that way looks correct in every check below and still crashes the head.

If you slice by hand anyway, verify the real result — including the start gcode:

```bash
unzip -p out.gcode.3mf Metadata/plate_1.gcode | grep -m1 '; layer_height'
unzip -l out.gcode.3mf | grep Metadata/plate_1.gcode   # proves it's sliced, not just a project 3mf
# the second trap: a generic start gcode gives itself away here
unzip -p out.gcode.3mf Metadata/plate_1.gcode | grep -c 'Draw the first line'   # must be 0
```

Both slicers write a numbered log (`00000.log`) into their **working directory**,
so a hand-run slice drops one wherever you happened to be — `cd` to a scratch
directory first, or expect to clean up. (The helper above already runs the slicer
in a temp directory and reads the log back only if the slice fails.)

### Headless caveat (thumbnails)
With no display the slice succeeds but logs `init opengl failed! skip thumbnail
generating` — the gcode is fine, but the 3mf has **no `Metadata/plate_*.png`** (so
a dashboard preview is blank). For a thumbnail, run under `xvfb-run -a …`.

## Slicing an EXISTING project 3mf — do NOT use the helper

A `.3mf` you were given (a purchased model, a Bambu Studio project) already
carries plate layout, per-object orientation, and per-object setting overrides.
**The helper would destroy all of it**: it passes `--arrange 1 --orient 1`,
which re-packs and re-orients the objects, and it builds settings from system
profiles instead of the ones inside the file.

Observed cost: a part whose author set `brim_type = brim_ears` per-object (with
matching `Metadata/brim_ear_points.txt`) got a plain full-width brim instead —
48% more first-layer extrusion, visibly not the designed part.

### First: does the project even target this printer?

The file carries the machine it was authored for. Slicing it unchanged on a
mismatch emits gcode for **that** printer — and every bed-related check below,
including `outside="false"`, is then measured against that machine's bed, not
the A1 mini's 180mm one. Check before anything else:

```bash
unzip -p project.3mf Metadata/project_settings.config \
  | python3 -c 'import json,sys; c=json.load(sys.stdin); print(c["printer_model"], c["nozzle_diameter"])'
# want: Bambu Lab A1 mini  ['0.4']
```

Anything else — stop. Converting it is a deliberate act (re-picking the machine
profile means the layout may no longer fit), not something to do in passing.

### Then: slice the project's own plates

**Prefer Bambu Studio if it is installed.** A BS-authored project is the format
BS wrote, so it opens as-is; Orca rejects most of them (next section). Only fall
back to Orca when BS is absent.

```bash
# Bambu Studio (needs its bundled libs + C locale)
LD_LIBRARY_PATH=/opt/bambustudio-bin/bin LC_ALL=C \
  /opt/bambustudio-bin/bin/bambu-studio --allow-newer-file --slice N \
    --outputdir "$PWD" --export-3mf out.gcode.3mf project.3mf

# OrcaSlicer (same flags; see the -18 caveat below)
orca-slicer --allow-newer-file --slice N \
    --outputdir "$PWD" --export-3mf out.gcode.3mf project.3mf
```

`--slice N` is the **plate number** (1-based; `0` = all plates). No
`--load-settings`, no `--arrange`, no `--orient` — everything comes from the file.
`--outputdir` and the verification commands below must agree on one directory;
they are written for `$PWD`.

### Orca refuses most Bambu Studio projects (`return -18`)

Bambu Studio writes `-1` for "auto" in some fields; Orca range-checks them and
bails before slicing:

```
Param values in 3mf/config error:
  raft_first_layer_expansion: -1 not in range [0.000000, ...]
  tree_support_wall_count:    -1 not in range [0.000000,2.000000]
run found error, return -18, exit...
```

Nothing in that message says the values are unused or how to proceed. **If Bambu
Studio is available, use it instead — no rewriting needed.** Otherwise rewrite
just those keys **in a copy**, never the original:

```python
import json, zipfile
SRC, DST = "project.3mf", "project-orca.3mf"
FIX = {"raft_first_layer_expansion": "2", "tree_support_wall_count": "0"}
with zipfile.ZipFile(SRC) as zin, zipfile.ZipFile(DST, "w", zipfile.ZIP_DEFLATED) as zout:
    for item in zin.infolist():
        data = zin.read(item.filename)
        if item.filename == "Metadata/project_settings.config":
            cfg = json.loads(data); cfg.update(FIX)
            data = json.dumps(cfg, indent=4).encode()
        zout.writestr(item, data)      # copy every other entry byte-for-byte
```

**Check they are actually unused before touching them** — `enable_support` and
`raft_layers` both `0`, and no per-object override in
`Metadata/model_settings.config`. If support or a raft IS enabled, these values
change the print and the substitute must be chosen deliberately.

### Verify a project slice

The per-plate checks below still apply (`Metadata/plate_N.gcode`, layer height,
no off-bed extrusion in the start gcode). Two more are worth reading:

```bash
unzip -p out.gcode.3mf Metadata/slice_info.config | grep -E 'weight|prediction|outside'
```

`outside="false"` proves nothing hangs off the bed.

To confirm *which* objects the plate really contains, count the distinct label
ids in the gcode — `start printing object` is emitted **once per layer per
object**, so a plain `grep -c` counts layers, not parts:

```bash
unzip -p out.gcode.3mf Metadata/plate_N.gcode \
  | grep -oE 'unique label id: [0-9]+' | sort -u
```

Cross-check against `slice_info.config`'s `identify_id=` values. They normally
agree — but if objects were excluded by flipping `printable="0"` in
`3D/3dmodel.model`, `slice_info` still lists them while the gcode does not.
The gcode is the truth.

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
- **AMS + single colour (verified on the real A1 mini)**: starting a single-filament
  slice with `use_ams=false` on an **AMS-equipped** A1 fails before the first layer
  with `print_error 0x03008015` — with no external spool, the printer has no source.
  **Verified fix:** start with `use_ams` + an `ams_map` mapping the filament to a
  *loaded* AMS slot, e.g. `bambu job start … --ams-map "0"` (or via the serve API,
  `{"use_ams":true,"ams_map":[0]}`). A 12-layer test coin then ran start→FINISH with
  `print_error=0`. (`-1` in the map = external spool, if you load one and prefer that.)
  Always `--dry-run` first and watch the first layer; the printer screen also shows
  the plain-language cause.

## Timelapse → use the `bambu-timelapse` skill

For a smooth per-layer timelapse (external camera, head parked out of the way each
layer), use the **bambu-timelapse** skill — it has the verified recipe and a slice
helper. Short version: use the A1's **native** `time_lapse_gcode` (a firmware-safe
absolute-Z lift + park) and arm it by starting the print with `timelapse: true`;
do **not** hand-roll a custom `layer_change_gcode` park.

> An earlier version of this section recommended a custom park with a **relative**
> `G91`/`G1 Z0.3`/`G90` hop. That was **disproven on the real A1**: the firmware
> mishandles relative Z in layer-change gcode and flattens every layer to the hop
> height (a flat plate), and a no-hop park scrapes the print and detaches it
> mid-print. The native path in bambu-timelapse avoids both.
