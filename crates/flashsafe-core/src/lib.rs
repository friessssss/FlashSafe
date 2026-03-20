//! flashsafe-core: flash detection and mitigation logic.
//!
//! This crate contains all detection and mitigation algorithms.
//! It has no UI dependencies and can be tested independently.

pub mod detection;
pub mod mitigation;

/// A single captured frame of luminance data ready for analysis.
#[derive(Debug, Clone)]
pub struct LuminanceFrame {
    /// Width of the frame in pixels.
    pub width: u32,
    /// Height of the frame in pixels.
    pub height: u32,
    /// Per-pixel relative luminance values in [0.0, 1.0].
    pub data: Vec<f32>,
}

impl LuminanceFrame {
    /// Create a new LuminanceFrame. `data.len()` must equal `width * height`.
    pub fn new(width: u32, height: u32, data: Vec<f32>) -> Self {
        assert_eq!(
            data.len(),
            (width * height) as usize,
            "data length must equal width * height"
        );
        Self { width, height, data }
    }

    /// Compute the mean luminance across all pixels.
    pub fn mean_luminance(&self) -> f32 {
        if self.data.is_empty() {
            return 0.0;
        }
        self.data.iter().sum::<f32>() / self.data.len() as f32
    }
}
