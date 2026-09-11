//! Connector host runtime lifecycle and protocol envelopes.
//!
//! The host-runtime model uses caller-provided timestamps and performs no
//! transport I/O. The separate FCP client and durable delivery session own
//! bounded network/storage work; model admission alone never invokes a provider.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use thiserror::Error;

const TRANSITION_HISTORY_CAPACITY: usize = 64;
const SANDBOX_DECISION_HISTORY_CAPACITY: usize = 128;

#[derive(Debug, Error)]
pub(crate) enum FcpDeliveryError {
    #[error("{0}")]
    State(&'static str),
    #[error("connector_storage_failed")]
    Storage(#[from] crate::Error),
    #[error("connector_transport_failed")]
    Transport(#[from] FcpTransportError),
    #[error("connector_payload_invalid")]
    Payload(#[from] serde_json::Error),
    #[error("connector_clock_invalid")]
    Clock(#[from] std::time::SystemTimeError),
    #[error("connector_integer_out_of_range")]
    Integer(#[from] std::num::TryFromIntError),
}

type DeliveryResult<T> = Result<T, FcpDeliveryError>;

/// One runtime-owned durable transport loop. A dispatched row is never
/// automatically replayed, including after process restart or receipt loss.
pub(crate) struct FcpDeliverySession {
    config: FcpTransportConfig,
    client: FcpHostClient,
    storage: crate::storage::StorageHandle,
    generation: String,
    cursor: i64,
    cursor_epoch: String,
    bridge: crate::connector_outbound_bridge::ConnectorOutboundBridge,
    inbound: crate::connector_inbound_bridge::ConnectorInboundBridge,
    event_bus: std::sync::Arc<crate::events::EventBus>,
    next_poll: Vec<std::time::Instant>,
    poll_cursor: usize,
    pending_admission: Option<(i64, Vec<crate::connector_reliability::ConnectorOutboxEntry>)>,
}

impl FcpDeliverySession {
    pub(crate) async fn open(
        cx: &crate::cx::Cx,
        safety: &crate::config::SafetyConfig,
        storage: crate::storage::StorageHandle,
        event_bus: std::sync::Arc<crate::events::EventBus>,
    ) -> DeliveryResult<Self> {
        use crate::connector_reliability::{
            ConnectorStorageMutation as Mutation, ConnectorStorageOutcome as Outcome,
        };
        let config = safety
            .connector_transport
            .clone()
            .ok_or(FcpTransportError::InvalidConfig)?;
        let client = FcpHostClient::new(config.clone(), &safety.connector_host_runtime)?;
        // Host/zone policy is part of the persisted authority, not only routes.
        let generation = fcp_identity_hash(&format!(
            "{}:{}",
            config.generation()?,
            serde_json::to_string(safety)?
        ));
        let Outcome::Cursor {
            after_event_id,
            cursor_epoch,
        } = storage
            .connector_mutation_with_cx(
                cx,
                Mutation::Initialize {
                    generation: generation.clone(),
                    now_ms: delivery_now_ms()?,
                },
            )
            .await?
        else {
            return Err(FcpDeliveryError::State("connector_cursor_unavailable"));
        };
        let mut bridge =
            crate::connector_outbound_bridge::ConnectorOutboundBridge::new(Default::default());
        bridge.set_policy_engine(crate::policy::PolicyEngine::from_safety_config(safety));
        for route in &config.outbound {
            bridge.add_rule(route.rule.clone());
            bridge.register_sandbox_zone(
                &route.invoke.connector_id,
                safety.connector_host_runtime.sandbox.clone(),
            );
        }
        let inbound = crate::connector_inbound_bridge::ConnectorInboundBridge::new(
            std::sync::Arc::clone(&event_bus),
            crate::connector_inbound_bridge::ConnectorInboundBridgeConfig {
                classifier: safety.data_classifier.clone(),
                ..Default::default()
            },
        );
        let next_poll = vec![std::time::Instant::now(); config.inbound.len()];
        Ok(Self {
            config,
            client,
            storage,
            generation,
            cursor: after_event_id,
            cursor_epoch,
            bridge,
            inbound,
            event_bus,
            next_poll,
            poll_cursor: 0,
            pending_admission: None,
        })
    }

    pub(crate) async fn tick(&mut self, cx: &crate::cx::Cx) -> DeliveryResult<()> {
        self.deliver_ingress(cx).await?;
        self.dispatch_pending(cx).await?;
        self.admit_events(cx).await?;
        for offset in 0..self.config.inbound.len() {
            let index = (self.poll_cursor + offset) % self.config.inbound.len();
            cx.checkpoint().map_err(|_| FcpTransportError::Cancelled)?;
            if std::time::Instant::now() >= self.next_poll[index] {
                self.next_poll[index] = std::time::Instant::now()
                    + std::time::Duration::from_millis(self.config.inbound[index].poll_interval_ms);
                self.poll_cursor = (index + 1) % self.config.inbound.len();
                self.poll_ingress(cx, index).await?;
                // At most one provider poll per tick. A large subscription
                // set cannot monopolize the task for N network deadlines.
                break;
            }
        }
        self.deliver_ingress(cx).await
    }

    async fn admit_events(&mut self, cx: &crate::cx::Cx) -> DeliveryResult<()> {
        use crate::connector_outbound_bridge::{
            OutboundEvent, OutboundEventSource, OutboundSeverity,
        };
        use crate::connector_reliability::{ConnectorDeliveryState as State, ConnectorOutboxEntry};
        if !self.flush_admission(cx).await? {
            return Ok(());
        }
        let page = self
            .storage
            .get_events_stream_page_with_cx(
                cx,
                crate::storage::EventStreamQuery {
                    after_id: Some(self.cursor),
                    limit: Some(16),
                    ..Default::default()
                },
            )
            .await?;
        let through = page
            .events
            .last()
            .map_or(page.retention.max_event_id, |event| event.id);
        let retention = self
            .storage
            .check_event_retention_in_epoch_with_cx(cx, self.cursor, through, &self.cursor_epoch)
            .await?;
        if !matches!(
            retention.status,
            crate::storage::EventRetentionStatus::CompleteNoPruning
        ) {
            return Err(FcpDeliveryError::State(
                "connector_source_history_unavailable",
            ));
        }
        for event in page.events {
            let now_ms = delivery_now_ms()?;
            let mut outbound = OutboundEvent {
                source: OutboundEventSource::PatternDetected,
                event_type: format!("pattern.{}", event.event_type),
                // Durable cursor CAS owns deduplication. Volatile bridge dedup
                // must not suppress a transaction retried after saturation.
                correlation_id: None,
                timestamp_ms: u64::try_from(now_ms)?,
                pane_id: Some(event.pane_id),
                workflow_id: None,
                payload: serde_json::Value::Null,
                severity: match event.severity.as_str() {
                    "critical" => OutboundSeverity::Critical,
                    "warning" => OutboundSeverity::Warning,
                    _ => OutboundSeverity::Info,
                },
            };
            let mut entries = Vec::new();
            for route in &self.config.outbound {
                if !route.rule.matches(&outbound) {
                    continue;
                }
                let key = fcp_identity_hash(&serde_json::to_string(&(
                    &self.generation,
                    event.id,
                    &route.rule.rule_id,
                ))?);
                let correlation_id = delivery_request_id();
                let mut invoke = route.invoke.clone();
                if let Some(pointer) = &route.correlation_input_pointer {
                    *invoke
                        .input
                        .pointer_mut(pointer)
                        .ok_or(FcpTransportError::InvalidConfig)? =
                        serde_json::Value::String(correlation_id.clone());
                }
                // Classify the exact configured bytes to be sent. Neither
                // raw captured text nor an unrelated route's approval can
                // authorize this invocation.
                outbound.payload = serde_json::json!({
                    "input": invoke.input, "capability": invoke.capability.as_str(),
                    "target": invoke.target,
                });
                let admission = self
                    .bridge
                    .process_event_for_rule(&outbound, Some(&route.rule.rule_id))
                    .map_err(|_| FcpDeliveryError::State("connector_policy_admission_failed"))?;
                let planned = self.bridge.drain_actions();
                let approved_input = planned
                    .first()
                    .and_then(|action| action.params.get("input"));
                let binding_preserved = planned.first().is_some_and(|action| {
                    action.params.get("target") == Some(&serde_json::json!(invoke.target))
                        && action
                            .params
                            .get("capability")
                            .and_then(serde_json::Value::as_str)
                            == Some(invoke.capability.as_str())
                });
                let admitted = binding_preserved
                    && planned.len() == 1
                    && approved_input.is_some()
                    && admission
                        .actions_dispatched
                        .iter()
                        .any(|action| action.rule_id == route.rule.rule_id);
                // Rejected input is not retained in the durable record. In
                // particular, privacy-denied credentials must not enter SQLite.
                invoke.input = if admitted {
                    approved_input.cloned().unwrap_or_default()
                } else {
                    serde_json::Value::Null
                };
                if !admitted {
                    invoke.target = None;
                }
                let mut entry = ConnectorOutboxEntry {
                    key,
                    generation: self.generation.clone(),
                    source_event_id: event.id,
                    rule_id: route.rule.rule_id.clone(),
                    invoke,
                    action_kind: route.rule.action_kind,
                    correlation_id,
                    state: if admitted {
                        State::Admitted
                    } else {
                        State::Rejected
                    },
                    revision: 0,
                    due_at_ms: now_ms,
                    created_at_ms: now_ms,
                    updated_at_ms: now_ms,
                    receipt_id: None,
                    receipt_hash: None,
                    reason_code: (!admitted).then(|| "connector_admission_denied".to_string()),
                };
                // Admission is one atomic cursor transaction. Reserve an equal
                // share for every configured route, plus JSON-array and writer
                // overhead, so a large/redaction-expanded input cannot wedge
                // the cursor permanently. Leave room in each persisted row for
                // its later receipt and revision updates as well.
                let entry_budget = ((4 * 1024 * 1024 - 2048) / self.config.outbound.len().max(1))
                    .min(1024 * 1024 - 4096);
                if serde_json::to_vec(&entry)?.len() > entry_budget {
                    entry.state = State::Rejected;
                    entry.invoke.input = serde_json::Value::Null;
                    entry.invoke.target = None;
                    entry.reason_code = Some("connector_admission_payload_too_large".into());
                }
                entries.push(entry);
            }
            self.pending_admission = Some((event.id, entries));
            if !self.flush_admission(cx).await? {
                return Ok(());
            }
        }
        Ok(())
    }

    async fn flush_admission(&mut self, cx: &crate::cx::Cx) -> DeliveryResult<bool> {
        use crate::connector_reliability::{
            ConnectorStorageMutation as Mutation, ConnectorStorageOutcome as Outcome,
        };
        let Some((through_event_id, entries)) = &self.pending_admission else {
            return Ok(true);
        };
        match self
            .storage
            .connector_mutation_with_cx(
                cx,
                Mutation::Admit {
                    generation: self.generation.clone(),
                    expected_cursor: self.cursor,
                    through_event_id: *through_event_id,
                    entries: entries.clone(),
                    max_pending: self.config.max_pending_actions,
                    max_retained: self.config.max_retained_actions,
                },
            )
            .await?
        {
            Outcome::Admitted => {
                self.cursor = *through_event_id;
                self.pending_admission = None;
            }
            Outcome::Saturated => return Ok(false),
            _ => return Err(FcpDeliveryError::State("connector_admission_conflict")),
        }
        Ok(true)
    }

    async fn transition(
        &self,
        cx: &crate::cx::Cx,
        entry: &crate::connector_reliability::ConnectorOutboxEntry,
        next_state: crate::connector_reliability::ConnectorDeliveryState,
        receipt: Option<&FcpAcknowledgement>,
        reason: Option<&str>,
    ) -> DeliveryResult<crate::connector_reliability::ConnectorOutboxEntry> {
        use crate::connector_reliability::{
            ConnectorStorageMutation as Mutation, ConnectorStorageOutcome as Outcome,
        };
        match self
            .storage
            .connector_mutation_with_cx(
                cx,
                Mutation::Transition {
                    key: entry.key.clone(),
                    expected_revision: entry.revision,
                    expected_state: entry.state,
                    next_state,
                    now_ms: delivery_now_ms()?.max(entry.updated_at_ms),
                    receipt_id: receipt.map(|ack| ack.receipt_id.clone()),
                    receipt_hash: receipt.map(|ack| ack.receipt_hash.clone()),
                    reason_code: reason.map(str::to_string),
                },
            )
            .await?
        {
            Outcome::Transitioned(entry) => Ok(*entry),
            _ => Err(FcpDeliveryError::State(
                "connector_dispatch_ownership_conflict",
            )),
        }
    }

    async fn dispatch_pending(&mut self, cx: &crate::cx::Cx) -> DeliveryResult<()> {
        use crate::connector_reliability::ConnectorDeliveryState as State;
        let pending = self
            .storage
            .dispatchable_connector_outbox_with_cx(cx, &self.generation, 1)
            .await?;
        for entry in pending {
            cx.checkpoint().map_err(|_| FcpTransportError::Cancelled)?;
            if entry.state == State::Dispatched {
                // Another owner may still be settling this exact request.
                // Preserve its CAS revision and never resend. Dispatched is
                // explicitly unresolved, not successful or safe to retry.
                continue;
            }
            if entry.due_at_ms > delivery_now_ms()? {
                continue;
            }
            let request = ConnectorOperationRequest {
                action: entry.invoke.operation.clone(),
                correlation_id: entry.correlation_id.clone(),
                capability: entry.invoke.capability,
                target: entry.invoke.target.clone(),
            };
            if self
                .bridge
                .policy_engine()
                .connector_host_runtime()
                .validate_operation_request(&request)
                .is_err()
            {
                self.transition(
                    cx,
                    &entry,
                    State::Rejected,
                    None,
                    Some("connector_sandbox_denied"),
                )
                .await?;
                continue;
            }
            let deadline = self.client.operation_deadline()?;
            let preflight = async {
                self.client
                    .observe_operation(cx, deadline, &entry.invoke)
                    .await?;
                self.client
                    .prepare_invocation(
                        cx,
                        deadline,
                        &entry.invoke,
                        &entry.correlation_id,
                        &entry.key,
                    )
                    .await
            }
            .await;
            let prepared = match preflight {
                Ok(prepared) => prepared,
                Err(FcpTransportError::Cancelled) => {
                    return Err(FcpTransportError::Cancelled.into());
                }
                Err(FcpTransportError::Unavailable | FcpTransportError::CredentialUnavailable) => {
                    use crate::connector_reliability::{
                        ConnectorStorageMutation as Mutation, ConnectorStorageOutcome as Outcome,
                    };
                    let now_ms = delivery_now_ms()?.max(entry.updated_at_ms);
                    let delay = 1000_i64 << u32::try_from(entry.revision.clamp(0, 8))?;
                    let due_at_ms = now_ms
                        .checked_add(delay)
                        .ok_or(FcpDeliveryError::State("connector_retry_clock_overflow"))?;
                    if !matches!(
                        self.storage
                            .connector_mutation_with_cx(
                                cx,
                                Mutation::Reschedule {
                                    key: entry.key.clone(),
                                    expected_revision: entry.revision,
                                    now_ms,
                                    due_at_ms,
                                }
                            )
                            .await?,
                        Outcome::Transitioned(_)
                    ) {
                        return Err(FcpDeliveryError::State(
                            "connector_preflight_ownership_conflict",
                        ));
                    }
                    continue;
                }
                Err(_) => {
                    self.transition(
                        cx,
                        &entry,
                        State::Rejected,
                        None,
                        Some("connector_preflight_rejected"),
                    )
                    .await?;
                    continue;
                }
            };
            let dispatched = self
                .transition(cx, &entry, State::Dispatched, None, None)
                .await?;
            match self.client.invoke(cx, deadline, prepared).await {
                Ok(ack) => {
                    self.transition(
                        cx,
                        &dispatched,
                        if ack.success {
                            State::Completed
                        } else {
                            State::Failed
                        },
                        Some(&ack),
                        None,
                    )
                    .await?;
                    let action = delivery_action(&dispatched);
                    if ack.success {
                        self.bridge
                            .record_action_success(&action, u64::try_from(delivery_now_ms()?)?);
                    } else {
                        self.bridge.record_action_failure(
                            &action,
                            "connector_provider_failed",
                            crate::connector_reliability::ConnectorErrorKind::Permanent,
                            u64::try_from(delivery_now_ms()?)?,
                        );
                    }
                }
                Err(_) => {
                    // If cancellation prevents this settlement, the persisted
                    // Dispatched state still forbids replay after restart.
                    self.transition(
                        cx,
                        &dispatched,
                        State::Indeterminate,
                        None,
                        Some("connector_receipt_unconfirmed"),
                    )
                    .await?;
                    self.bridge.record_action_failure(
                        &delivery_action(&dispatched),
                        "connector_receipt_unconfirmed",
                        crate::connector_reliability::ConnectorErrorKind::Indeterminate,
                        u64::try_from(delivery_now_ms()?)?,
                    );
                }
            }
        }
        Ok(())
    }

    async fn poll_ingress(&mut self, cx: &crate::cx::Cx, index: usize) -> DeliveryResult<()> {
        use crate::connector_inbound_bridge::{ConnectorSignal, ConnectorSignalKind};
        use crate::connector_reliability::{
            ConnectorStorageMutation as Mutation, ConnectorStorageOutcome as Outcome,
        };
        let subscription = &self.config.inbound[index];
        let key = fcp_identity_hash(&serde_json::to_string(&(
            &self.generation,
            &subscription.subscription_id,
        ))?);
        let cursor = self
            .storage
            .connector_ingress_cursor_with_cx(cx, &key)
            .await?;
        let mut operation = subscription.invoke.clone();
        *operation
            .input
            .pointer_mut(&subscription.cursor_input_pointer)
            .ok_or(FcpTransportError::InvalidConfig)? = cursor.into();
        let request_id = delivery_request_id();
        let request = ConnectorOperationRequest {
            action: operation.operation.clone(),
            correlation_id: request_id.clone(),
            capability: operation.capability,
            target: operation.target.clone(),
        };
        self.bridge
            .policy_engine()
            .connector_host_runtime()
            .validate_operation_request(&request)
            .map_err(|_| FcpDeliveryError::State("connector_ingress_sandbox_denied"))?;
        let deadline = self.client.operation_deadline()?;
        self.client
            .observe_operation(cx, deadline, &operation)
            .await?;
        let prepared = self
            .client
            .prepare_invocation(
                cx,
                deadline,
                &operation,
                &request_id,
                &fcp_identity_hash(&request_id),
            )
            .await?;
        let ack = self.client.invoke(cx, deadline, prepared).await?;
        if !ack.success {
            return Err(FcpDeliveryError::State("connector_ingress_poll_failed"));
        }
        let rows = ack
            .result
            .as_ref()
            .and_then(|result| result.pointer(&subscription.records_pointer))
            .and_then(serde_json::Value::as_array)
            .ok_or(FcpTransportError::ProtocolInvalid)?;
        if rows.len() > self.config.max_ingress_batch {
            return Err(FcpDeliveryError::State("connector_ingress_batch_limit"));
        }
        let mut records = Vec::with_capacity(rows.len());
        let mut previous = cursor;
        for row in rows {
            let id = row
                .pointer(&subscription.identity_pointer)
                .and_then(serde_json::Value::as_i64)
                .ok_or(FcpTransportError::ProtocolInvalid)?;
            if id <= previous {
                return Err(FcpDeliveryError::State(
                    "connector_ingress_cursor_not_increasing",
                ));
            }
            previous = id;
            let signal = ConnectorSignal::new(
                &operation.connector_id,
                ConnectorSignalKind::Poll,
                row.clone(),
            )
            .with_pane_id(subscription.pane_id)
            .with_correlation_id(format!("{key}:{id}"));
            let detection = self
                .inbound
                .prepare_signal(&signal)
                .map_err(|_| FcpDeliveryError::State("connector_ingress_classification_denied"))?;
            records.push((
                id,
                crate::storage::StoredEvent {
                    id: 0,
                    pane_id: subscription.pane_id,
                    rule_id: detection.rule_id,
                    agent_type: detection.agent_type.to_string(),
                    event_type: detection.event_type,
                    severity: serde_json::to_value(detection.severity)?
                        .as_str()
                        .ok_or(FcpTransportError::ProtocolInvalid)?
                        .to_string(),
                    confidence: detection.confidence,
                    extracted: Some(detection.extracted),
                    matched_text: Some(detection.matched_text),
                    segment_id: None,
                    detected_at: delivery_now_ms()?,
                    dedupe_key: Some(format!("connector:{key}:{id}")),
                    handled_at: None,
                    handled_by_workflow_id: None,
                    handled_status: None,
                },
            ));
        }
        if records.is_empty() {
            return Ok(());
        }
        let count = records.len();
        match self
            .storage
            .connector_mutation_with_cx(
                cx,
                Mutation::Ingest {
                    subscription_key: key,
                    expected_cursor: cursor,
                    records,
                },
            )
            .await?
        {
            Outcome::Ingested(outcomes) => {
                if outcomes.len() != count || outcomes.iter().any(|outcome| outcome.event_id() <= 0)
                {
                    return Err(FcpDeliveryError::State("connector_ingress_invalid_commit"));
                }
            }
            Outcome::Saturated => {}
            _ => return Err(FcpDeliveryError::State("connector_ingress_commit_conflict")),
        }
        Ok(())
    }

    async fn deliver_ingress(&self, cx: &crate::cx::Cx) -> DeliveryResult<()> {
        use crate::connector_reliability::{
            ConnectorStorageMutation as Mutation, ConnectorStorageOutcome as Outcome,
        };
        let pending = self.storage.pending_connector_ingress_with_cx(cx).await?;
        let mut delivered = Vec::new();
        for event in pending {
            cx.checkpoint().map_err(|_| FcpTransportError::Cancelled)?;
            let detection = crate::patterns::Detection {
                rule_id: event.rule_id,
                agent_type: crate::patterns::AgentType::Unknown,
                event_type: event.event_type,
                severity: serde_json::from_value(event.severity.into())?,
                confidence: event.confidence,
                extracted: event.extracted.unwrap_or_default(),
                matched_text: event.matched_text.unwrap_or_default(),
                span: (0, 0),
            };
            if self
                .event_bus
                .publish(crate::events::Event::PatternDetected {
                    pane_id: event.pane_id,
                    pane_uuid: None,
                    detection,
                    event_id: Some(event.id),
                })
                > 0
            {
                delivered.push(event.id);
            }
        }
        if !delivered.is_empty() {
            if !matches!(
                self.storage
                    .connector_mutation_with_cx(
                        cx,
                        Mutation::AcknowledgeIngress {
                            event_ids: delivered
                        }
                    )
                    .await?,
                Outcome::IngressAcknowledged
            ) {
                return Err(FcpDeliveryError::State("connector_ingress_ack_conflict"));
            }
        }
        Ok(())
    }
}

fn delivery_now_ms() -> DeliveryResult<i64> {
    Ok(i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis(),
    )?)
}

fn delivery_action(
    entry: &crate::connector_reliability::ConnectorOutboxEntry,
) -> crate::connector_outbound_bridge::ConnectorAction {
    crate::connector_outbound_bridge::ConnectorAction {
        target_connector: entry.invoke.connector_id.clone(),
        action_kind: entry.action_kind,
        correlation_id: entry.correlation_id.clone(),
        params: entry.invoke.input.clone(),
        created_at_ms: entry.created_at_ms.try_into().unwrap_or_default(),
    }
}

fn delivery_request_id() -> String {
    let mut bytes: [u8; 16] = rand::random();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Operator-owned operation binding. Event payloads cannot select an operation,
/// capability, destination, or credential source.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FcpOperation {
    pub connector_id: String,
    pub operation: String,
    pub capability: ConnectorCapability,
    pub target: Option<String>,
    pub input: serde_json::Value,
}

impl std::fmt::Debug for FcpOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FcpOperation")
            .field("identity_hash", &fcp_identity_hash(&self.operation))
            .field("capability", &self.capability)
            .finish_non_exhaustive()
    }
}

/// A persisted routing rule plus its trusted FCP operation binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FcpOutboundRoute {
    pub rule: crate::connector_outbound_bridge::OutboundRoutingRule,
    pub invoke: FcpOperation,
    /// Existing input slot receiving the stable source correlation string.
    /// For example, `/params/0` can bind a SQL uniqueness key.
    pub correlation_input_pointer: Option<String>,
}

/// Bounded service polling through the same authenticated host invocation path.
/// Rows must have strictly increasing positive integer identities. The durable
/// cursor is substituted into an existing operation-input slot before each poll.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FcpInboundSubscription {
    pub subscription_id: String,
    pub invoke: FcpOperation,
    pub pane_id: u64,
    pub records_pointer: String,
    pub identity_pointer: String,
    pub cursor_input_pointer: String,
    pub poll_interval_ms: u64,
}

/// Explicit local-host transport. Credentials are opaque regular files, never
/// serialized into an outbox, response, diagnostic, or configuration dump.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FcpTransportConfig {
    pub endpoint: String,
    pub capability_token_file: std::path::PathBuf,
    pub admin_token_file: std::path::PathBuf,
    pub request_timeout_ms: u64,
    pub max_payload_bytes: usize,
    pub max_pending_actions: usize,
    pub max_retained_actions: usize,
    pub max_ingress_batch: usize,
    #[serde(default)]
    pub outbound: Vec<FcpOutboundRoute>,
    #[serde(default)]
    pub inbound: Vec<FcpInboundSubscription>,
}

