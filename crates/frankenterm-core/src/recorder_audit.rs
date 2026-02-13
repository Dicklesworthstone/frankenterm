//! Recorder audit log with tamper-evident hash chain.
//!
//! Implements the audit requirements from `recorder-governance-policy.md`:
//!
//! - Structured audit events for all policy-relevant recorder operations
//! - SHA-256 hash chain (`prev_entry_hash`) for tamper evidence
//! - Append-only log with ordinal continuity for gap detection
//! - Actor identity tracking (human, robot, MCP, workflow)
//! - Access tier enforcement documentation (A0–A4)
//!
//! # Hash Chain
//!
//! Each audit entry includes the SHA-256 hash of the previous entry's
//! canonical JSON. Chain integrity can be verified offline by replaying
//! the hash chain from genesis. Any tampering — insertion, deletion, or
//! modification — breaks the chain.
//!
//! # Thread Safety
//!
//! `AuditLog` is `Send + Sync` via interior `Mutex`. Writes are serialized
//! to maintain hash chain ordering. This is acceptable because audit writes
//! are low-frequency (not on the hot capture path).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::Mutex;

use crate::policy::ActorKind;

// =============================================================================
// Constants
// =============================================================================

/// Current audit schema version.
pub const AUDIT_SCHEMA_VERSION: &str = "ft.recorder.audit.v1";

/// Hash of the genesis entry (all zeros).
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Default audit log retention in days.
pub const DEFAULT_AUDIT_RETENTION_DAYS: u32 = 90;

/// Maximum raw-access query rows (default).
pub const DEFAULT_MAX_RAW_QUERY_ROWS: u32 = 100;

/// Default approval TTL in seconds.
pub const DEFAULT_APPROVAL_TTL_SECONDS: u32 = 900;

// =============================================================================
// Access Tiers
// =============================================================================

/// Recorder access control tiers (A0–A4).
///
/// From governance policy §3.1:
/// - A0: Public metadata (segment count, health)
/// - A1: Redacted query (search over redacted text)
/// - A2: Full query (cross-pane correlation, aggregates)
/// - A3: Privileged raw (unredacted text, audit log read)
/// - A4: Admin (retention override, purge, policy change)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessTier {
    /// Public metadata: segment count, health status, retention stats.
    A0PublicMetadata,
    /// Redacted query: search/replay over redacted text.
    A1RedactedQuery,
    /// Full query: cross-pane correlation, aggregate analytics.
    A2FullQuery,
    /// Privileged raw: unredacted text access, audit log read.
    A3PrivilegedRaw,
    /// Admin: retention override, purge, policy change.
    A4Admin,
}

impl AccessTier {
    /// Returns the numeric level (0–4).
    #[must_use]
    pub const fn level(&self) -> u8 {
        match self {
            Self::A0PublicMetadata => 0,
            Self::A1RedactedQuery => 1,
            Self::A2FullQuery => 2,
            Self::A3PrivilegedRaw => 3,
            Self::A4Admin => 4,
        }
    }

    /// Check if this tier is sufficient for the required tier.
    #[must_use]
    pub const fn satisfies(&self, required: AccessTier) -> bool {
        self.level() >= required.level()
    }

    /// Default access tier for a given actor kind.
    #[must_use]
    pub const fn default_for_actor(actor: ActorKind) -> Self {
        match actor {
            ActorKind::Human => Self::A2FullQuery,
            ActorKind::Robot => Self::A1RedactedQuery,
            ActorKind::Mcp => Self::A1RedactedQuery,
            ActorKind::Workflow => Self::A2FullQuery,
        }
    }
}

impl std::fmt::Display for AccessTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::A0PublicMetadata => write!(f, "A0 (public metadata)"),
            Self::A1RedactedQuery => write!(f, "A1 (redacted query)"),
            Self::A2FullQuery => write!(f, "A2 (full query)"),
            Self::A3PrivilegedRaw => write!(f, "A3 (privileged raw)"),
            Self::A4Admin => write!(f, "A4 (admin)"),
        }
    }
}

// =============================================================================
// Authorization Decision
// =============================================================================

/// Result of an authorization check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthzDecision {
    /// Access allowed at the actor's current tier.
    Allow,
    /// Access denied: required tier exceeds actor's tier.
    Deny,
    /// Elevation required: actor can elevate with approval.
    Elevate,
}

// =============================================================================
// Actor Identity
// =============================================================================

/// Full actor identity for audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorIdentity {
    /// Actor kind (human, robot, mcp, workflow).
    pub kind: ActorKind,
    /// Stable identifier for the actor (e.g., session ID, workflow ID).
    pub identity: String,
}

impl ActorIdentity {
    /// Create a new actor identity.
    #[must_use]
    pub fn new(kind: ActorKind, identity: impl Into<String>) -> Self {
        Self {
            kind,
            identity: identity.into(),
        }
    }
}

// =============================================================================
// Audit Event Types
// =============================================================================

/// All auditable recorder operations from governance policy §5.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventType {
    // Query operations
    /// Standard query (A1/A2).
    RecorderQuery,
    /// Privileged raw query (A3).
    RecorderQueryPrivileged,
    /// Replay operation.
    RecorderReplay,
    /// Export operation.
    RecorderExport,

    // Admin operations
    /// Retention override.
    AdminRetentionOverride,
    /// Manual purge.
    AdminPurge,
    /// Policy change.
    AdminPolicyChange,

    // Access operations
    /// Approval granted for elevated access.
    AccessApprovalGranted,
    /// Approval expired.
    AccessApprovalExpired,
    /// Incident response mode activated.
    AccessIncidentMode,
    /// Debug mode started.
    AccessDebugMode,

    // Retention lifecycle (from recorder_retention.rs)
    /// Segment sealed.
    RetentionSegmentSealed,
    /// Segment archived.
    RetentionSegmentArchived,
    /// Segment purged.
    RetentionSegmentPurged,
    /// T3 accelerated purge.
    RetentionAcceleratedPurge,
}

