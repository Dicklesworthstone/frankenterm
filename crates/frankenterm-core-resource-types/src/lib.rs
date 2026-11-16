//! `frankenterm-core-resource-types` — shared resource primitives (ft-usvnt / ft-y0loj.3.1).
//!
//! Tier-1 leaf crate sitting **below** `frankenterm-core`. Holds the load-bearing
//! resource primitives consumed by *both* `frankenterm-core` and
//! `frankenterm-core-fleet`.
//!
//! Extracting these here breaks the symmetry that previously forced
//! `frankenterm-core-fleet` to depend on `frankenterm-core` *just* to read a
//! `BackpressureTier` enum, which in turn made `frankenterm-core →
//! frankenterm-core-fleet` a cargo cycle (see
//! `docs/proposals/ft-l3tfo-cold-build-measurements.md`).
//!
//! Modules:
//! - [`backpressure`] — capture/storage queue-depth FSM (Green / Yellow / Red / Black).
//! - [`backpressure_severity`] — continuous-severity throttling function (sigmoid).
//! - [`resource_admission`] — high-core resource admission and placement DTOs/planner.
//!
//! ## Modules NOT extracted in this pass (and why)
//!
//! `memory_budget` and `memory_pressure` were initially staged for inclusion but
//! reverted: their `Manager` types call `crate::cx::Cx::current()`,
//! `crate::runtime_async::sleep_with_cx`, and `crate::outcome::CancelKind` —
//! all of which live in higher-tier `frankenterm-core` modules. Promoting the
//! whole files here would require either (a) extracting `cx`/`runtime_async`/
//! `outcome` first, or (b) splitting each `.rs` file into a leaf "tier types"
//! shard plus a core-resident "manager" shard. Both are valid follow-ups and
//! tracked under ft-y0loj.3.1.next.
//!
//! Anything pulled into this crate that needs `crate::cx`, `crate::runtime_async`,
//! or other higher-tier modules must stay in `frankenterm-core` for now and
//! grow a thin trait at this layer instead.

pub mod backpressure;
pub mod backpressure_severity;
pub mod resource_admission;
