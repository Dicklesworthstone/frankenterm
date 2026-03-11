//! Explainability console for orchestration, policy, and connector decisions (ft-3681t.9.7).
//!
//! Provides operator-facing surfaces that show **why** actions were chosen, blocked,
//! retried, or rolled back. Aggregates decision traces from the PolicyEngine decision
//! log, audit chain, connector governor, and workflow engine into a queryable,
//! correlated view for incident triage and trust-building.
//!
//! # Architecture
//!
//! ```text
//! PolicyDecisionLog ─┐
//! AuditChain ────────┤
//! ConnectorGovernor ──┼──► ExplainabilityConsole ──► DecisionTrace[]
//! WorkflowEngine ────┤                                  │
//! ExplanationTemplates┘                                  ▼
//!                                                   TraceRenderer
//!                                                   (human / json)
//! ```
//!
//! # Key types
//!
//! - [`DecisionTrace`]: Complete causal chain from trigger to outcome.
//! - [`TraceQuery`]: Filter parameters for querying traces.
//! - [`TraceResult`]: Paginated result set with summary statistics.
//! - [`ExplainabilityConsole`]: Main entry point aggregating all decision sources.
//! - [`CausalLink`]: Edge in the causal graph connecting related decisions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::policy::{ActionKind, ActorKind, PolicySurface};
use crate::policy_decision_log::DecisionOutcome;

// ── Decision Trace ──────────────────────────────────────────────────────────

/// A complete decision trace showing why an action was taken or blocked.
///
/// Traces are the primary explainability artifact: each trace captures the
/// full causal chain from the triggering event through rule evaluation,
/// policy checks, and final outcome with human-readable reasoning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionTrace {
    /// Unique trace ID (monotonic within the console).
    pub trace_id: u64,
    /// Unix timestamp (ms) when the decision was made.
    pub timestamp_ms: u64,
    /// The action that was evaluated.
    pub action: ActionKind,
    /// The actor who requested the action.
    pub actor: ActorKind,
    /// Subsystem surface where the request originated.
    pub surface: PolicySurface,
    /// Target pane ID (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// The final outcome.
    pub outcome: DecisionOutcome,
    /// Rule that determined the outcome (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// Human-readable reason for the decision.
    pub reason: String,
    /// Number of rules evaluated before reaching this decision.
    pub rules_evaluated: u32,
    /// Explanation template ID (if one matched).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation_id: Option<String>,
    /// Structured context data (action-specific details).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub context: HashMap<String, String>,
    /// Causal links to related decisions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causal_links: Vec<CausalLink>,
    /// Correlation ID for grouping related traces.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Source subsystem that produced this trace.
    pub source: TraceSource,
    /// Severity assessment of this decision.
    pub severity: TraceSeverity,
}

/// Where the trace originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceSource {
    /// PolicyEngine decision log.
    Policy,
    /// Audit chain event.
    Audit,
    /// Connector governor routing decision.
    Connector,
    /// Workflow engine step decision.
    Workflow,
    /// Command guard evaluation.
    CommandGuard,
    /// Rate limiter throttle.
    RateLimiter,
    /// Quarantine enforcement.
    Quarantine,
}

/// Severity assessment of a decision trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceSeverity {
    /// Normal operation, informational.
    Info,
    /// Decision warrants attention.
    Warning,
    /// Action was blocked or failed.
    Denied,
    /// Critical safety enforcement.
    Critical,
}

/// A causal link connecting two related decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalLink {
    /// The related trace ID.
    pub related_trace_id: u64,
    /// Nature of the relationship.
    pub relationship: CausalRelationship,
    /// Optional description of the causal connection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Nature of a causal relationship between traces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalRelationship {
    /// This trace was triggered by the related trace.
    TriggeredBy,
    /// This trace triggered the related trace.
    Triggered,
    /// This trace is a retry of the related trace.
    RetryOf,
    /// This trace overrides/supersedes the related trace.
    Overrides,
    /// This trace is the rollback/compensation of the related trace.
    CompensationOf,
    /// Related by correlation (same operation or workflow).
    Correlated,
}

