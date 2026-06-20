//! Pure, online "parked frame per layer" detector — the I/O-free core of the smooth
//! timelapse, ported from the validated Python `live_mine_smooth.py` so `bambu serve`
//! can drive it in-process (one ffmpeg as the only external tool; no python3 runtime).
//!
//! Signal: each layer the A1's native timelapse parks the head off to the far-left
//! X-min with the print exposed. Against a causal per-pixel EMA "background" (the
//! recent typical scene — head printing at center + the growing object), that park is
//! the head appearing ANOMALOUSLY dark in the LEFT zone, so dark-vs-background mass in
//! the left strip (`left_mass`) spikes once per layer. [`LiveParkDetector`] tracks the
//! EMA, thresholds `left_mass` against a robust rolling baseline, groups above-threshold
//! frames into islands, rejects implausible ones (too-long span = a filament wipe), and
//! emits the sharpest frame of each island when it CLOSES (a few-frames lag) — the
//! online analog of the batch picker.
//!
//! Tuning is config-driven with NO defaults (the knobs depend on camera/printer
//! placement, which moves): [`ParkTuning`] deserializes from JSON and a missing field
//! is a hard error, never a silent stale value. The caller owns all I/O (ffmpeg, frame
//! bytes, writing `latest_park.jpg`); this stays pure so it's exhaustively unit-tested.

use std::collections::VecDeque;

use serde::Deserialize;

/// Park-detection heuristics for ONE camera/printer setup. Deserialized from a config
/// (e.g. `scripts/tuning.example.json`); there are deliberately NO defaults, so a
/// missing field fails to parse rather than running with a wrong baked value. Extra
/// keys in the JSON (the batch/select knobs, `_comment`s) are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct ParkTuning {
    /// Stream sampling rate (frames/s) the detector is fed at.
    pub fps: f64,
    /// Park zone = the left this-fraction of the frame (camera framing).
    pub left_frac: f64,
    /// EMA background time-constant (seconds).
    pub ema_seconds: f64,
    /// Minimum `left_mass` for a real park (scales with framing/lighting/scale).
    pub abs_floor: f64,
    /// Threshold = rolling median + `mad_k` * MAD of `left_mass`.
    pub mad_k: f64,
    /// Above-threshold frames within this gap (seconds) form one island/park.
    pub merge_gap_s: f64,
    /// An island whose spike SPAN exceeds this (seconds) is a wipe/purge, not a park.
    pub max_island_s: f64,
    /// Parks closer than this (seconds) keep only the stronger.
    pub min_sep_s: f64,
    /// Pick the sharpest frame among those >= this * the island's max `left_mass`.
    pub candidate_frac: f64,
    /// Settle the background this long (seconds) before emitting.
    pub warmup_s: f64,
    /// Rolling-baseline window (seconds) for the threshold.
    pub baseline_s: f64,
}

/// One emitted park: the chosen frame index, its time, and why it was chosen. `replace`
/// = this park lands within `min_sep` of and is stronger than the one just emitted (the
/// same layer), so the IO layer overwrites that frame rather than adding one.
#[derive(Debug, Clone, PartialEq)]
pub struct Park {
    /// Frame index of the chosen (sharpest) frame of the island.
    pub idx: u64,
    /// Its timestamp (seconds) = `idx / fps`.
    pub t: f64,
    pub left_mass: f64,
    pub sharpness: f64,
    /// How far the island's peak rose above the floor, capped at 1.0.
    pub confidence: f64,
    /// Supersede the previously emitted park (a stronger close pair) vs. a new one.
    pub replace: bool,
}

/// EMA smoothing factor for a ~`ema_seconds` background at sampling `fps`.
pub fn ema_alpha(ema_seconds: f64, fps: f64) -> f64 {
    (1.0 - 1.0 / (ema_seconds * fps).max(1.0)).clamp(0.0, 0.999)
}

