//! Persisted settings, presets and migration.

use serde::{Deserialize, Serialize};

use crate::filter::FilterParams;

/// Bump when the on-disk shape or meaning of settings changes.
pub const CONFIG_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    Low,
    #[default]
    Medium,
    High,
    Custom,
}

impl Preset {
    /// Filter parameters for a named preset (`Custom` → medium as a base).
    pub fn params(self) -> FilterParams {
        let mut p = match self {
            Preset::Low => FilterParams {
                rise_per_sec: 1.5,
                hold_rise_per_sec: 0.3,
                fall_per_sec: 5.0,
                hold_fall_per_sec: 1.0,
                strobe_trigger: 3.5,
                hold_secs: 1.0,
                min_gain: 0.05,
                region_radius: 5,
                burst_secs: 0.05,
            },
            Preset::Medium | Preset::Custom => FilterParams::default(),
            Preset::High => FilterParams {
                rise_per_sec: 0.4,
                hold_rise_per_sec: 0.12,
                fall_per_sec: 1.5,
                hold_fall_per_sec: 0.3,
                strobe_trigger: 2.5,
                hold_secs: 2.5,
                min_gain: 0.02,
                region_radius: 4,
                burst_secs: 0.06,
            },
        };
        p.clamp();
        p
    }
}

/// All presets, for the UI.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SensitivityPresets {
    pub low: FilterParams,
    pub medium: FilterParams,
    pub high: FilterParams,
}

pub fn sensitivity_presets() -> SensitivityPresets {
    SensitivityPresets {
        low: Preset::Low.params(),
        medium: Preset::Medium.params(),
        high: Preset::High.params(),
    }
}

/// Serializable settings mirrored in the UI and engine thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FlashSafeConfig {
    pub config_version: u32,
    /// When false the mirror shows the game unfiltered (troubleshooting only).
    pub enabled: bool,
    /// Last selected target window (HWND as u64). 0 = none.
    pub target_hwnd: u64,
    /// Human-readable title for display only.
    pub target_title: String,
    pub sensitivity_preset: Preset,
    pub filter: FilterParams,
}

impl Default for FlashSafeConfig {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            enabled: true,
            target_hwnd: 0,
            target_title: String::new(),
            sensitivity_preset: Preset::Medium,
            filter: Preset::Medium.params(),
        }
    }
}

impl FlashSafeConfig {
    /// Parse saved JSON, migrating older files. Never fails on unknown or
    /// legacy fields; unparseable input falls back to defaults.
    pub fn from_json(s: &str) -> Self {
        let raw: serde_json::Value = serde_json::from_str(s).unwrap_or(serde_json::Value::Null);
        let version = raw.get("configVersion").and_then(|v| v.as_u64()).unwrap_or(0);
        let mut cfg: FlashSafeConfig = serde_json::from_value(raw.clone()).unwrap_or_else(|_| {
            // Salvage what we can from a partially incompatible file.
            let mut c = FlashSafeConfig::default();
            if let Some(h) = raw.get("targetHwnd").and_then(|v| v.as_u64()) {
                c.target_hwnd = h;
            }
            if let Some(t) = raw.get("targetTitle").and_then(|v| v.as_str()) {
                c.target_title = t.to_string();
            }
            c
        });
        if version < 2 {
            // v1 stored the old threat-score pipeline; its values don't map
            // onto the slew filter, so restart from the preset.
            if cfg.sensitivity_preset == Preset::Custom {
                cfg.sensitivity_preset = Preset::Medium;
            }
            cfg.filter = cfg.sensitivity_preset.params();
        }
        cfg.config_version = CONFIG_VERSION;
        cfg.clamp();
        cfg
    }

    pub fn clamp(&mut self) {
        if self.sensitivity_preset != Preset::Custom {
            self.filter = self.sensitivity_preset.params();
        }
        self.filter.clamp();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut c = FlashSafeConfig {
            sensitivity_preset: Preset::Custom,
            ..Default::default()
        };
        c.filter.rise_per_sec = 1.1;
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(FlashSafeConfig::from_json(&s), c);
    }

    #[test]
    fn migrates_v1_file() {
        let v1 = r#"{"enabled":true,"targetHwnd":42,"targetTitle":"Game","monitorAllScreens":false,
            "presentDelayMs":0,"sensitivityPreset":"high","pipeline":{"gridSize":16}}"#;
        let c = FlashSafeConfig::from_json(v1);
        assert_eq!(c.config_version, CONFIG_VERSION);
        assert_eq!(c.target_hwnd, 42);
        assert_eq!(c.sensitivity_preset, Preset::High);
        assert_eq!(c.filter, Preset::High.params());
    }

    #[test]
    fn garbage_falls_back_to_default() {
        assert_eq!(FlashSafeConfig::from_json("not json"), FlashSafeConfig::default());
        let odd = r#"{"targetHwnd":7,"sensitivityPreset":"extreme"}"#;
        assert_eq!(FlashSafeConfig::from_json(odd).target_hwnd, 7);
    }

    #[test]
    fn presets_are_ordered_by_strength() {
        let p = sensitivity_presets();
        assert!(p.low.rise_per_sec > p.medium.rise_per_sec && p.medium.rise_per_sec > p.high.rise_per_sec);
        assert!(p.low.fall_per_sec > p.medium.fall_per_sec && p.medium.fall_per_sec > p.high.fall_per_sec);
        assert!(p.low.min_gain >= p.medium.min_gain && p.medium.min_gain >= p.high.min_gain);
    }
}
