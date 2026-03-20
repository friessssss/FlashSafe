//! Flash detection logic.
//!
//! Implements a sliding-window luminance-change detector based on the
//! WCAG 2.1 / W3C flash criteria: a flash is a pair of opposing luminance
//! transitions where the relative luminance of the brighter state is ≥ 10%
//! of maximum and occurs more than 3 times per second.

use crate::LuminanceFrame;
use std::time::{Duration, Instant};
use thiserror::Error;

/// Errors from the detection subsystem.
#[derive(Debug, Error)]
pub enum DetectionError {
    #[error("frame buffer is empty")]
    EmptyBuffer,
}

/// Configuration for the flash detector.
#[derive(Debug, Clone)]
pub struct DetectorConfig {
    /// Number of frames per second the capture pipeline delivers.
    pub fps: f32,
    /// Maximum allowed flashes per second before mitigation triggers.
    pub max_flashes_per_second: f32,
    /// Minimum absolute luminance change (0–1) to count as a transition.
    pub luminance_change_threshold: f32,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            fps: 60.0,
            max_flashes_per_second: 3.0,
            luminance_change_threshold: 0.1,
        }
    }
}

/// Result of analysing a single new frame.
#[derive(Debug, Clone, PartialEq)]
pub enum DetectionResult {
    /// No harmful flash detected.
    Safe,
    /// Potential flash detected; mitigation should be applied.
    FlashDetected { flashes_per_second: f32 },
}

/// Stateful flash detector. Feed frames one at a time via [`FlashDetector::push`].
pub struct FlashDetector {
    config: DetectorConfig,
    /// Circular history of mean luminance values (newest last).
    history: Vec<f32>,
    /// Maximum history length = fps * analysis_window_seconds.
    max_history: usize,
}

impl FlashDetector {
    /// Create a new detector. `analysis_window_seconds` is the sliding window
    /// used to count flashes (1.0 s is typical).
    pub fn new(config: DetectorConfig, analysis_window_seconds: f32) -> Self {
        let max_history = (config.fps * analysis_window_seconds).ceil() as usize;
        Self {
            config,
            history: Vec::with_capacity(max_history + 1),
            max_history,
        }
    }

    /// Push a new frame and return the detection result.
    pub fn push(&mut self, frame: &LuminanceFrame) -> DetectionResult {
        let mean = frame.mean_luminance();
        self.history.push(mean);
        if self.history.len() > self.max_history {
            self.history.remove(0);
        }
        self.analyse()
    }

    fn analyse(&self) -> DetectionResult {
        if self.history.len() < 2 {
            return DetectionResult::Safe;
        }
        let threshold = self.config.luminance_change_threshold;
        let mut flashes = 0u32;
        let mut last_direction: i8 = 0;

        for window in self.history.windows(2) {
            let delta = window[1] - window[0];
            let direction: i8 = if delta > threshold {
                1
            } else if delta < -threshold {
                -1
            } else {
                0
            };
            if direction != 0 && direction != last_direction && last_direction != 0 {
                flashes += 1;
            }
            if direction != 0 {
                last_direction = direction;
            }
        }

        let window_seconds = self.history.len() as f32 / self.config.fps;
        let flashes_per_second = if window_seconds > 0.0 {
            flashes as f32 / window_seconds
        } else {
            0.0
        };

        if flashes_per_second > self.config.max_flashes_per_second {
            DetectionResult::FlashDetected { flashes_per_second }
        } else {
            DetectionResult::Safe
        }
    }

    /// Reset the detector's history (e.g. on scene cut).
    pub fn reset(&mut self) {
        self.history.clear();
    }
}

// ---------------------------------------------------------------------------
// LuminanceDetector — WCAG 2.1 general flash detector with timestamp-based
// sliding window and ITU-R BT.709 BGRA→luminance conversion.
// ---------------------------------------------------------------------------

/// Classification of a detected flash event.
#[derive(Debug, Clone, PartialEq)]
pub enum FlashKind {
    /// General (luminance) flash per WCAG 2.1 §2.3.
    General,
    /// Red flash (high red-channel dominance).
    Red,
}

/// Emitted by [`LuminanceDetector`] when the flash threshold is exceeded.
#[derive(Debug, Clone)]
pub struct FlashEvent {
    /// What kind of flash was detected.
    pub kind: FlashKind,
    /// Flash rate in flashes per second at the moment of detection.
    pub severity: f32,
    /// Wall-clock timestamp of the triggering frame.
    pub timestamp: Instant,
}

/// Configuration for [`LuminanceDetector`].
#[derive(Debug, Clone)]
pub struct LuminanceDetectorConfig {
    /// Duration of the sliding analysis window (must be ≥ 200 ms).
    pub window: Duration,
    /// Minimum absolute luminance change (linear 0–1) to count as a transition.
    pub change_threshold: f32,
    /// Flash rate (flashes/sec) at or above which a [`FlashEvent`] is emitted.
    pub max_flashes_per_second: f32,
}

impl Default for LuminanceDetectorConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_millis(1000),
            change_threshold: 0.1,
            max_flashes_per_second: 3.0,
        }
    }
}

