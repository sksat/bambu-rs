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

use serde::{Deserialize, Serialize};

/// Park-detection heuristics for ONE camera/printer setup. Deserialized from a config
/// (e.g. `scripts/tuning.example.json`); there are deliberately NO defaults, so a
/// missing field fails to parse rather than running with a wrong baked value. Extra
/// keys in the JSON (the batch/select knobs, `_comment`s) are ignored.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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

/// One burst frame for [`select_park_frame`]: its capture offset (ms after the layer edge)
/// and the decoded grayscale (`w*h`, row-major).
pub struct SelectFrame {
    pub offset_ms: u64,
    pub gray: Vec<u8>,
}

/// Knobs for [`select_park_frame`]. Distinct from [`ParkTuning`]: selection scores against
/// the per-burst MEDIAN (not the live EMA), so its cutoffs differ. No defaults — every knob
/// is supplied by the caller (it depends on camera/printer placement).
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
pub struct SelectTuning {
    /// Park zone = the left this-fraction of the frame.
    pub left_frac: f64,
    /// The park's left-mass must exceed this × the burst-median left-mass (relative outlier).
    pub min_outlier: f64,
    /// …and be at least this mean darkness (0–255) over the park zone (absolute floor).
    pub min_left_density: f64,
    /// Among frames with left-mass ≥ this × the burst max, pick the sharpest.
    pub select_candidate_frac: f64,
    /// Reject the pick (skip the layer) below this confidence.
    pub min_confidence: f64,
}

/// Why a layer's burst yielded no parked frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// No frame showed a strong left excursion — the park fell outside the burst, or the
    /// head stayed over the print. A gap beats emitting a head-over-print frame.
    ParkNotCaptured,
    /// A park was found but it wasn't confident enough.
    LowConfidence,
}

/// The outcome of picking a layer's parked frame from its burst.
#[derive(Clone, Debug, PartialEq)]
pub enum Selection {
    Selected { offset_ms: u64, confidence: f64 },
    Skipped { reason: SkipReason, confidence: f64 },
}

fn norm(v: f64, lo: f64, hi: f64) -> f64 {
    if hi <= lo { 0.0 } else { (v - lo) / (hi - lo) }
}

