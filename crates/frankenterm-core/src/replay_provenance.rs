//! Replay provenance logs and decision-explanation trace (ft-og6q6.3.4).
//!
//! Emits structured provenance logs during replay execution so every decision
//! can be traced to its triggering event, input data, and rule version.
//!
//! # Components
//!
//! - [`ReplayProvenanceEmitter`] — Emits structured log events at each replay decision point.
//! - [`DecisionExplanationTrace`] — Builds explanation chains: trigger → rule → output.
//! - [`ReplayAuditTrail`] — Append-only tamper-evident chain linking replay runs.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Mutex;

// ============================================================================
// Verbosity levels
// ============================================================================

/// Controls how much detail the provenance emitter captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceVerbosity {
    /// Decisions only — no input data or event context.
    Minimal,
    /// Decisions + input hashes.
    Standard,
    /// Decisions + full input data + event context.
    Verbose,
}

impl Default for ProvenanceVerbosity {
    fn default() -> Self {
        Self::Standard
    }
}

// ============================================================================
// Decision types
// ============================================================================

/// The kind of decision captured by provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionType {
    PatternMatch,
    WorkflowStep,
    PolicyEvaluation,
    SideEffectBarrier,
    MergeReorder,
    OverrideApplied,
    CheckpointCreate,
    FaultInjection,
}

impl DecisionType {
    /// Human-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PatternMatch => "pattern_match",
            Self::WorkflowStep => "workflow_step",
            Self::PolicyEvaluation => "policy_evaluation",
            Self::SideEffectBarrier => "side_effect_barrier",
            Self::MergeReorder => "merge_reorder",
            Self::OverrideApplied => "override_applied",
            Self::CheckpointCreate => "checkpoint_create",
            Self::FaultInjection => "fault_injection",
        }
    }
}

// ============================================================================
// Provenance entry
// ============================================================================

/// A single provenance log entry capturing one replay decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceEntry {
    /// Unique replay run identifier.
    pub replay_run_id: String,
    /// Monotonic position within this replay.
    pub event_position: u64,
    /// The event that triggered this decision.
    pub event_id: String,
    /// What kind of decision.
    pub decision_type: DecisionType,
    /// Identifier of the rule/workflow/policy that matched.
    pub rule_id: String,
    /// Hash of the rule definition at replay time.
    pub definition_hash: String,
    /// Hash of the input data.
    pub input_hash: String,
    /// Summary of the decision output.
    pub output_summary: String,
    /// Wall-clock time of the decision (ms since epoch).
    pub wall_clock_ms: u64,
    /// Virtual clock time of the decision (ms in replay timeline).
    pub virtual_clock_ms: u64,
    /// Full input data (only present when verbosity is Verbose).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_data: Option<serde_json::Value>,
    /// Full event context (only present when verbosity is Verbose).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_context: Option<serde_json::Value>,
}

impl ProvenanceEntry {
    /// Compute a deterministic hash of this entry.
    #[must_use]
    pub fn hash(&self) -> String {
        let canonical = serde_json::to_string(self).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        hex::encode(hasher.finalize())
    }
}

// ============================================================================
// ReplayProvenanceEmitter
// ============================================================================

/// Configuration for the provenance emitter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceConfig {
    /// How much detail to capture.
    pub verbosity: ProvenanceVerbosity,
    /// Maximum entries to buffer in memory before eviction.
    pub max_memory_entries: usize,
}

impl Default for ProvenanceConfig {
    fn default() -> Self {
        Self {
            verbosity: ProvenanceVerbosity::Standard,
            max_memory_entries: 10_000,
        }
    }
}

/// Emits structured provenance log events during replay.
///
/// Thread-safe via `Mutex<ProvenanceInner>`.
pub struct ReplayProvenanceEmitter {
    config: ProvenanceConfig,
    replay_run_id: String,
    inner: Mutex<ProvenanceInner>,
}

struct ProvenanceInner {
    entries: Vec<ProvenanceEntry>,
    next_position: u64,
}

impl ReplayProvenanceEmitter {
    /// Create a new emitter for the given replay run.
    #[must_use]
    pub fn new(replay_run_id: String, config: ProvenanceConfig) -> Self {
        Self {
            config,
            replay_run_id,
            inner: Mutex::new(ProvenanceInner {
                entries: Vec::new(),
                next_position: 0,
            }),
        }
    }

