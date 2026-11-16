//! Memory-budgeted search prefetch advisor.
//!
//! This module keeps prefetch admission as a cheap, explainable decision
//! surface: callers provide recent query/capture signal plus semantic and
//! fleet memory-budget snapshots, and the advisor returns whether prefetch
//! should be admitted, yielded, or refused.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::fleet_memory_controller::{
    FleetMemoryTier, FleetMemoryTierBudgetRecord, FleetMemoryTierBudgetSnapshot, FleetPressureTier,
};
use crate::storage::SemanticBudgetSnapshot;

/// Search cache surface that a prefetch candidate would warm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPrefetchKind {
    /// Lexical index pages or posting-list data.
    LexicalIndex,
    /// Semantic vector index shards or embedding-neighbor data.
    SemanticIndex,
    /// Hot SQLite row/page cache data for repeated search filters.
    HotRowPageCache,
}

/// Configuration for bounded prefetch admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPrefetchAdvisorConfig {
    /// Minimum recent observations before a candidate is worth warming.
    pub min_recent_query_observations: u32,
    /// Minimum expected tail-latency reduction before warming.
    pub min_estimated_latency_saved_ms: u64,
    /// Highest fleet pressure tier at which prefetch remains admissible.
    pub max_admissible_pressure_tier: FleetPressureTier,
    /// Per-candidate byte cap. Larger candidates are refused even with budget.
    pub max_prefetch_bytes_per_candidate: u64,
}

impl Default for SearchPrefetchAdvisorConfig {
    fn default() -> Self {
        Self {
            min_recent_query_observations: 2,
            min_estimated_latency_saved_ms: 10,
            max_admissible_pressure_tier: FleetPressureTier::Normal,
            max_prefetch_bytes_per_candidate: 256 * 1024 * 1024,
        }
    }
}

/// A prefetch candidate derived from recent query/capture patterns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPrefetchCandidate {
    /// Stable query/cache key fingerprint for diagnostics.
    pub query_fingerprint: String,
    /// Cache surface to warm.
    pub kind: SearchPrefetchKind,
    /// Estimated resident bytes this prefetch would occupy.
    pub estimated_bytes: u64,
    /// Number of recent matching query observations.
    pub recent_query_observations: u32,
    /// Bytes captured since the prior observation window for this query.
    pub recent_capture_bytes: u64,
    /// Expected p99 latency without prefetch.
    pub estimated_tail_latency_without_prefetch_ms: u64,
    /// Expected p99 latency after a successful prefetch hit.
    pub estimated_tail_latency_with_prefetch_ms: u64,
}

impl SearchPrefetchCandidate {
    /// Build a candidate for a repeated query/cache key.
    #[must_use]
    pub fn repeated_query(
        query_fingerprint: impl Into<String>,
        kind: SearchPrefetchKind,
        estimated_bytes: u64,
        recent_query_observations: u32,
        latency_without_prefetch_ms: u64,
        latency_with_prefetch_ms: u64,
    ) -> Self {
        Self {
            query_fingerprint: query_fingerprint.into(),
            kind,
            estimated_bytes,
            recent_query_observations,
            recent_capture_bytes: 0,
            estimated_tail_latency_without_prefetch_ms: latency_without_prefetch_ms,
            estimated_tail_latency_with_prefetch_ms: latency_with_prefetch_ms,
        }
    }

    /// Estimated tail-latency reduction for this candidate.
    #[must_use]
    pub const fn estimated_latency_saved_ms(&self) -> u64 {
        self.estimated_tail_latency_without_prefetch_ms
            .saturating_sub(self.estimated_tail_latency_with_prefetch_ms)
    }
}

/// External snapshots used for prefetch admission.
#[derive(Debug, Clone, Copy)]
pub struct SearchPrefetchContext<'a> {
    /// Current epoch timestamp in milliseconds.
    pub now_ms: i64,
    /// Semantic-lane budget state, if the caller has it.
    pub semantic_budget: Option<&'a SemanticBudgetSnapshot>,
    /// Fleet memory tier-budget snapshot.
    pub tier_budget: &'a FleetMemoryTierBudgetSnapshot,
}

