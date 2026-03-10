//! Forensic export pipeline types and query engine.
//!
//! Provides forensic query/export capabilities that reconstruct who did what,
//! when, why, and under which policy context across terminal and connector
//! actions. Supports filtering, pagination, and multiple export formats
//! (JSON, JSONL, CSV) for compliance workflows.
//!
//! Part of ft-3681t.6.4 precursor work.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

// =============================================================================
// Forensic record — the canonical audit entry
// =============================================================================

/// A single forensic record capturing one auditable action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForensicRecord {
    /// Unique record identifier (UUID or monotonic).
    pub record_id: String,
    /// Millisecond timestamp of the action.
    pub timestamp_ms: u64,
    /// Who performed the action.
    pub actor: ForensicActor,
    /// What was done.
    pub action: ForensicAction,
    /// What resource was acted upon.
    pub target: ForensicTarget,
    /// Policy decision that governed this action.
    pub policy_decision: ForensicPolicyDecision,
    /// Outcome of the action.
    pub outcome: ForensicOutcome,
    /// Correlation identifiers for tracing across subsystems.
    pub correlation: CorrelationIds,
    /// Sensitivity classification of this record.
    pub sensitivity: SensitivityLevel,
    /// Arbitrary key-value metadata.
    pub metadata: BTreeMap<String, String>,
}

/// The actor who performed an auditable action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ForensicActor {
    /// A human operator.
    Operator { operator_id: String, session_id: String },
    /// An AI agent.
    Agent { agent_id: String, model: String },
    /// The system itself (automated).
    System { subsystem: String },
    /// A connector acting on behalf of an external service.
    Connector { connector_id: String, provider: String },
}

impl fmt::Display for ForensicActor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Operator { operator_id, .. } => write!(f, "operator:{operator_id}"),
            Self::Agent { agent_id, .. } => write!(f, "agent:{agent_id}"),
            Self::System { subsystem } => write!(f, "system:{subsystem}"),
            Self::Connector { connector_id, .. } => write!(f, "connector:{connector_id}"),
        }
    }
}

/// The action that was performed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ForensicAction {
    /// Text or command sent to a terminal pane.
    PaneWrite { pane_id: String, command_summary: String },
    /// Workflow started, stopped, or modified.
    WorkflowLifecycle { workflow_id: String, transition: String },
    /// Policy rule was evaluated.
    PolicyEvaluation { rule_id: String, surface: String },
    /// Connector dispatched an outbound action.
    ConnectorDispatch { connector_id: String, action_type: String },
    /// Credential was issued, rotated, or revoked.
    CredentialAction { credential_id: String, action_type: String },
    /// Quarantine state was changed.
    QuarantineChange { component_id: String, new_state: String },
    /// Kill switch was tripped or reset.
    KillSwitchChange { new_level: String },
    /// Configuration was modified.
    ConfigChange { config_key: String },
    /// Session lifecycle (connect, disconnect, resume).
    SessionLifecycle { session_id: String, transition: String },
    /// Custom action for extensibility.
    Custom { category: String, detail: String },
}

impl fmt::Display for ForensicAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PaneWrite { pane_id, .. } => write!(f, "pane_write:{pane_id}"),
            Self::WorkflowLifecycle { workflow_id, .. } => {
                write!(f, "workflow:{workflow_id}")
            }
            Self::PolicyEvaluation { rule_id, .. } => write!(f, "policy:{rule_id}"),
            Self::ConnectorDispatch { connector_id, .. } => {
                write!(f, "connector:{connector_id}")
            }
            Self::CredentialAction { credential_id, .. } => {
                write!(f, "credential:{credential_id}")
            }
            Self::QuarantineChange { component_id, .. } => {
                write!(f, "quarantine:{component_id}")
            }
            Self::KillSwitchChange { new_level } => write!(f, "kill_switch:{new_level}"),
            Self::ConfigChange { config_key } => write!(f, "config:{config_key}"),
            Self::SessionLifecycle { session_id, .. } => write!(f, "session:{session_id}"),
            Self::Custom { category, .. } => write!(f, "custom:{category}"),
        }
    }
}

