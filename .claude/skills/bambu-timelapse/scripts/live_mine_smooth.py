#!/usr/bin/env python3
"""LIVE version of mine_smooth: watch a camera's MJPEG /stream during a print and
emit the parked ("head off to the far-left, object exposed") frame for each layer in
near-real-time (a few-seconds lag), updating `latest_park.jpg` and accumulating the
frames so the smooth timelapse is ready at FINISH — no separate batch mine needed.

Same signal as mine_smooth (causal EMA background, left-zone dark-mass spike per
layer), but the peak-picker runs ONLINE: it opens an island when the left-mass rises,
buffers it, and emits the sharpest frame when the island closes (~park duration +
merge_gap lag). One ffmpeg reads the stream and tees two frame-synced outputs — a tiny
gray rawvideo (to stdin, for detection) and full-resolution JPEGs (to a ring dir) — so
the emitted preview is full-res without decoding JPEGs in Python.

stdlib + ffmpeg only. Reuses mine_smooth's scoring. The pure LiveParkDetector is unit-
tested; the ffmpeg/stream plumbing is a thin integration layer.

Usage:
  live_mine_smooth.py http://<ustreamer-host>/stream --out captures/live/ext-1 --sample-fps 4
  # during the print; Ctrl-C (or --max-seconds) stops and (with --assemble) renders the mp4.
"""
import argparse
import collections
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from statistics import median

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mine_smooth import ema_alpha, score_one_frame  # noqa: E402

DECODE_W, DECODE_H = 64, 36


# ── pure online detector (no IO) ────────────────────────────────────────────────

class LiveParkDetector:
    """Online analog of mine_smooth.pick_park_peaks. Feed grayscale frames one at a
    time via push(); it returns an emitted park dict when an island CLOSES, else None.
    Emitting on close (not on rise) reproduces the batch picker's "sharpest frame of
    the dwell" and costs a few-frames lag."""

    def __init__(self, w, h, cfg):
        # cfg carries every knob explicitly — no baked defaults (they're setup-specific).
        self.w, self.h = w, h
        self.fps = cfg["fps"]
        self.park_hi = max(1, int(cfg["left_frac"] * w))
        self.alpha = ema_alpha(cfg["ema_seconds"], self.fps)
        self.abs_floor = cfg["abs_floor"]
        self.mad_k = cfg["mad_k"]
        self.merge_gap = max(1, round(cfg["merge_gap_s"] * self.fps))
        self.max_island = max(1, round(cfg["max_island_s"] * self.fps))
        self.min_sep_s = cfg["min_sep_s"]
        self.cand_frac = cfg["candidate_frac"]
        self.warmup = max(1, round(cfg["warmup_s"] * self.fps))
        self.baseline = collections.deque(maxlen=max(8, round(cfg["baseline_s"] * self.fps)))
        self.ema = None
        self.seen = 0
        self.state = "idle"
        self.island = []          # [{idx, left_mass, sharpness}]
        self.start_idx = None
        self.last_hi = None
        self.last_emit_idx = None
        self.last_emit_lm = None

    def _threshold(self):
        if not self.baseline:
            return self.abs_floor
        med = median(self.baseline)
        mad = median([abs(v - med) for v in self.baseline]) if len(self.baseline) > 1 else 0.0
        return max(self.abs_floor, med + self.mad_k * mad)

    def _close(self):
        """Pick the sharpest strong frame of the open island. If it lands within
        min_sep of the last emit, keep only the STRONGER of the two (batch parity): a
        weaker island is dropped, a stronger one SUPERSEDES the just-emitted park — same
        layer, so the IO layer overwrites it rather than adding a frame (flagged
        `replace`). Without this, a weak false island just before the real park would
        permanently shadow it."""
        island_max = max(f["left_mass"] for f in self.island)
        strong = [f for f in self.island if f["left_mass"] >= self.cand_frac * island_max]
        best = max(strong, key=lambda f: f["sharpness"])
        self.state, self.island, self.start_idx = "idle", [], None
        replace = False
        if (self.last_emit_idx is not None
                and (best["idx"] - self.last_emit_idx) / self.fps < self.min_sep_s):
            if island_max <= self.last_emit_lm:
                return None                      # too close AND not stronger → drop
            replace = True                       # stronger → supersede the previous park
        self.last_emit_idx, self.last_emit_lm = best["idx"], island_max
        conf = round(min(1.0, (island_max - self.abs_floor) / (self.abs_floor + 1e-9)), 3)
        return {"idx": best["idx"], "t": round(best["idx"] / self.fps, 2),
                "left_mass": round(best["left_mass"], 1),
                "sharpness": round(best["sharpness"], 1), "confidence": conf,
                "replace": replace}

    def push(self, gray, idx):
        self.ema, lm, sh, _cx = score_one_frame(gray, self.ema, self.alpha, self.w, self.h, self.park_hi)
        self.seen += 1
        thr = self._threshold()
        above = lm >= thr
        if not above:                            # only QUIET frames define the background;
            self.baseline.append(lm)             # spikes (parks/wipes) must not raise the threshold
        if self.seen <= self.warmup:             # let the background settle before emitting
            return None
        if self.state == "suppress":         # a rejected wipe is still going — wait it out
            if not above:
                self.state = "idle"
            return None
        if self.state == "idle":
            if above:
                self.state, self.island = "in_island", [{"idx": idx, "left_mass": lm, "sharpness": sh}]
                self.start_idx = self.last_hi = idx
            return None
        # in_island
        if above:
            self.island.append({"idx": idx, "left_mass": lm, "sharpness": sh})
            self.last_hi = idx
        if (self.last_hi - self.start_idx + 1) > self.max_island:  # spike SPAN too long → a wipe;
            self.state, self.island, self.start_idx = "suppress", [], None  # suppress till it ends
            return None
        if (idx - self.last_hi) >= self.merge_gap:         # island closed (gap of quiet)
            return self._close()
        return None

    def flush(self):
        """Stream ended/disconnected: close an open island only if it had a real peak."""
        if self.state == "in_island" and self.island:
            if max(f["left_mass"] for f in self.island) >= self.abs_floor:
                return self._close()
            self.state, self.island, self.start_idx = "idle", [], None
        return None


