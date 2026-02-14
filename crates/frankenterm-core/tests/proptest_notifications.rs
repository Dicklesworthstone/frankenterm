//! Property-based tests for the `notifications` module.
//!
//! Covers `NotificationPayload` serde roundtrips, field preservation,
//! confidence bounds, structural invariants, `NotificationDeliveryRecord`
//! and `NotificationDelivery` serialization properties.

use frankenterm_core::notifications::{
    NotificationDelivery, NotificationDeliveryRecord, NotificationPayload,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_notification_payload() -> impl Strategy<Value = NotificationPayload> {
    (
        "[a-z.]{3,20}",                                          // event_type
        0_u64..10_000,                                           // pane_id
        "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}", // timestamp
        "[A-Za-z ]{3,30}",                                       // summary
        "[A-Za-z ]{5,50}",                                       // description
        "info|warning|critical",                                 // severity
        "[a-z]{3,10}",                                           // agent_type
        0.0_f64..1.0,                                            // confidence
        proptest::option::of("[a-z ]{3,20}"),                    // quick_fix
        0_u64..100,                                              // suppressed_since_last
    )
        .prop_map(
            |(
                event_type,
                pane_id,
                timestamp,
                summary,
                description,
                severity,
                agent_type,
                confidence,
                quick_fix,
                suppressed,
            )| {
                NotificationPayload {
                    event_type,
                    pane_id,
                    timestamp,
                    summary,
                    description,
                    severity,
                    agent_type,
                    confidence,
                    quick_fix,
                    suppressed_since_last: suppressed,
                }
            },
        )
}

fn arb_delivery_record() -> impl Strategy<Value = NotificationDeliveryRecord> {
    (
        "[a-z_-]{3,20}",         // target
        proptest::bool::ANY,     // accepted
        0_u16..600,              // status_code
        proptest::option::of("[a-z ]{3,30}"), // error
    )
        .prop_map(|(target, accepted, status_code, error)| {
            NotificationDeliveryRecord {
                target,
                accepted,
                status_code,
                error,
            }
        })
}

fn arb_delivery() -> impl Strategy<Value = NotificationDelivery> {
    (
        "[a-z_]{3,15}",          // sender
        proptest::bool::ANY,     // success
        proptest::bool::ANY,     // rate_limited
        proptest::option::of("[a-z ]{3,30}"), // error
        proptest::collection::vec(arb_delivery_record(), 0..5), // records
    )
        .prop_map(|(sender, success, rate_limited, error, records)| {
            NotificationDelivery {
                sender,
                success,
                rate_limited,
                error,
                records,
            }
        })
}

// =========================================================================
// NotificationPayload — serde roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// NotificationPayload serde roundtrip preserves all fields.
    #[test]
    fn prop_payload_serde_roundtrip(payload in arb_notification_payload()) {
        let json = serde_json::to_string(&payload).unwrap();
        let back: NotificationPayload = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.event_type, &payload.event_type);
        prop_assert_eq!(back.pane_id, payload.pane_id);
        prop_assert_eq!(&back.timestamp, &payload.timestamp);
        prop_assert_eq!(&back.summary, &payload.summary);
        prop_assert_eq!(&back.description, &payload.description);
        prop_assert_eq!(&back.severity, &payload.severity);
        prop_assert_eq!(&back.agent_type, &payload.agent_type);
        prop_assert!((back.confidence - payload.confidence).abs() < f64::EPSILON);
        prop_assert_eq!(&back.quick_fix, &payload.quick_fix);
        prop_assert_eq!(back.suppressed_since_last, payload.suppressed_since_last);
    }

    /// Serialization is deterministic.
    #[test]
    fn prop_payload_serde_deterministic(payload in arb_notification_payload()) {
        let json1 = serde_json::to_string(&payload).unwrap();
        let json2 = serde_json::to_string(&payload).unwrap();
        prop_assert_eq!(&json1, &json2);
    }

    /// JSON with quick_fix=None omits the field (skip_serializing_if).
    #[test]
    fn prop_payload_none_quick_fix_omitted(
        event_type in "[a-z.]{3,10}",
        pane_id in 0_u64..100,
    ) {
        let payload = NotificationPayload {
            event_type,
            pane_id,
            timestamp: "2026-01-01T00:00:00".to_string(),
            summary: "test".to_string(),
            description: "test".to_string(),
            severity: "info".to_string(),
            agent_type: "test".to_string(),
            confidence: 0.5,
            quick_fix: None,
            suppressed_since_last: 0,
        };
        let json = serde_json::to_string(&payload).unwrap();
        prop_assert!(
            !json.contains("quick_fix"),
            "None quick_fix should be omitted from JSON: {}", json
        );
    }

    /// JSON with quick_fix=Some includes the field.
    #[test]
    fn prop_payload_some_quick_fix_included(
        fix in "[a-z ]{3,20}",
    ) {
        let payload = NotificationPayload {
            event_type: "test.event".to_string(),
            pane_id: 1,
            timestamp: "2026-01-01T00:00:00".to_string(),
            summary: "test".to_string(),
            description: "test".to_string(),
            severity: "info".to_string(),
            agent_type: "test".to_string(),
            confidence: 0.5,
            quick_fix: Some(fix.clone()),
            suppressed_since_last: 0,
        };
        let json = serde_json::to_string(&payload).unwrap();
        prop_assert!(
            json.contains("quick_fix"),
            "Some quick_fix should be present in JSON"
        );
    }
}

