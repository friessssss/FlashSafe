//! Area-averaged tile luminance from a BGRA frame.
//!
//! This is the CPU reference for the GPU tile pass: each tile's value is the
//! mean *linear* relative luminance of the pixels it covers (not a point
//! sample), so small moving details don't alias into fake flashes.

use crate::luma::relative_luminance8;

/// Fixed tile grid used by both the CPU reference and the GPU pipeline.
pub const TILE_COLS: usize = 32;
pub const TILE_ROWS: usize = 18;

/// A borrowed BGRA8 image with an explicit row stride (e.g. a mapped D3D11
/// texture, whose `RowPitch` may exceed `width * 4`).
#[derive(Debug, Clone, Copy)]
pub struct BgraFrame<'a> {
    pub data: &'a [u8],
    pub row_pitch: usize,
    pub width: usize,
    pub height: usize,
}

impl<'a> BgraFrame<'a> {
    /// Tightly packed frame (`row_pitch = width * 4`).
    pub fn packed(data: &'a [u8], width: usize, height: usize) -> Self {
        Self { data, row_pitch: width * 4, width, height }
    }

    fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.row_pitch >= self.width * 4
            && self.data.len() >= self.row_pitch * (self.height - 1) + self.width * 4
    }
}

/// Fill `out` (`cols * rows`, row-major) with per-tile mean linear luminance.
///
/// `step` subsamples pixels inside each tile (1 = every pixel); a step of
/// 2–4 is plenty for a luminance average. An invalid frame yields zeros.
pub fn tile_luminance_bgra(frame: BgraFrame<'_>, cols: usize, rows: usize, step: usize, out: &mut [f32]) {
    assert_eq!(out.len(), cols * rows, "output length must be cols * rows");
    out.fill(0.0);
    if !frame.is_valid() {
        return;
    }
    let BgraFrame { data, row_pitch, width, height } = frame;
    let step = step.max(1);
    for ty in 0..rows {
        let y0 = ty * height / rows;
        let y1 = ((ty + 1) * height / rows).max(y0 + 1).min(height);
        for tx in 0..cols {
            let x0 = tx * width / cols;
            let x1 = ((tx + 1) * width / cols).max(x0 + 1).min(width);
            let mut sum = 0.0f32;
            let mut n = 0u32;
            for y in (y0..y1).step_by(step) {
                let row = &data[y * row_pitch..];
                for x in (x0..x1).step_by(step) {
                    let p = &row[x * 4..x * 4 + 3];
                    sum += relative_luminance8(p[2], p[1], p[0]);
                    n += 1;
                }
            }
            out[ty * cols + tx] = if n > 0 { sum / n as f32 } else { 0.0 };
        }
    }
}

/// Convenience for tests and tools: a solid-colour BGRA frame.
pub fn solid_bgra(width: usize, height: usize, r: u8, g: u8, b: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(width * height * 4);
    for _ in 0..width * height {
        v.extend_from_slice(&[b, g, r, 255]);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_frames() {
        let mut out = vec![0.0; 4 * 2];
        let white = solid_bgra(64, 32, 255, 255, 255);
        tile_luminance_bgra(BgraFrame::packed(&white, 64, 32), 4, 2, 1, &mut out);
        assert!(out.iter().all(|v| (v - 1.0).abs() < 1e-5));
        let black = solid_bgra(64, 32, 0, 0, 0);
        tile_luminance_bgra(BgraFrame::packed(&black, 64, 32), 4, 2, 1, &mut out);
        assert!(out.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn area_average_and_padding() {
        // Left half white, right half black, with 16 bytes of row padding.
        let (w, h, pitch) = (8usize, 4usize, 8 * 4 + 16);
        let mut data = vec![0u8; pitch * h];
        for y in 0..h {
            for x in 0..w / 2 {
                data[y * pitch + x * 4..y * pitch + x * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        let frame = BgraFrame { data: &data, row_pitch: pitch, width: w, height: h };
        let mut one = [0.0];
        tile_luminance_bgra(frame, 1, 1, 1, &mut one);
        assert!((one[0] - 0.5).abs() < 1e-5);
        let mut two = [0.0; 2];
        tile_luminance_bgra(frame, 2, 1, 1, &mut two);
        assert!((two[0] - 1.0).abs() < 1e-5 && two[1] == 0.0);
    }
}
