//! Telemetry export surface for `vibe_cockpit` collectors and CLI bridge.
//!
//! Provides two complementary surfaces behind the `vc-export` feature:
//! 1. **Cursor-based FrankenTerm event export** (`export_events_page_with_cx`):
//!    monotone, epoch-aware, retention-gap-checked, secret-redacted, bounded
//!    JSON / JSONL export over FrankenTerm's SQLite flight recorder
//!    (`StorageHandle`) for external `vibe_cockpit` collectors.
//! 2. **Outbound `vibe_cockpit` CLI bridge** (`VcExport`): wraps the optional
//!    `vibe_cockpit` binary via [`SubprocessBridge`] with fail-open degradation
//!    when `vibe_cockpit` is absent.
//!
//! Feature-gated behind `vc-export`.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, warn};

use crate::redactor::Redactor;
use crate::robot_envelope::RobotEnvelope;
use crate::storage::{
    EventRetentionSnapshot, EventRetentionStatus, EventStreamQuery, StorageHandle, StoredEvent,
};
use crate::subprocess_bridge::SubprocessBridge;

/// Stable schema identifier for cursor-based `vibe_cockpit` exports.
pub const VC_EXPORT_SCHEMA_VERSION: &str = "ft.vc_export.v1";
/// Default page size when a caller does not specify an explicit limit.
pub const VC_EXPORT_DEFAULT_LIMIT: usize = 100;
/// Hard upper bound on records returned by a single cursor page.
pub const VC_EXPORT_MAX_LIMIT: usize = 500;
/// Maximum serialized byte length accepted for an incoming cursor token.
pub const VC_EXPORT_MAX_CURSOR_LEN: usize = 256;

/// Opaque/structured monotone cursor returned by [`export_events_page_with_cx`].
///
/// Binds the numeric `after_id` to storage's durable `cursor_epoch` and
/// retention `generation`, allowing collectors to distinguish ordinary filter
/// gaps from retention pruning, database resets, or restored backups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcExportCursor {
    /// Durable retention epoch reported by SQLite storage.
    pub cursor_epoch: String,
    /// Last exported event ID (exclusive lower bound for the next request).
    pub after_id: i64,
    /// Last observed retention generation when this cursor was issued.
    pub generation: i64,
}

impl VcExportCursor {
    /// Encode into a compact, deterministic cursor token (`v1:<epoch>:<after_id>:<generation>`).
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "v1:{}:{}:{}",
            self.cursor_epoch, self.after_id, self.generation
        )
    }

    /// Parse a previously encoded cursor token.
    pub fn parse(raw: &str) -> Result<Self, VcExportError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.len() > VC_EXPORT_MAX_CURSOR_LEN {
            return Err(VcExportError::InvalidCursor {
                reason: "cursor token empty or exceeds maximum length",
            });
        }
        let mut parts = trimmed.split(':');
        let version = parts.next().ok_or(VcExportError::InvalidCursor {
            reason: "missing cursor version prefix",
        })?;
        if version != "v1" {
            return Err(VcExportError::InvalidCursor {
                reason: "unsupported cursor version (expected v1)",
            });
        }
        let cursor_epoch = parts.next().ok_or(VcExportError::InvalidCursor {
            reason: "missing cursor epoch",
        })?;
        let after_id_raw = parts.next().ok_or(VcExportError::InvalidCursor {
            reason: "missing cursor after_id",
        })?;
        let generation_raw = parts.next().ok_or(VcExportError::InvalidCursor {
            reason: "missing cursor generation",
        })?;
        if parts.next().is_some() {
            return Err(VcExportError::InvalidCursor {
                reason: "unexpected trailing cursor segments",
            });
        }
        if cursor_epoch.is_empty()
            || !cursor_epoch
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(VcExportError::InvalidCursor {
                reason: "cursor epoch contains invalid characters",
            });
        }
        let after_id: i64 = after_id_raw
            .parse()
            .map_err(|_| VcExportError::InvalidCursor {
                reason: "cursor after_id is not a valid integer",
            })?;
        let generation: i64 = generation_raw
            .parse()
            .map_err(|_| VcExportError::InvalidCursor {
                reason: "cursor generation is not a valid integer",
            })?;
        if after_id < 0 || generation < 0 {
            return Err(VcExportError::InvalidCursor {
                reason: "cursor numeric fields must be non-negative",
            });
        }
        Ok(Self {
            cursor_epoch: cursor_epoch.to_string(),
            after_id,
            generation,
        })
    }
}

/// Request parameters for [`export_events_page_with_cx`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcExportRequest {
    /// Optional resume cursor obtained from the previous page.
    pub cursor: Option<VcExportCursor>,
    /// Maximum number of records to return (`1..=VC_EXPORT_MAX_LIMIT`).
    pub limit: usize,
    /// Optional pane ID filter.
    pub pane_id: Option<u64>,
    /// Optional exact rule ID filter.
    pub rule_id: Option<String>,
    /// Optional exact event type filter.
    pub event_type: Option<String>,
    /// Optional lower timestamp bound (epoch ms).
    pub since: Option<i64>,
    /// Optional upper timestamp bound (epoch ms).
    pub until: Option<i64>,
    /// Include secret-redacted `matched_text` excerpts in exported records.
    pub include_matched_text: bool,
}

impl Default for VcExportRequest {
    fn default() -> Self {
        Self {
            cursor: None,
            limit: VC_EXPORT_DEFAULT_LIMIT,
            pane_id: None,
            rule_id: None,
            event_type: None,
            since: None,
            until: None,
            include_matched_text: false,
        }
    }
}

/// Stable, redacted event record emitted for external `vibe_cockpit` collectors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VcExportRecord {
    /// Globally stable identity scoped to storage's durable retention epoch.
    pub record_id: String,
    /// Monotone SQLite event row ID within `cursor_epoch`.
    pub event_id: i64,
    /// Pane that produced the event.
    pub pane_id: u64,
    /// Pattern rule identifier.
    pub rule_id: String,
    /// Detected agent type.
    pub agent_type: String,
    /// Event category/type.
    pub event_type: String,
    /// Event severity label.
    pub severity: String,
    /// Pattern detection confidence score.
    pub confidence: f64,
    /// Detection timestamp (epoch ms).
    pub detected_at: i64,
    /// Optional source capture segment ID.
    pub segment_id: Option<i64>,
    /// Optional deduplication key.
    pub dedupe_key: Option<String>,
    /// Optional handling timestamp (epoch ms).
    pub handled_at: Option<i64>,
    /// Optional handling status.
    pub handled_status: Option<String>,
    /// Secret-redacted matched text excerpt (omitted unless requested).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_text: Option<String>,
    /// Secret-redacted extracted structured metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted: Option<serde_json::Value>,
}