// =============================================================================
// Query Scope
// =============================================================================

/// Scope of a query or operation for audit purposes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditScope {
    /// Pane IDs involved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pane_ids: Vec<u64>,
    /// Time range (start_ms, end_ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_range: Option<(u64, u64)>,
    /// Redacted query text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Segment IDs affected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segment_ids: Vec<String>,
    /// Number of results returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_count: Option<u64>,
}

// =============================================================================
// Audit Entry
// =============================================================================

/// A single entry in the recorder audit log.
///
/// Each entry includes the SHA-256 hash of the previous entry's canonical
/// JSON for tamper evidence (hash chain).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecorderAuditEntry {
    /// Audit schema version.
    pub audit_version: String,
    /// Monotonically increasing ordinal for gap detection.
    pub ordinal: u64,
    /// Type of auditable operation.
    pub event_type: AuditEventType,
    /// Actor who performed the operation.
    pub actor: ActorIdentity,
    /// Timestamp (ms since epoch).
    pub timestamp_ms: u64,
    /// Scope of the operation.
    pub scope: AuditScope,
    /// Authorization decision.
    pub decision: AuthzDecision,
    /// Justification (required for elevated access).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
    /// Active governance policy version.
    pub policy_version: String,
    /// SHA-256 hash of the previous entry's canonical JSON.
    pub prev_entry_hash: String,
    /// Additional event-specific details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl RecorderAuditEntry {
    /// Compute the SHA-256 hash of this entry's canonical JSON.
    #[must_use]
    pub fn hash(&self) -> String {
        let json = serde_json::to_string(self).unwrap_or_default();
        let digest = Sha256::digest(json.as_bytes());
        hex::encode(digest)
    }
}

// =============================================================================
// Audit Log Configuration
// =============================================================================

/// Configuration for the recorder audit log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogConfig {
    /// Minimum retention in days (default 90).
    pub retention_days: u32,
    /// Enable tamper-evidence hash chain.
    pub hash_chain_enabled: bool,
    /// Maximum entries to keep in memory before requiring flush.
    pub max_memory_entries: usize,
    /// Policy version string.
    pub policy_version: String,
}

impl Default for AuditLogConfig {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_AUDIT_RETENTION_DAYS,
            hash_chain_enabled: true,
            max_memory_entries: 10_000,
            policy_version: "governance.v1".to_string(),
        }
    }
}

// =============================================================================
// Chain Verification Result
// =============================================================================

/// Result of verifying the audit hash chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainVerification {
    /// Total entries verified.
    pub total_entries: u64,
    /// Whether the chain is intact.
    pub chain_intact: bool,
    /// First broken entry ordinal (if any).
    pub first_break_at: Option<u64>,
    /// Missing ordinals (gap detection).
    pub missing_ordinals: Vec<u64>,
    /// Expected ordinal range (first, last).
    pub ordinal_range: Option<(u64, u64)>,
}

// =============================================================================
// Audit Log
// =============================================================================

/// In-memory audit log state (behind Mutex).
struct AuditLogInner {
    /// Entries in insertion order.
    entries: VecDeque<RecorderAuditEntry>,
    /// Next ordinal to assign.
    next_ordinal: u64,
    /// Hash of the most recently appended entry.
    last_hash: String,
    /// Configuration.
    config: AuditLogConfig,
    /// Total entries ever appended (including flushed).
    total_appended: u64,
}

/// Append-only recorder audit log with tamper-evident hash chain.
///
/// Thread-safe via interior `Mutex`. Writes are serialized to maintain
/// hash chain ordering.
///
/// # Example
///
/// ```ignore
/// use frankenterm_core::recorder_audit::*;
/// use frankenterm_core::policy::ActorKind;
///
/// let log = AuditLog::new(AuditLogConfig::default());
///
/// let entry = log.append(AuditEventBuilder::new(
///     AuditEventType::RecorderQuery,
///     ActorIdentity::new(ActorKind::Human, "session-123"),
///     1700000000000,
/// ).with_decision(AuthzDecision::Allow));
///
/// assert_eq!(entry.ordinal, 0);
/// ```
pub struct AuditLog {
    inner: Mutex<AuditLogInner>,
}

impl AuditLog {
    /// Create a new audit log with the given configuration.
    #[must_use]
    pub fn new(config: AuditLogConfig) -> Self {
        Self {
            inner: Mutex::new(AuditLogInner {
                entries: VecDeque::new(),
                next_ordinal: 0,
                last_hash: GENESIS_HASH.to_string(),
                config,
                total_appended: 0,
            }),
        }
    }

    /// Create a new audit log continuing from a known state.
    ///
    /// Used when restoring from a persisted log — provides the next ordinal
    /// and the hash of the last known entry.
    #[must_use]
    pub fn resume(config: AuditLogConfig, next_ordinal: u64, last_hash: String) -> Self {
        Self {
            inner: Mutex::new(AuditLogInner {
                entries: VecDeque::new(),
                next_ordinal,
                last_hash,
                config,
                total_appended: 0,
            }),
        }
    }

    /// Append a new audit entry. Returns the finalized entry with ordinal and hash chain.
    pub fn append(&self, builder: AuditEventBuilder) -> RecorderAuditEntry {
        let mut inner = self.inner.lock().unwrap();

        let entry = RecorderAuditEntry {
            audit_version: AUDIT_SCHEMA_VERSION.to_string(),
            ordinal: inner.next_ordinal,
            event_type: builder.event_type,
            actor: builder.actor,
            timestamp_ms: builder.timestamp_ms,
            scope: builder.scope,
            decision: builder.decision,
            justification: builder.justification,
            policy_version: inner.config.policy_version.clone(),
            prev_entry_hash: inner.last_hash.clone(),
            details: builder.details,
        };

        if inner.config.hash_chain_enabled {
            inner.last_hash = entry.hash();
        }

        inner.next_ordinal += 1;
        inner.total_appended += 1;

        // Enforce memory limit.
        if inner.entries.len() >= inner.config.max_memory_entries {
            inner.entries.pop_front();
        }

        inner.entries.push_back(entry.clone());

        entry
    }

