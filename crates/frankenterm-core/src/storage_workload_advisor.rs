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

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::events::MetricsSnapshot;
use crate::storage_cardinality_sketch::StorageDistinctSketchSnapshot;
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

/// Snapshot of the largest-by-rowcount table at sampling time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotTableSnapshot {
    pub name: String,
    pub row_count: u64,
}

/// br-ft-1650n.15: bundle of write/read/search operation counts
/// sampled from `StorageHandle::stats()` over the advisor's
/// sampling window. Used by [`WorkloadProfile::from_snapshots`]
/// so the feeder doesn't need a positional 3-tuple at the call
/// site.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadOpCounts {
    pub writes: u64,
    pub reads: u64,
    pub searches: u64,
}

impl WorkloadOpCounts {
    #[must_use]
    pub const fn new(writes: u64, reads: u64, searches: u64) -> Self {
        Self {
            writes,
            reads,
            searches,
        }
    }
}

/// br-ft-1650n.15: which lexical search backends are actually
/// registered in this session. The advisor uses both flags to
/// pick `IndexChoice::{Fts5, Tantivy, Hybrid, NoChange}` —
/// see [`classify`] for the truth table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchBackendsInUse {
    pub fts5: bool,
    pub tantivy: bool,
}

impl SearchBackendsInUse {
    #[must_use]
    pub const fn fts5_only() -> Self {
        Self {
            fts5: true,
            tantivy: false,
        }
    }
    #[must_use]
    pub const fn tantivy_only() -> Self {
        Self {
            fts5: false,
            tantivy: true,
        }
    }
    #[must_use]
    pub const fn both() -> Self {
        Self {
            fts5: true,
            tantivy: true,
        }
    }
    #[must_use]
    pub const fn neither() -> Self {
        Self {
            fts5: false,
            tantivy: false,
        }
    }
}

/// br-ft-1650n.15: tail-latency snapshot in microseconds. The
/// advisor uses both fields in `MigrationPriority` gates —
/// p99_write > 100 ms → Medium; p99_read > 50 ms → Low.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailLatencySnapshot {
    pub p99_write_us: u64,
    pub p99_read_us: u64,
}

impl TailLatencySnapshot {
    #[must_use]
    pub const fn new(p99_write_us: u64, p99_read_us: u64) -> Self {
        Self {
            p99_write_us,
            p99_read_us,
        }
    }
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

