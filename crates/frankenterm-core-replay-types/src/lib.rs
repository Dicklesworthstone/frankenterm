//! `frankenterm-core-replay-types` — Leaf crate of replay-cluster types
//! that `frankenterm-core` depends on (ft-j1qjt.1).
//!
//! Carved out of `frankenterm-core` to break one of the two `core → replay`
//! back-edges identified in
//! `docs/proposals/ft-j1qjt-replay-tier1-blocker.md`. The
//! `DecisionType` / `DecisionEvent` types live here so the future
//! `frankenterm-core-replay` extraction (ft-j1qjt.2) does not need to
//! pull `replay_decision_graph` along with the rest of the cluster.
//!
//! ## Dependency direction
//!
//! Leaf — depends only on `serde` + `serde_json`. ZERO `frankenterm-core`
//! deps. Both `frankenterm-core` and the future
//! `frankenterm-core-replay` will depend on this crate; no cycle.
//!
//! ## What's NOT here
//!
//! The remaining `core → replay` edge — `replay_capture::CaptureAdapter`
//! and its inner `RecorderEventSource` field — is filed as a follow-up
//! bead (ft-j1qjt.1.1). That extraction needs `RecorderEventSource`
//! pulled out of `recording.rs` first, which is its own surgery.

pub mod recorder_metadata;
pub mod replay_decision_graph;
pub mod swarm_causal_event;
