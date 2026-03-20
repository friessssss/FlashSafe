//! FlashSafe application entry point.
//!
//! v0.1: system tray shell + basic config-backed menu.
//! Full capture/mitigation pipeline will be wired in later tasks.

use anyhow::{Context, Result};
use flashsafe_app::pipeline::{self, PipelineCmd};
use flashsafe_core::{
    capture::CaptureConfig,
    config::{load as load_config, save as save_config, FlashSafeConfig, Profile},
};
use std::sync::{mpsc, Arc, Mutex};
use tracing::{info, warn};
use tray_item::TrayItem;
use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK};

mod logging;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayEnabled {
    Enabled,
    Disabled,
}

#[derive(Debug)]
struct AppState {
    enabled: TrayEnabled,
    config: FlashSafeConfig,
    /// Channel to push config updates into the running pipeline.
    pipeline_tx: Option<mpsc::Sender<PipelineCmd>>,
}

fn profile_from_config(config: &FlashSafeConfig) -> Profile {
    // These are deterministic constants in `Profile::config()`, so float equality should
    // be exact after load+clamp. Still, use a tolerance to be robust to future changes.
    let strict = Profile::Strict.config();
    let balanced = Profile::Balanced.config();
    let minimal = Profile::Minimal.config();

    let eq = |a: f32, b: f32| (a - b).abs() <= 1e-5;
    let same = |p: Profile| match p {
        Profile::Strict => {
            eq(config.luminance_threshold, strict.luminance_threshold)
                && eq(config.red_threshold, strict.red_threshold)
                && eq(config.flash_rate_hz, strict.flash_rate_hz)
                && eq(config.mitigation_level, strict.mitigation_level)
                && config.ramp_ms == strict.ramp_ms
        }
        Profile::Balanced => {
            eq(config.luminance_threshold, balanced.luminance_threshold)
                && eq(config.red_threshold, balanced.red_threshold)
                && eq(config.flash_rate_hz, balanced.flash_rate_hz)
                && eq(config.mitigation_level, balanced.mitigation_level)
                && config.ramp_ms == balanced.ramp_ms
        }
        Profile::Minimal => {
            eq(config.luminance_threshold, minimal.luminance_threshold)
                && eq(config.red_threshold, minimal.red_threshold)
                && eq(config.flash_rate_hz, minimal.flash_rate_hz)
                && eq(config.mitigation_level, minimal.mitigation_level)
                && config.ramp_ms == minimal.ramp_ms
        }
    };

    if same(Profile::Strict) {
        Profile::Strict
    } else if same(Profile::Balanced) {
        Profile::Balanced
    } else {
        Profile::Minimal
    }
}

fn profile_display(profile: Profile) -> &'static str {
    match profile {
        Profile::Strict => "Strict",
        Profile::Balanced => "Balanced",
        Profile::Minimal => "Minimal",
    }
}

fn enabled_display(enabled: TrayEnabled) -> &'static str {
    match enabled {
        TrayEnabled::Enabled => "Active",
        TrayEnabled::Disabled => "Inactive",
    }
}

fn message_box_settings() -> Result<()> {
    let body = "FlashSafe (Phase 1)\n\nUse the tray menu to enable/disable mitigation and select a sensitivity profile.\n\nSettings panel will be expanded in later phases.";
    let title = "FlashSafe Settings";

    // Convert Rust UTF-8 to UTF-16 for MessageBoxW.
    fn to_utf16_ptr(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let title_w = to_utf16_ptr(title);
    let body_w = to_utf16_ptr(body);

    // SAFETY: vectors are kept alive for the duration of the call.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(body_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            MB_OK,
        );
    }
    Ok(())
}

fn tooltip_text(state: &AppState, profile: Profile) -> String {
    format!(
        "FlashSafe — {} ({})",
        enabled_display(state.enabled),
        profile_display(profile)
    )
}

