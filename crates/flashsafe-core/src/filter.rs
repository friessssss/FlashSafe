//! Brightness rise/fall limiter with area budgets and strobe hold — the
//! FlashSafe mitigation filter.
//!
//! This is the CPU reference implementation ([`PixelFilter`]). The GPU path
//! runs the same per-pixel step ([`apply_pixel`]) in a shader and the same
//! [`TileFilter`] on the CPU; the test suite scores the output with the WCAG
//! judge in [`crate::wcag`].
//!
//! # Model
//!
//! * **Per pixel** the state is the colour displayed last frame, `C` (linear
//!   RGB), with luminance `P`. For input `x` with luminance `L`:
//!   - brightening (`L > P`): `P' = P + s_rise·(L − P)`, output `x · P'/L` —
//!     the current picture, dimmed;
//!   - darkening (`L ≤ P`): output `lerp(C, x, s_fall)` — a crossfade, since a
//!     gain ≤ 1 can't hold a pixel brighter than a frame that went black.
//!
//!   Pixels whose brightness isn't changing are never touched, so static
//!   bright content (a title bar, a HUD) next to a flash is left alone while
//!   the flashing pixels beside it are fully limited.
//! * **Per tile** (a [`TILE_COLS`]×[`TILE_ROWS`] grid) the net change
//!   `L_tile − P_tile` is what gets budgeted. Detailed textures panning
//!   across the screen change many pixels but net out to ~zero, so they pass;
//!   a flash is a coherent net change. The per-pixel detail only decides
//!   *which* pixels absorb the limit.
//! * **Per region** (a full-size window of tiles about the size of WCAG's 10°
//!   visual field, shifted inward at screen edges), two token buckets bound
//!   the displayed average: over any interval `T` it may rise by at most
//!   `rise·(T + burst_secs)` and fall by at most `fall·(T + burst_secs)`.
//!   Each tile takes the strictest scale of any window covering it, so the
//!   bound holds for every region-sized window. A flash is a rise-then-fall
//!   pair, so bounding rises bounds how many ≥10% flashes per second can
//!   reach the screen; bounding falls turns harsh cuts to dark into short
//!   fades and keeps a single dark frame from becoming a long dip.
//! * **Strobe hold** — opposing transitions in a region's input luminance
//!   accumulate an activity score. Past `strobe_trigger` the region uses the
//!   much slower `hold_*` rates until `hold_secs` after the strobing stops.
//!
//! Per-tile scales are interpolated bilinearly per pixel. The rise scale is
//! eroded (3×3 min) and the fall scale dilated (3×3 max, ignoring tiles with
//! no darkening) first, so every pixel is limited at least as strictly as its
//! own tile requires and the bound survives interpolation.
//!
//! When the source stops changing mid-fade the engine must keep re-presenting
//! the last frame until [`TileFilter::is_settled`] — WGC only delivers frames
//! when content changes.

use serde::{Deserialize, Serialize};

use crate::tiles::{tile_stats_f32x4, TileStats, TILE_COLS, TILE_ROWS};
use crate::wcag::TransitionTracker;

/// Longest frame gap the filter integrates over. Larger gaps (the game stopped
/// presenting) would otherwise allow one big jump in a single displayed frame.
pub const MAX_DT: f32 = 1.0 / 30.0;
/// Time constant (s) of the strobe activity decay.
pub const ACTIVITY_TAU: f32 = 1.0;
/// Transition size (linear luminance) that counts toward strobe activity.
pub const ACTIVITY_THRESHOLD: f32 = 0.08;
/// Remaining per-tile change below which the display counts as caught up.
pub const SETTLE_EPS: f32 = 1e-3;
/// BT.709 / WCAG luminance weights for linear RGB.
pub const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

const EPS: f32 = 1e-5;

/// User-tunable filter parameters (presets live in [`crate::config`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FilterParams {
    /// Max increase of displayed region luminance per second (linear 0–1).
    pub rise_per_sec: f32,
    /// Rise limit while a region is in strobe hold.
    pub hold_rise_per_sec: f32,
    /// Max decrease of displayed region luminance per second.
    pub fall_per_sec: f32,
    /// Fall limit while a region is in strobe hold.
    pub hold_fall_per_sec: f32,
    /// Activity (≈ recent opposing transitions) that enters strobe hold.
    pub strobe_trigger: f32,
    /// How long hold lasts after strobing stops (s).
    pub hold_secs: f32,
    /// Lowest gain ever applied to a brightening pixel, so the picture never
    /// goes fully black.
    pub min_gain: f32,
    /// Horizontal region radius in tiles (vertical follows the grid aspect).
    pub region_radius: u32,
    /// Token-bucket depth, in seconds of the current rate.
    pub burst_secs: f32,
}

impl Default for FilterParams {
    fn default() -> Self {
        Self {
            rise_per_sec: 0.8,
            hold_rise_per_sec: 0.2,
            fall_per_sec: 2.5,
            hold_fall_per_sec: 0.5,
            strobe_trigger: 2.8,
            hold_secs: 1.5,
            min_gain: 0.03,
            region_radius: 5,
            burst_secs: 0.04,
        }
    }
}

