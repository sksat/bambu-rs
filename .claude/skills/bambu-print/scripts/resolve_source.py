#!/usr/bin/env python3
"""Resolve which filament source to print from on a Bambu A1 mini — the step that,
skipped, cost a whole debugging session: we kept starting prints from the EMPTY
external spool (use_ams=false) while the PETG sat in an AMS slot, so every print
died with 0x03008015 "no source".

Given the printer's status JSON (from `bambu status --json` or the serve's
GET /api/status) and the filament the SLICE needs, it finds the source — an AMS
tray or the external spool — actually loaded with that filament, and prints the
use_ams / ams_map to start with. If nothing matches it says so (don't start;
load the filament) instead of guessing.

Usage:
  resolve_source.py --status status.json --want PETG [--nozzle 245]
  bambu status --json | resolve_source.py --want PETG
  # --want from a sliced 3mf's project_settings.config:
  resolve_source.py --status status.json --want-3mf model.gcode.3mf

Key facts (device-verified on a real A1 mini):
- The typed status (GET /api/status, `bambu status --json`) exposes each tray's
  filament as `material` and `color` — NOT the raw report keys `tray_type` /
  `tray_color`. Reading the raw keys returns None and hides the filament.
- ams.active_tray / target_tray: 255 = none, 254 = the external spool, 0..3 = an
  AMS tray id. ams_map values use the same 0..3 (hardware labels trays 1..4).
- ams_map is a forward index: one entry per slice filament, value = AMS tray id
  (0..3) or -1 for the external spool. Single-filament slice -> a 1-element list.
"""
import argparse
import json
import re
import subprocess
import sys
import zipfile


def load_status(path):
    data = sys.stdin.read() if path == "-" else open(path).read()
    return json.loads(data)


def want_from_3mf(path):
    """Pull the required filament type (and first nozzle temp) from a sliced 3mf's
    Metadata/project_settings.config."""
    with zipfile.ZipFile(path) as z:
        cfg = json.loads(z.read("Metadata/project_settings.config"))
    ftypes = cfg.get("filament_type") or []
    temps = cfg.get("nozzle_temperature") or []
    want = ftypes[0] if ftypes else None
    nozzle = int(temps[0]) if temps else None
    return want, nozzle


def sources(status):
    """All loaded sources as (label, ams_map_value, material, temp_min, temp_max).
    ams_map_value is the AMS tray id (0..3) or -1 for the external spool."""
    ams = status.get("ams") or {}
    out = []
    ext = ams.get("external") or {}
    if ext.get("material"):
        out.append(("external spool", -1, ext.get("material"),
                    ext.get("nozzle_temp_min"), ext.get("nozzle_temp_max")))
    for unit in ams.get("units", []):
        for t in unit.get("trays", []):
            mat = t.get("material")
            if not mat:
                continue
            tid = int(t.get("id", -99))
            out.append((f"AMS slot {tid + 1} (tray id {tid})", tid, mat,
                        t.get("nozzle_temp_min"), t.get("nozzle_temp_max")))
    return out


def material_matches(want, have):
    """PETG matches PETG / 'PETG Translucent' / 'PETG Basic'; case-insensitive,
    family-level (the slice's temp is what actually prints, sub-brand differs)."""
    want = (want or "").strip().upper()
    have = (have or "").strip().upper()
    if not want or not have:
        return False
    # compare the leading material token (PETG, PLA, ABS, ASA, TPU, PA, PC, ...)
    wt = re.split(r"[ _-]", want)[0]
    ht = re.split(r"[ _-]", have)[0]
    return wt == ht or have.startswith(wt) or want.startswith(ht)


def temp_ok(nozzle, tmin, tmax):
    if nozzle is None or tmin is None or tmax is None:
        return True  # unknown range -> don't block on it
    return tmin <= nozzle <= tmax


def resolve(status, want, nozzle):
    matches = [s for s in sources(status) if material_matches(want, s[2])]
    warnings = []
    # Prefer a temp-compatible match if any are.
    temp_ok_matches = [m for m in matches if temp_ok(nozzle, m[3], m[4])]
    chosen_pool = temp_ok_matches or matches
    if matches and not temp_ok_matches:
        warnings.append(
            f"the matching source's nozzle range doesn't cover {nozzle}C; check it")

    if not chosen_pool:
        loaded = ", ".join(f"{lbl}={mat}" for lbl, _, mat, *_ in sources(status)) or "nothing"
        return {
            "ok": False,
            "reason": f"no loaded source has {want!r}. Loaded: {loaded}. "
                      f"Load {want} into a slot (or the external spool) before printing — "
                      f"do NOT start, it will fail with 0x03008015 'no source'.",
            "warnings": warnings,
        }
    if len(chosen_pool) > 1:
        opts = "; ".join(f"{lbl} ({mat})" for lbl, _, mat, *_ in chosen_pool)
        warnings.append(f"multiple sources have {want}: {opts}. Picking the first; "
                        f"confirm with the user if it matters.")

    label, mapval, mat, *_ = chosen_pool[0]
    use_ams = mapval != -1
    return {
        "ok": True,
        "source": label,
        "material": mat,
        "use_ams": use_ams,
        # single-filament slice -> one entry. Multi-filament needs one per filament.
        "ams_map": [mapval] if use_ams else [-1],
        "reason": f"{want} is loaded in {label}; start with "
                  + (f"use_ams=true ams_map=[{mapval}]" if use_ams
                     else "use_ams=false (external spool)"),
        "warnings": warnings,
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--status", default="-", help="status JSON file, or - for stdin")
    ap.add_argument("--want", help="required filament family, e.g. PETG")
    ap.add_argument("--want-3mf", help="read the required filament from a sliced 3mf")
    ap.add_argument("--nozzle", type=int, help="slice nozzle temp (C), for the range check")
    args = ap.parse_args()

    want, nozzle = args.want, args.nozzle
    if args.want_3mf:
        w, n = want_from_3mf(args.want_3mf)
        want = want or w
        nozzle = nozzle or n
    if not want:
        ap.error("need --want or --want-3mf")

    status = load_status(args.status)
    result = resolve(status, want, nozzle)
    print(json.dumps(result, indent=2, ensure_ascii=False))
    sys.exit(0 if result["ok"] else 2)


if __name__ == "__main__":
    main()