/// Tenengrad-ish gradient energy: high for a settled/in-focus frame, low for a
/// motion-blurred travel frame. Sum of squared neighbour differences, normalised.
fn sharpness(gray: &[u8], w: usize, h: usize) -> f64 {
    let mut s: i64 = 0;
    for y in 0..h {
        let row = y * w;
        for x in 0..w - 1 {
            let d = gray[row + x] as i64 - gray[row + x + 1] as i64;
            s += d * d;
        }
    }
    for y in 0..h - 1 {
        let row = y * w;
        let nxt = row + w;
        for x in 0..w {
            let d = gray[row + x] as i64 - gray[nxt + x] as i64;
            s += d * d;
        }
    }
    s as f64 / (w * h) as f64
}

/// Median of a slice (average of the two middles for an even count), matching
/// Python's `statistics.median`. Empty → 0.0.
fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

fn round_to(x: f64, decimals: i32) -> f64 {
    let f = 10f64.powi(decimals);
    (x * f).round() / f
}

#[derive(Clone, Copy)]
struct IslandFrame {
    idx: u64,
    left_mass: f64,
    sharpness: f64,
}

#[derive(PartialEq)]
enum State {
    Idle,
    InIsland,
    Suppress,
}

/// Online park detector. Feed grayscale frames one at a time via [`push`]; it returns a
/// [`Park`] when an island CLOSES (else `None`), reproducing the batch picker's "sharpest
/// frame of the dwell" at the cost of a few-frames lag. [`flush`] closes a still-open
/// island when the stream ends.
///
/// [`push`]: LiveParkDetector::push
/// [`flush`]: LiveParkDetector::flush
pub struct LiveParkDetector {
    w: usize,
    h: usize,
    fps: f64,
    park_hi: usize,
    alpha: f64,
    abs_floor: f64,
    mad_k: f64,
    merge_gap: u64,
    max_island: u64,
    min_sep_s: f64,
    cand_frac: f64,
    warmup: u64,
    baseline_cap: usize,
    baseline: VecDeque<f64>,
    ema: Option<Vec<f64>>,
    seen: u64,
    state: State,
    island: Vec<IslandFrame>,
    start_idx: Option<u64>,
    last_hi: Option<u64>,
    last_emit_idx: Option<u64>,
    last_emit_lm: Option<f64>,
}

impl LiveParkDetector {
    /// Build a detector for `w`x`h` grayscale frames with the given tuning. Every knob is
    /// taken from `cfg` — no baked defaults.
    pub fn new(w: usize, h: usize, cfg: &ParkTuning) -> Self {
        let fps = cfg.fps;
        let samples = |secs: f64| ((secs * fps).round() as i64).max(1) as u64;
        Self {
            w,
            h,
            fps,
            park_hi: ((cfg.left_frac * w as f64) as usize).max(1),
            alpha: ema_alpha(cfg.ema_seconds, fps),
            abs_floor: cfg.abs_floor,
            mad_k: cfg.mad_k,
            merge_gap: samples(cfg.merge_gap_s),
            max_island: samples(cfg.max_island_s),
            min_sep_s: cfg.min_sep_s,
            cand_frac: cfg.candidate_frac,
            warmup: samples(cfg.warmup_s),
            baseline_cap: (((cfg.baseline_s * fps).round() as i64).max(8)) as usize,
            baseline: VecDeque::new(),
            ema: None,
            seen: 0,
            state: State::Idle,
            island: Vec::new(),
            start_idx: None,
            last_hi: None,
            last_emit_idx: None,
            last_emit_lm: None,
        }
    }

    /// Update the causal per-pixel EMA background with one frame and score it: the
    /// dark-vs-background mass in the LEFT zone (the park signal) and the frame's
    /// sharpness. The EMA is seeded from the first frame.
    fn score(&mut self, gray: &[u8]) -> (f64, f64) {
        match &mut self.ema {
            None => self.ema = Some(gray.iter().map(|&v| v as f64).collect()),
            Some(ema) => {
                for (e, &v) in ema.iter_mut().zip(gray.iter()) {
                    *e = self.alpha * *e + (1.0 - self.alpha) * v as f64;
                }
            }
        }
        let ema = self.ema.as_ref().unwrap();
        let mut left_mass = 0.0;
        for y in 0..self.h {
            let row = y * self.w;
            for x in 0..self.park_hi {
                let d = ema[row + x] - gray[row + x] as f64;
                if d > 0.0 {
                    left_mass += d;
                }
            }
        }
        (left_mass, sharpness(gray, self.w, self.h))
    }

