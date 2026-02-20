//! Sorted set of non-overlapping intervals with automatic merge-on-insert.
//!
//! Maintains a collection of disjoint `[start, end)` intervals in sorted order.
//! When a new interval is inserted, any overlapping or adjacent intervals are
//! merged automatically. All operations maintain the invariant that intervals
//! are sorted and non-overlapping.
//!
//! # Use Cases
//!
//! - Track active time windows for pane sessions
//! - Gap detection in telemetry streams
//! - Coalesce overlapping output capture byte ranges
//! - Resource occupancy tracking across agent swarms
//!
//! # Complexity
//!
//! | Operation    | Time          |
//! |-------------|---------------|
//! | `insert`    | O(n) worst    |
//! | `contains`  | O(log n)      |
//! | `intersects`| O(log n)      |
//! | `remove`    | O(n) worst    |
//! | `span`      | O(n)          |
//! | `gaps`      | O(n)          |
//!
//! Bead: ft-283h4.31

use serde::{Deserialize, Serialize};

/// A half-open interval `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Interval {
    /// Inclusive start.
    pub start: i64,
    /// Exclusive end.
    pub end: i64,
}

impl Interval {
    /// Create a new interval `[start, end)`.
    ///
    /// # Panics
    ///
    /// Panics if `start > end`.
    #[must_use]
    pub fn new(start: i64, end: i64) -> Self {
        assert!(start <= end, "start {start} > end {end}");
        Self { start, end }
    }

    /// Length of the interval.
    #[must_use]
    pub fn len(&self) -> i64 {
        self.end - self.start
    }

    /// Whether the interval is empty (zero length).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Whether this interval contains a point.
    #[must_use]
    pub fn contains_point(&self, point: i64) -> bool {
        self.start <= point && point < self.end
    }

    /// Whether this interval overlaps or is adjacent to another.
    #[must_use]
    pub fn overlaps_or_adjacent(&self, other: &Interval) -> bool {
        self.start <= other.end && other.start <= self.end
    }

    /// Whether this interval strictly overlaps another (not just adjacent).
    #[must_use]
    pub fn overlaps(&self, other: &Interval) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// Merge with another interval (union).
    #[must_use]
    pub fn merge(&self, other: &Interval) -> Interval {
        Interval {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// Configuration for `DisjointIntervals`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisjointIntervalsConfig {
    /// Whether to merge adjacent (touching) intervals.
    pub merge_adjacent: bool,
}

impl Default for DisjointIntervalsConfig {
    fn default() -> Self {
        Self {
            merge_adjacent: true,
        }
    }
}

/// Statistics about a `DisjointIntervals` set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisjointIntervalsStats {
    /// Number of disjoint intervals.
    pub interval_count: usize,
    /// Total span covered by all intervals.
    pub total_span: i64,
    /// Number of insert operations.
    pub insert_count: u64,
    /// Number of remove operations.
    pub remove_count: u64,
    /// Approximate memory usage in bytes.
    pub memory_bytes: usize,
}

/// A sorted set of non-overlapping intervals with merge-on-insert.
///
/// # Example
///
/// ```
/// use frankenterm_core::disjoint_intervals::DisjointIntervals;
///
/// let mut di = DisjointIntervals::new();
/// di.insert(1, 5);   // [1, 5)
/// di.insert(3, 8);   // merged: [1, 8)
/// di.insert(10, 15); // [1, 8), [10, 15)
///
/// assert!(di.contains(3));
/// assert!(!di.contains(9));
/// assert_eq!(di.count(), 2);
/// assert_eq!(di.span(), 12);  // 7 + 5
/// ```
#[derive(Debug, Clone)]
pub struct DisjointIntervals {
    /// Sorted, non-overlapping intervals.
    intervals: Vec<Interval>,
    /// Whether to merge adjacent (touching) intervals.
    merge_adjacent: bool,
    /// Operation counters.
    insert_ops: u64,
    remove_ops: u64,
}

impl DisjointIntervals {
    /// Create an empty interval set (merges adjacent by default).
    #[must_use]
    pub fn new() -> Self {
        Self {
            intervals: Vec::new(),
            merge_adjacent: true,
            insert_ops: 0,
            remove_ops: 0,
        }
    }

