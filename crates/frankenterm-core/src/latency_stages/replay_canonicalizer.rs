use super::{InvariantDomain, TraceAction, TraceStep};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// Trace format version for backward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TraceFormatVersion {
    /// Legacy unordered trace (v1).
    V1,
    /// Canonical ordered trace with sequence numbers (v2).
    V2,
}

impl fmt::Display for TraceFormatVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V1 => write!(f, "v1"),
            Self::V2 => write!(f, "v2"),
        }
    }
}

/// Ordering mode for canonical trace normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CanonicalOrdering {
    /// Order by timestamp only — breaks ties by sequence number.
    Temporal,
    /// Order by (domain, stage, timestamp) — groups related actions.
    DomainGrouped,
    /// Order by causal dependency (sequence number).
    Causal,
}

impl fmt::Display for CanonicalOrdering {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Temporal => write!(f, "temporal"),
            Self::DomainGrouped => write!(f, "domain-grouped"),
            Self::Causal => write!(f, "causal"),
        }
    }
}

/// A single entry in a deterministic trace v2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceEntry {
    /// Monotonic sequence number assigned at capture time.
    pub seq: u64,
    /// Timestamp in microseconds.
    pub timestamp_us: u64,
    /// The action that occurred.
    pub action: TraceAction,
    /// Domain this entry belongs to (for grouping).
    pub domain: InvariantDomain,
    /// Optional causal predecessor sequence number.
    pub causal_parent: Option<u64>,
    /// Fingerprint of the action for dedup / comparison.
    pub fingerprint: u64,
}

impl TraceEntry {
    /// Compute a deterministic fingerprint of an action.
    pub fn compute_fingerprint(action: &TraceAction, domain: InvariantDomain) -> u64 {
        // FNV-1a of the debug representation for stable hashing.
        let repr = format!("{action:?}|{domain}");
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in repr.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
}

impl fmt::Display for TraceEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] @{}μs {}", self.seq, self.timestamp_us, self.action)
    }
}

/// A complete deterministic trace with version metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeterministicTrace {
    /// Format version.
    pub version: TraceFormatVersion,
    /// Unique trace ID (typically a hash of seed + config).
    pub trace_id: String,
    /// Seed used for deterministic replay (0 = unseeded).
    pub seed: u64,
    /// The ordered list of trace entries.
    pub entries: Vec<TraceEntry>,
    /// Timestamp of trace creation (epoch μs).
    pub created_at_us: u64,
    /// Total wall-clock duration of the captured run (μs).
    pub duration_us: u64,
}

impl DeterministicTrace {
    /// Create a new empty v2 trace.
    pub fn new_v2(trace_id: String, seed: u64, created_at_us: u64) -> Self {
        Self {
            version: TraceFormatVersion::V2,
            trace_id,
            seed,
            entries: Vec::new(),
            created_at_us,
            duration_us: 0,
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the trace has entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Append an entry, auto-assigning the next sequence number.
    pub fn push(
        &mut self,
        action: TraceAction,
        domain: InvariantDomain,
        timestamp_us: u64,
        causal_parent: Option<u64>,
    ) {
        let seq = self.entries.len() as u64;
        let fingerprint = TraceEntry::compute_fingerprint(&action, domain);
        self.entries.push(TraceEntry {
            seq,
            timestamp_us,
            action,
            domain,
            causal_parent,
            fingerprint,
        });
        if timestamp_us > self.created_at_us {
            self.duration_us = timestamp_us - self.created_at_us;
        }
    }

    /// Compute a digest of the entire trace for quick equality checks.
    pub fn digest(&self) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        for entry in &self.entries {
            hash ^= entry.fingerprint;
            hash = hash.wrapping_mul(0x100000001b3);
            hash ^= entry.timestamp_us;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
}

impl fmt::Display for DeterministicTrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Trace[{}, id={}, seed={}, entries={}, duration={}μs]",
            self.version,
            self.trace_id,
            self.seed,
            self.entries.len(),
            self.duration_us
        )
    }
}

/// Result of comparing two traces for replay isomorphism.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplayComparisonResult {
    /// Traces are identical (same ordering and content).
    Identical,
    /// Traces are isomorphic (same content, different ordering).
    Isomorphic {
        /// Number of entries that differ in position.
        reordered_count: usize,
    },
    /// Traces differ in content.
    Divergent {
        /// Index of the first divergent entry in the canonical form.
        first_divergence_idx: usize,
        /// Description of the mismatch.
        description: String,
    },
}

