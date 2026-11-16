// Requires the `agent-mail` feature flag.
#![cfg(feature = "agent-mail")]
//! Property-based tests for agent mail bridge types (ft-3kxe).
//!
//! Validates:
//! 1. MessageId serde roundtrip, equality, and Hash consistency
//! 2. MailMessage serde roundtrip and forward compatibility
//! 3. ReservationStatus enum serde roundtrip and variant discrimination
//! 4. ReservationResult serde roundtrip with granted/conflict vectors
//! 5. FileConflict serde roundtrip and forward compatibility
//! 6. AgentRegistration serde roundtrip
//! 7. SendResponse serde roundtrip
//! 8. ReleaseResponse serde roundtrip
//! 9. AgentMailBridgeError Display/Clone/Eq

use proptest::prelude::*;
use std::collections::HashMap;

use frankenterm_core::agent_mail_bridge::{
    AgentMailBridgeError, AgentRegistration, FileConflict, MailMessage, MessageId, ReleaseResponse,
    ReservationResult, ReservationStatus, SendResponse,
};

// =============================================================================
// Strategies
// =============================================================================

fn message_id_strategy() -> impl Strategy<Value = String> {
    "[a-z0-9-]{1,30}".prop_map(String::from)
}

fn agent_name_strategy() -> impl Strategy<Value = String> {
    "[A-Z][a-z]{2,10}[A-Z][a-z]{2,10}".prop_map(String::from)
}

fn subject_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 .:!?-]{0,80}".prop_map(String::from)
}

fn body_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 \n.:!?-]{0,200}".prop_map(String::from)
}

fn reservation_status_strategy() -> impl Strategy<Value = ReservationStatus> {
    prop_oneof![
        Just(ReservationStatus::Granted),
        Just(ReservationStatus::Conflict),
        Just(ReservationStatus::Unavailable),
    ]
}

fn file_path_strategy() -> impl Strategy<Value = String> {
    "[a-z_/]{1,40}\\.rs".prop_map(String::from)
}

fn mail_message_strategy() -> impl Strategy<Value = MailMessage> {
    (
        prop::option::of(message_id_strategy()),
        prop::option::of(agent_name_strategy()),
        prop::option::of(agent_name_strategy()),
        subject_strategy(),
        body_strategy(),
        prop::option::of("[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z"),
        prop::bool::ANY,
        prop::option::of(0u32..100),
    )
        .prop_map(
            |(id, from, to, subject, body, timestamp, read, priority)| MailMessage {
                id,
                from,
                to,
                subject,
                body,
                timestamp,
                read,
                priority,
                extra: HashMap::new(),
            },
        )
}

fn file_conflict_strategy() -> impl Strategy<Value = FileConflict> {
    (
        file_path_strategy(),
        prop::option::of(agent_name_strategy()),
        prop::option::of("[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z"),
    )
        .prop_map(|(path, held_by, since)| FileConflict {
            path,
            held_by,
            since,
            extra: HashMap::new(),
        })
}

fn reservation_result_strategy() -> impl Strategy<Value = ReservationResult> {
    (
        reservation_status_strategy(),
        prop::collection::vec(file_path_strategy(), 0..5),
        prop::collection::vec(file_conflict_strategy(), 0..3),
    )
        .prop_map(|(status, granted, conflicts)| ReservationResult {
            status,
            granted,
            conflicts,
            extra: HashMap::new(),
        })
}

fn agent_registration_strategy() -> impl Strategy<Value = AgentRegistration> {
    (
        prop::option::of(0u64..100_000),
        prop::option::of(agent_name_strategy()),
        prop::option::of("[a-z/]{1,40}"),
    )
        .prop_map(|(agent_id, agent_name, project)| AgentRegistration {
            agent_id,
            agent_name,
            project,
            extra: HashMap::new(),
        })
}

fn send_response_strategy() -> impl Strategy<Value = SendResponse> {
    (prop::option::of(message_id_strategy()), prop::bool::ANY).prop_map(|(message_id, success)| {
        SendResponse {
            message_id,
            success,
            extra: HashMap::new(),
        }
    })
}

