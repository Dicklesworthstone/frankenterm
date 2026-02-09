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
//! - `environment`: Environment detection (WezTerm, shell, agents, system)
//! - `approval`: Allow-once approvals for RequireApproval decisions
//! - `policy`: Safety and rate limiting
//! - `wait`: Wait-for utilities (no fixed sleeps)
//! - `accounts`: Account management and selection policy
//! - `plan`: Action plan types for unified workflow representation
//! - `browser`: Browser automation scaffolding (feature-gated: `browser`)
//! - `sync`: Optional sync scaffolding (feature-gated: `sync`)
//! - `web`: Optional HTTP server scaffolding (feature-gated: `web`)
//!
//! # Safety
//!
//! This crate forbids unsafe code.

#![forbid(unsafe_code)]
#![feature(stmt_expr_attributes)]

pub mod accounts;
pub mod alerts;
pub mod api_schema;
pub mod approval;
pub mod backpressure;
pub mod backup;
pub mod cass;
pub mod caut;
#[cfg(test)]
pub mod chaos;
pub mod circuit_breaker;
pub mod cleanup;
pub mod config;
pub mod config_profiles;
pub mod crash;
pub mod degradation;
pub mod desktop_notify;
pub mod diagnostic;
pub mod docs_gen;
pub mod dry_run;
pub mod email_notify;
pub mod environment;
pub mod error;
pub mod error_codes;
pub mod event_templates;
pub mod events;
pub mod explanations;
pub mod export;
pub mod extensions;
pub mod incident_bundle;
pub mod ingest;
#[cfg(unix)]
pub mod ipc;
pub mod learn;
pub mod lock;
pub mod logging;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "metrics")]
pub mod metrics;
pub mod notifications;
pub mod output;
pub mod patterns;
pub mod plan;
pub mod policy;
pub mod pool;
pub mod recording;
pub mod replay;
pub mod reports;
pub mod retry;
pub mod robot_types;
pub mod rulesets;
pub mod runtime;
pub mod screen_state;
pub mod search_explain;
pub mod secrets;
pub mod session_correlation;
pub mod setup;
pub mod storage;
pub mod storage_targets;
pub mod suggestions;
pub mod tailer;
pub mod undo;
pub mod wait;
pub mod watchdog;
pub mod webhook;
pub mod wezterm;
pub mod workflows;

#[cfg(feature = "vendored")]
pub mod vendored;

#[cfg(feature = "vendored")]
pub mod wezterm_native;

#[cfg(feature = "native-wezterm")]
pub mod native_events;

#[cfg(feature = "browser")]
pub mod browser;

// tui and ftui are mutually exclusive feature flags (unless `rollout` is active).
// The legacy `tui` feature uses ratatui/crossterm; the new `ftui` feature uses FrankenTUI.
// Both compile the `tui` module but with different rendering backends.
// The `rollout` feature compiles both backends and enables runtime selection via
// the WA_TUI_BACKEND environment variable (see docs/ftui-rollout-strategy.md).
// See docs/adr/0004-phased-rollout-and-rollback.md for migration details.
#[cfg(all(feature = "tui", feature = "ftui", not(feature = "rollout")))]
compile_error!(
    "Features `tui` and `ftui` are mutually exclusive. \
     Use `--features tui` for the legacy ratatui backend or \
     `--features ftui` for the FrankenTUI backend, not both. \
     Use `--features rollout` for runtime backend selection during migration."
);

#[cfg(any(feature = "tui", feature = "ftui"))]
pub mod tui;

#[cfg(feature = "web")]
pub mod web;

pub mod ui_query;

pub mod distributed;
pub mod simulation;
pub mod wire_protocol;

#[cfg(feature = "sync")]
pub mod sync;

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
