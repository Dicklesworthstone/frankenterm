//! Incremental terminal-grid reflow algorithm
//! ([BR-TERM-EMULATOR-UPLIFT.2.3] / `ft-mpc9b.2.3`).
//!
//! Sub-epic 2's reflow correctness layer. The current
//! `frankenterm/term/src/screen.rs::rewrap_lines` does logical-
//! line-based rewrap with a signature cache; the bead asks for a
//! per-line **O(damage)** skip predicate so a line that wraps
//! identically under the new width never re-emits cells. This
//! module ships the pure-function algorithm; integration into
//! `screen.rs` (the WezTerm-derived terminal-state code) is the
//! follow-on bead.
//!
//! ## What this module is
//!
//! - [`Cell`] — one terminal cell with codepoint + visual width
//!   (1 or 2) + opaque style attrs.
//! - [`WrapSet`] — the set of wrap points for a logical line under
//!   a given physical width.
//! - [`compute_wrap_set`] — the pure wrap algorithm. Honors:
//!   wide-character cells (CJK takes 2 cells; wrap MUST NOT split
//!   them mid-cell), zero-width-joiner sequences (combining marks
//!   travel with their base cell).
//! - [`should_skip_reflow`] — the O(damage) predicate. Returns
//!   `true` iff the line wraps identically under both the old and
//!   new widths — the load-bearing optimization.
//! - [`reflow_line`] — incremental per-line reflow producing the
//!   re-lined row slices.
//! - [`remap_cursor`] — proportional cursor coordinate remap
//!   across a width change.
//! - [`ReflowEvent`] / [`ReflowHealth`] — structured-logging row +
//!   ft-doctor counter snapshot.
//!
//! ## What this module is NOT
//!
//! - The actual cell/style types from `frankenterm-term` —
//!   `Cell` here is a minimal reflow-only representation. The
//!   integration bead maps `frankenterm_term::Cell` onto this.
//! - BiDi reordering — that's `A11Y.4` cross-link; this module
//!   handles the LTR wrap algorithm; BiDi is post-processing.
//! - The `screen.rs` migration — separate bead.

use serde::{Deserialize, Serialize};

// ============================================================================
// Cell
// ============================================================================

/// One terminal cell. The reflow algorithm only needs three
/// properties of a cell:
///
/// 1. Whether it occupies 1 or 2 visual columns (CJK / emoji
///    typically 2; combining marks 0; everything else 1).
/// 2. Its style attrs as an opaque token — the algorithm doesn't
///    interpret them, just preserves them across wrap boundaries.
/// 3. Whether it's a "joiner" (combining mark, ZWJ, variation
///    selector) that MUST travel with the preceding base cell —
///    even if the preceding cell would otherwise have been the
///    last cell of a wrap row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cell {
    /// Codepoint (kept opaque for the reflow algorithm; only
    /// `width` and `is_joiner` matter).
    pub ch: char,
    /// Visual column count: 0 (combining mark / ZWJ), 1 (ASCII /
    /// most Latin), or 2 (CJK / wide emoji).
    pub width: u8,
    /// Whether this cell is a combining mark / ZWJ / variation
    /// selector. Joiners attach to the preceding base cell and
    /// MUST NOT cross a wrap boundary.
    pub is_joiner: bool,
    /// Opaque style attrs — preserved verbatim across reflow.
    /// The integration layer hashes/encodes the real
    /// `frankenterm_term` style into this u32.
    pub style: u32,
}

impl Cell {
    #[must_use]
    pub const fn new(ch: char, width: u8, style: u32) -> Self {
        Self {
            ch,
            width,
            is_joiner: false,
            style,
        }
    }

    /// Construct a joiner cell (combining mark / ZWJ / variation
    /// selector). Width is always 0 by convention — the reflow
    /// algorithm uses the explicit `is_joiner` flag, not the
    /// width, to decide attachment.
    #[must_use]
    pub const fn joiner(ch: char, style: u32) -> Self {
        Self {
            ch,
            width: 0,
            is_joiner: true,
            style,
        }
    }
}