fn build_tray(state: Arc<Mutex<AppState>>) -> Result<TrayItem> {
    let profile = {
        let st = state.lock().expect("state lock poisoned");
        profile_from_config(&st.config)
    };

    let mut tray = TrayItem::new("FlashSafe", tray_item::IconSource::Resource(""))?;

    // Toggle item.
    let enable_label = {
        let st = state.lock().expect("state lock poisoned");
        match st.enabled {
            TrayEnabled::Enabled => "Disable FlashSafe".to_string(),
            TrayEnabled::Disabled => "Enable FlashSafe".to_string(),
        }
    };

    tray.add_menu_item(&enable_label, {
        let state = Arc::clone(&state);
        let _tooltip = |st: &AppState| {
            let p = profile_from_config(&st.config);
            tooltip_text(st, p)
        };
        move || {
            let pipeline_update = {
                let mut st = state.lock().expect("state lock poisoned");
                st.enabled = match st.enabled {
                    TrayEnabled::Enabled => TrayEnabled::Disabled,
                    TrayEnabled::Disabled => TrayEnabled::Enabled,
                };
                // Push effective config: 0 mitigation when disabled.
                let effective = if st.enabled == TrayEnabled::Disabled {
                    FlashSafeConfig { mitigation_level: 0.0, ..st.config.clone() }
                } else {
                    st.config.clone()
                };
                st.pipeline_tx.as_ref().map(|tx| tx.send(PipelineCmd::UpdateConfig(effective)))
            };
            drop(pipeline_update); // result ignored — best-effort

            // Note: tray-item 0.10 does not expose set_tooltip; tooltip update skipped.
        }
    })?;

    // Profile selection: three separate items (acts like radio).
    for p in [Profile::Strict, Profile::Balanced, Profile::Minimal] {
        let label = format!(
            "{}{}",
            profile_display(p),
            if p == profile { " (✓)" } else { "" }
        );
        tray.add_menu_item(&label, {
            let state = Arc::clone(&state);
            move || {
                let pipeline_update = {
                    let mut st = state.lock().expect("state lock poisoned");
                    st.config = p.config();
                    st.config.clamp();
                    if let Err(e) = save_config(&st.config) {
                        warn!("failed to save config after profile change: {e:?}");
                    }
                    if st.enabled == TrayEnabled::Enabled {
                        st.pipeline_tx
                            .as_ref()
                            .map(|tx| tx.send(PipelineCmd::UpdateConfig(st.config.clone())))
                    } else {
                        None
                    }
                };
                drop(pipeline_update);

                // Note: tray-item 0.10 does not expose set_tooltip; tooltip update skipped.
            }
        })?;
    }

    // "Open Settings" placeholder: info dialog.
    tray.add_menu_item("Open Settings", move || {
        if let Err(e) = message_box_settings() {
            warn!("failed to open settings dialog: {e:?}");
        }
    })?;

    // Quit.
    tray.add_menu_item("Quit", move || {
        info!("FlashSafe quitting (tray menu)");
        std::process::exit(0);
    })?;

    Ok(tray)
}

fn main() -> Result<()> {
    // Initialise structured logging (stderr + rolling file).
    // The guard must live for the duration of main so the non-blocking writer
    // flushes on exit.
    let _log_guard = logging::init()?;

    info!("FlashSafe v{} starting", env!("CARGO_PKG_VERSION"));

    let loaded_config = load_config().context("loading flashsafe config")?;

    // Spawn the capture→detect→mitigate pipeline.
    let pipeline_handle = match pipeline::spawn(CaptureConfig::default(), loaded_config.clone(), None) {
        Ok(h) => {
            info!("pipeline started");
            Some(h)
        }
        Err(e) => {
            warn!(error = %e, "pipeline failed to start — running without capture");
            None
        }
    };

    let pipeline_tx = pipeline_handle
        .as_ref()
        .map(|h| h.cmd_sender());

    let state = Arc::new(Mutex::new(AppState {
        enabled: TrayEnabled::Enabled,
        config: loaded_config,
        pipeline_tx,
    }));

    // Create tray icon and then keep the process alive.
    let _tray = build_tray(Arc::clone(&state))?;
    info!("FlashSafe tray shell running");
    std::thread::park();
    Ok(())
}
