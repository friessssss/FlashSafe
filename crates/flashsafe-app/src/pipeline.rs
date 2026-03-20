//! End-to-end capture → detect → mitigate pipeline.
//!
//! [`Pipeline`] owns all subsystems and runs the frame loop on a dedicated
//! thread.  Config changes are delivered via an `mpsc` channel so the owning
//! thread (or UI) can update thresholds without stopping the pipeline.

use anyhow::Result;
use flashsafe_core::{
    capture::{CaptureConfig, Capturer},
    config::FlashSafeConfig,
    detection::{
        LuminanceDetector, LuminanceDetectorConfig, RedFlashDetector, RedFlashDetectorConfig,
    },
    gamma::GammaDimmer,
    mitigation::MitigationFilter,
};
use std::{
    collections::VecDeque,
    sync::mpsc,
    sync::{Arc, RwLock},
    thread,
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

/// A single end-to-end frame timing measurement.
#[derive(Debug, Clone, Copy)]
pub struct FrameTiming {
    /// Wall-clock time at which capture started.
    pub captured_at: Instant,
    /// Total time from capture start to mitigation output (inclusive).
    pub total: Duration,
    /// Whether a flash event (luminance or red) was detected this frame.
    pub flash_detected: bool,
}

/// Shared pipeline stats for tray/UI status surfaces.
#[derive(Debug, Clone)]
pub struct PipelineStats {
    pub frames_processed: u64,
    pub flash_events_total: u64,
    pub mitigation_activations: u64,
    pub last_flash_at: Option<Instant>,
    pub current_mitigation_level: f32,
    pub session_start: Instant,
    /// Rolling 60-second flash event timestamps.
    pub recent_flash_times: VecDeque<Instant>,
}

impl Default for PipelineStats {
    fn default() -> Self {
        Self {
            frames_processed: 0,
            flash_events_total: 0,
            mitigation_activations: 0,
            last_flash_at: None,
            current_mitigation_level: 0.0,
            session_start: Instant::now(),
            recent_flash_times: VecDeque::new(),
        }
    }
}

impl PipelineStats {
    pub fn flashes_per_minute(&self) -> u32 {
        let now = Instant::now();
        self.recent_flash_times
            .iter()
            .filter(|&&t| now.duration_since(t) <= Duration::from_secs(60))
            .count() as u32
    }
}

/// Message sent to the pipeline thread to request a config update or shutdown.
pub enum PipelineCmd {
    /// Replace the active [`FlashSafeConfig`] on the next frame.
    UpdateConfig(FlashSafeConfig),
    /// Ask the pipeline to stop after the current frame.
    Shutdown,
}

/// Handle returned from [`Pipeline::spawn`].
///
/// Drop the handle to signal shutdown (the join is best-effort).
pub struct PipelineHandle {
    sender: mpsc::Sender<PipelineCmd>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PipelineHandle {
    /// Return a cloneable sender for pushing [`PipelineCmd`] messages.
    pub fn cmd_sender(&self) -> mpsc::Sender<PipelineCmd> {
        self.sender.clone()
    }

    /// Send a config update to the running pipeline.
    pub fn update_config(&self, cfg: FlashSafeConfig) {
        let _ = self.sender.send(PipelineCmd::UpdateConfig(cfg));
    }

    /// Request a graceful shutdown and wait for the thread to exit.
    pub fn shutdown(mut self) {
        let _ = self.sender.send(PipelineCmd::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PipelineHandle {
    fn drop(&mut self) {
        let _ = self.sender.send(PipelineCmd::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Spawn the capture→detect→mitigate pipeline on a background thread.
///
/// # Arguments
/// * `capture_cfg` – DXGI capture config (adapter/monitor selection).
/// * `initial_cfg` – Initial [`FlashSafeConfig`] (sensitivity + mitigation).
/// * `timing_tx`   – Optional channel to receive per-frame [`FrameTiming`]
///                    measurements (used by benchmarks; `None` in production).
///
/// Returns a [`PipelineHandle`] plus shared [`PipelineStats`].
pub fn spawn(
    capture_cfg: CaptureConfig,
    initial_cfg: FlashSafeConfig,
    timing_tx: Option<mpsc::SyncSender<FrameTiming>>,
) -> Result<(PipelineHandle, Arc<RwLock<PipelineStats>>)> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<PipelineCmd>();
    let stats = Arc::new(RwLock::new(PipelineStats::default()));

    // Validate that we can create a Capturer before spawning the thread.
    let mut capturer = Capturer::new(capture_cfg)?;
    let stats_for_thread = Arc::clone(&stats);

    let handle = thread::Builder::new()
        .name("flashsafe-pipeline".into())
        .spawn(move || {
            run_loop(
                &mut capturer,
                initial_cfg,
                cmd_rx,
                timing_tx,
                stats_for_thread,
            );
        })?;

    Ok((
        PipelineHandle {
            sender: cmd_tx,
            thread: Some(handle),
        },
        stats,
    ))
}

// ---------------------------------------------------------------------------
// Internal pipeline loop
// ---------------------------------------------------------------------------

fn run_loop(
    capturer: &mut Capturer,
    mut cfg: FlashSafeConfig,
    cmd_rx: mpsc::Receiver<PipelineCmd>,
    timing_tx: Option<mpsc::SyncSender<FrameTiming>>,
    stats: Arc<RwLock<PipelineStats>>,
) {
    let mut lum_det = build_lum_detector(&cfg);
    let mut red_det = build_red_detector(&cfg);
    let mut mit_filter = MitigationFilter::new(Duration::from_millis(cfg.ramp_ms as u64));
    // GammaDimmer owns the hardware gamma ramp (or overlay fallback) for this
    // pipeline session.  Dropping it at end-of-loop restores the original ramp.
    let dimmer = GammaDimmer::new();

    let mut frame_count = 0u64;

    loop {
        // --- Drain pending commands (non-blocking) ---
        loop {
            match cmd_rx.try_recv() {
                Ok(PipelineCmd::Shutdown) => {
                    info!(frames = frame_count, "pipeline shutting down");
                    return;
                }
                Ok(PipelineCmd::UpdateConfig(new_cfg)) => {
                    info!("pipeline config updated");
                    lum_det = build_lum_detector(&new_cfg);
                    red_det = build_red_detector(&new_cfg);
                    mit_filter =
                        MitigationFilter::new(Duration::from_millis(new_cfg.ramp_ms as u64));
                    // Restore display immediately when disabled (mitigation_level == 0).
                    dimmer.set_level(0.0);
                    cfg = new_cfg;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    info!(frames = frame_count, "pipeline command channel closed — stopping");
                    return;
                }
            }
        }

        // --- Capture ---
        let t_capture = Instant::now();
        let raw_frame = match capturer.grab() {
            Ok(Some(f)) => f,
            Ok(None) => continue, // timeout — no new frame yet
            Err(e) => {
                warn!(error = %e, "capture error — skipping frame");
                continue;
            }
        };

        let ts = t_capture; // use capture start as frame timestamp
        let w = raw_frame.width;
        let h = raw_frame.height;
        let data = raw_frame.data;

        // --- Detect ---
        let lum_event = lum_det.push_bgra(&data, w, h, ts);
        let red_event = red_det.push_bgra(&data, w, h, ts);

        let flash_detected = lum_event.is_some() || red_event.is_some();

        if let Some(ref ev) = lum_event {
            debug!(
                severity = ev.severity,
                "luminance flash event at {:.3} flashes/s", ev.severity
            );
        }
        if let Some(ref ev) = red_event {
            debug!(
                severity = ev.severity,
                "red flash event at {:.3} flashes/s", ev.severity
            );
        }

        // --- Mitigate ---
        // Advance the smooth-ramp state without touching the (read-only) pixel
        // buffer; apply the resulting level to the hardware gamma ramp instead.
        let target_level = if flash_detected { cfg.mitigation_level } else { 0.0 };
        mit_filter.tick(target_level, ts);
        dimmer.set_level(mit_filter.level());

        let total = t_capture.elapsed();
        frame_count += 1;

        debug!(
            frame = frame_count,
            flash = flash_detected,
            latency_us = total.as_micros(),
            "frame processed"
        );

        if let Ok(mut st) = stats.write() {
            st.frames_processed = frame_count;
            st.current_mitigation_level = target_level;

            if flash_detected {
                st.flash_events_total += 1;
                st.last_flash_at = Some(ts);
                st.recent_flash_times.push_back(ts);
            }

            if target_level > 0.0 {
                st.mitigation_activations += 1;
            }

            let cutoff = ts.checked_sub(Duration::from_secs(60)).unwrap_or(ts);
            while let Some(&front) = st.recent_flash_times.front() {
                if front < cutoff {
                    st.recent_flash_times.pop_front();
                } else {
                    break;
                }
            }
        }

        // --- Emit timing (benchmarks only) ---
        if let Some(ref tx) = timing_tx {
            let timing = FrameTiming {
                captured_at: t_capture,
                total,
                flash_detected,
            };
            // Use try_send so a slow consumer never blocks the pipeline.
            if tx.try_send(timing).is_err() {
                debug!("timing channel full — dropping measurement");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers to build detectors from config
// ---------------------------------------------------------------------------

fn build_lum_detector(cfg: &FlashSafeConfig) -> LuminanceDetector {
    LuminanceDetector::new(LuminanceDetectorConfig {
        change_threshold: cfg.luminance_threshold,
        max_flashes_per_second: cfg.flash_rate_hz,
        ..Default::default()
    })
}

fn build_red_detector(cfg: &FlashSafeConfig) -> RedFlashDetector {
    RedFlashDetector::new(RedFlashDetectorConfig {
        change_threshold: cfg.red_threshold,
        max_flashes_per_second: cfg.flash_rate_hz,
        ..Default::default()
    })
}
