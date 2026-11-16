//! `frankenterm-core-telemetry-types` — telemetry primitives + snapshot types leaf crate (ft-yf2am / ft-y0loj.3.2).
//!
//! Tier-1 leaf crate carved out from `frankenterm-core`. Holds five leaf-clean
//! modules (4852 LOC total) — telemetry building blocks (probabilistic data
//! structures, EMAs, snapshot composition) with zero `crate::*` deps:
//!
//! - [`ewma`] — exponentially-weighted moving average (sliding rate estimators).
//! - [`exp_histogram`] — exponential histogram (bounded-error percentile sketches).
//! - [`count_min_sketch`] — count-min sketch (sub-linear frequency estimation).
//! - [`hyperloglog`] — HyperLogLog (cardinality estimation).
//! - [`context_snapshot`] — context capture + serializable snapshot envelope.
//!
//! Re-exported from `frankenterm-core` so existing `crate::ewma::*`,
//! `crate::context_snapshot::*`, etc. paths keep resolving unchanged.
//!
//! ## What's NOT here (and why)
//!
//! The bead's original intent was to extract `unified_telemetry.rs` and split
//! `memory_budget`/`memory_pressure` types from their managers. Survey found:
//!
//! - `unified_telemetry` (949 LOC) is a **composer**, not a leaf — it pulls
//!   snapshot types from 9 subsystems (policy, swarm_scheduler,
//!   swarm_work_queue, vendored::mux_pool, telemetry, storage_telemetry,
//!   tailer, fleet_memory_controller, fleet_scrollback_coordinator) and
//!   bundles them into a unified envelope. Cannot move out as-is.
//! - `memory_budget` + `memory_pressure` Manager types call
//!   `crate::cx::Cx::current()`, `crate::runtime_async::sleep_with_cx`, and
//!   `crate::outcome::CancelKind` — same blocker noted in ft-usvnt. Splitting
//!   the types from the managers (orphan-rule constraint on inherent impls)
//!   needs its own discovery pass.
//! - `metrics.rs` (1557 LOC) depends on `cx`/`events`/`outcome`/`runtime` —
//!   not leaf-clean.
//! - `telemetry.rs` + `storage_telemetry.rs` + `runtime_telemetry.rs` all
//!   depend on `cx`/`runtime_async` — not leaf-clean.
//!
//! What IS leaf-clean and ships here is the *primitives* layer that
//! everything above is built on.

pub mod context_snapshot;
pub mod count_min_sketch;
pub mod ewma;
pub mod exp_histogram;
pub mod hyperloglog;
