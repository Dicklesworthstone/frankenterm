//! Embedding daemon — background service for async embedding computation.
//!
//! Runs as a separate process/thread to avoid blocking the terminal.
//! Requires the `semantic-search` feature.

mod protocol;
mod server;
mod client;
mod worker;

pub use protocol::{DaemonRequest, DaemonResponse, EmbedRequest, EmbedResponse};
pub use server::EmbedServer;
pub use client::EmbedClient;
pub use worker::EmbedWorker;
