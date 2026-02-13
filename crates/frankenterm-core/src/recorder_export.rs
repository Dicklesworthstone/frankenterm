//! Recorder data export with access control, format options, and audit.
//!
//! Bead: wa-zb58
//!
//! Supports exporting recorder events to structured formats (JSON lines,
//! CSV, plaintext transcript) with:
//!
//! - Authorization checks (same tier model as query)
//! - Post-export redaction for sensitive data
//! - Audit logging of all export operations
//! - Configurable output format and filtering
//!
//! # Export Pipeline
//!
//! ```text
//! ExportRequest ──→ authorize ──→ query events ──→ redact ──→ format ──→ ExportResult
//!                                                                │
//!                                                           audit log
//! ```

use serde::{Deserialize, Serialize};
use std::fmt::Write as FmtWrite;

use crate::recorder_audit::{
    AccessTier, ActorIdentity, AuditEventBuilder, AuditEventType, AuditLog, AuditScope,
    AuthzDecision,
};
use crate::recorder_query::{
    QueryEventKind, RecorderEventReader, RecorderQueryExecutor, RecorderQueryRequest, TimeRange,
};
use crate::recorder_retention::SensitivityTier;

// =============================================================================
// Export format
// =============================================================================

/// Output format for exported data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    /// One JSON object per line (JSONL/NDJSON).
    JsonLines,
    /// Comma-separated values with header row.
    Csv,
    /// Human-readable plaintext transcript.
    Transcript,
}

impl std::fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::JsonLines => write!(f, "jsonl"),
            Self::Csv => write!(f, "csv"),
            Self::Transcript => write!(f, "transcript"),
        }
    }
}

// =============================================================================
// Export request
// =============================================================================

/// Configuration for a recorder export operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRequest {
    /// Output format.
    pub format: ExportFormat,
    /// Time range to export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_range: Option<TimeRange>,
    /// Pane IDs to include (empty = all).
    #[serde(default)]
    pub pane_ids: Vec<u64>,
    /// Event kinds to include (empty = all).
    #[serde(default)]
    pub kind_filter: Vec<QueryEventKind>,
    /// Maximum events to export (0 = unlimited).
    #[serde(default)]
    pub max_events: usize,
    /// Whether to include text content (false = metadata-only).
    #[serde(default = "default_true")]
    pub include_text: bool,
    /// Maximum sensitivity tier to include.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sensitivity: Option<SensitivityTier>,
    /// Human-readable label for the export (stored in audit trail).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for ExportRequest {
    fn default() -> Self {
        Self {
            format: ExportFormat::JsonLines,
            time_range: None,
            pane_ids: Vec::new(),
            kind_filter: Vec::new(),
            max_events: 0,
            include_text: true,
            max_sensitivity: None,
            label: None,
        }
    }
}

impl ExportRequest {
    /// Create a JSONL export for a time range.
    #[must_use]
    pub fn jsonl(start_ms: u64, end_ms: u64) -> Self {
        Self {
            format: ExportFormat::JsonLines,
            time_range: Some(TimeRange { start_ms, end_ms }),
            ..Default::default()
        }
    }

    /// Create a CSV export for specific panes.
    #[must_use]
    pub fn csv_for_panes(pane_ids: Vec<u64>) -> Self {
        Self {
            format: ExportFormat::Csv,
            pane_ids,
            ..Default::default()
        }
    }

    /// Create a transcript export for a time range.
    #[must_use]
    pub fn transcript(start_ms: u64, end_ms: u64) -> Self {
        Self {
            format: ExportFormat::Transcript,
            time_range: Some(TimeRange { start_ms, end_ms }),
            ..Default::default()
        }
    }

    /// Set the maximum events to export.
    #[must_use]
    pub fn with_max_events(mut self, max: usize) -> Self {
        self.max_events = max;
        self
    }

    /// Set a label for audit purposes.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Determine the minimum access tier required.
    #[must_use]
    pub fn required_tier(&self) -> AccessTier {
        if !self.include_text {
            AccessTier::A0PublicMetadata
        } else if self.max_sensitivity == Some(SensitivityTier::T3Restricted) {
            AccessTier::A3PrivilegedRaw
        } else if self.pane_ids.len() > 1 {
            AccessTier::A2FullQuery
        } else {
            AccessTier::A1RedactedQuery
        }
    }
}

