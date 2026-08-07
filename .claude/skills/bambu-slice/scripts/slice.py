#!/usr/bin/env python3
"""Slice an STL/3MF for the Bambu A1 mini, reliably.

The slicer CLI (OrcaSlicer or Bambu Studio) does NOT resolve a profile's `inherits`
chain: Bambu's system profiles are *diffs*, so loading a leaf (e.g. "0.12mm Fine
@BBL A1M") drops every setting that lives in a parent — layer_height, speeds, temps
— and they silently fall back to built-in defaults (layer_height -> 0.2mm). A naive
`--load-settings <leaf>.json` is quietly wrong for any non-default setting.

Two flavours of the same trap, both handled here:
  1. process/filament: flatten the inherits chain (root->leaf merge) so the FULL
     profile is applied. Verified against the produced layer height.
  2. machine start/end/layer gcode: on Bambu Studio these long, machine-specific
     blocks do NOT live on the inherits chain at all — only a *generic* fallback
     does (its prime line `G1 ... Y200 ... E ;Draw the first line` drives off the
     A1 mini's 180mm bed and never primes). The real blocks live in sibling
     "<machine> template <key>.json" files, merged here explicitly. If they're
     missed, the start-gcode check below fails loudly.

Usage:
  scripts/slice.py model.stl out.gcode.3mf [--layer 0.20] [--filament "Bambu PLA Basic @BBL A1M"]
  scripts/slice.py model.stl out.gcode.3mf --process "0.20mm Strength @BBL A1M"
"""
import argparse, json, os, re, shutil, subprocess, sys, tempfile

LAYER_PROCESS = {  # A1 mini, 0.4 nozzle
    "0.08": "0.08mm Extra Fine @BBL A1M", "0.12": "0.12mm Fine @BBL A1M",
    "0.16": "0.16mm Optimal @BBL A1M", "0.20": "0.20mm Standard @BBL A1M",
    "0.24": "0.24mm Draft @BBL A1M", "0.28": "0.28mm Extra Draft @BBL A1M",
}
# Long machine gcode blocks that Bambu Studio keeps in sibling "template" files,
# off the inherits chain (see module docstring). Merged into the machine profile.
TEMPLATE_GCODES = ["machine_start_gcode", "machine_end_gcode", "layer_change_gcode",
                   "change_filament_gcode", "time_lapse_gcode"]

# Set by detect_slicer() in main(): (argv-prefix, BBL-profiles-dir, subprocess-env).
BIN, PROFILES, ENV = None, None, None

def detect_slicer():
    """Prefer OrcaSlicer (the originally-verified path); fall back to Bambu Studio
    (`/opt/bambustudio-bin`), which ships the same BBL profiles + CLI flags but
    needs its bundled libs on LD_LIBRARY_PATH and LC_ALL=C (segfault workaround)."""
    orca = shutil.which("orca-slicer")
    orca_prof = "/opt/orca-slicer/resources/profiles/BBL"
    if orca and os.path.isdir(orca_prof):
        return [orca], orca_prof, dict(os.environ)
    bs = "/opt/bambustudio-bin"
    bs_bin = os.path.join(bs, "bin", "bambu-studio")
    bs_prof = os.path.join(bs, "resources", "profiles", "BBL")
    if os.path.exists(bs_bin) and os.path.isdir(bs_prof):
        env = dict(os.environ)
        env["LD_LIBRARY_PATH"] = os.path.join(bs, "bin")
        env["LC_ALL"] = "C"
        return [bs_bin], bs_prof, env
    sys.exit("no slicer found: need `orca-slicer` (+ /opt/orca-slicer profiles) or /opt/bambustudio-bin")

def merge_machine_templates(merged, machine_name):
    for key in TEMPLATE_GCODES:
        path = os.path.join(PROFILES, "machine", f"{machine_name} template {key}.json")
        if os.path.exists(path):
            d = json.load(open(path))
            if key in d:
                merged[key] = d[key]

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
    merged["inherits"] = ""             # resolved already — don't let the slicer re-chase
    merged.pop("instantiation", None)
    if subdir == "machine":             # pull in the real start/end/layer gcode
        merge_machine_templates(merged, leaf)
    if overrides:
        merged.update(overrides)
    return merged

def bed_bounds(mach_prof):
    xs, ys = [], []
    for p in mach_prof.get("printable_area") or []:
        try:
            x, y = p.split("x"); xs.append(float(x)); ys.append(float(y))
        except ValueError:
            pass
    return (max(xs) if xs else 0.0), (max(ys) if ys else 0.0)

def start_gcode_fault(g, max_x, max_y):
    """Detect the missing-template trap in the output: a generic fallback start
    gcode extrudes off the bed (its `;Draw the first line` prime hits Y200 on a
    180mm A1 mini). The real start's out-of-bed wipe moves (Y185) carry NO
    extrusion, so 'extruding move past the printable area' is a clean signal."""
    start = g.split("; CHANGE_LAYER", 1)[0]        # everything before the first layer
    if "Draw the first line" in start:
        return "generic fallback start gcode ('Draw the first line') — machine_start_gcode not merged"
    if max_x and max_y:
        for ln in start.splitlines():
            s = ln.strip()
            if not s.startswith(("G0 ", "G1 ")):
                continue
            e = re.search(r"\bE(-?[0-9.]+)", s)
            if not e or float(e.group(1)) <= 0:    # only extruding moves matter
                continue
            # Both bounds, not just the maximum: off the bed is off the bed, and
            # a negative prime (G1 X-5 E1) is as much a crash as one past Y200.
            # 1mm of slack absorbs the profile's own edge-of-bed rounding.
            xm = re.search(r"\bX(-?[0-9.]+)", s); ym = re.search(r"\bY(-?[0-9.]+)", s)
            off = ((xm and not (-1 <= float(xm.group(1)) <= max_x + 1))
                   or (ym and not (-1 <= float(ym.group(1)) <= max_y + 1)))
            if off:
                return f"start gcode extrudes off the bed: {s[:60]!r} — real machine_start_gcode missing"
    return None

