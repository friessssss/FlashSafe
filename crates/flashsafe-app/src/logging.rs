//! Logging initialisation for the FlashSafe application.
//!
//! Sets up a dual-sink `tracing` subscriber:
//!
//! 1. **stderr** — human-readable coloured output for interactive use.
//! 2. **File** — plain-text structured logs with daily rotation at
//!    `%APPDATA%\FlashSafe\logs\` (Windows) / `~/.local/share/FlashSafe/logs/`
//!    (other platforms), retaining a maximum of 7 daily files.
//!
//! # Log level configuration
//! The log filter is resolved in priority order:
//! 1. `FLASHSAFE_LOG` environment variable
//! 2. `RUST_LOG` environment variable
//! 3. Default: `info`
//!
//! # Usage
//! ```rust,ignore
//! let _guard = flashsafe_app::logging::init()?;
//! // _guard must remain alive for the duration of the process so the
//! // non-blocking file writer flushes correctly on shutdown.
//! ```
//!
//! # Privacy
//! No raw pixel data or personally-identifiable information is ever logged.
//! All log call sites in the codebase must adhere to this constraint.

use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Initialise the global `tracing` subscriber.
///
/// Returns a [`WorkerGuard`] that **must** be kept alive (typically bound to a
/// variable in `main`) to ensure the non-blocking file writer flushes all
/// buffered records before the process exits.
pub fn init() -> Result<WorkerGuard> {
    let log_dir = log_directory()?;
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create log directory {}", log_dir.display()))?;

    // Resolve log filter: FLASHSAFE_LOG → RUST_LOG → "info".
    let env_filter = EnvFilter::try_from_env("FLASHSAFE_LOG")
        .or_else(|_| EnvFilter::try_from_env("RUST_LOG"))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // Daily rolling file, keep at most 7 files.
    let file_appender = rolling::Builder::new()
        .rotation(rolling::Rotation::DAILY)
        .filename_prefix("flashsafe")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&log_dir)
        .with_context(|| {
            format!("failed to create rolling log appender at {}", log_dir.display())
        })?;

    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    // stderr layer: coloured, for interactive terminals.
    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_thread_ids(false);

    // File layer: no ANSI escape codes; plain text suitable for parsing.
    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    Ok(guard)
}

/// Returns the platform-appropriate log directory.
///
/// - Windows: `%APPDATA%\FlashSafe\logs`
/// - Other:   `~/.local/share/FlashSafe/logs`
fn log_directory() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
        Ok(PathBuf::from(appdata).join("FlashSafe").join("logs"))
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("FlashSafe")
            .join("logs"))
    }
}
