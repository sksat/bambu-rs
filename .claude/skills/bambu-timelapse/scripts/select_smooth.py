#!/usr/bin/env python3
"""Pick the parked ("head out of the way, object visible") frame from each layer's
capture BURST of a Bambu A1 mini smooth timelapse, and optionally assemble the
selected frames into an mp4.

Why a burst + selection: `bambu serve` grabs several frames per layer at different
delays after the MQTT layer edge (`frame_<n>_layer_<L>_t<offset>.jpg`), because the
native park's "object-clear" moment lands at a wildly variable delay — device-measured
~300-1700ms, jittering layer-to-layer and drifting with print height — that no single
fixed delay reliably catches. This script picks, per layer, the burst frame where the
toolhead is parked at the far-left with the print exposed.

How: it isolates the transient toolhead by subtracting the per-burst MEDIAN image
(static object/bed/printer cancel out, including dark fixtures), then measures the
head's dark mass in the LEFT park zone. The park frame's left-mass is a strong
OUTLIER vs the burst median; the sharpest such frame is chosen. If no frame shows
that outlier (the park fell outside the burst), the layer is SKIPPED rather than
emitting a head-over-print frame — a bad frame is worse than a gap.

stdlib only: JPEGs are decoded to tiny raw grayscale via ffmpeg (already required by
the rest of the timelapse tooling); all scoring is pure Python. No numpy/Pillow.

Heuristics have NO code defaults (camera/printer placement is setup-specific): pass them
via --config (copy tuning.example.json) and/or --<knob> overrides; select needs only its
own knobs (left_frac, min_outlier, min_left_density, min_confidence, select_candidate_frac).

Usage:
  select_smooth.py <capture_dir>/<cam-id> --config tuning.json [--report scores.json]
  select_smooth.py captures/<run>_smooth/ext-1 --config tuning.json --out selected --assemble out.mp4 --out-fps 12
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from statistics import median

DECODE_W, DECODE_H = 64, 36  # small grayscale for fast pure-Python scoring
FRAME_RE = re.compile(r"frame_(\d+)_layer_(\d+)_t(\d+)\.jpg$")


# ── pure scoring core (no IO) ───────────────────────────────────────────────────
# A "frame" is {"offset_ms": int, "gray": [int]*(w*h)}; w,h passed alongside.

def _median_image(grays):
    """Per-pixel median across the burst — the static bed/object/background; the
    moving toolhead, present at any spot in only a frame or two, is washed out."""
    cols = zip(*grays)
    return [median(col) for col in cols]


def _dark_saliency(gray, med):
    """How much darker than the burst median each pixel is — isolates the dark
    toolhead/gantry that differs from the static scene; ~0 on bed/object."""
    return [m - g if m > g else 0 for g, m in zip(gray, med)]


def _sharpness(gray, w, h):
    """Gradient energy (Tenengrad-ish): high for a settled/in-focus frame, low for
    a motion-blurred travel frame. Sum of squared neighbour differences."""
    s = 0
    for y in range(h):
        row = y * w
        for x in range(w - 1):
            d = gray[row + x] - gray[row + x + 1]
            s += d * d
    for y in range(h - 1):
        row = y * w
        nxt = row + w
        for x in range(w):
            d = gray[row + x] - gray[nxt + x]
            s += d * d
    return s / (w * h)


def _centroid_x(dark, w, h):
    num = den = 0
    for y in range(h):
        row = y * w
        for x in range(w):
            v = dark[row + x]
            num += x * v
            den += v
    return (num / den) if den else (w / 2.0)


def _norm(v, lo, hi):
    return 0.0 if hi <= lo else (v - lo) / (hi - lo)


def select_frame(frames, w, h, cfg):
    """Pick the parked frame from one layer's burst, or skip. Pure: `frames` is a
    list of {"offset_ms", "gray"} already decoded to grayscale. Returns a dict with
    decision ("selected"/"skip"), selected_offset_ms, confidence, reason, per_frame.

    Signal (validated against device frames): isolate the transient toolhead by
    subtracting the per-burst MEDIAN (static object/bed/printer/dark-fixtures cancel
    out), then measure its dark mass in the LEFT park zone (left `left_frac` of the
    frame — where the X-min park appears for a left-mounted camera). The park frame's
    left-mass is a strong OUTLIER vs the burst median; the sharpest such frame wins.
    A layer whose park fell outside the burst shows no outlier and is SKIPPED
    (a head-over-print frame would be worse than a gap).

    Every knob comes from `cfg` (see _tuning.py) — NO baked defaults, since they depend
    on the camera/printer placement; a missing one is a KeyError surfaced loudly by the
    caller's resolve_tuning rather than a silent stale value."""
    left_frac = cfg["left_frac"]                      # park zone = left this fraction
    min_outlier = cfg["min_outlier"]                  # park left-mass vs burst median
    min_left_density = cfg["min_left_density"]        # mean park-zone darkness (0-255)
    cand_frac = cfg["select_candidate_frac"]          # sharpest among frames >= this * max
    min_conf = cfg["min_confidence"]

    grays = [f["gray"] for f in frames]
    med = _median_image(grays)
    darks = [_dark_saliency(g, med) for g in grays]

    # Park zone = the left LEFT_FRAC of the frame: the X-min park maps there for a
    # left-mounted camera (configurable for other framings). Static dark fixtures in
    # the strip cancel via the median subtraction, so only the transient parked head
    # — anomalously present at the left — scores here.
    park_hi = max(1, int(left_frac * w))

    def left_mass(d):
        return sum(d[y * w + x] for y in range(h) for x in range(park_hi))

    L = [left_mass(d) for d in darks]
    cx = [_centroid_x(d, w, h) for d in darks]
    sharp = [_sharpness(g, w, h) for g in grays]

    L_med = median(L) or 1.0
    L_max = max(L)
    L_lo, L_hi = min(L), max(L)
    sh_lo, sh_hi = min(sharp), max(sharp)

    per = []
    for i, f in enumerate(frames):
        per.append({
            "offset_ms": f["offset_ms"], "left_mass": round(L[i], 1),
            "left_rank": round(_norm(L[i], L_lo, L_hi), 3),
            "sharp_rank": round(_norm(sharp[i], sh_lo, sh_hi), 3),
            "centroid_x": round(cx[i], 2), "sharpness": round(sharp[i], 1),
        })

    def skip(reason, conf=0.0):
        return {"decision": "skip", "selected_offset_ms": None, "confidence": conf,
                "reason": reason, "per_frame": per, "park_zone_x": park_hi}

    outlier = L_max / L_med                          # is the best a relative outlier...
    left_density = L_max / (park_hi * h)             # ...AND absolutely dark enough
    if outlier < min_outlier or left_density < min_left_density:
        return skip("park_not_captured")             # no substantial left excursion

    # The park is among the strongly-left frames; pick the SHARPEST of them (the
    # settled dwell, not the motion-blurred travel into/out of the park).
    cand = [i for i in range(len(frames)) if L[i] >= cand_frac * L_max]
    best = max(cand, key=lambda i: sharp[i])
    conf = round(min(1.0, (outlier - 1.5) / 4.0) * 0.6
                 + _norm(sharp[best], sh_lo, sh_hi) * 0.4, 3)
    if conf < min_conf:
        return skip("low_confidence", conf)
    return {"decision": "selected", "selected_offset_ms": frames[best]["offset_ms"],
            "confidence": conf, "reason": None, "per_frame": per, "park_zone_x": park_hi}


