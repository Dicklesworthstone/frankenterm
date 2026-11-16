//! Property-based serde roundtrip coverage for the 6-enum recorder
//! metadata cluster + the schema-version constant extracted under
//! ft-j1qjt.3 (commit c62dda66).
//!
//! The cluster previously lived in `frankenterm-core/src/recording.rs`
//! with no proptest coverage of its own; the move to a leaf crate is
//! the right moment to add it. Each test asserts:
//!
//!   - JSON roundtrip is the identity for every enum variant
//!     (Debug+Clone+Copy+Serialize+Deserialize are all derived, so the
//!     property is "if you can spell the variant, the wire form
//!     decodes back to the same variant").
//!   - The `serde(rename_all = "snake_case")` tag survives the trip —
//!     guards against an accidental rename-attribute revert.
//!   - `RECORDER_EVENT_SCHEMA_VERSION_V1` is the documented stable
//!     string. If anyone bumps it (intentionally or otherwise), this
//!     assertion fires and the change has to be paired with a
//!     migration plan.

use frankenterm_core_replay_types::recorder_metadata::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEventSource,
    RecorderIngressKind, RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
};
use proptest::prelude::*;

fn arb_event_source() -> impl Strategy<Value = RecorderEventSource> {
    prop_oneof![
        Just(RecorderEventSource::WeztermMux),
        Just(RecorderEventSource::RobotMode),
        Just(RecorderEventSource::Mcp),
        Just(RecorderEventSource::WorkflowEngine),
        Just(RecorderEventSource::Beads),
        Just(RecorderEventSource::Rch),
        Just(RecorderEventSource::AgentMail),
        Just(RecorderEventSource::Git),
        Just(RecorderEventSource::OperatorAction),
        Just(RecorderEventSource::RecoveryFlow),
    ]
}

fn arb_text_encoding() -> impl Strategy<Value = RecorderTextEncoding> {
    Just(RecorderTextEncoding::Utf8)
}

fn arb_redaction_level() -> impl Strategy<Value = RecorderRedactionLevel> {
    prop_oneof![
        Just(RecorderRedactionLevel::None),
        Just(RecorderRedactionLevel::Partial),
        Just(RecorderRedactionLevel::Full),
    ]
}

fn arb_ingress_kind() -> impl Strategy<Value = RecorderIngressKind> {
    prop_oneof![
        Just(RecorderIngressKind::SendText),
        Just(RecorderIngressKind::Paste),
        Just(RecorderIngressKind::WorkflowAction),
    ]
}

fn arb_segment_kind() -> impl Strategy<Value = RecorderSegmentKind> {
    prop_oneof![
        Just(RecorderSegmentKind::Delta),
        Just(RecorderSegmentKind::Gap),
        Just(RecorderSegmentKind::Snapshot),
    ]
}

fn arb_control_marker_type() -> impl Strategy<Value = RecorderControlMarkerType> {
    prop_oneof![
        Just(RecorderControlMarkerType::PromptBoundary),
        Just(RecorderControlMarkerType::Resize),
        Just(RecorderControlMarkerType::PolicyDecision),
        Just(RecorderControlMarkerType::ApprovalCheckpoint),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn event_source_serde_roundtrip_is_identity(value in arb_event_source()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: RecorderEventSource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(value, back);
    }

    #[test]
    fn text_encoding_serde_roundtrip_is_identity(value in arb_text_encoding()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: RecorderTextEncoding = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(value, back);
    }

    #[test]
    fn redaction_level_serde_roundtrip_is_identity(value in arb_redaction_level()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: RecorderRedactionLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(value, back);
    }

    #[test]
    fn ingress_kind_serde_roundtrip_is_identity(value in arb_ingress_kind()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: RecorderIngressKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(value, back);
    }

    #[test]
    fn segment_kind_serde_roundtrip_is_identity(value in arb_segment_kind()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: RecorderSegmentKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(value, back);
    }

    #[test]
    fn control_marker_type_serde_roundtrip_is_identity(value in arb_control_marker_type()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: RecorderControlMarkerType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(value, back);
    }
}

/// Pin the snake_case rename attribute. If anyone reverts it (or
/// switches to PascalCase), the wire format would break for every
/// stored snapshot in `docs/asupersync-runtime-inventory.json` and
/// every persisted recorder event.
#[test]
fn recorder_metadata_enums_serialize_in_snake_case() {
    assert_eq!(
        serde_json::to_string(&RecorderEventSource::WeztermMux).unwrap(),
        r#""wezterm_mux""#,
    );
    assert_eq!(
        serde_json::to_string(&RecorderEventSource::WorkflowEngine).unwrap(),
        r#""workflow_engine""#,
    );
    assert_eq!(
        serde_json::to_string(&RecorderEventSource::Mcp).unwrap(),
        r#""mcp""#,
    );
    assert_eq!(
        serde_json::to_string(&RecorderEventSource::Beads).unwrap(),
        r#""beads""#,
    );
    assert_eq!(
        serde_json::to_string(&RecorderEventSource::Rch).unwrap(),
        r#""rch""#,
    );
    assert_eq!(
        serde_json::to_string(&RecorderEventSource::AgentMail).unwrap(),
        r#""agent_mail""#,
    );
    assert_eq!(
        serde_json::to_string(&RecorderEventSource::Git).unwrap(),
        r#""git""#,
    );
    assert_eq!(
        serde_json::to_string(&RecorderRedactionLevel::Partial).unwrap(),
        r#""partial""#,
    );
    assert_eq!(
        serde_json::to_string(&RecorderIngressKind::SendText).unwrap(),
        r#""send_text""#,
    );
    assert_eq!(
        serde_json::to_string(&RecorderSegmentKind::Snapshot).unwrap(),
        r#""snapshot""#,
    );
    assert_eq!(
        serde_json::to_string(&RecorderControlMarkerType::PromptBoundary).unwrap(),
        r#""prompt_boundary""#,
    );
}

/// Pin the schema-version string. Any bump to this constant is a
/// breaking change for the recorder event contract and must be paired
/// with a migration story; this assertion forces that conversation.
#[test]
fn recorder_event_schema_version_v1_is_stable() {
    assert_eq!(RECORDER_EVENT_SCHEMA_VERSION_V1, "ft.recorder.event.v1");
}
