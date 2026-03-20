//! Runtime configuration and built-in sensitivity profiles for FlashSafe.
//!
//! [`FlashSafeConfig`] is serialised to / deserialised from TOML and stored at
//! `%APPDATA%\FlashSafe\config.toml` on Windows.  Missing keys fall back to
//! serde defaults, so upgrading the app never causes a parse failure.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Top-level runtime configuration.
///
/// All fields have serde `default` annotations so that a partial or empty
/// `config.toml` is valid: missing keys are filled from [`FlashSafeConfig::default`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlashSafeConfig {
    /// Minimum luminance change (0–1, linear) to count as a general-flash transition.
    pub luminance_threshold: f32,
    /// Minimum red-ratio change (0–1) to count as a red-flash transition.
    pub red_threshold: f32,
    /// Flash rate (Hz) at or above which mitigation is triggered.
    pub flash_rate_hz: f32,
    /// Mitigation strength: 0.0 = passthrough, 1.0 = full black.
    pub mitigation_level: f32,
    /// Fade-in / fade-out ramp duration in milliseconds (0 = instant).
    pub ramp_ms: u32,
}

impl Default for FlashSafeConfig {
    /// Balanced profile defaults.
    fn default() -> Self {
        Profile::Balanced.config()
    }
}

impl FlashSafeConfig {
    /// Validate ranges and clamp any out-of-range values in place.
    pub fn clamp(&mut self) {
        self.luminance_threshold = self.luminance_threshold.clamp(0.0, 1.0);
        self.red_threshold = self.red_threshold.clamp(0.0, 1.0);
        self.flash_rate_hz = self.flash_rate_hz.max(0.1);
        self.mitigation_level = self.mitigation_level.clamp(0.0, 1.0);
    }
}

/// Named sensitivity presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// WCAG-compliant thresholds, 80 % dimming.  Best for photosensitive users.
    Strict,
    /// Slightly relaxed thresholds, 60 % dimming.  Recommended for most users.
    Balanced,
    /// High thresholds, 40 % dimming.  Catches only severe events.
    Minimal,
}

impl Profile {
    /// Return the [`FlashSafeConfig`] for this preset.
    pub fn config(self) -> FlashSafeConfig {
        match self {
            Profile::Strict => FlashSafeConfig {
                luminance_threshold: 0.1,
                red_threshold: 0.2,
                flash_rate_hz: 3.0,
                mitigation_level: 0.8,
                ramp_ms: 50,
            },
            Profile::Balanced => FlashSafeConfig {
                luminance_threshold: 0.15,
                red_threshold: 0.25,
                flash_rate_hz: 3.0,
                mitigation_level: 0.6,
                ramp_ms: 50,
            },
            Profile::Minimal => FlashSafeConfig {
                luminance_threshold: 0.25,
                red_threshold: 0.35,
                flash_rate_hz: 3.5,
                mitigation_level: 0.4,
                ramp_ms: 30,
            },
        }
    }
}

/// Return the platform config directory: `%APPDATA%\FlashSafe` on Windows,
/// `~/.config/FlashSafe` elsewhere.
pub fn config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join("FlashSafe")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let base = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join(".config").join("FlashSafe")
    }
}

/// Load config from `%APPDATA%\FlashSafe\config.toml`.
///
/// Returns `Ok(default)` if the file does not exist.  Parse errors are
/// propagated so callers can warn the user.
pub fn load() -> Result<FlashSafeConfig> {
    let path = config_dir().join("config.toml");
    if !path.exists() {
        return Ok(FlashSafeConfig::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config from {}", path.display()))?;
    let mut cfg: FlashSafeConfig = toml::from_str(&raw)
        .with_context(|| format!("parsing config from {}", path.display()))?;
    cfg.clamp();
    Ok(cfg)
}

/// Persist `config` to `%APPDATA%\FlashSafe\config.toml`, creating the
/// directory if needed.
pub fn save(config: &FlashSafeConfig) -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating config dir {}", dir.display()))?;
    let path = dir.join("config.toml");
    let toml_str = toml::to_string_pretty(config).context("serialising config to TOML")?;
    std::fs::write(&path, toml_str)
        .with_context(|| format!("writing config to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_is_balanced() {
        let cfg = FlashSafeConfig::default();
        let balanced = Profile::Balanced.config();
        assert_eq!(cfg, balanced);
    }

    #[test]
    fn test_round_trip_toml() {
        let original = Profile::Strict.config();
        let serialised = toml::to_string_pretty(&original).unwrap();
        let deserialised: FlashSafeConfig = toml::from_str(&serialised).unwrap();
        assert_eq!(original, deserialised, "round-trip must be lossless");
    }

    #[test]
    fn test_partial_toml_uses_defaults() {
        // Only override one field — all others must fall back to defaults.
        let partial = r#"mitigation_level = 0.9"#;
        let cfg: FlashSafeConfig = toml::from_str(partial).unwrap();
        let expected_default = FlashSafeConfig::default();
        assert_eq!(
            cfg.luminance_threshold, expected_default.luminance_threshold,
            "missing keys must use defaults"
        );
        assert!(
            (cfg.mitigation_level - 0.9).abs() < 1e-5,
            "explicit key must be honoured"
        );
    }

    #[test]
    fn test_all_profiles_valid() {
        for profile in [Profile::Strict, Profile::Balanced, Profile::Minimal] {
            let mut cfg = profile.config();
            cfg.clamp();
            // After clamping nothing should have changed (values are already in range)
            assert_eq!(cfg, profile.config(), "{profile:?} values must already be in range");
        }
    }

    #[test]
    fn test_empty_toml_uses_defaults() {
        let cfg: FlashSafeConfig = toml::from_str("").unwrap();
        assert_eq!(cfg, FlashSafeConfig::default(), "empty TOML must yield defaults");
    }
}
