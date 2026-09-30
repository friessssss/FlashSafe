//! Area-averaged tile statistics.
//!
//! Each tile's values are means over the pixels it covers (not point
//! samples), so small moving details don't alias into fake flashes.

use crate::luma::relative_luminance8;

/// Fixed tile grid used by both the CPU reference and the GPU pipeline.
pub const TILE_COLS: usize = 32;
pub const TILE_ROWS: usize = 18;

/// Per-tile means of the per-pixel filter quantities, in linear relative
/// luminance. `#[repr(C)]` so it matches an `RGBA32F` texel read back from
/// the GPU stats pass: `(L, P, max(0, L − P), max(0, P − L))`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TileStats {
    /// Mean input luminance `L`.
    pub luma: f32,
    /// Mean luminance `P` displayed last frame.
    pub shown: f32,
    /// Mean per-pixel brightening `max(0, L − P)`.
    pub rise: f32,
    /// Mean per-pixel darkening `max(0, P − L)`.
    pub fall: f32,
}

impl TileStats {
    /// Stats of a tile whose pixels are all identical.
    pub fn uniform(luma: f32, shown: f32) -> Self {
        Self {
            luma,
            shown,
            rise: (luma - shown).max(0.0),
            fall: (shown - luma).max(0.0),
        }
    }
}

/// Pixel range `[start, end)` of tile `t` of `tiles` along an axis of `n` pixels.
#[inline]
pub fn tile_span(t: usize, n: usize, tiles: usize) -> (usize, usize) {
    let a = t * n / tiles;
    (a, ((t + 1) * n / tiles).max(a + 1).min(n))
}

/// Tile means of a 4-channel `f32` image laid out as `(L, P, rise, fall)`
/// per pixel. `row_stride` is in floats (≥ `width * 4`), matching a mapped
/// `RGBA32F` texture's `RowPitch / 4`.
pub fn tile_stats_f32x4(
    data: &[f32],
    row_stride: usize,
    width: usize,
    height: usize,
    cols: usize,
    rows: usize,
    out: &mut [TileStats],
) {
    assert_eq!(out.len(), cols * rows, "output length must be cols * rows");
    out.fill(TileStats::default());
    if width == 0 || height == 0 || row_stride < width * 4 || data.len() < row_stride * (height - 1) + width * 4 {
        return;
    }
    for ty in 0..rows {
        let (y0, y1) = tile_span(ty, height, rows);
        for tx in 0..cols {
            let (x0, x1) = tile_span(tx, width, cols);
            let mut acc = [0.0f32; 4];
            for y in y0..y1 {
                let row = &data[y * row_stride + x0 * 4..y * row_stride + x1 * 4];
                for px in row.as_chunks::<4>().0 {
                    for (a, v) in acc.iter_mut().zip(px) {
                        *a += v;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as f32;
            out[ty * cols + tx] = TileStats {
                luma: acc[0] / n,
                shown: acc[1] / n,
                rise: acc[2] / n,
                fall: acc[3] / n,
            };
        }
    }
}

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
        let (y0, y1) = tile_span(ty, height, rows);
        for tx in 0..cols {
            let (x0, x1) = tile_span(tx, width, cols);
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
    fn f32x4_stats_with_stride() {
        // 4×2 image, 2×1 tiles, 2 floats of row padding.
        let (w, h, stride) = (4usize, 2usize, 4 * 4 + 2);
        let mut data = vec![0.0f32; stride * h];
        for y in 0..h {
            for x in 0..w {
                let v = if x < 2 { [1.0, 0.5, 0.5, 0.0] } else { [0.0, 0.25, 0.0, 0.25] };
                data[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&v);
            }
        }
        let mut out = [TileStats::default(); 2];
        tile_stats_f32x4(&data, stride, w, h, 2, 1, &mut out);
        assert_eq!(out[0], TileStats { luma: 1.0, shown: 0.5, rise: 0.5, fall: 0.0 });
        assert_eq!(out[1], TileStats::uniform(0.0, 0.25));
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