/// Admission outcome for a prefetch candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPrefetchDecisionKind {
    /// Candidate was admitted and bytes were reserved by the advisor.
    Admit,
    /// Candidate yielded because fleet memory pressure is above the threshold.
    YieldPressure,
    /// Candidate yielded because semantic search is in budget backoff.
    YieldSemanticBackoff,
    /// Candidate was refused because the search-cache tier had no room.
    RefuseBudget,
    /// Candidate exceeded the per-candidate byte cap.
    RefuseCandidateTooLarge,
    /// Candidate had insufficient repeated-query or capture signal.
    SkipLowSignal,
    /// Candidate carried no byte estimate.
    SkipZeroBytes,
}

/// Operator-facing explanation for one prefetch decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPrefetchDecision {
    /// Admission/refusal class.
    pub kind: SearchPrefetchDecisionKind,
    /// Candidate query/cache key fingerprint.
    pub query_fingerprint: String,
    /// Search cache surface considered.
    pub prefetch_kind: SearchPrefetchKind,
    /// Estimated bytes for this candidate.
    pub estimated_bytes: u64,
    /// Remaining search-cache tier budget after advisor reservations.
    pub remaining_search_cache_budget_bytes: u64,
    /// Fleet memory pressure seen by the advisor.
    pub pressure_tier: FleetPressureTier,
    /// Expected p99 latency improvement if the prefetch hits.
    pub estimated_latency_saved_ms: u64,
    /// Human-readable reason suitable for telemetry surfaces.
    pub reason: String,
}

impl SearchPrefetchDecision {
    #[must_use]
    fn new(
        kind: SearchPrefetchDecisionKind,
        candidate: &SearchPrefetchCandidate,
        remaining_search_cache_budget_bytes: u64,
        pressure_tier: FleetPressureTier,
        reason: &'static str,
    ) -> Self {
        Self {
            kind,
            query_fingerprint: candidate.query_fingerprint.clone(),
            prefetch_kind: candidate.kind,
            estimated_bytes: candidate.estimated_bytes,
            remaining_search_cache_budget_bytes,
            pressure_tier,
            estimated_latency_saved_ms: candidate.estimated_latency_saved_ms(),
            reason: reason.to_string(),
        }
    }
}

/// Per-entry diagnostics for currently admitted prefetch state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPrefetchEntryDiagnostics {
    /// Candidate query/cache key fingerprint.
    pub query_fingerprint: String,
    /// Search cache surface warmed by this entry.
    pub kind: SearchPrefetchKind,
    /// Resident bytes reserved for this prefetch.
    pub bytes: u64,
    /// Hits observed against this prefetched entry.
    pub hits: u64,
    /// Misses observed against this prefetched entry.
    pub misses: u64,
}

/// Aggregate operator diagnostics for search prefetch behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPrefetchTelemetry {
    /// Candidates considered by the advisor.
    pub considered_candidates: u64,
    /// Candidates admitted for prefetch.
    pub admitted_prefetches: u64,
    /// Candidates skipped for insufficient signal.
    pub skipped_low_signal: u64,
    /// Candidates yielded due to fleet memory pressure.
    pub yielded_pressure: u64,
    /// Candidates yielded due to semantic budget backoff.
    pub yielded_semantic_backoff: u64,
    /// Candidates refused by search-cache budget.
    pub refused_budget: u64,
    /// Candidates refused by per-candidate byte cap.
    pub refused_candidate_too_large: u64,
    /// Candidates skipped because they had no byte estimate.
    pub skipped_zero_bytes: u64,
    /// Prefetch hits observed by callers.
    pub prefetch_hits: u64,
    /// Prefetch misses observed by callers.
    pub prefetch_misses: u64,
    /// Prefetched entries evicted by callers or the advisor.
    pub prefetch_evictions: u64,
    /// Bytes currently reserved for admitted prefetches.
    pub active_prefetch_bytes: u64,
    /// Cumulative bytes admitted for prefetch.
    pub admitted_prefetch_bytes: u64,
    /// Cumulative bytes refused by budget/candidate caps.
    pub refused_prefetch_bytes: u64,
    /// Cumulative bytes evicted from prefetched entries.
    pub evicted_prefetch_bytes: u64,
    /// Most recent decision returned by the advisor.
    pub last_decision: Option<SearchPrefetchDecision>,
    /// Currently admitted entries, sorted by fingerprint.
    pub entries: Vec<SearchPrefetchEntryDiagnostics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchPrefetchEntry {
    query_fingerprint: String,
    kind: SearchPrefetchKind,
    bytes: u64,
    hits: u64,
    misses: u64,
}

