//! Entropy-aware capture scheduling (information-theoretically optimal).
//!
//! Current capture scheduling treats all panes equally or uses simple heuristics.
//! This is wasteful: a pane spewing repetitive log lines (low entropy) gets the
//! same capture frequency as one producing novel, information-rich output.
//!
//! This module uses Shannon entropy of each pane's output stream to determine
//! capture intervals. High-entropy (lots of new info) = capture more frequently.
//! Low-entropy (repetitive) = capture less frequently.
//!
//! # Entropy-to-interval mapping
//!
//! ```text
//! interval = base_interval / max(entropy_density, floor)
//! ```
//!
//! Where `entropy_density = H(pane) / H_max` is the normalized entropy (0..1).
//!
//! # VOI composition
//!
//! The [`EntropyScheduler::entropy_density`] method returns per-pane normalized
//! entropy suitable as the "information content" input to the VOI scheduler:
//!
//! ```text
//! VOI(pane) = value_of_capture(pane) × entropy_density(pane)
//! ```

use crate::entropy_accounting::EntropyEstimator;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Maximum Shannon entropy for a byte stream (log₂ 256 = 8 bits/byte).
const H_MAX: f64 = 8.0;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for entropy-aware capture scheduling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EntropySchedulerConfig {
    /// Base capture interval in milliseconds (used when entropy density = 1.0).
    pub base_interval_ms: u64,
    /// Minimum capture interval in milliseconds (clamp for high-entropy panes).
    pub min_interval_ms: u64,
    /// Maximum capture interval in milliseconds (clamp for low-entropy panes).
    pub max_interval_ms: u64,
    /// Entropy density floor — prevents division by near-zero.
    /// Panes with density below this are treated as `floor`.
    pub density_floor: f64,
    /// Sliding window size in bytes for per-pane entropy estimation.
    pub window_size: usize,
    /// Minimum bytes before entropy estimate is considered reliable.
    pub min_samples: u64,
    /// Interval to use before enough samples are collected.
    pub warmup_interval_ms: u64,
}

impl Default for EntropySchedulerConfig {
    fn default() -> Self {
        Self {
            base_interval_ms: 1000,
            min_interval_ms: 50,
            max_interval_ms: 30_000,
            density_floor: 0.05,
            window_size: 65_536, // 64 KB
            min_samples: 256,
            warmup_interval_ms: 500,
        }
    }
}

// =============================================================================
// Per-pane state
// =============================================================================

/// Per-pane entropy tracking state.
struct PaneEntropyState {
    /// Sliding-window entropy estimator.
    estimator: EntropyEstimator,
    /// Last computed capture interval (ms).
    last_interval_ms: u64,
    /// Last entropy density (0.0..=1.0).
    last_density: f64,
}

// =============================================================================
// Scheduling decision
// =============================================================================

/// A per-pane scheduling decision from the entropy scheduler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntropyDecision {
    /// Pane ID.
    pub pane_id: u64,
    /// Shannon entropy in bits/byte (0.0..=8.0).
    pub entropy: f64,
    /// Normalized entropy density (0.0..=1.0).
    pub density: f64,
    /// Recommended capture interval in milliseconds.
    pub interval_ms: u64,
    /// Total bytes fed to this pane's estimator.
    pub total_bytes: u64,
    /// Whether the estimate is still in warmup (below min_samples).
    pub in_warmup: bool,
}

/// Result of a full scheduling round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntropyScheduleResult {
    /// Per-pane decisions, sorted by interval ascending (most urgent first).
    pub decisions: Vec<EntropyDecision>,
    /// Mean entropy density across all panes.
    pub mean_density: f64,
    /// Number of panes still in warmup.
    pub warmup_count: usize,
}

// =============================================================================
// Serializable snapshot
// =============================================================================

/// Serializable snapshot of the entropy scheduler state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntropySchedulerSnapshot {
    pub pane_count: usize,
    pub config: EntropySchedulerConfig,
    pub pane_states: Vec<PaneSnapshotEntry>,
}

