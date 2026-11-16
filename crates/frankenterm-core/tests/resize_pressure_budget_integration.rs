//! Cross-module integration tests: memory pressure → resize budget → action pipeline.
//!
//! Validates that the full pressure-to-action chain works correctly when
//! `MemoryPressureTier` values flow through `ResizeMemoryPolicy` to produce
//! coherent `ResizeMemoryBudget` decisions, and that helper functions respect
//! those decisions under real-world-like scenarios.
//!
//! Contributes to wa-1u90p.7.1 (unit test expansion).

use frankenterm_core::memory_pressure::MemoryPressureTier;
use frankenterm_core::resize_memory_controls::{
    ResizeMemoryBudget, ResizeMemoryConfig, ResizeMemoryPolicy, effective_cold_batch_size,
    effective_overscan_rows, scratch_allocation_allowed,
};

// ---------------------------------------------------------------------------
// Full pipeline: pressure → budget → effective values
// ---------------------------------------------------------------------------

#[test]
fn green_pressure_allows_full_batch_and_overscan() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
    let budget = policy.compute_budget(MemoryPressureTier::Green);

    // Green should give the largest batch size and overscan.
    assert_eq!(budget.cold_batch_size, 64);
    assert!(!budget.cold_reflow_paused);
    assert!(!budget.compact_before_resize);

    // Effective batch size should equal configured batch size when plenty remaining.
    let eff = effective_cold_batch_size(&budget, 10_000);
    assert_eq!(eff, 64);

    // Overscan should be capped by budget, not by scrollback.
    let overscan = effective_overscan_rows(&budget, 24, 100_000);
    assert_eq!(overscan, budget.overscan_cap);
}

#[test]
fn yellow_pressure_reduces_batch_and_enables_compaction() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
    let green = policy.compute_budget(MemoryPressureTier::Green);
    let yellow = policy.compute_budget(MemoryPressureTier::Yellow);

    // Yellow batch should be smaller than Green.
    assert!(yellow.cold_batch_size < green.cold_batch_size);
    assert_eq!(yellow.cold_batch_size, 32);

    // Yellow enables compaction.
    assert!(yellow.compact_before_resize);
    assert!(!yellow.cold_reflow_paused);

    // Scratch allocation should be more limited.
    assert!(yellow.max_scratch_bytes < green.max_scratch_bytes);
}

#[test]
fn orange_pressure_further_restricts_budget() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
    let yellow = policy.compute_budget(MemoryPressureTier::Yellow);
    let orange = policy.compute_budget(MemoryPressureTier::Orange);

    assert!(orange.cold_batch_size < yellow.cold_batch_size);
    assert!(orange.overscan_cap < yellow.overscan_cap);
    assert!(orange.backlog_cap < yellow.backlog_cap);
    assert!(orange.max_scratch_bytes < yellow.max_scratch_bytes);
    assert!(orange.compact_before_resize);
    assert!(!orange.cold_reflow_paused);
}

#[test]
fn red_pressure_pauses_reflow_and_minimizes_budget() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
    let budget = policy.compute_budget(MemoryPressureTier::Red);

    // Red should pause cold reflow entirely.
    assert!(budget.cold_reflow_paused);
    assert_eq!(budget.cold_batch_size, 1);
    assert!(budget.compact_before_resize);

    // Effective batch size is 0 because reflow is paused.
    assert_eq!(effective_cold_batch_size(&budget, 1_000_000), 0);

    // Overscan should be at minimum.
    let overscan = effective_overscan_rows(&budget, 24, 100_000);
    assert!(overscan <= budget.overscan_cap);
}

// ---------------------------------------------------------------------------
// Monotonicity: stricter pressure → stricter budget
// ---------------------------------------------------------------------------

#[test]
fn budget_batch_sizes_decrease_monotonically_with_pressure() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
    let tiers = [
        MemoryPressureTier::Green,
        MemoryPressureTier::Yellow,
        MemoryPressureTier::Orange,
        MemoryPressureTier::Red,
    ];
    let budgets: Vec<ResizeMemoryBudget> =
        tiers.iter().map(|t| policy.compute_budget(*t)).collect();

    for w in budgets.windows(2) {
        assert!(
            w[0].cold_batch_size >= w[1].cold_batch_size,
            "batch size should decrease: {:?}={} >= {:?}={}",
            w[0].tier,
            w[0].cold_batch_size,
            w[1].tier,
            w[1].cold_batch_size,
        );
    }
}