    /// br-ft-1650n.15 wired-pass slice: build a `WorkloadProfile`
    /// from ready-made runtime snapshots. The CLI / dashboard
    /// path collects these snapshots independently (different
    /// permission requirements, different sampling cadences) and
    /// hands them here to materialize the advisor input.
    ///
    /// `op_counts` is the (writes, reads, searches) tuple sampled
    /// from `StorageHandle::stats()` over the dashboard window.
    /// `search_backends` records which index backends are actually
    /// in use this session (FTS5, Tantivy). `tail_latency` is the
    /// (p99_write_us, p99_read_us) pair from telemetry. `lag_bytes`
    /// is the WAL checkpoint lag.
    ///
    /// Returns `WorkloadProfile` ready to hand to [`classify`].
    /// All inputs are non-allocating values; the builder is a
    /// pure function over its arguments and `cardinality` is
    /// `Copy` (it's a serde struct of u64/f64 fields).
    #[must_use]
    pub fn from_snapshots(
        op_counts: WorkloadOpCounts,
        search_backends: SearchBackendsInUse,
        cardinality: &StorageDistinctSketchSnapshot,
        hot_table: Option<HotTableSnapshot>,
        tail_latency: TailLatencySnapshot,
        checkpoint_lag_bytes: u64,
    ) -> Self {
        Self {
            total_writes: op_counts.writes,
            total_reads: op_counts.reads,
            total_searches: op_counts.searches,
            fts_enabled: search_backends.fts5,
            tantivy_enabled: search_backends.tantivy,
            estimated_distinct_panes: cardinality.estimated_distinct_panes,
            estimated_distinct_sessions: cardinality.estimated_distinct_sessions,
            hot_table,
            p99_write_latency_us: tail_latency.p99_write_us,
            p99_read_latency_us: tail_latency.p99_read_us,
            checkpoint_lag_bytes,
        }
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

/// br-ft-1650n.15 live-wiring: build a `WorkloadProfile` from
/// the real `EventBusMetrics::snapshot()` plus a
/// `StorageDistinctSketchSnapshot`, threading the storage-side
/// fields a caller still has to provide (since the EventBus
/// only sees event-flow counts, not direct storage write/read
/// metrics).
///
/// **Fields derived from `MetricsSnapshot`:**
/// - `total_writes`: `events_published` (approx — every published
///   event corresponds to a captured-write at the storage layer
///   in production).
/// - `total_reads`: `events_delivered` (each delivered event
///   represents at least one subscriber pulling state).
/// - `total_searches`: caller-supplied; the EventBus doesn't
///   distinguish search from other read traffic.
///
/// **Fields derived from `StorageDistinctSketchSnapshot`:**
/// - `estimated_distinct_panes`
/// - `estimated_distinct_sessions`
///
/// **Caller-supplied (the EventBus has no signal for these):**
/// - `total_searches` (FTS5/Tantivy query count from the search
///   layer's own counters).
/// - `search_backends` (which backends are registered).
/// - `hot_table` (caller queries `count_table` for the candidate
///   tables and picks the largest).
/// - `tail_latency` (from the latency telemetry pipeline).
/// - `checkpoint_lag_bytes` (from `PRAGMA wal_checkpoint` or
///   storage-doctor).
///
/// The CLI / dashboard wire-up is bounded to those caller-supplied
/// fields. This function is the bridge from observable metrics
/// to a structured `WorkloadProfile`.
#[must_use]
pub fn build_profile_from_event_bus_metrics(
    metrics: &MetricsSnapshot,
    cardinality: &StorageDistinctSketchSnapshot,
    total_searches: u64,
    search_backends: SearchBackendsInUse,
    hot_table: Option<HotTableSnapshot>,
    tail_latency: TailLatencySnapshot,
    checkpoint_lag_bytes: u64,
) -> WorkloadProfile {
    WorkloadProfile::from_snapshots(
        WorkloadOpCounts::new(
            metrics.events_published,
            metrics.events_delivered,
            total_searches,
        ),
        search_backends,
        cardinality,
        hot_table,
        tail_latency,
        checkpoint_lag_bytes,
    )
}

/// br-ft-1650n.15 live-wiring: end-to-end CLI/dashboard entry
/// point. Builds a `WorkloadProfile` from the supplied snapshots
/// and runs the classifier in one call.
///
/// Operators wiring `ft storage advise` (or the equivalent
/// dashboard panel) call this with the snapshots they've
/// already collected from telemetry pipelines. The function is
/// pure — same inputs always produce the same `AdvisorReport`.
#[must_use]
pub fn advise_from_event_bus_metrics(
    metrics: &MetricsSnapshot,
    cardinality: &StorageDistinctSketchSnapshot,
    total_searches: u64,
    search_backends: SearchBackendsInUse,
    hot_table: Option<HotTableSnapshot>,
    tail_latency: TailLatencySnapshot,
    checkpoint_lag_bytes: u64,
) -> AdvisorReport {
    let profile = build_profile_from_event_bus_metrics(
        metrics,
        cardinality,
        total_searches,
        search_backends,
        hot_table,
        tail_latency,
        checkpoint_lag_bytes,
    );
    classify(&profile)
}

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
            reasons.push("no search backend in use; cannot recommend index strategy".to_string());
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
    let confidence =
        if total >= MIN_SAMPLE_OPS * 10 && (profile.fts_enabled || profile.tantivy_enabled) {
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

/// br-ft-1650n.15 autoscaler-observable metrics: counter substrate
/// for autoscalers and dashboards that poll the advisor's verdict
/// stream without keeping a history.
///
/// Each `record_report` call atomically bumps the per-variant
/// counters for `IndexChoice`, `MigrationPriority`, and
/// `Confidence`, plus the `total_recommendations` /
/// `total_data_needed` rollups. `last_priority_level` is updated
/// to the most recent migration-priority level so an autoscaler
/// can poll a single field instead of replaying the entire
/// history.
///
/// Counter ordering: `Relaxed` is intentional — this is an
/// observability surface, not a synchronization primitive. The
/// only invariant is that each individual counter increments
/// monotonically. `last_priority_level` uses the same `Relaxed`
/// memory order; readers should accept that it may be observed
/// out-of-order with `total_recommendations` (e.g., a reader
/// might see `total_recommendations = N+1` paired with
/// `last_priority_level` from observation N). For autoscaler
/// gating this is acceptable because the counter sequence
/// converges within microseconds.
#[derive(Debug)]
pub struct AdvisorMetrics {
    index_fts5_count: AtomicU64,
    index_tantivy_count: AtomicU64,
    index_hybrid_count: AtomicU64,
    index_no_change_count: AtomicU64,
    priority_none_count: AtomicU64,
    priority_low_count: AtomicU64,
    priority_medium_count: AtomicU64,
    priority_high_count: AtomicU64,
    confidence_high_count: AtomicU64,
    confidence_medium_count: AtomicU64,
    confidence_low_count: AtomicU64,
    total_recommendations: AtomicU64,
    total_data_needed: AtomicU64,
    /// 0 = unset/no observation yet; 1 = None; 2 = Low; 3 = Medium;
    /// 4 = High. Encoded as a numeric ladder so an autoscaler can
    /// gate on `≥ 3` without having to compare enum variants.
    last_priority_level: AtomicU8,
}

impl Default for AdvisorMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl AdvisorMetrics {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            index_fts5_count: AtomicU64::new(0),
            index_tantivy_count: AtomicU64::new(0),
            index_hybrid_count: AtomicU64::new(0),
            index_no_change_count: AtomicU64::new(0),
            priority_none_count: AtomicU64::new(0),
            priority_low_count: AtomicU64::new(0),
            priority_medium_count: AtomicU64::new(0),
            priority_high_count: AtomicU64::new(0),
            confidence_high_count: AtomicU64::new(0),
            confidence_medium_count: AtomicU64::new(0),
            confidence_low_count: AtomicU64::new(0),
            total_recommendations: AtomicU64::new(0),
            total_data_needed: AtomicU64::new(0),
            last_priority_level: AtomicU8::new(0),
        }
    }

    /// Record a single advisor report. Bumps the counters for
    /// every dimension reflected in the report, and updates
    /// `last_priority_level` so an autoscaler can poll the most
    /// recent priority without keeping a history.
    pub fn record_report(&self, report: &AdvisorReport) {
        match report {
            AdvisorReport::Recommendation(rec) => {
                self.total_recommendations.fetch_add(1, Ordering::Relaxed);
                let index_counter = match rec.index {
                    IndexChoice::Fts5 => &self.index_fts5_count,
                    IndexChoice::Tantivy => &self.index_tantivy_count,
                    IndexChoice::Hybrid => &self.index_hybrid_count,
                    IndexChoice::NoChange => &self.index_no_change_count,
                };
                index_counter.fetch_add(1, Ordering::Relaxed);
                let (priority_counter, ladder) = match rec.migration_priority {
                    MigrationPriority::None => (&self.priority_none_count, 1u8),
                    MigrationPriority::Low => (&self.priority_low_count, 2u8),
                    MigrationPriority::Medium => (&self.priority_medium_count, 3u8),
                    MigrationPriority::High => (&self.priority_high_count, 4u8),
                };
                priority_counter.fetch_add(1, Ordering::Relaxed);
                self.last_priority_level.store(ladder, Ordering::Relaxed);
                let confidence_counter = match rec.confidence {
                    Confidence::High => &self.confidence_high_count,
                    Confidence::Medium => &self.confidence_medium_count,
                    Confidence::Low => &self.confidence_low_count,
                };
                confidence_counter.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    advisor_event = "recommendation_recorded",
                    index = ?rec.index,
                    priority = ?rec.migration_priority,
                    confidence = ?rec.confidence,
                    backend = ?rec.backend,
                    "advisor recommendation observed"
                );
            }
            AdvisorReport::DataNeeded { reasons } => {
                self.total_data_needed.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    advisor_event = "data_needed_recorded",
                    reason_count = reasons.len(),
                    "advisor data-needed observed"
                );
            }
        }
    }

    /// Most recent migration priority observed, or `None` if no
    /// recommendation has been recorded yet. Autoscalers gate on
    /// the priority directly; the ladder encoding lets dashboards
    /// rank-compare without an extra `match`.
    #[must_use]
    pub fn last_priority(&self) -> Option<MigrationPriority> {
        match self.last_priority_level.load(Ordering::Relaxed) {
            0 => None,
            1 => Some(MigrationPriority::None),
            2 => Some(MigrationPriority::Low),
            3 => Some(MigrationPriority::Medium),
            4 => Some(MigrationPriority::High),
            _ => None,
        }
    }

    /// True if the most recent recommendation was
    /// `MigrationPriority::High`. Convenience for autoscalers
    /// gating a fast-path eviction or shed signal.
    #[must_use]
    pub fn is_critical(&self) -> bool {
        self.last_priority_level.load(Ordering::Relaxed) >= 4
    }

    /// Snapshot every counter for export to dashboards or
    /// autoscaler control loops.
    #[must_use]
    pub fn snapshot(&self) -> AdvisorMetricsSnapshot {
        AdvisorMetricsSnapshot {
            index_fts5_count: self.index_fts5_count.load(Ordering::Relaxed),
            index_tantivy_count: self.index_tantivy_count.load(Ordering::Relaxed),
            index_hybrid_count: self.index_hybrid_count.load(Ordering::Relaxed),
            index_no_change_count: self.index_no_change_count.load(Ordering::Relaxed),
            priority_none_count: self.priority_none_count.load(Ordering::Relaxed),
            priority_low_count: self.priority_low_count.load(Ordering::Relaxed),
            priority_medium_count: self.priority_medium_count.load(Ordering::Relaxed),
            priority_high_count: self.priority_high_count.load(Ordering::Relaxed),
            confidence_high_count: self.confidence_high_count.load(Ordering::Relaxed),
            confidence_medium_count: self.confidence_medium_count.load(Ordering::Relaxed),
            confidence_low_count: self.confidence_low_count.load(Ordering::Relaxed),
            total_recommendations: self.total_recommendations.load(Ordering::Relaxed),
            total_data_needed: self.total_data_needed.load(Ordering::Relaxed),
            last_priority_level: self.last_priority_level.load(Ordering::Relaxed),
        }
    }

    /// br-ft-1650n.15 autoscaler/CLI convenience: build a profile
    /// from the EventBus snapshot + caller-supplied search-side
    /// signals, run the classifier, record the verdict in this
    /// metrics instance, and return the report. The natural shape
    /// for a dashboard or autoscaler control-loop iteration that
    /// wants both the verdict and the side-effect of bumping the
    /// observability counters in a single call.
    #[allow(clippy::too_many_arguments)]
    pub fn recommend_and_record(
        &self,
        metrics: &MetricsSnapshot,
        cardinality: &StorageDistinctSketchSnapshot,
        total_searches: u64,
        search_backends: SearchBackendsInUse,
        hot_table: Option<HotTableSnapshot>,
        tail_latency: TailLatencySnapshot,
        checkpoint_lag_bytes: u64,
    ) -> AdvisorReport {
        let report = advise_from_event_bus_metrics(
            metrics,
            cardinality,
            total_searches,
            search_backends,
            hot_table,
            tail_latency,
            checkpoint_lag_bytes,
        );
        self.record_report(&report);
        report
    }
}