struct LuminanceSample {
    luminance: f32,
    timestamp: Instant,
}

/// Stateful luminance flash detector.
///
/// Feed frames via [`LuminanceDetector::push_bgra`] (raw BGRA pixels) or
/// [`LuminanceDetector::push_sample`] (pre-computed luminance, for testing).
/// Each call evicts samples outside the configured window and returns
/// `Some(FlashEvent)` when the WCAG flash threshold is reached.
pub struct LuminanceDetector {
    config: LuminanceDetectorConfig,
    samples: Vec<LuminanceSample>,
}

impl LuminanceDetector {
    pub fn new(config: LuminanceDetectorConfig) -> Self {
        Self {
            config,
            samples: Vec::new(),
        }
    }

    /// Compute mean relative luminance (ITU-R BT.709, sRGB-linearised) from a
    /// BGRA frame and push it into the sliding window.
    ///
    /// `data` must be `width * height * 4` bytes in BGRA order (the layout
    /// produced by the DXGI Desktop Duplication API).
    pub fn push_bgra(
        &mut self,
        data: &[u8],
        width: u32,
        height: u32,
        timestamp: Instant,
    ) -> Option<FlashEvent> {
        let luminance = bgra_mean_luminance_bt709(data, width, height);
        self.push_sample(luminance, timestamp)
    }

    /// Push a pre-computed mean luminance value (useful for unit tests and
    /// benchmarks that do not need the BGRA conversion path).
    pub fn push_sample(&mut self, luminance: f32, timestamp: Instant) -> Option<FlashEvent> {
        // Evict samples that have aged out of the analysis window.
        let cutoff = timestamp
            .checked_sub(self.config.window)
            .unwrap_or(timestamp);
        self.samples.retain(|s| s.timestamp >= cutoff);
        self.samples.push(LuminanceSample {
            luminance,
            timestamp,
        });

        if self.samples.len() < 2 {
            return None;
        }

        let flashes = count_flash_transitions(&self.samples, self.config.change_threshold);
        let window_secs = self.config.window.as_secs_f32();
        let flashes_per_second = flashes as f32 / window_secs;

        if flashes_per_second >= self.config.max_flashes_per_second {
            Some(FlashEvent {
                kind: FlashKind::General,
                severity: flashes_per_second,
                timestamp,
            })
        } else {
            None
        }
    }

    /// Reset the detector (e.g. on scene cut or pipeline restart).
    pub fn reset(&mut self) {
        self.samples.clear();
    }
}

/// Convert a BGRA frame to mean relative luminance using ITU-R BT.709
/// coefficients applied to sRGB-linearised channel values.
///
/// BGRA byte order: `[B, G, R, A, B, G, R, A, …]`
fn bgra_mean_luminance_bt709(data: &[u8], width: u32, height: u32) -> f32 {
    let pixel_count = (width * height) as usize;
    debug_assert_eq!(
        data.len(),
        pixel_count * 4,
        "BGRA data length mismatch: expected {} bytes for {}×{}", pixel_count * 4, width, height
    );
    if pixel_count == 0 {
        return 0.0;
    }
    let sum: f64 = data.chunks_exact(4).map(|px| {
        // DXGI BGRA: index 0=B, 1=G, 2=R, 3=A
        let b = srgb_to_linear(px[0] as f64 / 255.0);
        let g = srgb_to_linear(px[1] as f64 / 255.0);
        let r = srgb_to_linear(px[2] as f64 / 255.0);
        // BT.709 luminance coefficients
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }).sum();
    (sum / pixel_count as f64) as f32
}

/// IEC 61966-2-1 (sRGB) transfer function inverse: sRGB encoded → linear light.
#[inline]
fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Count the number of opposing-direction luminance transitions (flash pairs)
/// in `samples` using the WCAG direction-change algorithm.
fn count_flash_transitions(samples: &[LuminanceSample], threshold: f32) -> u32 {
    let mut flashes = 0u32;
    let mut last_dir: i8 = 0;
    for w in samples.windows(2) {
        let delta = w[1].luminance - w[0].luminance;
        let dir: i8 = if delta > threshold {
            1
        } else if delta < -threshold {
            -1
        } else {
            0
        };
        if dir != 0 && dir != last_dir && last_dir != 0 {
            flashes += 1;
        }
        if dir != 0 {
            last_dir = dir;
        }
    }
    flashes
}