impl fmt::Display for ReplayComparisonResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identical => write!(f, "identical"),
            Self::Isomorphic { reordered_count } => {
                write!(f, "isomorphic ({reordered_count} reordered)")
            }
            Self::Divergent {
                first_divergence_idx,
                description,
            } => {
                write!(f, "divergent at [{first_divergence_idx}]: {description}")
            }
        }
    }
}

/// Mismatch diagnostic for trace comparison debugging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceMismatch {
    /// Position in the canonical trace.
    pub canonical_idx: usize,
    /// Expected action fingerprint.
    pub expected_fingerprint: u64,
    /// Actual action fingerprint (None if entry missing).
    pub actual_fingerprint: Option<u64>,
    /// Human-readable explanation.
    pub explanation: String,
}

/// Configuration for the replay canonicalizer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalizerConfig {
    /// Ordering mode for canonical form.
    pub ordering: CanonicalOrdering,
    /// Whether to strip timestamps during canonicalization (for order-only comparison).
    pub strip_timestamps: bool,
    /// Whether to collapse duplicate consecutive actions.
    pub dedup_consecutive: bool,
    /// Maximum entries to process (0 = unlimited).
    pub max_entries: usize,
}

impl Default for CanonicalizerConfig {
    fn default() -> Self {
        Self {
            ordering: CanonicalOrdering::Causal,
            strip_timestamps: false,
            dedup_consecutive: false,
            max_entries: 0,
        }
    }
}

/// Snapshot of canonicalizer state for telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalizerSnapshot {
    /// Total traces canonicalized.
    pub traces_processed: u64,
    /// Total entries processed across all traces.
    pub entries_processed: u64,
    /// Total entries deduped.
    pub entries_deduped: u64,
    /// Total comparisons made.
    pub comparisons_made: u64,
    /// Configuration in use.
    pub config: CanonicalizerConfig,
}

/// Degradation state for the canonicalizer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CanonicalizerDegradation {
    /// Operating normally.
    Healthy,
    /// High dedup ratio suggests repetitive traces.
    HighDedupRatio { ratio: f64 },
    /// Processing many large traces.
    HighVolume { entries_processed: u64 },
}

impl fmt::Display for CanonicalizerDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::HighDedupRatio { ratio } => write!(f, "high-dedup({ratio:.2})"),
            Self::HighVolume { entries_processed } => write!(f, "high-volume({entries_processed})"),
        }
    }
}

/// Log entry for canonicalizer operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalizerLogEntry {
    /// Timestamp of the log event.
    pub timestamp_us: u64,
    /// Trace ID that was processed.
    pub trace_id: String,
    /// Number of entries in the input.
    pub input_entries: usize,
    /// Number of entries after canonicalization.
    pub output_entries: usize,
    /// Duration of canonicalization (μs).
    pub duration_us: u64,
}

/// The replay canonicalizer: normalizes traces into canonical form and
/// compares them for replay determinism / isomorphism.
pub struct ReplayCanonicalizer {
    config: CanonicalizerConfig,
    traces_processed: u64,
    entries_processed: u64,
    entries_deduped: u64,
    comparisons_made: u64,
}

impl ReplayCanonicalizer {
    /// Create a new canonicalizer with the given config.
    pub fn new(config: CanonicalizerConfig) -> Self {
        Self {
            config,
            traces_processed: 0,
            entries_processed: 0,
            entries_deduped: 0,
            comparisons_made: 0,
        }
    }