    /// Create with default config.
    #[must_use]
    pub fn with_defaults(replay_run_id: String) -> Self {
        Self::new(replay_run_id, ProvenanceConfig::default())
    }

    /// Record a replay decision.
    ///
    /// Returns the assigned event_position.
    pub fn record(&self, params: ProvenanceRecordParams) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let position = inner.next_position;
        inner.next_position += 1;

        let input_hash = compute_hash(&params.input_data);

        let entry = ProvenanceEntry {
            replay_run_id: self.replay_run_id.clone(),
            event_position: position,
            event_id: params.event_id,
            decision_type: params.decision_type,
            rule_id: params.rule_id,
            definition_hash: params.definition_hash,
            input_hash,
            output_summary: params.output_summary,
            wall_clock_ms: params.wall_clock_ms,
            virtual_clock_ms: params.virtual_clock_ms,
            input_data: match self.config.verbosity {
                ProvenanceVerbosity::Verbose => Some(params.input_data),
                _ => None,
            },
            event_context: match self.config.verbosity {
                ProvenanceVerbosity::Verbose => params.event_context,
                _ => None,
            },
        };

        // FIFO eviction
        if inner.entries.len() >= self.config.max_memory_entries {
            inner.entries.remove(0);
        }
        inner.entries.push(entry);
        position
    }

    /// Get all recorded entries.
    #[must_use]
    pub fn entries(&self) -> Vec<ProvenanceEntry> {
        self.inner.lock().unwrap().entries.clone()
    }

    /// Number of recorded entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    /// Whether the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().entries.is_empty()
    }

    /// Get the replay run ID.
    #[must_use]
    pub fn replay_run_id(&self) -> &str {
        &self.replay_run_id
    }

    /// Get the verbosity level.
    #[must_use]
    pub fn verbosity(&self) -> ProvenanceVerbosity {
        self.config.verbosity
    }

    /// Export entries as JSONL string.
    #[must_use]
    pub fn to_jsonl(&self) -> String {
        let entries = self.entries();
        entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Parse entries from JSONL string.
    pub fn from_jsonl(jsonl: &str) -> Result<Vec<ProvenanceEntry>, String> {
        jsonl
            .lines()
            .filter(|l| !l.trim().is_empty())
            .enumerate()
            .map(|(i, line)| {
                serde_json::from_str(line)
                    .map_err(|e| format!("line {}: {}", i + 1, e))
            })
            .collect()
    }

    /// Drain all entries from the buffer, resetting it.
    pub fn drain(&self) -> Vec<ProvenanceEntry> {
        let mut inner = self.inner.lock().unwrap();
        std::mem::take(&mut inner.entries)
    }

    /// Entries filtered by decision type.
    #[must_use]
    pub fn entries_of_type(&self, dt: DecisionType) -> Vec<ProvenanceEntry> {
        self.entries()
            .into_iter()
            .filter(|e| e.decision_type == dt)
            .collect()
    }
}

/// Parameters for recording a provenance entry.
#[derive(Debug, Clone)]
pub struct ProvenanceRecordParams {
    pub event_id: String,
    pub decision_type: DecisionType,
    pub rule_id: String,
    pub definition_hash: String,
    pub output_summary: String,
    pub wall_clock_ms: u64,
    pub virtual_clock_ms: u64,
    pub input_data: serde_json::Value,
    pub event_context: Option<serde_json::Value>,
}

// ============================================================================
// DecisionExplanationTrace
// ============================================================================

/// A single link in the decision explanation chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplanationLink {
    /// The event that triggered this decision.
    pub triggering_event_id: String,
    /// The rule that matched.
    pub rule_id: String,
    /// Hash of the rule definition at replay time.
    pub replay_definition_hash: String,
    /// Hash of the rule definition in the original artifact.
    pub artifact_definition_hash: String,
    /// Whether the definitions differ (counterfactual).
    pub definition_mismatch: bool,
    /// The decision output.
    pub decision_output: String,
}

/// Explanation trace for a single decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionExplanationTrace {
    /// Provenance entry position this trace explains.
    pub event_position: u64,
    /// Chain of explanation links (trigger → rule → output).
    pub chain: Vec<ExplanationLink>,
    /// Overall mismatch detected (any link has definition_mismatch).
    pub has_counterfactual: bool,
}

