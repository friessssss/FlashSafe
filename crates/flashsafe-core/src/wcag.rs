//! WCAG 2.x "three flashes" judge.
//!
//! A *transition* is a change in relative luminance of at least `threshold`
//! (WCAG: 10% of max) where the darker state is below `dark_limit` (WCAG:
//! 0.80). A *flash* is a pair of opposing transitions. Content fails when more
//! than three flashes occur in any one-second window.
//!
//! Transitions are found with a hysteresis extremum tracker, so a gradual
//! strobe sampled at a high frame rate (small per-frame deltas) still counts —
//! unlike a naive per-frame delta check.
//!
//! This is used by the test suite to score filter output, and its tracker is
//! reused by the filter to measure strobe activity. Not a medical device.

use std::collections::VecDeque;

/// WCAG general-flash thresholds.
pub const WCAG_TRANSITION: f32 = 0.10;
pub const WCAG_DARK_LIMIT: f32 = 0.80;
pub const WCAG_MAX_FLASHES_PER_SECOND: f32 = 3.0;

/// Tracks local extrema of a luminance signal and reports each opposing
/// transition of at least `threshold`.
#[derive(Debug, Clone)]
pub struct TransitionTracker {
    threshold: f32,
    dark_limit: f32,
    /// +1 rising, -1 falling, 0 undecided.
    dir: i8,
    /// Extreme in the current direction (max while rising, min while falling).
    extreme: f32,
    /// Min / max seen while undecided.
    lo: f32,
    hi: f32,
    started: bool,
}

impl TransitionTracker {
    pub fn new(threshold: f32, dark_limit: f32) -> Self {
        Self {
            threshold,
            dark_limit,
            dir: 0,
            extreme: 0.0,
            lo: 0.0,
            hi: 0.0,
            started: false,
        }
    }

    pub fn wcag() -> Self {
        Self::new(WCAG_TRANSITION, WCAG_DARK_LIMIT)
    }

    /// Feed one sample; returns the direction (+1 / -1) of a completed
    /// transition, if this sample completes one.
    pub fn push(&mut self, l: f32) -> Option<i8> {
        if !self.started {
            self.started = true;
            self.lo = l;
            self.hi = l;
            self.extreme = l;
            return None;
        }
        match self.dir {
            0 => {
                self.lo = self.lo.min(l);
                self.hi = self.hi.max(l);
                if l - self.lo >= self.threshold {
                    let from = self.lo;
                    self.dir = 1;
                    self.extreme = l;
                    return self.counts(from, l).then_some(1);
                }
                if self.hi - l >= self.threshold {
                    let from = self.hi;
                    self.dir = -1;
                    self.extreme = l;
                    return self.counts(l, from).then_some(-1);
                }
                None
            }
            1 => {
                if l > self.extreme {
                    self.extreme = l;
                    None
                } else if self.extreme - l >= self.threshold {
                    let from = self.extreme;
                    self.dir = -1;
                    self.extreme = l;
                    self.counts(l, from).then_some(-1)
                } else {
                    None
                }
            }
            _ => {
                if l < self.extreme {
                    self.extreme = l;
                    None
                } else if l - self.extreme >= self.threshold {
                    let from = self.extreme;
                    self.dir = 1;
                    self.extreme = l;
                    self.counts(from, l).then_some(1)
                } else {
                    None
                }
            }
        }
    }

    fn counts(&self, darker: f32, _brighter: f32) -> bool {
        darker < self.dark_limit
    }

    pub fn reset(&mut self) {
        *self = Self::new(self.threshold, self.dark_limit);
    }
}

/// Sliding-window flash counter over a timestamped luminance series.
#[derive(Debug, Clone)]
pub struct FlashJudge {
    tracker: TransitionTracker,
    window_secs: f64,
    transitions: VecDeque<f64>,
    max_transitions_in_window: usize,
}

impl Default for FlashJudge {
    fn default() -> Self {
        Self::new(TransitionTracker::wcag(), 1.0)
    }
}

impl FlashJudge {
    pub fn new(tracker: TransitionTracker, window_secs: f64) -> Self {
        Self {
            tracker,
            window_secs,
            transitions: VecDeque::new(),
            max_transitions_in_window: 0,
        }
    }

    /// Feed a sample at time `t` (seconds, monotonically increasing).
    pub fn push(&mut self, t: f64, l: f32) {
        if self.tracker.push(l).is_some() {
            self.transitions.push_back(t);
        }
        while let Some(&front) = self.transitions.front() {
            if t - front >= self.window_secs {
                self.transitions.pop_front();
            } else {
                break;
            }
        }
        self.max_transitions_in_window = self.max_transitions_in_window.max(self.transitions.len());
    }

    /// Highest number of flashes (transition pairs) seen in any window.
    pub fn max_flashes_in_window(&self) -> f32 {
        self.max_transitions_in_window as f32 / 2.0
    }

    /// True when no window exceeded the WCAG three-flash limit.
    pub fn passes(&self) -> bool {
        self.max_flashes_in_window() <= WCAG_MAX_FLASHES_PER_SECOND
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(hz: f64, fps: f64, secs: f64, lo: f32, hi: f32) -> Vec<(f64, f32)> {
        let n = (fps * secs) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / fps;
                let on = ((t * hz * 2.0).floor() as i64) % 2 == 0;
                (t, if on { hi } else { lo })
            })
            .collect()
    }

    fn judge(series: &[(f64, f32)]) -> FlashJudge {
        let mut j = FlashJudge::default();
        for &(t, l) in series {
            j.push(t, l);
        }
        j
    }

    #[test]
    fn fast_strobe_fails() {
        let j = judge(&square(10.0, 60.0, 2.0, 0.0, 1.0));
        assert!(!j.passes());
        assert!(j.max_flashes_in_window() >= 9.0);
    }

    #[test]
    fn slow_strobe_passes() {
        assert!(judge(&square(2.0, 60.0, 3.0, 0.0, 1.0)).passes());
    }

    #[test]
    fn small_amplitude_passes() {
        assert!(judge(&square(10.0, 60.0, 2.0, 0.40, 0.48)).passes());
    }

    #[test]
    fn bright_only_flicker_is_exempt() {
        // Darker state above 0.80 — not a WCAG flash.
        assert!(judge(&square(10.0, 60.0, 2.0, 0.85, 1.0)).passes());
    }

    #[test]
    fn gradual_strobe_at_high_fps_is_caught() {
        // 8 Hz sine at 240 fps: per-frame deltas < 0.1, but it is a strobe.
        let fps = 240.0;
        let series: Vec<(f64, f32)> = (0..480)
            .map(|i| {
                let t = i as f64 / fps;
                (t, 0.5 + 0.4 * (t * 8.0 * std::f64::consts::TAU).sin() as f32)
            })
            .collect();
        assert!(!judge(&series).passes());
    }

    #[test]
    fn slow_fade_is_one_transition() {
        let series: Vec<(f64, f32)> = (0..120).map(|i| (i as f64 / 60.0, i as f32 / 120.0)).collect();
        assert_eq!(judge(&series).max_flashes_in_window(), 0.5);
    }
}
