//! `frankenterm-core-replay` — 24-module replay sub-crate extracted from
//! `frankenterm-core` (ft-y0loj.4 / ft-j1qjt.2).
//!
//! See `Cargo.toml` for the precise scope. Only modules with **zero
//! inbound references from non-replay core** were moved; the three
//! bridge modules (`replay`, `replay_capture`, `replay_fixture_harvest`)
//! stay in core because policy.rs / runtime.rs / workflows / recorder_replay
//! reach into them. Their full extraction is filed as ft-j1qjt.2.x and
//! requires `event_id` / `ingest::CapturedSegment` / `recording`
//! extracted to leaves first.

pub mod replay_artifact_registry;
pub mod replay_checkpoint;
pub mod replay_ci_gate;
pub mod replay_cli;
pub mod replay_counterfactual;
pub mod replay_decision_diff;
pub mod replay_fault_injection;
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
