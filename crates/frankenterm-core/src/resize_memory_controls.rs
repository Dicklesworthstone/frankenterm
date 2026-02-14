//! Memory-pressure controls for massive multi-tab scrollback during resize.
//!
//! This module provides adaptive memory controls that prevent allocator
//! thrash and memory spikes during large-scale rewrap/repaint operations
//! across many tabs with deep scrollback (`wa-1u90p.5.5`).
//!
//! The [`ResizeMemoryPolicy`] engine maps [`MemoryPressureTier`] to concrete
//! resize parameters: batch sizes, overscan caps, backlog limits, and
//! compaction triggers. Higher pressure tiers progressively reduce memory
//! impact of resize operations while preserving viewport responsiveness.

use serde::{Deserialize, Serialize};

use crate::memory_pressure::MemoryPressureTier;

/// Configuration for memory-pressure-aware resize controls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResizeMemoryConfig {
    /// Enable memory-pressure-aware resize throttling.
    pub enabled: bool,
    /// Maximum cold scrollback batch size under Green (normal) pressure.
    pub normal_batch_size: usize,
    /// Maximum cold scrollback batch size under Yellow pressure.
    pub yellow_batch_size: usize,
    /// Maximum cold scrollback batch size under Orange pressure.
    pub orange_batch_size: usize,
    /// Whether to pause cold scrollback processing entirely under Red pressure.
    pub red_pause_cold_reflow: bool,
    /// Maximum viewport overscan rows under Green (normal) pressure.
    pub normal_overscan_cap: usize,
    /// Maximum viewport overscan rows under Yellow pressure.
    pub yellow_overscan_cap: usize,
    /// Maximum viewport overscan rows under Orange/Red pressure.
    pub pressure_overscan_cap: usize,
    /// Maximum cold scrollback backlog depth under Green pressure.
    pub normal_backlog_cap: usize,
    /// Maximum cold scrollback backlog depth under Yellow pressure.
    pub yellow_backlog_cap: usize,
    /// Maximum cold scrollback backlog depth under Orange pressure.
    pub orange_backlog_cap: usize,
    /// Whether to trigger pre-resize compaction of scrollback lines.
    pub pre_resize_compaction_enabled: bool,
    /// Number of scrollback lines to compact per batch during pre-resize.
    pub compaction_batch_size: usize,
    /// Maximum per-pane scratch buffer allocation for resize (bytes).
    pub max_scratch_buffer_bytes: usize,
}

impl Default for ResizeMemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            normal_batch_size: 64,
            yellow_batch_size: 32,
            orange_batch_size: 8,
            red_pause_cold_reflow: true,
            normal_overscan_cap: 256,
            yellow_overscan_cap: 128,
            pressure_overscan_cap: 32,
            normal_backlog_cap: 1_048_576,
            yellow_backlog_cap: 524_288,
            orange_backlog_cap: 131_072,
            pre_resize_compaction_enabled: true,
            compaction_batch_size: 256,
            max_scratch_buffer_bytes: 64 * 1024 * 1024, // 64 MiB
        }
    }
}

/// Computed adaptive parameters for a resize operation under current memory pressure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeMemoryBudget {
    /// Memory pressure tier driving these parameters.
    pub tier: MemoryPressureTier,
    /// Maximum cold scrollback batch size for this resize.
    pub cold_batch_size: usize,
    /// Whether cold scrollback processing should be paused.
    pub cold_reflow_paused: bool,
    /// Maximum viewport overscan rows.
    pub overscan_cap: usize,
    /// Maximum cold scrollback backlog depth.
    pub backlog_cap: usize,
    /// Whether pre-resize scrollback compaction should run.
    pub compact_before_resize: bool,
    /// Lines per compaction batch.
    pub compaction_batch_size: usize,
    /// Maximum scratch buffer allocation (bytes).
    pub max_scratch_bytes: usize,
}

/// Metrics for memory-pressure-aware resize decisions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeMemoryMetrics {
    /// Total resize budget computations.
    pub budget_computations: u64,
    /// Computations at each tier.
    pub green_computations: u64,
    pub yellow_computations: u64,
    pub orange_computations: u64,
    pub red_computations: u64,
    /// Number of cold reflow pauses triggered by Red tier.
    pub cold_reflow_pauses: u64,
    /// Number of pre-resize compaction triggers.
    pub compaction_triggers: u64,
    /// Number of batch size reductions applied (Yellow/Orange/Red).
    pub batch_size_reductions: u64,
    /// Number of overscan cap reductions applied.
    pub overscan_cap_reductions: u64,
    /// Number of backlog cap reductions applied.
    pub backlog_cap_reductions: u64,
}