/// Explicit continuity / gap classification accompanying every exported page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VcExportGapState {
    /// No retention deletion or epoch reset occurred across the resumed range.
    Continuous,
    /// Retention pruned one or more event IDs in `(after_id, through_id]`.
    RetentionPruned {
        start_id: i64,
        end_id: i64,
        deleted_event_count: i64,
    },
    /// Database retention epoch changed (e.g. fresh DB, reset, or ledger rotation).
    /// No records are synthesized across an epoch boundary; collectors must reset
    /// their cursor and resync from the new epoch.
    EpochReset {
        expected_epoch: String,
        current_epoch: String,
    },
    /// Caller's cursor `after_id` exceeds storage's durable high-water mark
    /// (e.g. database restored from an older snapshot).
    CursorAheadOfHighWater {
        requested_after_id: i64,
        max_event_id: i64,
    },
    /// Database upgraded from legacy history before the retention ledger began.
    LegacyHistoryUnavailable { evidence_from_event_id: i64 },
}

/// One bounded, redacted page of FrankenTerm events for `vibe_cockpit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VcExportPage {
    /// Versioned contract identifier (`ft.vc_export.v1`).
    pub schema_version: String,
    /// Ordered ascending by `event_id`.
    pub records: Vec<VcExportRecord>,
    /// Monotone cursor to pass on the subsequent poll.
    pub next_cursor: VcExportCursor,
    /// Encoded string form of `next_cursor`.
    pub next_cursor_token: String,
    /// True if at least one more matching event exists after `next_cursor`.
    pub has_more: bool,
    /// Explicit continuity / retention-gap / restart classification.
    pub gap: VcExportGapState,
    /// Raw durable retention snapshot observed atomically with the page query.
    pub retention: EventRetentionSnapshot,
    /// Always true: every exported text/JSON payload passes through `Redactor`.
    pub redaction_applied: bool,
    /// Export timestamp (epoch ms).
    pub exported_at_ms: i64,
}

impl VcExportPage {
    /// Render exported records as newline-delimited JSON (JSONL) suitable for
    /// streaming collector pipelines.
    pub fn to_jsonl(&self) -> Result<String, serde_json::Error> {
        let mut out = String::new();
        for record in &self.records {
            let line = serde_json::to_string(record)?;
            out.push_str(&line);
            out.push('\n');
        }
        Ok(out)
    }
}

/// Typed, content-free errors returned by cursor-based VC export.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum VcExportError {
    /// Incoming cursor token was malformed or failed validation.
    #[error("invalid vc-export cursor: {reason}")]
    InvalidCursor { reason: &'static str },
    /// Requested page size fell outside `1..=VC_EXPORT_MAX_LIMIT`.
    #[error("invalid vc-export limit {requested}: must be in 1..={max}")]
    InvalidLimit { requested: usize, max: usize },
    /// Caller `Cx` was cancelled before the export completed.
    #[error("vc-export cancelled")]
    Cancelled,
    /// Underlying storage query failed.
    #[error("vc-export storage query failed ({phase})")]
    StorageFailed { phase: &'static str },
}

fn redact_text(redactor: &Redactor, text: &str) -> String {
    redactor.redact(&crate::output::normalize_terminal_text_for_redaction(text))
}

fn redact_json_value(redactor: &Redactor, value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(redact_text(redactor, &s)),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .into_iter()
                .map(|item| redact_json_value(redactor, item))
                .collect(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, redact_json_value(redactor, v)))
                .collect(),
        ),
        primitive => primitive,
    }
}

fn stored_event_to_vc_record(
    event: StoredEvent,
    cursor_epoch: &str,
    include_matched_text: bool,
    redactor: &Redactor,
) -> VcExportRecord {
    let matched_text = if include_matched_text {
        event.matched_text.map(|text| redact_text(redactor, &text))
    } else {
        None
    };
    let extracted = event.extracted.map(|val| redact_json_value(redactor, val));

    VcExportRecord {
        record_id: format!("ft-evt:{cursor_epoch}:{}", event.id),
        event_id: event.id,
        pane_id: event.pane_id,
        rule_id: event.rule_id,
        agent_type: event.agent_type,
        event_type: event.event_type,
        severity: event.severity,
        confidence: event.confidence,
        detected_at: event.detected_at,
        segment_id: event.segment_id,
        dedupe_key: event.dedupe_key,
        handled_at: event.handled_at,
        handled_status: event
            .handled_status
            .map(|text| redact_text(redactor, &text)),
        matched_text,
        extracted,
    }
}

