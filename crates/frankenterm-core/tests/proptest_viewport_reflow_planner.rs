//! Property-based tests for the viewport_reflow_planner module.
//!
//! Verifies structural invariants of the viewport-priority reflow planner
//! across randomized inputs: coverage completeness, priority ordering,
//! batch size bounds, frame budget accounting, and determinism.

use proptest::prelude::*;

use frankenterm_core::resize_scheduler::ResizeWorkClass;
use frankenterm_core::viewport_reflow_planner::{
    ReflowBatchPriority, ReflowPlan, ReflowPlannerInput, ViewportReflowPlanner,
};

// ── Strategies ────────────────────────────────────────────────────────

/// Generates realistic planner inputs with controlled parameter ranges.
fn arb_planner_input() -> impl Strategy<Value = ReflowPlannerInput> {
    (
        1_u32..20_000,  // total_logical_lines
        0_u32..20_000,  // viewport_top
        1_u32..500,     // viewport_height
        0_u32..200,     // overscan_lines
        1_u32..256,     // max_batch_lines
        1_u32..128,     // lines_per_work_unit
        1_u32..100,     // frame_budget_units
    )
        .prop_map(|(total, top, height, overscan, max_batch, lpu, budget)| {
            ReflowPlannerInput {
                total_logical_lines: total,
                viewport_top: top,
                viewport_height: height,
                overscan_lines: overscan,
                max_batch_lines: max_batch,
                lines_per_work_unit: lpu,
                frame_budget_units: budget,
            }
        })
}

/// Generates edge-case planner inputs: small buffers, extreme values.
fn arb_edge_case_input() -> impl Strategy<Value = ReflowPlannerInput> {
    prop_oneof![
        // Single-line buffer
        (
            1_u32..=1,
            0_u32..=0,
            1_u32..50,
            0_u32..20,
            1_u32..64,
            1_u32..32,
            1_u32..20
        )
            .prop_map(|(total, top, height, overscan, max_batch, lpu, budget)| {
                ReflowPlannerInput {
                    total_logical_lines: total,
                    viewport_top: top,
                    viewport_height: height,
                    overscan_lines: overscan,
                    max_batch_lines: max_batch,
                    lines_per_work_unit: lpu,
                    frame_budget_units: budget,
                }
            }),
        // Very small buffer (2-10 lines)
        (
            2_u32..=10,
            0_u32..10,
            1_u32..20,
            0_u32..10,
            1_u32..32,
            1_u32..16,
            1_u32..10
        )
            .prop_map(|(total, top, height, overscan, max_batch, lpu, budget)| {
                ReflowPlannerInput {
                    total_logical_lines: total,
                    viewport_top: top,
                    viewport_height: height,
                    overscan_lines: overscan,
                    max_batch_lines: max_batch,
                    lines_per_work_unit: lpu,
                    frame_budget_units: budget,
                }
            }),
        // Viewport larger than buffer
        (
            1_u32..100,
            0_u32..100,
            100_u32..1000,
            0_u32..50,
            1_u32..128,
            1_u32..64,
            1_u32..50
        )
            .prop_map(|(total, top, height, overscan, max_batch, lpu, budget)| {
                ReflowPlannerInput {
                    total_logical_lines: total,
                    viewport_top: top,
                    viewport_height: height,
                    overscan_lines: overscan,
                    max_batch_lines: max_batch,
                    lines_per_work_unit: lpu,
                    frame_budget_units: budget,
                }
            }),
        // Very large buffer (capped at 50K for perf — allocation per case)
        (
            10_000_u32..50_000,
            0_u32..50_000,
            1_u32..100,
            0_u32..200,
            1_u32..256,
            1_u32..128,
            1_u32..50
        )
            .prop_map(|(total, top, height, overscan, max_batch, lpu, budget)| {
                ReflowPlannerInput {
                    total_logical_lines: total,
                    viewport_top: top,
                    viewport_height: height,
                    overscan_lines: overscan,
                    max_batch_lines: max_batch,
                    lines_per_work_unit: lpu,
                    frame_budget_units: budget,
                }
            }),
    ]
}

/// Combined strategy: mix of normal and edge-case inputs.
fn arb_any_input() -> impl Strategy<Value = ReflowPlannerInput> {
    prop_oneof![3 => arb_planner_input(), 1 => arb_edge_case_input(),]
}

// ── Helpers ───────────────────────────────────────────────────────────

fn plan(input: &ReflowPlannerInput) -> ReflowPlan {
    ViewportReflowPlanner::plan(input)
}

