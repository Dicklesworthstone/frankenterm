//! Property-based tests for the `ipc` module.
//!
//! Covers `IpcRequest` tagged enum serde roundtrips (all 7 variants),
//! `IpcResponse` serde roundtrips, constructor invariants, and
//! skip_serializing_if behavior for optional fields.

#![cfg(unix)]

use frankenterm_core::ipc::{IpcRequest, IpcResponse};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_ipc_request() -> impl Strategy<Value = IpcRequest> {
    prop_oneof![
        (0_u64..100_000, "[A-Z_]{3,15}", "[a-zA-Z0-9+/=]{5,40}").prop_map(
            |(pane_id, name, value)| IpcRequest::UserVar {
                pane_id,
                name,
                value,
            }
        ),
        Just(IpcRequest::Ping),
        Just(IpcRequest::Status),
        (0_u64..100_000).prop_map(|pane_id| IpcRequest::PaneState { pane_id }),
        (
            0_u64..100_000,
            0_u32..100,
            proptest::option::of(0_u64..60_000)
        )
            .prop_map(|(pane_id, priority, ttl_ms)| IpcRequest::SetPanePriority {
                pane_id,
                priority,
                ttl_ms,
            }),
        (0_u64..100_000).prop_map(|pane_id| IpcRequest::ClearPanePriority { pane_id }),
        proptest::collection::vec("[a-z_]{2,10}", 0..5).prop_map(|args| IpcRequest::Rpc { args }),
    ]
}

fn arb_ipc_response() -> impl Strategy<Value = IpcResponse> {
    (
        any::<bool>(),
        proptest::option::of("[a-z ]{5,30}"),
        proptest::option::of("[a-z._]{3,15}"),
        proptest::option::of("[A-Za-z ]{5,30}"),
        0_u64..10_000,
        "[0-9.]{3,8}",
        1_000_000_000_000_u64..2_000_000_000_000,
    )
        .prop_map(
            |(ok, error, error_code, hint, elapsed_ms, version, now)| IpcResponse {
                ok,
                error,
                error_code,
                hint,
                data: None,
                elapsed_ms,
                version,
                now,
            },
        )
}

// =========================================================================
// IpcRequest — tagged enum serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// IpcRequest serde roundtrip preserves variant and fields.
    #[test]
    fn prop_request_serde(request in arb_ipc_request()) {
        let json = serde_json::to_string(&request).unwrap();
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(
            format!("{:?}", back),
            format!("{:?}", request),
        );
    }

    /// IpcRequest JSON always contains the "type" tag.
    #[test]
    fn prop_request_has_type_tag(request in arb_ipc_request()) {
        let json = serde_json::to_string(&request).unwrap();
        prop_assert!(json.contains("\"type\""), "JSON should contain type tag: {}", json);
    }

    /// IpcRequest tag is snake_case.
    #[test]
    fn prop_request_tag_snake_case(request in arb_ipc_request()) {
        let json = serde_json::to_string(&request).unwrap();
        let expected_tag = match &request {
            IpcRequest::UserVar { .. } => "user_var",
            IpcRequest::Ping => "ping",
            IpcRequest::Status => "status",
            IpcRequest::PaneState { .. } => "pane_state",
            IpcRequest::SetPanePriority { .. } => "set_pane_priority",
            IpcRequest::ClearPanePriority { .. } => "clear_pane_priority",
            IpcRequest::Rpc { .. } => "rpc",
        };
        prop_assert!(
            json.contains(expected_tag),
            "expected tag '{}' in JSON: {}",
            expected_tag, json
        );
    }

    /// IpcRequest serde is deterministic.
    #[test]
    fn prop_request_deterministic(request in arb_ipc_request()) {
        let j1 = serde_json::to_string(&request).unwrap();
        let j2 = serde_json::to_string(&request).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// UserVar requests contain pane_id, name, and value fields.
    #[test]
    fn prop_user_var_fields(
        pane_id in 0_u64..100_000,
        name in "[A-Z_]{3,10}",
        value in "[a-zA-Z0-9]{5,20}",
    ) {
        let request = IpcRequest::UserVar { pane_id, name: name.clone(), value: value.clone() };
        let json = serde_json::to_string(&request).unwrap();
        let has_pane = json.contains(&format!("\"pane_id\":{pane_id}"));
        prop_assert!(has_pane, "expected pane_id in JSON");
        let has_name = json.contains(&format!("\"name\":\"{name}\""));
        prop_assert!(has_name, "expected name in JSON");
        let has_value = json.contains(&format!("\"value\":\"{value}\""));
        prop_assert!(has_value, "expected value in JSON");
    }

    /// SetPanePriority with None ttl_ms roundtrips correctly.
    #[test]
    fn prop_set_priority_no_ttl(pane_id in 0_u64..1000, priority in 0_u32..100) {
        let request = IpcRequest::SetPanePriority { pane_id, priority, ttl_ms: None };
        let json = serde_json::to_string(&request).unwrap();
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        match back {
            IpcRequest::SetPanePriority { ttl_ms, .. } => {
                prop_assert_eq!(ttl_ms, None);
            }
            _ => prop_assert!(false, "expected SetPanePriority"),
        }
    }

    /// Rpc with empty args roundtrips.
    #[test]
    fn prop_rpc_empty_args(_dummy in 0..1_u8) {
        let request = IpcRequest::Rpc { args: vec![] };
        let json = serde_json::to_string(&request).unwrap();
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        match back {
            IpcRequest::Rpc { args } => prop_assert!(args.is_empty()),
            _ => prop_assert!(false, "expected Rpc"),
        }
    }
}