/// Pick the parked ("head out of the way, object visible") frame from ONE layer's capture
/// burst, or skip the layer. Pure port of the skill's `select_smooth.select_frame`.
///
/// Isolate the transient toolhead by subtracting the per-burst MEDIAN (static
/// bed/object/printer/fixtures cancel), then measure how much it CHANGES the LEFT park
/// zone (left `left_frac` of the frame, where an X-min park maps for a left-mounted
/// camera). The change is measured by ABSOLUTE deviation from the median, not just the
/// darker direction: depending on the camera the parked head reads dark (a nozzle against a
/// bright bed) OR bright (the white extruder body against a darker backdrop), and a
/// dark-only measure misses the bright case entirely (it scored ~0 and skipped every layer
/// on the front-right A1 framing). The park frame's left change is a strong OUTLIER vs the
/// burst median; the sharpest such frame (the settled dwell, not the motion-blurred travel)
/// wins. A layer whose park fell outside the burst shows no outlier and is SKIPPED.
pub fn select_park_frame(
    frames: &[SelectFrame],
    w: usize,
    h: usize,
    cfg: &SelectTuning,
) -> Selection {
    if frames.is_empty() {
        return Selection::Skipped {
            reason: SkipReason::ParkNotCaptured,
            confidence: 0.0,
        };
    }
    let n_px = w * h;
    // Per-pixel median across the burst — the static scene (bed/object/fixtures).
    let mut med = vec![0f64; n_px];
    let mut col = Vec::with_capacity(frames.len());
    for (p, m) in med.iter_mut().enumerate() {
        col.clear();
        col.extend(frames.iter().map(|f| f.gray[p] as f64));
        *m = median(&col);
    }
    let park_hi = ((cfg.left_frac * w as f64) as usize).max(1);
    // Change saliency (absolute deviation from the median, either polarity — the parked head
    // may read dark or bright depending on the camera) summed over the left zone, + sharpness.
    let mut left = Vec::with_capacity(frames.len());
    let mut sharp = Vec::with_capacity(frames.len());
    for f in frames {
        let mut lm = 0.0;
        for y in 0..h {
            let row = y * w;
            for x in 0..park_hi {
                lm += (med[row + x] - f.gray[row + x] as f64).abs();
            }
        }
        left.push(lm);
        sharp.push(sharpness(&f.gray, w, h));
    }
    let l_med = {
        let m = median(&left);
        if m == 0.0 { 1.0 } else { m }
    };
    let l_max = left.iter().cloned().fold(f64::MIN, f64::max);
    let outlier = l_max / l_med; // a relative outlier…
    let left_density = l_max / (park_hi * h) as f64; // …and absolutely dark enough
    if outlier < cfg.min_outlier || left_density < cfg.min_left_density {
        return Selection::Skipped {
            reason: SkipReason::ParkNotCaptured,
            confidence: 0.0,
        };
    }
    let sh_lo = sharp.iter().cloned().fold(f64::MAX, f64::min);
    let sh_hi = sharp.iter().cloned().fold(f64::MIN, f64::max);
    // The park is among the strongly-left frames; pick the sharpest of them.
    let mut best = 0usize;
    let mut best_sharp = f64::MIN;
    for (i, (&l, &s)) in left.iter().zip(sharp.iter()).enumerate() {
        if l >= cfg.select_candidate_frac * l_max && s > best_sharp {
            best_sharp = s;
            best = i;
        }
    }
    let conf = round_to(
        ((outlier - 1.5) / 4.0).min(1.0) * 0.6 + norm(sharp[best], sh_lo, sh_hi) * 0.4,
        3,
    );
    if conf < cfg.min_confidence {
        return Selection::Skipped {
            reason: SkipReason::LowConfidence,
            confidence: conf,
        };
    }
    Selection::Selected {
        offset_ms: frames[best].offset_ms,
        confidence: conf,
    }
}

/// A picked frame for one layer's segment: which stream frame index to keep (→ its ring
/// JPEG) and the selection confidence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentPick {
    pub layer: i64,
    pub idx: u64,
    pub confidence: f64,
}

struct Segment {
    layer: i64,
    start_ms: u64,
    frames: Vec<(u64, u64, Vec<u8>)>, // (offset_ms, stream idx, gray)
}

/// Segment the CONTINUOUS camera stream by print layer and pick the parked frame per layer
/// with [`select_park_frame`] — the dense-stream alternative to the sparse snapshot burst.
///
/// The native park runs at the LAYER CHANGE: a brief (~0.5 s) far-left dwell whose delay vs
/// the `layer_num` edge is large and WILDLY variable (it's the layer-change gcode, so it
/// lands near the layer's end relative to the edge, drifting with layer time). A sparse grid
/// of single grabs misses it, and so does a short fixed window after the edge — the very
/// failure that defeated the snapshot burst. The robust answer is to accumulate the WHOLE
/// layer (every frame until the next layer edge) and let the MEDIAN subtraction isolate the
/// transient far-left head against the layer's typical printing frames. So `window_ms` is a
/// generous SAFETY CAP (max accumulation before forcing a selection, to bound memory if the
/// A1's `layer_num` sticks), NOT a gate: in normal operation the next layer edge finalizes
/// the segment first, well within the cap. Feed gray frames as they arrive (their stream
/// `idx`, capture time, and the current layer); each layer's frames accumulate until the next
/// layer (or the cap), then the selector runs over them and emits the chosen frame's index.
/// Pure (no I/O): the caller owns ffmpeg + the ring JPEGs and copies the picked index out.
pub struct SegmentSelector {
    w: usize,
    h: usize,
    /// Safety cap (ms): the max a single layer's frames accumulate before a forced selection.
    /// Normally the next layer edge finalizes the segment first; this only bites when
    /// `layer_num` stalls, so it's set well above any real layer time.
    window_ms: u64,
    cfg: SelectTuning,
    pending: Option<Segment>,
    done_layer: Option<i64>,
}