/// Per-pane snapshot entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneSnapshotEntry {
    pub pane_id: u64,
    pub entropy: f64,
    pub density: f64,
    pub interval_ms: u64,
    pub total_bytes: u64,
    pub in_warmup: bool,
}

// =============================================================================
// EntropyScheduler
// =============================================================================

/// Entropy-aware capture scheduler.
///
/// Maintains per-pane [`EntropyEstimator`]s and maps measured entropy to
/// capture intervals. High-entropy panes are polled more frequently.
pub struct EntropyScheduler {
    config: EntropySchedulerConfig,
    panes: HashMap<u64, PaneEntropyState>,
}

impl EntropyScheduler {
    /// Create a new scheduler with the given configuration.
    pub fn new(config: EntropySchedulerConfig) -> Self {
        Self {
            config,
            panes: HashMap::new(),
        }
    }

    /// Register a pane for entropy tracking.
    ///
    /// Idempotent — re-registering a pane that already exists is a no-op.
    pub fn register_pane(&mut self, pane_id: u64) {
        self.panes
            .entry(pane_id)
            .or_insert_with(|| PaneEntropyState {
                estimator: EntropyEstimator::new(self.config.window_size),
                last_interval_ms: self.config.warmup_interval_ms,
                last_density: 0.0,
            });
    }

    /// Remove a pane from tracking.
    pub fn unregister_pane(&mut self, pane_id: u64) {
        self.panes.remove(&pane_id);
    }

    /// Feed output bytes from a pane into its entropy estimator.
    pub fn feed_bytes(&mut self, pane_id: u64, data: &[u8]) {
        if let Some(state) = self.panes.get_mut(&pane_id) {
            state.estimator.update_block(data);
            self.recompute_pane(pane_id);
        }
    }

    /// Feed a single byte from a pane.
    pub fn feed_byte(&mut self, pane_id: u64, byte: u8) {
        if let Some(state) = self.panes.get_mut(&pane_id) {
            state.estimator.update(byte);
            self.recompute_pane(pane_id);
        }
    }

    /// Get the normalized entropy density for a pane (0.0..=1.0).
    ///
    /// This is the primary composition point with the VOI scheduler:
    /// ```text
    /// VOI(pane) = value_of_capture(pane) × entropy_density(pane)
    /// ```
    ///
    /// Returns `None` if the pane is not registered, or the density floor
    /// if the pane hasn't collected enough samples yet.
    pub fn entropy_density(&self, pane_id: u64) -> Option<f64> {
        self.panes.get(&pane_id).map(|s| s.last_density)
    }

    /// Get the raw Shannon entropy for a pane (bits/byte, 0.0..=8.0).
    pub fn entropy(&self, pane_id: u64) -> Option<f64> {
        self.panes.get(&pane_id).map(|s| {
            // Reconstruct from density; avoids needing &mut for cached entropy
            s.last_density * H_MAX
        })
    }

    /// Get the recommended capture interval for a pane (milliseconds).
    pub fn interval_ms(&self, pane_id: u64) -> Option<u64> {
        self.panes.get(&pane_id).map(|s| s.last_interval_ms)
    }

    /// Whether a pane's entropy estimate is still in warmup.
    pub fn in_warmup(&self, pane_id: u64) -> Option<bool> {
        self.panes
            .get(&pane_id)
            .map(|s| s.estimator.total_bytes() < self.config.min_samples)
    }

    /// Number of tracked panes.
    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    /// Produce a full scheduling round.
    ///
    /// Returns per-pane decisions sorted by interval ascending (most
    /// urgent = shortest interval first).
    pub fn schedule(&self) -> EntropyScheduleResult {
        let mut decisions: Vec<EntropyDecision> = self
            .panes
            .iter()
            .map(|(&pane_id, state)| {
                let in_warmup = state.estimator.total_bytes() < self.config.min_samples;
                EntropyDecision {
                    pane_id,
                    entropy: state.last_density * H_MAX,
                    density: state.last_density,
                    interval_ms: state.last_interval_ms,
                    total_bytes: state.estimator.total_bytes(),
                    in_warmup,
                }
            })
            .collect();

        decisions.sort_by_key(|d| d.interval_ms);

        let mean_density = if decisions.is_empty() {
            0.0
        } else {
            decisions.iter().map(|d| d.density).sum::<f64>() / decisions.len() as f64
        };

        let warmup_count = decisions.iter().filter(|d| d.in_warmup).count();

        EntropyScheduleResult {
            decisions,
            mean_density,
            warmup_count,
        }
    }

