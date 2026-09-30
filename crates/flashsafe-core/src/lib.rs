//! FlashSafe core: configuration, fast flash metrics, and mitigation parameters.
//! Platform capture and GPU presentation live in the Tauri host crate.

pub mod config;
pub mod detection;
pub mod mitigation;

pub use config::{sensitivity_presets, FlashSafeConfig, PipelineSettings, SensitivityPresets};
pub use detection::{
    DownsampleStats, FastFlashDetector, FastFlashDetectorConfig, FlashMetrics,
};
pub use mitigation::MitigationParams;
