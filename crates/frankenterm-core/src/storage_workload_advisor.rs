//! br-ft-1650n.15: Storage/Search Workload Advisor substrate.
//!
//! Analyzes a serializable [`WorkloadProfile`] and emits a
//! [`StorageRecommendation`] that pairs a backend/index choice
//! with a confidence band and the proof commands an operator can
//! run to validate the suggestion before migrating.
//!
//! Companion of `ft-l1jgo` (StorageBackend trait callsite
//! migration): once callsites can route through a trait, operators
//! still need evidence for **which** backend and index strategy
//! fits a workload. This module is the substrate for that
//! evidence pipeline.
//!
//! ## What ships in this slice
//!
//! - [`WorkloadProfile`] — serde-friendly snapshot of the
//!   write/read/search mix, FTS/Tantivy usage, hot tables,
//!   cardinality estimates, checkpoint lag, and tail latency.
//! - [`StorageRecommendation`] — the advisor's structured output:
//!   `backend`, `index`, `migration_priority`, `confidence`, and a
//!   list of `proof_commands` an operator should run before
//!   migrating.
//! - [`AdvisorReport`] — the union of `Recommendation(_)` and
//!   `DataNeeded { reasons }` so the caller can distinguish "we
//!   recommend X" from "we don't have enough signal yet".
//! - [`classify`] — the substrate classifier. Pure function over
//!   `WorkloadProfile` with documented thresholds. Emits
//!   `AdvisorReport`.
//!
//! ## What is deferred
//!
//! - Live wiring: the wired-pass needs a feeder that builds
//!   `WorkloadProfile` from runtime telemetry (storage health
//!   snapshot + tantivy stats + cardinality sketch). That's a
//!   follow-up bead — the substrate ships as a pure function
//!   so the feeder can iterate independently.
//! - End-to-end replay harness: the bead's "Replay/e2e with
//!   synthetic storage profiles" item builds a fixture-driven
//!   integration test on top of this substrate. The unit tests
//!   below pin the classifier thresholds; e2e-replay is a
//!   wired-pass concern.
//! - Cross-callsite recommendations: per-table or per-query
//!   advice would require coupling to the `ft-l1jgo` callsite
//!   migration plan (`scripts/storage_backend_callsites.py`).
//!   The current substrate emits whole-workload recommendations
//!   only.

use serde::{Deserialize, Serialize};

/// br-ft-1650n.15: serializable snapshot of a storage/search
/// workload's high-level signature, suitable as input to the
/// advisor classifier.
///
/// Operators (or wired-pass collectors) populate the fields
/// from a combination of:
/// - `StorageHandle::stats()` for write/read/search counts +
///   checkpoint lag.
/// - `frankenterm-core-tantivy::tantivy_query::SearchService`
///   metrics for FTS/Tantivy usage.
/// - `storage_cardinality_sketch::StorageDistinctSketchSnapshot`
///   for distinct-pane / distinct-session cardinality estimates.
/// - Per-table row counts via the StorageBackend trait's
///   `count_table` helper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkloadProfile {
    /// Total write operations observed during the sampling
    /// window.
    pub total_writes: u64,
    /// Total read operations observed.
    pub total_reads: u64,
    /// Total search (FTS / Tantivy / semantic) operations.
    pub total_searches: u64,
    /// Whether the FTS5 virtual table is in use this session.
    pub fts_enabled: bool,
    /// Whether the tantivy index is in use this session.
    pub tantivy_enabled: bool,
    /// Approximate distinct pane_id cardinality estimate.
    /// Derived from `StorageDistinctSketch::estimated_distinct_panes`
    /// in production wiring.
    pub estimated_distinct_panes: u64,
    /// Approximate distinct session_id cardinality estimate.
    pub estimated_distinct_sessions: u64,
    /// Largest table by row count, if any.
    pub hot_table: Option<HotTableSnapshot>,
    /// p99 write latency observed in the sampling window
    /// (microseconds). 0 if not measured.
    pub p99_write_latency_us: u64,
    /// p99 read latency observed (microseconds).
    pub p99_read_latency_us: u64,
    /// Last-known checkpoint lag in bytes (WAL frames not yet
    /// checkpointed).
    pub checkpoint_lag_bytes: u64,
}

