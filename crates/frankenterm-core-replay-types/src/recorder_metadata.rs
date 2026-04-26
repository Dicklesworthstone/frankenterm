//! Recorder event metadata enums + the schema-version constant.
//!
//! Extracted from `frankenterm-core/src/recording.rs` under ft-j1qjt.3
//! (a.k.a. ft-j1qjt.1.1) so the replay-cluster types core depends on can
//! eventually move out of core entirely. Today these enums are imported
//! by ~12 files in core (recording, replay_capture, ingest, recorder_*,
//! workflows/runner, etc.) and by every replay_*.rs module — moving them
//! here means they're reachable from both directions without forcing a
//! cargo cycle.
//!
//! All seven items are pure data — `Copy` enums and a single `&'static str`
//! constant — with zero crate-internal dependencies. They are the
//! definition of "leaf-clean".

use serde::{Deserialize, Serialize};

/// Schema version string for the v1 recorder event contract.
pub const RECORDER_EVENT_SCHEMA_VERSION_V1: &str = "ft.recorder.event.v1";

/// Source subsystem that produced the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderEventSource {
    WeztermMux,
    RobotMode,
    WorkflowEngine,
    OperatorAction,
    RecoveryFlow,
}

/// Text encoding used for ingress/egress payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderTextEncoding {
    Utf8,
}

/// Redaction level applied to captured text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderRedactionLevel {
    None,
    Partial,
    Full,
}

/// How ingress text was injected into the mux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderIngressKind {
    SendText,
    Paste,
    WorkflowAction,
}

/// Kind of egress output segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderSegmentKind {
    Delta,
    Gap,
    Snapshot,
}

/// Type of control marker event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderControlMarkerType {
    PromptBoundary,
    Resize,
    PolicyDecision,
    ApprovalCheckpoint,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn arb_event_source() -> impl Strategy<Value = RecorderEventSource> {
        prop_oneof![
            Just(RecorderEventSource::WeztermMux),
            Just(RecorderEventSource::RobotMode),
            Just(RecorderEventSource::WorkflowEngine),
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
        #[test]
        fn recorder_metadata_enums_round_trip_through_json(
            source in arb_event_source(),
            encoding in arb_text_encoding(),
            redaction in arb_redaction_level(),
            ingress in arb_ingress_kind(),
            segment in arb_segment_kind(),
            marker in arb_control_marker_type(),
        ) {
            let source_json = serde_json::to_string(&source).unwrap();
            let encoding_json = serde_json::to_string(&encoding).unwrap();
            let redaction_json = serde_json::to_string(&redaction).unwrap();
            let ingress_json = serde_json::to_string(&ingress).unwrap();
            let segment_json = serde_json::to_string(&segment).unwrap();
            let marker_json = serde_json::to_string(&marker).unwrap();

            prop_assert_eq!(serde_json::from_str::<RecorderEventSource>(&source_json).unwrap(), source);
            prop_assert_eq!(serde_json::from_str::<RecorderTextEncoding>(&encoding_json).unwrap(), encoding);
            prop_assert_eq!(serde_json::from_str::<RecorderRedactionLevel>(&redaction_json).unwrap(), redaction);
            prop_assert_eq!(serde_json::from_str::<RecorderIngressKind>(&ingress_json).unwrap(), ingress);
            prop_assert_eq!(serde_json::from_str::<RecorderSegmentKind>(&segment_json).unwrap(), segment);
            prop_assert_eq!(serde_json::from_str::<RecorderControlMarkerType>(&marker_json).unwrap(), marker);
        }
    }

    #[test]
    fn recorder_metadata_wire_names_are_stable_snake_case() {
        assert_eq!(
            serde_json::to_string(&RecorderEventSource::WeztermMux).unwrap(),
            "\"wezterm_mux\""
        );
        assert_eq!(
            serde_json::to_string(&RecorderTextEncoding::Utf8).unwrap(),
            "\"utf8\""
        );
        assert_eq!(
            serde_json::to_string(&RecorderRedactionLevel::Partial).unwrap(),
            "\"partial\""
        );
        assert_eq!(
            serde_json::to_string(&RecorderIngressKind::WorkflowAction).unwrap(),
            "\"workflow_action\""
        );
        assert_eq!(
            serde_json::to_string(&RecorderSegmentKind::Snapshot).unwrap(),
            "\"snapshot\""
        );
        assert_eq!(
            serde_json::to_string(&RecorderControlMarkerType::ApprovalCheckpoint).unwrap(),
            "\"approval_checkpoint\""
        );
    }
}
