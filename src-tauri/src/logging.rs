//! File logging: `%APPDATA%\FlashSafe\logs\flashsafe.YYYY-MM-DD.log`, daily
//! rotation, 7 files kept. Friends can attach these to bug reports.
//!
//! Filter: `FLASHSAFE_LOG`, then `RUST_LOG`, then `info`.
//! Never log pixel data or window titles beyond what the user selected.

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Install the global subscriber. Keep the guard alive for the whole process
/// so buffered records are flushed on exit.
pub fn init() -> Result<WorkerGuard> {
    let dir = crate::app_dir().join("logs");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let filter = EnvFilter::try_from_env("FLASHSAFE_LOG")
        .or_else(|_| EnvFilter::try_from_env("RUST_LOG"))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let appender = rolling::Builder::new()
        .rotation(rolling::Rotation::DAILY)
        .filename_prefix("flashsafe")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .context("rolling log appender")?;
    let (file, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(fmt::layer().with_ansi(false).with_writer(file))
        .try_init()
        .context("install tracing subscriber")?;
    Ok(guard)
}
