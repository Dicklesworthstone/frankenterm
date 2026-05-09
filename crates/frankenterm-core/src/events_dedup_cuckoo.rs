//! br-ft-events-dedup-cuckoo: alien-uplift §7.6 Cuckoo Filter for
//! high-volume event deduplication.
//!
//! Drop-in alternative to [`crate::events::EventDeduplicator`] backed by
//! `crate::cuckoo_filter::CuckooFilter` (Fan, Andersen, Kaminsky, Mitzenmacher 2014).
//!
//! # When to choose this over EventDeduplicator
//!
//! - **EventDeduplicator** (HashMap-backed, the default): exact dedup with
//!   per-key history (suppressed_count, first_seen, last_seen). Memory
//!   ~140 bytes/key → ~280KB at default 2000 cap. Use for audit/safety
//!   events where a false-positive (silently dropping a true-new event)
//!   is unacceptable.
//!
//! - **EventCuckooDedup** (this module): approximate dedup with a bounded
//!   false-positive rate from the underlying 32-bit fingerprints. The default
//!   2000-event configuration is currently roughly tens of KB excluding allocator
//!   bookkeeping, still far smaller than the HashMap-backed exact dedup path.
//!   Use for high-volume analytics/UI/telemetry events where a small
//!   false-positive risk is acceptable in exchange for bounded memory + speed.
//!
//! # False-positive direction
//!
//! `check()` returns `New` when the cuckoo filter says NOT in set
//! (always correct — no false negatives for membership). Returns
//! `PossibleDuplicate` when the filter says IN set (correct in true-
//! positive case + false-positive case where the filter's fingerprint table
//! accidentally contains a colliding fingerprint for an unrelated event).
//!
//! For dedup semantics: PossibleDuplicate means "we'd suppress this
//! event"; the false-positive direction is **dropping a true-new
//! event**, NEVER firing twice on a true duplicate.
//!
//! # Coverage guarantee (Fan et al. 2014)
//!
//! For a Cuckoo filter at load factor α with f-bit fingerprints and
//! b-slot buckets, the false-positive rate is bounded by `2b / 2^f`
//! per query. FrankenTerm's current `CuckooFilter` stores 32-bit
//! fingerprints, so the default b=4 configuration has a bound of
//! `8 / 2^32` per query, excluding non-uniformity in the non-cryptographic
//! hash.
//!
//! # What ships in this slice
//!
//! - [`EventCuckooDedup`] — sliding-membership dedup with `check()`,
//!   `forget()`, `count()`, `load_factor()`, `snapshot()`.
//! - [`CuckooDedupVerdict`] — `New` / `PossibleDuplicate` enum.
//! - [`EventCuckooDedupSnapshot`] — serde snapshot for telemetry.
//!
//! ## What is deferred
//!
//! - Window-based expiration (per-key TTL). The cuckoo filter has no
//!   per-entry timestamp; window eviction would require either a
//!   sidecar timestamp map (defeats the memory advantage) OR
//!   coarse-grained periodic-clear ('clear if max_window_secs since
//!   last clear'). Filed as a follow-up.
//! - Wiring into events.rs callsites where the EventDeduplicator is
//!   used today (the operator opt-in).

use crate::cuckoo_filter::{CuckooConfig, CuckooFilter};
use serde::{Deserialize, Serialize};

/// br-ft-events-dedup-cuckoo: dedup verdict for a [`EventCuckooDedup::check`]
/// query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CuckooDedupVerdict {
    /// Filter says key is NOT in set — definitely new (no false negatives).
    New,
    /// Filter says key IS in set — probably duplicate (true positive
    /// with bounded false-positive risk from 32-bit fingerprints).
    PossibleDuplicate,
}

/// br-ft-events-dedup-cuckoo: high-volume event deduplicator backed by
/// a Cuckoo filter. Drop-in alternative to
/// [`crate::events::EventDeduplicator`] for events where low false-
/// positive risk (true-new event silently dropped) is acceptable in
/// exchange for bounded memory.
///
/// See module docstring for the full trade-off analysis + when to
/// choose this over the exact EventDeduplicator.
#[derive(Debug, Clone)]
pub struct EventCuckooDedup {
    filter: CuckooFilter,
    /// Expected event capacity, used for telemetry + load-factor
    /// reporting.
    expected_items: usize,
}

impl Default for EventCuckooDedup {
    fn default() -> Self {
        Self::with_capacity(2000)
    }
}

impl EventCuckooDedup {
    /// Default expected capacity (matches EventDeduplicator's
    /// DEFAULT_MAX_CAPACITY of 2000 keys).
    pub const DEFAULT_CAPACITY: usize = 2000;

