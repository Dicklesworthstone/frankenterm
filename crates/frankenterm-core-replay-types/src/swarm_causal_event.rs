//! Operational causal-event contract for swarm flight recorder evidence.
//!
//! The legacy [`crate::recorder_metadata::RECORDER_EVENT_SCHEMA_VERSION_V1`]
//! event contract captures mux ingress/egress. This module defines the
//! companion envelope for operational evidence around that stream: Beads,
//! RCH, Agent Mail, git, MCP, robot, policy, and operator events.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Schema version for the operational causal-event envelope.
pub const SWARM_CAUSAL_EVENT_SCHEMA_VERSION_V1: &str = "ft.swarm.causal_event.v1";

/// Default payload byte ceiling for persisted operational event bodies.
pub const DEFAULT_MAX_PAYLOAD_BYTES: usize = 64 * 1024;

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
}

/// Versioned event envelope consumed by the flight-recorder incident DAG.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
            },
            artifacts,
            payload,
        };
        event.validate_with_max_payload(DEFAULT_MAX_PAYLOAD_BYTES)?;
        Ok(event)
    }

    /// Validate using the default payload byte ceiling.
    pub fn validate(&self) -> Result<(), SwarmCausalEventError> {
        self.validate_with_max_payload(DEFAULT_MAX_PAYLOAD_BYTES)
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
        if self.event_id.is_empty() {
            return Err(SwarmCausalEventError::EmptyEventId);
        }
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

        let actual_payload_bytes = serialized_payload_len(&self.payload);
        if actual_payload_bytes > max_payload_bytes {
            return Err(SwarmCausalEventError::PayloadTooLarge {
                actual: actual_payload_bytes,
                max: max_payload_bytes,
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
        if self.privacy.sensitivity == CausalPayloadSensitivity::SecretBearing
            && !matches!(
                self.privacy.redaction_status,
                CausalRedactionStatus::Redacted
                    | CausalRedactionStatus::HashOnly
                    | CausalRedactionStatus::Unavailable
            )
        {
            return Err(SwarmCausalEventError::SecretPayloadNotRedacted);
        }
        if self.source == SwarmCausalEventSource::SourceUnavailable
            && self
                .payload
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
        {
            return Err(SwarmCausalEventError::MissingUnavailableReason);
        }
        validate_required_correlation(self.source, &self.correlation)?;
        if self
            .artifacts
            .iter()
            .any(|artifact| artifact.uri.is_empty())
        {
            return Err(SwarmCausalEventError::EmptyArtifactUri);
        }
        Ok(())
    }
}

fn validate_required_correlation(
    source: SwarmCausalEventSource,
    correlation: &CausalCorrelationKeys,
) -> Result<(), SwarmCausalEventError> {
    let ok = match source {
        SwarmCausalEventSource::Pane => correlation.pane_id.is_some(),
        SwarmCausalEventSource::Beads => correlation.bead_id.is_some(),
        SwarmCausalEventSource::Rch => correlation.rch_build_id.is_some(),
        SwarmCausalEventSource::AgentMail => correlation.thread_id.is_some(),
        SwarmCausalEventSource::Git => correlation.git_commit.is_some(),
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

fn payload_hash(payload: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(payload).unwrap_or_default());
    let digest = hasher.finalize();
    hex_bytes(&digest[..])
}

fn serialized_payload_len(payload: &Value) -> usize {
    serde_json::to_vec(payload).map_or(usize::MAX, |bytes| bytes.len())
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
        assert_eq!(
            serde_json::to_string(&SwarmCausalEventSource::AgentMail).unwrap(),
            "\"agent_mail\""
        );
        assert_eq!(
            serde_json::to_string(&SwarmCausalEventSource::Rch).unwrap(),
            "\"rch\""
        );
        assert_eq!(
            serde_json::to_string(&SwarmCausalEventSource::Beads).unwrap(),
            "\"beads\""
        );
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
    }
}