impl SearchPrefetchEntry {
    #[must_use]
    fn diagnostics(&self) -> SearchPrefetchEntryDiagnostics {
        SearchPrefetchEntryDiagnostics {
            query_fingerprint: self.query_fingerprint.clone(),
            kind: self.kind,
            bytes: self.bytes,
            hits: self.hits,
            misses: self.misses,
        }
    }
}

/// Stateful advisor that reserves budget for admitted search prefetches.
#[derive(Debug, Clone)]
pub struct SearchPrefetchAdvisor {
    config: SearchPrefetchAdvisorConfig,
    telemetry: SearchPrefetchTelemetry,
    entries: BTreeMap<String, SearchPrefetchEntry>,
}

impl Default for SearchPrefetchAdvisor {
    fn default() -> Self {
        Self::new(SearchPrefetchAdvisorConfig::default())
    }
}

impl SearchPrefetchAdvisor {
    /// Construct an advisor with explicit configuration.
    #[must_use]
    pub fn new(config: SearchPrefetchAdvisorConfig) -> Self {
        Self {
            config,
            telemetry: SearchPrefetchTelemetry::default(),
            entries: BTreeMap::new(),
        }
    }

    /// Return the advisor configuration.
    #[must_use]
    pub const fn config(&self) -> SearchPrefetchAdvisorConfig {
        self.config
    }

    /// Return operator diagnostics, including current entry-level counters.
    #[must_use]
    pub fn telemetry(&self) -> SearchPrefetchTelemetry {
        let mut telemetry = self.telemetry.clone();
        telemetry.entries = self
            .entries
            .values()
            .map(SearchPrefetchEntry::diagnostics)
            .collect();
        telemetry
    }

    /// Evaluate one candidate and reserve bytes when it is admitted.
    #[must_use]
    pub fn evaluate_candidate(
        &mut self,
        candidate: SearchPrefetchCandidate,
        context: SearchPrefetchContext<'_>,
    ) -> SearchPrefetchDecision {
        self.telemetry.considered_candidates =
            self.telemetry.considered_candidates.saturating_add(1);

        let remaining_budget = self.remaining_search_cache_budget(context.tier_budget);
        let pressure_tier = context.tier_budget.pressure_tier();
        let decision = self.evaluate_candidate_inner(&candidate, &context, remaining_budget);

        match decision.kind {
            SearchPrefetchDecisionKind::Admit => self.record_admitted_candidate(candidate),
            SearchPrefetchDecisionKind::YieldPressure => {
                self.telemetry.yielded_pressure = self.telemetry.yielded_pressure.saturating_add(1);
            }
            SearchPrefetchDecisionKind::YieldSemanticBackoff => {
                self.telemetry.yielded_semantic_backoff =
                    self.telemetry.yielded_semantic_backoff.saturating_add(1);
            }
            SearchPrefetchDecisionKind::RefuseBudget => {
                self.telemetry.refused_budget = self.telemetry.refused_budget.saturating_add(1);
                self.telemetry.refused_prefetch_bytes = self
                    .telemetry
                    .refused_prefetch_bytes
                    .saturating_add(decision.estimated_bytes);
            }
            SearchPrefetchDecisionKind::RefuseCandidateTooLarge => {
                self.telemetry.refused_candidate_too_large =
                    self.telemetry.refused_candidate_too_large.saturating_add(1);
                self.telemetry.refused_prefetch_bytes = self
                    .telemetry
                    .refused_prefetch_bytes
                    .saturating_add(decision.estimated_bytes);
            }
            SearchPrefetchDecisionKind::SkipLowSignal => {
                self.telemetry.skipped_low_signal =
                    self.telemetry.skipped_low_signal.saturating_add(1);
            }
            SearchPrefetchDecisionKind::SkipZeroBytes => {
                self.telemetry.skipped_zero_bytes =
                    self.telemetry.skipped_zero_bytes.saturating_add(1);
            }
        }

        self.telemetry.last_decision = Some(decision.clone());
        debug_assert_eq!(pressure_tier, decision.pressure_tier);
        decision
    }

    /// Record a hit against an admitted prefetch entry.
    ///
    /// Returns `true` when the fingerprint was actively prefetched.
    pub fn record_prefetch_hit(&mut self, query_fingerprint: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(query_fingerprint) {
            entry.hits = entry.hits.saturating_add(1);
            self.telemetry.prefetch_hits = self.telemetry.prefetch_hits.saturating_add(1);
            return true;
        }
        self.record_prefetch_miss(query_fingerprint);
        false
    }

