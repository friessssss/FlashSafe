//! Flash mitigation strategies.
//!
//! When the detector fires, a mitigation strategy is applied to the captured
//! frame before it is composited back onto the screen.

use crate::LuminanceFrame;
use std::time::{Duration, Instant};

/// Available mitigation modes.
#[derive(Debug, Clone, PartialEq)]
pub enum MitigationMode {
    /// Blend the frame toward a neutral grey (reduces contrast).
    Dim { factor: f32 },
    /// Replace the entire frame with a solid colour (maximum suppression).
    Blackout,
    /// No mitigation (passthrough). Used for testing.
    None,
}

impl Default for MitigationMode {
    fn default() -> Self {
        MitigationMode::Dim { factor: 0.3 }
    }
}

/// Apply a mitigation to raw BGRA pixel data in-place.
///
/// * `pixels` – mutable slice of raw bytes in BGRA order.
/// * `mode` – the mitigation to apply.
pub fn apply(pixels: &mut [u8], mode: &MitigationMode) {
    match mode {
        MitigationMode::None => {}
        MitigationMode::Blackout => {
            for chunk in pixels.chunks_mut(4) {
                chunk[0] = 0; // B
                chunk[1] = 0; // G
                chunk[2] = 0; // R
                // leave alpha intact
            }
        }
        MitigationMode::Dim { factor } => {
            let f = factor.clamp(0.0, 1.0);
            for chunk in pixels.chunks_mut(4) {
                chunk[0] = (chunk[0] as f32 * f) as u8;
                chunk[1] = (chunk[1] as f32 * f) as u8;
                chunk[2] = (chunk[2] as f32 * f) as u8;
            }
        }
    }
}

/// Compute a LuminanceFrame from raw BGRA bytes using the BT.709 coefficients.
pub fn bgra_to_luminance(width: u32, height: u32, pixels: &[u8]) -> LuminanceFrame {
    let expected = (width * height * 4) as usize;
    assert_eq!(pixels.len(), expected, "pixel buffer size mismatch");
    let data: Vec<f32> = pixels
        .chunks(4)
        .map(|p| {
            let b = p[0] as f32 / 255.0;
            let g = p[1] as f32 / 255.0;
            let r = p[2] as f32 / 255.0;
            // BT.709 relative luminance
            0.2126 * r + 0.7152 * g + 0.0722 * b
        })
        .collect();
    LuminanceFrame::new(width, height, data)
}

// ---------------------------------------------------------------------------
// MitigationFilter — stateful per-frame dimmer with optional ramp
// ---------------------------------------------------------------------------

/// Stateful frame-level mitigation filter.
///
/// `MitigationLevel` is a value in `[0.0, 1.0]`:
/// - `0.0` → passthrough (no change)
/// - `1.0` → full black (all RGB channels zeroed)
///
/// When `ramp_duration > 0` the effective level transitions smoothly between
/// the previous and target values, preventing abrupt visual cuts.
pub struct MitigationFilter {
    /// How long a full 0→1 or 1→0 ramp takes.  Set to `Duration::ZERO` for
    /// instant transitions.
    ramp_duration: Duration,
    /// Effective level used for the most-recent frame.
    current_level: f32,
    /// Timestamp of the most-recent `apply` call (used to advance the ramp).
    last_update: Option<Instant>,
}

impl MitigationFilter {
    /// Create a new filter.  `ramp_duration` controls fade-in / fade-out speed.
    pub fn new(ramp_duration: Duration) -> Self {
        Self {
            ramp_duration,
            current_level: 0.0,
            last_update: None,
        }
    }

    /// Apply mitigation in-place to a BGRA buffer.
    ///
    /// `target_level` is the desired mitigation level for this frame.
    /// The method advances `current_level` toward `target_level` (ramp),
    /// then multiplies each R/G/B channel by `(1.0 - current_level)`.
    /// Alpha is never modified.
    ///
    /// **Zero-copy fast path**: when `current_level` is exactly `0.0` and
    /// `target_level` is also `0.0`, the pixel buffer is not touched.
    pub fn apply(&mut self, data: &mut [u8], target_level: f32, timestamp: Instant) {
        let target = target_level.clamp(0.0, 1.0);
        self.advance_ramp(target, timestamp);
        self.last_update = Some(timestamp);

        if self.current_level == 0.0 {
            return; // zero-copy fast path
        }

        let factor = (1.0_f32 - self.current_level).clamp(0.0, 1.0);
        for px in data.chunks_mut(4) {
            px[0] = (px[0] as f32 * factor) as u8; // B
            px[1] = (px[1] as f32 * factor) as u8; // G
            px[2] = (px[2] as f32 * factor) as u8; // R
            // px[3] (alpha) left unchanged
        }
    }

    /// Advance the ramp toward `target_level` without modifying any pixel buffer.
    ///
    /// Use this when mitigation is applied via `GammaDimmer::set_level` rather
    /// than in-place pixel writes.  After calling `tick`, read the result with
    /// [`level`][Self::level] and pass it to the dimmer.
    pub fn tick(&mut self, target_level: f32, timestamp: Instant) {
        let target = target_level.clamp(0.0, 1.0);
        self.advance_ramp(target, timestamp);
        self.last_update = Some(timestamp);
    }

