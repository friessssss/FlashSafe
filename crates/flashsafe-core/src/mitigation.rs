//! Uniforms for GPU mitigation pass (tone + temporal blend).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MitigationParams {
    /// Effective mitigation after attack/release smoothing [0,1].
    pub strength: f32,
    pub exposure_scale: f32,
    pub highlight_knee: f32,
    pub temporal_blend: f32,
    pub desaturate: f32,
}

impl Default for MitigationParams {
    fn default() -> Self {
        Self {
            strength: 0.0,
            exposure_scale: 1.0,
            highlight_knee: 0.9,
            temporal_blend: 0.0,
            desaturate: 0.0,
        }
    }
}

/// First-order smoothing toward target (attack / release asymmetry).
pub fn smooth_toward(current: f32, target: f32, dt_secs: f32, attack_tau: f32, release_tau: f32) -> f32 {
    let tau = if target > current {
        attack_tau.max(1e-3)
    } else {
        release_tau.max(1e-3)
    };
    let alpha = 1.0 - (-dt_secs / tau).exp();
    current + (target - current) * alpha.clamp(0.0, 1.0)
}