/// The resource targeted by an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ForensicTarget {
    /// A terminal pane.
    Pane { pane_id: String, workspace: String },
    /// A workflow.
    Workflow { workflow_id: String },
    /// A session.
    Session { session_id: String },
    /// A connector.
    Connector { connector_id: String },
    /// A credential.
    Credential { credential_id: String },
    /// A policy rule.
    PolicyRule { rule_id: String },
    /// A component (for quarantine operations).
    Component { component_id: String, component_kind: String },
    /// The system itself.
    System { subsystem: String },
}

/// The policy decision that governed an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForensicPolicyDecision {
    /// Whether the action was allowed.
    pub decision: PolicyVerdict,
    /// Which rules matched.
    pub matched_rules: Vec<String>,
    /// The policy surface that was evaluated.
    pub surface: String,
    /// Reason for the decision.
    pub reason: String,
}

/// The verdict from policy evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyVerdict {
    /// Action was allowed.
    Allow,
    /// Action was denied.
    Deny,
    /// Action was allowed but flagged for review.
    AllowWithFlag,
    /// No policy matched (default allow).
    NoMatch,
}

impl fmt::Display for PolicyVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allow => write!(f, "allow"),
            Self::Deny => write!(f, "deny"),
            Self::AllowWithFlag => write!(f, "allow_with_flag"),
            Self::NoMatch => write!(f, "no_match"),
        }
    }
}

/// The outcome of an auditable action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForensicOutcome {
    /// Action completed successfully.
    Success,
    /// Action failed with error.
    Failed { error: String },
    /// Action was denied by policy.
    Denied { reason: String },
    /// Action was blocked by quarantine.
    Blocked { blocker: String },
    /// Action timed out.
    Timeout,
    /// Action was rolled back.
    RolledBack { reason: String },
}

impl fmt::Display for ForensicOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => write!(f, "success"),
            Self::Failed { error } => write!(f, "failed: {error}"),
            Self::Denied { reason } => write!(f, "denied: {reason}"),
            Self::Blocked { blocker } => write!(f, "blocked: {blocker}"),
            Self::Timeout => write!(f, "timeout"),
            Self::RolledBack { reason } => write!(f, "rolled_back: {reason}"),
        }
    }
}

/// Correlation identifiers for cross-subsystem tracing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CorrelationIds {
    /// Top-level trace ID.
    pub trace_id: Option<String>,
    /// Span within the trace.
    pub span_id: Option<String>,
    /// Session correlation.
    pub session_id: Option<String>,
    /// Workflow correlation.
    pub workflow_id: Option<String>,
    /// Transaction correlation.
    pub transaction_id: Option<String>,
}

/// Sensitivity classification for redaction control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensitivityLevel {
    /// Public — no redaction needed.
    Public,
    /// Internal — visible within the organization.
    Internal,
    /// Confidential — limited distribution.
    Confidential,
    /// Restricted — highly sensitive, strongest redaction.
    Restricted,
}

impl fmt::Display for SensitivityLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Public => write!(f, "public"),
            Self::Internal => write!(f, "internal"),
            Self::Confidential => write!(f, "confidential"),
            Self::Restricted => write!(f, "restricted"),
        }
    }
}

// =============================================================================
// Query engine
// =============================================================================

