//! Property-based tests for the search daemon wire protocol.
//!
//! Covers serde roundtrip, JSON wire-format stability, and codec boundary
//! validation for `DaemonRequest`, `DaemonResponse`, `EmbedRequest`, and
//! `EmbedResponse`.
//!
//! Requires the `semantic-search` feature.

#![cfg(feature = "semantic-search")]

use frankenterm_core::search::daemon::{
    DaemonRequest, DaemonResponse, EmbedRequest, EmbedResponse,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_embed_request() -> impl Strategy<Value = EmbedRequest> {
    (
        0_u64..100_000,
        "[a-zA-Z0-9 ]{1,100}",
        proptest::option::of("[a-z0-9-]{3,20}"),
    )
        .prop_map(|(id, text, model)| EmbedRequest { id, text, model })
}

fn arb_embed_response() -> impl Strategy<Value = EmbedResponse> {
    (
        0_u64..100_000,
        proptest::collection::vec(-1.0_f32..1.0, 0..16),
        "[a-z0-9-]{3,20}",
        0_u64..60_000,
    )
        .prop_map(|(id, vector, model, elapsed_ms)| EmbedResponse {
            id,
            vector,
            model,
            elapsed_ms,
        })
}

fn arb_daemon_request() -> impl Strategy<Value = DaemonRequest> {
    prop_oneof![
        arb_embed_request().prop_map(DaemonRequest::Embed),
        Just(DaemonRequest::Shutdown),
        Just(DaemonRequest::Ping),
    ]
}

fn arb_daemon_response() -> impl Strategy<Value = DaemonResponse> {
    prop_oneof![
        arb_embed_response().prop_map(DaemonResponse::Embed),
        Just(DaemonResponse::Pong),
        "[a-zA-Z ]{1,50}".prop_map(DaemonResponse::Error),
    ]
}

// =========================================================================
// Serde roundtrip (JSON string)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// DaemonRequest serde roundtrip via JSON string.
    #[test]
    fn prop_request_serde_json_roundtrip(req in arb_daemon_request()) {
        let json = serde_json::to_string(&req).unwrap();
        let back: DaemonRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back, &req);
    }

    /// DaemonResponse serde roundtrip via JSON string.
    #[test]
    fn prop_response_serde_json_roundtrip(resp in arb_daemon_response()) {
        let json = serde_json::to_string(&resp).unwrap();
        let back: DaemonResponse = serde_json::from_str(&json).unwrap();
        // f32 vectors may lose precision through JSON — compare with tolerance
        match (&back, &resp) {
            (DaemonResponse::Embed(a), DaemonResponse::Embed(b)) => {
                prop_assert_eq!(a.id, b.id);
                prop_assert_eq!(&a.model, &b.model);
                prop_assert_eq!(a.elapsed_ms, b.elapsed_ms);
                prop_assert_eq!(a.vector.len(), b.vector.len());
                for (va, vb) in a.vector.iter().zip(b.vector.iter()) {
                    prop_assert!((va - vb).abs() < 1e-6,
                        "vector element mismatch: {} vs {}", va, vb);
                }
            }
            _ => prop_assert_eq!(&back, &resp),
        }
    }

    /// EmbedRequest serde roundtrip.
    #[test]
    fn prop_embed_request_serde_roundtrip(req in arb_embed_request()) {
        let json = serde_json::to_string(&req).unwrap();
        let back: EmbedRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back, &req);
    }

    /// EmbedResponse serde roundtrip preserves id, model, elapsed_ms.
    #[test]
    fn prop_embed_response_serde_roundtrip(resp in arb_embed_response()) {
        let json = serde_json::to_string(&resp).unwrap();
        let back: EmbedResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, resp.id);
        prop_assert_eq!(&back.model, &resp.model);
        prop_assert_eq!(back.elapsed_ms, resp.elapsed_ms);
        prop_assert_eq!(back.vector.len(), resp.vector.len());
    }
}

// =========================================================================
// Wire codec roundtrip (to_json_bytes / from_json_bytes)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// DaemonRequest wire codec roundtrip.
    #[test]
    fn prop_request_wire_roundtrip(req in arb_daemon_request()) {
        let bytes = req.to_json_bytes().unwrap();
        let back = DaemonRequest::from_json_bytes(&bytes).unwrap();
        prop_assert_eq!(&back, &req);
    }

    /// DaemonResponse wire codec roundtrip.
    #[test]
    fn prop_response_wire_roundtrip(resp in arb_daemon_response()) {
        let bytes = resp.to_json_bytes().unwrap();
        let back = DaemonResponse::from_json_bytes(&bytes).unwrap();
        // Again handle f32 precision
        match (&back, &resp) {
            (DaemonResponse::Embed(a), DaemonResponse::Embed(b)) => {
                prop_assert_eq!(a.id, b.id);
                prop_assert_eq!(&a.model, &b.model);
                prop_assert_eq!(a.vector.len(), b.vector.len());
            }
            _ => prop_assert_eq!(&back, &resp),
        }
    }
}

