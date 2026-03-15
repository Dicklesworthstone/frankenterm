//! Mission-level Agent Mail coordination kernel.
//!
//! This module bridges mission-orchestration coordination events into an
//! Agent-Mail-compatible transport surface:
//!
//! - Emit structured lifecycle notices (`start`, `progress`, `handoff`)
//! - Convert mission conflict/deconfliction output into contention signals
//! - Consume inbox events and enforce ack-required coordination flows
//! - Carry deterministic mission/fleet/task metadata in every envelope

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::fleet_launcher::{FleetLaunchStatus, LaunchOutcome, LaunchPlan, StartupStrategy};
use crate::mission_loop::ConflictDetectionReport;

const COORDINATION_ENVELOPE_MARKER: &str = "[ft-coordination-envelope]";
const COORDINATION_ENVELOPE_VERSION: u32 = 1;

/// Configuration for mission coordination mail behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionAgentMailConfig {
    /// Deadline window for ack-required events.
    pub ack_timeout_ms: i64,
}

impl Default for MissionAgentMailConfig {
    fn default() -> Self {
        Self {
            ack_timeout_ms: 5 * 60 * 1000,
        }
    }
}

/// Mission/fleet/task context carried in coordination envelopes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MissionCoordinationContext {
    /// Mission identifier.
    pub mission_id: String,
    /// Optional fleet identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet_id: Option<String>,
    /// Optional bead/task identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bead_id: Option<String>,
    /// Optional assignment identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<String>,
    /// Optional explicit thread id. If absent, one is derived deterministically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Correlation id for end-to-end traceability.
    pub correlation_id: String,
    /// Optional scenario id (tests/e2e/drills).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario_id: Option<String>,
}

impl MissionCoordinationContext {
    /// Deterministically derive a canonical thread id when one is not provided.
    #[must_use]
    pub fn canonical_thread_id(&self) -> String {
        if let Some(thread_id) = self
            .thread_id
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            return thread_id.trim().to_string();
        }

        let mut tokens = vec![sanitize_token(&self.mission_id)];
        if let Some(fleet_id) = self
            .fleet_id
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            tokens.push(sanitize_token(fleet_id));
        }
        if let Some(bead_id) = self
            .bead_id
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            tokens.push(sanitize_token(bead_id));
        }
        format!("mission-{}", tokens.join("-"))
    }
}

/// Coordination event kind carried in envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinationEventKind {
    /// Mission/fleet start announcement.
    StartNotice,
    /// In-flight progress announcement.
    ProgressUpdate,
    /// Ownership handoff announcement.
    Handoff,
    /// Contention/deconfliction signal.
    ContentionSignal,
    /// Acknowledgement of an ack-required coordination message.
    Acknowledgement,
}

impl CoordinationEventKind {
    #[must_use]
    fn subject_tag(self) -> &'static str {
        match self {
            Self::StartNotice => "start",
            Self::ProgressUpdate => "progress",
            Self::Handoff => "handoff",
            Self::ContentionSignal => "contention",
            Self::Acknowledgement => "ack",
        }
    }
}

/// Serialized coordination envelope embedded into message bodies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoordinationEnvelope {
    /// Envelope schema version.
    pub version: u32,
    /// Coordination event kind.
    pub event_kind: CoordinationEventKind,
    /// Whether receiver acknowledgement is required.
    pub ack_required: bool,
    /// Emission timestamp (Unix ms).
    pub emitted_at_ms: i64,
    /// Stable reason code.
    pub reason_code: String,
    /// Stable error code when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Mission/fleet/task context.
    pub context: MissionCoordinationContext,
    /// Arbitrary extra metadata.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

/// Event request accepted by the coordination kernel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoordinationEventRequest {
    /// Coordination event kind.
    pub kind: CoordinationEventKind,
    /// Human-readable summary for subject generation.
    pub summary: String,
    /// Human-readable body (envelope appended automatically).
    pub body: String,
    /// Recipients to notify.
    pub recipients: Vec<String>,
    /// Ack-required semantics.
    pub ack_required: bool,
    /// Context linkage.
    pub context: MissionCoordinationContext,
    /// Stable reason code.
    pub reason_code: String,
    /// Optional stable error code.
    pub error_code: Option<String>,
    /// Additional metadata.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

/// Transport-facing inbox message abstraction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoordinationInboxMessage {
    /// Unique message id.
    pub id: Option<String>,
    /// Sender.
    pub from: Option<String>,
    /// Subject.
    pub subject: String,
    /// Body.
    pub body: String,
    /// Read/ack state from transport.
    pub read: bool,
    /// Optional thread id.
    pub thread_id: Option<String>,
    /// Optional transport timestamp.
    pub timestamp: Option<String>,
    /// Optional transport ack-required signal.
    pub ack_required: Option<bool>,
}

/// Minimal transport contract for coordination mail.
pub trait MissionMailTransport {
    /// Send a message.
    fn send_message(&self, to: &str, subject: &str, body: &str) -> Result<String, String>;
    /// Fetch inbox contents for an agent.
    fn fetch_inbox(&self, agent_name: &str) -> Vec<CoordinationInboxMessage>;
}

/// Successful delivery metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DispatchedCoordinationMessage {
    /// Recipient.
    pub recipient: String,
    /// Message id assigned by transport.
    pub message_id: String,
    /// Subject used for delivery.
    pub subject: String,
    /// Thread id carried in envelope.
    pub thread_id: String,
    /// Correlation id carried in envelope.
    pub correlation_id: String,
    /// Ack-required semantics.
    pub ack_required: bool,
    /// Ack deadline in Unix ms when `ack_required=true`.
    pub ack_deadline_ms: Option<i64>,
}

/// Failed delivery metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FailedCoordinationMessage {
    /// Recipient that failed.
    pub recipient: String,
    /// Subject attempted.
    pub subject: String,
    /// Error text.
    pub error: String,
}

/// Aggregate dispatch report for one emission operation.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct MissionMailDispatchReport {
    /// Number of recipients attempted.
    pub attempted: usize,
    /// Successful deliveries.
    pub delivered: Vec<DispatchedCoordinationMessage>,
    /// Failed deliveries.
    pub failed: Vec<FailedCoordinationMessage>,
}