fn now_epoch_ms_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Export one bounded, secret-redacted, cursor-addressed page of FrankenTerm
/// events for external `vibe_cockpit` collectors.
///
/// Guarantees:
/// - **Monotone cursor**: records are strictly ascending by `event_id`;
///   `next_cursor.after_id` never moves backwards within an epoch.
/// - **Bounded allocation**: `limit` is validated to `1..=VC_EXPORT_MAX_LIMIT`.
/// - **Redaction**: `matched_text`, `handled_status`, and all string leaves
///   inside `extracted` are terminal-normalized and pass through the storage
///   handle's configured [`Redactor`] before serialization, including history
///   written before that policy was configured.
/// - **Explicit restart/retention-gap reporting**: when resuming from a cursor,
///   epoch mismatches and cursor-ahead-of-high-water conditions fail closed
///   with zero fabricated records and an explicit [`VcExportGapState`], and
///   retention deletions surface their exact deleted ID interval.
pub async fn export_events_page_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    request: &VcExportRequest,
) -> Result<VcExportPage, VcExportError> {
    if cx.checkpoint().is_err() {
        return Err(VcExportError::Cancelled);
    }
    if request.limit == 0 || request.limit > VC_EXPORT_MAX_LIMIT {
        return Err(VcExportError::InvalidLimit {
            requested: request.limit,
            max: VC_EXPORT_MAX_LIMIT,
        });
    }

    let query = EventStreamQuery {
        after_id: request.cursor.as_ref().map(|c| c.after_id),
        limit: Some(request.limit.saturating_add(1)),
        pane_id: request.pane_id,
        rule_id: request.rule_id.clone(),
        event_type: request.event_type.clone(),
        triage_state: None,
        label: None,
        unhandled_only: false,
        since: request.since,
        until: request.until,
    };

    let page = storage
        .get_events_stream_page_with_cx(cx, query)
        .await
        .map_err(|_| {
            if cx.checkpoint().is_err() {
                VcExportError::Cancelled
            } else {
                VcExportError::StorageFailed {
                    phase: "events_stream_page",
                }
            }
        })?;

    if cx.checkpoint().is_err() {
        return Err(VcExportError::Cancelled);
    }

    let retention = page.retention;
    let mut events = page.events;
    let has_more_raw = events.len() > request.limit;
    if has_more_raw {
        events.truncate(request.limit);
    }

    let exported_at_ms = now_epoch_ms_i64();

    if let Some(prev_cursor) = request.cursor.as_ref() {
        let through_id = events
            .last()
            .map_or(retention.max_event_id, |ev| ev.id)
            .max(prev_cursor.after_id);
        let check = storage
            .check_event_retention_in_epoch_with_cx(
                cx,
                prev_cursor.after_id,
                through_id,
                &prev_cursor.cursor_epoch,
            )
            .await
            .map_err(|_| {
                if cx.checkpoint().is_err() {
                    VcExportError::Cancelled
                } else {
                    VcExportError::StorageFailed {
                        phase: "retention_check",
                    }
                }
            })?;

        match check.status {
            EventRetentionStatus::CursorEpochMismatch => {
                let reset_cursor = VcExportCursor {
                    cursor_epoch: check.snapshot.cursor_epoch.clone(),
                    after_id: 0,
                    generation: check.snapshot.generation,
                };
                let token = reset_cursor.encode();
                return Ok(VcExportPage {
                    schema_version: VC_EXPORT_SCHEMA_VERSION.to_string(),
                    records: Vec::new(),
                    next_cursor: reset_cursor,
                    next_cursor_token: token,
                    has_more: check.snapshot.max_event_id > 0,
                    gap: VcExportGapState::EpochReset {
                        expected_epoch: prev_cursor.cursor_epoch.clone(),
                        current_epoch: check.snapshot.cursor_epoch.clone(),
                    },
                    retention: check.snapshot,
                    redaction_applied: true,
                    exported_at_ms,
                });
            }
            EventRetentionStatus::CursorAheadOfHighWater { max_event_id } => {
                let clamped_cursor = VcExportCursor {
                    cursor_epoch: check.snapshot.cursor_epoch.clone(),
                    after_id: max_event_id,
                    generation: check.snapshot.generation,
                };
                let token = clamped_cursor.encode();
                return Ok(VcExportPage {
                    schema_version: VC_EXPORT_SCHEMA_VERSION.to_string(),
                    records: Vec::new(),
                    next_cursor: clamped_cursor,
                    next_cursor_token: token,
                    has_more: false,
                    gap: VcExportGapState::CursorAheadOfHighWater {
                        requested_after_id: prev_cursor.after_id,
                        max_event_id,
                    },
                    retention: check.snapshot,
                    redaction_applied: true,
                    exported_at_ms,
                });
            }
            EventRetentionStatus::Pruned { interval } => {
                let redactor = storage.redactor();
                let next_after_id = events
                    .last()
                    .map_or(prev_cursor.after_id, |ev| ev.id.max(prev_cursor.after_id));
                let records = events
                    .into_iter()
                    .map(|ev| {
                        stored_event_to_vc_record(
                            ev,
                            &check.snapshot.cursor_epoch,
                            request.include_matched_text,
                            redactor,
                        )
                    })
                    .collect();
                let next_cursor = VcExportCursor {
                    cursor_epoch: check.snapshot.cursor_epoch.clone(),
                    after_id: next_after_id,
                    generation: check.snapshot.generation,
                };
                let token = next_cursor.encode();
                return Ok(VcExportPage {
                    schema_version: VC_EXPORT_SCHEMA_VERSION.to_string(),
                    records,
                    next_cursor,
                    next_cursor_token: token,
                    has_more: has_more_raw,
                    gap: VcExportGapState::RetentionPruned {
                        start_id: interval.start_id,
                        end_id: interval.end_id,
                        deleted_event_count: check.snapshot.deleted_event_count,
                    },
                    retention: check.snapshot,
                    redaction_applied: true,
                    exported_at_ms,
                });
            }
            EventRetentionStatus::LegacyHistoryUnavailable {
                evidence_from_event_id,
            } => {
                let redactor = storage.redactor();
                let next_after_id = events
                    .last()
                    .map_or(prev_cursor.after_id, |ev| ev.id.max(prev_cursor.after_id));
                let records = events
                    .into_iter()
                    .map(|ev| {
                        stored_event_to_vc_record(
                            ev,
                            &check.snapshot.cursor_epoch,
                            request.include_matched_text,
                            redactor,
                        )
                    })
                    .collect();
                let next_cursor = VcExportCursor {
                    cursor_epoch: check.snapshot.cursor_epoch.clone(),
                    after_id: next_after_id,
                    generation: check.snapshot.generation,
                };
                let token = next_cursor.encode();
                return Ok(VcExportPage {
                    schema_version: VC_EXPORT_SCHEMA_VERSION.to_string(),
                    records,
                    next_cursor,
                    next_cursor_token: token,
                    has_more: has_more_raw,
                    gap: VcExportGapState::LegacyHistoryUnavailable {
                        evidence_from_event_id,
                    },
                    retention: check.snapshot,
                    redaction_applied: true,
                    exported_at_ms,
                });
            }
            EventRetentionStatus::CompleteNoPruning => {}
        }
    }

    let redactor = storage.redactor();
    let baseline_after_id = request.cursor.as_ref().map_or(0, |c| c.after_id);
    let next_after_id = events
        .last()
        .map_or(baseline_after_id, |ev| ev.id.max(baseline_after_id));
    let records = events
        .into_iter()
        .map(|ev| {
            stored_event_to_vc_record(
                ev,
                &retention.cursor_epoch,
                request.include_matched_text,
                redactor,
            )
        })
        .collect();
    let next_cursor = VcExportCursor {
        cursor_epoch: retention.cursor_epoch.clone(),
        after_id: next_after_id,
        generation: retention.generation,
    };
    let next_cursor_token = next_cursor.encode();

    Ok(VcExportPage {
        schema_version: VC_EXPORT_SCHEMA_VERSION.to_string(),
        records,
        next_cursor,
        next_cursor_token,
        has_more: has_more_raw,
        gap: VcExportGapState::Continuous,
        retention,
        redaction_applied: true,
        exported_at_ms,
    })
}