// =============================================================================
// Export result
// =============================================================================

/// Result of an export operation.
#[derive(Debug, Clone)]
pub struct ExportResult {
    /// The exported data as a string.
    pub data: String,
    /// Number of events exported.
    pub event_count: usize,
    /// Export format used.
    pub format: ExportFormat,
    /// Whether any events were redacted.
    pub redaction_applied: bool,
    /// Effective access tier used.
    pub effective_tier: AccessTier,
    /// Byte size of the exported data.
    pub data_bytes: usize,
}

// =============================================================================
// Export errors
// =============================================================================

/// Errors from the export engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// Access denied.
    AccessDenied {
        actor_tier: AccessTier,
        required_tier: AccessTier,
    },
    /// Elevation required.
    ElevationRequired {
        required_tier: AccessTier,
        current_tier: AccessTier,
    },
    /// No events match the export criteria.
    NoMatchingEvents,
    /// Export too large.
    TooLarge {
        event_count: usize,
        max_events: usize,
    },
    /// Format error during serialization.
    FormatError(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AccessDenied {
                actor_tier,
                required_tier,
            } => write!(
                f,
                "export denied: actor tier {} < required tier {}",
                actor_tier, required_tier
            ),
            Self::ElevationRequired {
                required_tier,
                current_tier,
            } => write!(
                f,
                "export elevation required: {} → {}",
                current_tier, required_tier
            ),
            Self::NoMatchingEvents => write!(f, "no events match export criteria"),
            Self::TooLarge {
                event_count,
                max_events,
            } => write!(
                f,
                "export too large: {} events exceeds limit of {}",
                event_count, max_events
            ),
            Self::FormatError(msg) => write!(f, "export format error: {}", msg),
        }
    }
}

impl std::error::Error for ExportError {}

// =============================================================================
// Export engine
// =============================================================================

/// Maximum default export size (events) to prevent accidental huge exports.
pub const DEFAULT_MAX_EXPORT_EVENTS: usize = 50_000;

/// Export engine that wraps the query executor with format serialization.
pub struct RecorderExporter<R: RecorderEventReader> {
    executor: RecorderQueryExecutor<R>,
    max_export_events: usize,
}

impl<R: RecorderEventReader> RecorderExporter<R> {
    /// Create a new exporter wrapping the given query executor.
    pub fn new(executor: RecorderQueryExecutor<R>) -> Self {
        Self {
            executor,
            max_export_events: DEFAULT_MAX_EXPORT_EVENTS,
        }
    }

    /// Set the maximum export size.
    pub fn with_max_events(mut self, max: usize) -> Self {
        self.max_export_events = max;
        self
    }

    /// Execute an export operation.
    pub fn export(
        &self,
        actor: &ActorIdentity,
        request: &ExportRequest,
        now_ms: u64,
    ) -> Result<ExportResult, ExportError> {
        // 1. Build a query request from the export request.
        let mut query = RecorderQueryRequest::default();
        query.time_range = request.time_range;
        query.pane_ids = request.pane_ids.clone();
        query.include_text = request.include_text;
        query.max_sensitivity = request.max_sensitivity;

        // Use the export's max_events or the global limit.
        let limit = if request.max_events > 0 {
            request.max_events
        } else {
            self.max_export_events
        };
        query.limit = limit;

        // 2. Execute the query via the query executor (handles authz + redaction + audit).
        let query_result = self
            .executor
            .execute(actor, &query, now_ms)
            .map_err(|e| match e {
                crate::recorder_query::QueryError::AccessDenied {
                    actor_tier,
                    required_tier,
                } => ExportError::AccessDenied {
                    actor_tier,
                    required_tier,
                },
                crate::recorder_query::QueryError::ElevationRequired {
                    required_tier,
                    current_tier,
                } => ExportError::ElevationRequired {
                    required_tier,
                    current_tier,
                },
                crate::recorder_query::QueryError::InvalidRequest(msg) => {
                    ExportError::FormatError(msg)
                }
                crate::recorder_query::QueryError::Internal(msg) => ExportError::FormatError(msg),
            })?;

        // 3. Apply kind filter (query executor doesn't have this).
        let events: Vec<_> = if request.kind_filter.is_empty() {
            query_result.events
        } else {
            query_result
                .events
                .into_iter()
                .filter(|e| request.kind_filter.contains(&e.event_kind))
                .collect()
        };

        if events.is_empty() {
            return Err(ExportError::NoMatchingEvents);
        }

        // 4. Check size limit.
        if events.len() > self.max_export_events {
            return Err(ExportError::TooLarge {
                event_count: events.len(),
                max_events: self.max_export_events,
            });
        }

        // 5. Format the output.
        let data = match request.format {
            ExportFormat::JsonLines => format_jsonl(&events)?,
            ExportFormat::Csv => format_csv(&events)?,
            ExportFormat::Transcript => format_transcript(&events)?,
        };

        let data_bytes = data.len();

        // 6. Audit the export.
        self.audit_export(actor, request, events.len(), data_bytes, now_ms);

        Ok(ExportResult {
            data,
            event_count: events.len(),
            format: request.format,
            redaction_applied: query_result.redaction_applied,
            effective_tier: query_result.effective_tier,
            data_bytes,
        })
    }