impl SegmentSelector {
    pub fn new(w: usize, h: usize, window_ms: u64, cfg: SelectTuning) -> Self {
        Self {
            w,
            h,
            window_ms,
            cfg,
            pending: None,
            done_layer: None,
        }
    }

    /// Feed one stream frame. Returns a [`SegmentPick`] when a layer's window CLOSES with a
    /// selected park — finalized either when the NEXT layer starts or `window_ms` elapses.
    /// Frames after a layer's window (until the next layer) are ignored. `None` otherwise.
    pub fn push(&mut self, layer: i64, idx: u64, t_ms: u64, gray: Vec<u8>) -> Option<SegmentPick> {
        if let Some(seg) = &mut self.pending {
            if seg.layer == layer {
                let off = t_ms.saturating_sub(seg.start_ms);
                if off <= self.window_ms {
                    seg.frames.push((off, idx, gray));
                    return None;
                }
                // Window elapsed → finalize; ignore later same-layer frames until next layer.
                let pick = self.finalize();
                self.done_layer = Some(layer);
                return pick;
            }
            // A new layer arrived → finalize the old segment, open a fresh one.
            let pick = self.finalize();
            self.open(layer, idx, t_ms, gray);
            return pick;
        }
        if self.done_layer == Some(layer) {
            return None; // this layer's window already closed
        }
        self.open(layer, idx, t_ms, gray);
        None
    }

    /// Finalize the last open segment at stream end.
    pub fn finish(&mut self) -> Option<SegmentPick> {
        self.finalize()
    }

    fn open(&mut self, layer: i64, idx: u64, t_ms: u64, gray: Vec<u8>) {
        self.pending = Some(Segment {
            layer,
            start_ms: t_ms,
            frames: vec![(0, idx, gray)],
        });
    }

    fn finalize(&mut self) -> Option<SegmentPick> {
        let seg = self.pending.take()?;
        let frames: Vec<SelectFrame> = seg
            .frames
            .iter()
            .map(|(off, _idx, g)| SelectFrame {
                offset_ms: *off,
                gray: g.clone(),
            })
            .collect();
        match select_park_frame(&frames, self.w, self.h, &self.cfg) {
            Selection::Selected {
                offset_ms,
                confidence,
            } => {
                let idx = seg
                    .frames
                    .iter()
                    .find(|(off, _, _)| *off == offset_ms)
                    .map(|(_, i, _)| *i)?;
                Some(SegmentPick {
                    layer: seg.layer,
                    idx,
                    confidence,
                })
            }
            Selection::Skipped { .. } => None,
        }
    }
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

#[cfg(test)]
mod select_tests {
    //! Mirrors the skill's test_select_smooth.py: synthetic grayscale bursts (bright bed,
    //! a STATIC dark object that must not be mistaken for the head, a dark moving head that
    //! is over-print in the majority and parks far-left in a minority).
    use super::*;

    const SW: usize = 48;
    const SH: usize = 24;
    const BG: u8 = 200;
    const OBJ: u8 = 110;
    const HEAD: u8 = 25;
    const CENTER: i64 = 24;

    fn sframe(head_x: i64, sharp: bool, obj_x: i64) -> Vec<u8> {
        let head_hw: i64 = 5;
        let mut img = vec![BG; SW * SH];
        for y in 0..SH {
            let row = y * SW;
            let lo = (obj_x - 3).max(0) as usize;
            let hi = (obj_x + 3).min(SW as i64) as usize;
            for px in img[row + lo..row + hi].iter_mut() {
                *px = OBJ;
            }
            for x in 0..SW {
                let d = (x as i64 - head_x).abs();
                if d <= head_hw {
                    img[row + x] = HEAD;
                } else if !sharp && d <= head_hw + 3 {
                    img[row + x] = (HEAD as f64
                        + (d - head_hw) as f64 / 3.0 * (BG as f64 - HEAD as f64))
                        as u8;
                }
            }
        }
        img
    }