impl Default for WorkloadProfile {
    fn default() -> Self {
        Self {
            total_writes: 0,
            total_reads: 0,
            total_searches: 0,
            fts_enabled: false,
            tantivy_enabled: false,
            estimated_distinct_panes: 0,
            estimated_distinct_sessions: 0,
            hot_table: None,
            p99_write_latency_us: 0,
            p99_read_latency_us: 0,
            checkpoint_lag_bytes: 0,
        }
    }
}

/// Snapshot of the largest-by-rowcount table at sampling time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotTableSnapshot {
    pub name: String,
    pub row_count: u64,
}

/// Workload mix classification used as a coarse pre-filter
/// before the full classifier runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadMix {
    /// Writes dominate (≥ 60% of operations).
    WriteHeavy,
    /// Searches dominate (≥ 30% of operations and at least
    /// 1.5× the write share).
    SearchHeavy,
    /// Reads dominate (≥ 50% of operations) without search
    /// dominance.
    ReadHeavy,
    /// Roughly balanced or sample too small to bucket.
    Balanced,
}

impl WorkloadProfile {
    /// Total operations across writes + reads + searches.
    /// 0 implies the sample is empty.
    #[must_use]
    pub fn total_ops(&self) -> u64 {
        self.total_writes
            .saturating_add(self.total_reads)
            .saturating_add(self.total_searches)
    }

    /// Coarse workload-mix classification. Used as a pre-filter
    /// in [`classify`]; exposed for direct dashboard display.
    #[must_use]
    pub fn mix(&self) -> WorkloadMix {
        let total = self.total_ops();
        if total == 0 {
            return WorkloadMix::Balanced;
        }
        let writes = self.total_writes;
        let reads = self.total_reads;
        let searches = self.total_searches;
        // SearchHeavy gate: searches ≥ 30% AND searches ≥ 1.5×
        // writes (so a balanced 33/33/33 isn't classified as
        // search-heavy; need a clear majority).
        if searches.saturating_mul(10) >= total.saturating_mul(3)
            && searches.saturating_mul(2) >= writes.saturating_mul(3)
        {
            return WorkloadMix::SearchHeavy;
        }
        if writes.saturating_mul(10) >= total.saturating_mul(6) {
            return WorkloadMix::WriteHeavy;
        }
        if reads.saturating_mul(10) >= total.saturating_mul(5) {
            return WorkloadMix::ReadHeavy;
        }
        WorkloadMix::Balanced
    }
}

/// Recommended backend choice. Mirrors the substrate types
/// `RusqliteBackend` (today) and `FrankenSQLiteBackend` (future,
/// gated on ft-kcdqp).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendChoice {
    /// Stay on the RusqliteBackend (the production default).
    Rusqlite,
    /// Migrate to FrankenSQLiteBackend once ft-kcdqp lands.
    /// Currently a forward-looking recommendation only.
    FrankenSqlite,
    /// Sample is too sparse / signals contradict; the advisor
    /// has no specific backend recommendation.
    NoChange,
}

/// Recommended index strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexChoice {
    /// FTS5 virtual table on `output_segments` (the SQLite-native
    /// path; lowest operational complexity).
    Fts5,
    /// Tantivy lexical index (richer query language; separate
    /// process / file layout).
    Tantivy,
    /// Hybrid: FTS5 for short-query latency + Tantivy for
    /// complex-query feature parity.
    Hybrid,
    /// Sample is too sparse to recommend an index strategy;
    /// fall back to whatever's already provisioned.
    NoChange,
}