/// Serializable snapshot of [`AdvisorMetrics`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvisorMetricsSnapshot {
    pub index_fts5_count: u64,
    pub index_tantivy_count: u64,
    pub index_hybrid_count: u64,
    pub index_no_change_count: u64,
    pub priority_none_count: u64,
    pub priority_low_count: u64,
    pub priority_medium_count: u64,
    pub priority_high_count: u64,
    pub confidence_high_count: u64,
    pub confidence_medium_count: u64,
    pub confidence_low_count: u64,
    pub total_recommendations: u64,
    pub total_data_needed: u64,
    pub last_priority_level: u8,
}

impl AdvisorMetricsSnapshot {
    /// Decode the `last_priority_level` ladder back into a typed
    /// `Option<MigrationPriority>`. `None` means no recommendation
    /// has been recorded yet (counter is at the sentinel `0`).
    #[must_use]
    pub fn last_priority(&self) -> Option<MigrationPriority> {
        match self.last_priority_level {
            0 => None,
            1 => Some(MigrationPriority::None),
            2 => Some(MigrationPriority::Low),
            3 => Some(MigrationPriority::Medium),
            4 => Some(MigrationPriority::High),
            _ => None,
        }
    }

    /// One-line dashboard / CLI banner summarizing the counter
    /// state. Format is intentionally compact and stable so
    /// scripts can tail it. Example:
    ///
    /// ```text
    /// advisor: rec=42 data_needed=3 last_priority=high index{fts5=10,tantivy=8,hybrid=20,no_change=4} confidence{h=30,m=8,l=4}
    /// ```
    ///
    /// Output is ASCII-only, single line, no trailing newline.
    #[must_use]
    pub fn summarize(&self) -> String {
        let priority_label = match self.last_priority() {
            None => "unset",
            Some(MigrationPriority::None) => "none",
            Some(MigrationPriority::Low) => "low",
            Some(MigrationPriority::Medium) => "medium",
            Some(MigrationPriority::High) => "high",
        };
        format!(
            "advisor: rec={rec} data_needed={dn} last_priority={pri} \
             index{{fts5={fts5},tantivy={tan},hybrid={hyb},no_change={nc}}} \
             confidence{{h={ch},m={cm},l={cl}}}",
            rec = self.total_recommendations,
            dn = self.total_data_needed,
            pri = priority_label,
            fts5 = self.index_fts5_count,
            tan = self.index_tantivy_count,
            hyb = self.index_hybrid_count,
            nc = self.index_no_change_count,
            ch = self.confidence_high_count,
            cm = self.confidence_medium_count,
            cl = self.confidence_low_count,
        )
    }
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
            other @ AdvisorReport::Recommendation(_) => {
                panic!("expected DataNeeded, got {other:?}")
            }
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
            other @ AdvisorReport::DataNeeded { .. } => {
                panic!("expected Recommendation, got {other:?}")
            }
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
            other @ AdvisorReport::DataNeeded { .. } => {
                panic!("expected Recommendation, got {other:?}")
            }
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
            other @ AdvisorReport::DataNeeded { .. } => {
                panic!("expected Recommendation, got {other:?}")
            }
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
            other @ AdvisorReport::DataNeeded { .. } => {
                panic!("expected Recommendation, got {other:?}")
            }
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
            other @ AdvisorReport::DataNeeded { .. } => {
                panic!("expected Recommendation, got {other:?}")
            }
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
            other @ AdvisorReport::DataNeeded { .. } => {
                panic!("expected Recommendation, got {other:?}")
            }
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

