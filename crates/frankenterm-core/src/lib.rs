//! frankenterm-core: Core library for FrankenTerm
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
//! - `explanations`: Reusable explanation templates for ft why and errors
//! - `suggestions`: Context-aware suggestion system for actionable errors
//! - `workflows`: Durable workflow execution
//! - `config`: Configuration management
//! - `cx`: Asupersync capability context adapters (feature-gated: `asupersync-runtime`)
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
pub mod agent_correlator;
pub mod alerts;
pub mod api_schema;
pub mod approval;
pub mod auto_tune;
pub mod backpressure;
pub mod backpressure_severity;
pub mod backup;
pub mod bayesian_ledger;
pub mod bloom_filter;
pub mod bocpd;
pub mod build_coord;
pub mod cass;
pub mod causal_dag;
pub mod caut;
#[cfg(test)]
pub mod chaos;
pub mod circuit_breaker;
pub mod cleanup;
pub mod command_guard;
pub mod completion_token;
pub mod concurrent_map;
pub mod config;
pub mod config_profiles;
pub mod conformal;
pub mod content_dedup;
pub mod continuous_backpressure;
pub mod cpu_pressure;
pub mod crash;
pub mod cross_pane_correlation;
#[cfg(feature = "asupersync-runtime")]
pub mod cx;
pub mod degradation;
pub mod desktop_notify;
pub mod diagnostic;
pub mod differential_snapshot;
pub mod docs_gen;
pub mod drift;
pub mod dry_run;
pub mod email_notify;
pub mod entropy_accounting;
pub mod environment;
pub mod error;
pub mod error_clustering;
pub mod error_codes;
pub mod event_templates;
pub mod events;
pub mod explanations;
pub mod export;
pub mod extensions;
#[cfg(unix)]
pub mod fd_budget;
pub mod incident_bundle;
pub mod ingest;
#[cfg(unix)]
pub mod ipc;
pub mod kalman_watchdog;
pub mod learn;
pub mod lock;
pub mod logging;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod memory_budget;
pub mod memory_pressure;
#[cfg(feature = "metrics")]
pub mod metrics;
pub mod notifications;
pub mod orphan_reaper;
#[cfg(any(feature = "web", feature = "sync", feature = "asupersync-runtime"))]
pub mod outcome;
pub mod output;
pub mod pane_lifecycle;
pub mod pane_tiers;
pub mod patterns;
pub mod plan;
pub mod policy;
pub mod pool;
pub mod priority;
pub mod process_tree;
pub mod process_triage;
pub mod protocol_recovery;
pub mod recording;
pub mod replay;
pub mod reservoir_sampler;
pub mod reports;
pub mod restore_layout;
pub mod restore_process;
pub mod restore_scrollback;
pub mod retry;
pub mod robot_types;
pub mod rulesets;
pub mod runtime;
pub mod runtime_compat;
pub mod screen_state;
pub mod scrollback_eviction;
pub mod search_explain;
pub mod secrets;
pub mod session_correlation;
pub mod session_dna;
pub mod session_pane_state;
pub mod session_restore;
pub mod session_retention;
pub mod session_topology;
pub mod setup;
pub mod sharded_counter;
pub mod snapshot_engine;
pub mod spectral;
pub mod storage;
pub mod storage_targets;
pub mod stream_hash;
pub mod suggestions;
pub mod survival;
pub mod tailer;
pub mod telemetry;
pub mod token_bucket;
pub mod undo;
pub mod user_preferences;
pub mod voi;
pub mod wait;
pub mod watcher_client;
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
// the FT_TUI_BACKEND environment variable (see docs/ftui-rollout-strategy.md).
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