/// Filter criteria for forensic queries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForensicQuery {
    /// Filter by time range (inclusive).
    pub time_range: Option<TimeRange>,
    /// Filter by actor kind/id.
    pub actor_filter: Option<String>,
    /// Filter by action kind.
    pub action_filter: Option<String>,
    /// Filter by policy verdict.
    pub verdict_filter: Option<PolicyVerdict>,
    /// Filter by outcome kind.
    pub outcome_filter: Option<String>,
    /// Filter by sensitivity level (minimum).
    pub min_sensitivity: Option<SensitivityLevel>,
    /// Filter by correlation trace ID.
    pub trace_id: Option<String>,
    /// Filter by workflow ID.
    pub workflow_id: Option<String>,
    /// Full-text search in metadata values.
    pub text_search: Option<String>,
    /// Maximum records to return.
    pub limit: Option<usize>,
    /// Records to skip (for pagination).
    pub offset: Option<usize>,
    /// Sort order.
    pub sort: SortOrder,
}

/// Time range for queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRange {
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Sort order for query results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// Newest first.
    #[default]
    TimestampDesc,
    /// Oldest first.
    TimestampAsc,
}

/// Export format for forensic data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    /// Pretty JSON.
    Json,
    /// Newline-delimited JSON.
    Jsonl,
    /// Comma-separated values.
    Csv,
}

impl fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json => write!(f, "json"),
            Self::Jsonl => write!(f, "jsonl"),
            Self::Csv => write!(f, "csv"),
        }
    }
}

/// Result of a forensic query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForensicQueryResult {
    /// Matching records.
    pub records: Vec<ForensicRecord>,
    /// Total matching records (before pagination).
    pub total_count: usize,
    /// Whether there are more records.
    pub has_more: bool,
    /// Query execution time in microseconds.
    pub query_time_us: u64,
}

// =============================================================================
// Forensic store — bounded in-memory store with query engine
// =============================================================================

/// Bounded in-memory forensic record store.
pub struct ForensicStore {
    records: Vec<ForensicRecord>,
    max_records: usize,
    telemetry: ForensicTelemetry,
}

/// Telemetry for the forensic store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ForensicTelemetry {
    pub records_ingested: u64,
    pub records_evicted: u64,
    pub queries_executed: u64,
    pub exports_completed: u64,
    pub records_redacted: u64,
}

/// Snapshot of forensic store telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForensicTelemetrySnapshot {
    pub captured_at_ms: u64,
    pub counters: ForensicTelemetry,
    pub current_record_count: usize,
    pub max_records: usize,
}

impl ForensicStore {
    /// Create a new forensic store with the given capacity.
    pub fn new(max_records: usize) -> Self {
        Self {
            records: Vec::new(),
            max_records: max_records.max(1),
            telemetry: ForensicTelemetry::default(),
        }
    }

    /// Ingest a forensic record.
    pub fn ingest(&mut self, record: ForensicRecord) {
        if self.records.len() >= self.max_records {
            self.records.remove(0);
            self.telemetry.records_evicted += 1;
        }
        self.records.push(record);
        self.telemetry.records_ingested += 1;
    }

    /// Number of records currently in the store.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Query the store with the given filter criteria.
    pub fn query(&mut self, q: &ForensicQuery) -> ForensicQueryResult {
        self.telemetry.queries_executed += 1;

        let mut matching: Vec<&ForensicRecord> = self
            .records
            .iter()
            .filter(|r| Self::matches_query(r, q))
            .collect();

        let total_count = matching.len();

        // Sort
        match q.sort {
            SortOrder::TimestampDesc => matching.sort_by_key(|b| std::cmp::Reverse(b.timestamp_ms)),
            SortOrder::TimestampAsc => matching.sort_by_key(|a| a.timestamp_ms),
        }

        // Pagination
        let offset = q.offset.unwrap_or(0);
        let limit = q.limit.unwrap_or(usize::MAX);

        let records: Vec<ForensicRecord> = matching
            .into_iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect();

        let has_more = offset + records.len() < total_count;

        ForensicQueryResult {
            records,
            total_count,
            has_more,
            query_time_us: 0, // no real timing in non-async context
        }
    }