    /// Create from config.
    #[must_use]
    pub fn from_config(config: &DisjointIntervalsConfig) -> Self {
        Self {
            intervals: Vec::new(),
            merge_adjacent: config.merge_adjacent,
            insert_ops: 0,
            remove_ops: 0,
        }
    }

    /// Number of disjoint intervals.
    #[must_use]
    pub fn count(&self) -> usize {
        self.intervals.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.intervals.is_empty()
    }

    /// Insert interval `[start, end)`. Merges with overlapping/adjacent intervals.
    ///
    /// Empty intervals (start == end) are ignored.
    ///
    /// # Panics
    ///
    /// Panics if `start > end`.
    pub fn insert(&mut self, start: i64, end: i64) {
        assert!(start <= end, "start {start} > end {end}");
        self.insert_ops += 1;

        if start == end {
            return; // empty interval
        }

        let mut new_interval = Interval::new(start, end);

        // Find all intervals that overlap or are adjacent
        let mut merged = Vec::new();
        for iv in &self.intervals {
            if self.should_merge(iv, &new_interval) {
                new_interval = new_interval.merge(iv);
            } else {
                merged.push(*iv);
            }
        }

        // Insert the merged interval in sorted position
        let pos = merged.partition_point(|iv| iv.start < new_interval.start);
        merged.insert(pos, new_interval);
        self.intervals = merged;
    }

    /// Check whether two intervals should be merged.
    fn should_merge(&self, a: &Interval, b: &Interval) -> bool {
        if self.merge_adjacent {
            a.overlaps_or_adjacent(b)
        } else {
            a.overlaps(b)
        }
    }

    /// Check if a point is contained in any interval.
    #[must_use]
    pub fn contains(&self, point: i64) -> bool {
        // Binary search for the interval that might contain the point
        let idx = self.intervals.partition_point(|iv| iv.end <= point);
        if idx < self.intervals.len() {
            self.intervals[idx].contains_point(point)
        } else {
            false
        }
    }

    /// Check if any stored interval overlaps with `[start, end)`.
    #[must_use]
    pub fn intersects(&self, start: i64, end: i64) -> bool {
        if start >= end {
            return false;
        }
        let query = Interval::new(start, end);
        // Find first interval that could overlap
        let idx = self.intervals.partition_point(|iv| iv.end <= start);
        if idx < self.intervals.len() {
            self.intervals[idx].overlaps(&query)
        } else {
            false
        }
    }

    /// Remove interval `[start, end)` — punch a hole in existing intervals.
    ///
    /// # Panics
    ///
    /// Panics if `start > end`.
    pub fn remove(&mut self, start: i64, end: i64) {
        assert!(start <= end, "start {start} > end {end}");
        self.remove_ops += 1;

        if start == end {
            return;
        }

        let hole = Interval::new(start, end);
        let mut result = Vec::new();

        for iv in &self.intervals {
            if !iv.overlaps(&hole) {
                // No overlap — keep as is
                result.push(*iv);
            } else {
                // Split: keep parts outside the hole
                if iv.start < hole.start {
                    result.push(Interval::new(iv.start, hole.start));
                }
                if iv.end > hole.end {
                    result.push(Interval::new(hole.end, iv.end));
                }
            }
        }

        self.intervals = result;
    }

    /// Total covered span (sum of all interval lengths).
    #[must_use]
    pub fn span(&self) -> i64 {
        self.intervals.iter().map(Interval::len).sum()
    }

    /// Get all intervals as a slice.
    #[must_use]
    pub fn intervals(&self) -> &[Interval] {
        &self.intervals
    }

    /// Get the gaps between intervals within `[lo, hi)`.
    #[must_use]
    pub fn gaps(&self, lo: i64, hi: i64) -> Vec<Interval> {
        if lo >= hi {
            return Vec::new();
        }
        let mut result = Vec::new();
        let mut current = lo;

        for iv in &self.intervals {
            if iv.start >= hi {
                break;
            }
            if iv.end <= lo {
                continue;
            }
            let gap_start = current.max(lo);
            let gap_end = iv.start.min(hi);
            if gap_start < gap_end {
                result.push(Interval::new(gap_start, gap_end));
            }
            current = iv.end;
        }

        // Final gap after last interval
        let final_start = current.max(lo);
        if final_start < hi {
            result.push(Interval::new(final_start, hi));
        }

        result
    }