    /// Access the underlying query executor's audit log.
    #[must_use]
    pub fn audit_log(&self) -> &AuditLog {
        self.executor.audit_log()
    }

    fn audit_export(
        &self,
        actor: &ActorIdentity,
        request: &ExportRequest,
        event_count: usize,
        data_bytes: usize,
        now_ms: u64,
    ) {
        self.executor.audit_log().append(
            AuditEventBuilder::new(AuditEventType::RecorderExport, actor.clone(), now_ms)
                .with_decision(AuthzDecision::Allow)
                .with_scope(AuditScope {
                    pane_ids: request.pane_ids.clone(),
                    time_range: request.time_range.map(|tr| (tr.start_ms, tr.end_ms)),
                    query: request.label.clone(),
                    segment_ids: Vec::new(),
                    result_count: Some(event_count as u64),
                })
                .with_details(serde_json::json!({
                    "format": request.format.to_string(),
                    "data_bytes": data_bytes,
                    "include_text": request.include_text,
                })),
        );
    }
}

// =============================================================================
// Format implementations
// =============================================================================

/// Serializable export row for JSON and CSV output.
#[derive(Debug, Serialize, Deserialize)]
struct ExportRow {
    event_id: String,
    pane_id: u64,
    source: String,
    occurred_at_ms: u64,
    sequence: u64,
    event_kind: String,
    sensitivity: String,
    redacted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

fn to_export_row(event: &crate::recorder_query::QueryResultEvent) -> ExportRow {
    ExportRow {
        event_id: event.event_id.clone(),
        pane_id: event.pane_id,
        source: format!("{:?}", event.source),
        occurred_at_ms: event.occurred_at_ms,
        sequence: event.sequence,
        event_kind: format!("{:?}", event.event_kind),
        sensitivity: format!("{:?}", event.sensitivity),
        redacted: event.redacted,
        session_id: event.session_id.clone(),
        text: event.text.clone(),
    }
}

fn format_jsonl(events: &[crate::recorder_query::QueryResultEvent]) -> Result<String, ExportError> {
    let mut output = String::new();
    for event in events {
        let row = to_export_row(event);
        let json =
            serde_json::to_string(&row).map_err(|e| ExportError::FormatError(e.to_string()))?;
        output.push_str(&json);
        output.push('\n');
    }
    Ok(output)
}

fn format_csv(events: &[crate::recorder_query::QueryResultEvent]) -> Result<String, ExportError> {
    let mut output = String::new();
    // Header.
    output.push_str("event_id,pane_id,source,occurred_at_ms,sequence,event_kind,sensitivity,redacted,session_id,text\n");

    for event in events {
        let row = to_export_row(event);
        let text_escaped = row.text.as_deref().unwrap_or("").replace('"', "\"\"");
        let session = row.session_id.as_deref().unwrap_or("");

        write!(
            output,
            "{},{},{},{},{},{},{},{},{},\"{}\"\n",
            row.event_id,
            row.pane_id,
            row.source,
            row.occurred_at_ms,
            row.sequence,
            row.event_kind,
            row.sensitivity,
            row.redacted,
            session,
            text_escaped,
        )
        .map_err(|e| ExportError::FormatError(e.to_string()))?;
    }
    Ok(output)
}

fn format_transcript(
    events: &[crate::recorder_query::QueryResultEvent],
) -> Result<String, ExportError> {
    let mut output = String::new();
    output.push_str("# Flight Recorder Transcript\n");
    output.push_str("#\n");

    if let (Some(first), Some(last)) = (events.first(), events.last()) {
        write!(
            output,
            "# Time range: {} — {}\n",
            first.occurred_at_ms, last.occurred_at_ms
        )
        .map_err(|e| ExportError::FormatError(e.to_string()))?;
    }

    let mut panes: Vec<u64> = events.iter().map(|e| e.pane_id).collect();
    panes.sort();
    panes.dedup();
    write!(output, "# Panes: {:?}\n", panes)
        .map_err(|e| ExportError::FormatError(e.to_string()))?;
    write!(output, "# Events: {}\n", events.len())
        .map_err(|e| ExportError::FormatError(e.to_string()))?;
    output.push_str("#\n\n");

    for event in events {
        let kind = match event.event_kind {
            QueryEventKind::IngressText => "IN",
            QueryEventKind::EgressOutput => "OUT",
            QueryEventKind::ControlMarker => "CTL",
            QueryEventKind::LifecycleMarker => "LCY",
        };

        let redacted_marker = if event.redacted { " [REDACTED]" } else { "" };

        write!(
            output,
            "[{:>12}] pane:{} {:>3}{} | ",
            event.occurred_at_ms, event.pane_id, kind, redacted_marker
        )
        .map_err(|e| ExportError::FormatError(e.to_string()))?;

        if let Some(text) = &event.text {
            // Truncate long lines for readability.
            if text.len() > 200 {
                write!(output, "{}...", &text[..197])
                    .map_err(|e| ExportError::FormatError(e.to_string()))?;
            } else {
                output.push_str(text);
            }
        } else {
            output.push_str("(no text)");
        }
        output.push('\n');
    }

    Ok(output)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::ActorKind;
    use crate::recorder_audit::AuditLogConfig;
    use crate::recorder_query::{InMemoryEventStore, QueryResultEvent};
    use crate::recording::RecorderEventSource;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_event(
        pane_id: u64,
        seq: u64,
        ts_ms: u64,
        text: &str,
    ) -> crate::recording::RecorderEvent {
        use crate::recording::*;
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: format!("evt-{}-{}", pane_id, seq),
            pane_id,
            session_id: Some("sess-1".into()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: ts_ms,
            recorded_at_ms: ts_ms + 1,
            sequence: seq,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::IngressText {
                text: text.to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        }
    }

    fn human() -> ActorIdentity {
        ActorIdentity::new(ActorKind::Human, "user-1")
    }

    fn robot() -> ActorIdentity {
        ActorIdentity::new(ActorKind::Robot, "bot-1")
    }

    fn test_exporter(
        events: Vec<crate::recording::RecorderEvent>,
    ) -> RecorderExporter<InMemoryEventStore> {
        let store = InMemoryEventStore::new();
        store.insert(events);
        let executor = RecorderQueryExecutor::new(store, AuditLog::new(AuditLogConfig::default()));
        RecorderExporter::new(executor)
    }

    fn sample_events() -> Vec<crate::recording::RecorderEvent> {
        vec![
            make_event(1, 0, 1000, "ls -la"),
            make_event(1, 1, 2000, "cat README.md"),
            make_event(2, 2, 3000, "echo hello"),
            make_event(1, 3, 4000, "cargo test"),
        ]
    }

    const NOW: u64 = 1700000000000;

    // -----------------------------------------------------------------------
    // JSONL export
    // -----------------------------------------------------------------------

    #[test]
    fn export_jsonl() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::jsonl(0, 5000);

        let result = exporter.export(&human(), &req, NOW).unwrap();

        assert_eq!(result.format, ExportFormat::JsonLines);
        assert_eq!(result.event_count, 4);

        // Each line should be valid JSON.
        for line in result.data.lines() {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(parsed.get("event_id").is_some());
            assert!(parsed.get("pane_id").is_some());
        }
    }

    #[test]
    fn export_jsonl_line_count() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::jsonl(0, 5000);

        let result = exporter.export(&human(), &req, NOW).unwrap();
        let lines: Vec<_> = result.data.lines().collect();
        assert_eq!(lines.len(), 4);
    }