    /// Canonicalize a trace into the configured ordering.
    pub fn canonicalize(&mut self, trace: &DeterministicTrace) -> DeterministicTrace {
        self.traces_processed += 1;
        let mut entries = trace.entries.clone();
        let input_len = entries.len();

        // Apply max_entries limit.
        if self.config.max_entries > 0 && entries.len() > self.config.max_entries {
            entries.truncate(self.config.max_entries);
        }

        // Sort by the configured ordering.
        match self.config.ordering {
            CanonicalOrdering::Temporal => {
                entries.sort_by(|a, b| a.timestamp_us.cmp(&b.timestamp_us).then(a.seq.cmp(&b.seq)));
            }
            CanonicalOrdering::DomainGrouped => {
                entries.sort_by(|a, b| {
                    let da = domain_sort_key(a.domain);
                    let db = domain_sort_key(b.domain);
                    da.cmp(&db)
                        .then(a.timestamp_us.cmp(&b.timestamp_us))
                        .then(a.seq.cmp(&b.seq))
                });
            }
            CanonicalOrdering::Causal => {
                entries.sort_by_key(|a| a.seq);
            }
        }

        // Optionally strip timestamps.
        if self.config.strip_timestamps {
            for entry in &mut entries {
                entry.timestamp_us = 0;
            }
        }

        // Optionally dedup consecutive identical actions.
        if self.config.dedup_consecutive && entries.len() > 1 {
            let before = entries.len();
            entries.dedup_by(|a, b| a.fingerprint == b.fingerprint);
            self.entries_deduped += (before - entries.len()) as u64;
        }

        self.entries_processed += input_len as u64;

        // Reassign sequence numbers in canonical order.
        for (i, entry) in entries.iter_mut().enumerate() {
            entry.seq = i as u64;
        }

        DeterministicTrace {
            version: TraceFormatVersion::V2,
            trace_id: trace.trace_id.clone(),
            seed: trace.seed,
            entries,
            created_at_us: trace.created_at_us,
            duration_us: trace.duration_us,
        }
    }

    /// Compare two traces for replay isomorphism.
    pub fn compare(
        &mut self,
        a: &DeterministicTrace,
        b: &DeterministicTrace,
    ) -> ReplayComparisonResult {
        self.comparisons_made += 1;

        let ca = self.canonicalize(a);
        let cb = self.canonicalize(b);

        // Quick length check.
        if ca.entries.len() != cb.entries.len() {
            return ReplayComparisonResult::Divergent {
                first_divergence_idx: ca.entries.len().min(cb.entries.len()),
                description: format!(
                    "length mismatch: {} vs {}",
                    ca.entries.len(),
                    cb.entries.len()
                ),
            };
        }

        // Check for identical canonical forms.
        let mut identical = true;
        let mut first_diff = None;
        for (i, (ea, eb)) in ca.entries.iter().zip(cb.entries.iter()).enumerate() {
            if ea.fingerprint != eb.fingerprint {
                identical = false;
                first_diff = Some(i);
                break;
            }
            if ea.timestamp_us != eb.timestamp_us {
                identical = false;
            }
        }

        if identical && first_diff.is_none() {
            // Check if the original ordering was the same.
            let orig_same = a
                .entries
                .iter()
                .zip(b.entries.iter())
                .all(|(ea, eb)| ea.fingerprint == eb.fingerprint && ea.seq == eb.seq);
            if orig_same {
                return ReplayComparisonResult::Identical;
            }
            // Same content after canonicalization but different original order.
            let reordered = a
                .entries
                .iter()
                .zip(b.entries.iter())
                .filter(|(ea, eb)| ea.seq != eb.seq || ea.fingerprint != eb.fingerprint)
                .count();
            return ReplayComparisonResult::Isomorphic {
                reordered_count: reordered,
            };
        }

        if let Some(idx) = first_diff {
            return ReplayComparisonResult::Divergent {
                first_divergence_idx: idx,
                description: format!(
                    "fingerprint mismatch: {} vs {}",
                    ca.entries[idx].fingerprint, cb.entries[idx].fingerprint
                ),
            };
        }

        // Timestamps differ but content is isomorphic.
        let reordered = a
            .entries
            .iter()
            .zip(b.entries.iter())
            .filter(|(ea, eb)| ea.timestamp_us != eb.timestamp_us)
            .count();
        ReplayComparisonResult::Isomorphic {
            reordered_count: reordered,
        }
    }

    /// Generate mismatch diagnostics between two traces.
    pub fn diagnose_mismatches(
        &self,
        a: &DeterministicTrace,
        b: &DeterministicTrace,
    ) -> Vec<TraceMismatch> {
        let mut mismatches = Vec::new();
        let max_len = a.entries.len().max(b.entries.len());
        for i in 0..max_len {
            match (a.entries.get(i), b.entries.get(i)) {
                (Some(ea), Some(eb)) if ea.fingerprint != eb.fingerprint => {
                    mismatches.push(TraceMismatch {
                        canonical_idx: i,
                        expected_fingerprint: ea.fingerprint,
                        actual_fingerprint: Some(eb.fingerprint),
                        explanation: format!("expected {} but got {}", ea.action, eb.action),
                    });
                }
                (Some(ea), None) => {
                    mismatches.push(TraceMismatch {
                        canonical_idx: i,
                        expected_fingerprint: ea.fingerprint,
                        actual_fingerprint: None,
                        explanation: format!("missing entry: expected {}", ea.action),
                    });
                }
                (None, Some(eb)) => {
                    mismatches.push(TraceMismatch {
                        canonical_idx: i,
                        expected_fingerprint: 0,
                        actual_fingerprint: Some(eb.fingerprint),
                        explanation: format!("extra entry: {}", eb.action),
                    });
                }
                _ => {}
            }
        }
        mismatches
    }

