//! br-ft-storage-cardinality: alien-uplift §9.4 Sketching for storage
//! cardinality estimates.
//!
//! Wraps the existing [`crate::hyperloglog::HyperLogLog`] (already in
//! the workspace via `frankenterm-core-telemetry-types`) with a
//! storage-domain-friendly API for estimating distinct counts of
//! pane_ids, session_ids, and embedder_ids without scanning the
//! database.
//!
//! # Why this exists
//!
//! `crate::storage_backend_helpers::count_table` runs
//! `SELECT COUNT(*) FROM <table>` which is O(n) on SQLite (no
//! row-count metadata maintained out-of-band). For dashboards +
//! telemetry that frequently ask "how many distinct panes have
//! we seen?", "how many sessions are in the recorder?", "how
//! many embedders have ingested data?", the exact COUNT(DISTINCT)
//! is wasteful — the operator wants an order-of-magnitude estimate
//! at sub-millisecond cost.
//!
//! HyperLogLog (Flajolet et al. 2007) gives bounded-error
//! cardinality estimation: standard error ~1.04/√m where m = 2^p
//! registers. At default p=14 (16 KB sketch), standard error is
//! ~0.81% — much tighter than the operator's "is this thousands
//! or millions?" question.
//!
//! # Coverage guarantee
//!
//! For an unbiased random hash function, `cardinality()` returns
//! `n × (1 + ε)` where `|ε| ≤ 1.04/√m` with 95% confidence
//! (Flajolet et al. 2007, Theorem 1). This is a distribution-free
//! guarantee that doesn't depend on the underlying value
//! distribution.
//!
//! # What ships in this slice
//!
//! - [`StorageDistinctSketch`] — three HLL counters (panes, sessions,
//!   embedders) + storage-friendly API.
//! - [`StorageDistinctSketchSnapshot`] — serde snapshot for
//!   inclusion in storage-doctor / runtime-telemetry reports.
//!
//! ## What is deferred
//!
//! - Cross-process merge (HyperLogLog has no `merge` method today;
//!   adding it requires upstream work in
//!   `frankenterm-core-telemetry-types::hyperloglog`).
//! - Serde of the sketch state (only the summary stats are
//!   serde-friendly; raw register state can't roundtrip until HLL
//!   itself derives Serialize/Deserialize).
//! - Wiring into the storage writer thread to update the sketch
//!   on every insert. This requires a per-StorageHandle sketch
//!   instance + lock-free updates from the writer thread.

use crate::hyperloglog::HyperLogLog;
use serde::{Deserialize, Serialize};

/// br-ft-storage-cardinality: estimator for distinct storage-domain
/// identifiers.
///
/// Maintains three HyperLogLog sketches:
/// - **Panes**: distinct pane_id values seen.
/// - **Sessions**: distinct session_id strings seen.
/// - **Embedders**: distinct embedder_id strings seen.
///
/// Each sketch uses default precision p=14 (16 KB, ~0.81% standard
/// error). Total memory: ~48 KB per StorageDistinctSketch instance.
#[derive(Debug, Clone)]
pub struct StorageDistinctSketch {
    panes: HyperLogLog,
    sessions: HyperLogLog,
    embedders: HyperLogLog,
}

impl Default for StorageDistinctSketch {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageDistinctSketch {
    /// Create a new sketch with default precision (p=14, ~0.81%
    /// standard error per counter).
    #[must_use]
    pub fn new() -> Self {
        Self {
            panes: HyperLogLog::new(),
            sessions: HyperLogLog::new(),
            embedders: HyperLogLog::new(),
        }
    }

    /// Create a new sketch with explicit HLL precision (p in 4..=18).
    /// Higher p = more accuracy + more memory (2^p bytes per counter).
    #[must_use]
    pub fn with_precision(precision: u8) -> Self {
        Self {
            panes: HyperLogLog::with_precision(precision),
            sessions: HyperLogLog::with_precision(precision),
            embedders: HyperLogLog::with_precision(precision),
        }
    }

    /// Record a pane_id observation. Multiple inserts of the same
    /// pane_id are coalesced (the HLL bucket update is idempotent
    /// after the first observation hits the same register/rho cell).
    pub fn record_pane_id(&mut self, pane_id: u64) {
        self.panes.insert(&pane_id);
    }

