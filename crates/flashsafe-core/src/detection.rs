//! Downsampled frame statistics, luminance spikes, peak clipping, and a light
//! temporal pattern heuristic (not a medical device).

use std::collections::VecDeque;
use std::time::Instant;

/// Quick luma from BGRA byte triplet (gamma-encoded, same as legacy FlashSafe).
#[inline]
pub fn bgr_luma_fast(b: u8, g: u8, r: u8) -> f32 {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Per-frame stats from a `grid_size × grid_size` downsample of BGRA.
#[derive(Debug, Clone, Default)]
pub struct DownsampleStats {
    pub cell_luma: Vec<f32>,
    pub global_mean: f32,
    pub global_max: f32,
    pub peak_clip_fraction: f32,
}

impl DownsampleStats {
    pub fn from_bgra(data: &[u8], width: u32, height: u32, grid_size: u32) -> Self {
        let g = grid_size.max(1);
        let cells = (g * g) as usize;
        let mut cell_luma = vec![0.0f32; cells];
        if width == 0 || height == 0 || data.len() < (width * height * 4) as usize {
            return Self::default();
        }
        let w = width as usize;
        let h = height as usize;
        let stride = w * 4;

        for cy in 0..g as usize {
            for cx in 0..g as usize {
                let px = ((cx * w + w / 2) / g as usize).min(w - 1);
                let py = ((cy * h + h / 2) / g as usize).min(h - 1);
                let i = py * stride + px * 4;
                if i + 2 < data.len() {
                    let lum = bgr_luma_fast(data[i], data[i + 1], data[i + 2]);
                    cell_luma[cy * g as usize + cx] = lum;
                }
            }
        }

        let global_mean = if cell_luma.is_empty() {
            0.0
        } else {
            cell_luma.iter().copied().sum::<f32>() / cell_luma.len() as f32
        };
        let global_max = cell_luma.iter().copied().fold(0.0f32, f32::max);
        let near_white = cell_luma.iter().filter(|&&v| v >= 0.95).count() as f32;
        let peak_clip_fraction = if cell_luma.is_empty() {
            0.0
        } else {
            near_white / cell_luma.len() as f32
        };

        Self {
            cell_luma,
            global_mean,
            global_max,
            peak_clip_fraction,
        }
    }

    /// Same as [`from_bgra`], but rows are spaced by `row_pitch` bytes (e.g. D3D11 mapped texture).
    pub fn from_bgra_strided(data: &[u8], row_pitch: usize, width: u32, height: u32, grid_size: u32) -> Self {
        let g = grid_size.max(1);
        let cells = (g * g) as usize;
        let mut cell_luma = vec![0.0f32; cells];
        let w = width as usize;
        let h = height as usize;
        if width == 0 || height == 0 || row_pitch < w * 4 {
            return Self::default();
        }
        let need = row_pitch * h;
        if data.len() < need {
            return Self::default();
        }

        for cy in 0..g as usize {
            for cx in 0..g as usize {
                let px = ((cx * w + w / 2) / g as usize).min(w - 1);
                let py = ((cy * h + h / 2) / g as usize).min(h - 1);
                let i = py * row_pitch + px * 4;
                if i + 2 < data.len() {
                    let lum = bgr_luma_fast(data[i], data[i + 1], data[i + 2]);
                    cell_luma[cy * g as usize + cx] = lum;
                }
            }
        }

        let global_mean = if cell_luma.is_empty() {
            0.0
        } else {
            cell_luma.iter().copied().sum::<f32>() / cell_luma.len() as f32
        };
        let global_max = cell_luma.iter().copied().fold(0.0f32, f32::max);
        let near_white = cell_luma.iter().filter(|&&v| v >= 0.95).count() as f32;
        let peak_clip_fraction = if cell_luma.is_empty() {
            0.0
        } else {
            near_white / cell_luma.len() as f32
        };

        Self {
            cell_luma,
            global_mean,
            global_max,
            peak_clip_fraction,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FastFlashDetectorConfig {
    pub spike_delta_threshold: f32,
    pub peak_clip_cell_fraction: f32,
    pub pattern_sensitivity: f32,
}

impl Default for FastFlashDetectorConfig {
    fn default() -> Self {
        Self {
            spike_delta_threshold: 0.12,
            peak_clip_cell_fraction: 0.35,
            pattern_sensitivity: 0.4,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct FlashMetrics {
    /// Combined threat in [0, 1] before smoothing.
    pub raw_threat: f32,
    pub spike_strength: f32,
    pub peak_strength: f32,
    pub pattern_strength: f32,
}

/// Band-pass-ish energy using two biquads (high-pass @ ~2.5 Hz, low-pass @ ~32 Hz)
/// on irregular samples — good enough for a heuristic slider.
#[derive(Debug, Clone)]
pub struct BandPassConfig {
    pub min_hz: f32,
    pub max_hz: f32,
}

impl Default for BandPassConfig {
    fn default() -> Self {
        Self {
            min_hz: 3.0,
            max_hz: 30.0,
        }
    }
}

struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn new_lowpass(sample_rate: f32, cutoff: f32) -> Self {
        let c = (std::f32::consts::PI * cutoff / sample_rate.max(1.0)).tan();
        let a0 = 1.0 + c;
        let b0 = c / a0;
        let b1 = b0;
        let b2 = 0.0;
        let a1 = (c - 1.0) / a0;
        let a2 = 0.0;
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn new_highpass(sample_rate: f32, cutoff: f32) -> Self {
        let c = (std::f32::consts::PI * cutoff / sample_rate.max(1.0)).tan();
        let a0 = 1.0 + c;
        let b0 = 1.0 / a0;
        let b1 = -b0;
        let b2 = 0.0;
        let a1 = (c - 1.0) / a0;
        let a2 = 0.0;
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

pub struct FastFlashDetector {
    cfg: FastFlashDetectorConfig,
    prev_mean: Option<f32>,
    hp: Biquad,
    lp: Biquad,
    band_samples: VecDeque<f32>,
}

impl FastFlashDetector {
    pub fn new(cfg: FastFlashDetectorConfig) -> Self {
        Self {
            cfg,
            prev_mean: None,
            hp: Biquad::new_highpass(120.0, 2.5),
            lp: Biquad::new_lowpass(120.0, 32.0),
            band_samples: VecDeque::with_capacity(128),
        }
    }

    pub fn update_config(&mut self, cfg: FastFlashDetectorConfig) {
        self.cfg = cfg;
    }

    /// Advance detector; `now` should be frame capture time.
    pub fn push(&mut self, stats: &DownsampleStats, _now: Instant) -> FlashMetrics {
        let mean = stats.global_mean;
        let spike_strength = if let Some(prev) = self.prev_mean {
            ((mean - prev).abs() / self.cfg.spike_delta_threshold.max(1e-4)).min(1.0)
        } else {
            0.0
        };
        self.prev_mean = Some(mean);

        let peak_strength = (stats.peak_clip_fraction / self.cfg.peak_clip_cell_fraction.max(1e-4))
            .min(1.0);

        let hp_out = self.hp.process(mean);
        let band = self.lp.process(hp_out).abs();
        self.band_samples.push_back(band);
        if self.band_samples.len() > 64 {
            self.band_samples.pop_front();
        }
        let pattern_strength = (self.band_samples.iter().copied().sum::<f32>()
            / self.band_samples.len().max(1) as f32
            * 4.0)
            .min(1.0);

        let mut raw = spike_strength.max(peak_strength);
        raw = raw * (1.0 - self.cfg.pattern_sensitivity)
            + raw.max(pattern_strength) * self.cfg.pattern_sensitivity;

        FlashMetrics {
            raw_threat: raw.clamp(0.0, 1.0),
            spike_strength,
            peak_strength,
            pattern_strength,
        }
    }

    pub fn reset(&mut self) {
        self.prev_mean = None;
        self.band_samples.clear();
        self.hp = Biquad::new_highpass(120.0, 2.5);
        self.lp = Biquad::new_lowpass(120.0, 32.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn spike_on_sudden_bright_frame() {
        let mut det = FastFlashDetector::new(FastFlashDetectorConfig::default());
        let base = DownsampleStats {
            cell_luma: vec![0.1; 16],
            global_mean: 0.1,
            global_max: 0.1,
            peak_clip_fraction: 0.0,
        };
        let now = Instant::now();
        let _ = det.push(&base, now);
        let flash = DownsampleStats {
            cell_luma: vec![0.95; 16],
            global_mean: 0.95,
            global_max: 0.95,
            peak_clip_fraction: 1.0,
        };
        let m = det.push(&flash, now + Duration::from_millis(16));
        assert!(m.spike_strength > 0.9);
        assert!(m.raw_threat > 0.5);
    }
}
