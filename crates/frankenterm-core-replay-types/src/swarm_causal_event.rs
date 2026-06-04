//! Operational causal-event contract for swarm flight recorder evidence.
//!
//! The legacy [`crate::recorder_metadata::RECORDER_EVENT_SCHEMA_VERSION_V1`]
//! event contract captures mux ingress/egress. This module defines the
//! companion envelope for operational evidence around that stream: Beads,
//! RCH, Agent Mail, git, MCP, robot, policy, and operator events.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Schema version for the operational causal-event envelope.
pub const SWARM_CAUSAL_EVENT_SCHEMA_VERSION_V1: &str = "ft.swarm.causal_event.v1";

/// Default payload byte ceiling for persisted operational event bodies.
pub const DEFAULT_MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// Default total payload byte ceiling for a reconstructed incident.
pub const DEFAULT_MAX_INCIDENT_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// Default event count ceiling for a reconstructed incident.
pub const DEFAULT_MAX_INCIDENT_EVENTS: usize = 4096;

/// Conservative payload byte ceiling for pane text snapshots.
pub const DEFAULT_PANE_PAYLOAD_BYTES: usize = 16 * 1024;

/// Conservative payload byte ceiling for large operational log snippets.
pub const DEFAULT_LOG_PAYLOAD_BYTES: usize = 32 * 1024;

/// Source subsystem that produced the causal event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmCausalEventSource {
    Pane,
    Robot,
    Mcp,
    Workflow,
    Policy,
    Beads,
    Rch,
    AgentMail,
    Git,
    Operator,
    Runtime,
    SourceUnavailable,
}

/// Coarse event class used for incident DAG grouping and fail-closed proof review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalEventClass {
    SourcePass,
    SourceFailure,
    InfrastructureFailure,
    DirtyTreeContamination,
    CommunicationOutage,
    PolicyDenial,
    OperatorCancellation,
    EvidenceUnavailable,
    Informational,
}

/// Redaction status applied before persistence/export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalRedactionStatus {
    NotRequired,
    Redacted,
    Truncated,
    HashOnly,
    Unavailable,
}

/// Payload sensitivity declaration used by validation to fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalPayloadSensitivity {
    Structural,
    UserText,
    SecretBearing,
    Redacted,
}

/// Retention class for continuous flight-recorder operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalRetentionClass {
    Ephemeral,
    Standard,
    Proof,
    Audit,
}

/// Machine-readable reason why raw payload content is absent or reduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalPayloadOmissionReason {
    RedactedSecret,
    TruncatedBySourceBudget,
    HashOnlyByPolicy,
    ExpiredByRetention,
    SourceUnavailable,
}

/// Byte and retention budget applied to one event source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalSourceBudget {
    pub max_payload_bytes: usize,
    pub retention_seconds: Option<u64>,
}

/// Aggregate budget for incident replay/export envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalIncidentBudget {
    pub max_total_payload_bytes: usize,
    pub max_events: usize,
}

/// Caller-supplied privacy policy for building a causal event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CausalPrivacyPolicy {
    pub max_payload_bytes: usize,
    pub retention_seconds: Option<u64>,
    pub omission_reason: Option<CausalPayloadOmissionReason>,
    pub omitted_payload_hash_sha256: Option<String>,
    pub omitted_payload_bytes: Option<usize>,
}

/// Payload-free privacy report for operator audit/export surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalPrivacyAudit {
    pub source: SwarmCausalEventSource,
    pub redaction_status: CausalRedactionStatus,
    pub retention_class: CausalRetentionClass,
    pub payload_bytes: usize,
    pub max_payload_bytes: usize,
    pub retention_seconds: Option<u64>,
    pub omission_reason: Option<CausalPayloadOmissionReason>,
    pub omitted_payload_hash_sha256: Option<String>,
    pub omitted_payload_bytes: Option<usize>,
}

/// Durable reference to source evidence outside the event payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalArtifactRef {
    pub kind: String,
    pub uri: String,
    pub sha256: Option<String>,
    pub size_bytes: Option<u64>,
}

/// Stable cross-system keys carried by every causal event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CausalCorrelationKeys {
    pub workspace_id: Option<String>,
    pub pane_id: Option<u64>,
    pub session_id: Option<String>,
    pub workflow_id: Option<String>,
    pub bead_id: Option<String>,
    pub thread_id: Option<String>,
    pub rch_build_id: Option<String>,
    pub rch_worker_id: Option<String>,
    pub git_commit: Option<String>,
    pub git_branch: Option<String>,
    pub command_id: Option<String>,
}

/// Causal linkage for deterministic incident DAG construction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CausalLinks {
    pub parent_event_ids: Vec<String>,
    pub caused_by_event_ids: Vec<String>,
    pub root_event_id: Option<String>,
}

/// Redaction, sensitivity, and retention declaration for a causal event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalPrivacy {
    pub sensitivity: CausalPayloadSensitivity,
    pub redaction_status: CausalRedactionStatus,
    pub retention_class: CausalRetentionClass,
    pub payload_hash_sha256: String,
    pub payload_bytes: usize,
    #[serde(default = "default_max_payload_bytes")]
    pub max_payload_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omission_reason: Option<CausalPayloadOmissionReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_payload_hash_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_payload_bytes: Option<usize>,
}

/// Versioned event envelope consumed by the flight-recorder incident DAG.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmCausalEvent {
    pub schema_version: String,
    pub event_id: String,
    pub source: SwarmCausalEventSource,
    pub event_class: CausalEventClass,
    pub occurred_at_ms: u64,
    pub ingested_at_ms: u64,
    pub ingest_sequence: u64,
    pub correlation: CausalCorrelationKeys,
    pub links: CausalLinks,
    pub privacy: CausalPrivacy,
    pub artifacts: Vec<CausalArtifactRef>,
    pub payload: Value,
}