/// Policy engine that computes resize memory budgets from pressure tiers.
///
/// Each call to [`compute_budget`](Self::compute_budget) returns a
/// [`ResizeMemoryBudget`] with tier-appropriate parameters and increments
/// the internal metrics counters for observability.
#[derive(Debug, Clone)]
pub struct ResizeMemoryPolicy {
    config: ResizeMemoryConfig,
    metrics: ResizeMemoryMetrics,
}

impl ResizeMemoryPolicy {
    /// Create a policy engine with the supplied configuration.
    #[must_use]
    pub fn new(config: ResizeMemoryConfig) -> Self {
        Self {
            config,
            metrics: ResizeMemoryMetrics::default(),
        }
    }

    /// Read current configuration.
    #[must_use]
    pub const fn config(&self) -> &ResizeMemoryConfig {
        &self.config
    }

    /// Read accumulated metrics.
    #[must_use]
    pub const fn metrics(&self) -> &ResizeMemoryMetrics {
        &self.metrics
    }

    /// Compute adaptive resize parameters for the given memory pressure tier.
    ///
    /// Returns a [`ResizeMemoryBudget`] that callers should use to size
    /// cold-scrollback batches, viewport overscan, backlog caps, and
    /// pre-resize compaction. Higher pressure tiers yield more conservative
    /// parameters to prevent memory spikes.
    pub fn compute_budget(&mut self, tier: MemoryPressureTier) -> ResizeMemoryBudget {
        self.metrics.budget_computations = self.metrics.budget_computations.saturating_add(1);

        if !self.config.enabled {
            return self.green_budget(tier);
        }

        match tier {
            MemoryPressureTier::Green => {
                self.metrics.green_computations =
                    self.metrics.green_computations.saturating_add(1);
                self.green_budget(tier)
            }
            MemoryPressureTier::Yellow => {
                self.metrics.yellow_computations =
                    self.metrics.yellow_computations.saturating_add(1);
                self.metrics.batch_size_reductions =
                    self.metrics.batch_size_reductions.saturating_add(1);
                self.metrics.overscan_cap_reductions =
                    self.metrics.overscan_cap_reductions.saturating_add(1);
                self.metrics.backlog_cap_reductions =
                    self.metrics.backlog_cap_reductions.saturating_add(1);
                if self.config.pre_resize_compaction_enabled {
                    self.metrics.compaction_triggers =
                        self.metrics.compaction_triggers.saturating_add(1);
                }
                ResizeMemoryBudget {
                    tier,
                    cold_batch_size: self.config.yellow_batch_size,
                    cold_reflow_paused: false,
                    overscan_cap: self.config.yellow_overscan_cap,
                    backlog_cap: self.config.yellow_backlog_cap,
                    compact_before_resize: self.config.pre_resize_compaction_enabled,
                    compaction_batch_size: self.config.compaction_batch_size,
                    max_scratch_bytes: self.config.max_scratch_buffer_bytes / 2,
                }
            }
            MemoryPressureTier::Orange => {
                self.metrics.orange_computations =
                    self.metrics.orange_computations.saturating_add(1);
                self.metrics.batch_size_reductions =
                    self.metrics.batch_size_reductions.saturating_add(1);
                self.metrics.overscan_cap_reductions =
                    self.metrics.overscan_cap_reductions.saturating_add(1);
                self.metrics.backlog_cap_reductions =
                    self.metrics.backlog_cap_reductions.saturating_add(1);
                if self.config.pre_resize_compaction_enabled {
                    self.metrics.compaction_triggers =
                        self.metrics.compaction_triggers.saturating_add(1);
                }
                ResizeMemoryBudget {
                    tier,
                    cold_batch_size: self.config.orange_batch_size,
                    cold_reflow_paused: false,
                    overscan_cap: self.config.pressure_overscan_cap,
                    backlog_cap: self.config.orange_backlog_cap,
                    compact_before_resize: self.config.pre_resize_compaction_enabled,
                    compaction_batch_size: self.config.compaction_batch_size / 2,
                    max_scratch_bytes: self.config.max_scratch_buffer_bytes / 4,
                }
            }
            MemoryPressureTier::Red => {
                self.metrics.red_computations = self.metrics.red_computations.saturating_add(1);
                self.metrics.batch_size_reductions =
                    self.metrics.batch_size_reductions.saturating_add(1);
                self.metrics.overscan_cap_reductions =
                    self.metrics.overscan_cap_reductions.saturating_add(1);
                self.metrics.backlog_cap_reductions =
                    self.metrics.backlog_cap_reductions.saturating_add(1);
                if self.config.red_pause_cold_reflow {
                    self.metrics.cold_reflow_pauses =
                        self.metrics.cold_reflow_pauses.saturating_add(1);
                }
                if self.config.pre_resize_compaction_enabled {
                    self.metrics.compaction_triggers =
                        self.metrics.compaction_triggers.saturating_add(1);
                }
                ResizeMemoryBudget {
                    tier,
                    cold_batch_size: 1,
                    cold_reflow_paused: self.config.red_pause_cold_reflow,
                    overscan_cap: self.config.pressure_overscan_cap,
                    backlog_cap: self.config.orange_backlog_cap / 4,
                    compact_before_resize: self.config.pre_resize_compaction_enabled,
                    compaction_batch_size: (self.config.compaction_batch_size / 4).max(1),
                    max_scratch_bytes: self.config.max_scratch_buffer_bytes / 8,
                }
            }
        }
    }

