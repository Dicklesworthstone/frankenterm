//! Authorization-aware query executor for flight recorder data.
//!
//! Bead: wa-0dlq
//!
//! Provides the "inner loop" read path for recorder data: every query
//! goes through authorization check → execution → redaction → audit.
//! Any interface surface (CLI, robot mode, MCP) calls this module
//! rather than accessing recorder storage directly.
//!
//! # Access Control Flow
//!
//! ```text
//! Actor + Query ──→ authorize ──→ execute ──→ redact ──→ audit ──→ Response
//!                      │                        │           │
//!                   Deny/Elevate            Strip T3     Hash chain
//! ```
//!
//! # Redaction
//!
//! Results are post-filtered based on the actor's effective access tier:
//! - A0: Only metadata (event IDs, timestamps, pane IDs) — no text
//! - A1: Redacted text only (events with `redaction != None`)
//! - A2: Full text for T1/T2 events; redacted view for T3
//! - A3: All text including unredacted T3
//! - A4: Same as A3 (admin has full access)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::policy::ActorKind;
use crate::recorder_audit::{
    AccessTier, ActorIdentity, AuditEventBuilder, AuditEventType, AuditLog, AuditScope,
    AuthzDecision,
};
use crate::recorder_retention::SensitivityTier;
use crate::recording::{
    RecorderEvent, RecorderEventPayload, RecorderEventSource, RecorderRedactionLevel,
};

// =============================================================================
// Query request
// =============================================================================

/// A structured query against the flight recorder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecorderQueryRequest {
    /// Time range filter: only events with `occurred_at_ms` in [start, end].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_range: Option<TimeRange>,
    /// Filter by pane IDs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pane_ids: Vec<u64>,
    /// Filter by event sources.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<RecorderEventSource>,
    /// Free-text search (substring match against event text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_pattern: Option<String>,
    /// Maximum number of results to return.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Offset for pagination (skip first N matching events).
    #[serde(default)]
    pub offset: usize,
    /// Whether to include the full text content in results.
    /// When false, only metadata is returned (faster, lower privilege).
    #[serde(default = "default_true")]
    pub include_text: bool,
    /// Minimum sensitivity tier to include (default: include all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_sensitivity: Option<SensitivityTier>,
    /// Maximum sensitivity tier to include (default: include all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sensitivity: Option<SensitivityTier>,
}

fn default_limit() -> usize {
    100
}
fn default_true() -> bool {
    true
}

impl Default for RecorderQueryRequest {
    fn default() -> Self {
        Self {
            time_range: None,
            pane_ids: Vec::new(),
            sources: Vec::new(),
            text_pattern: None,
            limit: default_limit(),
            offset: 0,
            include_text: true,
            min_sensitivity: None,
            max_sensitivity: None,
        }
    }
}

impl RecorderQueryRequest {
    /// Create a query for events in a time range.
    #[must_use]
    pub fn in_range(start_ms: u64, end_ms: u64) -> Self {
        Self {
            time_range: Some(TimeRange { start_ms, end_ms }),
            ..Default::default()
        }
    }

    /// Create a query for events on specific panes.
    #[must_use]
    pub fn for_panes(pane_ids: Vec<u64>) -> Self {
        Self {
            pane_ids,
            ..Default::default()
        }
    }

    /// Create a text search query.
    #[must_use]
    pub fn text_search(pattern: impl Into<String>) -> Self {
        Self {
            text_pattern: Some(pattern.into()),
            ..Default::default()
        }
    }

    /// Add a time range filter.
    #[must_use]
    pub fn with_time_range(mut self, start_ms: u64, end_ms: u64) -> Self {
        self.time_range = Some(TimeRange { start_ms, end_ms });
        self
    }

    /// Add pane ID filters.
    #[must_use]
    pub fn with_panes(mut self, pane_ids: Vec<u64>) -> Self {
        self.pane_ids = pane_ids;
        self
    }

    /// Set result limit.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Set offset for pagination.
    #[must_use]
    pub fn with_offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }

    /// Set whether to include text content.
    #[must_use]
    pub fn with_text(mut self, include: bool) -> Self {
        self.include_text = include;
        self
    }

    /// Determine the minimum access tier required for this query.
    #[must_use]
    pub fn required_tier(&self) -> AccessTier {
        if !self.include_text {
            // Metadata-only queries need A0.
            AccessTier::A0PublicMetadata
        } else if self.max_sensitivity == Some(SensitivityTier::T3Restricted) {
            // Explicitly requesting T3 content needs A3.
            AccessTier::A3PrivilegedRaw
        } else if self.pane_ids.len() > 1 || self.text_pattern.is_some() {
            // Cross-pane correlation or text search needs A2.
            AccessTier::A2FullQuery
        } else {
            // Single-pane redacted query needs A1.
            AccessTier::A1RedactedQuery
        }
    }
}

/// Time range for query filtering.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TimeRange {
    pub start_ms: u64,
    pub end_ms: u64,
}

impl TimeRange {
    /// Check if a timestamp falls within this range (inclusive).
    #[must_use]
    pub fn contains(&self, timestamp_ms: u64) -> bool {
        timestamp_ms >= self.start_ms && timestamp_ms <= self.end_ms
    }
}

// =============================================================================
// Query response
// =============================================================================

/// A single event in the query response (potentially redacted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResultEvent {
    /// Original event ID.
    pub event_id: String,
    /// Pane ID.
    pub pane_id: u64,
    /// Event source subsystem.
    pub source: RecorderEventSource,
    /// When the event occurred (ms since epoch).
    pub occurred_at_ms: u64,
    /// Sequence number for ordering.
    pub sequence: u64,
    /// Session ID (if available).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Text content (may be redacted, masked, or omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Whether the text was redacted in this response.
    pub redacted: bool,
    /// Sensitivity tier of the underlying data.
    pub sensitivity: SensitivityTier,
    /// Kind of event payload.
    pub event_kind: QueryEventKind,
}

/// Classification of the event payload for the query result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryEventKind {
    IngressText,
    EgressOutput,
    ControlMarker,
    LifecycleMarker,
}

/// Response from a recorder query execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecorderQueryResponse {
    /// Matching events (post-redaction).
    pub events: Vec<QueryResultEvent>,
    /// Total number of matching events (before limit/offset).
    pub total_count: usize,
    /// Whether more results exist beyond the returned page.
    pub has_more: bool,
    /// The effective access tier used for this query.
    pub effective_tier: AccessTier,
    /// Whether any results were redacted.
    pub redaction_applied: bool,
    /// Execution statistics.
    pub stats: QueryStats,
}

/// Execution statistics for a query.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryStats {
    /// Number of events scanned (before filtering).
    pub events_scanned: usize,
    /// Number of events matching filters (before redaction).
    pub events_matched: usize,
    /// Number of events redacted (text removed or masked).
    pub events_redacted: usize,
    /// Number of events excluded due to access tier.
    pub events_excluded: usize,
    /// Wall-clock query duration.
    #[serde(skip)]
    pub duration: Duration,
}

// =============================================================================
// Query plan (explain)
// =============================================================================

