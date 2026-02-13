use num::{Integer, ToPrimitive};
use std::cmp::{max, min, Ordering};
use std::fmt::Debug;
use std::ops::Range;

/// Track a set of integers, collapsing adjacent integers into ranges.
/// Internally stores the set in an array of ranges.
/// Allows adding and subtracting ranges.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RangeSet<T: Integer + Copy> {
    ranges: Vec<Range<T>>,
    needs_sort: bool,
}

pub fn range_is_empty<T: Integer>(range: &Range<T>) -> bool {
    range.start == range.end
}

/// Returns true if r1 intersects r2
pub fn intersects_range<T: Integer + Copy + Debug>(r1: &Range<T>, r2: &Range<T>) -> bool {
    let start = max(r1.start, r2.start);
    let end = min(r1.end, r2.end);

    end > start
}

/// Computes the intersection of r1 and r2
pub fn range_intersection<T: Integer + Copy + Debug>(
    r1: &Range<T>,
    r2: &Range<T>,
) -> Option<Range<T>> {
    let start = max(r1.start, r2.start);
    let end = min(r1.end, r2.end);

    if end > start {
        Some(start..end)
    } else {
        None
    }
}

/// Computes the r1 - r2, which may result in up to two non-overlapping ranges.
pub fn range_subtract<T: Integer + Copy + Debug>(
    r1: &Range<T>,
    r2: &Range<T>,
) -> (Option<Range<T>>, Option<Range<T>>) {
    let i_start = max(r1.start, r2.start);
    let i_end = min(r1.end, r2.end);

    if i_end > i_start {
        let a = if i_start == r1.start {
            // Intersection overlaps with the LHS
            None
        } else {
            // The LHS up to the intersection
            Some(r1.start..r1.end.min(i_start))
        };

        let b = if i_end == r1.end {
            // Intersection overlaps with the RHS
            None
        } else {
            // The intersection up to the RHS
            Some(r1.end.min(i_end)..r1.end)
        };

        (a, b)
    } else {
        // No intersection, so we're left with r1 with nothing removed
        (Some(r1.clone()), None)
    }
}

/// Merge two ranges to produce their union
pub fn range_union<T: Integer>(r1: Range<T>, r2: Range<T>) -> Range<T> {
    if range_is_empty(&r1) {
        r2
    } else if range_is_empty(&r2) {
        r1
    } else {
        let start = r1.start.min(r2.start);
        let end = r1.end.max(r2.end);
        start..end
    }
}

impl<T: Integer + Copy + Debug + ToPrimitive> From<RangeSet<T>> for Vec<Range<T>> {
    fn from(r: RangeSet<T>) -> Vec<Range<T>> {
        r.ranges
    }
}

impl<T: Integer + Copy + Debug + ToPrimitive> RangeSet<T> {
    /// Create a new set
    pub fn new() -> Self {
        Self {
            ranges: vec![],
            needs_sort: false,
        }
    }