impl MissionMailDispatchReport {
    fn merge(&mut self, other: Self) {
        self.attempted += other.attempted;
        self.delivered.extend(other.delivered);
        self.failed.extend(other.failed);
    }
}

/// Parsed inbound coordination message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboundCoordinationMessage {
    /// Raw message id.
    pub message_id: Option<String>,
    /// Sender.
    pub from: Option<String>,
    /// Subject.
    pub subject: String,
    /// Read state.
    pub read: bool,
    /// Parsed envelope.
    pub envelope: CoordinationEnvelope,
}

/// Parse failure for a coordination-like inbox message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoordinationParseFailure {
    /// Message id.
    pub message_id: Option<String>,
    /// Subject.
    pub subject: String,
    /// Parse error.
    pub error: String,
}

/// Inbox consumption report.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct InboxConsumptionReport {
    /// Parsed coordination messages.
    pub parsed: Vec<InboundCoordinationMessage>,
    /// Parse failures.
    pub parse_failures: Vec<CoordinationParseFailure>,
    /// Raw inbox message count.
    pub raw_count: usize,
}

/// Ack-required pending item surfaced to receivers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingAcknowledgement {
    /// Message that requires acknowledgement.
    pub message_id: String,
    /// Reply target (original sender).
    pub reply_to: Option<String>,
    /// Thread id for acknowledgement routing.
    pub thread_id: String,
    /// Deadline for acknowledgement.
    pub deadline_ms: i64,
    /// Whether the deadline has elapsed.
    pub overdue: bool,
    /// Original envelope context.
    pub context: MissionCoordinationContext,
}

/// Ack-required collection report.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AckRequirementReport {
    /// Pending ack items.
    pub pending: Vec<PendingAcknowledgement>,
    /// Parse failures from inbox consumption.
    pub parse_failures: Vec<CoordinationParseFailure>,
    /// Raw inbox message count.
    pub raw_count: usize,
}

/// Mission Agent Mail coordination kernel.
pub struct MissionAgentMailKernel<T: MissionMailTransport> {
    transport: T,
    config: MissionAgentMailConfig,
}

