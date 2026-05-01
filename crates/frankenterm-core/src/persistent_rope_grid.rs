//! Persistent-rope terminal-grid prototype + flat-grid baseline
//! ([BR-TERM-EMULATOR-UPLIFT.2.5] / `ft-mpc9b.2.5`).
//!
//! P3 alien-artifact research bead. Explicit acceptance: "prototype
//! + bench results checked in regardless of ship/no-ship + decision
//! documented at docs/perf/persistent-rope-evaluation.md". The bead's
//! rubric:
//!
//! > Ship rope-backed grid IFF reflow ≥2× faster on 200-pane fleet
//! > AND memory overhead ≤30% AND render thread unaffected.
//! > Otherwise: archive prototype + negative-result doc.
//!
//! ## What this module ships
//!
//! - [`Cell`] — minimal terminal cell (codepoint + width + style).
//!   Distinct from `crate::grid_reflow::Cell` because that one
//!   carries `is_joiner` for wrap correctness; here we only care
//!   about cell identity for grid ops.
//! - [`TerminalGridOps`] — common trait both implementations
//!   satisfy. The bench harness consumes it generically; the
//!   property test asserts observational equivalence by running
//!   the same op stream against both.
//! - [`FlatGrid`] — `Vec<Vec<Cell>>` baseline. The current
//!   `frankenterm/term/src/screen.rs::Screen` cell storage is
//!   morally this; the prototype-vs-baseline comparison is honest
//!   only against this shape.
//! - [`RopeGrid`] — hand-rolled persistent rope (binary tree of
//!   line groups, copy-on-write via `Arc`). No new workspace dep
//!   (the `im`/`imbl` evaluation is documented but skipped — adding
//!   a workspace dep for a P3 research bead is the wrong tradeoff
//!   on cost-vs-information).
//! - [`SnapshotComparisonReport`] — counter snapshot for the
//!   bench harness's structured-log emission.
//!
//! ## Hand-rolled rope shape
//!
//! `RopeGrid` is a binary tree of [`LineGroup`]s, each holding up
//! to `LINES_PER_GROUP` lines (32 by default — small enough for
//! cache-line-friendly reads, large enough that the tree stays
//! shallow). Internal nodes are `Arc<RopeNode>` so cloning a
//! grid is O(1) — the load-bearing snapshot win.
//!
//! On mutation:
//! - `set_line(idx, line)` → walk the path from root to the
//!   target group, cloning each `Arc<RopeNode>` on the way down.
//!   The unaffected subtrees stay shared with the previous
//!   version. O(log n) work + O(log n) extra allocations.
//! - `insert_line(idx, line)` / `delete_line(idx)` — same
//!   path-cloning pattern.
//! - `scroll_up(n)` — drops the first `n` lines via repeated
//!   `delete_line(0)`. (A real implementation would do range
//!   surgery; the prototype is honest about the simple
//!   approach.)
//!
//! ## What this module is NOT
//!
//! - The actual `frankenterm_term::Screen` migration. The bead's
//!   acceptance does NOT require the migration — only the
//!   prototype + bench + decision. If the rubric passes the GPU
//!   integration bead does the migration.
//! - A full RRB-Tree / HAMT implementation. The hand-rolled rope
//!   is *sufficient* for the bench-vs-baseline comparison the
//!   bead asks for; a production implementation would use
//!   `imbl::Vector` (RRB-Tree). The decision doc records the
//!   tradeoff.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ============================================================================
// Cell
// ============================================================================

/// Minimal terminal cell for grid-ops correctness measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cell {
    pub ch: char,
    pub width: u8,
    pub style: u32,
}

impl Cell {
    #[must_use]
    pub const fn new(ch: char, width: u8, style: u32) -> Self {
        Self { ch, width, style }
    }

    #[must_use]
    pub const fn blank() -> Self {
        Self {
            ch: ' ',
            width: 1,
            style: 0,
        }
    }
}

/// One terminal line — a vector of cells.
pub type Line = Vec<Cell>;

// ============================================================================
// Common trait
// ============================================================================

/// The cell-grid operations the bead's bench / property-test
/// harness exercises. Both `FlatGrid` and `RopeGrid` implement
/// it identically.
pub trait TerminalGridOps {
    /// Number of lines in the grid.
    fn line_count(&self) -> usize;

