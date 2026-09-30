//! FlashSafe core: colour math, tile statistics, the WCAG flash judge, the
//! mitigation filter (CPU reference) and persisted configuration.
//! Platform capture and GPU presentation live in the Tauri host crate.

pub mod config;
pub mod filter;
pub mod luma;
pub mod tiles;
pub mod wcag;

pub use config::{sensitivity_presets, FlashSafeConfig, Preset, SensitivityPresets, CONFIG_VERSION};
pub use filter::{apply_pixel, FilterParams, FilterSummary, PixelFilter, TileFilter};
pub use tiles::{tile_luminance_bgra, tile_stats_f32x4, BgraFrame, TileStats, TILE_COLS, TILE_ROWS};
