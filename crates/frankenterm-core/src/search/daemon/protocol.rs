//! Wire protocol for the embedding daemon.

use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_PROTOCOL_BYTES: usize = 4 * 1024 * 1024;

/// Request types for the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum DaemonRequest {
    Embed(EmbedRequest),
    Shutdown,
    Ping,
}

/// Response types from the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum DaemonResponse {
    Embed(EmbedResponse),
    Pong,
    Error(String),
}

/// Request to embed text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedRequest {
    pub id: u64,
    pub text: String,
    pub model: Option<String>,
}

/// Response with embedding vector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedResponse {
    pub id: u64,
    pub vector: Vec<f32>,
    pub model: String,
    pub elapsed_ms: u64,
}

/// Protocol encoding/decoding failures.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolCodecError {
    #[error("protocol payload too large: {actual} bytes (max {max})")]
    PayloadTooLarge { actual: usize, max: usize },
    #[error("protocol serialization failed: {message}")]
    Serialize { message: String },
    #[error("protocol deserialization failed: {message}")]
    Deserialize { message: String },
}

fn ensure_payload_size(bytes_len: usize) -> Result<(), ProtocolCodecError> {
    if bytes_len > MAX_PROTOCOL_BYTES {
        return Err(ProtocolCodecError::PayloadTooLarge {
            actual: bytes_len,
            max: MAX_PROTOCOL_BYTES,
        });
    }
    Ok(())
}

fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolCodecError> {
    let bytes = serde_json::to_vec(value).map_err(|err| ProtocolCodecError::Serialize {
        message: err.to_string(),
    })?;
    ensure_payload_size(bytes.len())?;
    Ok(bytes)
}

fn decode_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, ProtocolCodecError> {
    ensure_payload_size(bytes.len())?;
    serde_json::from_slice(bytes).map_err(|err| ProtocolCodecError::Deserialize {
        message: err.to_string(),
    })
}

impl DaemonRequest {
    /// Encode this request to JSON bytes for transport.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, ProtocolCodecError> {
        encode_json(self)
    }

    /// Decode a request from JSON transport bytes.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, ProtocolCodecError> {
        decode_json(bytes)
    }
}

impl DaemonResponse {
    /// Encode this response to JSON bytes for transport.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, ProtocolCodecError> {
        encode_json(self)
    }

    /// Decode a response from JSON transport bytes.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, ProtocolCodecError> {
        decode_json(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip_json() {
        let request = DaemonRequest::Embed(EmbedRequest {
            id: 42,
            text: "hello".to_string(),
            model: Some("fnv1a-hash-64".to_string()),
        });

        let bytes = request.to_json_bytes().expect("encode request");
        let decoded = DaemonRequest::from_json_bytes(&bytes).expect("decode request");
        assert_eq!(decoded, request);
    }

    #[test]
    fn response_round_trip_json() {
        let response = DaemonResponse::Embed(EmbedResponse {
            id: 9,
            vector: vec![0.1, 0.2, 0.3],
            model: "fnv1a-hash-3".to_string(),
            elapsed_ms: 7,
        });

        let bytes = response.to_json_bytes().expect("encode response");
        let decoded = DaemonResponse::from_json_bytes(&bytes).expect("decode response");
        assert_eq!(decoded, response);
    }

    #[test]
    fn decode_rejects_oversized_payload() {
        let oversized = vec![b'x'; MAX_PROTOCOL_BYTES + 1];
        let err = DaemonRequest::from_json_bytes(&oversized).unwrap_err();
        assert!(matches!(err, ProtocolCodecError::PayloadTooLarge { .. }));
    }

    #[test]
    fn encode_rejects_oversized_payload() {
        let request = DaemonRequest::Embed(EmbedRequest {
            id: 1,
            text: "x".repeat(MAX_PROTOCOL_BYTES + 1024),
            model: None,
        });
        let err = request.to_json_bytes().unwrap_err();
        assert!(matches!(err, ProtocolCodecError::PayloadTooLarge { .. }));
    }

    #[test]
    fn decode_rejects_invalid_json() {
        let err = DaemonResponse::from_json_bytes(br#"{"type":"pong","data":}"#).unwrap_err();
        assert!(matches!(err, ProtocolCodecError::Deserialize { .. }));
    }

    #[test]
    fn ping_serializes_with_tagged_shape() {
        let bytes = DaemonRequest::Ping.to_json_bytes().expect("encode ping");
        let json = String::from_utf8(bytes).expect("utf8");
        assert_eq!(json, r#"{"type":"ping"}"#);
    }
}