fn release_response_strategy() -> impl Strategy<Value = ReleaseResponse> {
    (0usize..100, prop::bool::ANY).prop_map(|(released, success)| ReleaseResponse {
        released,
        success,
        extra: HashMap::new(),
    })
}

fn bridge_error_strategy() -> impl Strategy<Value = AgentMailBridgeError> {
    prop_oneof![
        "[a-zA-Z0-9 ]{1,50}".prop_map(AgentMailBridgeError::SendFailed),
        "[a-zA-Z0-9 ]{1,50}".prop_map(AgentMailBridgeError::ReleaseFailed),
    ]
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn message_id_serde_roundtrip() {
    let id = MessageId::new("msg-42");
    let json = serde_json::to_string(&id).unwrap();
    let back: MessageId = serde_json::from_str(&json).unwrap();
    assert_eq!(back, id);
}

#[test]
fn message_id_hash_consistent() {
    use std::collections::HashSet;
    let mut set = HashSet::new();
    set.insert(MessageId::new("same"));
    set.insert(MessageId::new("same"));
    assert_eq!(set.len(), 1);
}

#[test]
fn reservation_status_snake_case_serialization() {
    let json = serde_json::to_string(&ReservationStatus::Granted).unwrap();
    assert_eq!(json, r#""granted""#);
    let json = serde_json::to_string(&ReservationStatus::Conflict).unwrap();
    assert_eq!(json, r#""conflict""#);
    let json = serde_json::to_string(&ReservationStatus::Unavailable).unwrap();
    assert_eq!(json, r#""unavailable""#);
}

#[test]
fn mail_message_minimal_json_deserializes() {
    let msg: MailMessage = serde_json::from_str("{}").unwrap();
    assert!(msg.id.is_none());
    assert!(msg.from.is_none());
    assert!(msg.subject.is_empty());
    assert!(!msg.read);
}

#[test]
fn mail_message_extra_fields_captured() {
    let json = r#"{"subject": "test", "unknown_field": 42}"#;
    let msg: MailMessage = serde_json::from_str(json).unwrap();
    assert!(msg.extra.contains_key("unknown_field"));
}

#[test]
fn bridge_error_display_send_failed() {
    let err = AgentMailBridgeError::SendFailed("timeout".to_string());
    let display = err.to_string();
    assert!(display.contains("send_message failed"));
    assert!(display.contains("timeout"));
}

#[test]
fn bridge_error_display_release_failed() {
    let err = AgentMailBridgeError::ReleaseFailed("not found".to_string());
    let display = err.to_string();
    assert!(display.contains("release_files failed"));
    assert!(display.contains("not found"));
}

#[test]
fn bridge_error_clone_eq() {
    let a = AgentMailBridgeError::SendFailed("x".to_string());
    let b = a.clone();
    assert_eq!(a, b);
}

#[test]
fn bridge_error_different_variants_not_equal() {
    let a = AgentMailBridgeError::SendFailed("x".to_string());
    let b = AgentMailBridgeError::ReleaseFailed("x".to_string());
    assert_ne!(a, b);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // ── MessageId serde roundtrip ───────────────────────────────────────

    #[test]
    fn message_id_roundtrip(id_str in message_id_strategy()) {
        let id = MessageId::new(&id_str);
        let json = serde_json::to_string(&id).expect("serialize");
        let back: MessageId = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&back, &id);
        prop_assert_eq!(back.as_str(), id_str.as_str());
    }

    // ── MessageId equality and clone ────────────────────────────────────

    #[test]
    fn message_id_clone_eq(id_str in message_id_strategy()) {
        let id = MessageId::new(&id_str);
        let cloned = id.clone();
        prop_assert_eq!(&id, &cloned);
    }

    #[test]
    fn message_id_different_not_eq(
        a in "[a-z]{1,10}",
        b in "[a-z]{1,10}",
    ) {
        prop_assume!(a != b);
        prop_assert_ne!(&MessageId::new(&a), &MessageId::new(&b));
    }

    // ── MessageId hash consistency ──────────────────────────────────────

    #[test]
    fn message_id_hash_consistent_prop(id_str in message_id_strategy()) {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(MessageId::new(&id_str));
        set.insert(MessageId::new(&id_str));
        prop_assert_eq!(set.len(), 1);
    }

    // ── MailMessage serde roundtrip ─────────────────────────────────────

    #[test]
    fn mail_message_roundtrip(msg in mail_message_strategy()) {
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: MailMessage = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&back.id, &msg.id);
        prop_assert_eq!(&back.from, &msg.from);
        prop_assert_eq!(&back.to, &msg.to);
        prop_assert_eq!(&back.subject, &msg.subject);
        prop_assert_eq!(&back.body, &msg.body);
        prop_assert_eq!(back.read, msg.read);
        prop_assert_eq!(&back.priority, &msg.priority);
    }

    // ── MailMessage forward compatibility ───────────────────────────────

    #[test]
    fn mail_message_extra_survives_roundtrip(
        key in "[a-z_]{3,12}",
        val in prop::num::i64::ANY,
    ) {
        let mut msg = MailMessage {
            id: None, from: None, to: None,
            subject: String::new(), body: String::new(),
            timestamp: None, read: false, priority: None,
            extra: HashMap::new(),
        };
        msg.extra.insert(key.clone(), serde_json::Value::Number(val.into()));

        let json = serde_json::to_string(&msg).expect("serialize");
        let back: MailMessage = serde_json::from_str(&json).expect("deserialize");

        prop_assert!(
            back.extra.contains_key(&key),
            "extra field '{}' lost in roundtrip",
            key
        );
        prop_assert_eq!(
            back.extra.get(&key).and_then(serde_json::Value::as_i64),
            Some(val),
        );
    }

    // ── ReservationStatus serde roundtrip ────────────────────────────────

    #[test]
    fn reservation_status_roundtrip(status in reservation_status_strategy()) {
        let json = serde_json::to_string(&status).expect("serialize");
        let back: ReservationStatus = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&back, &status);
    }

    // ── ReservationResult serde roundtrip ────────────────────────────────

    #[test]
    fn reservation_result_roundtrip(result in reservation_result_strategy()) {
        let json = serde_json::to_string(&result).expect("serialize");
        let back: ReservationResult = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&back.status, &result.status);
        prop_assert_eq!(&back.granted, &result.granted);
        prop_assert_eq!(back.conflicts.len(), result.conflicts.len());
        for (a, b) in back.conflicts.iter().zip(result.conflicts.iter()) {
            prop_assert_eq!(&a.path, &b.path);
            prop_assert_eq!(&a.held_by, &b.held_by);
            prop_assert_eq!(&a.since, &b.since);
        }
    }

    // ── ReservationResult granted count matches vec length ──────────────

    #[test]
    fn reservation_result_granted_count(
        paths in prop::collection::vec(file_path_strategy(), 0..10),
    ) {
        let result = ReservationResult {
            status: ReservationStatus::Granted,
            granted: paths.clone(),
            conflicts: Vec::new(),
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let back: ReservationResult = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(back.granted.len(), paths.len());
    }

    // ── FileConflict serde roundtrip ────────────────────────────────────

    #[test]
    fn file_conflict_roundtrip(fc in file_conflict_strategy()) {
        let json = serde_json::to_string(&fc).expect("serialize");
        let back: FileConflict = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&back.path, &fc.path);
        prop_assert_eq!(&back.held_by, &fc.held_by);
        prop_assert_eq!(&back.since, &fc.since);
    }

    // ── FileConflict forward compatibility ──────────────────────────────

    #[test]
    fn file_conflict_extra_survives(
        key in "[a-z_]{3,12}",
        val in "[a-zA-Z0-9]{1,20}",
    ) {
        let mut fc = FileConflict {
            path: "src/lib.rs".to_string(),
            held_by: None,
            since: None,
            extra: HashMap::new(),
        };
        fc.extra.insert(key.clone(), serde_json::Value::String(val.clone()));

        let json = serde_json::to_string(&fc).expect("serialize");
        let back: FileConflict = serde_json::from_str(&json).expect("deserialize");

        prop_assert!(back.extra.contains_key(&key));
        prop_assert_eq!(
            back.extra.get(&key).and_then(serde_json::Value::as_str),
            Some(val.as_str()),
        );
    }

    // ── AgentRegistration serde roundtrip ────────────────────────────────

    #[test]
    fn agent_registration_roundtrip(reg in agent_registration_strategy()) {
        let json = serde_json::to_string(&reg).expect("serialize");
        let back: AgentRegistration = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&back.agent_id, &reg.agent_id);
        prop_assert_eq!(&back.agent_name, &reg.agent_name);
        prop_assert_eq!(&back.project, &reg.project);
    }

    // ── SendResponse serde roundtrip ────────────────────────────────────

    #[test]
    fn send_response_roundtrip(resp in send_response_strategy()) {
        let json = serde_json::to_string(&resp).expect("serialize");
        let back: SendResponse = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&back.message_id, &resp.message_id);
        prop_assert_eq!(back.success, resp.success);
    }

    // ── ReleaseResponse serde roundtrip ─────────────────────────────────

    #[test]
    fn release_response_roundtrip(resp in release_response_strategy()) {
        let json = serde_json::to_string(&resp).expect("serialize");
        let back: ReleaseResponse = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(back.released, resp.released);
        prop_assert_eq!(back.success, resp.success);
    }

    // ── AgentMailBridgeError Display always non-empty ────────────────────

    #[test]
    fn bridge_error_display_nonempty(err in bridge_error_strategy()) {
        let e: &AgentMailBridgeError = &err;
        let display = e.to_string();
        prop_assert!(!display.is_empty());
    }

    // ── AgentMailBridgeError Clone produces equal values ─────────────────

    #[test]
    fn bridge_error_clone_is_equal(err in bridge_error_strategy()) {
        let e: &AgentMailBridgeError = &err;
        let cloned = e.clone();
        prop_assert_eq!(&err, &cloned);
    }

    // ── AgentMailBridgeError variant discrimination ──────────────────────

    #[test]
    fn bridge_error_send_contains_message(msg in "[a-zA-Z0-9 ]{1,30}") {
        let err = AgentMailBridgeError::SendFailed(msg.clone());
        let display = err.to_string();
        prop_assert!(
            display.contains(&msg),
            "Display '{}' should contain message '{}'",
            display, msg
        );
        prop_assert!(display.contains("send_message failed"));
    }

    #[test]
    fn bridge_error_release_contains_message(msg in "[a-zA-Z0-9 ]{1,30}") {
        let err = AgentMailBridgeError::ReleaseFailed(msg.clone());
        let display = err.to_string();
        prop_assert!(
            display.contains(&msg),
            "Display '{}' should contain message '{}'",
            display, msg
        );
        prop_assert!(display.contains("release_files failed"));
    }

    // ── ReservationStatus all variants reachable ────────────────────────

    #[test]
    fn reservation_status_variants_all_serialize(status in reservation_status_strategy()) {
        let json = serde_json::to_string(&status).expect("serialize");
        let valid = json == r#""granted""# || json == r#""conflict""# || json == r#""unavailable""#;
        prop_assert!(valid, "unexpected serialization: {}", json);
    }

    // ── Mail message defaults are stable ────────────────────────────────

    #[test]
    fn mail_message_defaults_stable(_dummy in 0..10u8) {
        let msg: MailMessage = serde_json::from_str("{}").expect("deserialize empty");
        prop_assert!(msg.id.is_none());
        prop_assert!(msg.from.is_none());
        prop_assert!(msg.to.is_none());
        prop_assert!(msg.subject.is_empty());
        prop_assert!(msg.body.is_empty());
        prop_assert!(!msg.read);
        prop_assert!(msg.priority.is_none());
        prop_assert!(msg.extra.is_empty());
    }
}