    /// Create a new dedup with the given expected capacity. The
    /// underlying CuckooFilter sizes itself for ~95% load factor at
    /// `expected_items`; inserts past that capacity will start
    /// failing with `InsertResult::Full`.
    #[must_use]
    pub fn with_capacity(expected_items: usize) -> Self {
        let expected_items = expected_items.max(16);
        Self {
            filter: CuckooFilter::with_capacity(expected_items),
            expected_items,
        }
    }

    /// Create a new dedup with explicit CuckooConfig.
    #[must_use]
    pub fn with_config(config: CuckooConfig) -> Self {
        let filter = CuckooFilter::with_config(config);
        let expected = filter.capacity();
        Self {
            filter,
            expected_items: expected,
        }
    }

    /// Check + record an event key.
    ///
    /// Returns `New` if the filter has no record of the key (definitely
    /// new — no false negatives) or `PossibleDuplicate` if the filter
    /// matches (true positive or fingerprint false-positive).
    ///
    /// On `New`, the key is inserted into the filter so subsequent
    /// `check()` calls for the same key return `PossibleDuplicate`.
    ///
    /// # Capacity behavior
    ///
    /// If the filter is full (`InsertResult::Full`), the verdict
    /// returned to the caller is still `New` (the caller should treat
    /// this event as a fresh observation), but the key is NOT
    /// recorded. Callers that care about full-state should check
    /// [`Self::load_factor`] periodically and call [`Self::clear`]
    /// when load exceeds a threshold (e.g., 0.95).
    pub fn check(&mut self, key: &str) -> CuckooDedupVerdict {
        if self.filter.lookup(key) {
            CuckooDedupVerdict::PossibleDuplicate
        } else {
            // Definitely new — record it for future dedup.
            // InsertResult::Full is treated as best-effort; the key
            // doesn't get recorded but the verdict is still New.
            let _ = self.filter.insert(key);
            CuckooDedupVerdict::New
        }
    }

    /// Forget a previously-recorded key (for window-expired entries).
    /// Returns true if the key was in the filter; false otherwise.
    ///
    /// Caller-driven window-eviction pattern: maintain a sidecar
    /// VecDeque of (key, recorded_at) pairs; periodically pop entries
    /// older than the window + call `forget()` on each.
    ///
    /// Cuckoo filter `delete()` is exact for true-positive entries and may
    /// incorrectly delete a different entry whose fingerprint collides with the
    /// requested key.
    /// In dedup terms: forgetting key A may also forget unrelated
    /// key B that shares A's fingerprint. This is acceptable for
    /// high-volume events (B's next observation is treated as `New`
    /// instead of `PossibleDuplicate` — same direction as the FPR
    /// trade-off).
    pub fn forget(&mut self, key: &str) -> bool {
        self.filter.delete(key)
    }

    /// Number of keys currently in the filter.
    #[must_use]
    pub fn count(&self) -> usize {
        self.filter.count()
    }

    /// Current load factor (count / capacity). Caller uses this to
    /// decide when to `clear()` or rotate to a fresh filter.
    #[must_use]
    pub fn load_factor(&self) -> f64 {
        self.filter.load_factor()
    }

    /// Reset the filter (drop all recorded keys).
    pub fn clear(&mut self) {
        self.filter.clear();
    }

    /// Memory used by the underlying filter in bytes (approximate).
    ///
    /// Includes the top-level filter struct, bucket array, and fingerprint
    /// storage, but not allocator bookkeeping.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.filter.memory_bytes()
    }

    /// Snapshot for inclusion in events / telemetry reports.
    #[must_use]
    pub fn snapshot(&self) -> EventCuckooDedupSnapshot {
        EventCuckooDedupSnapshot {
            count: self.count() as u64,
            expected_items: self.expected_items as u64,
            load_factor: self.load_factor(),
            memory_bytes: self.memory_bytes() as u64,
        }
    }
}