    /// Robust threshold: max(abs_floor, rolling median + k·MAD of the quiet `left_mass`).
    fn threshold(&self) -> f64 {
        if self.baseline.is_empty() {
            return self.abs_floor;
        }
        let bl: Vec<f64> = self.baseline.iter().copied().collect();
        let med = median(&bl);
        let mad = if bl.len() > 1 {
            let dev: Vec<f64> = bl.iter().map(|v| (v - med).abs()).collect();
            median(&dev)
        } else {
            0.0
        };
        self.abs_floor.max(med + self.mad_k * mad)
    }

    /// Feed one grayscale frame (length `w*h`) at index `idx`; returns a [`Park`] iff an
    /// island closed on it.
    pub fn push(&mut self, gray: &[u8], idx: u64) -> Option<Park> {
        let (lm, sh) = self.score(gray);
        self.seen += 1;
        let above = lm >= self.threshold();
        if !above {
            // Only QUIET frames define the background; spikes (parks/wipes) must not
            // raise the threshold and shadow the next park.
            if self.baseline.len() >= self.baseline_cap {
                self.baseline.pop_front();
            }
            self.baseline.push_back(lm);
        }
        if self.seen <= self.warmup {
            return None; // let the background settle before emitting
        }
        match self.state {
            State::Suppress => {
                // A rejected wipe is still going — wait until it ends.
                if !above {
                    self.state = State::Idle;
                }
                None
            }
            State::Idle => {
                if above {
                    self.state = State::InIsland;
                    self.island = vec![IslandFrame {
                        idx,
                        left_mass: lm,
                        sharpness: sh,
                    }];
                    self.start_idx = Some(idx);
                    self.last_hi = Some(idx);
                }
                None
            }
            State::InIsland => {
                if above {
                    self.island.push(IslandFrame {
                        idx,
                        left_mass: lm,
                        sharpness: sh,
                    });
                    self.last_hi = Some(idx);
                }
                let span = self.last_hi.unwrap() - self.start_idx.unwrap() + 1;
                if span > self.max_island {
                    // Spike SPAN too long → a wipe; suppress until it ends.
                    self.state = State::Suppress;
                    self.island.clear();
                    self.start_idx = None;
                    return None;
                }
                if idx - self.last_hi.unwrap() >= self.merge_gap {
                    return self.close(); // island closed (a gap of quiet)
                }
                None
            }
        }
    }

    /// Pick the sharpest strong frame of the open island. If it lands within `min_sep` of
    /// the last emit, keep only the STRONGER (batch parity): a weaker island is dropped, a
    /// stronger one supersedes the just-emitted park (flagged `replace`).
    fn close(&mut self) -> Option<Park> {
        let island_max = self
            .island
            .iter()
            .map(|f| f.left_mass)
            .fold(f64::MIN, f64::max);
        let cutoff = self.cand_frac * island_max;
        // Sharpest of the strong frames; on a tie keep the FIRST (matches the Python max).
        let best = self
            .island
            .iter()
            .filter(|f| f.left_mass >= cutoff)
            .fold(None::<IslandFrame>, |acc, &f| match acc {
                Some(b) if b.sharpness >= f.sharpness => Some(b),
                _ => Some(f),
            })
            .unwrap();
        self.state = State::Idle;
        self.island.clear();
        self.start_idx = None;

        let mut replace = false;
        if let (Some(le_idx), Some(le_lm)) = (self.last_emit_idx, self.last_emit_lm)
            && (best.idx as f64 - le_idx as f64) / self.fps < self.min_sep_s
        {
            if island_max <= le_lm {
                return None; // too close AND not stronger → drop
            }
            replace = true; // stronger → supersede the previous park
        }
        self.last_emit_idx = Some(best.idx);
        self.last_emit_lm = Some(island_max);
        let conf = round_to(
            ((island_max - self.abs_floor) / (self.abs_floor + 1e-9)).min(1.0),
            3,
        );
        Some(Park {
            idx: best.idx,
            t: round_to(best.idx as f64 / self.fps, 2),
            left_mass: round_to(best.left_mass, 1),
            sharpness: round_to(best.sharpness, 1),
            confidence: conf,
            replace,
        })
    }