    /// Smallest start across all intervals, or `None` if empty.
    #[must_use]
    pub fn min_start(&self) -> Option<i64> {
        self.intervals.first().map(|iv| iv.start)
    }

    /// Largest end across all intervals, or `None` if empty.
    #[must_use]
    pub fn max_end(&self) -> Option<i64> {
        self.intervals.last().map(|iv| iv.end)
    }

    /// Clear all intervals.
    pub fn clear(&mut self) {
        self.intervals.clear();
    }

    /// Get statistics.
    #[must_use]
    pub fn stats(&self) -> DisjointIntervalsStats {
        DisjointIntervalsStats {
            interval_count: self.intervals.len(),
            total_span: self.span(),
            insert_count: self.insert_ops,
            remove_count: self.remove_ops,
            memory_bytes: self.memory_bytes(),
        }
    }

    /// Approximate memory usage.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.intervals.capacity() * std::mem::size_of::<Interval>()
    }
}

impl Default for DisjointIntervals {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty() {
        let di = DisjointIntervals::new();
        assert!(di.is_empty());
        assert_eq!(di.count(), 0);
        assert_eq!(di.span(), 0);
    }

    #[test]
    fn test_single_insert() {
        let mut di = DisjointIntervals::new();
        di.insert(1, 5);
        assert_eq!(di.count(), 1);
        assert_eq!(di.span(), 4);
        assert!(di.contains(1));
        assert!(di.contains(4));
        assert!(!di.contains(5));
        assert!(!di.contains(0));
    }

    #[test]
    fn test_non_overlapping() {
        let mut di = DisjointIntervals::new();
        di.insert(1, 3);
        di.insert(5, 8);
        di.insert(10, 12);
        assert_eq!(di.count(), 3);
        assert_eq!(di.span(), 7);
    }

    #[test]
    fn test_merge_overlapping() {
        let mut di = DisjointIntervals::new();
        di.insert(1, 5);
        di.insert(3, 8);
        assert_eq!(di.count(), 1);
        assert_eq!(di.intervals()[0], Interval::new(1, 8));
    }

    #[test]
    fn test_merge_adjacent() {
        let mut di = DisjointIntervals::new();
        di.insert(1, 5);
        di.insert(5, 8);
        assert_eq!(di.count(), 1);
        assert_eq!(di.intervals()[0], Interval::new(1, 8));
    }

    #[test]
    fn test_merge_superset() {
        let mut di = DisjointIntervals::new();
        di.insert(2, 4);
        di.insert(6, 8);
        di.insert(1, 10); // covers both
        assert_eq!(di.count(), 1);
        assert_eq!(di.intervals()[0], Interval::new(1, 10));
    }

    #[test]
    fn test_merge_chain() {
        let mut di = DisjointIntervals::new();
        di.insert(1, 3);
        di.insert(5, 7);
        di.insert(9, 11);
        di.insert(2, 10); // bridges all three
        assert_eq!(di.count(), 1);
        assert_eq!(di.intervals()[0], Interval::new(1, 11));
    }

