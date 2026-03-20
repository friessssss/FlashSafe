//! `flashsafe-pipeline-bench` — end-to-end capture→detect→mitigate latency benchmark.
//!
//! Usage:
//!   cargo run --release --bin flashsafe-pipeline-bench
//!
//! Runs the full pipeline for 5 seconds on the primary display and reports:
//!   - Frames processed
//!   - Actual sustained FPS
//!   - Average, p50, and p99 end-to-end latency (capture → mitigate output)
//!   - Pass/fail against FLA-15 acceptance criteria:
//!       * p99 end-to-end latency < 16 ms
//!       * ≥ 60 fps sustained

use anyhow::Result;
use flashsafe_core::config::FlashSafeConfig;
use flashsafe_app::pipeline;
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let run_duration = Duration::from_secs(5);
    const CHAN_CAP: usize = 2048;

    let (timing_tx, timing_rx) = mpsc::sync_channel(CHAN_CAP);

    println!("Starting E2E pipeline benchmark for {} s …", run_duration.as_secs());

    let (handle, _stats) = pipeline::spawn(FlashSafeConfig::default(), Some(timing_tx))?;

    // Collect timings for the run duration.
    let bench_start = Instant::now();
    let mut timings: Vec<u64> = Vec::with_capacity(CHAN_CAP);

    while bench_start.elapsed() < run_duration {
        // Drain whatever has arrived.
        loop {
            match timing_rx.try_recv() {
                Ok(t) => timings.push(t.total.as_micros() as u64),
                Err(_) => break,
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    handle.shutdown();

    // Drain any remaining measurements flushed after shutdown.
    loop {
        match timing_rx.try_recv() {
            Ok(t) => timings.push(t.total.as_micros() as u64),
            Err(_) => break,
        }
    }

    if timings.is_empty() {
        eprintln!("No frames captured — is a display connected?");
        std::process::exit(1);
    }

    let elapsed_secs = run_duration.as_secs_f64();
    let fps = timings.len() as f64 / elapsed_secs;

    timings.sort_unstable();
    let avg_us = timings.iter().sum::<u64>() / timings.len() as u64;
    let p50_us = timings[timings.len() / 2];
    let p99_us = timings[(timings.len() as f64 * 0.99) as usize];

    let avg_ms = avg_us as f64 / 1_000.0;
    let p50_ms = p50_us as f64 / 1_000.0;
    let p99_ms = p99_us as f64 / 1_000.0;

    println!();
    println!("--- E2E Pipeline Results ---");
    println!("Frames processed : {}", timings.len());
    println!("Run duration     : {:.1} s", elapsed_secs);
    println!("Sustained FPS    : {fps:.1}");
    println!("Avg latency      : {avg_ms:.3} ms");
    println!("p50 latency      : {p50_ms:.3} ms");
    println!("p99 latency      : {p99_ms:.3} ms");
    println!();

    let fps_ok = fps >= 60.0;
    let lat_ok = p99_ms < 16.0;

    println!(
        "[{}] FPS target    : {fps:.1} {} 60.0",
        if fps_ok { "PASS" } else { "FAIL" },
        if fps_ok { ">=" } else { "<" }
    );
    println!(
        "[{}] p99 latency   : {p99_ms:.3} ms {} 16.000 ms",
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
