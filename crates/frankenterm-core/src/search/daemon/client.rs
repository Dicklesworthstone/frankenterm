//! Embedding daemon client.

/// Client for communicating with the embedding daemon.
pub struct EmbedClient {
    endpoint: String,
    timeout_ms: u64,
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
}