    /// Stream ended/disconnected: close an open island only if it had a real peak.
    pub fn flush(&mut self) -> Option<Park> {
        if self.state == State::InIsland && !self.island.is_empty() {
            let peak = self
                .island
                .iter()
                .map(|f| f.left_mass)
                .fold(f64::MIN, f64::max);
            if peak >= self.abs_floor {
                return self.close();
            }
            self.state = State::Idle;
            self.island.clear();
            self.start_idx = None;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 48;
    const H: usize = 24;
    const BG: u8 = 200;
    const FIX: u8 = 40;
    const OBJ: u8 = 110;
    const HEAD: u8 = 25;
    const CENTER: usize = W / 2;
    const LEFT: usize = 6;

    /// Short warmup/ema so the synthetic streams settle quickly; framing knobs mirror
    /// the example config. The code has no defaults, so the test states them.
    fn cfg() -> ParkTuning {
        ParkTuning {
            fps: 3.0,
            left_frac: 0.33,
            ema_seconds: 6.0,
            abs_floor: 1500.0,
            mad_k: 6.0,
            merge_gap_s: 1.2,
            max_island_s: 3.0,
            min_sep_s: 3.0,
            candidate_frac: 0.75,
            warmup_s: 0.5,
            baseline_s: 20.0,
        }
    }

    /// Bright bed + a STATIC dark fixture at the far-left edge + a static center object +
    /// a dark head bar at `head_x` (feathered edges when not `sharp` = motion blur).
    /// `weak` draws the head on only the top half of rows → a fainter island.
    fn cframe(head_x: usize, sharp: bool, weak: bool) -> Vec<u8> {
        let mut img = vec![BG; W * H];
        for y in 0..H {
            let row = y * W;
            img[row] = FIX;
            img[row + 1] = FIX;
            for x in (W / 2 - 3)..(W / 2 + 3) {
                img[row + x] = OBJ;
            }
            if weak && y >= H / 2 {
                continue; // head only on the top half → fainter
            }
            for x in 0..W {
                let d = (x as i64 - head_x as i64).unsigned_abs() as i64;
                if d <= 4 {
                    img[row + x] = HEAD;
                } else if !sharp && d <= 7 {
                    img[row + x] =
                        (HEAD as f64 + (d - 4) as f64 / 3.0 * (BG as f64 - HEAD as f64)) as u8;
                }
            }
        }
        img
    }

    fn park(head_x: usize) -> Vec<u8> {
        cframe(head_x, true, false)
    }

    /// Push frames one at a time; return `(push_idx, park)` emitted, plus a flush.
    fn run(frames: &[Vec<u8>]) -> Vec<(u64, Park)> {
        let mut det = LiveParkDetector::new(W, H, &cfg());
        let mut emits = Vec::new();
        for (idx, f) in frames.iter().enumerate() {
            if let Some(p) = det.push(f, idx as u64) {
                emits.push((idx as u64, p));
            }
        }
        if let Some(p) = det.flush() {
            emits.push((frames.len() as u64 - 1, p));
        }
        emits
    }

    fn repeat(f: &[u8], n: usize) -> Vec<Vec<u8>> {
        std::iter::repeat_n(f.to_vec(), n).collect()
    }

    fn chain(parts: &[Vec<Vec<u8>>]) -> Vec<Vec<u8>> {
        parts.iter().flatten().cloned().collect()
    }

    #[test]
    fn flat_stream_emits_nothing() {
        assert!(run(&repeat(&park(CENTER), 24)).is_empty());
    }

    #[test]
    fn single_park_emits_once_after_the_island_closes() {
        let frames = chain(&[
            repeat(&park(CENTER), 8),
            repeat(&park(LEFT), 3),
            repeat(&park(CENTER), 10),
        ]);
        let emits = run(&frames);
        assert_eq!(emits.len(), 1, "{emits:?}");
        let (push_idx, p) = &emits[0];
        assert!((8..=10).contains(&p.idx), "picked a park frame: {p:?}");
        assert!(*push_idx > p.idx, "emitted on CLOSE = lag");
    }

    #[test]
    fn dwell_emits_once_and_picks_sharpest() {
        // 4 park frames; the 2nd (idx 9) is the sharp settled one, the rest blurred travel.
        let dwell = vec![
            cframe(LEFT, false, false),
            cframe(LEFT, true, false),
            cframe(LEFT, false, false),
            cframe(LEFT, false, false),
        ];
        let frames = chain(&[repeat(&park(CENTER), 8), dwell, repeat(&park(CENTER), 10)]);
        let emits = run(&frames);
        assert_eq!(emits.len(), 1, "{emits:?}");
        assert_eq!(emits[0].1.idx, 9, "idx 9 = the sharp frame: {emits:?}");
    }

    #[test]
    fn two_parks_far_apart_emit_twice() {
        let frames = chain(&[
            repeat(&park(CENTER), 8),
            repeat(&park(LEFT), 2),
            repeat(&park(CENTER), 15),
            repeat(&park(LEFT), 2),
            repeat(&park(CENTER), 8),
        ]);
        assert_eq!(run(&frames).len(), 2);
    }

    #[test]
    fn two_close_parks_collapse_to_one() {
        let frames = chain(&[
            repeat(&park(CENTER), 8),
            repeat(&park(LEFT), 2),
            repeat(&park(CENTER), 2),
            repeat(&park(LEFT), 2),
            repeat(&park(CENTER), 8),
        ]);
        assert_eq!(run(&frames).len(), 1);
    }

    #[test]
    fn a_stronger_close_park_supersedes_the_weaker() {
        // a weak (partial) island, then the real STRONGER park <min_sep later but far
        // enough to close separately: emit the strong one flagged `replace`.
        let frames = chain(&[
            repeat(&park(CENTER), 8),
            repeat(&cframe(LEFT, true, true), 2),
            repeat(&park(CENTER), 5),
            repeat(&park(LEFT), 2),
            repeat(&park(CENTER), 8),
        ]);
        let emits = run(&frames);
        assert_eq!(emits.len(), 2, "{emits:?}");
        assert!(!emits[0].1.replace, "weak emitted first: {emits:?}");
        assert!(emits[1].1.replace, "strong supersedes it: {emits:?}");
        assert!(emits[1].1.left_mass > emits[0].1.left_mass);
    }

    #[test]
    fn long_left_event_is_rejected_as_a_wipe() {
        let frames = chain(&[
            repeat(&park(CENTER), 6),
            repeat(&park(LEFT), 12),
            repeat(&park(CENTER), 6),
        ]);
        assert!(run(&frames).is_empty());
    }

    #[test]
    fn tuning_missing_a_knob_is_a_hard_error() {
        // No defaults: a config missing `abs_floor` must fail to parse, not run with 0.
        let json = r#"{"fps":4,"left_frac":0.33,"ema_seconds":30,"mad_k":6,
            "merge_gap_s":1.2,"max_island_s":3,"min_sep_s":3,"candidate_frac":0.75,
            "warmup_s":4,"baseline_s":90}"#;
        let err = serde_json::from_str::<ParkTuning>(json).unwrap_err();
        assert!(err.to_string().contains("abs_floor"), "{err}");
    }

    #[test]
    fn the_example_config_parses_and_ignores_extra_keys() {
        // The shared example carries batch/select knobs + comments too; the live subset
        // must parse, ignoring the rest.
        let json = r#"{"_comment":"x","fps":4,"left_frac":0.33,"ema_seconds":30,
            "abs_floor":1500,"mad_k":6,"merge_gap_s":1.2,"max_island_s":3,"min_sep_s":3,
            "candidate_frac":0.75,"warmup_s":4,"baseline_s":90,"min_outlier":2.5,
            "min_left_density":3,"min_confidence":0.4,"select_candidate_frac":0.6}"#;
        let cfg: ParkTuning = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.abs_floor, 1500.0);
        assert_eq!(cfg.candidate_frac, 0.75);
    }

    #[test]
    fn ema_alpha_is_bounded_and_grows_with_the_window() {
        assert!(ema_alpha(1.0, 1.0) <= 0.999);
        assert!(ema_alpha(30.0, 4.0) > ema_alpha(6.0, 4.0));
        assert!(ema_alpha(0.0, 0.0) >= 0.0);
    }
}
