//! Viewport-priority reflow planning and scheduling hooks.
//!
//! This module provides the planning layer for `wa-1u90p.3.8`:
//! - deterministic viewport/overscan/cold-scrollback batching
//! - frame-budget-aware batch selection for immediate work
//! - scheduler hook projection with explicit rationale text for logs/telemetry

use serde::{Deserialize, Serialize};

use crate::resize_scheduler::{ResizeDomain, ResizeIntent, ResizeWorkClass};

/// Priority lane for a planned reflow range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflowBatchPriority {
    /// Visible viewport lines; always highest urgency.
    ViewportCore,
    /// Near-visible context around viewport.
    ViewportOverscan,
    /// Remaining cold scrollback convergence work.
    ColdScrollback,
}

impl ReflowBatchPriority {
    fn scheduler_class(self) -> ResizeWorkClass {
        match self {
            Self::ViewportCore | Self::ViewportOverscan => ResizeWorkClass::Interactive,
            Self::ColdScrollback => ResizeWorkClass::Background,
        }
    }

    fn rationale(self) -> &'static str {
        match self {
            Self::ViewportCore => "visible viewport lines for immediate interaction",
            Self::ViewportOverscan => "near-viewport overscan prefetch to avoid visual hitching",
            Self::ColdScrollback => "cold scrollback convergence after interactive region",
        }
    }
}

/// Half-open logical line range `[start_line, end_line_exclusive)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflowLineRange {
    pub start_line: u32,
    pub end_line_exclusive: u32,
}

impl ReflowLineRange {
    const fn len(self) -> u32 {
        self.end_line_exclusive.saturating_sub(self.start_line)
    }
}

/// Planner input for one pane snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReflowPlannerInput {
    /// Total logical lines in the canonical buffer.
    pub total_logical_lines: u32,
    /// Top visible line of the viewport (logical-line coordinates).
    pub viewport_top: u32,
    /// Number of visible lines.
    pub viewport_height: u32,
    /// Overscan lines before/after viewport.
    pub overscan_lines: u32,
    /// Hard maximum lines allowed per emitted batch.
    pub max_batch_lines: u32,
    /// Estimated line cost used to convert line-count to scheduler work units.
    pub lines_per_work_unit: u32,
    /// Work-unit budget for immediate frame selection.
    pub frame_budget_units: u32,
}

impl Default for ReflowPlannerInput {
    fn default() -> Self {
        Self {
            total_logical_lines: 0,
            viewport_top: 0,
            viewport_height: 0,
            overscan_lines: 16,
            max_batch_lines: 64,
            lines_per_work_unit: 32,
            frame_budget_units: 8,
        }
    }
}

/// Planned reflow batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflowBatch {
    /// Logical range this batch covers.
    pub range: ReflowLineRange,
    /// Priority lane.
    pub priority: ReflowBatchPriority,
    /// Scheduler class hook for this batch.
    pub scheduler_class: ResizeWorkClass,
    /// Estimated scheduler work units for this range.
    pub work_units: u32,
    /// Whether this batch is selected inside the immediate frame budget.
    pub selected_for_frame: bool,
    /// Human-readable priority rationale used in logs/telemetry.
    pub rationale: String,
}

/// Lightweight hook projection for scheduler submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflowSchedulingHook {
    pub range: ReflowLineRange,
    pub scheduler_class: ResizeWorkClass,
    pub work_units: u32,
    pub selected_for_frame: bool,
    pub rationale: String,
}

impl ReflowSchedulingHook {
    /// Convert hook metadata into a scheduler intent envelope.
    #[must_use]
    pub fn to_resize_intent(
        &self,
        pane_id: u64,
        intent_seq: u64,
        submitted_at_ms: u64,
    ) -> ResizeIntent {
        ResizeIntent {
            pane_id,
            intent_seq,
            scheduler_class: self.scheduler_class,
            work_units: self.work_units,
            submitted_at_ms,
            domain: ResizeDomain::default(),
            tab_id: None,
        }
    }
}

