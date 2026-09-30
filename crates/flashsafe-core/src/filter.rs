//! Luminance slew limiter with strobe hold — the FlashSafe mitigation filter.
//!
//! This is the CPU reference implementation. The GPU path mirrors it, and the
//! test suite scores it with the WCAG judge in [`crate::wcag`].
//!
//! # Model
//!
//! Per frame, for every tile of a [`TILE_COLS`]×[`TILE_ROWS`] grid:
//!
//! 1. **Displayed tile luminance** `S` may *fall* to the input instantly, but
//!    may only *rise* toward it. A flash is a rise-then-fall pair, so bounding
//!    the rise bounds how many ≥10% flashes per second reach the screen.
//!    Tiles that aren't brightening are never touched.
//! 2. **Area budget** — rises are budgeted per *region* (a box of tiles about
//!    the size of WCAG's 10° visual field) with a token bucket: over any
//!    interval `T` the displayed region average may rise by at most
//!    `rate·(T + burst_secs)`. The small burst lets a single frame step of a
//!    small object through at high frame rates. Each region scales the rises it contains to
//!    fit its budget, and each tile takes the strictest scale of any region
//!    covering it. So a small bright object moving over a dark scene gets the
//!    whole budget and passes untouched, while a flash covering a meaningful
//!    area is ramped. The bound holds for every region-sized window.
//! 3. **Strobe hold** — opposing transitions in a region's input luminance
//!    accumulate an activity score. Once it crosses `strobe_trigger`, that
//!    region's budget drops to `hold_rise_per_sec` until `hold_secs` after
//!    the strobing stops.
//! 4. **Gain** `S / L_in` (≤ 1, ≥ `min_gain`) is applied to linear RGB,
//!    interpolated bilinearly between tile centres. Hue is preserved and the
//!    image is never brightened.
//!
//! When the source stops changing mid-ramp the engine must keep re-presenting
//! the last frame until [`TileFilter::is_settled`] — otherwise a bright scene
//! would stay dimmed until the game draws a new frame.

use serde::{Deserialize, Serialize};

use crate::luma::{linear_to_srgb, srgb8_to_linear};
use crate::tiles::{TILE_COLS, TILE_ROWS};
use crate::wcag::TransitionTracker;

/// Longest frame gap the filter integrates over. Larger gaps (the game stopped
/// presenting) would otherwise allow one big jump in a single displayed frame.
pub const MAX_DT: f32 = 1.0 / 30.0;
/// Time constant (s) of the strobe activity decay.
pub const ACTIVITY_TAU: f32 = 1.0;
/// Transition size (linear luminance) that counts toward strobe activity.
pub const ACTIVITY_THRESHOLD: f32 = 0.08;

/// User-tunable filter parameters (presets live in [`crate::config`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FilterParams {
    /// Max increase of displayed region luminance per second (linear 0–1).
    pub rise_per_sec: f32,
    /// Rise limit while a tile is in strobe hold.
    pub hold_rise_per_sec: f32,
    /// Activity (≈ recent opposing transitions) that enters strobe hold.
    pub strobe_trigger: f32,
    /// How long hold lasts after strobing stops (s).
    pub hold_secs: f32,
    /// Lowest gain ever applied, so the picture never goes fully black.
    pub min_gain: f32,
    /// Horizontal averaging radius in tiles (vertical follows the grid aspect).
    pub region_radius: u32,
    /// Token-bucket depth, in seconds of `rise_per_sec`.
    pub burst_secs: f32,
}

impl Default for FilterParams {
    fn default() -> Self {
        Self {
            rise_per_sec: 0.8,
            hold_rise_per_sec: 0.2,
            strobe_trigger: 1.8,
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
        self.strobe_trigger = self.strobe_trigger.clamp(1.0, 10.0);
        self.hold_secs = self.hold_secs.clamp(0.0, 10.0);
        // The floor passes `min_gain · ΔL` instantly; keep it well under the
        // 0.1 WCAG transition so it can't create flashes by itself.
        self.min_gain = self.min_gain.clamp(0.0, 0.06);
        self.burst_secs = self.burst_secs.clamp(0.0, 0.1);
        self.region_radius = self.region_radius.clamp(0, TILE_COLS as u32 / 2);
    }

    /// Vertical radius matching the grid's aspect ratio.
    pub fn region_radius_y(&self) -> u32 {
        ((self.region_radius as f32 * TILE_ROWS as f32 / TILE_COLS as f32).round() as u32)
            .min(self.region_radius)
    }
}

#[derive(Debug, Clone)]
struct TileState {
    /// Tile luminance displayed last frame.
    shown: f32,
    /// Rise budget left in the region centred on this tile.
    tokens: f32,
    /// Strobe state of the region centred on this tile.
    activity: f32,
    hold_left: f32,
    tracker: TransitionTracker,
}

/// Aggregate numbers for the UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterSummary {
    pub mean_gain: f32,
    pub min_gain: f32,
    /// Fraction of tiles currently in strobe hold.
    pub hold_fraction: f32,
    /// Count of distinct mitigation episodes since creation.
    pub events: u64,
}

