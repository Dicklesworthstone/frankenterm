//! Property-based tests for wire_protocol.rs
//!
//! Tests: PaneMeta/PaneDelta/GapNotice/DetectionNotice/PanesMeta serde roundtrips,
//! WirePayload tagged enum variant preservation, WireEnvelope new/to_json/from_json,
//! BackoffConfig delay monotonicity and capping, ConnectionState serde roundtrip,
//! Aggregator dedup/ordering/multi-agent, AgentStreamer seq monotonicity.

use frankenterm_core::wire_protocol::{
    AgentStreamer, Aggregator, BackoffConfig, ConnectionState, GapNotice, IngestResult,
    MAX_MESSAGE_SIZE, PROTOCOL_VERSION, PaneDelta, PaneMeta, PanesMeta, WireEnvelope, WirePayload,
};
use proptest::prelude::*;

// ============================================================================
// Strategies
// ============================================================================

fn arb_pane_meta() -> impl Strategy<Value = PaneMeta> {
    (
        0..1000u64,
        proptest::option::of("[a-f0-9-]{8,20}"),
        "[a-zA-Z:_]{1,20}",
        proptest::option::of("[a-zA-Z0-9_ -]{1,30}"),
        proptest::option::of("/[a-z/]{1,30}"),
        proptest::option::of(1..200u16),
        proptest::option::of(1..300u16),
        proptest::bool::ANY,
        1_000_000_000_000i64..2_000_000_000_000i64,
    )
        .prop_map(
            |(pane_id, pane_uuid, domain, title, cwd, rows, cols, observed, timestamp_ms)| {
                PaneMeta {
                    pane_id,
                    pane_uuid,
                    domain,
                    title,
                    cwd,
                    rows,
                    cols,
                    observed,
                    timestamp_ms,
                }
            },
        )
}

fn arb_pane_delta() -> impl Strategy<Value = PaneDelta> {
    (
        0..1000u64,
        0..10000u64,
        "[a-zA-Z0-9 ]{0,100}",
        0..10000usize,
        1_000_000_000_000i64..2_000_000_000_000i64,
    )
        .prop_map(
            |(pane_id, seq, content, content_len, captured_at_ms)| PaneDelta {
                pane_id,
                seq,
                content,
                content_len,
                captured_at_ms,
            },
        )
}

fn arb_gap_notice() -> impl Strategy<Value = GapNotice> {
    (
        0..1000u64,
        0..10000u64,
        0..10000u64,
        "[a-z_]{1,30}",
        1_000_000_000_000i64..2_000_000_000_000i64,
    )
        .prop_map(
            |(pane_id, seq_before, seq_after, reason, detected_at_ms)| GapNotice {
                pane_id,
                seq_before,
                seq_after,
                reason,
                detected_at_ms,
            },
        )
}

fn arb_wire_payload() -> impl Strategy<Value = WirePayload> {
    prop_oneof![
        arb_pane_meta().prop_map(WirePayload::PaneMeta),
        arb_pane_delta().prop_map(WirePayload::PaneDelta),
        arb_gap_notice().prop_map(WirePayload::Gap),
        arb_pane_meta().prop_map(|pm| WirePayload::PanesMeta(PanesMeta {
            panes: vec![pm],
            timestamp_ms: 1_700_000_000_000,
        })),
    ]
}

fn arb_connection_state() -> impl Strategy<Value = ConnectionState> {
    prop_oneof![
        Just(ConnectionState::Disconnected),
        Just(ConnectionState::Connecting),
        Just(ConnectionState::Connected),
        (0..10u32).prop_map(|attempt| ConnectionState::Reconnecting { attempt }),
    ]
}

// ============================================================================
// PaneMeta properties
// ============================================================================