/// br-ft-events-dedup-cuckoo: serde snapshot for telemetry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventCuckooDedupSnapshot {
    pub count: u64,
    pub expected_items: u64,
    pub load_factor: f64,
    pub memory_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_dedup_returns_new_for_any_key() {
        let mut d = EventCuckooDedup::default();
        assert_eq!(d.check("evt-1"), CuckooDedupVerdict::New);
        assert_eq!(d.check("evt-2"), CuckooDedupVerdict::New);
    }

    #[test]
    fn repeated_check_of_same_key_returns_possible_duplicate() {
        let mut d = EventCuckooDedup::default();
        assert_eq!(d.check("evt-X"), CuckooDedupVerdict::New);
        assert_eq!(d.check("evt-X"), CuckooDedupVerdict::PossibleDuplicate);
        assert_eq!(d.check("evt-X"), CuckooDedupVerdict::PossibleDuplicate);
    }

    #[test]
    fn forget_removes_key_from_filter() {
        let mut d = EventCuckooDedup::default();
        d.check("evt-Y");
        assert_eq!(d.check("evt-Y"), CuckooDedupVerdict::PossibleDuplicate);
        let removed = d.forget("evt-Y");
        assert!(removed, "true positive forget should return true");
        // After forget, the key is treated as new again (assuming no
        // colliding fingerprint from another key).
        assert_eq!(d.check("evt-Y"), CuckooDedupVerdict::New);
    }

    #[test]
    fn clear_resets_filter() {
        let mut d = EventCuckooDedup::default();
        for i in 0..50 {
            d.check(&format!("evt-{i}"));
        }
        assert!(d.count() > 0);
        d.clear();
        assert_eq!(d.count(), 0);
        // After clear, all keys appear new again.
        assert_eq!(d.check("evt-0"), CuckooDedupVerdict::New);
    }

    #[test]
    fn count_reflects_inserted_keys() {
        let mut d = EventCuckooDedup::default();
        for i in 0..100 {
            d.check(&format!("evt-{i}"));
        }
        // Cuckoo filter count is approximate at high load + may
        // round to bucket boundaries; ±10% is reasonable.
        let c = d.count();
        assert!(
            (90..=110).contains(&c),
            "count={c} should be ≈ 100 after 100 distinct inserts"
        );
    }

    #[test]
    fn false_positive_rate_bounded_under_default_config() {
        // br-ft-events-dedup-cuckoo coverage test: insert 500 distinct
        // keys, then query 1000 brand-new keys. PossibleDuplicate
        // verdicts on the brand-new queries are false positives.
        // At default config (b=4, f=32), FPR is bounded by 2b/2^f.
        // Keep a generous sampling bound because this uses the same
        // non-cryptographic hash as production.
        let mut d = EventCuckooDedup::with_capacity(2000);
        for i in 0..500 {
            d.check(&format!("recorded-{i}"));
        }
        let mut false_positives = 0u32;
        for i in 0..1000 {
            // Distinct prefix so no overlap with "recorded-N".
            let k = format!("query-{i}");
            if d.check(&k) == CuckooDedupVerdict::PossibleDuplicate {
                false_positives += 1;
            }
        }
        let fpr = false_positives as f64 / 1000.0;
        assert!(
            fpr < 0.10,
            "br-ft-events-dedup-cuckoo bounded-FPR: must be < 0.10 \
             (got {fpr:.4}); cuckoo filter FPR is bounded by 32-bit fingerprints",
        );
    }

    #[test]
    fn snapshot_carries_documented_fields() {
        let mut d = EventCuckooDedup::with_capacity(1024);
        for i in 0..100 {
            d.check(&format!("evt-{i}"));
        }
        let s = d.snapshot();
        assert!(s.count > 0);
        assert_eq!(s.expected_items, 1024);
        assert!(s.load_factor > 0.0 && s.load_factor < 1.0);
        assert!(s.memory_bytes > 0);
    }

    #[test]
    fn custom_config_snapshot_uses_normalized_filter_capacity() {
        let d = EventCuckooDedup::with_config(CuckooConfig {
            num_buckets: 3,
            bucket_size: 0,
            max_kicks: 10,
        });
        let s = d.snapshot();
        assert_eq!(s.expected_items, 4);
    }

    #[test]
    fn snapshot_serde_roundtrips() {
        let mut d = EventCuckooDedup::default();
        for i in 0..50 {
            d.check(&format!("evt-{i}"));
        }
        let snap = d.snapshot();
        let json = serde_json::to_string(&snap).expect("snapshot serializes");
        let parsed: EventCuckooDedupSnapshot =
            serde_json::from_str(&json).expect("snapshot deserializes");
        assert_eq!(parsed, snap);
    }

    #[test]
    fn memory_bytes_far_smaller_than_hashmap_dedup() {
        // br-ft-events-dedup-cuckoo memory advantage: the default
        // filter is much smaller than the approximate 280KB
        // HashMap-backed EventDeduplicator at 2000 keys, but the
        // estimate must include u32 fingerprints and bucket metadata.
        let d = EventCuckooDedup::with_capacity(2000);
        let bytes = d.memory_bytes();
        assert!(
            bytes >= 16 * 1024,
            "default cuckoo dedup estimate must include u32 fingerprint slots; got {bytes} bytes"
        );
        assert!(
            bytes < 96 * 1024,
            "cuckoo dedup should stay well below HashMap-backed dedup; got {bytes} bytes"
        );
    }
}