// =========================================================================
// IpcResponse — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// IpcResponse serde roundtrip preserves all fields.
    #[test]
    fn prop_response_serde(response in arb_ipc_response()) {
        let json = serde_json::to_string(&response).unwrap();
        let back: IpcResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.ok, response.ok);
        prop_assert_eq!(&back.error, &response.error);
        prop_assert_eq!(&back.error_code, &response.error_code);
        prop_assert_eq!(&back.hint, &response.hint);
        prop_assert_eq!(back.elapsed_ms, response.elapsed_ms);
        prop_assert_eq!(&back.version, &response.version);
        prop_assert_eq!(back.now, response.now);
    }

    /// IpcResponse serde is deterministic.
    #[test]
    fn prop_response_deterministic(response in arb_ipc_response()) {
        let j1 = serde_json::to_string(&response).unwrap();
        let j2 = serde_json::to_string(&response).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// IpcResponse with None optional fields omits them from JSON.
    #[test]
    fn prop_response_skip_none_fields(
        elapsed_ms in 0_u64..10_000,
        now in 1_000_000_000_000_u64..2_000_000_000_000,
    ) {
        let response = IpcResponse {
            ok: true,
            error: None,
            error_code: None,
            hint: None,
            data: None,
            elapsed_ms,
            version: "0.1.0".to_string(),
            now,
        };
        let json = serde_json::to_string(&response).unwrap();
        prop_assert!(!json.contains("\"error\""), "None error should be omitted: {}", json);
        prop_assert!(!json.contains("\"error_code\""), "None error_code should be omitted: {}", json);
        prop_assert!(!json.contains("\"hint\""), "None hint should be omitted: {}", json);
        prop_assert!(!json.contains("\"data\""), "None data should be omitted: {}", json);
    }

    /// IpcResponse with Some optional fields includes them in JSON.
    #[test]
    fn prop_response_includes_some_fields(
        error in "[a-z ]{5,20}",
        code in "[a-z._]{3,10}",
        hint in "[A-Za-z ]{5,20}",
    ) {
        let response = IpcResponse {
            ok: false,
            error: Some(error.clone()),
            error_code: Some(code.clone()),
            hint: Some(hint.clone()),
            data: None,
            elapsed_ms: 42,
            version: "0.1.0".to_string(),
            now: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&response).unwrap();
        prop_assert!(json.contains("\"error\""));
        prop_assert!(json.contains("\"error_code\""));
        prop_assert!(json.contains("\"hint\""));
    }
}