// ============================================================================
// Wrap set
// ============================================================================

/// The wrap-point set for a logical line under a specific
/// physical width.
///
/// `breaks[i]` is the cell index in the original logical-line
/// `Cell` slice where row `i+1` begins. So a logical line of
/// length 200 with breaks `[80, 160]` produces 3 rows of length
/// 80/80/40.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrapSet {
    /// Physical width the wrap-set was computed for.
    pub width: usize,
    /// Sorted ascending cell indices where each subsequent row
    /// begins. Empty means the entire line fits in one row.
    pub breaks: Vec<usize>,
}

impl WrapSet {
    /// Number of physical rows this logical line occupies under
    /// `width`. Always `breaks.len() + 1` (a line with zero
    /// breaks still occupies one row).
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.breaks.len() + 1
    }
}

/// Compute the wrap set for a logical line under `width`.
///
/// Honors:
/// - Wide cells (width 2) never split mid-cell — if a wide cell
///   would land at column `width-1`, the wrap moves it to the
///   next row entirely.
/// - Joiner cells (combining marks, ZWJ, variation selectors)
///   attach to the preceding base cell — a joiner never starts
///   a new row.
///
/// The returned [`WrapSet`] is deterministic in the input.
///
/// # Panics
///
/// Panics if `width == 0`. Callers MUST ensure `width >= 1`.
#[must_use]
pub fn compute_wrap_set(cells: &[Cell], width: usize) -> WrapSet {
    assert!(width >= 1, "wrap width must be >= 1");
    let mut breaks = Vec::new();
    let mut col = 0usize;
    for (i, cell) in cells.iter().enumerate() {
        if cell.is_joiner {
            // Joiner attaches to the preceding base cell — never
            // starts a new row even if `col == width`.
            continue;
        }
        let cell_w = cell.width as usize;
        if cell_w == 0 {
            // Defensive: a non-joiner zero-width cell shouldn't
            // exist in practice, but if one does, treat it like
            // a joiner (no column advance).
            continue;
        }
        if col + cell_w > width {
            // Wrap before this cell.
            breaks.push(i);
            col = cell_w;
        } else {
            col += cell_w;
        }
    }
    WrapSet { width, breaks }
}

/// The O(damage) skip predicate.
///
/// Returns `true` iff the line wraps identically under both the
/// old and new widths — i.e., re-lining produces the same row
/// slices. The integration layer uses this to skip per-line
/// reflow work; lines that pass the predicate stay in their
/// existing rows untouched.
#[must_use]
pub fn should_skip_reflow(cells: &[Cell], old_width: usize, new_width: usize) -> bool {
    if old_width == new_width {
        return true;
    }
    let old = compute_wrap_set(cells, old_width);
    let new = compute_wrap_set(cells, new_width);
    old.breaks == new.breaks
}

// ============================================================================
// Reflow result
// ============================================================================

/// Re-lined row slice — a half-open `[start, end)` range over
/// the logical-line cell slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowSlice {
    pub start: usize,
    pub end: usize,
}