    /// Record a miss against an admitted or expected prefetch entry.
    pub fn record_prefetch_miss(&mut self, query_fingerprint: &str) {
        if let Some(entry) = self.entries.get_mut(query_fingerprint) {
            entry.misses = entry.misses.saturating_add(1);
        }
        self.telemetry.prefetch_misses = self.telemetry.prefetch_misses.saturating_add(1);
    }

    /// Evict one prefetched entry and return the released byte count.
    pub fn record_prefetch_eviction(&mut self, query_fingerprint: &str) -> u64 {
        if let Some(entry) = self.entries.remove(query_fingerprint) {
            self.telemetry.prefetch_evictions = self.telemetry.prefetch_evictions.saturating_add(1);
            self.telemetry.evicted_prefetch_bytes = self
                .telemetry
                .evicted_prefetch_bytes
                .saturating_add(entry.bytes);
            self.telemetry.active_prefetch_bytes = self
                .telemetry
                .active_prefetch_bytes
                .saturating_sub(entry.bytes);
            return entry.bytes;
        }
        0
    }

    #[must_use]
    fn evaluate_candidate_inner(
        &self,
        candidate: &SearchPrefetchCandidate,
        context: &SearchPrefetchContext<'_>,
        remaining_budget: u64,
    ) -> SearchPrefetchDecision {
        let pressure_tier = context.tier_budget.pressure_tier();

        if candidate.estimated_bytes == 0 {
            return SearchPrefetchDecision::new(
                SearchPrefetchDecisionKind::SkipZeroBytes,
                candidate,
                remaining_budget,
                pressure_tier,
                "prefetch_candidate_zero_bytes",
            );
        }

        if pressure_tier > self.config.max_admissible_pressure_tier {
            return SearchPrefetchDecision::new(
                SearchPrefetchDecisionKind::YieldPressure,
                candidate,
                remaining_budget,
                pressure_tier,
                "prefetch_yielded_to_fleet_memory_pressure",
            );
        }

        if semantic_backoff_active(context.semantic_budget, context.now_ms) {
            return SearchPrefetchDecision::new(
                SearchPrefetchDecisionKind::YieldSemanticBackoff,
                candidate,
                remaining_budget,
                pressure_tier,
                "prefetch_yielded_to_semantic_budget_backoff",
            );
        }

        if candidate.estimated_bytes > self.config.max_prefetch_bytes_per_candidate {
            return SearchPrefetchDecision::new(
                SearchPrefetchDecisionKind::RefuseCandidateTooLarge,
                candidate,
                remaining_budget,
                pressure_tier,
                "prefetch_candidate_exceeds_per_candidate_cap",
            );
        }

        if candidate.recent_query_observations < self.config.min_recent_query_observations
            && candidate.recent_capture_bytes == 0
        {
            return SearchPrefetchDecision::new(
                SearchPrefetchDecisionKind::SkipLowSignal,
                candidate,
                remaining_budget,
                pressure_tier,
                "prefetch_candidate_has_insufficient_recent_signal",
            );
        }

        if candidate.estimated_latency_saved_ms() < self.config.min_estimated_latency_saved_ms {
            return SearchPrefetchDecision::new(
                SearchPrefetchDecisionKind::SkipLowSignal,
                candidate,
                remaining_budget,
                pressure_tier,
                "prefetch_candidate_latency_savings_below_threshold",
            );
        }

        if candidate.estimated_bytes > remaining_budget {
            return SearchPrefetchDecision::new(
                SearchPrefetchDecisionKind::RefuseBudget,
                candidate,
                remaining_budget,
                pressure_tier,
                "prefetch_candidate_exceeds_search_cache_budget",
            );
        }

        SearchPrefetchDecision::new(
            SearchPrefetchDecisionKind::Admit,
            candidate,
            remaining_budget.saturating_sub(candidate.estimated_bytes),
            pressure_tier,
            "prefetch_candidate_admitted",
        )
    }

    fn record_admitted_candidate(&mut self, candidate: SearchPrefetchCandidate) {
        self.telemetry.admitted_prefetches = self.telemetry.admitted_prefetches.saturating_add(1);
        self.telemetry.admitted_prefetch_bytes = self
            .telemetry
            .admitted_prefetch_bytes
            .saturating_add(candidate.estimated_bytes);

        if let Some(previous) = self.entries.insert(
            candidate.query_fingerprint.clone(),
            SearchPrefetchEntry {
                query_fingerprint: candidate.query_fingerprint,
                kind: candidate.kind,
                bytes: candidate.estimated_bytes,
                hits: 0,
                misses: 0,
            },
        ) {
            self.telemetry.active_prefetch_bytes = self
                .telemetry
                .active_prefetch_bytes
                .saturating_sub(previous.bytes);
        }

        self.telemetry.active_prefetch_bytes = self
            .telemetry
            .active_prefetch_bytes
            .saturating_add(candidate.estimated_bytes);
    }

