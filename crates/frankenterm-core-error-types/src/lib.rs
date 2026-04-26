//! `frankenterm-core-error-types` — error code catalog leaf crate (ft-g6sa8 / ft-t2d70.1).
//!
//! Tier-1 leaf crate carved out from `frankenterm-core`. Holds the WA-XXXX
//! error code catalog (`error_codes` module) — pure type definitions +
//! lookup tables, no first-party deps.
//!
//! Re-exported from `frankenterm-core` as `frankenterm_core::error_codes::*`
//! so existing call sites resolve unchanged.

pub mod error_codes;