proptest! {
    #[test]
    fn pane_meta_serde_roundtrip(meta in arb_pane_meta()) {
        let json = serde_json::to_string(&meta).unwrap();
        let back: PaneMeta = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(meta.pane_id, back.pane_id);
        prop_assert_eq!(&meta.domain, &back.domain);
        prop_assert_eq!(meta.rows, back.rows);
        prop_assert_eq!(meta.cols, back.cols);
        prop_assert_eq!(meta.observed, back.observed);
        prop_assert_eq!(meta.timestamp_ms, back.timestamp_ms);
    }

    #[test]
    fn pane_delta_serde_roundtrip(delta in arb_pane_delta()) {
        let json = serde_json::to_string(&delta).unwrap();
        let back: PaneDelta = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(delta.pane_id, back.pane_id);
        prop_assert_eq!(delta.seq, back.seq);
        prop_assert_eq!(&delta.content, &back.content);
        prop_assert_eq!(delta.content_len, back.content_len);
        prop_assert_eq!(delta.captured_at_ms, back.captured_at_ms);
    }

    #[test]
    fn gap_notice_serde_roundtrip(gap in arb_gap_notice()) {
        let json = serde_json::to_string(&gap).unwrap();
        let back: GapNotice = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(gap.pane_id, back.pane_id);
        prop_assert_eq!(gap.seq_before, back.seq_before);
        prop_assert_eq!(gap.seq_after, back.seq_after);
        prop_assert_eq!(&gap.reason, &back.reason);
        prop_assert_eq!(gap.detected_at_ms, back.detected_at_ms);
    }
}

// ============================================================================
// WirePayload tagged enum properties
// ============================================================================

proptest! {
    #[test]
    fn wire_payload_serde_roundtrip(payload in arb_wire_payload()) {
        let json = serde_json::to_string(&payload).unwrap();
        // Verify the type tag is present
        let has_tag = json.contains("\"type\":\"pane_meta\"")
            || json.contains("\"type\":\"pane_delta\"")
            || json.contains("\"type\":\"gap\"")
            || json.contains("\"type\":\"detection\"")
            || json.contains("\"type\":\"panes_meta\"");
        prop_assert!(has_tag, "JSON should contain a type tag, got: {}", json);

        let back: WirePayload = serde_json::from_str(&json).unwrap();
        // Verify variant is preserved
        let same_variant = matches!(
            (&payload, &back),
            (WirePayload::PaneMeta(_), WirePayload::PaneMeta(_))
                | (WirePayload::PaneDelta(_), WirePayload::PaneDelta(_))
                | (WirePayload::Gap(_), WirePayload::Gap(_))
                | (WirePayload::Detection(_), WirePayload::Detection(_))
                | (WirePayload::PanesMeta(_), WirePayload::PanesMeta(_))
        );
        prop_assert!(same_variant, "Payload variant should be preserved after serde roundtrip");
    }
}

// ============================================================================
// WireEnvelope properties
// ============================================================================

proptest! {
    #[test]
    fn envelope_new_sets_protocol_version(
        seq in 0..10000u64,
        sender in "[a-z-]{3,20}",
        payload in arb_wire_payload(),
    ) {
        let envelope = WireEnvelope::new(seq, &sender, payload);
        prop_assert_eq!(envelope.version, PROTOCOL_VERSION);
        prop_assert_eq!(envelope.seq, seq);
        prop_assert_eq!(&envelope.sender, &sender);
        prop_assert!(envelope.sent_at_ms > 0, "sent_at_ms should be set");
    }

    #[test]
    fn envelope_json_roundtrip(
        seq in 1..10000u64,
        sender in "[a-z-]{3,20}",
        payload in arb_wire_payload(),
    ) {
        let envelope = WireEnvelope::new(seq, &sender, payload);
        let bytes = envelope.to_json().unwrap();
        let back = WireEnvelope::from_json(&bytes).unwrap();
        prop_assert_eq!(envelope.version, back.version);
        prop_assert_eq!(envelope.seq, back.seq);
        prop_assert_eq!(&envelope.sender, &back.sender);
        prop_assert_eq!(envelope.sent_at_ms, back.sent_at_ms);
    }

    #[test]
    fn envelope_json_under_max_size(
        seq in 1..100u64,
        sender in "[a-z]{3,10}",
        payload in arb_wire_payload(),
    ) {
        let envelope = WireEnvelope::new(seq, &sender, payload);
        let bytes = envelope.to_json().unwrap();
        prop_assert!(bytes.len() <= MAX_MESSAGE_SIZE,
            "Envelope JSON ({} bytes) should be under MAX_MESSAGE_SIZE ({})",
            bytes.len(), MAX_MESSAGE_SIZE);
    }

    #[test]
    fn envelope_sent_at_ms_is_positive(
        seq in 1..10000u64,
        sender in "[a-z-]{3,20}",
        payload in arb_wire_payload(),
    ) {
        let envelope = WireEnvelope::new(seq, &sender, payload);
        prop_assert!(envelope.sent_at_ms > 0,
            "sent_at_ms should be positive (non-zero), got: {}", envelope.sent_at_ms);
    }
}

