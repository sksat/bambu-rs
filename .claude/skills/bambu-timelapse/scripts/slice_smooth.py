#!/usr/bin/env python3
"""Slice for a NATIVE Smooth timelapse on the A1 mini: forces the Smooth mode
(timelapse_type=1) so the slicer inserts the printer's own per-layer
time_lapse_gcode (absolute-Z lift + park + external shutter), plus a brim for the
small-part adhesion that per-layer parking strains. Flattens the profile
`inherits` chains (OrcaSlicer's CLI does not — see the bambu-slice skill) and
FAILS LOUDLY if the timelapse block didn't make it into the gcode.

Arm it at print start with timelapse:true, or the printer skips the park and the
nozzle scrapes the print:
    bambu job start --file /out.gcode.3mf --plate 1 --confirm   # (needs a timelapse flag)
    curl -X POST $B/api/job/start -d '{"file":"/out.gcode.3mf","timelapse":true,"confirm":true}'

Usage: slice_smooth.py <model> <out.gcode.3mf> [scale=1.0] [filament="Bambu PLA Matte @BBL A1M"]
"""
import json, os, re, subprocess, sys, tempfile

PROFILES = "/opt/orca-slicer/resources/profiles/BBL"

def flatten(subdir, leaf, overrides=None):
    chain, name, seen = [], leaf, set()
    while name and name not in seen:
        seen.add(name)
        path = os.path.join(PROFILES, subdir, name + ".json")
        if not os.path.exists(path):
            sys.exit(f"profile not found: {path}")
        d = json.load(open(path)); chain.append(d); name = d.get("inherits", "")
    merged = {}
    for d in reversed(chain):
        merged.update(d)
    merged["inherits"] = ""
    merged.pop("instantiation", None)
    if overrides:
        merged.update(overrides)
    return merged

def tmp(d):
    f = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
    json.dump(d, f); f.close(); return f.name

if len(sys.argv) < 3:
    sys.exit(__doc__)
stl, out = sys.argv[1], sys.argv[2]
scale = sys.argv[3] if len(sys.argv) > 3 else "1.0"
filament = sys.argv[4] if len(sys.argv) > 4 else "Bambu PLA Matte @BBL A1M"

mach = tmp(flatten("machine", "Bambu Lab A1 mini 0.4 nozzle"))
proc = tmp(flatten("process", "0.20mm Standard @BBL A1M", {
    "layer_height": "0.20",
    "curr_bed_type": "Textured PEI Plate",
    # The A1's native per-layer time_lapse_gcode is inserted in EVERY normal slice
    # (a SKIPPABLE block, runtime-gated by timelapse_record_flag) — so we do NOT
    # set timelapse_type=1. On the A1 that's the X1/P1 wipe-tower variant: it tries
    # to park at a tower position off the small 180mm bed and the slice dies with
    # "found gcode in unprintable area". Just add a brim and arm with timelapse:true.
    "brim_type": "outer_only",  # small parts need the extra first-layer grip
    "brim_width": "5",
}))
fil = tmp(flatten("filament", filament))
outdir = os.path.dirname(os.path.abspath(out)) or "."
cmd = ["orca-slicer", "--load-settings", f"{mach};{proc}", "--load-filaments", fil,
       # --enable-timelapse explicitly inlines the machine's time_lapse_gcode at
       # every layer. The block tends to appear without it too, but timelapse_type
       # is just a UI label and doesn't gate it — this flag is the real switch, so
       # set it so the per-layer park can't silently go missing.
       "--enable-timelapse",
       "--scale", scale, "--arrange", "1", "--orient", "1", "--slice", "0",
       "--outputdir", outdir, "--export-3mf", os.path.basename(out), stl]
r = subprocess.run(cmd, capture_output=True, text=True)
for t in (mach, proc, fil):
    os.unlink(t)
made = os.path.join(outdir, os.path.basename(out))
if not os.path.exists(made):
    sys.stderr.write((r.stdout or "")[-2000:] + (r.stderr or "")[-2000:]); sys.exit("slice failed")

g = subprocess.run(["unzip", "-p", made, "Metadata/plate_1.gcode"],
                   capture_output=True, text=True).stdout
layers = re.search(r"total layer number: (\d+)", g)
blocks = g.count("; SKIPTYPE: timelapse")   # the native per-layer timelapse block
shutter = g.count("M1004 S5")               # external-camera shutter, one per block
brim = "brim" in g.lower()
t = re.search(r"model printing time: ([^;]+)", g)
print(f"OK {out} layers={layers.group(1) if layers else '?'} "
      f"time={t.group(1).strip() if t else '?'} "
      f"timelapse_blocks={blocks} external_shutter={shutter} brim={brim}")
if blocks == 0:
    sys.exit("ERROR: no timelapse block in the gcode — Smooth mode didn't take; "
             "do not expect a parked timelapse from this file.")
