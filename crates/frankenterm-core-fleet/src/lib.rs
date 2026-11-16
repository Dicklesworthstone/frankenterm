//! `frankenterm-core-fleet` — fleet orchestration sub-crate (ft-y0loj.3).
//!
//! Modules currently extracted:
//! - [`fleet_dashboard`] — alert evaluation + dedup
//!
//! `fleet_launcher`, `fleet_memory_controller`, and
//! `fleet_scrollback_coordinator` remain in `frankenterm-core` because
//! six in-tree importers (runtime, unified_telemetry, tx_execution,
//! mission_agent_mail, chaos_scale_harness, ntm_decommission)
//! consume their types substantively. Moving them out before
//! `frankenterm-core-memory` (backpressure / memory_budget /
//! memory_pressure) extracts as a shared types crate creates a cargo
//! cycle. Tracked in ft-usvnt under the ft-y0loj parent.
//!
//! Depends on `frankenterm-core` for `unified_telemetry`.

pub mod fleet_dashboard;