impl std::fmt::Debug for FcpTransportConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FcpTransportConfig")
            .field("endpoint_hash", &fcp_identity_hash(&self.endpoint))
            .field("outbound_rules", &self.outbound.len())
            .field("inbound_subscriptions", &self.inbound.len())
            .finish_non_exhaustive()
    }
}

/// Finite, content-free protocol errors. After dispatch every one of these
/// means an uncertain effect; a network error does not authorize a retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FcpTransportError {
    #[error("connector_transport_invalid_config")]
    InvalidConfig,
    #[error("connector_transport_credential_unavailable")]
    CredentialUnavailable,
    #[error("connector_transport_cancelled")]
    Cancelled,
    #[error("connector_transport_unavailable")]
    Unavailable,
    #[error("connector_transport_payload_limit")]
    PayloadLimit,
    #[error("connector_transport_protocol_invalid")]
    ProtocolInvalid,
    #[error("connector_transport_receipt_unconfirmed")]
    ReceiptUnconfirmed,
}

pub(crate) fn fcp_identity_hash(value: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(value.as_bytes()))
}

fn valid_fcp_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-/".contains(&byte))
}

// This client uses one identity for both the string-valued request ID and the
// host's UUID-valued correlation ID. Accept the hyphenated UUID representation
// only, so every accepted identity can deserialize as both upstream types.
fn valid_fcp_request_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn validate_fcp_introspection(
    response: &serde_json::Value,
    operation: &FcpOperation,
) -> Result<(), FcpTransportError> {
    // FCP IntrospectionResponse wraps its operation descriptors in tools, and
    // ToolDescriptor names are OperationInfo IDs. Bind the enclosing connector
    // too: a matching operation name from another connector is not authority.
    if response
        .pointer("/connector/id")
        .and_then(serde_json::Value::as_str)
        != Some(operation.connector_id.as_str())
    {
        return Err(FcpTransportError::ProtocolInvalid);
    }
    let tools = response
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .ok_or(FcpTransportError::ProtocolInvalid)?;
    let mut found = false;
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or(FcpTransportError::ProtocolInvalid)?;
        found |= name == operation.operation;
    }
    if found {
        Ok(())
    } else {
        Err(FcpTransportError::Unavailable)
    }
}

fn valid_input_pointer(input: &serde_json::Value, pointer: &str) -> bool {
    pointer.len() <= 1024 && pointer.starts_with('/') && input.pointer(pointer).is_some()
}