/// Session telemetry snapshot from vibe_cockpit.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SessionTelemetry {
    #[serde(default)]
    pub session_id: Option<String>,
    /// Duration in seconds.
    #[serde(default)]
    pub duration_secs: f64,
    /// Number of commands executed in the session.
    #[serde(default)]
    pub commands: usize,
    /// Number of errors encountered.
    #[serde(default)]
    pub errors: usize,
    /// Number of agent interactions (tool calls, messages).
    #[serde(default)]
    pub agent_interactions: usize,
    /// Forward-compatibility for extra fields.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl SessionTelemetry {
    fn degraded(session_id: &str) -> Self {
        Self {
            session_id: Some(session_id.to_string()),
            ..Self::default()
        }
    }
}

/// Agent-level metrics from vibe_cockpit.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct AgentMetrics {
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Total tokens consumed.
    #[serde(default)]
    pub total_tokens: u64,
    /// Number of tool invocations.
    #[serde(default)]
    pub tool_calls: usize,
    /// Session count for this agent.
    #[serde(default)]
    pub sessions: usize,
    /// Average session duration in seconds.
    #[serde(default)]
    pub avg_session_secs: f64,
    /// Forward-compatibility for extra fields.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl AgentMetrics {
    fn degraded(agent_id: &str) -> Self {
        Self {
            agent_id: Some(agent_id.to_string()),
            ..Self::default()
        }
    }
}

/// Vibe cockpit telemetry export bridge.
#[derive(Debug, Clone)]
pub struct VcExport {
    session_bridge: SubprocessBridge<SessionTelemetry>,
    metrics_bridge: SubprocessBridge<AgentMetrics>,
}

impl VcExport {
    /// Create a new export bridge looking for `vibe_cockpit` in PATH and `/dp`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            session_bridge: SubprocessBridge::new("vibe_cockpit"),
            metrics_bridge: SubprocessBridge::new("vibe_cockpit"),
        }
    }

    /// Check whether the `vibe_cockpit` binary can be found.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.session_bridge.is_available()
    }

    /// Export session telemetry for a given session ID.
    ///
    /// Fail-open behavior: returns default/degraded telemetry on any bridge error.
    #[must_use]
    pub fn export_session_telemetry(&self, session_id: &str) -> SessionTelemetry {
        self.try_export_session_telemetry(session_id)
            .unwrap_or_else(|| SessionTelemetry::degraded(session_id))
    }

    /// Export agent-level metrics for a given agent ID.
    ///
    /// Fail-open behavior: returns default/degraded metrics on any bridge error.
    #[must_use]
    pub fn export_agent_metrics(&self, agent_id: &str) -> AgentMetrics {
        self.try_export_agent_metrics(agent_id)
            .unwrap_or_else(|| AgentMetrics::degraded(agent_id))
    }

    /// Export session telemetry wrapped in a standardized robot envelope.
    #[must_use]
    pub fn export_session_telemetry_envelope(
        &self,
        session_id: &str,
    ) -> RobotEnvelope<SessionTelemetry> {
        match self.try_export_session_telemetry(session_id) {
            Some(telemetry) => RobotEnvelope::wrap("vibe_cockpit", telemetry),
            None => {
                RobotEnvelope::wrap_degraded("vibe_cockpit", SessionTelemetry::degraded(session_id))
            }
        }
    }

    /// Export agent metrics wrapped in a standardized robot envelope.
    #[must_use]
    pub fn export_agent_metrics_envelope(&self, agent_id: &str) -> RobotEnvelope<AgentMetrics> {
        match self.try_export_agent_metrics(agent_id) {
            Some(metrics) => RobotEnvelope::wrap("vibe_cockpit", metrics),
            None => RobotEnvelope::wrap_degraded("vibe_cockpit", AgentMetrics::degraded(agent_id)),
        }
    }

    fn try_export_session_telemetry(&self, session_id: &str) -> Option<SessionTelemetry> {
        match self
            .session_bridge
            .invoke(&["export", "--format=json", "--session", session_id])
        {
            Ok(mut telemetry) => {
                if telemetry.session_id.is_none() {
                    telemetry.session_id = Some(session_id.to_string());
                }
                debug!(
                    bridge = "vibe_cockpit",
                    session = session_id,
                    metrics_count = telemetry.agent_interactions,
                    "session telemetry exported"
                );
                Some(telemetry)
            }
            Err(err) => {
                warn!(
                    bridge = "vibe_cockpit",
                    session = session_id,
                    error = %err,
                    "session telemetry export failed; using degraded fallback"
                );
                None
            }
        }
    }

    fn try_export_agent_metrics(&self, agent_id: &str) -> Option<AgentMetrics> {
        match self
            .metrics_bridge
            .invoke(&["metrics", "--format=json", "--agent", agent_id])
        {
            Ok(mut metrics) => {
                if metrics.agent_id.is_none() {
                    metrics.agent_id = Some(agent_id.to_string());
                }
                debug!(
                    bridge = "vibe_cockpit",
                    agent = agent_id,
                    metrics_count = metrics.tool_calls,
                    "agent metrics exported"
                );
                Some(metrics)
            }
            Err(err) => {
                warn!(
                    bridge = "vibe_cockpit",
                    agent = agent_id,
                    error = %err,
                    "agent metrics export failed; using degraded fallback"
                );
                None
            }
        }
    }
}

