//! br-ft-ev0lj: storage-domain range prefilter for timestamp scans.
//!
//! This module provides the conservative substrate for the SuRF-style
//! retention-query prefilter described in ft-ev0lj. It stores observed
//! timestamp ranges as a compact, merged interval set, which is stricter
//! than a probabilistic SuRF: `could_have_match` has zero false negatives
//! and zero false positives for the recorded ranges. The storage call sites
//! can use this as the no-risk fallback before a later succinct-trie
//! representation replaces the backing structure.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::Path;

/// Default suffix-bit budget from the ft-ev0lj SuRF plan.
///
/// The exact interval backend does not need suffix bits, but keeping the
/// setting in the persisted snapshot preserves the call-site contract for a
/// future succinct representation.
pub const DEFAULT_RANGE_FILTER_SUFFIX_BITS: u8 = 4;

/// Exact storage range prefilter keyed by inclusive timestamp ranges.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct StorageRangeFilter {
    table: String,
    suffix_bits: u8,
    ranges: BTreeMap<i64, i64>,
}

impl StorageRangeFilter {
    /// Create an empty range filter for a storage table.
    #[must_use]
    pub fn new(table: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            suffix_bits: DEFAULT_RANGE_FILTER_SUFFIX_BITS,
            ranges: BTreeMap::new(),
        }
    }

    /// Create an empty range filter with an explicit suffix-bit budget.
    #[must_use]
    pub fn with_suffix_bits(table: impl Into<String>, suffix_bits: u8) -> Self {
        Self {
            table: table.into(),
            suffix_bits,
            ranges: BTreeMap::new(),
        }
    }

    /// Storage table this filter summarizes.
    #[must_use]
    pub fn table(&self) -> &str {
        &self.table
    }

    /// Configured suffix-bit budget for a future succinct range backend.
    #[must_use]
    pub fn suffix_bits(&self) -> u8 {
        self.suffix_bits
    }

    /// Number of merged inclusive intervals retained by the filter.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    /// Returns true when the filter has no observed ranges.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Record an inclusive timestamp range.
    ///
    /// Bounds are normalized, so callers may pass `(high, low)` without
    /// creating an invalid range. Overlapping and adjacent intervals are
    /// merged to keep the representation compact.
    pub fn record_range(&mut self, ts_min: i64, ts_max: i64) {
        let (mut start, mut end) = normalize_bounds(ts_min, ts_max);

        if let Some((&prev_start, &prev_end)) = self.ranges.range(..=start).next_back()
            && intervals_touch(prev_end, start)
        {
            start = prev_start;
            end = end.max(prev_end);
            self.ranges.remove(&prev_start);
        }

        while let Some((range_start, range_end)) = self
            .ranges
            .range(start..)
            .next()
            .map(|(&start, &end)| (start, end))
        {
            if !intervals_touch(end, range_start) {
                break;
            }
            end = end.max(range_end);
            self.ranges.remove(&range_start);
        }
        self.ranges.insert(start, end);
    }

    /// Return true if the recorded ranges may intersect the query range.
    ///
    /// For this exact backend, true means the ranges definitely intersect and
    /// false means SQLite can skip the query with zero false negatives.
    #[must_use]
    pub fn could_have_match(&self, ts_low: i64, ts_high: i64) -> bool {
        let (low, high) = normalize_bounds(ts_low, ts_high);

        if self
            .ranges
            .range(..=low)
            .next_back()
            .is_some_and(|(_, &range_end)| range_end >= low)
        {
            return true;
        }

        self.ranges
            .range(low..=high)
            .next()
            .is_some_and(|(_, &range_end)| range_end >= low)
    }

    /// False-positive bound for this backend.
    ///
    /// A probabilistic SuRF backend would use `suffix_bits` here. The exact
    /// interval representation has no false positives.
    #[must_use]
    pub fn false_positive_rate_bound(&self) -> f64 {
        0.0
    }

    /// Iterate merged inclusive ranges in ascending order.
    pub fn ranges(&self) -> impl Iterator<Item = (i64, i64)> + '_ {
        self.ranges.iter().map(|(&start, &end)| (start, end))
    }

    /// Build a serde-friendly snapshot for diagnostics and persistence.
    #[must_use]
    pub fn snapshot(&self) -> StorageRangeFilterSnapshot {
        StorageRangeFilterSnapshot {
            table: self.table.clone(),
            suffix_bits: self.suffix_bits,
            false_positive_rate_bound: self.false_positive_rate_bound(),
            ranges: self.ranges().collect(),
        }
    }

    /// Persist the filter as JSON sidecar bytes.
    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), StorageRangeFilterIoError> {
        let bytes = serde_json::to_vec(self).map_err(StorageRangeFilterIoError::Serde)?;
        std::fs::write(path, bytes).map_err(StorageRangeFilterIoError::Io)
    }

    /// Load a filter from JSON sidecar bytes.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, StorageRangeFilterIoError> {
        let bytes = std::fs::read(path).map_err(StorageRangeFilterIoError::Io)?;
        serde_json::from_slice(&bytes).map_err(StorageRangeFilterIoError::Serde)
    }
}