    /// Record a session_id observation.
    pub fn record_session_id(&mut self, session_id: &str) {
        self.sessions.insert(session_id);
    }

    /// Record an embedder_id observation.
    pub fn record_embedder_id(&mut self, embedder_id: &str) {
        self.embedders.insert(embedder_id);
    }

    /// Estimated count of distinct pane_ids observed.
    #[must_use]
    pub fn estimated_distinct_panes(&self) -> u64 {
        self.panes.cardinality()
    }

    /// Estimated count of distinct session_ids observed.
    #[must_use]
    pub fn estimated_distinct_sessions(&self) -> u64 {
        self.sessions.cardinality()
    }

    /// Estimated count of distinct embedder_ids observed.
    #[must_use]
    pub fn estimated_distinct_embedders(&self) -> u64 {
        self.embedders.cardinality()
    }

    /// Standard error of the cardinality estimate as a fraction
    /// (e.g., 0.0081 for default precision p=14). Same for all
    /// three counters since they share the same precision.
    #[must_use]
    pub fn standard_error(&self) -> f64 {
        self.panes.standard_error()
    }

    /// Total memory used by the three sketches in bytes.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.panes.memory_bytes() + self.sessions.memory_bytes() + self.embedders.memory_bytes()
    }

    /// Snapshot for inclusion in storage-doctor / runtime-telemetry
    /// reports.
    #[must_use]
    pub fn snapshot(&self) -> StorageDistinctSketchSnapshot {
        StorageDistinctSketchSnapshot {
            estimated_distinct_panes: self.estimated_distinct_panes(),
            estimated_distinct_sessions: self.estimated_distinct_sessions(),
            estimated_distinct_embedders: self.estimated_distinct_embedders(),
            standard_error: self.standard_error(),
            memory_bytes: self.memory_bytes() as u64,
        }
    }
}