/// Validation failure for the causal-event contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwarmCausalEventError {
    UnsupportedSchemaVersion { found: String },
    EmptyEventId,
    IngestedBeforeOccurred,
    SelfParentLink { event_id: String },
    SelfCauseLink { event_id: String },
    PayloadTooLarge { actual: usize, max: usize },
    PayloadHashMismatch { expected: String, actual: String },
    PayloadByteCountMismatch { expected: usize, actual: usize },
    SecretPayloadNotRedacted,
    MissingOmissionReason { status: CausalRedactionStatus },
    MissingOmittedPayloadReference { status: CausalRedactionStatus },
    InvalidOmittedPayloadHash { hash: String },
    InvalidPayloadBudget { max: usize },
    SecretLikeIdentifier { field: String },
    UnsafeArtifactUri { uri: String },
    MissingUnavailableReason,
    MissingCorrelationKey { source: SwarmCausalEventSource },
    EmptyArtifactUri,
}

impl std::fmt::Display for SwarmCausalEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion { found } => {
                write!(f, "unsupported swarm causal event schema version {found:?}")
            }
            Self::EmptyEventId => write!(f, "event_id must not be empty"),
            Self::IngestedBeforeOccurred => write!(f, "ingested_at_ms precedes occurred_at_ms"),
            Self::SelfParentLink { event_id } => {
                write!(f, "event {event_id} cannot list itself as a parent")
            }
            Self::SelfCauseLink { event_id } => {
                write!(f, "event {event_id} cannot list itself as caused_by")
            }
            Self::PayloadTooLarge { actual, max } => {
                write!(f, "payload is {actual} bytes, max is {max}")
            }
            Self::PayloadHashMismatch { expected, actual } => {
                write!(
                    f,
                    "payload hash mismatch: expected {expected}, actual {actual}"
                )
            }
            Self::PayloadByteCountMismatch { expected, actual } => {
                write!(
                    f,
                    "payload byte count mismatch: expected {expected}, actual {actual}"
                )
            }
            Self::SecretPayloadNotRedacted => {
                write!(
                    f,
                    "secret-bearing payload must be redacted, hash-only, or unavailable"
                )
            }
            Self::MissingOmissionReason { status } => {
                write!(f, "redaction status {status:?} requires an omission reason")
            }
            Self::MissingOmittedPayloadReference { status } => {
                write!(
                    f,
                    "redaction status {status:?} requires omitted payload hash and byte count"
                )
            }
            Self::InvalidOmittedPayloadHash { hash } => {
                write!(
                    f,
                    "omitted payload hash is not a sha256 hex digest: {hash:?}"
                )
            }
            Self::InvalidPayloadBudget { max } => {
                write!(f, "payload budget must be non-zero, got {max}")
            }
            Self::SecretLikeIdentifier { field } => {
                write!(
                    f,
                    "secret-like value is not allowed in identifier field {field}"
                )
            }
            Self::UnsafeArtifactUri { uri } => {
                write!(
                    f,
                    "artifact uri is not safe for persisted replay evidence: {uri:?}"
                )
            }
            Self::MissingUnavailableReason => {
                write!(f, "unavailable source event must include payload.reason")
            }
            Self::MissingCorrelationKey { source } => {
                write!(
                    f,
                    "event source {source:?} is missing its required correlation key"
                )
            }
            Self::EmptyArtifactUri => write!(f, "artifact uri must not be empty"),
        }
    }
}

impl std::error::Error for SwarmCausalEventError {}

impl CausalRedactionStatus {
    fn default_omission_reason(self) -> Option<CausalPayloadOmissionReason> {
        match self {
            Self::NotRequired => None,
            Self::Redacted => Some(CausalPayloadOmissionReason::RedactedSecret),
            Self::Truncated => Some(CausalPayloadOmissionReason::TruncatedBySourceBudget),
            Self::HashOnly => Some(CausalPayloadOmissionReason::HashOnlyByPolicy),
            Self::Unavailable => Some(CausalPayloadOmissionReason::SourceUnavailable),
        }
    }

    fn requires_omission_reason(self) -> bool {
        !matches!(self, Self::NotRequired)
    }
}

impl CausalPrivacyPolicy {
    pub fn for_source(
        source: SwarmCausalEventSource,
        retention_class: CausalRetentionClass,
        redaction_status: CausalRedactionStatus,
    ) -> Self {
        let budget = default_source_budget(source, retention_class);
        Self {
            max_payload_bytes: budget.max_payload_bytes,
            retention_seconds: budget.retention_seconds,
            omission_reason: redaction_status.default_omission_reason(),
            omitted_payload_hash_sha256: None,
            omitted_payload_bytes: None,
        }
    }

    #[must_use]
    pub fn with_omitted_payload_ref(
        mut self,
        omitted_payload_hash_sha256: impl Into<String>,
        omitted_payload_bytes: usize,
    ) -> Self {
        self.omitted_payload_hash_sha256 = Some(omitted_payload_hash_sha256.into());
        self.omitted_payload_bytes = Some(omitted_payload_bytes);
        self
    }
}

impl Default for CausalIncidentBudget {
    fn default() -> Self {
        Self {
            max_total_payload_bytes: DEFAULT_MAX_INCIDENT_PAYLOAD_BYTES,
            max_events: DEFAULT_MAX_INCIDENT_EVENTS,
        }
    }
}

