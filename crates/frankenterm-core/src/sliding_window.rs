//! Time-bucketed sliding window counter for rate monitoring.
//!
//! Divides a time window into fixed sub-buckets and records event counts per
//! bucket.  Old buckets are automatically expired when the window advances.
//! Callers pass explicit timestamps (milliseconds) to avoid `Instant` serde
//! issues and to enable deterministic testing.
//!
//! Useful for throughput monitoring, rate limiting, error rate tracking,
//! and burst detection.

use serde::{Deserialize, Serialize};

/// A fixed-size time-bucketed sliding window counter.
///
/// The window is divided into `n_buckets` sub-intervals.  As time advances,
/// old buckets are zeroed out.  All operations take O(n_buckets) worst case
/// (when many buckets need expiring) and O(1) amortized.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlidingWindow {
    buckets: Vec<u64>,
    n_buckets: usize,
    bucket_duration_ms: u64,
    window_duration_ms: u64,
    /// Index of the current (newest) bucket.
    head: usize,
    /// Timestamp (ms) at which the head bucket started.
    head_start_ms: u64,
    /// Whether any event has been recorded (to handle initial state).
    initialized: bool,
}

/// Configuration for creating a SlidingWindow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlidingWindowConfig {
    /// Total window duration in milliseconds.
    pub window_duration_ms: u64,
    /// Number of sub-buckets.
    pub n_buckets: usize,
}

impl Default for SlidingWindowConfig {
    fn default() -> Self {
        Self {
            window_duration_ms: 60_000, // 1 minute
            n_buckets: 60,              // 1-second buckets
        }
    }
}

/// Snapshot of window state for reporting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSnapshot {
    pub total_count: u64,
    pub rate_per_second: f64,
    pub window_duration_ms: u64,
    pub n_buckets: usize,
    pub bucket_counts: Vec<u64>,
}

impl SlidingWindow {
    /// Create a new sliding window.
    ///
    /// # Panics
    /// Panics if `window_duration_ms == 0` or `n_buckets == 0`.
    #[must_use]
    pub fn new(window_duration_ms: u64, n_buckets: usize) -> Self {
        assert!(window_duration_ms > 0, "window duration must be > 0");
        assert!(n_buckets > 0, "n_buckets must be > 0");
        let bucket_duration_ms = window_duration_ms / n_buckets as u64;
        let bucket_duration_ms = bucket_duration_ms.max(1);
        Self {
            buckets: vec![0u64; n_buckets],
            n_buckets,
            bucket_duration_ms,
            window_duration_ms,
            head: 0,
            head_start_ms: 0,
            initialized: false,
        }
    }

    /// Create from a config.
    #[must_use]
    pub fn from_config(config: SlidingWindowConfig) -> Self {
        Self::new(config.window_duration_ms, config.n_buckets)
    }

    /// Record a single event at the given timestamp.
    pub fn record(&mut self, timestamp_ms: u64) {
        self.record_n(timestamp_ms, 1);
    }

    /// Record `count` events at the given timestamp.
    pub fn record_n(&mut self, timestamp_ms: u64, count: u64) {
        self.advance_to(timestamp_ms);
        self.buckets[self.head] += count;
    }

    /// Total event count within the window ending at `now_ms`.
    #[must_use]
    pub fn count(&self, now_ms: u64) -> u64 {
        if !self.initialized {
            return 0;
        }
        let expired = self.expired_bucket_count(now_ms);
        if expired >= self.n_buckets {
            return 0;
        }
        self.buckets
            .iter()
            .enumerate()
            .filter(|&(i, _)| !self.is_bucket_expired(i, expired))
            .map(|(_, &c)| c)
            .sum()
    }

    /// Events per second within the window ending at `now_ms`.
    #[must_use]
    pub fn rate_per_second(&self, now_ms: u64) -> f64 {
        let total = self.count(now_ms) as f64;
        let window_secs = self.window_duration_ms as f64 / 1000.0;
        if window_secs > 0.0 {
            total / window_secs
        } else {
            0.0
        }
    }

