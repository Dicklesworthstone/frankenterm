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
