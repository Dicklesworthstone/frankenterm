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
    #[must_use]
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
        // Use the export's max_events or the global limit.
        let limit = if request.max_events > 0 {
            request.max_events
        } else {
            self.max_export_events
        };
        let query = RecorderQueryRequest {
            time_range: request.time_range,
            pane_ids: request.pane_ids.clone(),
            include_text: request.include_text,
            max_sensitivity: request.max_sensitivity,
            limit,
            ..Default::default()
        };

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

        writeln!(
            output,
            "{},{},{},{},{},{},{},{},{},\"{}\"",
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
        writeln!(
            output,
            "# Time range: {} — {}",
            first.occurred_at_ms, last.occurred_at_ms
        )
        .map_err(|e| ExportError::FormatError(e.to_string()))?;
    }

    let mut panes: Vec<u64> = events.iter().map(|e| e.pane_id).collect();
    panes.sort();
    panes.dedup();
    writeln!(output, "# Panes: {:?}", panes)
        .map_err(|e| ExportError::FormatError(e.to_string()))?;
    writeln!(output, "# Events: {}", events.len())
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
                let end = (0..=197)
                    .rev()
                    .find(|&i| text.is_char_boundary(i))
                    .unwrap_or(0);
                write!(output, "{}...", &text[..end])
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
// Bounded recorder snapshots -> CASR canonical sessions
// =============================================================================

#[cfg(feature = "session-resume")]
pub use casr_export::{
    CasrExportError, CasrExportRequest, CasrExportResult, export_casr_with_cx,
};

#[cfg(feature = "session-resume")]
mod casr_export {
    use std::io::{self, Write};
    use std::path::PathBuf;

    use serde::Serialize;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    use crate::casr_types::{CanonicalMessage, CanonicalSession, MessageRole};
    use crate::cx::Cx;
    use crate::recorder_audit::{
        AccessTier, ActorIdentity, AuditEventBuilder, AuditEventType, AuditLog, AuditScope,
        AuthzDecision, check_authorization,
    };
    use crate::recorder_storage::RecorderEventReader;
    use crate::recording::{
        RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventPayload,
        RecorderEventSource, RecorderLifecyclePhase, RecorderRedactionLevel, RecorderSegmentKind,
    };
    use crate::redactor::{REDACTED_MARKER, Redactor};

    /// Finite bounds for one pane's retained recorder snapshot.
    ///
    /// No limit means "unlimited": zero and values above the hard ceilings are
    /// rejected. The source-byte budget counts stored frame bytes in every pane,
    /// including each four-byte framing header. Callers must also use a bounded
    /// native reader to limit individual frames before deserialization.
    #[derive(Debug, Clone, Copy)]
    pub struct CasrExportRequest {
        pub pane_id: u64,
        pub max_events: usize,
        pub max_output_bytes: usize,
        pub max_scan_events: usize,
        pub max_source_bytes: u64,
    }

    impl Default for CasrExportRequest {
        fn default() -> Self {
            Self {
                pane_id: 0,
                max_events: 10_000,
                max_output_bytes: 16 * 1024 * 1024,
                max_scan_events: 100_000,
                max_source_bytes: 256 * 1024 * 1024,
            }
        }
    }

    impl CasrExportRequest {
        /// Validate before opening the source or allocating an export.
        pub fn validate(&self) -> Result<(), CasrExportError> {
            if self.max_events == 0
                || self.max_events > 50_000
                || self.max_output_bytes == 0
                || self.max_output_bytes > 64 * 1024 * 1024
                || self.max_scan_events == 0
                || self.max_scan_events > 1_000_000
                || self.max_source_bytes == 0
                || self.max_source_bytes > 4 * 1024 * 1024 * 1024
            {
                return Err(CasrExportError::InvalidLimits);
            }
            Ok(())
        }
    }

    /// A complete conversion, ready for private, no-replace publication.
    ///
    /// The JSON is an FT envelope whose sessions use CASR's canonical model.
    /// It is terminal evidence, not an assertion of semantic user/assistant
    /// turns or a claim that CASR's CLI can import/resume this artifact.
    #[derive(Debug)]
    pub struct CasrExportResult {
        pub data: Vec<u8>,
        pub pane_id: u64,
        pub sessions_exported: usize,
        pub events_exported: usize,
        pub events_scanned: usize,
        pub redacted_events: usize,
        pub source_start_ordinal: u64,
        /// Exclusive captured head.
        pub source_end_ordinal: u64,
        pub source_prefix_missing: bool,
    }

    /// Content-free failures: storage diagnostics may contain recorded secrets.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
    pub enum CasrExportError {
        #[error("CASR export limits must be positive and within the supported ceilings")]
        InvalidLimits,
        #[error("CASR export requires redacted recorder-query access")]
        AccessDenied,
        #[error("CASR export was cancelled")]
        Cancelled,
        #[error("recorder snapshot could not be read")]
        SourceUnavailable,
        #[error("recorder snapshot changed while exporting")]
        SourceChanged,
        #[error("recorder snapshot has invalid schema, order, or continuity")]
        InvalidSource,
        #[error("recorder snapshot exceeds the scan or source-byte limit")]
        SourceLimitExceeded,
        #[error("selected pane exceeds the export-event limit")]
        EventLimitExceeded,
        #[error("CASR export exceeds the text or encoded-output byte limit")]
        OutputLimitExceeded,
        #[error("no events for the selected pane occur in the retained snapshot")]
        NoMatchingEvents,
        #[error("recorder timestamp cannot be represented by CASR")]
        TimestampOutOfRange,
        #[error("CASR export could not be serialized")]
        SerializationFailed,
    }

