//! Keystroke-to-present timing.
//!
//! Milestone 0 lives or dies on this number, so it is measured rather than
//! assumed, and it is measured on every keystroke in the real app rather than
//! in a synthetic loop.
//!
//! What is timed: the instant the `NSEvent` reaches our handler, through
//! layout and encoding, to the moment the drawable has actually been
//! presented on screen. The present time comes from Metal's own
//! `presentedTime` on the drawable, so the GPU's work and the compositor's
//! handoff are inside the number, not outside it.
//!
//! What is *not* inside it: the time before AppKit handed us the event (USB
//! polling, the window server's own hop) and the display's panel latency
//! after scanout. Those are real but not ours to control, and no software
//! timer on this machine can see them. So this is a floor, not the number a
//! high-speed camera would give.

use std::time::Duration;

/// Rolling window of recent samples.
pub struct Latency {
    samples: Vec<Duration>,
    capacity: usize,
    next: usize,
    /// Samples seen since the last reset, which can exceed `capacity`.
    total: u64,
}

impl Latency {
    pub fn new(capacity: usize) -> Self {
        Latency {
            samples: Vec::with_capacity(capacity),
            capacity,
            next: 0,
            total: 0,
        }
    }

    pub fn record(&mut self, d: Duration) {
        self.total += 1;
        if self.samples.len() < self.capacity {
            self.samples.push(d);
        } else {
            self.samples[self.next] = d;
            self.next = (self.next + 1) % self.capacity;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn count(&self) -> u64 {
        self.total
    }

    /// Percentile in milliseconds. `p` is 0.0..=1.0.
    pub fn percentile_ms(&self, p: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f64> = self.samples.iter().map(|d| d.as_secs_f64() * 1e3).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in durations"));
        // Nearest-rank, which for a window this small is more honest than
        // interpolating between two samples.
        let rank = (p * sorted.len() as f64).ceil().max(1.0) as usize;
        sorted[(rank - 1).min(sorted.len() - 1)]
    }

    pub fn max_ms(&self) -> f64 {
        self.samples
            .iter()
            .map(|d| d.as_secs_f64() * 1e3)
            .fold(0.0, f64::max)
    }

    /// One-line summary for the status bar.
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return "latency  --".to_string();
        }
        format!(
            "latency p50 {:.2}ms  p99 {:.2}ms  max {:.2}ms  n={}",
            self.percentile_ms(0.50),
            self.percentile_ms(0.99),
            self.max_ms(),
            self.count()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_track_the_window() {
        let mut l = Latency::new(100);
        for ms in 1..=100 {
            l.record(Duration::from_micros(ms * 1000));
        }
        assert!((l.percentile_ms(0.50) - 50.0).abs() < 1.0);
        assert!((l.percentile_ms(0.99) - 99.0).abs() < 1.0);
        assert!((l.max_ms() - 100.0).abs() < 0.001);
    }

    #[test]
    fn old_samples_fall_out_of_the_window() {
        let mut l = Latency::new(8);
        for _ in 0..8 {
            l.record(Duration::from_millis(100));
        }
        assert!((l.max_ms() - 100.0).abs() < 0.001);
        for _ in 0..8 {
            l.record(Duration::from_millis(1));
        }
        assert!(
            (l.max_ms() - 1.0).abs() < 0.001,
            "the slow samples should have aged out"
        );
        assert_eq!(l.count(), 16, "total count should survive the rotation");
    }

    #[test]
    fn empty_is_not_a_panic() {
        let l = Latency::new(4);
        assert_eq!(l.percentile_ms(0.5), 0.0);
        assert_eq!(l.max_ms(), 0.0);
        assert!(l.summary().contains("--"));
    }
}