impl<T: MissionMailTransport> MissionAgentMailKernel<T> {
    /// Create a new coordination kernel with default config.
    #[must_use]
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            config: MissionAgentMailConfig::default(),
        }
    }

    /// Create a new coordination kernel with explicit config.
    #[must_use]
    pub fn with_config(transport: T, config: MissionAgentMailConfig) -> Self {
        Self { transport, config }
    }

    /// Immutable access to configuration.
    #[must_use]
    pub fn config(&self) -> &MissionAgentMailConfig {
        &self.config
    }

    /// Emit a generic coordination event.
    #[must_use]
    pub fn emit_event_at(
        &self,
        now_ms: i64,
        mut request: CoordinationEventRequest,
    ) -> MissionMailDispatchReport {
        let recipients = canonicalize_recipients(&request.recipients);
        let mut report = MissionMailDispatchReport {
            attempted: recipients.len(),
            ..MissionMailDispatchReport::default()
        };

        if recipients.is_empty() {
            return report;
        }

        let thread_id = request.context.canonical_thread_id();
        request.context.thread_id = Some(thread_id.clone());

        let subject = render_subject(
            &request.context.mission_id,
            request.kind,
            request.summary.trim(),
        );
        let envelope = CoordinationEnvelope {
            version: COORDINATION_ENVELOPE_VERSION,
            event_kind: request.kind,
            ack_required: request.ack_required,
            emitted_at_ms: now_ms,
            reason_code: request.reason_code,
            error_code: request.error_code,
            context: request.context.clone(),
            metadata: request.metadata,
        };

        let serialized = match serde_json::to_string(&envelope) {
            Ok(serialized) => serialized,
            Err(err) => {
                for recipient in &recipients {
                    report.failed.push(FailedCoordinationMessage {
                        recipient: recipient.clone(),
                        subject: subject.clone(),
                        error: format!("envelope serialization failed: {err}"),
                    });
                }
                return report;
            }
        };
        let body = render_body(request.body.trim(), &serialized);

        for recipient in recipients {
            match self.transport.send_message(&recipient, &subject, &body) {
                Ok(message_id) => {
                    let ack_deadline = request
                        .ack_required
                        .then_some(now_ms.saturating_add(self.config.ack_timeout_ms));
                    debug!(
                        subsystem = "mission_agent_mail",
                        recipient = %recipient,
                        mission_id = %request.context.mission_id,
                        thread_id = %thread_id,
                        correlation_id = %request.context.correlation_id,
                        scenario_id = ?request.context.scenario_id,
                        reason_code = %envelope.reason_code,
                        error_code = ?envelope.error_code,
                        event_kind = ?request.kind,
                        ack_required = request.ack_required,
                        "coordination event delivered"
                    );
                    report.delivered.push(DispatchedCoordinationMessage {
                        recipient,
                        message_id,
                        subject: subject.clone(),
                        thread_id: thread_id.clone(),
                        correlation_id: request.context.correlation_id.clone(),
                        ack_required: request.ack_required,
                        ack_deadline_ms: ack_deadline,
                    });
                }
                Err(error) => {
                    warn!(
                        subsystem = "mission_agent_mail",
                        recipient = %recipient,
                        mission_id = %request.context.mission_id,
                        thread_id = %thread_id,
                        correlation_id = %request.context.correlation_id,
                        scenario_id = ?request.context.scenario_id,
                        reason_code = %envelope.reason_code,
                        error_code = ?envelope.error_code,
                        transport_error = %error,
                        "coordination event delivery failed"
                    );
                    report.failed.push(FailedCoordinationMessage {
                        recipient,
                        subject: subject.clone(),
                        error,
                    });
                }
            }
        }

        report
    }

    /// Emit a mission start notice (ack-required by default).
    #[must_use]
    pub fn emit_start_notice_at(
        &self,
        now_ms: i64,
        recipients: Vec<String>,
        context: MissionCoordinationContext,
        summary: &str,
        body: &str,
    ) -> MissionMailDispatchReport {
        self.emit_event_at(
            now_ms,
            CoordinationEventRequest {
                kind: CoordinationEventKind::StartNotice,
                summary: summary.to_string(),
                body: body.to_string(),
                recipients,
                ack_required: true,
                context,
                reason_code: "mission.start_notice".to_string(),
                error_code: None,
                metadata: HashMap::new(),
            },
        )
    }

    /// Emit a mission progress update.
    #[must_use]
    pub fn emit_progress_update_at(
        &self,
        now_ms: i64,
        recipients: Vec<String>,
        context: MissionCoordinationContext,
        summary: &str,
        body: &str,
    ) -> MissionMailDispatchReport {
        self.emit_event_at(
            now_ms,
            CoordinationEventRequest {
                kind: CoordinationEventKind::ProgressUpdate,
                summary: summary.to_string(),
                body: body.to_string(),
                recipients,
                ack_required: false,
                context,
                reason_code: "mission.progress_update".to_string(),
                error_code: None,
                metadata: HashMap::new(),
            },
        )
    }

    /// Emit a mission handoff notice (ack-required by default).
    #[must_use]
    pub fn emit_handoff_notice_at(
        &self,
        now_ms: i64,
        recipients: Vec<String>,
        context: MissionCoordinationContext,
        summary: &str,
        body: &str,
    ) -> MissionMailDispatchReport {
        self.emit_event_at(
            now_ms,
            CoordinationEventRequest {
                kind: CoordinationEventKind::Handoff,
                summary: summary.to_string(),
                body: body.to_string(),
                recipients,
                ack_required: true,
                context,
                reason_code: "mission.handoff".to_string(),
                error_code: None,
                metadata: HashMap::new(),
            },
        )
    }

    /// Emit a fleet-launch start notice derived from a launch plan.
    ///
    /// The emitted context and metadata link directly to fleet topology inputs
    /// so downstream schedulers/auditors can correlate launch intent with
    /// subsequent progress and outcome events.
    #[must_use]
    pub fn emit_fleet_launch_start_notice_at(
        &self,
        now_ms: i64,
        recipients: Vec<String>,
        launch_plan: &LaunchPlan,
        correlation_id: &str,
        scenario_id: Option<String>,
    ) -> MissionMailDispatchReport {
        let context = fleet_context_from_plan(launch_plan, correlation_id, scenario_id);
        let mut metadata = HashMap::new();
        metadata.insert("workspace_id".to_string(), launch_plan.workspace_id.clone());
        metadata.insert("domain".to_string(), launch_plan.domain.clone());
        metadata.insert("generation".to_string(), launch_plan.generation.to_string());
        metadata.insert(
            "strategy".to_string(),
            startup_strategy_label(launch_plan.strategy).to_string(),
        );
        metadata.insert(
            "planned_slots".to_string(),
            launch_plan.slots.len().to_string(),
        );

        self.emit_event_at(
            now_ms,
            CoordinationEventRequest {
                kind: CoordinationEventKind::StartNotice,
                summary: format!("fleet launch start: {}", launch_plan.name),
                body: format!(
                    "Launching fleet '{}' with {} planned slot(s).",
                    launch_plan.name,
                    launch_plan.slots.len()
                ),
                recipients,
                ack_required: true,
                context,
                reason_code: "mission.fleet_launch_start".to_string(),
                error_code: None,
                metadata,
            },
        )
    }

    /// Emit a fleet-launch progress update for a specific startup phase.
    #[must_use]
    pub fn emit_fleet_launch_progress_update_at(
        &self,
        now_ms: i64,
        recipients: Vec<String>,
        launch_plan: &LaunchPlan,
        phase_index: u32,
        started_slots: usize,
        correlation_id: &str,
        scenario_id: Option<String>,
    ) -> MissionMailDispatchReport {
        let context = fleet_context_from_plan(launch_plan, correlation_id, scenario_id);
        let mut metadata = HashMap::new();
        metadata.insert("workspace_id".to_string(), launch_plan.workspace_id.clone());
        metadata.insert("domain".to_string(), launch_plan.domain.clone());
        metadata.insert("generation".to_string(), launch_plan.generation.to_string());
        metadata.insert(
            "strategy".to_string(),
            startup_strategy_label(launch_plan.strategy).to_string(),
        );
        metadata.insert("phase_index".to_string(), phase_index.to_string());
        metadata.insert("started_slots".to_string(), started_slots.to_string());
        metadata.insert(
            "total_slots".to_string(),
            launch_plan.slots.len().to_string(),
        );

        self.emit_event_at(
            now_ms,
            CoordinationEventRequest {
                kind: CoordinationEventKind::ProgressUpdate,
                summary: format!("fleet launch progress: {}", launch_plan.name),
                body: format!(
                    "Phase {phase_index}: started {started_slots}/{} slot(s).",
                    launch_plan.slots.len()
                ),
                recipients,
                ack_required: false,
                context,
                reason_code: "mission.fleet_launch_progress".to_string(),
                error_code: None,
                metadata,
            },
        )
    }

    /// Emit a fleet-launch outcome notice derived from launch execution output.
    #[must_use]
    pub fn emit_fleet_launch_outcome_notice_at(
        &self,
        now_ms: i64,
        recipients: Vec<String>,
        launch_outcome: &LaunchOutcome,
        workspace_id: &str,
        domain: &str,
        generation: u64,
        correlation_id: &str,
        scenario_id: Option<String>,
    ) -> MissionMailDispatchReport {
        let context = fleet_context_from_outcome(
            launch_outcome,
            workspace_id,
            domain,
            generation,
            correlation_id,
            scenario_id,
        );
        let mut metadata = HashMap::new();
        metadata.insert("workspace_id".to_string(), workspace_id.to_string());
        metadata.insert("domain".to_string(), domain.to_string());
        metadata.insert("generation".to_string(), generation.to_string());
        metadata.insert(
            "status".to_string(),
            fleet_status_label(launch_outcome.status).to_string(),
        );
        metadata.insert(
            "successful_slots".to_string(),
            launch_outcome.successful_slots.to_string(),
        );
        metadata.insert(
            "failed_slots".to_string(),
            launch_outcome.failed_slots.to_string(),
        );
        metadata.insert(
            "total_slots".to_string(),
            launch_outcome.total_slots.to_string(),
        );

        self.emit_event_at(
            now_ms,
            CoordinationEventRequest {
                kind: CoordinationEventKind::ProgressUpdate,
                summary: format!("fleet launch outcome: {}", launch_outcome.name),
                body: format!(
                    "Launch outcome for '{}': status={}, success={}/{}, failed={}.",
                    launch_outcome.name,
                    fleet_status_label(launch_outcome.status),
                    launch_outcome.successful_slots,
                    launch_outcome.total_slots,
                    launch_outcome.failed_slots
                ),
                recipients,
                ack_required: false,
                context,
                reason_code: "mission.fleet_launch_outcome".to_string(),
                error_code: None,
                metadata,
            },
        )
    }

    /// Emit contention signals from mission-loop conflict detection output.
    #[must_use]
    pub fn emit_conflict_signals_at(
        &self,
        now_ms: i64,
        context: &MissionCoordinationContext,
        conflict_report: &ConflictDetectionReport,
    ) -> MissionMailDispatchReport {
        let mut aggregate = MissionMailDispatchReport::default();
        let mut conflict_reason_by_id: HashMap<&str, (&str, &str)> = HashMap::new();
        for conflict in &conflict_report.conflicts {
            conflict_reason_by_id.insert(
                &conflict.conflict_id,
                (&conflict.reason_code, &conflict.error_code),
            );
        }

        for message in &conflict_report.messages {
            let mut event_context = context.clone();
            if !message.thread_id.trim().is_empty() {
                event_context.thread_id = Some(message.thread_id.clone());
            }

            let (reason_code, error_code) = conflict_reason_by_id
                .get(message.conflict_id.as_str())
                .map_or(
                    ("mission.contention_detected", "robot.mission_contention"),
                    |value| *value,
                );

            let mut metadata = HashMap::new();
            metadata.insert("conflict_id".to_string(), message.conflict_id.clone());

            let partial = self.emit_event_at(
                now_ms,
                CoordinationEventRequest {
                    kind: CoordinationEventKind::ContentionSignal,
                    summary: message.subject.clone(),
                    body: message.body.clone(),
                    recipients: vec![message.recipient.clone()],
                    ack_required: true,
                    context: event_context,
                    reason_code: reason_code.to_string(),
                    error_code: Some(error_code.to_string()),
                    metadata,
                },
            );
            aggregate.merge(partial);
        }

        aggregate
    }

    /// Consume and parse coordination messages from inbox.
    #[must_use]
    pub fn consume_inbox(&self, agent_name: &str) -> InboxConsumptionReport {
        let inbox = self.transport.fetch_inbox(agent_name);
        let mut parsed = Vec::new();
        let mut parse_failures = Vec::new();

        for message in &inbox {
            match parse_envelope_from_body(&message.body) {
                Ok(envelope) => parsed.push(InboundCoordinationMessage {
                    message_id: message.id.clone(),
                    from: message.from.clone(),
                    subject: message.subject.clone(),
                    read: message.read,
                    envelope,
                }),
                Err(error) => {
                    if message.body.contains(COORDINATION_ENVELOPE_MARKER) {
                        warn!(
                            subsystem = "mission_agent_mail",
                            agent_name = %agent_name,
                            message_id = ?message.id,
                            subject = %message.subject,
                            parse_error = %error,
                            "failed to parse coordination envelope"
                        );
                        parse_failures.push(CoordinationParseFailure {
                            message_id: message.id.clone(),
                            subject: message.subject.clone(),
                            error,
                        });
                    }
                }
            }
        }

        InboxConsumptionReport {
            parsed,
            parse_failures,
            raw_count: inbox.len(),
        }
    }

    /// Collect unread ack-required coordination events that need acknowledgement.
    #[must_use]
    pub fn collect_pending_acknowledgements_at(
        &self,
        agent_name: &str,
        now_ms: i64,
    ) -> AckRequirementReport {
        let consumption = self.consume_inbox(agent_name);
        let pending = consumption
            .parsed
            .iter()
            .filter(|message| {
                !message.read
                    && message.envelope.ack_required
                    && message.envelope.event_kind != CoordinationEventKind::Acknowledgement
            })
            .map(|message| {
                let deadline_ms = message
                    .envelope
                    .emitted_at_ms
                    .saturating_add(self.config.ack_timeout_ms);
                PendingAcknowledgement {
                    message_id: message
                        .message_id
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string()),
                    reply_to: message.from.clone(),
                    thread_id: message.envelope.context.canonical_thread_id(),
                    deadline_ms,
                    overdue: now_ms > deadline_ms,
                    context: message.envelope.context.clone(),
                }
            })
            .collect::<Vec<_>>();

        AckRequirementReport {
            pending,
            parse_failures: consumption.parse_failures,
            raw_count: consumption.raw_count,
        }
    }

    /// Emit acknowledgement events for pending ack-required messages.
    #[must_use]
    pub fn emit_acknowledgements_at(
        &self,
        now_ms: i64,
        acknowledger: &str,
        pending: &[PendingAcknowledgement],
        note: &str,
    ) -> MissionMailDispatchReport {
        let mut report = MissionMailDispatchReport::default();

        for item in pending {
            let Some(reply_to) = item
                .reply_to
                .as_ref()
                .filter(|value| !value.trim().is_empty())
            else {
                report.failed.push(FailedCoordinationMessage {
                    recipient: "<missing>".to_string(),
                    subject: format!("[mission:{}][ack] acknowledgement", item.context.mission_id),
                    error: format!(
                        "cannot emit acknowledgement for message {}: missing sender",
                        item.message_id
                    ),
                });
                continue;
            };

            let mut metadata = HashMap::new();
            metadata.insert("ack_for_message_id".to_string(), item.message_id.clone());
            metadata.insert("acknowledged_by".to_string(), acknowledger.to_string());

            let mut context = item.context.clone();
            context.thread_id = Some(item.thread_id.clone());

            let body = if note.trim().is_empty() {
                format!(
                    "{acknowledger} acknowledged coordination message {}.",
                    item.message_id
                )
            } else {
                note.to_string()
            };

            let partial = self.emit_event_at(
                now_ms,
                CoordinationEventRequest {
                    kind: CoordinationEventKind::Acknowledgement,
                    summary: format!("ack {}", item.message_id),
                    body,
                    recipients: vec![reply_to.to_string()],
                    ack_required: false,
                    context,
                    reason_code: "mission.acknowledged".to_string(),
                    error_code: None,
                    metadata,
                },
            );
            report.merge(partial);
        }

        report
    }
}