    #[test]
    fn test_contains() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 5);
        di.insert(10, 15);
        assert!(di.contains(0));
        assert!(di.contains(4));
        assert!(!di.contains(5));
        assert!(!di.contains(7));
        assert!(di.contains(10));
        assert!(di.contains(14));
        assert!(!di.contains(15));
    }

    #[test]
    fn test_intersects() {
        let mut di = DisjointIntervals::new();
        di.insert(5, 10);
        assert!(di.intersects(3, 7));
        assert!(di.intersects(7, 12));
        assert!(di.intersects(5, 10));
        assert!(di.intersects(6, 8));
        assert!(!di.intersects(0, 5));
        assert!(!di.intersects(10, 15));
        assert!(!di.intersects(0, 3));
    }

    #[test]
    fn test_remove_middle() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        di.remove(3, 7);
        assert_eq!(di.count(), 2);
        assert_eq!(di.intervals()[0], Interval::new(0, 3));
        assert_eq!(di.intervals()[1], Interval::new(7, 10));
    }

    #[test]
    fn test_remove_left_edge() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        di.remove(0, 5);
        assert_eq!(di.count(), 1);
        assert_eq!(di.intervals()[0], Interval::new(5, 10));
    }

    #[test]
    fn test_remove_right_edge() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        di.remove(5, 10);
        assert_eq!(di.count(), 1);
        assert_eq!(di.intervals()[0], Interval::new(0, 5));
    }

    #[test]
    fn test_remove_entire() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        di.remove(0, 10);
        assert!(di.is_empty());
    }

    #[test]
    fn test_remove_no_overlap() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 5);
        di.remove(10, 20);
        assert_eq!(di.count(), 1);
    }

    #[test]
    fn test_gaps() {
        let mut di = DisjointIntervals::new();
        di.insert(2, 4);
        di.insert(7, 9);

        let gaps = di.gaps(0, 10);
        assert_eq!(gaps.len(), 3);
        assert_eq!(gaps[0], Interval::new(0, 2));
        assert_eq!(gaps[1], Interval::new(4, 7));
        assert_eq!(gaps[2], Interval::new(9, 10));
    }

    #[test]
    fn test_gaps_no_gaps() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        let gaps = di.gaps(0, 10);
        assert!(gaps.is_empty());
    }

    #[test]
    fn test_min_max() {
        let mut di = DisjointIntervals::new();
        assert_eq!(di.min_start(), None);
        assert_eq!(di.max_end(), None);

        di.insert(5, 10);
        di.insert(20, 30);
        assert_eq!(di.min_start(), Some(5));
        assert_eq!(di.max_end(), Some(30));
    }

    #[test]
    fn test_clear() {
        let mut di = DisjointIntervals::new();
        di.insert(1, 5);
        di.insert(10, 20);
        di.clear();
        assert!(di.is_empty());
    }

    #[test]
    fn test_empty_interval_ignored() {
        let mut di = DisjointIntervals::new();
        di.insert(5, 5); // empty
        assert!(di.is_empty());
    }

    #[test]
    fn test_negative_intervals() {
        let mut di = DisjointIntervals::new();
        di.insert(-10, -5);
        di.insert(-3, 3);
        assert_eq!(di.count(), 2);
        assert!(di.contains(-7));
        assert!(di.contains(0));
        assert!(!di.contains(-4));
    }

    #[test]
    fn test_config_serde() {
        let config = DisjointIntervalsConfig {
            merge_adjacent: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: DisjointIntervalsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn test_stats_serde() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        let stats = di.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: DisjointIntervalsStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn test_stats() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 5);
        di.insert(10, 20);
        di.remove(12, 15);
        let stats = di.stats();
        assert_eq!(stats.interval_count, 3);
        assert_eq!(stats.insert_count, 2);
        assert_eq!(stats.remove_count, 1);
    }

    #[test]
    fn test_clone_independence() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        let mut clone = di.clone();
        clone.insert(20, 30);
        assert_eq!(di.count(), 1);
        assert_eq!(clone.count(), 2);
    }

    #[test]
    fn test_interval_properties() {
        let iv = Interval::new(5, 10);
        assert_eq!(iv.len(), 5);
        assert!(!iv.is_empty());
        assert!(iv.contains_point(5));
        assert!(!iv.contains_point(10));

        let empty = Interval::new(3, 3);
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn test_insert_order_independent() {
        let mut di1 = DisjointIntervals::new();
        di1.insert(1, 5);
        di1.insert(3, 8);
        di1.insert(10, 15);

        let mut di2 = DisjointIntervals::new();
        di2.insert(10, 15);
        di2.insert(3, 8);
        di2.insert(1, 5);

        assert_eq!(di1.intervals(), di2.intervals());
    }

    // ── Expanded coverage ──────────────────────────────────────────

    #[test]
    fn default_is_empty() {
        let di = DisjointIntervals::default();
        assert!(di.is_empty());
        assert_eq!(di.count(), 0);
        assert_eq!(di.span(), 0);
    }

    #[test]
    fn from_config_merge_adjacent_false() {
        let config = DisjointIntervalsConfig {
            merge_adjacent: false,
        };
        let mut di = DisjointIntervals::from_config(&config);
        di.insert(1, 5);
        di.insert(5, 8); // adjacent but shouldn't merge
        assert_eq!(di.count(), 2);
        assert_eq!(di.intervals()[0], Interval::new(1, 5));
        assert_eq!(di.intervals()[1], Interval::new(5, 8));
    }

    #[test]
    fn from_config_merge_adjacent_false_overlapping_still_merges() {
        let config = DisjointIntervalsConfig {
            merge_adjacent: false,
        };
        let mut di = DisjointIntervals::from_config(&config);
        di.insert(1, 6);
        di.insert(5, 8); // overlapping should still merge
        assert_eq!(di.count(), 1);
        assert_eq!(di.intervals()[0], Interval::new(1, 8));
    }

    #[test]
    fn insert_duplicate_same_interval() {
        let mut di = DisjointIntervals::new();
        di.insert(1, 5);
        di.insert(1, 5);
        assert_eq!(di.count(), 1);
        assert_eq!(di.span(), 4);
    }

    #[test]
    fn insert_subset_no_change() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        di.insert(3, 7); // subset
        assert_eq!(di.count(), 1);
        assert_eq!(di.intervals()[0], Interval::new(0, 10));
    }

    #[test]
    fn insert_reverse_order() {
        let mut di = DisjointIntervals::new();
        di.insert(10, 15);
        di.insert(5, 8);
        di.insert(0, 3);
        assert_eq!(di.count(), 3);
        assert_eq!(di.intervals()[0], Interval::new(0, 3));
        assert_eq!(di.intervals()[1], Interval::new(5, 8));
        assert_eq!(di.intervals()[2], Interval::new(10, 15));
    }

    #[test]
    #[should_panic(expected = "start")]
    fn insert_start_greater_than_end_panics() {
        let mut di = DisjointIntervals::new();
        di.insert(10, 5);
    }

    #[test]
    #[should_panic(expected = "start")]
    fn remove_start_greater_than_end_panics() {
        let mut di = DisjointIntervals::new();
        di.remove(10, 5);
    }

    #[test]
    fn remove_empty_interval_noop() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        di.remove(5, 5);
        assert_eq!(di.count(), 1);
        assert_eq!(di.intervals()[0], Interval::new(0, 10));
    }

    #[test]
    fn remove_spanning_multiple_intervals() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 3);
        di.insert(5, 8);
        di.insert(10, 13);
        di.remove(2, 11); // spans all three
        assert_eq!(di.count(), 2);
        assert_eq!(di.intervals()[0], Interval::new(0, 2));
        assert_eq!(di.intervals()[1], Interval::new(11, 13));
    }

    #[test]
    fn remove_superset_of_all() {
        let mut di = DisjointIntervals::new();
        di.insert(5, 10);
        di.insert(15, 20);
        di.remove(0, 100);
        assert!(di.is_empty());
    }

    #[test]
    fn gaps_empty_set() {
        let di = DisjointIntervals::new();
        let gaps = di.gaps(0, 10);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0], Interval::new(0, 10));
    }

    #[test]
    fn gaps_invalid_range() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        let gaps = di.gaps(10, 5); // lo >= hi
        assert!(gaps.is_empty());
    }

    #[test]
    fn gaps_intervals_extend_beyond_query() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 20); // extends beyond [5, 15)
        let gaps = di.gaps(5, 15);
        assert!(gaps.is_empty());
    }

    #[test]
    fn gaps_partial_overlap_left() {
        let mut di = DisjointIntervals::new();
        di.insert(3, 8); // partially inside [0, 10)
        let gaps = di.gaps(0, 10);
        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0], Interval::new(0, 3));
        assert_eq!(gaps[1], Interval::new(8, 10));
    }

    #[test]
    fn contains_boundary_points() {
        let mut di = DisjointIntervals::new();
        di.insert(10, 20);
        assert!(di.contains(10)); // start inclusive
        assert!(di.contains(19)); // end - 1
        assert!(!di.contains(20)); // end exclusive
        assert!(!di.contains(9));
    }

    #[test]
    fn contains_empty_set() {
        let di = DisjointIntervals::new();
        assert!(!di.contains(0));
        assert!(!di.contains(100));
    }

    #[test]
    fn intersects_empty_range() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        assert!(!di.intersects(5, 5)); // empty query
    }

    #[test]
    fn intersects_empty_set() {
        let di = DisjointIntervals::new();
        assert!(!di.intersects(0, 10));
    }

    #[test]
    fn min_max_after_removal() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 5);
        di.insert(10, 20);
        di.insert(30, 40);
        di.remove(0, 5); // remove first
        assert_eq!(di.min_start(), Some(10));
        di.remove(30, 40); // remove last
        assert_eq!(di.max_end(), Some(20));
    }

    #[test]
    fn interval_overlaps_vs_overlaps_or_adjacent() {
        let a = Interval::new(0, 5);
        let b = Interval::new(5, 10); // adjacent
        assert!(!a.overlaps(&b), "adjacent should not strictly overlap");
        assert!(
            a.overlaps_or_adjacent(&b),
            "adjacent should overlap-or-adjacent"
        );

        let c = Interval::new(4, 10); // overlapping
        assert!(a.overlaps(&c));
        assert!(a.overlaps_or_adjacent(&c));

        let d = Interval::new(6, 10); // disjoint
        assert!(!a.overlaps(&d));
        assert!(!a.overlaps_or_adjacent(&d));
    }

    #[test]
    fn interval_merge() {
        let a = Interval::new(0, 5);
        let b = Interval::new(3, 10);
        let merged = a.merge(&b);
        assert_eq!(merged, Interval::new(0, 10));
    }

    #[test]
    fn interval_serde() {
        let iv = Interval::new(42, 100);
        let json = serde_json::to_string(&iv).unwrap();
        let back: Interval = serde_json::from_str(&json).unwrap();
        assert_eq!(iv, back);
    }

    #[test]
    fn interval_ord() {
        let a = Interval::new(0, 5);
        let b = Interval::new(1, 3);
        let c = Interval::new(0, 10);
        assert!(a < b);
        assert!(a < c);
    }

    #[test]
    #[should_panic(expected = "start")]
    fn interval_invalid_panics() {
        let _discard = Interval::new(10, 5);
    }

    #[test]
    fn stats_memory_bytes_positive() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        let stats = di.stats();
        assert!(stats.memory_bytes > 0);
    }

    #[test]
    fn config_default_merge_adjacent_true() {
        let config = DisjointIntervalsConfig::default();
        assert!(config.merge_adjacent);
    }

    #[test]
    fn stress_many_inserts() {
        let mut di = DisjointIntervals::new();
        for i in (0..100).step_by(3) {
            di.insert(i, i + 2);
        }
        // Each [i, i+2) is separated by a gap of 1
        assert_eq!(di.count(), 34);
        assert_eq!(di.span(), 68); // 34 * 2
    }

    #[test]
    fn stress_merge_all() {
        let mut di = DisjointIntervals::new();
        for i in 0..50 {
            di.insert(i * 2, i * 2 + 2); // [0,2), [2,4), [4,6)...
        }
        // All adjacent, should merge into one
        assert_eq!(di.count(), 1);
        assert_eq!(di.intervals()[0], Interval::new(0, 100));
    }

    #[test]
    fn clear_resets_intervals_not_stats() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        di.remove(3, 7);
        di.clear();
        assert!(di.is_empty());
        assert_eq!(di.span(), 0);
        let stats = di.stats();
        // insert and remove ops are still tracked
        assert_eq!(stats.insert_count, 1);
        assert_eq!(stats.remove_count, 1);
    }

    #[test]
    fn insert_after_clear() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        di.clear();
        di.insert(20, 30);
        assert_eq!(di.count(), 1);
        assert_eq!(di.intervals()[0], Interval::new(20, 30));
    }

    #[test]
    fn debug_format() {
        let di = DisjointIntervals::new();
        let dbg = format!("{:?}", di);
        assert!(dbg.contains("DisjointIntervals"));
    }

    #[test]
    fn intervals_accessor_sorted() {
        let mut di = DisjointIntervals::new();
        di.insert(20, 30);
        di.insert(0, 5);
        di.insert(10, 15);
        let ivs = di.intervals();
        for w in ivs.windows(2) {
            assert!(w[0].start < w[1].start);
        }
    }

    #[test]
    fn remove_then_reinsert() {
        let mut di = DisjointIntervals::new();
        di.insert(0, 10);
        di.remove(0, 10);
        assert!(di.is_empty());
        di.insert(0, 10);
        assert_eq!(di.count(), 1);
    }
}