impl FilterParams {
    pub fn clamp(&mut self) {
        self.rise_per_sec = self.rise_per_sec.clamp(0.05, 5.0);
        self.hold_rise_per_sec = self.hold_rise_per_sec.clamp(0.02, self.rise_per_sec);
        self.fall_per_sec = self.fall_per_sec.clamp(0.1, 20.0);
        self.hold_fall_per_sec = self.hold_fall_per_sec.clamp(0.05, self.fall_per_sec);
        self.strobe_trigger = self.strobe_trigger.clamp(1.0, 10.0);
        self.hold_secs = self.hold_secs.clamp(0.0, 10.0);
        // The floor passes `min_gain · ΔL` instantly; keep it well under the
        // 0.1 WCAG transition so it can't create flashes by itself.
        self.min_gain = self.min_gain.clamp(0.0, 0.06);
        self.region_radius = self.region_radius.clamp(0, TILE_COLS as u32 / 2);
        self.burst_secs = self.burst_secs.clamp(0.0, 0.1);
    }

    /// Vertical radius matching the grid's aspect ratio.
    pub fn region_radius_y(&self) -> u32 {
        ((self.region_radius as f32 * TILE_ROWS as f32 / TILE_COLS as f32).round() as u32)
            .min(self.region_radius)
    }
}

#[derive(Debug, Clone)]
struct RegionState {
    rise_tokens: f32,
    fall_tokens: f32,
    activity: f32,
    hold_left: f32,
    tracker: TransitionTracker,
}

/// Aggregate numbers for the UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterSummary {
    /// Mean over tiles of displayed / input luminance for brightening tiles.
    pub mean_gain: f32,
    /// Strongest current dimming of a brightening tile (1 = none).
    pub min_gain: f32,
    /// Fraction of regions currently in strobe hold.
    pub hold_fraction: f32,
    /// Count of distinct mitigation episodes since creation.
    pub events: u64,
}

/// Per-tile budget logic. Feed it tile statistics each frame and read back
/// per-tile rise / fall scales for the per-pixel step.
pub struct TileFilter {
    params: FilterParams,
    cols: usize,
    rows: usize,
    regions: Vec<RegionState>,
    luma: Vec<f32>,
    r_in: Vec<f32>,
    net_rise: Vec<f32>,
    net_fall: Vec<f32>,
    region_rise: Vec<f32>,
    region_fall: Vec<f32>,
    cov_rise: Vec<f32>,
    cov_fall: Vec<f32>,
    tile_rise: Vec<f32>,
    tile_fall: Vec<f32>,
    spent_rise: Vec<f32>,
    spent_fall: Vec<f32>,
    scratch: Vec<f32>,
    rise_scale: Vec<f32>,
    fall_scale: Vec<f32>,
    initialized: bool,
    settled: bool,
    in_event: bool,
    summary: FilterSummary,
}

impl TileFilter {
    pub fn new(params: FilterParams) -> Self {
        Self::with_grid(params, TILE_COLS, TILE_ROWS)
    }

    pub fn with_grid(mut params: FilterParams, cols: usize, rows: usize) -> Self {
        params.clamp();
        let n = cols * rows;
        let region = RegionState {
            rise_tokens: 0.0,
            fall_tokens: 0.0,
            activity: 0.0,
            hold_left: 0.0,
            tracker: TransitionTracker::new(ACTIVITY_THRESHOLD, 1.0),
        };
        Self {
            params,
            cols,
            rows,
            regions: vec![region; n],
            luma: vec![0.0; n],
            r_in: vec![0.0; n],
            net_rise: vec![0.0; n],
            net_fall: vec![0.0; n],
            region_rise: vec![0.0; n],
            region_fall: vec![0.0; n],
            cov_rise: vec![1.0; n],
            cov_fall: vec![1.0; n],
            tile_rise: vec![1.0; n],
            tile_fall: vec![1.0; n],
            spent_rise: vec![0.0; n],
            spent_fall: vec![0.0; n],
            scratch: vec![0.0; n],
            rise_scale: vec![1.0; n],
            fall_scale: vec![1.0; n],
            initialized: false,
            settled: true,
            in_event: false,
            summary: FilterSummary {
                mean_gain: 1.0,
                min_gain: 1.0,
                ..Default::default()
            },
        }
    }

    pub fn params(&self) -> &FilterParams {
        &self.params
    }

