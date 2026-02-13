//! Wire protocol for the embedding daemon.

/// Request types for the daemon.
#[derive(Debug, Clone)]
pub enum DaemonRequest {
    Embed(EmbedRequest),
    Shutdown,
    Ping,
}

/// Response types from the daemon.
#[derive(Debug, Clone)]
pub enum DaemonResponse {
    Embed(EmbedResponse),
    Pong,
    Error(String),
}

/// Request to embed text.
#[derive(Debug, Clone)]
pub struct EmbedRequest {
    pub id: u64,
    pub text: String,
    pub model: Option<String>,
}

/// Response with embedding vector.
#[derive(Debug, Clone)]
pub struct EmbedResponse {
    pub id: u64,
    pub vector: Vec<f32>,
    pub model: String,
    pub elapsed_ms: u64,
}
