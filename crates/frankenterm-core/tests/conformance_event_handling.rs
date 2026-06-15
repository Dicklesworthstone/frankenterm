//! Conformance + golden tests for runtime Event handling (ft-kch2u).
//!
//! The [`Event`] enum is internally-tagged serde (`#[serde(tag = "type",
//! rename_all = "snake_case")]`) and is the payload that flows through the IPC
//! `watch-events` / `SubscribeEvents` live transport, so its serialized shape is
//! a wire contract. These tests:
//!
//! 1. pin the exact tagged JSON (golden) for every non-feature-gated variant so
//!    a field rename/addition/removal fails CI instead of silently breaking IPC
//!    subscribers;
//! 2. prove `Event::type_name()` always equals the serde `type` tag and that
//!    `pane_id()` reflects the variant;
//! 3. prove every variant value round-trips losslessly (`Event` is not
//!    `PartialEq`, so values are compared after a serialize → deserialize →
//!    re-serialize cycle);
//! 4. prove the causality clocks (`VectorClock`, `LamportStamp`) obey their
//!    documented ordering/merge laws.
//!
//! Pure serde + logic — no RCH, no IO.

use frankenterm_core::events::{CausalRelation, Event, LamportStamp, UserVarPayload, VectorClock};
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use serde_json::{Value, json};

fn sample_detection() -> Detection {
    Detection {
        rule_id: "core.codex:usage_reached".to_string(),
        agent_type: AgentType::Codex,
        event_type: "usage_reached".to_string(),
        severity: Severity::Warning,
        confidence: 0.875,
        extracted: json!({ "reset_at": "2026-01-20" }),
        matched_text: "Usage limit reached".to_string(),
        // `span` is #[serde(skip)] on Detection, so it never appears on the wire.
        span: (0, 19),
    }
}

/// One instance of every non-feature-gated `Event` variant.
fn all_sample_events() -> Vec<Event> {
    vec![
        Event::SegmentCaptured {
            pane_id: 7,
            seq: 3,
            content_len: 42,
        },
        Event::GapDetected {
            pane_id: 7,
            seq_before: 3,
            seq_after: 9,
            reason: "overflow".to_string(),
            detected_at_ms: 1234,
        },
        Event::PatternDetected {
            pane_id: 7,
            pane_uuid: Some("uuid-1".to_string()),
            detection: sample_detection(),
            event_id: Some(99),
        },
        Event::PaneDiscovered {
            pane_id: 7,
            domain: "local".to_string(),
            title: "codex".to_string(),
        },
        Event::PaneDisappeared { pane_id: 7 },
        Event::WorkflowStarted {
            workflow_id: "w1".to_string(),
            workflow_name: "handle_compaction".to_string(),
            pane_id: 7,
        },
        Event::WorkflowStep {
            workflow_id: "w1".to_string(),
            step_name: "send".to_string(),
            result: "ok".to_string(),
        },
        Event::WorkflowCompleted {
            workflow_id: "w1".to_string(),
            success: true,
            reason: Some("done".to_string()),
        },
        Event::UserVarReceived {
            pane_id: 7,
            name: "FT_EVENT".to_string(),
            payload: UserVarPayload {
                value: "cmd=x".to_string(),
                event_type: Some("cmd_end".to_string()),
                event_data: None,
            },
        },
    ]
}

#[test]
fn event_serde_golden_pins_tagged_wire_schema() {
    // Full-shape goldens for the directly-constructible variants. PatternDetected
    // is excluded here because its nested Detection shape is owned/tested by
    // patterns.rs; it is still covered by the totality/round-trip tests below.
    let cases: Vec<(Event, Value)> = vec![
        (
            Event::SegmentCaptured {
                pane_id: 7,
                seq: 3,
                content_len: 42,
            },
            json!({"type": "segment_captured", "pane_id": 7, "seq": 3, "content_len": 42}),
        ),
        (
            Event::GapDetected {
                pane_id: 7,
                seq_before: 3,
                seq_after: 9,
                reason: "overflow".to_string(),
                detected_at_ms: 1234,
            },
            json!({"type": "gap_detected", "pane_id": 7, "seq_before": 3, "seq_after": 9, "reason": "overflow", "detected_at_ms": 1234}),
        ),
        (
            Event::PaneDiscovered {
                pane_id: 7,
                domain: "local".to_string(),
                title: "codex".to_string(),
            },
            json!({"type": "pane_discovered", "pane_id": 7, "domain": "local", "title": "codex"}),
        ),
        (
            Event::PaneDisappeared { pane_id: 7 },
            json!({"type": "pane_disappeared", "pane_id": 7}),
        ),
        (
            Event::WorkflowStarted {
                workflow_id: "w1".to_string(),
                workflow_name: "handle_compaction".to_string(),
                pane_id: 7,
            },
            json!({"type": "workflow_started", "workflow_id": "w1", "workflow_name": "handle_compaction", "pane_id": 7}),
        ),
        (
            Event::WorkflowStep {
                workflow_id: "w1".to_string(),
                step_name: "send".to_string(),
                result: "ok".to_string(),
            },
            json!({"type": "workflow_step", "workflow_id": "w1", "step_name": "send", "result": "ok"}),
        ),
        (
            Event::WorkflowCompleted {
                workflow_id: "w1".to_string(),
                success: true,
                reason: None,
            },
            json!({"type": "workflow_completed", "workflow_id": "w1", "success": true, "reason": null}),
        ),
        (
            Event::UserVarReceived {
                pane_id: 7,
                name: "FT_EVENT".to_string(),
                payload: UserVarPayload {
                    value: "cmd=x".to_string(),
                    event_type: Some("cmd_end".to_string()),
                    event_data: None,
                },
            },
            json!({"type": "user_var_received", "pane_id": 7, "name": "FT_EVENT", "payload": {"value": "cmd=x", "event_type": "cmd_end", "event_data": null}}),
        ),
    ];

    for (event, golden) in cases {
        let actual = serde_json::to_value(&event).expect("event serializes");
        assert_eq!(
            actual,
            golden,
            "wire-schema drift for Event::{}",
            event.type_name()
        );
    }
}