/// Migration priority band. Operators read this to decide
/// whether to schedule a migration window now or defer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPriority {
    /// No action required.
    None,
    /// Schedule when convenient — improvements measurable but
    /// not blocking.
    Low,
    /// Migrate within the next planning window — significant
    /// tail-latency or capacity headroom at stake.
    Medium,
    /// Migrate now — current configuration is approaching a
    /// known cliff (checkpoint lag, hot-table scan latency).
    High,
}

/// Confidence band for the recommendation. The advisor emits a
/// confidence value alongside every concrete recommendation so
/// operators can weight it against their own intuition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Sample large + signals consistent — recommendation is
    /// well-founded.
    High,
    /// Sample large enough to be informative but with at least
    /// one ambiguous signal.
    Medium,
    /// Sample is small or signals contradict; the recommendation
    /// is a best-guess.
    Low,
}

/// br-ft-1650n.15: structured advisor output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageRecommendation {
    pub backend: BackendChoice,
    pub index: IndexChoice,
    pub migration_priority: MigrationPriority,
    pub confidence: Confidence,
    /// Operator-facing rationale string (one to three sentences).
    pub rationale: String,
    /// Shell commands the operator should run to verify the
    /// recommendation before migrating. Each entry is a
    /// well-formed command-line string (no shell metachars
    /// requiring escape).
    pub proof_commands: Vec<String>,
}

/// Final advisor report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AdvisorReport {
    /// The advisor produced a concrete recommendation.
    Recommendation(StorageRecommendation),
    /// The sample was too sparse or signals were contradictory;
    /// the advisor declined to recommend and returned the
    /// reasons so the operator can collect more telemetry.
    DataNeeded { reasons: Vec<String> },
}

/// Minimum sample size before the classifier emits a concrete
/// recommendation. Below this, the advisor returns
/// `AdvisorReport::DataNeeded`.
const MIN_SAMPLE_OPS: u64 = 1_000;