    pub fn set_params(&mut self, mut params: FilterParams) {
        params.clamp();
        self.params = params;
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Forget all history (new target, or after a long pause).
    pub fn reset(&mut self) {
        let events = self.summary.events;
        *self = Self::with_grid(self.params, self.cols, self.rows);
        self.summary.events = events;
    }

    /// Advance by `dt` seconds. `stats` holds this frame's per-tile means
    /// (`cols * rows`, row-major). Afterwards read [`Self::rise_scales`] and
    /// [`Self::fall_scales`].
    pub fn update(&mut self, stats: &[TileStats], dt: f32) {
        assert_eq!(stats.len(), self.cols * self.rows);
        let dt = dt.clamp(0.0, MAX_DT);
        let p = self.params;
        let (c, r) = (self.cols, self.rows);
        let rx = p.region_radius as usize;
        let ry = p.region_radius_y() as usize;

        if !self.initialized {
            for reg in &mut self.regions {
                reg.rise_tokens = p.rise_per_sec * p.burst_secs;
                reg.fall_tokens = p.fall_per_sec * p.burst_secs;
            }
            self.initialized = true;
        }

        let mut settled = true;
        for (i, s) in stats.iter().enumerate() {
            self.luma[i] = s.luma;
            self.net_rise[i] = (s.luma - s.shown).max(0.0);
            self.net_fall[i] = (s.shown - s.luma).max(0.0);
            settled &= s.rise <= SETTLE_EPS && s.fall <= SETTLE_EPS;
        }
        box_blur(&self.luma, &mut self.r_in, &mut self.scratch, c, r, rx, ry);
        box_blur(&self.net_rise, &mut self.region_rise, &mut self.scratch, c, r, rx, ry);
        box_blur(&self.net_fall, &mut self.region_fall, &mut self.scratch, c, r, rx, ry);

        // Per region: strobe activity → rates → token buckets → fitting scales.
        let decay = (-dt / ACTIVITY_TAU).exp();
        let mut holding = 0usize;
        for (i, reg) in self.regions.iter_mut().enumerate() {
            reg.activity *= decay;
            if reg.tracker.push(self.r_in[i]).is_some() {
                reg.activity += 1.0;
            }
            if reg.activity >= p.strobe_trigger {
                reg.hold_left = p.hold_secs;
            } else {
                reg.hold_left = (reg.hold_left - dt).max(0.0);
            }
            let (rise, fall) = if reg.hold_left > 0.0 {
                holding += 1;
                (p.hold_rise_per_sec, p.hold_fall_per_sec)
            } else {
                (p.rise_per_sec, p.fall_per_sec)
            };
            reg.rise_tokens = reg.rise_tokens.min(rise * p.burst_secs) + rise * dt;
            reg.fall_tokens = reg.fall_tokens.min(fall * p.burst_secs) + fall * dt;
            self.region_rise[i] = fit(reg.rise_tokens, self.region_rise[i]);
            self.region_fall[i] = fit(reg.fall_tokens, self.region_fall[i]);
        }
        settled &= holding == 0;

        // Each tile takes the strictest scale of any region covering it.
        min_over_covering_windows(&self.region_rise, &mut self.cov_rise, &mut self.scratch, c, r, rx, ry);
        min_over_covering_windows(&self.region_fall, &mut self.cov_fall, &mut self.scratch, c, r, rx, ry);

        // Per tile: allowed net change → per-pixel scales. Only one side is
        // limited per tile; the other moves fully, so the displayed tile mean
        // lands exactly on the allowed value.
        let mut sum_gain = 0.0;
        let mut min_gain = 1.0f32;
        for (i, s) in stats.iter().enumerate() {
            if s.luma >= s.shown {
                let allowed = self.cov_rise[i] * self.net_rise[i];
                self.tile_rise[i] = if s.rise > EPS { ((allowed + s.fall) / s.rise).min(1.0) } else { 1.0 };
                // No darkening wanted here: don't let it constrain neighbours' fall scale.
                self.tile_fall[i] = if s.fall > EPS { 1.0 } else { f32::NEG_INFINITY };
                self.spent_rise[i] = allowed;
                self.spent_fall[i] = 0.0;
                let gain = if s.luma > EPS { ((s.shown + allowed) / s.luma).min(1.0) } else { 1.0 };
                sum_gain += gain;
                min_gain = min_gain.min(gain);
            } else {
                let allowed = self.cov_fall[i] * self.net_fall[i];
                self.tile_rise[i] = 1.0;
                self.tile_fall[i] = if s.fall > EPS { ((s.rise + allowed) / s.fall).min(1.0) } else { 1.0 };
                self.spent_rise[i] = 0.0;
                self.spent_fall[i] = allowed;
                sum_gain += 1.0;
            }
        }
        neighbourhood(&self.tile_rise, &mut self.rise_scale, c, r, f32::INFINITY, f32::min);
        neighbourhood(&self.tile_fall, &mut self.fall_scale, c, r, f32::NEG_INFINITY, f32::max);
        for v in &mut self.fall_scale {
            if !v.is_finite() {
                *v = 1.0;
            }
        }

        // Spend what each region actually let through.
        box_blur(&self.spent_rise, &mut self.region_rise, &mut self.scratch, c, r, rx, ry);
        box_blur(&self.spent_fall, &mut self.region_fall, &mut self.scratch, c, r, rx, ry);
        for (i, reg) in self.regions.iter_mut().enumerate() {
            reg.rise_tokens = (reg.rise_tokens - self.region_rise[i]).max(0.0);
            reg.fall_tokens = (reg.fall_tokens - self.region_fall[i]).max(0.0);
        }

        // Episode counting with hysteresis so one flash counts once.
        if !self.in_event && min_gain < 0.75 {
            self.in_event = true;
            self.summary.events += 1;
        } else if self.in_event && min_gain > 0.95 && holding == 0 {
            self.in_event = false;
        }
        let n = stats.len().max(1) as f32;
        self.summary.mean_gain = sum_gain / n;
        self.summary.min_gain = min_gain;
        self.summary.hold_fraction = holding as f32 / n;
        self.settled = settled;
    }

    /// Per-tile scale for brightening pixels, ready for bilinear sampling.
    pub fn rise_scales(&self) -> &[f32] {
        &self.rise_scale
    }

    /// Per-tile crossfade factor for darkening pixels, ready for bilinear sampling.
    pub fn fall_scales(&self) -> &[f32] {
        &self.fall_scale
    }

    pub fn summary(&self) -> FilterSummary {
        self.summary
    }

    /// True when the last frame's display had caught up with its input and
    /// no region is holding — the engine can stop re-presenting.
    pub fn is_settled(&self) -> bool {
        self.settled
    }
}

#[inline]
fn fit(tokens: f32, want: f32) -> f32 {
    if want > tokens {
        tokens / want
    } else {
        1.0
    }
}

/// Luminance of linear RGB.
#[inline]
pub fn luminance(c: [f32; 3]) -> f32 {
    LUMA[0] * c[0] + LUMA[1] * c[1] + LUMA[2] * c[2]
}

/// The per-pixel step, shared with the GPU shader. `x` is this frame's
/// linear colour, `prev` the colour displayed last frame.
#[inline]
pub fn apply_pixel(x: [f32; 3], prev: [f32; 3], rise_scale: f32, fall_scale: f32, min_gain: f32) -> [f32; 3] {
    let l = luminance(x);
    let p = luminance(prev);
    if l > p {
        let shown = (p + rise_scale * (l - p)).max(min_gain * l);
        let k = shown / l;
        [x[0] * k, x[1] * k, x[2] * k]
    } else {
        let t = fall_scale;
        [
            prev[0] + (x[0] - prev[0]) * t,
            prev[1] + (x[1] - prev[1]) * t,
            prev[2] + (x[2] - prev[2]) * t,
        ]
    }
}

/// CPU reference of the whole filter on linear-RGB frames.
pub struct PixelFilter {
    tiles: TileFilter,
    width: usize,
    height: usize,
    prev: Vec<[f32; 3]>,
    stats_img: Vec<f32>,
    stats: Vec<TileStats>,
    initialized: bool,
}

impl PixelFilter {
    pub fn new(params: FilterParams, width: usize, height: usize) -> Self {
        Self {
            tiles: TileFilter::new(params),
            width,
            height,
            prev: vec![[0.0; 3]; width * height],
            stats_img: vec![0.0; width * height * 4],
            stats: vec![TileStats::default(); TILE_COLS * TILE_ROWS],
            initialized: false,
        }
    }