// =========================================================================
// IpcResponse — constructor invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// IpcResponse::ok() always produces ok=true with no error.
    #[test]
    fn prop_response_ok_constructor(_dummy in 0..1_u8) {
        let response = IpcResponse::ok();
        prop_assert!(response.ok);
        prop_assert!(response.error.is_none());
        prop_assert!(response.error_code.is_none());
        prop_assert!(response.hint.is_none());
        prop_assert!(response.data.is_none());
        prop_assert!(!response.version.is_empty());
    }

    /// IpcResponse::ok_with_data() sets data and ok=true.
    #[test]
    fn prop_response_ok_with_data(val in any::<i64>()) {
        let data = serde_json::json!(val);
        let response = IpcResponse::ok_with_data(data.clone());
        prop_assert!(response.ok);
        prop_assert!(response.error.is_none());
        prop_assert_eq!(response.data, Some(data));
    }

    /// IpcResponse::error() sets ok=false with error message.
    #[test]
    fn prop_response_error_constructor(msg in "[a-z ]{5,30}") {
        let response = IpcResponse::error(&msg);
        prop_assert!(!response.ok);
        prop_assert_eq!(response.error, Some(msg));
        prop_assert!(response.error_code.is_none());
    }

    /// IpcResponse::error_with_code() sets code, message, and hint.
    #[test]
    fn prop_response_error_with_code(
        code in "[a-z._]{3,10}",
        msg in "[a-z ]{5,20}",
        hint in "[A-Za-z ]{5,20}",
    ) {
        let response = IpcResponse::error_with_code(&code, &msg, Some(hint.clone()));
        prop_assert!(!response.ok);
        prop_assert_eq!(response.error_code, Some(code));
        prop_assert_eq!(response.error, Some(msg));
        prop_assert_eq!(response.hint, Some(hint));
    }

    /// IpcResponse::error_with_code() without hint sets hint=None.
    #[test]
    fn prop_response_error_no_hint(
        code in "[a-z._]{3,10}",
        msg in "[a-z ]{5,20}",
    ) {
        let response = IpcResponse::error_with_code(&code, &msg, None);
        prop_assert!(response.hint.is_none());
    }

    /// All constructor responses have non-empty version.
    #[test]
    fn prop_all_constructors_have_version(_dummy in 0..1_u8) {
        let responses = [
            IpcResponse::ok(),
            IpcResponse::ok_with_data(serde_json::json!(null)),
            IpcResponse::error("test"),
            IpcResponse::error_with_code("c", "m", None),
        ];
        for r in &responses {
            prop_assert!(!r.version.is_empty());
        }
    }

    /// All constructor responses have non-zero now timestamp.
    #[test]
    fn prop_all_constructors_have_timestamp(_dummy in 0..1_u8) {
        let responses = [
            IpcResponse::ok(),
            IpcResponse::error("test"),
        ];
        for r in &responses {
            prop_assert!(r.now > 0);
        }
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn request_ping_roundtrip() {
    let json = serde_json::to_string(&IpcRequest::Ping).unwrap();
    assert!(json.contains("\"type\":\"ping\""));
    let back: IpcRequest = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, IpcRequest::Ping));
}

#[test]
fn request_status_roundtrip() {
    let json = serde_json::to_string(&IpcRequest::Status).unwrap();
    assert!(json.contains("\"type\":\"status\""));
    let back: IpcRequest = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, IpcRequest::Status));
}

#[test]
fn response_ok_serde() {
    let response = IpcResponse::ok();
    let json = serde_json::to_string(&response).unwrap();
    let back: IpcResponse = serde_json::from_str(&json).unwrap();
    assert!(back.ok);
    assert!(back.error.is_none());
}

#[test]
fn response_error_serde() {
    let response = IpcResponse::error("something broke");
    let json = serde_json::to_string(&response).unwrap();
    let back: IpcResponse = serde_json::from_str(&json).unwrap();
    assert!(!back.ok);
    assert_eq!(back.error, Some("something broke".to_string()));
}
