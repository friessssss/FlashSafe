//! FlashSafe Tauri shell: UI is always shown; closing the window exits the process.

use flashsafe_core::{sensitivity_presets, FlashSafeConfig, SensitivityPresets};
use parking_lot::RwLock;
use serde::Serialize;
use std::sync::Arc;
use tauri::Manager;

mod logging;
#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use win::engine::EngineStats;
#[cfg(windows)]
use win::engine::{spawn_engine_thread, EngineCommand};

#[derive(Clone, Serialize)]
pub struct WindowRow {
    pub hwnd: u64,
    pub title: String,
    pub pid: u32,
}

#[cfg(not(windows))]
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStats {
    pub running: bool,
    pub frames: u64,
    pub fps: f32,
    pub flashes: u64,
    pub mitigation: f32,
    pub hold: f32,
    pub last_error: Option<String>,
}

#[cfg(windows)]
#[derive(Clone)]
struct EngineCtl {
    tx: std::sync::mpsc::Sender<EngineCommand>,
    stats: Arc<RwLock<EngineStats>>,
}

#[cfg(windows)]
#[derive(Clone)]
struct AppState {
    engine: Arc<RwLock<Option<EngineCtl>>>,
    config: Arc<RwLock<FlashSafeConfig>>,
}

#[tauri::command]
fn list_windows() -> Vec<WindowRow> {
    #[cfg(windows)]
    {
        win::engine::enumerate_capture_windows()
            .into_iter()
            .map(|(hwnd, title, pid)| WindowRow { hwnd, title, pid })
            .collect()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[tauri::command]
fn load_config() -> FlashSafeConfig {
    config::load_json().unwrap_or_else(|e| {
        tracing::warn!("loading settings failed, using defaults: {e:#}");
        FlashSafeConfig::default()
    })
}

#[tauri::command]
fn save_config(mut cfg: FlashSafeConfig) -> Result<(), String> {
    cfg.clamp();
    config::save_json(&cfg).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_sensitivity_presets() -> SensitivityPresets {
    sensitivity_presets()
}

#[tauri::command]
#[cfg(windows)]
fn start_engine(
    state: tauri::State<'_, AppState>,
    hwnd: u64,
    mut cfg: FlashSafeConfig,
) -> Result<(), String> {
    cfg.clamp();
    let mut eng = state.engine.write();
    if eng.is_none() {
        let (tx, stats) = spawn_engine_thread();
        *eng = Some(EngineCtl { tx, stats });
    }
    let ctl = eng.as_ref().ok_or("engine")?;
    ctl.tx
        .send(EngineCommand::Start {
            hwnd: hwnd as isize,
            config: cfg.clone(),
        })
        .map_err(|e: std::sync::mpsc::SendError<EngineCommand>| e.to_string())?;
    *state.config.write() = cfg;
    Ok(())
}

#[tauri::command]
#[cfg(not(windows))]
fn start_engine(_hwnd: u64, _cfg: FlashSafeConfig) -> Result<(), String> {
    Err("FlashSafe requires Windows".into())
}

#[tauri::command]
#[cfg(windows)]
fn stop_engine(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let eng = state.engine.read();
    if let Some(ref c) = *eng {
        c.tx
            .send(EngineCommand::Stop)
            .map_err(|e: std::sync::mpsc::SendError<EngineCommand>| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
#[cfg(not(windows))]
fn stop_engine() -> Result<(), String> {
    Ok(())
}

#[tauri::command]
#[cfg(windows)]
fn update_engine_config(
    state: tauri::State<'_, AppState>,
    mut cfg: FlashSafeConfig,
) -> Result<(), String> {
    cfg.clamp();
    let eng = state.engine.read();
    if let Some(ref ctl) = *eng {
        ctl.tx
            .send(EngineCommand::UpdateConfig(cfg.clone()))
            .map_err(|e: std::sync::mpsc::SendError<EngineCommand>| e.to_string())?;
    }
    *state.config.write() = cfg;
    Ok(())
}

#[tauri::command]
#[cfg(not(windows))]
fn update_engine_config(_cfg: FlashSafeConfig) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
#[cfg(windows)]
fn get_engine_stats(state: tauri::State<'_, AppState>) -> EngineStats {
    let eng = state.engine.read();
    if let Some(ref c) = *eng {
        return c.stats.read().clone();
    }
    EngineStats::default()
}

#[tauri::command]
#[cfg(not(windows))]
fn get_engine_stats() -> EngineStats {
    EngineStats::default()
}

/// `%APPDATA%\FlashSafe` — settings and logs.
pub(crate) fn app_dir() -> std::path::PathBuf {
    std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("FlashSafe")
}

mod config {
    use anyhow::Result;
    use flashsafe_core::FlashSafeConfig;
    use std::path::PathBuf;

    fn path() -> Result<PathBuf> {
        let dir = crate::app_dir();
        std::fs::create_dir_all(&dir)?;
        Ok(dir.join("settings.json"))
    }

    /// Loads and migrates settings; a missing or corrupt file yields defaults.
    pub fn load_json() -> Result<FlashSafeConfig> {
        let p = path()?;
        if !p.exists() {
            return Ok(FlashSafeConfig::default());
        }
        let s = std::fs::read_to_string(&p)?;
        Ok(FlashSafeConfig::from_json(&s))
    }

    pub fn save_json(cfg: &FlashSafeConfig) -> Result<()> {
        let p = path()?;
        let s = serde_json::to_string_pretty(cfg)?;
        std::fs::write(&p, s)?;
        Ok(())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _log_guard = logging::init()
        .map_err(|e| eprintln!("logging disabled: {e:#}"))
        .ok();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "FlashSafe starting");
    #[cfg(windows)]
    {
        tauri::Builder::default()
            .manage(AppState {
                engine: Arc::new(RwLock::new(None)),
                config: Arc::new(RwLock::new(FlashSafeConfig::default())),
            })
            .invoke_handler(tauri::generate_handler![
                list_windows,
                load_config,
                save_config,
                get_sensitivity_presets,
                start_engine,
                stop_engine,
                update_engine_config,
                get_engine_stats,
            ])
            .on_window_event(|window, event| {
                if window.label() != "main" {
                    return;
                }
                if let tauri::WindowEvent::CloseRequested { .. } = event {
                    let app = window.app_handle();
                    let state = app.state::<AppState>();
                    let eng = state.engine.write();
                    if let Some(ref c) = *eng {
                        let _ = c.tx.send(EngineCommand::Stop);
                    }
                }
            })
            .run(tauri::generate_context!())
            .expect("error while running tauri application");
    }
    #[cfg(not(windows))]
    {
        eprintln!("FlashSafe requires Windows.");
    }
}
