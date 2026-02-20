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
}
