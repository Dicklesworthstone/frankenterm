#![no_main]

//! [ft-h8v8v] Fuzz target for the distributed-mode wire-protocol surface.
//!
//! `WireEnvelope::from_json` and `Aggregator::ingest` are the entry
//! points where untrusted network bytes flow into frankenterm-core
//! when distributed mode is enabled. A panic, OOM, stack overflow, or
//! deserialization confusion on this boundary is a remote-DoS surface.
//!
//! The contract this harness pins:
//!
//! 1. **No panic on any byte sequence.** `WireEnvelope::from_json`
//!    and `Aggregator::ingest` must return `Ok(_)` or `Err(_)` for
//!    every input — never trip a debug-assert, integer overflow
//!    panic, or stack overflow on deeply-nested JSON.
//! 2. **Saturating-counter invariant.** The aggregator's
//!    `total_accepted` / `total_rejected` use `saturating_add`; this
//!    harness drives N consecutive ingests against one aggregator
//!    and asserts `agent_count() <= max_agents` and that reading
//!    the counters is panic-free.
//!
//! ## Modes (driven by Arbitrary)
//!
//! - **Single**: feed one byte slice into `WireEnvelope::from_json`
//!   AND `Aggregator::ingest` (which is `from_json` + state machine
//!   in one call). Hits the JSON parsing surface with no
//!   scaffolding — malformed UTF-8, deeply-nested arrays, integer
//!   overflow attempts on `version` / `seq`, adversarial payload
//!   variants.
//! - **Sequence**: drive a single Aggregator through up to N
//!   raw-bytes ingests, letting libFuzzer minimize toward inputs
//!   that hit the eviction + duplicate-seq + future-clock branches.
//! - **DifferentialSequence**: generate versioned wire envelopes,
//!   serialize them through the production codec, decode them back, and
//!   compare Aggregator behavior against a small reference model for
//!   version rejection, per-agent dedup, and stale-session pruning.
//!
//! ## Not fuzzed here
//!
//! - The TCP framing layer (bounds messages before they reach
//!   `from_json`).
//! - TLS handshakes / connection setup.
//! - The serialization side (`to_json`) — already round-tripped in
//!   `proptest_wire_protocol.rs`.

use std::collections::BTreeMap;

use arbitrary::Arbitrary;
use frankenterm_core::wire_protocol::{
    Aggregator, GapNotice, IngestResult, PROTOCOL_VERSION, PaneDelta, PaneMeta, PanesMeta,
    WireEnvelope, WirePayload, WireProtocolLimits, validate_sender_identity_with_limits,
};
use libfuzzer_sys::fuzz_target;

// Match the production wire-protocol caps so the harness reflects
// what the network code path actually accepts. Larger inputs are
// dropped at the framing layer before reaching from_json.
const MAX_BYTES: usize = 1024 * 1024; // 1 MiB
const MAX_SEQUENCE_LEN: usize = 32;
const MAX_AGENTS: usize = 16;

#[derive(Arbitrary, Debug)]
enum FuzzInput<'a> {
    /// Single ingest — feed one byte slice through both from_json
    /// (parser-only path) and Aggregator::ingest (parser + state).
    Single(&'a [u8]),
    /// Sequence — drive one Aggregator through up to MAX_SEQUENCE_LEN
    /// raw-bytes ingests with an advancing receive clock so stale-
    /// agent eviction can engage.
    Sequence {
        max_agents_choice: u8,
        ingests: Vec<&'a [u8]>,
        check_stats: bool,
    },
    /// Structure-aware differential harness for the versioned JSON
    /// codec plus Aggregator state machine.
    DifferentialSequence {
        max_agents_choice: u8,
        stale_after_choice: u8,
        commands: Vec<StructuredEnvelope>,
    },
}

