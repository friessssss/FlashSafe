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
use tray_item::{IconSource, TrayItem};
use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK};
use windows_sys::Win32::Foundation::HINSTANCE;
use windows_sys::Win32::UI::WindowsAndMessaging::{LoadIconW, IDI_APPLICATION};

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

fn tooltip_text(enabled: TrayEnabled, profile: Profile) -> String {
    format!(
        "FlashSafe — {} ({})",
        enabled_display(enabled),
        profile_display(profile)
    )
}

#[derive(Debug, Default, Clone, Copy)]
struct MenuIds {
    toggle: Option<u32>,
    strict: Option<u32>,
    balanced: Option<u32>,
    minimal: Option<u32>,
}

fn refresh_tray(tray: &mut TrayItem, ids: &MenuIds, enabled: TrayEnabled, profile: Profile) {
    // Tooltip.
    let _ = tray
        .inner_mut()
        .set_tooltip(&tooltip_text(enabled, profile));

    // Enable/disable menu label.
    if let Some(id) = ids.toggle {
        let label = match enabled {
            TrayEnabled::Enabled => "Disable FlashSafe".to_string(),
            TrayEnabled::Disabled => "Enable FlashSafe".to_string(),
        };
        let _ = tray.inner_mut().set_menu_item_label(&label, id);
    }

    let mk_profile_label = |p: Profile| -> String {
        format!(
            "{}{}",
            profile_display(p),
            if p == profile { " (✓)" } else { "" }
        )
    };

    if let Some(id) = ids.strict {
        let _ = tray
            .inner_mut()
            .set_menu_item_label(&mk_profile_label(Profile::Strict), id);
    }
    if let Some(id) = ids.balanced {
        let _ = tray
            .inner_mut()
            .set_menu_item_label(&mk_profile_label(Profile::Balanced), id);
    }
    if let Some(id) = ids.minimal {
        let _ = tray
            .inner_mut()
            .set_menu_item_label(&mk_profile_label(Profile::Minimal), id);
    }
}

fn build_tray(state: Arc<Mutex<AppState>>) -> Result<Arc<Mutex<TrayItem>>> {
    let initial_snapshot = {
        let st = state.lock().expect("state lock poisoned");
        (st.enabled, st.config.clone())
    };
    let initial_profile = profile_from_config(&initial_snapshot.1);

    let hicon = unsafe { LoadIconW(0 as HINSTANCE, IDI_APPLICATION) };
    let icon = if hicon != 0 {
        IconSource::RawIcon(hicon)
    } else {
        IconSource::Resource("IDI_APPLICATION")
    };

    let tray = TrayItem::new("FlashSafe", icon)?;
    let tray_arc = Arc::new(Mutex::new(tray));
    let menu_ids = Arc::new(Mutex::new(MenuIds::default()));

    // Add menu items on the tray thread.
    {
        let mut tray = tray_arc.lock().expect("tray mutex poisoned");
        let inner = tray.inner_mut();

        // Toggle item.
        let toggle_label = match initial_snapshot.0 {
            TrayEnabled::Enabled => "Disable FlashSafe",
            TrayEnabled::Disabled => "Enable FlashSafe",
        };
        let toggle_id = inner.add_menu_item_with_id(toggle_label, {
            let state = Arc::clone(&state);
            let tray_arc = Arc::clone(&tray_arc);
            let menu_ids = Arc::clone(&menu_ids);

            move || {
                let (enabled, config) = {
                    let mut st = state.lock().expect("state lock poisoned");
                    st.enabled = match st.enabled {
                        TrayEnabled::Enabled => TrayEnabled::Disabled,
                        TrayEnabled::Disabled => TrayEnabled::Enabled,
                    };
                    let effective = if st.enabled == TrayEnabled::Disabled {
                        FlashSafeConfig {
                            mitigation_level: 0.0,
                            ..st.config.clone()
                        }
                    } else {
                        st.config.clone()
                    };
                    st.pipeline_tx
                        .as_ref()
                        .map(|tx| tx.send(PipelineCmd::UpdateConfig(effective)));
                    (st.enabled, st.config.clone())
                };

                let profile = profile_from_config(&config);
                let ids = *menu_ids.lock().expect("menu ids mutex poisoned");
                if let Ok(mut tray) = tray_arc.lock() {
                    refresh_tray(&mut tray, &ids, enabled, profile);
                }
            }
        })?;

        // Profile header.
        inner.add_separator()?;
        inner.add_label("Profile")?;

        // Profile "radio" items.
        let mut ids_guard = menu_ids.lock().expect("menu ids mutex poisoned");
        for p in [Profile::Strict, Profile::Balanced, Profile::Minimal] {
            let label = mk_profile_label(p, initial_profile);
            let id = inner.add_menu_item_with_id(&label, {
                let state = Arc::clone(&state);
                let tray_arc = Arc::clone(&tray_arc);
                let menu_ids = Arc::clone(&menu_ids);
                move || {
                    let (enabled, config) = {
                        let mut st = state.lock().expect("state lock poisoned");
                        st.config = p.config();
                        st.config.clamp();
                        if let Err(e) = save_config(&st.config) {
                            warn!("failed to save config after profile change: {e:?}");
                        }
                        let effective = if st.enabled == TrayEnabled::Disabled {
                            FlashSafeConfig {
                                mitigation_level: 0.0,
                                ..st.config.clone()
                            }
                        } else {
                            st.config.clone()
                        };
                        st.pipeline_tx
                            .as_ref()
                            .map(|tx| tx.send(PipelineCmd::UpdateConfig(effective)));
                        (st.enabled, st.config.clone())
                    };

                    let profile = profile_from_config(&config);
                    let ids = *menu_ids.lock().expect("menu ids mutex poisoned");
                    if let Ok(mut tray) = tray_arc.lock() {
                        refresh_tray(&mut tray, &ids, enabled, profile);
                    }
                }
            })?;

            match p {
                Profile::Strict => ids_guard.strict = Some(id),
                Profile::Balanced => ids_guard.balanced = Some(id),
                Profile::Minimal => ids_guard.minimal = Some(id),
            }
        }

        ids_guard.toggle = Some(toggle_id);

        // Final separator + settings and quit.
        inner.add_separator()?;
        inner.add_menu_item_with_id("Open Settings", {
            move || {
                if let Err(e) = message_box_settings() {
                    warn!("failed to open settings dialog: {e:?}");
                }
            }
        })?;
        inner.add_menu_item_with_id("Quit", {
            move || {
                info!("FlashSafe quitting (tray menu)");
                std::process::exit(0);
            }
        })?;

        refresh_tray(
            &mut tray,
            &*menu_ids.lock().expect("menu ids mutex poisoned"),
            initial_snapshot.0,
            initial_profile,
        );
    }

    Ok(tray_arc)
}

fn mk_profile_label(p: Profile, selected: Profile) -> String {
    format!(
        "{}{}",
        profile_display(p),
        if p == selected { " (✓)" } else { "" }
    )
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
