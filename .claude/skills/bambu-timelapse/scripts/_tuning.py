"""Shared CLI plumbing for the park-detection HEURISTICS.

These constants are setup-specific — they depend on where the camera and printer sit
and how the toolhead looks — so the code carries NO defaults: you pass them per run via
`--config <json>` (copy tuning.example.json and calibrate for your setup) and/or
individual `--<knob>` overrides. A missing required knob is a clear error, not a silent
stale default, because the right value changes whenever the camera or printer moves.
"""
import json
import sys

# knob -> one-line help. NO defaults live here on purpose (see module docstring).
KNOBS = {
    "fps": "stream sampling rate (frames/s) for detection",
    "left_frac": "park zone = the left this-fraction of the frame (camera framing)",
    "ema_seconds": "EMA background time-constant (s)",
    "abs_floor": "minimum left-mass for a real park (scales with framing/lighting/scale)",
    "mad_k": "threshold = rolling median + k*MAD of left-mass",
    "merge_gap_s": "samples within this gap are one island/park (s)",
    "max_island_s": "an island longer than this is a wipe/purge, not a park (s)",
    "min_sep_s": "parks closer than this keep only the stronger (s)",
    "candidate_frac": "pick the sharpest frame among those >= this * island max",
    "warmup_s": "(live) settle the background this long before emitting (s)",
    "baseline_s": "(live) rolling-baseline window for the threshold (s)",
    "min_outlier": "(select) park left-mass outlier ratio vs the burst median",
    "min_left_density": "(select) min mean park-zone darkness, 0-255",
    "min_confidence": "(select) min selection confidence to keep a layer",
    "select_candidate_frac": "(select) sharpest among burst frames >= this * burst max "
                             "(separate from candidate_frac: select subtracts the burst median, "
                             "mine/live use the EMA background — so they calibrate differently)",
}


def add_tuning_args(ap):
    """Add `--config` plus a per-knob override for each KNOB (all default None so an
    unset override never masks the config)."""
    ap.add_argument("--config", help="JSON of tuning knobs for THIS setup "
                    "(copy scripts/tuning.example.json; re-calibrate if the camera/printer moves)")
    g = ap.add_argument_group("tuning overrides (override --config; the code has NO defaults)")
    for name, help_ in KNOBS.items():
        g.add_argument("--" + name.replace("_", "-"), type=float, default=None, help=help_)


def resolve_tuning(args, required):
    """Build the cfg from --config overlaid with any --<knob> CLI overrides. Exit with
    a clear message if any `required` knob is still unset — never fall back to a default."""
    cfg = {}
    if getattr(args, "config", None):
        with open(args.config) as fh:
            cfg.update(json.load(fh))
    for name in KNOBS:
        v = getattr(args, name, None)
        if v is not None:
            cfg[name] = v
    cfg = {k: v for k, v in cfg.items() if not k.startswith("_")}
    missing = [n for n in required if n not in cfg]
    if missing:
        sys.exit("missing tuning values (no baked defaults): " + ", ".join(missing)
                 + "\n  supply via --config <json> (copy scripts/tuning.example.json and "
                 "calibrate for your camera/printer), or e.g. --"
                 + missing[0].replace("_", "-") + " <value>")
    return cfg