fn render_subject(mission_id: &str, kind: CoordinationEventKind, summary: &str) -> String {
    let summary = if summary.is_empty() {
        "coordination event"
    } else {
        summary
    };
    format!(
        "[mission:{}][{}] {}",
        sanitize_token(mission_id),
        kind.subject_tag(),
        summary
    )
}

fn render_body(human_body: &str, serialized_envelope: &str) -> String {
    if human_body.is_empty() {
        format!("{COORDINATION_ENVELOPE_MARKER}\n{serialized_envelope}")
    } else {
        format!("{human_body}\n\n{COORDINATION_ENVELOPE_MARKER}\n{serialized_envelope}")
    }
}

fn parse_envelope_from_body(body: &str) -> Result<CoordinationEnvelope, String> {
    let Some(start) = body.rfind(COORDINATION_ENVELOPE_MARKER) else {
        return Err("coordination envelope marker not found".to_string());
    };
    let payload = body[start + COORDINATION_ENVELOPE_MARKER.len()..].trim();
    if payload.is_empty() {
        return Err("coordination envelope payload missing".to_string());
    }
    serde_json::from_str(payload).map_err(|err| format!("invalid coordination envelope: {err}"))
}

fn canonicalize_recipients(recipients: &[String]) -> Vec<String> {
    let mut set = BTreeSet::new();
    for recipient in recipients {
        let trimmed = recipient.trim();
        if !trimmed.is_empty() {
            set.insert(trimmed.to_string());
        }
    }
    set.into_iter().collect()
}

