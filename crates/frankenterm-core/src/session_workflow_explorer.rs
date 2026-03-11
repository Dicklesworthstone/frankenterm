//! Session/workflow explorer with timeline replay and extraction tools (ft-3681t.9.3).
//!
//! Provides deep session/workflow exploration: unified timeline views,
//! diffing checkpoints, extracting key events/errors/actions, and linking
//! directly into robot/workflow interventions.
//!
//! # Architecture
//!
//! ```text
//! Recording/Replay ──┐
//! ContextSnapshots ───┤
//! DiffSnapshots ──────┼──► SessionWorkflowExplorer ──► TimelineResult
//! WorkflowEngine ─────┤                                    │
//! ExplainConsole ─────┘                                    ▼
//!                                                    EventExtraction
//!                                                    (errors/actions/interventions)
//! ```
//!
//! # Key types
//!
//! - [`TimelineEvent`]: Normalized event from any subsystem with causal context.
//! - [`TimelineQuery`]: Filter/search parameters for timeline exploration.
//! - [`TimelineResult`]: Paginated results with summary statistics.
//! - [`WorkflowTrace`]: Step-by-step workflow execution trace.
//! - [`SessionDiff`]: Comparison between two points in session time.
//! - [`EventExtraction`]: Extracted key events (errors, denials, interventions).
//! - [`SessionWorkflowExplorer`]: Main entry point aggregating all sources.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Timeline Events ─────────────────────────────────────────────────────────

/// A normalized event in the session timeline, regardless of original source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEvent {
    /// Unique event ID within the explorer.
    pub event_id: u64,
    /// Unix timestamp (ms) when the event occurred.
    pub timestamp_ms: u64,
    /// Pane ID associated with this event (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// Event category.
    pub category: EventCategory,
    /// Event severity.
    pub severity: EventSeverity,
    /// Human-readable summary.
    pub summary: String,
    /// Source subsystem.
    pub source: EventSource,
    /// Structured detail payload.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub details: HashMap<String, String>,
    /// Correlation ID for grouping related events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// References to related events.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_events: Vec<EventRef>,
    /// Workflow step name (if from a workflow execution).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_step: Option<String>,
    /// Whether this event represents an actionable intervention point.
    pub is_intervention_point: bool,
}

/// Category of a timeline event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventCategory {
    /// Output from a pane (truncated/summarized).
    Output,
    /// Command input to a pane.
    Input,
    /// Policy decision (allow/deny/require_approval).
    PolicyDecision,
    /// Workflow step execution.
    WorkflowStep,
    /// Workflow lifecycle (start/complete/abort).
    WorkflowLifecycle,
    /// Pattern detection match.
    PatternMatch,
    /// Environment snapshot captured.
    Snapshot,
    /// Error or failure.
    Error,
    /// Resize event.
    Resize,
    /// Marker/annotation.
    Marker,
    /// State change (pane created/closed/etc).
    StateChange,
    /// Intervention (robot action, approval, rollback).
    Intervention,
}

/// Severity of a timeline event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSeverity {
    /// Routine, informational.
    Trace,
    /// Normal operation.
    Info,
    /// Noteworthy but not problematic.
    Notice,
    /// Potential issue.
    Warning,
    /// Action blocked or failed.
    Error,
    /// Critical safety enforcement or system failure.
    Critical,
}

/// Source subsystem of a timeline event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    /// Pane recording/replay.
    Recording,
    /// Policy engine decision.
    Policy,
    /// Workflow engine.
    Workflow,
    /// Pattern detection engine.
    Patterns,
    /// Context snapshot system.
    ContextSnapshot,
    /// Differential snapshot system.
    DiffSnapshot,
    /// Explainability console.
    Explainability,
    /// Robot/automation action.
    Robot,
    /// Operator/human action.
    Operator,
    /// System internal.
    System,
}

/// Reference to a related event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRef {
    /// The related event ID.
    pub event_id: u64,
    /// Nature of the relationship.
    pub relationship: EventRelationship,
}

/// How two events are related.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventRelationship {
    /// This event caused the related event.
    Caused,
    /// This event was caused by the related event.
    CausedBy,
    /// This event is a retry of the related event.
    RetryOf,
    /// This event is the compensation/rollback of the related event.
    CompensationOf,
    /// Events are correlated by shared context.
    Correlated,
}

// ── Timeline Query ──────────────────────────────────────────────────────────

/// Filter parameters for querying the timeline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimelineQuery {
    /// Filter by pane ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// Filter by event category.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<EventCategory>,
    /// Filter by minimum severity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_severity: Option<EventSeverity>,
    /// Filter by source subsystem.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<EventSource>,
    /// Filter by correlation ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Text search in summary field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_text: Option<String>,
    /// Start of time range (epoch ms, inclusive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// End of time range (epoch ms, exclusive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    /// Only intervention points.
    pub interventions_only: bool,
    /// Maximum results.
    pub limit: usize,
    /// Offset for pagination.
    pub offset: usize,
}

impl TimelineQuery {
    /// Query all events (up to limit).
    #[must_use]
    pub fn all(limit: usize) -> Self {
        Self {
            limit,
            ..Default::default()
        }
    }

    /// Query events for a specific pane.
    #[must_use]
    pub fn for_pane(pane_id: u64, limit: usize) -> Self {
        Self {
            pane_id: Some(pane_id),
            limit,
            ..Default::default()
        }
    }

    /// Query errors only.
    #[must_use]
    pub fn errors(limit: usize) -> Self {
        Self {
            min_severity: Some(EventSeverity::Error),
            limit,
            ..Default::default()
        }
    }

    /// Query intervention points only.
    #[must_use]
    pub fn interventions(limit: usize) -> Self {
        Self {
            interventions_only: true,
            limit,
            ..Default::default()
        }
    }

    /// Query by correlation ID.
    #[must_use]
    pub fn by_correlation(correlation_id: &str, limit: usize) -> Self {
        Self {
            correlation_id: Some(correlation_id.to_string()),
            limit,
            ..Default::default()
        }
    }

    /// Query a time range.
    #[must_use]
    pub fn time_range(since_ms: u64, until_ms: u64, limit: usize) -> Self {
        Self {
            since_ms: Some(since_ms),
            until_ms: Some(until_ms),
            limit,
            ..Default::default()
        }
    }

    /// Text search across summaries.
    #[must_use]
    pub fn search(text: &str, limit: usize) -> Self {
        Self {
            search_text: Some(text.to_string()),
            limit,
            ..Default::default()
        }
    }

