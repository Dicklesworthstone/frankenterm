//! Runtime-async surface guard — minimal post-migration form.
//!
//! Historically (ft-e34d9.10.5.4.1) this module was 886 LOC of test
//! scaffolding mirroring `SurfaceDisposition` / `SurfaceContractEntry` /
//! `SURFACE_CONTRACT_V1` from `runtime_async`, plus a `SurfaceGuardReport`
//! dashboard with serialisable surface entries, per-API guard checks,
//! regression-type catalogues, and call-site sweeps. The intent was to
//! catch silent surface drift during the Tokio→asupersync migration.
//!
//! That migration is over (ft-xbnl0.2.5: asupersync is the sole runtime),
//! and the contract types it mirrored were retired in ft-yqd3w. The
//! scaffolding had no external consumers — every public type was
//! exercised only by its own `#[cfg(test)] mod tests`.
//!
//! What survives in this module is the single load-bearing fact the
//! 886-line guard was protecting: the **list of source files** allowed to
//! reach for raw async-runtime primitives. Future drift checks (clippy
//! lint, grep CI gate, `forbidden_dep_guards.rs` integration) consult
//! `allowed_raw_runtime_files()` and `allowed_raw_tokio_runtime_builder_apis()`.
//!
//! The richer "guard report" object model is gone (ft-3hi74 folded into
//! ft-yqd3w). If a future audit needs a comparable structure, build a
//! tighter version rather than resurrecting the migration-era ledger.

/// Returns the list of source files where raw runtime imports are permitted.
///
/// All other files must use `runtime_async` wrappers + `cx` capability helpers.
#[must_use]
pub fn allowed_raw_runtime_files() -> Vec<&'static str> {
    vec!["runtime_async.rs", "cx.rs"]
}

/// Returns the only raw tokio runtime-builder constructors still permitted by
/// the migration contract. The list is intentionally empty post
/// ft-xbnl0.2.5: asupersync is now the sole runtime, and the historical
/// quarantine inventory was zeroed out at the same time. This accessor is
/// kept so future audits / lints have a stable API to consult.
#[must_use]
pub fn allowed_raw_tokio_runtime_builder_apis() -> &'static [&'static str] {
    crate::runtime_async::RAW_TOKIO_RUNTIME_BUILDER_QUARANTINE_V1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_raw_runtime_files_contains_only_runtime_async_and_cx() {
        // Pin the surface: only the two files that vend the project's
        // canonical async wrappers are allowed to import raw runtime
        // primitives. Adding a new file to this list is a deliberate
        // architecture decision and should land in its own commit
        // alongside the audit explaining why.
        let allowed = allowed_raw_runtime_files();
        assert_eq!(allowed.len(), 2);
        assert!(allowed.contains(&"runtime_async.rs"));
        assert!(allowed.contains(&"cx.rs"));
    }

    #[test]
    fn allowed_raw_tokio_runtime_builder_apis_is_empty_post_migration() {
        // ft-xbnl0.2.5 zeroed the tokio runtime-builder quarantine.
        // This guards against a regression where a tokio builder slips
        // back into the canonical surface.
        assert!(
            allowed_raw_tokio_runtime_builder_apis().is_empty(),
            "tokio runtime-builder quarantine is empty post ft-xbnl0.2.5; \
             a non-empty list means a tokio builder regressed back into \
             the runtime_async surface"
        );
    }
}
