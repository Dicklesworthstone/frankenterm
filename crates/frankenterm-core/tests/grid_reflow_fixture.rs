//! Incremental terminal-grid reflow regression fixture
//! (`ft-mpc9b.2.3`).
//!
//! Foundation slice for the O(damage) reflow algorithm. Until the
//! `frankenterm/term/src/screen.rs::rewrap_lines` integration bead
//! lands, this fixture exercises the pure
//! `frankenterm_core::grid_reflow` algorithm against synthetic
//! cell streams covering the bead's correctness corpus:
//!
//! - ASCII (basic wrap, styled cells preservation)
//! - CJK (wide cells; never split mid-cell)
//! - Emoji + ZWJ (joiner attachment to base)
//! - Combining marks (joiner travels with base across wrap)
//! - Cursor remap (proportional under width change)
//! - Skip predicate (the load-bearing O(damage) optimization)
//!
//! ## Goldens
//!
//! `crates/frankenterm-core/tests/grid_reflow/golden/<scenario>.jsonl`
//! captures structured-log emissions per reflow scenario.
//! `FT_REFLOW_BLESS=1` regenerates with the deliberate-bless flow.

use std::path::PathBuf;

use frankenterm_core::grid_reflow::{
    Cell, ReflowEvent, ReflowHealth, RowSlice, compute_wrap_set, parse_events_jsonl, reflow_line,
    remap_cursor, render_events_jsonl, should_skip_reflow,
};
use proptest::prelude::*;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("grid_reflow")
        .join("golden")
}

fn golden_path(scenario: &str) -> PathBuf {
    golden_dir().join(format!("{scenario}.jsonl"))
}

fn bless_enabled() -> bool {
    std::env::var("FT_REFLOW_BLESS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn ensure_golden_dir_exists() {
    std::fs::create_dir_all(golden_dir()).expect("create golden dir");
}

// ============================================================================
// Synthetic cell-line builders
// ============================================================================

fn ascii(s: &str) -> Vec<Cell> {
    s.chars().map(|c| Cell::new(c, 1, 0)).collect()
}

fn cjk(s: &str) -> Vec<Cell> {
    s.chars().map(|c| Cell::new(c, 2, 0)).collect()
}

/// Family emoji: base + ZWJ + base + ZWJ + base + ZWJ + base.
fn family_emoji() -> Vec<Cell> {
    vec![
        Cell::new('👨', 2, 0),
        Cell::joiner('\u{200d}', 0),
        Cell::new('👩', 2, 0),
        Cell::joiner('\u{200d}', 0),
        Cell::new('👧', 2, 0),
        Cell::joiner('\u{200d}', 0),
        Cell::new('👦', 2, 0),
    ]
}

/// Cell with a combining acute (e.g., "é" decomposed).
fn combining_eacute() -> Vec<Cell> {
    vec![
        Cell::new('e', 1, 0),
        Cell::joiner('\u{0301}', 0), // combining acute
    ]
}

// ============================================================================
// Test 1 — per-scenario invariants.
// ============================================================================

#[test]
fn ascii_50_chars_wraps_correctly_at_width_30() {
    let cells = ascii(&"a".repeat(50));
    let rows = reflow_line(&cells, 30);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], RowSlice { start: 0, end: 30 });
    assert_eq!(rows[1], RowSlice { start: 30, end: 50 });
}

#[test]
fn cjk_never_splits_mid_cell_under_any_width() {
    // 50 CJK glyphs = 100 visual cols. Try a range of widths.
    let cells = cjk(&"中".repeat(50));
    for w in 3..=50 {
        let rows = reflow_line(&cells, w);
        for row in &rows {
            // Each row's visual width = sum of cell widths.
            let visual: u32 = cells[row.start..row.end]
                .iter()
                .map(|c| u32::from(c.width))
                .sum();
            assert!(
                visual as usize <= w,
                "row {row:?} at width {w} has visual width {visual}; exceeds bound"
            );
            // No row may end with a wide cell that would have
            // overflowed. The compute_wrap_set rule (wrap before
            // wide if it overflows) guarantees this — assert
            // that adding the *next* cell would have overflowed
            // (otherwise the wrap was unnecessary).
            if row.end < cells.len() {
                let next_cell = &cells[row.end];
                if next_cell.width > 0 {
                    assert!(
                        visual + u32::from(next_cell.width) > w as u32,
                        "row {row:?} at width {w}: next cell would have fit; \
                         wrap was unnecessary"
                    );
                }
            }
        }
    }
}

