//! Embedding daemon server.

use super::protocol::{DaemonRequest, DaemonResponse};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Embedding server that processes embedding requests.
pub struct EmbedServer {
    running: Arc<AtomicBool>,
    port: u16,
}

impl EmbedServer {
    /// Create a new embed server on the given port.
    pub fn new(port: u16) -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            port,
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

    /// Process a single request (stub).
    pub fn handle(&self, request: DaemonRequest) -> DaemonResponse {
        match request {
            DaemonRequest::Ping => DaemonResponse::Pong,
            DaemonRequest::Shutdown => {
                self.running.store(false, Ordering::Relaxed);
                DaemonResponse::Pong
            }
            DaemonRequest::Embed(_req) => {
                DaemonResponse::Error("not yet implemented".into())
            }
        }
    }
}
