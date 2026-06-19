#!/usr/bin/env python3
"""Unit tests for mine_smooth — the continuous-recording park miner.

Two layers: pure 1-D peak picking on synthetic left-mass signals (deterministic),
and an integration test feeding synthetic grayscale frame sequences through
score_continuous_frames -> pick_park_peaks. No mp4 / ffmpeg.
Run: python3 test_mine_smooth.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mine_smooth import pick_park_peaks, score_continuous_frames  # noqa: E402

FPS = 3.0


def scores(left_masses, sharps=None):
    return [{"idx": i, "left_mass": float(lm),
             "sharpness": float(sharps[i]) if sharps else 100.0,
             "centroid_x": 5.0} for i, lm in enumerate(left_masses)]


class PickParkPeaksTests(unittest.TestCase):
    def test_flat_signal_has_no_parks(self):
        self.assertEqual(pick_park_peaks(scores([200] * 30), FPS), [])

    def test_single_park_is_one_peak(self):
        sig = [200] * 10 + [5000, 5000, 5000] + [200] * 10
        p = pick_park_peaks(scores(sig), FPS)
        self.assertEqual(len(p), 1, p)
        self.assertIn(p[0]["idx"], (10, 11, 12), p)

    def test_dwell_is_one_peak_and_picks_sharpest(self):
        # a 4-sample dwell must collapse to ONE park, choosing the sharpest frame
        sig = [200] * 5 + [4000, 5000, 4500, 5000] + [200] * 5
        sharp = [100] * 5 + [50, 90, 200, 60] + [100] * 5  # idx 7 sharpest
        p = pick_park_peaks(scores(sig, sharp), FPS)
        self.assertEqual(len(p), 1, p)
        self.assertEqual(p[0]["idx"], 7, p)

    def test_two_parks_far_apart_are_both_found(self):
        sig = [200] * 5 + [5000, 5000] + [200] * 20 + [5000, 5000] + [200] * 5
        self.assertEqual(len(pick_park_peaks(scores(sig), FPS)), 2)

    def test_two_close_events_collapse_to_one(self):
        # spikes ~1s apart (gap below merge/min-sep) → a single park
        sig = [200] * 5 + [5000, 5000] + [200, 200] + [5000, 5000] + [200] * 5
        self.assertEqual(len(pick_park_peaks(scores(sig), FPS)), 1)

    def test_long_event_is_rejected(self):
        # 12 samples = 4s > max_island_s → a wipe, not a park
        sig = [200] * 5 + [5000] * 12 + [200] * 5
        self.assertEqual(pick_park_peaks(scores(sig), FPS), [])

    def test_noisy_drifting_baseline_still_finds_the_park(self):
        base = [100 + (i % 5) * 40 + i * 2 for i in range(40)]  # noise + upward drift
        base[20:23] = [6000, 6000, 6000]                        # one clear park
        p = pick_park_peaks(scores(base), FPS)
        self.assertEqual(len(p), 1, p)
        self.assertIn(p[0]["idx"], (20, 21, 22), p)


# ── integration: synthetic frames through score_continuous_frames -> pick ──
W, H = 48, 24
BG, FIX, OBJ, HEAD = 200, 40, 110, 25


def cframe(head_x):
    """Bright bed + a STATIC dark fixture at the far-left edge (cols 0-1, must NOT
    be mistaken for a park) + a static center object + the dark head bar at head_x."""
    img = [BG] * (W * H)
    for y in range(H):
        row = y * W
        img[row + 0] = FIX
        img[row + 1] = FIX
        for x in range(W // 2 - 3, W // 2 + 3):
            img[row + x] = OBJ
        for x in range(max(0, head_x - 4), min(W, head_x + 5)):
            img[row + x] = HEAD
    return img


class ContinuousIntegrationTests(unittest.TestCase):
    def test_parks_in_a_synthetic_stream_are_recovered(self):
        # head prints at center, sweeping to the far-left (park) for 2 frames every
        # ~15 frames (~5s at 3fps); 3 parks over the stream.
        seq = []
        park_frames = {15, 16, 35, 36, 55, 56}
        for i in range(70):
            seq.append(cframe(6 if i in park_frames else W // 2))
        sc = score_continuous_frames(seq, W, H, {"fps": FPS, "ema_seconds": 6.0})
        parks = pick_park_peaks(sc, FPS)
        self.assertEqual(len(parks), 3, [p["idx"] for p in parks])
        for p in parks:  # each chosen frame sits in one of the park windows
            self.assertTrue(any(abs(p["idx"] - f) <= 2 for f in (15, 35, 55)), p)

    def test_static_left_fixture_is_not_a_park(self):
        # head never leaves center; the always-dark left fixture must not register.
        seq = [cframe(W // 2) for _ in range(40)]
        sc = score_continuous_frames(seq, W, H, {"fps": FPS, "ema_seconds": 6.0})
        self.assertEqual(pick_park_peaks(sc, FPS), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