fn sanitize_token(input: &str) -> String {
    let mut token = String::new();
    let mut previous_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            token.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            token.push('-');
            previous_dash = true;
        }
    }
    token.trim_matches('-').to_string()
}

fn fleet_context_from_plan(
    launch_plan: &LaunchPlan,
    correlation_id: &str,
    scenario_id: Option<String>,
) -> MissionCoordinationContext {
    MissionCoordinationContext {
        mission_id: format!(
            "fleet-launch:{}:g{}",
            launch_plan.name, launch_plan.generation
        ),
        fleet_id: Some(launch_plan.name.clone()),
        bead_id: None,
        assignment_id: None,
        thread_id: Some(format!(
            "fleet-launch-{}-g{}",
            sanitize_token(&launch_plan.name),
            launch_plan.generation
        )),
        correlation_id: correlation_id.to_string(),
        scenario_id,
    }
}

fn fleet_context_from_outcome(
    launch_outcome: &LaunchOutcome,
    workspace_id: &str,
    domain: &str,
    generation: u64,
    correlation_id: &str,
    scenario_id: Option<String>,
) -> MissionCoordinationContext {
    MissionCoordinationContext {
        mission_id: format!(
            "fleet-launch:{}:{}:{}:g{}",
            workspace_id, domain, launch_outcome.name, generation
        ),
        fleet_id: Some(launch_outcome.name.clone()),
        bead_id: None,
        assignment_id: None,
        thread_id: Some(format!(
            "fleet-launch-{}-g{}",
            sanitize_token(&launch_outcome.name),
            generation
        )),
        correlation_id: correlation_id.to_string(),
        scenario_id,
    }
}

fn fleet_status_label(status: FleetLaunchStatus) -> &'static str {
    match status {
        FleetLaunchStatus::Complete => "complete",
        FleetLaunchStatus::Partial => "partial",
        FleetLaunchStatus::Failed => "failed",
    }
}

fn startup_strategy_label(strategy: StartupStrategy) -> &'static str {
    match strategy {
        StartupStrategy::Parallel => "parallel",
        StartupStrategy::Sequential => "sequential",
        StartupStrategy::Phased => "phased",
    }
}

#[cfg(feature = "agent-mail")]
impl MissionMailTransport for crate::agent_mail_bridge::AgentMailBridge {
    fn send_message(&self, to: &str, subject: &str, body: &str) -> Result<String, String> {
        self.send_message(to, subject, body)
            .map(|id| id.as_str().to_string())
            .map_err(|err| err.to_string())
    }

    fn fetch_inbox(&self, agent_name: &str) -> Vec<CoordinationInboxMessage> {
        self.fetch_inbox(agent_name)
            .into_iter()
            .map(Into::into)
            .collect()
    }
}

