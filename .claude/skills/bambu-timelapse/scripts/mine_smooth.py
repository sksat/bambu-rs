#!/usr/bin/env python3
"""Mine the parked ("head off to the far-left, object exposed") frames out of a
CONTINUOUS plain recording — one per layer — and assemble a smooth timelapse.

Why this exists: the reactive per-layer burst (select_smooth.py) only catches the
native park on ~20% of layers, because the park's delay after the MQTT layer edge is
wildly variable (device-measured <100ms..>1500ms). But `bambu serve` also records the
camera's FULL stream to a live mp4 (`plain.mp4`) — every park is in there. This scans
that recording, finds each park (a recurring left-edge event, ~once per layer), and
keeps the cleanest frame of each → a near-complete smooth timelapse.

How (no layer labels needed):
  * Decode the mp4 to tiny raw grayscale at a few fps (one ffmpeg pass).
  * Track a per-pixel EMA "background" (the recent typical scene: head printing at
    center + the growing object). Each park is the head appearing ANOMALOUSLY at the
    far-left, so dark-vs-background mass in the LEFT zone spikes once per layer.
  * Detect those spikes (islands above a robust threshold), reject implausible ones
    (too long = filament wipe, not left enough), and pick the sharpest frame per
    island. Each surviving island = one layer's park.
  * Re-extract the full-resolution frame at each park's timestamp and assemble.

stdlib + ffmpeg only (no numpy/Pillow). Reuses select_smooth's pixel helpers.

Usage:
  mine_smooth.py captures/<run>_plain/ext-1/plain.mp4 --out /tmp/parks --assemble smooth.mp4
  mine_smooth.py plain.mp4 --report parks.json --sample-fps 3
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
from statistics import median

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from select_smooth import _centroid_x, _sharpness  # noqa: E402

DECODE_W, DECODE_H = 64, 36


# ── pure scoring core (no IO) ───────────────────────────────────────────────────

def ema_alpha(ema_seconds, fps):
    """EMA smoothing factor for a ~`ema_seconds` background at sampling `fps`."""
    return max(0.0, min(0.999, 1.0 - 1.0 / max(1.0, ema_seconds * fps)))


def score_one_frame(gray, ema, alpha, w, h, park_hi):
    """Update the causal per-pixel EMA background with one frame and score it.
    Returns (ema, left_mass, sharpness, centroid_x). The park is the head appearing
    dark in the LEFT zone where the background (recent typical scene) is bright, so
    `left_mass` (dark-vs-background mass in the left strip) spikes once per layer.
    Shared by the batch miner and the live detector so they score identically."""
    if ema is None:
        ema = list(gray)
    else:
        for i, v in enumerate(gray):
            ema[i] = alpha * ema[i] + (1.0 - alpha) * v
    left_mass = 0.0
    sal = [0.0] * (w * h)
    for y in range(h):
        row = y * w
        for x in range(w):
            d = ema[row + x] - gray[row + x]
            if d > 0:
                sal[row + x] = d
                if x < park_hi:
                    left_mass += d
    return ema, left_mass, _sharpness(gray, w, h), _centroid_x(sal, w, h)


def score_continuous_frames(frames, w, h, cfg):
    """Per-frame left-park saliency over a continuous recording. `frames` is a list
    of grayscale [int]*(w*h). Returns [{idx, left_mass, sharpness, centroid_x}].
    `cfg` must carry left_frac/ema_seconds/fps (no defaults — they're setup-specific)."""
    park_hi = max(1, int(cfg["left_frac"] * w))
    alpha = ema_alpha(cfg["ema_seconds"], cfg["fps"])
    out = []
    ema = None
    for idx, g in enumerate(frames):
        ema, lm, sh, cx = score_one_frame(g, ema, alpha, w, h, park_hi)
        out.append({"idx": idx, "left_mass": round(lm, 1),
                    "sharpness": round(sh, 1), "centroid_x": round(cx, 2)})
    return out


def _mad(vals, med):
    return median([abs(v - med) for v in vals]) if vals else 0.0


def pick_park_peaks(scores, fps, cfg):
    """Find the parks: islands of high left-mass, each = one layer's park. Returns
    [{idx, t, confidence, left_mass}] for the chosen (sharpest) frame of each island.

    Robust threshold (median + k·MAD of left_mass, plus an absolute floor); samples
    above it within `merge_gap_s` form one island; islands that are implausibly long
    (a filament-change wipe, not a park) or too close to the previous one are dropped;
    the sharpest frame of each island is kept. `cfg` carries every knob (no defaults)."""
    k = cfg["mad_k"]
    floor = cfg["abs_floor"]                     # min left_mass for a real park
    merge_gap_s = cfg["merge_gap_s"]             # samples within this = one island
    max_island_s = cfg["max_island_s"]           # longer = not a normal park
    min_sep_s = cfg["min_sep_s"]                 # parks closer than this → keep one
    cand_frac = cfg["candidate_frac"]            # sharpest among >=cand_frac of island max

    lm = [s["left_mass"] for s in scores]
    if not lm:
        return []
    med = median(lm)
    thr = max(floor, med + k * _mad(lm, med))
    gap = max(1, int(round(merge_gap_s * fps)))
    max_len = max(1, int(round(max_island_s * fps)))

    # group above-threshold samples into islands (allowing small gaps)
    islands = []
    cur = []
    last_hi = None
    for i, s in enumerate(scores):
        if s["left_mass"] >= thr:
            if last_hi is not None and i - last_hi > gap and cur:
                islands.append(cur)
                cur = []
            cur.append(i)
            last_hi = i
    if cur:
        islands.append(cur)

    parks = []
    last_t = None
    for isl in islands:
        peak = max(isl, key=lambda i: scores[i]["left_mass"])
        island_max = scores[peak]["left_mass"]
        duration_s = (isl[-1] - isl[0] + 1) / fps
        t = peak / fps
        if duration_s > max_island_s:
            continue                              # too long → wipe / not a park
        if last_t is not None and t - last_t < min_sep_s:
            # too close to the previous park — keep whichever is stronger
            if island_max <= scores[parks[-1]["idx"]]["left_mass"]:
                continue
            parks.pop()
        # sharpest frame among the strong samples of the island
        strong = [i for i in isl if scores[i]["left_mass"] >= cand_frac * island_max]
        best = max(strong, key=lambda i: scores[i]["sharpness"])
        # confidence: how far above the noise floor, capped
        conf = round(min(1.0, (island_max - thr) / (thr + 1e-9)), 3)
        parks.append({"idx": best, "t": round(best / fps, 2),
                      "left_mass": scores[best]["left_mass"], "confidence": conf})
        last_t = t
    return parks


def should_remove_source(rm_source, parks, kept, has_out):
    """Whether to delete the source recording after mining. Only when the mine FULLY
    succeeded — every detected park was extracted (`kept == parks`) and `--out` saved
    them. A partial extraction keeps the recording, since the missing park frames can
    only be recovered from it; likewise an empty run or a missing `--out`."""
    if not rm_source:
        return False, "not requested"
    if not has_out:
        return False, "refused: needs --out, or the recording would be the only copy"
    if parks == 0:
        return False, "refused: no parks found"
    if kept < parks:
        return False, f"refused: only {kept}/{parks} frames extracted"
    return True, "ok"


# ── IO: ffmpeg decode + extract + assemble ──────────────────────────────────────

def extract_gray_frames_from_mp4(path, fps, w=DECODE_W, h=DECODE_H):
    """One ffmpeg pass → list of grayscale [int]*(w*h) sampled at `fps`."""
    out = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", path, "-vf",
         f"fps={fps},scale={w}:{h},format=gray", "-f", "rawvideo", "-"],
        capture_output=True)
    data = out.stdout
    n = len(data) // (w * h)
    if n == 0:
        raise RuntimeError(f"decode {path}: no frames "
                           f"({out.stderr.decode('utf-8', 'replace')[:200]})")
    return [list(data[i * w * h:(i + 1) * w * h]) for i in range(n)]


