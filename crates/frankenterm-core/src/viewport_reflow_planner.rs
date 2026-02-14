//! Viewport-priority reflow planning and scheduling hooks.
//!
//! This module provides the planning layer for `wa-1u90p.3.8`:
//! - deterministic viewport/overscan/cold-scrollback batching
//! - frame-budget-aware batch selection for immediate work
//! - scheduler hook projection with explicit rationale text for logs/telemetry

use serde::{Deserialize, Serialize};

use crate::resize_scheduler::{ResizeIntent, ResizeWorkClass};

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
        let mut batches = Vec::with_capacity(ordered_ranges.len());
        for (range, priority) in ordered_ranges {
            let lines = range.len().max(1);
            let work_units = div_ceil(lines, lines_per_work_unit).max(1);
            let selected_for_frame = if !any_selected {
                // Always emit at least one selected batch to guarantee visible progress.
                any_selected = true;
                frame_spent = frame_spent.saturating_add(work_units);
                true
            } else if frame_spent.saturating_add(work_units) <= frame_budget_units {
                frame_spent = frame_spent.saturating_add(work_units);
                true
            } else {
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
}