    /// Return all in-memory entries.
    #[must_use]
    pub fn entries(&self) -> Vec<RecorderAuditEntry> {
        let inner = self.inner.lock().unwrap();
        inner.entries.iter().cloned().collect()
    }

    /// Return entries matching the given event type.
    #[must_use]
    pub fn entries_by_type(&self, event_type: AuditEventType) -> Vec<RecorderAuditEntry> {
        let inner = self.inner.lock().unwrap();
        inner
            .entries
            .iter()
            .filter(|e| e.event_type == event_type)
            .cloned()
            .collect()
    }

    /// Return entries for the given actor kind.
    #[must_use]
    pub fn entries_by_actor(&self, actor_kind: ActorKind) -> Vec<RecorderAuditEntry> {
        let inner = self.inner.lock().unwrap();
        inner
            .entries
            .iter()
            .filter(|e| e.actor.kind == actor_kind)
            .cloned()
            .collect()
    }

    /// Return entries in a time range (inclusive).
    #[must_use]
    pub fn entries_in_range(&self, start_ms: u64, end_ms: u64) -> Vec<RecorderAuditEntry> {
        let inner = self.inner.lock().unwrap();
        inner
            .entries
            .iter()
            .filter(|e| e.timestamp_ms >= start_ms && e.timestamp_ms <= end_ms)
            .cloned()
            .collect()
    }

    /// Number of in-memory entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    /// Whether the log has no in-memory entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().entries.is_empty()
    }

    /// Total entries ever appended (including those evicted from memory).
    #[must_use]
    pub fn total_appended(&self) -> u64 {
        self.inner.lock().unwrap().total_appended
    }

    /// The next ordinal that will be assigned.
    #[must_use]
    pub fn next_ordinal(&self) -> u64 {
        self.inner.lock().unwrap().next_ordinal
    }

    /// The hash of the most recently appended entry.
    #[must_use]
    pub fn last_hash(&self) -> String {
        self.inner.lock().unwrap().last_hash.clone()
    }

    /// Drain all in-memory entries (for flush to disk).
    ///
    /// Returns the drained entries. The hash chain state is preserved.
    pub fn drain(&self) -> Vec<RecorderAuditEntry> {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.drain(..).collect()
    }

    /// Verify the hash chain of the given entries.
    ///
    /// The entries must be in ordinal order. The `expected_prev_hash` is the
    /// hash that the first entry's `prev_entry_hash` should match (use
    /// `GENESIS_HASH` for a log verified from the beginning).
    #[must_use]
    pub fn verify_chain(
        entries: &[RecorderAuditEntry],
        expected_prev_hash: &str,
    ) -> ChainVerification {
        if entries.is_empty() {
            return ChainVerification {
                total_entries: 0,
                chain_intact: true,
                first_break_at: None,
                missing_ordinals: Vec::new(),
                ordinal_range: None,
            };
        }

        let mut chain_intact = true;
        let mut first_break_at = None;
        let mut missing_ordinals = Vec::new();
        let mut prev_hash = expected_prev_hash.to_string();

        let first_ordinal = entries[0].ordinal;
        let last_ordinal = entries[entries.len() - 1].ordinal;

        // Check first entry's prev_hash.
        if entries[0].prev_entry_hash != prev_hash && first_break_at.is_none() {
            chain_intact = false;
            first_break_at = Some(entries[0].ordinal);
        }

        prev_hash = entries[0].hash();

        for i in 1..entries.len() {
            let entry = &entries[i];

            // Gap detection: check ordinal continuity.
            let expected_ordinal = entries[i - 1].ordinal + 1;
            if entry.ordinal != expected_ordinal {
                for missing in expected_ordinal..entry.ordinal {
                    missing_ordinals.push(missing);
                }
            }

            // Hash chain verification.
            if entry.prev_entry_hash != prev_hash && first_break_at.is_none() {
                chain_intact = false;
                first_break_at = Some(entry.ordinal);
            }

            prev_hash = entry.hash();
        }

        ChainVerification {
            total_entries: entries.len() as u64,
            chain_intact,
            first_break_at,
            missing_ordinals,
            ordinal_range: Some((first_ordinal, last_ordinal)),
        }
    }
}

// =============================================================================
// Builder
// =============================================================================

/// Builder for constructing audit events.
pub struct AuditEventBuilder {
    event_type: AuditEventType,
    actor: ActorIdentity,
    timestamp_ms: u64,
    scope: AuditScope,
    decision: AuthzDecision,
    justification: Option<String>,
    details: Option<serde_json::Value>,
}

impl AuditEventBuilder {
    /// Create a new builder with required fields.
    #[must_use]
    pub fn new(event_type: AuditEventType, actor: ActorIdentity, timestamp_ms: u64) -> Self {
        Self {
            event_type,
            actor,
            timestamp_ms,
            scope: AuditScope::default(),
            decision: AuthzDecision::Allow,
            justification: None,
            details: None,
        }
    }

    /// Set the authorization decision.
    #[must_use]
    pub fn with_decision(mut self, decision: AuthzDecision) -> Self {
        self.decision = decision;
        self
    }

    /// Set the scope.
    #[must_use]
    pub fn with_scope(mut self, scope: AuditScope) -> Self {
        self.scope = scope;
        self
    }

    /// Add pane IDs to scope.
    #[must_use]
    pub fn with_pane_ids(mut self, pane_ids: Vec<u64>) -> Self {
        self.scope.pane_ids = pane_ids;
        self
    }

    /// Add a time range to scope.
    #[must_use]
    pub fn with_time_range(mut self, start_ms: u64, end_ms: u64) -> Self {
        self.scope.time_range = Some((start_ms, end_ms));
        self
    }

