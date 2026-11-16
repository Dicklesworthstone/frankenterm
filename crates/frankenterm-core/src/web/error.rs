//! Structured web error surface for Wave 4B migration.

use crate::VERSION;
use crate::web_framework::{Response, StatusCode, json_response_with_status};
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct ApiResponse<T> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    version: &'static str,
}

impl<T: Serialize> ApiResponse<T> {
    pub(super) fn success(data: T) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            error_code: None,
            version: VERSION,
        }
    }
}

impl ApiResponse<()> {
    pub(super) fn error(code: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(message.into()),
            error_code: Some(code.to_string()),
            version: VERSION,
        }
    }
}

pub(super) fn json_ok<T: Serialize>(data: T) -> Response {
    let resp = ApiResponse::success(data);
    json_response_with_status(StatusCode::OK, &resp)
}

pub(super) fn json_err(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let resp = ApiResponse::<()>::error(code, message);
    json_response_with_status(status, &resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_framework::{FrameworkResponseBody, FrameworkStatusCode};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct TestPayload {
        id: u64,
        label: String,
    }

    fn decode_json_body<T: for<'de> Deserialize<'de>>(response: &Response) -> T {
        match response.body_ref() {
            FrameworkResponseBody::Bytes(bytes) => {
                serde_json::from_slice(bytes).expect("response body should be valid json")
            }
            other => panic!("expected bytes body, got {other:?}"),
        }
    }

    #[test]
    fn api_response_success_sets_ok_data_and_version() {
        let payload = TestPayload {
            id: 7,
            label: "pane".to_string(),
        };

        let value = serde_json::to_value(ApiResponse::success(payload)).expect("serialize");

        assert_eq!(value["ok"], serde_json::Value::Bool(true));
        assert_eq!(value["data"]["id"], serde_json::json!(7));
        assert_eq!(value["data"]["label"], serde_json::json!("pane"));
        assert_eq!(value["version"], serde_json::json!(VERSION));
    }

    #[test]
    fn api_response_success_omits_error_fields() {
        let value = serde_json::to_value(ApiResponse::success(123u32)).expect("serialize");

        assert!(value.get("error").is_none());
        assert!(value.get("error_code").is_none());
    }

    #[test]
    fn api_response_error_sets_error_code_message_and_version() {
        let value = serde_json::to_value(ApiResponse::error("bad_request", "bad input"))
            .expect("serialize");

        assert_eq!(value["ok"], serde_json::Value::Bool(false));
        assert_eq!(value["error"], serde_json::json!("bad input"));
        assert_eq!(value["error_code"], serde_json::json!("bad_request"));
        assert_eq!(value["version"], serde_json::json!(VERSION));
    }

    #[test]
    fn api_response_error_omits_data_field() {
        let value = serde_json::to_value(ApiResponse::error("boom", "failed")).expect("serialize");

        assert!(value.get("data").is_none());
    }

    #[test]
    fn json_ok_wraps_payload_in_success_envelope() {
        let payload = TestPayload {
            id: 42,
            label: "worker".to_string(),
        };

        let response = json_ok(payload);

        assert_eq!(response.status(), FrameworkStatusCode::OK);
        assert_eq!(response.headers()[0].0.to_ascii_lowercase(), "content-type");
        assert_eq!(response.headers()[0].1.as_slice(), b"application/json");

        let body: serde_json::Value = decode_json_body(&response);
        assert_eq!(body["ok"], serde_json::Value::Bool(true));
        assert_eq!(body["data"]["id"], serde_json::json!(42));
        assert_eq!(body["data"]["label"], serde_json::json!("worker"));
        assert_eq!(body["version"], serde_json::json!(VERSION));
    }

    #[test]
    fn json_err_wraps_message_in_error_envelope_with_status() {
        let response = json_err(
            FrameworkStatusCode::BAD_REQUEST,
            "invalid_args",
            "format must be json or toon",
        );

        assert_eq!(response.status(), FrameworkStatusCode::BAD_REQUEST);

        let body: serde_json::Value = decode_json_body(&response);
        assert_eq!(body["ok"], serde_json::Value::Bool(false));
        assert_eq!(body["error_code"], serde_json::json!("invalid_args"));
        assert_eq!(
            body["error"],
            serde_json::json!("format must be json or toon")
        );
        assert_eq!(body["version"], serde_json::json!(VERSION));
        assert!(body.get("data").is_none());
    }
}