    /// Create a serializable snapshot.
    pub fn snapshot(&self) -> EntropySchedulerSnapshot {
        let pane_states: Vec<PaneSnapshotEntry> = self
            .panes
            .iter()
            .map(|(&pane_id, state)| PaneSnapshotEntry {
                pane_id,
                entropy: state.last_density * H_MAX,
                density: state.last_density,
                interval_ms: state.last_interval_ms,
                total_bytes: state.estimator.total_bytes(),
                in_warmup: state.estimator.total_bytes() < self.config.min_samples,
            })
            .collect();

        EntropySchedulerSnapshot {
            pane_count: self.panes.len(),
            config: self.config.clone(),
            pane_states,
        }
    }

    /// Recompute interval for a pane after new data.
    fn recompute_pane(&mut self, pane_id: u64) {
        let Some(state) = self.panes.get_mut(&pane_id) else {
            return;
        };

        if state.estimator.total_bytes() < self.config.min_samples {
            state.last_density = self.config.density_floor;
            state.last_interval_ms = self.config.warmup_interval_ms;
            return;
        }

        let h = state.estimator.entropy();
        let density = (h / H_MAX).clamp(0.0, 1.0);
        state.last_density = density;

        // Compute interval inline to avoid borrowing self while state is borrowed
        let effective_density = density.max(self.config.density_floor);
        let interval = self.config.base_interval_ms as f64 / effective_density;
        state.last_interval_ms =
            (interval as u64).clamp(self.config.min_interval_ms, self.config.max_interval_ms);
    }
}

impl std::fmt::Debug for EntropyScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntropyScheduler")
            .field("pane_count", &self.panes.len())
            .field("config", &self.config)
            .finish()
    }
}

// =============================================================================
// Free functions for standalone use
// =============================================================================

/// Compute the capture interval for a byte slice.
///
/// Convenience function for one-shot interval computation without
/// maintaining an `EntropyScheduler`.
pub fn schedule_interval(data: &[u8], config: &EntropySchedulerConfig) -> u64 {
    if data.is_empty() {
        return config.max_interval_ms;
    }
    let h = crate::entropy_accounting::compute_entropy(data);
    let density = (h / H_MAX).clamp(0.0, 1.0);
    let effective = density.max(config.density_floor);
    let interval = config.base_interval_ms as f64 / effective;
    (interval as u64).clamp(config.min_interval_ms, config.max_interval_ms)
}