    pub fn tiles(&self) -> &TileFilter {
        &self.tiles
    }

    /// Filter one frame (`width * height` linear RGB, row-major) that
    /// arrived `dt` seconds after the previous one. Returns the displayed frame.
    pub fn process(&mut self, input: &[[f32; 3]], dt: f32) -> &[[f32; 3]] {
        assert_eq!(input.len(), self.width * self.height);
        if !self.initialized {
            self.prev.copy_from_slice(input);
            self.initialized = true;
        }
        for (i, (x, c)) in input.iter().zip(&self.prev).enumerate() {
            let (l, p) = (luminance(*x), luminance(*c));
            self.stats_img[i * 4..i * 4 + 4].copy_from_slice(&[l, p, (l - p).max(0.0), (p - l).max(0.0)]);
        }
        let (w, h) = (self.width, self.height);
        tile_stats_f32x4(&self.stats_img, w * 4, w, h, TILE_COLS, TILE_ROWS, &mut self.stats);
        self.tiles.update(&self.stats, dt);
        let min_gain = self.tiles.params().min_gain;
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let sr = sample_tile(self.tiles.rise_scales(), TILE_COLS, TILE_ROWS, x, y, w, h);
                let sf = sample_tile(self.tiles.fall_scales(), TILE_COLS, TILE_ROWS, x, y, w, h);
                self.prev[i] = apply_pixel(input[i], self.prev[i], sr, sf, min_gain);
            }
        }
        &self.prev
    }
}

/// The region window "centred" on index `k` along an axis of length `n`:
/// `2r + 1` wide, shifted inward at the edges so every window has full size
/// (a viewer's visual field near the screen edge is still full-size, it just
/// sits inside the screen). Inclusive bounds.
#[inline]
pub fn window(k: usize, n: usize, r: usize) -> (usize, usize) {
    let w = (2 * r + 1).min(n);
    let lo = k.saturating_sub(r).min(n - w);
    (lo, lo + w - 1)
}