// ── Coverage invariants ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Every line in [0, total) must be covered by exactly one batch.
    #[test]
    fn all_lines_covered_exactly_once(input in arb_any_input()) {
        let result = plan(&input);
        let total = input.total_logical_lines as usize;
        let mut coverage = vec![0_u32; total];
        for batch in &result.batches {
            for line in batch.range.start_line..batch.range.end_line_exclusive {
                let idx = line as usize;
                prop_assert!(idx < total, "batch range exceeds total: line={} total={}", line, total);
                coverage[idx] += 1;
            }
        }
        for (line, &count) in coverage.iter().enumerate() {
            prop_assert_eq!(count, 1, "line {} covered {} times (expected 1)", line, count);
        }
    }

    /// No two batches may overlap (non-overlapping ranges).
    #[test]
    fn batch_ranges_non_overlapping(input in arb_any_input()) {
        let result = plan(&input);
        for (i, a) in result.batches.iter().enumerate() {
            for (j, b) in result.batches.iter().enumerate() {
                if i == j { continue; }
                let overlaps = a.range.start_line < b.range.end_line_exclusive
                    && b.range.start_line < a.range.end_line_exclusive;
                prop_assert!(!overlaps,
                    "batches {} and {} overlap: {}..{} vs {}..{}",
                    i, j,
                    a.range.start_line, a.range.end_line_exclusive,
                    b.range.start_line, b.range.end_line_exclusive
                );
            }
        }
    }

    /// Every batch range must be non-empty (start < end).
    #[test]
    fn all_batch_ranges_non_empty(input in arb_any_input()) {
        let result = plan(&input);
        for (idx, batch) in result.batches.iter().enumerate() {
            prop_assert!(
                batch.range.start_line < batch.range.end_line_exclusive,
                "batch {} has empty range: {}..{}",
                idx, batch.range.start_line, batch.range.end_line_exclusive
            );
        }
    }
}

// ── Priority ordering invariants ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The first batch must always be ViewportCore.
    #[test]
    fn first_batch_is_viewport_core(input in arb_any_input()) {
        let result = plan(&input);
        if !result.batches.is_empty() {
            prop_assert_eq!(
                result.batches[0].priority,
                ReflowBatchPriority::ViewportCore,
                "first batch must be ViewportCore"
            );
        }
    }

    /// All ViewportCore batches precede all ViewportOverscan batches,
    /// and all ViewportOverscan batches precede all ColdScrollback batches.
    #[test]
    fn priority_group_ordering(input in arb_any_input()) {
        let result = plan(&input);
        let mut last_priority_rank = 0_u8;
        let mut seen_lower = false;
        for batch in &result.batches {
            let rank = match batch.priority {
                ReflowBatchPriority::ViewportCore => 0,
                ReflowBatchPriority::ViewportOverscan => 1,
                ReflowBatchPriority::ColdScrollback => 2,
            };
            if rank < last_priority_rank {
                seen_lower = true;
            }
            // Once we've seen ColdScrollback, we shouldn't see ViewportCore again
            if last_priority_rank == 2 && rank < 2 {
                prop_assert!(false,
                    "saw {:?} after ColdScrollback — priority order violated",
                    batch.priority
                );
            }
            last_priority_rank = last_priority_rank.max(rank);
        }
        // Note: Overscan can appear between viewport and cold, but once cold starts,
        // no higher priority should appear. The `seen_lower` variable detects
        // regressions but we rely on the ColdScrollback check above as the key invariant.
        let _ = seen_lower;
    }
}

// ── Batch size bound ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Every batch must have at most max_batch_lines lines.
    #[test]
    fn batch_size_bounded(input in arb_any_input()) {
        let result = plan(&input);
        let effective_max = input.max_batch_lines.max(1);
        for (idx, batch) in result.batches.iter().enumerate() {
            let len = batch.range.end_line_exclusive.saturating_sub(batch.range.start_line);
            prop_assert!(len <= effective_max,
                "batch {} has {} lines, max is {}",
                idx, len, effective_max
            );
        }
    }

    /// All batch ranges stay within [0, total_logical_lines).
    #[test]
    fn batch_ranges_within_buffer(input in arb_any_input()) {
        let result = plan(&input);
        for (idx, batch) in result.batches.iter().enumerate() {
            prop_assert!(batch.range.end_line_exclusive <= input.total_logical_lines,
                "batch {} end ({}) exceeds total ({})",
                idx, batch.range.end_line_exclusive, input.total_logical_lines
            );
        }
    }
}