    fn sburst(specs: &[(u64, i64, bool)], obj_x: i64) -> Vec<SelectFrame> {
        specs
            .iter()
            .map(|&(o, hx, s)| SelectFrame {
                offset_ms: o,
                gray: sframe(hx, s, obj_x),
            })
            .collect()
    }

    fn ex() -> SelectTuning {
        SelectTuning {
            left_frac: 0.33,
            min_outlier: 2.5,
            min_left_density: 3.0,
            select_candidate_frac: 0.6,
            min_confidence: 0.40,
        }
    }

    fn sel(b: &[SelectFrame]) -> Selection {
        select_park_frame(b, SW, SH, &ex())
    }

    #[test]
    fn selects_far_left_parked() {
        let b = sburst(
            &[
                (300, CENTER, true),
                (500, CENTER, true),
                (700, 6, true),
                (900, 6, true),
                (1100, CENTER, true),
                (1300, CENTER, true),
            ],
            CENTER,
        );
        match sel(&b) {
            Selection::Selected { offset_ms, .. } => assert!(matches!(offset_ms, 700 | 900)),
            other => panic!("expected selected, got {other:?}"),
        }
    }

    #[test]
    fn all_over_print_is_skipped() {
        let b = sburst(
            &[
                (300, CENTER, true),
                (500, CENTER, true),
                (700, CENTER, true),
                (900, CENTER, true),
            ],
            CENTER,
        );
        assert!(matches!(sel(&b), Selection::Skipped { .. }));
    }

    #[test]
    fn prefers_sharp_park_over_blurred_more_left() {
        let b = sburst(
            &[
                (300, CENTER, true),
                (500, CENTER, true),
                (700, 3, false),
                (900, 8, true),
                (1100, CENTER, true),
                (1300, CENTER, true),
            ],
            CENTER,
        );
        assert_eq!(
            sel(&b),
            Selection::Selected {
                offset_ms: 900,
                confidence: match sel(&b) {
                    Selection::Selected { confidence, .. } => confidence,
                    _ => unreachable!(),
                },
            }
        );
    }

    #[test]
    fn static_dark_object_is_not_mistaken_for_head() {
        let b = sburst(
            &[
                (300, CENTER, true),
                (500, CENTER, true),
                (700, 6, true),
                (900, 6, true),
                (1100, CENTER, true),
                (1300, CENTER, true),
            ],
            CENTER,
        );
        assert!(matches!(sel(&b), Selection::Selected { .. }));
    }

    #[test]
    fn panned_camera_still_resolves_the_park() {
        let b = sburst(
            &[
                (300, CENTER + 10, true),
                (500, CENTER + 10, true),
                (700, 16, true),
                (900, 16, true),
                (1100, CENTER + 10, true),
                (1300, CENTER + 10, true),
            ],
            CENTER + 10,
        );
        match sel(&b) {
            Selection::Selected { offset_ms, .. } => assert!(matches!(offset_ms, 700 | 900)),
            other => panic!("expected selected, got {other:?}"),
        }
    }

    #[test]
    fn park_before_burst_is_skipped() {
        let b = sburst(
            &[
                (900, CENTER, true),
                (1100, CENTER, true),
                (1300, CENTER, true),
                (1500, CENTER, true),
            ],
            CENTER,
        );
        assert!(matches!(sel(&b), Selection::Skipped { .. }));
    }

    // ── bright-head park (the real front-right A1 framing) ──
    const DARK_BACKDROP: u8 = 80; // the purge bucket / wall filling the left of the frame
    const BRIGHT_HEAD: u8 = 240; // the WHITE extruder body, parked far-left