    /// Events per second using actual elapsed time since first event.
    #[must_use]
    pub fn effective_rate(&self, now_ms: u64) -> f64 {
        if !self.initialized {
            return 0.0;
        }
        let total = self.count(now_ms) as f64;
        let elapsed_ms = now_ms.saturating_sub(self.oldest_timestamp(now_ms));
        let elapsed_secs = elapsed_ms as f64 / 1000.0;
        if elapsed_secs > 0.0 {
            total / elapsed_secs
        } else {
            total // all events at same instant
        }
    }

    /// Whether no events have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.initialized || self.buckets.iter().all(|&c| c == 0)
    }

    /// Reset all buckets to zero.
    pub fn clear(&mut self) {
        for b in &mut self.buckets {
            *b = 0;
        }
        self.head = 0;
        self.head_start_ms = 0;
        self.initialized = false;
    }

    /// Oldest timestamp still within the window at `now_ms`.
    #[must_use]
    pub fn oldest_timestamp(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.window_duration_ms)
    }

    /// Number of sub-buckets.
    #[must_use]
    pub fn bucket_count(&self) -> usize {
        self.n_buckets
    }

    /// Total window duration in ms.
    #[must_use]
    pub fn window_duration_ms(&self) -> u64 {
        self.window_duration_ms
    }

    /// Duration of each sub-bucket in ms.
    #[must_use]
    pub fn bucket_duration_ms(&self) -> u64 {
        self.bucket_duration_ms
    }

    /// Take a snapshot of the current window state.
    #[must_use]
    pub fn snapshot(&self, now_ms: u64) -> WindowSnapshot {
        let expired = if self.initialized {
            self.expired_bucket_count(now_ms)
        } else {
            self.n_buckets
        };
        let bucket_counts: Vec<u64> = self
            .buckets
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                if self.is_bucket_expired(i, expired) {
                    0
                } else {
                    c
                }
            })
            .collect();
        WindowSnapshot {
            total_count: self.count(now_ms),
            rate_per_second: self.rate_per_second(now_ms),
            window_duration_ms: self.window_duration_ms,
            n_buckets: self.n_buckets,
            bucket_counts,
        }
    }

    /// Check if the event rate exceeds a threshold (events per second).
    #[must_use]
    pub fn exceeds_rate(&self, now_ms: u64, max_rate: f64) -> bool {
        self.rate_per_second(now_ms) > max_rate
    }

    /// Count in the most recent `last_n_buckets` buckets.
    #[must_use]
    pub fn recent_count(&self, now_ms: u64, last_n_buckets: usize) -> u64 {
        if !self.initialized || last_n_buckets == 0 {
            return 0;
        }
        let expired = self.expired_bucket_count(now_ms);
        let n = last_n_buckets.min(self.n_buckets);
        let mut total = 0u64;
        for offset in 0..n {
            let idx = (self.head + self.n_buckets - offset) % self.n_buckets;
            if !self.is_bucket_expired(idx, expired) {
                total += self.buckets[idx];
            }
        }
        total
    }

    // ---- internal helpers ----

    fn advance_to(&mut self, timestamp_ms: u64) {
        if !self.initialized {
            self.head_start_ms = timestamp_ms;
            self.initialized = true;
            return;
        }

        if timestamp_ms <= self.head_start_ms {
            // Event in the past — record in head bucket (best effort)
            return;
        }

        let elapsed = timestamp_ms - self.head_start_ms;
        let buckets_to_advance = (elapsed / self.bucket_duration_ms) as usize;

        if buckets_to_advance == 0 {
            return;
        }

        let clear_count = buckets_to_advance.min(self.n_buckets);
        for i in 1..=clear_count {
            let idx = (self.head + i) % self.n_buckets;
            self.buckets[idx] = 0;
        }

        self.head = (self.head + buckets_to_advance) % self.n_buckets;
        self.head_start_ms += buckets_to_advance as u64 * self.bucket_duration_ms;
    }

    fn expired_bucket_count(&self, now_ms: u64) -> usize {
        if now_ms <= self.head_start_ms {
            return 0;
        }
        let elapsed = now_ms - self.head_start_ms;
        (elapsed / self.bucket_duration_ms) as usize // how many buckets have passed since head was updated
    }

    fn is_bucket_expired(&self, bucket_idx: usize, expired_count: usize) -> bool {
        if expired_count >= self.n_buckets {
            return true;
        }
        // Buckets older than (head - n_buckets + expired_count) are expired
        let age = (self.head + self.n_buckets - bucket_idx) % self.n_buckets;
        age >= self.n_buckets.saturating_sub(expired_count)
    }
}

