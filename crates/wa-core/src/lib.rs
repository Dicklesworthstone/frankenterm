//! wa-core: Core library for WezTerm Automata
//!
//! This crate provides the core functionality for `wa`, a terminal hypervisor
//! for AI agent swarms running in WezTerm.
//!
//! # Architecture
//!
//! ```text
//! WezTerm CLI → Ingest Pipeline → Storage (SQLite/FTS5)
//!                    ↓
//!            Pattern Engine → Event Bus → Workflows
//!                                   ↓
//!                            Robot Mode / MCP
//! ```
//!
//! # Modules
//!
//! - `wezterm`: WezTerm CLI client wrapper
//! - `storage`: SQLite storage with FTS5 search
//! - `ingest`: Pane output capture and delta extraction
//! - `patterns`: Pattern detection engine
//! - `events`: Event bus for detections and signals
//! - `workflows`: Durable workflow execution
//! - `config`: Configuration management
//! - `approval`: Allow-once approvals for RequireApproval decisions
//! - `policy`: Safety and rate limiting
//! - `wait`: Wait-for utilities (no fixed sleeps)
//!
//! # Safety
//!
//! This crate forbids unsafe code.

#![forbid(unsafe_code)]
#![feature(stmt_expr_attributes)]

pub mod approval;
pub mod config;
pub mod dry_run;
pub mod error;
pub mod events;
pub mod ingest;
pub mod lock;
pub mod logging;
pub mod patterns;
pub mod policy;
pub mod runtime;
pub mod storage;
pub mod wait;
pub mod wezterm;
pub mod workflows;

pub use error::{Error, Result};

/// Library version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
