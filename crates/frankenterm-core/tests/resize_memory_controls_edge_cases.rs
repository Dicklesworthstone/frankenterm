//! Integration tests for resize memory controls under edge-case scenarios.
//!
//! Validates `ResizeMemoryPolicy` behavior across multi-step pressure
//! transitions, config extremes, and helper function boundary conditions.
//! Contributes to wa-1u90p.7.1 (unit test expansion).

use frankenterm_core::memory_pressure::MemoryPressureTier;
use frankenterm_core::resize_memory_controls::{
    ResizeMemoryBudget, ResizeMemoryConfig, ResizeMemoryMetrics, ResizeMemoryPolicy,
    effective_cold_batch_size, effective_overscan_rows, scratch_allocation_allowed,
};

// ---------------------------------------------------------------------------
// Multi-step pressure scenarios
// ---------------------------------------------------------------------------

#[test]
fn sustained_red_pressure_accumulates_pause_metrics() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());

    for _ in 0..10 {
        let budget = policy.compute_budget(MemoryPressureTier::Red);
        assert!(budget.cold_reflow_paused);
    }

    let m = policy.metrics();
    assert_eq!(m.red_computations, 10);
    assert_eq!(m.cold_reflow_pauses, 10);
    assert_eq!(m.budget_computations, 10);
}

#[test]
fn oscillating_pressure_produces_correct_metrics() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());

    // Rapidly oscillate between Green and Red (simulates flapping pressure).
    for _ in 0..5 {
        let _ = policy.compute_budget(MemoryPressureTier::Green);
        let _ = policy.compute_budget(MemoryPressureTier::Red);
    }

    let m = policy.metrics();
    assert_eq!(m.budget_computations, 10);
    assert_eq!(m.green_computations, 5);
    assert_eq!(m.red_computations, 5);
    assert_eq!(m.cold_reflow_pauses, 5);
    // Red triggers batch_size_reductions, overscan_cap_reductions, backlog_cap_reductions.
    assert_eq!(m.batch_size_reductions, 5);
    assert_eq!(m.overscan_cap_reductions, 5);
    assert_eq!(m.backlog_cap_reductions, 5);
}

#[test]
fn reset_then_reuse_starts_fresh() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
    let _ = policy.compute_budget(MemoryPressureTier::Orange);
    let _ = policy.compute_budget(MemoryPressureTier::Red);
    assert!(policy.metrics().budget_computations > 0);

    policy.reset_metrics();
    assert_eq!(policy.metrics().budget_computations, 0);
    assert_eq!(policy.metrics().orange_computations, 0);
    assert_eq!(policy.metrics().red_computations, 0);
    assert_eq!(policy.metrics().compaction_triggers, 0);

    // Policy still works after reset.
    let budget = policy.compute_budget(MemoryPressureTier::Yellow);
    assert_eq!(budget.cold_batch_size, 32);
    assert_eq!(policy.metrics().budget_computations, 1);
    assert_eq!(policy.metrics().yellow_computations, 1);
}

// ---------------------------------------------------------------------------
// Extreme config values
// ---------------------------------------------------------------------------

#[test]
fn zero_batch_sizes_produce_zero_budgets() {
    let config = ResizeMemoryConfig {
        normal_batch_size: 0,
        yellow_batch_size: 0,
        orange_batch_size: 0,
        ..ResizeMemoryConfig::default()
    };
    let mut policy = ResizeMemoryPolicy::new(config);

    assert_eq!(
        policy
            .compute_budget(MemoryPressureTier::Green)
            .cold_batch_size,
        0
    );
    assert_eq!(
        policy
            .compute_budget(MemoryPressureTier::Yellow)
            .cold_batch_size,
        0
    );
    assert_eq!(
        policy
            .compute_budget(MemoryPressureTier::Orange)
            .cold_batch_size,
        0
    );
    // Red always uses 1, independent of orange_batch_size.
    assert_eq!(
        policy
            .compute_budget(MemoryPressureTier::Red)
            .cold_batch_size,
        1
    );
}

#[test]
fn max_scratch_buffer_zero_blocks_all_allocations() {
    let config = ResizeMemoryConfig {
        max_scratch_buffer_bytes: 0,
        ..ResizeMemoryConfig::default()
    };
    let mut policy = ResizeMemoryPolicy::new(config);

    for tier in [
        MemoryPressureTier::Green,
        MemoryPressureTier::Yellow,
        MemoryPressureTier::Orange,
        MemoryPressureTier::Red,
    ] {
        let budget = policy.compute_budget(tier);
        assert_eq!(
            budget.max_scratch_bytes, 0,
            "tier {:?} should have zero scratch",
            tier
        );
        assert!(scratch_allocation_allowed(&budget, 0));
        assert!(!scratch_allocation_allowed(&budget, 1));
    }
}