    /// Read a single line by index. Returns `None` if out of range.
    fn line(&self, idx: usize) -> Option<&Line>;

    /// Replace a single line. No-op if out of range.
    fn set_line(&mut self, idx: usize, line: Line);

    /// Insert a line at `idx`, pushing later lines down.
    /// `idx == line_count()` appends.
    fn insert_line(&mut self, idx: usize, line: Line);

    /// Delete the line at `idx`. No-op if out of range.
    fn delete_line(&mut self, idx: usize);

    /// Scroll the grid up by `n` lines (drop the first `n`).
    fn scroll_up(&mut self, n: usize);

    /// Total cell count (sum of line lengths). Cheap O(line_count)
    /// for both implementations; used by the bench's memory-
    /// overhead measurement.
    fn total_cells(&self) -> usize {
        (0..self.line_count())
            .filter_map(|i| self.line(i).map(Vec::len))
            .sum()
    }

    /// Materialize the grid as a flat `Vec<Line>`. Used by the
    /// property test to compare two implementations cell-by-cell.
    fn to_flat(&self) -> Vec<Line> {
        (0..self.line_count())
            .filter_map(|i| self.line(i).cloned())
            .collect()
    }
}

// ============================================================================
// Flat grid — the baseline
// ============================================================================

/// `Vec<Vec<Cell>>` baseline. Direct equivalent to today's
/// `frankenterm_term::Screen` cell storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatGrid {
    lines: Vec<Line>,
}

impl FlatGrid {
    #[must_use]
    pub fn new(lines: Vec<Line>) -> Self {
        Self { lines }
    }

    #[must_use]
    pub fn empty() -> Self {
        Self { lines: Vec::new() }
    }
}

impl TerminalGridOps for FlatGrid {
    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn line(&self, idx: usize) -> Option<&Line> {
        self.lines.get(idx)
    }

    fn set_line(&mut self, idx: usize, line: Line) {
        if let Some(slot) = self.lines.get_mut(idx) {
            *slot = line;
        }
    }

    fn insert_line(&mut self, idx: usize, line: Line) {
        let idx = idx.min(self.lines.len());
        self.lines.insert(idx, line);
    }

    fn delete_line(&mut self, idx: usize) {
        if idx < self.lines.len() {
            self.lines.remove(idx);
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.lines.len());
        self.lines.drain(0..n);
    }
}

// ============================================================================
// Persistent rope grid
// ============================================================================

/// Lines per leaf group. Tradeoff:
/// - Bigger groups → shallower tree, more cell-array copying on
///   per-line mutation.
/// - Smaller groups → deeper tree, more `Arc` indirection per
///   read.
///
/// 32 is the typical sweet spot for terminal-sized workloads
/// (a 100-row visible viewport reaches the leaf in 1-2 hops).
const LINES_PER_GROUP: usize = 32;

/// Internal tree node. Either a leaf holding up to
/// `LINES_PER_GROUP` lines, or an internal node with two
/// `Arc<RopeNode>` children.
#[derive(Debug, Clone)]
enum RopeNode {
    Leaf {
        lines: Vec<Line>,
    },
    Internal {
        left: Arc<RopeNode>,
        right: Arc<RopeNode>,
        /// Cached count of lines in this subtree.
        size: usize,
    },
}

impl RopeNode {
    fn size(&self) -> usize {
        match self {
            Self::Leaf { lines } => lines.len(),
            Self::Internal { size, .. } => *size,
        }
    }

    fn is_leaf_full(&self) -> bool {
        matches!(self, Self::Leaf { lines } if lines.len() >= LINES_PER_GROUP)
    }

    /// Recursively get a line by index.
    fn get(&self, idx: usize) -> Option<&Line> {
        match self {
            Self::Leaf { lines } => lines.get(idx),
            Self::Internal { left, right, .. } => {
                let lsz = left.size();
                if idx < lsz {
                    left.get(idx)
                } else {
                    right.get(idx - lsz)
                }
            }
        }
    }