/// br-ft-1650n.15: classify a workload profile and emit an
/// advisor report.
///
/// Pure function — same input produces same output, with no
/// hidden state. Operators can call this from any context (CLI,
/// dashboard, replay harness).
#[must_use]
pub fn classify(profile: &WorkloadProfile) -> AdvisorReport {
    let total = profile.total_ops();

    // Sparse-sample gate.
    if total < MIN_SAMPLE_OPS {
        let mut reasons = Vec::new();
        reasons.push(format!(
            "total ops {total} below MIN_SAMPLE_OPS {MIN_SAMPLE_OPS}"
        ));
        if !profile.fts_enabled && !profile.tantivy_enabled {
            reasons
                .push("no search backend in use; cannot recommend index strategy".to_string());
        }
        return AdvisorReport::DataNeeded { reasons };
    }

    let mix = profile.mix();

    // Backend choice — keep conservative until ft-kcdqp lands.
    // For now: always recommend Rusqlite. The FrankenSqlite
    // recommendation lights up in a follow-up bead once the
    // backend ships and tail-latency measurements indicate a
    // concrete win.
    let backend = BackendChoice::Rusqlite;

    // Index strategy.
    let index = match (mix, profile.fts_enabled, profile.tantivy_enabled) {
        (WorkloadMix::SearchHeavy, true, true) => IndexChoice::Hybrid,
        (WorkloadMix::SearchHeavy, false, true) => IndexChoice::Tantivy,
        (WorkloadMix::SearchHeavy, true, false) => IndexChoice::Fts5,
        (_, true, true) => IndexChoice::Fts5, // not search-heavy → simpler path
        (_, false, true) => IndexChoice::Tantivy,
        (_, true, false) => IndexChoice::Fts5,
        (_, false, false) => IndexChoice::NoChange,
    };

    // Migration priority.
    let priority = if profile.checkpoint_lag_bytes > 64 * 1024 * 1024 {
        // Approaching WAL-checkpoint cliff (default page_size *
        // wal_autocheckpoint).
        MigrationPriority::High
    } else if profile.p99_write_latency_us > 100_000 {
        // 100 ms p99 write latency indicates real backpressure.
        MigrationPriority::Medium
    } else if profile.p99_read_latency_us > 50_000 {
        MigrationPriority::Low
    } else {
        MigrationPriority::None
    };

    // Confidence band.
    let confidence = if total >= MIN_SAMPLE_OPS * 10
        && (profile.fts_enabled || profile.tantivy_enabled)
    {
        Confidence::High
    } else if total >= MIN_SAMPLE_OPS * 3 {
        Confidence::Medium
    } else {
        Confidence::Low
    };

    let rationale = format!(
        "Mix: {mix:?}; FTS5 enabled: {fts}; Tantivy enabled: {tantivy}; \
         distinct panes: {panes}; p99 write μs: {p99w}; p99 read μs: {p99r}; \
         checkpoint lag: {lag} bytes",
        fts = profile.fts_enabled,
        tantivy = profile.tantivy_enabled,
        panes = profile.estimated_distinct_panes,
        p99w = profile.p99_write_latency_us,
        p99r = profile.p99_read_latency_us,
        lag = profile.checkpoint_lag_bytes,
    );

    let proof_commands = vec![
        "ft storage doctor --json".to_string(),
        "ft storage stats --tail-latency --p99".to_string(),
    ];

    AdvisorReport::Recommendation(StorageRecommendation {
        backend,
        index,
        migration_priority: priority,
        confidence,
        rationale,
        proof_commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with(total_writes: u64, total_reads: u64, total_searches: u64) -> WorkloadProfile {
        WorkloadProfile {
            total_writes,
            total_reads,
            total_searches,
            ..WorkloadProfile::default()
        }
    }

    #[test]
    fn empty_profile_returns_data_needed() {
        let report = classify(&WorkloadProfile::default());
        assert!(matches!(report, AdvisorReport::DataNeeded { .. }));
    }

    #[test]
    fn sparse_profile_returns_data_needed() {
        let profile = profile_with(100, 100, 100);
        let report = classify(&profile);
        match report {
            AdvisorReport::DataNeeded { reasons } => {
                assert!(!reasons.is_empty());
                assert!(reasons.iter().any(|r| r.contains("MIN_SAMPLE_OPS")));
            }
            other => panic!("expected DataNeeded, got {other:?}"),
        }
    }

    #[test]
    fn write_heavy_classifies_as_write_heavy() {
        let profile = profile_with(800, 100, 100);
        assert_eq!(profile.mix(), WorkloadMix::WriteHeavy);
    }

    #[test]
    fn search_heavy_classifies_as_search_heavy() {
        let profile = profile_with(100, 100, 800);
        assert_eq!(profile.mix(), WorkloadMix::SearchHeavy);
    }

    #[test]
    fn read_heavy_classifies_as_read_heavy() {
        let profile = profile_with(100, 800, 100);
        assert_eq!(profile.mix(), WorkloadMix::ReadHeavy);
    }

    #[test]
    fn balanced_classifies_as_balanced() {
        let profile = profile_with(333, 333, 334);
        assert_eq!(profile.mix(), WorkloadMix::Balanced);
    }

    #[test]
    fn search_heavy_with_both_indexes_recommends_hybrid() {
        let profile = WorkloadProfile {
            total_writes: 100,
            total_reads: 100,
            total_searches: 1_000,
            fts_enabled: true,
            tantivy_enabled: true,
            ..WorkloadProfile::default()
        };
        let report = classify(&profile);
        match report {
            AdvisorReport::Recommendation(rec) => {
                assert_eq!(rec.index, IndexChoice::Hybrid);
                assert_eq!(rec.backend, BackendChoice::Rusqlite);
            }
            other => panic!("expected Recommendation, got {other:?}"),
        }
    }

    #[test]
    fn search_heavy_with_only_tantivy_recommends_tantivy() {
        let profile = WorkloadProfile {
            total_writes: 100,
            total_reads: 100,
            total_searches: 1_000,
            fts_enabled: false,
            tantivy_enabled: true,
            ..WorkloadProfile::default()
        };
        match classify(&profile) {
            AdvisorReport::Recommendation(rec) => assert_eq!(rec.index, IndexChoice::Tantivy),
            other => panic!("expected Recommendation, got {other:?}"),
        }
    }

    #[test]
    fn checkpoint_lag_above_threshold_pushes_high_priority() {
        let profile = WorkloadProfile {
            total_writes: 1_500,
            total_reads: 1_500,
            total_searches: 1_500,
            fts_enabled: true,
            tantivy_enabled: false,
            checkpoint_lag_bytes: 128 * 1024 * 1024,
            ..WorkloadProfile::default()
        };
        match classify(&profile) {
            AdvisorReport::Recommendation(rec) => {
                assert_eq!(rec.migration_priority, MigrationPriority::High);
            }
            other => panic!("expected Recommendation, got {other:?}"),
        }
    }

    #[test]
    fn high_p99_write_latency_pushes_medium_priority() {
        let profile = WorkloadProfile {
            total_writes: 1_500,
            total_reads: 1_500,
            total_searches: 1_500,
            fts_enabled: true,
            tantivy_enabled: false,
            p99_write_latency_us: 250_000, // 250 ms
            ..WorkloadProfile::default()
        };
        match classify(&profile) {
            AdvisorReport::Recommendation(rec) => {
                assert_eq!(rec.migration_priority, MigrationPriority::Medium);
            }
            other => panic!("expected Recommendation, got {other:?}"),
        }
    }

    #[test]
    fn no_search_backend_recommends_no_change_index() {
        let profile = WorkloadProfile {
            total_writes: 1_500,
            total_reads: 1_500,
            total_searches: 1_500,
            fts_enabled: false,
            tantivy_enabled: false,
            ..WorkloadProfile::default()
        };
        match classify(&profile) {
            AdvisorReport::Recommendation(rec) => assert_eq!(rec.index, IndexChoice::NoChange),
            other => panic!("expected Recommendation, got {other:?}"),
        }
    }

    #[test]
    fn high_confidence_requires_large_sample_and_search_backend() {
        let profile = WorkloadProfile {
            total_writes: 5_000,
            total_reads: 5_000,
            total_searches: 5_000,
            fts_enabled: true,
            tantivy_enabled: false,
            ..WorkloadProfile::default()
        };
        match classify(&profile) {
            AdvisorReport::Recommendation(rec) => assert_eq!(rec.confidence, Confidence::High),
            other => panic!("expected Recommendation, got {other:?}"),
        }
    }

    #[test]
    fn recommendation_serde_roundtrip() {
        let rec = StorageRecommendation {
            backend: BackendChoice::Rusqlite,
            index: IndexChoice::Hybrid,
            migration_priority: MigrationPriority::High,
            confidence: Confidence::High,
            rationale: "test".to_string(),
            proof_commands: vec!["ft storage doctor --json".to_string()],
        };
        let json = serde_json::to_string(&rec).expect("serialize");
        let back: StorageRecommendation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(rec, back);
    }

    #[test]
    fn report_serde_roundtrip_for_both_variants() {
        let report = AdvisorReport::DataNeeded {
            reasons: vec!["sample too small".to_string()],
        };
        let json = serde_json::to_string(&report).expect("serialize");
        let back: AdvisorReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(report, back);

        let rec = AdvisorReport::Recommendation(StorageRecommendation {
            backend: BackendChoice::NoChange,
            index: IndexChoice::Fts5,
            migration_priority: MigrationPriority::Low,
            confidence: Confidence::Medium,
            rationale: "x".to_string(),
            proof_commands: vec![],
        });
        let json = serde_json::to_string(&rec).expect("serialize");
        let back: AdvisorReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(rec, back);
    }
}