// ── Trace Query ─────────────────────────────────────────────────────────────

/// Filter parameters for querying decision traces.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraceQuery {
    /// Filter by pane ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// Filter by action kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<ActionKind>,
    /// Filter by actor kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<ActorKind>,
    /// Filter by outcome.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<DecisionOutcome>,
    /// Filter by source subsystem.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<TraceSource>,
    /// Filter by minimum severity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_severity: Option<TraceSeverity>,
    /// Filter by correlation ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Filter by rule ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// Start of time range (epoch ms, inclusive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// End of time range (epoch ms, exclusive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    /// Maximum number of results to return.
    pub limit: usize,
    /// Offset for pagination.
    pub offset: usize,
}

impl TraceQuery {
    /// Create a query for all traces (up to limit).
    #[must_use]
    pub fn all(limit: usize) -> Self {
        Self {
            limit,
            ..Default::default()
        }
    }

    /// Create a query for a specific pane.
    #[must_use]
    pub fn for_pane(pane_id: u64, limit: usize) -> Self {
        Self {
            pane_id: Some(pane_id),
            limit,
            ..Default::default()
        }
    }

    /// Create a query for denied decisions only.
    #[must_use]
    pub fn denials(limit: usize) -> Self {
        Self {
            outcome: Some(DecisionOutcome::Deny),
            limit,
            ..Default::default()
        }
    }

    /// Create a query for a specific correlation ID.
    #[must_use]
    pub fn by_correlation(correlation_id: &str, limit: usize) -> Self {
        Self {
            correlation_id: Some(correlation_id.to_string()),
            limit,
            ..Default::default()
        }
    }

    /// Whether a trace matches this query's filters.
    #[must_use]
    pub fn matches(&self, trace: &DecisionTrace) -> bool {
        if let Some(pane_id) = self.pane_id {
            if trace.pane_id != Some(pane_id) {
                return false;
            }
        }
        if let Some(ref action) = self.action {
            if &trace.action != action {
                return false;
            }
        }
        if let Some(ref actor) = self.actor {
            if &trace.actor != actor {
                return false;
            }
        }
        if let Some(outcome) = self.outcome {
            if trace.outcome != outcome {
                return false;
            }
        }
        if let Some(source) = self.source {
            if trace.source != source {
                return false;
            }
        }
        if let Some(min_severity) = self.min_severity {
            if trace.severity < min_severity {
                return false;
            }
        }
        if let Some(ref corr) = self.correlation_id {
            if trace.correlation_id.as_ref() != Some(corr) {
                return false;
            }
        }
        if let Some(ref rule_id) = self.rule_id {
            if trace.rule_id.as_ref() != Some(rule_id) {
                return false;
            }
        }
        if let Some(since) = self.since_ms {
            if trace.timestamp_ms < since {
                return false;
            }
        }
        if let Some(until) = self.until_ms {
            if trace.timestamp_ms >= until {
                return false;
            }
        }
        true
    }
}

// ── Trace Result ────────────────────────────────────────────────────────────

/// Paginated result set from a trace query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceResult {
    /// Matching traces (paginated).
    pub traces: Vec<DecisionTrace>,
    /// Total number of matching traces (before pagination).
    pub total_count: usize,
    /// Summary statistics for the result set.
    pub summary: TraceSummary,
}

/// Summary statistics for a trace query result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraceSummary {
    /// Count by outcome.
    pub by_outcome: HashMap<String, usize>,
    /// Count by source.
    pub by_source: HashMap<String, usize>,
    /// Count by severity.
    pub by_severity: HashMap<String, usize>,
    /// Unique pane IDs involved.
    pub pane_ids: Vec<u64>,
    /// Unique rule IDs that triggered decisions.
    pub rule_ids: Vec<String>,
    /// Time range of results.
    pub earliest_ms: Option<u64>,
    pub latest_ms: Option<u64>,
}

// ── Explainability Console ──────────────────────────────────────────────────