    /// Whether an event matches this query.
    #[must_use]
    pub fn matches(&self, event: &TimelineEvent) -> bool {
        if let Some(pane_id) = self.pane_id {
            if event.pane_id != Some(pane_id) {
                return false;
            }
        }
        if let Some(category) = self.category {
            if event.category != category {
                return false;
            }
        }
        if let Some(min_sev) = self.min_severity {
            if event.severity < min_sev {
                return false;
            }
        }
        if let Some(source) = self.source {
            if event.source != source {
                return false;
            }
        }
        if let Some(ref corr) = self.correlation_id {
            if event.correlation_id.as_ref() != Some(corr) {
                return false;
            }
        }
        if let Some(ref text) = self.search_text {
            let lower = text.to_lowercase();
            if !event.summary.to_lowercase().contains(&lower) {
                return false;
            }
        }
        if let Some(since) = self.since_ms {
            if event.timestamp_ms < since {
                return false;
            }
        }
        if let Some(until) = self.until_ms {
            if event.timestamp_ms >= until {
                return false;
            }
        }
        if self.interventions_only && !event.is_intervention_point {
            return false;
        }
        true
    }
}

// ── Timeline Result ─────────────────────────────────────────────────────────

/// Paginated result set from a timeline query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineResult {
    /// Matching events (paginated).
    pub events: Vec<TimelineEvent>,
    /// Total matching events (before pagination).
    pub total_count: usize,
    /// Summary statistics.
    pub summary: TimelineSummary,
}

/// Summary statistics for a timeline query.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimelineSummary {
    /// Count by category.
    pub by_category: HashMap<String, usize>,
    /// Count by source.
    pub by_source: HashMap<String, usize>,
    /// Count by severity.
    pub by_severity: HashMap<String, usize>,
    /// Unique pane IDs.
    pub pane_ids: Vec<u64>,
    /// Intervention point count.
    pub intervention_count: usize,
    /// Time range.
    pub earliest_ms: Option<u64>,
    pub latest_ms: Option<u64>,
}

// ── Workflow Trace ──────────────────────────────────────────────────────────

/// Step-by-step workflow execution trace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowTrace {
    /// Workflow execution ID.
    pub execution_id: String,
    /// Workflow name.
    pub workflow_name: String,
    /// Pane ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// Steps in execution order.
    pub steps: Vec<WorkflowStepTrace>,
    /// Overall status.
    pub status: WorkflowTraceStatus,
    /// Start timestamp.
    pub started_ms: u64,
    /// End timestamp (if completed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_ms: Option<u64>,
    /// Total duration (ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Correlation ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

/// Individual step within a workflow trace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStepTrace {
    /// Step name.
    pub step_name: String,
    /// Step index (0-based).
    pub step_index: u32,
    /// Step outcome.
    pub outcome: StepOutcome,
    /// Step start timestamp.
    pub started_ms: u64,
    /// Step end timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_ms: Option<u64>,
    /// Human-readable description of what happened.
    pub description: String,
    /// Policy decisions that affected this step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policy_decisions: Vec<StepDecisionRef>,
    /// Any errors encountered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Outcome of a workflow step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    /// Step completed successfully.
    Completed,
    /// Step is still running.
    Running,
    /// Step is waiting for a condition.
    Waiting,
    /// Step was retried.
    Retried,
    /// Step was aborted.
    Aborted,
    /// Step failed with error.
    Failed,
    /// Step was skipped.
    Skipped,
}

/// Overall workflow trace status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTraceStatus {
    /// Workflow is actively running.
    Running,
    /// Workflow completed successfully.
    Completed,
    /// Workflow was aborted.
    Aborted,
    /// Workflow failed.
    Failed,
    /// Workflow is waiting.
    Waiting,
}

/// Reference to a policy decision within a step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDecisionRef {
    /// Decision trace ID (from ExplainabilityConsole).
    pub trace_id: u64,
    /// Outcome summary.
    pub outcome: String,
    /// Reason.
    pub reason: String,
}

// ── Session Diff ────────────────────────────────────────────────────────────

/// Comparison between two points in session time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDiff {
    /// Timestamp of the baseline point.
    pub baseline_ms: u64,
    /// Timestamp of the comparison point.
    pub comparison_ms: u64,
    /// Pane-level changes.
    pub pane_changes: Vec<PaneChange>,
    /// Events that occurred between the two points.
    pub events_between: usize,
    /// Errors that occurred between the two points.
    pub errors_between: usize,
    /// Policy denials between the two points.
    pub denials_between: usize,
    /// Workflow steps completed between the two points.
    pub workflow_steps_between: usize,
}

/// Change to a pane between two time points.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneChange {
    /// Pane ID.
    pub pane_id: u64,
    /// Type of change.
    pub change_type: PaneChangeType,
    /// Human-readable description.
    pub description: String,
}

/// Type of pane change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneChangeType {
    /// Pane was created.
    Created,
    /// Pane was closed.
    Closed,
    /// Pane output changed.
    OutputChanged,
    /// Pane was resized.
    Resized,
    /// Pane title changed.
    TitleChanged,
}

// ── Event Extraction ────────────────────────────────────────────────────────

/// Extracted key events from a session, organized for incident triage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventExtraction {
    /// Errors and failures.
    pub errors: Vec<ExtractedEvent>,
    /// Policy denials.
    pub denials: Vec<ExtractedEvent>,
    /// Intervention actions (robot or operator).
    pub interventions: Vec<ExtractedEvent>,
    /// State changes (pane lifecycle, workflow transitions).
    pub state_changes: Vec<ExtractedEvent>,
    /// High-severity events.
    pub critical_events: Vec<ExtractedEvent>,
    /// Time range of extraction.
    pub time_range_ms: (u64, u64),
    /// Total events examined.
    pub total_examined: usize,
}

/// A single extracted event for triage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedEvent {
    /// Event ID.
    pub event_id: u64,
    /// Timestamp.
    pub timestamp_ms: u64,
    /// Pane ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// Summary.
    pub summary: String,
    /// Category.
    pub category: EventCategory,
    /// Severity.
    pub severity: EventSeverity,
}

impl From<&TimelineEvent> for ExtractedEvent {
    fn from(e: &TimelineEvent) -> Self {
        Self {
            event_id: e.event_id,
            timestamp_ms: e.timestamp_ms,
            pane_id: e.pane_id,
            summary: e.summary.clone(),
            category: e.category,
            severity: e.severity,
        }
    }
}

