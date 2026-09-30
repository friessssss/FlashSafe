use serde::{Deserialize, Serialize};

fn default_sensitivity_preset() -> String {
    "medium".into()
}

/// Low / medium / high pipeline bundles for the sensitivity UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SensitivityPresets {
    pub low: PipelineSettings,
    pub medium: PipelineSettings,
    pub high: PipelineSettings,
}

/// Returns clamped preset pipelines (single source of truth for UI + saved config).
pub fn sensitivity_presets() -> SensitivityPresets {
    let mut low = PipelineSettings {
        grid_size: 12,
        spike_delta_threshold: 0.20,
        peak_clip_cell_fraction: 0.50,
        pattern_sensitivity: 0.22,
        max_mitigation: 0.55,
        attack_ms: 22.0,
        release_ms: 130.0,
        temporal_blend: 0.2,
        highlight_knee: 0.86,
        exposure_scale: 0.52,
        desaturate_on_threat: 0.18,
    };
    let mut medium = PipelineSettings::default();
    let mut high = PipelineSettings {
        grid_size: 20,
        spike_delta_threshold: 0.055,
        peak_clip_cell_fraction: 0.18,
        pattern_sensitivity: 0.72,
        max_mitigation: 0.98,
        attack_ms: 5.0,
        release_ms: 320.0,
        temporal_blend: 0.38,
        highlight_knee: 0.68,
        exposure_scale: 0.26,
        desaturate_on_threat: 0.52,
    };
    low.clamp();
    medium.clamp();
    high.clamp();
    SensitivityPresets {
        low,
        medium,
        high,
    }
}

/// Serializable settings mirrored in the UI and engine thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FlashSafeConfig {
    pub enabled: bool,
    /// Last selected target window (HWND as u64). 0 = none.
    pub target_hwnd: u64,
    /// Human-readable title for display only.
    pub target_title: String,
    pub monitor_all_screens: bool,
    /// Extra compositor delay (ms) before presenting mirror — tiny safety margin.
    pub present_delay_ms: u32,
    /// UI preset last chosen: `low` | `medium` | `high` | `custom`.
    #[serde(default = "default_sensitivity_preset")]
    pub sensitivity_preset: String,
    pub pipeline: PipelineSettings,
}

impl Default for FlashSafeConfig {
    fn default() -> Self {
        Self {
            // When false, detection still runs but GPU dimming stays off (preview / troubleshooting).
            enabled: true,
            target_hwnd: 0,
            target_title: String::new(),
            monitor_all_screens: false,
            present_delay_ms: 0,
            sensitivity_preset: default_sensitivity_preset(),
            pipeline: PipelineSettings::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PipelineSettings {
    /// Rows/columns for downsampled grid stats (e.g. 16 → 256 cells).
    pub grid_size: u32,
    /// Minimum mean luminance jump (linear 0–1) in one frame to count toward spike.
    pub spike_delta_threshold: f32,
    /// Fraction of downsample cells near white (≥0.95 luma) to flag peak clip.
    pub peak_clip_cell_fraction: f32,
    /// Weight for band-pass (3–30 Hz) energy in combined threat score [0,1].
    pub pattern_sensitivity: f32,
    /// Max mitigation blend factor when threat = 1.
    pub max_mitigation: f32,
    /// Attack time constant (ms) — how fast mitigation ramps up.
    pub attack_ms: f32,
    /// Release time constant (ms) — how fast mitigation decays.
    pub release_ms: f32,
    /// Temporal blend with previous mitigated frame (0 = off, 1 = heavy).
    pub temporal_blend: f32,
    /// Highlight knee: compress linear values above this toward 1.0.
    pub highlight_knee: f32,
    /// Optional global exposure scale when mitigating (multiplier on linear RGB).
    pub exposure_scale: f32,
    pub desaturate_on_threat: f32,
}

impl Default for PipelineSettings {
    fn default() -> Self {
        Self {
            grid_size: 16,
            spike_delta_threshold: 0.12,
            peak_clip_cell_fraction: 0.35,
            pattern_sensitivity: 0.4,
            max_mitigation: 0.9,
            attack_ms: 8.0,
            release_ms: 200.0,
            temporal_blend: 0.28,
            highlight_knee: 0.74,
            exposure_scale: 0.34,
            desaturate_on_threat: 0.38,
        }
    }
}

impl FlashSafeConfig {
    pub fn clamp(&mut self) {
        self.present_delay_ms = self.present_delay_ms.min(50);
        self.pipeline.clamp();
    }
}

impl PipelineSettings {
    pub fn clamp(&mut self) {
        self.grid_size = self.grid_size.clamp(4, 64);
        self.spike_delta_threshold = self.spike_delta_threshold.clamp(0.02, 0.5);
        self.peak_clip_cell_fraction = self.peak_clip_cell_fraction.clamp(0.05, 1.0);
        self.pattern_sensitivity = self.pattern_sensitivity.clamp(0.0, 1.0);
        self.max_mitigation = self.max_mitigation.clamp(0.0, 1.0);
        self.attack_ms = self.attack_ms.clamp(1.0, 500.0);
        self.release_ms = self.release_ms.clamp(10.0, 2000.0);
        self.temporal_blend = self.temporal_blend.clamp(0.0, 0.95);
        self.highlight_knee = self.highlight_knee.clamp(0.5, 0.99);
        self.exposure_scale = self.exposure_scale.clamp(0.2, 1.0);
        self.desaturate_on_threat = self.desaturate_on_threat.clamp(0.0, 1.0);
    }
}