#[cfg(feature = "agent-mail")]
impl From<crate::agent_mail_bridge::MailMessage> for CoordinationInboxMessage {
    fn from(message: crate::agent_mail_bridge::MailMessage) -> Self {
        let ack_required = message
            .extra
            .get("ack_required")
            .and_then(serde_json::Value::as_bool);
        let thread_id = message
            .extra
            .get("thread_id")
            .or_else(|| message.extra.get("thread"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        Self {
            id: message.id,
            from: message.from,
            subject: message.subject,
            body: message.body,
            read: message.read,
            thread_id,
            timestamp: message.timestamp,
            ack_required,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::RefCell;
    use std::collections::HashSet;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SentRecord {
        to: String,
        subject: String,
        body: String,
    }

    #[derive(Default)]
    struct MockTransport {
        sent: RefCell<Vec<SentRecord>>,
        inbox: RefCell<HashMap<String, Vec<CoordinationInboxMessage>>>,
        failing_recipients: RefCell<HashSet<String>>,
        next_id: RefCell<u64>,
    }

    impl MockTransport {
        fn fail_for(&self, recipient: &str) {
            self.failing_recipients
                .borrow_mut()
                .insert(recipient.to_string());
        }

        fn push_inbox(&self, agent_name: &str, message: CoordinationInboxMessage) {
            self.inbox
                .borrow_mut()
                .entry(agent_name.to_string())
                .or_default()
                .push(message);
        }

        fn sent(&self) -> Vec<SentRecord> {
            self.sent.borrow().clone()
        }
    }

    impl MissionMailTransport for MockTransport {
        fn send_message(&self, to: &str, subject: &str, body: &str) -> Result<String, String> {
            if self.failing_recipients.borrow().contains(to) {
                return Err("mock transport failure".to_string());
            }

            let mut next = self.next_id.borrow_mut();
            *next += 1;
            let id = format!("msg-{}", *next);
            self.sent.borrow_mut().push(SentRecord {
                to: to.to_string(),
                subject: subject.to_string(),
                body: body.to_string(),
            });
            Ok(id)
        }

        fn fetch_inbox(&self, agent_name: &str) -> Vec<CoordinationInboxMessage> {
            self.inbox
                .borrow()
                .get(agent_name)
                .cloned()
                .unwrap_or_default()
        }
    }

    fn sample_context() -> MissionCoordinationContext {
        MissionCoordinationContext {
            mission_id: "ft-3681t.3.4".to_string(),
            fleet_id: Some("fleet-alpha".to_string()),
            bead_id: Some("ft-3681t.3.4".to_string()),
            assignment_id: Some("assignment-1".to_string()),
            thread_id: None,
            correlation_id: "corr-123".to_string(),
            scenario_id: Some("scn-001".to_string()),
        }
    }

    #[test]
    fn canonical_thread_id_is_stable_and_sanitized() {
        let context = sample_context();
        assert_eq!(
            context.canonical_thread_id(),
            "mission-ft-3681t-3-4-fleet-alpha-ft-3681t-3-4"
        );
    }

    #[test]
    fn emit_event_deduplicates_recipients_and_tracks_delivery() {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);

        let report = kernel.emit_event_at(
            10_000,
            CoordinationEventRequest {
                kind: CoordinationEventKind::StartNotice,
                summary: "start".to_string(),
                body: "mission started".to_string(),
                recipients: vec![
                    "AgentB".to_string(),
                    "AgentA".to_string(),
                    "AgentB".to_string(),
                    "   ".to_string(),
                ],
                ack_required: true,
                context: sample_context(),
                reason_code: "mission.start_notice".to_string(),
                error_code: None,
                metadata: HashMap::new(),
            },
        );

        assert_eq!(report.attempted, 2);
        assert_eq!(report.delivered.len(), 2);
        assert!(report.failed.is_empty());
        assert!(report.delivered.iter().all(|item| item.ack_required));
        assert!(
            report
                .delivered
                .iter()
                .all(|item| item.ack_deadline_ms == Some(310_000))
        );
    }

    #[test]
    fn emit_event_collects_partial_transport_failures() {
        let transport = MockTransport::default();
        transport.fail_for("AgentB");
        let kernel = MissionAgentMailKernel::new(transport);

        let report = kernel.emit_progress_update_at(
            20_000,
            vec!["AgentA".to_string(), "AgentB".to_string()],
            sample_context(),
            "progress",
            "slice 1 complete",
        );

        assert_eq!(report.attempted, 2);
        assert_eq!(report.delivered.len(), 1);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].recipient, "AgentB");
    }

    #[test]
    fn emit_conflict_signals_maps_deconfliction_payloads() {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let context = sample_context();

        let conflict_report = ConflictDetectionReport {
            cycle_id: 7,
            detected_at_ms: 50_000,
            conflicts: vec![crate::mission_loop::AssignmentConflict {
                conflict_id: "conflict-1".to_string(),
                conflict_type: crate::mission_loop::ConflictType::ConcurrentBeadClaim,
                involved_agents: vec!["AgentA".to_string(), "AgentB".to_string()],
                involved_beads: vec!["ft-3681t.3.4".to_string()],
                conflicting_paths: Vec::new(),
                detected_at_ms: 50_000,
                resolution: crate::mission_loop::ConflictResolution::PendingManualResolution,
                reason_code: "concurrent_bead_claim".to_string(),
                error_code: "FTM2002".to_string(),
            }],
            messages: vec![crate::mission_loop::DeconflictionMessage {
                recipient: "AgentA".to_string(),
                subject: "contention".to_string(),
                body: "Resolve claim collision".to_string(),
                thread_id: "ft-3681t.3.4".to_string(),
                importance: "high".to_string(),
                conflict_id: "conflict-1".to_string(),
            }],
            auto_resolved_count: 0,
            pending_resolution_count: 1,
        };

        let report = kernel.emit_conflict_signals_at(50_010, &context, &conflict_report);
        assert_eq!(report.attempted, 1);
        assert_eq!(report.delivered.len(), 1);

        let sent = kernel.transport.sent();
        assert_eq!(sent.len(), 1);
        let parsed = parse_envelope_from_body(&sent[0].body).expect("envelope must parse");
        assert_eq!(parsed.event_kind, CoordinationEventKind::ContentionSignal);
        assert_eq!(parsed.reason_code, "concurrent_bead_claim");
        assert_eq!(parsed.error_code.as_deref(), Some("FTM2002"));
        assert_eq!(
            parsed.metadata.get("conflict_id").map(String::as_str),
            Some("conflict-1")
        );
    }

    #[test]
    fn consume_inbox_parses_and_reports_invalid_envelopes() {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);

        let envelope = CoordinationEnvelope {
            version: 1,
            event_kind: CoordinationEventKind::StartNotice,
            ack_required: true,
            emitted_at_ms: 1000,
            reason_code: "mission.start_notice".to_string(),
            error_code: None,
            context: sample_context(),
            metadata: HashMap::new(),
        };
        let serialized = serde_json::to_string(&envelope).unwrap();

        kernel.transport.push_inbox(
            "AgentA",
            CoordinationInboxMessage {
                id: Some("m1".to_string()),
                from: Some("AgentLead".to_string()),
                subject: "start".to_string(),
                body: render_body("hello", &serialized),
                read: false,
                thread_id: Some("ft-3681t.3.4".to_string()),
                timestamp: None,
                ack_required: Some(true),
            },
        );
        kernel.transport.push_inbox(
            "AgentA",
            CoordinationInboxMessage {
                id: Some("m2".to_string()),
                from: Some("AgentLead".to_string()),
                subject: "bad".to_string(),
                body: format!("{COORDINATION_ENVELOPE_MARKER}\n{{bad json}}"),
                read: false,
                thread_id: None,
                timestamp: None,
                ack_required: None,
            },
        );

        let report = kernel.consume_inbox("AgentA");
        assert_eq!(report.raw_count, 2);
        assert_eq!(report.parsed.len(), 1);
        assert_eq!(report.parse_failures.len(), 1);
    }