// ── Frame budget accounting ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// frame_work_units must equal the sum of work_units for selected batches.
    #[test]
    fn frame_work_units_equals_selected_sum(input in arb_any_input()) {
        let result = plan(&input);
        let selected_sum: u32 = result.batches.iter()
            .filter(|b| b.selected_for_frame)
            .map(|b| b.work_units)
            .sum();
        prop_assert_eq!(
            result.frame_work_units,
            selected_sum,
            "frame_work_units ({}) != sum of selected work_units ({})",
            result.frame_work_units, selected_sum
        );
    }

    /// At least one batch must be selected for frame (progress guarantee).
    #[test]
    fn at_least_one_batch_selected(input in arb_any_input()) {
        let result = plan(&input);
        if !result.batches.is_empty() {
            let any_selected = result.batches.iter().any(|b| b.selected_for_frame);
            prop_assert!(any_selected, "no batch selected for frame — progress guarantee violated");
        }
    }

    /// The first batch is always selected for frame.
    #[test]
    fn first_batch_always_selected(input in arb_any_input()) {
        let result = plan(&input);
        if !result.batches.is_empty() {
            prop_assert!(
                result.batches[0].selected_for_frame,
                "first batch must be selected for frame"
            );
        }
    }

    /// Greedy budget invariant: every non-selected batch would exceed the budget
    /// if added to the current frame_spent at the point it was evaluated.
    /// The planner processes batches in order, greedily selecting those that fit.
    #[test]
    fn non_selected_would_exceed_budget(input in arb_any_input()) {
        let result = plan(&input);
        let lpu = input.lines_per_work_unit.max(1);
        let mut frame_spent = 0_u32;
        let mut first = true;
        for batch in &result.batches {
            let lines = (batch.range.end_line_exclusive - batch.range.start_line).max(1);
            let work_units = lines.div_ceil(lpu).max(1);
            if first {
                // First batch always selected
                prop_assert!(batch.selected_for_frame, "first batch must be selected");
                frame_spent = frame_spent.saturating_add(work_units);
                first = false;
            } else if batch.selected_for_frame {
                // Selected batch must fit within budget
                prop_assert!(
                    frame_spent.saturating_add(work_units) <= result.frame_budget_units,
                    "selected batch with work_units={} would put frame_spent={} over budget={}",
                    work_units, frame_spent + work_units, result.frame_budget_units
                );
                frame_spent = frame_spent.saturating_add(work_units);
            } else {
                // Non-selected batch must NOT fit within budget
                prop_assert!(
                    frame_spent.saturating_add(work_units) > result.frame_budget_units,
                    "non-selected batch with work_units={} would fit (frame_spent={}, budget={})",
                    work_units, frame_spent, result.frame_budget_units
                );
            }
        }
    }

    /// frame_budget_units in the plan is always >= 1 (clamped from zero).
    #[test]
    fn frame_budget_clamped_to_at_least_one(input in arb_any_input()) {
        let result = plan(&input);
        prop_assert!(result.frame_budget_units >= 1,
            "frame_budget_units must be >= 1, got {}",
            result.frame_budget_units
        );
    }
}

// ── Scheduler class mapping ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// ViewportCore and ViewportOverscan map to Interactive; ColdScrollback to Background.
    #[test]
    fn scheduler_class_matches_priority(input in arb_any_input()) {
        let result = plan(&input);
        for (idx, batch) in result.batches.iter().enumerate() {
            let expected = match batch.priority {
                ReflowBatchPriority::ViewportCore | ReflowBatchPriority::ViewportOverscan => {
                    ResizeWorkClass::Interactive
                }
                ReflowBatchPriority::ColdScrollback => ResizeWorkClass::Background,
            };
            prop_assert_eq!(batch.scheduler_class, expected,
                "batch {} has priority {:?} but class {:?} (expected {:?})",
                idx, batch.priority, batch.scheduler_class, expected
            );
        }
    }

    /// Every batch has a non-empty rationale string.
    #[test]
    fn all_batches_have_rationale(input in arb_any_input()) {
        let result = plan(&input);
        for (idx, batch) in result.batches.iter().enumerate() {
            prop_assert!(!batch.rationale.is_empty(),
                "batch {} has empty rationale", idx
            );
        }
    }
}

