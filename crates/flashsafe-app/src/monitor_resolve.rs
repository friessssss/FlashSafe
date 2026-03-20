//! Resolve which DXGI output (monitor) a target-window title is on.
//!
//! [`resolve_capture_config`] converts a [`FlashSafeConfig`] into a
//! [`CaptureConfig`] by locating the visible window whose title matches
//! `target_process_name` and mapping its monitor to a DXGI output index.

use flashsafe_core::{capture::CaptureConfig, config::FlashSafeConfig};
use tracing::{debug, warn};

/// Derive a [`CaptureConfig`] from [`FlashSafeConfig`].
///
/// When `monitor_all_screens` is `true` or `target_process_name` is `None`,
/// returns the primary-monitor default (adapter 0, output 0).  Otherwise
/// locates the visible window whose title exactly matches `target_process_name`
/// and returns the [`CaptureConfig`] that targets its monitor.
pub fn resolve_capture_config(cfg: &FlashSafeConfig) -> CaptureConfig {
    if cfg.monitor_all_screens || cfg.target_process_name.is_none() {
        return CaptureConfig::default();
    }

    let title = cfg.target_process_name.as_deref().unwrap_or_default();
    if title.is_empty() {
        return CaptureConfig::default();
    }

    #[cfg(windows)]
    {
        match resolve_output_for_title(title) {
            Ok(output_index) => {
                debug!(title, output_index, "resolved target window to DXGI output");
                CaptureConfig {
                    output_index,
                    ..CaptureConfig::default()
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    title,
                    "failed to resolve target window monitor — using primary monitor"
                );
                CaptureConfig::default()
            }
        }
    }

    #[cfg(not(windows))]
    {
        let _ = title;
        CaptureConfig::default()
    }
}

/// Find which DXGI output index (on adapter 0) hosts the visible window
/// whose title equals `target_title`.
///
/// Returns `Ok(0)` as a safe fallback when the window is found but its
/// monitor is not enumerated on adapter 0 (e.g. driven by a secondary GPU).
#[cfg(windows)]
fn resolve_output_for_title(target_title: &str) -> anyhow::Result<u32> {
    use anyhow::Context;
    use windows::Win32::{
        Foundation::{BOOL, HWND, LPARAM},
        Graphics::{
            Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput},
            Gdi::{MonitorFromWindow, MONITOR_DEFAULTTOPRIMARY},
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
        },
    };

    // --- Step 1: find HWND for a visible window with exactly this title ---

    struct SearchCtx {
        target: String,
        found: Option<HWND>,
    }

    unsafe extern "system" fn find_hwnd_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut SearchCtx);
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return BOOL(1);
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let written = GetWindowTextW(hwnd, &mut buf);
        if written <= 0 {
            return BOOL(1);
        }
        let title = String::from_utf16_lossy(&buf[..written as usize]);
        if title == ctx.target {
            ctx.found = Some(hwnd);
            return BOOL(0); // stop enumeration early
        }
        BOOL(1)
    }

    let mut ctx = SearchCtx {
        target: target_title.to_string(),
        found: None,
    };
    unsafe {
        // EnumWindows returns Err when the callback stops it early (BOOL(0));
        // that is the expected path — ignore the error and check ctx.found.
        let _ = EnumWindows(
            Some(find_hwnd_cb),
            LPARAM(&mut ctx as *mut SearchCtx as isize),
        );
    }
    let hwnd = ctx
        .found
        .ok_or_else(|| anyhow::anyhow!("window '{target_title}' not found"))?;

    // --- Step 2: get the HMONITOR that hosts the window ---
    let hmonitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };

    // --- Step 3: map HMONITOR → DXGI output index on adapter 0 ---
    unsafe {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().context("CreateDXGIFactory1 failed")?;
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapters1(0)
            .context("no DXGI adapters found")?;

        let mut i = 0u32;
        loop {
            let output: IDXGIOutput = match adapter.EnumOutputs(i) {
                Ok(o) => o,
                Err(_) => break, // no more outputs on this adapter
            };
            let desc = output.GetDesc().context("IDXGIOutput::GetDesc failed")?;
            if desc.Monitor == hmonitor {
                return Ok(i);
            }
            i += 1;
        }
    }

    // The monitor is not enumerated on adapter 0 (e.g. secondary GPU).
    // Fall back to the primary monitor.
    Ok(0)
}