impl FcpTransportConfig {
    pub fn validate(&self, host: &ConnectorHostConfig) -> Result<(), FcpTransportError> {
        let invalid = FcpTransportError::InvalidConfig;
        if !cfg!(unix) {
            // This implementation requires no-follow, nonblocking regular-file
            // credential authority; other platforms cannot silently weaken it.
            return Err(invalid);
        }
        let parsed = url::Url::parse(&self.endpoint).map_err(|_| invalid)?;
        let canonical =
            crate::runtime_async::http::ParsedUrl::parse(&self.endpoint).map_err(|_| invalid)?;
        // No DNS, redirects, userinfo, proxy, or alternate URL spelling can
        // expand the explicitly authorized owned-loopback host boundary.
        let loopback = match parsed.host() {
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            _ => false,
        };
        let _ = canonical;
        if !loopback
            || !self.endpoint.starts_with("http://")
            || parsed.scheme() != "http"
            || parsed.port().is_none()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || self.endpoint.contains('@')
            || self.endpoint.trim_end_matches('/') != parsed.as_str().trim_end_matches('/')
            || !self.capability_token_file.is_absolute()
            || !self.admin_token_file.is_absolute()
            || !(1..=30_000).contains(&self.request_timeout_ms)
            || !(1024..=1_048_576).contains(&self.max_payload_bytes)
            || !(1..=4096).contains(&self.max_pending_actions)
            || self.max_pending_actions < self.outbound.len()
            || self.max_retained_actions < self.max_pending_actions
            || self.max_retained_actions > 65_536
            || !(1..=256).contains(&self.max_ingress_batch)
            || self.outbound.len() > 64
            || self.inbound.len() > 64
            || (self.outbound.is_empty() && self.inbound.is_empty())
            || !valid_fcp_label(&host.host_id)
            || !valid_fcp_label(&host.sandbox.zone_id)
            || !host.sandbox.fail_closed
        {
            return Err(invalid);
        }
        host.validate().map_err(|_| invalid)?;
        let mut identities = std::collections::BTreeSet::new();
        for route in &self.outbound {
            if !valid_fcp_label(&route.rule.rule_id)
                // CredentialAction is an in-process broker transaction with
                // different prepared parameters, not an FCP invoke envelope.
                // Refuse it at startup rather than acknowledge an unusable
                // configuration or perform broker work during outbox admission.
                || route.rule.action_kind
                    == crate::connector_outbound_bridge::ConnectorActionKind::CredentialAction
                || route.rule.source_filter.is_some_and(|source| {
                    source != crate::connector_outbound_bridge::OutboundEventSource::PatternDetected
                })
                || !identities.insert(route.rule.rule_id.as_str())
                || route.rule.target_connector != route.invoke.connector_id
                || route
                    .correlation_input_pointer
                    .as_ref()
                    .is_some_and(|pointer| !valid_input_pointer(&route.invoke.input, pointer))
            {
                return Err(invalid);
            }
            self.validate_operation(&route.invoke)?;
        }
        for subscription in &self.inbound {
            if !valid_fcp_label(&subscription.subscription_id)
                || !identities.insert(subscription.subscription_id.as_str())
                || i64::try_from(subscription.pane_id).is_err()
                || subscription.records_pointer.len() > 1024
                || subscription.identity_pointer.len() > 1024
                || !subscription.records_pointer.starts_with('/')
                || !subscription.identity_pointer.starts_with('/')
                || !valid_input_pointer(
                    &subscription.invoke.input,
                    &subscription.cursor_input_pointer,
                )
                || !(100..=3_600_000).contains(&subscription.poll_interval_ms)
                || subscription.invoke.capability != ConnectorCapability::ReadState
            {
                return Err(invalid);
            }
            self.validate_operation(&subscription.invoke)?;
        }
        Ok(())
    }

    fn validate_operation(&self, operation: &FcpOperation) -> Result<(), FcpTransportError> {
        if !valid_fcp_label(&operation.connector_id)
            || !valid_fcp_label(&operation.operation)
            || operation
                .target
                .as_ref()
                .is_some_and(|target| target.len() > 4096)
            || serde_json::to_vec(&operation.input)
                .map_err(|_| FcpTransportError::InvalidConfig)?
                .len()
                > self.max_payload_bytes
        {
            return Err(FcpTransportError::InvalidConfig);
        }
        Ok(())
    }

    pub(crate) fn generation(&self) -> Result<String, FcpTransportError> {
        serde_json::to_string(self)
            .map(|json| fcp_identity_hash(&json))
            .map_err(|_| FcpTransportError::InvalidConfig)
    }
}

/// An acknowledged response whose receipt was independently queried through
/// the authenticated admin endpoint and matched to this invocation.
pub struct FcpAcknowledgement {
    pub receipt_id: String,
    pub receipt_hash: String,
    pub success: bool,
    pub result: Option<serde_json::Value>,
}

impl std::fmt::Debug for FcpAcknowledgement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FcpAcknowledgement")
            .field("receipt_hash", &self.receipt_hash)
            .field("success", &self.success)
            .finish_non_exhaustive()
    }
}

/// The HTTP request is prepared before the durable dispatched transition.
/// Its capability bytes are deliberately private and have no Debug/Serialize.
pub struct PreparedFcpInvocation {
    body: Vec<u8>,
    request_id: String,
    connector_id: String,
    operation: String,
    idempotency_key: String,
    admin_token: String,
}

pub struct FcpHostClient {
    config: FcpTransportConfig,
    zone_id: String,
    http: crate::runtime_async::http::HttpClient,
}

impl FcpHostClient {
    pub fn new(
        config: FcpTransportConfig,
        host: &ConnectorHostConfig,
    ) -> Result<Self, FcpTransportError> {
        config.validate(host)?;
        let http = crate::runtime_async::http::HttpClient::builder()
            .no_redirects()
            .no_retries()
            .no_proxy()
            .no_cookie_store()
            .max_connections_per_host(1)
            .max_total_connections(1)
            .max_body_size(config.max_payload_bytes)
            .request_timeout(std::time::Duration::from_millis(config.request_timeout_ms))
            .build();
        Ok(Self {
            config,
            zone_id: host.sandbox.zone_id.clone(),
            http,
        })
    }

    pub fn operation_deadline(&self) -> Result<std::time::Instant, FcpTransportError> {
        std::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(
                self.config.request_timeout_ms,
            ))
            .ok_or(FcpTransportError::InvalidConfig)
    }

    /// `request_id` must be a hyphenated UUID; it is also the FCP correlation ID.
    pub async fn prepare_invocation(
        &self,
        cx: &crate::cx::Cx,
        deadline: std::time::Instant,
        operation: &FcpOperation,
        request_id: &str,
        idempotency_key: &str,
    ) -> Result<PreparedFcpInvocation, FcpTransportError> {
        self.config.validate_operation(operation)?;
        if !valid_fcp_request_id(request_id)
            || idempotency_key.len() != 64
            || !idempotency_key.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(FcpTransportError::InvalidConfig);
        }
        cx.checkpoint().map_err(|_| FcpTransportError::Cancelled)?;
        remaining_fcp_time(deadline)?;
        let token_path = self.config.capability_token_file.clone();
        let admin_path = self.config.admin_token_file.clone();
        let read_cx = cx.clone();
        let read_mask = crate::cx::effective_cap_mask(cx);
        let (token, admin_bytes) = crate::runtime_async::spawn_blocking(move || {
            let _scope = crate::cx::Cx::set_current(Some(read_cx.clone()));
            let _capabilities = crate::cx::Cx::push_restriction(read_mask);
            read_cx
                .checkpoint()
                .map_err(|_| FcpTransportError::Cancelled)?;
            remaining_fcp_time(deadline)?;
            let token = read_fcp_secret_file(&token_path, 65_536)?;
            read_cx
                .checkpoint()
                .map_err(|_| FcpTransportError::Cancelled)?;
            let admin = read_fcp_secret_file(&admin_path, 8192)?;
            read_cx
                .checkpoint()
                .map_err(|_| FcpTransportError::Cancelled)?;
            remaining_fcp_time(deadline)?;
            Ok::<_, FcpTransportError>((token, admin))
        })
        .await
        .map_err(|_| FcpTransportError::CredentialUnavailable)??;
        cx.checkpoint().map_err(|_| FcpTransportError::Cancelled)?;
        let remaining = remaining_fcp_time(deadline)?;
        let admin_token = std::str::from_utf8(&admin_bytes)
            .map_err(|_| FcpTransportError::CredentialUnavailable)?
            .trim()
            .to_string();
        if admin_token.is_empty() || !admin_token.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(FcpTransportError::CredentialUnavailable);
        }
        let body = serde_json::to_vec(&serde_json::json!({
            "type": "invoke", "id": request_id,
            "connector_id": operation.connector_id, "operation": operation.operation,
            "zone_id": self.zone_id, "input": operation.input,
            "capability_token": token, "idempotency_key": idempotency_key,
            // FCP protocol.rs explicitly defines this as milliseconds from now.
            "correlation_id": request_id, "deadline_ms": u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
        })).map_err(|_| FcpTransportError::ProtocolInvalid)?;
        if body.len() > self.config.max_payload_bytes {
            return Err(FcpTransportError::PayloadLimit);
        }
        Ok(PreparedFcpInvocation {
            body,
            request_id: request_id.to_string(),
            connector_id: operation.connector_id.clone(),
            operation: operation.operation.clone(),
            idempotency_key: idempotency_key.to_string(),
            admin_token,
        })
    }

    /// Confirms this exact connector/operation exists in the real host inventory.
    /// This is an observation only; it never turns model authorization into delivery.
    pub async fn observe_operation(
        &self,
        cx: &crate::cx::Cx,
        deadline: std::time::Instant,
        operation: &FcpOperation,
    ) -> Result<(), FcpTransportError> {
        self.config.validate_operation(operation)?;
        let discovery = self
            .request_json(cx, deadline, "/rpc/discover", serde_json::json!({}), None)
            .await?;
        let connectors = discovery
            .get("connectors")
            .and_then(serde_json::Value::as_array)
            .ok_or(FcpTransportError::ProtocolInvalid)?;
        let connector = connectors
            .iter()
            .find(|connector| {
                connector.get("id").and_then(serde_json::Value::as_str)
                    == Some(operation.connector_id.as_str())
            })
            .ok_or(FcpTransportError::Unavailable)?;
        if connector
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
            || connector
                .pointer("/health/status")
                .and_then(serde_json::Value::as_str)
                != Some("healthy")
        {
            return Err(FcpTransportError::Unavailable);
        }
        let path = format!("/rpc/introspect/{}", operation.connector_id);
        let response = self
            .exchange(
                cx,
                deadline,
                crate::runtime_async::http::Method::Get,
                &path,
                Vec::new(),
                None,
            )
            .await?;
        validate_fcp_introspection(&response, operation)
    }

    /// Call only after durable dispatch ownership is acquired. Any error,
    /// cancellation, or owner drop after this call begins is indeterminate.
    pub async fn invoke(
        &self,
        cx: &crate::cx::Cx,
        deadline: std::time::Instant,
        request: PreparedFcpInvocation,
    ) -> Result<FcpAcknowledgement, FcpTransportError> {
        let response = self
            .exchange(
                cx,
                deadline,
                crate::runtime_async::http::Method::Post,
                "/rpc/invoke",
                request.body,
                None,
            )
            .await?;
        if response.get("type").and_then(serde_json::Value::as_str) != Some("response")
            || response.get("id").and_then(serde_json::Value::as_str)
                != Some(request.request_id.as_str())
        {
            return Err(FcpTransportError::ProtocolInvalid);
        }
        let success = match response.get("status").and_then(serde_json::Value::as_str) {
            Some("ok") if response.get("error").is_none_or(serde_json::Value::is_null) => true,
            Some("error") if response.get("error").is_some_and(|error| !error.is_null()) => false,
            _ => return Err(FcpTransportError::ProtocolInvalid),
        };
        let receipt_id = response
            .get("receipt_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or(FcpTransportError::ReceiptUnconfirmed)?;
        let receipts = self.request_json(cx, deadline, "/rpc/admin/receipts", serde_json::json!({
            "connector_id": request.connector_id, "operation": request.operation, "limit": 256,
        }), Some(&request.admin_token)).await?;
        let matches: Vec<&serde_json::Value> = receipts
            .get("receipts")
            .and_then(serde_json::Value::as_array)
            .ok_or(FcpTransportError::ProtocolInvalid)?
            .iter()
            .filter(|receipt| {
                receipt
                    .get("receipt_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(receipt_id)
            })
            .collect();
        let [receipt] = matches.as_slice() else {
            return Err(FcpTransportError::ReceiptUnconfirmed);
        };
        if receipt
            .get("connector_id")
            .and_then(serde_json::Value::as_str)
            != Some(request.connector_id.as_str())
            || receipt.get("operation").and_then(serde_json::Value::as_str)
                != Some(request.operation.as_str())
            || receipt
                .get("idempotency_key")
                .and_then(serde_json::Value::as_str)
                != Some(request.idempotency_key.as_str())
            || receipt.get("success").and_then(serde_json::Value::as_bool) != Some(success)
        {
            return Err(FcpTransportError::ReceiptUnconfirmed);
        }
        let receipt_json =
            serde_json::to_string(receipt).map_err(|_| FcpTransportError::ProtocolInvalid)?;
        Ok(FcpAcknowledgement {
            receipt_id: receipt_id.to_string(),
            receipt_hash: fcp_identity_hash(&receipt_json),
            success,
            result: response.get("result").cloned(),
        })
    }

    async fn request_json(
        &self,
        cx: &crate::cx::Cx,
        deadline: std::time::Instant,
        path: &str,
        body: serde_json::Value,
        admin: Option<&str>,
    ) -> Result<serde_json::Value, FcpTransportError> {
        let body = serde_json::to_vec(&body).map_err(|_| FcpTransportError::ProtocolInvalid)?;
        self.exchange(
            cx,
            deadline,
            crate::runtime_async::http::Method::Post,
            path,
            body,
            admin,
        )
        .await
    }

    async fn exchange(
        &self,
        cx: &crate::cx::Cx,
        deadline: std::time::Instant,
        method: crate::runtime_async::http::Method,
        path: &str,
        body: Vec<u8>,
        admin: Option<&str>,
    ) -> Result<serde_json::Value, FcpTransportError> {
        cx.checkpoint().map_err(|_| FcpTransportError::Cancelled)?;
        if body.len() > self.config.max_payload_bytes {
            return Err(FcpTransportError::PayloadLimit);
        }
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        if let Some(token) = admin {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
            headers.push(("x-fcp-zone".to_string(), "z:owner".to_string()));
        }
        let endpoint = format!("{}{path}", self.config.endpoint.trim_end_matches('/'));
        let response = self
            .http
            .request_with_timeout(
                cx,
                method,
                &endpoint,
                headers,
                body,
                remaining_fcp_time(deadline)?,
            )
            .await
            .map_err(|_| FcpTransportError::Unavailable)?;
        cx.checkpoint().map_err(|_| FcpTransportError::Cancelled)?;
        if response.status != 200 {
            return Err(FcpTransportError::Unavailable);
        }
        if response.body.len() > self.config.max_payload_bytes {
            return Err(FcpTransportError::PayloadLimit);
        }
        serde_json::from_slice(&response.body).map_err(|_| FcpTransportError::ProtocolInvalid)
    }
}

fn remaining_fcp_time(
    deadline: std::time::Instant,
) -> Result<std::time::Duration, FcpTransportError> {
    deadline
        .checked_duration_since(std::time::Instant::now())
        .filter(|remaining| remaining.as_millis() > 0)
        .ok_or(FcpTransportError::Unavailable)
}

fn read_fcp_secret_file(path: &std::path::Path, limit: u64) -> Result<Vec<u8>, FcpTransportError> {
    use std::io::Read;
    let denied = FcpTransportError::CredentialUnavailable;
    let metadata = std::fs::symlink_metadata(path).map_err(|_| denied)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > limit {
        return Err(denied);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(denied);
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A pathname replaced with a FIFO or symlink must not block the
        // executor or redirect credential authority between metadata and open.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| denied)?;
    let opened = file.metadata().map_err(|_| denied)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.dev() != opened.dev() || metadata.ino() != opened.ino() {
            return Err(denied);
        }
    }
    if !opened.is_file() || opened.len() > limit {
        return Err(denied);
    }
    let mut bytes = Vec::new();
    (&file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| denied)?;
    if bytes.is_empty() || u64::try_from(bytes.len()).map_err(|_| denied)? > limit {
        return Err(denied);
    }
    let after = file.metadata().map_err(|_| denied)?;
    let named = std::fs::symlink_metadata(path).map_err(|_| denied)?;
    if !after.is_file()
        || !named.is_file()
        || after.len() != opened.len()
        || after.len() != u64::try_from(bytes.len()).map_err(|_| denied)?
        || named.len() != after.len()
    {
        return Err(denied);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if after.dev() != named.dev()
            || after.ino() != named.ino()
            || after.dev() != opened.dev()
            || after.ino() != opened.ino()
            || after.mode() & 0o077 != 0
            || named.mode() & 0o077 != 0
            || after.mtime() != opened.mtime()
            || after.mtime_nsec() != opened.mtime_nsec()
            || after.ctime() != opened.ctime()
            || after.ctime_nsec() != opened.ctime_nsec()
        {
            return Err(denied);
        }
    }
    Ok(bytes)
}

/// Protocol version shared between the FrankenTerm control plane and connector host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConnectorProtocolVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl ConnectorProtocolVersion {
    /// Create a protocol version.
    #[must_use]
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Return true when `self` is a same-major forward upgrade from `current`.
    #[must_use]
    pub const fn is_same_major_upgrade_from(self, current: Self) -> bool {
        self.major == current.major
            && (self.minor > current.minor
                || (self.minor == current.minor && self.patch > current.patch))
    }
}

