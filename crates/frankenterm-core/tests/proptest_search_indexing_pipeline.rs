//! Property-based tests for the search indexing pipeline types.
//!
//! Covers serde roundtrip, Clone, and PartialEq for `PaneWatermark`,
//! `PipelineState`, and `PipelineSkipReason`.

use frankenterm_core::search::indexing_pipeline::{
    PaneWatermark, PipelineSkipReason, PipelineState,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_pane_watermark() -> impl Strategy<Value = PaneWatermark> {
    (
        0_u64..10_000,
        0_i64..2_000_000_000_000,
        0_u64..1_000_000,
        proptest::option::of("[a-z0-9-]{5,20}"),
    )
        .prop_map(
            |(pane_id, last_indexed_at_ms, total_docs_indexed, session_id)| PaneWatermark {
                pane_id,
                last_indexed_at_ms,
                total_docs_indexed,
                session_id,
            },
        )
}

fn arb_pipeline_state() -> impl Strategy<Value = PipelineState> {
    prop_oneof![
        Just(PipelineState::Running),
        Just(PipelineState::Paused),
        Just(PipelineState::Stopped),
    ]
}

fn arb_pipeline_skip_reason() -> impl Strategy<Value = PipelineSkipReason> {
    prop_oneof![
        Just(PipelineSkipReason::Paused),
        Just(PipelineSkipReason::ResizeStorm),
        Just(PipelineSkipReason::Stopped),
        Just(PipelineSkipReason::NoPanes),
    ]
}

// =========================================================================
// Serde roundtrip tests
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// PaneWatermark serde roundtrip preserves all fields.
    #[test]
    fn prop_watermark_serde_roundtrip(wm in arb_pane_watermark()) {
        let json = serde_json::to_string(&wm).unwrap();
        let back: PaneWatermark = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back, &wm);
    }

    /// PipelineState serde roundtrip preserves variant.
    #[test]
    fn prop_state_serde_roundtrip(state in arb_pipeline_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: PipelineState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, state);
    }

    /// PipelineSkipReason serde roundtrip preserves variant.
    #[test]
    fn prop_skip_reason_serde_roundtrip(reason in arb_pipeline_skip_reason()) {
        let json = serde_json::to_string(&reason).unwrap();
        let back: PipelineSkipReason = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, reason);
    }
}

// =========================================================================
// Clone + PartialEq
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// PaneWatermark Clone preserves equality.
    #[test]
    fn prop_watermark_clone_eq(wm in arb_pane_watermark()) {
        let cloned = wm.clone();
        prop_assert_eq!(&cloned, &wm);
    }

    /// PipelineState Copy preserves equality.
    #[test]
    fn prop_state_copy_eq(state in arb_pipeline_state()) {
        let copied = state;
        prop_assert_eq!(copied, state);
    }

    /// PipelineSkipReason Copy preserves equality.
    #[test]
    fn prop_skip_reason_copy_eq(reason in arb_pipeline_skip_reason()) {
        let copied = reason;
        prop_assert_eq!(copied, reason);
    }
}

// =========================================================================
// JSON shape
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// PaneWatermark JSON is a valid object.
    #[test]
    fn prop_watermark_json_is_object(wm in arb_pane_watermark()) {
        let json = serde_json::to_string(&wm).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// PipelineState JSON is a valid string.
    #[test]
    fn prop_state_json_is_string(state in arb_pipeline_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_string());
    }

    /// PipelineSkipReason JSON is a valid string.
    #[test]
    fn prop_skip_reason_json_is_string(reason in arb_pipeline_skip_reason()) {
        let json = serde_json::to_string(&reason).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_string());
    }

    /// PaneWatermark without session_id omits the field.
    #[test]
    fn prop_watermark_skip_serializing_none(wm_base in arb_pane_watermark()) {
        let wm = PaneWatermark {
            session_id: None,
            ..wm_base
        };
        let json = serde_json::to_string(&wm).unwrap();
        let check = !json.contains("session_id");
        prop_assert!(check, "session_id should be omitted when None");
    }
}

// =========================================================================
// Known-value unit tests
// =========================================================================

#[test]
fn pipeline_state_running_wire_format() {
    let json = serde_json::to_string(&PipelineState::Running).unwrap();
    assert_eq!(json, r#""running""#);
}

#[test]
fn pipeline_state_paused_wire_format() {
    let json = serde_json::to_string(&PipelineState::Paused).unwrap();
    assert_eq!(json, r#""paused""#);
}

#[test]
fn pipeline_state_stopped_wire_format() {
    let json = serde_json::to_string(&PipelineState::Stopped).unwrap();
    assert_eq!(json, r#""stopped""#);
}

#[test]
fn watermark_with_session_id_roundtrip() {
    let wm = PaneWatermark {
        pane_id: 42,
        last_indexed_at_ms: 1700000000000,
        total_docs_indexed: 150,
        session_id: Some("sess-abc".to_string()),
    };
    let json = serde_json::to_string(&wm).unwrap();
    assert!(json.contains("sess-abc"));
    let back: PaneWatermark = serde_json::from_str(&json).unwrap();
    assert_eq!(back, wm);
}

#[test]
fn watermark_without_session_id_roundtrip() {
    let wm = PaneWatermark {
        pane_id: 0,
        last_indexed_at_ms: 0,
        total_docs_indexed: 0,
        session_id: None,
    };
    let json = serde_json::to_string(&wm).unwrap();
    assert!(!json.contains("session_id"));
    let back: PaneWatermark = serde_json::from_str(&json).unwrap();
    assert_eq!(back, wm);
}

#[test]
fn all_skip_reasons_distinct() {
    let reasons = [
        PipelineSkipReason::Paused,
        PipelineSkipReason::ResizeStorm,
        PipelineSkipReason::Stopped,
        PipelineSkipReason::NoPanes,
    ];
    for i in 0..reasons.len() {
        for j in (i + 1)..reasons.len() {
            assert_ne!(reasons[i], reasons[j], "skip reasons should be distinct");
        }
    }
}