impl PartialEq for SlidingWindow {
    fn eq(&self, other: &Self) -> bool {
        self.n_buckets == other.n_buckets
            && self.bucket_duration_ms == other.bucket_duration_ms
            && self.window_duration_ms == other.window_duration_ms
            && self.buckets == other.buckets
            && self.head == other.head
            && self.head_start_ms == other.head_start_ms
            && self.initialized == other.initialized
    }
}

impl Eq for SlidingWindow {}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn new_is_empty() {
        let w = SlidingWindow::new(1000, 10);
        assert!(w.is_empty());
        assert_eq!(w.count(0), 0);
    }

    #[test]
    fn record_single_event() {
        let mut w = SlidingWindow::new(1000, 10);
        w.record(100);
        assert_eq!(w.count(100), 1);
        assert!(!w.is_empty());
    }

    #[test]
    fn record_multiple_events() {
        let mut w = SlidingWindow::new(1000, 10);
        w.record(100);
        w.record(200);
        w.record(300);
        assert_eq!(w.count(300), 3);
    }

    #[test]
    fn record_n() {
        let mut w = SlidingWindow::new(1000, 10);
        w.record_n(100, 5);
        assert_eq!(w.count(100), 5);
    }

    #[test]
    fn events_expire_after_window() {
        let mut w = SlidingWindow::new(1000, 10); // 1s window, 100ms buckets
        w.record(0);
        assert_eq!(w.count(0), 1);
        // After the full window, the event should be expired
        assert_eq!(w.count(1100), 0);
    }

    #[test]
    fn events_in_window_counted() {
        let mut w = SlidingWindow::new(1000, 10);
        w.record(100);
        w.record(500);
        w.record(900);
        assert_eq!(w.count(950), 3);
    }

    #[test]
    fn rate_per_second() {
        let mut w = SlidingWindow::new(10_000, 10); // 10s window
        for i in 0..100 {
            w.record(i * 100); // 100 events over 10s
        }
        let rate = w.rate_per_second(9900);
        assert!((rate - 10.0).abs() < 1.0); // ~10 events/sec
    }

    #[test]
    fn clear_resets() {
        let mut w = SlidingWindow::new(1000, 10);
        w.record(100);
        w.record(200);
        w.clear();
        assert!(w.is_empty());
        assert_eq!(w.count(200), 0);
    }

    #[test]
    fn bucket_count() {
        let w = SlidingWindow::new(1000, 10);
        assert_eq!(w.bucket_count(), 10);
    }

    #[test]
    fn window_duration() {
        let w = SlidingWindow::new(5000, 10);
        assert_eq!(w.window_duration_ms(), 5000);
    }

    #[test]
    fn bucket_duration() {
        let w = SlidingWindow::new(1000, 10);
        assert_eq!(w.bucket_duration_ms(), 100);
    }

    #[test]
    fn oldest_timestamp() {
        let w = SlidingWindow::new(1000, 10);
        assert_eq!(w.oldest_timestamp(2000), 1000);
        assert_eq!(w.oldest_timestamp(500), 0); // saturates to 0
    }

    #[test]
    fn from_config() {
        let config = SlidingWindowConfig {
            window_duration_ms: 5000,
            n_buckets: 50,
        };
        let w = SlidingWindow::from_config(config);
        assert_eq!(w.window_duration_ms(), 5000);
        assert_eq!(w.bucket_count(), 50);
    }

    #[test]
    fn default_config() {
        let config = SlidingWindowConfig::default();
        assert_eq!(config.window_duration_ms, 60_000);
        assert_eq!(config.n_buckets, 60);
    }

    #[test]
    fn snapshot() {
        let mut w = SlidingWindow::new(1000, 10);
        w.record(100);
        w.record(200);
        let snap = w.snapshot(200);
        assert_eq!(snap.total_count, 2);
        assert_eq!(snap.n_buckets, 10);
        assert_eq!(snap.window_duration_ms, 1000);
    }

    #[test]
    fn exceeds_rate() {
        let mut w = SlidingWindow::new(1000, 10); // 1s window
        for i in 0..100 {
            w.record(i * 10);
        }
        assert!(w.exceeds_rate(990, 50.0)); // 100/s > 50/s
        assert!(!w.exceeds_rate(990, 200.0)); // 100/s < 200/s
    }

    #[test]
    fn recent_count() {
        let mut w = SlidingWindow::new(1000, 10);
        // Record in different buckets (100ms each)
        w.record(50); // bucket 0
        w.record(150); // bucket 1
        w.record(250); // bucket 2
        // Recent 1 bucket should have 1 event
        let recent = w.recent_count(250, 1);
        assert!(recent >= 1);
    }

    #[test]
    fn serde_roundtrip() {
        let mut w = SlidingWindow::new(1000, 10);
        w.record(100);
        w.record(200);
        let json = serde_json::to_string(&w).unwrap();
        let back: SlidingWindow = serde_json::from_str(&json).unwrap();
        assert_eq!(w, back);
    }

    #[test]
    fn serde_config_roundtrip() {
        let config = SlidingWindowConfig {
            window_duration_ms: 5000,
            n_buckets: 50,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SlidingWindowConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn serde_snapshot_roundtrip() {
        let snap = WindowSnapshot {
            total_count: 42,
            rate_per_second: 4.2,
            window_duration_ms: 10_000,
            n_buckets: 10,
            bucket_counts: vec![1, 2, 3, 4, 5, 6, 7, 8, 3, 3],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: WindowSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn single_bucket() {
        let mut w = SlidingWindow::new(1000, 1);
        w.record(100);
        w.record(200);
        assert_eq!(w.count(200), 2);
        assert_eq!(w.count(1200), 0); // expired
    }

    #[test]
    fn many_buckets() {
        let mut w = SlidingWindow::new(1000, 100);
        for i in 0..100 {
            w.record(i * 10);
        }
        assert_eq!(w.count(990), 100);
    }

    #[test]
    fn advancing_clears_old_buckets() {
        let mut w = SlidingWindow::new(100, 10); // 10ms buckets, 100ms window
        w.record(0);
        w.record(50);
        // Advance past the window
        w.record(200);
        // Old events should be gone, only new one remains
        assert_eq!(w.count(200), 1);
    }

    #[test]
    fn past_events_go_to_head() {
        let mut w = SlidingWindow::new(1000, 10);
        w.record(500);
        w.record(100); // past event — recorded in current head
        assert_eq!(w.count(500), 2);
    }

    #[test]
    fn effective_rate() {
        let mut w = SlidingWindow::new(10_000, 10);
        w.record(0);
        w.record(1000);
        w.record(2000);
        let rate = w.effective_rate(2000);
        assert!(rate > 0.0);
    }

    #[test]
    fn effective_rate_empty() {
        let w = SlidingWindow::new(1000, 10);
        assert_eq!(w.effective_rate(100), 0.0);
    }

    #[test]
    #[should_panic(expected = "window duration must be > 0")]
    fn zero_duration_panics() {
        let _ = SlidingWindow::new(0, 10);
    }

    #[test]
    #[should_panic(expected = "n_buckets must be > 0")]
    fn zero_buckets_panics() {
        let _ = SlidingWindow::new(1000, 0);
    }

    #[test]
    fn debug_format() {
        let w = SlidingWindow::new(1000, 10);
        let dbg = format!("{w:?}");
        assert!(dbg.contains("SlidingWindow"));
    }

    #[test]
    fn clone() {
        let mut w = SlidingWindow::new(1000, 10);
        w.record(100);
        let cloned = w.clone();
        assert_eq!(w, cloned);
    }

    #[test]
    fn equality() {
        let mut a = SlidingWindow::new(1000, 10);
        let mut b = SlidingWindow::new(1000, 10);
        a.record(100);
        b.record(100);
        assert_eq!(a, b);
    }

    #[test]
    fn inequality() {
        let mut a = SlidingWindow::new(1000, 10);
        let b = SlidingWindow::new(1000, 10);
        a.record(100);
        assert_ne!(a, b);
    }
}