/// Compute the capture interval for a byte slice with default config.
pub fn schedule_interval_default(data: &[u8]) -> u64 {
    schedule_interval(data, &EntropySchedulerConfig::default())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Config ───────────────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let cfg = EntropySchedulerConfig::default();
        assert_eq!(cfg.base_interval_ms, 1000);
        assert_eq!(cfg.min_interval_ms, 50);
        assert_eq!(cfg.max_interval_ms, 30_000);
        assert!((cfg.density_floor - 0.05).abs() < 1e-10);
        assert_eq!(cfg.window_size, 65_536);
        assert_eq!(cfg.min_samples, 256);
        assert_eq!(cfg.warmup_interval_ms, 500);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = EntropySchedulerConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let cfg2: EntropySchedulerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.base_interval_ms, cfg2.base_interval_ms);
        assert_eq!(cfg.window_size, cfg2.window_size);
    }

    // ── Scheduler basics ─────────────────────────────────────────────

    #[test]
    fn scheduler_creation() {
        let sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        assert_eq!(sched.pane_count(), 0);
    }

    #[test]
    fn register_and_unregister() {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        sched.register_pane(1);
        sched.register_pane(2);
        assert_eq!(sched.pane_count(), 2);

        sched.unregister_pane(1);
        assert_eq!(sched.pane_count(), 1);
        assert!(sched.entropy_density(1).is_none());
        assert!(sched.entropy_density(2).is_some());
    }

    #[test]
    fn register_idempotent() {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        sched.register_pane(1);
        // Feed some data
        sched.feed_bytes(1, &[42; 1000]);
        // Re-register should not reset
        sched.register_pane(1);
        assert_eq!(sched.pane_count(), 1);
        // Data should still be there
        assert!(sched.in_warmup(1) == Some(false)); // 1000 > 256
    }

    #[test]
    fn missing_pane_returns_none() {
        let sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        assert!(sched.entropy_density(999).is_none());
        assert!(sched.entropy(999).is_none());
        assert!(sched.interval_ms(999).is_none());
        assert!(sched.in_warmup(999).is_none());
    }

    // ── Warmup behavior ──────────────────────────────────────────────

    #[test]
    fn warmup_before_min_samples() {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig {
            min_samples: 500,
            warmup_interval_ms: 200,
            ..Default::default()
        });
        sched.register_pane(1);
        sched.feed_bytes(1, &[42; 100]); // Only 100 < 500

        assert_eq!(sched.in_warmup(1), Some(true));
        assert_eq!(sched.interval_ms(1), Some(200));
    }

    #[test]
    fn warmup_exits_after_min_samples() {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig {
            min_samples: 100,
            ..Default::default()
        });
        sched.register_pane(1);

        // Feed uniform data (high entropy) past warmup
        let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
        sched.feed_bytes(1, &data);

        assert_eq!(sched.in_warmup(1), Some(false));
        // High-entropy data should get a short interval
        let interval = sched.interval_ms(1).unwrap();
        assert!(
            interval < sched.config.max_interval_ms,
            "high-entropy pane should have short interval, got {interval}"
        );
    }

    // ── Entropy mapping ──────────────────────────────────────────────

    #[test]
    fn constant_stream_gets_long_interval() {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        });
        sched.register_pane(1);
        sched.feed_bytes(1, &[0u8; 1000]);

        let interval = sched.interval_ms(1).unwrap();
        // Constant data has near-zero entropy → density floored → long interval
        // interval = base_interval / density_floor = 1000 / 0.05 = 20000
        assert_eq!(interval, 20_000);
    }

    #[test]
    fn uniform_random_gets_short_interval() {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        });
        sched.register_pane(1);

        // Feed all 256 byte values equally
        let mut data = Vec::with_capacity(256 * 40);
        for _ in 0..40 {
            for b in 0..=255u8 {
                data.push(b);
            }
        }
        sched.feed_bytes(1, &data);

        let density = sched.entropy_density(1).unwrap();
        assert!(
            density > 0.95,
            "uniform data should have density near 1.0, got {density}"
        );

        let interval = sched.interval_ms(1).unwrap();
        assert!(
            interval <= sched.config.base_interval_ms,
            "uniform data should have interval <= base, got {interval}"
        );
    }

    #[test]
    fn higher_entropy_shorter_interval() {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1); // low-entropy
        sched.register_pane(2); // high-entropy

        // Low entropy: constant bytes
        sched.feed_bytes(1, &[0u8; 1000]);

        // High entropy: all byte values
        let mut data = Vec::with_capacity(256 * 10);
        for _ in 0..10 {
            for b in 0..=255u8 {
                data.push(b);
            }
        }
        sched.feed_bytes(2, &data);

        let interval_low = sched.interval_ms(1).unwrap();
        let interval_high = sched.interval_ms(2).unwrap();

        assert!(
            interval_high < interval_low,
            "high-entropy should have shorter interval: high={interval_high}, low={interval_low}"
        );
    }

    #[test]
    fn english_text_midrange_interval() {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        });
        sched.register_pane(1);

        let text = b"The quick brown fox jumps over the lazy dog. \
            Shannon entropy measures the average information content per symbol. \
            English text typically has about 4 to 5 bits of entropy per byte.";
        // Feed enough for statistical significance
        for _ in 0..20 {
            sched.feed_bytes(1, text);
        }

        let density = sched.entropy_density(1).unwrap();
        assert!(
            density > 0.3 && density < 0.8,
            "English text density should be midrange, got {density}"
        );
    }

    // ── Density floor ────────────────────────────────────────────────

    #[test]
    fn density_floor_prevents_extreme_intervals() {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            density_floor: 0.1,
            base_interval_ms: 1000,
            max_interval_ms: 50_000,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);
        sched.feed_bytes(1, &[0u8; 1000]);

        // interval = 1000 / 0.1 = 10_000 (not infinity)
        let interval = sched.interval_ms(1).unwrap();
        assert_eq!(interval, 10_000);
    }

    // ── Schedule round ───────────────────────────────────────────────

    #[test]
    fn schedule_orders_by_interval_ascending() {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1); // low entropy
        sched.register_pane(2); // high entropy

        sched.feed_bytes(1, &[0u8; 1000]);
        let mut data = Vec::with_capacity(256 * 10);
        for _ in 0..10 {
            for b in 0..=255u8 {
                data.push(b);
            }
        }
        sched.feed_bytes(2, &data);

        let result = sched.schedule();
        assert_eq!(result.decisions.len(), 2);
        // First decision should have shorter interval (high entropy)
        assert!(result.decisions[0].interval_ms <= result.decisions[1].interval_ms);
        assert_eq!(result.decisions[0].pane_id, 2); // high-entropy pane
    }

    #[test]
    fn schedule_mean_density() {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);
        sched.register_pane(2);

        sched.feed_bytes(1, &[0u8; 1000]); // density ≈ floor
        let mut data = Vec::with_capacity(256 * 10);
        for _ in 0..10 {
            for b in 0..=255u8 {
                data.push(b);
            }
        }
        sched.feed_bytes(2, &data); // density ≈ 1.0

        let result = sched.schedule();
        // Mean of ~0.05 and ~1.0 ≈ 0.5-ish
        assert!(
            result.mean_density > 0.3 && result.mean_density < 0.7,
            "mean density should be midrange, got {}",
            result.mean_density
        );
    }

    #[test]
    fn schedule_warmup_count() {
        let cfg = EntropySchedulerConfig {
            min_samples: 500,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);
        sched.register_pane(2);

        sched.feed_bytes(1, &[0u8; 100]); // still in warmup (100 < 500)
        sched.feed_bytes(2, &[0u8; 1000]); // past warmup (1000 > 500)

        let result = sched.schedule();
        assert_eq!(result.warmup_count, 1);
    }

    #[test]
    fn schedule_empty_scheduler() {
        let sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        let result = sched.schedule();
        assert!(result.decisions.is_empty());
        assert_eq!(result.mean_density, 0.0);
        assert_eq!(result.warmup_count, 0);
    }

    // ── Free functions ───────────────────────────────────────────────

    #[test]
    fn schedule_interval_empty_data_max() {
        let cfg = EntropySchedulerConfig::default();
        assert_eq!(schedule_interval(&[], &cfg), cfg.max_interval_ms);
    }

    #[test]
    fn schedule_interval_constant_data() {
        let cfg = EntropySchedulerConfig::default();
        let interval = schedule_interval(&[0u8; 10_000], &cfg);
        // density floored at 0.05 → interval = 1000/0.05 = 20_000
        assert_eq!(interval, 20_000);
    }

    #[test]
    fn schedule_interval_high_entropy_data() {
        let cfg = EntropySchedulerConfig::default();
        let mut data = Vec::with_capacity(256 * 40);
        for _ in 0..40 {
            for b in 0..=255u8 {
                data.push(b);
            }
        }
        let interval = schedule_interval(&data, &cfg);
        // density ≈ 1.0 → interval ≈ 1000
        assert!(
            interval <= cfg.base_interval_ms,
            "high entropy interval should be <= base, got {interval}"
        );
    }

    #[test]
    fn schedule_interval_default_works() {
        let interval = schedule_interval_default(&[42; 10_000]);
        assert!(interval > 0);
    }

    // ── Snapshot ─────────────────────────────────────────────────────

    #[test]
    fn snapshot_captures_state() {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);
        sched.feed_bytes(1, &[42; 1000]);

        let snap = sched.snapshot();
        assert_eq!(snap.pane_count, 1);
        assert_eq!(snap.pane_states.len(), 1);
        assert_eq!(snap.pane_states[0].pane_id, 1);
        assert_eq!(snap.pane_states[0].total_bytes, 1000);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        sched.register_pane(1);
        let snap = sched.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let snap2: EntropySchedulerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap2.pane_count, snap.pane_count);
    }

    // ── VOI composition ──────────────────────────────────────────────

    #[test]
    fn entropy_density_for_voi_composition() {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);

        // Feed uniform data
        let mut data = Vec::with_capacity(256 * 10);
        for _ in 0..10 {
            for b in 0..=255u8 {
                data.push(b);
            }
        }
        sched.feed_bytes(1, &data);

        let density = sched.entropy_density(1).unwrap();
        // This is the value that would be multiplied by VOI's value_of_capture
        assert!(density > 0.9, "uniform data density should be near 1.0");

        // Simulated VOI composition:
        let value_of_capture = 0.5; // from VOI scheduler
        let composed_voi = value_of_capture * density;
        assert!(composed_voi > 0.45);
    }

    // ── Debug impl ───────────────────────────────────────────────────

    #[test]
    fn debug_impl() {
        let sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        let s = format!("{sched:?}");
        assert!(s.contains("EntropyScheduler"));
        assert!(s.contains("pane_count"));
    }

    // ── Entropy bounds ───────────────────────────────────────────────

    #[test]
    fn entropy_always_in_valid_range() {
        let cfg = EntropySchedulerConfig {
            min_samples: 1,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);

        // Various data patterns
        for pattern in [
            vec![0u8; 100],
            vec![255u8; 100],
            (0..100).map(|i| (i % 2) as u8).collect::<Vec<_>>(),
            (0..100).map(|i| (i % 256) as u8).collect::<Vec<_>>(),
        ] {
            sched.feed_bytes(1, &pattern);
            let h = sched.entropy(1).unwrap();
            assert!(h >= 0.0 && h <= 8.0, "entropy {h} out of range [0, 8]");
            let d = sched.entropy_density(1).unwrap();
            assert!(d >= 0.0 && d <= 1.0, "density {d} out of range [0, 1]");
        }
    }

    // ── Interval clamping ────────────────────────────────────────────

    #[test]
    fn interval_clamped_to_min() {
        let cfg = EntropySchedulerConfig {
            min_interval_ms: 100,
            base_interval_ms: 50, // base < min means everything clamps to min
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg.clone());
        sched.register_pane(1);

        let mut data = Vec::with_capacity(256 * 10);
        for _ in 0..10 {
            for b in 0..=255u8 {
                data.push(b);
            }
        }
        sched.feed_bytes(1, &data);

        let interval = sched.interval_ms(1).unwrap();
        assert!(
            interval >= cfg.min_interval_ms,
            "interval {interval} should be >= min {}",
            cfg.min_interval_ms
        );
    }

    #[test]
    fn interval_clamped_to_max() {
        let cfg = EntropySchedulerConfig {
            max_interval_ms: 5_000,
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg.clone());
        sched.register_pane(1);
        sched.feed_bytes(1, &[0u8; 1000]);

        let interval = sched.interval_ms(1).unwrap();
        assert!(
            interval <= cfg.max_interval_ms,
            "interval {interval} should be <= max {}",
            cfg.max_interval_ms
        );
    }

    // ── Feed to unregistered pane ────────────────────────────────────

    #[test]
    fn feed_to_unregistered_pane_is_noop() {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        // Should not panic
        sched.feed_bytes(999, &[0u8; 100]);
        sched.feed_byte(999, 42);
        assert_eq!(sched.pane_count(), 0);
    }

    // ── Decision serde ───────────────────────────────────────────────

    #[test]
    fn decision_serde_roundtrip() {
        let d = EntropyDecision {
            pane_id: 42,
            entropy: 4.5,
            density: 0.5625,
            interval_ms: 1778,
            total_bytes: 10_000,
            in_warmup: false,
        };
        let json = serde_json::to_string(&d).unwrap();
        let d2: EntropyDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(d2.pane_id, 42);
        assert!((d2.density - 0.5625).abs() < 1e-10);
    }

    #[test]
    fn schedule_result_serde_roundtrip() {
        let result = EntropyScheduleResult {
            decisions: vec![EntropyDecision {
                pane_id: 1,
                entropy: 6.0,
                density: 0.75,
                interval_ms: 1333,
                total_bytes: 5000,
                in_warmup: false,
            }],
            mean_density: 0.75,
            warmup_count: 0,
        };
        let json = serde_json::to_string(&result).unwrap();
        let r2: EntropyScheduleResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r2.decisions.len(), 1);
        assert!((r2.mean_density - 0.75).abs() < 1e-10);
    }
}

