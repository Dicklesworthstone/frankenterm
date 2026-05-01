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
//! - `redactor`: Secret redaction for read, export, and audit surfaces
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
// ft-y0loj.2 / ft-mr35k: ars_* modules (15 total — ars_blast_radius,
// ars_compile, ars_drift, ars_evidence, ars_evolve, ars_explain,
// ars_federation, ars_fst, ars_generalize, ars_intercept, ars_replay,
// ars_secret_scan, ars_serialize, ars_symbolic_exec, ars_timeout) were
// extracted into the `frankenterm-core-ars` sub-crate. Consumers depend
// on that crate directly. No re-export here (the new sub-crate depends
// on frankenterm-core for `mdl_extraction` / `token_bucket` /
// `workflows::*` types — re-exporting would create a regular dep
// cycle). Tests in this crate's tests/ dir resolve via a dev-dep
// cycle, which Cargo allows.
pub mod asupersync_observability;
pub mod auto_tune;
// `backpressure` + `backpressure_severity` extracted to
// `frankenterm-core-resource-types` (ft-usvnt / ft-y0loj.3.1) so that
// `frankenterm-core-fleet` can read `BackpressureTier` without a `core → fleet`
// edge. Re-export here so existing `crate::backpressure::*` and
// `frankenterm_core::backpressure::*` paths keep resolving unchanged.
pub use frankenterm_core_resource_types::{backpressure, backpressure_severity};
// ft-yf2am / ft-y0loj.3.2: 5 leaf-clean telemetry primitives
// (context_snapshot, count_min_sketch, ewma, exp_histogram, hyperloglog)
// extracted to `frankenterm-core-telemetry-types`. Re-exported so existing
// `crate::ewma::*`, `crate::context_snapshot::*`, etc. paths keep resolving.
pub use frankenterm_core_telemetry_types::{
    context_snapshot, count_min_sketch, ewma, exp_histogram, hyperloglog,
};
pub mod backup;
pub mod bayesian_ledger;
#[cfg(feature = "subprocess-bridge")]
pub mod beads_bridge;
#[cfg(feature = "subprocess-bridge")]
pub mod beads_types;
pub mod bench_stats;
pub mod beta_feedback_loop;
pub mod bimap;
pub mod binomial_heap;
pub mod bloom_filter;
pub mod bocpd;
pub mod build_coord;
pub mod byte_compression;
// `canary_rehearsal` extracted to `frankenterm-core-audit-types`
// (ft-mq7fl / ft-8nqx0 Phase 3). Leaf-clean per the boundary scan.
pub use frankenterm_core_audit_types::canary_rehearsal;
#[cfg(feature = "subprocess-bridge")]
pub mod canary_rollout_controller;
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
// `context_snapshot` extracted to `frankenterm-core-telemetry-types` (ft-yf2am / ft-y0loj.3.2).
pub mod continuous_backpressure;
pub mod cooldown_tracker;
pub mod cost_tracker;
// `count_min_sketch` extracted to `frankenterm-core-telemetry-types`.
pub mod cpu_pressure;
pub mod crash;
pub mod crash_persistence_gate;
pub mod crdt;
pub mod cross_crate_integration;
pub mod cross_pane_correlation;
pub mod cuckoo_filter;
// `cutover_evidence` extracted to `frankenterm-core-audit-types`
// (ft-mq7fl / ft-8nqx0 Phase 3). Leaf-clean per the boundary scan.
pub use frankenterm_core_audit_types::cutover_evidence;
pub mod cutover_playbook;
pub mod cx;
pub mod dancing_links;
pub mod dashboard;
pub mod dataflow;
pub mod degradation;
pub mod dependency_eradication;
pub mod desktop_notify;
pub mod diagnostic;
pub mod diagnostic_redaction;
pub mod diagram_render;
pub mod differential_snapshot;
pub mod disaster_recovery_drills;
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
// `error_codes` extracted to `frankenterm-core-error-types` (ft-g6sa8 / ft-t2d70.1).
// Re-export so `crate::error_codes::*` and `frankenterm_core::error_codes::*` paths
// keep resolving unchanged. Leaf-clean: zero `crate::*` deps.
pub use frankenterm_core_error_types::error_codes;
pub mod event_id;
pub mod event_stream;
pub mod event_templates;
pub mod events;
// `ewma` extracted to `frankenterm-core-telemetry-types`.
// `exp_histogram` extracted to `frankenterm-core-telemetry-types`.
pub mod explainability_console;
pub mod explanations;
pub mod export;
pub mod extensions;
#[cfg(unix)]
pub mod fd_budget;
pub mod fenwick_tree;
pub mod fibonacci_heap;
// ft-y0loj.3: PARTIAL extraction. `fleet_dashboard` (the only fleet
// module with zero in-tree importers) moved to the new
// `frankenterm-core-fleet` sub-crate. The other three (`fleet_launcher`,
// `fleet_memory_controller`, `fleet_scrollback_coordinator`) stay in
// frankenterm-core because six production importers (runtime,
// unified_telemetry, tx_execution, mission_agent_mail,
// chaos_scale_harness, ntm_decommission) consume their types
// substantively, and the new sub-crate's reverse dep on backpressure /
// memory_budget / memory_pressure / etc. would create a cargo cycle if
// core depended back on it. Full extraction blocks on
// frankenterm-core-memory landing first (filed as ft-usvnt under
// ft-y0loj parent).
pub mod fleet_launcher;
pub mod fleet_memory_controller;
pub mod fleet_scrollback_coordinator;
pub mod forbidden_dep_guards;
// `forensic_export` extracted to `frankenterm-core-audit-types` (ft-rqu5e /
// ft-8nqx0 Phase 1). Leaf-clean per the boundary scan (zero `crate::*` deps,
// only `std` + `serde`). Re-exported so `crate::forensic_export::*` and
// `frankenterm_core::forensic_export::*` paths continue to resolve unchanged.
pub use frankenterm_core_audit_types::forensic_export;
pub mod gc;
pub mod graph_scoring;
pub mod headless_mux_server;
// `hyperloglog` extracted to `frankenterm-core-telemetry-types`.
pub mod identity_graph;
pub mod incident_bundle;
pub mod ingest;
pub mod input_latency;
pub mod input_reserve;
pub mod interval_tree;
pub mod intervention_console;
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
// `mdl_extraction` extracted to `frankenterm-core-audit-types`
// (ft-nsoxc / ft-8nqx0 Phase 5). Leaf-clean. Re-exported so
// `crate::mdl_extraction::*` and `frankenterm_core::mdl_extraction::*`
// paths continue resolving for non-ARS consumers (workflows,
// proptests, etc.).
pub use frankenterm_core_audit_types::mdl_extraction;
pub mod memory_budget;
pub mod memory_pressure;
pub mod merkle_tree;
#[cfg(feature = "metrics")]
pub mod metrics;
pub mod migration_artifact_contracts;
// `migration_rehearsal` extracted to `frankenterm-core-audit-types`
// (ft-mq7fl / ft-8nqx0 Phase 3). Leaf-clean per the boundary scan.
pub use frankenterm_core_audit_types::migration_rehearsal;
#[cfg(feature = "subprocess-bridge")]
pub mod mission_agent_mail;
#[cfg(feature = "subprocess-bridge")]
pub mod mission_dispatch;
#[cfg(feature = "subprocess-bridge")]
pub mod mission_events;
#[cfg(feature = "subprocess-bridge")]
pub mod mission_loop;
pub mod mux_client;
pub mod namespace_isolation;
pub mod network_observer;
pub mod network_reliability;
pub mod notifications;
pub mod ntm_decommission;
pub mod ntm_importer;
pub mod ntm_parity;
pub mod operator_runbooks;
pub mod orphan_reaper;
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
// `policy_audit_chain`, `policy_compliance`, `policy_metrics`, `policy_quarantine`
// extracted to `frankenterm-core-policy-types` (ft-0pykm / ft-t2d70.3). All four
// are leaf-clean (zero `crate::*` deps). Re-export so existing
// `crate::policy_*::*` and `frankenterm_core::policy_*::*` paths keep resolving.
// `policy_decision_log`, `policy_diagnostics`, `policy_dsl` stay in core
// (cross-cluster deps to runtime + connector telemetry).
pub use frankenterm_core_policy_types::{
    policy_audit_chain, policy_compliance, policy_metrics, policy_quarantine,
};
pub mod policy_decision_log;
pub mod policy_diagnostics;
pub mod policy_dsl;
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
pub mod redactor;
pub mod release_readiness_gates;
// ft-y0loj.4 / ft-j1qjt: replay extraction ATTEMPTED but REVERTED — the
// replay cluster is not a tier-1 leaf. policy.rs / runtime.rs /
// workflows/runner.rs / workflows/mod.rs hold ~16 inline references to
// `crate::replay_capture::{SharedCaptureAdapter, DecisionEvent,
// DecisionType, CollectingCaptureSink, CaptureAdapter, CaptureConfig}`
// and recorder_replay.rs holds 1 ref to
// `crate::replay_fixture_harvest::FtreplayArtifact`. Extracting replay
// into its own crate creates a regular dep cycle (core needs the capture
// types; replay needs core for event_id/policy/etc.). Filed as
// follow-up: extract `replay_capture` types (and FtreplayArtifact) into
// a smaller shared leaf crate first, similar to the
// frankenterm-core-resource-types pattern (ft-usvnt). See
// docs/proposals/ft-j1qjt-replay-tier1-blocker.md for the audit.
// ft-j1qjt.2: 24 of 28 replay_* modules extracted to
// `frankenterm-core-replay`. Only the 3 bridge modules with non-replay
// core importers stay here:
//   - `replay`                 (Recording type, used by recording.rs:1490)
//   - `replay_capture`         (used by policy/runtime/workflows; needs
//                               event_id / ingest / recording leafified
//                               first — filed as ft-j1qjt.2.x)
//   - `replay_fixture_harvest` (used by recorder_replay.rs; same blocker)
// Plus the two already in `frankenterm-core-replay-types`:
//   - `replay_decision_graph`  (ft-j1qjt.1)
//   - `recorder_metadata`      (ft-j1qjt.3 — referenced as a `recording.rs` re-export)
//
// Tests in `crates/frankenterm-core/tests/proptest_replay*.rs` resolve
// the moved modules via a dev-dep cycle to `frankenterm-core-replay`,
// which Cargo allows.
pub mod replay;
pub mod replay_capture;
pub use frankenterm_core_replay_types::replay_decision_graph;
pub mod replay_fixture_harvest;
pub mod reports;
pub mod repro_dedup_bug;
pub mod reservoir_sampler;
pub mod resize_crash_forensics;
pub mod resize_invariants;
pub mod resize_memory_controls;
pub mod resize_scheduler;
pub mod restart_scheduler;
pub mod restore_layout;
pub mod restore_process;
pub mod restore_scrollback;
pub mod retry;
pub mod ring_buffer;
pub mod robot_api_contracts;
#[cfg(feature = "vc-export")]
pub mod robot_envelope;
pub mod robot_idempotency;
pub mod robot_ntm_surface;
pub mod robot_sdk_contracts;
pub mod robot_types;
pub mod rope;
pub mod rulesets;
pub mod runtime;
pub mod runtime_async;
pub mod runtime_async_surface_guard;
pub mod runtime_diagnostics_ux;
pub mod runtime_health;
pub mod runtime_performance_contract;
pub mod runtime_proof;
pub mod runtime_slo_gates;
pub mod runtime_telemetry;
pub mod safe_channel;
pub mod scan_pipeline;
pub mod scope_tree;
pub mod scope_watchdog;
pub mod screen_state;
pub mod scrollback_eviction;
pub mod scrollback_tiers;
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
#[cfg(feature = "session-resume")]
pub mod session_resume;
pub mod session_retention;
#[cfg(feature = "redis-session")]
pub mod session_store;
pub mod session_topology;
pub mod session_workflow_explorer;
pub mod setup;
#[cfg(feature = "subprocess-bridge")]
pub mod shadow_mode_evaluator;
pub mod sharded_counter;
pub mod sharding;
pub mod shortest_path;
pub mod simd_scan;
pub mod skip_list;
pub mod sliding_window;
pub mod slo_conformance;
pub mod snapshot_engine;
pub mod soak_confidence_gate;
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
pub mod swarm_command_center;
pub mod swarm_pipeline;
pub mod swarm_scheduler;
pub mod swarm_work_queue;
pub mod tailer;
// ft-y0loj.1: tantivy_* + recorder_lexical_* modules extracted into the
// `frankenterm-core-tantivy` sub-crate. The `recorder-lexical` feature
// gate that used to live here is now encoded by crate membership —
// consumers that need lexical search depend on the new sub-crate
// directly. No re-export here (would create a cargo cycle since the
// new sub-crate depends on frankenterm-core for `recorder_storage` /
// `recording` types).
pub mod telemetry;
pub mod test_artifacts;
pub mod time_series;
// `token_bucket` extracted to `frankenterm-core-audit-types`
// (ft-nsoxc / ft-8nqx0 Phase 5). Leaf-clean. Re-exported so
// `crate::token_bucket::*` paths continue resolving for non-ARS
// consumers.
pub use frankenterm_core_audit_types::token_bucket;
pub mod topological_sort;
pub mod topology_orchestration;
// `traceability_verification` extracted to `frankenterm-core-audit-types`
// (ft-rqu5e / ft-8nqx0 Phase 1). Leaf-clean per the boundary scan.
// Re-exported so `crate::traceability_verification::*` and
// `frankenterm_core::traceability_verification::*` paths continue to resolve.
pub use frankenterm_core_audit_types::traceability_verification;
pub mod trauma_guard;
pub mod treap;
pub mod trie;
// `tuning_config` extracted to `frankenterm-core-config-types` (ft-otfxs / ft-t2d70.2).
// Re-export so `crate::tuning_config::*` and `frankenterm_core::tuning_config::*` paths
// keep resolving unchanged. Leaf-clean: zero `crate::*` deps.
pub use frankenterm_core_config_types::tuning_config;
#[cfg(feature = "subprocess-bridge")]
pub mod tx_execution;
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

#[cfg(feature = "vendored")]
pub mod vendored;
pub mod vendored_async_contracts;
#[cfg(feature = "vendored")]
pub mod vendored_migration_map;

#[cfg(feature = "vendored")]
pub mod wezterm_native;

#[cfg(feature = "native-wezterm")]
pub mod native_events;

#[cfg(feature = "browser")]
pub mod browser;

// ft-y0loj.1: recorder_lexical_* modules moved to frankenterm-core-tantivy
// (they live with the rest of the lexical-search cluster).

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