    // -----------------------------------------------------------------------
    // CSV export
    // -----------------------------------------------------------------------

    #[test]
    fn export_csv() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest {
            format: ExportFormat::Csv,
            ..Default::default()
        };

        let result = exporter.export(&human(), &req, NOW).unwrap();

        assert_eq!(result.format, ExportFormat::Csv);
        let lines: Vec<_> = result.data.lines().collect();
        // Header + 4 data rows.
        assert_eq!(lines.len(), 5);
        assert!(lines[0].starts_with("event_id,"));
    }

    #[test]
    fn csv_header_columns() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest {
            format: ExportFormat::Csv,
            ..Default::default()
        };

        let result = exporter.export(&human(), &req, NOW).unwrap();
        let header = result.data.lines().next().unwrap();
        let cols: Vec<_> = header.split(',').collect();
        assert_eq!(cols.len(), 10);
        assert_eq!(cols[0], "event_id");
        assert_eq!(cols[1], "pane_id");
        assert_eq!(cols[9], "text");
    }

    // -----------------------------------------------------------------------
    // Transcript export
    // -----------------------------------------------------------------------

    #[test]
    fn export_transcript() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::transcript(0, 5000);

        let result = exporter.export(&human(), &req, NOW).unwrap();

        assert_eq!(result.format, ExportFormat::Transcript);
        assert!(result.data.starts_with("# Flight Recorder Transcript"));
        assert!(result.data.contains("pane:1"));
        assert!(result.data.contains("ls -la"));
        assert!(result.data.contains("echo hello"));
    }

    #[test]
    fn transcript_shows_event_kind() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::transcript(0, 5000);

        let result = exporter.export(&human(), &req, NOW).unwrap();
        // IngressText events should show "IN".
        assert!(result.data.contains(" IN "));
    }

    // -----------------------------------------------------------------------
    // Access control
    // -----------------------------------------------------------------------

    #[test]
    fn robot_denied_cross_pane_export() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::csv_for_panes(vec![1, 2]);

        let result = exporter.export(&robot(), &req, NOW);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExportError::ElevationRequired { .. } => {}
            other => panic!("expected ElevationRequired, got {:?}", other),
        }
    }

    #[test]
    fn human_allowed_cross_pane_export() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::csv_for_panes(vec![1, 2]);

        let result = exporter.export(&human(), &req, NOW);
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // Filtering
    // -----------------------------------------------------------------------

    #[test]
    fn export_with_time_range() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::jsonl(1500, 3500);

        let result = exporter.export(&human(), &req, NOW).unwrap();
        assert_eq!(result.event_count, 2); // Events at 2000 and 3000.
    }

    #[test]
    fn export_with_pane_filter() {
        let exporter = test_exporter(sample_events());
        let mut req = ExportRequest::default();
        req.pane_ids = vec![2];

        let result = exporter.export(&human(), &req, NOW).unwrap();
        assert_eq!(result.event_count, 1); // Only pane 2 event.
    }

    #[test]
    fn export_with_kind_filter() {
        let exporter = test_exporter(sample_events());
        let mut req = ExportRequest::default();
        req.kind_filter = vec![QueryEventKind::LifecycleMarker];

        // All events are IngressText, so LifecycleMarker filter yields empty.
        let result = exporter.export(&human(), &req, NOW);
        assert!(matches!(result, Err(ExportError::NoMatchingEvents)));
    }

    #[test]
    fn export_max_events() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::default().with_max_events(2);

        let result = exporter.export(&human(), &req, NOW).unwrap();
        assert_eq!(result.event_count, 2);
    }

    // -----------------------------------------------------------------------
    // No matching events
    // -----------------------------------------------------------------------

    #[test]
    fn no_matching_events() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::jsonl(99999, 99999);

        let result = exporter.export(&human(), &req, NOW);
        assert!(matches!(result, Err(ExportError::NoMatchingEvents)));
    }

    // -----------------------------------------------------------------------
    // Audit trail
    // -----------------------------------------------------------------------

    #[test]
    fn export_generates_audit_entries() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::jsonl(0, 5000).with_label("incident-42");

        let _ = exporter.export(&human(), &req, NOW);

        let entries = exporter.audit_log().entries();
        // Query audit + Export audit = 2 entries.
        assert_eq!(entries.len(), 2);

        // The second entry should be the export audit.
        let export_entry = &entries[1];
        assert_eq!(export_entry.event_type, AuditEventType::RecorderExport);
        assert_eq!(export_entry.actor.kind, ActorKind::Human);

        // Check export details.
        let details = export_entry.details.as_ref().unwrap();
        assert_eq!(details["format"], "jsonl");
    }

    #[test]
    fn denied_export_generates_query_audit() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::csv_for_panes(vec![1, 2]);

        let _ = exporter.export(&robot(), &req, NOW);

        let entries = exporter.audit_log().entries();
        // Only the denied query audit entry (no export audit since export didn't happen).
        assert_eq!(entries.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Format display
    // -----------------------------------------------------------------------

    #[test]
    fn format_display() {
        assert_eq!(ExportFormat::JsonLines.to_string(), "jsonl");
        assert_eq!(ExportFormat::Csv.to_string(), "csv");
        assert_eq!(ExportFormat::Transcript.to_string(), "transcript");
    }

    // -----------------------------------------------------------------------
    // Error display
    // -----------------------------------------------------------------------

    #[test]
    fn export_error_display() {
        let err = ExportError::AccessDenied {
            actor_tier: AccessTier::A1RedactedQuery,
            required_tier: AccessTier::A3PrivilegedRaw,
        };
        let msg = err.to_string();
        assert!(msg.contains("denied"));

        let err = ExportError::TooLarge {
            event_count: 100,
            max_events: 50,
        };
        assert!(err.to_string().contains("100"));
    }

    // -----------------------------------------------------------------------
    // Required tier
    // -----------------------------------------------------------------------

    #[test]
    fn required_tier_metadata() {
        let req = ExportRequest {
            include_text: false,
            ..Default::default()
        };
        assert_eq!(req.required_tier(), AccessTier::A0PublicMetadata);
    }

    #[test]
    fn required_tier_cross_pane() {
        let req = ExportRequest::csv_for_panes(vec![1, 2]);
        assert_eq!(req.required_tier(), AccessTier::A2FullQuery);
    }

    #[test]
    fn required_tier_t3() {
        let mut req = ExportRequest::default();
        req.max_sensitivity = Some(SensitivityTier::T3Restricted);
        assert_eq!(req.required_tier(), AccessTier::A3PrivilegedRaw);
    }

    // -----------------------------------------------------------------------
    // Data size tracking
    // -----------------------------------------------------------------------

    #[test]
    fn data_bytes_tracked() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::jsonl(0, 5000);

        let result = exporter.export(&human(), &req, NOW).unwrap();
        assert_eq!(result.data_bytes, result.data.len());
        assert!(result.data_bytes > 0);
    }

    // -----------------------------------------------------------------------
    // Builder/label
    // -----------------------------------------------------------------------

    #[test]
    fn export_with_label() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::jsonl(0, 5000).with_label("debug session 42");

        let result = exporter.export(&human(), &req, NOW).unwrap();
        assert_eq!(result.event_count, 4);

        // Label should appear in audit.
        let entries = exporter.audit_log().entries();
        let export_entry = entries.last().unwrap();
        assert_eq!(
            export_entry.scope.query.as_deref(),
            Some("debug session 42")
        );
    }

    // -----------------------------------------------------------------------
    // ExportRequest serialization roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn export_request_serializable() {
        let req = ExportRequest::jsonl(1000, 2000)
            .with_max_events(50)
            .with_label("test");

        let json = serde_json::to_string(&req).unwrap();
        let decoded: ExportRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.format, ExportFormat::JsonLines);
        assert_eq!(decoded.max_events, 50);
        assert_eq!(decoded.label.as_deref(), Some("test"));
    }
}