# ── IO: ffmpeg stream plumbing ──────────────────────────────────────────────────

def live_ffmpeg_cmd(stream_url, ring_dir, fps, w, h):
    """One ffmpeg, two frame-synced outputs: tiny gray rawvideo to stdout (detection)
    and full-res JPEGs to ring_dir (full_%09d.jpg, frame index aligned to the gray)."""
    return [
        "ffmpeg", "-v", "error", "-f", "mpjpeg", "-i", stream_url,
        "-filter_complex",
        f"[0:v]fps={fps},split=2[full][det];[det]scale={w}:{h},format=gray[gray]",
        "-map", "[gray]", "-f", "rawvideo", "pipe:1",
        "-map", "[full]", "-start_number", "0", "-q:v", "3",
        os.path.join(ring_dir, "full_%09d.jpg"),
    ]


def _read_exact(stream, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = stream.read(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return buf


def run_live(stream_url, out_dir, w, h, cfg, max_seconds=None):
    fps = cfg["fps"]
    ring = os.path.join(out_dir, ".ring")
    os.makedirs(ring, exist_ok=True)
    os.makedirs(out_dir, exist_ok=True)
    ring_keep = max(40, int(60 * fps))           # ~60s of full-res frames buffered
    det = LiveParkDetector(w, h, cfg)
    # stderr to /dev/null: on a clean stop we terminate ffmpeg mid-write (broken-pipe
    # noise); a real connection failure surfaces as zero frames read (warned below).
    proc = subprocess.Popen(live_ffmpeg_cmd(stream_url, ring, fps, w, h),
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    jl = open(os.path.join(out_dir, "parks.jsonl"), "a")
    idx = emitted = 0
    pending = collections.deque()                # parks awaiting their full-res JPEG
    start = time.monotonic()
    stop = {"flag": False}
    signal.signal(signal.SIGINT, lambda *_: stop.update(flag=True))

    def write_park(park):
        nonlocal emitted
        src = os.path.join(ring, f"full_{park['idx']:09}.jpg")
        # a stronger close park supersedes the one just emitted (same layer): reuse its
        # index and overwrite, so the timelapse keeps exactly one frame per park.
        replace = park.get("replace") and emitted > 0
        n = emitted - 1 if replace else emitted
        tmp = os.path.join(out_dir, "latest_park.jpg.tmp")
        shutil.copy(src, tmp)
        os.replace(tmp, os.path.join(out_dir, "latest_park.jpg"))
        shutil.copy(src, os.path.join(out_dir, f"park_{n:06}.jpg"))
        jl.write(json.dumps({"n": n, **park}) + "\n")
        jl.flush()
        if not replace:
            emitted += 1
        print(f"park #{n}: t={park['t']}s conf={park['confidence']}"
              + (" (replaced)" if replace else ""), flush=True)

    def drain(final=False):
        # Write queued parks FIFO. The full-res JPEG branch can lag detection by a few
        # ticks, so block at the HEAD until its frame lands rather than reordering frames
        # — but don't busy-wait the read loop (that lets ffmpeg's pipe back up): just
        # retry next frame. A park whose frame never arrives is dropped with a WARNING,
        # never silently, since a missing layer is otherwise invisible.
        while pending:
            park = pending[0]
            src = os.path.join(ring, f"full_{park['idx']:09}.jpg")
            if not os.path.exists(src):
                if not final:
                    return                       # not ready yet — retry next frame
                for _ in range(40):              # final flush: give ffmpeg a last ~2 s
                    time.sleep(0.05)
                    if os.path.exists(src):
                        break
                if not os.path.exists(src):
                    print(f"warning: dropped park t={park['t']}s — its full-res frame "
                          f"never arrived from ffmpeg", file=sys.stderr)
                    pending.popleft()
                    continue
            write_park(park)
            pending.popleft()

    try:
        while not stop["flag"]:
            if max_seconds and time.monotonic() - start > max_seconds:
                break
            buf = _read_exact(proc.stdout, w * h)
            if buf is None:
                break                            # stream ended
            park = det.push(list(buf), idx)
            if park:
                pending.append(park)
            drain()
            if idx % int(max(1, fps * 5)) == 0:  # ring cleanup every ~5s
                for f in os.listdir(ring):
                    if f.startswith("full_") and int(f[5:14]) < idx - ring_keep:
                        os.remove(os.path.join(ring, f))
            idx += 1
        last = det.flush()
        if last:
            pending.append(last)
        drain(final=True)
    finally:
        proc.terminate()
        jl.close()
        shutil.rmtree(ring, ignore_errors=True)
    if idx == 0:
        print(f"warning: read 0 frames from {stream_url} — check the URL and that ffmpeg "
              f"can open the stream (try: ffmpeg -f mpjpeg -i {stream_url} -frames:v 1 /tmp/t.jpg)",
              file=sys.stderr)
    return emitted


REQUIRED = ["fps", "left_frac", "ema_seconds", "abs_floor", "mad_k", "merge_gap_s",
            "max_island_s", "min_sep_s", "candidate_frac", "warmup_s", "baseline_s"]


def main():
    from _tuning import add_tuning_args, resolve_tuning
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("stream_url", help="camera MJPEG stream, e.g. http://<host>/stream")
    ap.add_argument("--out", required=True, help="dir for latest_park.jpg + park_*.jpg + parks.jsonl")
    ap.add_argument("--assemble", help="on stop, assemble park_*.jpg into this mp4")
    ap.add_argument("--out-fps", type=int, default=12, help="output timelapse playback fps")
    ap.add_argument("--width", type=int, default=DECODE_W, help="detection decode width")
    ap.add_argument("--height", type=int, default=DECODE_H, help="detection decode height")
    ap.add_argument("--max-seconds", type=float, help="stop after N seconds (for testing)")
    add_tuning_args(ap)
    args = ap.parse_args()
    cfg = resolve_tuning(args, REQUIRED)

    print(f"watching {args.stream_url} -> {args.out} (Ctrl-C to stop)", flush=True)
    n = run_live(args.stream_url, args.out, args.width, args.height, cfg, args.max_seconds)
    print(f"emitted {n} parked frames -> {args.out}")

    if args.assemble and n > 0:
        cmd = ["ffmpeg", "-y", "-v", "error", "-framerate", str(args.out_fps),
               "-i", os.path.join(args.out, "park_%06d.jpg"),
               "-vf", "scale='min(1280,iw)':-2,format=yuv420p",
               "-c:v", "libx264", "-crf", "23", args.assemble]
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode:
            sys.exit(f"ffmpeg assemble failed: {r.stderr[-400:]}")
        print(f"assembled {args.assemble}")


if __name__ == "__main__":
    main()
