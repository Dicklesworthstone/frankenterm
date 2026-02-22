//! Embedding daemon client.

use std::time::Instant;

use thiserror::Error;

use super::protocol::{
    DaemonRequest, DaemonResponse, EmbedRequest, EmbedResponse, ProtocolCodecError,
};

/// Client for communicating with the embedding daemon.
pub struct EmbedClient {
    endpoint: String,
    timeout_ms: u64,
}

/// Embed daemon client failures.
#[derive(Debug, Error)]
pub enum EmbedClientError {
    #[error("daemon request timed out after {timeout_ms}ms during {operation}")]
    Timeout {
        timeout_ms: u64,
        operation: &'static str,
    },
    #[error("daemon returned error during {operation}: {message}")]
    Daemon {
        operation: &'static str,
        message: String,
    },
    #[error("unexpected daemon response during {operation}")]
    UnexpectedResponse { operation: &'static str },
    #[error("daemon transport failed: {message}")]
    Transport { message: String },
    #[error(transparent)]
    Protocol(#[from] ProtocolCodecError),
}

impl EmbedClient {
    /// Create a new client connecting to the given endpoint.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            timeout_ms: 5000,
        }
    }

    /// Set the request timeout in milliseconds.
    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }

    /// Get the endpoint.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Get the timeout.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    fn effective_timeout_ms(&self) -> u64 {
        self.timeout_ms.max(1)
    }

    fn enforce_timeout(
        &self,
        started_at: Instant,
        operation: &'static str,
    ) -> Result<(), EmbedClientError> {
        let timeout_ms = self.effective_timeout_ms();
        if started_at.elapsed().as_millis() > u128::from(timeout_ms) {
            return Err(EmbedClientError::Timeout {
                timeout_ms,
                operation,
            });
        }
        Ok(())
    }

    fn operation_name(request: &DaemonRequest) -> &'static str {
        match request {
            DaemonRequest::Ping => "ping",
            DaemonRequest::Shutdown => "shutdown",
            DaemonRequest::Embed(_) => "embed",
        }
    }

    /// Send a raw daemon request using a caller-provided transport closure.
    ///
    /// The transport takes request bytes and returns response bytes.
    pub fn call_with<F>(
        &self,
        request: DaemonRequest,
        mut transport: F,
    ) -> Result<DaemonResponse, EmbedClientError>
    where
        F: FnMut(&[u8]) -> Result<Vec<u8>, String>,
    {
        let operation = Self::operation_name(&request);
        let started_at = Instant::now();
        let request_bytes = request.to_json_bytes()?;
        self.enforce_timeout(started_at, operation)?;

        let response_bytes =
            transport(&request_bytes).map_err(|message| EmbedClientError::Transport { message })?;
        self.enforce_timeout(started_at, operation)?;

        let response = DaemonResponse::from_json_bytes(&response_bytes)?;
        self.enforce_timeout(started_at, operation)?;
        Ok(response)
    }

    /// Issue a ping request and require a pong response.
    pub fn ping_with<F>(&self, transport: F) -> Result<(), EmbedClientError>
    where
        F: FnMut(&[u8]) -> Result<Vec<u8>, String>,
    {
        match self.call_with(DaemonRequest::Ping, transport)? {
            DaemonResponse::Pong => Ok(()),
            DaemonResponse::Error(message) => Err(EmbedClientError::Daemon {
                operation: "ping",
                message,
            }),
            _ => Err(EmbedClientError::UnexpectedResponse { operation: "ping" }),
        }
    }

    /// Issue a shutdown request and require a pong response.
    pub fn shutdown_with<F>(&self, transport: F) -> Result<(), EmbedClientError>
    where
        F: FnMut(&[u8]) -> Result<Vec<u8>, String>,
    {
        match self.call_with(DaemonRequest::Shutdown, transport)? {
            DaemonResponse::Pong => Ok(()),
            DaemonResponse::Error(message) => Err(EmbedClientError::Daemon {
                operation: "shutdown",
                message,
            }),
            _ => Err(EmbedClientError::UnexpectedResponse {
                operation: "shutdown",
            }),
        }
    }

    /// Issue an embed request and require an embed response payload.
    pub fn embed_with<F>(
        &self,
        id: u64,
        text: impl Into<String>,
        model: Option<String>,
        transport: F,
    ) -> Result<EmbedResponse, EmbedClientError>
    where
        F: FnMut(&[u8]) -> Result<Vec<u8>, String>,
    {
        let request = DaemonRequest::Embed(EmbedRequest {
            id,
            text: text.into(),
            model,
        });
        match self.call_with(request, transport)? {
            DaemonResponse::Embed(response) => Ok(response),
            DaemonResponse::Error(message) => Err(EmbedClientError::Daemon {
                operation: "embed",
                message,
            }),
            _ => Err(EmbedClientError::UnexpectedResponse { operation: "embed" }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::daemon::server::EmbedServer;

    fn loopback_transport(
        server: &EmbedServer,
    ) -> impl FnMut(&[u8]) -> Result<Vec<u8>, String> + '_ {
        move |request_bytes| Ok(server.handle_encoded(request_bytes))
    }

    #[test]
    fn ping_with_local_loopback_succeeds() {
        let client = EmbedClient::new("loopback://daemon");
        let server = EmbedServer::new(0);
        client
            .ping_with(loopback_transport(&server))
            .expect("ping succeeds");
    }

    #[test]
    fn embed_with_local_loopback_returns_vector() {
        let client = EmbedClient::new("loopback://daemon");
        let server = EmbedServer::new(0);
        let response = client
            .embed_with(
                7,
                "semantic retrieval",
                Some("fnv1a-hash-32".to_string()),
                loopback_transport(&server),
            )
            .expect("embed succeeds");

        assert_eq!(response.id, 7);
        assert_eq!(response.model, "fnv1a-hash-32");
        assert_eq!(response.vector.len(), 32);
    }

    #[test]
    fn embed_with_maps_daemon_error_response() {
        let client = EmbedClient::new("loopback://daemon");
        let server = EmbedServer::new(0);
        let err = client
            .embed_with(
                8,
                "ignored",
                Some("unknown-model".to_string()),
                loopback_transport(&server),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            EmbedClientError::Daemon {
                operation: "embed",
                ..
            }
        ));
    }

    #[test]
    fn call_with_maps_transport_error() {
        let client = EmbedClient::new("loopback://daemon");
        let err = client
            .call_with(DaemonRequest::Ping, |_| {
                Err("socket write failed".to_string())
            })
            .unwrap_err();
        assert!(matches!(err, EmbedClientError::Transport { .. }));
    }

    #[test]
    fn call_with_enforces_timeout_budget() {
        let client = EmbedClient::new("loopback://daemon").with_timeout_ms(1);
        let err = client
            .call_with(DaemonRequest::Ping, |_| {
                std::thread::sleep(std::time::Duration::from_millis(5));
                DaemonResponse::Pong
                    .to_json_bytes()
                    .map_err(|codec| codec.to_string())
            })
            .unwrap_err();
        assert!(matches!(
            err,
            EmbedClientError::Timeout {
                operation: "ping",
                ..
            }
        ));
    }

    #[test]
    fn timeout_ms_zero_is_clamped_to_one() {
        let client = EmbedClient::new("loopback://daemon").with_timeout_ms(0);
        let err = client
            .call_with(DaemonRequest::Ping, |_| {
                std::thread::sleep(std::time::Duration::from_millis(2));
                DaemonResponse::Pong
                    .to_json_bytes()
                    .map_err(|codec| codec.to_string())
            })
            .unwrap_err();
        assert!(matches!(
            err,
            EmbedClientError::Timeout { timeout_ms: 1, .. }
        ));
    }
}