impl DecisionExplanationTrace {
    /// Build a trace from a single triggering event + rule match.
    #[must_use]
    pub fn single(
        event_position: u64,
        triggering_event_id: String,
        rule_id: String,
        replay_definition_hash: String,
        artifact_definition_hash: String,
        decision_output: String,
    ) -> Self {
        let definition_mismatch = replay_definition_hash != artifact_definition_hash;
        let link = ExplanationLink {
            triggering_event_id,
            rule_id,
            replay_definition_hash,
            artifact_definition_hash,
            definition_mismatch,
            decision_output,
        };
        Self {
            event_position,
            chain: vec![link],
            has_counterfactual: definition_mismatch,
        }
    }

    /// Append another link to the explanation chain.
    pub fn push_link(&mut self, link: ExplanationLink) {
        if link.definition_mismatch {
            self.has_counterfactual = true;
        }
        self.chain.push(link);
    }

    /// Number of links in the chain.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.chain.len()
    }
}

/// Collects explanation traces during a replay.
pub struct ExplanationTraceCollector {
    traces: Mutex<Vec<DecisionExplanationTrace>>,
}

impl ExplanationTraceCollector {
    #[must_use]
    pub fn new() -> Self {
        Self {
            traces: Mutex::new(Vec::new()),
        }
    }

    /// Add a trace.
    pub fn add(&self, trace: DecisionExplanationTrace) {
        self.traces.lock().unwrap().push(trace);
    }

    /// Get all traces.
    #[must_use]
    pub fn traces(&self) -> Vec<DecisionExplanationTrace> {
        self.traces.lock().unwrap().clone()
    }

    /// Number of traces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.traces.lock().unwrap().len()
    }

    /// Whether the collector is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.traces.lock().unwrap().is_empty()
    }

    /// Count traces with counterfactual mismatches.
    #[must_use]
    pub fn counterfactual_count(&self) -> usize {
        self.traces
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.has_counterfactual)
            .count()
    }
}

impl Default for ExplanationTraceCollector {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// ReplayAuditTrail — tamper-evident chain of replay runs
// ============================================================================

/// Genesis hash for the audit chain.
pub const REPLAY_AUDIT_GENESIS: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// Schema version for replay audit entries.
pub const REPLAY_AUDIT_VERSION: &str = "ft.replay.audit.v1";

/// A single entry in the replay audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayAuditEntry {
    /// Schema version.
    pub audit_version: String,
    /// Monotonic ordinal.
    pub ordinal: u64,
    /// Replay run identifier.
    pub replay_run_id: String,
    /// Who initiated the replay.
    pub actor: String,
    /// When the replay started (ms since epoch).
    pub started_at_ms: u64,
    /// When the replay completed (0 if still running).
    pub completed_at_ms: u64,
    /// Path/identifier of the source artifact.
    pub artifact_ref: String,
    /// Override package applied (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub override_ref: Option<String>,
    /// Total decisions made in this replay.
    pub decision_count: u64,
    /// Total anomalies detected.
    pub anomaly_count: u64,
    /// Hash of the previous entry in the chain.
    pub prev_entry_hash: String,
}

impl ReplayAuditEntry {
    /// Compute the SHA-256 hash of this entry's canonical JSON.
    #[must_use]
    pub fn hash(&self) -> String {
        let canonical = serde_json::to_string(self).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        hex::encode(hasher.finalize())
    }
}

/// Result of verifying the audit trail chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditChainVerification {
    /// Total entries checked.
    pub total_entries: u64,
    /// Whether the chain is intact (no tampering).
    pub chain_intact: bool,
    /// Ordinal of first break (if any).
    pub first_break_at: Option<u64>,
    /// Missing ordinals (gaps).
    pub missing_ordinals: Vec<u64>,
}

/// Append-only tamper-evident audit trail for replay runs.
pub struct ReplayAuditTrail {
    inner: Mutex<AuditTrailInner>,
}

struct AuditTrailInner {
    entries: Vec<ReplayAuditEntry>,
    next_ordinal: u64,
    last_hash: String,
}