#[test]
fn type_name_matches_serde_tag_for_every_variant() {
    for event in all_sample_events() {
        let value = serde_json::to_value(&event).expect("event serializes");
        let tag = value
            .get("type")
            .and_then(Value::as_str)
            .expect("internally-tagged Event must carry a string `type`");
        assert_eq!(
            event.type_name(),
            tag,
            "type_name() must equal the serde `type` tag"
        );
        assert!(!event.type_name().is_empty());
    }
}

#[test]
fn pane_id_reflects_variant() {
    assert_eq!(
        Event::SegmentCaptured {
            pane_id: 5,
            seq: 0,
            content_len: 0
        }
        .pane_id(),
        Some(5)
    );
    assert_eq!(Event::PaneDisappeared { pane_id: 9 }.pane_id(), Some(9));
    assert_eq!(
        Event::PatternDetected {
            pane_id: 11,
            pane_uuid: None,
            detection: sample_detection(),
            event_id: None,
        }
        .pane_id(),
        Some(11)
    );
    // Workflow step/completed are not pane-scoped.
    assert_eq!(
        Event::WorkflowStep {
            workflow_id: "w".to_string(),
            step_name: "s".to_string(),
            result: "r".to_string(),
        }
        .pane_id(),
        None
    );
    assert_eq!(
        Event::WorkflowCompleted {
            workflow_id: "w".to_string(),
            success: false,
            reason: None,
        }
        .pane_id(),
        None
    );
}

#[test]
fn event_value_roundtrip_is_lossless() {
    for event in all_sample_events() {
        let value = serde_json::to_value(&event).expect("event serializes");
        let decoded: Event =
            serde_json::from_value(value.clone()).expect("event deserializes from its own value");
        let reserialized = serde_json::to_value(&decoded).expect("decoded event re-serializes");
        assert_eq!(
            value,
            reserialized,
            "round-trip drift for Event::{}",
            event.type_name()
        );
    }
}

#[test]
fn vector_clock_relation_conformance() {
    let empty_a = VectorClock::new();
    let empty_b = VectorClock::new();
    assert_eq!(empty_a.relation_to(&empty_b), CausalRelation::Equal);

    let mut a = VectorClock::new();
    a.increment("n1"); // {n1:1}
    // a dominates the empty clock.
    assert_eq!(a.relation_to(&empty_b), CausalRelation::After);
    assert_eq!(empty_b.relation_to(&a), CausalRelation::Before);

    let mut b = VectorClock::new();
    b.increment("n2"); // {n2:1}; divergent from a -> concurrent (both directions).
    assert_eq!(a.relation_to(&b), CausalRelation::Concurrent);
    assert_eq!(b.relation_to(&a), CausalRelation::Concurrent);

    // The merged clock dominates both inputs and equals an identical frontier.
    let mut merged = VectorClock::new();
    merged.increment("n1");
    merged.merge(&b); // {n1:1, n2:1}
    assert_eq!(merged.relation_to(&a), CausalRelation::After);
    assert_eq!(merged.relation_to(&b), CausalRelation::After);

    let mut twin = VectorClock::new();
    twin.increment("n1");
    twin.increment("n2");
    assert_eq!(merged.relation_to(&twin), CausalRelation::Equal);
}

#[test]
fn vector_clock_merge_is_pointwise_max_and_idempotent() {
    let mut a = VectorClock::new();
    a.increment("n1");
    a.increment("n1"); // n1:2

    let mut b = VectorClock::new();
    b.increment("n1"); // n1:1
    b.increment("n2"); // n2:1

    a.merge(&b);
    assert_eq!(a.get("n1"), 2, "pointwise max keeps the larger counter");
    assert_eq!(a.get("n2"), 1);
    assert_eq!(a.node_count(), 2);
    assert_eq!(a.get("absent"), 0, "missing nodes read as zero");

    // Merging the same clock again is a no-op.
    let n1_before = a.get("n1");
    a.merge(&b);
    assert_eq!(a.get("n1"), n1_before);
    assert_eq!(a.get("n2"), 1);
}

#[test]
fn lamport_stamp_total_order_and_roundtrip() {
    let a = LamportStamp {
        counter: 1,
        node_id: "n1".to_string(),
    };
    let b = LamportStamp {
        counter: 2,
        node_id: "n1".to_string(),
    };
    let c = LamportStamp {
        counter: 2,
        node_id: "n2".to_string(),
    };
    // Counter dominates; node_id breaks ties deterministically.
    assert!(a < b);
    assert!(b < c);
    assert!(a < c);

    let value = serde_json::to_value(&b).expect("lamport serializes");
    assert_eq!(value, json!({"counter": 2, "node_id": "n1"}));
    let back: LamportStamp = serde_json::from_value(value).expect("lamport deserializes");
    assert_eq!(back, b);
}