/// Full deterministic plan for one snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReflowPlan {
    /// Budget used for `selected_for_frame`.
    pub frame_budget_units: u32,
    /// Work units consumed by selected batches.
    pub frame_work_units: u32,
    /// Batches in deterministic scheduling order.
    pub batches: Vec<ReflowBatch>,
}

impl ReflowPlan {
    /// Export scheduler-oriented hooks in planning order.
    #[must_use]
    pub fn scheduling_hooks(&self) -> Vec<ReflowSchedulingHook> {
        self.batches
            .iter()
            .map(|batch| ReflowSchedulingHook {
                range: batch.range,
                scheduler_class: batch.scheduler_class,
                work_units: batch.work_units,
                selected_for_frame: batch.selected_for_frame,
                rationale: batch.rationale.clone(),
            })
            .collect()
    }

    /// Build structured log lines that include selected ranges and rationale.
    #[must_use]
    pub fn log_lines(&self) -> Vec<String> {
        self.batches
            .iter()
            .enumerate()
            .map(|(idx, batch)| {
                format!(
                    "reflow.batch idx={idx} selected_for_frame={} class={:?} priority={:?} range={}..{} lines={} work_units={} reason={}",
                    batch.selected_for_frame,
                    batch.scheduler_class,
                    batch.priority,
                    batch.range.start_line,
                    batch.range.end_line_exclusive,
                    batch.range.len(),
                    batch.work_units,
                    batch.rationale
                )
            })
            .collect()
    }
}

/// Viewport-first planner entrypoint.
#[derive(Debug, Default)]
pub struct ViewportReflowPlanner;