    /// br-ft-1650n.15 wired-pass: from_snapshots threads every
    /// snapshot field into the WorkloadProfile correctly.
    #[test]
    fn from_snapshots_threads_all_fields() {
        let cardinality = StorageDistinctSketchSnapshot {
            estimated_distinct_panes: 42,
            estimated_distinct_sessions: 7,
            estimated_distinct_embedders: 3,
            standard_error: 0.0081,
            memory_bytes: 49_152,
        };
        let profile = WorkloadProfile::from_snapshots(
            WorkloadOpCounts::new(500, 1_000, 250),
            SearchBackendsInUse::fts5_only(),
            &cardinality,
            Some(HotTableSnapshot {
                name: "output_segments".to_string(),
                row_count: 1_234_567,
            }),
            TailLatencySnapshot::new(75_000, 12_000),
            16 * 1024 * 1024,
        );

        assert_eq!(profile.total_writes, 500);
        assert_eq!(profile.total_reads, 1_000);
        assert_eq!(profile.total_searches, 250);
        assert!(profile.fts_enabled);
        assert!(!profile.tantivy_enabled);
        assert_eq!(profile.estimated_distinct_panes, 42);
        assert_eq!(profile.estimated_distinct_sessions, 7);
        assert_eq!(
            profile.hot_table.as_ref().map(|h| h.name.as_str()),
            Some("output_segments")
        );
        assert_eq!(profile.p99_write_latency_us, 75_000);
        assert_eq!(profile.p99_read_latency_us, 12_000);
        assert_eq!(profile.checkpoint_lag_bytes, 16 * 1024 * 1024);
    }

