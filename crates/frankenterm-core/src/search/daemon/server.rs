//! Embedding daemon server.

use super::protocol::{DaemonRequest, DaemonResponse};
use super::worker::EmbedWorker;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Embedding server that processes embedding requests.
pub struct EmbedServer {
    running: Arc<AtomicBool>,
    port: u16,
    worker: EmbedWorker,
}

impl EmbedServer {
    /// Create a new embed server on the given port.
    pub fn new(port: u16) -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
            port,
            worker: EmbedWorker::new(0),
        }
    }

    /// Get the port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Check if the server is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Set running state.
    pub fn set_running(&self, running: bool) {
        self.running.store(running, Ordering::Relaxed);
    }

    /// Number of processed embed requests.
    pub fn processed(&self) -> u64 {
        self.worker.processed()
    }

    /// Process a single request (stub).
    pub fn handle(&self, request: DaemonRequest) -> DaemonResponse {
        match request {
            DaemonRequest::Ping => DaemonResponse::Pong,
            DaemonRequest::Shutdown => {
                self.running.store(false, Ordering::Relaxed);
                DaemonResponse::Pong
            }
            DaemonRequest::Embed(req) => {
                if !self.is_running() {
                    return DaemonResponse::Error("embedding daemon is not running".to_string());
                }
                match self.worker.process(&req) {
                    Ok(resp) => DaemonResponse::Embed(resp),
                    Err(err) => DaemonResponse::Error(err),
                }
            }
        }
    }

    /// Process a JSON-encoded request payload and return an encoded response.
    pub fn handle_encoded(&self, request_bytes: &[u8]) -> Vec<u8> {
        let response = match DaemonRequest::from_json_bytes(request_bytes) {
            Ok(request) => self.handle(request),
            Err(err) => DaemonResponse::Error(format!("invalid daemon request: {err}")),
        };

        match response.to_json_bytes() {
            Ok(bytes) => bytes,
            Err(err) => {
                let fallback =
                    DaemonResponse::Error(format!("failed to encode daemon response: {err}"));
                fallback.to_json_bytes().unwrap_or_else(|_| {
                    br#"{"type":"error","data":"failed to encode daemon response"}"#.to_vec()
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::daemon::protocol::EmbedRequest;

    fn embed_request(id: u64, text: &str, model: Option<&str>) -> DaemonRequest {
        DaemonRequest::Embed(EmbedRequest {
            id,
            text: text.to_string(),
            model: model.map(ToString::to_string),
        })
    }

    fn decode_response(bytes: &[u8]) -> DaemonResponse {
        DaemonResponse::from_json_bytes(bytes).expect("decode daemon response")
    }

    #[test]
    fn new_server_is_running() {
        let server = EmbedServer::new(4040);
        assert!(server.is_running());
        assert_eq!(server.port(), 4040);
    }

    #[test]
    fn ping_and_shutdown_update_state() {
        let server = EmbedServer::new(5050);
        assert!(matches!(
            server.handle(DaemonRequest::Ping),
            DaemonResponse::Pong
        ));
        assert!(matches!(
            server.handle(DaemonRequest::Shutdown),
            DaemonResponse::Pong
        ));
        assert!(!server.is_running());
    }

    #[test]
    fn embed_request_returns_vector_response() {
        let server = EmbedServer::new(6060);
        let response = server.handle(embed_request(11, "hybrid search", Some("fnv1a-hash-64")));
        let embed = match response {
            DaemonResponse::Embed(data) => data,
            other => panic!("expected embed response, got {other:?}"),
        };

        assert_eq!(embed.id, 11);
        assert_eq!(embed.model, "fnv1a-hash-64");
        assert_eq!(embed.vector.len(), 64);
        assert_eq!(server.processed(), 1);
    }

    #[test]
    fn embed_request_errors_when_stopped() {
        let server = EmbedServer::new(7070);
        server.set_running(false);
        let response = server.handle(embed_request(12, "ignored", None));
        let err = match response {
            DaemonResponse::Error(err) => err,
            other => panic!("expected error response, got {other:?}"),
        };
        assert!(err.contains("not running"));
    }

    #[test]
    fn embed_request_returns_model_validation_error() {
        let server = EmbedServer::new(8080);
        let response = server.handle(embed_request(13, "ignored", Some("unknown-model")));
        let err = match response {
            DaemonResponse::Error(err) => err,
            other => panic!("expected error response, got {other:?}"),
        };
        assert!(err.contains("unsupported embed model"));
    }

    // ── DarkMill test expansion ──────────────────────────────────────

    #[test]
    fn port_is_stored() {
        let server = EmbedServer::new(9090);
        assert_eq!(server.port(), 9090);
    }

    #[test]
    fn port_zero() {
        let server = EmbedServer::new(0);
        assert_eq!(server.port(), 0);
    }

    #[test]
    fn port_max() {
        let server = EmbedServer::new(u16::MAX);
        assert_eq!(server.port(), u16::MAX);
    }

    #[test]
    fn set_running_false_stops_server() {
        let server = EmbedServer::new(1000);
        assert!(server.is_running());
        server.set_running(false);
        assert!(!server.is_running());
    }

    #[test]
    fn set_running_true_restarts_server() {
        let server = EmbedServer::new(1000);
        server.set_running(false);
        assert!(!server.is_running());
        server.set_running(true);
        assert!(server.is_running());
    }

    #[test]
    fn set_running_idempotent() {
        let server = EmbedServer::new(1000);
        server.set_running(true);
        assert!(server.is_running());
        server.set_running(true);
        assert!(server.is_running());
    }

    #[test]
    fn processed_starts_at_zero() {
        let server = EmbedServer::new(1000);
        assert_eq!(server.processed(), 0);
    }

    #[test]
    fn processed_increments_on_embed() {
        let server = EmbedServer::new(1000);
        server.handle(embed_request(1, "a", None));
        server.handle(embed_request(2, "b", None));
        assert_eq!(server.processed(), 2);
    }

    #[test]
    fn processed_not_incremented_by_ping() {
        let server = EmbedServer::new(1000);
        server.handle(DaemonRequest::Ping);
        assert_eq!(server.processed(), 0);
    }

    #[test]
    fn processed_not_incremented_by_shutdown() {
        let server = EmbedServer::new(1000);
        server.handle(DaemonRequest::Shutdown);
        assert_eq!(server.processed(), 0);
    }

    #[test]
    fn shutdown_then_embed_fails() {
        let server = EmbedServer::new(1000);
        server.handle(DaemonRequest::Shutdown);
        let response = server.handle(embed_request(1, "test", None));
        let is_err = matches!(response, DaemonResponse::Error(_));
        assert!(is_err, "embed after shutdown should fail");
    }

    #[test]
    fn multiple_pings_all_return_pong() {
        let server = EmbedServer::new(1000);
        for _ in 0..5 {
            let is_pong = matches!(server.handle(DaemonRequest::Ping), DaemonResponse::Pong);
            assert!(is_pong);
        }
    }

    #[test]
    fn embed_with_explicit_dimension() {
        let server = EmbedServer::new(1000);
        let resp = server.handle(embed_request(1, "test", Some("fnv1a-hash-32")));
        let embed = match resp {
            DaemonResponse::Embed(data) => data,
            other => panic!("expected embed, got {other:?}"),
        };
        assert_eq!(embed.vector.len(), 32);
        assert_eq!(embed.model, "fnv1a-hash-32");
    }

    #[test]
    fn embed_default_model_is_128d() {
        let server = EmbedServer::new(1000);
        let resp = server.handle(embed_request(1, "test", None));
        let embed = match resp {
            DaemonResponse::Embed(data) => data,
            other => panic!("expected embed, got {other:?}"),
        };
        assert_eq!(embed.vector.len(), 128);
        assert_eq!(embed.model, "fnv1a-hash-128");
    }

    #[test]
    fn embed_id_passes_through() {
        let server = EmbedServer::new(1000);
        for id in [0, 42, u64::MAX] {
            let resp = server.handle(embed_request(id, "test", None));
            let embed = match resp {
                DaemonResponse::Embed(data) => data,
                other => panic!("expected embed, got {other:?}"),
            };
            assert_eq!(embed.id, id);
        }
    }

    #[test]
    fn embed_invalid_dimension_returns_error() {
        let server = EmbedServer::new(1000);
        let resp = server.handle(embed_request(1, "test", Some("fnv1a-hash-0")));
        let is_err = matches!(resp, DaemonResponse::Error(_));
        assert!(is_err, "zero dimension should error");
    }

    #[test]
    fn embed_non_numeric_dimension_returns_error() {
        let server = EmbedServer::new(1000);
        let resp = server.handle(embed_request(1, "test", Some("fnv1a-hash-xyz")));
        let is_err = matches!(resp, DaemonResponse::Error(_));
        assert!(is_err, "non-numeric dimension should error");
    }

    #[test]
    fn embed_empty_text_succeeds() {
        let server = EmbedServer::new(1000);
        let resp = server.handle(embed_request(1, "", None));
        let is_embed = matches!(resp, DaemonResponse::Embed(_));
        assert!(is_embed, "empty text should produce embedding");
    }

    #[test]
    fn embed_unicode_text_succeeds() {
        let server = EmbedServer::new(1000);
        let resp = server.handle(embed_request(1, "日本語 🦀", None));
        let is_embed = matches!(resp, DaemonResponse::Embed(_));
        assert!(is_embed, "unicode text should produce embedding");
    }

    #[test]
    fn restart_after_shutdown_allows_embeds() {
        let server = EmbedServer::new(1000);
        server.handle(DaemonRequest::Shutdown);
        assert!(!server.is_running());
        server.set_running(true);
        let resp = server.handle(embed_request(1, "after restart", None));
        let is_embed = matches!(resp, DaemonResponse::Embed(_));
        assert!(is_embed, "embed after restart should succeed");
    }

    #[test]
    fn embed_processed_not_incremented_on_stopped() {
        let server = EmbedServer::new(1000);
        server.set_running(false);
        server.handle(embed_request(1, "ignored", None));
        assert_eq!(server.processed(), 0);
    }

    #[test]
    fn embed_processed_not_incremented_on_model_error() {
        let server = EmbedServer::new(1000);
        server.handle(embed_request(1, "ignored", Some("bad-model")));
        assert_eq!(server.processed(), 0);
    }

    #[test]
    fn sequential_embed_shutdown_ping_sequence() {
        let server = EmbedServer::new(1000);
        // Embed first
        let r1 = server.handle(embed_request(1, "first", None));
        let is_embed = matches!(r1, DaemonResponse::Embed(_));
        assert!(is_embed);
        // Shutdown
        let r2 = server.handle(DaemonRequest::Shutdown);
        let is_pong = matches!(r2, DaemonResponse::Pong);
        assert!(is_pong);
        assert!(!server.is_running());
        // Ping still works after shutdown
        let r3 = server.handle(DaemonRequest::Ping);
        let is_pong2 = matches!(r3, DaemonResponse::Pong);
        assert!(is_pong2);
        // But embed fails
        let r4 = server.handle(embed_request(2, "after shutdown", None));
        let is_err = matches!(r4, DaemonResponse::Error(_));
        assert!(is_err);
        assert_eq!(server.processed(), 1);
    }

    #[test]
    fn different_texts_produce_different_embeddings() {
        let server = EmbedServer::new(1000);
        let r1 = server.handle(embed_request(1, "hello", None));
        let r2 = server.handle(embed_request(2, "world", None));
        let v1 = match r1 {
            DaemonResponse::Embed(data) => data.vector,
            other => panic!("expected embed, got {other:?}"),
        };
        let v2 = match r2 {
            DaemonResponse::Embed(data) => data.vector,
            other => panic!("expected embed, got {other:?}"),
        };
        assert_ne!(v1, v2);
    }

    #[test]
    fn same_text_produces_same_embedding() {
        let server = EmbedServer::new(1000);
        let r1 = server.handle(embed_request(1, "deterministic", None));
        let r2 = server.handle(embed_request(2, "deterministic", None));
        let v1 = match r1 {
            DaemonResponse::Embed(data) => data.vector,
            other => panic!("expected embed, got {other:?}"),
        };
        let v2 = match r2 {
            DaemonResponse::Embed(data) => data.vector,
            other => panic!("expected embed, got {other:?}"),
        };
        assert_eq!(v1, v2);
    }

    #[test]
    fn handle_encoded_ping_round_trip() {
        let server = EmbedServer::new(9999);
        let request_bytes = DaemonRequest::Ping
            .to_json_bytes()
            .expect("encode ping request");
        let response_bytes = server.handle_encoded(&request_bytes);
        assert!(matches!(
            decode_response(&response_bytes),
            DaemonResponse::Pong
        ));
    }

    #[test]
    fn handle_encoded_embed_round_trip() {
        let server = EmbedServer::new(9999);
        let request_bytes = embed_request(55, "semantic daemon", Some("fnv1a-hash-16"))
            .to_json_bytes()
            .expect("encode embed request");
        let response_bytes = server.handle_encoded(&request_bytes);
        let response = decode_response(&response_bytes);
        let embed = match response {
            DaemonResponse::Embed(embed) => embed,
            other => panic!("expected embed response, got {other:?}"),
        };
        assert_eq!(embed.id, 55);
        assert_eq!(embed.model, "fnv1a-hash-16");
        assert_eq!(embed.vector.len(), 16);
    }

    #[test]
    fn handle_encoded_invalid_json_maps_to_error_response() {
        let server = EmbedServer::new(9999);
        let response_bytes = server.handle_encoded(br#"{"type":"ping","data":}"#);
        let response = decode_response(&response_bytes);
        let err = match response {
            DaemonResponse::Error(err) => err,
            other => panic!("expected error response, got {other:?}"),
        };
        assert!(err.contains("invalid daemon request"));
    }

    #[test]
    fn handle_encoded_oversized_request_maps_to_error_response() {
        let server = EmbedServer::new(9999);
        let oversized = vec![b'x'; (4 * 1024 * 1024) + 1];
        let response_bytes = server.handle_encoded(&oversized);
        let response = decode_response(&response_bytes);
        let err = match response {
            DaemonResponse::Error(err) => err,
            other => panic!("expected error response, got {other:?}"),
        };
        assert!(err.contains("invalid daemon request"));
    }
}