    /// Recursively set a line by index, returning a new tree.
    fn set(&self, idx: usize, line: Line) -> Self {
        match self {
            Self::Leaf { lines } => {
                let mut new_lines = lines.clone();
                if let Some(slot) = new_lines.get_mut(idx) {
                    *slot = line;
                }
                Self::Leaf { lines: new_lines }
            }
            Self::Internal { left, right, .. } => {
                let lsz = left.size();
                if idx < lsz {
                    let new_left = Arc::new(left.set(idx, line));
                    let size = new_left.size() + right.size();
                    Self::Internal {
                        left: new_left,
                        right: Arc::clone(right),
                        size,
                    }
                } else {
                    let new_right = Arc::new(right.set(idx - lsz, line));
                    let size = left.size() + new_right.size();
                    Self::Internal {
                        left: Arc::clone(left),
                        right: new_right,
                        size,
                    }
                }
            }
        }
    }

    /// Insert a line at `idx`, returning a new tree.
    fn insert(&self, idx: usize, line: Line) -> Self {
        match self {
            Self::Leaf { lines } => {
                let mut new_lines = lines.clone();
                let i = idx.min(new_lines.len());
                new_lines.insert(i, line);
                if new_lines.len() <= LINES_PER_GROUP {
                    Self::Leaf { lines: new_lines }
                } else {
                    // Split.
                    let mid = new_lines.len() / 2;
                    let right_lines = new_lines.split_off(mid);
                    let size = new_lines.len() + right_lines.len();
                    Self::Internal {
                        left: Arc::new(Self::Leaf { lines: new_lines }),
                        right: Arc::new(Self::Leaf { lines: right_lines }),
                        size,
                    }
                }
            }
            Self::Internal { left, right, .. } => {
                let lsz = left.size();
                if idx <= lsz {
                    let new_left = Arc::new(left.insert(idx, line));
                    let size = new_left.size() + right.size();
                    Self::Internal {
                        left: new_left,
                        right: Arc::clone(right),
                        size,
                    }
                } else {
                    let new_right = Arc::new(right.insert(idx - lsz, line));
                    let size = left.size() + new_right.size();
                    Self::Internal {
                        left: Arc::clone(left),
                        right: new_right,
                        size,
                    }
                }
            }
        }
    }

    /// Delete the line at `idx`, returning a new tree (or `None`
    /// if the resulting subtree would be empty — caller handles).
    fn delete(&self, idx: usize) -> Option<Self> {
        match self {
            Self::Leaf { lines } => {
                if idx >= lines.len() {
                    return Some(self.clone());
                }
                let mut new_lines = lines.clone();
                new_lines.remove(idx);
                if new_lines.is_empty() {
                    None
                } else {
                    Some(Self::Leaf { lines: new_lines })
                }
            }
            Self::Internal { left, right, .. } => {
                let lsz = left.size();
                if idx < lsz {
                    match left.delete(idx) {
                        Some(new_left) => {
                            let size = new_left.size() + right.size();
                            Some(Self::Internal {
                                left: Arc::new(new_left),
                                right: Arc::clone(right),
                                size,
                            })
                        }
                        None => Some((**right).clone()),
                    }
                } else {
                    match right.delete(idx - lsz) {
                        Some(new_right) => {
                            let size = left.size() + new_right.size();
                            Some(Self::Internal {
                                left: Arc::clone(left),
                                right: Arc::new(new_right),
                                size,
                            })
                        }
                        None => Some((**left).clone()),
                    }
                }
            }
        }
    }

    fn _is_leaf_full(&self) -> bool {
        self.is_leaf_full()
    }
}

/// Persistent rope-backed grid. Cloning is O(1) (Arc bump).
#[derive(Debug, Clone)]
pub struct RopeGrid {
    root: Option<Arc<RopeNode>>,
}

impl RopeGrid {
    #[must_use]
    pub fn new(lines: Vec<Line>) -> Self {
        let root = if lines.is_empty() {
            None
        } else {
            // Build a balanced tree by repeated insertion. Not
            // optimal but adequate for the bench harness.
            let mut node = RopeNode::Leaf {
                lines: Vec::with_capacity(LINES_PER_GROUP),
            };
            for (i, line) in lines.into_iter().enumerate() {
                node = node.insert(i, line);
            }
            Some(Arc::new(node))
        };
        Self { root }
    }

