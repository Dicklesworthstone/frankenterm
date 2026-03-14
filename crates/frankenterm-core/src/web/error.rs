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