    /// Export records matching the query in the given format.
    pub fn export(&mut self, q: &ForensicQuery, format: ExportFormat) -> String {
        let result = self.query(q);
        self.telemetry.exports_completed += 1;

        match format {
            ExportFormat::Json => {
                serde_json::to_string_pretty(&result.records).unwrap_or_default()
            }
            ExportFormat::Jsonl => result
                .records
                .iter()
                .filter_map(|r| serde_json::to_string(r).ok())
                .collect::<Vec<_>>()
                .join("\n"),
            ExportFormat::Csv => self.export_csv(&result.records),
        }
    }

    /// Redact records above the given sensitivity threshold.
    pub fn redact_above(&mut self, threshold: SensitivityLevel) -> usize {
        let mut count = 0;
        for record in &mut self.records {
            if record.sensitivity > threshold {
                record.metadata.clear();
                record.correlation = CorrelationIds::default();
                count += 1;
            }
        }
        self.telemetry.records_redacted += count as u64;
        count
    }

    /// Get a telemetry snapshot.
    pub fn telemetry_snapshot(&self, now_ms: u64) -> ForensicTelemetrySnapshot {
        ForensicTelemetrySnapshot {
            captured_at_ms: now_ms,
            counters: self.telemetry.clone(),
            current_record_count: self.records.len(),
            max_records: self.max_records,
        }
    }

    fn matches_query(record: &ForensicRecord, q: &ForensicQuery) -> bool {
        // Time range filter
        if let Some(ref range) = q.time_range {
            if record.timestamp_ms < range.start_ms || record.timestamp_ms > range.end_ms {
                return false;
            }
        }

        // Actor filter (substring match on display)
        if let Some(ref actor_f) = q.actor_filter {
            if !record.actor.to_string().contains(actor_f.as_str()) {
                return false;
            }
        }

        // Action filter (substring match on display)
        if let Some(ref action_f) = q.action_filter {
            if !record.action.to_string().contains(action_f.as_str()) {
                return false;
            }
        }

        // Verdict filter
        if let Some(verdict_f) = q.verdict_filter {
            if record.policy_decision.decision != verdict_f {
                return false;
            }
        }

        // Outcome filter (substring match)
        if let Some(ref outcome_f) = q.outcome_filter {
            if !record.outcome.to_string().contains(outcome_f.as_str()) {
                return false;
            }
        }

        // Minimum sensitivity
        if let Some(min_sens) = q.min_sensitivity {
            if record.sensitivity < min_sens {
                return false;
            }
        }

        // Trace ID correlation
        if let Some(ref tid) = q.trace_id {
            if record.correlation.trace_id.as_deref() != Some(tid.as_str()) {
                return false;
            }
        }

        // Workflow ID correlation
        if let Some(ref wid) = q.workflow_id {
            if record.correlation.workflow_id.as_deref() != Some(wid.as_str()) {
                return false;
            }
        }

        // Text search in metadata values
        if let Some(ref text) = q.text_search {
            let found = record
                .metadata
                .values()
                .any(|v| v.contains(text.as_str()));
            if !found {
                return false;
            }
        }

        true
    }