    /// Upgrade a v1 trace (from ModelChecker) to v2 format.
    pub fn upgrade_trace(
        &mut self,
        steps: &[TraceStep],
        trace_id: String,
        seed: u64,
    ) -> DeterministicTrace {
        let mut trace = DeterministicTrace::new_v2(trace_id, seed, 0);
        for step in steps {
            let domain = action_domain(&step.action);
            trace.push(step.action.clone(), domain, step.timestamp_us, None);
        }
        trace
    }

    /// Get a snapshot of canonicalizer state.
    pub fn snapshot(&self) -> CanonicalizerSnapshot {
        CanonicalizerSnapshot {
            traces_processed: self.traces_processed,
            entries_processed: self.entries_processed,
            entries_deduped: self.entries_deduped,
            comparisons_made: self.comparisons_made,
            config: self.config.clone(),
        }
    }

    /// Detect degradation conditions.
    pub fn detect_degradation(&self) -> CanonicalizerDegradation {
        if self.entries_processed > 100_000 {
            return CanonicalizerDegradation::HighVolume {
                entries_processed: self.entries_processed,
            };
        }
        if self.entries_processed > 0 {
            let ratio = self.entries_deduped as f64 / self.entries_processed as f64;
            if ratio > 0.5 {
                return CanonicalizerDegradation::HighDedupRatio { ratio };
            }
        }
        CanonicalizerDegradation::Healthy
    }

    /// Create a log entry for a canonicalization operation.
    pub fn log_entry(
        &self,
        trace_id: &str,
        input_entries: usize,
        output_entries: usize,
        duration_us: u64,
    ) -> CanonicalizerLogEntry {
        CanonicalizerLogEntry {
            timestamp_us: self.entries_processed, // monotonic proxy
            trace_id: trace_id.to_string(),
            input_entries,
            output_entries,
            duration_us,
        }
    }

    /// Reset counters.
    pub fn reset(&mut self) {
        self.traces_processed = 0;
        self.entries_processed = 0;
        self.entries_deduped = 0;
        self.comparisons_made = 0;
    }
}

// ── E3 Impl: Bridge methods and convenience API ───────────────────

impl ReplayCanonicalizer {
    /// Canonicalize and compare two model-checker trace outputs directly.
    pub fn compare_mc_traces(
        &mut self,
        a: &[TraceStep],
        b: &[TraceStep],
        seed: u64,
    ) -> ReplayComparisonResult {
        let ta = self.upgrade_trace(a, "mc-a".to_string(), seed);
        let tb = self.upgrade_trace(b, "mc-b".to_string(), seed);
        self.compare(&ta, &tb)
    }

    /// Check replay determinism: run canonicalize twice on the same trace
    /// and verify the output is identical (self-consistency check).
    pub fn verify_determinism(&mut self, trace: &DeterministicTrace) -> bool {
        let c1 = self.canonicalize(trace);
        let c2 = self.canonicalize(trace);
        c1.entries.iter().zip(c2.entries.iter()).all(|(a, b)| {
            a.fingerprint == b.fingerprint && a.seq == b.seq && a.timestamp_us == b.timestamp_us
        })
    }

    /// Extract a sub-trace containing only entries in the given domain.
    pub fn filter_by_domain(
        &self,
        trace: &DeterministicTrace,
        domain: InvariantDomain,
    ) -> DeterministicTrace {
        let entries: Vec<TraceEntry> = trace
            .entries
            .iter()
            .filter(|e| e.domain == domain)
            .cloned()
            .enumerate()
            .map(|(i, mut e)| {
                e.seq = i as u64;
                e
            })
            .collect();
        DeterministicTrace {
            version: trace.version,
            trace_id: format!("{}-{}", trace.trace_id, domain),
            seed: trace.seed,
            entries,
            created_at_us: trace.created_at_us,
            duration_us: trace.duration_us,
        }
    }

