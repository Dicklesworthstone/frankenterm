//! Per-row quad-cache invalidation analyzer (ft-mpc9b.1.5).
//!
//! The render-path overhaul retires the global "rebuild all quads
//! every frame" model in favor of per-row caching: each visible row
//! owns its quad list; only rows whose `DirtyLineBitmap` bit is set
//! get rebuilt; clean rows reuse last frame's quads.
//!
//! ## What this module ships (foundation, ft-mpc9b.1.5)
//!
//! - `RowInvalidation` — typed per-row decision (`Cached` /
//!   `Rebuild`) plus the row index. Replaces the implicit
//!   integer-pair conventions a hand-rolled implementation would
//!   accumulate.
//! - `RowInvalidationPlan` — the per-pane batch the paint loop
//!   consumes: a `Vec<RowInvalidation>` plus aggregate counters
//!   (rebuilt / cached) so `RenderReport`-style telemetry can
//!   surface cache hit-rate without re-walking the vec.
//! - `plan_from_dirty_bitmap()` — pure-logic conversion from a
//!   `DirtyLineBitmap` snapshot to a `RowInvalidationPlan`. The
//!   continuation bead wires this into the per-pane paint hot
//!   path; the function itself takes only the bitmap so it's
//!   trivially testable in isolation.
//!
//! ## What is deferred (continuation, see follow-up bead)
//!
//! - Replacing the existing `LfuCache<LineQuadCacheKey,
//!   LineQuadCacheValue>` (in `crates/frankenterm-gui/src/termwindow/mod.rs`
//!   field `line_quad_cache`) with a row-indexed `Vec<Quad>` per
//!   the bead's spec. The LFU eviction policy in place today is
//!   conceptually adjacent but the row-indexed model is more
//!   appropriate when paired with `DirtyLineBitmap`'s deterministic
//!   row addressing.
//! - The frame-build path that walks `RowInvalidationPlan` and
//!   emits the actual quads.
//! - Cache-hit-rate telemetry surfaced via `ft doctor`.
//! - The bench at `crates/frankenterm-core/benches/quad_cache_typing.rs`
//!   that asserts ≥95% rows cache-hit per frame under the
//!   1-char-per-second typing pattern (ft-mpc9b.7 RQ-S8).

#![allow(dead_code)]

use super::dirty_lines::DirtyLineBitmap;

/// Per-row decision the paint loop consumes. Either reuse the
/// cached quad list for this row (`Cached`) or rebuild it because
/// the row's content has changed since last frame (`Rebuild`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowDecision {
    /// Row is clean; reuse the cached `Vec<Quad>`.
    Cached,
    /// Row is dirty; the paint loop must rebuild its quads from
    /// the current cell contents.
    Rebuild,
}

/// A single per-row decision with the row index attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowInvalidation {
    pub row: u32,
    pub decision: RowDecision,
}

/// The per-pane batch returned from `plan_from_dirty_bitmap`. The
/// vec holds one entry per addressable row; the counters give the
/// paint loop O(1) access to telemetry without re-walking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowInvalidationPlan {
    pub rows: Vec<RowInvalidation>,
    pub rebuilt_count: u32,
    pub cached_count: u32,
}

impl RowInvalidationPlan {
    /// Total rows the plan covers (`rebuilt_count + cached_count`).
    pub fn total(&self) -> u32 {
        self.rebuilt_count.saturating_add(self.cached_count)
    }

    /// Cache hit-rate as a fraction of `total()`. Returns 1.0 when
    /// `total()` is 0 (vacuously every row was cached) so the
    /// telemetry path doesn't have to special-case empty panes.
    pub fn hit_rate(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            return 1.0;
        }
        f64::from(self.cached_count) / f64::from(total)
    }

    /// Whether at least one row needs a rebuild this frame.
    pub fn has_dirty_rows(&self) -> bool {
        self.rebuilt_count > 0
    }
}

