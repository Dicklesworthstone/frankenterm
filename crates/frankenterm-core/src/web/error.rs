//! Structured web error surface for Wave 4B migration.

#[allow(clippy::wildcard_imports)]
use super::*;
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
    Response::json(&resp).unwrap_or_else(|_| Response::internal_error())
}

pub(super) fn json_err(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let resp = ApiResponse::<()>::error(code, message);
    let body = serde_json::to_vec(&resp).unwrap_or_default();
    Response::with_status(status)
        .header("content-type", b"application/json".to_vec())
        .body(ResponseBody::Bytes(body))
}