    impl CasrExportError {
        fn code(self) -> &'static str {
            match self {
                Self::InvalidLimits => "invalid_limits",
                Self::AccessDenied => "access_denied",
                Self::Cancelled => "cancelled",
                Self::SourceUnavailable => "source_unavailable",
                Self::SourceChanged => "source_changed",
                Self::InvalidSource => "invalid_source",
                Self::SourceLimitExceeded => "source_limit",
                Self::EventLimitExceeded => "event_limit",
                Self::OutputLimitExceeded => "output_limit",
                Self::NoMatchingEvents => "no_matching_events",
                Self::TimestampOutOfRange => "timestamp_out_of_range",
                Self::SerializationFailed => "serialization_failed",
            }
        }
    }

    /// Convert a bounded native read-only snapshot without opening a writer.
    ///
    /// The caller owns blocking execution and output publication. All rows in
    /// the captured head count toward scan bounds, including foreign panes.
    /// An error returns no partial artifact. The audit log is the supplied
    /// in-memory log; the exported receipt does not claim durable audit storage.
    pub fn export_casr_with_cx<R: RecorderEventReader + ?Sized>(
        cx: &Cx,
        reader: &R,
        actor: &ActorIdentity,
        request: &CasrExportRequest,
        audit_log: &AuditLog,
        now_ms: u64,
    ) -> Result<CasrExportResult, CasrExportError> {
        let decision = check_authorization(actor.kind, AccessTier::A1RedactedQuery);
        let result = request.validate().and_then(|()| {
            checkpoint(cx)?;
            if decision != AuthzDecision::Allow {
                return Err(CasrExportError::AccessDenied);
            }
            convert_snapshot(cx, reader, actor, request, now_ms)
        });
        let (count, details) = match &result {
            Ok(export) => (
                export.events_exported as u64,
                json!({
                    "format": "ft.recorder.casr.v1",
                    "operation": "convert_snapshot",
                    "outcome": "converted",
                    "sessions": export.sessions_exported,
                    "events_scanned": export.events_scanned,
                    "redacted_events": export.redacted_events,
                    "bytes": export.data.len(),
                }),
            ),
            Err(error) => (
                0,
                json!({
                    "format": "ft.recorder.casr.v1",
                    "operation": "convert_snapshot",
                    "outcome": "refused",
                    "reason": error.code(),
                }),
            ),
        };
        audit_log.append(
            AuditEventBuilder::new(AuditEventType::RecorderExport, actor.clone(), now_ms)
                .with_decision(decision)
                .with_scope(AuditScope {
                    pane_ids: vec![request.pane_id],
                    result_count: Some(count),
                    ..AuditScope::default()
                })
                .with_details(details),
        );
        result
    }

    fn checkpoint(cx: &Cx) -> Result<(), CasrExportError> {
        cx.checkpoint().map_err(|_| CasrExportError::Cancelled)
    }

    struct BoundedOutput {
        bytes: Vec<u8>,
        limit: usize,
        exceeded: bool,
    }

    impl Write for BoundedOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(io::Error::other("output byte limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Serialize)]
    struct Bundle<'a> {
        schema_version: &'static str,
        representation: &'static str,
        receipt: serde_json::Value,
        sessions: &'a [CanonicalSession],
    }

    // Keep messages in durable order. Only the bounded redaction inspection
    // text is joined: nontext markers and other-pane events must not create a
    // way to split a credential around the detector.
    struct TextStream {
        text: String,
        // (message index, start byte, exclusive end byte)
        chunks: Vec<(usize, usize, usize)>,
    }

    struct SessionBuilder {
        canonical: CanonicalSession,
        raw_session_id: Option<String>,
        streams: Vec<TextStream>,
        // Only input/delta streams are indexed. Snapshot streams are
        // independent, and must not turn each later lookup into a full scan.
        stream_lookup: Vec<(RecorderEventSource, u8, usize)>,
        redacted: Vec<bool>,
        first_ordinal: u64,
        last_ordinal: u64,
        last_observed_ms: u64,
        contains_gap: bool,
    }

    impl SessionBuilder {
        fn new(event: &RecorderEvent, ordinal: u64, timestamp: i64) -> Self {
            // Stable identity across repeated exports without copying arbitrary
            // source identifiers. Length framing distinguishes ambiguous pairs.
            // This is collision isolation, not authenticated source provenance.
            let mut digest = Sha256::new();
            digest.update(b"ft.recorder.casr.session.v1\0");
            digest.update(event.pane_id.to_le_bytes());
            digest.update(ordinal.to_le_bytes());
            digest.update((event.event_id.len() as u64).to_le_bytes());
            digest.update(event.event_id.as_bytes());
            match &event.session_id {
                Some(session_id) => {
                    digest.update([1]);
                    digest.update((session_id.len() as u64).to_le_bytes());
                    digest.update(session_id.as_bytes());
                }
                None => digest.update([0]),
            }
            let source_key = hex::encode(digest.finalize());
            Self {
                canonical: CanonicalSession {
                    session_id: format!("ft-recorder-{}-{ordinal}-{source_key}", event.pane_id),
                    provider_slug: "frankenterm".into(),
                    workspace: None,
                    title: Some(format!("Recorded terminal pane {}", event.pane_id)),
                    started_at: Some(timestamp),
                    ended_at: None,
                    messages: Vec::new(),
                    metadata: serde_json::Value::Null,
                    source_path: PathBuf::from(format!(
                        "frankenterm-recorder/{source_key}/pane-{}/part-{ordinal}",
                        event.pane_id
                    )),
                    model_name: None,
                },
                raw_session_id: event.session_id.clone(),
                streams: Vec::new(),
                stream_lookup: Vec::new(),
                redacted: Vec::new(),
                first_ordinal: ordinal,
                last_ordinal: ordinal,
                last_observed_ms: event.occurred_at_ms,
                contains_gap: false,
            }
        }

        fn push(
            &mut self,
            event: &RecorderEvent,
            ordinal: u64,
            timestamp: i64,
            actor_tier: AccessTier,
        ) {
            let idx = self.canonical.messages.len();
            let mut extra = json!({
                "pane_id": event.pane_id,
                "recorder_ordinal": ordinal,
                "sequence": event.sequence,
                "source": event.source,
            });
            let (role, content, stream_kind, protected) = match &event.payload {
                RecorderEventPayload::IngressText {
                    text,
                    redaction,
                    ingress_kind,
                    ..
                } => {
                    extra["ingress_kind"] = json!(ingress_kind);
                    extra["delivery_verified"] = json!(false);
                    (
                        "terminal_input_attempt",
                        text.as_str(),
                        Some(0),
                        must_mask(*redaction, actor_tier),
                    )
                }
                RecorderEventPayload::EgressOutput {
                    text,
                    redaction,
                    segment_kind,
                    is_gap,
                    ..
                } => {
                    extra["segment_kind"] = json!(segment_kind);
                    extra["is_gap"] = json!(is_gap);
                    let gap = *is_gap || *segment_kind == RecorderSegmentKind::Gap;
                    self.contains_gap |= gap;
                    if gap {
                        // A discontinuous fragment cannot establish a complete
                        // token; retain its boundary without releasing its text.
                        (
                            "recorder_gap",
                            "Recorder output continuity gap; fragment omitted.",
                            None,
                            false,
                        )
                    } else if *segment_kind == RecorderSegmentKind::Snapshot {
                        (
                            "terminal_snapshot",
                            text.as_str(),
                            Some(2),
                            must_mask(*redaction, actor_tier),
                        )
                    } else {
                        (
                            "terminal_output",
                            text.as_str(),
                            Some(1),
                            must_mask(*redaction, actor_tier),
                        )
                    }
                }
                RecorderEventPayload::ControlMarker { control_marker_type, .. } => {
                    extra["control_marker_type"] = json!(control_marker_type);
                    ("recorder_control", "Recorder control marker.", None, false)
                }
                RecorderEventPayload::LifecycleMarker { lifecycle_phase, .. } => {
                    extra["lifecycle_phase"] = json!(lifecycle_phase);
                    ("recorder_lifecycle", "Recorder lifecycle marker.", None, false)
                }
            };
            let message_content = if protected {
                REDACTED_MARKER.to_owned()
            } else {
                content.to_owned()
            };
            self.canonical.messages.push(CanonicalMessage {
                idx,
                role: MessageRole::Other(role.into()),
                content: message_content,
                timestamp: Some(timestamp),
                author: None,
                tool_calls: Vec::new(),
                tool_results: Vec::new(),
                extra,
            });
            self.redacted.push(protected);
            if let Some(kind) = stream_kind {
                // Keep full raw text internal even when an event is policy
                // masked: a key can cross the protected/visible boundary.
                // Snapshots are independent complete buffers, not deltas.
                let stream_index = if kind == 2 {
                    None
                } else {
                    self.stream_lookup
                        .iter()
                        .find(|(source, stream_kind, _)| {
                            *source == event.source && *stream_kind == kind
                        })
                        .map(|(_, _, index)| *index)
                };
                let stream_index = stream_index.unwrap_or_else(|| {
                    let index = self.streams.len();
                    self.streams.push(TextStream {
                        text: String::new(),
                        chunks: Vec::new(),
                    });
                    if kind != 2 {
                        self.stream_lookup.push((event.source, kind, index));
                    }
                    index
                });
                let stream = &mut self.streams[stream_index];
                let start = stream.text.len();
                stream.text.push_str(content);
                stream.chunks.push((idx, start, stream.text.len()));
            }
            self.last_ordinal = ordinal;
            self.last_observed_ms = event.occurred_at_ms;
        }

        fn finish(
            mut self,
            cx: &Cx,
            redactor: &Redactor,
        ) -> Result<(CanonicalSession, usize), CasrExportError> {
            for stream in &self.streams {
                checkpoint(cx)?;
                let mut trace = redactor.redact_with_replacement_spans(&stream.text);
                trace
                    .replacements
                    .sort_unstable_by_key(|(_, start, end)| (*start, *end));

                // Escape sequences may hide a credential from the raw pattern
                // matcher. Inspect the whole stream after ANSI removal too.
                // Without a byte mapping for that normalization, mask the
                // entire affected stream conservatively.
                let normalized = crate::output::strip_ansi(&stream.text);
                let normalized = normalized
                    .chars()
                    .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
                    .collect::<String>();
                let mask_stream = normalized != stream.text
                    && redactor.redact(&normalized) != normalized;
                let mut span_index = 0;
                for &(idx, start, end) in &stream.chunks {
                    while span_index < trace.replacements.len()
                        && trace.replacements[span_index].2 <= start
                    {
                        span_index += 1;
                    }
                    let touched = trace.replacements[span_index..]
                        .iter()
                        .take_while(|(_, secret_start, _)| *secret_start < end)
                        .any(|(_, secret_start, secret_end)| {
                            *secret_end > start && *secret_start < end
                        });
                    if mask_stream || touched {
                        self.canonical.messages[idx].content = REDACTED_MARKER.into();
                        self.redacted[idx] = true;
                    }
                }
            }
            // No caller-controlled paths, identifiers, marker details, or
            // lifecycle reasons become outward metadata.
            self.canonical.metadata = json!({
                "representation": "recorded_terminal_events",
                "semantic_roles_verified": false,
                "input_delivery_verified": false,
                "unsupported_fields": [
                    "provider_chat_roles", "tool_calls", "tool_results", "attachments"
                ],
                "ordering": "durable_recorder_ordinal",
                "terminal_text": "verbatim_except_redaction",
                "first_ordinal": self.first_ordinal,
                "last_ordinal": self.last_ordinal,
                "last_observed_at_ms": self.last_observed_ms,
                "contains_gap": self.contains_gap,
            });
            let redacted_count = self.redacted.iter().filter(|masked| **masked).count();
            Ok((self.canonical, redacted_count))
        }
    }

    fn must_mask(redaction: RecorderRedactionLevel, actor_tier: AccessTier) -> bool {
        match redaction {
            RecorderRedactionLevel::None => false,
            RecorderRedactionLevel::Partial => !actor_tier.satisfies(AccessTier::A2FullQuery),
            RecorderRedactionLevel::Full => true,
        }
    }

    fn starts_session(event: &RecorderEvent) -> bool {
        matches!(
            event.payload,
            RecorderEventPayload::LifecycleMarker {
                lifecycle_phase:
                    RecorderLifecyclePhase::CaptureStarted | RecorderLifecyclePhase::PaneOpened,
                ..
            }
        )
    }

    fn ends_session(event: &RecorderEvent) -> bool {
        matches!(
            event.payload,
            RecorderEventPayload::LifecycleMarker {
                lifecycle_phase:
                    RecorderLifecyclePhase::CaptureStopped | RecorderLifecyclePhase::PaneClosed,
                ..
            }
        )
    }

    fn is_gap(event: &RecorderEvent) -> bool {
        matches!(
            event.payload,
            RecorderEventPayload::EgressOutput { is_gap: true, .. }
                | RecorderEventPayload::EgressOutput {
                    segment_kind: RecorderSegmentKind::Gap,
                    ..
                }
        )
    }

    fn finish_session(
        current: &mut Option<SessionBuilder>,
        sessions: &mut Vec<CanonicalSession>,
        redacted_events: &mut usize,
        cx: &Cx,
        redactor: &Redactor,
    ) -> Result<(), CasrExportError> {
        if let Some(session) = current.take() {
            let (session, redacted) = session.finish(cx, redactor)?;
            *redacted_events += redacted;
            sessions.push(session);
        }
        Ok(())
    }

    fn convert_snapshot<R: RecorderEventReader + ?Sized>(
        cx: &Cx,
        reader: &R,
        actor: &ActorIdentity,
        request: &CasrExportRequest,
        now_ms: u64,
    ) -> Result<CasrExportResult, CasrExportError> {
        let head = reader
            .head_offset()
            .map_err(|_| CasrExportError::SourceUnavailable)?;
        let mut cursor = reader
            .open_cursor_from_start()
            .map_err(|_| CasrExportError::SourceUnavailable)?;
        let source_start_ordinal = cursor.current_offset().ordinal;
        if source_start_ordinal > head.ordinal {
            return Err(CasrExportError::InvalidSource);
        }
        let mut next_ordinal = source_start_ordinal;
        let mut source_bytes_scanned = 0u64;
        let mut events_scanned = 0;
        let mut events_exported = 0;
        let mut text_bytes = 0usize;
        let mut redacted_events = 0;
        let mut sessions = Vec::new();
        let mut current: Option<SessionBuilder> = None;
        let actor_tier = AccessTier::default_for_actor(actor.kind);
        let redactor = Redactor::new();
        loop {
            checkpoint(cx)?;
            // A short batch is not EOF. Reading one at a time also avoids
            // buffering a backend-selected number of large decoded records.
            let records = cursor
                .next_batch(1)
                .map_err(|_| CasrExportError::SourceUnavailable)?;
            if records.is_empty() {
                if next_ordinal != head.ordinal
                    || cursor.current_offset().ordinal != head.ordinal
                {
                    return Err(CasrExportError::InvalidSource);
                }
                break;
            }
            if records.len() != 1 {
                return Err(CasrExportError::InvalidSource);
            }
            let record = records
                .into_iter()
                .next()
                .ok_or(CasrExportError::InvalidSource)?;
            if record.offset.ordinal != next_ordinal || next_ordinal >= head.ordinal {
                return Err(CasrExportError::InvalidSource);
            }
            next_ordinal = next_ordinal
                .checked_add(1)
                .ok_or(CasrExportError::InvalidSource)?;
            let after = cursor.current_offset();
            if after.ordinal != next_ordinal || after.segment_id != record.offset.segment_id {
                return Err(CasrExportError::InvalidSource);
            }
            // Native readers retain the just-read segment until the next
            // call and advance by the actual four-byte header plus payload.
            // Count stored bytes, including JSON whitespace and unknown fields.
            let frame_bytes = after
                .byte_offset
                .checked_sub(record.offset.byte_offset)
                .filter(|bytes| *bytes >= 4)
                .ok_or(CasrExportError::InvalidSource)?;
            source_bytes_scanned = source_bytes_scanned
                .checked_add(frame_bytes)
                .ok_or(CasrExportError::SourceLimitExceeded)?;
            if source_bytes_scanned > request.max_source_bytes {
                return Err(CasrExportError::SourceLimitExceeded);
            }
            events_scanned += 1;
            if events_scanned > request.max_scan_events {
                return Err(CasrExportError::SourceLimitExceeded);
            }
            let event = record.event;
            if event.schema_version != RECORDER_EVENT_SCHEMA_VERSION_V1 {
                return Err(CasrExportError::InvalidSource);
            }
            if event.pane_id != request.pane_id {
                continue;
            }
            events_exported += 1;
            if events_exported > request.max_events {
                return Err(CasrExportError::EventLimitExceeded);
            }
            let timestamp = i64::try_from(event.occurred_at_ms)
                .map_err(|_| CasrExportError::TimestampOutOfRange)?;
            let len = match &event.payload {
                RecorderEventPayload::IngressText { text, .. }
                | RecorderEventPayload::EgressOutput { text, .. } => text.len(),
                _ => 0,
            };
            text_bytes = text_bytes
                .checked_add(len)
                .ok_or(CasrExportError::OutputLimitExceeded)?;
            if text_bytes > request.max_output_bytes {
                return Err(CasrExportError::OutputLimitExceeded);
            }
            let gap = is_gap(&event);
            if current.as_ref().is_some_and(|session| {
                session.raw_session_id != event.session_id || starts_session(&event) || gap
            }) {
                finish_session(&mut current, &mut sessions, &mut redacted_events, cx, &redactor)?;
            }
            let session = current.get_or_insert_with(|| {
                SessionBuilder::new(&event, record.offset.ordinal, timestamp)
            });
            session.push(&event, record.offset.ordinal, timestamp, actor_tier);
            if ends_session(&event) {
                session.canonical.ended_at = Some(timestamp);
            }
            if ends_session(&event) || gap {
                finish_session(&mut current, &mut sessions, &mut redacted_events, cx, &redactor)?;
            }
        }
        checkpoint(cx)?;
        if reader
            .head_offset()
            .map_err(|_| CasrExportError::SourceUnavailable)?
            != head
        {
            return Err(CasrExportError::SourceChanged);
        }
        if events_exported == 0 {
            return Err(CasrExportError::NoMatchingEvents);
        }
        finish_session(&mut current, &mut sessions, &mut redacted_events, cx, &redactor)?;
        checkpoint(cx)?;
        let source_prefix_missing = source_start_ordinal != 0;
        let bundle = Bundle {
            schema_version: "ft.recorder.casr.v1",
            representation: "terminal_transcript",
            receipt: json!({
                "operation": "convert_snapshot",
                "pane_id": request.pane_id,
                "created_at_ms": now_ms,
                "sessions_exported": sessions.len(),
                "events_exported": events_exported,
                "events_scanned": events_scanned,
                "redacted_events": redacted_events,
                "source_start_ordinal": source_start_ordinal,
                "source_end_ordinal": head.ordinal,
                "source_prefix_missing": source_prefix_missing,
                "complete_retained_snapshot": true,
                "complete_original_conversation": false,
                "semantic_roles_verified": false,
                "redaction": "catalog_and_access_tier",
                "redaction_unit": "affected_event_or_ansi_obscured_stream",
                "source_framed_bytes_scanned": source_bytes_scanned,
                "audit_persistence": "caller_owned",
            }),
            sessions: &sessions,
        };
        let mut output = BoundedOutput {
            bytes: Vec::new(),
            limit: request.max_output_bytes,
            exceeded: false,
        };
        if serde_json::to_writer(&mut output, &bundle).is_err() {
            return Err(if output.exceeded {
                CasrExportError::OutputLimitExceeded
            } else {
                CasrExportError::SerializationFailed
            });
        }
        checkpoint(cx)?;
        Ok(CasrExportResult {
            data: output.bytes,
            pane_id: request.pane_id,
            sessions_exported: sessions.len(),
            events_exported,
            events_scanned,
            redacted_events,
            source_start_ordinal,
            source_end_ordinal: head.ordinal,
            source_prefix_missing,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::policy::ActorKind;
        use crate::recorder_audit::AuditLogConfig;
        use crate::recorder_storage::{
            CursorRecord, EventCursorError, RecorderEventCursor, RecorderOffset,
        };
        use crate::recording::{
            RecorderControlMarkerType, RecorderEventCausality, RecorderIngressKind,
            RecorderTextEncoding,
        };

        #[derive(Clone)]
        struct Snapshot {
            events: Vec<RecorderEvent>,
            start: u64,
            padding: u64,
            hole_at: Option<usize>,
            stop_after: Option<usize>,
            fail_after: Option<usize>,
        }

        impl Snapshot {
            fn new(events: Vec<RecorderEvent>) -> Self {
                Self {
                    events,
                    start: 0,
                    padding: 0,
                    hole_at: None,
                    stop_after: None,
                    fail_after: None,
                }
            }

            fn bytes(&self) -> u64 {
                self.events.iter().map(|event| {
                    4 + serde_json::to_vec(event).unwrap().len() as u64 + self.padding
                }).sum()
            }
        }

        struct SnapshotCursor {
            snapshot: Snapshot,
            index: usize,
            byte_offset: u64,
        }

        fn offset(ordinal: u64, byte_offset: u64) -> RecorderOffset {
            RecorderOffset { segment_id: 0, byte_offset, ordinal }
        }

        impl RecorderEventReader for Snapshot {
            fn open_cursor(
                &self,
                from: RecorderOffset,
            ) -> Result<Box<dyn RecorderEventCursor>, EventCursorError> {
                let index = from.ordinal.saturating_sub(self.start) as usize;
                let index = index.min(self.events.len());
                let byte_offset = self.events[..index].iter().map(|event| {
                    4 + serde_json::to_vec(event).unwrap().len() as u64 + self.padding
                }).sum();
                Ok(Box::new(SnapshotCursor {
                    snapshot: self.clone(),
                    index,
                    byte_offset,
                }))
            }

            fn head_offset(&self) -> Result<RecorderOffset, EventCursorError> {
                Ok(offset(self.start + self.events.len() as u64, self.bytes()))
            }
        }

        impl RecorderEventCursor for SnapshotCursor {
            fn next_batch(&mut self, max: usize) -> Result<Vec<CursorRecord>, EventCursorError> {
                if self.snapshot.fail_after == Some(self.index) {
                    return Err(EventCursorError::Unavailable("RAW_SECRET_DIAGNOSTIC".into()));
                }
                if max == 0
                    || self.index == self.snapshot.events.len()
                    || self.snapshot.stop_after == Some(self.index)
                {
                    return Ok(Vec::new());
                }
                let event = self.snapshot.events[self.index].clone();
                let mut ordinal = self.snapshot.start + self.index as u64;
                if self.snapshot.hole_at == Some(self.index) {
                    ordinal += 1;
                }
                let record = CursorRecord {
                    event,
                    offset: offset(ordinal, self.byte_offset),
                };
                self.byte_offset += 4 + serde_json::to_vec(&record.event).unwrap().len() as u64
                    + self.snapshot.padding;
                self.index += 1;
                Ok(vec![record])
            }

            fn current_offset(&self) -> RecorderOffset {
                offset(self.snapshot.start + self.index as u64, self.byte_offset)
            }
        }

        fn output(text: &str, pane_id: u64) -> RecorderEvent {
            RecorderEvent {
                schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.into(),
                event_id: "PRIVATE_EVENT_ID".into(),
                pane_id,
                session_id: Some("PRIVATE_SESSION_ID".into()),
                workflow_id: Some("PRIVATE_WORKFLOW_ID".into()),
                correlation_id: Some("PRIVATE_CORRELATION_ID".into()),
                source: RecorderEventSource::WeztermMux,
                occurred_at_ms: 1_000,
                recorded_at_ms: 1_001,
                sequence: 7,
                causality: RecorderEventCausality::default(),
                payload: RecorderEventPayload::EgressOutput {
                    text: text.into(),
                    encoding: RecorderTextEncoding::Utf8,
                    redaction: RecorderRedactionLevel::None,
                    segment_kind: RecorderSegmentKind::Delta,
                    is_gap: false,
                },
            }
        }

        fn control() -> RecorderEvent {
            let mut event = output("", 7);
            event.payload = RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::PolicyDecision,
                details: json!({"secret": "PRIVATE_CONTROL_DETAILS"}),
            };
            event
        }

        fn lifecycle(phase: RecorderLifecyclePhase) -> RecorderEvent {
            let mut event = output("", 7);
            event.payload = RecorderEventPayload::LifecycleMarker {
                lifecycle_phase: phase,
                reason: Some("PRIVATE_LIFECYCLE_REASON".into()),
                details: json!({"secret": "PRIVATE_LIFECYCLE_DETAILS"}),
            };
            event
        }

        fn request() -> CasrExportRequest {
            CasrExportRequest { pane_id: 7, ..CasrExportRequest::default() }
        }

        fn export(
            snapshot: &Snapshot,
            actor: ActorKind,
            request: CasrExportRequest,
        ) -> Result<CasrExportResult, CasrExportError> {
            export_casr_with_cx(
                &Cx::for_testing(),
                snapshot,
                &ActorIdentity::new(actor, "casr-export-test"),
                &request,
                &AuditLog::new(AuditLogConfig::default()),
                2_000,
            )
        }

        fn bundle(export: &CasrExportResult) -> serde_json::Value {
            serde_json::from_slice(&export.data).unwrap()
        }

        #[test]
        fn split_secret_across_markers_and_foreign_panes_is_masked_in_durable_order() {
            let mut second = output("abcdefghijklmnopqrstuvwxyz123456\n", 7);
            // Timestamp regression and equal source sequence values do not
            // replace the durable ordinal as ordering authority.
            second.occurred_at_ms = 900;
            let source = Snapshot::new(vec![
                output("key=sk-", 7),
                control(),
                output("FOREIGN_PANE_MUST_NOT_ESCAPE", 8),
                second,
                output("public tail", 7),
            ]);
            let export = export(&source, ActorKind::Human, request()).unwrap();
            assert_eq!(export.events_scanned, 5);
            assert_eq!(export.events_exported, 4);
            assert_eq!(export.redacted_events, 2);
            let text = String::from_utf8(export.data.clone()).unwrap();
            for private in ["sk-", "abcdefghijklmnopqrstuvwxyz123456", "FOREIGN_PANE",
                "PRIVATE_EVENT_ID", "PRIVATE_SESSION_ID", "PRIVATE_WORKFLOW_ID",
                "PRIVATE_CORRELATION_ID", "PRIVATE_CONTROL_DETAILS"] {
                assert!(!text.contains(private), "{private}");
            }
            let decoded = bundle(&export);
            let messages = decoded["sessions"][0]["messages"].as_array().unwrap();
            assert_eq!(messages[0]["content"], REDACTED_MARKER);
            assert_eq!(messages[1]["role"]["Other"], "recorder_control");
            assert_eq!(messages[2]["content"], REDACTED_MARKER);
            assert_eq!(messages[2]["extra"]["recorder_ordinal"], 3);
            assert_eq!(messages[2]["timestamp"], 900);
            assert_eq!(messages[3]["content"], "public tail");
            for message in messages {
                assert!(message["role"].get("Other").is_some());
                assert_eq!(message["tool_calls"], json!([]));
                assert_eq!(message["tool_results"], json!([]));
            }
        }

        #[test]
        fn ansi_obscured_secret_is_masked_across_record_boundaries() {
            let source = Snapshot::new(vec![
                output("key=ghp_\u{1b}[", 7),
                control(),
                output("31mabcdefghijklmnopqrstuvwxyz0123456789\u{1b}[0m", 7),
            ]);
            let export = export(&source, ActorKind::Human, request()).unwrap();
            assert_eq!(export.redacted_events, 2);
            let decoded = bundle(&export);
            let messages = decoded["sessions"][0]["messages"].as_array().unwrap();
            assert_eq!(messages[0]["content"], REDACTED_MARKER);
            assert_eq!(messages[2]["content"], REDACTED_MARKER);
        }

        #[test]
        fn input_is_an_unverified_attempt_and_access_tiers_mask_whole_events() {
            let mut input = output("", 7);
            input.source = RecorderEventSource::RobotMode;
            input.payload = RecorderEventPayload::IngressText {
                text: "possibly denied command".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::Partial,
                ingress_kind: RecorderIngressKind::SendText,
            };
            let mut full = output("FULL_PROTECTED_TEXT", 7);
            if let RecorderEventPayload::EgressOutput { redaction, .. } = &mut full.payload {
                *redaction = RecorderRedactionLevel::Full;
            }
            let source = Snapshot::new(vec![input, control(), full]);
            let robot = export(&source, ActorKind::Robot, request()).unwrap();
            let robot_json = bundle(&robot);
            assert_eq!(robot.redacted_events, 2);
            assert_eq!(robot_json["sessions"][0]["messages"][0]["content"], REDACTED_MARKER);
            assert_eq!(robot_json["sessions"][0]["messages"][0]["role"]["Other"], "terminal_input_attempt");
            assert_eq!(robot_json["sessions"][0]["messages"][0]["extra"]["delivery_verified"], false);
            let human = export(&source, ActorKind::Human, request()).unwrap();
            let human_json = bundle(&human);
            assert_eq!(human.redacted_events, 1);
            assert_eq!(human_json["sessions"][0]["messages"][0]["content"], "possibly denied command");
            assert_eq!(human_json["sessions"][0]["messages"][2]["content"], REDACTED_MARKER);
        }

        #[test]
        fn protected_fragment_does_not_allow_visible_remainder_of_a_key_to_escape() {
            let mut protected = output("sk-", 7);
            if let RecorderEventPayload::EgressOutput { redaction, .. } = &mut protected.payload {
                *redaction = RecorderRedactionLevel::Full;
            }
            let source = Snapshot::new(vec![protected, output("abcdefghijklmnopqrstuvwxyz123456", 7)]);
            let export = export(&source, ActorKind::Human, request()).unwrap();
            assert_eq!(export.redacted_events, 2);
            assert!(!String::from_utf8(export.data).unwrap().contains("abcdefghijklmnopqrstuvwxyz"));
        }

        #[test]
        fn boundaries_prefix_loss_and_snapshots_are_explicit_without_inventing_turns() {
            let mut gap = output("PRIVATE_GAP_FRAGMENT", 7);
            if let RecorderEventPayload::EgressOutput { is_gap, .. } = &mut gap.payload {
                *is_gap = true;
            }
            let mut snapshot = output("visible snapshot", 7);
            if let RecorderEventPayload::EgressOutput { segment_kind, .. } = &mut snapshot.payload {
                *segment_kind = RecorderSegmentKind::Snapshot;
            }
            let mut other_session = output("different source session", 7);
            other_session.session_id = Some("ANOTHER_PRIVATE_SESSION_ID".into());
            let mut source = Snapshot::new(vec![
                lifecycle(RecorderLifecyclePhase::CaptureStarted),
                output("first capture", 7),
                lifecycle(RecorderLifecyclePhase::CaptureStopped),
                output("open-ended capture", 7),
                gap,
                snapshot,
                other_session,
            ]);
            source.start = 80;
            let export = export(&source, ActorKind::Human, request()).unwrap();
            assert!(export.source_prefix_missing);
            assert_eq!(export.source_start_ordinal, 80);
            assert_eq!(export.source_end_ordinal, 87);
            assert_eq!(export.sessions_exported, 5);
            let decoded = bundle(&export);
            let sessions = decoded["sessions"].as_array().unwrap();
            assert_eq!(sessions[0]["ended_at"], 1_000);
            assert!(sessions[1].get("ended_at").is_none());
            assert_eq!(sessions[2]["metadata"]["contains_gap"], true);
            assert_eq!(sessions[3]["messages"][0]["role"]["Other"], "terminal_snapshot");
            assert!(sessions[4].get("ended_at").is_none());
            assert_eq!(decoded["receipt"]["complete_original_conversation"], false);
            let data = String::from_utf8(export.data).unwrap();
            for private in ["PRIVATE_GAP_FRAGMENT", "PRIVATE_LIFECYCLE_REASON",
                "PRIVATE_LIFECYCLE_DETAILS", "ANOTHER_PRIVATE_SESSION_ID"] {
                assert!(!data.contains(private), "{private}");
            }
        }

        #[test]
        fn limits_count_unselected_records_and_actual_stored_bytes_and_never_truncate() {
            let mut source = Snapshot::new(vec![output("other pane", 8), output("selected", 7)]);
            // Simulate stored JSON whitespace / unknown additive fields that
            // are no longer visible in the deserialized event.
            source.padding = 4_096;
            let mut limits = request();
            limits.max_source_bytes = source.bytes();
            limits.max_scan_events = 2;
            limits.max_events = 1;
            let exact = export(&source, ActorKind::Human, limits).unwrap();
            assert_eq!(exact.events_exported, 1);
            assert_eq!(bundle(&exact)["receipt"]["source_framed_bytes_scanned"], source.bytes());
            limits.max_source_bytes -= 1;
            assert_eq!(export(&source, ActorKind::Human, limits).unwrap_err(), CasrExportError::SourceLimitExceeded);
            limits = request();
            limits.max_scan_events = 1;
            assert_eq!(export(&source, ActorKind::Human, limits).unwrap_err(), CasrExportError::SourceLimitExceeded);
            let selected = Snapshot::new(vec![output("first", 7), output("second", 7)]);
            limits = request();
            limits.max_events = 1;
            assert_eq!(export(&selected, ActorKind::Human, limits).unwrap_err(), CasrExportError::EventLimitExceeded);
            limits = request();
            limits.max_output_bytes = 10;
            assert_eq!(export(&source, ActorKind::Human, limits).unwrap_err(), CasrExportError::OutputLimitExceeded);
        }

        #[test]
        fn unknown_schema_missing_rows_and_early_eof_refuse_the_entire_export() {
            let mut invalid = output("safe", 8);
            invalid.schema_version = "PRIVATE_UNSUPPORTED_SCHEMA".into();
            assert_eq!(
                export(&Snapshot::new(vec![invalid, output("selected", 7)]), ActorKind::Human, request()).unwrap_err(),
                CasrExportError::InvalidSource
            );
            let mut hole = Snapshot::new(vec![output("first", 7), output("second", 7)]);
            hole.hole_at = Some(1);
            assert_eq!(export(&hole, ActorKind::Human, request()).unwrap_err(), CasrExportError::InvalidSource);
            hole.hole_at = None;
            hole.stop_after = Some(1);
            assert_eq!(export(&hole, ActorKind::Human, request()).unwrap_err(), CasrExportError::InvalidSource);
        }

        #[test]
        fn reader_failures_and_audit_receipts_do_not_echo_source_diagnostics() {
            let mut source = Snapshot::new(vec![output("safe", 7)]);
            source.fail_after = Some(0);
            let audit = AuditLog::new(AuditLogConfig::default());
            let error = export_casr_with_cx(
                &Cx::for_testing(), &source,
                &ActorIdentity::new(ActorKind::Human, "cli.record.export_casr"),
                &request(), &audit, 2_000,
            ).unwrap_err();
            assert_eq!(error, CasrExportError::SourceUnavailable);
            assert!(!error.to_string().contains("RAW_SECRET_DIAGNOSTIC"));
            let entries = audit.entries();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].scope.pane_ids, vec![7]);
            assert!(!serde_json::to_string(&entries).unwrap().contains("RAW_SECRET_DIAGNOSTIC"));
        }

        #[test]
        fn invalid_limits_and_pre_cancelled_context_do_not_open_the_reader() {
            struct NeverOpen;
            impl RecorderEventReader for NeverOpen {
                fn open_cursor(&self, _: RecorderOffset) -> Result<Box<dyn RecorderEventCursor>, EventCursorError> {
                    panic!("reader must not open");
                }
                fn head_offset(&self) -> Result<RecorderOffset, EventCursorError> {
                    panic!("reader must not open");
                }
            }
            let cx = Cx::for_testing();
            let actor = ActorIdentity::new(ActorKind::Human, "test");
            let audit = AuditLog::new(AuditLogConfig::default());
            let mut limits = request();
            limits.max_events = 0;
            assert_eq!(
                export_casr_with_cx(&cx, &NeverOpen, &actor, &limits, &audit, 0).unwrap_err(),
                CasrExportError::InvalidLimits
            );
            cx.cancel_with(crate::outcome::CancelKind::User, Some("test"));
            assert_eq!(
                export_casr_with_cx(&cx, &NeverOpen, &actor, &request(), &audit, 0).unwrap_err(),
                CasrExportError::Cancelled
            );
        }

        #[test]
        fn absent_pane_and_unrepresentable_timestamp_are_typed_refusals() {
            assert_eq!(
                export(&Snapshot::new(vec![output("other", 8)]), ActorKind::Human, request()).unwrap_err(),
                CasrExportError::NoMatchingEvents
            );
            let mut event = output("selected", 7);
            event.occurred_at_ms = u64::MAX;
            assert_eq!(
                export(&Snapshot::new(vec![event]), ActorKind::Human, request()).unwrap_err(),
                CasrExportError::TimestampOutOfRange
            );
        }

        #[test]
        fn canonical_identity_is_stable_and_isolated_from_other_source_sessions() {
            let source = Snapshot::new(vec![output("public", 7)]);
            let first = bundle(&export(&source, ActorKind::Human, request()).unwrap());
            let again = bundle(&export(&source, ActorKind::Human, request()).unwrap());
            assert_eq!(first["sessions"][0]["session_id"], again["sessions"][0]["session_id"]);
            let mut other = source.clone();
            other.events[0].event_id = "ANOTHER_PRIVATE_EVENT_ID".into();
            let other_export = export(&other, ActorKind::Human, request()).unwrap();
            let other_json = bundle(&other_export);
            assert_ne!(first["sessions"][0]["session_id"], other_json["sessions"][0]["session_id"]);
            assert_ne!(first["sessions"][0]["source_path"], other_json["sessions"][0]["source_path"]);
            assert!(!String::from_utf8(other_export.data).unwrap().contains("ANOTHER_PRIVATE_EVENT_ID"));
        }

        #[test]
        fn snapshots_remain_independent_without_splitting_delta_redaction() {
            let mut events = vec![output("key=sk-", 7)];
            for index in 0..128 {
                let mut event = output(&format!("visible snapshot {index}"), 7);
                if let RecorderEventPayload::EgressOutput { segment_kind, .. } = &mut event.payload {
                    *segment_kind = RecorderSegmentKind::Snapshot;
                }
                events.push(event);
            }
            events.push(output("abcdefghijklmnopqrstuvwxyz123456", 7));
            let export = export(&Snapshot::new(events), ActorKind::Human, request()).unwrap();
            assert_eq!(export.events_exported, 130);
            assert_eq!(export.redacted_events, 2);
            let decoded = bundle(&export);
            let messages = decoded["sessions"][0]["messages"].as_array().unwrap();
            assert_eq!(messages[0]["content"], REDACTED_MARKER);
            assert_eq!(messages[129]["content"], REDACTED_MARKER);
            assert_eq!(messages[1]["role"]["Other"], "terminal_snapshot");
            assert_eq!(messages[1]["content"], "visible snapshot 0");
            assert_eq!(messages[128]["content"], "visible snapshot 127");
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::ActorKind;
    use crate::recorder_audit::AuditLogConfig;
    use crate::recorder_query::InMemoryEventStore;

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
        assert_eq!(result.data.lines().count(), 4);
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
        let req = ExportRequest {
            pane_ids: vec![2],
            ..Default::default()
        };

        let result = exporter.export(&human(), &req, NOW).unwrap();
        assert_eq!(result.event_count, 1); // Only pane 2 event.
    }

    #[test]
    fn export_with_kind_filter() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest {
            kind_filter: vec![QueryEventKind::LifecycleMarker],
            ..Default::default()
        };

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
        let req = ExportRequest {
            max_sensitivity: Some(SensitivityTier::T3Restricted),
            ..Default::default()
        };
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

    // ── Batch 2: DarkBadger wa-1u90p.7.1 ─────────────────────────────────

    // ── ExportFormat traits ──

    #[test]
    fn export_format_debug() {
        assert_eq!(format!("{:?}", ExportFormat::JsonLines), "JsonLines");
        assert_eq!(format!("{:?}", ExportFormat::Csv), "Csv");
        assert_eq!(format!("{:?}", ExportFormat::Transcript), "Transcript");
    }

    #[test]
    fn export_format_clone_and_copy() {
        let fmt = ExportFormat::Csv;
        let cloned = fmt;
        let copied = fmt;
        assert_eq!(fmt, cloned);
        assert_eq!(fmt, copied);
    }

    #[test]
    fn export_format_eq_and_ne() {
        assert_eq!(ExportFormat::JsonLines, ExportFormat::JsonLines);
        assert_ne!(ExportFormat::JsonLines, ExportFormat::Csv);
        assert_ne!(ExportFormat::Csv, ExportFormat::Transcript);
    }

    #[test]
    fn export_format_hash_distinct() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ExportFormat::JsonLines);
        set.insert(ExportFormat::Csv);
        set.insert(ExportFormat::Transcript);
        assert_eq!(set.len(), 3);
        // Inserting duplicate should not increase size
        set.insert(ExportFormat::Csv);
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn export_format_serde_roundtrip() {
        for fmt in [
            ExportFormat::JsonLines,
            ExportFormat::Csv,
            ExportFormat::Transcript,
        ] {
            let json = serde_json::to_string(&fmt).unwrap();
            let decoded: ExportFormat = serde_json::from_str(&json).unwrap();
            assert_eq!(fmt, decoded);
        }
    }

    #[test]
    fn export_format_serde_values() {
        // Verify the snake_case rename
        assert_eq!(
            serde_json::to_string(&ExportFormat::JsonLines).unwrap(),
            "\"json_lines\""
        );
        assert_eq!(
            serde_json::to_string(&ExportFormat::Csv).unwrap(),
            "\"csv\""
        );
        assert_eq!(
            serde_json::to_string(&ExportFormat::Transcript).unwrap(),
            "\"transcript\""
        );
    }

    // ── ExportRequest constructors and required_tier ──

    #[test]
    fn export_request_transcript_constructor() {
        let req = ExportRequest::transcript(1000, 5000);
        assert_eq!(req.format, ExportFormat::Transcript);
        assert_eq!(req.time_range.unwrap().start_ms, 1000);
        assert_eq!(req.time_range.unwrap().end_ms, 5000);
        assert!(req.pane_ids.is_empty());
        assert!(req.include_text);
    }

    #[test]
    fn export_request_csv_for_panes_constructor() {
        let req = ExportRequest::csv_for_panes(vec![10, 20, 30]);
        assert_eq!(req.format, ExportFormat::Csv);
        assert_eq!(req.pane_ids, vec![10, 20, 30]);
        assert!(req.time_range.is_none());
    }

    #[test]
    fn export_request_required_tier_a1_single_pane_with_text() {
        // Single pane with text → A1RedactedQuery
        let req = ExportRequest {
            pane_ids: vec![1],
            include_text: true,
            ..Default::default()
        };
        assert_eq!(req.required_tier(), AccessTier::A1RedactedQuery);
    }

    #[test]
    fn export_request_required_tier_a1_no_pane_filter() {
        // No pane filter, with text → A1RedactedQuery
        let req = ExportRequest::default();
        assert_eq!(req.required_tier(), AccessTier::A1RedactedQuery);
    }

    #[test]
    fn export_request_default_values() {
        let req = ExportRequest::default();
        assert_eq!(req.format, ExportFormat::JsonLines);
        assert!(req.time_range.is_none());
        assert!(req.pane_ids.is_empty());
        assert!(req.kind_filter.is_empty());
        assert_eq!(req.max_events, 0);
        assert!(req.include_text);
        assert!(req.max_sensitivity.is_none());
        assert!(req.label.is_none());
    }

    #[test]
    fn export_request_debug_and_clone() {
        let req = ExportRequest::jsonl(100, 200).with_label("debug-test");
        let debug_str = format!("{:?}", req);
        assert!(debug_str.contains("ExportRequest"));
        assert!(debug_str.contains("debug-test"));

        let cloned = req.clone();
        assert_eq!(cloned.format, ExportFormat::JsonLines);
        assert_eq!(cloned.label.as_deref(), Some("debug-test"));
    }

    // ── ExportError traits and Display coverage ──

    #[test]
    fn export_error_display_no_matching_events() {
        let err = ExportError::NoMatchingEvents;
        assert_eq!(err.to_string(), "no events match export criteria");
    }

    #[test]
    fn export_error_display_format_error() {
        let err = ExportError::FormatError("bad json".to_string());
        let msg = err.to_string();
        assert!(msg.contains("format error"));
        assert!(msg.contains("bad json"));
    }

    #[test]
    fn export_error_display_elevation_required() {
        let err = ExportError::ElevationRequired {
            required_tier: AccessTier::A3PrivilegedRaw,
            current_tier: AccessTier::A1RedactedQuery,
        };
        let msg = err.to_string();
        assert!(msg.contains("elevation required"));
    }

    #[test]
    fn export_error_clone_and_eq() {
        let err1 = ExportError::NoMatchingEvents;
        let err2 = err1.clone();
        assert_eq!(err1, err2);

        let err3 = ExportError::TooLarge {
            event_count: 100,
            max_events: 50,
        };
        let err4 = err3.clone();
        assert_eq!(err3, err4);

        assert_ne!(err1, err3);
    }

    #[test]
    fn export_error_debug() {
        let err = ExportError::FormatError("test".to_string());
        let debug = format!("{:?}", err);
        assert!(debug.contains("FormatError"));
        assert!(debug.contains("test"));
    }

    #[test]
    fn export_error_is_std_error() {
        let err: Box<dyn std::error::Error> =
            Box::new(ExportError::FormatError("oops".to_string()));
        assert!(err.to_string().contains("oops"));
    }

    // ── ExportResult traits ──

    #[test]
    fn export_result_debug_and_clone() {
        let result = ExportResult {
            data: "test data".to_string(),
            event_count: 3,
            format: ExportFormat::Csv,
            redaction_applied: true,
            effective_tier: AccessTier::A1RedactedQuery,
            data_bytes: 9,
        };
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("ExportResult"));
        assert!(debug_str.contains("Csv"));

        let cloned = result.clone();
        assert_eq!(cloned.event_count, 3);
        assert_eq!(cloned.format, ExportFormat::Csv);
        assert!(cloned.redaction_applied);
        assert_eq!(cloned.effective_tier, AccessTier::A1RedactedQuery);
        assert_eq!(cloned.data_bytes, 9);
        assert_eq!(cloned.data, "test data");
    }

    // ── Exporter with max events ──

    #[test]
    fn exporter_with_max_events_sets_limit() {
        let store = InMemoryEventStore::new();
        let executor = RecorderQueryExecutor::new(store, AuditLog::new(AuditLogConfig::default()));
        let exporter = RecorderExporter::new(executor).with_max_events(42);
        assert_eq!(exporter.max_export_events, 42);
    }

    #[test]
    fn default_max_export_events_constant() {
        assert_eq!(DEFAULT_MAX_EXPORT_EVENTS, 50_000);
    }

    // ── Transcript format edge cases ──

    #[test]
    fn transcript_with_long_text_truncated() {
        // Create an event with text longer than 200 chars
        let long_text = "x".repeat(300);
        let exporter = test_exporter(vec![make_event(1, 0, 1000, &long_text)]);
        let req = ExportRequest::transcript(0, 2000);

        let result = exporter.export(&human(), &req, NOW).unwrap();
        // The transcript should truncate the text
        assert!(result.data.contains("..."));
        // Should not contain the full 300-char text
        assert!(!result.data.contains(&long_text));
    }

    #[test]
    fn transcript_header_contains_metadata() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest::transcript(0, 5000);
        let result = exporter.export(&human(), &req, NOW).unwrap();

        assert!(result.data.contains("# Flight Recorder Transcript"));
        assert!(result.data.contains("# Time range:"));
        assert!(result.data.contains("# Panes:"));
        assert!(result.data.contains("# Events: 4"));
    }

    // ── CSV content validation ──

    #[test]
    fn csv_data_rows_match_event_count() {
        let exporter = test_exporter(sample_events());
        let req = ExportRequest {
            format: ExportFormat::Csv,
            ..Default::default()
        };
        let result = exporter.export(&human(), &req, NOW).unwrap();
        // Header + 4 data rows
        assert_eq!(result.data.lines().count(), 5);
        assert_eq!(result.event_count, 4);
    }

    // ── ExportRequest serde skip_serializing_if ──

    #[test]
    fn export_request_serde_skips_none_fields() {
        let req = ExportRequest::default();
        let json = serde_json::to_string(&req).unwrap();
        // time_range, max_sensitivity, and label are None → should be skipped
        assert!(!json.contains("time_range"));
        assert!(!json.contains("max_sensitivity"));
        assert!(!json.contains("label"));
    }

    #[test]
    fn export_request_serde_includes_set_fields() {
        let req = ExportRequest::jsonl(100, 200).with_label("my-export");
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("time_range"));
        assert!(json.contains("\"label\":\"my-export\""));
    }
}
