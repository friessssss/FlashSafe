//! Colour math shared by the tile stats, the WCAG judge and the filter.
//!
//! Everything downstream works in *linear* relative luminance (0–1), which is
//! what WCAG 2.x flash thresholds are defined in.

use std::sync::OnceLock;

/// IEC 61966-2-1 (sRGB) decode: encoded 0–1 → linear 0–1.
#[inline]
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB encode: linear 0–1 → encoded 0–1.
#[inline]
pub fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn lut() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0.0f32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            *v = srgb_to_linear(i as f32 / 255.0);
        }
        t
    })
}

/// Linear value of an 8-bit sRGB channel.
#[inline]
pub fn srgb8_to_linear(c: u8) -> f32 {
    lut()[c as usize]
}

/// WCAG relative luminance (BT.709 weights on linear RGB) of 8-bit sRGB.
#[inline]
pub fn relative_luminance8(r: u8, g: u8, b: u8) -> f32 {
    0.2126 * srgb8_to_linear(r) + 0.7152 * srgb8_to_linear(g) + 0.0722 * srgb8_to_linear(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_and_roundtrip() {
        assert_eq!(relative_luminance8(0, 0, 0), 0.0);
        assert!((relative_luminance8(255, 255, 255) - 1.0).abs() < 1e-5);
        for i in 0..=255u8 {
            let c = i as f32 / 255.0;
            assert!((linear_to_srgb(srgb_to_linear(c)) - c).abs() < 1e-4);
        }
        // sRGB mid-grey is ~21% linear.
        assert!((srgb8_to_linear(128) - 0.2158).abs() < 1e-3);
    }
}