impl RowSlice {
    #[must_use]
    pub const fn len(&self) -> usize {
        self.end - self.start
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// Reflow a single logical line under `new_width`. The result is
/// the sequence of `RowSlice` ranges over the input cells.
#[must_use]
pub fn reflow_line(cells: &[Cell], new_width: usize) -> Vec<RowSlice> {
    let wrap = compute_wrap_set(cells, new_width);
    let mut rows = Vec::with_capacity(wrap.row_count());
    let mut start = 0usize;
    for &b in &wrap.breaks {
        rows.push(RowSlice { start, end: b });
        start = b;
    }
    rows.push(RowSlice {
        start,
        end: cells.len(),
    });
    rows
}

// ============================================================================
// Cursor remap
// ============================================================================

/// Remap a cursor `(physical_row, physical_col)` across a width
/// change.
///
/// The remap is **logical**: the cursor's position in the
/// logical line is preserved (proportional under wrap-set
/// change). For a line that wraps identically under both
/// widths, the result is exactly the input.
///
/// The bead's special case: a cursor that lands in column 0 on a
/// non-first row should be re-attached to the end of the prior
/// row (so it keeps its association with the just-typed text).
/// The integration bead in `screen.rs` already does this; this
/// helper preserves the logical position and lets the integration
/// apply the col-0 fixup.
#[must_use]
pub fn remap_cursor(
    cells: &[Cell],
    cursor: (usize, usize),
    old_wrap: &WrapSet,
    new_wrap: &WrapSet,
) -> (usize, usize) {
    let (row, col) = cursor;
    // Linearize: cell index in the logical line.
    let row_start = if row == 0 {
        0
    } else {
        old_wrap.breaks.get(row - 1).copied().unwrap_or(cells.len())
    };
    let logical_idx = (row_start + col).min(cells.len());
    // Find which new row the logical idx falls into.
    let mut new_row = 0usize;
    let mut new_row_start = 0usize;
    for (i, &b) in new_wrap.breaks.iter().enumerate() {
        if logical_idx < b {
            break;
        }
        new_row = i + 1;
        new_row_start = b;
    }
    let new_col = logical_idx - new_row_start;
    (new_row, new_col)
}

// ============================================================================
// Structured logging
// ============================================================================

/// One row of `tests/grid_reflow/logs/<scenario>.jsonl` per the
/// bead's structured-logging schema. Emitted per reflow event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflowEvent {
    pub ts_ms: u64,
    pub old_width: u32,
    pub new_width: u32,
    pub lines_reflowed: u32,
    pub lines_skipped: u32,
    pub total_lines: u32,
    pub duration_ms: u32,
}

/// Cumulative health snapshot for `ft doctor`. Mirrors the
/// other `*Health` shapes from prior beads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflowHealth {
    pub reflows_total: u64,
    pub lines_reflowed_total: u64,
    pub lines_skipped_total: u64,
    pub last_reflow_duration_ms: u32,
}

impl ReflowHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            reflows_total: 0,
            lines_reflowed_total: 0,
            lines_skipped_total: 0,
            last_reflow_duration_ms: 0,
        }
    }

    /// Skip ratio across lifetime (0.0 = no skips ever, 1.0 = all
    /// skips). The bead's "O(damage)" optimization is observable
    /// here: a healthy steady-state typing workload should report
    /// > 0.95 skip ratio.
    #[must_use]
    pub fn skip_ratio(&self) -> f64 {
        let total = self.lines_reflowed_total + self.lines_skipped_total;
        if total == 0 {
            return 0.0;
        }
        self.lines_skipped_total as f64 / total as f64
    }
}