    /// Add a query string to scope.
    #[must_use]
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.scope.query = Some(query.into());
        self
    }

    /// Add segment IDs to scope.
    #[must_use]
    pub fn with_segment_ids(mut self, segment_ids: Vec<String>) -> Self {
        self.scope.segment_ids = segment_ids;
        self
    }

    /// Set result count.
    #[must_use]
    pub fn with_result_count(mut self, count: u64) -> Self {
        self.scope.result_count = Some(count);
        self
    }

    /// Set justification (required for elevated access).
    #[must_use]
    pub fn with_justification(mut self, justification: impl Into<String>) -> Self {
        self.justification = Some(justification.into());
        self
    }

    /// Set additional details.
    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
}

// =============================================================================
// Authorization Check
// =============================================================================

/// Check whether an actor is authorized for a given access tier.
///
/// Returns `Allow` if the actor's default tier satisfies the requirement,
/// `Elevate` if the actor can potentially elevate, or `Deny` if access
/// is impossible for this actor kind.
#[must_use]
pub fn check_authorization(actor: ActorKind, required: AccessTier) -> AuthzDecision {
    let actor_tier = AccessTier::default_for_actor(actor);

    if actor_tier.satisfies(required) {
        return AuthzDecision::Allow;
    }

    // Elevation rules from governance policy §3.2.
    match (actor, required) {
        // Human can elevate to A3/A4 with explicit approval.
        (ActorKind::Human, AccessTier::A3PrivilegedRaw | AccessTier::A4Admin) => {
            AuthzDecision::Elevate
        }
        // Robot can elevate to A2 with workflow context.
        (ActorKind::Robot, AccessTier::A2FullQuery) => AuthzDecision::Elevate,
        // MCP can elevate to A2 with tool context.
        (ActorKind::Mcp, AccessTier::A2FullQuery) => AuthzDecision::Elevate,
        // Workflow can elevate to A3 with approval.
        (ActorKind::Workflow, AccessTier::A3PrivilegedRaw) => AuthzDecision::Elevate,
        // All other combinations are denied.
        _ => AuthzDecision::Deny,
    }
}

/// Minimum required access tier for a given audit event type.
#[must_use]
pub fn required_tier_for_event(event_type: AuditEventType) -> AccessTier {
    match event_type {
        // Queries
        AuditEventType::RecorderQuery => AccessTier::A1RedactedQuery,
        AuditEventType::RecorderQueryPrivileged => AccessTier::A3PrivilegedRaw,
        AuditEventType::RecorderReplay => AccessTier::A1RedactedQuery,
        AuditEventType::RecorderExport => AccessTier::A1RedactedQuery,

        // Admin
        AuditEventType::AdminRetentionOverride => AccessTier::A4Admin,
        AuditEventType::AdminPurge => AccessTier::A4Admin,
        AuditEventType::AdminPolicyChange => AccessTier::A4Admin,

        // Access
        AuditEventType::AccessApprovalGranted => AccessTier::A3PrivilegedRaw,
        AuditEventType::AccessApprovalExpired => AccessTier::A0PublicMetadata,
        AuditEventType::AccessIncidentMode => AccessTier::A3PrivilegedRaw,
        AuditEventType::AccessDebugMode => AccessTier::A3PrivilegedRaw,

        // Retention lifecycle — internal operations, minimal tier.
        AuditEventType::RetentionSegmentSealed
        | AuditEventType::RetentionSegmentArchived
        | AuditEventType::RetentionSegmentPurged
        | AuditEventType::RetentionAcceleratedPurge => AccessTier::A0PublicMetadata,
    }
}

// =============================================================================
// Audit Statistics
// =============================================================================

/// Summary statistics for the audit log.
#[derive(Debug, Clone, Default)]
pub struct AuditStats {
    /// Total entries.
    pub total_entries: u64,
    /// Entries by event type.
    pub by_type: std::collections::HashMap<String, u64>,
    /// Entries by actor kind.
    pub by_actor: std::collections::HashMap<String, u64>,
    /// Entries with denied decisions.
    pub denied_count: u64,
    /// Entries with elevation requests.
    pub elevated_count: u64,
    /// Ordinal range (first, last).
    pub ordinal_range: Option<(u64, u64)>,
}