impl SwarmCausalEvent {
    /// Build a validated event and derive payload privacy metadata.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_id: impl Into<String>,
        source: SwarmCausalEventSource,
        event_class: CausalEventClass,
        occurred_at_ms: u64,
        ingested_at_ms: u64,
        ingest_sequence: u64,
        correlation: CausalCorrelationKeys,
        links: CausalLinks,
        sensitivity: CausalPayloadSensitivity,
        redaction_status: CausalRedactionStatus,
        retention_class: CausalRetentionClass,
        artifacts: Vec<CausalArtifactRef>,
        payload: Value,
    ) -> Result<Self, SwarmCausalEventError> {
        Self::new_with_privacy_policy(
            event_id,
            source,
            event_class,
            occurred_at_ms,
            ingested_at_ms,
            ingest_sequence,
            correlation,
            links,
            sensitivity,
            redaction_status,
            retention_class,
            CausalPrivacyPolicy::for_source(source, retention_class, redaction_status),
            artifacts,
            payload,
        )
    }

    /// Build a validated event with an explicit source privacy policy.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_privacy_policy(
        event_id: impl Into<String>,
        source: SwarmCausalEventSource,
        event_class: CausalEventClass,
        occurred_at_ms: u64,
        ingested_at_ms: u64,
        ingest_sequence: u64,
        correlation: CausalCorrelationKeys,
        links: CausalLinks,
        sensitivity: CausalPayloadSensitivity,
        redaction_status: CausalRedactionStatus,
        retention_class: CausalRetentionClass,
        privacy_policy: CausalPrivacyPolicy,
        artifacts: Vec<CausalArtifactRef>,
        payload: Value,
    ) -> Result<Self, SwarmCausalEventError> {
        let payload_bytes = serialized_payload_len(&payload);
        let payload_hash_sha256 = payload_hash(&payload);
        let event = Self {
            schema_version: SWARM_CAUSAL_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.into(),
            source,
            event_class,
            occurred_at_ms,
            ingested_at_ms,
            ingest_sequence,
            correlation,
            links,
            privacy: CausalPrivacy {
                sensitivity,
                redaction_status,
                retention_class,
                payload_hash_sha256,
                payload_bytes,
                max_payload_bytes: privacy_policy.max_payload_bytes,
                retention_seconds: privacy_policy.retention_seconds,
                omission_reason: privacy_policy.omission_reason,
                omitted_payload_hash_sha256: privacy_policy.omitted_payload_hash_sha256,
                omitted_payload_bytes: privacy_policy.omitted_payload_bytes,
            },
            artifacts,
            payload,
        };
        event.validate()?;
        Ok(event)
    }

    /// Validate using the default payload byte ceiling.
    pub fn validate(&self) -> Result<(), SwarmCausalEventError> {
        self.validate_with_max_payload(self.privacy.max_payload_bytes)
    }

    /// Validate using a caller-supplied payload byte ceiling.
    pub fn validate_with_max_payload(
        &self,
        max_payload_bytes: usize,
    ) -> Result<(), SwarmCausalEventError> {
        if self.schema_version != SWARM_CAUSAL_EVENT_SCHEMA_VERSION_V1 {
            return Err(SwarmCausalEventError::UnsupportedSchemaVersion {
                found: self.schema_version.clone(),
            });
        }
        if max_payload_bytes == 0 {
            return Err(SwarmCausalEventError::InvalidPayloadBudget {
                max: max_payload_bytes,
            });
        }
        if self.privacy.max_payload_bytes == 0 {
            return Err(SwarmCausalEventError::InvalidPayloadBudget {
                max: self.privacy.max_payload_bytes,
            });
        }
        let effective_max_payload_bytes = max_payload_bytes.min(self.privacy.max_payload_bytes);
        if self.event_id.trim().is_empty() {
            return Err(SwarmCausalEventError::EmptyEventId);
        }
        validate_graph_identifier("event_id", &self.event_id)?;
        if self.ingested_at_ms < self.occurred_at_ms {
            return Err(SwarmCausalEventError::IngestedBeforeOccurred);
        }
        if self
            .links
            .parent_event_ids
            .iter()
            .any(|id| id == &self.event_id)
        {
            return Err(SwarmCausalEventError::SelfParentLink {
                event_id: self.event_id.clone(),
            });
        }
        for parent in &self.links.parent_event_ids {
            validate_graph_identifier("links.parent_event_ids", parent)?;
        }
        if self
            .links
            .caused_by_event_ids
            .iter()
            .any(|id| id == &self.event_id)
        {
            return Err(SwarmCausalEventError::SelfCauseLink {
                event_id: self.event_id.clone(),
            });
        }
        for caused_by in &self.links.caused_by_event_ids {
            validate_graph_identifier("links.caused_by_event_ids", caused_by)?;
        }
        if let Some(root_event_id) = &self.links.root_event_id {
            validate_graph_identifier("links.root_event_id", root_event_id)?;
        }

        let actual_payload_bytes = serialized_payload_len(&self.payload);
        if actual_payload_bytes > effective_max_payload_bytes {
            return Err(SwarmCausalEventError::PayloadTooLarge {
                actual: actual_payload_bytes,
                max: effective_max_payload_bytes,
            });
        }
        let actual_hash = payload_hash(&self.payload);
        if self.privacy.payload_hash_sha256 != actual_hash {
            return Err(SwarmCausalEventError::PayloadHashMismatch {
                expected: self.privacy.payload_hash_sha256.clone(),
                actual: actual_hash,
            });
        }
        if self.privacy.payload_bytes != actual_payload_bytes {
            return Err(SwarmCausalEventError::PayloadByteCountMismatch {
                expected: self.privacy.payload_bytes,
                actual: actual_payload_bytes,
            });
        }
        if self.privacy.redaction_status.requires_omission_reason()
            && self.privacy.omission_reason.is_none()
        {
            return Err(SwarmCausalEventError::MissingOmissionReason {
                status: self.privacy.redaction_status,
            });
        }
        if matches!(
            self.privacy.redaction_status,
            CausalRedactionStatus::HashOnly
        ) && (self.privacy.omitted_payload_hash_sha256.is_none()
            || self.privacy.omitted_payload_bytes.is_none())
        {
            return Err(SwarmCausalEventError::MissingOmittedPayloadReference {
                status: self.privacy.redaction_status,
            });
        }
        if let Some(hash) = &self.privacy.omitted_payload_hash_sha256
            && !is_sha256_hex(hash)
        {
            return Err(SwarmCausalEventError::InvalidOmittedPayloadHash { hash: hash.clone() });
        }
        if secret_payload_requires_redaction(self)
            || payload_contains_unredacted_secret(&self.payload)
        {
            return Err(SwarmCausalEventError::SecretPayloadNotRedacted);
        }
        if self.source == SwarmCausalEventSource::SourceUnavailable
            && self
                .payload
                .get("reason")
                .and_then(Value::as_str)
                .is_none_or(|reason| reason.trim().is_empty())
        {
            return Err(SwarmCausalEventError::MissingUnavailableReason);
        }
        validate_required_correlation(self.source, &self.correlation)?;
        validate_correlation_strings(&self.correlation)?;
        for artifact in &self.artifacts {
            validate_artifact_ref(artifact)?;
        }
        Ok(())
    }

    /// Build a payload-free privacy audit record for operator/export surfaces.
    pub fn privacy_audit(&self) -> CausalPrivacyAudit {
        CausalPrivacyAudit {
            source: self.source,
            redaction_status: self.privacy.redaction_status,
            retention_class: self.privacy.retention_class,
            payload_bytes: self.privacy.payload_bytes,
            max_payload_bytes: self.privacy.max_payload_bytes,
            retention_seconds: self.privacy.retention_seconds,
            omission_reason: self.privacy.omission_reason,
            omitted_payload_hash_sha256: self.privacy.omitted_payload_hash_sha256.clone(),
            omitted_payload_bytes: self.privacy.omitted_payload_bytes,
        }
    }
}

