//! FlashSafe application entry point.
//!
//! v0.1 skeleton: initialises structured logging and prints a startup banner.
//! Screen capture and overlay will be wired in subsequent tasks.

use anyhow::Result;
use tracing::info;

mod logging;

fn main() -> Result<()> {
    // Initialise structured logging (stderr + rolling file).
    // The guard must live for the duration of main so the non-blocking writer
    // flushes on exit.
    let _log_guard = logging::init()?;

    info!("FlashSafe v{} starting", env!("CARGO_PKG_VERSION"));
    info!("Detection and capture pipeline not yet wired — skeleton only");

    Ok(())
}