impl Default for ConnectorProtocolVersion {
    fn default() -> Self {
        Self::new(1, 0, 0)
    }
}

impl std::fmt::Display for ConnectorProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Normalized connector failure classes for deterministic automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorFailureClass {
    Auth,
    Quota,
    Network,
    Policy,
    Validation,
    Timeout,
    Unknown,
}

impl ConnectorFailureClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Quota => "quota",
            Self::Network => "network",
            Self::Policy => "policy",
            Self::Validation => "validation",
            Self::Timeout => "timeout",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ConnectorFailureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Runtime budget guardrails to isolate connector execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorRuntimeBudgets {
    /// CPU budget in milliseconds available per second window.
    pub cpu_millis_per_second: u32,
    /// Memory budget in bytes.
    pub memory_bytes: u64,
    /// I/O throughput budget in bytes per second.
    pub io_bytes_per_second: u64,
    /// Maximum in-flight connector operations.
    pub max_inflight_ops: u32,
}

impl Default for ConnectorRuntimeBudgets {
    fn default() -> Self {
        Self {
            cpu_millis_per_second: 750,
            memory_bytes: 512 * 1024 * 1024,
            io_bytes_per_second: 16 * 1024 * 1024,
            max_inflight_ops: 256,
        }
    }
}

impl ConnectorRuntimeBudgets {
    /// Validate budget values are non-zero.
    pub fn validate(&self) -> Result<(), ConnectorHostRuntimeError> {
        if self.cpu_millis_per_second == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "cpu_millis_per_second must be > 0".to_string(),
            });
        }
        if self.memory_bytes == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "memory_bytes must be > 0".to_string(),
            });
        }
        if self.io_bytes_per_second == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "io_bytes_per_second must be > 0".to_string(),
            });
        }
        if self.max_inflight_ops == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "max_inflight_ops must be > 0".to_string(),
            });
        }
        Ok(())
    }
}

/// Capability gates available to connector operations inside sandbox zones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorCapability {
    Invoke,
    ReadState,
    StreamEvents,
    FilesystemRead,
    FilesystemWrite,
    NetworkEgress,
    SecretBroker,
    ProcessExec,
}

impl ConnectorCapability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invoke => "invoke",
            Self::ReadState => "read_state",
            Self::StreamEvents => "stream_events",
            Self::FilesystemRead => "filesystem_read",
            Self::FilesystemWrite => "filesystem_write",
            Self::NetworkEgress => "network_egress",
            Self::SecretBroker => "secret_broker",
            Self::ProcessExec => "process_exec",
        }
    }
}

impl std::fmt::Display for ConnectorCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Explicit capability envelope and target constraints for connector execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorCapabilityEnvelope {
    pub allowed_capabilities: Vec<ConnectorCapability>,
    pub filesystem_read_prefixes: Vec<String>,
    pub filesystem_write_prefixes: Vec<String>,
    pub network_allow_hosts: Vec<String>,
    pub allowed_exec_commands: Vec<String>,
}

impl Default for ConnectorCapabilityEnvelope {
    fn default() -> Self {
        Self {
            allowed_capabilities: vec![
                ConnectorCapability::Invoke,
                ConnectorCapability::ReadState,
                ConnectorCapability::StreamEvents,
            ],
            filesystem_read_prefixes: Vec::new(),
            filesystem_write_prefixes: Vec::new(),
            network_allow_hosts: Vec::new(),
            allowed_exec_commands: Vec::new(),
        }
    }
}

impl ConnectorCapabilityEnvelope {
    pub fn validate(&self) -> Result<(), ConnectorHostRuntimeError> {
        if self.allowed_capabilities.is_empty() {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "allowed_capabilities must not be empty".to_string(),
            });
        }
        for prefix in &self.filesystem_read_prefixes {
            if prefix.trim().is_empty() {
                return Err(ConnectorHostRuntimeError::InvalidConfig {
                    reason: "filesystem_read_prefixes must not contain empty values".to_string(),
                });
            }
        }
        for prefix in &self.filesystem_write_prefixes {
            if prefix.trim().is_empty() {
                return Err(ConnectorHostRuntimeError::InvalidConfig {
                    reason: "filesystem_write_prefixes must not contain empty values".to_string(),
                });
            }
        }
        for host in &self.network_allow_hosts {
            if host.trim().is_empty() {
                return Err(ConnectorHostRuntimeError::InvalidConfig {
                    reason: "network_allow_hosts must not contain empty values".to_string(),
                });
            }
        }
        for command in &self.allowed_exec_commands {
            if command.trim().is_empty() {
                return Err(ConnectorHostRuntimeError::InvalidConfig {
                    reason: "allowed_exec_commands must not contain empty values".to_string(),
                });
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn allows_capability(&self, capability: ConnectorCapability) -> bool {
        self.allowed_capabilities.contains(&capability)
    }

    #[must_use]
    pub fn allows_target(&self, capability: ConnectorCapability, target: Option<&str>) -> bool {
        match capability {
            ConnectorCapability::FilesystemRead => target.is_some_and(|path| {
                self.filesystem_read_prefixes
                    .iter()
                    .any(|prefix| path_is_within_prefix(path, prefix))
            }),
            ConnectorCapability::FilesystemWrite => target.is_some_and(|path| {
                self.filesystem_write_prefixes
                    .iter()
                    .any(|prefix| path_is_within_prefix(path, prefix))
            }),
            ConnectorCapability::NetworkEgress => target.is_some_and(|host| {
                self.network_allow_hosts
                    .iter()
                    .any(|allowed| network_host_matches(allowed, host))
            }),
            ConnectorCapability::ProcessExec => target.is_some_and(|command| {
                self.allowed_exec_commands
                    .iter()
                    .any(|allowed| allowed == command)
            }),
            ConnectorCapability::Invoke
            | ConnectorCapability::ReadState
            | ConnectorCapability::StreamEvents
            | ConnectorCapability::SecretBroker => true,
        }
    }
}

fn network_host_matches(allowed: &str, host: &str) -> bool {
    let allowed = allowed.to_ascii_lowercase();
    let host = host.to_ascii_lowercase();

    if let Some(suffix) = allowed.strip_prefix("*.") {
        let suffix = format!(".{suffix}");
        host.len() > suffix.len() && host.ends_with(&suffix)
    } else {
        host == allowed
    }
}

fn normalize_absolute_path(path: &str) -> Option<Vec<String>> {
    use std::path::Component;

    let mut saw_root = false;
    let mut parts: Vec<String> = Vec::new();

    for component in std::path::Path::new(path).components() {
        match component {
            Component::RootDir => saw_root = true,
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Prefix(_) => return None,
        }
    }

    if !saw_root {
        return None;
    }

    Some(parts)
}

fn path_is_within_prefix(path: &str, prefix: &str) -> bool {
    let Some(path_parts) = normalize_absolute_path(path) else {
        return false;
    };
    let Some(prefix_parts) = normalize_absolute_path(prefix) else {
        return false;
    };

    if prefix_parts.len() > path_parts.len() {
        return false;
    }

    path_parts
        .iter()
        .zip(prefix_parts.iter())
        .all(|(candidate, required)| candidate == required)
}

/// Sandbox zone boundary for connector runtime operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorSandboxZone {
    pub zone_id: String,
    pub fail_closed: bool,
    pub capability_envelope: ConnectorCapabilityEnvelope,
}

impl Default for ConnectorSandboxZone {
    fn default() -> Self {
        Self {
            zone_id: "zone.default".to_string(),
            fail_closed: true,
            capability_envelope: ConnectorCapabilityEnvelope::default(),
        }
    }
}

impl ConnectorSandboxZone {
    pub fn validate(&self) -> Result<(), ConnectorHostRuntimeError> {
        if self.zone_id.trim().is_empty() {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "sandbox.zone_id must not be empty".to_string(),
            });
        }
        self.capability_envelope.validate()
    }
}

/// Auditable sandbox decision for each operation authorization attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorSandboxDecision {
    pub decision_id: String,
    pub at_ms: u64,
    pub zone_id: String,
    pub action: String,
    pub capability: ConnectorCapability,
    pub target: Option<String>,
    pub allowed: bool,
    pub reason_code: String,
}

/// Input used for sandbox authorization of connector operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorOperationRequest {
    pub action: String,
    pub correlation_id: String,
    pub capability: ConnectorCapability,
    pub target: Option<String>,
}

impl ConnectorOperationRequest {
    #[must_use]
    pub fn new(
        action: impl Into<String>,
        correlation_id: impl Into<String>,
        capability: ConnectorCapability,
    ) -> Self {
        Self {
            action: action.into(),
            correlation_id: correlation_id.into(),
            capability,
            target: None,
        }
    }

    #[must_use]
    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }
}

/// Host runtime configuration for connector embedding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorHostConfig {
    /// Stable host identifier used in operation envelope IDs.
    pub host_id: String,
    /// Current protocol version for connector interactions.
    pub protocol_version: ConnectorProtocolVersion,
    /// Runtime isolation budgets.
    pub budgets: ConnectorRuntimeBudgets,
    /// Startup timeout budget in milliseconds.
    pub startup_timeout_ms: u64,
    /// Expected heartbeat interval in milliseconds.
    pub heartbeat_interval_ms: u64,
    /// Backoff before retry after failures in milliseconds.
    pub failure_backoff_ms: u64,
    /// Sandbox zone and capability envelope constraints for connector execution.
    pub sandbox: ConnectorSandboxZone,
}

impl Default for ConnectorHostConfig {
    fn default() -> Self {
        Self {
            host_id: "connector-host-0".to_string(),
            protocol_version: ConnectorProtocolVersion::default(),
            budgets: ConnectorRuntimeBudgets::default(),
            startup_timeout_ms: 10_000,
            heartbeat_interval_ms: 1_000,
            failure_backoff_ms: 5_000,
            sandbox: ConnectorSandboxZone::default(),
        }
    }
}

impl ConnectorHostConfig {
    /// Validate config values are coherent.
    pub fn validate(&self) -> Result<(), ConnectorHostRuntimeError> {
        if self.host_id.trim().is_empty() {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "host_id must not be empty".to_string(),
            });
        }
        if self.startup_timeout_ms == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "startup_timeout_ms must be > 0".to_string(),
            });
        }
        if self.heartbeat_interval_ms == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "heartbeat_interval_ms must be > 0".to_string(),
            });
        }
        if self.failure_backoff_ms == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "failure_backoff_ms must be > 0".to_string(),
            });
        }
        self.sandbox.validate()?;
        self.budgets.validate()
    }
}

/// Runtime usage snapshot for budget checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorRuntimeUsage {
    pub cpu_millis_in_window: u32,
    pub memory_bytes: u64,
    pub io_bytes_in_window: u64,
    pub inflight_ops: u32,
}

impl ConnectorRuntimeUsage {
    /// Return the first exceeded budget dimension, if any.
    #[must_use]
    pub fn exceeded_dimension(&self, budgets: &ConnectorRuntimeBudgets) -> Option<&'static str> {
        if self.cpu_millis_in_window > budgets.cpu_millis_per_second {
            return Some("cpu_millis_per_second");
        }
        if self.memory_bytes > budgets.memory_bytes {
            return Some("memory_bytes");
        }
        if self.io_bytes_in_window > budgets.io_bytes_per_second {
            return Some("io_bytes_per_second");
        }
        if self.inflight_ops > budgets.max_inflight_ops {
            return Some("max_inflight_ops");
        }
        None
    }
}

/// Concrete failure payload used by degraded/failed states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorFailure {
    pub class: ConnectorFailureClass,
    pub reason_code: String,
    pub observed_at_ms: u64,
}

/// Coarse lifecycle phases for transition records and policy checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorLifecyclePhase {
    Stopped,
    Starting,
    Running,
    Degraded,
    Failed,
}

impl ConnectorLifecyclePhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for ConnectorLifecyclePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Full lifecycle state with failure context when degraded/failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorLifecycleState {
    Stopped,
    Starting,
    Running,
    Degraded(ConnectorFailure),
    Failed(ConnectorFailure),
}

impl ConnectorLifecycleState {
    #[must_use]
    pub const fn phase(&self) -> ConnectorLifecyclePhase {
        match self {
            Self::Stopped => ConnectorLifecyclePhase::Stopped,
            Self::Starting => ConnectorLifecyclePhase::Starting,
            Self::Running => ConnectorLifecyclePhase::Running,
            Self::Degraded(_) => ConnectorLifecyclePhase::Degraded,
            Self::Failed(_) => ConnectorLifecyclePhase::Failed,
        }
    }

    #[must_use]
    pub const fn failure(&self) -> Option<&ConnectorFailure> {
        match self {
            Self::Degraded(failure) | Self::Failed(failure) => Some(failure),
            Self::Stopped | Self::Starting | Self::Running => None,
        }
    }
}

/// Startup probe result used to make degraded/failure paths deterministic in tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupProbeResult {
    Healthy,
    Failed {
        class: ConnectorFailureClass,
        reason_code: String,
    },
}

impl StartupProbeResult {
    #[must_use]
    pub const fn healthy() -> Self {
        Self::Healthy
    }

    #[must_use]
    pub fn failed(class: ConnectorFailureClass, reason_code: impl Into<String>) -> Self {
        Self::Failed {
            class,
            reason_code: reason_code.into(),
        }
    }
}