pub fn default_incident_budget() -> CausalIncidentBudget {
    CausalIncidentBudget::default()
}

pub fn default_source_budget(
    source: SwarmCausalEventSource,
    retention_class: CausalRetentionClass,
) -> CausalSourceBudget {
    CausalSourceBudget {
        max_payload_bytes: default_max_payload_bytes_for_source(source),
        retention_seconds: default_retention_seconds(retention_class),
    }
}

pub const fn default_max_payload_bytes_for_source(source: SwarmCausalEventSource) -> usize {
    match source {
        SwarmCausalEventSource::Pane => DEFAULT_PANE_PAYLOAD_BYTES,
        SwarmCausalEventSource::Beads
        | SwarmCausalEventSource::Rch
        | SwarmCausalEventSource::AgentMail => DEFAULT_LOG_PAYLOAD_BYTES,
        SwarmCausalEventSource::Git => DEFAULT_PANE_PAYLOAD_BYTES,
        SwarmCausalEventSource::Robot
        | SwarmCausalEventSource::Mcp
        | SwarmCausalEventSource::Workflow
        | SwarmCausalEventSource::Policy
        | SwarmCausalEventSource::Operator
        | SwarmCausalEventSource::Runtime
        | SwarmCausalEventSource::SourceUnavailable => DEFAULT_MAX_PAYLOAD_BYTES,
    }
}

pub const fn default_retention_seconds(retention_class: CausalRetentionClass) -> Option<u64> {
    match retention_class {
        CausalRetentionClass::Ephemeral => Some(24 * 60 * 60),
        CausalRetentionClass::Standard => Some(30 * 24 * 60 * 60),
        CausalRetentionClass::Proof => Some(180 * 24 * 60 * 60),
        CausalRetentionClass::Audit => None,
    }
}

fn validate_required_correlation(
    source: SwarmCausalEventSource,
    correlation: &CausalCorrelationKeys,
) -> Result<(), SwarmCausalEventError> {
    let ok = match source {
        SwarmCausalEventSource::Pane => correlation.pane_id.is_some(),
        SwarmCausalEventSource::Beads => is_present(correlation.bead_id.as_ref()),
        SwarmCausalEventSource::Rch => is_present(correlation.rch_build_id.as_ref()),
        SwarmCausalEventSource::AgentMail => is_present(correlation.thread_id.as_ref()),
        SwarmCausalEventSource::Git => is_present(correlation.git_commit.as_ref()),
        SwarmCausalEventSource::Robot
        | SwarmCausalEventSource::Mcp
        | SwarmCausalEventSource::Workflow
        | SwarmCausalEventSource::Policy
        | SwarmCausalEventSource::Operator
        | SwarmCausalEventSource::Runtime
        | SwarmCausalEventSource::SourceUnavailable => true,
    };
    if ok {
        Ok(())
    } else {
        Err(SwarmCausalEventError::MissingCorrelationKey { source })
    }
}