/// Build a `RowInvalidationPlan` for a pane from its
/// `DirtyLineBitmap` snapshot. Rows in `0..bitmap.capacity()` are
/// each assigned `Rebuild` if their bit is set, `Cached`
/// otherwise.
///
/// The bitmap snapshot is borrowed immutably; the paint loop is
/// expected to call this once per pane per frame, then either
/// `clear()` the bitmap at frame end (continuation work) or rely
/// on the next frame's mark calls to drive incremental
/// invalidation.
pub fn plan_from_dirty_bitmap(bitmap: &DirtyLineBitmap) -> RowInvalidationPlan {
    let capacity = bitmap.capacity();
    let mut rows = Vec::with_capacity(capacity);
    let mut rebuilt_count: u32 = 0;
    let mut cached_count: u32 = 0;
    for row in 0..capacity {
        let decision = if bitmap.contains(row) {
            rebuilt_count = rebuilt_count.saturating_add(1);
            RowDecision::Rebuild
        } else {
            cached_count = cached_count.saturating_add(1);
            RowDecision::Cached
        };
        rows.push(RowInvalidation {
            row: row as u32,
            decision,
        });
    }
    RowInvalidationPlan {
        rows,
        rebuilt_count,
        cached_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bitmap_yields_empty_plan() {
        let bm = DirtyLineBitmap::new(0);
        let plan = plan_from_dirty_bitmap(&bm);
        assert_eq!(plan.rows.len(), 0);
        assert_eq!(plan.total(), 0);
        assert_eq!(plan.rebuilt_count, 0);
        assert_eq!(plan.cached_count, 0);
        // Vacuous hit rate is 1.0 — no rows means no rebuilds.
        assert!((plan.hit_rate() - 1.0).abs() < 1e-9);
        assert!(!plan.has_dirty_rows());
    }

    #[test]
    fn fully_clean_bitmap_caches_every_row() {
        let bm = DirtyLineBitmap::new(24);
        let plan = plan_from_dirty_bitmap(&bm);
        assert_eq!(plan.rows.len(), 24);
        assert_eq!(plan.cached_count, 24);
        assert_eq!(plan.rebuilt_count, 0);
        assert_eq!(plan.total(), 24);
        assert!((plan.hit_rate() - 1.0).abs() < 1e-9);
        assert!(!plan.has_dirty_rows());
        for inv in &plan.rows {
            assert_eq!(inv.decision, RowDecision::Cached);
        }
    }

    #[test]
    fn fully_dirty_bitmap_rebuilds_every_row() {
        let mut bm = DirtyLineBitmap::new(24);
        bm.mark_all();
        let plan = plan_from_dirty_bitmap(&bm);
        assert_eq!(plan.rebuilt_count, 24);
        assert_eq!(plan.cached_count, 0);
        assert!((plan.hit_rate() - 0.0).abs() < 1e-9);
        assert!(plan.has_dirty_rows());
    }

    #[test]
    fn single_dirty_row_yields_one_rebuild_among_cached() {
        // Models the bead's headline win: typing 1 char/sec → 1
        // row rebuilt, the rest cached.
        let mut bm = DirtyLineBitmap::new(24);
        bm.mark(7);
        let plan = plan_from_dirty_bitmap(&bm);
        assert_eq!(plan.rebuilt_count, 1);
        assert_eq!(plan.cached_count, 23);
        assert_eq!(plan.rows[7].decision, RowDecision::Rebuild);
        for (i, inv) in plan.rows.iter().enumerate() {
            if i != 7 {
                assert_eq!(inv.decision, RowDecision::Cached);
            }
        }
    }

    #[test]
    fn multi_dirty_rows_partition_correctly() {
        let mut bm = DirtyLineBitmap::new(80);
        for row in [0, 1, 5, 17, 79] {
            bm.mark(row);
        }
        let plan = plan_from_dirty_bitmap(&bm);
        assert_eq!(plan.rebuilt_count, 5);
        assert_eq!(plan.cached_count, 75);
        assert_eq!(plan.total(), 80);

        for &row in &[0u32, 1, 5, 17, 79] {
            assert_eq!(
                plan.rows[row as usize].decision,
                RowDecision::Rebuild,
                "row {} should rebuild",
                row
            );
        }
    }

    #[test]
    fn row_indices_are_in_ascending_order() {
        let mut bm = DirtyLineBitmap::new(64);
        bm.mark(40);
        bm.mark(10);
        bm.mark(0);
        let plan = plan_from_dirty_bitmap(&bm);
        // The plan walks rows 0..capacity in order — even though
        // the bitmap got marks in arbitrary order, the plan is
        // sorted because the loop iterates row indices.
        for (i, inv) in plan.rows.iter().enumerate() {
            assert_eq!(inv.row, i as u32);
        }
    }

    #[test]
    fn hit_rate_is_a_proper_fraction_in_range() {
        let mut bm = DirtyLineBitmap::new(100);
        for row in 0..25 {
            bm.mark(row);
        }
        let plan = plan_from_dirty_bitmap(&bm);
        assert_eq!(plan.rebuilt_count, 25);
        assert_eq!(plan.cached_count, 75);
        assert!((plan.hit_rate() - 0.75).abs() < 1e-9);
        assert!(plan.has_dirty_rows());
    }

    #[test]
    fn plan_round_trips_after_clear() {
        // Mark, plan, clear bitmap, plan again — second plan
        // should report all-cached.
        let mut bm = DirtyLineBitmap::new(16);
        bm.mark_range(2..7);
        let plan_a = plan_from_dirty_bitmap(&bm);
        assert_eq!(plan_a.rebuilt_count, 5);

        bm.clear();
        let plan_b = plan_from_dirty_bitmap(&bm);
        assert_eq!(plan_b.rebuilt_count, 0);
        assert_eq!(plan_b.cached_count, 16);
    }

    #[test]
    fn plan_responds_to_resize_grow() {
        let mut bm = DirtyLineBitmap::new(8);
        bm.mark(3);
        let plan_small = plan_from_dirty_bitmap(&bm);
        assert_eq!(plan_small.rows.len(), 8);
        assert_eq!(plan_small.rebuilt_count, 1);

        bm.resize(16);
        let plan_grown = plan_from_dirty_bitmap(&bm);
        // After resize-grow, the original mark survives and the
        // 8 new rows are clean.
        assert_eq!(plan_grown.rows.len(), 16);
        assert_eq!(plan_grown.rebuilt_count, 1);
        assert_eq!(plan_grown.cached_count, 15);
        assert_eq!(plan_grown.rows[3].decision, RowDecision::Rebuild);
    }

    #[test]
    fn plan_responds_to_resize_shrink() {
        let mut bm = DirtyLineBitmap::new(32);
        bm.mark(20);
        bm.mark(31);
        let plan_full = plan_from_dirty_bitmap(&bm);
        assert_eq!(plan_full.rebuilt_count, 2);

        bm.resize(16);
        let plan_shrunk = plan_from_dirty_bitmap(&bm);
        // The mark at row 20 + 31 are past the new capacity and
        // are dropped by the bitmap's own shrink logic.
        assert_eq!(plan_shrunk.rows.len(), 16);
        assert_eq!(plan_shrunk.rebuilt_count, 0);
    }

    #[test]
    fn typing_1_char_per_second_pattern_hits_95_percent_target() {
        // The bead's headline acceptance target: "type 1 char per
        // second for 60 sec, assert ≥95% rows are cache hits per
        // frame". We model 24 visible rows + 1 dirty row per
        // frame; the cache hit rate must be 23/24 ≈ 0.958.
        let mut bm = DirtyLineBitmap::new(24);
        bm.mark(15); // single cell typed → its row dirty
        let plan = plan_from_dirty_bitmap(&bm);
        let hr = plan.hit_rate();
        assert!(
            hr >= 0.95,
            "1-char-per-second hit rate {} below 0.95 target",
            hr
        );
    }
}
