//! FlashSafe application entry point.
//!
//! v0.1: system tray shell + basic config-backed menu.
//! Full capture/mitigation pipeline will be wired in later tasks.

use anyhow::{Context, Result};
use eframe::egui;
use flashsafe_app::pipeline::{self, PipelineCmd, PipelineStats};
use flashsafe_core::config::{load as load_config, save as save_config, FlashSafeConfig, Profile};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};
use tray_item::{IconSource, TrayItem};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, MessageBoxW,
    MB_OK,
};
use windows_sys::Win32::Foundation::HINSTANCE;
use windows_sys::Win32::UI::WindowsAndMessaging::{LoadIconW, IDI_APPLICATION, IDI_SHIELD};

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
    /// Shared pipeline stats for tray tooltip + quick stats view.
    pipeline_stats: Option<Arc<RwLock<PipelineStats>>>,
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

fn format_uptime(session_start: std::time::Instant) -> String {
    let elapsed = session_start.elapsed();
    let total_secs = elapsed.as_secs();
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins}m {secs:02}s")
}

fn message_box_stats(stats: Option<PipelineStats>, profile: Profile, config: &FlashSafeConfig) -> Result<()> {
    let (uptime, frames_processed, flash_events_total, flashes_last_minute, mitigation_activations, current_dim) =
        if let Some(st) = stats {
            (
                format_uptime(st.session_start),
                st.frames_processed,
                st.flash_events_total,
                st.flashes_per_minute(),
                st.mitigation_activations,
                st.current_mitigation_level * 100.0,
            )
        } else {
            ("n/a".to_string(), 0, 0, 0, 0, 0.0)
        };

    let body = format!(
        "FlashSafe - Session Statistics\n\nUptime:              {uptime}\nFrames processed:    {frames_processed}\nFlash events:        {flash_events_total} total ({flashes_last_minute} in last 60s)\nMitigation active:   {mitigation_activations} times\nProfile:             {}\nThresholds:          Lum {:.2} / Red {:.2} / Rate {:.1} Hz\nCurrent dim level:   {:.1}%",
        profile_display(profile),
        config.luminance_threshold,
        config.red_threshold,
        config.flash_rate_hz,
        current_dim
    );
    let title = "FlashSafe - View Stats";

    // Convert Rust UTF-8 to UTF-16 for MessageBoxW.
    fn to_utf16_ptr(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let title_w = to_utf16_ptr(title);
    let body_w = to_utf16_ptr(&body);

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

fn tooltip_text(enabled: TrayEnabled, profile: Profile, flashes_per_minute: Option<u32>) -> String {
    match flashes_per_minute {
        Some(value) => format!(
            "FlashSafe - {} ({}) | {} flashes/min",
            enabled_display(enabled),
            profile_display(profile),
            value
        ),
        None => format!(
            "FlashSafe - {} ({})",
            enabled_display(enabled),
            profile_display(profile)
        ),
    }
}

fn tray_icon_for_enabled(enabled: TrayEnabled) -> IconSource {
    let resource = match enabled {
        TrayEnabled::Enabled => IDI_SHIELD,
        TrayEnabled::Disabled => IDI_APPLICATION,
    };
    let hicon = unsafe { LoadIconW(0 as HINSTANCE, resource) };
    if hicon != 0 {
        IconSource::RawIcon(hicon)
    } else {
        IconSource::Resource("IDI_APPLICATION")
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct MenuIds {
    toggle: Option<u32>,
    strict: Option<u32>,
    balanced: Option<u32>,
    minimal: Option<u32>,
}

#[derive(Debug, Clone)]
struct RunningApp {
    pid: u32,
    title: String,
}

#[derive(Debug)]
struct UiShellState {
    focus_requested: AtomicBool,
}

impl UiShellState {
    fn new() -> Self {
        Self {
            focus_requested: AtomicBool::new(false),
        }
    }
}

struct SettingsUi {
    state: Arc<Mutex<AppState>>,
    shell_state: Arc<UiShellState>,
    selected_profile: Profile,
    edit_config: FlashSafeConfig,
    running_apps: Vec<RunningApp>,
    selected_pid: Option<u32>,
    last_refresh: Instant,
    save_status: String,
}

impl SettingsUi {
    fn new(state: Arc<Mutex<AppState>>, shell_state: Arc<UiShellState>) -> Self {
        let (cfg, selected_profile) = {
            let st = state.lock().expect("state lock poisoned");
            (st.config.clone(), profile_from_config(&st.config))
        };
        let mut ui = Self {
            state,
            shell_state,
            selected_profile,
            edit_config: cfg,
            running_apps: Vec::new(),
            selected_pid: None,
            last_refresh: Instant::now() - Duration::from_secs(10),
            save_status: String::new(),
        };
        ui.refresh_running_apps();
        ui
    }

    fn refresh_running_apps(&mut self) {
        self.running_apps = enumerate_visible_windows();
        if self.running_apps.iter().all(|app| Some(app.pid) != self.selected_pid) {
            self.selected_pid = None;
        }
        self.last_refresh = Instant::now();
    }

    fn save_current(&mut self) {
        self.edit_config.clamp();
        {
            let mut st = self.state.lock().expect("state lock poisoned");
            st.config = self.edit_config.clone();
            st.enabled = if st.config.enabled {
                TrayEnabled::Enabled
            } else {
                TrayEnabled::Disabled
            };
            let effective = if st.config.enabled {
                st.config.clone()
            } else {
                FlashSafeConfig {
                    mitigation_level: 0.0,
                    ..st.config.clone()
                }
            };
            let _ = st
                .pipeline_tx
                .as_ref()
                .map(|tx| tx.send(PipelineCmd::UpdateConfig(effective)));
            if let Err(e) = save_config(&st.config) {
                self.save_status = format!("Save failed: {e}");
                return;
            }
        }
        self.save_status = "Saved settings".to_string();
    }
}

impl eframe::App for SettingsUi {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.shell_state.focus_requested.swap(false, Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }

        if self.last_refresh.elapsed() >= Duration::from_secs(2) {
            self.refresh_running_apps();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("FlashSafe Settings");
            ui.separator();

            ui.group(|ui| {
                ui.label("App Selector");
                ui.checkbox(&mut self.edit_config.monitor_all_screens, "Monitor all screens");
                if !self.edit_config.monitor_all_screens {
                    egui::ComboBox::from_label("Target application")
                        .selected_text(
                            self.selected_pid
                                .and_then(|pid| {
                                    self.running_apps
                                        .iter()
                                        .find(|a| a.pid == pid)
                                        .map(|a| format!("{} (PID {})", a.title, a.pid))
                                })
                                .unwrap_or_else(|| "Select running app".to_string()),
                        )
                        .show_ui(ui, |ui| {
                            for app in &self.running_apps {
                                let label = format!("{} (PID {})", app.title, app.pid);
                                if ui
                                    .selectable_label(self.selected_pid == Some(app.pid), label)
                                    .clicked()
                                {
                                    self.selected_pid = Some(app.pid);
                                    self.edit_config.target_process_name = Some(app.title.clone());
                                }
                            }
                        });
                } else {
                    self.edit_config.target_process_name = None;
                }
                if ui.button("Refresh app list").clicked() {
                    self.refresh_running_apps();
                }
            });

            ui.add_space(8.0);
            ui.group(|ui| {
                ui.label("Settings");
                egui::ComboBox::from_label("Sensitivity profile")
                    .selected_text(match self.selected_profile {
                        Profile::Strict => "High",
                        Profile::Balanced => "Medium",
                        Profile::Minimal => "Low",
                    })
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(self.selected_profile == Profile::Minimal, "Low")
                            .clicked()
                        {
                            self.selected_profile = Profile::Minimal;
                            self.edit_config = Profile::Minimal.config();
                        }
                        if ui
                            .selectable_label(self.selected_profile == Profile::Balanced, "Medium")
                            .clicked()
                        {
                            self.selected_profile = Profile::Balanced;
                            self.edit_config = Profile::Balanced.config();
                        }
                        if ui
                            .selectable_label(self.selected_profile == Profile::Strict, "High")
                            .clicked()
                        {
                            self.selected_profile = Profile::Strict;
                            self.edit_config = Profile::Strict.config();
                        }
                    });
                ui.add(
                    egui::Slider::new(&mut self.edit_config.mitigation_level, 0.0..=1.0)
                        .text("Mitigation level")
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                );
                ui.add(
                    egui::Slider::new(&mut self.edit_config.ramp_ms, 0..=500)
                        .text("Ramp duration (ms)"),
                );
                ui.checkbox(&mut self.edit_config.enabled, "Enable FlashSafe");
                if ui.button("Save").clicked() {
                    self.save_current();
                }
                if !self.save_status.is_empty() {
                    ui.label(&self.save_status);
                }
            });

            ui.add_space(8.0);
            ui.group(|ui| {
                ui.label("Live Status");
                let snapshot = {
                    let st = self.state.lock().expect("state lock poisoned");
                    let stats = st
                        .pipeline_stats
                        .as_ref()
                        .and_then(|shared| shared.read().ok().map(|s| s.clone()));
                    (st.enabled, stats)
                };

                let (enabled, stats_opt) = snapshot;
                ui.label(format!(
                    "Pipeline: {}",
                    if enabled == TrayEnabled::Enabled { "ON" } else { "OFF" }
                ));
                if let Some(stats) = stats_opt {
                    let uptime_s = stats.session_start.elapsed().as_secs_f32().max(1.0);
                    let fps = stats.frames_processed as f32 / uptime_s;
                    let last_flash = stats
                        .last_flash_at
                        .map(|t| format!("{:.1}s ago", t.elapsed().as_secs_f32()))
                        .unwrap_or_else(|| "never".to_string());
                    ui.label(format!("Flashes/min: {}", stats.flashes_per_minute()));
                    ui.label(format!("Frame processing rate: {:.1} fps", fps));
                    ui.label(format!("Last flash: {last_flash}"));
                } else {
                    ui.label("Pipeline stats unavailable");
                }
            });
        });
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let length = GetWindowTextLengthW(hwnd);
    if length <= 0 {
        return BOOL(1);
    }
    let mut buf = vec![0u16; (length + 1) as usize];
    let written = GetWindowTextW(hwnd, &mut buf);
    if written <= 0 {
        return BOOL(1);
    }
    let title = String::from_utf16_lossy(&buf[..written as usize]);
    if title.trim().is_empty() {
        return BOOL(1);
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return BOOL(1);
    }

    let apps = &mut *(lparam.0 as *mut Vec<RunningApp>);
    apps.push(RunningApp { pid, title });
    BOOL(1)
}

fn enumerate_visible_windows() -> Vec<RunningApp> {
    let mut apps: Vec<RunningApp> = Vec::new();
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::EnumWindows(
            Some(enum_windows_proc),
            LPARAM((&mut apps as *mut Vec<RunningApp>) as isize),
        )
    }
    .ok();
    let mut seen: HashSet<(u32, String)> = HashSet::new();
    apps.retain(|a| seen.insert((a.pid, a.title.clone())));
    apps.sort_by(|a, b| a.title.cmp(&b.title));
    apps
}