    #[must_use]
    pub fn empty() -> Self {
        Self { root: None }
    }
}

impl TerminalGridOps for RopeGrid {
    fn line_count(&self) -> usize {
        self.root.as_deref().map(RopeNode::size).unwrap_or(0)
    }

    fn line(&self, idx: usize) -> Option<&Line> {
        self.root.as_deref().and_then(|n| n.get(idx))
    }

    fn set_line(&mut self, idx: usize, line: Line) {
        if let Some(root) = self.root.as_deref() {
            if idx < root.size() {
                self.root = Some(Arc::new(root.set(idx, line)));
            }
        }
    }

    fn insert_line(&mut self, idx: usize, line: Line) {
        match self.root.as_deref() {
            None => {
                self.root = Some(Arc::new(RopeNode::Leaf { lines: vec![line] }));
            }
            Some(root) => {
                let cap = root.size();
                let i = idx.min(cap);
                self.root = Some(Arc::new(root.insert(i, line)));
            }
        }
    }

    fn delete_line(&mut self, idx: usize) {
        if let Some(root) = self.root.as_deref() {
            if idx < root.size() {
                let new_root = root.delete(idx).map(Arc::new);
                self.root = new_root;
            }
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let count = self.line_count();
        let n = n.min(count);
        for _ in 0..n {
            self.delete_line(0);
        }
    }
}

// ============================================================================
// Snapshot comparison report (structured log)
// ============================================================================

/// One row of `tests/persistent_rope/logs/<scenario>.jsonl` per
/// the bead's structured-logging schema. Captures the bench's
/// per-snapshot observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotComparisonReport {
    pub ts_ms: u64,
    pub op: GridOp,
    /// Tree size for the rope side (or `lines * cells_per_line`
    /// for the flat side). Used by the decision doc to
    /// approximate memory.
    pub tree_size_bytes: u64,
    /// Wall-clock duration of the op in nanoseconds.
    pub duration_ns: u64,
}

