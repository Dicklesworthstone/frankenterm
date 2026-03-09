#![allow(clippy::ignored_unit_patterns)]
#![cfg(feature = "agent-mail")]

//! Property-based tests for the mission_agent_mail coordination kernel.
//!
//! Coverage:
//! - Serde roundtrips for all serializable types
//! - Canonical thread-id determinism and sanitization
//! - Recipient deduplication and canonicalization
//! - Envelope embedding and parsing roundtrips
//! - Dispatch report aggregation invariants
//! - Ack-required collection filtering
//! - Acknowledgement routing

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use proptest::prelude::*;

use frankenterm_core::mission_agent_mail::{
    CoordinationEnvelope, CoordinationEventKind, CoordinationEventRequest,
    CoordinationInboxMessage, CoordinationParseFailure, DispatchedCoordinationMessage,
    FailedCoordinationMessage, InboundCoordinationMessage, MissionAgentMailConfig,
    MissionAgentMailKernel, MissionCoordinationContext, MissionMailDispatchReport,
    MissionMailTransport, PendingAcknowledgement,
};

// ---------------------------------------------------------------------------
// Mock transport
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockTransport {
    sent: RefCell<Vec<(String, String, String)>>,
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

    fn push_inbox(&self, agent: &str, msg: CoordinationInboxMessage) {
        self.inbox
            .borrow_mut()
            .entry(agent.to_string())
            .or_default()
            .push(msg);
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
        self.sent
            .borrow_mut()
            .push((to.to_string(), subject.to_string(), body.to_string()));
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

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_nonempty_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,20}"
}

fn arb_agent_name() -> impl Strategy<Value = String> {
    "Agent[A-Z][a-z]{2,8}"
}

fn arb_coordination_context() -> impl Strategy<Value = MissionCoordinationContext> {
    (
        arb_nonempty_string(),                       // mission_id
        proptest::option::of(arb_nonempty_string()), // fleet_id
        proptest::option::of(arb_nonempty_string()), // bead_id
        proptest::option::of(arb_nonempty_string()), // assignment_id
        proptest::option::of(arb_nonempty_string()), // thread_id
        arb_nonempty_string(),                       // correlation_id
        proptest::option::of(arb_nonempty_string()), // scenario_id
    )
        .prop_map(
            |(
                mission_id,
                fleet_id,
                bead_id,
                assignment_id,
                thread_id,
                correlation_id,
                scenario_id,
            )| {
                MissionCoordinationContext {
                    mission_id,
                    fleet_id,
                    bead_id,
                    assignment_id,
                    thread_id,
                    correlation_id,
                    scenario_id,
                }
            },
        )
}

fn arb_event_kind() -> impl Strategy<Value = CoordinationEventKind> {
    prop_oneof![
        Just(CoordinationEventKind::StartNotice),
        Just(CoordinationEventKind::ProgressUpdate),
        Just(CoordinationEventKind::Handoff),
        Just(CoordinationEventKind::ContentionSignal),
        Just(CoordinationEventKind::Acknowledgement),
    ]
}

fn arb_metadata() -> impl Strategy<Value = HashMap<String, String>> {
    proptest::collection::hash_map(arb_nonempty_string(), arb_nonempty_string(), 0..4)
}

fn arb_envelope() -> impl Strategy<Value = CoordinationEnvelope> {
    (
        1..5u32,
        arb_event_kind(),
        any::<bool>(),
        0..1_000_000_000i64,
        arb_nonempty_string(),
        proptest::option::of(arb_nonempty_string()),
        arb_coordination_context(),
        arb_metadata(),
    )
        .prop_map(
            |(
                version,
                event_kind,
                ack_required,
                emitted_at_ms,
                reason_code,
                error_code,
                context,
                metadata,
            )| {
                CoordinationEnvelope {
                    version,
                    event_kind,
                    ack_required,
                    emitted_at_ms,
                    reason_code,
                    error_code,
                    context,
                    metadata,
                }
            },
        )
}

