//! `frankenterm-core-config-types` — operator-tunable config types leaf crate (ft-otfxs / ft-t2d70.2).
//!
//! Tier-1 leaf crate carved out from `frankenterm-core`. Holds the `tuning_config`
//! module (1384 LOC) — the operator-tunable constants that get loaded from the
//! `[tuning]` section of `ft.toml`. Pure type definitions + `validate()` impls,
//! zero `crate::*` deps.
//!
//! Re-exported from `frankenterm-core` as `frankenterm_core::tuning_config::*`
//! so existing call sites resolve unchanged.
//!
//! ## What's NOT here
//!
//! `config.rs` (7293 LOC, 135 cross-cluster refs to ~30 modules including
//! connector_*, mcp_*, patterns, events, webhook, etc.) is too entangled to
//! extract as a pure leaf — it would pull half of `frankenterm-core` with it.
//! `config_profiles.rs` (858 LOC) calls back into `crate::config::resolve_*`
//! and `crate::error::ConfigError`, blocking direct extraction. Both are
//! tracked under ft-t2d70 follow-ups.

pub mod tuning_config;