impl ReplayAuditTrail {
    /// Create a new empty audit trail.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(AuditTrailInner {
                entries: Vec::new(),
                next_ordinal: 0,
                last_hash: REPLAY_AUDIT_GENESIS.to_string(),
            }),
        }
    }

    /// Append a new replay run entry.
    ///
    /// Returns the ordinal assigned to this entry.
    pub fn append(&self, params: AuditEntryParams) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let ordinal = inner.next_ordinal;
        inner.next_ordinal += 1;

        let entry = ReplayAuditEntry {
            audit_version: REPLAY_AUDIT_VERSION.to_string(),
            ordinal,
            replay_run_id: params.replay_run_id,
            actor: params.actor,
            started_at_ms: params.started_at_ms,
            completed_at_ms: params.completed_at_ms,
            artifact_ref: params.artifact_ref,
            override_ref: params.override_ref,
            decision_count: params.decision_count,
            anomaly_count: params.anomaly_count,
            prev_entry_hash: inner.last_hash.clone(),
        };

        inner.last_hash = entry.hash();
        inner.entries.push(entry);
        ordinal
    }

    /// Get all entries.
    #[must_use]
    pub fn entries(&self) -> Vec<ReplayAuditEntry> {
        self.inner.lock().unwrap().entries.clone()
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    /// Whether the trail is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().entries.is_empty()
    }

    /// Get the hash of the last entry (or genesis hash).
    #[must_use]
    pub fn last_hash(&self) -> String {
        self.inner.lock().unwrap().last_hash.clone()
    }

    /// Verify the integrity of the chain.
    #[must_use]
    pub fn verify(&self) -> AuditChainVerification {
        let inner = self.inner.lock().unwrap();
        verify_chain(&inner.entries)
    }

    /// Export entries as JSONL.
    #[must_use]
    pub fn to_jsonl(&self) -> String {
        let entries = self.entries();
        entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Import entries from JSONL (for verification).
    pub fn from_jsonl(jsonl: &str) -> Result<Vec<ReplayAuditEntry>, String> {
        jsonl
            .lines()
            .filter(|l| !l.trim().is_empty())
            .enumerate()
            .map(|(i, line)| {
                serde_json::from_str(line)
                    .map_err(|e| format!("line {}: {}", i + 1, e))
            })
            .collect()
    }
}

impl Default for ReplayAuditTrail {
    fn default() -> Self {
        Self::new()
    }
}

/// Parameters for appending a replay audit entry.
#[derive(Debug, Clone)]
pub struct AuditEntryParams {
    pub replay_run_id: String,
    pub actor: String,
    pub started_at_ms: u64,
    pub completed_at_ms: u64,
    pub artifact_ref: String,
    pub override_ref: Option<String>,
    pub decision_count: u64,
    pub anomaly_count: u64,
}