/// Transition record for auditable lifecycle behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorLifecycleTransition {
    pub at_ms: u64,
    pub from: ConnectorLifecyclePhase,
    pub to: ConnectorLifecyclePhase,
    pub reason_code: String,
}

/// Health/liveness projection for operator and machine APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorHealthSnapshot {
    pub host_id: String,
    pub protocol_version: ConnectorProtocolVersion,
    pub phase: ConnectorLifecyclePhase,
    pub is_live: bool,
    pub is_ready: bool,
    pub last_transition_at_ms: u64,
    pub last_heartbeat_at_ms: Option<u64>,
    pub heartbeat_age_ms: Option<u64>,
    pub active_failures: u32,
    pub sandbox_zone_id: String,
    pub budgets: ConnectorRuntimeBudgets,
    pub latest_usage: Option<ConnectorRuntimeUsage>,
    pub last_failure: Option<ConnectorFailure>,
    pub last_sandbox_decision: Option<ConnectorSandboxDecision>,
}

/// Protocol envelope for connector operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorOperationEnvelope {
    pub operation_id: String,
    pub correlation_id: String,
    pub host_id: String,
    pub zone_id: String,
    pub protocol_version: ConnectorProtocolVersion,
    pub action: String,
    pub capability: ConnectorCapability,
    pub target: Option<String>,
    pub decision_id: String,
    pub issued_at_ms: u64,
}

/// Runtime manager for connector host lifecycle and budget isolation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorHostRuntime {
    config: ConnectorHostConfig,
    state: ConnectorLifecycleState,
    last_transition_at_ms: u64,
    last_heartbeat_at_ms: Option<u64>,
    last_upgrade_at_ms: Option<u64>,
    active_failures: u32,
    operation_seq: u64,
    sandbox_decision_seq: u64,
    latest_usage: Option<ConnectorRuntimeUsage>,
    transition_history: VecDeque<ConnectorLifecycleTransition>,
    sandbox_decisions: VecDeque<ConnectorSandboxDecision>,
}

impl ConnectorHostRuntime {
    /// Create a new connector host runtime in the `stopped` phase.
    pub fn new(config: ConnectorHostConfig) -> Result<Self, ConnectorHostRuntimeError> {
        config.validate()?;
        Ok(Self {
            config,
            state: ConnectorLifecycleState::Stopped,
            last_transition_at_ms: 0,
            last_heartbeat_at_ms: None,
            last_upgrade_at_ms: None,
            active_failures: 0,
            operation_seq: 0,
            sandbox_decision_seq: 0,
            latest_usage: None,
            transition_history: VecDeque::with_capacity(TRANSITION_HISTORY_CAPACITY),
            sandbox_decisions: VecDeque::with_capacity(SANDBOX_DECISION_HISTORY_CAPACITY),
        })
    }

    /// Current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> &ConnectorLifecycleState {
        &self.state
    }

    /// Runtime configuration.
    #[must_use]
    pub const fn config(&self) -> &ConnectorHostConfig {
        &self.config
    }

    /// Transition history (oldest to newest).
    #[must_use]
    pub fn transition_history(&self) -> Vec<ConnectorLifecycleTransition> {
        self.transition_history.iter().cloned().collect()
    }

    /// Sandbox decision history (oldest to newest).
    #[must_use]
    pub fn sandbox_decision_history(&self) -> Vec<ConnectorSandboxDecision> {
        self.sandbox_decisions.iter().cloned().collect()
    }

    /// Start the host with a healthy startup probe.
    pub fn start(&mut self, now_ms: u64) -> Result<(), ConnectorHostRuntimeError> {
        self.start_with_probe(now_ms, StartupProbeResult::Healthy)
    }

    /// Start the host with an explicit startup probe result.
    pub fn start_with_probe(
        &mut self,
        now_ms: u64,
        probe: StartupProbeResult,
    ) -> Result<(), ConnectorHostRuntimeError> {
        let from = self.state.phase();
        if matches!(
            from,
            ConnectorLifecyclePhase::Starting | ConnectorLifecyclePhase::Running
        ) {
            return Err(ConnectorHostRuntimeError::InvalidTransition {
                from,
                to: ConnectorLifecyclePhase::Starting,
                reason: "host is already starting or running".to_string(),
            });
        }

        self.transition(
            now_ms,
            ConnectorLifecycleState::Starting,
            "lifecycle.start.requested",
        );
        match probe {
            StartupProbeResult::Healthy => {
                self.transition(
                    now_ms,
                    ConnectorLifecycleState::Running,
                    "lifecycle.start.ready",
                );
                self.last_heartbeat_at_ms = Some(now_ms);
                Ok(())
            }
            StartupProbeResult::Failed { class, reason_code } => {
                ensure_reason_code(&reason_code)?;
                self.active_failures = self.active_failures.saturating_add(1);
                let failure = ConnectorFailure {
                    class,
                    reason_code: reason_code.clone(),
                    observed_at_ms: now_ms,
                };
                self.transition(
                    now_ms,
                    ConnectorLifecycleState::Failed(failure),
                    "lifecycle.start.failed",
                );
                Err(ConnectorHostRuntimeError::StartupProbeFailed { class, reason_code })
            }
        }
    }

    /// Stop the host.
    pub fn stop(&mut self, now_ms: u64) -> Result<(), ConnectorHostRuntimeError> {
        if self.state.phase() == ConnectorLifecyclePhase::Stopped {
            return Err(ConnectorHostRuntimeError::InvalidTransition {
                from: ConnectorLifecyclePhase::Stopped,
                to: ConnectorLifecyclePhase::Stopped,
                reason: "host is already stopped".to_string(),
            });
        }
        self.transition(
            now_ms,
            ConnectorLifecycleState::Stopped,
            "lifecycle.stop.requested",
        );
        self.last_heartbeat_at_ms = None;
        self.latest_usage = None;
        Ok(())
    }

    /// Restart the host using a healthy startup probe.
    pub fn restart(&mut self, now_ms: u64) -> Result<(), ConnectorHostRuntimeError> {
        self.restart_with_probe(now_ms, StartupProbeResult::Healthy)
    }

    /// Restart the host with an explicit startup probe result.
    pub fn restart_with_probe(
        &mut self,
        now_ms: u64,
        probe: StartupProbeResult,
    ) -> Result<(), ConnectorHostRuntimeError> {
        if self.state.phase() != ConnectorLifecyclePhase::Stopped {
            self.stop(now_ms)?;
        }
        self.start_with_probe(now_ms, probe)
    }

    /// Upgrade protocol version and restart if host is currently live.
    pub fn upgrade_and_restart(
        &mut self,
        now_ms: u64,
        new_version: ConnectorProtocolVersion,
        probe: StartupProbeResult,
    ) -> Result<(), ConnectorHostRuntimeError> {
        if !new_version.is_same_major_upgrade_from(self.config.protocol_version) {
            return Err(ConnectorHostRuntimeError::ProtocolUpgradeRejected {
                reason: format!(
                    "new version {new_version} must be a same-major forward upgrade from current {}",
                    self.config.protocol_version
                ),
            });
        }

        let was_live = matches!(
            self.state.phase(),
            ConnectorLifecyclePhase::Starting
                | ConnectorLifecyclePhase::Running
                | ConnectorLifecyclePhase::Degraded
        );
        if was_live {
            self.stop(now_ms)?;
        }
        self.config.protocol_version = new_version;
        self.last_upgrade_at_ms = Some(now_ms);
        self.transition(now_ms, self.state.clone(), "lifecycle.upgrade.applied");
        if was_live {
            self.start_with_probe(now_ms, probe)?;
        }
        Ok(())
    }

    /// Record a heartbeat from the connector host.
    pub fn record_heartbeat(&mut self, now_ms: u64) -> Result<(), ConnectorHostRuntimeError> {
        match self.state.phase() {
            ConnectorLifecyclePhase::Running | ConnectorLifecyclePhase::Degraded => {
                self.last_heartbeat_at_ms = Some(now_ms);
                Ok(())
            }
            phase => Err(ConnectorHostRuntimeError::HostNotRunnable { phase }),
        }
    }

    /// Observe runtime usage and enforce configured budgets.
    pub fn observe_usage(
        &mut self,
        now_ms: u64,
        usage: ConnectorRuntimeUsage,
    ) -> Result<(), ConnectorHostRuntimeError> {
        self.latest_usage = Some(usage);
        if let Some(dimension) = usage.exceeded_dimension(&self.config.budgets) {
            self.active_failures = self.active_failures.saturating_add(1);
            let failure = ConnectorFailure {
                class: ConnectorFailureClass::Quota,
                reason_code: format!("budget_exceeded.{dimension}"),
                observed_at_ms: now_ms,
            };
            self.transition(
                now_ms,
                ConnectorLifecycleState::Degraded(failure),
                "lifecycle.degraded.budget_exceeded",
            );
            return Err(ConnectorHostRuntimeError::BudgetExceeded {
                dimension: dimension.to_string(),
            });
        }

        if self.state.phase() == ConnectorLifecyclePhase::Degraded {
            self.transition(
                now_ms,
                ConnectorLifecycleState::Running,
                "lifecycle.degraded.recovered",
            );
        }

        Ok(())
    }

    /// Mark an explicit runtime failure.
    pub fn mark_failure(
        &mut self,
        now_ms: u64,
        class: ConnectorFailureClass,
        reason_code: impl Into<String>,
    ) -> Result<(), ConnectorHostRuntimeError> {
        let reason_code = reason_code.into();
        ensure_reason_code(&reason_code)?;
        self.active_failures = self.active_failures.saturating_add(1);
        let failure = ConnectorFailure {
            class,
            reason_code,
            observed_at_ms: now_ms,
        };
        self.transition(
            now_ms,
            ConnectorLifecycleState::Failed(failure),
            "lifecycle.failed",
        );
        Ok(())
    }

    /// Build a versioned operation envelope with monotonic operation ID.
    pub fn build_operation_envelope(
        &mut self,
        now_ms: u64,
        action: impl Into<String>,
        correlation_id: impl Into<String>,
    ) -> Result<ConnectorOperationEnvelope, ConnectorHostRuntimeError> {
        let action = action.into();
        let correlation_id = correlation_id.into();
        let capability = infer_capability_from_action(&action);
        self.authorize_operation(
            now_ms,
            ConnectorOperationRequest::new(action, correlation_id, capability),
        )
    }

    /// Validate request identity and sandbox predicates without changing host
    /// lifecycle, sequences, or decision history. This is admission only;
    /// runnable phase and dispatch authority are separate requirements.
    pub fn validate_operation_request(
        &self,
        request: &ConnectorOperationRequest,
    ) -> Result<(), ConnectorHostRuntimeError> {
        if request.action.trim().is_empty() {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "action must not be empty".to_string(),
            });
        }

        if request.correlation_id.trim().is_empty() {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "correlation_id must not be empty".to_string(),
            });
        }

        let capability_allowed = self
            .config
            .sandbox
            .capability_envelope
            .allows_capability(request.capability);
        let target_allowed = self
            .config
            .sandbox
            .capability_envelope
            .allows_target(request.capability, request.target.as_deref());

        if !capability_allowed || !target_allowed {
            return Err(ConnectorHostRuntimeError::SandboxViolation {
                zone_id: self.config.sandbox.zone_id.clone(),
                capability: request.capability,
                reason_code: if !capability_allowed {
                    format!("sandbox.denied.capability.{}", request.capability)
                } else {
                    format!("sandbox.denied.target.{}", request.capability)
                },
            });
        }
        Ok(())
    }

    /// Authorize a connector operation against sandbox zone and capability envelope.
    pub fn authorize_operation(
        &mut self,
        now_ms: u64,
        request: ConnectorOperationRequest,
    ) -> Result<ConnectorOperationEnvelope, ConnectorHostRuntimeError> {
        if self.state.phase() != ConnectorLifecyclePhase::Running {
            return Err(ConnectorHostRuntimeError::HostNotRunnable {
                phase: self.state.phase(),
            });
        }
        let denied_reason = match self.validate_operation_request(&request) {
            Ok(()) => None,
            Err(ConnectorHostRuntimeError::SandboxViolation { reason_code, .. }) => {
                Some(reason_code)
            }
            Err(error) => return Err(error),
        };
        // Preflight both counters before recording an allowed decision. Failed
        // envelope allocation must not leave a successful sandbox receipt.
        let next_operation_seq = if denied_reason.is_none() {
            self.operation_seq.checked_add(1).ok_or_else(|| {
                ConnectorHostRuntimeError::InvalidConfig {
                    reason: "operation sequence overflow".to_string(),
                }
            })?
        } else {
            self.operation_seq
        };
        self.sandbox_decision_seq = self.sandbox_decision_seq.checked_add(1).ok_or_else(|| {
            ConnectorHostRuntimeError::InvalidConfig {
                reason: "sandbox decision sequence overflow".to_string(),
            }
        })?;
        let decision_id = format!(
            "{}-sd-{:016x}",
            self.config.host_id, self.sandbox_decision_seq
        );

        if let Some(reason_code) = denied_reason {
            let decision = ConnectorSandboxDecision {
                decision_id: decision_id.clone(),
                at_ms: now_ms,
                zone_id: self.config.sandbox.zone_id.clone(),
                action: request.action.clone(),
                capability: request.capability,
                target: request.target.clone(),
                allowed: false,
                reason_code: reason_code.clone(),
            };
            self.record_sandbox_decision(decision);

            if self.config.sandbox.fail_closed {
                self.active_failures = self.active_failures.saturating_add(1);
                let failure = ConnectorFailure {
                    class: ConnectorFailureClass::Policy,
                    reason_code: reason_code.clone(),
                    observed_at_ms: now_ms,
                };
                self.transition(
                    now_ms,
                    ConnectorLifecycleState::Failed(failure),
                    "lifecycle.failed.sandbox_violation",
                );
            }

            return Err(ConnectorHostRuntimeError::SandboxViolation {
                zone_id: self.config.sandbox.zone_id.clone(),
                capability: request.capability,
                reason_code,
            });
        }

        let allowed_decision = ConnectorSandboxDecision {
            decision_id: decision_id.clone(),
            at_ms: now_ms,
            zone_id: self.config.sandbox.zone_id.clone(),
            action: request.action.clone(),
            capability: request.capability,
            target: request.target.clone(),
            allowed: true,
            reason_code: "sandbox.allowed".to_string(),
        };
        self.record_sandbox_decision(allowed_decision);

        self.operation_seq = next_operation_seq;
        let operation_id = format!("{}-op-{:016x}", self.config.host_id, self.operation_seq);

        Ok(ConnectorOperationEnvelope {
            operation_id,
            correlation_id: request.correlation_id,
            host_id: self.config.host_id.clone(),
            zone_id: self.config.sandbox.zone_id.clone(),
            protocol_version: self.config.protocol_version,
            action: request.action,
            capability: request.capability,
            target: request.target,
            decision_id,
            issued_at_ms: now_ms,
        })
    }

    /// Render the current health/liveness snapshot.
    #[must_use]
    pub fn health_snapshot(&self, now_ms: u64) -> ConnectorHealthSnapshot {
        let heartbeat_age_ms = self
            .last_heartbeat_at_ms
            .map(|last| now_ms.saturating_sub(last));
        let live_deadline_ms = self.config.heartbeat_interval_ms.saturating_mul(3);
        let is_live = matches!(
            self.state.phase(),
            ConnectorLifecyclePhase::Running | ConnectorLifecyclePhase::Degraded
        ) && heartbeat_age_ms.is_some_and(|age| age <= live_deadline_ms);
        let is_ready = self.state.phase() == ConnectorLifecyclePhase::Running
            && is_live
            && self
                .latest_usage
                .is_none_or(|usage| usage.exceeded_dimension(&self.config.budgets).is_none());

        ConnectorHealthSnapshot {
            host_id: self.config.host_id.clone(),
            protocol_version: self.config.protocol_version,
            phase: self.state.phase(),
            is_live,
            is_ready,
            last_transition_at_ms: self.last_transition_at_ms,
            last_heartbeat_at_ms: self.last_heartbeat_at_ms,
            heartbeat_age_ms,
            active_failures: self.active_failures,
            sandbox_zone_id: self.config.sandbox.zone_id.clone(),
            budgets: self.config.budgets,
            latest_usage: self.latest_usage,
            last_failure: self.state.failure().cloned(),
            last_sandbox_decision: self.sandbox_decisions.back().cloned(),
        }
    }

    fn record_sandbox_decision(&mut self, decision: ConnectorSandboxDecision) {
        self.sandbox_decisions.push_back(decision);
        while self.sandbox_decisions.len() > SANDBOX_DECISION_HISTORY_CAPACITY {
            self.sandbox_decisions.pop_front();
        }
    }

    fn transition(&mut self, at_ms: u64, to_state: ConnectorLifecycleState, reason_code: &str) {
        let transition = ConnectorLifecycleTransition {
            at_ms,
            from: self.state.phase(),
            to: to_state.phase(),
            reason_code: reason_code.to_string(),
        };
        self.state = to_state;
        self.last_transition_at_ms = at_ms;
        self.transition_history.push_back(transition);
        while self.transition_history.len() > TRANSITION_HISTORY_CAPACITY {
            self.transition_history.pop_front();
        }
    }
}