// ============================================================================
// ConnectionState properties
// ============================================================================

proptest! {
    #[test]
    fn connection_state_serde_roundtrip(state in arb_connection_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: ConnectionState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(state, back);
    }

    #[test]
    fn connection_state_display_format_non_empty(state in arb_connection_state()) {
        let debug_str = format!("{:?}", state);
        let display_str = format!("{:?}", state); // ConnectionState doesn't impl Display, so use Debug
        prop_assert!(!debug_str.is_empty(),
            "Debug format should be non-empty");
        prop_assert!(!display_str.is_empty(),
            "Display format should be non-empty");
    }
}

// ============================================================================
// BackoffConfig properties
// ============================================================================

proptest! {
    #[test]
    fn backoff_delay_monotonically_nondecreasing(
        initial_ms in 100..5000u64,
        max_ms in 5000..60000u64,
        multiplier in 1.0..4.0f64,
    ) {
        let cfg = BackoffConfig { initial_ms, max_ms, multiplier };
        let mut prev = 0u64;
        for attempt in 0..10u32 {
            let delay = cfg.delay_ms(attempt);
            prop_assert!(delay >= prev,
                "delay should be non-decreasing: attempt={}, prev={}, cur={}",
                attempt, prev, delay);
            prev = delay;
        }
    }

    #[test]
    fn backoff_delay_capped_at_max(
        initial_ms in 100..2000u64,
        max_ms in 2000..30000u64,
        multiplier in 1.5..3.0f64,
        attempt in 0..20u32,
    ) {
        let cfg = BackoffConfig { initial_ms, max_ms, multiplier };
        let delay = cfg.delay_ms(attempt);
        prop_assert!(delay <= max_ms,
            "delay {} should be capped at max_ms {}", delay, max_ms);
    }

    #[test]
    fn backoff_delay_starts_at_initial(
        initial_ms in 100..5000u64,
        max_ms in 5000..60000u64,
        multiplier in 1.0..4.0f64,
    ) {
        let cfg = BackoffConfig { initial_ms, max_ms, multiplier };
        let delay = cfg.delay_ms(0);
        prop_assert_eq!(delay, initial_ms,
            "first delay should equal initial_ms");
    }

    #[test]
    fn backoff_custom_delay_formula_correct(
        initial_ms in 100..1000u64,
        max_ms in 5000..10000u64,
        multiplier in 2.0..3.0f64,
        attempt in 0..5u32,
    ) {
        let cfg = BackoffConfig { initial_ms, max_ms, multiplier };
        let delay = cfg.delay_ms(attempt);
        let expected = (initial_ms as f64 * multiplier.powi(attempt as i32)).min(max_ms as f64) as u64;
        prop_assert_eq!(delay, expected,
            "delay formula should be min(initial * multiplier^attempt, max)");
    }
}

#[test]
fn backoff_default_values() {
    let cfg = BackoffConfig::default();
    assert_eq!(cfg.initial_ms, 500);
    assert_eq!(cfg.max_ms, 30_000);
    assert!((cfg.multiplier - 2.0).abs() < f64::EPSILON);
}

// ============================================================================
// AgentStreamer properties
// ============================================================================