    #[must_use]
    fn remaining_search_cache_budget(&self, snapshot: &FleetMemoryTierBudgetSnapshot) -> u64 {
        search_cache_tier(snapshot)
            .map(FleetMemoryTierBudgetRecord::remaining_budget_bytes)
            .unwrap_or(0)
            .saturating_sub(self.telemetry.active_prefetch_bytes)
    }
}

#[must_use]
fn search_cache_tier(
    snapshot: &FleetMemoryTierBudgetSnapshot,
) -> Option<&FleetMemoryTierBudgetRecord> {
    snapshot
        .tiers
        .iter()
        .find(|record| record.tier == FleetMemoryTier::SearchIndexCache)
}

#[must_use]
fn semantic_backoff_active(snapshot: Option<&SemanticBudgetSnapshot>, now_ms: i64) -> bool {
    snapshot
        .and_then(|semantic_snapshot| semantic_snapshot.backoff_until_ms)
        .is_some_and(|backoff_until_ms| now_ms < backoff_until_ms)
}

#[cfg(test)]
mod tests {
    use super::{
        SearchPrefetchAdvisor, SearchPrefetchCandidate, SearchPrefetchContext,
        SearchPrefetchDecisionKind, SearchPrefetchKind,
    };
    use crate::fleet_memory_controller::{
        FleetMemoryTier, FleetMemoryTierBudgetRecord, FleetMemoryTierBudgetSnapshot,
    };
    use crate::storage::{SemanticBudgetConfig, SemanticBudgetMetrics, SemanticBudgetSnapshot};