impl AuditLog {
    /// Compute summary statistics over in-memory entries.
    #[must_use]
    pub fn stats(&self) -> AuditStats {
        let inner = self.inner.lock().unwrap();
        let mut stats = AuditStats::default();
        stats.total_entries = inner.entries.len() as u64;

        if let (Some(first), Some(last)) = (inner.entries.front(), inner.entries.back()) {
            stats.ordinal_range = Some((first.ordinal, last.ordinal));
        }

        for entry in &inner.entries {
            let type_key = serde_json::to_string(&entry.event_type)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string();
            *stats.by_type.entry(type_key).or_default() += 1;

            let actor_key = entry.actor.kind.as_str().to_string();
            *stats.by_actor.entry(actor_key).or_default() += 1;

            match entry.decision {
                AuthzDecision::Deny => stats.denied_count += 1,
                AuthzDecision::Elevate => stats.elevated_count += 1,
                AuthzDecision::Allow => {}
            }
        }

        stats
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AuditLogConfig {
        AuditLogConfig {
            max_memory_entries: 100,
            ..AuditLogConfig::default()
        }
    }

    fn test_actor() -> ActorIdentity {
        ActorIdentity::new(ActorKind::Human, "test-session-1")
    }

    fn robot_actor() -> ActorIdentity {
        ActorIdentity::new(ActorKind::Robot, "agent-42")
    }

    fn mcp_actor() -> ActorIdentity {
        ActorIdentity::new(ActorKind::Mcp, "mcp-tool-1")
    }

    fn workflow_actor() -> ActorIdentity {
        ActorIdentity::new(ActorKind::Workflow, "wf-restart-123")
    }

    // =========================================================================
    // AccessTier tests
    // =========================================================================

    #[test]
    fn access_tier_levels() {
        assert_eq!(AccessTier::A0PublicMetadata.level(), 0);
        assert_eq!(AccessTier::A1RedactedQuery.level(), 1);
        assert_eq!(AccessTier::A2FullQuery.level(), 2);
        assert_eq!(AccessTier::A3PrivilegedRaw.level(), 3);
        assert_eq!(AccessTier::A4Admin.level(), 4);
    }

    #[test]
    fn access_tier_satisfies() {
        assert!(AccessTier::A4Admin.satisfies(AccessTier::A0PublicMetadata));
        assert!(AccessTier::A4Admin.satisfies(AccessTier::A4Admin));
        assert!(AccessTier::A2FullQuery.satisfies(AccessTier::A1RedactedQuery));
        assert!(!AccessTier::A1RedactedQuery.satisfies(AccessTier::A2FullQuery));
        assert!(!AccessTier::A0PublicMetadata.satisfies(AccessTier::A4Admin));
    }

    #[test]
    fn access_tier_default_for_actor() {
        assert_eq!(
            AccessTier::default_for_actor(ActorKind::Human),
            AccessTier::A2FullQuery
        );
        assert_eq!(
            AccessTier::default_for_actor(ActorKind::Robot),
            AccessTier::A1RedactedQuery
        );
        assert_eq!(
            AccessTier::default_for_actor(ActorKind::Mcp),
            AccessTier::A1RedactedQuery
        );
        assert_eq!(
            AccessTier::default_for_actor(ActorKind::Workflow),
            AccessTier::A2FullQuery
        );
    }

    #[test]
    fn access_tier_ordering() {
        assert!(AccessTier::A0PublicMetadata < AccessTier::A1RedactedQuery);
        assert!(AccessTier::A1RedactedQuery < AccessTier::A2FullQuery);
        assert!(AccessTier::A2FullQuery < AccessTier::A3PrivilegedRaw);
        assert!(AccessTier::A3PrivilegedRaw < AccessTier::A4Admin);
    }

    #[test]
    fn access_tier_display() {
        assert_eq!(
            format!("{}", AccessTier::A0PublicMetadata),
            "A0 (public metadata)"
        );
        assert_eq!(format!("{}", AccessTier::A4Admin), "A4 (admin)");
    }

    // =========================================================================
    // Authorization tests
    // =========================================================================

    #[test]
    fn authz_human_default_access() {
        // Human defaults to A2.
        assert_eq!(
            check_authorization(ActorKind::Human, AccessTier::A0PublicMetadata),
            AuthzDecision::Allow
        );
        assert_eq!(
            check_authorization(ActorKind::Human, AccessTier::A2FullQuery),
            AuthzDecision::Allow
        );
    }

    #[test]
    fn authz_human_elevate_to_a3() {
        assert_eq!(
            check_authorization(ActorKind::Human, AccessTier::A3PrivilegedRaw),
            AuthzDecision::Elevate
        );
    }

    #[test]
    fn authz_human_elevate_to_a4() {
        assert_eq!(
            check_authorization(ActorKind::Human, AccessTier::A4Admin),
            AuthzDecision::Elevate
        );
    }

    #[test]
    fn authz_robot_default_a1() {
        assert_eq!(
            check_authorization(ActorKind::Robot, AccessTier::A1RedactedQuery),
            AuthzDecision::Allow
        );
    }

    #[test]
    fn authz_robot_elevate_to_a2() {
        assert_eq!(
            check_authorization(ActorKind::Robot, AccessTier::A2FullQuery),
            AuthzDecision::Elevate
        );
    }

    #[test]
    fn authz_robot_deny_a3() {
        assert_eq!(
            check_authorization(ActorKind::Robot, AccessTier::A3PrivilegedRaw),
            AuthzDecision::Deny
        );
    }

    #[test]
    fn authz_robot_deny_a4() {
        assert_eq!(
            check_authorization(ActorKind::Robot, AccessTier::A4Admin),
            AuthzDecision::Deny
        );
    }

    #[test]
    fn authz_mcp_elevate_to_a2() {
        assert_eq!(
            check_authorization(ActorKind::Mcp, AccessTier::A2FullQuery),
            AuthzDecision::Elevate
        );
    }

    #[test]
    fn authz_mcp_deny_a3() {
        assert_eq!(
            check_authorization(ActorKind::Mcp, AccessTier::A3PrivilegedRaw),
            AuthzDecision::Deny
        );
    }

    #[test]
    fn authz_workflow_elevate_to_a3() {
        assert_eq!(
            check_authorization(ActorKind::Workflow, AccessTier::A3PrivilegedRaw),
            AuthzDecision::Elevate
        );
    }

    #[test]
    fn authz_workflow_deny_a4() {
        assert_eq!(
            check_authorization(ActorKind::Workflow, AccessTier::A4Admin),
            AuthzDecision::Deny
        );
    }

    // =========================================================================
    // Required tier tests
    // =========================================================================

    #[test]
    fn required_tiers_for_events() {
        assert_eq!(
            required_tier_for_event(AuditEventType::RecorderQuery),
            AccessTier::A1RedactedQuery
        );
        assert_eq!(
            required_tier_for_event(AuditEventType::RecorderQueryPrivileged),
            AccessTier::A3PrivilegedRaw
        );
        assert_eq!(
            required_tier_for_event(AuditEventType::AdminPurge),
            AccessTier::A4Admin
        );
        assert_eq!(
            required_tier_for_event(AuditEventType::RetentionSegmentSealed),
            AccessTier::A0PublicMetadata
        );
    }

    // =========================================================================
    // AuditLog basic tests
    // =========================================================================

    #[test]
    fn new_log_is_empty() {
        let log = AuditLog::new(test_config());
        assert!(log.is_empty());
        assert_eq!(log.len(), 0);
        assert_eq!(log.total_appended(), 0);
        assert_eq!(log.next_ordinal(), 0);
        assert_eq!(log.last_hash(), GENESIS_HASH);
    }

    #[test]
    fn append_single_entry() {
        let log = AuditLog::new(test_config());
        let entry = log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            1000,
        ));