    /// Returns true if this set is empty
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Returns the total size of the range (the sum of the start..end
    /// distance of all contained ranges)
    pub fn len(&self) -> T {
        let mut total = num::zero();
        for r in &self.ranges {
            total = total + r.end - r.start;
        }
        total
    }

    /// Returns true if this set contains the specified integer
    pub fn contains(&self, value: T) -> bool {
        for r in &self.ranges {
            if r.contains(&value) {
                return true;
            }
        }
        false
    }

    /// Returns a rangeset containing all of the integers that are present
    /// in self but not in other.
    /// The current implementation is `O(n**2)` but this should be "OK"
    /// as the likely scenario is that there will be a large contiguous
    /// range for the scrollback, and a smaller contiguous range for changes
    /// in the viewport.
    /// If that doesn't hold up, we can improve this.
    pub fn difference(&self, other: &Self) -> Self {
        let mut result = self.clone();

        for range in &other.ranges {
            result.remove_range(range.clone());
        }

        result
    }

    pub fn intersection(&self, other: &Self) -> Self {
        let mut result = Self::new();
        for range in &other.ranges {
            for r in &self.ranges {
                if let Some(i) = range_intersection(r, range) {
                    result.add_range(i);
                }
            }
        }

        result
    }

    pub fn intersection_with_range(&self, range: Range<T>) -> Self {
        let mut result = Self::new();

        for r in &self.ranges {
            if let Some(i) = range_intersection(r, &range) {
                result.add_range(i);
            }
        }

        result
    }

    /// Remove a single integer from the set
    pub fn remove(&mut self, value: T) {
        self.remove_range(value..value + num::one());
    }

    /// Remove a range of integers from the set
    pub fn remove_range(&mut self, range: Range<T>) {
        let mut to_add = vec![];
        let mut to_remove = vec![];

        for (idx, r) in self.ranges.iter().enumerate() {
            match range_subtract(r, &range) {
                (None, None) => to_remove.push(idx),
                (Some(a), Some(b)) => {
                    to_remove.push(idx);
                    to_add.push(a);
                    to_add.push(b);
                }
                (Some(a), None) | (None, Some(a)) if a != *r => {
                    to_remove.push(idx);
                    to_add.push(a);
                }
                _ => {}
            }
        }

        for idx in to_remove.into_iter().rev() {
            self.ranges.remove(idx);
        }

        for r in to_add {
            self.add_range(r);
        }
    }

    /// Remove a set of ranges from this set
    pub fn remove_set(&mut self, set: &Self) {
        for r in set.iter() {
            self.remove_range(r.clone());
        }
    }

    /// Add a single integer to the set
    pub fn add(&mut self, value: T) {
        self.add_range(value..value + num::one());
    }

    /// Add a range of integers to the set
    pub fn add_range(&mut self, range: Range<T>) {
        if range_is_empty(&range) {
            return;
        }

        if self.ranges.is_empty() {
            self.ranges.push(range);
            return;
        }

        self.sort_if_needed();

        match self.intersection_helper(&range) {
            (Some(a), Some(b)) if b == a + 1 => {
                // This range intersects with two or more adjacent ranges and will
                // therefore join them together

                let second = self.ranges[b].clone();
                let merged = range_union(range, second);

                self.ranges.remove(b);
                self.add_range(merged)
            }
            (Some(a), _) => self.merge_into_range(a, range),
            (None, Some(_)) => unreachable!(),
            (None, None) => {
                // No intersection, so find the insertion point
                let idx = self.insertion_point(&range);
                self.ranges.insert(idx, range.clone());
            }
        }
    }

    pub fn add_range_unchecked(&mut self, range: Range<T>) {
        self.ranges.push(range);
        self.needs_sort = true;
    }

    /// Add a set of ranges to this set
    pub fn add_set(&mut self, set: &Self) {
        for r in set.iter() {
            self.add_range(r.clone());
        }
    }

    fn merge_into_range(&mut self, idx: usize, range: Range<T>) {
        let existing = self.ranges[idx].clone();
        self.ranges[idx] = range_union(existing, range);
    }

    fn intersection_helper(&self, range: &Range<T>) -> (Option<usize>, Option<usize>) {
        if self.needs_sort {
            panic!("rangeset needs sorting");
        }

        let idx = match self.binary_search_ranges(range) {
            Ok(idx) => idx,
            Err(idx) => idx.saturating_sub(1),
        };

        let mut first = None;
        if let Some(r) = self.ranges.get(idx) {
            if intersects_range(r, range) || r.end == range.start {
                first = Some(idx);
            }
        }
        if let Some(r) = self.ranges.get(idx + 1) {
            if intersects_range(r, range) || r.end == range.start {
                if first.is_some() {
                    return (first, Some(idx + 1));
                }
            }
        }
        (first, None)
    }

    pub fn sort_if_needed(&mut self) {
        if self.needs_sort {
            self.ranges.sort_by_key(|r| r.start);
            self.needs_sort = false;
        }
    }

    fn binary_search_ranges(&self, range: &Range<T>) -> Result<usize, usize> {
        self.ranges.binary_search_by(|r| {
            if range.start >= r.start && range.end <= r.end {
                Ordering::Equal
            } else if range.start < r.start {
                Ordering::Greater
            } else if range.end > r.end {
                Ordering::Less
            } else {
                unreachable!()
            }
        })
    }

    fn insertion_point(&self, range: &Range<T>) -> usize {
        if self.needs_sort {
            panic!("rangeset needs sorting");
        }

        match self.binary_search_ranges(range) {
            Ok(idx) => idx,
            Err(idx) => idx,
        }
    }

    /// Returns an iterator over the ranges that comprise the set
    pub fn iter(&self) -> impl Iterator<Item = &Range<T>> {
        self.ranges.iter()
    }

    /// Returns an iterator over all of the contained values.
    /// Take care when the range is very large!
    pub fn iter_values<'a>(&'a self) -> impl Iterator<Item = T> + 'a {
        self.ranges.iter().flat_map(|r| num::range(r.start, r.end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect<T: Integer + Copy + Debug + ToPrimitive>(set: &RangeSet<T>) -> Vec<Range<T>> {
        set.iter().cloned().collect()
    }

    #[test]
    fn add_range() {
        let mut set = RangeSet::new();
        set.add(1);
        set.add(2);
        set.add(4);
        assert_eq!(collect(&set), vec![1..3, 4..5]);

        set.add_range(2..6);
        assert_eq!(collect(&set), vec![1..6]);
    }

    #[test]
    fn remove_range() {
        let mut set = RangeSet::new();
        set.add_range(1..5);
        set.add_range(8..10);
        assert_eq!(collect(&set), vec![1..5, 8..10]);

        // Middle
        set.remove(2);
        assert_eq!(collect(&set), vec![1..2, 3..5, 8..10]);

        // RHS
        set.remove_range(4..7);
        assert_eq!(collect(&set), vec![1..2, 3..4, 8..10]);

        // Complete overlap of one range, LHS overlap with another
        set.remove_range(3..9);
        assert_eq!(collect(&set), vec![1..2, 9..10]);
    }

    #[test]
    fn difference() {
        let mut set = RangeSet::new();
        set.add_range(1..10);

        let mut other = RangeSet::new();
        other.add_range(1..15);

        let diff = other.difference(&set);
        assert_eq!(collect(&diff), vec![10..15]);

        let diff = set.difference(&other);
        assert_eq!(collect(&diff), vec![]);
    }

    #[test]
    fn difference_more() {
        let mut set = RangeSet::new();
        set.add(1);
        set.add(3);
        set.add(5);

        let mut other = RangeSet::new();
        other.add(2);
        other.add(4);
        other.add(6);

        let diff = other.difference(&set);
        assert_eq!(collect(&diff), vec![2..3, 4..5, 6..7]);

        let diff = set.difference(&other);
        assert_eq!(collect(&diff), vec![1..2, 3..4, 5..6]);
    }

    #[test]
    fn difference_moar() {
        let data = [0xd604, 0xc7ac, 0xbe0c, 0xb79c, 0xce58];
        let mut set = RangeSet::new();
        for &c in &data {
            set.add(c);
        }

        let diff = set.difference(&set);
        assert_eq!(collect(&diff), vec![]);
    }

    // ── Free functions ─────────────────────────────────────────

    #[test]
    fn range_is_empty_on_empty_range() {
        assert!(range_is_empty(&(5..5)));
    }

    #[test]
    fn range_is_empty_on_nonempty_range() {
        assert!(!range_is_empty(&(1..5)));
    }

    #[test]
    fn intersects_overlapping() {
        assert!(intersects_range(&(1..5), &(3..7)));
    }

    #[test]
    fn intersects_nonoverlapping() {
        assert!(!intersects_range(&(1..3), &(5..7)));
    }

    #[test]
    fn intersects_adjacent_are_not_intersecting() {
        assert!(!intersects_range(&(1..3), &(3..5)));
    }

    #[test]
    fn intersects_contained() {
        assert!(intersects_range(&(1..10), &(3..5)));
    }

    #[test]
    fn range_intersection_overlapping() {
        assert_eq!(range_intersection(&(1..5), &(3..7)), Some(3..5));
    }

    #[test]
    fn range_intersection_nonoverlapping() {
        assert_eq!(range_intersection(&(1..3), &(5..7)), None);
    }

    #[test]
    fn range_intersection_contained() {
        assert_eq!(range_intersection(&(1..10), &(3..5)), Some(3..5));
    }

    #[test]
    fn range_subtract_no_overlap() {
        assert_eq!(range_subtract(&(1..5), &(7..10)), (Some(1..5), None));
    }

    #[test]
    fn range_subtract_full_overlap() {
        assert_eq!(range_subtract(&(3..5), &(1..10)), (None, None));
    }

    #[test]
    fn range_subtract_left_overlap() {
        // Subtracting the left portion: intersection starts at r1.start,
        // so the first result is None, remainder is in second position
        assert_eq!(range_subtract(&(1..5), &(1..3)), (None, Some(3..5)));
    }

    #[test]
    fn range_subtract_right_overlap() {
        assert_eq!(range_subtract(&(1..5), &(3..5)), (Some(1..3), None));
    }

    #[test]
    fn range_subtract_middle() {
        assert_eq!(range_subtract(&(1..10), &(3..7)), (Some(1..3), Some(7..10)));
    }

    #[test]
    fn range_union_basic() {
        assert_eq!(range_union(1..5, 3..7), 1..7);
    }

    #[test]
    fn range_union_with_empty_first() {
        assert_eq!(range_union(5..5, 3..7), 3..7);
    }

    #[test]
    fn range_union_with_empty_second() {
        assert_eq!(range_union(1..5, 3..3), 1..5);
    }

    // ── RangeSet construction ──────────────────────────────────

    #[test]
    fn new_is_empty() {
        let set: RangeSet<i32> = RangeSet::new();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
    }

    #[test]
    fn default_is_empty() {
        let set: RangeSet<i32> = RangeSet::default();
        assert!(set.is_empty());
    }

    // ── contains ───────────────────────────────────────────────

    #[test]
    fn contains_added_value() {
        let mut set = RangeSet::new();
        set.add(5);
        assert!(set.contains(5));
    }

    #[test]
    fn does_not_contain_missing_value() {
        let mut set = RangeSet::new();
        set.add(5);
        assert!(!set.contains(4));
        assert!(!set.contains(6));
    }

    #[test]
    fn contains_values_in_range() {
        let mut set = RangeSet::new();
        set.add_range(10..20);
        assert!(set.contains(10));
        assert!(set.contains(15));
        assert!(set.contains(19));
        assert!(!set.contains(9));
        assert!(!set.contains(20));
    }

    // ── len ────────────────────────────────────────────────────

    #[test]
    fn len_single_value() {
        let mut set = RangeSet::new();
        set.add(1);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn len_range() {
        let mut set = RangeSet::new();
        set.add_range(5..10);
        assert_eq!(set.len(), 5);
    }

    #[test]
    fn len_multiple_ranges() {
        let mut set = RangeSet::new();
        set.add_range(1..3);
        set.add_range(10..15);
        assert_eq!(set.len(), 7);
    }

    // ── add_range_unchecked ────────────────────────────────────

    #[test]
    fn add_range_unchecked_then_sort() {
        let mut set = RangeSet::new();
        set.add_range_unchecked(10..15);
        set.add_range_unchecked(1..5);
        set.sort_if_needed();
        let ranges = collect(&set);
        assert_eq!(ranges, vec![1..5, 10..15]);
    }

    // ── add_set / remove_set ───────────────────────────────────

    #[test]
    fn add_set_merges() {
        let mut set1 = RangeSet::new();
        set1.add_range(1..5);

        let mut set2 = RangeSet::new();
        set2.add_range(3..8);

        set1.add_set(&set2);
        assert_eq!(collect(&set1), vec![1..8]);
    }

    #[test]
    fn remove_set_subtracts() {
        let mut set1 = RangeSet::new();
        set1.add_range(1..10);

        let mut set2 = RangeSet::new();
        set2.add_range(3..7);

        set1.remove_set(&set2);
        assert_eq!(collect(&set1), vec![1..3, 7..10]);
    }

    // ── intersection ───────────────────────────────────────────

    #[test]
    fn intersection_overlapping_sets() {
        let mut set1 = RangeSet::new();
        set1.add_range(1..10);

        let mut set2 = RangeSet::new();
        set2.add_range(5..15);

        let inter = set1.intersection(&set2);
        assert_eq!(collect(&inter), vec![5..10]);
    }

    #[test]
    fn intersection_disjoint_sets() {
        let mut set1 = RangeSet::new();
        set1.add_range(1..5);

        let mut set2 = RangeSet::new();
        set2.add_range(10..15);

        let inter = set1.intersection(&set2);
        assert!(inter.is_empty());
    }

    #[test]
    fn intersection_with_range_method() {
        let mut set = RangeSet::new();
        set.add_range(1..10);
        set.add_range(20..30);

        let inter = set.intersection_with_range(5..25);
        assert_eq!(collect(&inter), vec![5..10, 20..25]);
    }

    // ── iter_values ────────────────────────────────────────────

    #[test]
    fn iter_values_returns_all_integers() {
        let mut set = RangeSet::new();
        set.add_range(1..4);
        set.add_range(7..9);
        let values: Vec<i32> = set.iter_values().collect();
        assert_eq!(values, vec![1, 2, 3, 7, 8]);
    }

    #[test]
    fn iter_values_empty_set() {
        let set: RangeSet<i32> = RangeSet::new();
        let values: Vec<i32> = set.iter_values().collect();
        assert!(values.is_empty());
    }

    // ── From<RangeSet> for Vec ─────────────────────────────────

    #[test]
    fn into_vec_conversion() {
        let mut set = RangeSet::new();
        set.add_range(1..5);
        set.add_range(10..15);
        let v: Vec<Range<i32>> = set.into();
        assert_eq!(v, vec![1..5, 10..15]);
    }

    // ── Clone / PartialEq / Eq ─────────────────────────────────

    #[test]
    fn clone_produces_equal_set() {
        let mut set = RangeSet::new();
        set.add_range(1..10);
        let cloned = set.clone();
        assert_eq!(set, cloned);
    }

    // ── Edge cases ─────────────────────────────────────────────

    #[test]
    fn add_empty_range_is_noop() {
        let mut set = RangeSet::new();
        set.add_range(5..5);
        assert!(set.is_empty());
    }

    #[test]
    fn add_adjacent_ranges_merge() {
        let mut set = RangeSet::new();
        set.add_range(1..5);
        set.add_range(5..10);
        assert_eq!(collect(&set), vec![1..10]);
    }

    #[test]
    fn add_overlapping_ranges_merge() {
        let mut set = RangeSet::new();
        set.add_range(1..7);
        set.add_range(5..10);
        assert_eq!(collect(&set), vec![1..10]);
    }

    #[test]
    fn remove_from_middle_splits() {
        let mut set = RangeSet::new();
        set.add_range(1..10);
        set.remove(5);
        assert_eq!(collect(&set), vec![1..5, 6..10]);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let mut set = RangeSet::new();
        set.add_range(1..5);
        set.remove(10);
        assert_eq!(collect(&set), vec![1..5]);
    }
}