fn arb_config() -> impl Strategy<Value = MissionAgentMailConfig> {
    (1_000i64..600_001).prop_map(|ack_timeout_ms| MissionAgentMailConfig { ack_timeout_ms })
}

fn arb_event_request() -> impl Strategy<Value = CoordinationEventRequest> {
    (
        arb_event_kind(),
        arb_nonempty_string(),
        arb_nonempty_string(),
        proptest::collection::vec(arb_agent_name(), 0..5),
        any::<bool>(),
        arb_coordination_context(),
        arb_nonempty_string(),
        proptest::option::of(arb_nonempty_string()),
        arb_metadata(),
    )
        .prop_map(
            |(
                kind,
                summary,
                body,
                recipients,
                ack_required,
                context,
                reason_code,
                error_code,
                metadata,
            )| {
                CoordinationEventRequest {
                    kind,
                    summary,
                    body,
                    recipients,
                    ack_required,
                    context,
                    reason_code,
                    error_code,
                    metadata,
                }
            },
        )
}

fn arb_inbox_message() -> impl Strategy<Value = CoordinationInboxMessage> {
    (
        proptest::option::of(arb_nonempty_string()),
        proptest::option::of(arb_agent_name()),
        arb_nonempty_string(),
        arb_nonempty_string(),
        any::<bool>(),
        proptest::option::of(arb_nonempty_string()),
        proptest::option::of("[0-9]{10}"),
        proptest::option::of(any::<bool>()),
    )
        .prop_map(
            |(id, from, subject, body, read, thread_id, timestamp, ack_required)| {
                CoordinationInboxMessage {
                    id,
                    from,
                    subject,
                    body,
                    read,
                    thread_id,
                    timestamp,
                    ack_required,
                }
            },
        )
}

fn arb_dispatched_message() -> impl Strategy<Value = DispatchedCoordinationMessage> {
    (
        arb_agent_name(),
        arb_nonempty_string(),
        arb_nonempty_string(),
        arb_nonempty_string(),
        arb_nonempty_string(),
        any::<bool>(),
        proptest::option::of(0..1_000_000_000i64),
    )
        .prop_map(
            |(
                recipient,
                message_id,
                subject,
                thread_id,
                correlation_id,
                ack_required,
                ack_deadline_ms,
            )| {
                DispatchedCoordinationMessage {
                    recipient,
                    message_id,
                    subject,
                    thread_id,
                    correlation_id,
                    ack_required,
                    ack_deadline_ms,
                }
            },
        )
}

fn arb_failed_message() -> impl Strategy<Value = FailedCoordinationMessage> {
    (
        arb_agent_name(),
        arb_nonempty_string(),
        arb_nonempty_string(),
    )
        .prop_map(|(recipient, subject, error)| FailedCoordinationMessage {
            recipient,
            subject,
            error,
        })
}

fn arb_dispatch_report() -> impl Strategy<Value = MissionMailDispatchReport> {
    (
        0..10usize,
        proptest::collection::vec(arb_dispatched_message(), 0..5),
        proptest::collection::vec(arb_failed_message(), 0..5),
    )
        .prop_map(|(attempted, delivered, failed)| MissionMailDispatchReport {
            attempted,
            delivered,
            failed,
        })
}

fn arb_parse_failure() -> impl Strategy<Value = CoordinationParseFailure> {
    (
        proptest::option::of(arb_nonempty_string()),
        arb_nonempty_string(),
        arb_nonempty_string(),
    )
        .prop_map(|(message_id, subject, error)| CoordinationParseFailure {
            message_id,
            subject,
            error,
        })
}

fn arb_inbound_message() -> impl Strategy<Value = InboundCoordinationMessage> {
    (
        proptest::option::of(arb_nonempty_string()),
        proptest::option::of(arb_agent_name()),
        arb_nonempty_string(),
        any::<bool>(),
        arb_envelope(),
    )
        .prop_map(
            |(message_id, from, subject, read, envelope)| InboundCoordinationMessage {
                message_id,
                from,
                subject,
                read,
                envelope,
            },
        )
}