#[test]
fn budget_scratch_bytes_decrease_monotonically_with_pressure() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
    let tiers = [
        MemoryPressureTier::Green,
        MemoryPressureTier::Yellow,
        MemoryPressureTier::Orange,
        MemoryPressureTier::Red,
    ];
    let budgets: Vec<ResizeMemoryBudget> =
        tiers.iter().map(|t| policy.compute_budget(*t)).collect();

    for w in budgets.windows(2) {
        assert!(
            w[0].max_scratch_bytes >= w[1].max_scratch_bytes,
            "scratch bytes should decrease: {:?}={} >= {:?}={}",
            w[0].tier,
            w[0].max_scratch_bytes,
            w[1].tier,
            w[1].max_scratch_bytes,
        );
    }
}

#[test]
fn budget_overscan_cap_decreases_monotonically() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
    let tiers = [
        MemoryPressureTier::Green,
        MemoryPressureTier::Yellow,
        MemoryPressureTier::Orange,
        MemoryPressureTier::Red,
    ];
    let budgets: Vec<ResizeMemoryBudget> =
        tiers.iter().map(|t| policy.compute_budget(*t)).collect();

    for w in budgets.windows(2) {
        assert!(
            w[0].overscan_cap >= w[1].overscan_cap,
            "overscan_cap should decrease: {:?}={} >= {:?}={}",
            w[0].tier,
            w[0].overscan_cap,
            w[1].tier,
            w[1].overscan_cap,
        );
    }
}

#[test]
fn budget_backlog_cap_decreases_monotonically() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
    let tiers = [
        MemoryPressureTier::Green,
        MemoryPressureTier::Yellow,
        MemoryPressureTier::Orange,
        MemoryPressureTier::Red,
    ];
    let budgets: Vec<ResizeMemoryBudget> =
        tiers.iter().map(|t| policy.compute_budget(*t)).collect();

    for w in budgets.windows(2) {
        assert!(
            w[0].backlog_cap >= w[1].backlog_cap,
            "backlog_cap should decrease: {:?}={} >= {:?}={}",
            w[0].tier,
            w[0].backlog_cap,
            w[1].tier,
            w[1].backlog_cap,
        );
    }
}

// ---------------------------------------------------------------------------
// Scratch allocation coherence
// ---------------------------------------------------------------------------

#[test]
fn scratch_allocation_respects_budget_at_all_tiers() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());

    for tier in [
        MemoryPressureTier::Green,
        MemoryPressureTier::Yellow,
        MemoryPressureTier::Orange,
        MemoryPressureTier::Red,
    ] {
        let budget = policy.compute_budget(tier);
        // Zero bytes should always be allowed.
        assert!(
            scratch_allocation_allowed(&budget, 0),
            "{tier:?}: 0 bytes should be allowed"
        );
        // One byte over the limit should be denied (unless limit is usize::MAX).
        if budget.max_scratch_bytes < usize::MAX {
            assert!(
                !scratch_allocation_allowed(&budget, budget.max_scratch_bytes + 1),
                "{tier:?}: max+1 bytes should be denied"
            );
        }
        // Exactly at the limit should be allowed.
        assert!(
            scratch_allocation_allowed(&budget, budget.max_scratch_bytes),
            "{tier:?}: exactly max bytes should be allowed"
        );
    }
}

// ---------------------------------------------------------------------------
// Effective values under small remaining counts
// ---------------------------------------------------------------------------

#[test]
fn effective_batch_size_capped_by_remaining() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
    let budget = policy.compute_budget(MemoryPressureTier::Green);
    assert_eq!(budget.cold_batch_size, 64);

    // If only 10 lines remain, effective batch should be min(64, 10) = 10.
    assert_eq!(effective_cold_batch_size(&budget, 10), 10);
    assert_eq!(effective_cold_batch_size(&budget, 1), 1);
    assert_eq!(effective_cold_batch_size(&budget, 0), 0);
}

#[test]
fn effective_overscan_limited_by_scrollback_minus_viewport() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());
    let budget = policy.compute_budget(MemoryPressureTier::Green);

    // 24-row viewport, 30 total scrollback → 6 available for overscan.
    let overscan = effective_overscan_rows(&budget, 24, 30);
    assert_eq!(overscan, 6);

    // Same viewport, fewer lines than viewport → 0 overscan.
    let overscan_tiny = effective_overscan_rows(&budget, 24, 20);
    assert_eq!(overscan_tiny, 0);
}