// =========================================================================
// Clone + Debug
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// DaemonRequest Clone preserves equality.
    #[test]
    fn prop_request_clone(req in arb_daemon_request()) {
        let cloned = req.clone();
        prop_assert_eq!(&cloned, &req);
    }

    /// DaemonResponse Clone preserves equality.
    #[test]
    fn prop_response_clone(resp in arb_daemon_response()) {
        let cloned = resp.clone();
        // f32 NaN != NaN so use Debug comparison
        let dbg_cloned = format!("{:?}", cloned);
        let dbg_resp = format!("{:?}", resp);
        prop_assert_eq!(dbg_cloned, dbg_resp);
    }

    /// DaemonRequest Debug is non-empty.
    #[test]
    fn prop_request_debug_nonempty(req in arb_daemon_request()) {
        let dbg = format!("{:?}", req);
        prop_assert!(!dbg.is_empty());
    }

    /// DaemonResponse Debug is non-empty.
    #[test]
    fn prop_response_debug_nonempty(resp in arb_daemon_response()) {
        let dbg = format!("{:?}", resp);
        prop_assert!(!dbg.is_empty());
    }
}

// =========================================================================
// Wire format shape
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// DaemonRequest JSON is always a valid JSON object.
    #[test]
    fn prop_request_json_is_object(req in arb_daemon_request()) {
        let json = serde_json::to_string(&req).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// DaemonResponse JSON is always a valid JSON object.
    #[test]
    fn prop_response_json_is_object(resp in arb_daemon_response()) {
        let json = serde_json::to_string(&resp).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// DaemonRequest JSON contains "type" discriminator.
    #[test]
    fn prop_request_json_has_type_tag(req in arb_daemon_request()) {
        let json = serde_json::to_string(&req).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("type").is_some(),
            "request JSON should contain a 'type' field");
    }

    /// DaemonResponse JSON contains "type" discriminator.
    #[test]
    fn prop_response_json_has_type_tag(resp in arb_daemon_response()) {
        let json = serde_json::to_string(&resp).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("type").is_some(),
            "response JSON should contain a 'type' field");
    }

    /// Request type tag exactly matches the enum variant.
    #[test]
    fn prop_request_type_tag_matches_variant(req in arb_daemon_request()) {
        let json = serde_json::to_string(&req).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let expected = match req {
            DaemonRequest::Embed(_) => "embed",
            DaemonRequest::Shutdown => "shutdown",
            DaemonRequest::Ping => "ping",
        };
        prop_assert_eq!(&value["type"], expected);
    }

    /// Response type tag exactly matches the enum variant.
    #[test]
    fn prop_response_type_tag_matches_variant(resp in arb_daemon_response()) {
        let json = serde_json::to_string(&resp).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let expected = match resp {
            DaemonResponse::Embed(_) => "embed",
            DaemonResponse::Pong => "pong",
            DaemonResponse::Error(_) => "error",
        };
        prop_assert_eq!(&value["type"], expected);
    }
}

// =========================================================================
// Unit tests — known wire format
// =========================================================================

#[test]
fn ping_wire_format() {
    let bytes = DaemonRequest::Ping.to_json_bytes().unwrap();
    let json = String::from_utf8(bytes).unwrap();
    assert_eq!(json, r#"{"type":"ping"}"#);
}

#[test]
fn shutdown_wire_format() {
    let bytes = DaemonRequest::Shutdown.to_json_bytes().unwrap();
    let json = String::from_utf8(bytes).unwrap();
    assert_eq!(json, r#"{"type":"shutdown"}"#);
}

#[test]
fn pong_wire_format() {
    let bytes = DaemonResponse::Pong.to_json_bytes().unwrap();
    let json = String::from_utf8(bytes).unwrap();
    assert_eq!(json, r#"{"type":"pong"}"#);
}

#[test]
fn error_response_wire_format() {
    let resp = DaemonResponse::Error("bad input".to_string());
    let bytes = resp.to_json_bytes().unwrap();
    let json = String::from_utf8(bytes).unwrap();
    assert!(json.contains(r#""type":"error""#));
    assert!(json.contains("bad input"));
}

#[test]
fn embed_request_wire_contains_type_and_data() {
    let req = DaemonRequest::Embed(EmbedRequest {
        id: 1,
        text: "hello".to_string(),
        model: Some("test".to_string()),
    });
    let bytes = req.to_json_bytes().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["type"], "embed");
    assert!(value["data"].is_object());
    assert_eq!(value["data"]["id"], 1);
    assert_eq!(value["data"]["text"], "hello");
}

#[test]
fn embed_response_preserves_empty_vector() {
    let resp = DaemonResponse::Embed(EmbedResponse {
        id: 0,
        vector: vec![],
        model: "empty".to_string(),
        elapsed_ms: 0,
    });
    let bytes = resp.to_json_bytes().unwrap();
    let back = DaemonResponse::from_json_bytes(&bytes).unwrap();
    if let DaemonResponse::Embed(e) = back {
        assert!(e.vector.is_empty());
    } else {
        panic!("Expected Embed response");
    }
}

#[test]
fn embed_request_none_model_omits_field_or_null() {
    let req = EmbedRequest {
        id: 5,
        text: "test".to_string(),
        model: None,
    };
    let json = serde_json::to_string(&req).unwrap();
    let back: EmbedRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(back.model, None);
}

#[test]
fn request_wire_rejects_unknown_type() {
    let bad = br#"{"type":"definitely-unknown"}"#;
    assert!(DaemonRequest::from_json_bytes(bad).is_err());
}

#[test]
fn response_wire_rejects_malformed_json() {
    let bad = br#"{"type":"pong""#;
    assert!(DaemonResponse::from_json_bytes(bad).is_err());
}
