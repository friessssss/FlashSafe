//! Flash detection logic.
//!
//! Implements a sliding-window luminance-change detector based on the
//! WCAG 2.1 / W3C flash criteria: a flash is a pair of opposing luminance
//! transitions where the relative luminance of the brighter state is ≥ 10%
//! of maximum and occurs more than 3 times per second.

use crate::LuminanceFrame;
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