    #[test]
    fn collect_pending_acks_filters_read_and_ack_messages() {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);

        let mut ctx = sample_context();
        ctx.thread_id = Some("ft-3681t.3.4".to_string());
        let mk_msg = |id: &str, kind: CoordinationEventKind, ack_required: bool, read: bool| {
            let envelope = CoordinationEnvelope {
                version: 1,
                event_kind: kind,
                ack_required,
                emitted_at_ms: 5000,
                reason_code: "mission.test".to_string(),
                error_code: None,
                context: ctx.clone(),
                metadata: HashMap::new(),
            };
            let serialized = serde_json::to_string(&envelope).unwrap();
            CoordinationInboxMessage {
                id: Some(id.to_string()),
                from: Some("Leader".to_string()),
                subject: "coordination".to_string(),
                body: render_body("body", &serialized),
                read,
                thread_id: Some("ft-3681t.3.4".to_string()),
                timestamp: None,
                ack_required: Some(ack_required),
            }
        };

        kernel.transport.push_inbox(
            "AgentA",
            mk_msg("m1", CoordinationEventKind::StartNotice, true, false),
        );
        kernel.transport.push_inbox(
            "AgentA",
            mk_msg("m2", CoordinationEventKind::Handoff, true, true),
        );
        kernel.transport.push_inbox(
            "AgentA",
            mk_msg("m3", CoordinationEventKind::ProgressUpdate, false, false),
        );
        kernel.transport.push_inbox(
            "AgentA",
            mk_msg("m4", CoordinationEventKind::Acknowledgement, true, false),
        );