/// Render a slice of events as JSONL.
#[must_use]
pub fn render_events_jsonl(events: &[ReflowEvent]) -> String {
    let mut out = String::new();
    for ev in events {
        let line = serde_json::to_string(ev).expect("ReflowEvent always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Parse a JSONL string back into events.
pub fn parse_events_jsonl(jsonl: &str) -> Result<Vec<ReflowEvent>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(serde_json::from_str(trimmed)?);
    }
    Ok(out)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn ascii(s: &str) -> Vec<Cell> {
        s.chars().map(|c| Cell::new(c, 1, 0)).collect()
    }

    #[test]
    fn empty_line_has_one_row() {
        let cells: Vec<Cell> = vec![];
        let w = compute_wrap_set(&cells, 80);
        assert_eq!(w.row_count(), 1);
        assert_eq!(w.breaks.len(), 0);
    }

    #[test]
    fn ascii_short_line_no_wraps() {
        let cells = ascii("hello world");
        let w = compute_wrap_set(&cells, 80);
        assert!(w.breaks.is_empty());
        assert_eq!(w.row_count(), 1);
    }

    #[test]
    fn ascii_long_line_wraps_at_width() {
        let cells = ascii(&"a".repeat(100));
        let w = compute_wrap_set(&cells, 80);
        assert_eq!(w.breaks, vec![80]);
        assert_eq!(w.row_count(), 2);
    }

    #[test]
    fn wide_cell_does_not_split_at_width_minus_one() {
        // Place a wide CJK at index 79 in width=80. The previous
        // 79 cells fill columns 0..79, then the wide cell wants
        // columns 79..81 — but 81 > 80, so it must wrap to the
        // next row.
        let mut cells = vec![Cell::new('a', 1, 0); 79];
        cells.push(Cell::new('中', 2, 0));
        let w = compute_wrap_set(&cells, 80);
        assert_eq!(
            w.breaks,
            vec![79],
            "wide cell at boundary must wrap, not split"
        );
        assert_eq!(w.row_count(), 2);
    }

    #[test]
    fn wide_cell_at_exact_boundary_stays_on_row() {
        // 78 ASCII + 1 wide CJK → cols 0..78 + 78..80 = 80. Fits.
        let mut cells = vec![Cell::new('a', 1, 0); 78];
        cells.push(Cell::new('中', 2, 0));
        let w = compute_wrap_set(&cells, 80);
        assert!(w.breaks.is_empty());
        assert_eq!(w.row_count(), 1);
    }

    #[test]
    fn joiner_cells_travel_with_base() {
        // Family emoji: base + ZWJ + base + ZWJ + base + ZWJ + base.
        // Total width: 2 (only the base cells contribute). At
        // width=80 fits in one row.
        let cells = vec![
            Cell::new('👨', 2, 0),
            Cell::joiner('\u{200d}', 0),
            Cell::new('👩', 2, 0),
            Cell::joiner('\u{200d}', 0),
            Cell::new('👧', 2, 0),
            Cell::joiner('\u{200d}', 0),
            Cell::new('👦', 2, 0),
        ];
        // Total visual width = 2 + 2 + 2 + 2 = 8.
        let w = compute_wrap_set(&cells, 80);
        assert!(w.breaks.is_empty());
    }

    #[test]
    fn joiner_at_wrap_point_stays_with_base() {
        // 79 ASCII (cols 0..79) + base wide (cols 79..81 → wraps,
        // so wide goes to next row at index 79) + joiner. The
        // joiner MUST NOT start a new row — it travels with the
        // wide cell that's already on the new row.
        let mut cells = vec![Cell::new('a', 1, 0); 79];
        cells.push(Cell::new('👨', 2, 0));
        cells.push(Cell::joiner('\u{200d}', 0));
        let w = compute_wrap_set(&cells, 80);
        // Wrap is BEFORE the wide cell at index 79. The joiner
        // travels with the wide.
        assert_eq!(w.breaks, vec![79]);
    }

    #[test]
    fn skip_predicate_identifies_unchanged_lines() {
        let cells = ascii("hello");
        // Line fits in both widths — must skip.
        assert!(should_skip_reflow(&cells, 80, 100));
        assert!(should_skip_reflow(&cells, 100, 80));
    }

    #[test]
    fn skip_predicate_rejects_changed_lines() {
        let cells = ascii(&"a".repeat(100));
        // Old width 80 wraps; new width 200 doesn't. Different
        // wrap sets → must NOT skip.
        assert!(!should_skip_reflow(&cells, 80, 200));
    }

    #[test]
    fn skip_predicate_returns_true_when_widths_equal() {
        let cells = ascii(&"a".repeat(100));
        assert!(should_skip_reflow(&cells, 80, 80));
    }

    #[test]
    fn reflow_line_produces_correct_row_slices() {
        let cells = ascii(&"abcdefghij".repeat(10)); // 100 cells
        let rows = reflow_line(&cells, 30);
        // 100 / 30 = 3 full rows + 1 partial → 4 rows.
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0], RowSlice { start: 0, end: 30 });
        assert_eq!(rows[1], RowSlice { start: 30, end: 60 });
        assert_eq!(rows[2], RowSlice { start: 60, end: 90 });
        assert_eq!(
            rows[3],
            RowSlice {
                start: 90,
                end: 100
            }
        );
        for r in &rows {
            assert!(r.len() <= 30);
        }
    }

    #[test]
    fn reflow_line_preserves_cell_count() {
        let cells = ascii(&"x".repeat(50));
        let rows = reflow_line(&cells, 13);
        let total: usize = rows.iter().map(|r| r.len()).sum();
        assert_eq!(total, 50);
    }

    #[test]
    fn cursor_remap_preserves_position_when_widths_unchanged() {
        let cells = ascii(&"a".repeat(50));
        let w80 = compute_wrap_set(&cells, 80);
        // Cursor at logical position 25 → row 0, col 25.
        let remapped = remap_cursor(&cells, (0, 25), &w80, &w80);
        assert_eq!(remapped, (0, 25));
    }

    #[test]
    fn cursor_remap_under_wrap_change() {
        let cells = ascii(&"a".repeat(100));
        let old = compute_wrap_set(&cells, 80); // 80, 20
        let new = compute_wrap_set(&cells, 50); // 50, 50
        // Old cursor at (0, 75) — logical position 75.
        let remapped = remap_cursor(&cells, (0, 75), &old, &new);
        // 75 < 50? No. So new row 1, col 75-50 = 25.
        assert_eq!(remapped, (1, 25));
    }

    #[test]
    fn cursor_remap_clamps_oob_position_to_line_end() {
        let cells = ascii("hello"); // 5 cells
        let w = compute_wrap_set(&cells, 80);
        let remapped = remap_cursor(&cells, (0, 100), &w, &w);
        assert_eq!(remapped, (0, 5));
    }

    #[test]
    fn skip_ratio_baseline_is_zero() {
        let h = ReflowHealth::baseline();
        assert_eq!(h.skip_ratio(), 0.0);
    }

    #[test]
    fn skip_ratio_handles_steady_state_typing() {
        let h = ReflowHealth {
            reflows_total: 100,
            lines_reflowed_total: 5,
            lines_skipped_total: 195,
            last_reflow_duration_ms: 0,
        };
        // 195 / 200 = 0.975
        assert!(h.skip_ratio() > 0.95);
    }

    #[test]
    fn jsonl_event_roundtrip() {
        let events = vec![
            ReflowEvent {
                ts_ms: 0,
                old_width: 80,
                new_width: 100,
                lines_reflowed: 3,
                lines_skipped: 97,
                total_lines: 100,
                duration_ms: 2,
            },
            ReflowEvent {
                ts_ms: 100,
                old_width: 100,
                new_width: 80,
                lines_reflowed: 50,
                lines_skipped: 50,
                total_lines: 100,
                duration_ms: 8,
            },
        ];
        let rendered = render_events_jsonl(&events);
        let parsed = parse_events_jsonl(&rendered).unwrap();
        assert_eq!(parsed, events);
    }

    #[test]
    fn ascii_styled_cells_preserve_style_across_wrap() {
        let cells = vec![Cell::new('a', 1, 0xDEADBEEF); 100];
        let rows = reflow_line(&cells, 30);
        // Every row's cells must carry the same style.
        for row in &rows {
            for i in row.start..row.end {
                assert_eq!(cells[i].style, 0xDEADBEEF);
            }
        }
    }

    #[test]
    fn width_one_wraps_every_cell() {
        let cells = ascii("abcde");
        let w = compute_wrap_set(&cells, 1);
        assert_eq!(w.row_count(), 5);
        assert_eq!(w.breaks, vec![1, 2, 3, 4]);
    }

    #[test]
    #[should_panic(expected = "wrap width must be >= 1")]
    fn width_zero_panics() {
        let _ = compute_wrap_set(&[], 0);
    }
}