fn is_present(value: Option<&String>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

fn validate_correlation_strings(
    correlation: &CausalCorrelationKeys,
) -> Result<(), SwarmCausalEventError> {
    for (field, value) in [
        (
            "correlation.workspace_id",
            correlation.workspace_id.as_deref(),
        ),
        ("correlation.session_id", correlation.session_id.as_deref()),
        (
            "correlation.workflow_id",
            correlation.workflow_id.as_deref(),
        ),
        ("correlation.bead_id", correlation.bead_id.as_deref()),
        ("correlation.thread_id", correlation.thread_id.as_deref()),
        (
            "correlation.rch_build_id",
            correlation.rch_build_id.as_deref(),
        ),
        (
            "correlation.rch_worker_id",
            correlation.rch_worker_id.as_deref(),
        ),
        ("correlation.git_commit", correlation.git_commit.as_deref()),
        ("correlation.git_branch", correlation.git_branch.as_deref()),
        ("correlation.command_id", correlation.command_id.as_deref()),
    ] {
        if let Some(value) = value {
            reject_secret_like(field, value)?;
        }
    }
    Ok(())
}

fn validate_graph_identifier(field: &str, value: &str) -> Result<(), SwarmCausalEventError> {
    reject_secret_like(field, value)?;
    if value.contains('/')
        || value.contains('\\')
        || value
            .split(['.', ':'])
            .any(|part| part == ".." || part.trim().is_empty())
    {
        return Err(SwarmCausalEventError::UnsafeArtifactUri {
            uri: value.to_string(),
        });
    }
    Ok(())
}

fn validate_artifact_ref(artifact: &CausalArtifactRef) -> Result<(), SwarmCausalEventError> {
    if artifact.uri.trim().is_empty() {
        return Err(SwarmCausalEventError::EmptyArtifactUri);
    }
    reject_secret_like("artifact.kind", &artifact.kind)?;
    reject_secret_like("artifact.uri", &artifact.uri)?;
    if artifact.uri.starts_with('/')
        || artifact.uri.contains('\\')
        || artifact.uri.split('/').any(|part| part == "..")
    {
        return Err(SwarmCausalEventError::UnsafeArtifactUri {
            uri: artifact.uri.clone(),
        });
    }
    Ok(())
}

fn reject_secret_like(field: &str, value: &str) -> Result<(), SwarmCausalEventError> {
    if looks_secret_like(value) {
        Err(SwarmCausalEventError::SecretLikeIdentifier {
            field: field.to_string(),
        })
    } else {
        Ok(())
    }
}

fn secret_payload_requires_redaction(event: &SwarmCausalEvent) -> bool {
    event.privacy.sensitivity == CausalPayloadSensitivity::SecretBearing
        && !matches!(
            event.privacy.redaction_status,
            CausalRedactionStatus::Redacted
                | CausalRedactionStatus::HashOnly
                | CausalRedactionStatus::Unavailable
        )
}

fn payload_contains_unredacted_secret(payload: &Value) -> bool {
    match payload {
        Value::String(value) => looks_secret_like(value) && !is_redacted_marker(value),
        Value::Array(values) => values.iter().any(payload_contains_unredacted_secret),
        Value::Object(values) => values.iter().any(|(key, value)| {
            let secret_key = key_looks_secret_like(key);
            if secret_key && !value_is_redacted(value) {
                true
            } else {
                payload_contains_unredacted_secret(value)
            }
        }),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn value_is_redacted(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(value) => is_redacted_marker(value),
        Value::Array(values) => values.iter().all(value_is_redacted),
        Value::Object(values) => values.values().all(value_is_redacted),
        Value::Bool(_) | Value::Number(_) => false,
    }
}

fn key_looks_secret_like(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("authorization")
        || lower.contains("password")
        || lower.contains("passwd")
        || lower.contains("secret")
        || lower.contains("token")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("private_key")
}

fn looks_secret_like(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("authorization: bearer ")
        || lower.contains("bearer sk-")
        || lower.contains("bearer ghp_")
        || lower.contains("password=")
        || lower.contains("passwd=")
        || lower.contains("secret=")
        || lower.contains("token=")
        || lower.contains("api_key=")
        || lower.contains("apikey=")
        || lower.contains("private_key=")
        || lower.contains("aws_secret_access_key=")
        || lower.contains("sk-live-")
        || lower.contains("ghp_")
}

fn is_redacted_marker(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.eq_ignore_ascii_case("[redacted]")
        || trimmed.eq_ignore_ascii_case("<redacted>")
        || trimmed.eq_ignore_ascii_case("***redacted***")
        || trimmed.eq_ignore_ascii_case("redacted")
}

fn is_sha256_hex(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn default_max_payload_bytes() -> usize {
    DEFAULT_MAX_PAYLOAD_BYTES
}

fn payload_hash(payload: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json_string(payload).as_bytes());
    let digest = hasher.finalize();
    hex_bytes(&digest[..])
}

fn serialized_payload_len(payload: &Value) -> usize {
    canonical_json_string(payload).len()
}

fn canonical_json_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value).unwrap_or_default(),
        Value::Array(values) => {
            let mut out = String::from("[");
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&canonical_json_string(value));
            }
            out.push(']');
            out
        }
        Value::Object(values) => {
            let mut out = String::from("{");
            let sorted: BTreeMap<&String, &Value> = values.iter().collect();
            for (index, (key, value)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_default());
                out.push(':');
                out.push_str(&canonical_json_string(value));
            }
            out.push('}');
            out
        }
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_event(source: SwarmCausalEventSource) -> SwarmCausalEvent {
        let mut correlation = CausalCorrelationKeys {
            workspace_id: Some("/repo".to_string()),
            ..Default::default()
        };
        match source {
            SwarmCausalEventSource::Pane => correlation.pane_id = Some(7),
            SwarmCausalEventSource::Beads => correlation.bead_id = Some("ft-abc12.1".to_string()),
            SwarmCausalEventSource::Rch => {
                correlation.rch_build_id = Some("29844447185338425".to_string());
            }
            SwarmCausalEventSource::AgentMail => {
                correlation.thread_id = Some("mail-thread-1".to_string());
            }
            SwarmCausalEventSource::Git => {
                correlation.git_commit = Some("abcdef123456".to_string());
            }
            _ => {}
        }
        SwarmCausalEvent::new(
            format!("event-{source:?}"),
            source,
            CausalEventClass::Informational,
            1000,
            1001,
            1,
            correlation,
            CausalLinks::default(),
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::NotRequired,
            CausalRetentionClass::Proof,
            vec![CausalArtifactRef {
                kind: "jsonl".to_string(),
                uri: "tests/e2e/logs/example.jsonl".to_string(),
                sha256: None,
                size_bytes: Some(123),
            }],
            json!({"kind": "example"}),
        )
        .unwrap()
    }

    #[test]
    fn source_wire_names_cover_operational_evidence() {
        for (source, wire_name) in [
            (SwarmCausalEventSource::Pane, "pane"),
            (SwarmCausalEventSource::Robot, "robot"),
            (SwarmCausalEventSource::Mcp, "mcp"),
            (SwarmCausalEventSource::Workflow, "workflow"),
            (SwarmCausalEventSource::Policy, "policy"),
            (SwarmCausalEventSource::Beads, "beads"),
            (SwarmCausalEventSource::Rch, "rch"),
            (SwarmCausalEventSource::AgentMail, "agent_mail"),
            (SwarmCausalEventSource::Git, "git"),
            (SwarmCausalEventSource::Operator, "operator"),
            (SwarmCausalEventSource::Runtime, "runtime"),
            (
                SwarmCausalEventSource::SourceUnavailable,
                "source_unavailable",
            ),
        ] {
            assert_eq!(
                serde_json::to_string(&source).unwrap(),
                format!("\"{wire_name}\"")
            );
        }
    }

    #[test]
    fn validates_all_source_required_correlation_keys() {
        for source in [
            SwarmCausalEventSource::Pane,
            SwarmCausalEventSource::Robot,
            SwarmCausalEventSource::Mcp,
            SwarmCausalEventSource::Workflow,
            SwarmCausalEventSource::Policy,
            SwarmCausalEventSource::Beads,
            SwarmCausalEventSource::Rch,
            SwarmCausalEventSource::AgentMail,
            SwarmCausalEventSource::Git,
            SwarmCausalEventSource::Operator,
            SwarmCausalEventSource::Runtime,
            SwarmCausalEventSource::SourceUnavailable,
        ] {
            let event = if source == SwarmCausalEventSource::SourceUnavailable {
                SwarmCausalEvent::new(
                    "event-source-unavailable",
                    source,
                    CausalEventClass::CommunicationOutage,
                    1000,
                    1001,
                    1,
                    CausalCorrelationKeys::default(),
                    CausalLinks::default(),
                    CausalPayloadSensitivity::Structural,
                    CausalRedactionStatus::Unavailable,
                    CausalRetentionClass::Proof,
                    Vec::new(),
                    json!({"reason": "agent_mail_unavailable"}),
                )
                .unwrap()
            } else {
                base_event(source)
            };
            event.validate().unwrap();
        }
    }

    #[test]
    fn rejects_missing_source_required_correlation_key() {
        let err = SwarmCausalEvent::new(
            "rch-without-build-id",
            SwarmCausalEventSource::Rch,
            CausalEventClass::InfrastructureFailure,
            1000,
            1001,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks::default(),
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::NotRequired,
            CausalRetentionClass::Proof,
            Vec::new(),
            json!({"reason": "worker_selection_refused"}),
        )
        .unwrap_err();
        assert_eq!(
            err,
            SwarmCausalEventError::MissingCorrelationKey {
                source: SwarmCausalEventSource::Rch
            }
        );
    }

    #[test]
    fn rejects_blank_ids_and_required_correlation_keys() {
        let err = SwarmCausalEvent::new(
            "  ",
            SwarmCausalEventSource::Robot,
            CausalEventClass::Informational,
            1000,
            1001,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks::default(),
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::NotRequired,
            CausalRetentionClass::Proof,
            Vec::new(),
            json!({"kind": "example"}),
        )
        .unwrap_err();
        assert_eq!(err, SwarmCausalEventError::EmptyEventId);

        let err = SwarmCausalEvent::new(
            "rch-with-blank-build-id",
            SwarmCausalEventSource::Rch,
            CausalEventClass::InfrastructureFailure,
            1000,
            1001,
            1,
            CausalCorrelationKeys {
                rch_build_id: Some("  ".to_string()),
                ..Default::default()
            },
            CausalLinks::default(),
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::NotRequired,
            CausalRetentionClass::Proof,
            Vec::new(),
            json!({"reason": "worker_selection_refused"}),
        )
        .unwrap_err();
        assert_eq!(
            err,
            SwarmCausalEventError::MissingCorrelationKey {
                source: SwarmCausalEventSource::Rch
            }
        );
    }

    #[test]
    fn unavailable_source_event_has_stable_golden_shape() {
        let event = SwarmCausalEvent::new(
            "mail-unavailable-1",
            SwarmCausalEventSource::SourceUnavailable,
            CausalEventClass::CommunicationOutage,
            10,
            11,
            7,
            CausalCorrelationKeys {
                workspace_id: Some("/repo".to_string()),
                ..Default::default()
            },
            CausalLinks::default(),
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::Unavailable,
            CausalRetentionClass::Proof,
            Vec::new(),
            json!({"reason": "agent_mail_unavailable"}),
        )
        .unwrap();
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["schema_version"], SWARM_CAUSAL_EVENT_SCHEMA_VERSION_V1);
        assert_eq!(json["source"], "source_unavailable");
        assert_eq!(json["event_class"], "communication_outage");
        assert_eq!(json["correlation"]["workspace_id"], "/repo");
        assert_eq!(json["payload"]["reason"], "agent_mail_unavailable");
        assert_eq!(json["privacy"]["redaction_status"], "unavailable");
        assert!(
            json["privacy"]["payload_hash_sha256"]
                .as_str()
                .is_some_and(|hash| hash.len() == 64)
        );
    }

    #[test]
    fn source_golden_shapes_cover_each_event_source() {
        for source in [
            SwarmCausalEventSource::Pane,
            SwarmCausalEventSource::Robot,
            SwarmCausalEventSource::Mcp,
            SwarmCausalEventSource::Workflow,
            SwarmCausalEventSource::Policy,
            SwarmCausalEventSource::Beads,
            SwarmCausalEventSource::Rch,
            SwarmCausalEventSource::AgentMail,
            SwarmCausalEventSource::Git,
            SwarmCausalEventSource::Operator,
            SwarmCausalEventSource::Runtime,
        ] {
            let json = serde_json::to_value(base_event(source)).unwrap();
            assert_eq!(json["schema_version"], SWARM_CAUSAL_EVENT_SCHEMA_VERSION_V1);
            assert_eq!(
                json["source"],
                serde_json::to_value(source).unwrap(),
                "source {source:?} changed its stable JSON shape"
            );
            assert_eq!(json["payload"], json!({"kind": "example"}));
            assert_eq!(json["privacy"]["payload_bytes"], 18);
            assert!(
                json["privacy"]["payload_hash_sha256"]
                    .as_str()
                    .is_some_and(|hash| hash.len() == 64)
            );
        }
    }

    #[test]
    fn payload_hash_uses_canonical_json_key_order() {
        let first: Value = serde_json::from_str(r#"{"b":2,"a":{"d":4,"c":3}}"#).unwrap();
        let second: Value = serde_json::from_str(r#"{"a":{"c":3,"d":4},"b":2}"#).unwrap();

        assert_eq!(
            canonical_json_string(&first),
            r#"{"a":{"c":3,"d":4},"b":2}"#
        );
        assert_eq!(payload_hash(&first), payload_hash(&second));
        assert_eq!(serialized_payload_len(&first), 25);
    }

    #[test]
    fn rejects_payloads_over_budget() {
        let event = SwarmCausalEvent::new(
            "too-large",
            SwarmCausalEventSource::Robot,
            CausalEventClass::Informational,
            1,
            2,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks::default(),
            CausalPayloadSensitivity::UserText,
            CausalRedactionStatus::Redacted,
            CausalRetentionClass::Standard,
            Vec::new(),
            json!({"text": "0123456789"}),
        )
        .unwrap();
        assert!(matches!(
            event.validate_with_max_payload(4),
            Err(SwarmCausalEventError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_secret_payload_without_redaction() {
        let err = SwarmCausalEvent::new(
            "secret",
            SwarmCausalEventSource::Mcp,
            CausalEventClass::Informational,
            1,
            2,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks::default(),
            CausalPayloadSensitivity::SecretBearing,
            CausalRedactionStatus::NotRequired,
            CausalRetentionClass::Audit,
            Vec::new(),
            json!({"token": "raw"}),
        )
        .unwrap_err();
        assert_eq!(err, SwarmCausalEventError::SecretPayloadNotRedacted);
    }

    #[test]
    fn redacted_secret_payload_gets_audit_reason_without_raw_value() {
        let event = SwarmCausalEvent::new(
            "redacted-secret",
            SwarmCausalEventSource::AgentMail,
            CausalEventClass::Informational,
            1,
            2,
            1,
            CausalCorrelationKeys {
                thread_id: Some("ft-ogr3n.5".to_string()),
                ..Default::default()
            },
            CausalLinks::default(),
            CausalPayloadSensitivity::SecretBearing,
            CausalRedactionStatus::Redacted,
            CausalRetentionClass::Audit,
            Vec::new(),
            json!({"token": "[REDACTED]", "body": "safe"}),
        )
        .unwrap();

        assert_eq!(
            event.privacy.omission_reason,
            Some(CausalPayloadOmissionReason::RedactedSecret)
        );
        assert_eq!(event.privacy.retention_seconds, None);

        let audit = serde_json::to_string(&event.privacy_audit()).unwrap();
        assert!(!audit.contains("token"));
        assert!(!audit.contains("[REDACTED]"));
        assert!(audit.contains("redacted_secret"));
    }

    #[test]
    fn rejects_secret_fixture_even_when_sensitivity_is_mislabeled() {
        let err = SwarmCausalEvent::new(
            "mislabeled-secret",
            SwarmCausalEventSource::Rch,
            CausalEventClass::SourceFailure,
            1,
            2,
            1,
            CausalCorrelationKeys {
                rch_build_id: Some("29844447185338425".to_string()),
                ..Default::default()
            },
            CausalLinks::default(),
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::NotRequired,
            CausalRetentionClass::Proof,
            Vec::new(),
            json!({"headers": {"authorization": "Bearer sk-live-raw-secret"}}),
        )
        .unwrap_err();

        assert_eq!(err, SwarmCausalEventError::SecretPayloadNotRedacted);
    }

    #[test]
    fn source_and_incident_budgets_are_machine_readable() {
        let pane_budget = default_source_budget(
            SwarmCausalEventSource::Pane,
            CausalRetentionClass::Ephemeral,
        );
        assert_eq!(pane_budget.max_payload_bytes, DEFAULT_PANE_PAYLOAD_BYTES);
        assert_eq!(pane_budget.retention_seconds, Some(24 * 60 * 60));

        let rch_budget =
            default_source_budget(SwarmCausalEventSource::Rch, CausalRetentionClass::Proof);
        assert_eq!(rch_budget.max_payload_bytes, DEFAULT_LOG_PAYLOAD_BYTES);
        assert_eq!(rch_budget.retention_seconds, Some(180 * 24 * 60 * 60));

        let incident_budget = default_incident_budget();
        assert_eq!(
            incident_budget.max_total_payload_bytes,
            DEFAULT_MAX_INCIDENT_PAYLOAD_BYTES
        );
        assert_eq!(incident_budget.max_events, DEFAULT_MAX_INCIDENT_EVENTS);

        let mut event = base_event(SwarmCausalEventSource::Robot);
        event.privacy.max_payload_bytes = 4;
        assert!(matches!(
            event.validate_with_max_payload(usize::MAX),
            Err(SwarmCausalEventError::PayloadTooLarge { max: 4, .. })
        ));

        let err = SwarmCausalEvent::new(
            "oversized-pane",
            SwarmCausalEventSource::Pane,
            CausalEventClass::Informational,
            1,
            2,
            1,
            CausalCorrelationKeys {
                pane_id: Some(7),
                ..Default::default()
            },
            CausalLinks::default(),
            CausalPayloadSensitivity::UserText,
            CausalRedactionStatus::Truncated,
            CausalRetentionClass::Ephemeral,
            Vec::new(),
            json!({"text": "x".repeat(DEFAULT_PANE_PAYLOAD_BYTES)}),
        )
        .unwrap_err();
        assert!(matches!(err, SwarmCausalEventError::PayloadTooLarge { .. }));
    }

    #[test]
    fn hash_only_payload_requires_omitted_payload_reference() {
        let err = SwarmCausalEvent::new(
            "hash-only-missing-ref",
            SwarmCausalEventSource::Beads,
            CausalEventClass::Informational,
            1,
            2,
            1,
            CausalCorrelationKeys {
                bead_id: Some("ft-ogr3n.5".to_string()),
                ..Default::default()
            },
            CausalLinks::default(),
            CausalPayloadSensitivity::SecretBearing,
            CausalRedactionStatus::HashOnly,
            CausalRetentionClass::Audit,
            Vec::new(),
            json!({"omitted": true}),
        )
        .unwrap_err();
        assert_eq!(
            err,
            SwarmCausalEventError::MissingOmittedPayloadReference {
                status: CausalRedactionStatus::HashOnly
            }
        );

        let event = SwarmCausalEvent::new_with_privacy_policy(
            "hash-only-with-ref",
            SwarmCausalEventSource::Beads,
            CausalEventClass::Informational,
            1,
            2,
            1,
            CausalCorrelationKeys {
                bead_id: Some("ft-ogr3n.5".to_string()),
                ..Default::default()
            },
            CausalLinks::default(),
            CausalPayloadSensitivity::SecretBearing,
            CausalRedactionStatus::HashOnly,
            CausalRetentionClass::Audit,
            CausalPrivacyPolicy::for_source(
                SwarmCausalEventSource::Beads,
                CausalRetentionClass::Audit,
                CausalRedactionStatus::HashOnly,
            )
            .with_omitted_payload_ref("a".repeat(64), 8192),
            Vec::new(),
            json!({"omitted": true}),
        )
        .unwrap();

        assert_eq!(event.privacy.omitted_payload_bytes, Some(8192));
        assert_eq!(
            event.privacy.omission_reason,
            Some(CausalPayloadOmissionReason::HashOnlyByPolicy)
        );
    }

    #[test]
    fn rejects_secret_like_identifiers_and_unsafe_artifact_uris() {
        let err = SwarmCausalEvent::new(
            "token=raw-secret",
            SwarmCausalEventSource::Robot,
            CausalEventClass::Informational,
            1,
            2,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks::default(),
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::NotRequired,
            CausalRetentionClass::Standard,
            Vec::new(),
            json!({}),
        )
        .unwrap_err();
        assert_eq!(
            err,
            SwarmCausalEventError::SecretLikeIdentifier {
                field: "event_id".to_string()
            }
        );

        let err = SwarmCausalEvent::new(
            "unsafe-artifact",
            SwarmCausalEventSource::Robot,
            CausalEventClass::Informational,
            1,
            2,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks::default(),
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::NotRequired,
            CausalRetentionClass::Standard,
            vec![CausalArtifactRef {
                kind: "jsonl".to_string(),
                uri: "logs/../secret.jsonl".to_string(),
                sha256: None,
                size_bytes: None,
            }],
            json!({}),
        )
        .unwrap_err();
        assert_eq!(
            err,
            SwarmCausalEventError::UnsafeArtifactUri {
                uri: "logs/../secret.jsonl".to_string()
            }
        );
    }

    #[test]
    fn rejects_self_parent_and_self_cause_links() {
        let event = SwarmCausalEvent::new(
            "self-parent",
            SwarmCausalEventSource::Robot,
            CausalEventClass::Informational,
            1,
            2,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks {
                parent_event_ids: vec!["self-parent".to_string()],
                ..Default::default()
            },
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::NotRequired,
            CausalRetentionClass::Standard,
            Vec::new(),
            json!({}),
        )
        .unwrap_err();
        assert!(matches!(
            event,
            SwarmCausalEventError::SelfParentLink { .. }
        ));

        let event = SwarmCausalEvent::new(
            "self-cause",
            SwarmCausalEventSource::Robot,
            CausalEventClass::Informational,
            1,
            2,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks {
                caused_by_event_ids: vec!["self-cause".to_string()],
                ..Default::default()
            },
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::NotRequired,
            CausalRetentionClass::Standard,
            Vec::new(),
            json!({}),
        )
        .unwrap_err();
        assert!(matches!(event, SwarmCausalEventError::SelfCauseLink { .. }));
    }

    #[test]
    fn rejects_unavailable_source_without_reason() {
        let err = SwarmCausalEvent::new(
            "missing-reason",
            SwarmCausalEventSource::SourceUnavailable,
            CausalEventClass::CommunicationOutage,
            1,
            2,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks::default(),
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::Unavailable,
            CausalRetentionClass::Proof,
            Vec::new(),
            json!({}),
        )
        .unwrap_err();
        assert_eq!(err, SwarmCausalEventError::MissingUnavailableReason);

        let err = SwarmCausalEvent::new(
            "blank-reason",
            SwarmCausalEventSource::SourceUnavailable,
            CausalEventClass::CommunicationOutage,
            1,
            2,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks::default(),
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::Unavailable,
            CausalRetentionClass::Proof,
            Vec::new(),
            json!({"reason": "  "}),
        )
        .unwrap_err();
        assert_eq!(err, SwarmCausalEventError::MissingUnavailableReason);
    }

    #[test]
    fn rejects_blank_artifact_uri() {
        let err = SwarmCausalEvent::new(
            "blank-artifact-uri",
            SwarmCausalEventSource::Robot,
            CausalEventClass::Informational,
            1,
            2,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks::default(),
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::NotRequired,
            CausalRetentionClass::Proof,
            vec![CausalArtifactRef {
                kind: "json".to_string(),
                uri: "  ".to_string(),
                sha256: None,
                size_bytes: None,
            }],
            json!({"kind": "example"}),
        )
        .unwrap_err();
        assert_eq!(err, SwarmCausalEventError::EmptyArtifactUri);
    }
}