/// br-ft-storage-cardinality: serde-friendly snapshot of a
/// [`StorageDistinctSketch`] for inclusion in storage-doctor and
/// runtime-telemetry reports.
///
/// Only the summary statistics are persisted; raw register state
/// can't roundtrip until upstream HyperLogLog derives
/// Serialize/Deserialize (deferred).
// br-ft-cc_2 inline fix: drop `Eq` from the derive — f64 has no
// `Eq` impl (only PartialEq), so deriving Eq on a struct with an
// f64 field is a compile-time error. The sibling commit
// 0c2c77459 introduced this regression; restoring lib build so
// proptests can compile. PartialEq alone is sufficient for every
// caller of StorageDistinctSketchSnapshot today (snapshots are
// compared via `assert_eq!` in tests, never used as HashMap
// keys or HashSet members which require Eq).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageDistinctSketchSnapshot {
    pub estimated_distinct_panes: u64,
    pub estimated_distinct_sessions: u64,
    pub estimated_distinct_embedders: u64,
    /// Standard error of the cardinality estimate as a fraction
    /// (e.g. 0.0081 for default precision p=14).
    pub standard_error: f64,
    pub memory_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sketch_returns_zero_for_all_counters() {
        let s = StorageDistinctSketch::new();
        assert_eq!(s.estimated_distinct_panes(), 0);
        assert_eq!(s.estimated_distinct_sessions(), 0);
        assert_eq!(s.estimated_distinct_embedders(), 0);
    }

    #[test]
    fn repeated_inserts_of_same_value_coalesce() {
        let mut s = StorageDistinctSketch::new();
        for _ in 0..1000 {
            s.record_pane_id(42);
        }
        // After 1000 inserts of pane_id=42, the cardinality estimate
        // should be ≈ 1 (HLL is approximate; for very small
        // cardinalities it may bias upward to 1-2).
        let est = s.estimated_distinct_panes();
        assert!(
            est <= 2,
            "1000 inserts of same value should yield distinct ≈ 1 (got {est})"
        );
    }

    #[test]
    fn three_counters_are_independent() {
        let mut s = StorageDistinctSketch::new();
        s.record_pane_id(1);
        s.record_pane_id(2);
        s.record_pane_id(3);
        s.record_session_id("session-A");
        // Sessions only got 1 insert; embedders got 0.
        let panes = s.estimated_distinct_panes();
        assert!((2..=4).contains(&panes), "panes ≈ 3 expected (got {panes})");
        let sessions = s.estimated_distinct_sessions();
        assert!(
            (1..=2).contains(&sessions),
            "sessions ≈ 1 expected (got {sessions})"
        );
        assert_eq!(s.estimated_distinct_embedders(), 0);
    }

    #[test]
    fn cardinality_estimate_within_bounded_error_at_default_precision() {
        // br-ft-storage-cardinality coverage test: at default
        // precision p=14, standard error is ~0.81%. Insert 10_000
        // distinct pane_ids; estimate should be within ±5% (sampling
        // slack covers ~6× standard error, generous for this test
        // size).
        let mut s = StorageDistinctSketch::new();
        for i in 0..10_000u64 {
            s.record_pane_id(i);
        }
        let est = s.estimated_distinct_panes();
        let true_n = 10_000.0;
        let err = (est as f64 - true_n).abs() / true_n;
        assert!(
            err < 0.05,
            "br-ft-storage-cardinality bounded-error guarantee: \
             estimated={est} vs true=10_000 has relative error \
             {err:.4}; must be < 0.05 at default precision p=14",
        );
    }

    #[test]
    fn cardinality_estimate_scales_through_at_least_100k() {
        // Robustness: ensure no overflow / discontinuity at higher
        // cardinalities. 100_000 inserts is well within the HLL
        // operating range.
        let mut s = StorageDistinctSketch::new();
        for i in 0..100_000u64 {
            s.record_pane_id(i);
        }
        let est = s.estimated_distinct_panes();
        let true_n = 100_000.0;
        let err = (est as f64 - true_n).abs() / true_n;
        assert!(
            err < 0.05,
            "100k cardinality estimate has relative error {err:.4}; must be < 0.05",
        );
    }

    #[test]
    fn snapshot_carries_all_documented_fields() {
        let mut s = StorageDistinctSketch::new();
        for i in 0..50u64 {
            s.record_pane_id(i);
        }
        for i in 0..10 {
            s.record_session_id(&format!("session-{i}"));
        }
        s.record_embedder_id("default-embedder");
        let snap = s.snapshot();
        assert!(snap.estimated_distinct_panes >= 40 && snap.estimated_distinct_panes <= 60);
        assert!(snap.estimated_distinct_sessions >= 8 && snap.estimated_distinct_sessions <= 12);
        assert!(snap.estimated_distinct_embedders >= 1 && snap.estimated_distinct_embedders <= 2);
        assert!(snap.standard_error > 0.0 && snap.standard_error < 0.05);
        assert!(snap.memory_bytes > 0);
    }

    #[test]
    fn snapshot_serde_roundtrips_through_json() {
        let mut s = StorageDistinctSketch::new();
        for i in 0..100u64 {
            s.record_pane_id(i);
        }
        let snap = s.snapshot();
        let json = serde_json::to_string(&snap).expect("snapshot serializes");
        let parsed: StorageDistinctSketchSnapshot =
            serde_json::from_str(&json).expect("snapshot deserializes");
        assert_eq!(parsed, snap);
    }

    #[test]
    fn memory_bytes_is_three_times_single_hll_memory() {
        // Sanity: total memory is the sum of three HLL counter
        // memories (default p=14 → 16384 bytes per counter, ~48 KB
        // total).
        let s = StorageDistinctSketch::new();
        let bytes = s.memory_bytes();
        // Default precision p=14 → 2^14 = 16384 bytes per HLL.
        // Total = 3 × 16384 = 49152.
        assert_eq!(
            bytes, 49152,
            "default-precision sketch should use exactly 3 × 16384 = 49152 bytes"
        );
    }

    #[test]
    fn explicit_precision_changes_memory_footprint() {
        let small = StorageDistinctSketch::with_precision(8); // 2^8 = 256 bytes per HLL
        let large = StorageDistinctSketch::with_precision(16); // 2^16 = 65536 bytes per HLL
        assert!(small.memory_bytes() < large.memory_bytes());
        assert!(small.standard_error() > large.standard_error());
    }
}