// ── Explorer ────────────────────────────────────────────────────────────────

/// Main entry point for session/workflow exploration.
///
/// Collects events from multiple subsystems into a unified timeline,
/// supports querying, diffing, and extraction for incident triage.
pub struct SessionWorkflowExplorer {
    /// All events, ordered by timestamp_ms then event_id.
    events: Vec<TimelineEvent>,
    /// Next event ID.
    next_event_id: u64,
    /// Maximum events to retain.
    capacity: usize,
    /// Workflow traces by execution ID.
    workflow_traces: HashMap<String, WorkflowTrace>,
    /// Index: correlation_id → event indices.
    correlation_index: HashMap<String, Vec<usize>>,
    /// Index: pane_id → event indices.
    pane_index: HashMap<u64, Vec<usize>>,
    /// Telemetry.
    telemetry: ExplorerTelemetry,
}

/// Telemetry counters for the explorer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExplorerTelemetry {
    /// Total events ingested.
    pub events_ingested: u64,
    /// Total events evicted.
    pub events_evicted: u64,
    /// Total queries executed.
    pub queries_executed: u64,
    /// Total extractions performed.
    pub extractions_performed: u64,
    /// Total diffs computed.
    pub diffs_computed: u64,
    /// Workflow traces tracked.
    pub workflow_traces_tracked: u64,
}