// =============================================================================
// Proptest
// =============================================================================

#[cfg(test)]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn entropy_bounds_valid(
            data in prop::collection::vec(any::<u8>(), 1..10_000)
        ) {
            let h = crate::entropy_accounting::compute_entropy(&data);
            prop_assert!(h >= 0.0, "entropy {h} < 0");
            prop_assert!(h <= 8.0, "entropy {h} > 8");
        }

        #[test]
        fn constant_stream_minimal_entropy(
            byte_val in any::<u8>(),
            len in 100..10_000usize
        ) {
            let data = vec![byte_val; len];
            let h = crate::entropy_accounting::compute_entropy(&data);
            prop_assert!(h < 0.01, "constant stream entropy {h} should be ~0");
        }

        #[test]
        fn high_entropy_gets_shorter_interval(
            low_data in prop::collection::vec(0u8..2, 1000..5000),
            high_data in prop::collection::vec(any::<u8>(), 1000..5000)
        ) {
            let cfg = EntropySchedulerConfig {
                min_samples: 10,
                ..Default::default()
            };
            let low_interval = schedule_interval(&low_data, &cfg);
            let high_interval = schedule_interval(&high_data, &cfg);
            prop_assert!(
                high_interval <= low_interval,
                "high-entropy interval ({high_interval}) should be <= low-entropy ({low_interval})"
            );
        }

        #[test]
        fn sliding_window_converges(
            initial in prop::collection::vec(any::<u8>(), 1000..5000),
            replacement in prop::collection::vec(0u8..1, 1000..5000)
        ) {
            let mut estimator = EntropyEstimator::new(1024);
            for &b in &initial {
                estimator.update(b);
            }
            let h_before = estimator.entropy();

            for &b in &replacement {
                estimator.update(b);
            }
            let h_after = estimator.entropy();

            prop_assert!(
                h_after <= h_before + 0.01,
                "after switching to low-entropy data, entropy should decrease: before={h_before}, after={h_after}"
            );
        }

        #[test]
        fn density_always_normalized(
            data in prop::collection::vec(any::<u8>(), 100..10_000)
        ) {
            let cfg = EntropySchedulerConfig {
                min_samples: 10,
                ..Default::default()
            };
            let mut sched = EntropyScheduler::new(cfg);
            sched.register_pane(1);
            sched.feed_bytes(1, &data);

            let density = sched.entropy_density(1).unwrap();
            prop_assert!(density >= 0.0, "density {density} < 0");
            prop_assert!(density <= 1.0, "density {density} > 1");
        }

        #[test]
        fn interval_always_in_bounds(
            data in prop::collection::vec(any::<u8>(), 100..10_000),
            min_ms in 10u64..100,
            max_ms in 5_000u64..60_000,
            base_ms in 100u64..5_000,
        ) {
            let cfg = EntropySchedulerConfig {
                min_interval_ms: min_ms,
                max_interval_ms: max_ms,
                base_interval_ms: base_ms,
                min_samples: 10,
                ..Default::default()
            };
            let interval = schedule_interval(&data, &cfg);
            prop_assert!(
                interval >= min_ms,
                "interval {interval} < min {min_ms}"
            );
            prop_assert!(
                interval <= max_ms,
                "interval {interval} > max {max_ms}"
            );
        }
    }
}