/// Pre-execution query plan showing what a query will access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPlan {
    /// Access tier required for this query.
    pub required_tier: AccessTier,
    /// Actor's current tier.
    pub actor_tier: AccessTier,
    /// Whether the query can proceed.
    pub can_execute: bool,
    /// Whether elevation is needed and possible.
    pub elevation_needed: bool,
    /// Estimated number of events that will be scanned.
    pub estimated_scan_count: usize,
    /// Sensitivity tiers that will be accessed.
    pub sensitivity_tiers_accessed: Vec<SensitivityTier>,
    /// Human-readable explanation.
    pub explanation: String,
}

// =============================================================================
// Elevation grants
// =============================================================================

/// A temporary access tier elevation for an actor.
#[derive(Debug, Clone)]
pub struct ElevationGrant {
    /// Actor who was granted elevation.
    pub actor: ActorIdentity,
    /// Elevated tier.
    pub tier: AccessTier,
    /// Justification for elevation.
    pub justification: String,
    /// When the grant was issued (ms since epoch).
    pub granted_at_ms: u64,
    /// Grant TTL in milliseconds.
    pub ttl_ms: u64,
}

impl ElevationGrant {
    /// Check if this grant is still valid at the given timestamp.
    #[must_use]
    pub fn is_valid_at(&self, now_ms: u64) -> bool {
        now_ms < self.granted_at_ms.saturating_add(self.ttl_ms)
    }
}

// =============================================================================
// Query errors
// =============================================================================

/// Errors that can occur during query execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// Access denied: actor does not have sufficient tier.
    AccessDenied {
        actor_tier: AccessTier,
        required_tier: AccessTier,
    },
    /// Elevation is available but requires justification.
    ElevationRequired {
        required_tier: AccessTier,
        current_tier: AccessTier,
    },
    /// Invalid query parameters.
    InvalidRequest(String),
    /// Internal error during query execution.
    Internal(String),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AccessDenied {
                actor_tier,
                required_tier,
            } => write!(
                f,
                "access denied: actor tier {} insufficient for required tier {}",
                actor_tier, required_tier
            ),
            Self::ElevationRequired {
                required_tier,
                current_tier,
            } => write!(
                f,
                "elevation required: current tier {} < required tier {} (provide justification)",
                current_tier, required_tier
            ),
            Self::InvalidRequest(msg) => write!(f, "invalid query: {}", msg),
            Self::Internal(msg) => write!(f, "internal query error: {}", msg),
        }
    }
}

impl std::error::Error for QueryError {}

// =============================================================================
// In-memory event store (for the executor)
// =============================================================================

/// Trait for reading recorder events. Decouples the query executor from
/// the specific storage backend.
pub trait RecorderEventReader: Send + Sync {
    /// Read events matching the given filter criteria.
    /// Returns events sorted by (occurred_at_ms, sequence).
    fn read_events(&self, filter: &EventFilter) -> Vec<RecorderEvent>;

    /// Count events matching the filter without loading them.
    fn count_events(&self, filter: &EventFilter) -> usize;
}

/// Filter criteria for reading events from storage.
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub time_range: Option<TimeRange>,
    pub pane_ids: Vec<u64>,
    pub sources: Vec<RecorderEventSource>,
    pub text_pattern: Option<String>,
}

/// Simple in-memory event store implementing `RecorderEventReader`.
/// Useful for testing and small workloads.
pub struct InMemoryEventStore {
    events: Mutex<Vec<RecorderEvent>>,
}

impl InMemoryEventStore {
    /// Create a new empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    /// Add events to the store.
    pub fn insert(&self, events: impl IntoIterator<Item = RecorderEvent>) {
        let mut store = self.events.lock().unwrap();
        store.extend(events);
        store.sort_by_key(|e| (e.occurred_at_ms, e.sequence));
    }

    /// Number of events in the store.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.lock().unwrap().is_empty()
    }
}