/// Main entry point for the explainability system.
///
/// Collects decision traces from multiple subsystems and provides
/// queryable, correlated views for operators and automation clients.
pub struct ExplainabilityConsole {
    /// All collected traces, ordered by trace_id.
    traces: Vec<DecisionTrace>,
    /// Next trace ID to assign.
    next_trace_id: u64,
    /// Maximum traces to retain.
    capacity: usize,
    /// Index: correlation_id → trace indices.
    correlation_index: HashMap<String, Vec<usize>>,
    /// Index: pane_id → trace indices.
    pane_index: HashMap<u64, Vec<usize>>,
    /// Telemetry counters.
    telemetry: ConsoleTelemetry,
}

/// Telemetry counters for the explainability console.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConsoleTelemetry {
    /// Total traces ingested.
    pub traces_ingested: u64,
    /// Total traces evicted (capacity).
    pub traces_evicted: u64,
    /// Total queries executed.
    pub queries_executed: u64,
    /// Total traces matched across all queries.
    pub traces_matched: u64,
}

impl ExplainabilityConsole {
    /// Create a new console with the given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            traces: Vec::new(),
            next_trace_id: 1,
            capacity: capacity.max(1),
            correlation_index: HashMap::new(),
            pane_index: HashMap::new(),
            telemetry: ConsoleTelemetry::default(),
        }
    }

    /// Ingest a new decision trace from any subsystem.
    ///
    /// Assigns a trace_id, updates indices, and evicts oldest traces
    /// if capacity is exceeded. Returns the assigned trace_id.
    pub fn ingest(&mut self, mut trace: DecisionTrace) -> u64 {
        let trace_id = self.next_trace_id;
        self.next_trace_id += 1;
        trace.trace_id = trace_id;

        // Update indices
        let idx = self.traces.len();
        if let Some(ref corr) = trace.correlation_id {
            self.correlation_index
                .entry(corr.clone())
                .or_default()
                .push(idx);
        }
        if let Some(pane_id) = trace.pane_id {
            self.pane_index.entry(pane_id).or_default().push(idx);
        }

        self.traces.push(trace);
        self.telemetry.traces_ingested += 1;

        // Evict oldest if over capacity
        while self.traces.len() > self.capacity {
            self.traces.remove(0);
            self.telemetry.traces_evicted += 1;
            // Rebuild indices after removal (indices shifted)
            self.rebuild_indices();
        }

        trace_id
    }

    /// Ingest a trace built from a policy decision log entry.
    pub fn ingest_policy_decision(
        &mut self,
        action: ActionKind,
        actor: ActorKind,
        surface: PolicySurface,
        pane_id: Option<u64>,
        outcome: DecisionOutcome,
        rule_id: Option<String>,
        reason: String,
        rules_evaluated: u32,
        timestamp_ms: u64,
        correlation_id: Option<String>,
    ) -> u64 {
        let severity = match outcome {
            DecisionOutcome::Deny => TraceSeverity::Denied,
            DecisionOutcome::RequireApproval => TraceSeverity::Warning,
            DecisionOutcome::Allow => TraceSeverity::Info,
        };

        let trace = DecisionTrace {
            trace_id: 0, // assigned by ingest
            timestamp_ms,
            action,
            actor,
            surface,
            pane_id,
            outcome,
            rule_id,
            reason,
            rules_evaluated,
            explanation_id: None,
            context: HashMap::new(),
            causal_links: Vec::new(),
            correlation_id,
            source: TraceSource::Policy,
            severity,
        };

        self.ingest(trace)
    }

    /// Ingest a connector routing decision trace.
    pub fn ingest_connector_decision(
        &mut self,
        connector_id: &str,
        action: ActionKind,
        outcome: DecisionOutcome,
        reason: String,
        timestamp_ms: u64,
    ) -> u64 {
        let mut context = HashMap::new();
        context.insert("connector_id".to_string(), connector_id.to_string());

        let trace = DecisionTrace {
            trace_id: 0,
            timestamp_ms,
            action,
            actor: ActorKind::Robot,
            surface: PolicySurface::Connector,
            pane_id: None,
            outcome,
            rule_id: None,
            reason,
            rules_evaluated: 0,
            explanation_id: None,
            context,
            causal_links: Vec::new(),
            correlation_id: None,
            source: TraceSource::Connector,
            severity: if outcome == DecisionOutcome::Deny {
                TraceSeverity::Denied
            } else {
                TraceSeverity::Info
            },
        };

        self.ingest(trace)
    }

    /// Query traces with the given filter.
    pub fn query(&mut self, query: &TraceQuery) -> TraceResult {
        self.telemetry.queries_executed += 1;

        let matching: Vec<&DecisionTrace> = self
            .traces
            .iter()
            .filter(|t| query.matches(t))
            .collect();

        let total_count = matching.len();
        self.telemetry.traces_matched += total_count as u64;

        // Build summary
        let summary = Self::build_summary(&matching);

        // Apply pagination
        let traces: Vec<DecisionTrace> = matching
            .into_iter()
            .skip(query.offset)
            .take(if query.limit == 0 { usize::MAX } else { query.limit })
            .cloned()
            .collect();

        TraceResult {
            traces,
            total_count,
            summary,
        }
    }

    /// Get a specific trace by ID.
    #[must_use]
    pub fn get_trace(&self, trace_id: u64) -> Option<&DecisionTrace> {
        self.traces.iter().find(|t| t.trace_id == trace_id)
    }

    /// Get all traces correlated to a given trace.
    #[must_use]
    pub fn get_correlated(&self, trace_id: u64) -> Vec<&DecisionTrace> {
        let trace = match self.get_trace(trace_id) {
            Some(t) => t,
            None => return Vec::new(),
        };

        let corr_id = match &trace.correlation_id {
            Some(c) => c,
            None => return Vec::new(),
        };

        self.traces
            .iter()
            .filter(|t| t.correlation_id.as_ref() == Some(corr_id) && t.trace_id != trace_id)
            .collect()
    }

    /// Link two traces with a causal relationship.
    pub fn link_traces(
        &mut self,
        from_id: u64,
        to_id: u64,
        relationship: CausalRelationship,
        description: Option<String>,
    ) -> bool {
        let from_idx = self.traces.iter().position(|t| t.trace_id == from_id);
        if let Some(idx) = from_idx {
            self.traces[idx].causal_links.push(CausalLink {
                related_trace_id: to_id,
                relationship,
                description,
            });
            return true;
        }
        false
    }

    /// Get the number of stored traces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.traces.len()
    }

    /// Whether the console has no traces.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.traces.is_empty()
    }

    /// Get the telemetry snapshot.
    #[must_use]
    pub fn telemetry(&self) -> &ConsoleTelemetry {
        &self.telemetry
    }

    /// Render a trace as a human-readable string.
    #[must_use]
    pub fn render_trace(trace: &DecisionTrace) -> String {
        let outcome_str = match trace.outcome {
            DecisionOutcome::Allow => "ALLOW",
            DecisionOutcome::Deny => "DENY",
            DecisionOutcome::RequireApproval => "REQUIRE_APPROVAL",
        };

        let rule_str = trace
            .rule_id
            .as_deref()
            .unwrap_or("(no rule)");

        let mut lines = vec![
            format!(
                "[{}] #{} {} {:?} → {} (rule: {})",
                trace.timestamp_ms,
                trace.trace_id,
                format!("{:?}", trace.source).to_lowercase(),
                trace.action,
                outcome_str,
                rule_str,
            ),
            format!("  reason: {}", trace.reason),
        ];

        if let Some(pane_id) = trace.pane_id {
            lines.push(format!("  pane: {pane_id}"));
        }

        if !trace.context.is_empty() {
            for (k, v) in &trace.context {
                lines.push(format!("  {k}: {v}"));
            }
        }

        if !trace.causal_links.is_empty() {
            for link in &trace.causal_links {
                lines.push(format!(
                    "  → {:?} trace #{}{}",
                    link.relationship,
                    link.related_trace_id,
                    link.description
                        .as_ref()
                        .map(|d| format!(" ({d})"))
                        .unwrap_or_default(),
                ));
            }
        }

        lines.join("\n")
    }

    /// Build summary statistics from a set of traces.
    fn build_summary(traces: &[&DecisionTrace]) -> TraceSummary {
        let mut summary = TraceSummary::default();
        let mut pane_set = std::collections::HashSet::new();
        let mut rule_set = std::collections::HashSet::new();

        for trace in traces {
            // By outcome
            let outcome_key = format!("{:?}", trace.outcome).to_lowercase();
            *summary.by_outcome.entry(outcome_key).or_insert(0) += 1;

            // By source
            let source_key = format!("{:?}", trace.source).to_lowercase();
            *summary.by_source.entry(source_key).or_insert(0) += 1;

            // By severity
            let severity_key = format!("{:?}", trace.severity).to_lowercase();
            *summary.by_severity.entry(severity_key).or_insert(0) += 1;

            // Pane IDs
            if let Some(pane_id) = trace.pane_id {
                pane_set.insert(pane_id);
            }

            // Rule IDs
            if let Some(ref rule_id) = trace.rule_id {
                rule_set.insert(rule_id.clone());
            }

            // Time range
            match summary.earliest_ms {
                None => summary.earliest_ms = Some(trace.timestamp_ms),
                Some(e) if trace.timestamp_ms < e => summary.earliest_ms = Some(trace.timestamp_ms),
                _ => {}
            }
            match summary.latest_ms {
                None => summary.latest_ms = Some(trace.timestamp_ms),
                Some(l) if trace.timestamp_ms > l => summary.latest_ms = Some(trace.timestamp_ms),
                _ => {}
            }
        }

        summary.pane_ids = pane_set.into_iter().collect();
        summary.pane_ids.sort_unstable();
        summary.rule_ids = rule_set.into_iter().collect();
        summary.rule_ids.sort();

        summary
    }

    /// Rebuild all indices (after eviction).
    fn rebuild_indices(&mut self) {
        self.correlation_index.clear();
        self.pane_index.clear();

        for (idx, trace) in self.traces.iter().enumerate() {
            if let Some(ref corr) = trace.correlation_id {
                self.correlation_index
                    .entry(corr.clone())
                    .or_default()
                    .push(idx);
            }
            if let Some(pane_id) = trace.pane_id {
                self.pane_index.entry(pane_id).or_default().push(idx);
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_trace(
        action: ActionKind,
        outcome: DecisionOutcome,
        source: TraceSource,
        pane_id: Option<u64>,
        timestamp_ms: u64,
    ) -> DecisionTrace {
        DecisionTrace {
            trace_id: 0,
            timestamp_ms,
            action,
            actor: ActorKind::Robot,
            surface: PolicySurface::Robot,
            pane_id,
            outcome,
            rule_id: None,
            reason: "test reason".to_string(),
            rules_evaluated: 1,
            explanation_id: None,
            context: HashMap::new(),
            causal_links: Vec::new(),
            correlation_id: None,
            source,
            severity: TraceSeverity::Info,
        }
    }

    // -- Console basics --

    #[test]
    fn console_empty_initially() {
        let console = ExplainabilityConsole::new(100);
        assert!(console.is_empty());
        assert_eq!(console.len(), 0);
    }

    #[test]
    fn console_ingest_assigns_trace_id() {
        let mut console = ExplainabilityConsole::new(100);
        let trace = make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, Some(1), 1000);
        let id = console.ingest(trace);
        assert_eq!(id, 1);
        assert_eq!(console.len(), 1);

        let trace2 = make_trace(ActionKind::SendText, DecisionOutcome::Deny, TraceSource::Policy, Some(2), 1001);
        let id2 = console.ingest(trace2);
        assert_eq!(id2, 2);
    }

    #[test]
    fn console_capacity_eviction() {
        let mut console = ExplainabilityConsole::new(3);
        for i in 0..5 {
            let trace = make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, Some(i as u64), 1000 + i);
            console.ingest(trace);
        }
        assert_eq!(console.len(), 3);
        // Oldest traces should be evicted
        assert!(console.get_trace(1).is_none());
        assert!(console.get_trace(2).is_none());
        assert!(console.get_trace(3).is_some());
    }

    #[test]
    fn console_get_trace_by_id() {
        let mut console = ExplainabilityConsole::new(100);
        let trace = make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, Some(42), 1000);
        let id = console.ingest(trace);
        let found = console.get_trace(id).unwrap();
        assert_eq!(found.pane_id, Some(42));
    }

    #[test]
    fn console_get_trace_not_found() {
        let console = ExplainabilityConsole::new(100);
        assert!(console.get_trace(999).is_none());
    }

    // -- Query tests --

    #[test]
    fn query_all() {
        let mut console = ExplainabilityConsole::new(100);
        for i in 0..5 {
            let trace = make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, Some(i), 1000 + i);
            console.ingest(trace);
        }
        let result = console.query(&TraceQuery::all(10));
        assert_eq!(result.total_count, 5);
        assert_eq!(result.traces.len(), 5);
    }

    #[test]
    fn query_by_pane() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, Some(1), 1000));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Deny, TraceSource::Policy, Some(2), 1001));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, Some(1), 1002));

        let result = console.query(&TraceQuery::for_pane(1, 10));
        assert_eq!(result.total_count, 2);
    }

    #[test]
    fn query_denials_only() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1000));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Deny, TraceSource::Policy, None, 1001));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Deny, TraceSource::Policy, None, 1002));

        let result = console.query(&TraceQuery::denials(10));
        assert_eq!(result.total_count, 2);
        assert!(result.traces.iter().all(|t| t.outcome == DecisionOutcome::Deny));
    }

    #[test]
    fn query_by_source() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1000));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Connector, None, 1001));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Workflow, None, 1002));

        let query = TraceQuery {
            source: Some(TraceSource::Connector),
            limit: 10,
            ..Default::default()
        };
        let result = console.query(&query);
        assert_eq!(result.total_count, 1);
    }

    #[test]
    fn query_by_time_range() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1000));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 2000));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 3000));

        let query = TraceQuery {
            since_ms: Some(1500),
            until_ms: Some(2500),
            limit: 10,
            ..Default::default()
        };
        let result = console.query(&query);
        assert_eq!(result.total_count, 1);
        assert_eq!(result.traces[0].timestamp_ms, 2000);
    }

    #[test]
    fn query_by_severity() {
        let mut console = ExplainabilityConsole::new(100);

        let mut t1 = make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1000);
        t1.severity = TraceSeverity::Info;
        console.ingest(t1);

        let mut t2 = make_trace(ActionKind::SendText, DecisionOutcome::Deny, TraceSource::Policy, None, 1001);
        t2.severity = TraceSeverity::Denied;
        console.ingest(t2);

        let mut t3 = make_trace(ActionKind::SendText, DecisionOutcome::Deny, TraceSource::Policy, None, 1002);
        t3.severity = TraceSeverity::Critical;
        console.ingest(t3);

        let query = TraceQuery {
            min_severity: Some(TraceSeverity::Denied),
            limit: 10,
            ..Default::default()
        };
        let result = console.query(&query);
        assert_eq!(result.total_count, 2);
    }

    #[test]
    fn query_pagination() {
        let mut console = ExplainabilityConsole::new(100);
        for i in 0..10 {
            console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1000 + i));
        }

        let query = TraceQuery {
            limit: 3,
            offset: 2,
            ..Default::default()
        };
        let result = console.query(&query);
        assert_eq!(result.total_count, 10);
        assert_eq!(result.traces.len(), 3);
        assert_eq!(result.traces[0].trace_id, 3); // offset=2 skips first two
    }

    // -- Correlation tests --

    #[test]
    fn correlation_groups_traces() {
        let mut console = ExplainabilityConsole::new(100);

        let mut t1 = make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1000);
        t1.correlation_id = Some("op-123".to_string());
        let id1 = console.ingest(t1);

        let mut t2 = make_trace(ActionKind::SendText, DecisionOutcome::Deny, TraceSource::Connector, None, 1001);
        t2.correlation_id = Some("op-123".to_string());
        console.ingest(t2);

        let mut t3 = make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1002);
        t3.correlation_id = Some("op-456".to_string());
        console.ingest(t3);

        let correlated = console.get_correlated(id1);
        assert_eq!(correlated.len(), 1);
        assert_eq!(correlated[0].source, TraceSource::Connector);
    }

    #[test]
    fn correlation_query() {
        let mut console = ExplainabilityConsole::new(100);

        let mut t1 = make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1000);
        t1.correlation_id = Some("op-123".to_string());
        console.ingest(t1);

        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1001));

        let result = console.query(&TraceQuery::by_correlation("op-123", 10));
        assert_eq!(result.total_count, 1);
    }

    // -- Causal link tests --

    #[test]
    fn link_traces_creates_causal_edge() {
        let mut console = ExplainabilityConsole::new(100);
        let id1 = console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Deny, TraceSource::Policy, None, 1000));
        let id2 = console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1001));

        let linked = console.link_traces(id2, id1, CausalRelationship::RetryOf, Some("retry after denial".into()));
        assert!(linked);

        let trace = console.get_trace(id2).unwrap();
        assert_eq!(trace.causal_links.len(), 1);
        assert_eq!(trace.causal_links[0].related_trace_id, id1);
        assert_eq!(trace.causal_links[0].relationship, CausalRelationship::RetryOf);
    }

    #[test]
    fn link_nonexistent_trace_returns_false() {
        let mut console = ExplainabilityConsole::new(100);
        assert!(!console.link_traces(999, 1, CausalRelationship::TriggeredBy, None));
    }

    // -- Convenience ingest methods --

    #[test]
    fn ingest_policy_decision_works() {
        let mut console = ExplainabilityConsole::new(100);
        let id = console.ingest_policy_decision(
            ActionKind::SendText,
            ActorKind::Robot,
            PolicySurface::Robot,
            Some(42),
            DecisionOutcome::Deny,
            Some("safety.alt_screen".into()),
            "Alt screen active".into(),
            3,
            1000,
            None,
        );
        let trace = console.get_trace(id).unwrap();
        assert_eq!(trace.source, TraceSource::Policy);
        assert_eq!(trace.severity, TraceSeverity::Denied);
        assert_eq!(trace.pane_id, Some(42));
    }

    #[test]
    fn ingest_connector_decision_works() {
        let mut console = ExplainabilityConsole::new(100);
        let id = console.ingest_connector_decision(
            "fcp.github",
            ActionKind::SendText,
            DecisionOutcome::Allow,
            "Connector healthy".into(),
            2000,
        );
        let trace = console.get_trace(id).unwrap();
        assert_eq!(trace.source, TraceSource::Connector);
        assert_eq!(trace.context.get("connector_id").unwrap(), "fcp.github");
    }

    // -- Summary tests --

    #[test]
    fn summary_counts_by_outcome() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1000));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1001));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Deny, TraceSource::Policy, None, 1002));

        let result = console.query(&TraceQuery::all(10));
        assert_eq!(result.summary.by_outcome.get("allow"), Some(&2));
        assert_eq!(result.summary.by_outcome.get("deny"), Some(&1));
    }

    #[test]
    fn summary_tracks_pane_ids() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, Some(1), 1000));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, Some(3), 1001));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, Some(1), 1002));

        let result = console.query(&TraceQuery::all(10));
        assert_eq!(result.summary.pane_ids, vec![1, 3]);
    }

    #[test]
    fn summary_time_range() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1000));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 3000));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 2000));

        let result = console.query(&TraceQuery::all(10));
        assert_eq!(result.summary.earliest_ms, Some(1000));
        assert_eq!(result.summary.latest_ms, Some(3000));
    }

    // -- Render tests --

    #[test]
    fn render_trace_contains_key_info() {
        let mut trace = make_trace(ActionKind::SendText, DecisionOutcome::Deny, TraceSource::Policy, Some(42), 1000);
        trace.trace_id = 7;
        trace.rule_id = Some("safety.alt_screen".into());
        trace.reason = "Alt screen is active".into();

        let rendered = ExplainabilityConsole::render_trace(&trace);
        assert!(rendered.contains("DENY"));
        assert!(rendered.contains("#7"));
        assert!(rendered.contains("safety.alt_screen"));
        assert!(rendered.contains("Alt screen is active"));
        assert!(rendered.contains("pane: 42"));
    }

    #[test]
    fn render_trace_shows_causal_links() {
        let mut trace = make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1000);
        trace.trace_id = 5;
        trace.causal_links.push(CausalLink {
            related_trace_id: 3,
            relationship: CausalRelationship::RetryOf,
            description: Some("retry after timeout".into()),
        });

        let rendered = ExplainabilityConsole::render_trace(&trace);
        assert!(rendered.contains("RetryOf"));
        assert!(rendered.contains("trace #3"));
        assert!(rendered.contains("retry after timeout"));
    }

    // -- Telemetry tests --

    #[test]
    fn telemetry_tracks_ingestion() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1000));
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1001));

        assert_eq!(console.telemetry().traces_ingested, 2);
    }

    #[test]
    fn telemetry_tracks_evictions() {
        let mut console = ExplainabilityConsole::new(2);
        for i in 0..5 {
            console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1000 + i));
        }
        assert_eq!(console.telemetry().traces_evicted, 3);
    }

    #[test]
    fn telemetry_tracks_queries() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(ActionKind::SendText, DecisionOutcome::Allow, TraceSource::Policy, None, 1000));
        let _ = console.query(&TraceQuery::all(10));
        let _ = console.query(&TraceQuery::all(10));
        assert_eq!(console.telemetry().queries_executed, 2);
        assert_eq!(console.telemetry().traces_matched, 2);
    }

    // -- Serde roundtrip tests --

    #[test]
    fn decision_trace_serde_roundtrip() {
        let mut trace = make_trace(ActionKind::SendText, DecisionOutcome::Deny, TraceSource::Policy, Some(42), 1000);
        trace.trace_id = 1;
        trace.rule_id = Some("test.rule".into());
        trace.context.insert("key".into(), "value".into());

        let json = serde_json::to_string(&trace).unwrap();
        let trace2: DecisionTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(trace2.trace_id, 1);
        assert_eq!(trace2.outcome, DecisionOutcome::Deny);
        assert_eq!(trace2.context.get("key").unwrap(), "value");
    }

    #[test]
    fn trace_query_serde_roundtrip() {
        let query = TraceQuery::denials(50);
        let json = serde_json::to_string(&query).unwrap();
        let query2: TraceQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(query2.outcome, Some(DecisionOutcome::Deny));
        assert_eq!(query2.limit, 50);
    }

    #[test]
    fn trace_result_serde_roundtrip() {
        let result = TraceResult {
            traces: Vec::new(),
            total_count: 0,
            summary: TraceSummary::default(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let result2: TraceResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result2.total_count, 0);
    }

    #[test]
    fn trace_severity_ordering() {
        assert!(TraceSeverity::Info < TraceSeverity::Warning);
        assert!(TraceSeverity::Warning < TraceSeverity::Denied);
        assert!(TraceSeverity::Denied < TraceSeverity::Critical);
    }

    #[test]
    fn causal_relationship_serde() {
        let link = CausalLink {
            related_trace_id: 5,
            relationship: CausalRelationship::CompensationOf,
            description: None,
        };
        let json = serde_json::to_string(&link).unwrap();
        assert!(json.contains("compensation_of"));
        let link2: CausalLink = serde_json::from_str(&json).unwrap();
        assert_eq!(link2.relationship, CausalRelationship::CompensationOf);
    }

    // -- Rule ID query test --

    #[test]
    fn query_by_rule_id() {
        let mut console = ExplainabilityConsole::new(100);
        let mut t1 = make_trace(ActionKind::SendText, DecisionOutcome::Deny, TraceSource::Policy, None, 1000);
        t1.rule_id = Some("safety.alt_screen".into());
        console.ingest(t1);

        let mut t2 = make_trace(ActionKind::SendText, DecisionOutcome::Deny, TraceSource::Policy, None, 1001);
        t2.rule_id = Some("safety.rate_limit".into());
        console.ingest(t2);

        let query = TraceQuery {
            rule_id: Some("safety.alt_screen".into()),
            limit: 10,
            ..Default::default()
        };
        let result = console.query(&query);
        assert_eq!(result.total_count, 1);
    }
}