    /// A frame where the LEFT zone is a static DARK backdrop (bucket/wall) and the parked
    /// head is BRIGHT — the inverse polarity of [`sframe`]. Over-print frames leave the left
    /// zone dark; a park lands the bright head over it, so the park BRIGHTENS the left zone.
    fn sframe_bright(head_x: i64, sharp: bool) -> Vec<u8> {
        let head_hw: i64 = 5;
        let left_w = (SW as f64 * 0.33) as usize;
        let mut img = vec![BG; SW * SH];
        for y in 0..SH {
            let row = y * SW;
            for px in img[row..row + left_w].iter_mut() {
                *px = DARK_BACKDROP; // static dark left backdrop
            }
            for px in img[row + (CENTER as usize - 3)..row + (CENTER as usize + 3)].iter_mut() {
                *px = OBJ; // static center print object
            }
            for x in 0..SW {
                let d = (x as i64 - head_x).abs();
                if d <= head_hw {
                    img[row + x] = BRIGHT_HEAD;
                } else if !sharp && d <= head_hw + 3 {
                    let base = if x < left_w { DARK_BACKDROP } else { BG };
                    img[row + x] = (BRIGHT_HEAD as f64
                        + (d - head_hw) as f64 / 3.0 * (base as f64 - BRIGHT_HEAD as f64))
                        as u8;
                }
            }
        }
        img
    }