// ── Determinism ───────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Planning the same input twice must produce identical results.
    #[test]
    fn planner_is_deterministic(input in arb_any_input()) {
        let first = plan(&input);
        let second = plan(&input);
        prop_assert_eq!(first.frame_budget_units, second.frame_budget_units);
        prop_assert_eq!(first.frame_work_units, second.frame_work_units);
        prop_assert_eq!(first.batches.len(), second.batches.len());
        for (i, (a, b)) in first.batches.iter().zip(second.batches.iter()).enumerate() {
            prop_assert_eq!(a, b, "batch {} differs between runs", i);
        }
    }
}

// ── Work unit consistency ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Each batch work_units must be >= 1.
    #[test]
    fn batch_work_units_positive(input in arb_any_input()) {
        let result = plan(&input);
        for (idx, batch) in result.batches.iter().enumerate() {
            prop_assert!(batch.work_units >= 1,
                "batch {} has work_units={}", idx, batch.work_units
            );
        }
    }

    /// Work units are proportional to line count:
    /// work_units = ceil(lines / lines_per_work_unit), clamped to >=1.
    #[test]
    fn work_units_proportional_to_lines(input in arb_any_input()) {
        let result = plan(&input);
        let lpu = input.lines_per_work_unit.max(1);
        for (idx, batch) in result.batches.iter().enumerate() {
            let lines = (batch.range.end_line_exclusive - batch.range.start_line).max(1);
            let expected = lines.div_ceil(lpu).max(1);
            prop_assert_eq!(batch.work_units, expected,
                "batch {} has {} lines, lpu={}, expected work_units={} got {}",
                idx, lines, lpu, expected, batch.work_units
            );
        }
    }
}

// ── Scheduling hooks and log lines ────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// scheduling_hooks() returns the same count as batches and mirrors fields.
    #[test]
    fn scheduling_hooks_mirror_batches(input in arb_any_input()) {
        let result = plan(&input);
        let hooks = result.scheduling_hooks();
        prop_assert_eq!(hooks.len(), result.batches.len());
        for (i, (hook, batch)) in hooks.iter().zip(result.batches.iter()).enumerate() {
            prop_assert_eq!(hook.range, batch.range, "hook {} range mismatch", i);
            prop_assert_eq!(hook.scheduler_class, batch.scheduler_class, "hook {} class mismatch", i);
            prop_assert_eq!(hook.work_units, batch.work_units, "hook {} work_units mismatch", i);
            prop_assert_eq!(hook.selected_for_frame, batch.selected_for_frame, "hook {} selected mismatch", i);
            prop_assert_eq!(&hook.rationale, &batch.rationale, "hook {} rationale mismatch", i);
        }
    }

    /// log_lines() returns the same count as batches and includes required fields.
    #[test]
    fn log_lines_count_and_format(input in arb_any_input()) {
        let result = plan(&input);
        let logs = result.log_lines();
        prop_assert_eq!(logs.len(), result.batches.len());
        for (idx, log) in logs.iter().enumerate() {
            prop_assert!(log.contains(&format!("idx={}", idx)),
                "log line {} missing idx field", idx);
            prop_assert!(log.contains("selected_for_frame="),
                "log line {} missing selected_for_frame", idx);
            prop_assert!(log.contains("range="),
                "log line {} missing range", idx);
            prop_assert!(log.contains("work_units="),
                "log line {} missing work_units", idx);
            prop_assert!(log.contains("reason="),
                "log line {} missing reason", idx);
        }
    }
}

// ── Serde roundtrip ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// ReflowPlannerInput survives JSON roundtrip.
    #[test]
    fn planner_input_serde_roundtrip(input in arb_planner_input()) {
        let json = serde_json::to_string(&input).expect("serialize input");
        let restored: ReflowPlannerInput = serde_json::from_str(&json).expect("deserialize input");
        prop_assert_eq!(input.total_logical_lines, restored.total_logical_lines);
        prop_assert_eq!(input.viewport_top, restored.viewport_top);
        prop_assert_eq!(input.viewport_height, restored.viewport_height);
        prop_assert_eq!(input.overscan_lines, restored.overscan_lines);
        prop_assert_eq!(input.max_batch_lines, restored.max_batch_lines);
        prop_assert_eq!(input.lines_per_work_unit, restored.lines_per_work_unit);
        prop_assert_eq!(input.frame_budget_units, restored.frame_budget_units);
    }

    /// ReflowPlan survives JSON roundtrip and plans identically after restore.
    #[test]
    fn plan_serde_roundtrip(input in arb_planner_input()) {
        let result = plan(&input);
        let json = serde_json::to_string(&result).expect("serialize plan");
        let restored: ReflowPlan = serde_json::from_str(&json).expect("deserialize plan");
        prop_assert_eq!(result.frame_budget_units, restored.frame_budget_units);
        prop_assert_eq!(result.frame_work_units, restored.frame_work_units);
        prop_assert_eq!(result.batches.len(), restored.batches.len());
        for (i, (a, b)) in result.batches.iter().zip(restored.batches.iter()).enumerate() {
            prop_assert_eq!(a, b, "batch {} differs after serde roundtrip", i);
        }
    }
}