    fn tier_budget(
        search_budget_bytes: u64,
        search_actual_bytes: u64,
    ) -> FleetMemoryTierBudgetSnapshot {
        FleetMemoryTierBudgetSnapshot::from_tiers([
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::HotResident, 8_000, 4_000),
            FleetMemoryTierBudgetRecord::new(
                FleetMemoryTier::SearchIndexCache,
                search_budget_bytes,
                search_actual_bytes,
            ),
        ])
    }

    fn pressured_tier_budget() -> FleetMemoryTierBudgetSnapshot {
        FleetMemoryTierBudgetSnapshot::from_tiers([
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::HotResident, 1_000, 1_600),
            FleetMemoryTierBudgetRecord::new(FleetMemoryTier::SearchIndexCache, 8_000, 2_000),
        ])
    }

    fn semantic_snapshot(backoff_until_ms: Option<i64>) -> SemanticBudgetSnapshot {
        SemanticBudgetSnapshot {
            config: SemanticBudgetConfig::default(),
            metrics: SemanticBudgetMetrics::default(),
            ewma_semantic_latency_ms: 0.0,
            backoff_until_ms,
            cache_entries: 0,
        }
    }

    fn candidate(name: &str, estimated_bytes: u64) -> SearchPrefetchCandidate {
        SearchPrefetchCandidate::repeated_query(
            name,
            SearchPrefetchKind::SemanticIndex,
            estimated_bytes,
            4,
            120,
            35,
        )
    }

    fn context<'a>(
        tier_budget: &'a FleetMemoryTierBudgetSnapshot,
        semantic_budget: Option<&'a SemanticBudgetSnapshot>,
    ) -> SearchPrefetchContext<'a> {
        SearchPrefetchContext {
            now_ms: 1_000,
            semantic_budget,
            tier_budget,
        }
    }

    #[test]
    fn cache_budget_accounting_admits_then_refuses_prefetch_ft_vvec2() {
        let tiers = tier_budget(1_000, 400);
        let semantic = semantic_snapshot(None);
        let mut advisor = SearchPrefetchAdvisor::default();

        let first = advisor.evaluate_candidate(
            candidate("repeat:error", 300),
            context(&tiers, Some(&semantic)),
        );
        assert_eq!(first.kind, SearchPrefetchDecisionKind::Admit);
        assert_eq!(first.remaining_search_cache_budget_bytes, 300);

        let second = advisor.evaluate_candidate(
            candidate("repeat:warning", 350),
            context(&tiers, Some(&semantic)),
        );
        assert_eq!(second.kind, SearchPrefetchDecisionKind::RefuseBudget);
        assert_eq!(second.remaining_search_cache_budget_bytes, 300);

        let telemetry = advisor.telemetry();
        assert_eq!(telemetry.admitted_prefetches, 1);
        assert_eq!(telemetry.refused_budget, 1);
        assert_eq!(telemetry.active_prefetch_bytes, 300);
        assert_eq!(telemetry.refused_prefetch_bytes, 350);
    }

    #[test]
    fn pressure_and_semantic_backoff_yield_before_budget_checks_ft_vvec2() {
        let pressured = pressured_tier_budget();
        let mut advisor = SearchPrefetchAdvisor::default();
        let pressure_decision =
            advisor.evaluate_candidate(candidate("pressure", 50), context(&pressured, None));
        assert_eq!(
            pressure_decision.kind,
            SearchPrefetchDecisionKind::YieldPressure
        );

        let tiers = tier_budget(1_000, 100);
        let semantic = semantic_snapshot(Some(5_000));
        let semantic_decision = advisor.evaluate_candidate(
            candidate("semantic", 2_000),
            context(&tiers, Some(&semantic)),
        );
        assert_eq!(
            semantic_decision.kind,
            SearchPrefetchDecisionKind::YieldSemanticBackoff
        );

        let telemetry = advisor.telemetry();
        assert_eq!(telemetry.yielded_pressure, 1);
        assert_eq!(telemetry.yielded_semantic_backoff, 1);
        assert_eq!(telemetry.refused_budget, 0);
    }

    #[test]
    fn operator_diagnostics_track_hits_misses_evictions_and_refusals_ft_vvec2() {
        let tiers = tier_budget(1_000, 100);
        let mut advisor = SearchPrefetchAdvisor::default();
        let admitted =
            advisor.evaluate_candidate(candidate("repeat:tail", 250), context(&tiers, None));
        assert_eq!(admitted.kind, SearchPrefetchDecisionKind::Admit);

        assert!(advisor.record_prefetch_hit("repeat:tail"));
        advisor.record_prefetch_miss("repeat:tail");
        assert!(!advisor.record_prefetch_hit("repeat:unknown"));
        let evicted = advisor.record_prefetch_eviction("repeat:tail");
        assert_eq!(evicted, 250);

        let refused =
            advisor.evaluate_candidate(candidate("repeat:too-large", 950), context(&tiers, None));
        assert_eq!(refused.kind, SearchPrefetchDecisionKind::RefuseBudget);

        let telemetry = advisor.telemetry();
        assert_eq!(telemetry.prefetch_hits, 1);
        assert_eq!(telemetry.prefetch_misses, 2);
        assert_eq!(telemetry.prefetch_evictions, 1);
        assert_eq!(telemetry.evicted_prefetch_bytes, 250);
        assert_eq!(telemetry.refused_budget, 1);
        assert_eq!(telemetry.active_prefetch_bytes, 0);
    }

    #[test]
    fn replay_repeated_search_workload_lowers_tail_latency_without_exceeding_budget_ft_vvec2() {
        let tiers = tier_budget(2_048, 256);
        let mut advisor = SearchPrefetchAdvisor::default();
        let admitted =
            advisor.evaluate_candidate(candidate("repeat:agent-state", 512), context(&tiers, None));
        assert_eq!(admitted.kind, SearchPrefetchDecisionKind::Admit);

        let baseline_tail_latencies = [120_u64, 118, 121, 122, 119, 123, 120, 121];
        let mut prefetch_tail_latencies = Vec::with_capacity(baseline_tail_latencies.len());
        for baseline in baseline_tail_latencies {
            if advisor.record_prefetch_hit("repeat:agent-state") {
                prefetch_tail_latencies.push(35);
            } else {
                prefetch_tail_latencies.push(baseline);
            }
        }

        let baseline_tail = tail_latency_ms(&baseline_tail_latencies);
        let prefetch_tail = tail_latency_ms(&prefetch_tail_latencies);
        assert!(prefetch_tail < baseline_tail);

        let telemetry = advisor.telemetry();
        assert_eq!(telemetry.prefetch_hits, 8);
        assert!(telemetry.active_prefetch_bytes <= 512);
        assert!(telemetry.active_prefetch_bytes <= tiers.totals.resident_budget_bytes);
    }

    fn tail_latency_ms(samples: &[u64]) -> u64 {
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let last_index = sorted.len().saturating_sub(1);
        sorted[last_index]
    }
}