/// Stateful per-tile filter. Feed tile luminance each frame, read back gains.
pub struct TileFilter {
    params: FilterParams,
    cols: usize,
    rows: usize,
    tiles: Vec<TileState>,
    r_in: Vec<f32>,
    demand: Vec<f32>,
    region_demand: Vec<f32>,
    scale: Vec<f32>,
    scratch: Vec<f32>,
    gains: Vec<f32>,
    initialized: bool,
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
        Self {
            params,
            cols,
            rows,
            tiles: vec![
                TileState {
                    shown: 0.0,
                    tokens: 0.0,
                    activity: 0.0,
                    hold_left: 0.0,
                    tracker: TransitionTracker::new(ACTIVITY_THRESHOLD, 1.0),
                };
                n
            ],
            r_in: vec![0.0; n],
            demand: vec![0.0; n],
            region_demand: vec![0.0; n],
            scale: vec![1.0; n],
            scratch: vec![0.0; n],
            gains: vec![1.0; n],
            initialized: false,
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

    /// Advance by `dt` seconds with this frame's tile luminance
    /// (`cols * rows`, row-major, linear). Returns per-tile gains.
    pub fn update(&mut self, tile_luma: &[f32], dt: f32) -> &[f32] {
        assert_eq!(tile_luma.len(), self.cols * self.rows);
        let dt = dt.clamp(0.0, MAX_DT);
        let (c, r) = (self.cols, self.rows);
        let rx = self.params.region_radius as usize;
        let ry = self.params.region_radius_y() as usize;
        box_blur(tile_luma, &mut self.r_in, &mut self.scratch, c, r, rx, ry);

        let p = self.params;
        if !self.initialized {
            for (t, &l) in self.tiles.iter_mut().zip(tile_luma) {
                t.shown = l;
                t.tokens = p.rise_per_sec * p.burst_secs;
            }
            self.initialized = true;
        }

        // Requested brightening per tile, averaged per region.
        for (d, (t, &l)) in self.demand.iter_mut().zip(self.tiles.iter().zip(tile_luma)) {
            *d = (l - t.shown).max(0.0);
        }
        box_blur(&self.demand, &mut self.region_demand, &mut self.scratch, c, r, rx, ry);

        // Per region: strobe activity → budget → scale that fits the budget.
        let decay = (-dt / ACTIVITY_TAU).exp();
        let mut holding = 0usize;
        for (i, t) in self.tiles.iter_mut().enumerate() {
            t.activity *= decay;
            if t.tracker.push(self.r_in[i]).is_some() {
                t.activity += 1.0;
            }
            if t.activity >= p.strobe_trigger {
                t.hold_left = p.hold_secs;
            } else {
                t.hold_left = (t.hold_left - dt).max(0.0);
            }
            let rate = if t.hold_left > 0.0 {
                holding += 1;
                p.hold_rise_per_sec
            } else {
                p.rise_per_sec
            };
            t.tokens = t.tokens.min(rate * p.burst_secs) + rate * dt;
            let want = self.region_demand[i];
            self.scale[i] = if want > t.tokens { t.tokens / want } else { 1.0 };
        }

        // Each tile takes the strictest scale of any region that covers it.
        min_over_covering_windows(&self.scale, &mut self.region_demand, &mut self.scratch, c, r, rx, ry);
        let mut sum_gain = 0.0;
        let mut min_gain = 1.0f32;
        for (i, t) in self.tiles.iter_mut().enumerate() {
            let l = tile_luma[i];
            // Falls pass through; rises are scaled to the budget.
            let s = (t.shown + self.region_demand[i] * self.demand[i]).min(l);
            let g = if l > 1e-4 { (s / l).clamp(p.min_gain, 1.0) } else { 1.0 };
            // Actual rise, for spending the region budgets below.
            self.demand[i] = (g * l - t.shown).max(0.0);
            t.shown = g * l;
            self.gains[i] = g;
            sum_gain += g;
            min_gain = min_gain.min(g);
        }
        box_blur(&self.demand, &mut self.region_demand, &mut self.scratch, c, r, rx, ry);
        for (t, spent) in self.tiles.iter_mut().zip(&self.region_demand) {
            t.tokens = (t.tokens - spent).max(0.0);
        }

        // Episode counting with hysteresis so one flash counts once.
        if !self.in_event && min_gain < 0.75 {
            self.in_event = true;
            self.summary.events += 1;
        } else if self.in_event && min_gain > 0.95 && holding == 0 {
            self.in_event = false;
        }
        let n = self.tiles.len().max(1) as f32;
        self.summary.mean_gain = sum_gain / n;
        self.summary.min_gain = min_gain;
        self.summary.hold_fraction = holding as f32 / n;
        &self.gains
    }

    pub fn gains(&self) -> &[f32] {
        &self.gains
    }

    pub fn summary(&self) -> FilterSummary {
        self.summary
    }

    /// True when the displayed image has caught up with the source and no
    /// tile is holding — the engine can stop re-presenting the last frame.
    pub fn is_settled(&self) -> bool {
        self.gains.iter().all(|&g| g > 0.999) && self.tiles.iter().all(|t| t.hold_left <= 0.0)
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

/// Bilinear sample of the gain grid at pixel centre `(px, py)` of a
/// `width × height` frame, matching a GPU linear sampler with clamp
/// addressing over a `cols × rows` texture.
pub fn sample_gain(gains: &[f32], cols: usize, rows: usize, px: usize, py: usize, width: usize, height: usize) -> f32 {
    let fx = ((px as f32 + 0.5) / width as f32 * cols as f32 - 0.5).clamp(0.0, (cols - 1) as f32);
    let fy = ((py as f32 + 0.5) / height as f32 * rows as f32 - 0.5).clamp(0.0, (rows - 1) as f32);
    let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(cols - 1), (y0 + 1).min(rows - 1));
    let (ax, ay) = (fx - x0 as f32, fy - y0 as f32);
    let top = gains[y0 * cols + x0] * (1.0 - ax) + gains[y0 * cols + x1] * ax;
    let bot = gains[y1 * cols + x0] * (1.0 - ax) + gains[y1 * cols + x1] * ax;
    top * (1.0 - ay) + bot * ay
}

/// Apply a gain grid to a BGRA frame in place (linear-light multiply).
/// CPU reference for the GPU apply pass.
pub fn apply_gains_bgra(data: &mut [u8], row_pitch: usize, width: usize, height: usize, gains: &[f32], cols: usize, rows: usize) {
    for y in 0..height {
        for x in 0..width {
            let g = sample_gain(gains, cols, rows, x, y, width, height);
            if g >= 0.999 {
                continue;
            }
            let i = y * row_pitch + x * 4;
            for c in &mut data[i..i + 3] {
                let lin = srgb8_to_linear(*c) * g;
                *c = (linear_to_srgb(lin) * 255.0 + 0.5) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::{solid_bgra, tile_luminance_bgra, BgraFrame};
    use crate::wcag::{FlashJudge, TransitionTracker};

    const C: usize = TILE_COLS;
    const R: usize = TILE_ROWS;

    /// Drive the filter with a per-tile luminance function and score the
    /// *displayed* luminance of every region with the WCAG judge.
    /// Returns (worst flashes-per-window over all regions, gain log, filter).
    fn run(
        params: FilterParams,
        fps: f64,
        secs: f64,
        jitter: bool,
        f: impl Fn(f64, usize, usize) -> f32,
    ) -> (f32, Vec<Vec<f32>>, TileFilter) {
        let mut filt = TileFilter::new(params);
        let (rx, ry) = (filt.params().region_radius as usize, filt.params().region_radius_y() as usize);
        let mut judges: Vec<FlashJudge> = (0..C * R).map(|_| FlashJudge::default()).collect();
        let mut gains_log = Vec::new();
        let (mut shown, mut region, mut scratch) = (vec![0.0; C * R], vec![0.0; C * R], vec![0.0; C * R]);
        let mut luma = vec![0.0f32; C * R];
        let (mut t, mut prev_t, mut i) = (0.0f64, 0.0f64, 0u64);
        while t < secs {
            for y in 0..R {
                for x in 0..C {
                    luma[y * C + x] = f(t, x, y);
                }
            }
            let dt = if i == 0 { 1.0 / fps } else { t - prev_t } as f32;
            let g = filt.update(&luma, dt).to_vec();
            for k in 0..C * R {
                shown[k] = luma[k] * g[k];
            }
            box_blur(&shown, &mut region, &mut scratch, C, R, rx, ry);
            for (j, v) in judges.iter_mut().zip(&region) {
                j.push(t, *v);
            }
            gains_log.push(g);
            prev_t = t;
            i += 1;
            // Deterministic jitter: frame times vary ±40%.
            let k = if jitter { 0.6 + 0.8 * ((i * 7919 % 101) as f64 / 100.0) } else { 1.0 };
            t += k / fps;
        }
        let worst = judges.iter().map(|j| j.max_flashes_in_window()).fold(0.0, f32::max);
        (worst, gains_log, filt)
    }

    fn square(t: f64, hz: f64) -> bool {
        ((t * hz * 2.0).floor() as i64) % 2 == 0
    }

    fn presets() -> [(&'static str, FilterParams); 3] {
        use crate::config::Preset;
        [("low", Preset::Low.params()), ("medium", Preset::Medium.params()), ("high", Preset::High.params())]
    }

    #[test]
    fn full_screen_strobe_is_neutralised_for_every_preset_and_frame_rate() {
        for (name, p) in presets() {
            for &fps in &[30.0, 60.0, 144.0] {
                for &hz in &[4.0, 10.0, 20.0] {
                    for &jitter in &[false, true] {
                        let (worst, _, _) = run(p, fps, 3.0, jitter, |t, _, _| {
                            if square(t, hz) { 1.0 } else { 0.02 }
                        });
                        assert!(worst <= 3.0, "{name} fps {fps} hz {hz} jitter {jitter}: {worst} flashes/s");
                    }
                }
            }
        }
    }

    #[test]
    fn red_strobe_is_neutralised() {
        // Saturated red vs black: ΔL ≈ 0.21, a WCAG general flash.
        let red = crate::luma::relative_luminance8(255, 0, 0);
        for (name, p) in presets() {
            let (worst, _, _) = run(p, 60.0, 3.0, false, |t, _, _| if square(t, 8.0) { red } else { 0.0 });
            assert!(worst <= 3.0, "{name}: {worst}");
        }
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
    fn localized_strobe_is_neutralised() {
        // Strobe in a block covering ~30% of the screen on a mid-grey scene.
        let (worst, gains, _) = run(FilterParams::default(), 60.0, 4.0, false, |t, x, y| {
            if (4..20).contains(&x) && (3..13).contains(&y) {
                if square(t, 12.0) { 1.0 } else { 0.0 }
            } else {
                0.2
            }
        });
        assert!(worst <= 3.0, "{worst}");
        // Far corner (outside every window that touches the strobe) is untouched.
        assert!(gains.last().unwrap()[(R - 1) * C + (C - 1)] > 0.99);
    }

    #[test]
    fn dark_strobe_on_bright_scene_is_neutralised() {
        let (worst, _, _) = run(FilterParams::default(), 60.0, 4.0, false, |t, _, _| {
            if square(t, 8.0) { 0.7 } else { 0.0 }
        });
        assert!(worst <= 3.0, "{worst}");
    }

    #[test]
    fn single_camera_flash_is_ramped() {
        // Dark scene, one 3-frame full-white flash.
        let p = FilterParams::default();
        let (_, gains, _) = run(p, 60.0, 1.0, false, |t, _, _| if (0.5..0.55).contains(&t) { 1.0 } else { 0.03 });
        // Displayed luminance during the flash may rise at most rise_per_sec·dt per frame.
        let flash_frame = gains.iter().map(|g| g[0]).fold(1.0, f32::min);
        assert!(flash_frame < 0.1, "flash gain {flash_frame}");
    }

    #[test]
    fn normal_gameplay_is_not_dimmed() {
        // A bright object sweeping across a dark scene (including the edges)
        // during a 2-second fade-in, for each preset and frame rate.
        for (name, p) in presets() {
            for &fps in &[60.0, 144.0] {
                let (_, gains, _) = run(p, fps, 3.0, false, |t, x, y| {
                    let obj_x = (t * 10.0) as usize % C;
                    let base = (t / 2.0).min(1.0) as f32 * 0.3;
                    if x == obj_x && y == R / 2 { 1.0 } else { base }
                });
                let worst = gains.iter().skip(5).flat_map(|g| g.iter()).fold(1.0f32, |a, &b| a.min(b));
                assert!(worst > 0.85, "{name} @ {fps}: normal content dimmed to {worst}");
            }
        }
    }

    #[test]
    fn scene_cut_to_bright_ramps_up_and_settles() {
        let p = FilterParams::default();
        let (_, gains, filt) = run(p, 60.0, 3.0, false, |t, _, _| if t < 0.5 { 0.02 } else { 0.8 });
        let at_cut = gains[31][0];
        assert!(at_cut < 0.2, "cut should start dim, got {at_cut}");
        // After ~0.8/rise seconds it must be fully restored.
        assert!(filt.is_settled());
        assert!(gains.last().unwrap().iter().all(|&g| g > 0.999));
    }

    #[test]
    fn strobe_hold_engages_and_releases() {
        let p = FilterParams::default();
        let mut filt = TileFilter::new(p);
        let n = C * R;
        for i in 0..120 {
            let t = i as f64 / 60.0;
            let l = if square(t, 10.0) { 1.0 } else { 0.0 };
            filt.update(&vec![l; n], 1.0 / 60.0);
        }
        assert!(filt.summary().hold_fraction > 0.99);
        assert!(filt.summary().events >= 1);
        for _ in 0..(60.0 * (p.hold_secs + 3.0)) as usize {
            filt.update(&vec![0.3; n], 1.0 / 60.0);
        }
        assert_eq!(filt.summary().hold_fraction, 0.0);
        assert!(filt.is_settled());
    }

    #[test]
    fn long_frame_gap_cannot_jump() {
        let mut filt = TileFilter::new(FilterParams::default());
        let n = C * R;
        filt.update(&vec![0.0; n], 1.0 / 60.0);
        let g = filt.update(&vec![1.0; n], 2.0)[0];
        let p = FilterParams::default();
        let bound = (p.rise_per_sec * (MAX_DT + p.burst_secs)).max(p.min_gain);
        assert!(g <= bound + 1e-4, "{g} > {bound}");
    }

    #[test]
    fn end_to_end_pixels() {
        // Real BGRA frames through tiles → filter → apply → tiles again.
        let (w, h) = (128usize, 72usize);
        let mut filt = TileFilter::new(FilterParams::default());
        let mut judge = FlashJudge::new(TransitionTracker::wcag(), 1.0);
        let mut tiles = vec![0.0; C * R];
        let mut out_tiles = vec![0.0; C * R];
        for i in 0..180 {
            let t = i as f64 / 60.0;
            let v = if square(t, 10.0) { 255 } else { 5 };
            let mut frame = solid_bgra(w, h, v, v, v);
            tile_luminance_bgra(BgraFrame::packed(&frame, w, h), C, R, 1, &mut tiles);
            let gains = filt.update(&tiles, 1.0 / 60.0).to_vec();
            apply_gains_bgra(&mut frame, w * 4, w, h, &gains, C, R);
            tile_luminance_bgra(BgraFrame::packed(&frame, w, h), C, R, 1, &mut out_tiles);
            judge.push(t, out_tiles.iter().sum::<f32>() / out_tiles.len() as f32);
        }
        assert!(judge.passes(), "{}", judge.max_flashes_in_window());
    }

    #[test]
    fn windows_are_full_size_and_cover() {
        for n in [1usize, 5, 18, 32] {
            for r in [0usize, 1, 3, 5] {
                for k in 0..n {
                    let (lo, hi) = window(k, n, r);
                    assert_eq!(hi - lo + 1, (2 * r + 1).min(n));
                    assert!(hi < n);
                    // Every tile is covered by its own window.
                    assert!((lo..=hi).contains(&k));
                }
            }
        }
    }

    #[test]
    fn sample_gain_matches_texel_centres() {
        let gains: Vec<f32> = (0..C * R).map(|i| i as f32).collect();
        // Pixel whose centre is the exact centre of tile (3, 2), 11 px tiles.
        let g = sample_gain(&gains, C, R, 3 * 11 + 5, 2 * 11 + 5, C * 11, R * 11);
        let expect = gains[2 * C + 3];
        assert!((g - expect).abs() < 1e-3, "{g} vs {expect}");
        // Corners clamp to the corner tiles.
        assert_eq!(sample_gain(&gains, C, R, 0, 0, C * 11, R * 11), gains[0]);
    }
}