    /// br-ft-1650n.15: SearchBackendsInUse convenience
    /// constructors round-trip to the right boolean pair.
    #[test]
    fn search_backends_constructors() {
        assert_eq!(
            SearchBackendsInUse::fts5_only(),
            SearchBackendsInUse {
                fts5: true,
                tantivy: false
            }
        );
        assert_eq!(
            SearchBackendsInUse::tantivy_only(),
            SearchBackendsInUse {
                fts5: false,
                tantivy: true
            }
        );
        assert_eq!(
            SearchBackendsInUse::both(),
            SearchBackendsInUse {
                fts5: true,
                tantivy: true
            }
        );
        assert_eq!(
            SearchBackendsInUse::neither(),
            SearchBackendsInUse::default()
        );
    }

    /// br-ft-1650n.15: end-to-end smoke — a search-heavy snapshot
    /// with both indexes lights up the Hybrid recommendation.
    /// Mirrors `search_heavy_with_both_indexes_recommends_hybrid`
    /// but constructs the profile via the snapshot feeder rather
    /// than the field-literal path.
    #[test]
    fn from_snapshots_into_classify_search_heavy_hybrid() {
        let cardinality = StorageDistinctSketchSnapshot {
            estimated_distinct_panes: 0,
            estimated_distinct_sessions: 0,
            estimated_distinct_embedders: 0,
            standard_error: 0.0,
            memory_bytes: 0,
        };
        let profile = WorkloadProfile::from_snapshots(
            WorkloadOpCounts::new(100, 100, 1_000),
            SearchBackendsInUse::both(),
            &cardinality,
            None,
            TailLatencySnapshot::default(),
            0,
        );
        match classify(&profile) {
            AdvisorReport::Recommendation(rec) => {
                assert_eq!(rec.index, IndexChoice::Hybrid);
                assert_eq!(rec.backend, BackendChoice::Rusqlite);
            }
            other @ AdvisorReport::DataNeeded { .. } => {
                panic!("expected Recommendation, got {other:?}")
            }
        }
    }

    /// br-ft-1650n.15 live-wiring: build_profile_from_event_bus_metrics
    /// threads the EventBus snapshot's events_published and
    /// events_delivered into the WorkloadProfile's writes/reads
    /// fields (the documented mapping at
    /// `build_profile_from_event_bus_metrics`).
    #[test]
    fn build_profile_from_event_bus_metrics_threads_published_and_delivered() {
        let metrics = MetricsSnapshot {
            events_published: 5_000,
            events_dropped_no_subscribers: 100,
            events_dropped_dedup: 50,
            events_delivered: 4_850,
            active_subscribers: 4,
            subscriber_lag_events: 12,
            bus_lock_poisoned_count: 0,
            delta_dedup_full_count: 0,
        };
        let cardinality = StorageDistinctSketchSnapshot {
            estimated_distinct_panes: 200,
            estimated_distinct_sessions: 25,
            estimated_distinct_embedders: 1,
            standard_error: 0.0081,
            memory_bytes: 49_152,
        };
        let profile = build_profile_from_event_bus_metrics(
            &metrics,
            &cardinality,
            500, // total_searches
            SearchBackendsInUse::fts5_only(),
            None,
            TailLatencySnapshot::default(),
            0,
        );
        assert_eq!(profile.total_writes, 5_000);
        assert_eq!(profile.total_reads, 4_850);
        assert_eq!(profile.total_searches, 500);
        assert!(profile.fts_enabled);
        assert!(!profile.tantivy_enabled);
        assert_eq!(profile.estimated_distinct_panes, 200);
        assert_eq!(profile.estimated_distinct_sessions, 25);
    }