    #[allow(clippy::unused_self)]
    fn export_csv(&self, records: &[ForensicRecord]) -> String {
        let mut out =
            String::from("record_id,timestamp_ms,actor,action,verdict,outcome,sensitivity\n");
        for r in records {
            out.push_str(&format!(
                "{},{},{},{},{},{},{}\n",
                r.record_id,
                r.timestamp_ms,
                r.actor,
                r.action,
                r.policy_decision.decision,
                r.outcome,
                r.sensitivity,
            ));
        }
        out
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(id: &str, ts: u64, sensitivity: SensitivityLevel) -> ForensicRecord {
        ForensicRecord {
            record_id: id.to_string(),
            timestamp_ms: ts,
            actor: ForensicActor::Agent {
                agent_id: "agent-1".to_string(),
                model: "test".to_string(),
            },
            action: ForensicAction::PaneWrite {
                pane_id: "p1".to_string(),
                command_summary: "ls -la".to_string(),
            },
            target: ForensicTarget::Pane {
                pane_id: "p1".to_string(),
                workspace: "default".to_string(),
            },
            policy_decision: ForensicPolicyDecision {
                decision: PolicyVerdict::Allow,
                matched_rules: vec!["rule-1".to_string()],
                surface: "pane_write".to_string(),
                reason: "matched allow rule".to_string(),
            },
            outcome: ForensicOutcome::Success,
            correlation: CorrelationIds {
                trace_id: Some("trace-1".to_string()),
                span_id: Some("span-1".to_string()),
                session_id: Some("sess-1".to_string()),
                workflow_id: None,
                transaction_id: None,
            },
            sensitivity,
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn ingest_and_query_basic() {
        let mut store = ForensicStore::new(100);
        store.ingest(make_record("r1", 1000, SensitivityLevel::Public));
        store.ingest(make_record("r2", 2000, SensitivityLevel::Internal));

        let result = store.query(&ForensicQuery::default());
        assert_eq!(result.total_count, 2);
        assert_eq!(result.records.len(), 2);
        // Default sort is desc
        assert_eq!(result.records[0].record_id, "r2");
        assert_eq!(result.records[1].record_id, "r1");
    }

    #[test]
    fn eviction_on_capacity() {
        let mut store = ForensicStore::new(3);
        for i in 0..5 {
            store.ingest(make_record(&format!("r{i}"), i * 1000, SensitivityLevel::Public));
        }
        assert_eq!(store.len(), 3);
        let result = store.query(&ForensicQuery::default());
        assert_eq!(result.records[0].record_id, "r4");
        assert_eq!(result.records[2].record_id, "r2");
        let snap = store.telemetry_snapshot(5000);
        assert_eq!(snap.counters.records_ingested, 5);
        assert_eq!(snap.counters.records_evicted, 2);
    }

    #[test]
    fn time_range_filter() {
        let mut store = ForensicStore::new(100);
        store.ingest(make_record("r1", 1000, SensitivityLevel::Public));
        store.ingest(make_record("r2", 2000, SensitivityLevel::Public));
        store.ingest(make_record("r3", 3000, SensitivityLevel::Public));

        let result = store.query(&ForensicQuery {
            time_range: Some(TimeRange {
                start_ms: 1500,
                end_ms: 2500,
            }),
            ..Default::default()
        });
        assert_eq!(result.total_count, 1);
        assert_eq!(result.records[0].record_id, "r2");
    }

    #[test]
    fn actor_filter() {
        let mut store = ForensicStore::new(100);
        store.ingest(make_record("r1", 1000, SensitivityLevel::Public));

        let mut r2 = make_record("r2", 2000, SensitivityLevel::Public);
        r2.actor = ForensicActor::Operator {
            operator_id: "bob".to_string(),
            session_id: "s1".to_string(),
        };
        store.ingest(r2);

        let result = store.query(&ForensicQuery {
            actor_filter: Some("bob".to_string()),
            ..Default::default()
        });
        assert_eq!(result.total_count, 1);
        assert_eq!(result.records[0].record_id, "r2");
    }

    #[test]
    fn verdict_filter() {
        let mut store = ForensicStore::new(100);
        store.ingest(make_record("r1", 1000, SensitivityLevel::Public));

        let mut r2 = make_record("r2", 2000, SensitivityLevel::Public);
        r2.policy_decision.decision = PolicyVerdict::Deny;
        store.ingest(r2);

        let result = store.query(&ForensicQuery {
            verdict_filter: Some(PolicyVerdict::Deny),
            ..Default::default()
        });
        assert_eq!(result.total_count, 1);
        assert_eq!(result.records[0].record_id, "r2");
    }

    #[test]
    fn pagination() {
        let mut store = ForensicStore::new(100);
        for i in 0..10 {
            store.ingest(make_record(&format!("r{i}"), i * 1000, SensitivityLevel::Public));
        }

        let page1 = store.query(&ForensicQuery {
            limit: Some(3),
            offset: Some(0),
            sort: SortOrder::TimestampAsc,
            ..Default::default()
        });
        assert_eq!(page1.total_count, 10);
        assert_eq!(page1.records.len(), 3);
        assert!(page1.has_more);
        assert_eq!(page1.records[0].record_id, "r0");

        let page2 = store.query(&ForensicQuery {
            limit: Some(3),
            offset: Some(3),
            sort: SortOrder::TimestampAsc,
            ..Default::default()
        });
        assert_eq!(page2.records[0].record_id, "r3");
        assert!(page2.has_more);
    }

    #[test]
    fn sensitivity_filter() {
        let mut store = ForensicStore::new(100);
        store.ingest(make_record("r1", 1000, SensitivityLevel::Public));
        store.ingest(make_record("r2", 2000, SensitivityLevel::Confidential));
        store.ingest(make_record("r3", 3000, SensitivityLevel::Restricted));

        let result = store.query(&ForensicQuery {
            min_sensitivity: Some(SensitivityLevel::Confidential),
            ..Default::default()
        });
        assert_eq!(result.total_count, 2);
    }

    #[test]
    fn trace_id_filter() {
        let mut store = ForensicStore::new(100);
        store.ingest(make_record("r1", 1000, SensitivityLevel::Public));

        let mut r2 = make_record("r2", 2000, SensitivityLevel::Public);
        r2.correlation.trace_id = Some("special-trace".to_string());
        store.ingest(r2);

        let result = store.query(&ForensicQuery {
            trace_id: Some("special-trace".to_string()),
            ..Default::default()
        });
        assert_eq!(result.total_count, 1);
        assert_eq!(result.records[0].record_id, "r2");
    }

    #[test]
    fn text_search_in_metadata() {
        let mut store = ForensicStore::new(100);
        let mut r1 = make_record("r1", 1000, SensitivityLevel::Public);
        r1.metadata.insert("command".to_string(), "rm -rf /".to_string());
        store.ingest(r1);

        store.ingest(make_record("r2", 2000, SensitivityLevel::Public));

        let result = store.query(&ForensicQuery {
            text_search: Some("rm -rf".to_string()),
            ..Default::default()
        });
        assert_eq!(result.total_count, 1);
        assert_eq!(result.records[0].record_id, "r1");
    }

    #[test]
    fn redact_above_threshold() {
        let mut store = ForensicStore::new(100);
        let mut r1 = make_record("r1", 1000, SensitivityLevel::Confidential);
        r1.metadata.insert("secret".to_string(), "value".to_string());
        store.ingest(r1);

        store.ingest(make_record("r2", 2000, SensitivityLevel::Public));

        let count = store.redact_above(SensitivityLevel::Internal);
        assert_eq!(count, 1);

        let result = store.query(&ForensicQuery::default());
        let r1 = result.records.iter().find(|r| r.record_id == "r1").unwrap();
        assert!(r1.metadata.is_empty());
        assert_eq!(r1.correlation, CorrelationIds::default());
    }

    #[test]
    fn export_json() {
        let mut store = ForensicStore::new(100);
        store.ingest(make_record("r1", 1000, SensitivityLevel::Public));

        let json = store.export(&ForensicQuery::default(), ExportFormat::Json);
        assert!(json.contains("r1"));
        let parsed: Vec<ForensicRecord> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn export_jsonl() {
        let mut store = ForensicStore::new(100);
        store.ingest(make_record("r1", 1000, SensitivityLevel::Public));
        store.ingest(make_record("r2", 2000, SensitivityLevel::Public));

        let jsonl = store.export(&ForensicQuery::default(), ExportFormat::Jsonl);
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let _: ForensicRecord = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn export_csv() {
        let mut store = ForensicStore::new(100);
        store.ingest(make_record("r1", 1000, SensitivityLevel::Public));

        let csv = store.export(&ForensicQuery::default(), ExportFormat::Csv);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "record_id,timestamp_ms,actor,action,verdict,outcome,sensitivity");
        assert!(lines[1].starts_with("r1,1000,"));
    }

    #[test]
    fn empty_store_query() {
        let mut store = ForensicStore::new(100);
        let result = store.query(&ForensicQuery::default());
        assert_eq!(result.total_count, 0);
        assert!(result.records.is_empty());
        assert!(!result.has_more);
    }

    #[test]
    fn actor_display() {
        assert_eq!(
            ForensicActor::Operator {
                operator_id: "bob".to_string(),
                session_id: "s1".to_string(),
            }
            .to_string(),
            "operator:bob"
        );
        assert_eq!(
            ForensicActor::Agent {
                agent_id: "a1".to_string(),
                model: "test".to_string(),
            }
            .to_string(),
            "agent:a1"
        );
        assert_eq!(
            ForensicActor::System {
                subsystem: "policy".to_string(),
            }
            .to_string(),
            "system:policy"
        );
        assert_eq!(
            ForensicActor::Connector {
                connector_id: "c1".to_string(),
                provider: "slack".to_string(),
            }
            .to_string(),
            "connector:c1"
        );
    }

    #[test]
    fn outcome_display() {
        assert_eq!(ForensicOutcome::Success.to_string(), "success");
        assert_eq!(
            ForensicOutcome::Failed {
                error: "oops".to_string()
            }
            .to_string(),
            "failed: oops"
        );
        assert_eq!(
            ForensicOutcome::Denied {
                reason: "policy".to_string()
            }
            .to_string(),
            "denied: policy"
        );
        assert_eq!(ForensicOutcome::Timeout.to_string(), "timeout");
    }

    #[test]
    fn sensitivity_ordering() {
        assert!(SensitivityLevel::Public < SensitivityLevel::Internal);
        assert!(SensitivityLevel::Internal < SensitivityLevel::Confidential);
        assert!(SensitivityLevel::Confidential < SensitivityLevel::Restricted);
    }

    #[test]
    fn policy_verdict_serde_roundtrip() {
        for verdict in [
            PolicyVerdict::Allow,
            PolicyVerdict::Deny,
            PolicyVerdict::AllowWithFlag,
            PolicyVerdict::NoMatch,
        ] {
            let json = serde_json::to_string(&verdict).unwrap();
            let back: PolicyVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(verdict, back);
        }
    }

    #[test]
    fn export_format_display() {
        assert_eq!(ExportFormat::Json.to_string(), "json");
        assert_eq!(ExportFormat::Jsonl.to_string(), "jsonl");
        assert_eq!(ExportFormat::Csv.to_string(), "csv");
    }

    #[test]
    fn sort_order_serde() {
        for order in [SortOrder::TimestampAsc, SortOrder::TimestampDesc] {
            let json = serde_json::to_string(&order).unwrap();
            let back: SortOrder = serde_json::from_str(&json).unwrap();
            assert_eq!(order, back);
        }
    }

    #[test]
    fn combined_filters() {
        let mut store = ForensicStore::new(100);

        let mut r1 = make_record("r1", 1000, SensitivityLevel::Confidential);
        r1.policy_decision.decision = PolicyVerdict::Deny;
        store.ingest(r1);

        let mut r2 = make_record("r2", 2000, SensitivityLevel::Public);
        r2.policy_decision.decision = PolicyVerdict::Deny;
        store.ingest(r2);

        let mut r3 = make_record("r3", 3000, SensitivityLevel::Confidential);
        r3.policy_decision.decision = PolicyVerdict::Allow;
        store.ingest(r3);

        // Only denied + confidential+
        let result = store.query(&ForensicQuery {
            verdict_filter: Some(PolicyVerdict::Deny),
            min_sensitivity: Some(SensitivityLevel::Confidential),
            ..Default::default()
        });
        assert_eq!(result.total_count, 1);
        assert_eq!(result.records[0].record_id, "r1");
    }

    #[test]
    fn telemetry_snapshot_accurate() {
        let mut store = ForensicStore::new(5);
        for i in 0..7 {
            store.ingest(make_record(&format!("r{i}"), i * 1000, SensitivityLevel::Public));
        }
        store.query(&ForensicQuery::default());
        store.query(&ForensicQuery::default());
        store.export(&ForensicQuery::default(), ExportFormat::Json);
        store.redact_above(SensitivityLevel::Restricted);

        let snap = store.telemetry_snapshot(10000);
        assert_eq!(snap.counters.records_ingested, 7);
        assert_eq!(snap.counters.records_evicted, 2);
        assert_eq!(snap.counters.queries_executed, 3); // 2 manual + 1 from export
        assert_eq!(snap.counters.exports_completed, 1);
        assert_eq!(snap.current_record_count, 5);
        assert_eq!(snap.max_records, 5);
    }

    #[test]
    fn workflow_id_filter() {
        let mut store = ForensicStore::new(100);
        let mut r1 = make_record("r1", 1000, SensitivityLevel::Public);
        r1.correlation.workflow_id = Some("wf-deploy".to_string());
        store.ingest(r1);
        store.ingest(make_record("r2", 2000, SensitivityLevel::Public));

        let result = store.query(&ForensicQuery {
            workflow_id: Some("wf-deploy".to_string()),
            ..Default::default()
        });
        assert_eq!(result.total_count, 1);
        assert_eq!(result.records[0].record_id, "r1");
    }

    #[test]
    fn forensic_record_full_serde_roundtrip() {
        let mut metadata = BTreeMap::new();
        metadata.insert("key".to_string(), "value".to_string());

        let record = ForensicRecord {
            record_id: "r1".to_string(),
            timestamp_ms: 42000,
            actor: ForensicActor::Agent {
                agent_id: "a1".to_string(),
                model: "gpt-test".to_string(),
            },
            action: ForensicAction::WorkflowLifecycle {
                workflow_id: "wf1".to_string(),
                transition: "start".to_string(),
            },
            target: ForensicTarget::Workflow {
                workflow_id: "wf1".to_string(),
            },
            policy_decision: ForensicPolicyDecision {
                decision: PolicyVerdict::AllowWithFlag,
                matched_rules: vec!["flag-rule".to_string()],
                surface: "workflow_start".to_string(),
                reason: "flagged for review".to_string(),
            },
            outcome: ForensicOutcome::RolledBack {
                reason: "post-check failed".to_string(),
            },
            correlation: CorrelationIds {
                trace_id: Some("t1".to_string()),
                span_id: Some("s1".to_string()),
                session_id: Some("sess1".to_string()),
                workflow_id: Some("wf1".to_string()),
                transaction_id: Some("tx1".to_string()),
            },
            sensitivity: SensitivityLevel::Confidential,
            metadata,
        };

        let json = serde_json::to_string(&record).unwrap();
        let back: ForensicRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn action_display() {
        assert_eq!(
            ForensicAction::PaneWrite {
                pane_id: "p1".to_string(),
                command_summary: "ls".to_string(),
            }
            .to_string(),
            "pane_write:p1"
        );
        assert_eq!(
            ForensicAction::KillSwitchChange {
                new_level: "emergency".to_string(),
            }
            .to_string(),
            "kill_switch:emergency"
        );
        assert_eq!(
            ForensicAction::Custom {
                category: "audit".to_string(),
                detail: "test".to_string(),
            }
            .to_string(),
            "custom:audit"
        );
    }
}