#[test]
fn very_large_config_values_do_not_overflow() {
    let config = ResizeMemoryConfig {
        normal_batch_size: usize::MAX,
        yellow_batch_size: usize::MAX / 2,
        orange_batch_size: usize::MAX / 4,
        normal_overscan_cap: usize::MAX,
        yellow_overscan_cap: usize::MAX / 2,
        pressure_overscan_cap: usize::MAX / 4,
        normal_backlog_cap: usize::MAX,
        yellow_backlog_cap: usize::MAX / 2,
        orange_backlog_cap: usize::MAX / 4,
        max_scratch_buffer_bytes: usize::MAX,
        compaction_batch_size: usize::MAX,
        ..ResizeMemoryConfig::default()
    };
    let mut policy = ResizeMemoryPolicy::new(config);

    // These should not panic from integer overflow.
    let green = policy.compute_budget(MemoryPressureTier::Green);
    assert_eq!(green.cold_batch_size, usize::MAX);

    let yellow = policy.compute_budget(MemoryPressureTier::Yellow);
    assert_eq!(yellow.cold_batch_size, usize::MAX / 2);

    let orange = policy.compute_budget(MemoryPressureTier::Orange);
    assert_eq!(orange.cold_batch_size, usize::MAX / 4);

    let red = policy.compute_budget(MemoryPressureTier::Red);
    assert_eq!(red.cold_batch_size, 1);

    // Scratch bytes: usize::MAX / 2, /4, /8 should not overflow.
    assert_eq!(yellow.max_scratch_bytes, usize::MAX / 2);
    assert_eq!(orange.max_scratch_bytes, usize::MAX / 4);
    assert_eq!(red.max_scratch_bytes, usize::MAX / 8);
}

// ---------------------------------------------------------------------------
// Helper function edge cases
// ---------------------------------------------------------------------------

#[test]
fn effective_cold_batch_paused_ignores_remaining() {
    let budget = ResizeMemoryBudget {
        tier: MemoryPressureTier::Red,
        cold_batch_size: 100,
        cold_reflow_paused: true,
        overscan_cap: 32,
        backlog_cap: 1000,
        compact_before_resize: true,
        compaction_batch_size: 64,
        max_scratch_bytes: 1024,
    };
    // Even with large remaining, pause => 0.
    assert_eq!(effective_cold_batch_size(&budget, 1_000_000), 0);
    assert_eq!(effective_cold_batch_size(&budget, 0), 0);
}

#[test]
fn effective_cold_batch_size_with_usize_max_remaining() {
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
    assert_eq!(effective_cold_batch_size(&budget, usize::MAX), 64);
}

#[test]
fn effective_overscan_rows_large_scrollback() {
    let budget = ResizeMemoryBudget {
        tier: MemoryPressureTier::Yellow,
        cold_batch_size: 32,
        cold_reflow_paused: false,
        overscan_cap: 128,
        backlog_cap: 524_288,
        compact_before_resize: true,
        compaction_batch_size: 256,
        max_scratch_bytes: 32 * 1024 * 1024,
    };
    // 24 physical rows, 1_000_000 scrollback => 999_976 available but capped at 128.
    assert_eq!(effective_overscan_rows(&budget, 24, 1_000_000), 128);
}

#[test]
fn effective_overscan_rows_single_row_viewport() {
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
    // 1 physical row, 100 scrollback lines => 99 available, capped at 256 => 99.
    assert_eq!(effective_overscan_rows(&budget, 1, 100), 99);
}

// ---------------------------------------------------------------------------
// Serde stability
// ---------------------------------------------------------------------------

#[test]
fn config_deserializes_with_missing_fields_as_defaults() {
    // Simulate a config JSON missing new fields.
    let json = r#"{"enabled": true}"#;
    let config: ResizeMemoryConfig = serde_json::from_str(json).expect("partial deserialize");
    assert!(config.enabled);
    assert_eq!(config.normal_batch_size, 64);
    assert_eq!(config.max_scratch_buffer_bytes, 64 * 1024 * 1024);
}

#[test]
fn metrics_default_is_all_zeros() {
    let m = ResizeMemoryMetrics::default();
    assert_eq!(m.budget_computations, 0);
    assert_eq!(m.green_computations, 0);
    assert_eq!(m.yellow_computations, 0);
    assert_eq!(m.orange_computations, 0);
    assert_eq!(m.red_computations, 0);
    assert_eq!(m.cold_reflow_pauses, 0);
    assert_eq!(m.compaction_triggers, 0);
    assert_eq!(m.batch_size_reductions, 0);
    assert_eq!(m.overscan_cap_reductions, 0);
    assert_eq!(m.backlog_cap_reductions, 0);
}

// ---------------------------------------------------------------------------
// Budget field consistency under pressure
// ---------------------------------------------------------------------------

#[test]
fn all_tiers_set_compact_before_resize_consistently() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());

    assert!(
        !policy
            .compute_budget(MemoryPressureTier::Green)
            .compact_before_resize
    );
    assert!(
        policy
            .compute_budget(MemoryPressureTier::Yellow)
            .compact_before_resize
    );
    assert!(
        policy
            .compute_budget(MemoryPressureTier::Orange)
            .compact_before_resize
    );
    assert!(
        policy
            .compute_budget(MemoryPressureTier::Red)
            .compact_before_resize
    );
}

#[test]
fn only_red_pauses_cold_reflow_by_default() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());

    assert!(
        !policy
            .compute_budget(MemoryPressureTier::Green)
            .cold_reflow_paused
    );
    assert!(
        !policy
            .compute_budget(MemoryPressureTier::Yellow)
            .cold_reflow_paused
    );
    assert!(
        !policy
            .compute_budget(MemoryPressureTier::Orange)
            .cold_reflow_paused
    );
    assert!(
        policy
            .compute_budget(MemoryPressureTier::Red)
            .cold_reflow_paused
    );
}
