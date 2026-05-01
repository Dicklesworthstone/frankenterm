//! Shared test fixtures for `frankenterm-core`.
//!
//! **Bead:** [BR-RC-RUNTIME-SEMANTICS.G14.0] / `ft-t9a6q.3`.
//! **Doc:** `docs/runtime/labruntime-conventions.md`.
//! **Doctrine:** `runtime-proof-trait.md`.
//!
//! # Modules
//!
//! - [`lab_runtime`] — wraps `asupersync::LabRuntime` in an
//!   ergonomic single-call API for deterministic, virtual-time
//!   tests. Replaces the ~30-line boilerplate currently inlined
//!   into per-test sites (cpu_pressure.rs, native_events.rs,
//!   telemetry.rs, etc.).
//!
//! Test fixtures are exposed unconditionally rather than behind a
//! `#[cfg(test)]` because integration tests under
//! `crates/frankenterm-core/tests/` need to reach in. The
//! asupersync dependency they pull in is already a regular dep
//! of frankenterm-core, so there is no extra cost.

pub mod lab_runtime;
