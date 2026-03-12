//! frankenterm-core: Core library for FrankenTerm
//!
//! This crate provides the core functionality for `ft`, a swarm-native terminal
//! platform for AI agent fleets.
//!
//! # Architecture
//!
//! ```text
//! Backend Adapters → Ingest Pipeline → Storage (SQLite/FTS5)
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
//! - `search`: 2-tier semantic search (embedding + lexical + fusion)
//!
//! # Safety
//!
//! This crate forbids unsafe code.

#![forbid(unsafe_code)]
#![feature(stmt_expr_attributes)]

pub mod accounts;
pub mod adaptive_radix_tree;
pub mod aegis_backpressure;
pub mod aegis_diagnostics;
pub mod aegis_entropy_anomaly;
pub mod agent_config_templates;
pub mod agent_correlator;
#[cfg(feature = "agent-detection")]
pub mod agent_detection;
#[cfg(feature = "agent-mail")]
pub mod agent_mail_bridge;
pub mod agent_pane_state;
pub mod agent_provider;
pub mod alerts;
pub mod api_schema;
pub mod approval;
pub mod ars_blast_radius;
pub mod ars_compile;
pub mod ars_drift;
pub mod ars_evidence;
pub mod ars_evolve;
pub mod ars_explain;
pub mod ars_federation;
pub mod ars_fst;
pub mod ars_generalize;
pub mod ars_intercept;
pub mod ars_replay;
pub mod ars_secret_scan;
pub mod ars_serialize;
pub mod ars_symbolic_exec;
pub mod ars_timeout;
pub mod asupersync_observability;
pub mod auto_tune;
pub mod backpressure;
pub mod backpressure_severity;
pub mod backup;
pub mod bayesian_ledger;
#[cfg(feature = "subprocess-bridge")]
pub mod beads_bridge;
#[cfg(feature = "subprocess-bridge")]
pub mod beads_types;
pub mod bimap;
pub mod binomial_heap;
pub mod bloom_filter;
pub mod bocpd;
pub mod build_coord;
pub mod byte_compression;
#[cfg(feature = "subprocess-bridge")]
pub mod canary_rollout_controller;
pub mod canary_rehearsal;
pub mod cancellation;
pub mod cancellation_safe_channel;
pub mod capacity_governor;
#[cfg(feature = "session-resume")]
pub mod casr_types;
pub mod cass;
pub mod causal_dag;
pub mod caut;
pub mod chaos;
pub mod chaos_scale_harness;
pub mod circuit_breaker;
pub mod cleanup;
#[cfg(feature = "subprocess-bridge")]
pub mod code_scanner;
pub mod command_guard;
pub mod command_transport;
pub mod compact_bitset;
pub mod completion_token;
pub mod concurrent_map;
pub mod config;
pub mod config_profiles;
pub mod conformal;
pub mod connector_bundles;
pub mod connector_credential_broker;
pub mod connector_data_classification;
pub mod connector_event_model;
pub mod connector_governor;
pub mod connector_host_runtime;
pub mod connector_inbound_bridge;
pub mod connector_lifecycle;
pub mod connector_mesh;
pub mod connector_outbound_bridge;
pub mod connector_registry;
pub mod connector_reliability;
pub mod connector_sdk;
pub mod connector_testbed;
pub mod consistent_hash;
pub mod content_dedup;
pub mod context_budget;
pub mod context_snapshot;
pub mod continuous_backpressure;
pub mod cooldown_tracker;
pub mod cost_tracker;
pub mod count_min_sketch;
pub mod cpu_pressure;
pub mod crash;
pub mod crash_persistence_gate;
pub mod crdt;
pub mod cross_crate_integration;
pub mod cross_pane_correlation;
pub mod cuckoo_filter;
pub mod cutover_evidence;
pub mod cutover_playbook;
#[cfg(feature = "asupersync-runtime")]
pub mod cx;
pub mod dancing_links;
pub mod dashboard;
pub mod dataflow;
pub mod degradation;
pub mod dependency_eradication;
pub mod forbidden_dep_guards;
pub mod desktop_notify;
pub mod diagnostic;
pub mod diagnostic_redaction;
pub mod disaster_recovery_drills;
pub mod diagram_render;
pub mod differential_snapshot;
pub mod disjoint_intervals;
#[cfg(feature = "disk-pressure")]
pub mod disk_ballast;
#[cfg(feature = "disk-pressure")]
pub mod disk_pressure;
#[cfg(feature = "disk-pressure")]
pub mod disk_scoring;
pub mod docs_gen;
pub mod drift;
pub mod dry_run;
pub mod dual_run_shadow_comparator;
pub mod durable_state;
pub mod edit_distance;
pub mod email_notify;
pub mod entropy_accounting;
pub mod entropy_scheduler;
pub mod environment;
pub mod error;
pub mod error_clustering;
pub mod error_codes;
pub mod event_id;
pub mod event_templates;
pub mod event_stream;
pub mod events;
pub mod ewma;
pub mod exp_histogram;
pub mod explainability_console;
pub mod explanations;
pub mod export;
pub mod extensions;
#[cfg(unix)]
pub mod fd_budget;
pub mod fenwick_tree;
pub mod fibonacci_heap;
pub mod fleet_dashboard;
pub mod fleet_launcher;
pub mod forensic_export;
pub mod gc;
pub mod graph_scoring;
pub mod headless_mux_server;
pub mod hyperloglog;
pub mod identity_graph;
pub mod incident_bundle;
pub mod ingest;
pub mod input_reserve;
pub mod intervention_console;
pub mod interval_tree;
#[cfg(unix)]
pub mod ipc;
pub mod kalman_watchdog;
pub mod kd_tree;
pub mod latency_model;
pub mod latency_stages;
pub mod learn;
pub mod lfu_cache;
pub mod lock;
pub mod lock_orchestration;
pub mod logging;
pub mod lru_cache;
pub mod manifest_dep_eradication;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "mcp-client")]
pub mod mcp_client;
#[cfg(feature = "mcp")]
pub mod mcp_error;
#[cfg(any(feature = "mcp", feature = "mcp-client"))]
#[doc(hidden)]
pub mod mcp_framework;
pub mod mdl_extraction;
pub mod memory_budget;
pub mod memory_pressure;
pub mod merkle_tree;
pub mod migration_artifact_contracts;
pub mod migration_rehearsal;
#[cfg(feature = "metrics")]
pub mod metrics;
#[cfg(feature = "subprocess-bridge")]
pub mod mission_agent_mail;
#[cfg(feature = "subprocess-bridge")]
pub mod mission_events;
#[cfg(feature = "subprocess-bridge")]
pub mod mission_loop;
pub mod namespace_isolation;
pub mod network_observer;
pub mod network_reliability;
pub mod notifications;
pub mod ntm_decommission;
pub mod ntm_importer;
pub mod ntm_parity;
pub mod operator_runbooks;
pub mod orphan_reaper;
#[cfg(any(feature = "web", feature = "sync", feature = "asupersync-runtime"))]
pub mod outcome;
pub mod output;
pub mod output_compression;
pub mod pairing_heap;
pub mod pane_lifecycle;
pub mod pane_tiers;
pub mod pane_typestate;
pub mod pattern_trigger;
pub mod patterns;
pub mod persistent_ds;
pub mod plan;
#[cfg(feature = "subprocess-bridge")]
pub mod planner_features;
pub mod policy;
pub mod policy_audit_chain;
pub mod policy_compliance;
pub mod policy_decision_log;
pub mod policy_diagnostics;
pub mod policy_dsl;
pub mod policy_metrics;
pub mod policy_quarantine;
pub mod pool;
pub mod priority;
pub mod process_tree;
pub mod process_triage;
pub mod protocol_recovery;
pub mod quantile_sketch;
pub mod query_contract;
pub mod quota_gate;
pub mod r_tree;
pub mod rate_limit_tracker;
pub mod recorder_audit;
pub mod recorder_export;
pub mod recorder_invariants;
pub mod recorder_migration;
pub mod recorder_query;
pub mod recorder_replay;
pub mod recorder_retention;
pub mod recorder_storage;
pub mod recording;
pub mod replay;
pub mod replay_artifact_registry;
pub mod replay_capture;
pub mod replay_checkpoint;
pub mod replay_ci_gate;
pub mod replay_cli;
pub mod replay_counterfactual;
pub mod replay_decision_diff;
pub mod replay_decision_graph;
pub mod replay_fault_injection;
pub mod replay_fixture_harvest;
pub mod replay_guardrails;
pub mod replay_guardrails_gate;
pub mod replay_guide;
pub mod replay_mcp;
pub mod replay_merge;
pub mod replay_performance;
pub mod replay_post_incident;
pub mod replay_provenance;
pub mod replay_remediation;
pub mod replay_report;
pub mod replay_risk_scoring;
pub mod replay_robot;
pub mod replay_scenario_matrix;
pub mod replay_shadow_rollout;
pub mod replay_side_effect_barrier;
pub mod replay_test_orchestrator;
pub mod replay_usability_pilot;
pub mod reports;
pub mod repro_dedup_bug;
pub mod reservoir_sampler;
pub mod resize_crash_forensics;
pub mod resize_invariants;
pub mod resize_memory_controls;
pub mod resize_scheduler;
pub mod restore_layout;
pub mod restore_process;
pub mod restore_scrollback;
pub mod retry;
pub mod ring_buffer;
pub mod robot_api_contracts;
#[cfg(feature = "vc-export")]
pub mod robot_envelope;
pub mod robot_sdk_contracts;
pub mod robot_idempotency;
pub mod robot_types;
pub mod rope;
pub mod rulesets;
pub mod runtime;
pub mod runtime_compat;
pub mod runtime_compat_surface_guard;
pub mod runtime_diagnostics_ux;
pub mod runtime_health;
pub mod runtime_performance_contract;
pub mod runtime_slo_gates;
pub mod runtime_telemetry;
pub mod safe_channel;
pub mod scan_pipeline;
pub mod scope_tree;
pub mod scope_watchdog;
pub mod screen_state;
pub mod scrollback_eviction;
pub mod search;
#[cfg(feature = "frankensearch")]
pub mod search_bridge;
pub mod search_explain;
pub mod secrets;
pub mod segment_tree;
pub mod self_stabilize;
pub mod semantic_anomaly;
pub mod semantic_anomaly_watchdog;
pub mod semantic_quality;
pub mod semantic_shock_response;
pub mod sequence_model;
pub mod session_correlation;
pub mod session_dna;
pub mod session_pane_state;
pub mod session_profiles;
pub mod session_restore;
pub mod session_workflow_explorer;
#[cfg(feature = "session-resume")]
pub mod session_resume;
pub mod session_retention;
#[cfg(feature = "redis-session")]
pub mod session_store;
pub mod session_topology;
pub mod setup;
#[cfg(feature = "subprocess-bridge")]
pub mod shadow_mode_evaluator;
pub mod sharded_counter;
pub mod sharding;
pub mod shortest_path;
pub mod simd_scan;
pub mod skip_list;
pub mod sliding_window;
pub mod soak_confidence_gate;
pub mod slo_conformance;
pub mod snapshot_engine;
pub mod sparse_table;
pub mod spectral;
pub mod splay_tree;
pub mod spsc_ring_buffer;
pub mod storage;
pub mod storage_targets;
pub mod storage_telemetry;
pub mod stream_hash;
#[cfg(feature = "subprocess-bridge")]
pub mod subprocess_bridge;
pub mod suffix_array;
pub mod suggestions;
pub mod survival;
pub mod swarm_pipeline;
pub mod swarm_scheduler;
pub mod swarm_command_center;
pub mod swarm_work_queue;
pub mod tailer;
#[cfg(feature = "recorder-lexical")]
pub mod tantivy_ingest;
#[cfg(feature = "recorder-lexical")]
pub mod tantivy_policy;
#[cfg(feature = "recorder-lexical")]
pub mod tantivy_quality;
#[cfg(feature = "recorder-lexical")]
pub mod tantivy_query;
#[cfg(feature = "recorder-lexical")]
pub mod tantivy_reindex;
pub mod telemetry;
pub mod test_artifacts;
pub mod time_series;
pub mod token_bucket;
pub mod topological_sort;
pub mod topology_orchestration;
pub mod traceability_verification;
pub mod trauma_guard;
pub mod treap;
pub mod trie;
#[cfg(feature = "subprocess-bridge")]
pub mod tx_idempotency;
#[cfg(feature = "subprocess-bridge")]
pub mod tx_observability;
#[cfg(feature = "subprocess-bridge")]
pub mod tx_plan_compiler;
pub mod undo;
pub mod unified_telemetry;
pub mod union_find;
pub mod user_preferences;
pub mod utf8_chunked;
pub mod ux_scenario_validation;
pub mod van_emde_boas;
#[cfg(feature = "vc-export")]
pub mod vc_export;
pub mod viewport_reflow_planner;
pub mod voi;
pub mod wait;
pub mod wal_engine;
pub mod watchdog;
pub mod watcher_client;
pub mod wavelet_tree;
pub mod webhook;
pub mod wezterm;
pub mod work_stealing_deque;
pub mod workflows;
pub mod xor_filter;

pub mod vendored_async_contracts;
#[cfg(feature = "vendored")]
pub mod vendored;
#[cfg(feature = "vendored")]
pub mod vendored_migration_map;

#[cfg(feature = "vendored")]
pub mod wezterm_native;

#[cfg(feature = "native-wezterm")]
pub mod native_events;

#[cfg(feature = "browser")]
pub mod browser;

#[cfg(feature = "recorder-lexical")]
pub mod recorder_lexical_ingest;
#[cfg(feature = "recorder-lexical")]
pub mod recorder_lexical_schema;

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
     Use `--features ftui` for the FrankenTUI backend, not both. \
     Use `--features rollout` for runtime backend selection during migration."
);

#[cfg(any(feature = "tui", feature = "ftui"))]
pub mod tui;

#[cfg(feature = "web")]
pub mod web;
#[cfg(feature = "web")]
pub mod web_framework;

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
