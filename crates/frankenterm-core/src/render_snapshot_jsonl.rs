//! Per-frame JSONL emitter for paint-frame traces
//! (br-ft-ept8r sub-task 4 of ft-q6x91).
//!
//! The bead's sub-task 4 calls for "JSON-line per-frame logs at
//! `tests/render_snapshot/logs/<scenario>.jsonl`". This module
//! ships the emitter + parser; the file path + scenario naming
//! are the integration's responsibility.
//!
//! Cross-references:
//! - [`crate::render_snapshot_guard::RenderFrameTiming`] —
//!   per-frame timing the integration captures at paint
//!   boundaries.
//! - [`crate::render_snapshot_guard::classify_lock_wait`] — the
//!   green/yellow/red classification each row carries.
//! - parent: ft-ept8r (paint.rs rewrite + CallGraph populator
//!   + CI gate + e2e heavy-burst + visual regression).

use serde::{Deserialize, Serialize};

use crate::render_snapshot_guard::{LockWaitClassification, RenderFrameTiming, classify_lock_wait};

/// JSONL row schema for one paint-frame trace.
///
/// The schema is intentionally stable: the CI lane's
/// structured-log sink ingests these for cross-build trend
/// analysis (lock-wait p99 regression tracking). Field changes
/// are a breaking schema change for the sink.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderFrameJsonRow {
    /// Operator-supplied scenario tag (`heavy_burst`,
    /// `idle_baseline`, `snap_back`, etc.). Lets the
    /// trend-analysis sink group rows by scenario across
    /// builds.
    pub scenario: String,
    /// 0-based frame index within the scenario.
    pub frame_index: u32,
    /// Wall-clock timestamp from
    /// `RenderFrameTiming::ts_ns`.
    pub ts_ns: u64,
    /// Snapshot-acquire latency in nanoseconds.
    pub acquire_ns: u64,
    /// Snapshot hold duration in nanoseconds.
    pub hold_ns: u64,
    /// Per-frame dirty line count (same field as the bead's
    /// per-frame `dirty_lines_observed`).
    pub dirty_lines_observed: u32,
    /// Stable label for the lock-wait classification (Green /
    /// Yellow / Red per
    /// [`classify_lock_wait`](crate::render_snapshot_guard::classify_lock_wait)).
    /// Lets the sink histogram by tier without re-deriving from
    /// the raw ns. Owned `String` so the row deserializes from
    /// a borrowed JSON line without lifetime games.
    pub classification: String,
}

impl RenderFrameJsonRow {
    /// Build a row from a single
    /// [`RenderFrameTiming`] + scenario tag + frame index.
    /// Pure function so the integration can emit rows
    /// incrementally as paint frames complete.
    #[must_use]
    pub fn from_timing(
        scenario: impl Into<String>,
        frame_index: u32,
        timing: &RenderFrameTiming,
    ) -> Self {
        Self {
            scenario: scenario.into(),
            frame_index,
            ts_ns: timing.ts_ns,
            acquire_ns: timing.acquire_ns,
            hold_ns: timing.hold_ns,
            dirty_lines_observed: timing.dirty_lines_observed,
            classification: classification_label(classify_lock_wait(timing.acquire_ns)).to_string(),
        }
    }

    /// Stable string label for the
    /// [`LockWaitClassification`]. Surfaced as a free helper so
    /// callers can map a classification independently of a row.
    #[must_use]
    pub const fn classification_label(c: LockWaitClassification) -> &'static str {
        classification_label(c)
    }
}

#[must_use]
const fn classification_label(c: LockWaitClassification) -> &'static str {
    match c {
        LockWaitClassification::Green => "green",
        LockWaitClassification::Yellow => "yellow",
        LockWaitClassification::Red => "red",
    }
}

/// Render a slice of per-frame timings as multi-line JSONL.
/// One JSON object per line, terminated with `\n`. The
/// integration writes the result to
/// `tests/render_snapshot/logs/<scenario>.jsonl`.
///
/// Empty input returns the empty string (no trailing newline)
/// so the CI lane's "scenario produced zero frames" check is
/// just `output.is_empty()`.
#[must_use]
pub fn render_frame_trace_to_jsonl(scenario: &str, timings: &[RenderFrameTiming]) -> String {
    let mut out = String::new();
    for (idx, timing) in timings.iter().enumerate() {
        let row = RenderFrameJsonRow::from_timing(scenario, idx as u32, timing);
        out.push_str(&serde_json::to_string(&row).expect("RenderFrameJsonRow serializes"));
        out.push('\n');
    }
    out
}