    #[test]
    fn selects_a_bright_head_park() {
        // Real hardware: on the front-right A1 framing the parked head is the white extruder
        // body over a darker backdrop, so it BRIGHTENS the left zone. A dark-only saliency
        // scored ~0 here and skipped every layer; the absolute-change saliency catches it.
        // (Validated against the live capture: dark density 2.5 → skip, abs density 9.2 → pick.)
        let specs = [
            (300u64, CENTER, true),
            (500, CENTER, true),
            (700, 6, true),
            (900, 6, true),
            (1100, CENTER, true),
            (1300, CENTER, true),
        ];
        let b: Vec<SelectFrame> = specs
            .iter()
            .map(|&(o, hx, s)| SelectFrame {
                offset_ms: o,
                gray: sframe_bright(hx, s),
            })
            .collect();
        match sel(&b) {
            Selection::Selected { offset_ms, .. } => {
                assert!(
                    matches!(offset_ms, 700 | 900),
                    "picks the bright park: {offset_ms}"
                )
            }
            other => panic!("the bright-head park must be selected, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod segment_tests {
    //! Software-only DEVICE MODEL of the native park: the head is over the print (center)
    //! most of the time and parks FAR-LEFT for a brief dwell at a per-layer-VARIABLE delay
    //! (the real device's jitter). Feeding the modeled continuous stream through
    //! SegmentSelector reproduces in software both the win (a frame-accurate dense stream
    //! catches the jittery park every layer) and the failure (too-coarse sampling misses
    //! the brief dwell) — no ffmpeg, camera, or printer needed.
    use super::*;

    const SW: usize = 48;
    const SH: usize = 24;
    const BG: u8 = 200;
    const OBJ: u8 = 110;
    const HEAD: u8 = 25;
    const CENTER: i64 = 24;

    fn sframe(head_x: i64) -> Vec<u8> {
        let head_hw: i64 = 5;
        let mut img = vec![BG; SW * SH];
        for y in 0..SH {
            let row = y * SW;
            for px in img[row + (CENTER as usize - 3)..row + (CENTER as usize + 3)].iter_mut() {
                *px = OBJ; // a static dark print object at center
            }
            for x in 0..SW {
                if (x as i64 - head_x).abs() <= head_hw {
                    img[row + x] = HEAD;
                }
            }
        }
        img
    }

    fn ex() -> SelectTuning {
        SelectTuning {
            left_frac: 0.33,
            min_outlier: 2.5,
            min_left_density: 3.0,
            select_candidate_frac: 0.6,
            min_confidence: 0.40,
        }
    }

    /// Model one layer's camera frames at `fps` over `layer_ms`: head over-print (center)
    /// except parked FAR-LEFT during `[park_at_ms, park_at_ms+park_dur_ms)`.
    fn sim_layer(
        park_at_ms: u64,
        park_dur_ms: u64,
        fps: u64,
        layer_ms: u64,
    ) -> Vec<(u64, Vec<u8>)> {
        let dt = (1000 / fps).max(1);
        let mut out = Vec::new();
        let mut t = 0u64;
        while t < layer_ms {
            let parked = t >= park_at_ms && t < park_at_ms + park_dur_ms;
            out.push((t, sframe(if parked { 6 } else { CENTER })));
            t += dt;
        }
        out
    }

    fn run(
        sel: &mut SegmentSelector,
        layers: &[(i64, u64, u64, u64, u64)], // (layer, park_at, park_dur, fps, layer_ms)
    ) -> Vec<SegmentPick> {
        let mut idx = 0u64;
        let mut base = 0u64;
        let mut picks = Vec::new();
        for &(layer, pa, pd, fps, lms) in layers {
            for (t, gray) in sim_layer(pa, pd, fps, lms) {
                if let Some(p) = sel.push(layer, idx, base + t, gray) {
                    picks.push(p);
                }
                idx += 1;
            }
            base += lms;
        }
        if let Some(p) = sel.finish() {
            picks.push(p);
        }
        picks
    }

    #[test]
    fn dense_stream_catches_the_jittery_park_each_layer() {
        // 10 fps, parks at WILDLY varying delays (what defeats a sparse fixed grid).
        let mut sel = SegmentSelector::new(SW, SH, 3000, ex());
        let layers: Vec<_> = [500u64, 2300, 900, 2400, 1500]
            .iter()
            .enumerate()
            .map(|(l, &pa)| (l as i64, pa, 500u64, 10u64, 4000u64))
            .collect();
        let picks = run(&mut sel, &layers);
        assert_eq!(picks.len(), 5, "one park caught per layer: {picks:?}");
        assert!(picks.iter().all(|p| p.confidence > 0.0), "{picks:?}");
        // picks are emitted in layer order
        assert_eq!(
            picks.iter().map(|p| p.layer).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
    }

    #[test]
    fn an_over_print_only_layer_is_skipped() {
        // Layer 0 never parks → no pick (a gap beats a head-over-print frame); layer 1 does.
        let mut sel = SegmentSelector::new(SW, SH, 3000, ex());
        let picks = run(
            &mut sel,
            &[(0, 99_999, 500, 10, 4000), (1, 800, 500, 10, 4000)],
        );
        assert_eq!(picks.len(), 1, "{picks:?}");
        assert_eq!(picks[0].layer, 1);
    }

    #[test]
    fn too_coarse_sampling_misses_the_brief_park() {
        // 1 fps (1000ms spacing) over a 300ms dwell tucked between samples → no frame lands
        // in the park → skipped. This is the real failure mode; the fix is dense sampling.
        let mut sel = SegmentSelector::new(SW, SH, 3000, ex());
        let picks = run(
            &mut sel,
            &[(0, 1300, 300, 1, 4000), (1, 1300, 300, 1, 4000)],
        );
        assert!(
            picks.is_empty(),
            "coarse sampling misses the brief park: {picks:?}"
        );
    }

    #[test]
    fn full_layer_window_catches_a_park_at_the_layer_change() {
        // The REAL native park is the LAYER-CHANGE gcode: it fires near the END of a layer
        // relative to the layer_num edge (here ~14s into a 16s layer), NOT in the first few
        // seconds. This is what actually defeated the capture on hardware.
        //
        // Layers: two that park late + a third (never parks) to provide the closing edge.
        let layers = [
            (0i64, 14_000u64, 500u64, 10u64, 16_000u64),
            (1, 14_000, 500, 10, 16_000),
            (2, 99_999, 500, 10, 16_000),
        ];
        // A short window closes long before the park → catches nothing (the hardware bug).
        let mut short = SegmentSelector::new(SW, SH, 3000, ex());
        assert!(
            run(&mut short, &layers).is_empty(),
            "a 3s window closes before the layer-change park"
        );
        // A full-layer window finalizes on the NEXT layer edge → the park is in the segment.
        let mut full = SegmentSelector::new(SW, SH, 120_000, ex());
        let picks = run(&mut full, &layers);
        assert_eq!(
            picks.iter().map(|p| p.layer).collect::<Vec<_>>(),
            vec![0, 1],
            "full-layer segmenting catches the late park each parked layer: {picks:?}"
        );
    }
}
