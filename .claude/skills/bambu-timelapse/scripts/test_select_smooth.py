#!/usr/bin/env python3
"""Unit tests for select_smooth.select_frame — the pure park-frame picker.

Synthetic grayscale frames only (no JPEGs / ffmpeg): a bright bed, a STATIC dark
print object (must NOT be mistaken for the head), and a dark MOVING "toolhead" bar
that is over the print in the MAJORITY of the burst (so the per-burst median = the
printing scene, as on the real printer) and parks far-left in a minority of frames.
Run: python3 test_select_smooth.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from select_smooth import select_frame  # noqa: E402

W, H = 48, 24
BG, OBJ, HEAD = 200, 110, 25
CENTER = W // 2  # default print/print-head position


def frame(head_x, sharp=True, obj_x=CENTER, head_hw=5):
    """Bright bed + a static dark print object at obj_x + a dark head bar at head_x.
    `sharp=False` feathers the head edges over 3px to mimic travel motion blur."""
    img = [BG] * (W * H)
    for y in range(H):
        row = y * W
        for x in range(max(0, obj_x - 3), min(W, obj_x + 3)):
            img[row + x] = OBJ
        for x in range(W):
            d = abs(x - head_x)
            if d <= head_hw:
                img[row + x] = HEAD
            elif not sharp and d <= head_hw + 3:
                img[row + x] = int(HEAD + (d - head_hw) / 3.0 * (BG - HEAD))
    return img


def burst(specs, obj_x=CENTER):
    """specs: list of (offset_ms, head_x, sharp)."""
    return [{"offset_ms": o, "gray": frame(hx, sharp=s, obj_x=obj_x)}
            for o, hx, s in specs]


class SelectFrameTests(unittest.TestCase):
    def test_selects_far_left_parked(self):
        # majority over-print; head parks far-left (sharp) for two frames
        b = burst([(300, CENTER, True), (500, CENTER, True), (700, 6, True),
                   (900, 6, True), (1100, CENTER, True), (1300, CENTER, True)])
        r = select_frame(b, W, H)
        self.assertEqual(r["decision"], "selected", r)
        self.assertIn(r["selected_offset_ms"], (700, 900), r)

    def test_all_over_print_is_skipped(self):
        # head never leaves the print -> object zone never clears -> skip
        b = burst([(300, CENTER, True), (500, CENTER, True), (700, CENTER, True),
                   (900, CENTER, True)])
        r = select_frame(b, W, H)
        self.assertEqual(r["decision"], "skip", r)

    def test_prefers_sharp_park_over_blurred_more_left(self):
        # the MOST-left frame is a travel-blur; a slightly-less-left frame is the
        # settled sharp park -> pick the sharp one, not just the leftmost.
        b = burst([(300, CENTER, True), (500, CENTER, True), (700, 3, False),
                   (900, 8, True), (1100, CENTER, True), (1300, CENTER, True)])
        r = select_frame(b, W, H)
        self.assertEqual(r["decision"], "selected", r)
        self.assertEqual(r["selected_offset_ms"], 900, r)

    def test_static_dark_object_is_not_mistaken_for_head(self):
        # a big static dark object at center every frame; only the head moves. The
        # object must not look like "head over center" and block a valid park pick.
        b = burst([(300, CENTER, True), (500, CENTER, True), (700, 6, True),
                   (900, 6, True), (1100, CENTER, True), (1300, CENTER, True)])
        r = select_frame(b, W, H)
        self.assertEqual(r["decision"], "selected", r)

    def test_panned_camera_still_resolves_the_park(self):
        # whole scene shifted right (object + park ~10px right): zones derive from
        # the median/saliency, not image geometry, so it still works.
        b = burst([(300, CENTER + 10, True), (500, CENTER + 10, True), (700, 16, True),
                   (900, 16, True), (1100, CENTER + 10, True), (1300, CENTER + 10, True)],
                  obj_x=CENTER + 10)
        r = select_frame(b, W, H)
        self.assertEqual(r["decision"], "selected", r)
        self.assertIn(r["selected_offset_ms"], (700, 900), r)

    def test_park_before_burst_is_skipped(self):
        # the park already happened; the whole burst caught the head back over the
        # print -> must skip, not emit a head-over-print frame.
        b = burst([(900, CENTER, True), (1100, CENTER, True), (1300, CENTER, True),
                   (1500, CENTER, True)])
        r = select_frame(b, W, H)
        self.assertEqual(r["decision"], "skip", r)


if __name__ == "__main__":
    unittest.main(verbosity=2)