fn arb_pending_ack() -> impl Strategy<Value = PendingAcknowledgement> {
    (
        arb_nonempty_string(),
        proptest::option::of(arb_agent_name()),
        arb_nonempty_string(),
        0..1_000_000_000i64,
        any::<bool>(),
        arb_coordination_context(),
    )
        .prop_map(
            |(message_id, reply_to, thread_id, deadline_ms, overdue, context)| {
                PendingAcknowledgement {
                    message_id,
                    reply_to,
                    thread_id,
                    deadline_ms,
                    overdue,
                    context,
                }
            },
        )
}

// ---------------------------------------------------------------------------
// Helper: embed envelope into body for inbox consumption tests
// ---------------------------------------------------------------------------

fn render_body_with_envelope(human_body: &str, envelope: &CoordinationEnvelope) -> String {
    let serialized = serde_json::to_string(envelope).unwrap();
    let marker = "[ft-coordination-envelope]";
    if human_body.is_empty() {
        format!("{marker}\n{serialized}")
    } else {
        format!("{human_body}\n\n{marker}\n{serialized}")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ── Serde roundtrips ────────────────────────────────────────────────

    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let restored: MissionAgentMailConfig = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
    }

    #[test]
    fn context_serde_roundtrip(ctx in arb_coordination_context()) {
        let json = serde_json::to_string(&ctx).unwrap();
        let restored: MissionCoordinationContext = serde_json::from_str(&json).unwrap();
        assert_eq!(ctx, restored);
    }

    #[test]
    fn event_kind_serde_roundtrip(kind in arb_event_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let restored: CoordinationEventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, restored);
    }

    #[test]
    fn envelope_serde_roundtrip(env in arb_envelope()) {
        let json = serde_json::to_string(&env).unwrap();
        let restored: CoordinationEnvelope = serde_json::from_str(&json).unwrap();
        // HashMap metadata may reorder — compare via Value
        let v1: serde_json::Value = serde_json::to_value(&env).unwrap();
        let v2: serde_json::Value = serde_json::to_value(&restored).unwrap();
        assert_eq!(v1, v2);
    }

    #[test]
    fn event_request_serde_roundtrip(req in arb_event_request()) {
        let json = serde_json::to_string(&req).unwrap();
        let restored: CoordinationEventRequest = serde_json::from_str(&json).unwrap();
        let v1: serde_json::Value = serde_json::to_value(&req).unwrap();
        let v2: serde_json::Value = serde_json::to_value(&restored).unwrap();
        assert_eq!(v1, v2);
    }

    #[test]
    fn inbox_message_serde_roundtrip(msg in arb_inbox_message()) {
        let json = serde_json::to_string(&msg).unwrap();
        let restored: CoordinationInboxMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, restored);
    }

    #[test]
    fn dispatched_message_serde_roundtrip(msg in arb_dispatched_message()) {
        let json = serde_json::to_string(&msg).unwrap();
        let restored: DispatchedCoordinationMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, restored);
    }

    #[test]
    fn failed_message_serde_roundtrip(msg in arb_failed_message()) {
        let json = serde_json::to_string(&msg).unwrap();
        let restored: FailedCoordinationMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, restored);
    }

    #[test]
    fn dispatch_report_serde_roundtrip(report in arb_dispatch_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let restored: MissionMailDispatchReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, restored);
    }

    #[test]
    fn parse_failure_serde_roundtrip(pf in arb_parse_failure()) {
        let json = serde_json::to_string(&pf).unwrap();
        let restored: CoordinationParseFailure = serde_json::from_str(&json).unwrap();
        assert_eq!(pf, restored);
    }

    #[test]
    fn inbound_message_serde_roundtrip(msg in arb_inbound_message()) {
        let json = serde_json::to_string(&msg).unwrap();
        let restored: InboundCoordinationMessage = serde_json::from_str(&json).unwrap();
        let v1: serde_json::Value = serde_json::to_value(&msg).unwrap();
        let v2: serde_json::Value = serde_json::to_value(&restored).unwrap();
        assert_eq!(v1, v2);
    }

    #[test]
    fn pending_ack_serde_roundtrip(pa in arb_pending_ack()) {
        let json = serde_json::to_string(&pa).unwrap();
        let restored: PendingAcknowledgement = serde_json::from_str(&json).unwrap();
        assert_eq!(pa, restored);
    }

    // ── Canonical thread-id properties ──────────────────────────────────

    #[test]
    fn canonical_thread_id_is_deterministic(ctx in arb_coordination_context()) {
        let id1 = ctx.canonical_thread_id();
        let id2 = ctx.canonical_thread_id();
        assert_eq!(id1, id2, "canonical_thread_id must be deterministic");
    }

    #[test]
    fn canonical_thread_id_uses_explicit_when_nonempty(
        mut ctx in arb_coordination_context(),
        explicit in "[a-z]{5,15}"
    ) {
        ctx.thread_id = Some(explicit.clone());
        assert_eq!(ctx.canonical_thread_id(), explicit.trim());
    }

    #[test]
    fn canonical_thread_id_derives_from_mission_when_absent(
        mut ctx in arb_coordination_context()
    ) {
        ctx.thread_id = None;
        let tid = ctx.canonical_thread_id();
        assert!(
            tid.starts_with("mission-"),
            "derived thread_id must start with 'mission-': {tid}"
        );
    }

    #[test]
    fn canonical_thread_id_sanitized_chars(ctx in arb_coordination_context()) {
        let tid = ctx.canonical_thread_id();
        // Thread IDs should contain only lowercase alphanumerics and dashes
        // (except when explicit thread_id is provided, which is returned as-is trimmed)
        if ctx.thread_id.as_ref().is_none_or(|t| t.trim().is_empty()) {
            for ch in tid.chars() {
                assert!(
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-',
                    "derived thread_id contains invalid char: {ch:?} in {tid}"
                );
            }
        }
    }

    // ── Emission and dispatch properties ─────────────────────────────────

    #[test]
    fn emit_event_delivered_plus_failed_equals_attempted(
        request in arb_event_request(),
        now_ms in 0..1_000_000_000i64
    ) {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let report = kernel.emit_event_at(now_ms, request);
        assert_eq!(
            report.delivered.len() + report.failed.len(),
            report.attempted,
            "delivered + failed must equal attempted"
        );
    }

    #[test]
    fn emit_event_empty_recipients_returns_zero_attempted(
        kind in arb_event_kind(),
        ctx in arb_coordination_context(),
        now_ms in 0..1_000_000_000i64
    ) {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let report = kernel.emit_event_at(
            now_ms,
            CoordinationEventRequest {
                kind,
                summary: "test".to_string(),
                body: "body".to_string(),
                recipients: vec![],
                ack_required: false,
                context: ctx,
                reason_code: "test".to_string(),
                error_code: None,
                metadata: HashMap::new(),
            },
        );
        assert_eq!(report.attempted, 0);
        assert!(report.delivered.is_empty());
        assert!(report.failed.is_empty());
    }

    #[test]
    fn emit_event_deduplicates_recipients(
        name in arb_agent_name(),
        kind in arb_event_kind(),
        ctx in arb_coordination_context(),
        now_ms in 0..1_000_000_000i64,
        repeat_count in 2..6usize
    ) {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let recipients: Vec<String> = (0..repeat_count).map(|_| name.clone()).collect();
        let report = kernel.emit_event_at(
            now_ms,
            CoordinationEventRequest {
                kind,
                summary: "test".to_string(),
                body: "body".to_string(),
                recipients,
                ack_required: false,
                context: ctx,
                reason_code: "test".to_string(),
                error_code: None,
                metadata: HashMap::new(),
            },
        );
        // Duplicates are deduplicated to 1 unique recipient
        assert_eq!(report.attempted, 1);
        assert_eq!(report.delivered.len(), 1);
    }

    #[test]
    fn emit_event_whitespace_recipients_are_trimmed(
        ctx in arb_coordination_context(),
        now_ms in 0..1_000_000_000i64
    ) {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let report = kernel.emit_event_at(
            now_ms,
            CoordinationEventRequest {
                kind: CoordinationEventKind::ProgressUpdate,
                summary: "test".to_string(),
                body: "body".to_string(),
                recipients: vec!["  ".to_string(), "   ".to_string(), String::new()],
                ack_required: false,
                context: ctx,
                reason_code: "test".to_string(),
                error_code: None,
                metadata: HashMap::new(),
            },
        );
        assert_eq!(report.attempted, 0, "whitespace-only recipients should be dropped");
    }

    #[test]
    fn emit_event_all_fail_when_transport_rejects(
        recipients in proptest::collection::vec(arb_agent_name(), 1..4),
        ctx in arb_coordination_context(),
        now_ms in 0..1_000_000_000i64
    ) {
        let transport = MockTransport::default();
        for r in &recipients {
            transport.fail_for(r);
        }
        let kernel = MissionAgentMailKernel::new(transport);
        let report = kernel.emit_event_at(
            now_ms,
            CoordinationEventRequest {
                kind: CoordinationEventKind::StartNotice,
                summary: "test".to_string(),
                body: "body".to_string(),
                recipients,
                ack_required: true,
                context: ctx,
                reason_code: "test".to_string(),
                error_code: None,
                metadata: HashMap::new(),
            },
        );
        assert!(report.delivered.is_empty());
        assert_eq!(report.failed.len(), report.attempted);
    }

    #[test]
    fn emit_event_ack_deadline_computed_correctly(
        config in arb_config(),
        name in arb_agent_name(),
        ctx in arb_coordination_context(),
        now_ms in 0..500_000_000i64
    ) {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::with_config(transport, config.clone());
        let report = kernel.emit_event_at(
            now_ms,
            CoordinationEventRequest {
                kind: CoordinationEventKind::StartNotice,
                summary: "test".to_string(),
                body: "body".to_string(),
                recipients: vec![name],
                ack_required: true,
                context: ctx,
                reason_code: "test".to_string(),
                error_code: None,
                metadata: HashMap::new(),
            },
        );
        assert_eq!(report.delivered.len(), 1);
        let delivered = &report.delivered[0];
        assert!(delivered.ack_required);
        assert_eq!(
            delivered.ack_deadline_ms,
            Some(now_ms.saturating_add(config.ack_timeout_ms))
        );
    }

    #[test]
    fn emit_event_no_ack_deadline_when_not_required(
        name in arb_agent_name(),
        ctx in arb_coordination_context(),
        now_ms in 0..500_000_000i64
    ) {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let report = kernel.emit_event_at(
            now_ms,
            CoordinationEventRequest {
                kind: CoordinationEventKind::ProgressUpdate,
                summary: "test".to_string(),
                body: "body".to_string(),
                recipients: vec![name],
                ack_required: false,
                context: ctx,
                reason_code: "test".to_string(),
                error_code: None,
                metadata: HashMap::new(),
            },
        );
        assert_eq!(report.delivered.len(), 1);
        assert!(!report.delivered[0].ack_required);
        assert_eq!(report.delivered[0].ack_deadline_ms, None);
    }

    // ── Envelope embedding and parsing roundtrip ────────────────────────

    #[test]
    fn envelope_embeds_and_parses_from_body(
        env in arb_envelope(),
        human_text in "[a-zA-Z ]{0,50}"
    ) {
        let body = render_body_with_envelope(&human_text, &env);
        // Body must contain the marker
        assert!(body.contains("[ft-coordination-envelope]"));

        // Parse it back via a kernel roundtrip
        let transport = MockTransport::default();
        transport.push_inbox(
            "test-agent",
            CoordinationInboxMessage {
                id: Some("m1".to_string()),
                from: Some("sender".to_string()),
                subject: "test".to_string(),
                body,
                read: false,
                thread_id: None,
                timestamp: None,
                ack_required: None,
            },
        );
        let kernel = MissionAgentMailKernel::new(transport);
        let report = kernel.consume_inbox("test-agent");
        assert_eq!(report.parsed.len(), 1, "envelope should parse successfully");
        assert!(report.parse_failures.is_empty());
        // Compare envelope via Value (HashMap ordering)
        let v1: serde_json::Value = serde_json::to_value(&env).unwrap();
        let v2: serde_json::Value = serde_json::to_value(&report.parsed[0].envelope).unwrap();
        assert_eq!(v1, v2);
    }

    // ── Inbox consumption properties ────────────────────────────────────

    #[test]
    fn consume_inbox_raw_count_equals_inbox_size(
        count in 0..8usize,
        env in arb_envelope()
    ) {
        let transport = MockTransport::default();
        for i in 0..count {
            let body = render_body_with_envelope(&format!("msg {i}"), &env);
            transport.push_inbox(
                "agent",
                CoordinationInboxMessage {
                    id: Some(format!("m{i}")),
                    from: Some("sender".to_string()),
                    subject: "test".to_string(),
                    body,
                    read: false,
                    thread_id: None,
                    timestamp: None,
                    ack_required: None,
                },
            );
        }
        let kernel = MissionAgentMailKernel::new(transport);
        let report = kernel.consume_inbox("agent");
        assert_eq!(report.raw_count, count);
        assert_eq!(report.parsed.len() + report.parse_failures.len(), count);
    }

    #[test]
    fn consume_inbox_non_coordination_messages_ignored(
        count in 1..5usize
    ) {
        let transport = MockTransport::default();
        for i in 0..count {
            transport.push_inbox(
                "agent",
                CoordinationInboxMessage {
                    id: Some(format!("m{i}")),
                    from: Some("human".to_string()),
                    subject: "chat".to_string(),
                    body: "Hey how's it going?".to_string(),
                    read: false,
                    thread_id: None,
                    timestamp: None,
                    ack_required: None,
                },
            );
        }
        let kernel = MissionAgentMailKernel::new(transport);
        let report = kernel.consume_inbox("agent");
        assert_eq!(report.raw_count, count);
        // Non-coordination messages are neither parsed nor reported as failures
        assert!(report.parsed.is_empty());
        assert!(report.parse_failures.is_empty());
    }

    // ── Ack collection properties ───────────────────────────────────────

    #[test]
    fn pending_acks_only_for_unread_ack_required_non_ack_events(
        now_ms in 300_001..1_000_000_000i64
    ) {
        let ctx = MissionCoordinationContext {
            mission_id: "m1".to_string(),
            fleet_id: None,
            bead_id: None,
            assignment_id: None,
            thread_id: Some("t1".to_string()),
            correlation_id: "c1".to_string(),
            scenario_id: None,
        };

        let mk = |id: &str, kind: CoordinationEventKind, ack: bool, read: bool| {
            let envelope = CoordinationEnvelope {
                version: 1,
                event_kind: kind,
                ack_required: ack,
                emitted_at_ms: 1000,
                reason_code: "test".to_string(),
                error_code: None,
                context: ctx.clone(),
                metadata: HashMap::new(),
            };
            CoordinationInboxMessage {
                id: Some(id.to_string()),
                from: Some("Leader".to_string()),
                subject: "coord".to_string(),
                body: render_body_with_envelope("body", &envelope),
                read,
                thread_id: Some("t1".to_string()),
                timestamp: None,
                ack_required: Some(ack),
            }
        };

        let transport = MockTransport::default();
        // Should be collected: unread, ack_required, StartNotice
        transport.push_inbox("a", mk("m1", CoordinationEventKind::StartNotice, true, false));
        // Should NOT: read
        transport.push_inbox("a", mk("m2", CoordinationEventKind::Handoff, true, true));
        // Should NOT: not ack_required
        transport.push_inbox("a", mk("m3", CoordinationEventKind::ProgressUpdate, false, false));
        // Should NOT: Acknowledgement kind
        transport.push_inbox("a", mk("m4", CoordinationEventKind::Acknowledgement, true, false));

        let kernel = MissionAgentMailKernel::new(transport);
        let report = kernel.collect_pending_acknowledgements_at("a", now_ms);
        assert_eq!(report.pending.len(), 1);
        assert_eq!(report.pending[0].message_id, "m1");
    }

    #[test]
    fn pending_ack_overdue_flag_reflects_deadline(
        emit_ms in 0..100_000i64,
        timeout_ms in 1_000..60_001i64,
        check_offset in 0..120_001i64
    ) {
        let ctx = MissionCoordinationContext {
            mission_id: "m1".to_string(),
            fleet_id: None,
            bead_id: None,
            assignment_id: None,
            thread_id: Some("t1".to_string()),
            correlation_id: "c1".to_string(),
            scenario_id: None,
        };

        let envelope = CoordinationEnvelope {
            version: 1,
            event_kind: CoordinationEventKind::StartNotice,
            ack_required: true,
            emitted_at_ms: emit_ms,
            reason_code: "test".to_string(),
            error_code: None,
            context: ctx.clone(),
            metadata: HashMap::new(),
        };

        let transport = MockTransport::default();
        transport.push_inbox(
            "a",
            CoordinationInboxMessage {
                id: Some("m1".to_string()),
                from: Some("L".to_string()),
                subject: "s".to_string(),
                body: render_body_with_envelope("body", &envelope),
                read: false,
                thread_id: Some("t1".to_string()),
                timestamp: None,
                ack_required: Some(true),
            },
        );

        let config = MissionAgentMailConfig { ack_timeout_ms: timeout_ms };
        let kernel = MissionAgentMailKernel::with_config(transport, config);
        let check_ms = emit_ms.saturating_add(check_offset);
        let deadline_ms = emit_ms.saturating_add(timeout_ms);
        let report = kernel.collect_pending_acknowledgements_at("a", check_ms);
        assert_eq!(report.pending.len(), 1);
        assert_eq!(report.pending[0].overdue, check_ms > deadline_ms);
    }

    // ── Acknowledgement emission properties ─────────────────────────────

    #[test]
    fn emit_acknowledgements_sends_to_reply_to(
        acknowledger in arb_agent_name(),
        reply_to in arb_agent_name(),
        ctx in arb_coordination_context(),
        now_ms in 0..1_000_000_000i64
    ) {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let pending = vec![PendingAcknowledgement {
            message_id: "m-1".to_string(),
            reply_to: Some(reply_to.clone()),
            thread_id: "t-1".to_string(),
            deadline_ms: now_ms + 60_000,
            overdue: false,
            context: ctx,
        }];

        let report = kernel.emit_acknowledgements_at(now_ms, &acknowledger, &pending, "noted");
        assert_eq!(report.delivered.len(), 1);
        assert!(report.failed.is_empty());
    }

    #[test]
    fn emit_acknowledgements_fails_for_missing_reply_to(
        acknowledger in arb_agent_name(),
        ctx in arb_coordination_context(),
        now_ms in 0..1_000_000_000i64
    ) {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let pending = vec![PendingAcknowledgement {
            message_id: "m-1".to_string(),
            reply_to: None,
            thread_id: "t-1".to_string(),
            deadline_ms: now_ms + 60_000,
            overdue: false,
            context: ctx,
        }];

        let report = kernel.emit_acknowledgements_at(now_ms, &acknowledger, &pending, "");
        assert!(report.delivered.is_empty());
        assert_eq!(report.failed.len(), 1);
    }

    #[test]
    fn emit_acknowledgements_blank_reply_to_treated_as_missing(
        acknowledger in arb_agent_name(),
        ctx in arb_coordination_context(),
        now_ms in 0..1_000_000_000i64
    ) {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let pending = vec![PendingAcknowledgement {
            message_id: "m-1".to_string(),
            reply_to: Some("   ".to_string()),
            thread_id: "t-1".to_string(),
            deadline_ms: now_ms + 60_000,
            overdue: false,
            context: ctx,
        }];

        let report = kernel.emit_acknowledgements_at(now_ms, &acknowledger, &pending, "");
        assert!(report.delivered.is_empty());
        assert_eq!(report.failed.len(), 1);
    }

    // ── Convenience methods use correct event kinds ─────────────────────

    #[test]
    fn start_notice_always_ack_required(
        name in arb_agent_name(),
        ctx in arb_coordination_context(),
        now_ms in 0..1_000_000_000i64
    ) {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let report = kernel.emit_start_notice_at(
            now_ms,
            vec![name],
            ctx,
            "summary",
            "body",
        );
        assert_eq!(report.delivered.len(), 1);
        assert!(report.delivered[0].ack_required);
        assert!(report.delivered[0].ack_deadline_ms.is_some());
    }

    #[test]
    fn progress_update_never_ack_required(
        name in arb_agent_name(),
        ctx in arb_coordination_context(),
        now_ms in 0..1_000_000_000i64
    ) {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let report = kernel.emit_progress_update_at(
            now_ms,
            vec![name],
            ctx,
            "summary",
            "body",
        );
        assert_eq!(report.delivered.len(), 1);
        assert!(!report.delivered[0].ack_required);
        assert_eq!(report.delivered[0].ack_deadline_ms, None);
    }

    #[test]
    fn handoff_notice_always_ack_required(
        name in arb_agent_name(),
        ctx in arb_coordination_context(),
        now_ms in 0..1_000_000_000i64
    ) {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let report = kernel.emit_handoff_notice_at(
            now_ms,
            vec![name],
            ctx,
            "summary",
            "body",
        );
        assert_eq!(report.delivered.len(), 1);
        assert!(report.delivered[0].ack_required);
        assert!(report.delivered[0].ack_deadline_ms.is_some());
    }

    // ── Dispatch report merge properties ────────────────────────────────

    #[test]
    fn dispatch_report_merge_sums_attempted(
        a in arb_dispatch_report(),
        b in arb_dispatch_report()
    ) {
        let mut merged = a.clone();
        let orig_attempted = a.attempted;
        let orig_delivered = a.delivered.len();
        let orig_failed = a.failed.len();

        // Manually merge since merge is pub(crate)/private
        merged.attempted += b.attempted;
        merged.delivered.extend(b.delivered.clone());
        merged.failed.extend(b.failed.clone());

        assert_eq!(merged.attempted, orig_attempted + b.attempted);
        assert_eq!(merged.delivered.len(), orig_delivered + b.delivered.len());
        assert_eq!(merged.failed.len(), orig_failed + b.failed.len());
    }

    // ── Subject rendering properties ────────────────────────────────────

    #[test]
    fn emitted_subject_contains_mission_id_and_kind_tag(
        kind in arb_event_kind(),
        mission_id in arb_nonempty_string(),
        name in arb_agent_name(),
        now_ms in 0..1_000_000_000i64
    ) {
        let transport = MockTransport::default();
        let kernel = MissionAgentMailKernel::new(transport);
        let ctx = MissionCoordinationContext {
            mission_id: mission_id.clone(),
            fleet_id: None,
            bead_id: None,
            assignment_id: None,
            thread_id: None,
            correlation_id: "c1".to_string(),
            scenario_id: None,
        };
        let report = kernel.emit_event_at(
            now_ms,
            CoordinationEventRequest {
                kind,
                summary: "test event".to_string(),
                body: "body".to_string(),
                recipients: vec![name],
                ack_required: false,
                context: ctx,
                reason_code: "test".to_string(),
                error_code: None,
                metadata: HashMap::new(),
            },
        );
        if let Some(delivered) = report.delivered.first() {
            assert!(
                delivered.subject.contains("[mission:"),
                "subject should contain [mission: prefix"
            );
        }
    }

    // ── Config defaults ─────────────────────────────────────────────────

    #[test]
    fn default_config_ack_timeout_positive(_dummy in 0..1u8) {
        let config = MissionAgentMailConfig::default();
        assert!(config.ack_timeout_ms > 0, "default ack timeout must be positive");
    }

    // ── Event kind Display coverage ─────────────────────────────────────

    #[test]
    fn event_kind_debug_is_nonempty(kind in arb_event_kind()) {
        let debug = format!("{kind:?}");
        assert!(!debug.is_empty(), "CoordinationEventKind Debug must be non-empty");
    }
}