proptest! {
    /// Streamer sequence numbers are strictly monotonically increasing.
    #[test]
    fn streamer_seq_monotonic(n_events in 1..20usize) {
        let mut streamer = AgentStreamer::new("test-agent");
        let mut prev_seq = 0u64;
        for i in 0..n_events {
            let event = frankenterm_core::events::Event::SegmentCaptured {
                pane_id: i as u64,
                seq: i as u64,
                content_len: 10,
            };
            if let Some(envelope) = streamer.event_to_envelope(&event) {
                prop_assert!(envelope.seq > prev_seq,
                    "seq should be strictly increasing: prev={}, cur={}",
                    prev_seq, envelope.seq);
                prev_seq = envelope.seq;
            }
        }
        prop_assert_eq!(streamer.messages_sent(), n_events as u64);
    }

    /// Streamer starts disconnected.
    #[test]
    fn streamer_initial_state(_sender in "[a-z]{3,10}") {
        let streamer = AgentStreamer::new("test");
        prop_assert_eq!(streamer.state(), ConnectionState::Disconnected);
        prop_assert_eq!(streamer.seq(), 0);
        prop_assert_eq!(streamer.messages_sent(), 0);
        prop_assert_eq!(streamer.messages_dropped(), 0);
    }

    #[test]
    fn streamer_mark_connected_resets_state(_sender in "[a-z]{3,10}") {
        let mut streamer = AgentStreamer::new("test");
        // Start disconnected
        prop_assert_eq!(streamer.state(), ConnectionState::Disconnected);
        // Mark connected
        streamer.mark_connected();
        prop_assert_eq!(streamer.state(), ConnectionState::Connected,
            "mark_connected should transition to Connected state");
    }

    #[test]
    fn streamer_mark_disconnected_resets_state(_sender in "[a-z]{3,10}") {
        let mut streamer = AgentStreamer::new("test");
        // Mark connected first
        streamer.mark_connected();
        prop_assert_eq!(streamer.state(), ConnectionState::Connected);
        // Mark disconnected
        streamer.mark_disconnected();
        prop_assert_eq!(streamer.state(), ConnectionState::Disconnected,
            "mark_disconnected should transition to Disconnected state");
    }

    #[test]
    fn streamer_with_custom_backoff_uses_config(
        initial_ms in 100..2000u64,
        max_ms in 5000..20000u64,
        multiplier in 1.5..3.0f64,
    ) {
        let backoff = BackoffConfig { initial_ms, max_ms, multiplier };
        let mut streamer = AgentStreamer::with_backoff("test", backoff.clone());

        // Trigger first reconnect
        let delay = streamer.mark_reconnecting();
        prop_assert_eq!(delay, initial_ms,
            "first reconnect delay should match custom initial_ms");

        // Trigger second reconnect
        let delay = streamer.mark_reconnecting();
        let expected = (initial_ms as f64 * multiplier).min(max_ms as f64) as u64;
        prop_assert_eq!(delay, expected,
            "second reconnect delay should match custom backoff formula");
    }

    #[test]
    fn streamer_reconnect_attempt_state_tracks_backoff_formula(
        initial_ms in 100..2000u64,
        max_ms in 2000..30000u64,
        multiplier in 1.2..3.0f64,
        attempts in 1..8u32,
    ) {
        let backoff = BackoffConfig {
            initial_ms,
            max_ms,
            multiplier,
        };
        let mut streamer = AgentStreamer::with_backoff("test", backoff.clone());

        for attempt in 0..attempts {
            let delay = streamer.mark_reconnecting();
            prop_assert_eq!(
                delay,
                backoff.delay_ms(attempt),
                "delay should follow configured backoff for attempt={}", attempt
            );
            prop_assert_eq!(
                streamer.state(),
                ConnectionState::Reconnecting { attempt },
                "state should expose current reconnect attempt"
            );
        }
    }

    #[test]
    fn streamer_reconnect_backoff_resets_after_connected_or_disconnected(
        initial_ms in 100..2000u64,
        max_ms in 2000..30000u64,
        multiplier in 1.2..3.0f64,
        warmup_attempts in 1..8u32,
    ) {
        let backoff = BackoffConfig {
            initial_ms,
            max_ms,
            multiplier,
        };

        let mut streamer = AgentStreamer::with_backoff("test", backoff.clone());
        for _ in 0..warmup_attempts {
            streamer.mark_reconnecting();
        }
        streamer.mark_connected();
        let delay_after_connect = streamer.mark_reconnecting();
        prop_assert_eq!(
            delay_after_connect,
            initial_ms,
            "backoff should restart from initial_ms after mark_connected"
        );
        prop_assert_eq!(
            streamer.state(),
            ConnectionState::Reconnecting { attempt: 0 }
        );

        let mut streamer = AgentStreamer::with_backoff("test", backoff);
        for _ in 0..warmup_attempts {
            streamer.mark_reconnecting();
        }
        streamer.mark_disconnected();
        let delay_after_disconnect = streamer.mark_reconnecting();
        prop_assert_eq!(
            delay_after_disconnect,
            initial_ms,
            "backoff should restart from initial_ms after mark_disconnected"
        );
        prop_assert_eq!(
            streamer.state(),
            ConnectionState::Reconnecting { attempt: 0 }
        );
    }
}

