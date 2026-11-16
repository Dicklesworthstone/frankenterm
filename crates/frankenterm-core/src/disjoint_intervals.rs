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

    // ── Proptest properties ──────────────────────────────────────────

    mod proptest_disjoint_intervals {
        use super::*;
        use proptest::prelude::*;

        /// Generate a valid interval [start, end) where start < end.
        fn arb_interval() -> impl Strategy<Value = (i64, i64)> {
            (-1000i64..1000i64).prop_flat_map(|start| (Just(start), (start + 1)..=(start + 200)))
        }

        /// Generate a possibly-empty interval [start, end) where start <= end.
        fn arb_interval_or_empty() -> impl Strategy<Value = (i64, i64)> {
            (-1000i64..1000i64).prop_flat_map(|start| (Just(start), start..=(start + 200)))
        }

        /// Generate a sequence of insert operations.
        fn arb_inserts(max_len: usize) -> impl Strategy<Value = Vec<(i64, i64)>> {
            proptest::collection::vec(arb_interval(), 1..=max_len)
        }

        /// Helper: check that intervals are sorted and non-overlapping.
        fn assert_invariants(di: &DisjointIntervals) {
            let ivs = di.intervals();
            for iv in ivs {
                assert!(iv.start < iv.end, "empty interval found: {iv:?}");
            }
            for w in ivs.windows(2) {
                assert!(
                    w[0].end <= w[1].start,
                    "intervals overlap or not sorted: {:?}, {:?}",
                    w[0],
                    w[1]
                );
            }
        }

        /// Stricter check for merge_adjacent=true: no adjacent intervals.
        fn assert_invariants_merge_adjacent(di: &DisjointIntervals) {
            assert_invariants(di);
            let ivs = di.intervals();
            for w in ivs.windows(2) {
                assert!(
                    w[0].end < w[1].start,
                    "adjacent intervals not merged: {:?}, {:?}",
                    w[0],
                    w[1]
                );
            }
        }

        proptest! {
            /// After any sequence of inserts, the sorted-disjoint invariant holds.
            #[test]
            fn sorted_disjoint_after_inserts(ops in arb_inserts(30)) {
                let mut di = DisjointIntervals::new();
                for (s, e) in &ops {
                    di.insert(*s, *e);
                }
                assert_invariants_merge_adjacent(&di);
            }

            /// After any sequence of inserts and removes, the invariant holds.
            #[test]
            fn sorted_disjoint_after_inserts_and_removes(
                inserts in arb_inserts(20),
                removes in proptest::collection::vec(arb_interval_or_empty(), 0..10)
            ) {
                let mut di = DisjointIntervals::new();
                for (s, e) in &inserts {
                    di.insert(*s, *e);
                }
                for (s, e) in &removes {
                    di.remove(*s, *e);
                }
                assert_invariants_merge_adjacent(&di);
            }

            /// Insert order does not affect the final interval set.
            #[test]
            fn insert_order_independence(ops in arb_inserts(15)) {
                let mut di_fwd = DisjointIntervals::new();
                for (s, e) in &ops {
                    di_fwd.insert(*s, *e);
                }

                let mut di_rev = DisjointIntervals::new();
                for (s, e) in ops.iter().rev() {
                    di_rev.insert(*s, *e);
                }

                prop_assert_eq!(di_fwd.intervals(), di_rev.intervals());
            }

            /// Every point in an inserted interval is contained.
            #[test]
            fn contains_all_inserted_points((start, end) in arb_interval()) {
                let mut di = DisjointIntervals::new();
                di.insert(start, end);
                for p in start..end {
                    prop_assert!(di.contains(p), "point {} not found in [{}, {})", p, start, end);
                }
                // Boundary: end is exclusive
                let check = !di.contains(end);
                prop_assert!(check, "end point {} should not be contained", end);
            }

            /// After removing an interval, no point in that range is contained.
            #[test]
            fn remove_clears_all_points(
                (ins_s, ins_e) in arb_interval(),
                (rem_s, rem_e) in arb_interval()
            ) {
                let mut di = DisjointIntervals::new();
                di.insert(ins_s, ins_e);
                di.remove(rem_s, rem_e);
                for p in rem_s..rem_e {
                    if ins_s <= p && p < ins_e {
                        let check = !di.contains(p);
                        prop_assert!(check, "removed point {} still contained", p);
                    }
                }
                assert_invariants_merge_adjacent(&di);
            }

            /// Span equals sum of individual interval lengths.
            #[test]
            fn span_equals_sum_of_lengths(ops in arb_inserts(20)) {
                let mut di = DisjointIntervals::new();
                for (s, e) in &ops {
                    di.insert(*s, *e);
                }
                let computed_span: i64 = di.intervals().iter().map(|iv| iv.len()).sum();
                prop_assert_eq!(di.span(), computed_span);
            }

            /// Gaps + intervals = full range [lo, hi).
            #[test]
            fn gaps_plus_intervals_cover_range(
                ops in arb_inserts(15),
                lo in -500i64..500i64,
                width in 1i64..500i64,
            ) {
                let hi = lo + width;
                let mut di = DisjointIntervals::new();
                for (s, e) in &ops {
                    di.insert(*s, *e);
                }
                let gaps = di.gaps(lo, hi);
                // Verify all gaps are valid
                for g in &gaps {
                    prop_assert!(g.start >= lo);
                    prop_assert!(g.end <= hi);
                    prop_assert!(g.start < g.end);
                }
                // Coverage: every point in [lo, hi) is in either an interval or a gap
                let gap_span: i64 = gaps.iter().map(|g| g.len()).sum();
                let interval_span: i64 = di.intervals().iter()
                    .filter_map(|iv| {
                        let clamp_s = iv.start.max(lo);
                        let clamp_e = iv.end.min(hi);
                        if clamp_s < clamp_e { Some(clamp_e - clamp_s) } else { None }
                    })
                    .sum();
                let total = gap_span + interval_span;
                let expected = hi - lo;
                prop_assert_eq!(total, expected);
            }

            /// intersects(a, b) == true iff there exists a point p in [a,b) contained.
            #[test]
            fn intersects_consistent_with_contains(
                ops in arb_inserts(10),
                (q_start, q_end) in arb_interval()
            ) {
                let mut di = DisjointIntervals::new();
                for (s, e) in &ops {
                    di.insert(*s, *e);
                }
                let intersects_result = di.intersects(q_start, q_end);
                let brute_force = (q_start..q_end).any(|p| di.contains(p));
                prop_assert_eq!(intersects_result, brute_force);
            }

            /// Empty intervals are always ignored by insert.
            #[test]
            fn empty_insert_is_noop(base_ops in arb_inserts(10), point in -1000i64..1000i64) {
                let mut di = DisjointIntervals::new();
                for (s, e) in &base_ops {
                    di.insert(*s, *e);
                }
                let before = di.intervals().to_vec();
                di.insert(point, point); // empty interval
                let after = di.intervals().to_vec();
                prop_assert_eq!(before, after, "empty insert changed intervals");
            }

            /// Empty removes are always noops.
            #[test]
            fn empty_remove_is_noop(base_ops in arb_inserts(10), point in -1000i64..1000i64) {
                let mut di = DisjointIntervals::new();
                for (s, e) in &base_ops {
                    di.insert(*s, *e);
                }
                let before = di.intervals().to_vec();
                di.remove(point, point); // empty removal
                let after = di.intervals().to_vec();
                prop_assert_eq!(before, after, "empty remove changed intervals");
            }

            /// Inserting a subset of an existing interval doesn't change the set.
            #[test]
            fn insert_subset_is_idempotent((outer_s, outer_e) in arb_interval()) {
                let mut di = DisjointIntervals::new();
                di.insert(outer_s, outer_e);
                let before = di.intervals().to_vec();
                // Insert a sub-interval
                let mid_s = outer_s + (outer_e - outer_s) / 3;
                let mid_e = outer_s + 2 * (outer_e - outer_s) / 3;
                if mid_s < mid_e {
                    di.insert(mid_s, mid_e);
                    let after = di.intervals().to_vec();
                    prop_assert_eq!(before, after, "subset insert changed intervals");
                }
            }

            /// Idempotency: inserting the same interval twice doesn't change the set.
            #[test]
            fn double_insert_idempotent((s, e) in arb_interval()) {
                let mut di = DisjointIntervals::new();
                di.insert(s, e);
                let after_first = di.intervals().to_vec();
                di.insert(s, e);
                let after_second = di.intervals().to_vec();
                prop_assert_eq!(after_first, after_second);
            }

            /// Span is monotonically non-decreasing under inserts.
            #[test]
            fn span_monotone_under_inserts(ops in arb_inserts(20)) {
                let mut di = DisjointIntervals::new();
                let mut prev_span = 0i64;
                for (s, e) in &ops {
                    di.insert(*s, *e);
                    let new_span = di.span();
                    prop_assert!(new_span >= prev_span,
                        "span decreased from {} to {}", prev_span, new_span);
                    prev_span = new_span;
                }
            }

            /// min_start and max_end are consistent with the interval set.
            #[test]
            fn min_max_consistent(ops in arb_inserts(15)) {
                let mut di = DisjointIntervals::new();
                for (s, e) in &ops {
                    di.insert(*s, *e);
                }
                if let (Some(min_s), Some(max_e)) = (di.min_start(), di.max_end()) {
                    let ivs = di.intervals();
                    prop_assert_eq!(min_s, ivs.first().unwrap().start);
                    prop_assert_eq!(max_e, ivs.last().unwrap().end);
                    prop_assert!(min_s < max_e);
                } else {
                    prop_assert!(di.is_empty());
                }
            }

            /// count() matches intervals().len().
            #[test]
            fn count_matches_len(ops in arb_inserts(20)) {
                let mut di = DisjointIntervals::new();
                for (s, e) in &ops {
                    di.insert(*s, *e);
                }
                prop_assert_eq!(di.count(), di.intervals().len());
            }

            /// Stats are consistent with actual state.
            #[test]
            fn stats_consistent(
                inserts in arb_inserts(15),
                removes in proptest::collection::vec(arb_interval_or_empty(), 0..5)
            ) {
                let mut di = DisjointIntervals::new();
                for (s, e) in &inserts {
                    di.insert(*s, *e);
                }
                for (s, e) in &removes {
                    di.remove(*s, *e);
                }
                let stats = di.stats();
                prop_assert_eq!(stats.interval_count, di.count());
                prop_assert_eq!(stats.total_span, di.span());
                prop_assert_eq!(stats.insert_count, inserts.len() as u64);
                prop_assert_eq!(stats.remove_count, removes.len() as u64);
                prop_assert!(stats.memory_bytes > 0);
            }

            /// With merge_adjacent=false, adjacent intervals stay separate.
            #[test]
            fn no_merge_adjacent_keeps_touching_separate(
                base in -500i64..500i64,
                width1 in 1i64..100i64,
                width2 in 1i64..100i64,
            ) {
                let config = DisjointIntervalsConfig { merge_adjacent: false };
                let mut di = DisjointIntervals::from_config(&config);
                let end1 = base + width1;
                let start2 = end1; // exactly adjacent
                let end2 = start2 + width2;
                di.insert(base, end1);
                di.insert(start2, end2);
                prop_assert_eq!(di.count(), 2, "adjacent intervals merged with merge_adjacent=false");
                assert_invariants(&di);
            }

            /// Serde roundtrip for Interval.
            #[test]
            fn interval_serde_roundtrip((s, e) in arb_interval()) {
                let iv = Interval::new(s, e);
                let json = serde_json::to_string(&iv).unwrap();
                let back: Interval = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(iv, back);
            }

            /// Serde roundtrip for DisjointIntervalsStats.
            #[test]
            fn stats_serde_roundtrip(ops in arb_inserts(10)) {
                let mut di = DisjointIntervals::new();
                for (s, e) in &ops {
                    di.insert(*s, *e);
                }
                let stats = di.stats();
                let json = serde_json::to_string(&stats).unwrap();
                let back: DisjointIntervalsStats = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(stats, back);
            }

            /// Serde roundtrip for DisjointIntervalsConfig.
            #[test]
            fn config_serde_roundtrip(merge_adj in proptest::bool::ANY) {
                let config = DisjointIntervalsConfig { merge_adjacent: merge_adj };
                let json = serde_json::to_string(&config).unwrap();
                let back: DisjointIntervalsConfig = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(config, back);
            }

            /// After clear, the set is empty but operation counters are preserved.
            #[test]
            fn clear_preserves_counters(
                inserts in arb_inserts(10),
                removes in proptest::collection::vec(arb_interval_or_empty(), 0..5)
            ) {
                let mut di = DisjointIntervals::new();
                for (s, e) in &inserts {
                    di.insert(*s, *e);
                }
                for (s, e) in &removes {
                    di.remove(*s, *e);
                }
                let stats_before = di.stats();
                di.clear();
                prop_assert!(di.is_empty());
                prop_assert_eq!(di.span(), 0);
                let stats_after = di.stats();
                prop_assert_eq!(stats_after.insert_count, stats_before.insert_count);
                prop_assert_eq!(stats_after.remove_count, stats_before.remove_count);
            }
        }
    }
}