#[derive(Arbitrary, Debug)]
struct StructuredEnvelope {
    sender_choice: u8,
    seq: u64,
    version_delta: u8,
    payload_kind: u8,
    payload_seq: u16,
    received_at_ms: i16,
    sent_at_ms: i16,
    make_payload_invalid: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelOutcome {
    Accepted,
    Duplicate,
    Rejected,
}

#[derive(Clone, Copy, Debug)]
struct ModelSession {
    last_seq: u64,
    messages_received: u64,
    last_seen_ms: i64,
}

#[derive(Debug)]
struct ReferenceAggregator {
    sessions: BTreeMap<String, ModelSession>,
    max_agents: usize,
    stale_after_ms: i64,
    accepted: u64,
    rejected: u64,
    limits: WireProtocolLimits,
}

fn parse_only(bytes: &[u8], limits: WireProtocolLimits) {
    if bytes.len() > MAX_BYTES {
        return;
    }
    // Contract 1: from_json must return Ok or Err, never panic.
    let _: Result<WireEnvelope, _> = WireEnvelope::from_json_with_limits(bytes, limits);
}

fn parse_and_ingest(bytes: &[u8], agg: &mut Aggregator) {
    if bytes.len() > MAX_BYTES {
        return;
    }
    // Contract 1: ingest is from_json + state machine; must not panic.
    let _ = agg.ingest(bytes);
}

fn sender_for_choice(choice: u8) -> String {
    match choice % 6 {
        0 => "agent-a".to_string(),
        1 => "agent-b".to_string(),
        2 => "agent-c".to_string(),
        3 => "agent:bad".to_string(),
        4 => " ".to_string(),
        _ => "agent.long".to_string(),
    }
}

fn valid_sender_choices() -> [&'static str; 4] {
    ["agent-a", "agent-b", "agent-c", "agent.long"]
}

fn payload_for_command(command: &StructuredEnvelope) -> WirePayload {
    let pane_id = u64::from(command.payload_seq % 8);
    let payload_seq = u64::from(command.payload_seq);
    match command.payload_kind % 4 {
        0 => {
            let content = format!("fuzz-delta-{payload_seq}");
            let content_len = if command.make_payload_invalid {
                content.len().saturating_add(1)
            } else {
                content.len()
            };
            WirePayload::PaneDelta(PaneDelta {
                pane_id,
                seq: payload_seq,
                content,
                content_len,
                captured_at_ms: i64::from(command.sent_at_ms),
            })
        }
        1 => {
            let seq_before = payload_seq;
            let seq_after = if command.make_payload_invalid {
                seq_before
            } else {
                seq_before.saturating_add(1)
            };
            WirePayload::Gap(GapNotice {
                pane_id,
                seq_before,
                seq_after,
                reason: "fuzz-gap".to_string(),
                detected_at_ms: i64::from(command.sent_at_ms),
            })
        }
        2 => WirePayload::PaneMeta(PaneMeta {
            pane_id,
            pane_uuid: Some(format!("pane-{pane_id}")),
            domain: "local".to_string(),
            title: Some("fuzz-pane".to_string()),
            cwd: Some("/tmp/fuzz".to_string()),
            rows: Some(24),
            cols: Some(80),
            observed: true,
            timestamp_ms: i64::from(command.sent_at_ms),
        }),
        _ => WirePayload::PanesMeta(PanesMeta {
            panes: vec![PaneMeta {
                pane_id,
                pane_uuid: None,
                domain: "local".to_string(),
                title: None,
                cwd: None,
                rows: Some(24),
                cols: Some(80),
                observed: true,
                timestamp_ms: i64::from(command.sent_at_ms),
            }],
            timestamp_ms: i64::from(command.sent_at_ms),
        }),
    }
}

fn envelope_for_command(command: &StructuredEnvelope) -> WireEnvelope {
    WireEnvelope {
        version: PROTOCOL_VERSION.saturating_add(u32::from(command.version_delta % 2)),
        seq: command.seq,
        sender: sender_for_choice(command.sender_choice),
        sent_at_ms: i64::from(command.sent_at_ms),
        payload: payload_for_command(command),
    }
}

fn payload_is_valid(payload: &WirePayload) -> bool {
    match payload {
        WirePayload::PaneDelta(delta) => delta.content_len == delta.content.len(),
        WirePayload::Gap(gap) => gap.seq_after > gap.seq_before,
        WirePayload::PaneMeta(_) | WirePayload::Detection(_) | WirePayload::PanesMeta(_) => true,
    }
}

fn actual_outcome(result: Result<IngestResult, impl core::fmt::Debug>) -> ModelOutcome {
    match result {
        Ok(IngestResult::Accepted(_)) => ModelOutcome::Accepted,
        Ok(IngestResult::Duplicate { .. }) => ModelOutcome::Duplicate,
        Err(_) => ModelOutcome::Rejected,
    }
}

impl ReferenceAggregator {
    fn new(max_agents: usize, stale_after_ms: i64, limits: WireProtocolLimits) -> Self {
        Self {
            sessions: BTreeMap::new(),
            max_agents,
            stale_after_ms,
            accepted: 0,
            rejected: 0,
            limits,
        }
    }

    fn prune_stale(&mut self, now_ms: i64) {
        if self.stale_after_ms <= 0 {
            return;
        }
        self.sessions
            .retain(|_, session| now_ms.saturating_sub(session.last_seen_ms) < self.stale_after_ms);
    }