# ── IO: decode, group, assemble ─────────────────────────────────────────────────

def decode_gray(path, w=DECODE_W, h=DECODE_H):
    out = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", path, "-vf", f"scale={w}:{h},format=gray",
         "-f", "rawvideo", "-"],
        capture_output=True)
    data = out.stdout
    if len(data) != w * h:
        raise RuntimeError(f"decode {path}: got {len(data)} bytes, want {w*h} "
                           f"({out.stderr.decode('utf-8','replace')[:200]})")
    return list(data)


def group_bursts(cam_dir):
    """{layer:int -> [(offset_ms, path), ...] sorted by offset} from the filenames."""
    groups = {}
    for name in os.listdir(cam_dir):
        m = FRAME_RE.search(name)
        if not m:
            continue
        layer, offset = int(m.group(2)), int(m.group(3))
        groups.setdefault(layer, []).append((offset, os.path.join(cam_dir, name)))
    for layer in groups:
        groups[layer].sort()
    return groups


def main():
    from _tuning import add_tuning_args, resolve_tuning
    required = ["left_frac", "min_outlier", "min_left_density",
                "min_confidence", "select_candidate_frac"]
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cam_dir", help="a capture camera dir, e.g. captures/<run>_smooth/ext-1")
    ap.add_argument("--out", help="copy selected frames here (one per kept layer)")
    ap.add_argument("--report", help="write per-layer scores JSON here")
    ap.add_argument("--assemble", help="assemble selected frames into this mp4 (needs --out)")
    ap.add_argument("--out-fps", type=int, default=12, help="output timelapse playback fps")
    ap.add_argument("--width", type=int, default=DECODE_W)
    ap.add_argument("--height", type=int, default=DECODE_H)
    ap.add_argument("--hold-last", action="store_true",
                    help="on a skipped layer, repeat the previous kept frame instead of dropping it")
    add_tuning_args(ap)
    args = ap.parse_args()
    cfg = resolve_tuning(args, required)

    groups = group_bursts(args.cam_dir)
    if not groups:
        sys.exit(f"no burst frames (frame_*_layer_*_t*.jpg) in {args.cam_dir}")

    results = []
    selected_paths = []
    last_kept = None
    for layer in sorted(groups):
        items = groups[layer]
        frames = [{"offset_ms": off, "gray": decode_gray(p, args.width, args.height),
                   "path": p} for off, p in items]
        sel = select_frame(frames, args.width, args.height, cfg)
        by_off = {f["offset_ms"]: f["path"] for f in frames}
        chosen = by_off.get(sel["selected_offset_ms"]) if sel["decision"] == "selected" else None
        results.append({"layer": layer, **{k: sel[k] for k in
                        ("decision", "selected_offset_ms", "confidence", "reason")},
                        "selected_path": chosen, "per_frame": sel["per_frame"]})
        if chosen:
            selected_paths.append(chosen)
            last_kept = chosen
        elif args.hold_last and last_kept:
            selected_paths.append(last_kept)

    kept = sum(1 for r in results if r["decision"] == "selected")
    skipped = len(results) - kept
    reasons = {}
    for r in results:
        if r["reason"]:
            reasons[r["reason"]] = reasons.get(r["reason"], 0) + 1
    summary = {"layers": len(results), "selected": kept, "skipped": skipped,
               "skip_reasons": reasons}
    print(json.dumps(summary, indent=2))

    if args.report:
        with open(args.report, "w") as fh:
            json.dump({"summary": summary, "layers": results}, fh, indent=2)
        print(f"wrote {args.report}")

    if args.out:
        os.makedirs(args.out, exist_ok=True)
        # Clear stale picks from a previous run, else `sel_%06d.jpg` would let ffmpeg
        # append leftover frames from an earlier/longer run onto this video.
        for old in os.listdir(args.out):
            if old.startswith("sel_") and old.endswith(".jpg"):
                os.remove(os.path.join(args.out, old))
        for n, p in enumerate(selected_paths):
            shutil.copy(p, os.path.join(args.out, f"sel_{n:06}.jpg"))
        print(f"copied {len(selected_paths)} frames -> {args.out}")
        if args.assemble:
            cmd = ["ffmpeg", "-y", "-framerate", str(args.out_fps), "-i",
                   os.path.join(args.out, "sel_%06d.jpg"),
                   "-vf", "scale='min(1280,iw)':-2,format=yuv420p",
                   "-c:v", "libx264", "-crf", "23", args.assemble]
            r = subprocess.run(cmd, capture_output=True, text=True)
            if r.returncode:
                sys.exit(f"ffmpeg assemble failed: {r.stderr[-400:]}")
            print(f"assembled {args.assemble}")


if __name__ == "__main__":
    main()