// ---------------------------------------------------------------------------
// Pressure oscillation: budgets should be stateless per-call
// ---------------------------------------------------------------------------

#[test]
fn pressure_oscillation_produces_correct_budgets_each_call() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());

    // Oscillate rapidly and verify each call reflects the requested tier.
    for _ in 0..20 {
        let green = policy.compute_budget(MemoryPressureTier::Green);
        assert_eq!(green.cold_batch_size, 64);
        assert!(!green.cold_reflow_paused);

        let red = policy.compute_budget(MemoryPressureTier::Red);
        assert_eq!(red.cold_batch_size, 1);
        assert!(red.cold_reflow_paused);
    }
    // Metrics should reflect total calls.
    let m = policy.metrics();
    assert_eq!(m.budget_computations, 40);
    assert_eq!(m.green_computations, 20);
    assert_eq!(m.red_computations, 20);
}

// ---------------------------------------------------------------------------
// Tier stored in budget matches requested tier
// ---------------------------------------------------------------------------

#[test]
fn budget_tier_field_matches_requested_tier() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());

    for tier in [
        MemoryPressureTier::Green,
        MemoryPressureTier::Yellow,
        MemoryPressureTier::Orange,
        MemoryPressureTier::Red,
    ] {
        let budget = policy.compute_budget(tier);
        assert_eq!(budget.tier, tier, "budget.tier should match requested tier");
    }
}

// ---------------------------------------------------------------------------
// Custom config with extreme values
// ---------------------------------------------------------------------------

#[test]
fn custom_aggressive_config_produces_tighter_budgets() {
    let config = ResizeMemoryConfig {
        normal_batch_size: 8,
        yellow_batch_size: 4,
        orange_batch_size: 2,
        normal_overscan_cap: 16,
        yellow_overscan_cap: 8,
        pressure_overscan_cap: 4,
        normal_backlog_cap: 100,
        yellow_backlog_cap: 50,
        orange_backlog_cap: 25,
        max_scratch_buffer_bytes: 1024,
        ..ResizeMemoryConfig::default()
    };
    let mut policy = ResizeMemoryPolicy::new(config);

    let green = policy.compute_budget(MemoryPressureTier::Green);
    assert_eq!(green.cold_batch_size, 8);
    assert_eq!(green.overscan_cap, 16);
    assert_eq!(green.backlog_cap, 100);

    let yellow = policy.compute_budget(MemoryPressureTier::Yellow);
    assert_eq!(yellow.cold_batch_size, 4);
    assert_eq!(yellow.overscan_cap, 8);
    assert_eq!(yellow.backlog_cap, 50);

    let orange = policy.compute_budget(MemoryPressureTier::Orange);
    assert_eq!(orange.cold_batch_size, 2);
    assert_eq!(orange.overscan_cap, 4);
    assert_eq!(orange.backlog_cap, 25);

    // Red always uses batch_size=1, independent of config.
    let red = policy.compute_budget(MemoryPressureTier::Red);
    assert_eq!(red.cold_batch_size, 1);
}

// ---------------------------------------------------------------------------
// Policy disabled: budget still reflects tier
// ---------------------------------------------------------------------------

#[test]
fn disabled_policy_still_returns_tier_in_budget() {
    let config = ResizeMemoryConfig {
        enabled: false,
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
        assert_eq!(budget.tier, tier);
    }
}

// ---------------------------------------------------------------------------
// Metric consistency across full tier sweep
// ---------------------------------------------------------------------------

#[test]
fn metrics_account_for_all_tier_computations() {
    let mut policy = ResizeMemoryPolicy::new(ResizeMemoryConfig::default());

    let _ = policy.compute_budget(MemoryPressureTier::Green);
    let _ = policy.compute_budget(MemoryPressureTier::Green);
    let _ = policy.compute_budget(MemoryPressureTier::Yellow);
    let _ = policy.compute_budget(MemoryPressureTier::Orange);
    let _ = policy.compute_budget(MemoryPressureTier::Orange);
    let _ = policy.compute_budget(MemoryPressureTier::Orange);
    let _ = policy.compute_budget(MemoryPressureTier::Red);

    let m = policy.metrics();
    assert_eq!(m.budget_computations, 7);
    assert_eq!(m.green_computations, 2);
    assert_eq!(m.yellow_computations, 1);
    assert_eq!(m.orange_computations, 3);
    assert_eq!(m.red_computations, 1);
    assert_eq!(m.cold_reflow_pauses, 1); // Only Red pauses.
}
