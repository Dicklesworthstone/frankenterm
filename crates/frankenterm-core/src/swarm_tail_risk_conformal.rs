//! br-ft-qhav9: Composition B §12.1 conformal-promotion layer.
//!
//! Wraps `crate::runtime_telemetry::SwarmTailRiskMonitor`'s nonconformity
//! scores with a sliding-window online conformal layer. Provides a
//! distribution-free coverage guarantee:
//! `P(score_{t+1} ≤ predicted_bound) ≥ 1 - α` finite-sample, regardless
//! of the underlying distribution (Vovk, Gammerman, Shafer 2005). The
//! (1-α) quantile is the `⌈(1-α)(N+1)⌉`-th order statistic of the
//! calibration window.
//!
//! # Why a sibling module
//!
//! The natural home for this layer is inside `runtime_telemetry`, but
//! that file is 12 974 LOC (br-ft-l869f tracks the eventual extraction)
//! and was under heavy concurrent edit pressure when this slice landed.
//! Living here as a sibling module avoids the contention while the
//! roadmap-only ft-l869f extraction lands. Once the SwarmTailRisk*
//! subsystem moves to `runtime_telemetry/swarm_tail_risk.rs`, this
//! file should fold into the same module.
//!
//! # Coverage degradation under distribution shift
//!
//! FIFO eviction means stale samples drop out within `window_size`
//! observations, so the bound tracks the new distribution within a
//! bounded window. For stronger guarantees under shift, see Gibbs &
//! Candès 2021's adaptive-conformal variant — a follow-up if this
//! baseline proves insufficient in production.

use serde::{Deserialize, Serialize};

/// br-ft-qhav9: configuration for the conformal calibration layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SwarmTailRiskConformalConfig {
    /// Miscoverage budget α. Coverage ≥ 1 - α. Default: 0.05 (95%).
    pub alpha: f64,
    /// Maximum sliding calibration window size. Larger window → tighter
    /// quantile estimate at the cost of slower adaptation to shift.
    /// Default: 1000.
    pub window_size: usize,
    /// Minimum window fill before bound is meaningful. Below this,
    /// `bound()` returns `None` so consumers can fall back to
    /// threshold-based behavior during warmup. Default: 30.
    pub min_observations: usize,
}

impl Default for SwarmTailRiskConformalConfig {
    fn default() -> Self {
        Self {
            alpha: 0.05,
            window_size: 1000,
            min_observations: 30,
        }
    }
}

impl SwarmTailRiskConformalConfig {
    /// Sanitize α to [0.001, 0.5]: α=0 is meaningless (perfect coverage
    /// requires infinite samples); α≥0.5 is below random-baseline.
    fn normalized_alpha(&self) -> f64 {
        self.alpha.clamp(0.001, 0.5)
    }

    /// Sanitize window_size to ≥ min_observations.
    fn normalized_window_size(&self) -> usize {
        self.window_size.max(self.min_observations).max(1)
    }
}

/// br-ft-qhav9: online conformal calibration layer wrapping a sliding
/// window of nonconformity scores. The (1-α) quantile is the
/// `⌈(1-α)(N+1)⌉`-th order statistic of the window.
#[derive(Debug, Clone)]
pub struct SwarmTailRiskConformalLayer {
    config: SwarmTailRiskConformalConfig,
    /// FIFO calibration window. VecDeque so eviction is O(1) (mirrors
    /// br-ft-z9byv VecDeque-backed Histogram).
    window: std::collections::VecDeque<f64>,
}