    /// Reset accumulated metrics to zero.
    pub fn reset_metrics(&mut self) {
        self.metrics = ResizeMemoryMetrics::default();
    }

    fn green_budget(&self, tier: MemoryPressureTier) -> ResizeMemoryBudget {
        ResizeMemoryBudget {
            tier,
            cold_batch_size: self.config.normal_batch_size,
            cold_reflow_paused: false,
            overscan_cap: self.config.normal_overscan_cap,
            backlog_cap: self.config.normal_backlog_cap,
            compact_before_resize: false,
            compaction_batch_size: self.config.compaction_batch_size,
            max_scratch_bytes: self.config.max_scratch_buffer_bytes,
        }
    }
}

/// Determine effective batch size given a memory budget and scrollback depth.
///
/// Returns the smaller of the budget batch size and the remaining lines.
#[must_use]
pub fn effective_cold_batch_size(budget: &ResizeMemoryBudget, remaining_lines: usize) -> usize {
    if budget.cold_reflow_paused {
        return 0;
    }
    budget.cold_batch_size.min(remaining_lines)
}

/// Determine effective overscan rows given a memory budget and physical rows.
///
/// Clamps overscan to the budget cap and the available scrollback.
#[must_use]
pub fn effective_overscan_rows(
    budget: &ResizeMemoryBudget,
    physical_rows: usize,
    scrollback_lines: usize,
) -> usize {
    let max_available = scrollback_lines.saturating_sub(physical_rows);
    budget.overscan_cap.min(max_available)
}

