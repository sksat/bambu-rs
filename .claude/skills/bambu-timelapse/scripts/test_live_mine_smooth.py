#!/usr/bin/env python3
"""Unit tests for the ONLINE LiveParkDetector — synthetic grayscale frames pushed one
at a time (as the live loop does), asserting emitted park frame indices. No ffmpeg /
stream. Heuristics come from the example config (the code has no defaults).
Run: python3 test_live_mine_smooth.py
"""
import json
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from live_mine_smooth import LiveParkDetector, live_ffmpeg_cmd  # noqa: E402

FPS = 3.0
# short warmup/ema so the synthetic streams settle quickly; framing knobs from example.
CFG = {**json.load(open(os.path.join(HERE, "tuning.example.json"))),
       "fps": FPS, "ema_seconds": 6.0, "warmup_s": 0.5, "baseline_s": 20.0}

W, H = 48, 24
BG, FIX, OBJ, HEAD = 200, 40, 110, 25


def cframe(head_x, sharp=True, weak=False):
    """Bright bed + a STATIC dark fixture at the far-left edge + a static center object
    + a dark head bar at head_x (feathered edges when not sharp = motion blur). `weak`
    draws the head on only the top half of rows → a fainter island (less left-mass),
    e.g. a partial/blurred travel move that must not shadow the real park."""
    img = [BG] * (W * H)
    for y in range(H):
        row = y * W
        img[row + 0] = FIX
        img[row + 1] = FIX
        for x in range(W // 2 - 3, W // 2 + 3):
            img[row + x] = OBJ
        if weak and y >= H // 2:
            continue                             # head only on the top half → fainter
        for x in range(W):
            d = abs(x - head_x)
            if d <= 4:
                img[row + x] = HEAD
            elif not sharp and d <= 7:
                img[row + x] = int(HEAD + (d - 4) / 3.0 * (BG - HEAD))
    return img


def run(frames):
    """Push frames one at a time; return [(push_idx, park)] emitted, plus a flush."""
    det = LiveParkDetector(W, H, CFG)
    emits = []
    for idx, f in enumerate(frames):
        e = det.push(f, idx)
        if e:
            emits.append((idx, e))
    fe = det.flush()
    if fe:
        emits.append((len(frames) - 1, fe))
    return emits


CENTER, LEFT = W // 2, 6


class LiveDetectorTests(unittest.TestCase):
    def test_flat_stream_emits_nothing(self):
        self.assertEqual(run([cframe(CENTER) for _ in range(24)]), [])

    def test_single_park_emits_once_after_the_island_closes(self):
        frames = [cframe(CENTER)] * 8 + [cframe(LEFT)] * 3 + [cframe(CENTER)] * 10
        emits = run(frames)
        self.assertEqual(len(emits), 1, emits)
        push_idx, park = emits[0]
        self.assertIn(park["idx"], (8, 9, 10), park)          # picked a park frame
        self.assertGreater(push_idx, park["idx"])             # emitted on CLOSE = lag

    def test_dwell_emits_once_and_picks_sharpest(self):
        # 4 park frames; the 2nd is the sharp settled one, the others blurred travel
        park = [cframe(LEFT, sharp=False), cframe(LEFT, sharp=True),
                cframe(LEFT, sharp=False), cframe(LEFT, sharp=False)]
        frames = [cframe(CENTER)] * 8 + park + [cframe(CENTER)] * 10
        emits = run(frames)
        self.assertEqual(len(emits), 1, emits)
        self.assertEqual(emits[0][1]["idx"], 9, emits)        # idx 9 = the sharp frame

    def test_two_parks_far_apart_emit_twice(self):
        frames = ([cframe(CENTER)] * 8 + [cframe(LEFT)] * 2 + [cframe(CENTER)] * 15
                  + [cframe(LEFT)] * 2 + [cframe(CENTER)] * 8)
        self.assertEqual(len(run(frames)), 2)

    def test_two_close_parks_collapse_to_one(self):
        frames = ([cframe(CENTER)] * 8 + [cframe(LEFT)] * 2 + [cframe(CENTER)] * 2
                  + [cframe(LEFT)] * 2 + [cframe(CENTER)] * 8)
        self.assertEqual(len(run(frames)), 1)

    def test_a_stronger_close_park_supersedes_the_weaker(self):
        # a weak (partial/blurred) island, then the real STRONGER park <min_sep later but
        # far enough to close separately: the detector emits the strong one flagged
        # `replace` so the IO layer overwrites the weak frame (keep-stronger, batch parity).
        frames = ([cframe(CENTER)] * 8 + [cframe(LEFT, weak=True)] * 2 + [cframe(CENTER)] * 5
                  + [cframe(LEFT)] * 2 + [cframe(CENTER)] * 8)
        emits = run(frames)
        self.assertEqual(len(emits), 2, emits)                 # both islands close
        self.assertFalse(emits[0][1]["replace"], emits)        # weak emitted first
        self.assertTrue(emits[1][1]["replace"], emits)         # strong supersedes it
        self.assertGreater(emits[1][1]["left_mass"], emits[0][1]["left_mass"])

    def test_long_left_event_is_rejected_as_a_wipe(self):
        frames = [cframe(CENTER)] * 6 + [cframe(LEFT)] * 12 + [cframe(CENTER)] * 6
        self.assertEqual(run(frames), [])


class FfmpegCmdTests(unittest.TestCase):
    def test_tee_command_has_synced_gray_and_full_outputs(self):
        cmd = live_ffmpeg_cmd("http://cam/stream", "/ring", 4, 64, 36)
        joined = " ".join(cmd)
        self.assertIn("-f mpjpeg -i http://cam/stream", joined)
        self.assertIn("split=2[full][det]", joined)           # one input, two outputs
        self.assertIn("scale=64:36,format=gray", joined)       # detection stream
        self.assertIn("rawvideo pipe:1", joined)
        self.assertTrue(joined.endswith("/ring/full_%09d.jpg"))  # full-res ring, index-aligned


if __name__ == "__main__":
    unittest.main(verbosity=2)