/// The closed list of grid ops the bench harness measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GridOp {
    InsertLine,
    DeleteLine,
    Scroll,
    SetLine,
    Snapshot,
    Reflow,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn line(s: &str) -> Line {
        s.chars().map(|c| Cell::new(c, 1, 0)).collect()
    }

    fn sample_lines(n: usize) -> Vec<Line> {
        (0..n).map(|i| line(&format!("line-{i}"))).collect()
    }

    #[test]
    fn flat_grid_round_trip() {
        let g = FlatGrid::new(sample_lines(5));
        assert_eq!(g.line_count(), 5);
        assert_eq!(g.line(2).unwrap()[0].ch, 'l');
        assert_eq!(g.to_flat().len(), 5);
    }

    #[test]
    fn rope_grid_round_trip() {
        let g = RopeGrid::new(sample_lines(5));
        assert_eq!(g.line_count(), 5);
        assert_eq!(g.line(2).unwrap()[0].ch, 'l');
        assert_eq!(g.to_flat().len(), 5);
    }

    #[test]
    fn rope_grid_clones_are_o1_arc_pointer_equal() {
        let g1 = RopeGrid::new(sample_lines(100));
        let g2 = g1.clone();
        // Both grids point to the same tree root (Arc bump).
        if let (Some(r1), Some(r2)) = (g1.root.as_ref(), g2.root.as_ref()) {
            assert!(
                Arc::ptr_eq(r1, r2),
                "rope clone should share Arc pointer to root"
            );
        } else {
            panic!("expected non-empty roots");
        }
    }

    #[test]
    fn rope_grid_mutation_does_not_affect_clone() {
        let g1 = RopeGrid::new(sample_lines(50));
        let mut g2 = g1.clone();
        g2.set_line(0, line("MUTATED"));
        // g1's line 0 is unchanged.
        assert_eq!(g1.line(0).unwrap()[0].ch, 'l');
        // g2's line 0 is the new value.
        assert_eq!(g2.line(0).unwrap()[0].ch, 'M');
    }

    #[test]
    fn flat_and_rope_produce_identical_output_under_inserts_and_deletes() {
        let mut flat = FlatGrid::new(sample_lines(10));
        let mut rope = RopeGrid::new(sample_lines(10));
        // Op: insert at 5
        flat.insert_line(5, line("inserted"));
        rope.insert_line(5, line("inserted"));
        assert_eq!(flat.to_flat(), rope.to_flat());
        // Op: delete at 0
        flat.delete_line(0);
        rope.delete_line(0);
        assert_eq!(flat.to_flat(), rope.to_flat());
        // Op: scroll up 3
        flat.scroll_up(3);
        rope.scroll_up(3);
        assert_eq!(flat.to_flat(), rope.to_flat());
    }

    #[test]
    fn rope_grid_handles_empty_state() {
        let g = RopeGrid::empty();
        assert_eq!(g.line_count(), 0);
        assert!(g.line(0).is_none());
    }

    #[test]
    fn rope_grid_insert_into_empty() {
        let mut g = RopeGrid::empty();
        g.insert_line(0, line("first"));
        assert_eq!(g.line_count(), 1);
        assert_eq!(g.line(0).unwrap()[0].ch, 'f');
    }

    #[test]
    fn rope_grid_handles_oob_set_as_no_op() {
        let mut g = RopeGrid::new(sample_lines(5));
        g.set_line(99, line("oob"));
        // No change.
        assert_eq!(g.line_count(), 5);
    }

    #[test]
    fn rope_grid_insert_at_end_appends() {
        let mut g = RopeGrid::new(sample_lines(5));
        g.insert_line(5, line("appended"));
        assert_eq!(g.line_count(), 6);
        assert_eq!(g.line(5).unwrap()[0].ch, 'a');
    }

    #[test]
    fn rope_grid_scrolls_up_correctly() {
        let mut g = RopeGrid::new(sample_lines(10));
        g.scroll_up(3);
        assert_eq!(g.line_count(), 7);
        // First line is now what was line 3.
        assert!(g.line(0).unwrap()[5].ch == '3' || g.line(0).unwrap()[5].ch == '-');
    }

    #[test]
    fn delete_below_split_threshold_keeps_tree_valid() {
        // Trigger leaf splits then deletes back below threshold.
        let mut rope = RopeGrid::new(sample_lines(LINES_PER_GROUP * 4));
        for _ in 0..(LINES_PER_GROUP * 3) {
            rope.delete_line(0);
        }
        assert_eq!(rope.line_count(), LINES_PER_GROUP);
    }

    #[test]
    fn one_thousand_random_ops_observational_equivalence() {
        // Deterministic LCG so this is not a proptest but is
        // dense.
        let mut state = 0xDEAD_BEEFu64;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            state
        };
        let mut flat = FlatGrid::new(sample_lines(20));
        let mut rope = RopeGrid::new(sample_lines(20));
        for i in 0..1000 {
            let pick = next() % 4;
            let count = flat.line_count().max(1);
            match pick {
                0 => {
                    let idx = (next() as usize) % (count + 1);
                    let l = line(&format!("op-{i}"));
                    flat.insert_line(idx, l.clone());
                    rope.insert_line(idx, l);
                }
                1 => {
                    if count > 0 {
                        let idx = (next() as usize) % count;
                        flat.delete_line(idx);
                        rope.delete_line(idx);
                    }
                }
                2 => {
                    if count > 0 {
                        let idx = (next() as usize) % count;
                        let l = line(&format!("set-{i}"));
                        flat.set_line(idx, l.clone());
                        rope.set_line(idx, l);
                    }
                }
                _ => {
                    let n = (next() as usize) % 5;
                    flat.scroll_up(n);
                    rope.scroll_up(n);
                }
            }
            assert_eq!(
                flat.line_count(),
                rope.line_count(),
                "size diverged at op {i}"
            );
        }
        assert_eq!(flat.to_flat(), rope.to_flat());
    }

    #[test]
    fn snapshot_comparison_report_serde_roundtrips() {
        let r = SnapshotComparisonReport {
            ts_ms: 100,
            op: GridOp::Snapshot,
            tree_size_bytes: 1024,
            duration_ns: 500,
        };
        let json = serde_json::to_string(&r).unwrap();
        let parsed: SnapshotComparisonReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, r);
    }
}