/// Mean over each region window (separable). The "region" used by both the
/// filter and the tests' judge.
pub fn box_blur(src: &[f32], dst: &mut [f32], scratch: &mut [f32], c: usize, r: usize, rx: usize, ry: usize) {
    for y in 0..r {
        for x in 0..c {
            let (x0, x1) = window(x, c, rx);
            let s: f32 = src[y * c + x0..=y * c + x1].iter().sum();
            scratch[y * c + x] = s / (x1 - x0 + 1) as f32;
        }
    }
    for y in 0..r {
        let (y0, y1) = window(y, r, ry);
        for x in 0..c {
            let s: f32 = (y0..=y1).map(|yy| scratch[yy * c + x]).sum();
            dst[y * c + x] = s / (y1 - y0 + 1) as f32;
        }
    }
}

/// For each tile, the minimum of `src` over every region window that
/// contains the tile (separable, since windows are axis-aligned boxes).
pub fn min_over_covering_windows(src: &[f32], dst: &mut [f32], scratch: &mut [f32], c: usize, r: usize, rx: usize, ry: usize) {
    let covering_min = |t: usize, n: usize, rad: usize, get: &dyn Fn(usize) -> f32| -> f32 {
        let (k0, k1) = (t.saturating_sub(2 * rad), (t + 2 * rad).min(n - 1));
        (k0..=k1)
            .filter(|&k| {
                let (lo, hi) = window(k, n, rad);
                (lo..=hi).contains(&t)
            })
            .map(get)
            .fold(f32::INFINITY, f32::min)
    };
    for y in 0..r {
        for x in 0..c {
            scratch[y * c + x] = covering_min(x, c, rx, &|k| src[y * c + k]);
        }
    }
    for y in 0..r {
        for x in 0..c {
            dst[y * c + x] = covering_min(y, r, ry, &|k| scratch[k * c + x]);
        }
    }
}

/// 3×3 neighbourhood reduction (clamped at edges).
fn neighbourhood(src: &[f32], dst: &mut [f32], c: usize, r: usize, init: f32, op: fn(f32, f32) -> f32) {
    for y in 0..r {
        for x in 0..c {
            let mut acc = init;
            for yy in y.saturating_sub(1)..=(y + 1).min(r - 1) {
                for xx in x.saturating_sub(1)..=(x + 1).min(c - 1) {
                    acc = op(acc, src[yy * c + xx]);
                }
            }
            dst[y * c + x] = acc;
        }
    }
}