// ============================================================================
// PanesMeta properties
// ============================================================================

proptest! {
    #[test]
    fn panes_meta_multiple_panes_serde_roundtrip(
        panes in prop::collection::vec(arb_pane_meta(), 0..10),
        timestamp_ms in 1_000_000_000_000i64..2_000_000_000_000i64,
    ) {
        let meta = PanesMeta { panes, timestamp_ms };
        let json = serde_json::to_string(&meta).unwrap();
        let back: PanesMeta = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(meta.panes.len(), back.panes.len(),
            "panes count should be preserved");
        prop_assert_eq!(meta.timestamp_ms, back.timestamp_ms,
            "timestamp_ms should be preserved");
        for (orig, decoded) in meta.panes.iter().zip(back.panes.iter()) {
            prop_assert_eq!(orig.pane_id, decoded.pane_id);
            prop_assert_eq!(&orig.domain, &decoded.domain);
        }
    }
}

// ============================================================================
// Aggregator properties
// ============================================================================

proptest! {
    /// Messages with increasing seq from same sender are all accepted.
    #[test]
    fn aggregator_accepts_increasing_seq(n_messages in 1..20usize) {
        let mut agg = Aggregator::new(10);
        for i in 1..=n_messages {
            let envelope = WireEnvelope::new(
                i as u64,
                "agent-a",
                WirePayload::Gap(GapNotice {
                    pane_id: 1,
                    seq_before: 0,
                    seq_after: 1,
                    reason: "test".to_string(),
                    detected_at_ms: 0,
                }),
            );
            let result = agg.ingest_envelope(envelope).unwrap();
            prop_assert!(matches!(result, IngestResult::Accepted(_)),
                "Message with seq {} should be accepted", i);
        }
        prop_assert_eq!(agg.total_accepted(), n_messages as u64);
        prop_assert_eq!(agg.agent_last_seq("agent-a"), Some(n_messages as u64));
    }

    /// Duplicate or old seq from same sender is rejected.
    #[test]
    fn aggregator_dedup_rejects_old_seq(
        high_seq in 5..100u64,
        low_seq in 1..5u64,
    ) {
        let mut agg = Aggregator::new(10);
        let gap = GapNotice {
            pane_id: 1,
            seq_before: 0,
            seq_after: 1,
            reason: "test".to_string(),
            detected_at_ms: 0,
        };

        // First message with high seq
        let e1 = WireEnvelope::new(high_seq, "agent", WirePayload::Gap(gap.clone()));
        let result = agg.ingest_envelope(e1).unwrap();
        prop_assert!(matches!(result, IngestResult::Accepted(_)));

        // Second message with lower seq
        let e2 = WireEnvelope::new(low_seq, "agent", WirePayload::Gap(gap));
        let result = agg.ingest_envelope(e2).unwrap();
        prop_assert!(matches!(result, IngestResult::Duplicate { .. }),
            "seq {} after {} should be duplicate", low_seq, high_seq);

        prop_assert_eq!(agg.total_accepted(), 1);
    }

    /// Different senders are tracked independently.
    #[test]
    fn aggregator_independent_senders(
        n_senders in 2..6usize,
        n_messages in 1..5usize,
    ) {
        let mut agg = Aggregator::new(100);
        let gap = GapNotice {
            pane_id: 1,
            seq_before: 0,
            seq_after: 1,
            reason: "test".to_string(),
            detected_at_ms: 0,
        };

        for sender_idx in 0..n_senders {
            let sender = format!("agent-{}", sender_idx);
            for msg_idx in 1..=n_messages {
                let envelope = WireEnvelope::new(
                    msg_idx as u64,
                    &sender,
                    WirePayload::Gap(gap.clone()),
                );
                let result = agg.ingest_envelope(envelope).unwrap();
                prop_assert!(matches!(result, IngestResult::Accepted(_)));
            }
        }
        prop_assert_eq!(agg.agent_count(), n_senders);
        prop_assert_eq!(agg.total_accepted(), (n_senders * n_messages) as u64);
    }

    /// Aggregator capacity limit: more agents than max_agents fails with TooManyAgents.
    #[test]
    fn aggregator_capacity_limit_rejects_excess_agents(
        max_agents in 1..5usize,
        n_agents in 5..10usize,
    ) {
        prop_assume!(n_agents > max_agents);
        let mut agg = Aggregator::new(max_agents);
        let gap = GapNotice {
            pane_id: 1,
            seq_before: 0,
            seq_after: 1,
            reason: "test".to_string(),
            detected_at_ms: 0,
        };

        let mut accepted_count = 0;
        let mut rejected_count = 0;

        for i in 0..n_agents {
            let sender = format!("agent-{}", i);
            let envelope = WireEnvelope::new(1, &sender, WirePayload::Gap(gap.clone()));
            match agg.ingest_envelope(envelope) {
                Ok(IngestResult::Accepted(_)) => {
                    accepted_count += 1;
                }
                Err(frankenterm_core::wire_protocol::WireProtocolError::TooManyAgents { .. }) => {
                    rejected_count += 1;
                }
                other => {
                    prop_assert!(false, "Unexpected result: {:?}", other);
                }
            }
        }

        prop_assert_eq!(accepted_count, max_agents,
            "should accept exactly max_agents agents");
        prop_assert_eq!(rejected_count, n_agents - max_agents,
            "should reject excess agents");
        prop_assert_eq!(agg.agent_count(), max_agents);
    }

    /// Conservation property: total_accepted + total_rejected = total processed.
    #[test]
    fn aggregator_conservation_property(
        n_valid in 1..10usize,
        n_invalid in 1..10usize,
    ) {
        let mut agg = Aggregator::new(100);
        let gap = GapNotice {
            pane_id: 1,
            seq_before: 0,
            seq_after: 1,
            reason: "test".to_string(),
            detected_at_ms: 0,
        };

        // Send n_valid valid messages
        for i in 1..=n_valid {
            let envelope = WireEnvelope::new(i as u64, "agent-a", WirePayload::Gap(gap.clone()));
            let _ = agg.ingest_envelope(envelope);
        }

        // Send n_invalid invalid messages (malformed JSON)
        for _ in 0..n_invalid {
            let _ = agg.ingest(b"invalid json");
        }

        let total_processed = (n_valid + n_invalid) as u64;
        let total_counted = agg.total_accepted() + agg.total_rejected();
        prop_assert_eq!(total_counted, total_processed,
            "total_accepted + total_rejected should equal total processed");
    }
}

// ============================================================================
// Constants
// ============================================================================

#[test]
fn protocol_version_is_1() {
    assert_eq!(PROTOCOL_VERSION, 1);
}

#[test]
fn max_message_size_is_1mib() {
    assert_eq!(MAX_MESSAGE_SIZE, 1_048_576);
}

// ============================================================================
// Error handling
// ============================================================================

#[test]
fn oversized_message_rejected() {
    let huge = vec![b'{'; MAX_MESSAGE_SIZE + 1];
    let err = WireEnvelope::from_json(&huge);
    assert!(err.is_err());
}

#[test]
fn empty_bytes_rejected() {
    let err = WireEnvelope::from_json(b"");
    assert!(err.is_err());
}

#[test]
fn invalid_json_rejected() {
    let err = WireEnvelope::from_json(b"not json");
    assert!(err.is_err());
}