fn run_settings_window(state: Arc<Mutex<AppState>>, shell_state: Arc<UiShellState>) {
    let native_options = eframe::NativeOptions::default();
    let _ = eframe::run_native(
        "FlashSafe Settings",
        native_options,
        Box::new(move |_cc| Box::new(SettingsUi::new(state, shell_state))),
    );
}

fn refresh_tray(tray: &mut TrayItem, ids: &MenuIds, enabled: TrayEnabled, profile: Profile) {
    // Icon reflects active/inactive status.
    let _ = tray.inner_mut().set_icon(tray_icon_for_enabled(enabled));

    // Tooltip.
    let _ = tray
        .inner_mut()
        .set_tooltip(&tooltip_text(enabled, profile, None));

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

fn build_tray(state: Arc<Mutex<AppState>>, shell_state: Arc<UiShellState>) -> Result<Arc<Mutex<TrayItem>>> {
    let initial_snapshot = {
        let st = state.lock().expect("state lock poisoned");
        (st.enabled, st.config.clone())
    };
    let initial_profile = profile_from_config(&initial_snapshot.1);

    let icon = tray_icon_for_enabled(initial_snapshot.0);

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
                    st.config.enabled = st.enabled == TrayEnabled::Enabled;
                    if let Err(e) = save_config(&st.config) {
                        warn!("failed to save config after toggle change: {e:?}");
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
                        let currently_enabled = st.config.enabled;
                        let monitor_all = st.config.monitor_all_screens;
                        let target_process_name = st.config.target_process_name.clone();
                        st.config = p.config();
                        st.config.enabled = currently_enabled;
                        st.config.monitor_all_screens = monitor_all;
                        st.config.target_process_name = target_process_name;
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
            let shell_state = Arc::clone(&shell_state);
            move || {
                shell_state.focus_requested.store(true, Ordering::SeqCst);
            }
        })?;
        inner.add_menu_item_with_id("View Stats", {
            let state = Arc::clone(&state);
            move || {
                let (stats, profile, config) = {
                    let st = state.lock().expect("state lock poisoned");
                    let profile = profile_from_config(&st.config);
                    let stats = st
                        .pipeline_stats
                        .as_ref()
                        .and_then(|shared| shared.read().ok().map(|g| g.clone()));
                    (stats, profile, st.config.clone())
                };

                if let Err(e) = message_box_stats(stats, profile, &config) {
                    warn!("failed to open stats dialog: {e:?}");
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
    // The pipeline resolves which monitor to capture from `loaded_config`
    // (target_process_name + monitor_all_screens).
    let (pipeline_handle, pipeline_stats) =
        match pipeline::spawn(loaded_config.clone(), None) {
        Ok((h, stats)) => {
            info!("pipeline started");
            (Some(h), Some(stats))
        }
        Err(e) => {
            warn!(error = %e, "pipeline failed to start — running without capture");
            (None, None)
        }
    };

    let pipeline_tx = pipeline_handle
        .as_ref()
        .map(|h| h.cmd_sender());

    let state = Arc::new(Mutex::new(AppState {
        enabled: if loaded_config.enabled {
            TrayEnabled::Enabled
        } else {
            TrayEnabled::Disabled
        },
        config: loaded_config,
        pipeline_tx,
        pipeline_stats,
    }));
    let shell_state = Arc::new(UiShellState::new());

    // Create tray icon and then keep the process alive.
    let tray = build_tray(Arc::clone(&state), Arc::clone(&shell_state))?;
    {
        let state = Arc::clone(&state);
        let shell_state = Arc::clone(&shell_state);
        std::thread::spawn(move || run_settings_window(state, shell_state));
    }
    {
        let state = Arc::clone(&state);
        let tray = Arc::clone(&tray);
        std::thread::spawn(move || loop {
            let (enabled, profile, flashes_per_minute) = {
                let st = state.lock().expect("state lock poisoned");
                let flashes = st
                    .pipeline_stats
                    .as_ref()
                    .and_then(|shared| shared.read().ok().map(|s| s.flashes_per_minute()));
                (st.enabled, profile_from_config(&st.config), flashes)
            };

            if let Ok(mut t) = tray.lock() {
                let _ = t
                    .inner_mut()
                    .set_tooltip(&tooltip_text(enabled, profile, flashes_per_minute));
            }
            std::thread::sleep(Duration::from_secs(1));
        });
    }
    info!("FlashSafe tray shell running");
    std::thread::park();
    Ok(())
}