    /// br-ft-1650n.15 live-wiring: advise_from_event_bus_metrics
    /// produces a Recommendation when the EventBus snapshot
    /// crosses MIN_SAMPLE_OPS and a search backend is registered.
    /// End-to-end exercises the build → classify pipeline.
    #[test]
    fn advise_from_event_bus_metrics_produces_recommendation_above_threshold() {
        let metrics = MetricsSnapshot {
            events_published: 10_000,
            events_dropped_no_subscribers: 0,
            events_dropped_dedup: 0,
            events_delivered: 9_950,
            active_subscribers: 3,
            subscriber_lag_events: 0,
            bus_lock_poisoned_count: 0,
            delta_dedup_full_count: 0,
        };
        let cardinality = StorageDistinctSketchSnapshot {
            estimated_distinct_panes: 50,
            estimated_distinct_sessions: 10,
            estimated_distinct_embedders: 1,
            standard_error: 0.0081,
            memory_bytes: 49_152,
        };
        let report = advise_from_event_bus_metrics(
            &metrics,
            &cardinality,
            300, // total_searches
            SearchBackendsInUse::fts5_only(),
            Some(HotTableSnapshot {
                name: "output_segments".to_string(),
                row_count: 2_000_000,
            }),
            TailLatencySnapshot::new(20_000, 5_000),
            8 * 1024 * 1024,
        );
        match report {
            AdvisorReport::Recommendation(rec) => {
                assert_eq!(rec.backend, BackendChoice::Rusqlite);
                assert_eq!(rec.index, IndexChoice::Fts5);
                // total_ops > MIN_SAMPLE_OPS * 10 + FTS5 enabled
                // → Confidence::High per the documented gate.
                assert_eq!(rec.confidence, Confidence::High);
            }
            other @ AdvisorReport::DataNeeded { .. } => {
                panic!("expected Recommendation, got {other:?}")
            }
        }
    }

