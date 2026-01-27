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
//! - `event_templates`: Human-readable event summary templates
//! - `explanations`: Reusable explanation templates for wa why and errors
//! - `suggestions`: Context-aware suggestion system for actionable errors
//! - `workflows`: Durable workflow execution
//! - `config`: Configuration management
//! - `approval`: Allow-once approvals for RequireApproval decisions
//! - `policy`: Safety and rate limiting
//! - `wait`: Wait-for utilities (no fixed sleeps)
//! - `accounts`: Account management and selection policy
//! - `plan`: Action plan types for unified workflow representation
//!
//! # Safety
//!
//! This crate forbids unsafe code.

#![forbid(unsafe_code)]
#![feature(stmt_expr_attributes)]

pub mod accounts;
pub mod approval;
pub mod caut;
pub mod config;
pub mod crash;
pub mod dry_run;
pub mod error;
pub mod error_codes;
pub mod event_templates;
pub mod events;
pub mod explanations;
pub mod ingest;
#[cfg(unix)]
pub mod ipc;
pub mod lock;
pub mod logging;
pub mod output;
pub mod patterns;
pub mod plan;
pub mod policy;
pub mod runtime;
pub mod storage;
pub mod suggestions;
pub mod tailer;
pub mod wait;
pub mod wezterm;
pub mod workflows;

#[cfg(feature = "tui")]
pub mod tui;

pub use error::{Error, Result, StorageError};

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
