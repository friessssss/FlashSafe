//! FlashSafe application entry point.
//!
//! v0.1 skeleton: initialises logging and prints a startup banner.
//! Screen capture and overlay will be wired in subsequent tasks.

use anyhow::Result;
use tracing::info;

fn main() -> Result<()> {
    // Initialise structured logging. RUST_LOG controls the filter.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("FlashSafe v{} starting", env!("CARGO_PKG_VERSION"));
    info!("Detection and capture pipeline not yet wired — skeleton only");

    Ok(())
}
