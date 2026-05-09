//! Embedding daemon client.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use super::protocol::{
    DaemonRequest, DaemonResponse, EmbedRequest, EmbedResponse, ProtocolCodecError,
};

const DEFAULT_MAX_IN_FLIGHT_TRANSPORTS: usize = 16;

/// Client for communicating with the embedding daemon.
pub struct EmbedClient {
    endpoint: String,
    timeout_ms: u64,
    max_in_flight_transports: usize,
    in_flight_transports: Arc<AtomicUsize>,
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
            max_in_flight_transports: DEFAULT_MAX_IN_FLIGHT_TRANSPORTS,
            in_flight_transports: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Set the request timeout in milliseconds.
    #[must_use]
    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }

    /// Set the maximum number of daemon transport workers allowed to be alive
    /// for this client at one time.
    #[must_use]
    pub fn with_max_in_flight_transports(mut self, max_in_flight: usize) -> Self {
        self.max_in_flight_transports = max_in_flight.max(1);
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

    fn timeout_duration(&self) -> Duration {
        Duration::from_millis(self.effective_timeout_ms())
    }

    fn timeout_error(&self, operation: &'static str) -> EmbedClientError {
        EmbedClientError::Timeout {
            timeout_ms: self.effective_timeout_ms(),
            operation,
        }
    }

    fn acquire_transport_slot(
        &self,
        operation: &'static str,
    ) -> Result<TransportSlotGuard, EmbedClientError> {
        loop {
            let current = self.in_flight_transports.load(Ordering::Acquire);
            if current >= self.max_in_flight_transports {
                return Err(EmbedClientError::Transport {
                    message: format!(
                        "daemon transport worker limit exceeded during {operation}: \
                         {current} in flight, max {}",
                        self.max_in_flight_transports
                    ),
                });
            }
            if self
                .in_flight_transports
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(TransportSlotGuard {
                    in_flight: Arc::clone(&self.in_flight_transports),
                });
            }
        }
    }

    fn remaining_timeout(
        &self,
        started_at: Instant,
        operation: &'static str,
    ) -> Result<Duration, EmbedClientError> {
        let timeout = self.timeout_duration();
        let elapsed = started_at.elapsed();
        if elapsed >= timeout {
            return Err(self.timeout_error(operation));
        }
        timeout
            .checked_sub(elapsed)
            .ok_or_else(|| self.timeout_error(operation))
    }

    fn enforce_timeout(
        &self,
        started_at: Instant,
        operation: &'static str,
    ) -> Result<(), EmbedClientError> {
        self.remaining_timeout(started_at, operation)?;
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
        transport: F,
    ) -> Result<DaemonResponse, EmbedClientError>
    where
        F: FnOnce(&[u8]) -> Result<Vec<u8>, String> + Send + 'static,
    {
        let operation = Self::operation_name(&request);
        let started_at = Instant::now();
        let request_bytes = request.to_json_bytes()?;
        self.enforce_timeout(started_at, operation)?;

        let response_bytes =
            self.run_transport_with_timeout(request_bytes, transport, operation, started_at)?;
        self.enforce_timeout(started_at, operation)?;

        let response = DaemonResponse::from_json_bytes(&response_bytes)?;
        self.enforce_timeout(started_at, operation)?;
        Ok(response)
    }

    fn run_transport_with_timeout<F>(
        &self,
        request_bytes: Vec<u8>,
        transport: F,
        operation: &'static str,
        started_at: Instant,
    ) -> Result<Vec<u8>, EmbedClientError>
    where
        F: FnOnce(&[u8]) -> Result<Vec<u8>, String> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let transport_slot = self.acquire_transport_slot(operation)?;
        thread::Builder::new()
            .name(format!("ft-embed-daemon-client-{operation}"))
            .spawn(move || {
                let _transport_slot = transport_slot;
                let result = transport(&request_bytes);
                let _ = tx.send(result);
            })
            .map_err(|err| EmbedClientError::Transport {
                message: format!("failed to start daemon transport worker: {err}"),
            })?;

        let wait_for = self.remaining_timeout(started_at, operation)?;
        match rx.recv_timeout(wait_for) {
            Ok(Ok(response_bytes)) => Ok(response_bytes),
            Ok(Err(message)) => Err(EmbedClientError::Transport { message }),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(self.timeout_error(operation)),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(EmbedClientError::Transport {
                message: "daemon transport worker terminated before response".to_string(),
            }),
        }
    }

    /// Issue a ping request and require a pong response.
    pub fn ping_with<F>(&self, transport: F) -> Result<(), EmbedClientError>
    where
        F: FnOnce(&[u8]) -> Result<Vec<u8>, String> + Send + 'static,
    {
        match self.call_with(DaemonRequest::Ping, transport)? {
            DaemonResponse::Pong => Ok(()),
            DaemonResponse::Error(message) => Err(EmbedClientError::Daemon {
                operation: "ping",
                message,
            }),
            DaemonResponse::Embed(_) => {
                Err(EmbedClientError::UnexpectedResponse { operation: "ping" })
            }
        }
    }

    /// Issue a shutdown request and require a pong response.
    pub fn shutdown_with<F>(&self, transport: F) -> Result<(), EmbedClientError>
    where
        F: FnOnce(&[u8]) -> Result<Vec<u8>, String> + Send + 'static,
    {
        match self.call_with(DaemonRequest::Shutdown, transport)? {
            DaemonResponse::Pong => Ok(()),
            DaemonResponse::Error(message) => Err(EmbedClientError::Daemon {
                operation: "shutdown",
                message,
            }),
            DaemonResponse::Embed(_) => Err(EmbedClientError::UnexpectedResponse {
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
        F: FnOnce(&[u8]) -> Result<Vec<u8>, String> + Send + 'static,
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
            DaemonResponse::Pong => {
                Err(EmbedClientError::UnexpectedResponse { operation: "embed" })
            }
        }
    }
}

struct TransportSlotGuard {
    in_flight: Arc<AtomicUsize>,
}

impl Drop for TransportSlotGuard {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::daemon::server::EmbedServer;
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};

    fn loopback_transport(
        server: Arc<EmbedServer>,
    ) -> impl FnOnce(&[u8]) -> Result<Vec<u8>, String> + Send + 'static {
        move |request_bytes| Ok(server.handle_encoded(request_bytes))
    }

    #[test]
    fn ping_with_local_loopback_succeeds() {
        let client = EmbedClient::new("loopback://daemon");
        let server = Arc::new(EmbedServer::new(0));
        client
            .ping_with(loopback_transport(server))
            .expect("ping succeeds");
    }

    #[test]
    fn embed_with_local_loopback_returns_vector() {
        let client = EmbedClient::new("loopback://daemon");
        let server = Arc::new(EmbedServer::new(0));
        let response = client
            .embed_with(
                7,
                "semantic retrieval",
                Some("fnv1a-hash-32".to_string()),
                loopback_transport(server),
            )
            .expect("embed succeeds");

        assert_eq!(response.id, 7);
        assert_eq!(response.model, "fnv1a-hash-32");
        assert_eq!(response.vector.len(), 32);
    }

    #[test]
    fn embed_with_maps_daemon_error_response() {
        let client = EmbedClient::new("loopback://daemon");
        let server = Arc::new(EmbedServer::new(0));
        let err = client
            .embed_with(
                8,
                "ignored",
                Some("unknown-model".to_string()),
                loopback_transport(server),
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
                std::thread::sleep(std::time::Duration::from_millis(20));
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
    fn call_with_returns_before_blocking_transport_finishes() {
        let client = EmbedClient::new("loopback://daemon").with_timeout_ms(20);
        let started_at = Instant::now();
        let err = client
            .call_with(DaemonRequest::Ping, |_| {
                std::thread::sleep(std::time::Duration::from_millis(750));
                DaemonResponse::Pong
                    .to_json_bytes()
                    .map_err(|codec| codec.to_string())
            })
            .unwrap_err();
        let elapsed = started_at.elapsed();

        assert!(matches!(
            err,
            EmbedClientError::Timeout {
                operation: "ping",
                ..
            }
        ));
        assert!(
            elapsed < std::time::Duration::from_millis(400),
            "timeout should bound the blocking transport wait, elapsed={elapsed:?}"
        );
    }

    struct BlockedTransportRelease {
        gate: Arc<(Mutex<bool>, Condvar)>,
    }

    impl BlockedTransportRelease {
        fn new(gate: Arc<(Mutex<bool>, Condvar)>) -> Self {
            Self { gate }
        }

        fn release(&self) {
            let (lock, condvar) = &*self.gate;
            let mut released = lock.lock().expect("transport gate lock");
            *released = true;
            condvar.notify_all();
        }
    }

    impl Drop for BlockedTransportRelease {
        fn drop(&mut self) {
            self.release();
        }
    }

    fn wait_for_transport_drain(client: &EmbedClient) {
        for _ in 0..100 {
            if client.in_flight_transports.load(Ordering::Acquire) == 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(client.in_flight_transports.load(Ordering::Acquire), 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(8))]

        #[test]
        fn proptest_call_with_caps_blocked_transport_workers(attempts in 1usize..=12) {
            let max_in_flight = 3;
            let client = EmbedClient::new("loopback://daemon")
                .with_timeout_ms(1)
                .with_max_in_flight_transports(max_in_flight);
            let gate = Arc::new((Mutex::new(false), Condvar::new()));
            let release_guard = BlockedTransportRelease::new(Arc::clone(&gate));
            let started = Arc::new(AtomicUsize::new(0));

            for _ in 0..attempts {
                let gate = Arc::clone(&gate);
                let started_for_transport = Arc::clone(&started);
                let result = client.call_with(DaemonRequest::Ping, move |_| {
                    started_for_transport.fetch_add(1, Ordering::AcqRel);
                    let (lock, condvar) = &*gate;
                    let mut released = lock.lock().map_err(|err| err.to_string())?;
                    while !*released {
                        released = condvar.wait(released).map_err(|err| err.to_string())?;
                    }
                    DaemonResponse::Pong
                        .to_json_bytes()
                        .map_err(|codec| codec.to_string())
                });

                let bounded_result = match result {
                    Err(EmbedClientError::Timeout {
                        operation: "ping",
                        ..
                    }) => true,
                    Err(EmbedClientError::Transport { message }) => {
                        message.contains("transport worker limit exceeded")
                    }
                    _ => false,
                };
                prop_assert!(bounded_result);
                prop_assert!(started.load(Ordering::Acquire) <= max_in_flight);
                prop_assert!(
                    client.in_flight_transports.load(Ordering::Acquire) <= max_in_flight
                );
            }

            prop_assert_eq!(started.load(Ordering::Acquire), attempts.min(max_in_flight));
            release_guard.release();
            wait_for_transport_drain(&client);
        }
    }

    #[test]
    fn timeout_ms_zero_is_clamped_to_one() {
        let client = EmbedClient::new("loopback://daemon").with_timeout_ms(0);
        let err = client
            .call_with(DaemonRequest::Ping, |_| {
                std::thread::sleep(std::time::Duration::from_millis(20));
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