        let pending = kernel.collect_pending_acknowledgements_at("AgentA", 305_100);
        assert_eq!(pending.pending.len(), 1);
        assert_eq!(pending.pending[0].message_id, "m1");
        assert!(pending.pending[0].overdue);
    }

    #[test]
    fn emit_acknowledgements_routes_to_original_sender() {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let context = sample_context();
        let pending = vec![PendingAcknowledgement {
            message_id: "m-123".to_string(),
            reply_to: Some("Leader".to_string()),
            thread_id: "ft-3681t.3.4".to_string(),
            deadline_ms: 10_000,
            overdue: false,
            context,
        }];

        let report = kernel.emit_acknowledgements_at(9_000, "AgentA", &pending, "ack");
        assert_eq!(report.delivered.len(), 1);
        assert!(report.failed.is_empty());

        let sent = kernel.transport.sent();
        assert_eq!(sent.len(), 1);
        let parsed = parse_envelope_from_body(&sent[0].body).expect("envelope parse");
        assert_eq!(parsed.event_kind, CoordinationEventKind::Acknowledgement);
        assert_eq!(
            parsed
                .metadata
                .get("ack_for_message_id")
                .map(String::as_str),
            Some("m-123")
        );
    }

    #[test]
    fn emit_acknowledgements_reports_missing_sender() {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let pending = vec![PendingAcknowledgement {
            message_id: "m-404".to_string(),
            reply_to: None,
            thread_id: "ft-3681t.3.4".to_string(),
            deadline_ms: 10_000,
            overdue: false,
            context: sample_context(),
        }];

        let report = kernel.emit_acknowledgements_at(9_000, "AgentA", &pending, "");
        assert!(report.delivered.is_empty());
        assert_eq!(report.failed.len(), 1);
    }

    fn sample_launch_plan() -> LaunchPlan {
        LaunchPlan {
            name: "fleet-alpha".to_string(),
            slots: Vec::new(),
            layout_template: None,
            strategy: crate::fleet_launcher::StartupStrategy::Phased,
            phases: Vec::new(),
            generation: 7,
            workspace_id: "ws-1".to_string(),
            domain: "local".to_string(),
            planned_at: 42,
            warnings: Vec::new(),
        }
    }

    fn sample_launch_outcome(status: FleetLaunchStatus) -> LaunchOutcome {
        LaunchOutcome {
            name: "fleet-alpha".to_string(),
            slot_outcomes: Vec::new(),
            status,
            registry_snapshot: Vec::new(),
            completed_at: 55,
            total_slots: 8,
            successful_slots: 6,
            failed_slots: 2,
            pre_launch_checkpoint: None,
            bootstrap_dispatches: Vec::new(),
        }
    }

    #[test]
    fn emit_fleet_launch_start_notice_includes_fleet_metadata() {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let plan = sample_launch_plan();

        let report = kernel.emit_fleet_launch_start_notice_at(
            1_000,
            vec!["AgentA".to_string()],
            &plan,
            "corr-fleet-1",
            Some("scenario-a".to_string()),
        );

        assert_eq!(report.delivered.len(), 1);
        let sent = kernel.transport.sent();
        let envelope = parse_envelope_from_body(&sent[0].body).expect("parse envelope");
        assert_eq!(envelope.reason_code, "mission.fleet_launch_start");
        assert_eq!(envelope.context.fleet_id.as_deref(), Some("fleet-alpha"));
        assert_eq!(
            envelope.metadata.get("planned_slots").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            envelope.metadata.get("generation").map(String::as_str),
            Some("7")
        );
        assert_eq!(
            envelope.metadata.get("strategy").map(String::as_str),
            Some("phased")
        );
    }

    #[test]
    fn emit_fleet_launch_outcome_notice_labels_status() {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let outcome = sample_launch_outcome(FleetLaunchStatus::Partial);

        let report = kernel.emit_fleet_launch_outcome_notice_at(
            2_000,
            vec!["AgentA".to_string()],
            &outcome,
            "ws-1",
            "local",
            7,
            "corr-fleet-2",
            Some("scenario-b".to_string()),
        );

        assert_eq!(report.delivered.len(), 1);
        let sent = kernel.transport.sent();
        let envelope = parse_envelope_from_body(&sent[0].body).expect("parse envelope");
        assert_eq!(envelope.reason_code, "mission.fleet_launch_outcome");
        assert_eq!(
            envelope.metadata.get("status").map(String::as_str),
            Some("partial")
        );
        assert_eq!(
            envelope
                .metadata
                .get("successful_slots")
                .map(String::as_str),
            Some("6")
        );
    }

    #[test]
    fn emit_fleet_launch_progress_update_includes_strategy_metadata() {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let plan = sample_launch_plan();

        let report = kernel.emit_fleet_launch_progress_update_at(
            1_500,
            vec!["AgentA".to_string()],
            &plan,
            1,
            2,
            "corr-fleet-3",
            Some("scenario-c".to_string()),
        );

        assert_eq!(report.delivered.len(), 1);
        let sent = kernel.transport.sent();
        let envelope = parse_envelope_from_body(&sent[0].body).expect("parse envelope");
        assert_eq!(envelope.reason_code, "mission.fleet_launch_progress");
        assert_eq!(
            envelope.metadata.get("strategy").map(String::as_str),
            Some("phased")
        );
        assert_eq!(
            envelope.metadata.get("phase_index").map(String::as_str),
            Some("1")
        );
    }

    // ========================================================================
    // canonical_thread_id edge cases
    // ========================================================================

    #[test]
    fn canonical_thread_id_uses_explicit_when_provided() {
        let ctx = MissionCoordinationContext {
            mission_id: "m-1".to_string(),
            fleet_id: Some("fleet-1".to_string()),
            bead_id: Some("bead-1".to_string()),
            assignment_id: None,
            thread_id: Some("custom-thread".to_string()),
            correlation_id: "corr-1".to_string(),
            scenario_id: None,
        };
        assert_eq!(ctx.canonical_thread_id(), "custom-thread");
    }

    #[test]
    fn canonical_thread_id_trims_whitespace() {
        let ctx = MissionCoordinationContext {
            mission_id: "m-1".to_string(),
            fleet_id: None,
            bead_id: None,
            assignment_id: None,
            thread_id: Some("  padded-thread  ".to_string()),
            correlation_id: "corr-1".to_string(),
            scenario_id: None,
        };
        assert_eq!(ctx.canonical_thread_id(), "padded-thread");
    }

    #[test]
    fn canonical_thread_id_ignores_empty_explicit() {
        let ctx = MissionCoordinationContext {
            mission_id: "m-2".to_string(),
            fleet_id: None,
            bead_id: None,
            assignment_id: None,
            thread_id: Some("   ".to_string()),
            correlation_id: "corr-2".to_string(),
            scenario_id: None,
        };
        // Empty/whitespace-only thread_id falls back to derived
        assert!(ctx.canonical_thread_id().starts_with("mission-"));
        assert!(ctx.canonical_thread_id().contains("m-2"));
    }

    #[test]
    fn canonical_thread_id_includes_fleet_and_bead() {
        let ctx = MissionCoordinationContext {
            mission_id: "m-3".to_string(),
            fleet_id: Some("fleet-A".to_string()),
            bead_id: Some("ft-123".to_string()),
            assignment_id: None,
            thread_id: None,
            correlation_id: "corr-3".to_string(),
            scenario_id: None,
        };
        let tid = ctx.canonical_thread_id();
        assert!(tid.starts_with("mission-"));
        assert!(tid.contains("fleet"));
        assert!(tid.contains("ft-123") || tid.contains("ft_123"));
    }

    #[test]
    fn canonical_thread_id_mission_only() {
        let ctx = MissionCoordinationContext {
            mission_id: "solo-mission".to_string(),
            fleet_id: None,
            bead_id: None,
            assignment_id: None,
            thread_id: None,
            correlation_id: "corr-4".to_string(),
            scenario_id: None,
        };
        let tid = ctx.canonical_thread_id();
        assert!(tid.starts_with("mission-"));
        assert!(tid.contains("solo"));
    }

    // ========================================================================
    // Type serde roundtrips
    // ========================================================================

    #[test]
    fn coordination_event_kind_serde_roundtrip() {
        let kinds = [
            CoordinationEventKind::StartNotice,
            CoordinationEventKind::ProgressUpdate,
            CoordinationEventKind::Handoff,
            CoordinationEventKind::ContentionSignal,
            CoordinationEventKind::Acknowledgement,
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let back: CoordinationEventKind = serde_json::from_str(&json).unwrap();
            assert_eq!(*kind, back);
        }
    }

    #[test]
    fn mission_agent_mail_config_serde_roundtrip() {
        let config = MissionAgentMailConfig {
            ack_timeout_ms: 30_000,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: MissionAgentMailConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ack_timeout_ms, 30_000);
    }

    #[test]
    fn mission_agent_mail_config_default() {
        let config = MissionAgentMailConfig::default();
        assert!(
            config.ack_timeout_ms > 0,
            "default ack_timeout_ms must be positive"
        );
    }

    #[test]
    fn coordination_context_serde_roundtrip() {
        let ctx = MissionCoordinationContext {
            mission_id: "m-serde".to_string(),
            fleet_id: Some("fleet-1".to_string()),
            bead_id: Some("ft-abc".to_string()),
            assignment_id: None,
            thread_id: None,
            correlation_id: "corr-serde".to_string(),
            scenario_id: Some("test-scenario".to_string()),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let back: MissionCoordinationContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mission_id, "m-serde");
        assert_eq!(back.fleet_id.as_deref(), Some("fleet-1"));
        assert_eq!(back.bead_id.as_deref(), Some("ft-abc"));
        assert!(back.assignment_id.is_none());
        assert!(back.thread_id.is_none());
        assert_eq!(back.scenario_id.as_deref(), Some("test-scenario"));
    }
}
