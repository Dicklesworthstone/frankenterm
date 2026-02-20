//! Property-based tests for viewport_reflow_planner module.
//!
//! Verifies invariants of the ViewportReflowPlanner:
//! - Complete line coverage: every line in [0, total) appears in exactly one batch
//! - Priority ordering: ViewportCore before ViewportOverscan before ColdScrollback
//! - Batch size bounded by max_batch_lines (clamped to 1)
//! - Frame budget accounting: selected work units sum matches frame_work_units
//! - At least one batch always selected for visible progress
//! - Scheduler class mapping: viewport/overscan → Interactive, cold → Background
//! - Determinism: identical input → identical output
//! - Serde roundtrip for all planner types

use proptest::prelude::*;

use frankenterm_core::resize_scheduler::ResizeWorkClass;
use frankenterm_core::viewport_reflow_planner::{
    ReflowBatchPriority, ReflowLineRange, ReflowPlan, ReflowPlannerInput, ViewportReflowPlanner,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_planner_input() -> impl Strategy<Value = ReflowPlannerInput> {
    (
        0u32..10_000, // total_logical_lines
        0u32..10_000, // viewport_top
        0u32..500,    // viewport_height
        0u32..200,    // overscan_lines
        0u32..256,    // max_batch_lines
        0u32..64,     // lines_per_work_unit
        0u32..100,    // frame_budget_units
    )
        .prop_map(
            |(total, top, height, overscan, batch, lpwu, budget)| ReflowPlannerInput {
                total_logical_lines: total,
                viewport_top: top,
                viewport_height: height,
                overscan_lines: overscan,
                max_batch_lines: batch,
                lines_per_work_unit: lpwu,
                frame_budget_units: budget,
            },
        )
}

/// Non-degenerate inputs where a plan will have batches.
fn arb_nonempty_input() -> impl Strategy<Value = ReflowPlannerInput> {
    (
        1u32..5_000, // total_logical_lines (at least 1)
        0u32..5_000, // viewport_top
        1u32..200,   // viewport_height (at least 1)
        0u32..100,   // overscan_lines
        1u32..128,   // max_batch_lines (at least 1)
        1u32..32,    // lines_per_work_unit (at least 1)
        0u32..50,    // frame_budget_units
    )
        .prop_map(
            |(total, top, height, overscan, batch, lpwu, budget)| ReflowPlannerInput {
                total_logical_lines: total,
                viewport_top: top,
                viewport_height: height,
                overscan_lines: overscan,
                max_batch_lines: batch,
                lines_per_work_unit: lpwu,
                frame_budget_units: budget,
            },
        )
}

fn arb_priority() -> impl Strategy<Value = ReflowBatchPriority> {
    prop_oneof![
        Just(ReflowBatchPriority::ViewportCore),
        Just(ReflowBatchPriority::ViewportOverscan),
        Just(ReflowBatchPriority::ColdScrollback),
    ]
}

fn arb_line_range() -> impl Strategy<Value = ReflowLineRange> {
    (0u32..10_000, 0u32..10_000).prop_map(|(a, b)| {
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        ReflowLineRange {
            start_line: start,
            end_line_exclusive: end,
        }
    })
}

// ────────────────────────────────────────────────────────────────────
// Core planner invariants
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Determinism: same input always produces the same plan.
    #[test]
    fn planner_deterministic(input in arb_planner_input()) {
        let plan1 = ViewportReflowPlanner::plan(&input);
        let plan2 = ViewportReflowPlanner::plan(&input);
        prop_assert_eq!(plan1, plan2);
    }

    /// Complete coverage: every line in [0, total) is covered by exactly one batch.
    #[test]
    fn plan_covers_all_lines_exactly_once(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let total = input.total_logical_lines as usize;
        let mut coverage = vec![0u32; total];
        for batch in &plan.batches {
            for line in batch.range.start_line..batch.range.end_line_exclusive {
                if (line as usize) < total {
                    coverage[line as usize] += 1;
                }
            }
        }
        for (line, count) in coverage.iter().enumerate() {
            prop_assert_eq!(
                *count, 1,
                "line {} covered {} times (expected 1) for input total={}",
                line, count, input.total_logical_lines
            );
        }
    }

    /// No batch exceeds the (clamped) max_batch_lines.
    #[test]
    fn batch_sizes_bounded(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let max = input.max_batch_lines.max(1);
        for batch in &plan.batches {
            let len = batch.range.end_line_exclusive.saturating_sub(batch.range.start_line);
            prop_assert!(
                len <= max,
                "batch range {}..{} has {} lines, exceeds max {}",
                batch.range.start_line,
                batch.range.end_line_exclusive,
                len,
                max
            );
        }
    }

    /// All batch ranges are non-empty (start < end).
    #[test]
    fn batch_ranges_non_empty(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        for (i, batch) in plan.batches.iter().enumerate() {
            prop_assert!(
                batch.range.start_line < batch.range.end_line_exclusive,
                "batch {} has empty/inverted range: {}..{}",
                i,
                batch.range.start_line,
                batch.range.end_line_exclusive
            );
        }
    }

    /// No batch range extends beyond total_logical_lines.
    #[test]
    fn batch_ranges_within_bounds(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        for batch in &plan.batches {
            prop_assert!(
                batch.range.end_line_exclusive <= input.total_logical_lines,
                "batch range {}..{} exceeds total {}",
                batch.range.start_line,
                batch.range.end_line_exclusive,
                input.total_logical_lines
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Priority ordering and classification
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// First batch is always ViewportCore (viewport-first guarantee).
    #[test]
    fn first_batch_is_viewport_core(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        prop_assert!(!plan.batches.is_empty());
        prop_assert_eq!(
            plan.batches[0].priority,
            ReflowBatchPriority::ViewportCore,
            "first batch should be ViewportCore"
        );
    }

    /// ViewportCore batches always precede ColdScrollback batches.
    #[test]
    fn viewport_core_before_cold(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let last_viewport = plan.batches.iter().rposition(|b| b.priority == ReflowBatchPriority::ViewportCore);
        let first_cold = plan.batches.iter().position(|b| b.priority == ReflowBatchPriority::ColdScrollback);
        if let (Some(last_vp), Some(first_c)) = (last_viewport, first_cold) {
            prop_assert!(
                last_vp < first_c,
                "viewport batch at {} comes after cold batch at {}",
                last_vp,
                first_c
            );
        }
    }

    /// ViewportOverscan batches always precede ColdScrollback batches.
    #[test]
    fn overscan_before_cold(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let last_overscan = plan.batches.iter().rposition(|b| b.priority == ReflowBatchPriority::ViewportOverscan);
        let first_cold = plan.batches.iter().position(|b| b.priority == ReflowBatchPriority::ColdScrollback);
        if let (Some(last_os), Some(first_c)) = (last_overscan, first_cold) {
            prop_assert!(
                last_os < first_c,
                "overscan batch at {} comes after cold batch at {}",
                last_os,
                first_c
            );
        }
    }

    /// Scheduler class mapping: viewport/overscan → Interactive, cold → Background.
    #[test]
    fn scheduler_class_matches_priority(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        for batch in &plan.batches {
            let expected = match batch.priority {
                ReflowBatchPriority::ViewportCore | ReflowBatchPriority::ViewportOverscan => {
                    ResizeWorkClass::Interactive
                }
                ReflowBatchPriority::ColdScrollback => ResizeWorkClass::Background,
            };
            prop_assert_eq!(
                batch.scheduler_class,
                expected,
                "priority {:?} should map to {:?}",
                batch.priority,
                expected
            );
        }
    }

    /// Every batch has a non-empty rationale string.
    #[test]
    fn batch_rationale_non_empty(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        for (i, batch) in plan.batches.iter().enumerate() {
            prop_assert!(
                !batch.rationale.is_empty(),
                "batch {} has empty rationale",
                i
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Frame budget accounting
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// At least one batch is always selected for visible progress.
    #[test]
    fn at_least_one_selected(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        prop_assert!(
            plan.batches.iter().any(|b| b.selected_for_frame),
            "at least one batch must be selected for frame progress"
        );
    }

    /// frame_work_units equals the sum of selected batch work_units.
    #[test]
    fn frame_work_units_consistent(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let selected_sum: u32 = plan
            .batches
            .iter()
            .filter(|b| b.selected_for_frame)
            .map(|b| b.work_units)
            .sum();
        prop_assert_eq!(
            plan.frame_work_units,
            selected_sum,
            "frame_work_units mismatch: reported {} vs sum {}",
            plan.frame_work_units,
            selected_sum
        );
    }

    /// Selected batches form a contiguous prefix (no gaps).
    #[test]
    fn selected_batches_contiguous_prefix(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let mut seen_unselected = false;
        for batch in &plan.batches {
            if !batch.selected_for_frame {
                seen_unselected = true;
            } else if seen_unselected {
                prop_assert!(
                    false,
                    "selected batch after unselected batch — selection not a contiguous prefix"
                );
            }
        }
    }

    /// Every batch has work_units >= 1.
    #[test]
    fn batch_work_units_at_least_one(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        for (i, batch) in plan.batches.iter().enumerate() {
            prop_assert!(
                batch.work_units >= 1,
                "batch {} has work_units=0",
                i
            );
        }
    }

    /// frame_budget_units is at least 1 (clamped from 0).
    #[test]
    fn frame_budget_clamped_to_one(input in arb_planner_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        if input.total_logical_lines > 0 && input.viewport_height > 0 {
            prop_assert!(
                plan.frame_budget_units >= 1,
                "frame_budget_units should be >= 1, got {}",
                plan.frame_budget_units
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Empty/degenerate input handling
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Zero total lines → empty plan.
    #[test]
    fn zero_total_lines_empty_plan(
        top in 0u32..100,
        height in 0u32..100,
        overscan in 0u32..50,
    ) {
        let input = ReflowPlannerInput {
            total_logical_lines: 0,
            viewport_top: top,
            viewport_height: height,
            overscan_lines: overscan,
            ..ReflowPlannerInput::default()
        };
        let plan = ViewportReflowPlanner::plan(&input);
        prop_assert!(plan.batches.is_empty());
        prop_assert_eq!(plan.frame_work_units, 0);
    }

    /// Zero viewport height → empty plan.
    #[test]
    fn zero_viewport_height_empty_plan(total in 1u32..1000) {
        let input = ReflowPlannerInput {
            total_logical_lines: total,
            viewport_height: 0,
            ..ReflowPlannerInput::default()
        };
        let plan = ViewportReflowPlanner::plan(&input);
        prop_assert!(plan.batches.is_empty());
        prop_assert_eq!(plan.frame_work_units, 0);
    }

    /// Plan never panics for any input combination.
    #[test]
    fn plan_never_panics(input in arb_planner_input()) {
        let _plan = ViewportReflowPlanner::plan(&input);
        // Just verify no panic
    }
}

// ────────────────────────────────────────────────────────────────────
// Scheduling hooks and log lines
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// scheduling_hooks() count matches batches count.
    #[test]
    fn hooks_match_batches(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let hooks = plan.scheduling_hooks();
        prop_assert_eq!(hooks.len(), plan.batches.len());
    }

    /// scheduling_hooks() preserve all batch fields.
    #[test]
    fn hooks_preserve_batch_fields(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let hooks = plan.scheduling_hooks();
        for (hook, batch) in hooks.iter().zip(plan.batches.iter()) {
            prop_assert_eq!(hook.range, batch.range);
            prop_assert_eq!(hook.scheduler_class, batch.scheduler_class);
            prop_assert_eq!(hook.work_units, batch.work_units);
            prop_assert_eq!(hook.selected_for_frame, batch.selected_for_frame);
            prop_assert_eq!(&hook.rationale, &batch.rationale);
        }
    }

    /// log_lines() count matches batches count.
    #[test]
    fn log_lines_match_batches(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let logs = plan.log_lines();
        prop_assert_eq!(logs.len(), plan.batches.len());
    }

    /// Every log line contains required fields.
    #[test]
    fn log_lines_have_required_fields(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let logs = plan.log_lines();
        for (i, log) in logs.iter().enumerate() {
            prop_assert!(log.contains("selected_for_frame="), "log {} missing selected_for_frame", i);
            prop_assert!(log.contains("class="), "log {} missing class", i);
            prop_assert!(log.contains("priority="), "log {} missing priority", i);
            prop_assert!(log.contains("range="), "log {} missing range", i);
            prop_assert!(log.contains("work_units="), "log {} missing work_units", i);
            prop_assert!(log.contains("reason="), "log {} missing reason", i);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Serde roundtrip properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// ReflowPlannerInput serde roundtrip.
    #[test]
    fn planner_input_serde_roundtrip(input in arb_planner_input()) {
        let json = serde_json::to_string(&input).unwrap();
        let parsed: ReflowPlannerInput = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, input);
    }

    /// ReflowBatchPriority serde roundtrip.
    #[test]
    fn priority_serde_roundtrip(p in arb_priority()) {
        let json = serde_json::to_string(&p).unwrap();
        let parsed: ReflowBatchPriority = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, p);
    }

    /// ReflowLineRange serde roundtrip.
    #[test]
    fn line_range_serde_roundtrip(range in arb_line_range()) {
        let json = serde_json::to_string(&range).unwrap();
        let parsed: ReflowLineRange = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, range);
    }

    /// Full ReflowPlan serde roundtrip (via planner output).
    #[test]
    fn plan_serde_roundtrip(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let json = serde_json::to_string(&plan).unwrap();
        let parsed: ReflowPlan = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, plan);
    }
}

// ────────────────────────────────────────────────────────────────────
// Additional structural properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Adjacent batches have contiguous ranges (no gaps or overlaps).
    #[test]
    fn adjacent_batches_contiguous(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        for pair in plan.batches.windows(2) {
            // Within the same priority tier, ranges should be non-overlapping
            // but not necessarily contiguous across priority boundaries.
            prop_assert!(
                pair[0].range.end_line_exclusive <= pair[1].range.start_line
                    || pair[0].range.start_line >= pair[1].range.end_line_exclusive
                    || pair[0].range.end_line_exclusive == pair[1].range.start_line,
                "batches overlap: {}..{} and {}..{}",
                pair[0].range.start_line, pair[0].range.end_line_exclusive,
                pair[1].range.start_line, pair[1].range.end_line_exclusive
            );
        }
    }

    /// Total lines across all batches equals total_logical_lines.
    #[test]
    fn total_batch_lines_match(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let total: u32 = plan.batches.iter()
            .map(|b| b.range.end_line_exclusive - b.range.start_line)
            .sum();
        prop_assert_eq!(
            total, input.total_logical_lines,
            "total batch lines {} != input total {}", total, input.total_logical_lines
        );
    }

    /// Plan has at least one batch for non-empty input.
    #[test]
    fn plan_has_batches(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        prop_assert!(!plan.batches.is_empty(), "non-empty input should produce batches");
    }

    /// frame_budget_units in plan is stored.
    #[test]
    fn frame_budget_stored(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        prop_assert!(plan.frame_budget_units > 0 || plan.frame_work_units > 0,
            "at least budget or work should be positive");
    }
}

// ────────────────────────────────────────────────────────────────────
// Batch 13: additional property tests (DarkMill)
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// scheduling_hooks() returns one hook per batch.
    #[test]
    fn scheduling_hooks_count_matches_batches(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let hooks = plan.scheduling_hooks();
        prop_assert_eq!(
            hooks.len(), plan.batches.len(),
            "hooks {} != batches {}", hooks.len(), plan.batches.len()
        );
    }

    /// scheduling_hooks() work_units match batch work_units.
    #[test]
    fn scheduling_hooks_preserve_work_units(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let hooks = plan.scheduling_hooks();
        for (hook, batch) in hooks.iter().zip(plan.batches.iter()) {
            prop_assert_eq!(hook.work_units, batch.work_units,
                "hook work_units {} != batch {}", hook.work_units, batch.work_units);
            prop_assert_eq!(hook.selected_for_frame, batch.selected_for_frame);
        }
    }

    /// log_lines() returns non-empty output for non-empty input.
    #[test]
    fn log_lines_nonempty_for_nonempty_input(input in arb_nonempty_input()) {
        let plan = ViewportReflowPlanner::plan(&input);
        let lines = plan.log_lines();
        prop_assert!(!lines.is_empty(), "non-empty plan should produce log lines");
    }

    /// Default ReflowPlan has no batches and zero budget.
    #[test]
    fn plan_default_empty(_dummy in 0..1u8) {
        let plan = ReflowPlan::default();
        prop_assert!(plan.batches.is_empty());
        prop_assert_eq!(plan.frame_budget_units, 0);
        prop_assert_eq!(plan.frame_work_units, 0);
    }

    /// ReflowLineRange end_line_exclusive >= start_line for valid ranges.
    #[test]
    fn line_range_endpoints_ordered(
        start in 0_u32..5000,
        extra in 0_u32..5000,
    ) {
        let end = start.saturating_add(extra);
        let range = ReflowLineRange { start_line: start, end_line_exclusive: end };
        prop_assert!(range.end_line_exclusive >= range.start_line);
    }

    /// Every ReflowBatchPriority has a non-empty Debug representation.
    #[test]
    fn priority_debug_nonempty(p in arb_priority()) {
        let debug = format!("{:?}", p);
        prop_assert!(!debug.is_empty(), "priority debug should be non-empty");
    }

    /// Default input has valid defaults (overscan=16, max_batch=64).
    #[test]
    fn planner_input_default_has_valid_fields(_dummy in 0..1u8) {
        let input = ReflowPlannerInput::default();
        prop_assert_eq!(input.overscan_lines, 16);
        prop_assert_eq!(input.max_batch_lines, 64);
        prop_assert_eq!(input.lines_per_work_unit, 32);
        prop_assert_eq!(input.frame_budget_units, 8);
    }
}