    fn ingest(&mut self, envelope: &WireEnvelope, received_at_ms: i64) -> ModelOutcome {
        if envelope.version != PROTOCOL_VERSION
            || envelope.seq == u64::MAX
            || validate_sender_identity_with_limits(&envelope.sender, self.limits).is_err()
            || !payload_is_valid(&envelope.payload)
        {
            self.rejected = self.rejected.saturating_add(1);
            return ModelOutcome::Rejected;
        }

        let is_new = !self.sessions.contains_key(&envelope.sender);
        if is_new && self.sessions.len() >= self.max_agents {
            self.prune_stale(received_at_ms);
        }
        if is_new && self.sessions.len() >= self.max_agents {
            self.rejected = self.rejected.saturating_add(1);
            return ModelOutcome::Rejected;
        }

        let session = self
            .sessions
            .entry(envelope.sender.clone())
            .or_insert(ModelSession {
                last_seq: 0,
                messages_received: 0,
                last_seen_ms: 0,
            });
        if session.messages_received > 0 && envelope.seq <= session.last_seq {
            session.last_seen_ms = session.last_seen_ms.max(received_at_ms);
            return ModelOutcome::Duplicate;
        }

        session.last_seq = envelope.seq;
        session.messages_received = session.messages_received.saturating_add(1);
        session.last_seen_ms = session.last_seen_ms.max(received_at_ms);
        self.accepted = self.accepted.saturating_add(1);
        ModelOutcome::Accepted
    }

    fn agent_count(&self) -> usize {
        self.sessions.len()
    }

    fn last_seq(&self, sender: &str) -> Option<u64> {
        self.sessions.get(sender).map(|session| session.last_seq)
    }
}

fn differential_sequence(
    max_agents_choice: u8,
    stale_after_choice: u8,
    commands: Vec<StructuredEnvelope>,
) {
    let limits = WireProtocolLimits::default();
    let max_agents = (usize::from(max_agents_choice) % MAX_AGENTS).max(1);
    let stale_after_ms = i64::from(stale_after_choice % 16) * 10;
    let mut actual = Aggregator::with_limits_and_stale_after(max_agents, limits, stale_after_ms);
    let mut reference = ReferenceAggregator::new(max_agents, stale_after_ms, limits);

    for command in commands.into_iter().take(MAX_SEQUENCE_LEN) {
        let envelope = envelope_for_command(&command);
        let bytes = envelope
            .to_json()
            .expect("structured envelope must serialize");
        let decoded = WireEnvelope::from_json_with_limits(&bytes, limits);
        let received_at_ms = i64::from(command.received_at_ms);
        let actual_result =
            decoded.and_then(|decoded| actual.ingest_envelope_at(decoded, received_at_ms));
        let actual_outcome = actual_outcome(actual_result);
        let expected_outcome = reference.ingest(&envelope, received_at_ms);

        assert_eq!(
            actual_outcome, expected_outcome,
            "Aggregator diverged from reference model for envelope {envelope:?}"
        );
        assert_eq!(actual.agent_count(), reference.agent_count());
        assert_eq!(actual.total_accepted(), reference.accepted);
        assert_eq!(actual.total_rejected(), reference.rejected);
        for sender in valid_sender_choices() {
            assert_eq!(
                actual.agent_last_seq(sender),
                reference.last_seq(sender),
                "last_seq diverged for sender {sender}"
            );
        }
    }
}

fuzz_target!(|input: FuzzInput| {
    let limits = WireProtocolLimits::default();
    match input {
        FuzzInput::Single(bytes) => {
            // Hit the parser-only path first (matches the wire frame
            // decode site that may peek before forwarding).
            parse_only(bytes, limits);
            // Then the parser+state path through Aggregator.
            let mut agg = Aggregator::with_limits(MAX_AGENTS, limits);
            parse_and_ingest(bytes, &mut agg);
        }
        FuzzInput::Sequence {
            max_agents_choice,
            ingests,
            check_stats,
        } => {
            let max_agents = (max_agents_choice as usize % MAX_AGENTS).max(1);
            let mut agg = Aggregator::with_limits(max_agents, limits);
            for bytes in ingests.into_iter().take(MAX_SEQUENCE_LEN) {
                parse_and_ingest(bytes, &mut agg);
            }
            if check_stats {
                // Contract 2: invariants the production saturating_add
                // semantics promise.
                assert!(
                    agg.agent_count() <= max_agents,
                    "agent_count {} exceeded max_agents {}",
                    agg.agent_count(),
                    max_agents
                );
                let _ = agg.total_accepted(); // panic-free read
                let _ = agg.total_rejected();
            }
        }
        FuzzInput::DifferentialSequence {
            max_agents_choice,
            stale_after_choice,
            commands,
        } => differential_sequence(max_agents_choice, stale_after_choice, commands),
    }
});