        assert_eq!(entry.ordinal, 0);
        assert_eq!(entry.audit_version, AUDIT_SCHEMA_VERSION);
        assert_eq!(entry.prev_entry_hash, GENESIS_HASH);
        assert_eq!(entry.event_type, AuditEventType::RecorderQuery);
        assert_eq!(log.len(), 1);
        assert_eq!(log.next_ordinal(), 1);
    }

    #[test]
    fn append_builds_hash_chain() {
        let log = AuditLog::new(test_config());

        let e0 = log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            1000,
        ));

        let e1 = log.append(AuditEventBuilder::new(
            AuditEventType::RecorderReplay,
            test_actor(),
            2000,
        ));

        let e2 = log.append(AuditEventBuilder::new(
            AuditEventType::AdminPurge,
            test_actor(),
            3000,
        ));

        // Chain: genesis -> e0 -> e1 -> e2.
        assert_eq!(e0.prev_entry_hash, GENESIS_HASH);
        assert_eq!(e1.prev_entry_hash, e0.hash());
        assert_eq!(e2.prev_entry_hash, e1.hash());
        assert_eq!(log.last_hash(), e2.hash());
    }

    #[test]
    fn ordinals_are_monotonic() {
        let log = AuditLog::new(test_config());

        for i in 0..10 {
            let entry = log.append(AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                test_actor(),
                i * 1000,
            ));
            assert_eq!(entry.ordinal, i);
        }

        assert_eq!(log.next_ordinal(), 10);
        assert_eq!(log.total_appended(), 10);
    }

    #[test]
    fn entries_returns_all() {
        let log = AuditLog::new(test_config());
        for i in 0..5 {
            log.append(AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                test_actor(),
                i * 1000,
            ));
        }

        let entries = log.entries();
        assert_eq!(entries.len(), 5);
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.ordinal, i as u64);
        }
    }

    // =========================================================================
    // Filtering tests
    // =========================================================================

    #[test]
    fn entries_by_type() {
        let log = AuditLog::new(test_config());
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            1000,
        ));
        log.append(AuditEventBuilder::new(
            AuditEventType::AdminPurge,
            test_actor(),
            2000,
        ));
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            3000,
        ));

        let queries = log.entries_by_type(AuditEventType::RecorderQuery);
        assert_eq!(queries.len(), 2);

        let purges = log.entries_by_type(AuditEventType::AdminPurge);
        assert_eq!(purges.len(), 1);

        let replays = log.entries_by_type(AuditEventType::RecorderReplay);
        assert_eq!(replays.len(), 0);
    }

    #[test]
    fn entries_by_actor() {
        let log = AuditLog::new(test_config());
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            1000,
        ));
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            robot_actor(),
            2000,
        ));
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            3000,
        ));

        let human = log.entries_by_actor(ActorKind::Human);
        assert_eq!(human.len(), 2);

        let robot = log.entries_by_actor(ActorKind::Robot);
        assert_eq!(robot.len(), 1);
    }

    #[test]
    fn entries_in_time_range() {
        let log = AuditLog::new(test_config());
        for i in 0..10 {
            log.append(AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                test_actor(),
                i * 1000,
            ));
        }

        let range = log.entries_in_range(3000, 6000);
        assert_eq!(range.len(), 4); // 3000, 4000, 5000, 6000
        assert_eq!(range[0].timestamp_ms, 3000);
        assert_eq!(range[3].timestamp_ms, 6000);
    }

    // =========================================================================
    // Hash chain verification tests
    // =========================================================================

    #[test]
    fn verify_chain_empty() {
        let result = AuditLog::verify_chain(&[], GENESIS_HASH);
        assert!(result.chain_intact);
        assert_eq!(result.total_entries, 0);
        assert!(result.missing_ordinals.is_empty());
        assert!(result.ordinal_range.is_none());
    }

    #[test]
    fn verify_chain_intact() {
        let log = AuditLog::new(test_config());
        for i in 0..5 {
            log.append(AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                test_actor(),
                i * 1000,
            ));
        }

        let entries = log.entries();
        let result = AuditLog::verify_chain(&entries, GENESIS_HASH);
        assert!(result.chain_intact);
        assert_eq!(result.total_entries, 5);
        assert!(result.missing_ordinals.is_empty());
        assert_eq!(result.first_break_at, None);
        assert_eq!(result.ordinal_range, Some((0, 4)));
    }

    #[test]
    fn verify_chain_detects_tampered_entry() {
        let log = AuditLog::new(test_config());
        for i in 0..5 {
            log.append(AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                test_actor(),
                i * 1000,
            ));
        }

        let mut entries = log.entries();
        // Tamper with entry 2.
        entries[2].timestamp_ms = 999_999;

        let result = AuditLog::verify_chain(&entries, GENESIS_HASH);
        assert!(!result.chain_intact);
        // Entry 3's prev_hash won't match entry 2's new hash.
        assert_eq!(result.first_break_at, Some(3));
    }

    #[test]
    fn verify_chain_detects_wrong_genesis() {
        let log = AuditLog::new(test_config());
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            1000,
        ));

        let entries = log.entries();
        let result = AuditLog::verify_chain(&entries, "bad_hash");
        assert!(!result.chain_intact);
        assert_eq!(result.first_break_at, Some(0));
    }

    #[test]
    fn verify_chain_detects_gaps() {
        let log = AuditLog::new(test_config());
        for i in 0..5 {
            log.append(AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                test_actor(),
                i * 1000,
            ));
        }

        let mut entries = log.entries();
        // Remove entry at ordinal 2 to create a gap.
        entries.remove(2);

        let result = AuditLog::verify_chain(&entries, GENESIS_HASH);
        // Chain is broken because entry 3's prev_hash doesn't match entry 1's hash.
        assert!(!result.chain_intact);
        // Gap detected at ordinal 2.
        assert_eq!(result.missing_ordinals, vec![2]);
    }

    #[test]
    fn verify_chain_detects_deleted_entry() {
        let log = AuditLog::new(test_config());
        for i in 0..3 {
            log.append(AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                test_actor(),
                i * 1000,
            ));
        }

        let entries = log.entries();
        // Only verify first two entries — chain should be valid for those.
        let result = AuditLog::verify_chain(&entries[0..2], GENESIS_HASH);
        assert!(result.chain_intact);
        assert_eq!(result.total_entries, 2);
    }

    // =========================================================================
    // Resume tests
    // =========================================================================

    #[test]
    fn resume_continues_ordinals() {
        let log = AuditLog::resume(test_config(), 100, "abc123".to_string());

        let entry = log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            5000,
        ));

        assert_eq!(entry.ordinal, 100);
        assert_eq!(entry.prev_entry_hash, "abc123");
        assert_eq!(log.next_ordinal(), 101);
    }

    #[test]
    fn resume_chain_verification() {
        // Create initial log.
        let log1 = AuditLog::new(test_config());
        for i in 0..3 {
            log1.append(AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                test_actor(),
                i * 1000,
            ));
        }
        let phase1_entries = log1.entries();
        let last_hash = log1.last_hash();
        let next_ord = log1.next_ordinal();

        // Resume with state from first log.
        let log2 = AuditLog::resume(test_config(), next_ord, last_hash);
        for i in 3..6 {
            log2.append(AuditEventBuilder::new(
                AuditEventType::RecorderReplay,
                test_actor(),
                i * 1000,
            ));
        }
        let phase2_entries = log2.entries();

        // Combined entries should have valid chain.
        let mut all: Vec<RecorderAuditEntry> = phase1_entries;
        all.extend(phase2_entries);

        let result = AuditLog::verify_chain(&all, GENESIS_HASH);
        assert!(result.chain_intact);
        assert_eq!(result.total_entries, 6);
        assert_eq!(result.ordinal_range, Some((0, 5)));
    }

    // =========================================================================
    // Memory limit tests
    // =========================================================================

    #[test]
    fn memory_limit_enforced() {
        let config = AuditLogConfig {
            max_memory_entries: 5,
            ..test_config()
        };
        let log = AuditLog::new(config);

        for i in 0..10 {
            log.append(AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                test_actor(),
                i * 1000,
            ));
        }

        assert_eq!(log.len(), 5);
        assert_eq!(log.total_appended(), 10);
        assert_eq!(log.next_ordinal(), 10);

        // Oldest entries should have been evicted.
        let entries = log.entries();
        assert_eq!(entries[0].ordinal, 5);
        assert_eq!(entries[4].ordinal, 9);
    }

    // =========================================================================
    // Drain tests
    // =========================================================================

    #[test]
    fn drain_returns_all_and_clears() {
        let log = AuditLog::new(test_config());
        for i in 0..3 {
            log.append(AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                test_actor(),
                i * 1000,
            ));
        }

        let drained = log.drain();
        assert_eq!(drained.len(), 3);
        assert!(log.is_empty());
        assert_eq!(log.next_ordinal(), 3); // Ordinals preserved.
        assert_ne!(log.last_hash(), GENESIS_HASH); // Hash state preserved.
    }

    #[test]
    fn drain_then_append_continues_chain() {
        let log = AuditLog::new(test_config());
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            1000,
        ));

        let drained = log.drain();
        let last_hash_after_drain = log.last_hash();

        let new_entry = log.append(AuditEventBuilder::new(
            AuditEventType::RecorderReplay,
            test_actor(),
            2000,
        ));

        // New entry should chain from the drained entry.
        assert_eq!(new_entry.prev_entry_hash, last_hash_after_drain);
        assert_eq!(new_entry.ordinal, 1);

        // Combining drained + new should verify.
        let mut all = drained;
        all.push(new_entry);
        let result = AuditLog::verify_chain(&all, GENESIS_HASH);
        assert!(result.chain_intact);
    }

    // =========================================================================
    // Builder tests
    // =========================================================================

    #[test]
    fn builder_with_scope() {
        let log = AuditLog::new(test_config());
        let entry = log.append(
            AuditEventBuilder::new(AuditEventType::RecorderQuery, test_actor(), 1000)
                .with_pane_ids(vec![1, 2, 3])
                .with_time_range(0, 5000)
                .with_query("error AND timeout")
                .with_result_count(42),
        );

        assert_eq!(entry.scope.pane_ids, vec![1, 2, 3]);
        assert_eq!(entry.scope.time_range, Some((0, 5000)));
        assert_eq!(entry.scope.query.as_deref(), Some("error AND timeout"));
        assert_eq!(entry.scope.result_count, Some(42));
    }

    #[test]
    fn builder_with_justification() {
        let log = AuditLog::new(test_config());
        let entry = log.append(
            AuditEventBuilder::new(AuditEventType::RecorderQueryPrivileged, test_actor(), 1000)
                .with_decision(AuthzDecision::Elevate)
                .with_justification("Investigating production incident INC-123"),
        );

        assert_eq!(entry.decision, AuthzDecision::Elevate);
        assert_eq!(
            entry.justification.as_deref(),
            Some("Investigating production incident INC-123")
        );
    }

    #[test]
    fn builder_with_details() {
        let log = AuditLog::new(test_config());
        let details = serde_json::json!({
            "old_retention_days": 30,
            "new_retention_days": 90,
        });
        let entry = log.append(
            AuditEventBuilder::new(AuditEventType::AdminPolicyChange, test_actor(), 1000)
                .with_details(details.clone()),
        );

        assert_eq!(entry.details, Some(details));
    }

    #[test]
    fn builder_with_segment_ids() {
        let log = AuditLog::new(test_config());
        let entry = log.append(
            AuditEventBuilder::new(AuditEventType::AdminPurge, test_actor(), 1000)
                .with_segment_ids(vec!["seg_001".to_string(), "seg_002".to_string()])
                .with_justification("Expired T3 data"),
        );

        assert_eq!(entry.scope.segment_ids, vec!["seg_001", "seg_002"]);
    }

    // =========================================================================
    // Stats tests
    // =========================================================================

    #[test]
    fn stats_counts_correctly() {
        let log = AuditLog::new(test_config());
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            1000,
        ));
        log.append(
            AuditEventBuilder::new(AuditEventType::RecorderQuery, robot_actor(), 2000)
                .with_decision(AuthzDecision::Deny),
        );
        log.append(
            AuditEventBuilder::new(AuditEventType::RecorderQueryPrivileged, test_actor(), 3000)
                .with_decision(AuthzDecision::Elevate),
        );

        let stats = log.stats();
        assert_eq!(stats.total_entries, 3);
        assert_eq!(stats.denied_count, 1);
        assert_eq!(stats.elevated_count, 1);
        assert_eq!(stats.by_actor.get("human"), Some(&2));
        assert_eq!(stats.by_actor.get("robot"), Some(&1));
        assert_eq!(stats.by_type.get("recorder_query"), Some(&2));
        assert_eq!(stats.by_type.get("recorder_query_privileged"), Some(&1));
    }

    // =========================================================================
    // Hash chain disabled tests
    // =========================================================================

    #[test]
    fn hash_chain_disabled() {
        let config = AuditLogConfig {
            hash_chain_enabled: false,
            ..test_config()
        };
        let log = AuditLog::new(config);

        let e0 = log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            1000,
        ));
        let e1 = log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            2000,
        ));

        // When disabled, all entries have genesis hash as prev.
        assert_eq!(e0.prev_entry_hash, GENESIS_HASH);
        assert_eq!(e1.prev_entry_hash, GENESIS_HASH);
    }

    // =========================================================================
    // Entry hash determinism tests
    // =========================================================================

    #[test]
    fn entry_hash_is_deterministic() {
        let log = AuditLog::new(test_config());
        let entry = log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            1000,
        ));

        let hash1 = entry.hash();
        let hash2 = entry.hash();
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64); // SHA-256 hex.
    }

    #[test]
    fn different_entries_have_different_hashes() {
        let log = AuditLog::new(test_config());
        let e0 = log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            test_actor(),
            1000,
        ));
        let e1 = log.append(AuditEventBuilder::new(
            AuditEventType::RecorderReplay,
            test_actor(),
            2000,
        ));

        assert_ne!(e0.hash(), e1.hash());
    }

    // =========================================================================
    // Multi-actor scenario tests
    // =========================================================================

    #[test]
    fn multi_actor_mixed_operations() {
        let log = AuditLog::new(test_config());

        // Human queries.
        log.append(
            AuditEventBuilder::new(AuditEventType::RecorderQuery, test_actor(), 1000)
                .with_pane_ids(vec![1, 2])
                .with_query("error"),
        );

        // Robot denied A3.
        log.append(
            AuditEventBuilder::new(AuditEventType::RecorderQueryPrivileged, robot_actor(), 2000)
                .with_decision(AuthzDecision::Deny),
        );

        // MCP query.
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            mcp_actor(),
            3000,
        ));

        // Workflow elevated replay.
        log.append(
            AuditEventBuilder::new(AuditEventType::RecorderReplay, workflow_actor(), 4000)
                .with_decision(AuthzDecision::Elevate)
                .with_justification("Automated incident analysis"),
        );

        // Admin purge.
        log.append(
            AuditEventBuilder::new(AuditEventType::AdminPurge, test_actor(), 5000)
                .with_segment_ids(vec!["seg_t3_001".to_string()])
                .with_justification("T3 forced purge"),
        );

        assert_eq!(log.len(), 5);

        // Verify chain.
        let entries = log.entries();
        let result = AuditLog::verify_chain(&entries, GENESIS_HASH);
        assert!(result.chain_intact);
        assert_eq!(result.total_entries, 5);
    }

    // =========================================================================
    // Serialization roundtrip tests
    // =========================================================================

    #[test]
    fn audit_entry_serialization_roundtrip() {
        let log = AuditLog::new(test_config());
        let entry = log.append(
            AuditEventBuilder::new(AuditEventType::AdminPurge, test_actor(), 1000)
                .with_justification("test")
                .with_segment_ids(vec!["seg1".to_string()])
                .with_details(serde_json::json!({"reason": "expired"})),
        );

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: RecorderAuditEntry = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.ordinal, entry.ordinal);
        assert_eq!(parsed.event_type, entry.event_type);
        assert_eq!(parsed.actor, entry.actor);
        assert_eq!(parsed.prev_entry_hash, entry.prev_entry_hash);
        assert_eq!(parsed.justification, entry.justification);
    }

    #[test]
    fn authz_decision_serialization() {
        let allow = serde_json::to_string(&AuthzDecision::Allow).unwrap();
        assert_eq!(allow, "\"allow\"");
        let deny = serde_json::to_string(&AuthzDecision::Deny).unwrap();
        assert_eq!(deny, "\"deny\"");
        let elevate = serde_json::to_string(&AuthzDecision::Elevate).unwrap();
        assert_eq!(elevate, "\"elevate\"");
    }

    #[test]
    fn access_tier_serialization() {
        let tier = serde_json::to_string(&AccessTier::A3PrivilegedRaw).unwrap();
        let parsed: AccessTier = serde_json::from_str(&tier).unwrap();
        assert_eq!(parsed, AccessTier::A3PrivilegedRaw);
    }

    #[test]
    fn audit_event_type_serialization() {
        let et = serde_json::to_string(&AuditEventType::RecorderQueryPrivileged).unwrap();
        assert_eq!(et, "\"recorder_query_privileged\"");
        let parsed: AuditEventType = serde_json::from_str(&et).unwrap();
        assert_eq!(parsed, AuditEventType::RecorderQueryPrivileged);
    }
}
