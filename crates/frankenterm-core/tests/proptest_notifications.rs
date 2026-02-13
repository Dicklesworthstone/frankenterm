//! Property-based tests for the `notifications` module.
//!
//! Covers `NotificationPayload` serde roundtrips, field preservation,
//! confidence bounds, and structural invariants.

use frankenterm_core::notifications::NotificationPayload;
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_notification_payload() -> impl Strategy<Value = NotificationPayload> {
    (
        "[a-z.]{3,20}",                          // event_type
        0_u64..10_000,                            // pane_id
        "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}", // timestamp
        "[A-Za-z ]{3,30}",                       // summary
        "[A-Za-z ]{5,50}",                       // description
        "info|warning|critical",                  // severity
        "[a-z]{3,10}",                            // agent_type
        0.0_f64..1.0,                             // confidence
        proptest::option::of("[a-z ]{3,20}"),     // quick_fix
        0_u64..100,                               // suppressed_since_last
    )
        .prop_map(
            |(event_type, pane_id, timestamp, summary, description, severity, agent_type, confidence, quick_fix, suppressed)| {
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