impl SwarmTailRiskConformalLayer {
    #[must_use]
    pub fn new(config: SwarmTailRiskConformalConfig) -> Self {
        let cap = config.normalized_window_size();
        Self {
            config,
            window: std::collections::VecDeque::with_capacity(cap),
        }
    }

    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(SwarmTailRiskConformalConfig::default())
    }

    /// Record a new nonconformity score. Non-finite values are dropped.
    pub fn record(&mut self, score: f64) {
        if !score.is_finite() {
            return;
        }
        let cap = self.config.normalized_window_size();
        while self.window.len() >= cap {
            self.window.pop_front();
        }
        self.window.push_back(score);
    }

    /// (1-α)-conformal upper bound on the next observation.
    ///
    /// Returns `None` until the window has at least `min_observations`
    /// entries. The bound is the `⌈(1-α)(N+1)⌉`-th order statistic
    /// (1-indexed). For α=0.05, N=100: `⌈95.95⌉ = 96`; that's the
    /// 96th-largest sample.
    #[must_use]
    pub fn bound(&self) -> Option<f64> {
        if self.window.len() < self.config.min_observations {
            return None;
        }
        let alpha = self.config.normalized_alpha();
        let n = self.window.len();
        let target_rank = ((1.0 - alpha) * (n as f64 + 1.0)).ceil() as usize;
        let idx = target_rank.saturating_sub(1).min(n - 1);
        let mut sorted: Vec<f64> = self.window.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(sorted[idx])
    }

    #[must_use]
    pub fn observation_count(&self) -> usize {
        self.window.len()
    }

    #[must_use]
    pub fn alpha(&self) -> f64 {
        self.config.normalized_alpha()
    }

    #[must_use]
    pub fn target_coverage(&self) -> f64 {
        1.0 - self.alpha()
    }

    /// Snapshot for telemetry / evidence record inclusion.
    #[must_use]
    pub fn snapshot(&self) -> SwarmTailRiskConformalSnapshot {
        SwarmTailRiskConformalSnapshot {
            alpha: self.alpha(),
            target_coverage: self.target_coverage(),
            observation_count: self.observation_count() as u64,
            warmup_threshold: self.config.min_observations as u64,
            current_bound: self.bound(),
        }
    }
}