    /// The effective mitigation level used for the most-recent frame.
    pub fn level(&self) -> f32 {
        self.current_level
    }

    /// Advance `current_level` toward `target` based on elapsed time.
    fn advance_ramp(&mut self, target: f32, timestamp: Instant) {
        if self.ramp_duration.is_zero() {
            self.current_level = target;
            return;
        }
        let elapsed_secs = match self.last_update {
            Some(prev) => timestamp.duration_since(prev).as_secs_f32(),
            None => 0.0,
        };
        // step = fraction of the full ramp covered in this frame
        let step = elapsed_secs / self.ramp_duration.as_secs_f32();
        self.current_level = if target > self.current_level {
            (self.current_level + step).min(target)
        } else {
            (self.current_level - step).max(target)
        };
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn make_pixel(b: u8, g: u8, r: u8) -> Vec<u8> {
        vec![b, g, r, 255]
    }

    #[test]
    fn test_zero_attenuation_passthrough() {
        let mut filter = MitigationFilter::new(Duration::ZERO);
        let original = make_pixel(200, 150, 100);
        let mut data = original.clone();
        filter.apply(&mut data, 0.0, Instant::now());
        assert_eq!(data, original, "0% mitigation must not alter any channel");
    }

    #[test]
    fn test_50_percent_attenuation() {
        let mut filter = MitigationFilter::new(Duration::ZERO);
        let mut data = make_pixel(200, 100, 50);
        filter.apply(&mut data, 0.5, Instant::now());
        // factor = 0.5  →  B=100, G=50, R=25, A=255
        assert_eq!(data[0], 100, "B channel at 50%");
        assert_eq!(data[1], 50, "G channel at 50%");
        assert_eq!(data[2], 25, "R channel at 50%");
        assert_eq!(data[3], 255, "alpha must be unchanged");
    }

    #[test]
    fn test_full_black_attenuation() {
        let mut filter = MitigationFilter::new(Duration::ZERO);
        let mut data = make_pixel(200, 150, 100);
        filter.apply(&mut data, 1.0, Instant::now());
        assert_eq!(data[0], 0, "B at 100%");
        assert_eq!(data[1], 0, "G at 100%");
        assert_eq!(data[2], 0, "R at 100%");
        assert_eq!(data[3], 255, "alpha must be unchanged");
    }

    #[test]
    fn test_ramp_advances_level() {
        let ramp = Duration::from_millis(100);
        let mut filter = MitigationFilter::new(ramp);
        let base = Instant::now();

        // First frame: level starts at 0, target = 1.0
        filter.apply(&mut vec![255u8; 4], 1.0, base);
        assert_eq!(filter.level(), 0.0, "no ramp on first frame (no elapsed time)");

        // Second frame: 50 ms later → should be 50% of the way through ramp
        filter.apply(&mut vec![255u8; 4], 1.0, base + Duration::from_millis(50));
        let lvl = filter.level();
        assert!(
            (lvl - 0.5).abs() < 0.01,
            "after 50ms of 100ms ramp expect level ≈ 0.5, got {lvl}"
        );

        // Third frame: 100 ms later → ramp complete
        filter.apply(&mut vec![255u8; 4], 1.0, base + Duration::from_millis(150));
        assert_eq!(filter.level(), 1.0, "ramp should reach target after full duration");
    }

    #[test]
    fn test_alpha_always_preserved() {
        let mut filter = MitigationFilter::new(Duration::ZERO);
        let mut data = vec![128u8, 128, 128, 200]; // alpha = 200
        filter.apply(&mut data, 0.5, Instant::now());
        assert_eq!(data[3], 200, "alpha channel must never be modified");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blackout_zeroes_rgb() {
        let mut pixels = vec![200u8, 150, 100, 255]; // B G R A
        apply(&mut pixels, &MitigationMode::Blackout);
        assert_eq!(&pixels[..3], &[0, 0, 0]);
        assert_eq!(pixels[3], 255); // alpha unchanged
    }

    #[test]
    fn test_dim_halves_brightness() {
        let mut pixels = vec![200u8, 100, 50, 255];
        apply(&mut pixels, &MitigationMode::Dim { factor: 0.5 });
        assert_eq!(pixels[0], 100);
        assert_eq!(pixels[1], 50);
        assert_eq!(pixels[2], 25);
        assert_eq!(pixels[3], 255);
    }

    #[test]
    fn test_luminance_white_pixel() {
        let pixels = vec![255u8, 255, 255, 255]; // white BGRA
        let frame = bgra_to_luminance(1, 1, &pixels);
        assert!((frame.mean_luminance() - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_luminance_black_pixel() {
        let pixels = vec![0u8, 0, 0, 255];
        let frame = bgra_to_luminance(1, 1, &pixels);
        assert!(frame.mean_luminance().abs() < 0.001);
    }
}