#[cfg(test)]
mod luminance_detector_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Build a sequence of (luminance, timestamp) pairs for a square wave.
    ///
    /// - `freq_hz`: oscillation frequency in Hz
    /// - `duration_secs`: total sequence length in seconds
    /// - `fps`: frames per second (determines sample spacing)
    /// - `bright` / `dark`: luminance levels for the two phases
    fn square_wave(
        freq_hz: f32,
        duration_secs: f32,
        fps: f32,
        bright: f32,
        dark: f32,
    ) -> Vec<(f32, Instant)> {
        let base = Instant::now();
        let n_frames = (duration_secs * fps).round() as usize;
        (0..n_frames)
            .map(|i| {
                let t = i as f32 / fps;
                // Which half-period are we in?
                let phase = (t * freq_hz).fract();
                let lum = if phase < 0.5 { bright } else { dark };
                let ts = base + Duration::from_secs_f32(t);
                (lum, ts)
            })
            .collect()
    }

    fn run_detector(frames: Vec<(f32, Instant)>) -> bool {
        let mut det = LuminanceDetector::new(LuminanceDetectorConfig::default());
        let mut detected = false;
        for (lum, ts) in frames {
            if det.push_sample(lum, ts).is_some() {
                detected = true;
                break;
            }
        }
        detected
    }

    #[test]
    fn test_3hz_square_wave_triggers() {
        // 3 Hz ≥ threshold (3.0) → event expected
        let frames = square_wave(3.0, 1.0, 60.0, 0.9, 0.1);
        assert!(
            run_detector(frames),
            "3 Hz square wave should trigger flash detection"
        );
    }

    #[test]
    fn test_5hz_square_wave_triggers() {
        let frames = square_wave(5.0, 1.0, 60.0, 0.9, 0.1);
        assert!(
            run_detector(frames),
            "5 Hz square wave should trigger flash detection"
        );
    }

    #[test]
    fn test_2hz_square_wave_no_trigger() {
        // 2 Hz < threshold → no event
        let frames = square_wave(2.0, 1.0, 60.0, 0.9, 0.1);
        assert!(
            !run_detector(frames),
            "2 Hz square wave should not trigger flash detection"
        );
    }

    #[test]
    fn test_slow_fade_no_trigger() {
        // Slow linear ramp: only ever going in one direction → 0 flash pairs
        let base = Instant::now();
        let fps = 60.0_f32;
        let n_frames = 60usize;
        let mut det = LuminanceDetector::new(LuminanceDetectorConfig::default());
        let mut detected = false;
        for i in 0..n_frames {
            let lum = i as f32 / (n_frames - 1) as f32; // 0.0 → 1.0
            let ts = base + Duration::from_secs_f32(i as f32 / fps);
            if det.push_sample(lum, ts).is_some() {
                detected = true;
                break;
            }
        }
        assert!(!detected, "slow linear fade should not trigger flash detection");
    }

    #[test]
    fn test_reset_clears_state() {
        let base = Instant::now();
        let mut det = LuminanceDetector::new(LuminanceDetectorConfig::default());
        // Build up state with a 5 Hz square wave for 0.5 s
        for (lum, ts) in square_wave(5.0, 0.5, 60.0, 0.9, 0.1) {
            det.push_sample(lum, ts);
        }
        det.reset();
        // After reset a single quiet sample must be Safe
        let result = det.push_sample(0.5, base + Duration::from_secs(1));
        assert!(result.is_none(), "detector should be clear after reset");
    }

    #[test]
    fn test_push_bgra_grayscale_luminance() {
        // A 1×1 white pixel in BGRA should have linear luminance ≈ 1.0
        let white: [u8; 4] = [255, 255, 255, 255]; // B G R A
        let lum = super::bgra_mean_luminance_bt709(&white, 1, 1);
        assert!(
            (lum - 1.0).abs() < 1e-4,
            "white pixel luminance should be ~1.0, got {lum}"
        );

        // A 1×1 black pixel should have luminance 0.0
        let black: [u8; 4] = [0, 0, 0, 255];
        let lum = super::bgra_mean_luminance_bt709(&black, 1, 1);
        assert!(
            lum.abs() < 1e-6,
            "black pixel luminance should be 0.0, got {lum}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LuminanceFrame;

    fn frame(lum: f32) -> LuminanceFrame {
        LuminanceFrame::new(1, 1, vec![lum])
    }

    #[test]
    fn test_no_flash_steady() {
        let mut det = FlashDetector::new(DetectorConfig::default(), 1.0);
        for _ in 0..60 {
            assert_eq!(det.push(&frame(0.5)), DetectionResult::Safe);
        }
    }

    #[test]
    fn test_flash_detected() {
        let config = DetectorConfig {
            fps: 10.0,
            max_flashes_per_second: 3.0,
            luminance_change_threshold: 0.1,
        };
        let mut det = FlashDetector::new(config, 1.0);
        // Push rapid alternating bright/dark frames
        for i in 0..10 {
            let lum = if i % 2 == 0 { 0.9 } else { 0.1 };
            det.push(&frame(lum));
        }
        let result = det.push(&frame(0.9));
        assert!(
            matches!(result, DetectionResult::FlashDetected { .. }),
            "expected FlashDetected, got {result:?}"
        );
    }

    #[test]
    fn test_reset_clears_history() {
        let config = DetectorConfig {
            fps: 10.0,
            max_flashes_per_second: 3.0,
            luminance_change_threshold: 0.1,
        };
        let mut det = FlashDetector::new(config, 1.0);
        for i in 0..10 {
            let lum = if i % 2 == 0 { 0.9 } else { 0.1 };
            det.push(&frame(lum));
        }
        det.reset();
        assert_eq!(det.push(&frame(0.5)), DetectionResult::Safe);
    }
}