fn ensure_reason_code(reason_code: &str) -> Result<(), ConnectorHostRuntimeError> {
    if reason_code.trim().is_empty() {
        return Err(ConnectorHostRuntimeError::InvalidConfig {
            reason: "reason_code must not be empty".to_string(),
        });
    }
    Ok(())
}

fn infer_capability_from_action(action: &str) -> ConnectorCapability {
    if action.contains("stream") {
        return ConnectorCapability::StreamEvents;
    }
    if action.contains("state") || action.contains("status") || action.contains("ping") {
        return ConnectorCapability::ReadState;
    }
    ConnectorCapability::Invoke
}

/// Deterministic connector-runtime error taxonomy.
#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ConnectorHostRuntimeError {
    #[error("invalid config: {reason}")]
    InvalidConfig { reason: String },
    #[error("invalid lifecycle transition {from} -> {to}: {reason}")]
    InvalidTransition {
        from: ConnectorLifecyclePhase,
        to: ConnectorLifecyclePhase,
        reason: String,
    },
    #[error("startup probe failed ({class}): {reason_code}")]
    StartupProbeFailed {
        class: ConnectorFailureClass,
        reason_code: String,
    },
    #[error("resource budget exceeded: {dimension}")]
    BudgetExceeded { dimension: String },
    #[error("host is not runnable in phase {phase}")]
    HostNotRunnable { phase: ConnectorLifecyclePhase },
    #[error("sandbox violation in zone {zone_id} for capability {capability}: {reason_code}")]
    SandboxViolation {
        zone_id: String,
        capability: ConnectorCapability,
        reason_code: String,
    },
    #[error("protocol upgrade rejected: {reason}")]
    ProtocolUpgradeRejected { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_request_ids_are_distinct_wire_uuids() {
        let first = delivery_request_id();
        let second = delivery_request_id();
        assert!(valid_fcp_request_id(&first));
        assert!(valid_fcp_request_id(&second));
        assert_ne!(first, second);
        assert_eq!(first.as_bytes()[14], b'4');
        assert!(b"89ab".contains(&first.as_bytes()[19]));
    }

    #[cfg(unix)]
    #[test]
    fn delivery_session_durable_dispatch_cas_prevents_restart_replay() {
        use crate::connector_reliability::ConnectorDeliveryState as State;
        crate::storage::run_storage_async_test(async {
            let temp = tempfile::tempdir().unwrap();
            let storage = crate::storage::StorageHandle::new(
                temp.path().join("delivery.db").to_str().unwrap(),
            )
            .await
            .unwrap();
            storage
                .upsert_pane(crate::storage::PaneRecord {
                    pane_id: 1,
                    pane_uuid: None,
                    domain: "delivery-test".into(),
                    window_id: None,
                    tab_id: None,
                    title: None,
                    cwd: None,
                    tty_name: None,
                    first_seen_at: 1,
                    last_seen_at: 1,
                    observed: true,
                    ignore_reason: None,
                    last_decision_at: None,
                })
                .await
                .unwrap();
            let cx = crate::cx::for_request();
            let mut safety = crate::config::SafetyConfig::default();
            safety.connector_transport = Some(FcpTransportConfig {
                endpoint: "http://127.0.0.1:1/".into(),
                capability_token_file: temp.path().join("absent-capability"),
                admin_token_file: temp.path().join("absent-admin"),
                request_timeout_ms: 100,
                max_payload_bytes: 4096,
                max_pending_actions: 8,
                max_retained_actions: 16,
                max_ingress_batch: 4,
                outbound: vec![FcpOutboundRoute {
                    rule: crate::connector_outbound_bridge::OutboundRoutingRule {
                        rule_id: "test-route".into(),
                        source_filter: None,
                        event_type_prefix: Some("pattern.test".into()),
                        min_severity: None,
                        target_connector: "fcp.sqlite".into(),
                        action_kind: crate::connector_outbound_bridge::ConnectorActionKind::Invoke,
                        enabled: true,
                        priority: 0,
                    },
                    invoke: introspection_operation(),
                    correlation_input_pointer: None,
                }],
                inbound: vec![FcpInboundSubscription {
                    subscription_id: "test-poll".into(),
                    invoke: FcpOperation {
                        input: serde_json::json!({"cursor": 0}),
                        ..introspection_operation()
                    },
                    pane_id: 1,
                    records_pointer: "/records".into(),
                    identity_pointer: "/id".into(),
                    cursor_input_pointer: "/cursor".into(),
                    poll_interval_ms: 1000,
                }],
            });
            let bus = std::sync::Arc::new(crate::events::EventBus::new(8));
            let mut session = FcpDeliverySession::open(
                &cx,
                &safety,
                storage.clone(),
                std::sync::Arc::clone(&bus),
            )
            .await
            .unwrap();
            let event_id = storage
                .record_event(crate::storage::StoredEvent {
                    id: 0,
                    pane_id: 1,
                    rule_id: "test.rule".into(),
                    agent_type: "unknown".into(),
                    event_type: "test".into(),
                    severity: "info".into(),
                    confidence: 1.0,
                    extracted: None,
                    matched_text: Some("source-private-canary".into()),
                    segment_id: None,
                    detected_at: 1,
                    dedupe_key: None,
                    handled_at: None,
                    handled_by_workflow_id: None,
                    handled_status: None,
                })
                .await
                .unwrap();
            session.admit_events(&cx).await.unwrap();
            assert_eq!(session.cursor, event_id);
            let admitted = storage
                .get_connector_outbox_with_cx(&cx, &session.generation, false, 8)
                .await
                .unwrap();
            assert_eq!(admitted.len(), 1);
            let entry = &admitted[0];
            assert_eq!(entry.state, State::Admitted);
            assert_eq!(entry.source_event_id, event_id);
            assert!(
                !serde_json::to_string(entry)
                    .unwrap()
                    .contains("source-private-canary")
            );
            session.admit_events(&cx).await.unwrap();
            assert_eq!(
                storage
                    .get_connector_outbox_with_cx(&cx, &session.generation, false, 8)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            let dispatched = session
                .transition(&cx, entry, State::Dispatched, None, None)
                .await
                .unwrap();
            assert_eq!(dispatched.revision, 1);
            assert!(
                session
                    .transition(&cx, entry, State::Dispatched, None, None)
                    .await
                    .is_err(),
                "stale ownership must not authorize a second send"
            );
            drop(session);
            let mut reopened = FcpDeliverySession::open(&cx, &safety, storage.clone(), bus)
                .await
                .unwrap();
            assert_eq!(reopened.cursor, event_id);
            // No credentials or server exist. Scanning must neither issue the
            // request again nor revoke a still-running owner's settlement CAS.
            reopened.dispatch_pending(&cx).await.unwrap();
            let rows = storage
                .get_connector_outbox_with_cx(&cx, &reopened.generation, false, 8)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].state, State::Dispatched);
            assert_eq!(rows[0].revision, dispatched.revision);
            assert!(
                reopened
                    .transition(&cx, &rows[0], State::Dispatched, None, None)
                    .await
                    .is_err()
            );
            let acknowledged = FcpAcknowledgement {
                receipt_id: fcp_identity_hash("receipt"),
                receipt_hash: fcp_identity_hash("receipt-content"),
                success: true,
                result: None,
            };
            assert_eq!(
                reopened
                    .transition(
                        &cx,
                        &dispatched,
                        State::Completed,
                        Some(&acknowledged),
                        None
                    )
                    .await
                    .unwrap()
                    .state,
                State::Completed
            );
            // A changed sandbox is a different admission generation. Denied
            // configured bytes must not be retained, dispatched or silently
            // borrow the preceding owner's approval.
            safety
                .connector_host_runtime
                .sandbox
                .capability_envelope
                .allowed_capabilities = vec![ConnectorCapability::Invoke];
            let mut denied = FcpDeliverySession::open(
                &cx,
                &safety,
                storage.clone(),
                std::sync::Arc::clone(&reopened.event_bus),
            )
            .await
            .unwrap();
            assert_ne!(denied.generation, reopened.generation);
            let mut source = storage
                .get_events_stream_with_cx(
                    &cx,
                    crate::storage::EventStreamQuery {
                        after_id: Some(0),
                        limit: Some(1),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .remove(0);
            source.id = 0;
            source.dedupe_key = None;
            storage.record_event(source.clone()).await.unwrap();
            denied.admit_events(&cx).await.unwrap();
            let denied_rows = storage
                .get_connector_outbox_with_cx(&cx, &denied.generation, false, 8)
                .await
                .unwrap();
            assert_eq!(denied_rows.len(), 1);
            assert_eq!(denied_rows[0].state, State::Rejected);
            assert!(denied_rows[0].invoke.input.is_null());
            denied.dispatch_pending(&cx).await.unwrap();

            // Backoff on the oldest admitted row must not hide a later ready
            // row. The serialized writer also rejects dispatch before due_at.
            use crate::connector_reliability::{
                ConnectorStorageMutation as Mutation, ConnectorStorageOutcome as Outcome,
            };
            reopened.admit_events(&cx).await.unwrap();
            let ready = storage
                .dispatchable_connector_outbox_with_cx(&cx, &reopened.generation, 1)
                .await
                .unwrap();
            assert_eq!(ready.len(), 1);
            let now_ms = delivery_now_ms().unwrap();
            let Outcome::Transitioned(delayed) = storage
                .connector_mutation_with_cx(
                    &cx,
                    Mutation::Reschedule {
                        key: ready[0].key.clone(),
                        expected_revision: ready[0].revision,
                        now_ms,
                        due_at_ms: now_ms + 60_000,
                    },
                )
                .await
                .unwrap()
            else {
                panic!("backoff was not committed");
            };
            assert!(
                reopened
                    .transition(&cx, &delayed, State::Dispatched, None, None)
                    .await
                    .is_err()
            );
            assert!(
                storage
                    .dispatchable_connector_outbox_with_cx(&cx, &reopened.generation, 1)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let next_event = storage.record_event(source.clone()).await.unwrap();
            reopened.admit_events(&cx).await.unwrap();
            let ready = storage
                .dispatchable_connector_outbox_with_cx(&cx, &reopened.generation, 1)
                .await
                .unwrap();
            assert_eq!(ready.len(), 1);
            assert_eq!(ready[0].source_event_id, next_event);

            // An individually valid transport input can still exceed the
            // durable row budget after its ownership/receipt envelope is added.
            // It must become a terminal rejection, never wedge the event cursor.
            let mut oversized_safety = safety.clone();
            oversized_safety.connector_host_runtime = ConnectorHostConfig::default();
            let oversized_config = oversized_safety.connector_transport.as_mut().unwrap();
            oversized_config.max_payload_bytes = 1_048_576;
            oversized_config.outbound[0].invoke.input =
                serde_json::json!({"query": "x".repeat(1_046_000)});
            let mut oversized = FcpDeliverySession::open(
                &cx,
                &oversized_safety,
                storage.clone(),
                std::sync::Arc::clone(&reopened.event_bus),
            )
            .await
            .unwrap();
            let oversized_event_id = storage.record_event(source.clone()).await.unwrap();
            oversized.admit_events(&cx).await.unwrap();
            assert_eq!(oversized.cursor, oversized_event_id);
            let rejected = storage
                .get_connector_outbox_with_cx(&cx, &oversized.generation, false, 8)
                .await
                .unwrap();
            assert_eq!(rejected.len(), 1);
            assert_eq!(rejected[0].state, State::Rejected);
            assert_eq!(
                rejected[0].reason_code.as_deref(),
                Some("connector_admission_payload_too_large")
            );
            assert_eq!(rejected[0].invoke.input, serde_json::Value::Null);
            drop(oversized);

            // Real SQLite ingress is retained until there is a live consumer.
            // This covers storage/publication, not a live-provider poll.
            let subscription_key = fcp_identity_hash("durable-ingress-test");
            source.dedupe_key = Some(format!("connector:{subscription_key}:1"));
            source.event_type = "connector.poll".into();
            let Outcome::Ingested(outcomes) = storage
                .connector_mutation_with_cx(
                    &cx,
                    Mutation::Ingest {
                        subscription_key,
                        expected_cursor: 0,
                        records: vec![(1, source)],
                    },
                )
                .await
                .unwrap()
            else {
                panic!("ingress commit missing");
            };
            assert_eq!(outcomes.len(), 1);
            assert!(outcomes[0].was_inserted());
            reopened.deliver_ingress(&cx).await.unwrap();
            assert_eq!(
                storage
                    .pending_connector_ingress_with_cx(&cx)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            let mut subscriber = reopened.event_bus.subscribe_detections();
            reopened.deliver_ingress(&cx).await.unwrap();
            let Some(Ok(crate::events::Event::PatternDetected {
                event_id: Some(published_id),
                ..
            })) = subscriber.try_recv()
            else {
                panic!("durably committed ingress was not published");
            };
            assert_eq!(published_id, outcomes[0].event_id());
            assert!(
                storage
                    .pending_connector_ingress_with_cx(&cx)
                    .await
                    .unwrap()
                    .is_empty()
            );
            reopened.deliver_ingress(&cx).await.unwrap();
            assert!(subscriber.try_recv().is_none());
            drop(denied);
            drop(reopened);
            storage.shutdown().await.unwrap();
        });
    }

    fn introspection_operation() -> FcpOperation {
        FcpOperation {
            connector_id: "fcp.sqlite".to_string(),
            operation: "sqlite.query".to_string(),
            capability: ConnectorCapability::ReadState,
            target: None,
            input: serde_json::json!({}),
        }
    }

    #[test]
    fn fcp_wire_introspection_accepts_host_envelope() {
        // Shape from fcp-host discovery.rs at 465f54120a806a1160b918e775710a6afd9a15bf.
        // This is parser coverage, not a live host or provider receipt.
        let response = serde_json::json!({
            "connector": {"id": "fcp.sqlite"},
            "tools": [{"name": "sqlite.execute"}, {"name": "sqlite.query"}],
            "introspection": {"operations": [{"id": "sqlite.query"}]},
        });
        assert_eq!(
            validate_fcp_introspection(&response, &introspection_operation()),
            Ok(())
        );
    }

    #[test]
    fn fcp_wire_introspection_binds_connector_identity() {
        for connector in [
            serde_json::json!({"id": "fcp.other"}),
            serde_json::json!({"id": 1}),
            serde_json::json!({}),
            serde_json::json!("fcp.sqlite"),
            serde_json::Value::Null,
        ] {
            let response = serde_json::json!({
                "connector": connector, "tools": [{"name": "sqlite.query"}],
            });
            assert_eq!(
                validate_fcp_introspection(&response, &introspection_operation()),
                Err(FcpTransportError::ProtocolInvalid)
            );
        }
    }

    #[test]
    fn fcp_wire_introspection_rejects_legacy_root_operations() {
        let response = serde_json::json!({
            "connector": {"id": "fcp.sqlite"},
            "operations": [{"id": "sqlite.query"}],
        });
        assert_eq!(
            validate_fcp_introspection(&response, &introspection_operation()),
            Err(FcpTransportError::ProtocolInvalid)
        );
    }

    #[test]
    fn fcp_wire_introspection_rejects_malformed_tools_even_after_match() {
        for malformed in [
            serde_json::json!({"name": 1}),
            serde_json::json!({"id": "sqlite.query"}),
            serde_json::Value::Null,
        ] {
            let response = serde_json::json!({
                "connector": {"id": "fcp.sqlite"},
                "tools": [{"name": "sqlite.query"}, malformed],
            });
            assert_eq!(
                validate_fcp_introspection(&response, &introspection_operation()),
                Err(FcpTransportError::ProtocolInvalid)
            );
        }
    }

    #[test]
    fn fcp_wire_introspection_requires_requested_tool() {
        for tools in [
            serde_json::json!([]),
            serde_json::json!([{"name": "sqlite.execute"}]),
        ] {
            let response = serde_json::json!({"connector": {"id": "fcp.sqlite"}, "tools": tools});
            assert_eq!(
                validate_fcp_introspection(&response, &introspection_operation()),
                Err(FcpTransportError::Unavailable)
            );
        }
    }

    #[test]
    fn fcp_wire_request_id_accepts_hyphenated_uuid() {
        assert!(valid_fcp_request_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(valid_fcp_request_id("550E8400-E29B-41D4-A716-446655440000"));
    }

    #[cfg(unix)]
    #[test]
    fn fcp_wire_prepare_rejects_label_before_async_credential_work() {
        use std::future::Future;

        let mut operation = introspection_operation();
        operation.input = serde_json::json!({"cursor": 0});
        let config = FcpTransportConfig {
            endpoint: "http://127.0.0.1:9".to_string(),
            // A character device can never be an authorized regular secret
            // file. This path must not be inspected for an invalid request ID.
            capability_token_file: "/dev/null".into(),
            admin_token_file: "/dev/null".into(),
            request_timeout_ms: 30_000,
            max_payload_bytes: 4096,
            max_pending_actions: 1,
            max_retained_actions: 1,
            max_ingress_batch: 1,
            outbound: Vec::new(),
            inbound: vec![FcpInboundSubscription {
                subscription_id: "poll".to_string(),
                invoke: operation.clone(),
                pane_id: 0,
                records_pointer: "/rows".to_string(),
                identity_pointer: "/id".to_string(),
                cursor_input_pointer: "/cursor".to_string(),
                poll_interval_ms: 100,
            }],
        };
        let client = FcpHostClient::new(config, &ConnectorHostConfig::default()).unwrap();
        let cx = crate::cx::Cx::for_testing();
        let key = "a".repeat(64);
        // No executor or I/O task is started: invalid identity must settle on
        // its first poll before spawn_blocking or any credential/network work.
        let mut future = std::pin::pin!(client.prepare_invocation(
            &cx,
            client.operation_deadline().unwrap(),
            &operation,
            "request-1",
            &key,
        ));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            future.as_mut().poll(&mut context),
            std::task::Poll::Ready(Err(FcpTransportError::InvalidConfig))
        ));
    }

    #[test]
    fn fcp_wire_request_id_rejects_labels_and_malformed_uuid() {
        for value in [
            "request-1",
            "",
            "550e8400e29b41d4a716446655440000",
            "550e8400_e29b-41d4-a716-446655440000",
            "550e8400-e29b-41d4-a716-44665544000g",
            "550e8400-e29b-41d4-a716-44665544000",
            "550e8400-e29b-41d4-a716-4466554400000",
            "é50e8400-e29b-41d4-a716-44665544000",
        ] {
            assert!(!valid_fcp_request_id(value), "accepted {value:?}");
        }
    }

    fn usage_within_budget() -> ConnectorRuntimeUsage {
        ConnectorRuntimeUsage {
            cpu_millis_in_window: 120,
            memory_bytes: 128 * 1024 * 1024,
            io_bytes_in_window: 512 * 1024,
            inflight_ops: 4,
        }
    }

    #[test]
    fn connector_host_runtime_start_stop_restart_happy_path() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(100).unwrap();
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Running);

        runtime.stop(200).unwrap();
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Stopped);

        runtime.restart(300).unwrap();
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Running);
    }

    #[test]
    fn connector_host_runtime_startup_probe_failure_sets_failed_state() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        let err = runtime
            .start_with_probe(
                100,
                StartupProbeResult::failed(ConnectorFailureClass::Network, "dial_failed"),
            )
            .unwrap_err();
        assert_eq!(
            err,
            ConnectorHostRuntimeError::StartupProbeFailed {
                class: ConnectorFailureClass::Network,
                reason_code: "dial_failed".to_string(),
            }
        );
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Failed);
        assert_eq!(runtime.health_snapshot(150).active_failures, 1);
    }

    #[test]
    fn connector_host_runtime_budget_exceedance_forces_degraded_state() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(100).unwrap();

        let err = runtime
            .observe_usage(
                120,
                ConnectorRuntimeUsage {
                    cpu_millis_in_window: 900,
                    ..usage_within_budget()
                },
            )
            .unwrap_err();
        assert_eq!(
            err,
            ConnectorHostRuntimeError::BudgetExceeded {
                dimension: "cpu_millis_per_second".to_string(),
            }
        );
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Degraded);
    }

    #[test]
    fn connector_host_runtime_degraded_recovers_when_usage_recovers() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(100).unwrap();
        let _ = runtime.observe_usage(
            110,
            ConnectorRuntimeUsage {
                memory_bytes: 1024 * 1024 * 1024,
                ..usage_within_budget()
            },
        );
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Degraded);

        runtime.observe_usage(140, usage_within_budget()).unwrap();
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Running);
    }

    #[test]
    fn connector_host_runtime_upgrade_and_restart_updates_protocol_version() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(100).unwrap();
        runtime
            .upgrade_and_restart(
                200,
                ConnectorProtocolVersion::new(1, 1, 0),
                StartupProbeResult::healthy(),
            )
            .unwrap();

        assert_eq!(
            runtime.config().protocol_version,
            ConnectorProtocolVersion::new(1, 1, 0)
        );
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Running);
    }

    #[test]
    fn connector_host_runtime_upgrade_rejects_major_version_skew() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(100).unwrap();

        let err = runtime
            .upgrade_and_restart(
                200,
                ConnectorProtocolVersion::new(2, 0, 0),
                StartupProbeResult::healthy(),
            )
            .expect_err("breaking major protocol upgrade must require a negotiated cutover");

        assert!(
            matches!(
                err,
                ConnectorHostRuntimeError::ProtocolUpgradeRejected { .. }
            ),
            "expected ProtocolUpgradeRejected, got {err:?}"
        );
        assert_eq!(
            runtime.config().protocol_version,
            ConnectorProtocolVersion::default()
        );
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Running);
    }

    #[test]
    fn connector_host_runtime_operation_envelope_monotonic_and_versioned() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(100).unwrap();
        runtime.observe_usage(105, usage_within_budget()).unwrap();

        let op1 = runtime
            .build_operation_envelope(110, "connector.invoke", "corr-1")
            .unwrap();
        let op2 = runtime
            .build_operation_envelope(120, "connector.invoke", "corr-2")
            .unwrap();

        assert!(op1.operation_id < op2.operation_id);
        assert_eq!(op1.protocol_version, ConnectorProtocolVersion::new(1, 0, 0));
        assert_eq!(op2.protocol_version, ConnectorProtocolVersion::new(1, 0, 0));
        assert_eq!(op1.zone_id, "zone.default");
        assert_eq!(op1.capability, ConnectorCapability::Invoke);
        assert!(op1.decision_id < op2.decision_id);
    }

    #[test]
    fn request_validation_is_read_only_and_shares_authorization_predicates() {
        let mut config = ConnectorHostConfig::default();
        config.sandbox.capability_envelope = ConnectorCapabilityEnvelope {
            allowed_capabilities: vec![
                ConnectorCapability::Invoke,
                ConnectorCapability::FilesystemRead,
                ConnectorCapability::FilesystemWrite,
                ConnectorCapability::NetworkEgress,
                ConnectorCapability::ProcessExec,
            ],
            filesystem_read_prefixes: vec!["/model/safe".to_string()],
            filesystem_write_prefixes: vec!["/model/output".to_string()],
            network_allow_hosts: vec!["model.example".to_string()],
            allowed_exec_commands: vec!["model-command".to_string()],
        };
        let runtime = ConnectorHostRuntime::new(config).unwrap();
        for (capability, target, allowed) in [
            (ConnectorCapability::Invoke, None, true),
            (ConnectorCapability::SecretBroker, None, false),
            (
                ConnectorCapability::FilesystemRead,
                Some("/model/safe/input"),
                true,
            ),
            (
                ConnectorCapability::FilesystemRead,
                Some("/model/safe/../secret"),
                false,
            ),
            (
                ConnectorCapability::FilesystemWrite,
                Some("/model/output/file"),
                true,
            ),
            (
                ConnectorCapability::FilesystemWrite,
                Some("/model/output-other/file"),
                false,
            ),
            (
                ConnectorCapability::NetworkEgress,
                Some("model.example"),
                true,
            ),
            (
                ConnectorCapability::NetworkEgress,
                Some("model.example.attacker"),
                false,
            ),
            (
                ConnectorCapability::ProcessExec,
                Some("model-command"),
                true,
            ),
            (
                ConnectorCapability::ProcessExec,
                Some("model-command --extra"),
                false,
            ),
        ] {
            let mut request =
                ConnectorOperationRequest::new("model.action", "model-correlation", capability);
            request.target = target.map(str::to_string);
            let before = runtime.clone();
            let validation = runtime.validate_operation_request(&request);
            assert_eq!(validation.is_ok(), allowed, "{capability:?} {target:?}");
            assert_eq!(runtime, before);
            let mut dispatch = runtime.clone();
            dispatch.start(100).unwrap();
            let authorized = dispatch.authorize_operation(101, request);
            if allowed {
                assert!(authorized.is_ok());
                assert_eq!(dispatch.operation_seq, 1);
            } else {
                assert_eq!(authorized.unwrap_err(), validation.unwrap_err());
                assert_eq!(dispatch.operation_seq, 0);
            }
        }
        for (action, correlation) in [(" ", "model"), ("model.action", "\t")] {
            let request =
                ConnectorOperationRequest::new(action, correlation, ConnectorCapability::Invoke);
            assert!(matches!(
                runtime.validate_operation_request(&request),
                Err(ConnectorHostRuntimeError::InvalidConfig { .. })
            ));
        }
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Stopped);
        assert!(runtime.sandbox_decision_history().is_empty());
    }

    #[test]
    fn operation_sequence_exhaustion_preserves_host_and_sandbox_receipts() {
        for (operation_seq, sandbox_decision_seq) in [(u64::MAX, 0), (0, u64::MAX)] {
            let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
            runtime.start(100).unwrap();
            runtime.operation_seq = operation_seq;
            runtime.sandbox_decision_seq = sandbox_decision_seq;
            let before = runtime.clone();
            let request = ConnectorOperationRequest::new(
                "model.action",
                "model-correlation",
                ConnectorCapability::Invoke,
            );
            assert!(matches!(
                runtime.authorize_operation(101, request),
                Err(ConnectorHostRuntimeError::InvalidConfig { .. })
            ));
            assert_eq!(runtime, before);
        }
    }

    #[test]
    fn connector_host_runtime_health_liveness_heartbeat_timeout() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(1_000).unwrap();
        runtime.record_heartbeat(1_500).unwrap();
        let healthy = runtime.health_snapshot(3_000);
        assert!(healthy.is_live);

        // heartbeat timeout = 3 * 1000ms = 3000ms; age here is 3501ms.
        let stale = runtime.health_snapshot(5_001);
        assert!(!stale.is_live);
        assert!(!stale.is_ready);
    }

    #[test]
    fn connector_host_runtime_transition_history_is_bounded() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        for i in 0..100 {
            runtime.transition(i, ConnectorLifecycleState::Stopped, "test.transition");
        }
        assert_eq!(
            runtime.transition_history().len(),
            TRANSITION_HISTORY_CAPACITY
        );
    }

    #[test]
    fn connector_host_runtime_config_validation_rejects_zero_budget() {
        let mut config = ConnectorHostConfig::default();
        config.budgets.max_inflight_ops = 0;
        let err = ConnectorHostRuntime::new(config).unwrap_err();
        assert_eq!(
            err,
            ConnectorHostRuntimeError::InvalidConfig {
                reason: "max_inflight_ops must be > 0".to_string(),
            }
        );
    }

    #[test]
    fn connector_host_runtime_sandbox_denies_missing_capability_fail_closed() {
        let mut config = ConnectorHostConfig::default();
        config.sandbox.capability_envelope.allowed_capabilities =
            vec![ConnectorCapability::ReadState];
        let mut runtime = ConnectorHostRuntime::new(config).unwrap();
        runtime.start(100).unwrap();
        runtime.observe_usage(110, usage_within_budget()).unwrap();

        let err = runtime
            .authorize_operation(
                120,
                ConnectorOperationRequest::new(
                    "connector.invoke",
                    "corr-deny-capability",
                    ConnectorCapability::Invoke,
                ),
            )
            .unwrap_err();
        assert_eq!(
            err,
            ConnectorHostRuntimeError::SandboxViolation {
                zone_id: "zone.default".to_string(),
                capability: ConnectorCapability::Invoke,
                reason_code: "sandbox.denied.capability.invoke".to_string(),
            }
        );
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Failed);
        assert_eq!(runtime.sandbox_decision_history().len(), 1);
        assert!(
            !runtime
                .sandbox_decision_history()
                .last()
                .expect("expected denial decision")
                .allowed
        );
    }

    #[test]
    fn connector_host_runtime_sandbox_enforces_target_allowlists() {
        let mut config = ConnectorHostConfig::default();
        config.sandbox.capability_envelope.allowed_capabilities = vec![
            ConnectorCapability::Invoke,
            ConnectorCapability::NetworkEgress,
        ];
        config.sandbox.capability_envelope.network_allow_hosts =
            vec!["api.frankenterm.dev".to_string()];
        let mut runtime = ConnectorHostRuntime::new(config).unwrap();
        runtime.start(100).unwrap();
        runtime.observe_usage(105, usage_within_budget()).unwrap();

        let denied = runtime.authorize_operation(
            110,
            ConnectorOperationRequest::new(
                "connector.network.call",
                "corr-net-deny",
                ConnectorCapability::NetworkEgress,
            )
            .with_target("evil.example.com"),
        );
        assert_eq!(
            denied.unwrap_err(),
            ConnectorHostRuntimeError::SandboxViolation {
                zone_id: "zone.default".to_string(),
                capability: ConnectorCapability::NetworkEgress,
                reason_code: "sandbox.denied.target.network_egress".to_string(),
            }
        );

        runtime
            .restart_with_probe(120, StartupProbeResult::healthy())
            .unwrap();
        runtime.observe_usage(121, usage_within_budget()).unwrap();
        let allowed = runtime
            .authorize_operation(
                125,
                ConnectorOperationRequest::new(
                    "connector.network.call",
                    "corr-net-allow",
                    ConnectorCapability::NetworkEgress,
                )
                .with_target("api.frankenterm.dev"),
            )
            .unwrap();
        assert_eq!(allowed.target.as_deref(), Some("api.frankenterm.dev"));
        assert_eq!(allowed.capability, ConnectorCapability::NetworkEgress);

        let decisions = runtime.sandbox_decision_history();
        assert_eq!(decisions.len(), 2);
        assert_eq!(
            decisions.iter().filter(|decision| decision.allowed).count(),
            1
        );
    }

    #[test]
    fn connector_network_allowlist_matches_dns_hosts_case_insensitively() {
        let envelope = ConnectorCapabilityEnvelope {
            allowed_capabilities: vec![ConnectorCapability::NetworkEgress],
            filesystem_read_prefixes: Vec::new(),
            filesystem_write_prefixes: Vec::new(),
            network_allow_hosts: vec![
                "API.FRANKENTERM.DEV".to_string(),
                "*.Example.COM".to_string(),
            ],
            allowed_exec_commands: Vec::new(),
        };

        assert!(envelope.allows_target(
            ConnectorCapability::NetworkEgress,
            Some("api.frankenterm.dev")
        ));
        assert!(envelope.allows_target(
            ConnectorCapability::NetworkEgress,
            Some("worker.example.com")
        ));
        assert!(!envelope.allows_target(ConnectorCapability::NetworkEgress, Some("example.com")));
        assert!(!envelope.allows_target(ConnectorCapability::NetworkEgress, Some(".example.com")));
    }

    #[test]
    fn connector_host_runtime_sandbox_filesystem_prefix_is_boundary_safe() {
        let mut config = ConnectorHostConfig::default();
        config.sandbox.fail_closed = false;
        config.sandbox.capability_envelope.allowed_capabilities = vec![
            ConnectorCapability::Invoke,
            ConnectorCapability::FilesystemRead,
        ];
        config.sandbox.capability_envelope.filesystem_read_prefixes =
            vec!["/workspace/safe".to_string()];
        let mut runtime = ConnectorHostRuntime::new(config).unwrap();
        runtime.start(100).unwrap();
        runtime.observe_usage(105, usage_within_budget()).unwrap();

        let allowed = runtime
            .authorize_operation(
                110,
                ConnectorOperationRequest::new(
                    "connector.fs.read",
                    "corr-fs-allow",
                    ConnectorCapability::FilesystemRead,
                )
                .with_target("/workspace/safe/file.txt"),
            )
            .unwrap();
        assert_eq!(allowed.target.as_deref(), Some("/workspace/safe/file.txt"));

        let boundary_bypass = runtime
            .authorize_operation(
                120,
                ConnectorOperationRequest::new(
                    "connector.fs.read",
                    "corr-fs-boundary",
                    ConnectorCapability::FilesystemRead,
                )
                .with_target("/workspace/safe2/file.txt"),
            )
            .unwrap_err();
        assert!(matches!(
            boundary_bypass,
            ConnectorHostRuntimeError::SandboxViolation { .. }
        ));

        let traversal_bypass = runtime
            .authorize_operation(
                130,
                ConnectorOperationRequest::new(
                    "connector.fs.read",
                    "corr-fs-traversal",
                    ConnectorCapability::FilesystemRead,
                )
                .with_target("/workspace/safe/../secrets.txt"),
            )
            .unwrap_err();
        assert!(matches!(
            traversal_bypass,
            ConnectorHostRuntimeError::SandboxViolation { .. }
        ));
    }

    #[test]
    fn connector_host_runtime_sandbox_decision_history_is_bounded() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(1).unwrap();
        runtime.observe_usage(2, usage_within_budget()).unwrap();

        for index in 0..200_u64 {
            let _ = runtime.authorize_operation(
                10 + index,
                ConnectorOperationRequest::new(
                    format!("connector.invoke.{index}"),
                    format!("corr-{index}"),
                    ConnectorCapability::Invoke,
                ),
            );
        }
        assert_eq!(
            runtime.sandbox_decision_history().len(),
            SANDBOX_DECISION_HISTORY_CAPACITY
        );
    }

    // ========================================================================
    // ConnectorProtocolVersion
    // ========================================================================

    #[test]
    fn protocol_version_display() {
        let v = ConnectorProtocolVersion::new(2, 3, 1);
        assert_eq!(v.to_string(), "2.3.1");
    }

    #[test]
    fn protocol_version_default() {
        let v = ConnectorProtocolVersion::default();
        assert_eq!(v.to_string(), "1.0.0");
    }

    #[test]
    fn protocol_version_serde_roundtrip() {
        let v = ConnectorProtocolVersion::new(3, 7, 11);
        let json = serde_json::to_string(&v).unwrap();
        let back: ConnectorProtocolVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn protocol_version_ordering() {
        let v1 = ConnectorProtocolVersion::new(1, 0, 0);
        let v2 = ConnectorProtocolVersion::new(1, 1, 0);
        let v3 = ConnectorProtocolVersion::new(2, 0, 0);
        assert!(v1 < v2);
        assert!(v2 < v3);
    }

    #[test]
    fn protocol_version_same_major_upgrade_contract() {
        let current = ConnectorProtocolVersion::new(1, 1, 3);

        assert!(ConnectorProtocolVersion::new(1, 1, 4).is_same_major_upgrade_from(current));
        assert!(ConnectorProtocolVersion::new(1, 2, 0).is_same_major_upgrade_from(current));
        assert!(!ConnectorProtocolVersion::new(1, 1, 3).is_same_major_upgrade_from(current));
        assert!(!ConnectorProtocolVersion::new(1, 1, 2).is_same_major_upgrade_from(current));
        assert!(!ConnectorProtocolVersion::new(2, 0, 0).is_same_major_upgrade_from(current));
    }

    // ========================================================================
    // ConnectorFailureClass
    // ========================================================================

    #[test]
    fn failure_class_as_str_and_display() {
        let classes = [
            (ConnectorFailureClass::Auth, "auth"),
            (ConnectorFailureClass::Quota, "quota"),
            (ConnectorFailureClass::Network, "network"),
            (ConnectorFailureClass::Policy, "policy"),
            (ConnectorFailureClass::Validation, "validation"),
            (ConnectorFailureClass::Timeout, "timeout"),
            (ConnectorFailureClass::Unknown, "unknown"),
        ];
        for (class, expected) in &classes {
            assert_eq!(class.as_str(), *expected);
            assert_eq!(class.to_string(), *expected);
        }
    }

    #[test]
    fn failure_class_serde_roundtrip() {
        let classes = [
            ConnectorFailureClass::Auth,
            ConnectorFailureClass::Quota,
            ConnectorFailureClass::Network,
            ConnectorFailureClass::Policy,
            ConnectorFailureClass::Validation,
            ConnectorFailureClass::Timeout,
            ConnectorFailureClass::Unknown,
        ];
        for class in &classes {
            let json = serde_json::to_string(class).unwrap();
            let back: ConnectorFailureClass = serde_json::from_str(&json).unwrap();
            assert_eq!(*class, back);
        }
    }

    // ========================================================================
    // ConnectorRuntimeBudgets
    // ========================================================================

    #[test]
    fn budgets_default_values() {
        let b = ConnectorRuntimeBudgets::default();
        assert_eq!(b.cpu_millis_per_second, 750);
        assert_eq!(b.memory_bytes, 512 * 1024 * 1024);
        assert_eq!(b.io_bytes_per_second, 16 * 1024 * 1024);
        assert_eq!(b.max_inflight_ops, 256);
        assert!(b.validate().is_ok());
    }

    #[test]
    fn budgets_validate_rejects_each_zero_field() {
        let base = ConnectorRuntimeBudgets::default();

        let zero_cpu = ConnectorRuntimeBudgets {
            cpu_millis_per_second: 0,
            ..base
        };
        assert!(zero_cpu.validate().is_err());

        let zero_mem = ConnectorRuntimeBudgets {
            memory_bytes: 0,
            ..base
        };
        assert!(zero_mem.validate().is_err());

        let zero_io = ConnectorRuntimeBudgets {
            io_bytes_per_second: 0,
            ..base
        };
        assert!(zero_io.validate().is_err());

        let zero_ops = ConnectorRuntimeBudgets {
            max_inflight_ops: 0,
            ..base
        };
        assert!(zero_ops.validate().is_err());
    }

    #[test]
    fn budgets_serde_roundtrip() {
        let b = ConnectorRuntimeBudgets {
            cpu_millis_per_second: 500,
            memory_bytes: 1024,
            io_bytes_per_second: 2048,
            max_inflight_ops: 10,
        };
        let json = serde_json::to_string(&b).unwrap();
        let back: ConnectorRuntimeBudgets = serde_json::from_str(&json).unwrap();
        assert_eq!(b, back);
    }

    // ========================================================================
    // ConnectorCapability
    // ========================================================================

    #[test]
    fn capability_serde_roundtrip_all_variants() {
        let caps = [
            ConnectorCapability::Invoke,
            ConnectorCapability::ReadState,
            ConnectorCapability::StreamEvents,
            ConnectorCapability::FilesystemRead,
            ConnectorCapability::FilesystemWrite,
            ConnectorCapability::NetworkEgress,
            ConnectorCapability::SecretBroker,
            ConnectorCapability::ProcessExec,
        ];
        for cap in &caps {
            let json = serde_json::to_string(cap).unwrap();
            let back: ConnectorCapability = serde_json::from_str(&json).unwrap();
            assert_eq!(*cap, back);
        }
    }

    // ========================================================================
    // ConnectorLifecyclePhase and ConnectorLifecycleState
    // ========================================================================

    #[test]
    fn lifecycle_phase_serde_roundtrip() {
        let phases = [
            ConnectorLifecyclePhase::Stopped,
            ConnectorLifecyclePhase::Starting,
            ConnectorLifecyclePhase::Running,
            ConnectorLifecyclePhase::Degraded,
            ConnectorLifecyclePhase::Failed,
        ];
        for phase in &phases {
            let json = serde_json::to_string(phase).unwrap();
            let back: ConnectorLifecyclePhase = serde_json::from_str(&json).unwrap();
            assert_eq!(*phase, back);
        }
    }

    // ========================================================================
    // ConnectorHostRuntimeError Display
    // ========================================================================

    #[test]
    fn error_display_formats() {
        let errors: Vec<ConnectorHostRuntimeError> = vec![
            ConnectorHostRuntimeError::InvalidConfig {
                reason: "bad config".to_string(),
            },
            ConnectorHostRuntimeError::InvalidTransition {
                from: ConnectorLifecyclePhase::Stopped,
                to: ConnectorLifecyclePhase::Running,
                reason: "not ready".to_string(),
            },
            ConnectorHostRuntimeError::StartupProbeFailed {
                class: ConnectorFailureClass::Timeout,
                reason_code: "probe_timeout".to_string(),
            },
            ConnectorHostRuntimeError::BudgetExceeded {
                dimension: "cpu".to_string(),
            },
        ];
        for err in &errors {
            let display = err.to_string();
            assert_ne!(display, "");
        }
    }

    // ========================================================================
    // ConnectorOperationRequest
    // ========================================================================

    #[test]
    fn operation_request_constructor() {
        let req = ConnectorOperationRequest::new(
            "connector.invoke.test",
            "corr-abc",
            ConnectorCapability::Invoke,
        );
        assert_eq!(req.action, "connector.invoke.test");
        assert_eq!(req.correlation_id, "corr-abc");
        assert_eq!(req.capability, ConnectorCapability::Invoke);
        assert!(req.target.is_none());
    }

    #[test]
    fn operation_request_with_target() {
        let req = ConnectorOperationRequest::new(
            "connector.read_state",
            "corr-xyz",
            ConnectorCapability::ReadState,
        )
        .with_target("pane-42");
        assert_eq!(req.target.as_deref(), Some("pane-42"));
    }
}