impl SessionWorkflowExplorer {
    /// Create a new explorer with the given event capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            events: Vec::new(),
            next_event_id: 1,
            capacity: capacity.max(1),
            workflow_traces: HashMap::new(),
            correlation_index: HashMap::new(),
            pane_index: HashMap::new(),
            telemetry: ExplorerTelemetry::default(),
        }
    }

    /// Ingest a new timeline event.
    ///
    /// Assigns an event_id, inserts in timestamp order, updates indices,
    /// and evicts oldest events if capacity is exceeded. Returns the assigned event_id.
    pub fn ingest(&mut self, mut event: TimelineEvent) -> u64 {
        let event_id = self.next_event_id;
        self.next_event_id += 1;
        event.event_id = event_id;

        self.events.push(event);
        self.telemetry.events_ingested += 1;

        // Update indices for the new event
        let idx = self.events.len() - 1;
        if let Some(ref corr) = self.events[idx].correlation_id {
            self.correlation_index
                .entry(corr.clone())
                .or_default()
                .push(idx);
        }
        if let Some(pane_id) = self.events[idx].pane_id {
            self.pane_index.entry(pane_id).or_default().push(idx);
        }

        // Evict if over capacity
        while self.events.len() > self.capacity {
            self.events.remove(0);
            self.telemetry.events_evicted += 1;
            self.rebuild_indices();
        }

        event_id
    }

    /// Convenience: ingest a policy decision event.
    pub fn ingest_policy_decision(
        &mut self,
        pane_id: Option<u64>,
        outcome: &str,
        reason: &str,
        rule_id: Option<&str>,
        timestamp_ms: u64,
        correlation_id: Option<String>,
    ) -> u64 {
        let severity = match outcome {
            "deny" | "Deny" => EventSeverity::Error,
            "require_approval" | "RequireApproval" => EventSeverity::Warning,
            _ => EventSeverity::Info,
        };

        let mut details = HashMap::new();
        details.insert("outcome".to_string(), outcome.to_string());
        if let Some(rid) = rule_id {
            details.insert("rule_id".to_string(), rid.to_string());
        }

        let event = TimelineEvent {
            event_id: 0,
            timestamp_ms,
            pane_id,
            category: EventCategory::PolicyDecision,
            severity,
            summary: format!("Policy {outcome}: {reason}"),
            source: EventSource::Policy,
            details,
            correlation_id,
            related_events: Vec::new(),
            workflow_step: None,
            is_intervention_point: outcome == "deny" || outcome == "Deny",
        };

        self.ingest(event)
    }

    /// Convenience: ingest a workflow step event.
    pub fn ingest_workflow_step(
        &mut self,
        execution_id: &str,
        workflow_name: &str,
        step_name: &str,
        step_index: u32,
        outcome: StepOutcome,
        description: &str,
        pane_id: Option<u64>,
        timestamp_ms: u64,
    ) -> u64 {
        let severity = match outcome {
            StepOutcome::Failed | StepOutcome::Aborted => EventSeverity::Error,
            StepOutcome::Retried => EventSeverity::Warning,
            _ => EventSeverity::Info,
        };

        let mut details = HashMap::new();
        details.insert("execution_id".to_string(), execution_id.to_string());
        details.insert("workflow".to_string(), workflow_name.to_string());
        details.insert("step_index".to_string(), step_index.to_string());
        details.insert("outcome".to_string(), format!("{:?}", outcome).to_lowercase());

        let event = TimelineEvent {
            event_id: 0,
            timestamp_ms,
            pane_id,
            category: EventCategory::WorkflowStep,
            severity,
            summary: format!("{workflow_name}/{step_name}: {description}"),
            source: EventSource::Workflow,
            details,
            correlation_id: Some(execution_id.to_string()),
            related_events: Vec::new(),
            workflow_step: Some(step_name.to_string()),
            is_intervention_point: false,
        };

        self.ingest(event)
    }

    /// Convenience: ingest an error event.
    pub fn ingest_error(
        &mut self,
        pane_id: Option<u64>,
        summary: &str,
        source: EventSource,
        timestamp_ms: u64,
        correlation_id: Option<String>,
    ) -> u64 {
        let event = TimelineEvent {
            event_id: 0,
            timestamp_ms,
            pane_id,
            category: EventCategory::Error,
            severity: EventSeverity::Error,
            summary: summary.to_string(),
            source,
            details: HashMap::new(),
            correlation_id,
            related_events: Vec::new(),
            workflow_step: None,
            is_intervention_point: true,
        };

        self.ingest(event)
    }

    /// Convenience: ingest an intervention event.
    pub fn ingest_intervention(
        &mut self,
        pane_id: Option<u64>,
        summary: &str,
        source: EventSource,
        timestamp_ms: u64,
        correlation_id: Option<String>,
    ) -> u64 {
        let event = TimelineEvent {
            event_id: 0,
            timestamp_ms,
            pane_id,
            category: EventCategory::Intervention,
            severity: EventSeverity::Notice,
            summary: summary.to_string(),
            source,
            details: HashMap::new(),
            correlation_id,
            related_events: Vec::new(),
            workflow_step: None,
            is_intervention_point: true,
        };

        self.ingest(event)
    }

    /// Register a workflow trace.
    pub fn register_workflow_trace(&mut self, trace: WorkflowTrace) {
        self.telemetry.workflow_traces_tracked += 1;
        self.workflow_traces
            .insert(trace.execution_id.clone(), trace);
    }

    /// Get a workflow trace by execution ID.
    #[must_use]
    pub fn get_workflow_trace(&self, execution_id: &str) -> Option<&WorkflowTrace> {
        self.workflow_traces.get(execution_id)
    }

    /// List all workflow traces, sorted by start time.
    #[must_use]
    pub fn list_workflow_traces(&self) -> Vec<&WorkflowTrace> {
        let mut traces: Vec<&WorkflowTrace> = self.workflow_traces.values().collect();
        traces.sort_by_key(|t| t.started_ms);
        traces
    }

    /// Query the timeline.
    pub fn query(&mut self, query: &TimelineQuery) -> TimelineResult {
        self.telemetry.queries_executed += 1;

        let matching: Vec<&TimelineEvent> =
            self.events.iter().filter(|e| query.matches(e)).collect();

        let total_count = matching.len();
        let summary = Self::build_summary(&matching);

        let events: Vec<TimelineEvent> = matching
            .into_iter()
            .skip(query.offset)
            .take(if query.limit == 0 {
                usize::MAX
            } else {
                query.limit
            })
            .cloned()
            .collect();

        TimelineResult {
            events,
            total_count,
            summary,
        }
    }

    /// Get a specific event by ID.
    #[must_use]
    pub fn get_event(&self, event_id: u64) -> Option<&TimelineEvent> {
        self.events.iter().find(|e| e.event_id == event_id)
    }

    /// Get all events correlated to a given event.
    #[must_use]
    pub fn get_correlated(&self, event_id: u64) -> Vec<&TimelineEvent> {
        let event = match self.get_event(event_id) {
            Some(e) => e,
            None => return Vec::new(),
        };

        let corr_id = match &event.correlation_id {
            Some(c) => c,
            None => return Vec::new(),
        };

        self.events
            .iter()
            .filter(|e| e.correlation_id.as_ref() == Some(corr_id) && e.event_id != event_id)
            .collect()
    }

    /// Compute a diff between two time points in the session.
    pub fn diff(&mut self, baseline_ms: u64, comparison_ms: u64) -> SessionDiff {
        self.telemetry.diffs_computed += 1;

        let (from, to) = if baseline_ms <= comparison_ms {
            (baseline_ms, comparison_ms)
        } else {
            (comparison_ms, baseline_ms)
        };

        let events_in_range: Vec<&TimelineEvent> = self
            .events
            .iter()
            .filter(|e| e.timestamp_ms >= from && e.timestamp_ms < to)
            .collect();

        let errors_between = events_in_range
            .iter()
            .filter(|e| e.severity >= EventSeverity::Error)
            .count();

        let denials_between = events_in_range
            .iter()
            .filter(|e| {
                e.category == EventCategory::PolicyDecision
                    && e.details.get("outcome").map(|o| o == "deny" || o == "Deny").unwrap_or(false)
            })
            .count();

        let workflow_steps_between = events_in_range
            .iter()
            .filter(|e| e.category == EventCategory::WorkflowStep)
            .count();

        // Compute pane changes from state change events
        let mut pane_changes = Vec::new();
        for event in &events_in_range {
            if event.category == EventCategory::StateChange {
                if let Some(pane_id) = event.pane_id {
                    let change_type = match event.details.get("change_type").map(|s| s.as_str()) {
                        Some("created") => PaneChangeType::Created,
                        Some("closed") => PaneChangeType::Closed,
                        Some("resized") => PaneChangeType::Resized,
                        Some("title_changed") => PaneChangeType::TitleChanged,
                        _ => PaneChangeType::OutputChanged,
                    };
                    pane_changes.push(PaneChange {
                        pane_id,
                        change_type,
                        description: event.summary.clone(),
                    });
                }
            }
        }

        SessionDiff {
            baseline_ms: from,
            comparison_ms: to,
            pane_changes,
            events_between: events_in_range.len(),
            errors_between,
            denials_between,
            workflow_steps_between,
        }
    }

    /// Extract key events for incident triage.
    pub fn extract(&mut self, since_ms: u64, until_ms: u64) -> EventExtraction {
        self.telemetry.extractions_performed += 1;

        let in_range: Vec<&TimelineEvent> = self
            .events
            .iter()
            .filter(|e| e.timestamp_ms >= since_ms && e.timestamp_ms < until_ms)
            .collect();

        let total_examined = in_range.len();

        let errors: Vec<ExtractedEvent> = in_range
            .iter()
            .filter(|e| e.category == EventCategory::Error)
            .map(|e| ExtractedEvent::from(*e))
            .collect();

        let denials: Vec<ExtractedEvent> = in_range
            .iter()
            .filter(|e| {
                e.category == EventCategory::PolicyDecision
                    && e.details.get("outcome").map(|o| o == "deny" || o == "Deny").unwrap_or(false)
            })
            .map(|e| ExtractedEvent::from(*e))
            .collect();

        let interventions: Vec<ExtractedEvent> = in_range
            .iter()
            .filter(|e| e.category == EventCategory::Intervention)
            .map(|e| ExtractedEvent::from(*e))
            .collect();

        let state_changes: Vec<ExtractedEvent> = in_range
            .iter()
            .filter(|e| e.category == EventCategory::StateChange)
            .map(|e| ExtractedEvent::from(*e))
            .collect();

        let critical_events: Vec<ExtractedEvent> = in_range
            .iter()
            .filter(|e| e.severity >= EventSeverity::Critical)
            .map(|e| ExtractedEvent::from(*e))
            .collect();

        EventExtraction {
            errors,
            denials,
            interventions,
            state_changes,
            critical_events,
            time_range_ms: (since_ms, until_ms),
            total_examined,
        }
    }

    /// Render a timeline event as a human-readable string.
    #[must_use]
    pub fn render_event(event: &TimelineEvent) -> String {
        let sev = format!("{:?}", event.severity).to_uppercase();
        let mut lines = vec![format!(
            "[{ts}] #{id} {sev} {cat:?} ({src:?}): {summary}",
            ts = event.timestamp_ms,
            id = event.event_id,
            cat = event.category,
            src = event.source,
            summary = event.summary,
        )];

        if let Some(pane_id) = event.pane_id {
            lines.push(format!("  pane: {pane_id}"));
        }
        if let Some(ref step) = event.workflow_step {
            lines.push(format!("  workflow_step: {step}"));
        }
        if event.is_intervention_point {
            lines.push("  [INTERVENTION POINT]".to_string());
        }
        if !event.details.is_empty() {
            for (k, v) in &event.details {
                lines.push(format!("  {k}: {v}"));
            }
        }

        lines.join("\n")
    }

    /// Get the number of stored events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the explorer has no events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Get telemetry snapshot.
    #[must_use]
    pub fn telemetry(&self) -> &ExplorerTelemetry {
        &self.telemetry
    }

    // ── Internal helpers ────────────────────────────────────────────────

    fn build_summary(events: &[&TimelineEvent]) -> TimelineSummary {
        let mut summary = TimelineSummary::default();
        let mut pane_set = std::collections::HashSet::new();

        for event in events {
            let cat_key = format!("{:?}", event.category).to_lowercase();
            *summary.by_category.entry(cat_key).or_insert(0) += 1;

            let src_key = format!("{:?}", event.source).to_lowercase();
            *summary.by_source.entry(src_key).or_insert(0) += 1;

            let sev_key = format!("{:?}", event.severity).to_lowercase();
            *summary.by_severity.entry(sev_key).or_insert(0) += 1;

            if let Some(pane_id) = event.pane_id {
                pane_set.insert(pane_id);
            }

            if event.is_intervention_point {
                summary.intervention_count += 1;
            }

            match summary.earliest_ms {
                None => summary.earliest_ms = Some(event.timestamp_ms),
                Some(e) if event.timestamp_ms < e => {
                    summary.earliest_ms = Some(event.timestamp_ms)
                }
                _ => {}
            }
            match summary.latest_ms {
                None => summary.latest_ms = Some(event.timestamp_ms),
                Some(l) if event.timestamp_ms > l => {
                    summary.latest_ms = Some(event.timestamp_ms)
                }
                _ => {}
            }
        }

        summary.pane_ids = pane_set.into_iter().collect();
        summary.pane_ids.sort_unstable();
        summary
    }

    fn rebuild_indices(&mut self) {
        self.correlation_index.clear();
        self.pane_index.clear();

        for (idx, event) in self.events.iter().enumerate() {
            if let Some(ref corr) = event.correlation_id {
                self.correlation_index
                    .entry(corr.clone())
                    .or_default()
                    .push(idx);
            }
            if let Some(pane_id) = event.pane_id {
                self.pane_index.entry(pane_id).or_default().push(idx);
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(
        category: EventCategory,
        severity: EventSeverity,
        source: EventSource,
        pane_id: Option<u64>,
        timestamp_ms: u64,
    ) -> TimelineEvent {
        TimelineEvent {
            event_id: 0,
            timestamp_ms,
            pane_id,
            category,
            severity,
            summary: "test event".to_string(),
            source,
            details: HashMap::new(),
            correlation_id: None,
            related_events: Vec::new(),
            workflow_step: None,
            is_intervention_point: false,
        }
    }

    // -- Explorer basics --

    #[test]
    fn explorer_empty_initially() {
        let explorer = SessionWorkflowExplorer::new(100);
        assert!(explorer.is_empty());
        assert_eq!(explorer.len(), 0);
    }

    #[test]
    fn explorer_ingest_assigns_event_id() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let e = make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            Some(1),
            1000,
        );
        let id = explorer.ingest(e);
        assert_eq!(id, 1);
        assert_eq!(explorer.len(), 1);

        let e2 = make_event(
            EventCategory::Error,
            EventSeverity::Error,
            EventSource::System,
            None,
            1001,
        );
        let id2 = explorer.ingest(e2);
        assert_eq!(id2, 2);
    }

    #[test]
    fn explorer_capacity_eviction() {
        let mut explorer = SessionWorkflowExplorer::new(3);
        for i in 0..5 {
            let e = make_event(
                EventCategory::Output,
                EventSeverity::Info,
                EventSource::Recording,
                Some(i as u64),
                1000 + i,
            );
            explorer.ingest(e);
        }
        assert_eq!(explorer.len(), 3);
        assert!(explorer.get_event(1).is_none());
        assert!(explorer.get_event(2).is_none());
        assert!(explorer.get_event(3).is_some());
    }

    #[test]
    fn explorer_get_event_by_id() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let mut e = make_event(
            EventCategory::Error,
            EventSeverity::Error,
            EventSource::Policy,
            Some(42),
            1000,
        );
        e.summary = "test error".to_string();
        let id = explorer.ingest(e);
        let found = explorer.get_event(id).unwrap();
        assert_eq!(found.pane_id, Some(42));
        assert_eq!(found.summary, "test error");
    }

    #[test]
    fn explorer_get_event_not_found() {
        let explorer = SessionWorkflowExplorer::new(100);
        assert!(explorer.get_event(999).is_none());
    }

    // -- Query tests --

    #[test]
    fn query_all() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        for i in 0..5u64 {
            explorer.ingest(make_event(
                EventCategory::Output,
                EventSeverity::Info,
                EventSource::Recording,
                Some(i),
                1000 + i,
            ));
        }
        let result = explorer.query(&TimelineQuery::all(10));
        assert_eq!(result.total_count, 5);
        assert_eq!(result.events.len(), 5);
    }

    #[test]
    fn query_by_pane() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            Some(1),
            1000,
        ));
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            Some(2),
            1001,
        ));
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            Some(1),
            1002,
        ));

        let result = explorer.query(&TimelineQuery::for_pane(1, 10));
        assert_eq!(result.total_count, 2);
    }

    #[test]
    fn query_errors_only() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1000,
        ));
        explorer.ingest(make_event(
            EventCategory::Error,
            EventSeverity::Error,
            EventSource::System,
            None,
            1001,
        ));
        explorer.ingest(make_event(
            EventCategory::Error,
            EventSeverity::Critical,
            EventSource::System,
            None,
            1002,
        ));

        let result = explorer.query(&TimelineQuery::errors(10));
        assert_eq!(result.total_count, 2);
    }

    #[test]
    fn query_by_source() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::PolicyDecision,
            EventSeverity::Info,
            EventSource::Policy,
            None,
            1000,
        ));
        explorer.ingest(make_event(
            EventCategory::WorkflowStep,
            EventSeverity::Info,
            EventSource::Workflow,
            None,
            1001,
        ));

        let query = TimelineQuery {
            source: Some(EventSource::Policy),
            limit: 10,
            ..Default::default()
        };
        let result = explorer.query(&query);
        assert_eq!(result.total_count, 1);
    }

    #[test]
    fn query_by_time_range() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1000,
        ));
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            2000,
        ));
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            3000,
        ));

        let result = explorer.query(&TimelineQuery::time_range(1500, 2500, 10));
        assert_eq!(result.total_count, 1);
        assert_eq!(result.events[0].timestamp_ms, 2000);
    }

    #[test]
    fn query_by_category() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1000,
        ));
        explorer.ingest(make_event(
            EventCategory::Marker,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1001,
        ));

        let query = TimelineQuery {
            category: Some(EventCategory::Marker),
            limit: 10,
            ..Default::default()
        };
        let result = explorer.query(&query);
        assert_eq!(result.total_count, 1);
    }

    #[test]
    fn query_by_severity() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Trace,
            EventSource::Recording,
            None,
            1000,
        ));
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Warning,
            EventSource::Recording,
            None,
            1001,
        ));
        explorer.ingest(make_event(
            EventCategory::Error,
            EventSeverity::Critical,
            EventSource::System,
            None,
            1002,
        ));

        let query = TimelineQuery {
            min_severity: Some(EventSeverity::Warning),
            limit: 10,
            ..Default::default()
        };
        let result = explorer.query(&query);
        assert_eq!(result.total_count, 2);
    }

    #[test]
    fn query_text_search() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let mut e1 = make_event(
            EventCategory::Error,
            EventSeverity::Error,
            EventSource::System,
            None,
            1000,
        );
        e1.summary = "connection timeout on port 8080".to_string();
        explorer.ingest(e1);

        let mut e2 = make_event(
            EventCategory::Error,
            EventSeverity::Error,
            EventSource::System,
            None,
            1001,
        );
        e2.summary = "disk full on /dev/sda".to_string();
        explorer.ingest(e2);

        let result = explorer.query(&TimelineQuery::search("timeout", 10));
        assert_eq!(result.total_count, 1);
        assert!(result.events[0].summary.contains("timeout"));
    }

    #[test]
    fn query_interventions_only() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1000,
        ));

        let mut e2 = make_event(
            EventCategory::Intervention,
            EventSeverity::Notice,
            EventSource::Robot,
            None,
            1001,
        );
        e2.is_intervention_point = true;
        explorer.ingest(e2);

        let result = explorer.query(&TimelineQuery::interventions(10));
        assert_eq!(result.total_count, 1);
    }

    #[test]
    fn query_pagination() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        for i in 0..10u64 {
            explorer.ingest(make_event(
                EventCategory::Output,
                EventSeverity::Info,
                EventSource::Recording,
                None,
                1000 + i,
            ));
        }

        let query = TimelineQuery {
            limit: 3,
            offset: 2,
            ..Default::default()
        };
        let result = explorer.query(&query);
        assert_eq!(result.total_count, 10);
        assert_eq!(result.events.len(), 3);
        assert_eq!(result.events[0].event_id, 3);
    }

    // -- Correlation tests --

    #[test]
    fn correlation_groups_events() {
        let mut explorer = SessionWorkflowExplorer::new(100);

        let mut e1 = make_event(
            EventCategory::PolicyDecision,
            EventSeverity::Error,
            EventSource::Policy,
            None,
            1000,
        );
        e1.correlation_id = Some("op-123".to_string());
        let id1 = explorer.ingest(e1);

        let mut e2 = make_event(
            EventCategory::Intervention,
            EventSeverity::Notice,
            EventSource::Robot,
            None,
            1001,
        );
        e2.correlation_id = Some("op-123".to_string());
        explorer.ingest(e2);

        let mut e3 = make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1002,
        );
        e3.correlation_id = Some("op-456".to_string());
        explorer.ingest(e3);

        let correlated = explorer.get_correlated(id1);
        assert_eq!(correlated.len(), 1);
        assert_eq!(correlated[0].category, EventCategory::Intervention);
    }

    #[test]
    fn correlation_query() {
        let mut explorer = SessionWorkflowExplorer::new(100);

        let mut e1 = make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1000,
        );
        e1.correlation_id = Some("op-123".to_string());
        explorer.ingest(e1);

        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1001,
        ));

        let result = explorer.query(&TimelineQuery::by_correlation("op-123", 10));
        assert_eq!(result.total_count, 1);
    }

    // -- Convenience ingest tests --

    #[test]
    fn ingest_policy_decision_deny() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let id = explorer.ingest_policy_decision(
            Some(42),
            "deny",
            "Alt screen active",
            Some("safety.alt_screen"),
            1000,
            None,
        );
        let event = explorer.get_event(id).unwrap();
        assert_eq!(event.category, EventCategory::PolicyDecision);
        assert_eq!(event.severity, EventSeverity::Error);
        assert!(event.is_intervention_point);
        assert!(event.summary.contains("deny"));
    }

    #[test]
    fn ingest_policy_decision_allow() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let id = explorer.ingest_policy_decision(
            Some(1),
            "allow",
            "No restrictions",
            None,
            1000,
            None,
        );
        let event = explorer.get_event(id).unwrap();
        assert_eq!(event.severity, EventSeverity::Info);
        assert!(!event.is_intervention_point);
    }

    #[test]
    fn ingest_workflow_step_event() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let id = explorer.ingest_workflow_step(
            "exec-001",
            "deploy",
            "check_health",
            2,
            StepOutcome::Completed,
            "Health check passed",
            Some(5),
            2000,
        );
        let event = explorer.get_event(id).unwrap();
        assert_eq!(event.category, EventCategory::WorkflowStep);
        assert_eq!(event.source, EventSource::Workflow);
        assert_eq!(event.workflow_step.as_deref(), Some("check_health"));
        assert_eq!(event.correlation_id.as_deref(), Some("exec-001"));
    }

    #[test]
    fn ingest_workflow_step_failed() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let id = explorer.ingest_workflow_step(
            "exec-002",
            "deploy",
            "run_tests",
            1,
            StepOutcome::Failed,
            "Tests failed",
            Some(3),
            2000,
        );
        let event = explorer.get_event(id).unwrap();
        assert_eq!(event.severity, EventSeverity::Error);
    }

    #[test]
    fn ingest_error_event() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let id = explorer.ingest_error(
            Some(7),
            "Process crashed with SIGSEGV",
            EventSource::System,
            3000,
            None,
        );
        let event = explorer.get_event(id).unwrap();
        assert_eq!(event.category, EventCategory::Error);
        assert!(event.is_intervention_point);
    }

    #[test]
    fn ingest_intervention_event() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let id = explorer.ingest_intervention(
            Some(7),
            "Robot sent Ctrl-C to stuck process",
            EventSource::Robot,
            3000,
            Some("incident-99".to_string()),
        );
        let event = explorer.get_event(id).unwrap();
        assert_eq!(event.category, EventCategory::Intervention);
        assert!(event.is_intervention_point);
        assert_eq!(event.correlation_id.as_deref(), Some("incident-99"));
    }

    // -- Workflow trace tests --

    #[test]
    fn register_and_get_workflow_trace() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let trace = WorkflowTrace {
            execution_id: "exec-001".to_string(),
            workflow_name: "deploy".to_string(),
            pane_id: Some(5),
            steps: vec![WorkflowStepTrace {
                step_name: "build".to_string(),
                step_index: 0,
                outcome: StepOutcome::Completed,
                started_ms: 1000,
                completed_ms: Some(2000),
                description: "Build succeeded".to_string(),
                policy_decisions: Vec::new(),
                error: None,
            }],
            status: WorkflowTraceStatus::Running,
            started_ms: 1000,
            completed_ms: None,
            duration_ms: None,
            correlation_id: None,
        };
        explorer.register_workflow_trace(trace);

        let found = explorer.get_workflow_trace("exec-001").unwrap();
        assert_eq!(found.workflow_name, "deploy");
        assert_eq!(found.steps.len(), 1);
    }

    #[test]
    fn list_workflow_traces_sorted() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        for (i, ts) in [3000u64, 1000, 2000].iter().enumerate() {
            let trace = WorkflowTrace {
                execution_id: format!("exec-{i}"),
                workflow_name: "test".to_string(),
                pane_id: None,
                steps: Vec::new(),
                status: WorkflowTraceStatus::Completed,
                started_ms: *ts,
                completed_ms: None,
                duration_ms: None,
                correlation_id: None,
            };
            explorer.register_workflow_trace(trace);
        }

        let traces = explorer.list_workflow_traces();
        assert_eq!(traces.len(), 3);
        assert_eq!(traces[0].started_ms, 1000);
        assert_eq!(traces[1].started_ms, 2000);
        assert_eq!(traces[2].started_ms, 3000);
    }

    // -- Session diff tests --

    #[test]
    fn diff_empty_range() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1000,
        ));
        let diff = explorer.diff(2000, 3000);
        assert_eq!(diff.events_between, 0);
        assert_eq!(diff.errors_between, 0);
    }

    #[test]
    fn diff_counts_events() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1000,
        ));
        explorer.ingest(make_event(
            EventCategory::Error,
            EventSeverity::Error,
            EventSource::System,
            None,
            1500,
        ));
        explorer.ingest(make_event(
            EventCategory::WorkflowStep,
            EventSeverity::Info,
            EventSource::Workflow,
            None,
            1800,
        ));
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            2500,
        ));

        let diff = explorer.diff(1000, 2000);
        assert_eq!(diff.events_between, 3);
        assert_eq!(diff.errors_between, 1);
        assert_eq!(diff.workflow_steps_between, 1);
    }

    #[test]
    fn diff_reversed_timestamps() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1500,
        ));
        // Reversed order should still work
        let diff = explorer.diff(2000, 1000);
        assert_eq!(diff.baseline_ms, 1000);
        assert_eq!(diff.comparison_ms, 2000);
        assert_eq!(diff.events_between, 1);
    }

    #[test]
    fn diff_tracks_pane_changes() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let mut e = make_event(
            EventCategory::StateChange,
            EventSeverity::Info,
            EventSource::System,
            Some(5),
            1500,
        );
        e.details
            .insert("change_type".to_string(), "created".to_string());
        e.summary = "Pane 5 created".to_string();
        explorer.ingest(e);

        let diff = explorer.diff(1000, 2000);
        assert_eq!(diff.pane_changes.len(), 1);
        assert_eq!(diff.pane_changes[0].pane_id, 5);
        assert_eq!(diff.pane_changes[0].change_type, PaneChangeType::Created);
    }

    // -- Extraction tests --

    #[test]
    fn extract_errors() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest_error(Some(1), "crash", EventSource::System, 1000, None);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1001,
        ));
        explorer.ingest_error(Some(2), "timeout", EventSource::System, 1002, None);

        let extraction = explorer.extract(0, 2000);
        assert_eq!(extraction.errors.len(), 2);
        assert_eq!(extraction.total_examined, 3);
    }

    #[test]
    fn extract_interventions() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest_intervention(
            Some(1),
            "Ctrl-C sent",
            EventSource::Robot,
            1000,
            None,
        );
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1001,
        ));

        let extraction = explorer.extract(0, 2000);
        assert_eq!(extraction.interventions.len(), 1);
    }

    #[test]
    fn extract_policy_denials() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest_policy_decision(
            Some(1),
            "deny",
            "Alt screen",
            None,
            1000,
            None,
        );
        explorer.ingest_policy_decision(
            Some(1),
            "allow",
            "No restrictions",
            None,
            1001,
            None,
        );

        let extraction = explorer.extract(0, 2000);
        assert_eq!(extraction.denials.len(), 1);
    }

    #[test]
    fn extract_time_range_filtering() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest_error(Some(1), "early error", EventSource::System, 500, None);
        explorer.ingest_error(Some(2), "target error", EventSource::System, 1500, None);
        explorer.ingest_error(Some(3), "late error", EventSource::System, 2500, None);

        let extraction = explorer.extract(1000, 2000);
        assert_eq!(extraction.errors.len(), 1);
        assert_eq!(extraction.errors[0].summary, "target error");
    }

    // -- Render tests --

    #[test]
    fn render_event_basic() {
        let mut event = make_event(
            EventCategory::Error,
            EventSeverity::Error,
            EventSource::System,
            Some(42),
            1000,
        );
        event.event_id = 7;
        event.summary = "Connection failed".to_string();
        event.is_intervention_point = true;

        let rendered = SessionWorkflowExplorer::render_event(&event);
        assert!(rendered.contains("#7"));
        assert!(rendered.contains("ERROR"));
        assert!(rendered.contains("Connection failed"));
        assert!(rendered.contains("pane: 42"));
        assert!(rendered.contains("[INTERVENTION POINT]"));
    }

    #[test]
    fn render_event_with_workflow_step() {
        let mut event = make_event(
            EventCategory::WorkflowStep,
            EventSeverity::Info,
            EventSource::Workflow,
            None,
            1000,
        );
        event.event_id = 3;
        event.workflow_step = Some("build".to_string());

        let rendered = SessionWorkflowExplorer::render_event(&event);
        assert!(rendered.contains("workflow_step: build"));
    }

    // -- Summary tests --

    #[test]
    fn summary_counts_categories() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1000,
        ));
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1001,
        ));
        explorer.ingest(make_event(
            EventCategory::Error,
            EventSeverity::Error,
            EventSource::System,
            None,
            1002,
        ));

        let result = explorer.query(&TimelineQuery::all(10));
        assert_eq!(result.summary.by_category.get("output"), Some(&2));
        assert_eq!(result.summary.by_category.get("error"), Some(&1));
    }

    #[test]
    fn summary_tracks_pane_ids() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            Some(1),
            1000,
        ));
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            Some(3),
            1001,
        ));
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            Some(1),
            1002,
        ));

        let result = explorer.query(&TimelineQuery::all(10));
        assert_eq!(result.summary.pane_ids, vec![1, 3]);
    }

    #[test]
    fn summary_time_range() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1000,
        ));
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            3000,
        ));
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            2000,
        ));

        let result = explorer.query(&TimelineQuery::all(10));
        assert_eq!(result.summary.earliest_ms, Some(1000));
        assert_eq!(result.summary.latest_ms, Some(3000));
    }

    #[test]
    fn summary_intervention_count() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest_intervention(None, "action 1", EventSource::Robot, 1000, None);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1001,
        ));
        explorer.ingest_intervention(None, "action 2", EventSource::Robot, 1002, None);

        let result = explorer.query(&TimelineQuery::all(10));
        assert_eq!(result.summary.intervention_count, 2);
    }

    // -- Telemetry tests --

    #[test]
    fn telemetry_tracks_ingestion() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1000,
        ));
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1001,
        ));
        assert_eq!(explorer.telemetry().events_ingested, 2);
    }

    #[test]
    fn telemetry_tracks_evictions() {
        let mut explorer = SessionWorkflowExplorer::new(2);
        for i in 0..5u64 {
            explorer.ingest(make_event(
                EventCategory::Output,
                EventSeverity::Info,
                EventSource::Recording,
                None,
                1000 + i,
            ));
        }
        assert_eq!(explorer.telemetry().events_evicted, 3);
    }

    #[test]
    fn telemetry_tracks_queries() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest(make_event(
            EventCategory::Output,
            EventSeverity::Info,
            EventSource::Recording,
            None,
            1000,
        ));
        let _ = explorer.query(&TimelineQuery::all(10));
        let _ = explorer.query(&TimelineQuery::all(10));
        assert_eq!(explorer.telemetry().queries_executed, 2);
    }

    #[test]
    fn telemetry_tracks_diffs() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let _ = explorer.diff(1000, 2000);
        assert_eq!(explorer.telemetry().diffs_computed, 1);
    }

    #[test]
    fn telemetry_tracks_extractions() {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let _ = explorer.extract(0, 1000);
        assert_eq!(explorer.telemetry().extractions_performed, 1);
    }

    // -- Serde roundtrip tests --

    #[test]
    fn timeline_event_serde_roundtrip() {
        let mut event = make_event(
            EventCategory::Error,
            EventSeverity::Error,
            EventSource::System,
            Some(42),
            1000,
        );
        event.event_id = 1;
        event.correlation_id = Some("corr-1".to_string());
        event.details.insert("key".into(), "value".into());

        let json = serde_json::to_string(&event).unwrap();
        let event2: TimelineEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event2.event_id, 1);
        assert_eq!(event2.severity, EventSeverity::Error);
        assert_eq!(event2.details.get("key").unwrap(), "value");
    }

    #[test]
    fn timeline_query_serde_roundtrip() {
        let query = TimelineQuery::errors(50);
        let json = serde_json::to_string(&query).unwrap();
        let query2: TimelineQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(query2.min_severity, Some(EventSeverity::Error));
        assert_eq!(query2.limit, 50);
    }

    #[test]
    fn workflow_trace_serde_roundtrip() {
        let trace = WorkflowTrace {
            execution_id: "exec-001".to_string(),
            workflow_name: "deploy".to_string(),
            pane_id: Some(5),
            steps: vec![],
            status: WorkflowTraceStatus::Completed,
            started_ms: 1000,
            completed_ms: Some(2000),
            duration_ms: Some(1000),
            correlation_id: None,
        };
        let json = serde_json::to_string(&trace).unwrap();
        let trace2: WorkflowTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(trace2.execution_id, "exec-001");
        assert_eq!(trace2.status, WorkflowTraceStatus::Completed);
    }

    #[test]
    fn session_diff_serde_roundtrip() {
        let diff = SessionDiff {
            baseline_ms: 1000,
            comparison_ms: 2000,
            pane_changes: vec![],
            events_between: 5,
            errors_between: 1,
            denials_between: 0,
            workflow_steps_between: 2,
        };
        let json = serde_json::to_string(&diff).unwrap();
        let diff2: SessionDiff = serde_json::from_str(&json).unwrap();
        assert_eq!(diff2.events_between, 5);
    }

    #[test]
    fn event_extraction_serde_roundtrip() {
        let extraction = EventExtraction {
            errors: vec![],
            denials: vec![],
            interventions: vec![],
            state_changes: vec![],
            critical_events: vec![],
            time_range_ms: (0, 1000),
            total_examined: 0,
        };
        let json = serde_json::to_string(&extraction).unwrap();
        let extraction2: EventExtraction = serde_json::from_str(&json).unwrap();
        assert_eq!(extraction2.total_examined, 0);
    }

    #[test]
    fn event_severity_ordering() {
        assert!(EventSeverity::Trace < EventSeverity::Info);
        assert!(EventSeverity::Info < EventSeverity::Notice);
        assert!(EventSeverity::Notice < EventSeverity::Warning);
        assert!(EventSeverity::Warning < EventSeverity::Error);
        assert!(EventSeverity::Error < EventSeverity::Critical);
    }
}