def tmp(d):
    f = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
    json.dump(d, f); f.close(); return f.name

def main():
    global BIN, PROFILES, ENV
    ap = argparse.ArgumentParser()
    ap.add_argument("stl"); ap.add_argument("out")
    ap.add_argument("--layer", default="0.20", help="layer height mm (0.08/0.12/0.16/0.20/0.24/0.28 or any value)")
    ap.add_argument("--process", help="explicit process profile name (overrides --layer)")
    ap.add_argument("--filament", default="Bambu PLA Basic @BBL A1M")
    ap.add_argument("--machine", default="Bambu Lab A1 mini 0.4 nozzle")
    # A1 mini ships a Textured PEI plate (PLA->65C, PETG->hotter). Without this the
    # merge defaults to "Cool Plate" (35C) -> poor adhesion / warping.
    ap.add_argument("--bed-type", default="Textured PEI Plate")
    ap.add_argument("--brim", help="force an outer brim of this width in mm (e.g. 5)")
    a = ap.parse_args()

    BIN, PROFILES, ENV = detect_slicer()

    # Slicer config values are strings — a bare JSON number is dropped to default.
    bed_ov = {"curr_bed_type": a.bed_type}
    if a.brim is not None:
        bed_ov.update({"brim_type": "outer_only", "brim_width": str(a.brim)})

    if a.process:
        proc = flatten("process", a.process, bed_ov)
        want = str(float(proc.get("layer_height", "0.2")))
    else:
        key = f"{float(a.layer):.2f}"
        name = LAYER_PROCESS.get(key)
        if not name:                    # non-standard height: nearest profile + force the height
            nearest = min(LAYER_PROCESS, key=lambda k: abs(float(k) - float(a.layer)))
            name = LAYER_PROCESS[nearest]
        proc = flatten("process", name, {"layer_height": a.layer, **bed_ov})
        want = str(float(a.layer))

    mach_prof = flatten("machine", a.machine)
    max_x, max_y = bed_bounds(mach_prof)
    mach, pf, fil = tmp(mach_prof), tmp(proc), tmp(flatten("filament", a.filament))
    outdir = os.path.dirname(os.path.abspath(a.out)) or "."
    os.makedirs(outdir, exist_ok=True)
    cmd = BIN + ["--load-settings", f"{mach};{pf}", "--load-filaments", fil,
                 "--arrange", "1", "--orient", "1", "--slice", "0",
                 "--outputdir", outdir, "--export-3mf", os.path.basename(a.out), a.stl]
    out = os.path.join(outdir, os.path.basename(a.out))
    # Refuse to write over the input. Slicing a .3mf in place is an easy typo,
    # and the cleanup below would delete the source before the slicer ever
    # opened it. samefile() catches hard links and symlinks too; it needs both
    # to exist, so fall back to comparing resolved paths.
    if os.path.exists(a.stl) and os.path.exists(out) and os.path.samefile(a.stl, out):
        sys.exit(f"refusing to overwrite the input: {a.stl} and {out} are the same file")
    if os.path.realpath(a.stl) == os.path.realpath(out):
        sys.exit(f"refusing to overwrite the input: {a.stl} and {out} resolve to the same path")
    # Clear any earlier archive FIRST. The checks below all read `out`, so a
    # stale file from a previous run would let a failed slice verify green and
    # ship settings that have nothing to do with this invocation.
    if os.path.exists(out):
        os.unlink(out)
    r = subprocess.run(cmd, capture_output=True, text=True, env=ENV)
    for t in (mach, pf, fil):
        os.unlink(t)
    # The slicer reports failure by exit status; it also prints config errors and
    # keeps going far enough to look busy. Observed: a project 3mf whose settings
    # it rejects exits non-zero having written nothing.
    if r.returncode != 0:
        sys.stderr.write((r.stdout or "")[-1500:] + (r.stderr or "")[-1500:])
        sys.exit(f"slice failed: {BIN[0]} exited {r.returncode}")
    if not os.path.exists(out):
        sys.stderr.write((r.stdout or "")[-1500:] + (r.stderr or "")[-1500:])
        sys.exit("slice failed: no output 3mf produced")
    g = subprocess.run(["unzip", "-p", out, "Metadata/plate_1.gcode"], capture_output=True, text=True).stdout
    m = re.search(r"^; layer_height = ([0-9.]+)", g, re.M)
    got = m.group(1) if m else "?"
    if got == "?" or abs(float(got) - float(want)) > 1e-6:
        sys.exit(f"VERIFY FAILED: requested {want}mm but the sliced gcode is {got}mm")
    fault = start_gcode_fault(g, max_x, max_y)
    if fault:
        sys.exit(f"VERIFY FAILED (start gcode): {fault}")
    layers = re.search(r"total layer number: (\d+)", g)
    bed = re.search(r"^M190 S([0-9]+)", g, re.M)
    print(f"OK {out}  layer_height={got}mm  layers={layers.group(1) if layers else '?'}  "
          f"filament='{a.filament}'  bed={bed.group(1) if bed else '?'}C  slicer={os.path.basename(BIN[0])}")

if __name__ == "__main__":
    main()