def extract_full_frame(path, t, out_path):
    """Pull the full-resolution frame at time `t` seconds into out_path (jpg)."""
    r = subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-ss", f"{t:.3f}", "-i", path,
         "-frames:v", "1", out_path], capture_output=True, text=True)
    return r.returncode == 0 and os.path.exists(out_path)


def main():
    from _tuning import add_tuning_args, resolve_tuning
    required = ["fps", "left_frac", "ema_seconds", "abs_floor", "mad_k",
                "merge_gap_s", "max_island_s", "min_sep_s", "candidate_frac"]
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("mp4", help="a continuous plain recording, e.g. captures/<run>_plain/ext-1/plain.mp4")
    ap.add_argument("--out", help="extract the parked frames here (one per park)")
    ap.add_argument("--assemble", help="assemble the parked frames into this mp4 (needs --out)")
    ap.add_argument("--report", help="write the parks JSON here")
    ap.add_argument("--out-fps", type=int, default=12, help="output timelapse playback fps")
    ap.add_argument("--width", type=int, default=DECODE_W, help="detection decode width")
    ap.add_argument("--height", type=int, default=DECODE_H, help="detection decode height")
    ap.add_argument("--rm-source", action="store_true",
                    help="delete the source recording after a successful mine (keeps the "
                         "extracted park JPEGs in --out and the assembled mp4 — the recording "
                         "is just a big intermediate). Refused if nothing was saved.")
    add_tuning_args(ap)
    args = ap.parse_args()
    cfg = resolve_tuning(args, required)

    frames = extract_gray_frames_from_mp4(args.mp4, cfg["fps"], args.width, args.height)
    scores = score_continuous_frames(frames, args.width, args.height, cfg)
    parks = pick_park_peaks(scores, cfg["fps"], cfg)
    summary = {"sampled_frames": len(frames), "sample_fps": cfg["fps"],
               "parks": len(parks)}
    print(json.dumps(summary, indent=2))

    if args.report:
        with open(args.report, "w") as fh:
            json.dump({"summary": summary, "parks": parks}, fh, indent=2)
        print(f"wrote {args.report}")

    kept = 0
    if args.out:
        os.makedirs(args.out, exist_ok=True)
        for old in os.listdir(args.out):
            if old.startswith("park_") and old.endswith(".jpg"):
                os.remove(os.path.join(args.out, old))
        for n, p in enumerate(parks):
            dst = os.path.join(args.out, f"park_{n:06}.jpg")
            if extract_full_frame(args.mp4, p["t"], dst):
                kept += 1
        print(f"extracted {kept}/{len(parks)} parked frames -> {args.out}")
        if args.assemble:
            cmd = ["ffmpeg", "-y", "-v", "error", "-framerate", str(args.out_fps),
                   "-i", os.path.join(args.out, "park_%06d.jpg"),
                   "-vf", "scale='min(1280,iw)':-2,format=yuv420p",
                   "-c:v", "libx264", "-crf", "23", args.assemble]
            r = subprocess.run(cmd, capture_output=True, text=True)
            if r.returncode:
                sys.exit(f"ffmpeg assemble failed: {r.stderr[-400:]}")
            print(f"assembled {args.assemble}")

    # The recording is a big transient; drop it once the parks are safely saved.
    do_rm, reason = should_remove_source(args.rm_source, len(parks), kept, bool(args.out))
    if do_rm:
        freed = os.path.getsize(args.mp4) // 1_000_000
        os.remove(args.mp4)
        print(f"removed source {args.mp4} (freed ~{freed} MB)")
    elif args.rm_source:
        print(f"--rm-source skipped ({reason}); kept the recording")


if __name__ == "__main__":
    main()