/// Verify the integrity of a chain of audit entries.
#[must_use]
pub fn verify_chain(entries: &[ReplayAuditEntry]) -> AuditChainVerification {
    if entries.is_empty() {
        return AuditChainVerification {
            total_entries: 0,
            chain_intact: true,
            first_break_at: None,
            missing_ordinals: vec![],
        };
    }

    let mut chain_intact = true;
    let mut first_break_at = None;
    let mut missing_ordinals = Vec::new();

    // Check genesis link.
    if entries[0].prev_entry_hash != REPLAY_AUDIT_GENESIS {
        chain_intact = false;
        first_break_at = Some(entries[0].ordinal);
    }

    // Check sequential links.
    for i in 1..entries.len() {
        let prev_hash = entries[i - 1].hash();
        if entries[i].prev_entry_hash != prev_hash {
            if chain_intact {
                chain_intact = false;
                first_break_at = Some(entries[i].ordinal);
            }
        }
    }

    // Check ordinal gaps.
    for i in 1..entries.len() {
        let expected = entries[i - 1].ordinal + 1;
        let actual = entries[i].ordinal;
        if actual > expected {
            for gap in expected..actual {
                missing_ordinals.push(gap);
            }
        }
    }

    AuditChainVerification {
        total_entries: entries.len() as u64,
        chain_intact,
        first_break_at,
        missing_ordinals,
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Compute SHA-256 hash of a JSON value.
fn compute_hash(data: &serde_json::Value) -> String {
    let canonical = serde_json::to_string(data).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hex::encode(hasher.finalize())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_params(
        event_id: &str,
        dt: DecisionType,
        rule: &str,
    ) -> ProvenanceRecordParams {
        ProvenanceRecordParams {
            event_id: event_id.to_string(),
            decision_type: dt,
            rule_id: rule.to_string(),
            definition_hash: "def_hash_abc".to_string(),
            output_summary: "allowed".to_string(),
            wall_clock_ms: 1000,
            virtual_clock_ms: 500,
            input_data: json!({"key": "value"}),
            event_context: Some(json!({"pane": 1})),
        }
    }

    fn make_audit_params(run_id: &str) -> AuditEntryParams {
        AuditEntryParams {
            replay_run_id: run_id.to_string(),
            actor: "agent_test".to_string(),
            started_at_ms: 1000,
            completed_at_ms: 2000,
            artifact_ref: "artifact_v1.ftreplay".to_string(),
            override_ref: None,
            decision_count: 42,
            anomaly_count: 0,
        }
    }

    // ── ProvenanceEmitter ────────────────────────────────────────────

    #[test]
    fn emitter_records_correct_fields() {
        let emitter = ReplayProvenanceEmitter::with_defaults("run_001".into());
        let pos = emitter.record(make_params("evt_1", DecisionType::PatternMatch, "rule_a"));
        assert_eq!(pos, 0);
        let entries = emitter.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].replay_run_id, "run_001");
        assert_eq!(entries[0].event_id, "evt_1");
        assert_eq!(entries[0].decision_type, DecisionType::PatternMatch);
        assert_eq!(entries[0].rule_id, "rule_a");
    }

    #[test]
    fn emitter_workflow_step_fields() {
        let emitter = ReplayProvenanceEmitter::with_defaults("run_002".into());
        emitter.record(make_params("evt_2", DecisionType::WorkflowStep, "wf_step_1"));
        let e = &emitter.entries()[0];
        assert_eq!(e.decision_type, DecisionType::WorkflowStep);
        assert_eq!(e.rule_id, "wf_step_1");
    }

    #[test]
    fn emitter_policy_evaluation_fields() {
        let emitter = ReplayProvenanceEmitter::with_defaults("run_003".into());
        emitter.record(make_params("evt_3", DecisionType::PolicyEvaluation, "policy_x"));
        let e = &emitter.entries()[0];
        assert_eq!(e.decision_type, DecisionType::PolicyEvaluation);
    }

    #[test]
    fn emitter_unique_run_id() {
        let e1 = ReplayProvenanceEmitter::with_defaults("run_a".into());
        let e2 = ReplayProvenanceEmitter::with_defaults("run_b".into());
        assert_ne!(e1.replay_run_id(), e2.replay_run_id());
    }

    #[test]
    fn emitter_position_increments() {
        let emitter = ReplayProvenanceEmitter::with_defaults("run_inc".into());
        let p0 = emitter.record(make_params("a", DecisionType::PatternMatch, "r"));
        let p1 = emitter.record(make_params("b", DecisionType::PatternMatch, "r"));
        let p2 = emitter.record(make_params("c", DecisionType::PatternMatch, "r"));
        assert_eq!(p0, 0);
        assert_eq!(p1, 1);
        assert_eq!(p2, 2);
    }

    #[test]
    fn emitter_minimal_omits_input() {
        let config = ProvenanceConfig {
            verbosity: ProvenanceVerbosity::Minimal,
            ..Default::default()
        };
        let emitter = ReplayProvenanceEmitter::new("run_min".into(), config);
        emitter.record(make_params("e", DecisionType::PatternMatch, "r"));
        let e = &emitter.entries()[0];
        assert!(e.input_data.is_none());
        assert!(e.event_context.is_none());
    }

    #[test]
    fn emitter_verbose_includes_context() {
        let config = ProvenanceConfig {
            verbosity: ProvenanceVerbosity::Verbose,
            ..Default::default()
        };
        let emitter = ReplayProvenanceEmitter::new("run_verb".into(), config);
        emitter.record(make_params("e", DecisionType::PatternMatch, "r"));
        let e = &emitter.entries()[0];
        assert!(e.input_data.is_some());
        assert!(e.event_context.is_some());
    }

    #[test]
    fn emitter_standard_omits_context() {
        let config = ProvenanceConfig {
            verbosity: ProvenanceVerbosity::Standard,
            ..Default::default()
        };
        let emitter = ReplayProvenanceEmitter::new("run_std".into(), config);
        emitter.record(make_params("e", DecisionType::PatternMatch, "r"));
        let e = &emitter.entries()[0];
        assert!(e.input_data.is_none());
        assert!(e.event_context.is_none());
    }

    #[test]
    fn emitter_input_hash_present() {
        let emitter = ReplayProvenanceEmitter::with_defaults("run_hash".into());
        emitter.record(make_params("e", DecisionType::PatternMatch, "r"));
        let e = &emitter.entries()[0];
        assert!(!e.input_hash.is_empty());
        assert_eq!(e.input_hash.len(), 64); // SHA-256 hex
    }

    #[test]
    fn emitter_jsonl_roundtrip() {
        let emitter = ReplayProvenanceEmitter::with_defaults("run_jsonl".into());
        emitter.record(make_params("a", DecisionType::PatternMatch, "r1"));
        emitter.record(make_params("b", DecisionType::WorkflowStep, "r2"));
        let jsonl = emitter.to_jsonl();
        let restored = ReplayProvenanceEmitter::from_jsonl(&jsonl).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].event_id, "a");
        assert_eq!(restored[1].event_id, "b");
    }

    #[test]
    fn emitter_buffer_sink() {
        let emitter = ReplayProvenanceEmitter::with_defaults("run_buf".into());
        for i in 0..5 {
            emitter.record(make_params(&format!("e{i}"), DecisionType::PatternMatch, "r"));
        }
        assert_eq!(emitter.len(), 5);
        let drained = emitter.drain();
        assert_eq!(drained.len(), 5);
        assert!(emitter.is_empty());
    }

    #[test]
    fn emitter_fifo_eviction() {
        let config = ProvenanceConfig {
            verbosity: ProvenanceVerbosity::Minimal,
            max_memory_entries: 3,
        };
        let emitter = ReplayProvenanceEmitter::new("run_evict".into(), config);
        for i in 0..5 {
            emitter.record(make_params(&format!("e{i}"), DecisionType::PatternMatch, "r"));
        }
        assert_eq!(emitter.len(), 3);
        // Should have the last 3 entries
        let entries = emitter.entries();
        assert_eq!(entries[0].event_id, "e2");
        assert_eq!(entries[2].event_id, "e4");
    }

    #[test]
    fn emitter_filter_by_type() {
        let emitter = ReplayProvenanceEmitter::with_defaults("run_filt".into());
        emitter.record(make_params("a", DecisionType::PatternMatch, "r"));
        emitter.record(make_params("b", DecisionType::WorkflowStep, "r"));
        emitter.record(make_params("c", DecisionType::PatternMatch, "r"));
        let filtered = emitter.entries_of_type(DecisionType::PatternMatch);
        assert_eq!(filtered.len(), 2);
    }

    // ── DecisionExplanationTrace ─────────────────────────────────────

    #[test]
    fn trace_single_no_mismatch() {
        let trace = DecisionExplanationTrace::single(
            0,
            "evt_1".to_string(),
            "rule_a".to_string(),
            "hash_x".to_string(),
            "hash_x".to_string(), // Same hash — no mismatch
            "allowed".to_string(),
        );
        assert_eq!(trace.depth(), 1);
        assert!(!trace.has_counterfactual);
        assert!(!trace.chain[0].definition_mismatch);
    }

    #[test]
    fn trace_single_with_mismatch() {
        let trace = DecisionExplanationTrace::single(
            0,
            "evt_1".to_string(),
            "rule_a".to_string(),
            "hash_x".to_string(),
            "hash_y".to_string(), // Different — counterfactual
            "blocked".to_string(),
        );
        assert!(trace.has_counterfactual);
        assert!(trace.chain[0].definition_mismatch);
    }

    #[test]
    fn trace_links_event_correctly() {
        let trace = DecisionExplanationTrace::single(
            42,
            "evt_42".to_string(),
            "rule_deep".to_string(),
            "h1".to_string(),
            "h1".to_string(),
            "pass".to_string(),
        );
        assert_eq!(trace.event_position, 42);
        assert_eq!(trace.chain[0].triggering_event_id, "evt_42");
        assert_eq!(trace.chain[0].rule_id, "rule_deep");
    }

    #[test]
    fn trace_push_link() {
        let mut trace = DecisionExplanationTrace::single(
            0, "e1".into(), "r1".into(), "h".into(), "h".into(), "ok".into(),
        );
        assert!(!trace.has_counterfactual);
        trace.push_link(ExplanationLink {
            triggering_event_id: "e2".into(),
            rule_id: "r2".into(),
            replay_definition_hash: "h1".into(),
            artifact_definition_hash: "h2".into(), // Mismatch
            definition_mismatch: true,
            decision_output: "diff".into(),
        });
        assert_eq!(trace.depth(), 2);
        assert!(trace.has_counterfactual);
    }

    #[test]
    fn trace_serde_roundtrip() {
        let trace = DecisionExplanationTrace::single(
            5, "evt".into(), "rule".into(), "h1".into(), "h2".into(), "out".into(),
        );
        let json = serde_json::to_string(&trace).unwrap();
        let back: DecisionExplanationTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(trace.event_position, back.event_position);
        assert_eq!(trace.has_counterfactual, back.has_counterfactual);
        assert_eq!(trace.chain.len(), back.chain.len());
    }

    #[test]
    fn trace_collector_counts() {
        let collector = ExplanationTraceCollector::new();
        collector.add(DecisionExplanationTrace::single(
            0, "e".into(), "r".into(), "h".into(), "h".into(), "ok".into(),
        ));
        collector.add(DecisionExplanationTrace::single(
            1, "e".into(), "r".into(), "a".into(), "b".into(), "diff".into(),
        ));
        assert_eq!(collector.len(), 2);
        assert_eq!(collector.counterfactual_count(), 1);
    }

    // ── ReplayAuditTrail ─────────────────────────────────────────────

    #[test]
    fn audit_trail_genesis() {
        let trail = ReplayAuditTrail::new();
        assert_eq!(trail.last_hash(), REPLAY_AUDIT_GENESIS);
        assert!(trail.is_empty());
    }

    #[test]
    fn audit_trail_append_and_chain() {
        let trail = ReplayAuditTrail::new();
        trail.append(make_audit_params("run_1"));
        trail.append(make_audit_params("run_2"));
        assert_eq!(trail.len(), 2);
        let entries = trail.entries();
        assert_eq!(entries[0].prev_entry_hash, REPLAY_AUDIT_GENESIS);
        assert_eq!(entries[1].prev_entry_hash, entries[0].hash());
    }

    #[test]
    fn audit_trail_verify_intact() {
        let trail = ReplayAuditTrail::new();
        trail.append(make_audit_params("run_1"));
        trail.append(make_audit_params("run_2"));
        trail.append(make_audit_params("run_3"));
        let v = trail.verify();
        assert!(v.chain_intact);
        assert_eq!(v.total_entries, 3);
        assert!(v.missing_ordinals.is_empty());
    }

    #[test]
    fn audit_trail_tamper_detection() {
        let trail = ReplayAuditTrail::new();
        trail.append(make_audit_params("run_1"));
        trail.append(make_audit_params("run_2"));
        trail.append(make_audit_params("run_3"));

        // Tamper with middle entry
        let mut entries = trail.entries();
        entries[1].decision_count = 999; // Modify field
        let v = verify_chain(&entries);
        assert!(!v.chain_intact);
        assert_eq!(v.first_break_at, Some(2)); // Entry 2 points to tampered entry 1
    }

    #[test]
    fn audit_trail_ordinal_gap_detection() {
        let mut entries = vec![];
        entries.push(ReplayAuditEntry {
            audit_version: REPLAY_AUDIT_VERSION.into(),
            ordinal: 0,
            replay_run_id: "r0".into(),
            actor: "a".into(),
            started_at_ms: 100,
            completed_at_ms: 200,
            artifact_ref: "art".into(),
            override_ref: None,
            decision_count: 10,
            anomaly_count: 0,
            prev_entry_hash: REPLAY_AUDIT_GENESIS.into(),
        });
        let h0 = entries[0].hash();
        entries.push(ReplayAuditEntry {
            audit_version: REPLAY_AUDIT_VERSION.into(),
            ordinal: 3, // Gap: missing 1, 2
            replay_run_id: "r3".into(),
            actor: "a".into(),
            started_at_ms: 300,
            completed_at_ms: 400,
            artifact_ref: "art".into(),
            override_ref: None,
            decision_count: 10,
            anomaly_count: 0,
            prev_entry_hash: h0,
        });
        let v = verify_chain(&entries);
        assert_eq!(v.missing_ordinals, vec![1, 2]);
    }

    #[test]
    fn audit_trail_jsonl_roundtrip() {
        let trail = ReplayAuditTrail::new();
        trail.append(make_audit_params("run_a"));
        trail.append(make_audit_params("run_b"));
        let jsonl = trail.to_jsonl();
        let restored = ReplayAuditTrail::from_jsonl(&jsonl).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].replay_run_id, "run_a");
        assert_eq!(restored[1].replay_run_id, "run_b");
    }

    #[test]
    fn audit_entry_hash_deterministic() {
        let trail = ReplayAuditTrail::new();
        trail.append(make_audit_params("run_1"));
        let entries = trail.entries();
        let h1 = entries[0].hash();
        let h2 = entries[0].hash();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn audit_trail_empty_verify() {
        let trail = ReplayAuditTrail::new();
        let v = trail.verify();
        assert!(v.chain_intact);
        assert_eq!(v.total_entries, 0);
    }

    // ── Serde roundtrips ─────────────────────────────────────────────

    #[test]
    fn provenance_entry_serde() {
        let emitter = ReplayProvenanceEmitter::with_defaults("run_serde".into());
        emitter.record(make_params("e", DecisionType::PatternMatch, "r"));
        let entry = &emitter.entries()[0];
        let json = serde_json::to_string(entry).unwrap();
        let back: ProvenanceEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.replay_run_id, back.replay_run_id);
        assert_eq!(entry.event_id, back.event_id);
    }

    #[test]
    fn provenance_config_serde() {
        let config = ProvenanceConfig {
            verbosity: ProvenanceVerbosity::Verbose,
            max_memory_entries: 500,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: ProvenanceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.verbosity, back.verbosity);
        assert_eq!(config.max_memory_entries, back.max_memory_entries);
    }

    #[test]
    fn decision_type_serde() {
        for dt in [
            DecisionType::PatternMatch,
            DecisionType::WorkflowStep,
            DecisionType::PolicyEvaluation,
            DecisionType::SideEffectBarrier,
            DecisionType::MergeReorder,
            DecisionType::OverrideApplied,
            DecisionType::CheckpointCreate,
            DecisionType::FaultInjection,
        ] {
            let json = serde_json::to_string(&dt).unwrap();
            let back: DecisionType = serde_json::from_str(&json).unwrap();
            assert_eq!(dt, back);
        }
    }

    #[test]
    fn verbosity_serde() {
        for v in [
            ProvenanceVerbosity::Minimal,
            ProvenanceVerbosity::Standard,
            ProvenanceVerbosity::Verbose,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: ProvenanceVerbosity = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn audit_entry_serde() {
        let trail = ReplayAuditTrail::new();
        trail.append(make_audit_params("run_s"));
        let entry = &trail.entries()[0];
        let json = serde_json::to_string(entry).unwrap();
        let back: ReplayAuditEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.replay_run_id, back.replay_run_id);
        assert_eq!(entry.ordinal, back.ordinal);
    }

    #[test]
    fn provenance_entry_hash_deterministic() {
        let emitter = ReplayProvenanceEmitter::with_defaults("run_det".into());
        emitter.record(make_params("e", DecisionType::PatternMatch, "r"));
        let entry = &emitter.entries()[0];
        let h1 = entry.hash();
        let h2 = entry.hash();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn different_inputs_different_hashes() {
        let emitter = ReplayProvenanceEmitter::with_defaults("run_diff".into());
        emitter.record(ProvenanceRecordParams {
            event_id: "e1".into(),
            decision_type: DecisionType::PatternMatch,
            rule_id: "r".into(),
            definition_hash: "dh".into(),
            output_summary: "ok".into(),
            wall_clock_ms: 100,
            virtual_clock_ms: 50,
            input_data: json!({"x": 1}),
            event_context: None,
        });
        emitter.record(ProvenanceRecordParams {
            event_id: "e2".into(),
            decision_type: DecisionType::PatternMatch,
            rule_id: "r".into(),
            definition_hash: "dh".into(),
            output_summary: "ok".into(),
            wall_clock_ms: 100,
            virtual_clock_ms: 50,
            input_data: json!({"x": 2}),
            event_context: None,
        });
        let entries = emitter.entries();
        assert_ne!(entries[0].input_hash, entries[1].input_hash);
    }
}