/// Check whether a scratch buffer allocation should be allowed.
///
/// Returns `true` if `requested_bytes` fits within the budget's scratch limit.
#[must_use]
pub fn scratch_allocation_allowed(budget: &ResizeMemoryBudget, requested_bytes: usize) -> bool {
    requested_bytes <= budget.max_scratch_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values_are_reasonable() {
        let config = ResizeMemoryConfig::default();
        assert!(config.enabled);
        assert_eq!(config.normal_batch_size, 64);
        assert_eq!(config.yellow_batch_size, 32);
        assert_eq!(config.orange_batch_size, 8);
        assert!(config.red_pause_cold_reflow);
        assert_eq!(config.normal_overscan_cap, 256);
        assert_eq!(config.yellow_overscan_cap, 128);
        assert_eq!(config.pressure_overscan_cap, 32);
        assert_eq!(config.normal_backlog_cap, 1_048_576);
        assert!(config.pre_resize_compaction_enabled);
        assert_eq!(config.max_scratch_buffer_bytes, 64 * 1024 * 1024);
    }

    #[test]
    fn green_tier_returns_normal_parameters() {
        let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
        let budget = policy.compute_budget(MemoryPressureTier::Green);

        assert_eq!(budget.tier, MemoryPressureTier::Green);
        assert_eq!(budget.cold_batch_size, 64);
        assert!(!budget.cold_reflow_paused);
        assert_eq!(budget.overscan_cap, 256);
        assert_eq!(budget.backlog_cap, 1_048_576);
        assert!(!budget.compact_before_resize);
        assert_eq!(budget.max_scratch_bytes, 64 * 1024 * 1024);
    }

    #[test]
    fn yellow_tier_reduces_batch_size_and_enables_compaction() {
        let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
        let budget = policy.compute_budget(MemoryPressureTier::Yellow);

        assert_eq!(budget.tier, MemoryPressureTier::Yellow);
        assert_eq!(budget.cold_batch_size, 32);
        assert!(!budget.cold_reflow_paused);
        assert_eq!(budget.overscan_cap, 128);
        assert_eq!(budget.backlog_cap, 524_288);
        assert!(budget.compact_before_resize);
        assert_eq!(budget.max_scratch_bytes, 32 * 1024 * 1024);
    }

    #[test]
    fn orange_tier_aggressively_reduces_parameters() {
        let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
        let budget = policy.compute_budget(MemoryPressureTier::Orange);

        assert_eq!(budget.tier, MemoryPressureTier::Orange);
        assert_eq!(budget.cold_batch_size, 8);
        assert!(!budget.cold_reflow_paused);
        assert_eq!(budget.overscan_cap, 32);
        assert_eq!(budget.backlog_cap, 131_072);
        assert!(budget.compact_before_resize);
        assert_eq!(budget.compaction_batch_size, 128);
        assert_eq!(budget.max_scratch_bytes, 16 * 1024 * 1024);
    }

    #[test]
    fn red_tier_pauses_cold_reflow_and_minimizes_budget() {
        let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
        let budget = policy.compute_budget(MemoryPressureTier::Red);

        assert_eq!(budget.tier, MemoryPressureTier::Red);
        assert_eq!(budget.cold_batch_size, 1);
        assert!(budget.cold_reflow_paused);
        assert_eq!(budget.overscan_cap, 32);
        assert_eq!(budget.backlog_cap, 131_072 / 4);
        assert!(budget.compact_before_resize);
        assert_eq!(budget.compaction_batch_size, 64);
        assert_eq!(budget.max_scratch_bytes, 8 * 1024 * 1024);
    }

    #[test]
    fn red_tier_respects_pause_disable_config() {
        let config = ResizeMemoryConfig {
            red_pause_cold_reflow: false,
            ..ResizeMemoryConfig::default()
        };
        let mut policy = ResizeMemoryPolicy::new(config);
        let budget = policy.compute_budget(MemoryPressureTier::Red);

        assert!(!budget.cold_reflow_paused);
        assert_eq!(policy.metrics().cold_reflow_pauses, 0);
    }

    #[test]
    fn disabled_policy_always_returns_green_parameters() {
        let config = ResizeMemoryConfig {
            enabled: false,
            ..ResizeMemoryConfig::default()
        };
        let mut policy = ResizeMemoryPolicy::new(config);

        for &tier in &[
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ] {
            let budget = policy.compute_budget(tier);
            assert_eq!(budget.cold_batch_size, 64);
            assert!(!budget.cold_reflow_paused);
            assert_eq!(budget.overscan_cap, 256);
            assert_eq!(budget.backlog_cap, 1_048_576);
            assert!(!budget.compact_before_resize);
        }
        assert_eq!(policy.metrics().budget_computations, 4);
        assert_eq!(policy.metrics().batch_size_reductions, 0);
    }

    #[test]
    fn metrics_accumulate_across_computations() {
        let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());

        let _ = policy.compute_budget(MemoryPressureTier::Green);
        let _ = policy.compute_budget(MemoryPressureTier::Yellow);
        let _ = policy.compute_budget(MemoryPressureTier::Orange);
        let _ = policy.compute_budget(MemoryPressureTier::Red);

        let m = policy.metrics();
        assert_eq!(m.budget_computations, 4);
        assert_eq!(m.green_computations, 1);
        assert_eq!(m.yellow_computations, 1);
        assert_eq!(m.orange_computations, 1);
        assert_eq!(m.red_computations, 1);
        assert_eq!(m.batch_size_reductions, 3); // Yellow + Orange + Red
        assert_eq!(m.overscan_cap_reductions, 3);
        assert_eq!(m.backlog_cap_reductions, 3);
        assert_eq!(m.cold_reflow_pauses, 1); // Red only
        assert_eq!(m.compaction_triggers, 3); // Yellow + Orange + Red
    }

    #[test]
    fn metrics_reset_clears_all_counters() {
        let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
        let _ = policy.compute_budget(MemoryPressureTier::Red);
        assert!(policy.metrics().budget_computations > 0);

        policy.reset_metrics();
        assert_eq!(policy.metrics().budget_computations, 0);
        assert_eq!(policy.metrics().cold_reflow_pauses, 0);
    }

    #[test]
    fn effective_cold_batch_size_respects_pause() {
        let budget = ResizeMemoryBudget {
            tier: MemoryPressureTier::Red,
            cold_batch_size: 1,
            cold_reflow_paused: true,
            overscan_cap: 32,
            backlog_cap: 1000,
            compact_before_resize: true,
            compaction_batch_size: 64,
            max_scratch_bytes: 1024,
        };
        assert_eq!(effective_cold_batch_size(&budget, 500), 0);
    }

    #[test]
    fn effective_cold_batch_size_clamps_to_remaining() {
        let budget = ResizeMemoryBudget {
            tier: MemoryPressureTier::Green,
            cold_batch_size: 64,
            cold_reflow_paused: false,
            overscan_cap: 256,
            backlog_cap: 1_048_576,
            compact_before_resize: false,
            compaction_batch_size: 256,
            max_scratch_bytes: 64 * 1024 * 1024,
        };
        assert_eq!(effective_cold_batch_size(&budget, 10), 10);
        assert_eq!(effective_cold_batch_size(&budget, 100), 64);
    }

    #[test]
    fn effective_overscan_rows_clamps_to_scrollback() {
        let budget = ResizeMemoryBudget {
            tier: MemoryPressureTier::Green,
            cold_batch_size: 64,
            cold_reflow_paused: false,
            overscan_cap: 256,
            backlog_cap: 1_048_576,
            compact_before_resize: false,
            compaction_batch_size: 256,
            max_scratch_bytes: 64 * 1024 * 1024,
        };
        // 50 physical rows, 100 total lines => 50 scrollback available
        assert_eq!(effective_overscan_rows(&budget, 50, 100), 50);
        // 50 physical rows, 500 total lines => 450 available but capped at 256
        assert_eq!(effective_overscan_rows(&budget, 50, 500), 256);
    }

    #[test]
    fn scratch_allocation_enforced_under_pressure() {
        let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
        let budget = policy.compute_budget(MemoryPressureTier::Orange);

        assert!(scratch_allocation_allowed(&budget, 1_000_000)); // 1 MB ok
        assert!(scratch_allocation_allowed(&budget, 16 * 1024 * 1024)); // 16 MiB ok (exactly at limit)
        assert!(!scratch_allocation_allowed(&budget, 17 * 1024 * 1024)); // 17 MiB exceeds
    }

    #[test]
    fn compaction_disabled_prevents_compaction_triggers() {
        let config = ResizeMemoryConfig {
            pre_resize_compaction_enabled: false,
            ..ResizeMemoryConfig::default()
        };
        let mut policy = ResizeMemoryPolicy::new(config);

        let budget = policy.compute_budget(MemoryPressureTier::Orange);
        assert!(!budget.compact_before_resize);
        assert_eq!(policy.metrics().compaction_triggers, 0);
    }

    #[test]
    fn progressive_degradation_across_all_tiers() {
        let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());

        let green = policy.compute_budget(MemoryPressureTier::Green);
        let yellow = policy.compute_budget(MemoryPressureTier::Yellow);
        let orange = policy.compute_budget(MemoryPressureTier::Orange);
        let red = policy.compute_budget(MemoryPressureTier::Red);

        // Batch sizes decrease monotonically.
        assert!(green.cold_batch_size > yellow.cold_batch_size);
        assert!(yellow.cold_batch_size > orange.cold_batch_size);
        assert!(orange.cold_batch_size > red.cold_batch_size);

        // Overscan caps decrease monotonically.
        assert!(green.overscan_cap > yellow.overscan_cap);
        assert!(yellow.overscan_cap > orange.overscan_cap);
        assert_eq!(orange.overscan_cap, red.overscan_cap); // Same floor

        // Backlog caps decrease monotonically.
        assert!(green.backlog_cap > yellow.backlog_cap);
        assert!(yellow.backlog_cap > orange.backlog_cap);
        assert!(orange.backlog_cap > red.backlog_cap);

        // Scratch bytes decrease monotonically.
        assert!(green.max_scratch_bytes > yellow.max_scratch_bytes);
        assert!(yellow.max_scratch_bytes > orange.max_scratch_bytes);
        assert!(orange.max_scratch_bytes > red.max_scratch_bytes);
    }
}