/// Diagnostics snapshot for storage-doctor / runtime reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageRangeFilterSnapshot {
    pub table: String,
    pub suffix_bits: u8,
    pub false_positive_rate_bound: f64,
    pub ranges: Vec<(i64, i64)>,
}

/// Error returned while reading or writing range-filter sidecars.
#[derive(Debug)]
pub enum StorageRangeFilterIoError {
    Io(std::io::Error),
    Serde(serde_json::Error),
}

impl Display for StorageRangeFilterIoError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "range filter I/O failed: {error}"),
            Self::Serde(error) => write!(f, "range filter JSON failed: {error}"),
        }
    }
}

impl Error for StorageRangeFilterIoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Serde(error) => Some(error),
        }
    }
}

fn normalize_bounds(a: i64, b: i64) -> (i64, i64) {
    if a <= b { (a, b) } else { (b, a) }
}

fn intervals_touch(left_end: i64, right_start: i64) -> bool {
    left_end
        .checked_add(1)
        .is_none_or(|next_after_left| next_after_left >= right_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_rejects_every_range() {
        let filter = StorageRangeFilter::new("events");

        assert!(!filter.could_have_match(i64::MIN, i64::MAX));
        assert_eq!(filter.false_positive_rate_bound(), 0.0);
        assert!(filter.is_empty());
    }

    #[test]
    fn overlapping_and_adjacent_ranges_merge() {
        let mut filter = StorageRangeFilter::new("events");

        filter.record_range(10, 20);
        filter.record_range(30, 40);
        filter.record_range(21, 29);
        filter.record_range(100, 90);

        assert_eq!(
            filter.ranges().collect::<Vec<_>>(),
            vec![(10, 40), (90, 100)]
        );
        assert_eq!(filter.len(), 2);
    }

    #[test]
    fn could_have_match_has_zero_false_negatives_for_covering_queries() {
        let mut filter = StorageRangeFilter::new("audit_actions");
        let observed = [(-20, -10), (0, 3), (9, 14), (100, 130)];

        for (start, end) in observed {
            filter.record_range(start, end);
        }

        for (start, end) in observed {
            for low in start..=end {
                for high in low..=end {
                    assert!(
                        filter.could_have_match(low, high),
                        "query [{low}, {high}] should intersect observed [{start}, {end}]"
                    );
                }
            }
        }
    }

    #[test]
    fn could_have_match_rejects_gaps_exactly() {
        let mut filter = StorageRangeFilter::new("audit_actions");

        filter.record_range(10, 20);
        filter.record_range(30, 40);

        assert!(!filter.could_have_match(i64::MIN, 9));
        assert!(!filter.could_have_match(21, 29));
        assert!(!filter.could_have_match(41, i64::MAX));
        assert!(filter.could_have_match(5, 10));
        assert!(filter.could_have_match(20, 30));
        assert!(filter.could_have_match(40, 50));
    }

    #[test]
    fn snapshot_preserves_filter_contract() {
        let mut filter = StorageRangeFilter::with_suffix_bits("action_history", 6);
        filter.record_range(5, 1);
        filter.record_range(10, 11);

        let snapshot = filter.snapshot();

        assert_eq!(snapshot.table, "action_history");
        assert_eq!(snapshot.suffix_bits, 6);
        assert_eq!(snapshot.false_positive_rate_bound, 0.0);
        assert_eq!(snapshot.ranges, vec![(1, 5), (10, 11)]);
    }
}
