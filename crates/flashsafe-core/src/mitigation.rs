//! Flash mitigation strategies.
//!
//! When the detector fires, a mitigation strategy is applied to the captured
//! frame before it is composited back onto the screen.

use crate::LuminanceFrame;

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
