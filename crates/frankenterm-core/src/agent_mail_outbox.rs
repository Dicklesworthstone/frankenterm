//! Durable Agent Mail outage-spool entry contract.
//!
//! The types here describe intended coordination writes captured while Agent
//! Mail is unavailable after the single allowed retry. They deliberately do not
//! claim delivery; replay state is recorded separately and remains auditable.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const SCHEMA_VERSION: u8 = 1;
pub const CONTRACT_ID: &str = "ft.agent_mail_outbox_entry.v1";
pub const SOURCE_BEAD: &str = "ft-dezx8.1";
pub const MAX_SUBJECT_BYTES: usize = 200;
pub const MAX_BODY_PREVIEW_BYTES: usize = 512;
pub const MAX_BODY_RETAINED_BYTES: u64 = 16 * 1024;
pub const MAX_ATTACHMENT_REFS: usize = 16;
pub const MAX_ATTACHMENT_REF_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_RECIPIENTS: usize = 64;
pub const MIN_RESERVATION_TTL_SECONDS: u64 = 60;
pub const MAX_RESERVATION_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;
pub const AGENT_MAIL_RETRY_LIMIT: u32 = 1;
pub const DEFAULT_RETENTION_DAYS: i64 = 30;
pub const OUTBOX_WRITER_DELIVERY_UNCLAIMED_NOTE: &str = "Queued in the Agent Mail fallback outbox after the allowed retry; Agent Mail delivery is not claimed until a replay receipt records delivery.";
pub const OUTBOX_WRITER_FALLBACK_GUIDANCE: [&str; 3] = [
    "Retry the Agent Mail operation once before queueing fallback intent.",
    "Persist the queued outbox entry with body digest or redacted preview only.",
    OUTBOX_WRITER_DELIVERY_UNCLAIMED_NOTE,
];
pub const FORBIDDEN_AGENT_MAIL_RECOVERY_COMMANDS: [&str; 7] = [
    "am service restart",
    "am service stop",
    "am doctor fix",
    "am doctor repair",
    "am doctor reconstruct",
    "kill am",
    "kill mcp-agent-mail",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMailOutboxEntry {
    pub schema_version: u8,
    pub contract_id: String,
    pub source_bead: String,
    pub replay_id: String,
    pub created_at: String,
    pub agent: OutboxAgentIdentity,
    pub thread_id: Option<String>,
    pub recipients: OutboxRecipients,
    pub subject: String,
    pub body_policy: BodyPolicy,
    pub importance: OutboxImportance,
    pub ack_required: bool,
    pub attachments: Vec<AttachmentRef>,
    pub source_operation: SourceOperation,
    pub failure_reason: FailureReason,
    pub state: OutboxState,
    pub state_reason: Option<String>,
    pub operator_decision: Option<OperatorDecision>,
    pub replay_receipt: Option<ReplayReceipt>,
    pub retention: RetentionPolicy,
    pub reservation_intent: Option<ReservationIntent>,
    pub beads_fallback: Option<BeadsFallbackSummary>,
}

impl AgentMailOutboxEntry {
    pub fn validate(&self) -> Result<(), AgentMailOutboxValidationError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(AgentMailOutboxValidationError::WrongSchemaVersion {
                found: self.schema_version,
            });
        }
        if self.contract_id != CONTRACT_ID {
            return Err(AgentMailOutboxValidationError::WrongContractId {
                found: self.contract_id.clone(),
            });
        }
        if self.source_bead != SOURCE_BEAD {
            return Err(AgentMailOutboxValidationError::WrongSourceBead {
                found: self.source_bead.clone(),
            });
        }

        validate_replay_id(&self.replay_id)?;
        validate_rfc3339_utc("created_at", &self.created_at)?;
        self.agent.validate()?;
        if let Some(thread_id) = &self.thread_id {
            validate_non_empty("thread_id", thread_id)?;
        }
        self.recipients.validate()?;
        validate_subject(&self.subject)?;
        self.body_policy.validate()?;
        if self.attachments.len() > MAX_ATTACHMENT_REFS {
            return Err(AgentMailOutboxValidationError::TooManyAttachments {
                count: self.attachments.len(),
            });
        }
        for attachment in &self.attachments {
            attachment.validate()?;
        }
        self.failure_reason.validate()?;
        self.validate_state()?;
        self.retention.validate()?;
        if let Some(intent) = &self.reservation_intent {
            intent.validate()?;
        }
        if let Some(fallback) = &self.beads_fallback {
            fallback.validate()?;
        }

        Ok(())
    }

    fn validate_state(&self) -> Result<(), AgentMailOutboxValidationError> {
        match self.state {
            OutboxState::Queued => {
                if self.operator_decision.is_some() || self.replay_receipt.is_some() {
                    return Err(AgentMailOutboxValidationError::InvalidState {
                        state: self.state,
                        reason: "queued entries must not carry replay or discard receipts",
                    });
                }
            }
            OutboxState::ReplayDryRunOk => {
                let receipt = self.replay_receipt.as_ref().ok_or(
                    AgentMailOutboxValidationError::MissingReplayReceipt { state: self.state },
                )?;
                receipt.require_dry_run_at(self.state)?;
            }
            OutboxState::Replayed => {
                let receipt = self.replay_receipt.as_ref().ok_or(
                    AgentMailOutboxValidationError::MissingReplayReceipt { state: self.state },
                )?;
                receipt.require_delivery(self.state)?;
            }
            OutboxState::ReplayFailed => {
                let receipt = self.replay_receipt.as_ref().ok_or(
                    AgentMailOutboxValidationError::MissingReplayReceipt { state: self.state },
                )?;
                receipt.require_failure(self.state)?;
            }
            OutboxState::Superseded => {
                require_optional_reason(self.state, self.state_reason.as_deref())?;
            }
            OutboxState::DiscardedByOperator => {
                let decision = self.operator_decision.as_ref().ok_or(
                    AgentMailOutboxValidationError::MissingOperatorDecision { state: self.state },
                )?;
                decision.validate()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxAgentIdentity {
    pub name: String,
    pub program: String,
    pub model: String,
    pub project_key: String,
}

impl OutboxAgentIdentity {
    fn validate(&self) -> Result<(), AgentMailOutboxValidationError> {
        validate_non_empty("agent.name", &self.name)?;
        validate_non_empty("agent.program", &self.program)?;
        validate_non_empty("agent.model", &self.model)?;
        validate_non_empty("agent.project_key", &self.project_key)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxRecipients {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
}

impl OutboxRecipients {
    fn validate(&self) -> Result<(), AgentMailOutboxValidationError> {
        let count = self.to.len() + self.cc.len() + self.bcc.len();
        if count == 0 {
            return Err(AgentMailOutboxValidationError::MissingField("recipients"));
        }
        if count > MAX_RECIPIENTS {
            return Err(AgentMailOutboxValidationError::TooManyRecipients { count });
        }
        for recipient in self.to.iter().chain(&self.cc).chain(&self.bcc) {
            validate_recipient(recipient)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyPolicy {
    pub body_sha256: String,
    pub body_storage: BodyStoragePolicy,
    pub body_preview_redacted: Option<String>,
    pub body_byte_count: u64,
    pub max_retained_bytes: u64,
    pub raw_body_retained: bool,
}

impl BodyPolicy {
    fn validate(&self) -> Result<(), AgentMailOutboxValidationError> {
        validate_sha256("body_policy.body_sha256", &self.body_sha256)?;
        if self.raw_body_retained {
            return Err(AgentMailOutboxValidationError::RawBodyRetained);
        }
        if self.max_retained_bytes == 0 || self.max_retained_bytes > MAX_BODY_RETAINED_BYTES {
            return Err(AgentMailOutboxValidationError::UnboundedBody {
                byte_count: self.body_byte_count,
                max_retained_bytes: self.max_retained_bytes,
            });
        }
        if self.body_byte_count == 0 || self.body_byte_count > self.max_retained_bytes {
            return Err(AgentMailOutboxValidationError::UnboundedBody {
                byte_count: self.body_byte_count,
                max_retained_bytes: self.max_retained_bytes,
            });
        }
        if let Some(preview) = &self.body_preview_redacted {
            validate_preview(preview)?;
        }
        if self.body_storage == BodyStoragePolicy::BoundedMarkdown
            && self.body_preview_redacted.is_none()
        {
            return Err(AgentMailOutboxValidationError::MissingField(
                "body_policy.body_preview_redacted",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyStoragePolicy {
    DigestOnly,
    BoundedMarkdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub path: String,
    pub sha256: Option<String>,
    pub byte_count: u64,
    pub content_type: Option<String>,
    pub retention: AttachmentRetention,
}

impl AttachmentRef {
    fn validate(&self) -> Result<(), AgentMailOutboxValidationError> {
        validate_repo_relative_path("attachments.path", &self.path)?;
        if let Some(sha256) = &self.sha256 {
            validate_sha256("attachments.sha256", sha256)?;
        }
        if self.byte_count > MAX_ATTACHMENT_REF_BYTES {
            return Err(AgentMailOutboxValidationError::AttachmentTooLarge {
                path: self.path.clone(),
                byte_count: self.byte_count,
            });
        }
        if let Some(content_type) = &self.content_type {
            validate_non_empty("attachments.content_type", content_type)?;
            if content_type.contains('\n') || content_type.contains('\r') {
                return Err(AgentMailOutboxValidationError::InvalidText {
                    field: "attachments.content_type",
                    value: content_type.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentRetention {
    ReferenceOnly,
    DigestOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureReason {
    pub class: FailureClass,
    pub summary: String,
    pub retry_count: u32,
    pub retry_limit: u32,
    pub last_attempt_at: String,
}

impl FailureReason {
    fn validate(&self) -> Result<(), AgentMailOutboxValidationError> {
        validate_non_empty("failure_reason.summary", &self.summary)?;
        validate_preview(&self.summary)?;
        if self.retry_count > self.retry_limit {
            return Err(AgentMailOutboxValidationError::InvalidRetryBudget {
                retry_count: self.retry_count,
                retry_limit: self.retry_limit,
            });
        }
        validate_rfc3339_utc("failure_reason.last_attempt_at", &self.last_attempt_at)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    AgentMailUnavailable,
    DatabaseRecoveryNotice,
    ApiUnreachable,
    ApiError,
    RegistrationFailed,
    ContactPermissionBlocked,
    AckUnavailable,
    Timeout,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceOperation {
    SendMessage,
    ReplyMessage,
    FileReservation,
    ReleaseReservation,
    BeadsStatusComment,
    BeadsCloseoutComment,
    StaleOwnerHandoffNotice,
    CoordinationNotice,
}

impl SourceOperation {
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::SendMessage => "send_message",
            Self::ReplyMessage => "reply_message",
            Self::FileReservation => "file_reservation",
            Self::ReleaseReservation => "release_reservation",
            Self::BeadsStatusComment => "beads_status_comment",
            Self::BeadsCloseoutComment => "beads_closeout_comment",
            Self::StaleOwnerHandoffNotice => "stale_owner_handoff_notice",
            Self::CoordinationNotice => "coordination_notice",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxImportance {
    Low,
    Normal,
    High,
    Urgent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxState {
    Queued,
    ReplayDryRunOk,
    Replayed,
    ReplayFailed,
    Superseded,
    DiscardedByOperator,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayReceipt {
    pub dry_run_at: Option<String>,
    pub replayed_at: Option<String>,
    pub delivered_message_id: Option<String>,
    pub failure_summary: Option<String>,
}

impl ReplayReceipt {
    fn require_dry_run_at(&self, state: OutboxState) -> Result<(), AgentMailOutboxValidationError> {
        let dry_run_at =
            self.dry_run_at
                .as_deref()
                .ok_or(AgentMailOutboxValidationError::InvalidState {
                    state,
                    reason: "replay_dry_run_ok requires dry_run_at",
                })?;
        validate_rfc3339_utc("replay_receipt.dry_run_at", dry_run_at)
    }

    fn require_delivery(&self, state: OutboxState) -> Result<(), AgentMailOutboxValidationError> {
        let replayed_at =
            self.replayed_at
                .as_deref()
                .ok_or(AgentMailOutboxValidationError::InvalidState {
                    state,
                    reason: "replayed requires replayed_at",
                })?;
        validate_rfc3339_utc("replay_receipt.replayed_at", replayed_at)?;
        let delivered_id = self.delivered_message_id.as_deref().ok_or(
            AgentMailOutboxValidationError::InvalidState {
                state,
                reason: "replayed requires delivered_message_id",
            },
        )?;
        validate_non_empty("replay_receipt.delivered_message_id", delivered_id)
    }

    fn require_failure(&self, state: OutboxState) -> Result<(), AgentMailOutboxValidationError> {
        let replayed_at =
            self.replayed_at
                .as_deref()
                .ok_or(AgentMailOutboxValidationError::InvalidState {
                    state,
                    reason: "replay_failed requires replayed_at",
                })?;
        validate_rfc3339_utc("replay_receipt.replayed_at", replayed_at)?;
        let failure = self.failure_summary.as_deref().ok_or(
            AgentMailOutboxValidationError::InvalidState {
                state,
                reason: "replay_failed requires failure_summary",
            },
        )?;
        validate_preview(failure)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorDecision {
    pub operator: String,
    pub decided_at: String,
    pub reason: String,
}

impl OperatorDecision {
    fn validate(&self) -> Result<(), AgentMailOutboxValidationError> {
        validate_non_empty("operator_decision.operator", &self.operator)?;
        validate_rfc3339_utc("operator_decision.decided_at", &self.decided_at)?;
        validate_preview(&self.reason)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    pub retain_until: String,
    pub automatic_discard: bool,
    pub redaction: Vec<RetentionRedaction>,
}

impl RetentionPolicy {
    fn validate(&self) -> Result<(), AgentMailOutboxValidationError> {
        validate_rfc3339_utc("retention.retain_until", &self.retain_until)?;
        if self.automatic_discard {
            return Err(AgentMailOutboxValidationError::AutomaticDiscard);
        }
        if self.redaction.is_empty() {
            return Err(AgentMailOutboxValidationError::MissingField(
                "retention.redaction",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionRedaction {
    BodyDigestOnly,
    BodyPreviewRedacted,
    AttachmentRefsOnly,
    BccAllowedForReplay,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationIntent {
    pub paths: Vec<String>,
    pub exclusive: bool,
    pub ttl_seconds: u64,
    pub reason_sha256: String,
    pub reason_preview_redacted: Option<String>,
}

impl ReservationIntent {
    fn validate(&self) -> Result<(), AgentMailOutboxValidationError> {
        if self.paths.is_empty() {
            return Err(AgentMailOutboxValidationError::MissingField(
                "reservation_intent.paths",
            ));
        }
        for path in &self.paths {
            validate_repo_relative_path("reservation_intent.paths", path)?;
        }
        if !(MIN_RESERVATION_TTL_SECONDS..=MAX_RESERVATION_TTL_SECONDS).contains(&self.ttl_seconds)
        {
            return Err(AgentMailOutboxValidationError::InvalidReservationTtl {
                ttl_seconds: self.ttl_seconds,
            });
        }
        validate_sha256("reservation_intent.reason_sha256", &self.reason_sha256)?;
        if let Some(preview) = &self.reason_preview_redacted {
            validate_preview(preview)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadsFallbackSummary {
    pub bead_id: String,
    pub action: BeadsFallbackAction,
    pub comment_sha256: String,
    pub comment_preview_redacted: String,
}

impl BeadsFallbackSummary {
    fn validate(&self) -> Result<(), AgentMailOutboxValidationError> {
        if !self.bead_id.starts_with("ft-") {
            return Err(AgentMailOutboxValidationError::InvalidText {
                field: "beads_fallback.bead_id",
                value: self.bead_id.clone(),
            });
        }
        validate_sha256("beads_fallback.comment_sha256", &self.comment_sha256)?;
        validate_preview(&self.comment_preview_redacted)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadsFallbackAction {
    CommentOnly,
    StatusTransition,
    CloseoutBlocked,
    CloseoutComplete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayIdSeed<'a> {
    pub project_key: &'a str,
    pub agent_name: &'a str,
    pub thread_id: Option<&'a str>,
    pub recipients_digest: &'a str,
    pub subject_digest: &'a str,
    pub body_digest: &'a str,
    pub source_operation: SourceOperation,
    pub created_at_bucket: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMailOutboxWriteRequest {
    pub agent: OutboxAgentIdentity,
    pub thread_id: Option<String>,
    pub recipients: OutboxRecipients,
    pub subject: String,
    pub body_markdown: String,
    pub body_preview_redacted: Option<String>,
    pub importance: OutboxImportance,
    pub ack_required: bool,
    pub attachments: Vec<AttachmentRef>,
    pub source_operation: SourceOperation,
    pub reservation_intent: Option<ReservationIntent>,
    pub beads_fallback: Option<BeadsFallbackSummary>,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMailOutboxFailure {
    pub class: FailureClass,
    pub summary: String,
    pub retry_count: u32,
    pub last_attempt_at: String,
}

impl AgentMailOutboxFailure {
    #[must_use]
    pub fn classified(
        summary: impl Into<String>,
        retry_count: u32,
        last_attempt_at: impl Into<String>,
    ) -> Self {
        let summary = summary.into();
        Self {
            class: classify_agent_mail_failure(&summary),
            summary,
            retry_count,
            last_attempt_at: last_attempt_at.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentMailRetryPolicy {
    retry_limit: u32,
}

impl AgentMailRetryPolicy {
    #[must_use]
    pub const fn one_retry() -> Self {
        Self {
            retry_limit: AGENT_MAIL_RETRY_LIMIT,
        }
    }

    #[must_use]
    pub const fn retry_limit(self) -> u32 {
        self.retry_limit
    }

    #[must_use]
    pub const fn retry_remaining(self, retry_count: u32) -> bool {
        retry_count < self.retry_limit
    }
}

impl Default for AgentMailRetryPolicy {
    fn default() -> Self {
        Self::one_retry()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AgentMailOutboxWriterError {
    #[error(
        "Agent Mail retry budget still has {remaining} retry attempt(s); queueing would skip the required retry"
    )]
    RetryBudgetRemaining {
        retry_count: u32,
        retry_limit: u32,
        remaining: u32,
    },
    #[error("invalid UTC RFC3339 timestamp in {field}: {value}")]
    InvalidTimestamp { field: &'static str, value: String },
    #[error("reservation_intent is required for {operation:?}")]
    MissingReservationIntent { operation: SourceOperation },
    #[error(transparent)]
    Validation(#[from] AgentMailOutboxValidationError),
}

pub fn build_queued_outbox_entry(
    request: AgentMailOutboxWriteRequest,
    failure: AgentMailOutboxFailure,
) -> Result<AgentMailOutboxEntry, AgentMailOutboxWriterError> {
    build_queued_outbox_entry_with_policy(request, failure, AgentMailRetryPolicy::default())
}

pub fn build_queued_outbox_entry_with_policy(
    request: AgentMailOutboxWriteRequest,
    failure: AgentMailOutboxFailure,
    retry_policy: AgentMailRetryPolicy,
) -> Result<AgentMailOutboxEntry, AgentMailOutboxWriterError> {
    if retry_policy.retry_remaining(failure.retry_count) {
        return Err(AgentMailOutboxWriterError::RetryBudgetRemaining {
            retry_count: failure.retry_count,
            retry_limit: retry_policy.retry_limit(),
            remaining: retry_policy.retry_limit() - failure.retry_count,
        });
    }
    if matches!(
        request.source_operation,
        SourceOperation::FileReservation | SourceOperation::ReleaseReservation
    ) && request.reservation_intent.is_none()
    {
        return Err(AgentMailOutboxWriterError::MissingReservationIntent {
            operation: request.source_operation,
        });
    }

    let created_at = parse_utc_timestamp("created_at", &request.created_at)?;
    parse_utc_timestamp("failure_reason.last_attempt_at", &failure.last_attempt_at)?;
    let body_digest = sha256_hex(request.body_markdown.as_bytes());
    let recipients_digest = recipients_digest(&request.recipients);
    let subject_digest = sha256_hex(request.subject.as_bytes());
    let replay_id = deterministic_replay_id(&ReplayIdSeed {
        project_key: &request.agent.project_key,
        agent_name: &request.agent.name,
        thread_id: request.thread_id.as_deref(),
        recipients_digest: &recipients_digest,
        subject_digest: &subject_digest,
        body_digest: &body_digest,
        source_operation: request.source_operation,
        created_at_bucket: &created_at_bucket(created_at),
    });
    let redaction = retention_redactions(&request);

    let entry = AgentMailOutboxEntry {
        schema_version: SCHEMA_VERSION,
        contract_id: CONTRACT_ID.to_owned(),
        source_bead: SOURCE_BEAD.to_owned(),
        replay_id,
        created_at: request.created_at,
        agent: request.agent,
        thread_id: request.thread_id,
        recipients: request.recipients,
        subject: request.subject,
        body_policy: BodyPolicy {
            body_sha256: body_digest,
            body_storage: BodyStoragePolicy::DigestOnly,
            body_preview_redacted: request.body_preview_redacted,
            body_byte_count: request.body_markdown.len() as u64,
            max_retained_bytes: MAX_BODY_RETAINED_BYTES,
            raw_body_retained: false,
        },
        importance: request.importance,
        ack_required: request.ack_required,
        attachments: request.attachments,
        source_operation: request.source_operation,
        failure_reason: FailureReason {
            class: failure.class,
            summary: failure.summary,
            retry_count: failure.retry_count,
            retry_limit: retry_policy.retry_limit(),
            last_attempt_at: failure.last_attempt_at,
        },
        state: OutboxState::Queued,
        state_reason: None,
        operator_decision: None,
        replay_receipt: None,
        retention: RetentionPolicy {
            retain_until: retain_until(created_at),
            automatic_discard: false,
            redaction,
        },
        reservation_intent: request.reservation_intent,
        beads_fallback: request.beads_fallback,
    };
    entry.validate()?;
    Ok(entry)
}

#[must_use]
pub fn deterministic_replay_id(seed: &ReplayIdSeed<'_>) -> String {
    let mut hasher = Sha256::new();
    for part in [
        CONTRACT_ID,
        seed.project_key,
        seed.agent_name,
        seed.thread_id.unwrap_or(""),
        seed.recipients_digest,
        seed.subject_digest,
        seed.body_digest,
        seed.source_operation.wire_name(),
        seed.created_at_bucket,
    ] {
        hasher.update(part.len().to_string().as_bytes());
        hasher.update(b":");
        hasher.update(part.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hex::encode(hasher.finalize());
    format!("amq1-{}", &digest[..24])
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[must_use]
pub fn classify_agent_mail_failure(summary: &str) -> FailureClass {
    let lower = summary.to_ascii_lowercase();
    if lower.contains("contact") && (lower.contains("blocked") || lower.contains("permission")) {
        FailureClass::ContactPermissionBlocked
    } else if lower.contains("ack")
        && (lower.contains("unavailable") || lower.contains("missing") || lower.contains("failed"))
    {
        FailureClass::AckUnavailable
    } else if lower.contains("database")
        && (lower.contains("recovery") || lower.contains("recover"))
    {
        FailureClass::DatabaseRecoveryNotice
    } else if lower.contains("registration") && lower.contains("fail") {
        FailureClass::RegistrationFailed
    } else if lower.contains("timeout") || lower.contains("timed out") {
        FailureClass::Timeout
    } else if lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("unreachable")
    {
        FailureClass::ApiUnreachable
    } else if lower.contains("agent mail") && lower.contains("unavailable") {
        FailureClass::AgentMailUnavailable
    } else if lower.contains("api")
        && (lower.contains("error") || lower.contains("failed") || lower.contains("unavailable"))
    {
        FailureClass::ApiError
    } else {
        FailureClass::Unknown
    }
}

fn parse_utc_timestamp(
    field: &'static str,
    value: &str,
) -> Result<DateTime<Utc>, AgentMailOutboxWriterError> {
    if !value.ends_with('Z') {
        return Err(AgentMailOutboxWriterError::InvalidTimestamp {
            field,
            value: value.to_owned(),
        });
    }
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|_| AgentMailOutboxWriterError::InvalidTimestamp {
            field,
            value: value.to_owned(),
        })
}

fn created_at_bucket(created_at: DateTime<Utc>) -> String {
    created_at.format("%Y-%m-%dT%H:%MZ").to_string()
}

fn retain_until(created_at: DateTime<Utc>) -> String {
    (created_at + Duration::days(DEFAULT_RETENTION_DAYS)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn recipients_digest(recipients: &OutboxRecipients) -> String {
    let mut parts = Vec::new();
    push_recipient_parts(&mut parts, "to", &recipients.to);
    push_recipient_parts(&mut parts, "cc", &recipients.cc);
    push_recipient_parts(&mut parts, "bcc", &recipients.bcc);
    sha256_hex(parts.join("\n").as_bytes())
}

fn push_recipient_parts(parts: &mut Vec<String>, role: &str, recipients: &[String]) {
    let mut recipients = recipients.to_vec();
    recipients.sort();
    for recipient in recipients {
        parts.push(format!("{role}:{recipient}"));
    }
}

fn retention_redactions(request: &AgentMailOutboxWriteRequest) -> Vec<RetentionRedaction> {
    let mut redactions = vec![RetentionRedaction::BodyDigestOnly];
    if request.body_preview_redacted.is_some() {
        redactions.push(RetentionRedaction::BodyPreviewRedacted);
    }
    redactions.push(RetentionRedaction::AttachmentRefsOnly);
    if !request.recipients.bcc.is_empty() {
        redactions.push(RetentionRedaction::BccAllowedForReplay);
    }
    redactions
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AgentMailOutboxValidationError {
    #[error("wrong schema_version {found}; expected {SCHEMA_VERSION}")]
    WrongSchemaVersion { found: u8 },
    #[error("wrong contract_id {found}; expected {CONTRACT_ID}")]
    WrongContractId { found: String },
    #[error("wrong source_bead {found}; expected {SOURCE_BEAD}")]
    WrongSourceBead { found: String },
    #[error("missing required field {0}")]
    MissingField(&'static str),
    #[error("invalid replay_id {value}")]
    InvalidReplayId { value: String },
    #[error("invalid UTC RFC3339 timestamp in {field}: {value}")]
    InvalidTimestamp { field: &'static str, value: String },
    #[error("invalid SHA-256 digest in {field}: {value}")]
    InvalidSha256 { field: &'static str, value: String },
    #[error("invalid text in {field}: {value}")]
    InvalidText { field: &'static str, value: String },
    #[error("subject is too long: {len} bytes")]
    SubjectTooLong { len: usize },
    #[error("too many recipients: {count}")]
    TooManyRecipients { count: usize },
    #[error("too many attachment references: {count}")]
    TooManyAttachments { count: usize },
    #[error("unsafe repository-relative path in {field}: {value}")]
    UnsafePath { field: &'static str, value: String },
    #[error("raw body retention is forbidden")]
    RawBodyRetained,
    #[error("body retention is unbounded: {byte_count} bytes with max {max_retained_bytes}")]
    UnboundedBody {
        byte_count: u64,
        max_retained_bytes: u64,
    },
    #[error("body preview is too large: {size} bytes")]
    BodyPreviewTooLarge { size: usize },
    #[error("body preview contains secret-like content")]
    SensitiveBodyPreview,
    #[error("body preview contains raw pane text markers")]
    RawPaneText,
    #[error("attachment {path} is too large: {byte_count} bytes")]
    AttachmentTooLarge { path: String, byte_count: u64 },
    #[error("invalid retry budget: retry_count {retry_count}, retry_limit {retry_limit}")]
    InvalidRetryBudget { retry_count: u32, retry_limit: u32 },
    #[error("invalid {state:?} state: {reason}")]
    InvalidState {
        state: OutboxState,
        reason: &'static str,
    },
    #[error("{state:?} requires an operator decision")]
    MissingOperatorDecision { state: OutboxState },
    #[error("{state:?} requires a replay receipt")]
    MissingReplayReceipt { state: OutboxState },
    #[error("automatic discard is forbidden; discard requires an operator decision")]
    AutomaticDiscard,
    #[error("invalid reservation TTL: {ttl_seconds}")]
    InvalidReservationTtl { ttl_seconds: u64 },
}

fn validate_non_empty(
    field: &'static str,
    value: &str,
) -> Result<(), AgentMailOutboxValidationError> {
    if value.trim().is_empty() {
        return Err(AgentMailOutboxValidationError::MissingField(field));
    }
    if value.contains('\0') {
        return Err(AgentMailOutboxValidationError::InvalidText {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_recipient(recipient: &str) -> Result<(), AgentMailOutboxValidationError> {
    validate_non_empty("recipients", recipient)?;
    if recipient.contains('/') || recipient.contains('\\') || recipient.contains('@') {
        return Err(AgentMailOutboxValidationError::InvalidText {
            field: "recipients",
            value: recipient.to_owned(),
        });
    }
    Ok(())
}

fn validate_subject(subject: &str) -> Result<(), AgentMailOutboxValidationError> {
    validate_non_empty("subject", subject)?;
    let len = subject.len();
    if len > MAX_SUBJECT_BYTES {
        return Err(AgentMailOutboxValidationError::SubjectTooLong { len });
    }
    if contains_secret_like(subject) || contains_raw_pane_text_marker(subject) {
        return Err(AgentMailOutboxValidationError::InvalidText {
            field: "subject",
            value: subject.to_owned(),
        });
    }
    Ok(())
}

fn validate_preview(preview: &str) -> Result<(), AgentMailOutboxValidationError> {
    validate_non_empty("body_preview_redacted", preview)?;
    let size = preview.len();
    if size > MAX_BODY_PREVIEW_BYTES {
        return Err(AgentMailOutboxValidationError::BodyPreviewTooLarge { size });
    }
    if contains_secret_like(preview) {
        return Err(AgentMailOutboxValidationError::SensitiveBodyPreview);
    }
    if contains_raw_pane_text_marker(preview) {
        return Err(AgentMailOutboxValidationError::RawPaneText);
    }
    Ok(())
}

fn validate_rfc3339_utc(
    field: &'static str,
    value: &str,
) -> Result<(), AgentMailOutboxValidationError> {
    if !value.ends_with('Z') || DateTime::parse_from_rfc3339(value).is_err() {
        return Err(AgentMailOutboxValidationError::InvalidTimestamp {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_replay_id(value: &str) -> Result<(), AgentMailOutboxValidationError> {
    let Some(suffix) = value.strip_prefix("amq1-") else {
        return Err(AgentMailOutboxValidationError::InvalidReplayId {
            value: value.to_owned(),
        });
    };
    if suffix.len() != 24 || !suffix.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(AgentMailOutboxValidationError::InvalidReplayId {
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_sha256(field: &'static str, value: &str) -> Result<(), AgentMailOutboxValidationError> {
    if value.len() != 64 || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(AgentMailOutboxValidationError::InvalidSha256 {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_repo_relative_path(
    field: &'static str,
    value: &str,
) -> Result<(), AgentMailOutboxValidationError> {
    if value.is_empty()
        || value.starts_with('/')
        || value.starts_with('~')
        || value.contains('\\')
        || value.contains("://")
    {
        return Err(AgentMailOutboxValidationError::UnsafePath {
            field,
            value: value.to_owned(),
        });
    }
    let parts: Vec<&str> = value.split('/').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == ".." || *part == ".git")
    {
        return Err(AgentMailOutboxValidationError::UnsafePath {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn require_optional_reason(
    state: OutboxState,
    reason: Option<&str>,
) -> Result<(), AgentMailOutboxValidationError> {
    let Some(reason) = reason else {
        return Err(AgentMailOutboxValidationError::InvalidState {
            state,
            reason: "state_reason is required",
        });
    };
    validate_preview(reason)
}

fn contains_secret_like(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "sk-",
        "ghp_",
        "github_pat_",
        "xoxb-",
        "akia",
        "-----begin",
        "password=",
        "token=",
        "secret=",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn contains_raw_pane_text_marker(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("pane_text")
        || lower.contains("raw pane")
        || lower.contains("terminal scrollback")
        || lower.contains("ft robot get-text")
        || value.contains("\u{1b}[")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> String {
        "0123456789abcdef".repeat(4)
    }

    fn valid_entry() -> AgentMailOutboxEntry {
        AgentMailOutboxEntry {
            schema_version: SCHEMA_VERSION,
            contract_id: CONTRACT_ID.to_owned(),
            source_bead: SOURCE_BEAD.to_owned(),
            replay_id: "amq1-0123456789abcdef01234567".to_owned(),
            created_at: "2026-05-29T13:35:00Z".to_owned(),
            agent: OutboxAgentIdentity {
                name: "IvoryCreek".to_owned(),
                program: "codex-cli".to_owned(),
                model: "gpt-5-codex".to_owned(),
                project_key: "/Users/jemanuel/projects/frankenterm".to_owned(),
            },
            thread_id: Some("ft-dezx8.1".to_owned()),
            recipients: OutboxRecipients {
                to: vec!["SapphireCardinal".to_owned()],
                cc: Vec::new(),
                bcc: Vec::new(),
            },
            subject: "ft-dezx8.1 fallback coordination".to_owned(),
            body_policy: BodyPolicy {
                body_sha256: digest(),
                body_storage: BodyStoragePolicy::DigestOnly,
                body_preview_redacted: Some("Coordination body stored by digest only.".to_owned()),
                body_byte_count: 128,
                max_retained_bytes: 4096,
                raw_body_retained: false,
            },
            importance: OutboxImportance::Normal,
            ack_required: false,
            attachments: Vec::new(),
            source_operation: SourceOperation::SendMessage,
            failure_reason: FailureReason {
                class: FailureClass::AgentMailUnavailable,
                summary: "Agent Mail API unavailable after the allowed retry.".to_owned(),
                retry_count: 1,
                retry_limit: 1,
                last_attempt_at: "2026-05-29T13:35:10Z".to_owned(),
            },
            state: OutboxState::Queued,
            state_reason: None,
            operator_decision: None,
            replay_receipt: None,
            retention: RetentionPolicy {
                retain_until: "2026-06-28T13:35:00Z".to_owned(),
                automatic_discard: false,
                redaction: vec![
                    RetentionRedaction::BodyDigestOnly,
                    RetentionRedaction::BodyPreviewRedacted,
                    RetentionRedaction::AttachmentRefsOnly,
                ],
            },
            reservation_intent: None,
            beads_fallback: None,
        }
    }

    fn valid_write_request() -> AgentMailOutboxWriteRequest {
        AgentMailOutboxWriteRequest {
            agent: OutboxAgentIdentity {
                name: "IvoryCreek".to_owned(),
                program: "codex-cli".to_owned(),
                model: "gpt-5-codex".to_owned(),
                project_key: "/Users/jemanuel/projects/frankenterm".to_owned(),
            },
            thread_id: Some("ft-dezx8.2".to_owned()),
            recipients: OutboxRecipients {
                to: vec!["SapphireCardinal".to_owned()],
                cc: vec!["YellowHorse".to_owned()],
                bcc: Vec::new(),
            },
            subject: "ft-dezx8.2 fallback coordination".to_owned(),
            body_markdown: "Agent Mail send failed after retry; queueing fallback intent."
                .to_owned(),
            body_preview_redacted: Some(
                "Agent Mail send failed after retry; queueing fallback intent.".to_owned(),
            ),
            importance: OutboxImportance::Normal,
            ack_required: false,
            attachments: Vec::new(),
            source_operation: SourceOperation::SendMessage,
            reservation_intent: None,
            beads_fallback: None,
            created_at: "2026-05-29T14:05:33Z".to_owned(),
        }
    }

    #[test]
    fn valid_fixture_entries_pass_validation() {
        for fixture in [
            include_str!(
                "../../../fixtures/agent-mail-outage-spool/valid/agent-mail-unavailable.json"
            ),
            include_str!("../../../fixtures/agent-mail-outage-spool/valid/contact-blocked.json"),
            include_str!(
                "../../../fixtures/agent-mail-outage-spool/valid/ack-required-message.json"
            ),
            include_str!("../../../fixtures/agent-mail-outage-spool/valid/reservation-intent.json"),
            include_str!(
                "../../../fixtures/agent-mail-outage-spool/valid/beads-fallback-closeout.json"
            ),
            include_str!(
                "../../../fixtures/agent-mail-outage-spool/valid/stale-owner-handoff.json"
            ),
            include_str!(
                "../../../fixtures/agent-mail-outage-spool/valid/writer-adapter-queued-send.json"
            ),
        ] {
            let entry: AgentMailOutboxEntry =
                serde_json::from_str(fixture).expect("fixture parses");
            entry.validate().expect("fixture validates");
        }
    }

    #[test]
    fn deterministic_replay_id_uses_all_seed_parts() {
        let seed = ReplayIdSeed {
            project_key: "/repo",
            agent_name: "IvoryCreek",
            thread_id: Some("ft-dezx8.1"),
            recipients_digest: "to:SapphireCardinal",
            subject_digest: "subject",
            body_digest: "body",
            source_operation: SourceOperation::SendMessage,
            created_at_bucket: "2026-05-29T13:35Z",
        };
        let first = deterministic_replay_id(&seed);
        let second = deterministic_replay_id(&seed);
        let mut reply_seed = seed.clone();
        reply_seed.source_operation = SourceOperation::ReplyMessage;

        assert_eq!(first, second);
        assert_ne!(first, deterministic_replay_id(&reply_seed));
        assert!(first.starts_with("amq1-"));
        assert_eq!(first.len(), 29);
    }

    #[test]
    fn writer_adapter_queues_failed_send_after_allowed_retry() {
        let request = valid_write_request();
        let failure = AgentMailOutboxFailure::classified(
            "API error: Agent Mail send returned HTTP 503",
            AGENT_MAIL_RETRY_LIMIT,
            "2026-05-29T14:05:38Z",
        );

        let entry = build_queued_outbox_entry(request.clone(), failure.clone())
            .expect("failed send queues after retry");
        let repeat =
            build_queued_outbox_entry(request.clone(), failure).expect("same write repeats");

        assert_eq!(entry.replay_id, repeat.replay_id);
        assert_eq!(entry.state, OutboxState::Queued);
        assert_eq!(entry.source_operation, SourceOperation::SendMessage);
        assert_eq!(entry.failure_reason.class, FailureClass::ApiError);
        assert_eq!(
            entry.failure_reason.summary,
            "API error: Agent Mail send returned HTTP 503"
        );
        assert_eq!(entry.failure_reason.retry_count, AGENT_MAIL_RETRY_LIMIT);
        assert_eq!(entry.failure_reason.retry_limit, AGENT_MAIL_RETRY_LIMIT);
        assert_eq!(
            entry.body_policy.body_sha256,
            sha256_hex(request.body_markdown.as_bytes())
        );
        assert_eq!(
            entry.body_policy.body_storage,
            BodyStoragePolicy::DigestOnly
        );
        assert!(!entry.body_policy.raw_body_retained);
        assert_eq!(entry.retention.retain_until, "2026-06-28T14:05:33Z");
        assert!(
            entry
                .retention
                .redaction
                .contains(&RetentionRedaction::BodyDigestOnly)
        );
        assert!(
            entry
                .retention
                .redaction
                .contains(&RetentionRedaction::BodyPreviewRedacted)
        );
        entry.validate().expect("queued entry validates");
    }

    #[test]
    fn writer_adapter_refuses_to_queue_before_retry_is_exhausted() {
        let failure = AgentMailOutboxFailure::classified(
            "connection refused while sending Agent Mail message",
            0,
            "2026-05-29T14:05:38Z",
        );

        assert!(matches!(
            build_queued_outbox_entry(valid_write_request(), failure),
            Err(AgentMailOutboxWriterError::RetryBudgetRemaining {
                retry_count: 0,
                retry_limit: AGENT_MAIL_RETRY_LIMIT,
                remaining: 1
            })
        ));
    }

    #[test]
    fn writer_adapter_classifies_failures_without_rewriting_summary() {
        for (summary, class) in [
            (
                "connection refused while sending Agent Mail message",
                FailureClass::ApiUnreachable,
            ),
            ("Agent Mail request timed out", FailureClass::Timeout),
            (
                "Contact permission blocked by policy",
                FailureClass::ContactPermissionBlocked,
            ),
            (
                "ack unavailable for ack_required message",
                FailureClass::AckUnavailable,
            ),
            (
                "API error: Agent Mail send returned HTTP 503",
                FailureClass::ApiError,
            ),
        ] {
            let failure = AgentMailOutboxFailure::classified(
                summary,
                AGENT_MAIL_RETRY_LIMIT,
                "2026-05-29T14:05:38Z",
            );
            assert_eq!(failure.class, class);
            assert_eq!(failure.summary, summary);
        }
    }

    #[test]
    fn writer_adapter_requires_reservation_intent_for_reservation_operations() {
        let mut request = valid_write_request();
        request.source_operation = SourceOperation::FileReservation;
        let failure = AgentMailOutboxFailure::classified(
            "Agent Mail unavailable while reserving files",
            AGENT_MAIL_RETRY_LIMIT,
            "2026-05-29T14:05:38Z",
        );

        assert!(matches!(
            build_queued_outbox_entry(request, failure),
            Err(AgentMailOutboxWriterError::MissingReservationIntent {
                operation: SourceOperation::FileReservation
            })
        ));
    }

    #[test]
    fn writer_guidance_does_not_authorize_agent_mail_repair_or_restart() {
        let guidance = OUTBOX_WRITER_FALLBACK_GUIDANCE
            .join("\n")
            .to_ascii_lowercase();

        for command in FORBIDDEN_AGENT_MAIL_RECOVERY_COMMANDS {
            assert!(!guidance.contains(command));
        }
        assert!(guidance.contains("delivery is not claimed"));
    }

    #[test]
    fn rejects_raw_body_retention() {
        let mut entry = valid_entry();
        entry.body_policy.raw_body_retained = true;

        assert!(matches!(
            entry.validate(),
            Err(AgentMailOutboxValidationError::RawBodyRetained)
        ));
    }

    #[test]
    fn rejects_secret_preview() {
        let mut entry = valid_entry();
        entry.body_policy.body_preview_redacted =
            Some("accidental token=sk-live-123456".to_owned());

        assert!(matches!(
            entry.validate(),
            Err(AgentMailOutboxValidationError::SensitiveBodyPreview)
        ));
    }

    #[test]
    fn rejects_raw_pane_text_preview() {
        let mut entry = valid_entry();
        entry.body_policy.body_preview_redacted =
            Some("raw pane text from ft robot get-text".to_owned());

        assert!(matches!(
            entry.validate(),
            Err(AgentMailOutboxValidationError::RawPaneText)
        ));
    }

    #[test]
    fn rejects_absolute_attachment_path() {
        let mut entry = valid_entry();
        entry.attachments.push(AttachmentRef {
            path: "/tmp/outbox.log".to_owned(),
            sha256: Some(digest()),
            byte_count: 10,
            content_type: Some("text/plain".to_owned()),
            retention: AttachmentRetention::DigestOnly,
        });

        assert!(matches!(
            entry.validate(),
            Err(AgentMailOutboxValidationError::UnsafePath { .. })
        ));
    }

    #[test]
    fn rejects_ambiguous_timestamp() {
        let mut entry = valid_entry();
        entry.created_at = "2026-05-29 13:35:00".to_owned();

        assert!(matches!(
            entry.validate(),
            Err(AgentMailOutboxValidationError::InvalidTimestamp { .. })
        ));
    }

    #[test]
    fn discarded_state_requires_operator_decision() {
        let mut entry = valid_entry();
        entry.state = OutboxState::DiscardedByOperator;

        assert!(matches!(
            entry.validate(),
            Err(AgentMailOutboxValidationError::MissingOperatorDecision { .. })
        ));
    }

    #[test]
    fn replayed_state_requires_delivery_receipt() {
        let mut entry = valid_entry();
        entry.state = OutboxState::Replayed;

        assert!(matches!(
            entry.validate(),
            Err(AgentMailOutboxValidationError::MissingReplayReceipt { .. })
        ));
    }

    #[test]
    fn reservation_intent_rejects_short_ttl() {
        let mut entry = valid_entry();
        entry.source_operation = SourceOperation::FileReservation;
        entry.reservation_intent = Some(ReservationIntent {
            paths: vec!["crates/frankenterm-core/src/agent_mail_outbox.rs".to_owned()],
            exclusive: true,
            ttl_seconds: 10,
            reason_sha256: digest(),
            reason_preview_redacted: Some("Reserve the outbox contract files.".to_owned()),
        });

        assert!(matches!(
            entry.validate(),
            Err(AgentMailOutboxValidationError::InvalidReservationTtl { .. })
        ));
    }
}