/// Parse a JSONL trace back into rows. Used by the cross-build
/// trend-analysis sink to ingest historical scenario logs.
///
/// Empty / blank lines are skipped (same convention as the
/// other `parse_*_jsonl` helpers in this crate).
pub fn parse_render_frame_trace(jsonl: &str) -> Result<Vec<RenderFrameJsonRow>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing(ts_ns: u64, acquire_ns: u64, hold_ns: u64, dirty: u32) -> RenderFrameTiming {
        RenderFrameTiming {
            ts_ns,
            acquire_ns,
            hold_ns,
            dirty_lines_observed: dirty,
        }
    }

    /// br-ft-ept8r: empty input → empty string. The CI lane's
    /// "scenario produced zero frames" check relies on this.
    #[test]
    fn empty_input_returns_empty_string() {
        let out = render_frame_trace_to_jsonl("idle_baseline", &[]);
        assert_eq!(out, "");
    }

    /// br-ft-ept8r: single frame → single JSON line + trailing
    /// `\n`. Each row carries the scenario tag, frame_index = 0,
    /// and the classification label derived from acquire_ns.
    #[test]
    fn single_frame_emits_one_row() {
        let timings = [timing(1_000, 50, 200, 3)];
        let out = render_frame_trace_to_jsonl("typing_one_cell", &timings);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1);
        let row: RenderFrameJsonRow = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(row.scenario, "typing_one_cell");
        assert_eq!(row.frame_index, 0);
        assert_eq!(row.ts_ns, 1_000);
        assert_eq!(row.acquire_ns, 50);
        assert_eq!(row.hold_ns, 200);
        assert_eq!(row.dirty_lines_observed, 3);
        // 50 ns < YELLOW_THRESHOLD_NS (100) → green.
        assert_eq!(row.classification, "green");
        assert!(out.ends_with('\n'));
    }

    /// br-ft-ept8r: multi-frame trace numbers frame_index 0..N
    /// monotonically. Lock-wait classification per frame matches
    /// `classify_lock_wait(acquire_ns)`.
    #[test]
    fn multi_frame_indexes_and_classifies_per_row() {
        let timings = [
            timing(1_000, 50, 100, 1),    // green
            timing(2_000, 250, 300, 2),   // yellow
            timing(3_000, 5_000, 400, 1), // red
            timing(4_000, 99, 150, 1),    // green (just under 100)
            timing(5_000, 100, 200, 1),   // yellow (boundary at 100)
            timing(6_000, 1_000, 250, 1), // red (boundary at 1000)
        ];
        let out = render_frame_trace_to_jsonl("heavy_burst", &timings);
        let parsed = parse_render_frame_trace(&out).expect("parse");
        assert_eq!(parsed.len(), 6);
        assert_eq!(
            parsed.iter().map(|r| r.frame_index).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 5],
        );
        assert_eq!(
            parsed
                .iter()
                .map(|r| r.classification.as_str())
                .collect::<Vec<_>>(),
            vec!["green", "yellow", "red", "green", "yellow", "red"],
        );
        for r in &parsed {
            assert_eq!(r.scenario, "heavy_burst");
        }
    }

    /// br-ft-ept8r: round-trip through `parse_render_frame_trace`
    /// preserves every field byte-for-byte. JSON shape is the
    /// CI sink's contract.
    #[test]
    fn jsonl_roundtrips_byte_for_byte() {
        let timings = [timing(1_000, 50, 200, 5), timing(2_500, 800, 350, 12)];
        let out = render_frame_trace_to_jsonl("snap_back", &timings);
        let parsed = parse_render_frame_trace(&out).unwrap();
        let original_rows: Vec<RenderFrameJsonRow> = timings
            .iter()
            .enumerate()
            .map(|(i, t)| RenderFrameJsonRow::from_timing("snap_back", i as u32, t))
            .collect();
        assert_eq!(parsed, original_rows);
    }

    /// br-ft-ept8r: parse skips blank / whitespace-only lines
    /// (matches other `parse_*_jsonl` helpers in the crate).
    #[test]
    fn parse_skips_blank_lines() {
        let timings = [timing(100, 30, 50, 1)];
        let mut out = render_frame_trace_to_jsonl("scenario", &timings);
        out.push_str("\n\n   \n");
        out.push_str(
            &serde_json::to_string(&RenderFrameJsonRow::from_timing(
                "scenario",
                1,
                &timing(200, 40, 60, 2),
            ))
            .unwrap(),
        );
        out.push('\n');
        let parsed = parse_render_frame_trace(&out).unwrap();
        assert_eq!(parsed.len(), 2);
    }

    /// br-ft-ept8r: `classification_label` const helper covers
    /// all 3 [`LockWaitClassification`] variants. Pinned at the
    /// boundary so any future variant addition forces the
    /// emitter to be updated.
    #[test]
    fn classification_label_covers_all_variants() {
        assert_eq!(
            RenderFrameJsonRow::classification_label(LockWaitClassification::Green),
            "green",
        );
        assert_eq!(
            RenderFrameJsonRow::classification_label(LockWaitClassification::Yellow),
            "yellow",
        );
        assert_eq!(
            RenderFrameJsonRow::classification_label(LockWaitClassification::Red),
            "red",
        );
    }
}