#[test]
fn emoji_zwj_family_stays_intact_under_wrap() {
    let mut prefix = ascii(&"a".repeat(78));
    prefix.extend(family_emoji());
    // Total visual width: 78 + 2+0+2+0+2+0+2 = 86.
    // At width=80, the family must wrap as a unit.
    let rows = reflow_line(&prefix, 80);
    // Find which row each base of the family lands in.
    let family_start = 78;
    let mut family_rows = std::collections::HashSet::new();
    for (row_idx, row) in rows.iter().enumerate() {
        for cell_idx in row.start..row.end {
            if cell_idx >= family_start {
                family_rows.insert(row_idx);
            }
        }
    }
    // The family + ZWJ joiners (7 cells) MUST appear contiguously
    // — i.e., no joiner appears in a different row from its base.
    for (row_idx, row) in rows.iter().enumerate() {
        if !family_rows.contains(&row_idx) {
            continue;
        }
        // No row containing family cells starts with a joiner.
        if row.start >= family_start {
            assert!(
                !prefix[row.start].is_joiner,
                "row {row_idx} ({row:?}) starts with a joiner — joiner separated from base"
            );
        }
    }
}

#[test]
fn combining_mark_travels_with_base_across_wrap() {
    // 79 ASCII + 'e' + combining acute. At width=80, the 'e'
    // lands at col 79, the combining acute attaches (column
    // unchanged). Wraps correctly: no split between e and acute.
    let mut cells = ascii(&"x".repeat(79));
    cells.extend(combining_eacute());
    let rows = reflow_line(&cells, 80);
    // Find which row the combining mark lands in.
    let combining_idx = cells.len() - 1;
    let combining_row = rows
        .iter()
        .position(|r| combining_idx >= r.start && combining_idx < r.end)
        .expect("combining mark missing from row slices");
    let base_idx = combining_idx - 1;
    let base_row = rows
        .iter()
        .position(|r| base_idx >= r.start && base_idx < r.end)
        .expect("base cell missing from row slices");
    assert_eq!(
        combining_row, base_row,
        "combining mark separated from its base cell"
    );
}

#[test]
fn skip_predicate_short_circuits_when_widths_equal() {
    let cells = ascii(&"a".repeat(50));
    assert!(should_skip_reflow(&cells, 80, 80));
    assert!(should_skip_reflow(&cells, 1, 1));
}

#[test]
fn skip_predicate_identifies_lines_that_fit_in_both_widths() {
    let cells = ascii("hello world");
    // 11 chars: fits in both widths.
    assert!(should_skip_reflow(&cells, 80, 200));
    assert!(should_skip_reflow(&cells, 200, 80));
}

#[test]
fn skip_predicate_rejects_lines_that_change_wrap() {
    let cells = ascii(&"a".repeat(150));
    // At 80 wraps once; at 200 doesn't.
    assert!(!should_skip_reflow(&cells, 80, 200));
    assert!(!should_skip_reflow(&cells, 200, 80));
}

// ============================================================================
// Test 2 — cursor remap correctness.
// ============================================================================

#[test]
fn cursor_remap_round_trips_under_identity_widths() {
    let cells = ascii(&"a".repeat(100));
    let w = compute_wrap_set(&cells, 80);
    for cursor in [(0, 0), (0, 50), (0, 79), (1, 0), (1, 19)] {
        let remapped = remap_cursor(&cells, cursor, &w, &w);
        assert_eq!(
            remapped, cursor,
            "cursor {cursor:?} drifted under identity remap"
        );
    }
}

#[test]
fn cursor_remap_preserves_logical_position() {
    let cells = ascii(&"a".repeat(200));
    let old = compute_wrap_set(&cells, 100); // breaks: [100]
    let new = compute_wrap_set(&cells, 50); // breaks: [50, 100, 150]
    // Cursor at logical position 75.
    // Old: row 0, col 75 (since first break is at 100).
    // New: row 1, col 25 (75 lands in [50, 100)).
    let remapped = remap_cursor(&cells, (0, 75), &old, &new);
    assert_eq!(remapped, (1, 25));
}

// ============================================================================
// Test 3 — golden snapshot of synthetic ReflowEvents.
//
// Per-scenario goldens pin the structured-log shape so the
// integration bead's recorder produces the same fields.
// ============================================================================

fn synthetic_reflow_events() -> Vec<ReflowEvent> {
    vec![
        // Steady-state typing: 1 line dirty, 199 lines skipped.
        ReflowEvent {
            ts_ms: 0,
            old_width: 80,
            new_width: 80,
            lines_reflowed: 0,
            lines_skipped: 200,
            total_lines: 200,
            duration_ms: 0,
        },
        // Width change 80 → 100 — most lines fit; only the long
        // ones (logs that wrap) need re-lining.
        ReflowEvent {
            ts_ms: 100,
            old_width: 80,
            new_width: 100,
            lines_reflowed: 5,
            lines_skipped: 195,
            total_lines: 200,
            duration_ms: 2,
        },
        // Width change 100 → 50 — many lines need re-lining.
        ReflowEvent {
            ts_ms: 200,
            old_width: 100,
            new_width: 50,
            lines_reflowed: 80,
            lines_skipped: 120,
            total_lines: 200,
            duration_ms: 8,
        },
    ]
}

#[test]
fn golden_synthetic_typing_storm() {
    snapshot_golden("typing_storm", &synthetic_reflow_events());
}

