//! `flashsafe-capture-bench` — measures DXGI capture throughput and latency.
//!
//! Usage:
//!   cargo run --release --bin flashsafe-capture-bench
//!
//! Prints:
//!   - Actual sustained FPS over 1 000 frames
//!   - Average, p50, and p99 per-frame grab latency
//!   - Pass/fail against the FLA-6 acceptance criteria:
//!       * >= 60 fps sustained
//!       * p99 grab latency < 5 ms

use anyhow::Result;
use flashsafe_core::capture::{CaptureConfig, Capturer};
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let config = CaptureConfig::default();
    let mut capturer = Capturer::new(config)?;

    println!(
        "Display: {}x{} — warming up for 1 s …",
        capturer.width(),
        capturer.height()
    );

    // Warm-up: discard frames for 1 second.
    let warmup_end = Instant::now() + Duration::from_secs(1);
    while Instant::now() < warmup_end {
        capturer.grab()?;
    }

    println!("Benchmarking 1 000 frames …");

    const TARGET: usize = 1_000;
    let mut latencies_us: Vec<u64> = Vec::with_capacity(TARGET);
    let bench_start = Instant::now();
    let mut grabbed = 0usize;

    while grabbed < TARGET {
        let t0 = Instant::now();
        if capturer.grab()?.is_some() {
            latencies_us.push(t0.elapsed().as_micros() as u64);
            grabbed += 1;
        }
    }

    let elapsed = bench_start.elapsed();
    let fps = TARGET as f64 / elapsed.as_secs_f64();

    latencies_us.sort_unstable();
    let avg_us = latencies_us.iter().sum::<u64>() / latencies_us.len() as u64;
    let p50_us = latencies_us[latencies_us.len() / 2];
    let p99_us = latencies_us[(latencies_us.len() as f64 * 0.99) as usize];

    let avg_ms = avg_us as f64 / 1_000.0;
    let p50_ms = p50_us as f64 / 1_000.0;
    let p99_ms = p99_us as f64 / 1_000.0;

    println!();
    println!("--- Results ---");
    println!("Frames captured  : {TARGET}");
    println!("Total time       : {:.3} s", elapsed.as_secs_f64());
    println!("Sustained FPS    : {fps:.1}");
    println!("Avg grab latency : {avg_ms:.3} ms");
    println!("p50 grab latency : {p50_ms:.3} ms");
    println!("p99 grab latency : {p99_ms:.3} ms");
    println!();

    let fps_ok = fps >= 60.0;
    let lat_ok = p99_ms < 5.0;

    println!(
        "[{}] FPS target    : {fps:.1} {} 60.0",
        if fps_ok { "PASS" } else { "FAIL" },
        if fps_ok { ">=" } else { "<" }
    );
    println!(
        "[{}] p99 latency   : {p99_ms:.3} ms {} 5.000 ms",
        if lat_ok { "PASS" } else { "FAIL" },
        if lat_ok { "<" } else { ">=" }
    );

    if fps_ok && lat_ok {
        println!("\nAll acceptance criteria met.");
        std::process::exit(0);
    } else {
        eprintln!("\nOne or more acceptance criteria not met.");
        std::process::exit(1);
    }
}