    /// br-ft-1650n.15 live-wiring: tiny EventBus snapshot below
    /// the sparse-sample gate produces DataNeeded even when other
    /// signals are present.
    #[test]
    fn advise_from_event_bus_metrics_below_threshold_returns_data_needed() {
        let metrics = MetricsSnapshot {
            events_published: 50,
            events_dropped_no_subscribers: 0,
            events_dropped_dedup: 0,
            events_delivered: 50,
            active_subscribers: 1,
            subscriber_lag_events: 0,
            bus_lock_poisoned_count: 0,
            delta_dedup_full_count: 0,
        };
        let cardinality = StorageDistinctSketchSnapshot {
            estimated_distinct_panes: 1,
            estimated_distinct_sessions: 1,
            estimated_distinct_embedders: 0,
            standard_error: 0.0081,
            memory_bytes: 49_152,
        };
        let report = advise_from_event_bus_metrics(
            &metrics,
            &cardinality,
            10,
            SearchBackendsInUse::neither(),
            None,
            TailLatencySnapshot::default(),
            0,
        );
        match report {
            AdvisorReport::DataNeeded { reasons } => {
                assert!(!reasons.is_empty());
                assert!(reasons.iter().any(|r| r.contains("MIN_SAMPLE_OPS")));
            }
            other @ AdvisorReport::Recommendation(_) => {
                panic!("expected DataNeeded, got {other:?}")
            }
        }
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

    fn rec(
        index: IndexChoice,
        priority: MigrationPriority,
        confidence: Confidence,
    ) -> AdvisorReport {
        AdvisorReport::Recommendation(StorageRecommendation {
            backend: BackendChoice::Rusqlite,
            index,
            migration_priority: priority,
            confidence,
            rationale: String::new(),
            proof_commands: Vec::new(),
        })
    }

    /// br-ft-1650n.15 autoscaler-observable: a fresh metrics
    /// instance starts with every counter at zero and no observed
    /// priority.
    #[test]
    fn advisor_metrics_new_starts_zero() {
        let metrics = AdvisorMetrics::new();
        let snap = metrics.snapshot();
        assert_eq!(snap.total_recommendations, 0);
        assert_eq!(snap.total_data_needed, 0);
        assert_eq!(snap.last_priority_level, 0);
        assert_eq!(metrics.last_priority(), None);
        assert!(!metrics.is_critical());
    }

    /// Recording a Recommendation bumps every per-variant counter
    /// for IndexChoice / MigrationPriority / Confidence and the
    /// total rollup.
    #[test]
    fn advisor_metrics_record_recommendation_bumps_per_variant_counters() {
        let metrics = AdvisorMetrics::new();
        metrics.record_report(&rec(
            IndexChoice::Hybrid,
            MigrationPriority::High,
            Confidence::High,
        ));
        let snap = metrics.snapshot();
        assert_eq!(snap.index_hybrid_count, 1);
        assert_eq!(snap.priority_high_count, 1);
        assert_eq!(snap.confidence_high_count, 1);
        assert_eq!(snap.total_recommendations, 1);
        assert_eq!(snap.total_data_needed, 0);
        // High → ladder = 4
        assert_eq!(snap.last_priority_level, 4);
        assert_eq!(metrics.last_priority(), Some(MigrationPriority::High));
        assert!(metrics.is_critical());
    }

    /// Recording a DataNeeded bumps only the data-needed rollup;
    /// last_priority_level is untouched (so an autoscaler doesn't
    /// drop its gate when the sample temporarily becomes too sparse).
    #[test]
    fn advisor_metrics_record_data_needed_does_not_reset_priority() {
        let metrics = AdvisorMetrics::new();
        metrics.record_report(&rec(
            IndexChoice::Fts5,
            MigrationPriority::Medium,
            Confidence::Medium,
        ));
        metrics.record_report(&AdvisorReport::DataNeeded {
            reasons: vec!["sparse".to_string()],
        });
        let snap = metrics.snapshot();
        assert_eq!(snap.total_recommendations, 1);
        assert_eq!(snap.total_data_needed, 1);
        // Medium → ladder = 3, must persist past the DataNeeded.
        assert_eq!(snap.last_priority_level, 3);
        assert_eq!(metrics.last_priority(), Some(MigrationPriority::Medium));
    }

    /// last_priority always reflects the most recent recommendation
    /// across multiple observations (the priority field is a
    /// sliding-window single-slot cache, not an aggregate).
    #[test]
    fn advisor_metrics_last_priority_tracks_most_recent_recommendation() {
        let metrics = AdvisorMetrics::new();
        metrics.record_report(&rec(
            IndexChoice::Fts5,
            MigrationPriority::High,
            Confidence::High,
        ));
        assert_eq!(metrics.last_priority(), Some(MigrationPriority::High));
        metrics.record_report(&rec(
            IndexChoice::Fts5,
            MigrationPriority::None,
            Confidence::High,
        ));
        assert_eq!(metrics.last_priority(), Some(MigrationPriority::None));
        assert!(!metrics.is_critical());
    }

    /// Counters accumulate monotonically across many observations.
    #[test]
    fn advisor_metrics_counters_accumulate() {
        let metrics = AdvisorMetrics::new();
        for _ in 0..3 {
            metrics.record_report(&rec(
                IndexChoice::Tantivy,
                MigrationPriority::Low,
                Confidence::Low,
            ));
        }
        for _ in 0..2 {
            metrics.record_report(&rec(
                IndexChoice::Hybrid,
                MigrationPriority::High,
                Confidence::High,
            ));
        }
        let snap = metrics.snapshot();
        assert_eq!(snap.index_tantivy_count, 3);
        assert_eq!(snap.index_hybrid_count, 2);
        assert_eq!(snap.priority_low_count, 3);
        assert_eq!(snap.priority_high_count, 2);
        assert_eq!(snap.confidence_low_count, 3);
        assert_eq!(snap.confidence_high_count, 2);
        assert_eq!(snap.total_recommendations, 5);
    }

    /// Snapshot serde roundtrips to the same value.
    #[test]
    fn advisor_metrics_snapshot_serde_roundtrip() {
        let metrics = AdvisorMetrics::new();
        metrics.record_report(&rec(
            IndexChoice::NoChange,
            MigrationPriority::Low,
            Confidence::Low,
        ));
        metrics.record_report(&AdvisorReport::DataNeeded {
            reasons: vec!["x".to_string()],
        });
        let snap = metrics.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: AdvisorMetricsSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(snap, back);
    }

    fn metrics_snapshot(events_published: u64, events_delivered: u64) -> MetricsSnapshot {
        MetricsSnapshot {
            events_published,
            events_dropped_no_subscribers: 0,
            events_dropped_dedup: 0,
            events_delivered,
            active_subscribers: 1,
            subscriber_lag_events: 0,
            bus_lock_poisoned_count: 0,
            delta_dedup_full_count: 0,
        }
    }

    /// br-ft-1650n.15: recommend_and_record runs the classifier
    /// AND bumps the counters in one call. Returns the report so
    /// callers can still gate on it.
    #[test]
    fn advisor_metrics_recommend_and_record_returns_and_records() {
        let cardinality = StorageDistinctSketchSnapshot {
            estimated_distinct_panes: 50,
            estimated_distinct_sessions: 10,
            estimated_distinct_embedders: 1,
            standard_error: 0.0081,
            memory_bytes: 49_152,
        };
        let metrics = AdvisorMetrics::new();
        let report = metrics.recommend_and_record(
            &metrics_snapshot(10_000, 9_950),
            &cardinality,
            300,
            SearchBackendsInUse::fts5_only(),
            None,
            TailLatencySnapshot::new(20_000, 5_000),
            8 * 1024 * 1024,
        );
        assert!(matches!(report, AdvisorReport::Recommendation(_)));
        let snap = metrics.snapshot();
        assert_eq!(snap.total_recommendations, 1);
        assert_eq!(snap.total_data_needed, 0);
        // FTS5-only registered → IndexChoice::Fts5 → fts5 counter
        // bumped.
        assert_eq!(snap.index_fts5_count, 1);
    }

    /// br-ft-1650n.15: recommend_and_record with a sparse sample
    /// records DataNeeded and does NOT bump any recommendation
    /// counter.
    #[test]
    fn advisor_metrics_recommend_and_record_sparse_records_data_needed() {
        let cardinality = StorageDistinctSketchSnapshot {
            estimated_distinct_panes: 1,
            estimated_distinct_sessions: 1,
            estimated_distinct_embedders: 0,
            standard_error: 0.0081,
            memory_bytes: 49_152,
        };
        let metrics = AdvisorMetrics::new();
        let report = metrics.recommend_and_record(
            &metrics_snapshot(10, 5),
            &cardinality,
            0,
            SearchBackendsInUse::neither(),
            None,
            TailLatencySnapshot::default(),
            0,
        );
        assert!(matches!(report, AdvisorReport::DataNeeded { .. }));
        let snap = metrics.snapshot();
        assert_eq!(snap.total_recommendations, 0);
        assert_eq!(snap.total_data_needed, 1);
    }

    /// br-ft-1650n.15: AdvisorMetricsSnapshot::summarize emits a
    /// stable single-line ASCII string suitable for dashboard
    /// banners and CLI tail-output.
    #[test]
    fn advisor_metrics_snapshot_summarize_format() {
        let metrics = AdvisorMetrics::new();
        metrics.record_report(&rec(
            IndexChoice::Fts5,
            MigrationPriority::High,
            Confidence::High,
        ));
        metrics.record_report(&rec(
            IndexChoice::Hybrid,
            MigrationPriority::Low,
            Confidence::Medium,
        ));
        metrics.record_report(&AdvisorReport::DataNeeded {
            reasons: vec!["x".to_string()],
        });
        let snap = metrics.snapshot();
        let summary = snap.summarize();

        assert!(!summary.contains('\n'));
        assert!(summary.is_ascii());

        assert!(summary.contains("rec=2"));
        assert!(summary.contains("data_needed=1"));
        // Most recent recommendation was MigrationPriority::Low →
        // "low".
        assert!(summary.contains("last_priority=low"));
        assert!(summary.contains("fts5=1"));
        assert!(summary.contains("hybrid=1"));
        assert!(summary.contains("h=1"));
        assert!(summary.contains("m=1"));
    }

    /// AdvisorMetricsSnapshot::summarize when no recommendation has
    /// been recorded reports `last_priority=unset`.
    #[test]
    fn advisor_metrics_snapshot_summarize_unset_priority() {
        let metrics = AdvisorMetrics::new();
        let summary = metrics.snapshot().summarize();
        assert!(summary.contains("last_priority=unset"));
        assert!(summary.contains("rec=0"));
        assert!(summary.contains("data_needed=0"));
    }

    /// AdvisorMetricsSnapshot::last_priority decodes the ladder
    /// back into a typed enum and round-trips with the runtime's
    /// `AdvisorMetrics::last_priority()`.
    #[test]
    fn advisor_metrics_snapshot_last_priority_decodes_ladder() {
        let metrics = AdvisorMetrics::new();
        assert_eq!(metrics.snapshot().last_priority(), None);

        metrics.record_report(&rec(
            IndexChoice::Fts5,
            MigrationPriority::Medium,
            Confidence::Medium,
        ));
        assert_eq!(
            metrics.snapshot().last_priority(),
            Some(MigrationPriority::Medium)
        );

        metrics.record_report(&rec(
            IndexChoice::Fts5,
            MigrationPriority::High,
            Confidence::High,
        ));
        assert_eq!(
            metrics.snapshot().last_priority(),
            Some(MigrationPriority::High)
        );
    }
}