    /// Extract the causal dependency chain for a given entry.
    pub fn causal_chain(&self, trace: &DeterministicTrace, entry_seq: u64) -> Vec<u64> {
        let mut chain = Vec::new();
        let mut current = Some(entry_seq);
        let index: HashMap<u64, &TraceEntry> = trace.entries.iter().map(|e| (e.seq, e)).collect();
        while let Some(seq) = current {
            chain.push(seq);
            current = index.get(&seq).and_then(|e| e.causal_parent);
        }
        chain.reverse();
        chain
    }

    /// Compute per-domain entry counts.
    pub fn domain_histogram(&self, trace: &DeterministicTrace) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for entry in &trace.entries {
            *counts.entry(entry.domain.to_string()).or_insert(0) += 1;
        }
        counts
    }

    /// Find entries whose fingerprint appears exactly once (unique actions).
    pub fn unique_fingerprints(&self, trace: &DeterministicTrace) -> Vec<u64> {
        let mut counts: HashMap<u64, usize> = HashMap::new();
        for entry in &trace.entries {
            *counts.entry(entry.fingerprint).or_insert(0) += 1;
        }
        trace
            .entries
            .iter()
            .filter(|e| counts.get(&e.fingerprint) == Some(&1))
            .map(|e| e.seq)
            .collect()
    }

    /// Merge two traces interleaving by timestamp (union merge).
    pub fn merge_traces(
        &mut self,
        a: &DeterministicTrace,
        b: &DeterministicTrace,
    ) -> DeterministicTrace {
        let mut entries: Vec<TraceEntry> =
            a.entries.iter().chain(b.entries.iter()).cloned().collect();
        entries.sort_by(|x, y| x.timestamp_us.cmp(&y.timestamp_us).then(x.seq.cmp(&y.seq)));
        for (i, entry) in entries.iter_mut().enumerate() {
            entry.seq = i as u64;
        }
        let duration = entries
            .last()
            .map_or(0, |e| e.timestamp_us)
            .saturating_sub(a.created_at_us.min(b.created_at_us));
        DeterministicTrace {
            version: TraceFormatVersion::V2,
            trace_id: format!("{}-{}", a.trace_id, b.trace_id),
            seed: a.seed ^ b.seed,
            entries,
            created_at_us: a.created_at_us.min(b.created_at_us),
            duration_us: duration,
        }
    }

    /// Slice a trace to entries within a time window.
    pub fn time_window(
        &self,
        trace: &DeterministicTrace,
        start_us: u64,
        end_us: u64,
    ) -> DeterministicTrace {
        let entries: Vec<TraceEntry> = trace
            .entries
            .iter()
            .filter(|e| e.timestamp_us >= start_us && e.timestamp_us <= end_us)
            .cloned()
            .enumerate()
            .map(|(i, mut e)| {
                e.seq = i as u64;
                e
            })
            .collect();
        DeterministicTrace {
            version: trace.version,
            trace_id: format!("{}-window", trace.trace_id),
            seed: trace.seed,
            entries,
            created_at_us: start_us,
            duration_us: end_us.saturating_sub(start_us),
        }
    }

    /// Total comparisons performed.
    pub fn total_comparisons(&self) -> u64 {
        self.comparisons_made
    }

    /// Total traces processed.
    pub fn total_traces(&self) -> u64 {
        self.traces_processed
    }

    /// Access the current config.
    pub fn config(&self) -> &CanonicalizerConfig {
        &self.config
    }
}

/// Map a TraceAction to its primary InvariantDomain.
pub(super) fn action_domain(action: &TraceAction) -> InvariantDomain {
    match action {
        TraceAction::ObserveLatency { .. } => InvariantDomain::Budget,
        TraceAction::SchedulerAdmit { .. } => InvariantDomain::Scheduler,
        TraceAction::RecoveryStep { .. } => InvariantDomain::Recovery,
        TraceAction::EpochAdvance { .. } => InvariantDomain::Composition,
        TraceAction::Reset { domain } => *domain,
    }
}

/// Sort key for domain-grouped ordering.
fn domain_sort_key(domain: InvariantDomain) -> u8 {
    match domain {
        InvariantDomain::Scheduler => 0,
        InvariantDomain::Budget => 1,
        InvariantDomain::Recovery => 2,
        InvariantDomain::Composition => 3,
    }
}
