//! Edge-case and boundary-condition tests for ViewportReflowPlanner.
//!
//! Covers: degenerate inputs, overscan boundaries, viewport extremes,
//! batch coverage completeness, scheduler class mapping, frame budget
//! accounting, hook projection, log format, and u32 overflow safety.

use frankenterm_core::resize_scheduler::ResizeWorkClass;
use frankenterm_core::viewport_reflow_planner::{
    ReflowBatchPriority, ReflowPlannerInput, ViewportReflowPlanner,
};

// ── zero/degenerate input handling ─────────────────────────

#[test]
fn zero_viewport_height_returns_empty_plan() {
    let input = ReflowPlannerInput {
        total_logical_lines: 500,
        viewport_top: 100,
        viewport_height: 0,
        overscan_lines: 16,
        max_batch_lines: 64,
        lines_per_work_unit: 32,
        frame_budget_units: 8,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    assert!(plan.batches.is_empty());
    assert_eq!(plan.frame_work_units, 0);
}

#[test]
fn zero_max_batch_lines_treated_as_one() {
    let input = ReflowPlannerInput {
        total_logical_lines: 10,
        viewport_top: 0,
        viewport_height: 5,
        overscan_lines: 2,
        max_batch_lines: 0,
        lines_per_work_unit: 1,
        frame_budget_units: 100,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    assert!(!plan.batches.is_empty());
    assert!(
        plan.batches.iter().all(|b| b
            .range
            .end_line_exclusive
            .saturating_sub(b.range.start_line)
            == 1),
        "batch sizes should be 1 when max_batch_lines is zero (clamped to 1)"
    );
}

#[test]
fn zero_lines_per_work_unit_treated_as_one() {
    let input = ReflowPlannerInput {
        total_logical_lines: 20,
        viewport_top: 5,
        viewport_height: 10,
        overscan_lines: 0,
        max_batch_lines: 10,
        lines_per_work_unit: 0,
        frame_budget_units: 100,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    assert!(!plan.batches.is_empty());
    for batch in &plan.batches {
        let lines = batch
            .range
            .end_line_exclusive
            .saturating_sub(batch.range.start_line)
            .max(1);
        assert_eq!(batch.work_units, lines);
    }
}

#[test]
fn zero_frame_budget_still_selects_first_batch() {
    let input = ReflowPlannerInput {
        total_logical_lines: 50,
        viewport_top: 10,
        viewport_height: 10,
        overscan_lines: 5,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 0,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    assert!(!plan.batches.is_empty());
    assert!(
        plan.batches[0].selected_for_frame,
        "first batch must always be selected to guarantee visible progress"
    );
    assert_eq!(plan.frame_budget_units, 1);
}

// ── overscan boundary conditions ───────────────────────────

#[test]
fn overscan_larger_than_buffer_produces_no_cold_scrollback() {
    let input = ReflowPlannerInput {
        total_logical_lines: 30,
        viewport_top: 10,
        viewport_height: 10,
        overscan_lines: 100,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 20,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    assert!(
        plan.batches
            .iter()
            .all(|b| b.priority != ReflowBatchPriority::ColdScrollback),
        "no cold scrollback when overscan covers entire buffer"
    );
}

#[test]
fn zero_overscan_yields_only_viewport_and_cold() {
    let input = ReflowPlannerInput {
        total_logical_lines: 100,
        viewport_top: 30,
        viewport_height: 20,
        overscan_lines: 0,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 50,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    assert!(
        plan.batches
            .iter()
            .all(|b| b.priority != ReflowBatchPriority::ViewportOverscan),
        "no overscan batches when overscan_lines is 0"
    );
    assert!(
        plan.batches
            .iter()
            .any(|b| b.priority == ReflowBatchPriority::ViewportCore)
    );
    assert!(
        plan.batches
            .iter()
            .any(|b| b.priority == ReflowBatchPriority::ColdScrollback)
    );
}

// ── viewport at extremes ───────────────────────────────────

#[test]
fn viewport_at_line_zero_has_no_pre_overscan() {
    let input = ReflowPlannerInput {
        total_logical_lines: 200,
        viewport_top: 0,
        viewport_height: 24,
        overscan_lines: 8,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 10,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    let viewport_batch = plan
        .batches
        .iter()
        .find(|b| b.priority == ReflowBatchPriority::ViewportCore)
        .expect("viewport batch should exist");
    assert_eq!(viewport_batch.range.start_line, 0);
}

#[test]
fn viewport_past_buffer_end_clamps_to_last_screen() {
    let input = ReflowPlannerInput {
        total_logical_lines: 200,
        viewport_top: 200,
        viewport_height: 24,
        overscan_lines: 8,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 10,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    let viewport_batch = plan
        .batches
        .iter()
        .find(|b| b.priority == ReflowBatchPriority::ViewportCore)
        .expect("viewport batch should exist");
    assert_eq!(viewport_batch.range.start_line, 176);
    assert_eq!(viewport_batch.range.end_line_exclusive, 200);
}

#[test]
fn single_line_buffer_produces_one_viewport_batch() {
    let input = ReflowPlannerInput {
        total_logical_lines: 1,
        viewport_top: 0,
        viewport_height: 24,
        overscan_lines: 8,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 10,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    assert_eq!(plan.batches.len(), 1);
    assert_eq!(plan.batches[0].priority, ReflowBatchPriority::ViewportCore);
    assert_eq!(plan.batches[0].range.start_line, 0);
    assert_eq!(plan.batches[0].range.end_line_exclusive, 1);
}

#[test]
fn viewport_height_equals_buffer_has_no_cold_scrollback() {
    let input = ReflowPlannerInput {
        total_logical_lines: 24,
        viewport_top: 0,
        viewport_height: 24,
        overscan_lines: 8,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 10,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    assert!(
        plan.batches
            .iter()
            .all(|b| b.priority != ReflowBatchPriority::ColdScrollback)
    );
}

#[test]
fn viewport_height_exceeds_buffer_covers_all_as_viewport() {
    let input = ReflowPlannerInput {
        total_logical_lines: 10,
        viewport_top: 0,
        viewport_height: 100,
        overscan_lines: 5,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 10,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    assert!(!plan.batches.is_empty());
    assert!(
        plan.batches
            .iter()
            .all(|b| b.priority == ReflowBatchPriority::ViewportCore)
    );
    assert_eq!(plan.batches[0].range.start_line, 0);
    assert_eq!(plan.batches[0].range.end_line_exclusive, 10);
}

// ── batch coverage completeness ────────────────────────────

#[test]
fn all_lines_covered_exactly_once() {
    let input = ReflowPlannerInput {
        total_logical_lines: 500,
        viewport_top: 200,
        viewport_height: 40,
        overscan_lines: 20,
        max_batch_lines: 16,
        lines_per_work_unit: 8,
        frame_budget_units: 100,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    let mut coverage = vec![false; 500];
    for batch in &plan.batches {
        for line in batch.range.start_line..batch.range.end_line_exclusive {
            assert!(
                !coverage[line as usize],
                "line {line} covered by multiple batches"
            );
            coverage[line as usize] = true;
        }
    }
    assert!(
        coverage.iter().all(|&c| c),
        "all lines must be covered by exactly one batch"
    );
}

#[test]
fn batch_ranges_are_non_empty() {
    let input = ReflowPlannerInput {
        total_logical_lines: 300,
        viewport_top: 100,
        viewport_height: 30,
        overscan_lines: 15,
        max_batch_lines: 8,
        lines_per_work_unit: 4,
        frame_budget_units: 20,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    for batch in &plan.batches {
        assert!(
            batch.range.start_line < batch.range.end_line_exclusive,
            "batch range must be non-empty: {}..{}",
            batch.range.start_line,
            batch.range.end_line_exclusive
        );
    }
}

// ── scheduler class mapping ────────────────────────────────

#[test]
fn viewport_and_overscan_are_interactive_cold_is_background() {
    let input = ReflowPlannerInput {
        total_logical_lines: 100,
        viewport_top: 30,
        viewport_height: 20,
        overscan_lines: 10,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 50,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    for batch in &plan.batches {
        match batch.priority {
            ReflowBatchPriority::ViewportCore | ReflowBatchPriority::ViewportOverscan => {
                assert_eq!(batch.scheduler_class, ResizeWorkClass::Interactive);
            }
            ReflowBatchPriority::ColdScrollback => {
                assert_eq!(batch.scheduler_class, ResizeWorkClass::Background);
            }
        }
    }
}

// ── frame budget accounting ─────────────────────────────────

#[test]
fn frame_work_units_equals_sum_of_selected_batch_work_units() {
    let input = ReflowPlannerInput {
        total_logical_lines: 500,
        viewport_top: 200,
        viewport_height: 40,
        overscan_lines: 20,
        max_batch_lines: 16,
        lines_per_work_unit: 8,
        frame_budget_units: 6,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    let selected_sum: u32 = plan
        .batches
        .iter()
        .filter(|b| b.selected_for_frame)
        .map(|b| b.work_units)
        .sum();
    assert_eq!(plan.frame_work_units, selected_sum);
}

#[test]
fn tight_budget_selects_minimum_batches() {
    let input = ReflowPlannerInput {
        total_logical_lines: 1000,
        viewport_top: 400,
        viewport_height: 50,
        overscan_lines: 25,
        max_batch_lines: 64,
        lines_per_work_unit: 32,
        frame_budget_units: 1,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    let selected_count = plan.batches.iter().filter(|b| b.selected_for_frame).count();
    assert!(selected_count >= 1, "at least one batch must be selected");
    assert!(
        selected_count <= 2,
        "tight budget should select very few batches, got {selected_count}"
    );
}

// ── scheduling hook projection ─────────────────────────────

#[test]
fn scheduling_hooks_preserve_batch_count_and_order() {
    let input = ReflowPlannerInput {
        total_logical_lines: 200,
        viewport_top: 50,
        viewport_height: 30,
        overscan_lines: 10,
        max_batch_lines: 16,
        lines_per_work_unit: 8,
        frame_budget_units: 10,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    let hooks = plan.scheduling_hooks();
    assert_eq!(hooks.len(), plan.batches.len());
    for (hook, batch) in hooks.iter().zip(plan.batches.iter()) {
        assert_eq!(hook.range, batch.range);
        assert_eq!(hook.scheduler_class, batch.scheduler_class);
        assert_eq!(hook.work_units, batch.work_units);
        assert_eq!(hook.selected_for_frame, batch.selected_for_frame);
        assert_eq!(hook.rationale, batch.rationale);
    }
}

#[test]
fn scheduling_hook_to_resize_intent_roundtrip() {
    let input = ReflowPlannerInput {
        total_logical_lines: 100,
        viewport_top: 20,
        viewport_height: 20,
        overscan_lines: 5,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 10,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    let hooks = plan.scheduling_hooks();
    let hook = &hooks[0];
    let intent = hook.to_resize_intent(42, 7, 1000);
    assert_eq!(intent.pane_id, 42);
    assert_eq!(intent.intent_seq, 7);
    assert_eq!(intent.submitted_at_ms, 1000);
    assert_eq!(intent.scheduler_class, hook.scheduler_class);
    assert_eq!(intent.work_units, hook.work_units);
}

// ── log_lines format ───────────────────────────────────────

#[test]
fn log_lines_include_all_required_fields() {
    let input = ReflowPlannerInput {
        total_logical_lines: 100,
        viewport_top: 30,
        viewport_height: 20,
        overscan_lines: 10,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 10,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    let logs = plan.log_lines();
    assert_eq!(logs.len(), plan.batches.len());
    for (idx, log) in logs.iter().enumerate() {
        assert!(log.contains(&format!("idx={idx}")));
        assert!(log.contains("selected_for_frame="));
        assert!(log.contains("class="));
        assert!(log.contains("priority="));
        assert!(log.contains("range="));
        assert!(log.contains("lines="));
        assert!(log.contains("work_units="));
        assert!(log.contains("reason="));
    }
}

// ── u32 boundary conditions ────────────────────────────────

#[test]
fn near_u32_max_total_lines_does_not_overflow() {
    let input = ReflowPlannerInput {
        total_logical_lines: u32::MAX,
        viewport_top: u32::MAX - 100,
        viewport_height: 50,
        overscan_lines: 20,
        max_batch_lines: 64,
        lines_per_work_unit: 32,
        frame_budget_units: 10,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    assert!(!plan.batches.is_empty());
    for batch in &plan.batches {
        let len = batch
            .range
            .end_line_exclusive
            .saturating_sub(batch.range.start_line);
        assert!(len <= 64);
    }
}

// ── cold scrollback interleaving ───────────────────────────

#[test]
fn cold_scrollback_interleaves_left_and_right() {
    let input = ReflowPlannerInput {
        total_logical_lines: 500,
        viewport_top: 200,
        viewport_height: 20,
        overscan_lines: 10,
        max_batch_lines: 16,
        lines_per_work_unit: 8,
        frame_budget_units: 100,
    };
    let plan = ViewportReflowPlanner::plan(&input);
    let cold_batches: Vec<_> = plan
        .batches
        .iter()
        .filter(|b| b.priority == ReflowBatchPriority::ColdScrollback)
        .collect();
    assert!(cold_batches.len() >= 2);
    let overscan_start = 190; // 200 - 10
    let overscan_end = 230; // 220 + 10
    let has_left = cold_batches
        .iter()
        .any(|b| b.range.end_line_exclusive <= overscan_start);
    let has_right = cold_batches
        .iter()
        .any(|b| b.range.start_line >= overscan_end);
    assert!(
        has_left && has_right,
        "cold scrollback should have batches on both sides of viewport"
    );
}

// ── viewport-first ordering guarantee ──────────────────────

#[test]
fn first_batch_is_always_viewport_core() {
    let cases = vec![
        (100u32, 50u32, 20u32, 10u32),
        (1000, 0, 24, 8),
        (1000, 976, 24, 8),
        (5, 0, 5, 2),
    ];
    for (total, top, height, overscan) in cases {
        let input = ReflowPlannerInput {
            total_logical_lines: total,
            viewport_top: top,
            viewport_height: height,
            overscan_lines: overscan,
            max_batch_lines: 32,
            lines_per_work_unit: 8,
            frame_budget_units: 10,
        };
        let plan = ViewportReflowPlanner::plan(&input);
        assert_eq!(
            plan.batches[0].priority,
            ReflowBatchPriority::ViewportCore,
            "first batch must be ViewportCore for total={total} top={top}"
        );
    }
}

// ── determinism across varied inputs ───────────────────────

#[test]
fn deterministic_across_multiple_plan_calls() {
    let inputs = vec![
        ReflowPlannerInput {
            total_logical_lines: 50,
            viewport_top: 0,
            viewport_height: 24,
            overscan_lines: 4,
            max_batch_lines: 8,
            lines_per_work_unit: 4,
            frame_budget_units: 3,
        },
        ReflowPlannerInput {
            total_logical_lines: 10000,
            viewport_top: 5000,
            viewport_height: 80,
            overscan_lines: 40,
            max_batch_lines: 64,
            lines_per_work_unit: 16,
            frame_budget_units: 12,
        },
    ];
    for input in &inputs {
        let a = ViewportReflowPlanner::plan(input);
        let b = ViewportReflowPlanner::plan(input);
        assert_eq!(
            a, b,
            "plan must be deterministic for identical input (total_lines={})",
            input.total_logical_lines
        );
    }
}