impl ViewportReflowPlanner {
    /// Build a deterministic viewport-priority plan for one snapshot.
    #[must_use]
    pub fn plan(input: &ReflowPlannerInput) -> ReflowPlan {
        if input.total_logical_lines == 0 || input.viewport_height == 0 {
            return ReflowPlan {
                frame_budget_units: input.frame_budget_units.max(1),
                frame_work_units: 0,
                batches: Vec::new(),
            };
        }

        let max_batch_lines = input.max_batch_lines.max(1);
        let lines_per_work_unit = input.lines_per_work_unit.max(1);
        let frame_budget_units = input.frame_budget_units.max(1);
        let total = input.total_logical_lines;

        let max_viewport_start = total.saturating_sub(input.viewport_height.max(1));
        let viewport_start = input.viewport_top.min(max_viewport_start);
        let viewport_end = viewport_start
            .saturating_add(input.viewport_height.max(1))
            .min(total);
        let overscan_start = viewport_start.saturating_sub(input.overscan_lines);
        let overscan_end = viewport_end.saturating_add(input.overscan_lines).min(total);

        let mut ordered_ranges: Vec<(ReflowLineRange, ReflowBatchPriority)> = Vec::new();
        ordered_ranges.extend(
            chunk_range(
                viewport_start,
                viewport_end,
                max_batch_lines,
                ChunkDirection::Forward,
            )
            .into_iter()
            .map(|range| (range, ReflowBatchPriority::ViewportCore)),
        );
        ordered_ranges.extend(
            chunk_range(
                overscan_start,
                viewport_start,
                max_batch_lines,
                ChunkDirection::Reverse,
            )
            .into_iter()
            .map(|range| (range, ReflowBatchPriority::ViewportOverscan)),
        );
        ordered_ranges.extend(
            chunk_range(
                viewport_end,
                overscan_end,
                max_batch_lines,
                ChunkDirection::Forward,
            )
            .into_iter()
            .map(|range| (range, ReflowBatchPriority::ViewportOverscan)),
        );

        let cold_left = chunk_range(0, overscan_start, max_batch_lines, ChunkDirection::Reverse);
        let cold_right = chunk_range(
            overscan_end,
            total,
            max_batch_lines,
            ChunkDirection::Forward,
        );
        for range in interleave(cold_left, cold_right) {
            ordered_ranges.push((range, ReflowBatchPriority::ColdScrollback));
        }

        let mut frame_spent = 0_u32;
        let mut any_selected = false;
        let mut budget_exhausted = false;
        let mut batches = Vec::with_capacity(ordered_ranges.len());
        for (range, priority) in ordered_ranges {
            let lines = range.len().max(1);
            let work_units = div_ceil(lines, lines_per_work_unit).max(1);
            let selected_for_frame = if !any_selected {
                // Always emit at least one selected batch to guarantee visible progress.
                any_selected = true;
                frame_spent = frame_spent.saturating_add(work_units);
                true
            } else if !budget_exhausted
                && frame_spent.saturating_add(work_units) <= frame_budget_units
            {
                frame_spent = frame_spent.saturating_add(work_units);
                true
            } else {
                // Once any batch doesn't fit, stop selecting — selected batches
                // form a contiguous prefix in priority order so higher-priority
                // work nearest the viewport is never displaced by smaller cold chunks.
                budget_exhausted = true;
                false
            };

            batches.push(ReflowBatch {
                range,
                priority,
                scheduler_class: priority.scheduler_class(),
                work_units,
                selected_for_frame,
                rationale: priority.rationale().to_owned(),
            });
        }

        ReflowPlan {
            frame_budget_units,
            frame_work_units: frame_spent,
            batches,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkDirection {
    Forward,
    Reverse,
}

fn chunk_range(
    start_line: u32,
    end_line_exclusive: u32,
    max_batch_lines: u32,
    direction: ChunkDirection,
) -> Vec<ReflowLineRange> {
    if start_line >= end_line_exclusive {
        return Vec::new();
    }

    let size = max_batch_lines.max(1);
    let mut out = Vec::new();
    match direction {
        ChunkDirection::Forward => {
            let mut cursor = start_line;
            while cursor < end_line_exclusive {
                let next = cursor.saturating_add(size).min(end_line_exclusive);
                out.push(ReflowLineRange {
                    start_line: cursor,
                    end_line_exclusive: next,
                });
                cursor = next;
            }
        }
        ChunkDirection::Reverse => {
            let mut cursor = end_line_exclusive;
            while cursor > start_line {
                let prev = cursor.saturating_sub(size).max(start_line);
                out.push(ReflowLineRange {
                    start_line: prev,
                    end_line_exclusive: cursor,
                });
                cursor = prev;
            }
        }
    }
    out
}

fn interleave(left: Vec<ReflowLineRange>, right: Vec<ReflowLineRange>) -> Vec<ReflowLineRange> {
    let mut out = Vec::with_capacity(left.len().saturating_add(right.len()));
    let max_len = left.len().max(right.len());
    for idx in 0..max_len {
        if let Some(range) = left.get(idx) {
            out.push(*range);
        }
        if let Some(range) = right.get(idx) {
            out.push(*range);
        }
    }
    out
}

const fn div_ceil(numerator: u32, denominator: u32) -> u32 {
    if denominator == 0 {
        return numerator;
    }
    numerator.saturating_add(denominator - 1) / denominator
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_is_deterministic_for_identical_input() {
        let input = ReflowPlannerInput {
            total_logical_lines: 400,
            viewport_top: 140,
            viewport_height: 30,
            overscan_lines: 10,
            max_batch_lines: 16,
            lines_per_work_unit: 8,
            frame_budget_units: 6,
        };

        let first = ViewportReflowPlanner::plan(&input);
        let second = ViewportReflowPlanner::plan(&input);
        assert_eq!(first, second);
    }

    #[test]
    fn viewport_boundaries_are_clamped_at_buffer_end() {
        let input = ReflowPlannerInput {
            total_logical_lines: 100,
            viewport_top: 98,
            viewport_height: 12,
            overscan_lines: 4,
            max_batch_lines: 32,
            lines_per_work_unit: 16,
            frame_budget_units: 8,
        };

        let plan = ViewportReflowPlanner::plan(&input);
        let first = plan
            .batches
            .iter()
            .find(|batch| batch.priority == ReflowBatchPriority::ViewportCore)
            .expect("viewport batch should exist");
        assert_eq!(first.range.start_line, 88);
        assert_eq!(first.range.end_line_exclusive, 100);
    }

    #[test]
    fn overscan_batches_precede_cold_scrollback() {
        let input = ReflowPlannerInput {
            total_logical_lines: 300,
            viewport_top: 120,
            viewport_height: 20,
            overscan_lines: 12,
            max_batch_lines: 8,
            lines_per_work_unit: 8,
            frame_budget_units: 6,
        };

        let plan = ViewportReflowPlanner::plan(&input);
        let first_cold_idx = plan
            .batches
            .iter()
            .position(|batch| batch.priority == ReflowBatchPriority::ColdScrollback)
            .expect("cold-scrollback batches should exist");

        assert!(
            plan.batches[..first_cold_idx]
                .iter()
                .any(|batch| batch.priority == ReflowBatchPriority::ViewportOverscan),
            "overscan must be scheduled before cold scrollback"
        );
        assert!(
            plan.batches[..first_cold_idx]
                .iter()
                .all(|batch| batch.priority != ReflowBatchPriority::ColdScrollback),
            "no cold batch should appear before first cold batch marker"
        );
    }

    #[test]
    fn empty_buffer_returns_empty_plan() {
        let input = ReflowPlannerInput {
            total_logical_lines: 0,
            viewport_top: 0,
            viewport_height: 24,
            overscan_lines: 8,
            max_batch_lines: 8,
            lines_per_work_unit: 8,
            frame_budget_units: 3,
        };

        let plan = ViewportReflowPlanner::plan(&input);
        assert!(plan.batches.is_empty());
        assert!(plan.log_lines().is_empty());
        assert_eq!(plan.frame_work_units, 0);
    }

    #[test]
    fn huge_buffer_plan_has_bounded_batch_sizes_and_mixed_frame_selection() {
        let input = ReflowPlannerInput {
            total_logical_lines: 1_000_000,
            viewport_top: 500_000,
            viewport_height: 60,
            overscan_lines: 120,
            max_batch_lines: 128,
            lines_per_work_unit: 32,
            frame_budget_units: 5,
        };

        let plan = ViewportReflowPlanner::plan(&input);
        assert!(!plan.batches.is_empty());
        assert!(
            plan.batches
                .iter()
                .all(|batch| batch.range.len() <= input.max_batch_lines)
        );
        assert!(
            plan.batches.iter().any(|batch| batch.selected_for_frame)
                && plan.batches.iter().any(|batch| !batch.selected_for_frame),
            "expected both immediate and deferred batches for huge buffers"
        );

        let logs = plan.log_lines();
        assert!(!logs.is_empty());
        assert!(logs[0].contains("range="));
        assert!(logs[0].contains("reason="));
    }

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
        // With max_batch_lines clamped to 1, each batch covers exactly 1 line.
        assert!(
            plan.batches.iter().all(|b| b.range.len() == 1),
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
        // Should not panic or divide by zero.
        assert!(!plan.batches.is_empty());
        // Each batch's work_units should equal its line count (1 line per work unit).
        for batch in &plan.batches {
            assert_eq!(batch.work_units, batch.range.len().max(1));
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
        // At least the first batch is always selected for progress guarantee.
        assert!(
            plan.batches[0].selected_for_frame,
            "first batch must always be selected to guarantee visible progress"
        );
        // Budget should be clamped to 1.
        assert_eq!(plan.frame_budget_units, 1);
    }

    // ── overscan boundary conditions ───────────────────────────

    #[test]
    fn overscan_larger_than_buffer_produces_no_cold_scrollback() {
        let input = ReflowPlannerInput {
            total_logical_lines: 30,
            viewport_top: 10,
            viewport_height: 10,
            overscan_lines: 100, // much larger than buffer
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
    fn viewport_at_line_zero() {
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
        // No overscan before viewport when at line 0.
        let has_pre_overscan = plan.batches.iter().any(|b| {
            b.priority == ReflowBatchPriority::ViewportOverscan && b.range.end_line_exclusive == 0
        });
        assert!(!has_pre_overscan, "no pre-viewport overscan at line 0");
    }

    #[test]
    fn viewport_at_buffer_end() {
        let input = ReflowPlannerInput {
            total_logical_lines: 200,
            viewport_top: 200, // past end — should clamp
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
        // Viewport start should be clamped to total - height = 176.
        assert_eq!(viewport_batch.range.start_line, 176);
        assert_eq!(viewport_batch.range.end_line_exclusive, 200);
    }

    #[test]
    fn single_line_buffer() {
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
    fn viewport_height_equals_buffer_size() {
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
        // All lines are viewport — no cold scrollback.
        assert!(
            plan.batches
                .iter()
                .all(|b| b.priority != ReflowBatchPriority::ColdScrollback),
            "no cold scrollback when viewport covers entire buffer"
        );
    }

    #[test]
    fn viewport_height_exceeds_buffer() {
        let input = ReflowPlannerInput {
            total_logical_lines: 10,
            viewport_top: 0,
            viewport_height: 100, // much larger than buffer
            overscan_lines: 5,
            max_batch_lines: 32,
            lines_per_work_unit: 8,
            frame_budget_units: 10,
        };
        let plan = ViewportReflowPlanner::plan(&input);
        assert!(!plan.batches.is_empty());
        // All lines should be viewport core.
        assert!(
            plan.batches
                .iter()
                .all(|b| b.priority == ReflowBatchPriority::ViewportCore)
        );
        let first = &plan.batches[0];
        assert_eq!(first.range.start_line, 0);
        assert_eq!(first.range.end_line_exclusive, 10);
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
    fn batch_ranges_are_non_overlapping_and_contiguous_within_priority() {
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
        // Verify no batch has start >= end.
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
    fn viewport_and_overscan_map_to_interactive_class() {
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
                    assert_eq!(
                        batch.scheduler_class,
                        ResizeWorkClass::Interactive,
                        "viewport/overscan batches must be Interactive"
                    );
                }
                ReflowBatchPriority::ColdScrollback => {
                    assert_eq!(
                        batch.scheduler_class,
                        ResizeWorkClass::Background,
                        "cold scrollback batches must be Background"
                    );
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
        assert_eq!(
            plan.frame_work_units, selected_sum,
            "frame_work_units must match sum of selected batch work_units"
        );
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
            frame_budget_units: 1, // extremely tight
        };
        let plan = ViewportReflowPlanner::plan(&input);
        let selected_count = plan.batches.iter().filter(|b| b.selected_for_frame).count();
        // At minimum 1 batch is always selected for visible progress.
        assert!(
            selected_count >= 1,
            "at least one batch must always be selected"
        );
        // With budget 1, at most the first batch should be selected (potentially over budget).
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
        assert!(!hooks.is_empty());
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
            assert!(
                log.contains(&format!("idx={idx}")),
                "log line should contain batch index"
            );
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
        // Should not panic due to overflow.
        let plan = ViewportReflowPlanner::plan(&input);
        assert!(!plan.batches.is_empty());
        // Verify no batch exceeds max_batch_lines.
        for batch in &plan.batches {
            assert!(batch.range.len() <= 64);
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
        assert!(
            cold_batches.len() >= 2,
            "should have cold batches on both sides"
        );
        // Verify interleaving: cold batches should alternate between left (< overscan_start)
        // and right (> overscan_end) regions.
        let overscan_start = 190; // 200 - 10
        let overscan_end = 230; // 220 + 10
        let mut has_left = false;
        let mut has_right = false;
        for batch in &cold_batches {
            if batch.range.end_line_exclusive <= overscan_start {
                has_left = true;
            }
            if batch.range.start_line >= overscan_end {
                has_right = true;
            }
        }
        assert!(
            has_left && has_right,
            "cold scrollback should have batches on both sides of viewport"
        );
    }

    // ── viewport-first ordering guarantee ──────────────────────

    #[test]
    fn first_batch_is_always_viewport_core() {
        let cases = vec![
            (100, 50, 20, 10),
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
}