impl Default for VcExport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(unix)]
    fn fixture_vc_export(script_body: &str) -> (tempfile::TempDir, VcExport) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("vibe_cockpit");
        write_executable(&bin, script_body);

        let bin_path = bin.to_string_lossy().into_owned();
        let session_bridge: SubprocessBridge<SessionTelemetry> = SubprocessBridge::new(&bin_path);
        let metrics_bridge: SubprocessBridge<AgentMetrics> = SubprocessBridge::new(&bin_path);

        (
            dir,
            VcExport {
                session_bridge,
                metrics_bridge,
            },
        )
    }

    #[test]
    fn test_session_telemetry_default_values() {
        let t = SessionTelemetry::default();
        assert!(t.session_id.is_none());
        assert!(t.duration_secs.abs() < f64::EPSILON);
        assert_eq!(t.commands, 0);
        assert_eq!(t.errors, 0);
        assert_eq!(t.agent_interactions, 0);
    }

    #[test]
    fn test_session_telemetry_full_parse() {
        let json = r#"{
            "session_id": "sess-123",
            "duration_secs": 3600.5,
            "commands": 42,
            "errors": 3,
            "agent_interactions": 15
        }"#;
        let t: SessionTelemetry = serde_json::from_str(json).unwrap();
        assert_eq!(t.session_id, Some("sess-123".to_string()));
        assert!((t.duration_secs - 3600.5).abs() < 0.01);
        assert_eq!(t.commands, 42);
        assert_eq!(t.errors, 3);
        assert_eq!(t.agent_interactions, 15);
    }

    #[test]
    fn test_session_telemetry_forward_compat() {
        let json = r#"{"new_field": "surprise", "commands": 5}"#;
        let t: SessionTelemetry = serde_json::from_str(json).unwrap();
        assert_eq!(t.commands, 5);
        assert!(t.extra.contains_key("new_field"));
    }

    #[test]
    fn test_session_telemetry_serde_roundtrip() {
        let t = SessionTelemetry {
            session_id: Some("abc".to_string()),
            duration_secs: 120.0,
            commands: 10,
            errors: 1,
            agent_interactions: 5,
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: SessionTelemetry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn test_session_telemetry_degraded_helper() {
        let t = SessionTelemetry::degraded("sess-1");
        assert_eq!(t.session_id, Some("sess-1".to_string()));
        assert_eq!(t.commands, 0);
    }

    #[test]
    fn test_agent_metrics_default_values() {
        let m = AgentMetrics::default();
        assert!(m.agent_id.is_none());
        assert_eq!(m.total_tokens, 0);
        assert_eq!(m.tool_calls, 0);
        assert_eq!(m.sessions, 0);
    }

    #[test]
    fn test_agent_metrics_full_parse() {
        let json = r#"{
            "agent_id": "agent-456",
            "total_tokens": 150000,
            "tool_calls": 200,
            "sessions": 5,
            "avg_session_secs": 1800.0
        }"#;
        let m: AgentMetrics = serde_json::from_str(json).unwrap();
        assert_eq!(m.agent_id, Some("agent-456".to_string()));
        assert_eq!(m.total_tokens, 150000);
        assert_eq!(m.tool_calls, 200);
        assert_eq!(m.sessions, 5);
    }

    #[test]
    fn test_agent_metrics_forward_compat() {
        let json = r#"{"total_tokens": 100, "cost_usd": 0.05}"#;
        let m: AgentMetrics = serde_json::from_str(json).unwrap();
        assert!(m.extra.contains_key("cost_usd"));
    }

    #[test]
    fn test_agent_metrics_serde_roundtrip() {
        let m = AgentMetrics {
            agent_id: Some("x".to_string()),
            total_tokens: 5000,
            tool_calls: 10,
            sessions: 2,
            avg_session_secs: 300.0,
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: AgentMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn test_agent_metrics_degraded_helper() {
        let m = AgentMetrics::degraded("agent-a");
        assert_eq!(m.agent_id, Some("agent-a".to_string()));
        assert_eq!(m.total_tokens, 0);
    }

    #[test]
    fn test_vc_export_new() {
        let vc = VcExport::new();
        assert_eq!(vc.session_bridge.binary_name(), "vibe_cockpit");
    }

    #[test]
    fn test_vc_export_default() {
        let vc = VcExport::default();
        assert_eq!(vc.session_bridge.binary_name(), "vibe_cockpit");
    }

    #[test]
    fn test_fail_open_when_vc_unavailable() {
        let vc = VcExport {
            session_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
            metrics_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
        };
        let telemetry = vc.export_session_telemetry("s1");
        assert_eq!(telemetry.session_id, Some("s1".to_string()));
        assert_eq!(telemetry.commands, 0);
    }

    #[test]
    fn test_fail_open_agent_metrics_when_vc_unavailable() {
        let vc = VcExport {
            session_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
            metrics_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
        };
        let metrics = vc.export_agent_metrics("a1");
        assert_eq!(metrics.agent_id, Some("a1".to_string()));
        assert_eq!(metrics.total_tokens, 0);
    }

    #[test]
    fn test_fail_open_session_envelope_when_vc_unavailable() {
        let vc = VcExport {
            session_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
            metrics_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
        };
        let envelope = vc.export_session_telemetry_envelope("s1");
        assert!(envelope.degraded);
        assert_eq!(envelope.data.session_id, Some("s1".to_string()));
    }

    #[test]
    fn test_fail_open_agent_envelope_when_vc_unavailable() {
        let vc = VcExport {
            session_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
            metrics_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
        };
        let envelope = vc.export_agent_metrics_envelope("a1");
        assert!(envelope.degraded);
        assert_eq!(envelope.data.agent_id, Some("a1".to_string()));
    }

    #[test]
    fn test_is_available_false_when_vc_missing() {
        let vc = VcExport {
            session_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
            metrics_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
        };
        assert!(!vc.is_available());
    }

    #[cfg(unix)]
    #[test]
    fn test_export_session_telemetry_parses() {
        let (_dir, vc) = fixture_vc_export(
            "#!/bin/sh\nif [ \"$1\" = \"export\" ]; then printf '{\"duration_secs\":9.0,\"commands\":2,\"errors\":1,\"agent_interactions\":4}'; else printf '{\"total_tokens\":10,\"tool_calls\":2,\"sessions\":1,\"avg_session_secs\":5.0}'; fi\n",
        );

        let telemetry = vc.export_session_telemetry("sess-42");
        assert_eq!(telemetry.session_id, Some("sess-42".to_string()));
        assert_eq!(telemetry.commands, 2);
        assert_eq!(telemetry.errors, 1);
        assert_eq!(telemetry.agent_interactions, 4);
    }

    #[cfg(unix)]
    #[test]
    fn test_export_agent_metrics_parses() {
        let (_dir, vc) = fixture_vc_export(
            "#!/bin/sh\nif [ \"$1\" = \"export\" ]; then printf '{\"duration_secs\":2.0,\"commands\":1,\"errors\":0,\"agent_interactions\":1}'; else printf '{\"total_tokens\":777,\"tool_calls\":8,\"sessions\":3,\"avg_session_secs\":12.5}'; fi\n",
        );

        let metrics = vc.export_agent_metrics("agent-z");
        assert_eq!(metrics.agent_id, Some("agent-z".to_string()));
        assert_eq!(metrics.total_tokens, 777);
        assert_eq!(metrics.tool_calls, 8);
        assert_eq!(metrics.sessions, 3);
    }

    #[cfg(unix)]
    #[test]
    fn test_export_session_envelope_not_degraded_on_success() {
        let (_dir, vc) = fixture_vc_export(
            "#!/bin/sh\nif [ \"$1\" = \"export\" ]; then printf '{\"duration_secs\":1.0,\"commands\":1,\"errors\":0,\"agent_interactions\":1}'; else printf '{\"total_tokens\":1,\"tool_calls\":1,\"sessions\":1,\"avg_session_secs\":1.0}'; fi\n",
        );
        let envelope = vc.export_session_telemetry_envelope("s-ok");
        assert!(!envelope.degraded);
        assert_eq!(envelope.source, "vibe_cockpit");
    }

    #[cfg(unix)]
    #[test]
    fn test_export_agent_envelope_not_degraded_on_success() {
        let (_dir, vc) = fixture_vc_export(
            "#!/bin/sh\nif [ \"$1\" = \"export\" ]; then printf '{\"duration_secs\":1.0,\"commands\":1,\"errors\":0,\"agent_interactions\":1}'; else printf '{\"total_tokens\":1,\"tool_calls\":9,\"sessions\":2,\"avg_session_secs\":1.0}'; fi\n",
        );
        let envelope = vc.export_agent_metrics_envelope("a-ok");
        assert!(!envelope.degraded);
        assert_eq!(envelope.source, "vibe_cockpit");
        assert_eq!(envelope.data.agent_id, Some("a-ok".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_export_fills_session_id_when_missing() {
        let (_dir, vc) = fixture_vc_export(
            "#!/bin/sh\nif [ \"$1\" = \"export\" ]; then printf '{\"duration_secs\":3.0,\"commands\":5,\"errors\":0,\"agent_interactions\":2}'; else printf '{\"total_tokens\":11,\"tool_calls\":2,\"sessions\":1,\"avg_session_secs\":5.0}'; fi\n",
        );
        let telemetry = vc.export_session_telemetry("sess-fill");
        assert_eq!(telemetry.session_id, Some("sess-fill".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_export_fills_agent_id_when_missing() {
        let (_dir, vc) = fixture_vc_export(
            "#!/bin/sh\nif [ \"$1\" = \"export\" ]; then printf '{\"duration_secs\":3.0,\"commands\":5,\"errors\":0,\"agent_interactions\":2}'; else printf '{\"total_tokens\":44,\"tool_calls\":6,\"sessions\":7,\"avg_session_secs\":8.5}'; fi\n",
        );
        let metrics = vc.export_agent_metrics("agent-fill");
        assert_eq!(metrics.agent_id, Some("agent-fill".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_invalid_json_falls_back_to_degraded_defaults() {
        let (_dir, vc) = fixture_vc_export("#!/bin/sh\nprintf 'not-json'\n");
        let telemetry = vc.export_session_telemetry("sess-bad");
        let metrics = vc.export_agent_metrics("agent-bad");
        assert_eq!(telemetry.session_id, Some("sess-bad".to_string()));
        assert_eq!(telemetry.commands, 0);
        assert_eq!(metrics.agent_id, Some("agent-bad".to_string()));
        assert_eq!(metrics.total_tokens, 0);
    }

    #[test]
    fn cursor_encode_parse_roundtrip_and_rejections() {
        let cursor = VcExportCursor {
            cursor_epoch: "epoch_abc-123".to_string(),
            after_id: 42,
            generation: 3,
        };
        let encoded = cursor.encode();
        assert_eq!(encoded, "v1:epoch_abc-123:42:3");
        assert_eq!(VcExportCursor::parse(&encoded).unwrap(), cursor);

        for bad in [
            "",
            "v2:epoch:1:0",
            "v1::1:0",
            "v1:bad/epoch:1:0",
            "v1:epoch:-1:0",
            "v1:epoch:1:-2",
            "v1:epoch:1:0:extra",
        ] {
            assert!(
                matches!(
                    VcExportCursor::parse(bad),
                    Err(VcExportError::InvalidCursor { .. })
                ),
                "expected InvalidCursor for {bad:?}"
            );
        }
    }

    #[test]
    fn vc_record_normalizes_custom_secrets_without_changing_identity() {
        let mut config = crate::config::Config::default();
        config.safety.redaction.patterns = vec!["PRIVATE_ORCHID".to_string()];
        config.safety.redaction.placeholder = "[workspace-hidden]".to_string();
        let redactor = config.safety.redaction.build_redactor().unwrap();
        let decorated = "PRIVATE_\u{1b}[31mORCHID\u{1b}[0m";
        let record = stored_event_to_vc_record(
            StoredEvent {
                id: 44,
                pane_id: 7,
                rule_id: "agent.secret_check".to_string(),
                agent_type: "codex".to_string(),
                event_type: "alert".to_string(),
                severity: "warning".to_string(),
                confidence: 0.95,
                extracted: Some(serde_json::json!({
                    "nested": [{"value": decorated}, "PRIVATE_ORCHID"],
                    "count": 3,
                    "enabled": true,
                })),
                matched_text: Some(format!("matched {decorated}")),
                segment_id: Some(9),
                detected_at: 1000,
                dedupe_key: Some("stable-dedupe-key".to_string()),
                handled_at: Some(1001),
                handled_by_workflow_id: Some("stable-workflow".to_string()),
                handled_status: Some(format!("handled {decorated}")),
            },
            "epoch",
            true,
            &redactor,
        );
        assert_eq!(record.record_id, "ft-evt:epoch:44");
        assert_eq!(record.event_id, 44);
        assert_eq!(record.rule_id, "agent.secret_check");
        assert_eq!(record.dedupe_key.as_deref(), Some("stable-dedupe-key"));
        assert_eq!(
            record.matched_text.as_deref(),
            Some("matched [workspace-hidden]")
        );
        assert_eq!(
            record.handled_status.as_deref(),
            Some("handled [workspace-hidden]")
        );
        let extracted = record.extracted.as_ref().unwrap();
        assert_eq!(extracted["nested"][0]["value"], "[workspace-hidden]");
        assert_eq!(extracted["nested"][1], "[workspace-hidden]");
        assert_eq!(extracted["count"], 3);
        assert_eq!(extracted["enabled"], true);
        let serialized = serde_json::to_string(&record).unwrap();
        assert!(!serialized.contains("PRIVATE_"));
        assert!(!serialized.contains("\\u001b"));
    }

    #[test]
    fn cursor_export_paginates_redacts_and_detects_retention_and_epoch_gaps() {
        use crate::runtime_async::{CompatRuntime, RuntimeBuilder};
        use crate::storage::PaneRecord;

        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("vc_export.sqlite3");
            let cx = crate::cx::for_request();
            let storage = StorageHandle::new_with_cx(&cx, db_path.to_str().unwrap())
                .await
                .unwrap();

            storage
                .upsert_pane_with_cx(
                    &cx,
                    PaneRecord {
                        pane_id: 7,
                        pane_uuid: Some("pane-uuid-7".to_string()),
                        domain: "local".to_string(),
                        window_id: Some(1),
                        tab_id: Some(1),
                        title: Some("agent".to_string()),
                        cwd: Some("/tmp".to_string()),
                        tty_name: None,
                        first_seen_at: 1_000,
                        last_seen_at: 1_000,
                        observed: true,
                        ignore_reason: None,
                        last_decision_at: None,
                    },
                )
                .await
                .unwrap();

            let secret_token = ["sk-", "ant-", "api03-", &"A".repeat(52)].concat();
            let custom_secret = "PRIVATE_ORCHID";
            assert_eq!(Redactor::new().redact(custom_secret), custom_secret);
            let mut ids = Vec::new();
            for i in 0..5i64 {
                let id = storage
                    .record_event_with_cx(
                        &cx,
                        StoredEvent {
                            id: 0,
                            pane_id: 7,
                            rule_id: "agent.secret_check".to_string(),
                            agent_type: "claude_code".to_string(),
                            event_type: "alert".to_string(),
                            severity: "warning".to_string(),
                            confidence: 0.95,
                            extracted: Some(serde_json::json!({
                                "idx": i,
                                "nested": {
                                    "token": format!("bearer {secret_token} {custom_secret}"),
                                    "decorated": "PRIVATE_\u{1b}[31mORCHID\u{1b}[0m"
                                }
                            })),
                            matched_text: Some(format!(
                                "leaked {secret_token} {custom_secret} #{i}"
                            )),
                            segment_id: None,
                            detected_at: 10_000 + i * 1_000,
                            dedupe_key: None,
                            handled_at: None,
                            handled_by_workflow_id: None,
                            handled_status: None,
                        },
                    )
                    .await
                    .unwrap();
                ids.push(id);
            }

            // Read-time policy must protect history written under the default
            // policy, not merely rely on the configured persistence writer.
            storage.shutdown().await.unwrap();
            let mut config = crate::config::Config::default();
            config.safety.redaction.patterns = vec![custom_secret.to_string()];
            config.safety.redaction.placeholder = "[workspace-hidden]".to_string();
            let storage_config = crate::storage::StorageConfig {
                redaction_config: config.safety.redaction,
                ..Default::default()
            };
            let storage =
                StorageHandle::with_config(db_path.to_str().unwrap(), storage_config.clone())
                    .await
                    .unwrap();

            // Reject invalid limits (0 and > VC_EXPORT_MAX_LIMIT).
            assert_eq!(
                export_events_page_with_cx(
                    &cx,
                    &storage,
                    &VcExportRequest {
                        limit: 0,
                        ..VcExportRequest::default()
                    }
                )
                .await,
                Err(VcExportError::InvalidLimit {
                    requested: 0,
                    max: VC_EXPORT_MAX_LIMIT,
                })
            );
            assert_eq!(
                export_events_page_with_cx(
                    &cx,
                    &storage,
                    &VcExportRequest {
                        limit: VC_EXPORT_MAX_LIMIT + 1,
                        ..VcExportRequest::default()
                    }
                )
                .await,
                Err(VcExportError::InvalidLimit {
                    requested: VC_EXPORT_MAX_LIMIT + 1,
                    max: VC_EXPORT_MAX_LIMIT,
                })
            );

            // Page 1: limit 2, include_matched_text=true -> redacts secrets everywhere.
            let page1 = export_events_page_with_cx(
                &cx,
                &storage,
                &VcExportRequest {
                    limit: 2,
                    include_matched_text: true,
                    ..VcExportRequest::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(page1.schema_version, VC_EXPORT_SCHEMA_VERSION);
            assert!(page1.redaction_applied);
            assert_eq!(page1.gap, VcExportGapState::Continuous);
            assert!(page1.has_more);
            assert_eq!(page1.records.len(), 2);
            assert_eq!(page1.records[0].event_id, ids[0]);
            assert_eq!(page1.records[1].event_id, ids[1]);
            let serialized_p1 = serde_json::to_string(&page1).unwrap();
            assert!(
                !serialized_p1.contains(secret_token.as_str()),
                "exported page must redact secrets in matched_text and extracted JSON"
            );
            assert!(!serialized_p1.contains(custom_secret));
            assert!(serialized_p1.contains("[workspace-hidden]"));
            assert_eq!(page1.records[0].rule_id, "agent.secret_check");
            assert_eq!(page1.to_jsonl().unwrap().lines().count(), 2);
            assert!(!page1.to_jsonl().unwrap().contains(custom_secret));

            // Page 2: resume from parsed cursor token.
            let c1 = VcExportCursor::parse(&page1.next_cursor_token).unwrap();
            let page2 = export_events_page_with_cx(
                &cx,
                &storage,
                &VcExportRequest {
                    cursor: Some(c1),
                    limit: 2,
                    ..VcExportRequest::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(page2.gap, VcExportGapState::Continuous);
            assert!(page2.has_more);
            assert_eq!(page2.records.len(), 2);
            assert_eq!(page2.records[0].event_id, ids[2]);
            assert_eq!(page2.records[1].event_id, ids[3]);
            assert!(page2.records[0].matched_text.is_none());

            // Page 3: final single record, has_more == false.
            let c2 = VcExportCursor::parse(&page2.next_cursor_token).unwrap();
            let page3 = export_events_page_with_cx(
                &cx,
                &storage,
                &VcExportRequest {
                    cursor: Some(c2),
                    limit: 2,
                    ..VcExportRequest::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(page3.gap, VcExportGapState::Continuous);
            assert!(!page3.has_more);
            assert_eq!(page3.records.len(), 1);
            assert_eq!(page3.records[0].event_id, ids[4]);

            // Epoch mismatch fails closed with EpochReset and zero fabricated records.
            let wrong_epoch = export_events_page_with_cx(
                &cx,
                &storage,
                &VcExportRequest {
                    cursor: Some(VcExportCursor {
                        // A well-formed epoch (32 lowercase hex) of another
                        // database; storage refuses a malformed one outright.
                        cursor_epoch: "b".repeat(32),
                        after_id: ids[1],
                        generation: 0,
                    }),
                    limit: 10,
                    ..VcExportRequest::default()
                },
            )
            .await
            .unwrap();
            assert!(wrong_epoch.records.is_empty());
            assert!(matches!(
                wrong_epoch.gap,
                VcExportGapState::EpochReset { .. }
            ));

            // Cursor ahead of high-water mark fails closed with CursorAheadOfHighWater.
            let ahead = export_events_page_with_cx(
                &cx,
                &storage,
                &VcExportRequest {
                    cursor: Some(VcExportCursor {
                        cursor_epoch: page1.retention.cursor_epoch.clone(),
                        after_id: ids[4] + 99,
                        generation: page1.retention.generation,
                    }),
                    limit: 10,
                    ..VcExportRequest::default()
                },
            )
            .await
            .unwrap();
            assert!(ahead.records.is_empty());
            assert_eq!(
                ahead.gap,
                VcExportGapState::CursorAheadOfHighWater {
                    requested_after_id: ids[4] + 99,
                    max_event_id: ids[4],
                }
            );

            // Retention deletion of events detected before 13_000 (ids[0..=2])
            // surfaces explicit RetentionPruned gap when resuming after ids[0].
            let deleted = storage
                .delete_events_by_retention_rule_with_cx(&cx, 13_000, None, &[], 100)
                .await
                .unwrap();
            assert!(deleted >= 2);
            let after_prune = export_events_page_with_cx(
                &cx,
                &storage,
                &VcExportRequest {
                    cursor: Some(VcExportCursor {
                        cursor_epoch: page1.retention.cursor_epoch.clone(),
                        after_id: ids[0],
                        generation: page1.retention.generation,
                    }),
                    limit: 10,
                    ..VcExportRequest::default()
                },
            )
            .await
            .unwrap();
            assert!(
                matches!(after_prune.gap, VcExportGapState::RetentionPruned { .. }),
                "expected RetentionPruned gap, got {:?}",
                after_prune.gap
            );

            assert_eq!(after_prune.records.len(), 2);
            let serialized_pruned = serde_json::to_string(&after_prune).unwrap();
            assert!(!serialized_pruned.contains(custom_secret));
            assert!(serialized_pruned.contains("[workspace-hidden]"));

            // Exercise the distinct legacy-history output branch after the
            // same authorized ledger rotation used by storage's own tests.
            storage.shutdown().await.unwrap();
            {
                let conn = rusqlite::Connection::open(&db_path).unwrap();
                conn.execute_batch(
                    "BEGIN IMMEDIATE;
                     INSERT INTO event_retention_rotation_authorizations (singleton) VALUES (1);
                     UPDATE event_retention_state
                     SET cursor_epoch = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
                         legacy_history_complete = 0,
                         evidence_from_event_id = max_event_id + 1
                     WHERE singleton = 1;
                     DELETE FROM event_retention_rotation_authorizations;
                     COMMIT;",
                )
                .unwrap();
            }
            let storage = StorageHandle::with_config(db_path.to_str().unwrap(), storage_config)
                .await
                .unwrap();
            let legacy = export_events_page_with_cx(
                &cx,
                &storage,
                &VcExportRequest {
                    cursor: Some(VcExportCursor {
                        cursor_epoch: "a".repeat(32),
                        after_id: 0,
                        generation: after_prune.retention.generation,
                    }),
                    limit: 10,
                    include_matched_text: true,
                    ..VcExportRequest::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(
                legacy.gap,
                VcExportGapState::LegacyHistoryUnavailable {
                    evidence_from_event_id: ids[4] + 1,
                }
            );
            assert_eq!(legacy.records.len(), 2);
            assert_eq!(legacy.records[0].event_id, ids[3]);
            assert_eq!(legacy.next_cursor.after_id, ids[4]);
            assert_eq!(
                legacy.records[0].record_id,
                format!("ft-evt:{}:{}", "a".repeat(32), ids[3])
            );
            let serialized_legacy = serde_json::to_string(&legacy).unwrap();
            assert!(!serialized_legacy.contains(custom_secret));
            assert!(serialized_legacy.contains("[workspace-hidden]"));

            // Pre-cancelled Cx returns Cancelled.
            let cancelled = crate::cx::Cx::for_testing();
            cancelled.cancel_with(
                crate::outcome::CancelKind::User,
                Some("vc-export pre-cancel"),
            );
            assert_eq!(
                export_events_page_with_cx(&cancelled, &storage, &VcExportRequest::default()).await,
                Err(VcExportError::Cancelled)
            );
            storage.shutdown().await.unwrap();
        });
    }
}