// =========================================================================
// NotificationPayload — structural properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Confidence in payload is always in [0.0, 1.0].
    #[test]
    fn prop_payload_confidence_bounded(payload in arb_notification_payload()) {
        prop_assert!(payload.confidence >= 0.0, "confidence {} < 0", payload.confidence);
        prop_assert!(payload.confidence <= 1.0, "confidence {} > 1", payload.confidence);
    }

    /// Severity is always one of the known values.
    #[test]
    fn prop_payload_severity_valid(payload in arb_notification_payload()) {
        let valid = ["info", "warning", "critical"];
        prop_assert!(
            valid.contains(&payload.severity.as_str()),
            "severity '{}' should be info/warning/critical", payload.severity
        );
    }

    /// Event type is never empty.
    #[test]
    fn prop_payload_event_type_nonempty(payload in arb_notification_payload()) {
        prop_assert!(!payload.event_type.is_empty(), "event_type should not be empty");
    }

    /// JSON produced is always valid (parseable as serde_json::Value).
    #[test]
    fn prop_payload_json_always_valid(payload in arb_notification_payload()) {
        let json = serde_json::to_string(&payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object(), "payload JSON should be an object");
    }

    /// JSON field count matches expected: 9 without quick_fix, 10 with.
    #[test]
    fn prop_payload_json_field_count(payload in arb_notification_payload()) {
        let json = serde_json::to_string(&payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        let expected = if payload.quick_fix.is_some() { 10 } else { 9 };
        prop_assert_eq!(
            obj.len(), expected,
            "expected {} fields, got {}", expected, obj.len()
        );
    }

    /// Roundtrip through serde_json::Value preserves payload equality.
    #[test]
    fn prop_payload_value_roundtrip(payload in arb_notification_payload()) {
        let value = serde_json::to_value(&payload).unwrap();
        let back: NotificationPayload = serde_json::from_value(value).unwrap();
        prop_assert_eq!(&back.event_type, &payload.event_type);
        prop_assert_eq!(back.pane_id, payload.pane_id);
        prop_assert!((back.confidence - payload.confidence).abs() < 1e-10);
    }

    /// Large pane_id values survive serde roundtrip.
    #[test]
    fn prop_payload_large_pane_id(pane_id in u64::MAX / 2..u64::MAX) {
        let payload = NotificationPayload {
            event_type: "test".to_string(),
            pane_id,
            timestamp: "2026-01-01T00:00:00".to_string(),
            summary: "s".to_string(),
            description: "d".to_string(),
            severity: "info".to_string(),
            agent_type: "a".to_string(),
            confidence: 0.5,
            quick_fix: None,
            suppressed_since_last: 0,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: NotificationPayload = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, pane_id);
    }

    /// Large suppressed_since_last values survive serde roundtrip.
    #[test]
    fn prop_payload_large_suppressed(suppressed in u64::MAX / 2..u64::MAX) {
        let payload = NotificationPayload {
            event_type: "test".to_string(),
            pane_id: 1,
            timestamp: "2026-01-01T00:00:00".to_string(),
            summary: "s".to_string(),
            description: "d".to_string(),
            severity: "warning".to_string(),
            agent_type: "a".to_string(),
            confidence: 0.5,
            quick_fix: None,
            suppressed_since_last: suppressed,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: NotificationPayload = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.suppressed_since_last, suppressed);
    }

    /// Confidence boundary values (0.0 and values near 1.0) roundtrip correctly.
    #[test]
    fn prop_payload_confidence_boundary(conf in prop_oneof![
        Just(0.0_f64),
        Just(0.5_f64),
        Just(1.0_f64),
        0.0_f64..=1.0,
    ]) {
        let payload = NotificationPayload {
            event_type: "test".to_string(),
            pane_id: 1,
            timestamp: "2026-01-01T00:00:00".to_string(),
            summary: "s".to_string(),
            description: "d".to_string(),
            severity: "info".to_string(),
            agent_type: "a".to_string(),
            confidence: conf,
            quick_fix: None,
            suppressed_since_last: 0,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: NotificationPayload = serde_json::from_str(&json).unwrap();
        prop_assert!((back.confidence - conf).abs() < 1e-10,
            "confidence mismatch: got {}, expected {}", back.confidence, conf);
    }
}

// =========================================================================
// NotificationDeliveryRecord — serialization properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// NotificationDeliveryRecord serializes to valid JSON.
    #[test]
    fn prop_record_serializes_to_valid_json(record in arb_delivery_record()) {
        let json = serde_json::to_string(&record).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Record serialization is deterministic.
    #[test]
    fn prop_record_serde_deterministic(record in arb_delivery_record()) {
        let json1 = serde_json::to_string(&record).unwrap();
        let json2 = serde_json::to_string(&record).unwrap();
        prop_assert_eq!(&json1, &json2);
    }

    /// Record JSON contains required fields.
    #[test]
    fn prop_record_json_has_required_fields(record in arb_delivery_record()) {
        let json = serde_json::to_string(&record).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        prop_assert!(obj.contains_key("target"), "missing 'target' field");
        prop_assert!(obj.contains_key("accepted"), "missing 'accepted' field");
        prop_assert!(obj.contains_key("status_code"), "missing 'status_code' field");
    }

    /// Record target field matches the struct value.
    #[test]
    fn prop_record_target_preserved(record in arb_delivery_record()) {
        let value = serde_json::to_value(&record).unwrap();
        let target_json = value["target"].as_str().unwrap();
        prop_assert_eq!(target_json, &record.target);
    }

    /// Record accepted boolean is preserved in JSON.
    #[test]
    fn prop_record_accepted_preserved(record in arb_delivery_record()) {
        let value = serde_json::to_value(&record).unwrap();
        let accepted_json = value["accepted"].as_bool().unwrap();
        prop_assert_eq!(accepted_json, record.accepted);
    }

    /// Record status_code is preserved in JSON.
    #[test]
    fn prop_record_status_code_preserved(record in arb_delivery_record()) {
        let value = serde_json::to_value(&record).unwrap();
        let code_json = value["status_code"].as_u64().unwrap() as u16;
        prop_assert_eq!(code_json, record.status_code);
    }
}

// =========================================================================
// NotificationDelivery — serialization properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// NotificationDelivery serializes to valid JSON.
    #[test]
    fn prop_delivery_serializes_to_valid_json(delivery in arb_delivery()) {
        let json = serde_json::to_string(&delivery).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Delivery serialization is deterministic.
    #[test]
    fn prop_delivery_serde_deterministic(delivery in arb_delivery()) {
        let json1 = serde_json::to_string(&delivery).unwrap();
        let json2 = serde_json::to_string(&delivery).unwrap();
        prop_assert_eq!(&json1, &json2);
    }

    /// Delivery JSON contains all required fields.
    #[test]
    fn prop_delivery_json_has_required_fields(delivery in arb_delivery()) {
        let value = serde_json::to_value(&delivery).unwrap();
        let obj = value.as_object().unwrap();
        prop_assert!(obj.contains_key("sender"), "missing 'sender'");
        prop_assert!(obj.contains_key("success"), "missing 'success'");
        prop_assert!(obj.contains_key("rate_limited"), "missing 'rate_limited'");
        prop_assert!(obj.contains_key("records"), "missing 'records'");
    }

    /// Delivery records array length matches struct.
    #[test]
    fn prop_delivery_records_count_preserved(delivery in arb_delivery()) {
        let value = serde_json::to_value(&delivery).unwrap();
        let records_json = value["records"].as_array().unwrap();
        prop_assert_eq!(
            records_json.len(), delivery.records.len(),
            "records count mismatch"
        );
    }

    /// Delivery sender name is preserved in JSON.
    #[test]
    fn prop_delivery_sender_preserved(delivery in arb_delivery()) {
        let value = serde_json::to_value(&delivery).unwrap();
        let sender_json = value["sender"].as_str().unwrap();
        prop_assert_eq!(sender_json, &delivery.sender);
    }

    /// Delivery success and rate_limited booleans are preserved.
    #[test]
    fn prop_delivery_booleans_preserved(delivery in arb_delivery()) {
        let value = serde_json::to_value(&delivery).unwrap();
        prop_assert_eq!(value["success"].as_bool().unwrap(), delivery.success);
        prop_assert_eq!(value["rate_limited"].as_bool().unwrap(), delivery.rate_limited);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn payload_minimal_roundtrip() {
    let payload = NotificationPayload {
        event_type: "e".to_string(),
        pane_id: 0,
        timestamp: "t".to_string(),
        summary: "s".to_string(),
        description: "d".to_string(),
        severity: "info".to_string(),
        agent_type: "a".to_string(),
        confidence: 0.0,
        quick_fix: None,
        suppressed_since_last: 0,
    };
    let json = serde_json::to_string(&payload).unwrap();
    let back: NotificationPayload = serde_json::from_str(&json).unwrap();
    assert_eq!(back.event_type, "e");
    assert_eq!(back.pane_id, 0);
}

#[test]
fn payload_with_all_fields() {
    let payload = NotificationPayload {
        event_type: "core.codex:error".to_string(),
        pane_id: 42,
        timestamp: "2026-01-29T17:00:00+00:00".to_string(),
        summary: "Codex error detected".to_string(),
        description: "The codex agent encountered an error.".to_string(),
        severity: "critical".to_string(),
        agent_type: "codex".to_string(),
        confidence: 0.99,
        quick_fix: Some("restart codex".to_string()),
        suppressed_since_last: 5,
    };
    let json = serde_json::to_string(&payload).unwrap();
    assert!(json.contains("quick_fix"));
    let back: NotificationPayload = serde_json::from_str(&json).unwrap();
    assert_eq!(back.quick_fix, Some("restart codex".to_string()));
    assert_eq!(back.suppressed_since_last, 5);
}

#[test]
fn delivery_record_empty_error_serializes() {
    let record = NotificationDeliveryRecord {
        target: "webhook".to_string(),
        accepted: true,
        status_code: 200,
        error: None,
    };
    let json = serde_json::to_string(&record).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(value.is_object());
    assert_eq!(value["status_code"], 200);
}

#[test]
fn delivery_with_records_serializes() {
    let delivery = NotificationDelivery {
        sender: "test".to_string(),
        success: true,
        rate_limited: false,
        error: None,
        records: vec![
            NotificationDeliveryRecord {
                target: "a".to_string(),
                accepted: true,
                status_code: 200,
                error: None,
            },
            NotificationDeliveryRecord {
                target: "b".to_string(),
                accepted: false,
                status_code: 500,
                error: Some("server error".to_string()),
            },
        ],
    };
    let json = serde_json::to_string(&delivery).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["records"].as_array().unwrap().len(), 2);
}