/// Bilinear sample of a tile grid at pixel centre `(px, py)` of a
/// `width × height` frame, matching a GPU linear sampler with clamp
/// addressing over a `cols × rows` texture.
pub fn sample_tile(grid: &[f32], cols: usize, rows: usize, px: usize, py: usize, width: usize, height: usize) -> f32 {
    let fx = ((px as f32 + 0.5) / width as f32 * cols as f32 - 0.5).clamp(0.0, (cols - 1) as f32);
    let fy = ((py as f32 + 0.5) / height as f32 * rows as f32 - 0.5).clamp(0.0, (rows - 1) as f32);
    let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(cols - 1), (y0 + 1).min(rows - 1));
    let (ax, ay) = (fx - x0 as f32, fy - y0 as f32);
    let top = grid[y0 * cols + x0] * (1.0 - ax) + grid[y0 * cols + x1] * ax;
    let bot = grid[y1 * cols + x0] * (1.0 - ax) + grid[y1 * cols + x1] * ax;
    top * (1.0 - ay) + bot * ay
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wcag::FlashJudge;

    const C: usize = TILE_COLS;
    const R: usize = TILE_ROWS;

    fn grey(l: f32) -> [f32; 3] {
        [l, l, l]
    }

    fn square(t: f64, hz: f64) -> bool {
        ((t * hz * 2.0).floor() as i64) % 2 == 0
    }

    fn presets() -> [(&'static str, FilterParams); 3] {
        use crate::config::Preset;
        [("low", Preset::Low.params()), ("medium", Preset::Medium.params()), ("high", Preset::High.params())]
    }

    /// One simulated run: `f(t, x, y)` gives each input pixel, `check` sees
    /// every (time, input, output) frame, and `run` returns the worst WCAG
    /// flash count over all regions of the *displayed* output.
    struct Sim {
        params: FilterParams,
        w: usize,
        h: usize,
        fps: f64,
        secs: f64,
        jitter: bool,
    }

    impl Sim {
        fn new(params: FilterParams) -> Self {
            // 2×2 pixels per tile.
            Self { params, w: C * 2, h: R * 2, fps: 60.0, secs: 3.0, jitter: false }
        }

        fn run(
            &self,
            f: impl Fn(f64, usize, usize) -> [f32; 3],
            mut check: impl FnMut(f64, &[[f32; 3]], &[[f32; 3]]),
        ) -> f32 {
            let (w, h) = (self.w, self.h);
            let mut pf = PixelFilter::new(self.params, w, h);
            let p = *pf.tiles().params();
            let (rx, ry) = (p.region_radius as usize, p.region_radius_y() as usize);
            let mut judges: Vec<FlashJudge> = (0..C * R).map(|_| FlashJudge::default()).collect();
            let mut input = vec![[0.0; 3]; w * h];
            let (mut img, mut stats) = (vec![0.0; w * h * 4], vec![TileStats::default(); C * R]);
            let (mut tl, mut region, mut scratch) = (vec![0.0; C * R], vec![0.0; C * R], vec![0.0; C * R]);
            let (mut t, mut prev_t, mut i) = (0.0f64, 0.0f64, 0u64);
            while t < self.secs {
                for y in 0..h {
                    for x in 0..w {
                        input[y * w + x] = f(t, x, y);
                    }
                }
                let dt = if i == 0 { 1.0 / self.fps } else { t - prev_t } as f32;
                let out = pf.process(&input, dt).to_vec();
                check(t, &input, &out);
                // Judge the displayed luminance over every region.
                for (k, o) in out.iter().enumerate() {
                    img[k * 4] = luminance(*o);
                }
                tile_stats_f32x4(&img, w * 4, w, h, C, R, &mut stats);
                for (d, s) in tl.iter_mut().zip(&stats) {
                    *d = s.luma;
                }
                box_blur(&tl, &mut region, &mut scratch, C, R, rx, ry);
                for (j, v) in judges.iter_mut().zip(&region) {
                    j.push(t, *v);
                }
                prev_t = t;
                i += 1;
                // Deterministic jitter: frame times vary ±40%.
                let k = if self.jitter { 0.6 + 0.8 * ((i * 7919 % 101) as f64 / 100.0) } else { 1.0 };
                t += k / self.fps;
            }
            judges.iter().map(|j| j.max_flashes_in_window()).fold(0.0, f32::max)
        }
    }

    fn lum_at(frame: &[[f32; 3]], w: usize, x: usize, y: usize) -> f32 {
        luminance(frame[y * w + x])
    }

    #[test]
    fn input_strobe_really_fails_wcag() {
        let mut j = FlashJudge::default();
        for i in 0..240 {
            let t = i as f64 / 60.0;
            j.push(t, if square(t, 10.0) { 1.0 } else { 0.02 });
        }
        assert!(!j.passes());
    }

    #[test]
    fn full_screen_strobe_is_neutralised_for_every_preset_and_frame_rate() {
        for (name, p) in presets() {
            for &fps in &[30.0, 60.0, 144.0] {
                for &hz in &[4.0, 10.0, 20.0] {
                    for &jitter in &[false, true] {
                        let sim = Sim { fps, jitter, ..Sim::new(p) };
                        let worst = sim.run(|t, _, _| grey(if square(t, hz) { 1.0 } else { 0.02 }), |_, _, _| {});
                        assert!(worst <= 3.0, "{name} fps {fps} hz {hz} jitter {jitter}: {worst} flashes/s");
                    }
                }
            }
        }
    }

    #[test]
    fn red_strobe_is_neutralised() {
        for (name, p) in presets() {
            let worst = Sim::new(p).run(|t, _, _| if square(t, 8.0) { [1.0, 0.0, 0.0] } else { [0.0; 3] }, |_, _, _| {});
            assert!(worst <= 3.0, "{name}: {worst}");
        }
    }

    #[test]
    fn localized_strobe_is_neutralised_and_far_corner_untouched() {
        let worst = Sim::new(FilterParams::default()).run(
            |t, x, y| {
                let (tx, ty) = (x / 2, y / 2);
                if (4..20).contains(&tx) && (3..13).contains(&ty) {
                    grey(if square(t, 12.0) { 1.0 } else { 0.0 })
                } else {
                    grey(0.2)
                }
            },
            |_, input, out| {
                let k = input.len() - 1;
                assert_eq!(input[k], out[k]);
            },
        );
        assert!(worst <= 3.0, "{worst}");
    }

    #[test]
    fn dark_strobe_on_bright_scene_is_neutralised() {
        for (name, p) in presets() {
            let worst = Sim::new(p).run(|t, _, _| grey(if square(t, 8.0) { 0.7 } else { 0.0 }), |_, _, _| {});
            assert!(worst <= 3.0, "{name}: {worst}");
        }
    }

    /// Regression: a static bright band (a title bar) in the top half of the
    /// first tile row, the rest of the screen flashing. The band must be left
    /// alone and the flashing pixels beside it limited like everywhere else —
    /// previously the shared tile gain left them bright, painting a bar.
    #[test]
    fn static_band_next_to_flash_leaves_no_bright_bar() {
        let p = crate::config::Preset::High.params();
        let w = C * 2;
        let worst = Sim::new(p).run(
            |t, _, y| grey(if y == 0 { 0.8 } else if square(t, 2.0) { 0.02 } else { 1.0 }),
            |_, input, out| {
                for x in 0..w {
                    assert_eq!(out[x], input[x], "band pixel {x} was altered");
                    let (row0, row1) = (lum_at(out, w, x, 1), lum_at(out, w, x, 2));
                    assert!(row0 <= row1 + 0.02, "bar at x {x}: {row0} vs {row1}");
                }
            },
        );
        assert!(worst <= 3.0, "{worst}");
    }

    #[test]
    fn static_hud_bar_over_strobe_is_untouched() {
        let w = C * 2;
        let hud = 16..18; // pixel rows of tile row 8
        let worst = Sim::new(FilterParams::default()).run(
            |t, _, y| grey(if hud.contains(&y) || square(t, 10.0) { 1.0 } else { 0.0 }),
            |_, input, out| {
                for x in 0..w {
                    for y in hud.clone() {
                        assert_eq!(out[y * w + x], input[y * w + x]);
                    }
                    // Pixels right next to the HUD are limited like the rest.
                    let (near, far) = (lum_at(out, w, x, 15), lum_at(out, w, x, 6));
                    assert!(near <= far + 0.02, "halo at x {x}: {near} vs {far}");
                }
            },
        );
        assert!(worst <= 3.0, "{worst}");
    }

    #[test]
    fn single_camera_flash_is_ramped() {
        let mut peak = 0.0f32;
        Sim { secs: 1.0, ..Sim::new(FilterParams::default()) }.run(
            |t, _, _| grey(if (0.5..0.55).contains(&t) { 1.0 } else { 0.03 }),
            |_, _, out| peak = peak.max(luminance(out[0])),
        );
        assert!(peak < 0.15, "flash reached {peak}");
    }

    #[test]
    fn normal_gameplay_is_not_dimmed_or_smeared() {
        // A bright object sweeping a dark scene (including the edges) during a
        // 2-second fade-in, for each preset and frame rate.
        for (name, p) in presets() {
            for &fps in &[60.0, 144.0] {
                Sim { fps, ..Sim::new(p) }.run(
                    |t, x, y| {
                        let obj_x = (t * 10.0) as usize % C;
                        let base = (t / 2.0).min(1.0) as f32 * 0.3;
                        grey(if x / 2 == obj_x && y / 2 == R / 2 { 1.0 } else { base })
                    },
                    |t, input, out| {
                        if t < 0.1 {
                            return;
                        }
                        for (i, o) in input.iter().zip(out) {
                            let (li, lo) = (luminance(*i), luminance(*o));
                            assert!(lo >= li * 0.85 - 1e-3, "{name} @ {fps}: dimmed {li} → {lo}");
                            assert!(lo <= li + 0.05, "{name} @ {fps}: smeared {li} → {lo}");
                        }
                    },
                );
            }
        }
    }

    #[test]
    fn dark_object_on_bright_scene_is_not_smeared() {
        Sim::new(FilterParams::default()).run(
            |t, x, y| {
                let obj_x = (t * 10.0) as usize % C;
                grey(if x / 2 == obj_x && y / 2 == R / 2 { 0.0 } else { 0.7 })
            },
            |_, input, out| {
                for (i, o) in input.iter().zip(out) {
                    assert!((luminance(*i) - luminance(*o)).abs() < 0.02);
                }
            },
        );
    }

    #[test]
    fn detailed_texture_panning_passes_through() {
        // Pixel-scale noise scrolling one pixel per frame: every pixel changes
        // a lot, but no area gets brighter or darker. 8×8 px tiles.
        let noise = |x: usize, y: usize| -> f32 {
            let h = (x as u32).wrapping_mul(374_761_393) ^ (y as u32).wrapping_mul(668_265_263);
            let h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
            0.1 + 0.8 * ((h >> 8) & 0xff) as f32 / 255.0
        };
        let (w, h) = (C * 8, R * 8);
        let sim = Sim { w, h, secs: 1.5, ..Sim::new(FilterParams::default()) };
        let mut worst_err = 0.0f32;
        sim.run(
            |t, x, y| grey(noise(x + (t * 60.0).round() as usize, y)),
            |t, input, out| {
                if t < 0.1 {
                    return;
                }
                let err: f32 = input.iter().zip(out).map(|(i, o)| (luminance(*i) - luminance(*o)).abs()).sum::<f32>()
                    / input.len() as f32;
                worst_err = worst_err.max(err);
            },
        );
        assert!(worst_err < 0.02, "pan distorted by {worst_err} on average");
    }

    #[test]
    fn single_dark_frame_is_a_short_shallow_dip() {
        let p = FilterParams::default();
        let (mut min_seen, mut frames_low, mut worst_recovery) = (1.0f32, 0u32, 0u32);
        // Isolated dark frames, 2 s apart (more often starts to look like a
        // slow strobe, which strobe hold deliberately treats more strictly).
        Sim { secs: 4.5, ..Sim::new(p) }.run(
            |t, _, _| {
                let frame = (t * 60.0).round() as i64;
                grey(if frame % 120 == 30 { 0.0 } else { 0.7 })
            },
            |_, _, out| {
                let l = luminance(out[0]);
                min_seen = min_seen.min(l);
                if l < 0.69 {
                    frames_low += 1;
                    worst_recovery = worst_recovery.max(frames_low);
                } else {
                    frames_low = 0;
                }
            },
        );
        let max_dip = p.fall_per_sec * (1.0 / 60.0 + p.burst_secs) + 0.01;
        assert!(min_seen >= 0.7 - max_dip, "dipped to {min_seen}");
        assert!(worst_recovery <= 15, "took {worst_recovery} frames to recover");
    }

    #[test]
    fn cut_to_black_fades_out() {
        let p = FilterParams::default();
        let mut last = 1.0f32;
        let mut black_at = None;
        Sim { secs: 1.5, ..Sim::new(p) }.run(
            |t, _, _| grey(if t < 0.5 { 0.7 } else { 0.0 }),
            |t, _, out| {
                let l = luminance(out[0]);
                if t >= 0.5 {
                    assert!(l <= last + 1e-6, "brightened while fading");
                    assert!(last - l <= p.fall_per_sec * (1.0 / 60.0 + p.burst_secs) + 1e-3, "fell too fast");
                    if l < 0.01 && black_at.is_none() {
                        black_at = Some(t - 0.5);
                    }
                }
                last = l;
            },
        );
        let took = black_at.expect("never reached black");
        assert!(took <= 0.7 / p.fall_per_sec as f64 + 0.1, "fade took {took}s");
    }

    #[test]
    fn scene_cut_to_bright_ramps_up_and_settles() {
        let p = FilterParams::default();
        let (mut first_after_cut, mut last_out) = (None, 0.0);
        Sim { w: C, h: R, ..Sim::new(p) }.run(
            |t, _, _| grey(if t < 0.5 { 0.02 } else { 0.8 }),
            |t, _, out| {
                let l = luminance(out[0]);
                if t >= 0.5 && first_after_cut.is_none() {
                    first_after_cut = Some(l);
                }
                last_out = l;
            },
        );
        assert!(first_after_cut.unwrap() < 0.2, "cut should start dim");
        assert!((last_out - 0.8).abs() < 1e-3, "never caught up: {last_out}");
        // Settling is reported once input and display agree.
        let mut pf = PixelFilter::new(p, C, R);
        for _ in 0..3 {
            pf.process(&vec![grey(0.5); C * R], 1.0 / 60.0);
        }
        assert!(pf.tiles().is_settled());
    }

    #[test]
    fn strobe_hold_engages_and_releases() {
        let p = FilterParams::default();
        let mut pf = PixelFilter::new(p, C, R);
        for i in 0..120 {
            let t = i as f64 / 60.0;
            pf.process(&vec![grey(if square(t, 10.0) { 1.0 } else { 0.0 }); C * R], 1.0 / 60.0);
        }
        assert!(pf.tiles().summary().hold_fraction > 0.99);
        assert!(pf.tiles().summary().events >= 1);
        for _ in 0..(60.0 * (p.hold_secs + 4.0)) as usize {
            pf.process(&vec![grey(0.3); C * R], 1.0 / 60.0);
        }
        assert_eq!(pf.tiles().summary().hold_fraction, 0.0);
        assert!(pf.tiles().is_settled());
    }

    #[test]
    fn long_frame_gap_cannot_jump() {
        let p = FilterParams::default();
        let mut pf = PixelFilter::new(p, C, R);
        pf.process(&vec![grey(0.0); C * R], 1.0 / 60.0);
        let l = luminance(pf.process(&vec![grey(1.0); C * R], 2.0)[0]);
        let bound = (p.rise_per_sec * (MAX_DT + p.burst_secs)).max(p.min_gain);
        assert!(l <= bound + 1e-4, "{l} > {bound}");
    }

    #[test]
    fn apply_pixel_preserves_hue_and_never_brightens() {
        let prev = [0.1, 0.1, 0.1];
        let x = [0.9, 0.3, 0.1];
        let o = apply_pixel(x, prev, 0.25, 1.0, 0.0);
        assert!(luminance(o) < luminance(x));
        assert!((o[0] / o[1] - x[0] / x[1]).abs() < 1e-5, "hue changed");
        assert_eq!(apply_pixel(x, prev, 1.0, 1.0, 0.0), x);
        // Darkening crossfades from the previous output.
        let o = apply_pixel([0.0; 3], [0.8; 3], 1.0, 0.25, 0.0);
        assert!((o[0] - 0.6).abs() < 1e-6);
    }

    #[test]
    fn windows_are_full_size_and_cover() {
        for n in [1usize, 5, 18, 32] {
            for r in [0usize, 1, 3, 5] {
                for k in 0..n {
                    let (lo, hi) = window(k, n, r);
                    assert_eq!(hi - lo + 1, (2 * r + 1).min(n));
                    assert!(hi < n);
                    assert!((lo..=hi).contains(&k));
                }
            }
        }
    }

    #[test]
    fn sample_tile_matches_texel_centres() {
        let grid: Vec<f32> = (0..C * R).map(|i| i as f32).collect();
        // Pixel whose centre is the exact centre of tile (3, 2), 11 px tiles.
        let g = sample_tile(&grid, C, R, 3 * 11 + 5, 2 * 11 + 5, C * 11, R * 11);
        assert!((g - grid[2 * C + 3]).abs() < 1e-3);
        assert_eq!(sample_tile(&grid, C, R, 0, 0, C * 11, R * 11), grid[0]);
    }
}