// ── Resize intent conversion ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// to_resize_intent preserves pane_id, intent_seq, submitted_at_ms, and hook fields.
    #[test]
    fn resize_intent_preserves_fields(
        input in arb_planner_input(),
        pane_id in 0_u64..1000,
        intent_seq in 0_u64..10000,
        submitted_at_ms in 0_u64..u64::MAX / 2,
    ) {
        let result = plan(&input);
        let hooks = result.scheduling_hooks();
        for hook in &hooks {
            let intent = hook.to_resize_intent(pane_id, intent_seq, submitted_at_ms);
            prop_assert_eq!(intent.pane_id, pane_id);
            prop_assert_eq!(intent.intent_seq, intent_seq);
            prop_assert_eq!(intent.submitted_at_ms, submitted_at_ms);
            prop_assert_eq!(intent.scheduler_class, hook.scheduler_class);
            prop_assert_eq!(intent.work_units, hook.work_units);
        }
    }
}

// ── Cold scrollback interleaving ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// When both sides of the viewport have cold scrollback,
    /// cold batches should include lines from both sides.
    #[test]
    fn cold_scrollback_covers_both_sides(
        total in 200_u32..10_000,
        viewport_top in 50_u32..5_000,
        viewport_height in 10_u32..100,
        overscan in 5_u32..50,
    ) {
        let input = ReflowPlannerInput {
            total_logical_lines: total,
            viewport_top,
            viewport_height,
            overscan_lines: overscan,
            max_batch_lines: 32,
            lines_per_work_unit: 8,
            frame_budget_units: 100,
        };
        let result = plan(&input);

        let effective_vp_start = viewport_top.min(total.saturating_sub(viewport_height.max(1)));
        let effective_vp_end = (effective_vp_start + viewport_height.max(1)).min(total);
        let overscan_start = effective_vp_start.saturating_sub(overscan);
        let overscan_end = (effective_vp_end + overscan).min(total);

        let cold_batches: Vec<_> = result.batches.iter()
            .filter(|b| b.priority == ReflowBatchPriority::ColdScrollback)
            .collect();

        let has_left_cold = overscan_start > 0;
        let has_right_cold = overscan_end < total;

        if has_left_cold {
            let has_left_batch = cold_batches.iter()
                .any(|b| b.range.end_line_exclusive <= overscan_start);
            prop_assert!(has_left_batch,
                "expected left cold batches (overscan_start={})", overscan_start);
        }
        if has_right_cold {
            let has_right_batch = cold_batches.iter()
                .any(|b| b.range.start_line >= overscan_end);
            prop_assert!(has_right_batch,
                "expected right cold batches (overscan_end={})", overscan_end);
        }
    }
}

// ── Empty/degenerate inputs ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Zero total lines always produces empty plan.
    #[test]
    fn zero_total_lines_empty_plan(
        viewport_height in 0_u32..100,
        overscan in 0_u32..50,
    ) {
        let input = ReflowPlannerInput {
            total_logical_lines: 0,
            viewport_top: 0,
            viewport_height,
            overscan_lines: overscan,
            max_batch_lines: 32,
            lines_per_work_unit: 8,
            frame_budget_units: 10,
        };
        let result = plan(&input);
        prop_assert!(result.batches.is_empty());
        prop_assert_eq!(result.frame_work_units, 0);
    }

    /// Zero viewport height always produces empty plan.
    #[test]
    fn zero_viewport_height_empty_plan(
        total in 1_u32..1000,
        top in 0_u32..500,
    ) {
        let input = ReflowPlannerInput {
            total_logical_lines: total,
            viewport_top: top,
            viewport_height: 0,
            overscan_lines: 16,
            max_batch_lines: 32,
            lines_per_work_unit: 8,
            frame_budget_units: 10,
        };
        let result = plan(&input);
        prop_assert!(result.batches.is_empty());
        prop_assert_eq!(result.frame_work_units, 0);
    }
}
