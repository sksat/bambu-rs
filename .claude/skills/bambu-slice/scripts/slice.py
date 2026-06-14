#!/usr/bin/env python3
"""Slice an STL/3MF for the Bambu A1 mini, reliably.

OrcaSlicer's CLI does NOT resolve a profile's `inherits` chain: Bambu's system
profiles are *diffs*, so loading a leaf (e.g. "0.12mm Fine @BBL A1M") drops every
setting that lives in a parent — layer_height, speeds, temps — and they silently
fall back to Orca's built-in defaults (layer_height -> 0.2mm). That makes a naive
`--load-settings <leaf>.json` quietly wrong for any non-default setting.

This flattens the machine/process/filament chains (root->leaf merge) so the FULL
profile is applied, slices, and VERIFIES the produced layer height matches the
request (never trust the profile name).

Usage:
  scripts/slice.py model.stl out.gcode.3mf [--layer 0.20] [--filament "Bambu PLA Basic @BBL A1M"]
  scripts/slice.py model.stl out.gcode.3mf --process "0.20mm Strength @BBL A1M"
"""
import argparse, json, os, re, subprocess, sys, tempfile

PROFILES = "/opt/orca-slicer/resources/profiles/BBL"
LAYER_PROCESS = {  # A1 mini, 0.4 nozzle
    "0.08": "0.08mm Extra Fine @BBL A1M", "0.12": "0.12mm Fine @BBL A1M",
    "0.16": "0.16mm Optimal @BBL A1M", "0.20": "0.20mm Standard @BBL A1M",
    "0.24": "0.24mm Draft @BBL A1M", "0.28": "0.28mm Extra Draft @BBL A1M",
}

def flatten(subdir, leaf, overrides=None):
    chain, name, seen = [], leaf, set()
    while name and name not in seen:
        seen.add(name)
        path = os.path.join(PROFILES, subdir, name + ".json")
        if not os.path.exists(path):
            sys.exit(f"profile not found: {path}")
        d = json.load(open(path)); chain.append(d); name = d.get("inherits", "")
    merged = {}
    for d in reversed(chain):           # root first; child overrides parent
        merged.update(d)
    merged["inherits"] = ""             # resolved already — don't let Orca re-chase
    merged.pop("instantiation", None)
    if overrides:
        merged.update(overrides)
    return merged

def tmp(d):
    f = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
    json.dump(d, f); f.close(); return f.name

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("stl"); ap.add_argument("out")
    ap.add_argument("--layer", default="0.20", help="layer height mm (0.08/0.12/0.16/0.20/0.24/0.28 or any value)")
    ap.add_argument("--process", help="explicit process profile name (overrides --layer)")
    ap.add_argument("--filament", default="Bambu PLA Basic @BBL A1M")
    ap.add_argument("--machine", default="Bambu Lab A1 mini 0.4 nozzle")
    a = ap.parse_args()

    if a.process:
        proc = flatten("process", a.process)
        want = str(float(proc.get("layer_height", "0.2")))
    else:
        key = f"{float(a.layer):.2f}"
        name = LAYER_PROCESS.get(key)
        if not name:                    # non-standard height: nearest profile + force the height
            nearest = min(LAYER_PROCESS, key=lambda k: abs(float(k) - float(a.layer)))
            name = LAYER_PROCESS[nearest]
        proc = flatten("process", name, {"layer_height": a.layer})
        want = str(float(a.layer))

    mach, pf, fil = tmp(flatten("machine", a.machine)), tmp(proc), tmp(flatten("filament", a.filament))
    outdir = os.path.dirname(os.path.abspath(a.out)) or "."
    os.makedirs(outdir, exist_ok=True)
    cmd = ["orca-slicer", "--load-settings", f"{mach};{pf}", "--load-filaments", fil,
           "--arrange", "1", "--orient", "1", "--slice", "0",
           "--outputdir", outdir, "--export-3mf", os.path.basename(a.out), a.stl]
    r = subprocess.run(cmd, capture_output=True, text=True)
    for t in (mach, pf, fil):
        os.unlink(t)
    out = os.path.join(outdir, os.path.basename(a.out))
    if not os.path.exists(out):
        sys.stderr.write((r.stdout or "")[-1500:] + (r.stderr or "")[-1500:])
        sys.exit("slice failed: no output 3mf produced")
    g = subprocess.run(["unzip", "-p", out, "Metadata/plate_1.gcode"], capture_output=True, text=True).stdout
    m = re.search(r"^; layer_height = ([0-9.]+)", g, re.M)
    got = m.group(1) if m else "?"
    if got == "?" or abs(float(got) - float(want)) > 1e-6:
        sys.exit(f"VERIFY FAILED: requested {want}mm but the sliced gcode is {got}mm")
    layers = re.search(r"total layer number: (\d+)", g)
    print(f"OK {out}  layer_height={got}mm  layers={layers.group(1) if layers else '?'}  filament='{a.filament}'")

if __name__ == "__main__":
    main()