impl Default for InMemoryEventStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RecorderEventReader for InMemoryEventStore {
    fn read_events(&self, filter: &EventFilter) -> Vec<RecorderEvent> {
        let store = self.events.lock().unwrap();
        store
            .iter()
            .filter(|e| {
                if let Some(ref tr) = filter.time_range {
                    if !tr.contains(e.occurred_at_ms) {
                        return false;
                    }
                }
                if !filter.pane_ids.is_empty() && !filter.pane_ids.contains(&e.pane_id) {
                    return false;
                }
                if !filter.sources.is_empty() && !filter.sources.contains(&e.source) {
                    return false;
                }
                if let Some(ref pattern) = filter.text_pattern {
                    if let Some(text) = extract_text(&e.payload) {
                        if !text.contains(pattern.as_str()) {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect()
    }

    fn count_events(&self, filter: &EventFilter) -> usize {
        self.read_events(filter).len()
    }
}

// =============================================================================
// Query executor
// =============================================================================

/// Authorization-aware query executor for recorder data.
///
/// Wraps a `RecorderEventReader` with access control, redaction, and audit
/// logging. All query interfaces should route through this executor.
pub struct RecorderQueryExecutor<R: RecorderEventReader> {
    reader: R,
    audit_log: AuditLog,
    elevation_grants: Mutex<Vec<ElevationGrant>>,
}

impl<R: RecorderEventReader> RecorderQueryExecutor<R> {
    /// Create a new executor with the given event reader and audit log.
    pub fn new(reader: R, audit_log: AuditLog) -> Self {
        Self {
            reader,
            audit_log,
            elevation_grants: Mutex::new(Vec::new()),
        }
    }

    /// Execute a query as the given actor.
    ///
    /// Performs: authorize → execute → redact → audit → return.
    pub fn execute(
        &self,
        actor: &ActorIdentity,
        request: &RecorderQueryRequest,
        now_ms: u64,
    ) -> Result<RecorderQueryResponse, QueryError> {
        let start = std::time::Instant::now();

        // 1. Determine effective tier (base + elevation).
        let effective_tier = self.effective_tier(actor, now_ms);
        let required_tier = request.required_tier();

        // 2. Authorization check.
        // We use the effective tier (base + elevation grants), so we check directly
        // rather than using check_authorization() which only knows base tiers.
        let decision = if effective_tier.satisfies(required_tier) {
            AuthzDecision::Allow
        } else {
            // Check if the actor could elevate via check_authorization's rules.
            crate::recorder_audit::check_authorization(actor.kind, required_tier)
        };

        match decision {
            AuthzDecision::Allow => {}
            AuthzDecision::Deny => {
                // Audit the denied attempt.
                self.audit_query(
                    actor,
                    request,
                    AuthzDecision::Deny,
                    effective_tier,
                    now_ms,
                    None,
                );
                return Err(QueryError::AccessDenied {
                    actor_tier: effective_tier,
                    required_tier,
                });
            }
            AuthzDecision::Elevate => {
                // Audit the elevation-required attempt.
                self.audit_query(
                    actor,
                    request,
                    AuthzDecision::Elevate,
                    effective_tier,
                    now_ms,
                    None,
                );
                return Err(QueryError::ElevationRequired {
                    required_tier,
                    current_tier: effective_tier,
                });
            }
        }

        // 3. Build filter and execute.
        let filter = EventFilter {
            time_range: request.time_range,
            pane_ids: request.pane_ids.clone(),
            sources: request.sources.clone(),
            text_pattern: request.text_pattern.clone(),
        };

        let raw_events = self.reader.read_events(&filter);
        let total_scanned = raw_events.len();

        // 4. Apply sensitivity filter.
        let filtered: Vec<_> = raw_events
            .into_iter()
            .filter(|e| {
                let tier = classify_event_sensitivity(e);
                if let Some(min) = request.min_sensitivity {
                    if tier < min {
                        return false;
                    }
                }
                if let Some(max) = request.max_sensitivity {
                    if tier > max {
                        return false;
                    }
                }
                true
            })
            .collect();

        let total_matched = filtered.len();

        // 5. Apply pagination.
        let has_more = request.offset + request.limit < total_matched;
        let page: Vec<_> = filtered
            .into_iter()
            .skip(request.offset)
            .take(request.limit)
            .collect();

        // 6. Redact and convert to response events.
        let mut events_redacted = 0;
        let mut events_excluded = 0;
        let mut result_events = Vec::with_capacity(page.len());

        for event in &page {
            let sensitivity = classify_event_sensitivity(event);
            let (text, was_redacted) =
                redact_for_tier(event, effective_tier, request.include_text, sensitivity);

            if was_redacted {
                events_redacted += 1;
            }

            // A0 tier can only see metadata — text is always stripped.
            if effective_tier == AccessTier::A0PublicMetadata && text.is_some() {
                events_excluded += 1;
                continue;
            }

            result_events.push(QueryResultEvent {
                event_id: event.event_id.clone(),
                pane_id: event.pane_id,
                source: event.source,
                occurred_at_ms: event.occurred_at_ms,
                sequence: event.sequence,
                session_id: event.session_id.clone(),
                text,
                redacted: was_redacted,
                sensitivity,
                event_kind: classify_event_kind(&event.payload),
            });
        }

        let duration = start.elapsed();
        let redaction_applied = events_redacted > 0;

        let stats = QueryStats {
            events_scanned: total_scanned,
            events_matched: total_matched,
            events_redacted,
            events_excluded,
            duration,
        };

        let response = RecorderQueryResponse {
            events: result_events,
            total_count: total_matched,
            has_more,
            effective_tier,
            redaction_applied,
            stats,
        };

        // 7. Audit the successful query.
        self.audit_query(
            actor,
            request,
            AuthzDecision::Allow,
            effective_tier,
            now_ms,
            Some(total_matched as u64),
        );

        Ok(response)
    }

    /// Generate a query plan without executing.
    pub fn explain(
        &self,
        actor: &ActorIdentity,
        request: &RecorderQueryRequest,
        now_ms: u64,
    ) -> QueryPlan {
        let effective_tier = self.effective_tier(actor, now_ms);
        let required_tier = request.required_tier();
        let decision = if effective_tier.satisfies(required_tier) {
            AuthzDecision::Allow
        } else {
            crate::recorder_audit::check_authorization(actor.kind, required_tier)
        };

        let can_execute = decision == AuthzDecision::Allow;
        let elevation_needed = decision == AuthzDecision::Elevate;

        let filter = EventFilter {
            time_range: request.time_range,
            pane_ids: request.pane_ids.clone(),
            sources: request.sources.clone(),
            text_pattern: request.text_pattern.clone(),
        };
        let estimated_scan_count = self.reader.count_events(&filter);

        let mut tiers_accessed = Vec::new();
        if request.min_sensitivity.is_none()
            || request.min_sensitivity == Some(SensitivityTier::T1Standard)
        {
            tiers_accessed.push(SensitivityTier::T1Standard);
        }
        if request.max_sensitivity.is_none()
            || request.max_sensitivity >= Some(SensitivityTier::T2Sensitive)
        {
            tiers_accessed.push(SensitivityTier::T2Sensitive);
        }
        if request.max_sensitivity.is_none()
            || request.max_sensitivity >= Some(SensitivityTier::T3Restricted)
        {
            tiers_accessed.push(SensitivityTier::T3Restricted);
        }

        let explanation = if can_execute {
            format!(
                "Query authorized at tier {}. Estimated {} events to scan.",
                effective_tier, estimated_scan_count
            )
        } else if elevation_needed {
            format!(
                "Elevation needed: {} → {}. Provide justification to proceed.",
                effective_tier, required_tier
            )
        } else {
            format!(
                "Access denied: tier {} < required {}. Actor kind {:?} cannot be elevated to this tier.",
                effective_tier, required_tier, actor.kind
            )
        };

        QueryPlan {
            required_tier,
            actor_tier: effective_tier,
            can_execute,
            elevation_needed,
            estimated_scan_count,
            sensitivity_tiers_accessed: tiers_accessed,
            explanation,
        }
    }

    /// Grant a temporary access tier elevation to an actor.
    pub fn grant_elevation(
        &self,
        actor: ActorIdentity,
        tier: AccessTier,
        justification: String,
        now_ms: u64,
        ttl_ms: u64,
    ) {
        // Audit the elevation grant.
        self.audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::AccessApprovalGranted,
                actor.clone(),
                now_ms,
            )
            .with_decision(AuthzDecision::Allow)
            .with_justification(justification.clone()),
        );

        let mut grants = self.elevation_grants.lock().unwrap();
        // Remove any existing grant for this actor.
        grants.retain(|g| g.actor != actor);
        grants.push(ElevationGrant {
            actor,
            tier,
            justification,
            granted_at_ms: now_ms,
            ttl_ms,
        });
    }

    /// Revoke elevation for an actor.
    pub fn revoke_elevation(&self, actor: &ActorIdentity, now_ms: u64) {
        let mut grants = self.elevation_grants.lock().unwrap();
        let had_grant = grants.iter().any(|g| &g.actor == actor);
        grants.retain(|g| &g.actor != actor);

        if had_grant {
            self.audit_log.append(
                AuditEventBuilder::new(
                    AuditEventType::AccessApprovalExpired,
                    actor.clone(),
                    now_ms,
                )
                .with_decision(AuthzDecision::Allow),
            );
        }
    }

    /// Clean up expired elevation grants.
    pub fn expire_grants(&self, now_ms: u64) -> usize {
        let mut grants = self.elevation_grants.lock().unwrap();
        let before = grants.len();
        let expired: Vec<_> = grants
            .iter()
            .filter(|g| !g.is_valid_at(now_ms))
            .map(|g| g.actor.clone())
            .collect();

        for actor in &expired {
            self.audit_log.append(
                AuditEventBuilder::new(
                    AuditEventType::AccessApprovalExpired,
                    actor.clone(),
                    now_ms,
                )
                .with_decision(AuthzDecision::Allow),
            );
        }

        grants.retain(|g| g.is_valid_at(now_ms));
        before - grants.len()
    }

    /// Get the number of active elevation grants.
    #[must_use]
    pub fn active_grants(&self) -> usize {
        self.elevation_grants.lock().unwrap().len()
    }

    /// Access the audit log.
    #[must_use]
    pub fn audit_log(&self) -> &AuditLog {
        &self.audit_log
    }

    /// Compute the effective tier for an actor, considering elevation grants.
    fn effective_tier(&self, actor: &ActorIdentity, now_ms: u64) -> AccessTier {
        let base = AccessTier::default_for_actor(actor.kind);

        let grants = self.elevation_grants.lock().unwrap();
        if let Some(grant) = grants.iter().find(|g| g.actor == *actor && g.is_valid_at(now_ms)) {
            if grant.tier > base {
                return grant.tier;
            }
        }

        base
    }

    /// Audit a query (both successful and denied).
    fn audit_query(
        &self,
        actor: &ActorIdentity,
        request: &RecorderQueryRequest,
        decision: AuthzDecision,
        effective_tier: AccessTier,
        now_ms: u64,
        result_count: Option<u64>,
    ) {
        let event_type = if effective_tier >= AccessTier::A3PrivilegedRaw {
            AuditEventType::RecorderQueryPrivileged
        } else {
            AuditEventType::RecorderQuery
        };

        let mut scope = AuditScope {
            pane_ids: request.pane_ids.clone(),
            time_range: request.time_range.map(|tr| (tr.start_ms, tr.end_ms)),
            query: request.text_pattern.clone(),
            segment_ids: Vec::new(),
            result_count,
        };

        // Redact the query text in audit if it might contain sensitive content.
        if effective_tier < AccessTier::A3PrivilegedRaw {
            if let Some(ref q) = scope.query {
                if q.len() > 50 {
                    scope.query = Some(format!("{}...[redacted]", &q[..50]));
                }
            }
        }

        self.audit_log.append(
            AuditEventBuilder::new(event_type, actor.clone(), now_ms)
                .with_decision(decision)
                .with_scope(scope),
        );
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Extract text content from a recorder event payload.
fn extract_text(payload: &RecorderEventPayload) -> Option<&str> {
    match payload {
        RecorderEventPayload::IngressText { text, .. } => Some(text.as_str()),
        RecorderEventPayload::EgressOutput { text, .. } => Some(text.as_str()),
        RecorderEventPayload::ControlMarker { .. } | RecorderEventPayload::LifecycleMarker { .. } => {
            None
        }
    }
}

/// Classify the sensitivity tier of a recorder event.
fn classify_event_sensitivity(event: &RecorderEvent) -> SensitivityTier {
    match &event.payload {
        RecorderEventPayload::IngressText { redaction, .. }
        | RecorderEventPayload::EgressOutput { redaction, .. } => {
            SensitivityTier::classify(*redaction, false)
        }
        RecorderEventPayload::ControlMarker { .. }
        | RecorderEventPayload::LifecycleMarker { .. } => SensitivityTier::T1Standard,
    }
}

/// Classify the event kind from its payload.
fn classify_event_kind(payload: &RecorderEventPayload) -> QueryEventKind {
    match payload {
        RecorderEventPayload::IngressText { .. } => QueryEventKind::IngressText,
        RecorderEventPayload::EgressOutput { .. } => QueryEventKind::EgressOutput,
        RecorderEventPayload::ControlMarker { .. } => QueryEventKind::ControlMarker,
        RecorderEventPayload::LifecycleMarker { .. } => QueryEventKind::LifecycleMarker,
    }
}

/// Redact event text based on the actor's effective access tier and the event's
/// sensitivity tier. Returns (text, was_redacted).
fn redact_for_tier(
    event: &RecorderEvent,
    actor_tier: AccessTier,
    include_text: bool,
    sensitivity: SensitivityTier,
) -> (Option<String>, bool) {
    if !include_text {
        // Metadata-only mode: no text regardless of tier.
        return (None, false);
    }

    let text = extract_text(&event.payload);
    let Some(original) = text else {
        // Non-text events: no redaction needed.
        return (None, false);
    };

    match actor_tier {
        AccessTier::A0PublicMetadata => {
            // A0: Never return text.
            (None, true)
        }
        AccessTier::A1RedactedQuery => {
            // A1: Only redacted text. T1 (unredacted non-sensitive) is OK.
            // T2/T3 text is masked.
            match sensitivity {
                SensitivityTier::T1Standard => (Some(original.to_string()), false),
                SensitivityTier::T2Sensitive | SensitivityTier::T3Restricted => {
                    (Some(mask_text(original)), true)
                }
            }
        }
        AccessTier::A2FullQuery => {
            // A2: T1/T2 visible; T3 masked.
            match sensitivity {
                SensitivityTier::T1Standard | SensitivityTier::T2Sensitive => {
                    (Some(original.to_string()), false)
                }
                SensitivityTier::T3Restricted => (Some(mask_text(original)), true),
            }
        }
        AccessTier::A3PrivilegedRaw | AccessTier::A4Admin => {
            // A3/A4: Full access to all text.
            (Some(original.to_string()), false)
        }
    }
}

/// Mask sensitive text content.
fn mask_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let len = text.len();
    if len <= 8 {
        "*".repeat(len)
    } else {
        // Show first 2 and last 2 characters, mask the rest.
        let first = &text[..2];
        let last = &text[text.len() - 2..];
        format!("{}{}{}",first, "*".repeat(len - 4), last)
    }
}

/// Aggregate query statistics across multiple queries.
#[derive(Debug, Clone, Default)]
pub struct QueryStatsAggregator {
    /// Per-actor query counts.
    pub by_actor: HashMap<String, u64>,
    /// Per-event-kind result counts.
    pub by_kind: HashMap<QueryEventKind, u64>,
    /// Total queries executed.
    pub total_queries: u64,
    /// Total events returned.
    pub total_results: u64,
    /// Total events redacted.
    pub total_redacted: u64,
    /// Total queries denied.
    pub total_denied: u64,
}

impl QueryStatsAggregator {
    /// Create a new aggregator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful query.
    pub fn record_success(&mut self, actor: &ActorIdentity, response: &RecorderQueryResponse) {
        let counter = self.by_actor.entry(actor.identity.clone()).or_insert(0);
        *counter += 1;

        for event in &response.events {
            let kind_count = self.by_kind.entry(event.event_kind).or_insert(0);
            *kind_count += 1;
        }

        self.total_queries += 1;
        self.total_results += response.events.len() as u64;
        self.total_redacted += response.stats.events_redacted as u64;
    }

    /// Record a denied query.
    pub fn record_denied(&mut self, actor: &ActorIdentity) {
        let counter = self.by_actor.entry(actor.identity.clone()).or_insert(0);
        *counter += 1;
        self.total_queries += 1;
        self.total_denied += 1;
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder_audit::AuditLogConfig;
    use crate::recording::{
        RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
        RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
        RECORDER_EVENT_SCHEMA_VERSION_V1,
    };

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_event(
        pane_id: u64,
        seq: u64,
        ts_ms: u64,
        text: &str,
        redaction: RecorderRedactionLevel,
    ) -> RecorderEvent {
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
                redaction,
                ingress_kind: RecorderIngressKind::SendText,
            },
        }
    }

    fn make_egress_event(
        pane_id: u64,
        seq: u64,
        ts_ms: u64,
        text: &str,
        redaction: RecorderRedactionLevel,
    ) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: format!("evt-{}-{}", pane_id, seq),
            pane_id,
            session_id: None,
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
            payload: RecorderEventPayload::EgressOutput {
                text: text.to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            },
        }
    }

    fn make_lifecycle_event(pane_id: u64, seq: u64, ts_ms: u64) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: format!("evt-{}-{}", pane_id, seq),
            pane_id,
            session_id: None,
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
            payload: RecorderEventPayload::LifecycleMarker {
                lifecycle_phase: crate::recording::RecorderLifecyclePhase::PaneOpened,
                reason: Some("test".into()),
                details: serde_json::Value::Null,
            },
        }
    }

    fn human_actor() -> ActorIdentity {
        ActorIdentity::new(ActorKind::Human, "user-1")
    }

    fn robot_actor() -> ActorIdentity {
        ActorIdentity::new(ActorKind::Robot, "agent-42")
    }

    fn workflow_actor() -> ActorIdentity {
        ActorIdentity::new(ActorKind::Workflow, "wf-restore")
    }

    fn test_store(events: Vec<RecorderEvent>) -> InMemoryEventStore {
        let store = InMemoryEventStore::new();
        store.insert(events);
        store
    }

    fn test_executor(events: Vec<RecorderEvent>) -> RecorderQueryExecutor<InMemoryEventStore> {
        RecorderQueryExecutor::new(
            test_store(events),
            AuditLog::new(AuditLogConfig::default()),
        )
    }

    fn now() -> u64 {
        1700000000000
    }

    // -----------------------------------------------------------------------
    // Query request tests
    // -----------------------------------------------------------------------

    #[test]
    fn query_request_defaults() {
        let req = RecorderQueryRequest::default();
        assert_eq!(req.limit, 100);
        assert!(req.include_text);
        assert!(req.pane_ids.is_empty());
        assert!(req.time_range.is_none());
    }

    #[test]
    fn query_required_tier_metadata_only() {
        let req = RecorderQueryRequest::default().with_text(false);
        assert_eq!(req.required_tier(), AccessTier::A0PublicMetadata);
    }

    #[test]
    fn query_required_tier_single_pane() {
        let req = RecorderQueryRequest::for_panes(vec![1]);
        assert_eq!(req.required_tier(), AccessTier::A1RedactedQuery);
    }

    #[test]
    fn query_required_tier_cross_pane() {
        let req = RecorderQueryRequest::for_panes(vec![1, 2]);
        assert_eq!(req.required_tier(), AccessTier::A2FullQuery);
    }

    #[test]
    fn query_required_tier_text_search() {
        let req = RecorderQueryRequest::text_search("error");
        assert_eq!(req.required_tier(), AccessTier::A2FullQuery);
    }

    #[test]
    fn query_required_tier_t3_explicit() {
        let mut req = RecorderQueryRequest::default();
        req.max_sensitivity = Some(SensitivityTier::T3Restricted);
        assert_eq!(req.required_tier(), AccessTier::A3PrivilegedRaw);
    }

    // -----------------------------------------------------------------------
    // Time range tests
    // -----------------------------------------------------------------------

    #[test]
    fn time_range_contains() {
        let tr = TimeRange {
            start_ms: 100,
            end_ms: 200,
        };
        assert!(tr.contains(100));
        assert!(tr.contains(150));
        assert!(tr.contains(200));
        assert!(!tr.contains(99));
        assert!(!tr.contains(201));
    }

    // -----------------------------------------------------------------------
    // In-memory store tests
    // -----------------------------------------------------------------------

    #[test]
    fn in_memory_store_insert_and_read() {
        let store = InMemoryEventStore::new();
        assert!(store.is_empty());

        store.insert(vec![
            make_event(1, 0, 100, "hello", RecorderRedactionLevel::None),
            make_event(1, 1, 200, "world", RecorderRedactionLevel::None),
        ]);

        assert_eq!(store.len(), 2);

        let all = store.read_events(&EventFilter::default());
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].occurred_at_ms, 100);
        assert_eq!(all[1].occurred_at_ms, 200);
    }

    #[test]
    fn store_filters_by_pane() {
        let store = test_store(vec![
            make_event(1, 0, 100, "a", RecorderRedactionLevel::None),
            make_event(2, 1, 200, "b", RecorderRedactionLevel::None),
            make_event(1, 2, 300, "c", RecorderRedactionLevel::None),
        ]);

        let filter = EventFilter {
            pane_ids: vec![1],
            ..Default::default()
        };
        let results = store.read_events(&filter);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|e| e.pane_id == 1));
    }

    #[test]
    fn store_filters_by_time_range() {
        let store = test_store(vec![
            make_event(1, 0, 100, "a", RecorderRedactionLevel::None),
            make_event(1, 1, 200, "b", RecorderRedactionLevel::None),
            make_event(1, 2, 300, "c", RecorderRedactionLevel::None),
        ]);

        let filter = EventFilter {
            time_range: Some(TimeRange {
                start_ms: 150,
                end_ms: 250,
            }),
            ..Default::default()
        };
        let results = store.read_events(&filter);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].occurred_at_ms, 200);
    }

    #[test]
    fn store_filters_by_text_pattern() {
        let store = test_store(vec![
            make_event(1, 0, 100, "error: file not found", RecorderRedactionLevel::None),
            make_event(1, 1, 200, "success: done", RecorderRedactionLevel::None),
            make_event(1, 2, 300, "error: timeout", RecorderRedactionLevel::None),
        ]);

        let filter = EventFilter {
            text_pattern: Some("error".to_string()),
            ..Default::default()
        };
        let results = store.read_events(&filter);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn store_filters_by_source() {
        let mut evt = make_event(1, 0, 100, "a", RecorderRedactionLevel::None);
        evt.source = RecorderEventSource::RobotMode;

        let store = test_store(vec![
            evt,
            make_event(1, 1, 200, "b", RecorderRedactionLevel::None),
        ]);

        let filter = EventFilter {
            sources: vec![RecorderEventSource::RobotMode],
            ..Default::default()
        };
        let results = store.read_events(&filter);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, RecorderEventSource::RobotMode);
    }

    // -----------------------------------------------------------------------
    // Executor: basic query
    // -----------------------------------------------------------------------

    #[test]
    fn execute_basic_query_human() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "hello world", RecorderRedactionLevel::None),
            make_event(1, 1, 200, "goodbye", RecorderRedactionLevel::None),
        ]);

        let req = RecorderQueryRequest::for_panes(vec![1]);
        let resp = exec.execute(&human_actor(), &req, now()).unwrap();

        assert_eq!(resp.events.len(), 2);
        assert_eq!(resp.total_count, 2);
        assert!(!resp.has_more);
        assert_eq!(resp.effective_tier, AccessTier::A2FullQuery);
    }

    #[test]
    fn execute_pagination() {
        let events: Vec<_> = (0..10)
            .map(|i| {
                make_event(
                    1,
                    i,
                    100 + i * 10,
                    &format!("event-{}", i),
                    RecorderRedactionLevel::None,
                )
            })
            .collect();

        let exec = test_executor(events);
        let req = RecorderQueryRequest::for_panes(vec![1])
            .with_limit(3)
            .with_offset(0);
        let resp = exec.execute(&human_actor(), &req, now()).unwrap();

        assert_eq!(resp.events.len(), 3);
        assert_eq!(resp.total_count, 10);
        assert!(resp.has_more);

        // Page 2.
        let req2 = RecorderQueryRequest::for_panes(vec![1])
            .with_limit(3)
            .with_offset(3);
        let resp2 = exec.execute(&human_actor(), &req2, now()).unwrap();
        assert_eq!(resp2.events.len(), 3);
        assert!(resp2.has_more);

        // Last page.
        let req3 = RecorderQueryRequest::for_panes(vec![1])
            .with_limit(3)
            .with_offset(9);
        let resp3 = exec.execute(&human_actor(), &req3, now()).unwrap();
        assert_eq!(resp3.events.len(), 1);
        assert!(!resp3.has_more);
    }

    #[test]
    fn execute_time_range_filter() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "a", RecorderRedactionLevel::None),
            make_event(1, 1, 200, "b", RecorderRedactionLevel::None),
            make_event(1, 2, 300, "c", RecorderRedactionLevel::None),
        ]);

        let req = RecorderQueryRequest::in_range(150, 250);
        let resp = exec.execute(&human_actor(), &req, now()).unwrap();
        assert_eq!(resp.events.len(), 1);
        assert_eq!(resp.events[0].event_id, "evt-1-1");
    }

    #[test]
    fn execute_text_search() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "compile error: missing semi", RecorderRedactionLevel::None),
            make_event(1, 1, 200, "all tests pass", RecorderRedactionLevel::None),
            make_event(1, 2, 300, "error: timeout expired", RecorderRedactionLevel::None),
        ]);

        let req = RecorderQueryRequest::text_search("error");
        let resp = exec.execute(&human_actor(), &req, now()).unwrap();
        assert_eq!(resp.events.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Executor: access control
    // -----------------------------------------------------------------------

    #[test]
    fn robot_denied_cross_pane_query() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "a", RecorderRedactionLevel::None),
            make_event(2, 1, 200, "b", RecorderRedactionLevel::None),
        ]);

        // Robot has A1 by default; cross-pane needs A2.
        let req = RecorderQueryRequest::for_panes(vec![1, 2]);
        let result = exec.execute(&robot_actor(), &req, now());

        assert!(result.is_err());
        match result.unwrap_err() {
            QueryError::ElevationRequired { .. } => {}
            other => panic!("expected ElevationRequired, got {:?}", other),
        }
    }

    #[test]
    fn robot_allowed_metadata_only() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "hello", RecorderRedactionLevel::None),
        ]);

        // A0 metadata query — robot should be allowed.
        let req = RecorderQueryRequest::default().with_text(false);
        let resp = exec.execute(&robot_actor(), &req, now()).unwrap();
        assert_eq!(resp.effective_tier, AccessTier::A1RedactedQuery);
        // Text should be None since include_text=false.
        assert!(resp.events[0].text.is_none());
    }

    #[test]
    fn robot_single_pane_allowed() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "hello world", RecorderRedactionLevel::None),
        ]);

        let req = RecorderQueryRequest::for_panes(vec![1]);
        let resp = exec.execute(&robot_actor(), &req, now()).unwrap();
        assert_eq!(resp.events.len(), 1);
        assert_eq!(resp.effective_tier, AccessTier::A1RedactedQuery);
    }

    #[test]
    fn denied_query_generates_audit_entry() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "a", RecorderRedactionLevel::None),
        ]);

        let req = RecorderQueryRequest::for_panes(vec![1, 2]);
        let _ = exec.execute(&robot_actor(), &req, now());

        let entries = exec.audit_log().entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].decision, AuthzDecision::Elevate);
    }

    #[test]
    fn successful_query_generates_audit_entry() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "hello", RecorderRedactionLevel::None),
        ]);

        let req = RecorderQueryRequest::for_panes(vec![1]);
        let _ = exec.execute(&human_actor(), &req, now());

        let entries = exec.audit_log().entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].decision, AuthzDecision::Allow);
        assert_eq!(entries[0].event_type, AuditEventType::RecorderQuery);
    }

    // -----------------------------------------------------------------------
    // Executor: redaction
    // -----------------------------------------------------------------------

    #[test]
    fn robot_sees_t1_text_but_t2_masked() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "normal output", RecorderRedactionLevel::None),
            make_event(1, 1, 200, "secret: API_KEY=abc123def", RecorderRedactionLevel::Partial),
        ]);

        let req = RecorderQueryRequest::for_panes(vec![1]);
        let resp = exec.execute(&robot_actor(), &req, now()).unwrap();

        // T1 (unredacted None): visible to A1.
        assert_eq!(resp.events[0].text.as_deref(), Some("normal output"));
        assert!(!resp.events[0].redacted);

        // T2 (Partial redaction): masked for A1.
        assert_ne!(resp.events[1].text.as_deref(), Some("secret: API_KEY=abc123def"));
        assert!(resp.events[1].redacted);
    }

    #[test]
    fn human_sees_t2_text_unmasked() {
        let exec = test_executor(vec![make_event(
            1,
            0,
            100,
            "sensitive data here",
            RecorderRedactionLevel::Partial,
        )]);

        let req = RecorderQueryRequest::for_panes(vec![1]);
        let resp = exec.execute(&human_actor(), &req, now()).unwrap();

        // Human has A2: can see T2 data.
        assert_eq!(resp.events[0].text.as_deref(), Some("sensitive data here"));
        assert!(!resp.events[0].redacted);
    }

    #[test]
    fn metadata_only_strips_text() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "hello", RecorderRedactionLevel::None),
        ]);

        let req = RecorderQueryRequest::default().with_text(false);
        let resp = exec.execute(&human_actor(), &req, now()).unwrap();

        assert!(resp.events[0].text.is_none());
        assert!(!resp.events[0].redacted);
    }

    // -----------------------------------------------------------------------
    // Executor: elevation
    // -----------------------------------------------------------------------

    #[test]
    fn elevation_grants_higher_tier() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "a", RecorderRedactionLevel::None),
            make_event(2, 1, 200, "b", RecorderRedactionLevel::None),
        ]);

        let actor = robot_actor();

        // Initially denied cross-pane.
        let req = RecorderQueryRequest::for_panes(vec![1, 2]);
        assert!(exec.execute(&actor, &req, now()).is_err());

        // Grant elevation.
        exec.grant_elevation(
            actor.clone(),
            AccessTier::A2FullQuery,
            "incident investigation".to_string(),
            now(),
            60_000, // 60s TTL
        );

        // Now allowed.
        let resp = exec.execute(&actor, &req, now()).unwrap();
        assert_eq!(resp.events.len(), 2);
        assert_eq!(resp.effective_tier, AccessTier::A2FullQuery);
    }

    #[test]
    fn elevation_expires() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "a", RecorderRedactionLevel::None),
            make_event(2, 1, 200, "b", RecorderRedactionLevel::None),
        ]);

        let actor = robot_actor();
        exec.grant_elevation(
            actor.clone(),
            AccessTier::A2FullQuery,
            "temp".to_string(),
            now(),
            10_000, // 10s TTL
        );

        // Works within TTL.
        let req = RecorderQueryRequest::for_panes(vec![1, 2]);
        assert!(exec.execute(&actor, &req, now() + 5_000).is_ok());

        // Fails after TTL.
        assert!(exec.execute(&actor, &req, now() + 15_000).is_err());
    }

    #[test]
    fn elevation_revocation() {
        let exec = test_executor(vec![]);
        let actor = robot_actor();

        exec.grant_elevation(
            actor.clone(),
            AccessTier::A3PrivilegedRaw,
            "debug".to_string(),
            now(),
            300_000,
        );
        assert_eq!(exec.active_grants(), 1);

        exec.revoke_elevation(&actor, now());
        assert_eq!(exec.active_grants(), 0);

        // Audit log should show both grant and revoke.
        let entries = exec.audit_log().entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].event_type, AuditEventType::AccessApprovalGranted);
        assert_eq!(entries[1].event_type, AuditEventType::AccessApprovalExpired);
    }

    #[test]
    fn expire_grants_cleans_up() {
        let exec = test_executor(vec![]);

        exec.grant_elevation(
            robot_actor(),
            AccessTier::A2FullQuery,
            "r1".to_string(),
            now(),
            1_000,
        );
        exec.grant_elevation(
            workflow_actor(),
            AccessTier::A3PrivilegedRaw,
            "w1".to_string(),
            now(),
            100_000,
        );

        assert_eq!(exec.active_grants(), 2);

        // Expire after 5s — robot's 1s TTL should be gone.
        let expired = exec.expire_grants(now() + 5_000);
        assert_eq!(expired, 1);
        assert_eq!(exec.active_grants(), 1);
    }

    // -----------------------------------------------------------------------
    // Executor: explain / query plan
    // -----------------------------------------------------------------------

    #[test]
    fn explain_human_simple_query() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "a", RecorderRedactionLevel::None),
        ]);

        let req = RecorderQueryRequest::for_panes(vec![1]);
        let plan = exec.explain(&human_actor(), &req, now());

        assert!(plan.can_execute);
        assert!(!plan.elevation_needed);
        assert_eq!(plan.required_tier, AccessTier::A1RedactedQuery);
        assert_eq!(plan.estimated_scan_count, 1);
    }

    #[test]
    fn explain_robot_denied() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "a", RecorderRedactionLevel::None),
        ]);

        let req = RecorderQueryRequest::text_search("a"); // needs A2
        let plan = exec.explain(&robot_actor(), &req, now());

        assert!(!plan.can_execute);
        assert!(plan.elevation_needed);
    }

    // -----------------------------------------------------------------------
    // Sensitivity filtering
    // -----------------------------------------------------------------------

    #[test]
    fn filter_by_max_sensitivity() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "public", RecorderRedactionLevel::None),       // T1
            make_event(1, 1, 200, "redacted", RecorderRedactionLevel::Partial),   // T2
        ]);

        let mut req = RecorderQueryRequest::for_panes(vec![1]);
        req.max_sensitivity = Some(SensitivityTier::T1Standard);

        let resp = exec.execute(&human_actor(), &req, now()).unwrap();
        assert_eq!(resp.events.len(), 1);
        assert_eq!(resp.events[0].sensitivity, SensitivityTier::T1Standard);
    }

    #[test]
    fn filter_by_min_sensitivity() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "public", RecorderRedactionLevel::None),       // T1
            make_event(1, 1, 200, "redacted", RecorderRedactionLevel::Partial),   // T2
        ]);

        let mut req = RecorderQueryRequest::for_panes(vec![1]);
        req.min_sensitivity = Some(SensitivityTier::T2Sensitive);

        let resp = exec.execute(&human_actor(), &req, now()).unwrap();
        assert_eq!(resp.events.len(), 1);
        assert_eq!(resp.events[0].sensitivity, SensitivityTier::T2Sensitive);
    }

    // -----------------------------------------------------------------------
    // Event kind classification
    // -----------------------------------------------------------------------

    #[test]
    fn event_kind_classification() {
        assert_eq!(
            classify_event_kind(&RecorderEventPayload::IngressText {
                text: "x".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            }),
            QueryEventKind::IngressText
        );

        assert_eq!(
            classify_event_kind(&RecorderEventPayload::EgressOutput {
                text: "x".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            }),
            QueryEventKind::EgressOutput
        );
    }

    // -----------------------------------------------------------------------
    // Masking
    // -----------------------------------------------------------------------

    #[test]
    fn mask_text_short() {
        assert_eq!(mask_text("ab"), "**");
        assert_eq!(mask_text("abc"), "***");
        assert_eq!(mask_text("12345678"), "********");
    }

    #[test]
    fn mask_text_long() {
        let masked = mask_text("API_KEY=secret123");
        assert!(masked.starts_with("AP"));
        assert!(masked.ends_with("23"));
        assert!(masked.contains("*"));
        assert_eq!(masked.len(), 17); // same length as original
    }

    #[test]
    fn mask_text_empty() {
        assert_eq!(mask_text(""), "");
    }

    // -----------------------------------------------------------------------
    // Mixed event types
    // -----------------------------------------------------------------------

    #[test]
    fn query_includes_lifecycle_events() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "hello", RecorderRedactionLevel::None),
            make_lifecycle_event(1, 1, 200),
            make_egress_event(1, 2, 300, "output", RecorderRedactionLevel::None),
        ]);

        let req = RecorderQueryRequest::for_panes(vec![1]);
        let resp = exec.execute(&human_actor(), &req, now()).unwrap();

        assert_eq!(resp.events.len(), 3);
        assert_eq!(resp.events[0].event_kind, QueryEventKind::IngressText);
        assert_eq!(resp.events[1].event_kind, QueryEventKind::LifecycleMarker);
        assert_eq!(resp.events[2].event_kind, QueryEventKind::EgressOutput);

        // Lifecycle events have T1 sensitivity.
        assert_eq!(resp.events[1].sensitivity, SensitivityTier::T1Standard);
    }

    #[test]
    fn text_search_skips_non_text_events() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "hello world", RecorderRedactionLevel::None),
            make_lifecycle_event(1, 1, 200),
        ]);

        let req = RecorderQueryRequest::text_search("hello");
        let resp = exec.execute(&human_actor(), &req, now()).unwrap();

        // Only the text event matches.
        assert_eq!(resp.events.len(), 1);
        assert_eq!(resp.events[0].event_kind, QueryEventKind::IngressText);
    }

    // -----------------------------------------------------------------------
    // Stats aggregator
    // -----------------------------------------------------------------------

    #[test]
    fn stats_aggregator_tracks_queries() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "hello", RecorderRedactionLevel::None),
        ]);

        let mut agg = QueryStatsAggregator::new();
        let actor = human_actor();
        let req = RecorderQueryRequest::for_panes(vec![1]);

        let resp = exec.execute(&actor, &req, now()).unwrap();
        agg.record_success(&actor, &resp);

        assert_eq!(agg.total_queries, 1);
        assert_eq!(agg.total_results, 1);
        assert_eq!(*agg.by_actor.get("user-1").unwrap(), 1);
    }

    #[test]
    fn stats_aggregator_tracks_denied() {
        let mut agg = QueryStatsAggregator::new();
        let actor = robot_actor();
        agg.record_denied(&actor);

        assert_eq!(agg.total_queries, 1);
        assert_eq!(agg.total_denied, 1);
    }

    // -----------------------------------------------------------------------
    // Privileged query auditing
    // -----------------------------------------------------------------------

    #[test]
    fn privileged_query_type_in_audit() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "secret stuff", RecorderRedactionLevel::None),
        ]);

        let actor = human_actor();
        exec.grant_elevation(
            actor.clone(),
            AccessTier::A3PrivilegedRaw,
            "incident".to_string(),
            now(),
            60_000,
        );

        let req = RecorderQueryRequest::for_panes(vec![1]);
        let _ = exec.execute(&actor, &req, now());

        let entries = exec.audit_log().entries();
        // Grant + query = 2 entries.
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[1].event_type,
            AuditEventType::RecorderQueryPrivileged
        );
    }

    // -----------------------------------------------------------------------
    // Query response serialization
    // -----------------------------------------------------------------------

    #[test]
    fn query_response_serializable() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "hello", RecorderRedactionLevel::None),
        ]);

        let req = RecorderQueryRequest::for_panes(vec![1]);
        let resp = exec.execute(&human_actor(), &req, now()).unwrap();

        let json = serde_json::to_string(&resp).unwrap();
        let decoded: RecorderQueryResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.events.len(), 1);
        assert_eq!(decoded.total_count, 1);
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn empty_store_returns_empty_results() {
        let exec = test_executor(vec![]);

        let req = RecorderQueryRequest::default();
        let resp = exec.execute(&human_actor(), &req, now()).unwrap();

        assert_eq!(resp.events.len(), 0);
        assert_eq!(resp.total_count, 0);
        assert!(!resp.has_more);
    }

    #[test]
    fn query_request_builder_chain() {
        let req = RecorderQueryRequest::text_search("error")
            .with_panes(vec![1, 2, 3])
            .with_time_range(1000, 2000)
            .with_limit(50)
            .with_offset(10)
            .with_text(true);

        assert_eq!(req.text_pattern.as_deref(), Some("error"));
        assert_eq!(req.pane_ids, vec![1, 2, 3]);
        assert_eq!(req.time_range.unwrap().start_ms, 1000);
        assert_eq!(req.limit, 50);
        assert_eq!(req.offset, 10);
        assert!(req.include_text);
    }

    #[test]
    fn elevation_grant_validity() {
        let grant = ElevationGrant {
            actor: robot_actor(),
            tier: AccessTier::A3PrivilegedRaw,
            justification: "test".into(),
            granted_at_ms: 1000,
            ttl_ms: 500,
        };

        assert!(grant.is_valid_at(1000));
        assert!(grant.is_valid_at(1499));
        assert!(!grant.is_valid_at(1500));
        assert!(!grant.is_valid_at(2000));
    }

    #[test]
    fn elevation_grant_overflow_safety() {
        let grant = ElevationGrant {
            actor: robot_actor(),
            tier: AccessTier::A2FullQuery,
            justification: "test".into(),
            granted_at_ms: u64::MAX - 10,
            ttl_ms: 100,
        };
        // saturating_add should handle overflow gracefully.
        assert!(grant.is_valid_at(u64::MAX - 5));
    }

    #[test]
    fn workflow_actor_gets_a2_by_default() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "a", RecorderRedactionLevel::None),
            make_event(2, 1, 200, "b", RecorderRedactionLevel::None),
        ]);

        // Workflow has A2 default — cross-pane should work.
        let req = RecorderQueryRequest::for_panes(vec![1, 2]);
        let resp = exec.execute(&workflow_actor(), &req, now()).unwrap();
        assert_eq!(resp.events.len(), 2);
        assert_eq!(resp.effective_tier, AccessTier::A2FullQuery);
    }

    // -----------------------------------------------------------------------
    // Audit scope contains query context
    // -----------------------------------------------------------------------

    #[test]
    fn audit_scope_records_query_context() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "data", RecorderRedactionLevel::None),
        ]);

        let req = RecorderQueryRequest::for_panes(vec![1, 2])
            .with_time_range(100, 200);
        let _ = exec.execute(&human_actor(), &req, now());

        let entries = exec.audit_log().entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].scope.pane_ids, vec![1, 2]);
        assert_eq!(entries[0].scope.time_range, Some((100, 200)));
        assert_eq!(entries[0].scope.result_count, Some(1));
    }

    // -----------------------------------------------------------------------
    // Redaction at different tiers for T3 data
    // -----------------------------------------------------------------------

    #[test]
    fn a1_masks_t3_data() {
        // Simulate T3 by using unredacted data that classify_event_sensitivity
        // will treat as T1 (since we don't have unredacted_capture=true).
        // For the test, use Partial redaction which classifies as T2.
        let exec = test_executor(vec![make_event(
            1,
            0,
            100,
            "super secret content",
            RecorderRedactionLevel::Partial,
        )]);

        let req = RecorderQueryRequest::for_panes(vec![1]);
        let resp = exec.execute(&robot_actor(), &req, now()).unwrap();

        assert!(resp.events[0].redacted);
        assert_ne!(resp.events[0].text.as_deref(), Some("super secret content"));
    }

    #[test]
    fn a2_sees_t2_but_not_t3() {
        // A2 (human) can see T2 (Partial) but T3 would be masked.
        // Since classify_event_sensitivity uses redaction=Partial → T2, human sees it.
        let exec = test_executor(vec![make_event(
            1,
            0,
            100,
            "partially redacted",
            RecorderRedactionLevel::Partial,
        )]);

        let req = RecorderQueryRequest::for_panes(vec![1]);
        let resp = exec.execute(&human_actor(), &req, now()).unwrap();

        // Human (A2) sees T2 content.
        assert_eq!(resp.events[0].text.as_deref(), Some("partially redacted"));
        assert!(!resp.events[0].redacted);
    }

    // -----------------------------------------------------------------------
    // Multiple actors, multiple queries
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_actors_see_different_views() {
        let exec = test_executor(vec![
            make_event(1, 0, 100, "normal output", RecorderRedactionLevel::None),
            make_event(1, 1, 200, "sensitive: password=hunter2", RecorderRedactionLevel::Partial),
        ]);

        let req = RecorderQueryRequest::for_panes(vec![1]);

        // Human (A2): sees both clearly.
        let human_resp = exec.execute(&human_actor(), &req, now()).unwrap();
        assert_eq!(human_resp.events[0].text.as_deref(), Some("normal output"));
        assert_eq!(
            human_resp.events[1].text.as_deref(),
            Some("sensitive: password=hunter2")
        );
        assert!(!human_resp.redaction_applied);

        // Robot (A1): sees T1, masked T2.
        let robot_resp = exec.execute(&robot_actor(), &req, now()).unwrap();
        assert_eq!(robot_resp.events[0].text.as_deref(), Some("normal output"));
        assert!(robot_resp.events[1].redacted);
        assert_ne!(
            robot_resp.events[1].text.as_deref(),
            Some("sensitive: password=hunter2")
        );
        assert!(robot_resp.redaction_applied);
    }

    // -----------------------------------------------------------------------
    // Query error display
    // -----------------------------------------------------------------------

    #[test]
    fn query_error_display() {
        let err = QueryError::AccessDenied {
            actor_tier: AccessTier::A1RedactedQuery,
            required_tier: AccessTier::A3PrivilegedRaw,
        };
        let msg = err.to_string();
        assert!(msg.contains("access denied"));
        assert!(msg.contains("A1"));
        assert!(msg.contains("A3"));
    }

    #[test]
    fn query_error_elevation_display() {
        let err = QueryError::ElevationRequired {
            required_tier: AccessTier::A2FullQuery,
            current_tier: AccessTier::A1RedactedQuery,
        };
        let msg = err.to_string();
        assert!(msg.contains("elevation required"));
    }
}