/// br-ft-qhav9: serialisable snapshot for inclusion in tail-risk
/// reports + evidence records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwarmTailRiskConformalSnapshot {
    pub alpha: f64,
    pub target_coverage: f64,
    pub observation_count: u64,
    pub warmup_threshold: u64,
    /// `None` during warmup; `Some(bound)` post-warmup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_bound: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conformal_layer_returns_none_during_warmup() {
        let mut layer = SwarmTailRiskConformalLayer::with_defaults();
        for i in 0..29 {
            layer.record(i as f64);
        }
        assert_eq!(layer.bound(), None, "warmup: bound is None until N ≥ 30");
        layer.record(29.0);
        assert!(
            layer.bound().is_some(),
            "post-warmup: bound is Some at N = 30"
        );
    }

    #[test]
    fn conformal_layer_drops_non_finite_scores() {
        let mut layer = SwarmTailRiskConformalLayer::with_defaults();
        layer.record(f64::NAN);
        layer.record(f64::INFINITY);
        layer.record(f64::NEG_INFINITY);
        assert_eq!(layer.observation_count(), 0);
        layer.record(42.0);
        assert_eq!(layer.observation_count(), 1);
    }

    #[test]
    fn conformal_layer_fifo_evicts_oldest_at_window_size() {
        let cfg = SwarmTailRiskConformalConfig {
            alpha: 0.05,
            window_size: 5,
            min_observations: 1,
        };
        let mut layer = SwarmTailRiskConformalLayer::new(cfg);
        for i in 0..10 {
            layer.record(i as f64);
        }
        assert_eq!(
            layer.observation_count(),
            5,
            "window must be capped at 5 after 10 inserts"
        );
        let b = layer.bound().expect("bound after warmup");
        assert!(
            b >= 5.0,
            "bound={b} must reflect the most recent 5 samples [5..=9], not [0..=4]; FIFO failed"
        );
    }

    #[test]
    fn conformal_layer_bound_is_correct_order_statistic() {
        // For α=0.05, N=99: ⌈0.95 × 100⌉ = ⌈95.0⌉ = 95; 0-idx = 94.
        // With sorted samples [1.0..=99.0], the 94th 0-indexed is 95.0.
        let cfg = SwarmTailRiskConformalConfig {
            alpha: 0.05,
            window_size: 100,
            min_observations: 30,
        };
        let mut layer = SwarmTailRiskConformalLayer::new(cfg);
        for i in 1..=99 {
            layer.record(i as f64);
        }
        let b = layer.bound().expect("bound after 99 samples");
        assert!(
            (b - 95.0).abs() < 1e-9,
            "br-ft-qhav9 algorithm: ⌈(1-α)(N+1)⌉-th order statistic for α=0.05, N=99 \
             must be the 95th sample (0-idx=94, value=95.0); got {b}"
        );
    }

    #[test]
    fn conformal_layer_coverage_holds_on_synthetic_distribution() {
        // Coverage guarantee: P(score_{t+1} ≤ bound) ≥ 1-α finite-sample.
        // With α=0.10 (90% target) + 1000 calibration samples from
        // uniform[0, 1] + 1000 NEW samples evaluated against the
        // FROZEN bound, ≥ 90% should fall below.
        let cfg = SwarmTailRiskConformalConfig {
            alpha: 0.10,
            window_size: 1000,
            min_observations: 100,
        };
        let mut layer = SwarmTailRiskConformalLayer::new(cfg);
        // Park-Miller LCG (deterministic, pure-Rust).
        let mut state: u64 = 12345;
        let next = |state: &mut u64| -> f64 {
            *state = (*state).wrapping_mul(48271).wrapping_rem(0x7fff_ffff);
            (*state as f64) / (0x7fff_ffff as f64)
        };
        // Calibration phase.
        for _ in 0..1000 {
            layer.record(next(&mut state));
        }
        let bound = layer.bound().expect("bound after 1000 samples");
        // Empirical-quantile bound for α=0.10 should be ≈ 0.90 with
        // sampling slack ~ √(α(1-α)/N) ≈ 0.0095.
        assert!(
            bound > 0.85 && bound < 0.95,
            "calibration bound={bound} must be in [0.85, 0.95] for uniform[0,1] + α=0.10 + N=1000"
        );
        // Coverage phase against frozen bound.
        let frozen_bound = bound;
        let mut covered = 0u32;
        for _ in 0..1000 {
            let s = next(&mut state);
            if s <= frozen_bound {
                covered += 1;
            }
        }
        let coverage_rate = f64::from(covered) / 1000.0;
        assert!(
            coverage_rate >= 0.85,
            "br-ft-qhav9 coverage guarantee: ≥ (1-α)=0.90 expected; got {coverage_rate:.3} for {covered} covered (with sampling slack)"
        );
    }

    #[test]
    fn conformal_layer_snapshot_serde_roundtrips() {
        let mut layer = SwarmTailRiskConformalLayer::with_defaults();
        for i in 0..50 {
            layer.record(i as f64);
        }
        let snap = layer.snapshot();
        assert!(snap.current_bound.is_some());
        let json = serde_json::to_string(&snap).expect("snapshot serializes");
        let parsed: SwarmTailRiskConformalSnapshot =
            serde_json::from_str(&json).expect("snapshot deserializes");
        assert_eq!(parsed, snap);
    }

    #[test]
    fn conformal_layer_config_normalizes_alpha_and_window_size() {
        let cfg = SwarmTailRiskConformalConfig {
            alpha: 0.0,
            window_size: 100,
            min_observations: 30,
        };
        assert!((cfg.normalized_alpha() - 0.001).abs() < 1e-9);

        let cfg = SwarmTailRiskConformalConfig {
            alpha: 0.99,
            window_size: 100,
            min_observations: 30,
        };
        assert!((cfg.normalized_alpha() - 0.5).abs() < 1e-9);

        let cfg = SwarmTailRiskConformalConfig {
            alpha: 0.05,
            window_size: 5,
            min_observations: 30,
        };
        assert_eq!(cfg.normalized_window_size(), 30);
    }
}