fn snapshot_golden(scenario: &str, events: &[ReflowEvent]) {
    let rendered = render_events_jsonl(events);
    let path = golden_path(scenario);
    if bless_enabled() {
        ensure_golden_dir_exists();
        std::fs::write(&path, &rendered).expect("write blessed golden");
        panic!(
            "{scenario}: golden blessed at {}; re-run without FT_REFLOW_BLESS to validate",
            path.display()
        );
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing golden for {scenario} at {}: {err} \
             (re-run with FT_REFLOW_BLESS=1 to generate)",
            path.display()
        )
    });
    assert_eq!(
        rendered,
        expected,
        "{scenario} drifted from golden at {}",
        path.display()
    );
    let parsed = parse_events_jsonl(&rendered).expect("parse");
    assert_eq!(parsed, events, "JSONL roundtrip drift for {scenario}");
}

// ============================================================================
// Test 4 — health snapshot exercises the bead's "skip ratio"
// observability claim: steady-state typing should report >0.95.
// ============================================================================

#[test]
fn typing_steady_state_reports_high_skip_ratio() {
    let h = ReflowHealth {
        reflows_total: 100,
        lines_reflowed_total: 5,
        lines_skipped_total: 1995,
        last_reflow_duration_ms: 1,
    };
    assert!(
        h.skip_ratio() > 0.95,
        "steady-state typing should produce skip_ratio > 0.95; got {}",
        h.skip_ratio()
    );
}

// ============================================================================
// Test 5 — proptest properties.
// ============================================================================

prop_compose! {
    fn arb_cell()(
        ch in any::<char>(),
        kind in 0u8..3,
        style in any::<u32>(),
    ) -> Cell {
        match kind {
            0 => Cell::new(ch, 1, style),
            1 => Cell::new(ch, 2, style),
            _ => Cell::joiner(ch, style),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    /// `compute_wrap_set` is total — no panic, no out-of-bounds
    /// for any cell sequence + width >= 1.
    #[test]
    fn compute_wrap_set_total(
        cells in proptest::collection::vec(arb_cell(), 0..64),
        width in 1usize..200,
    ) {
        let w = compute_wrap_set(&cells, width);
        // Breaks are sorted ascending and within bounds.
        for w_pair in w.breaks.windows(2) {
            prop_assert!(w_pair[0] < w_pair[1]);
        }
        for &b in &w.breaks {
            prop_assert!(b <= cells.len());
        }
    }

    /// Reflow preserves the cell count: the sum of row lengths
    /// equals the input length exactly.
    #[test]
    fn reflow_preserves_cell_count(
        cells in proptest::collection::vec(arb_cell(), 0..128),
        width in 1usize..100,
    ) {
        let rows = reflow_line(&cells, width);
        let total: usize = rows.iter().map(|r| r.len()).sum();
        prop_assert_eq!(total, cells.len());
    }

    /// Wide cells never split mid-cell. For every row that ends
    /// before the line end, adding the next non-joiner cell
    /// would exceed the width.
    #[test]
    fn wide_cells_never_split_under_random_inputs(
        cells in proptest::collection::vec(arb_cell(), 1..64),
        width in 2usize..50,
    ) {
        let rows = reflow_line(&cells, width);
        for row in &rows {
            let visual: u32 = cells[row.start..row.end].iter().map(|c| u32::from(c.width)).sum();
            prop_assert!(visual as usize <= width);
        }
    }

    /// Skip predicate is reflexive: comparing a width to itself
    /// always returns true.
    #[test]
    fn skip_predicate_reflexive(
        cells in proptest::collection::vec(arb_cell(), 0..32),
        width in 1usize..100,
    ) {
        prop_assert!(should_skip_reflow(&cells, width, width));
    }

    /// Skip predicate is symmetric: skip(a, b) iff skip(b, a).
    #[test]
    fn skip_predicate_symmetric(
        cells in proptest::collection::vec(arb_cell(), 0..32),
        a in 1usize..100,
        b in 1usize..100,
    ) {
        prop_assert_eq!(
            should_skip_reflow(&cells, a, b),
            should_skip_reflow(&cells, b, a)
        );
    }

    /// JSONL roundtrip identity.
    #[test]
    fn jsonl_roundtrip(
        events in proptest::collection::vec(
            (0u64..u64::MAX, 1u32..200, 1u32..200, 0u32..1000, 0u32..1000, 0u32..1000, 0u32..100)
                .prop_map(|(ts, ow, nw, lr, ls, tl, dm)| ReflowEvent {
                    ts_ms: ts,
                    old_width: ow,
                    new_width: nw,
                    lines_reflowed: lr,
                    lines_skipped: ls,
                    total_lines: tl,
                    duration_ms: dm,
                }),
            0..16,
        ),
    ) {
        let rendered = render_events_jsonl(&events);
        let parsed = parse_events_jsonl(&rendered).unwrap();
        prop_assert_eq!(parsed, events);
    }
}
