//! `FrankenSQLiteBackend` stub (br-ft-kcdqp).
//!
//! Compile-time scaffold for the storage-backend trait
//! implementation that ships once the upstream `frankensqlite`
//! project releases Phase 5+. The bead's external precondition
//! (frankensqlite Phase 5+) is monitored by
//! [`scripts/check_frankensqlite_readiness.py`][readiness]; the
//! stub here lets callers compile against `--features
//! frankensqlite-backend` today + verify the dispatch wiring +
//! exercise the fallback path before any real frankensqlite
//! method bodies land.
//!
//! [readiness]: ../../scripts/check_frankensqlite_readiness.py
//!
//! ## Why a stub
//!
//! The bead's Acceptance includes "Behind --features frankensqlite-
//! backend feature flag (default off)." Shipping the feature flag
//! + a backend type that compiles + the trait impl skeleton means:
//!
//! 1. Workspace `Cargo.toml`'s feature-flag matrix stays unbroken
//!    (PR CI flows like `--all-features` succeed).
//! 2. Downstream code that picks the backend via a `--backend`
//!    flag or feature gate can write against the real type today,
//!    swapping `Box<dyn StorageBackend>` constructors atomically
//!    when Phase 5+ lands.
//! 3. The stub's [`NOT_WIRED_HINT`] gives end users a single,
//!    discoverable error message + remediation pointer when they
//!    flip the feature on prematurely.
//!
//! Every operation returns `BackendError::Other(NOT_WIRED_HINT)`
//! except [`StorageBackend::backend_name`] (which returns
//! `"frankensqlite"` for telemetry continuity).
//!
//! ## What the wired-pass replaces
//!
//! When Phase 5+ ships, the wired-pass slice:
//!
//! 1. Adds `frankensqlite` as a `[dependencies]` entry behind
//!    `optional = true` + the `frankensqlite-backend` feature.
//! 2. Replaces every `Err(BackendError::Other(NOT_WIRED_HINT))`
//!    body with the real connection / query / transaction call.
//! 3. Adds a `StorageBackendFactory` impl returning a
//!    `FrankenSQLiteBackend` from a path + `OpenConfig`.
//! 4. Pushes a `frankensqlite_*` row into
//!    `crates/frankenterm-core/benches/storage_backend_comparison.rs`'s
//!    `backends_under_test()` (cc_1's harness at 33332b2be picks
//!    this up automatically).
//!
//! No callers of `Box<dyn StorageBackend>` need to change.

#![cfg(feature = "frankensqlite-backend")]

use crate::storage_backend_trait::{
    BackendError, OpenConfig, StorageBackend, ToSqlValue, TransactionGuard,
};

/// Stable diagnostic hint for every stubbed operation. Carries
/// the bead reference + the readiness checker so an operator
/// who hits this error knows the next step.
pub const NOT_WIRED_HINT: &str =
    "FrankenSQLiteBackend is a stub awaiting frankensqlite Phase 5+ release \
     (ft-kcdqp). Run scripts/check_frankensqlite_readiness.py to verify \
     upstream status; once it reports `ready`, the wired-pass slice fills in \
     the method bodies.";

/// Stable backend identifier for telemetry + diagnostic surfaces.
/// Returned by [`StorageBackend::backend_name`] so the existing
/// doctor / bench / aggregator pipelines see the right label
/// even on the stub.
pub const BACKEND_NAME: &str = "frankensqlite";

/// Stub implementation of [`StorageBackend`] over the future
/// frankensqlite connection type. Construction always fails today
/// (the stub does not actually open a database); when Phase 5+
/// ships the wired-pass replaces every method body in this file.
///
/// The struct is `pub` because downstream code that picks the
/// backend at compile time (via `cfg(feature = "frankensqlite-
/// backend")`) needs to name the type for trait-object boxing.
#[derive(Debug)]
pub struct FrankenSQLiteBackend {
    /// Captured at construction so the wired-pass has a place to
    /// stash the real connection without changing the struct
    /// layout. Stays `()` in the stub so the type is `Send +
    /// Sync` without ceremony.
    _connection: (),
    /// Captured at construction for diagnostic display. The stub
    /// reports it back via `path()` so test fixtures that hit the
    /// not-wired path can verify the path threaded through
    /// correctly.
    path: String,
}

impl FrankenSQLiteBackend {
    /// Construct a new backend over a database path. Returns
    /// `BackendError::Other(NOT_WIRED_HINT)` until the wired-pass
    /// fills in the real `frankensqlite::Connection::open` call.
    ///
    /// Signature mirrors [`crate::storage_backend_trait::RusqliteBackend::open`]
    /// so the migration tool at
    /// `crates/frankenterm-core/examples/storage_convert.rs` can
    /// swap to this constructor with a single line edit when the
    /// real impl lands.
    #[allow(clippy::needless_pass_by_value)]
    pub fn open(path: &str, _config: &OpenConfig) -> Result<Self, BackendError> {
        let _ = path; // intentionally unused in the stub; carried in the typed Err above
        Err(BackendError::Other(format!(
            "{NOT_WIRED_HINT} (attempted open at `{path}`)"
        )))
    }

    /// Test-only constructor that sidesteps the not-wired
    /// `open` path. Lets unit tests assert the trait surface
    /// returns NOT_WIRED for every method without relying on a
    /// real database. Hidden behind `#[cfg(test)]` so production
    /// callers cannot bypass the readiness gate.
    #[cfg(test)]
    fn for_stub_tests(path: impl Into<String>) -> Self {
        Self {
            _connection: (),
            path: path.into(),
        }
    }

    /// Diagnostic accessor: the path this backend was
    /// constructed over. The stub stores it so the not-wired
    /// errors can include it in their message; the wired-pass
    /// keeps the same accessor for `ft doctor` to surface.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl StorageBackend for FrankenSQLiteBackend {
    fn execute(&self, _sql: &str) -> Result<usize, BackendError> {
        Err(BackendError::Other(NOT_WIRED_HINT.to_string()))
    }

    fn execute_batch(&self, _sql: &str) -> Result<(), BackendError> {
        Err(BackendError::Other(NOT_WIRED_HINT.to_string()))
    }

    fn query_scalar(&self, _sql: &str) -> Result<Option<String>, BackendError> {
        Err(BackendError::Other(NOT_WIRED_HINT.to_string()))
    }

    fn begin_transaction(&self) -> Result<TransactionGuard<'_>, BackendError> {
        Err(BackendError::Other(NOT_WIRED_HINT.to_string()))
    }

    fn user_version(&self) -> Result<u32, BackendError> {
        Err(BackendError::Other(NOT_WIRED_HINT.to_string()))
    }

    fn set_user_version(&self, _version: u32) -> Result<(), BackendError> {
        Err(BackendError::Other(NOT_WIRED_HINT.to_string()))
    }

    fn backend_name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn query_row_strings(
        &self,
        _sql: &str,
        _params: &[&str],
    ) -> Result<Option<Vec<String>>, BackendError> {
        Err(BackendError::Other(NOT_WIRED_HINT.to_string()))
    }

    fn query_map_strings(
        &self,
        _sql: &str,
        _params: &[&str],
    ) -> Result<Vec<Vec<String>>, BackendError> {
        Err(BackendError::Other(NOT_WIRED_HINT.to_string()))
    }

    fn query_row_typed(
        &self,
        _sql: &str,
        _params: &[ToSqlValue<'_>],
    ) -> Result<Option<Vec<String>>, BackendError> {
        Err(BackendError::Other(NOT_WIRED_HINT.to_string()))
    }

    fn query_map_typed(
        &self,
        _sql: &str,
        _params: &[ToSqlValue<'_>],
    ) -> Result<Vec<Vec<String>>, BackendError> {
        Err(BackendError::Other(NOT_WIRED_HINT.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_returns_not_wired_with_path_in_message() {
        let err = FrankenSQLiteBackend::open("/tmp/x.db", &OpenConfig::default())
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("FrankenSQLiteBackend"));
        assert!(msg.contains("/tmp/x.db"), "path must thread through: {msg}");
        assert!(msg.contains("ft-kcdqp"), "bead reference must thread through: {msg}");
    }

    #[test]
    fn backend_name_is_stable_for_telemetry() {
        let backend = FrankenSQLiteBackend::for_stub_tests("/tmp/x.db");
        assert_eq!(backend.backend_name(), "frankensqlite");
        assert_eq!(BACKEND_NAME, "frankensqlite");
    }

    #[test]
    fn path_accessor_returns_construction_path() {
        let backend = FrankenSQLiteBackend::for_stub_tests("/tmp/some/path.db");
        assert_eq!(backend.path(), "/tmp/some/path.db");
    }

    #[test]
    fn execute_returns_not_wired() {
        let backend = FrankenSQLiteBackend::for_stub_tests("/tmp/x.db");
        let err = backend.execute("CREATE TABLE foo (id INTEGER)").unwrap_err();
        assert!(err.to_string().contains("not yet wired") || err.to_string().contains("stub"));
    }

    #[test]
    fn execute_batch_returns_not_wired() {
        let backend = FrankenSQLiteBackend::for_stub_tests("/tmp/x.db");
        assert!(backend.execute_batch("CREATE TABLE foo (id INTEGER)").is_err());
    }

    #[test]
    fn query_scalar_returns_not_wired() {
        let backend = FrankenSQLiteBackend::for_stub_tests("/tmp/x.db");
        assert!(backend.query_scalar("SELECT 1").is_err());
    }

    #[test]
    fn begin_transaction_returns_not_wired() {
        let backend = FrankenSQLiteBackend::for_stub_tests("/tmp/x.db");
        assert!(backend.begin_transaction().is_err());
    }

    #[test]
    fn user_version_returns_not_wired() {
        let backend = FrankenSQLiteBackend::for_stub_tests("/tmp/x.db");
        assert!(backend.user_version().is_err());
    }

    #[test]
    fn set_user_version_returns_not_wired() {
        let backend = FrankenSQLiteBackend::for_stub_tests("/tmp/x.db");
        assert!(backend.set_user_version(5).is_err());
    }

    #[test]
    fn query_row_strings_returns_not_wired() {
        let backend = FrankenSQLiteBackend::for_stub_tests("/tmp/x.db");
        assert!(backend.query_row_strings("SELECT 1", &[]).is_err());
    }

    #[test]
    fn query_map_strings_returns_not_wired() {
        let backend = FrankenSQLiteBackend::for_stub_tests("/tmp/x.db");
        assert!(backend.query_map_strings("SELECT 1", &[]).is_err());
    }

    #[test]
    fn query_row_typed_returns_not_wired() {
        let backend = FrankenSQLiteBackend::for_stub_tests("/tmp/x.db");
        assert!(backend.query_row_typed("SELECT 1", &[]).is_err());
    }

    #[test]
    fn query_map_typed_returns_not_wired() {
        let backend = FrankenSQLiteBackend::for_stub_tests("/tmp/x.db");
        assert!(backend.query_map_typed("SELECT 1", &[]).is_err());
    }

    #[test]
    fn backend_works_through_dyn_dispatch() {
        // Box<dyn StorageBackend> must accept the stub. This is
        // what call sites doing runtime backend selection rely on.
        let backend: Box<dyn StorageBackend> =
            Box::new(FrankenSQLiteBackend::for_stub_tests("/tmp/x.db"));
        assert_eq!(backend.backend_name(), "frankensqlite");
        assert!(backend.execute("CREATE TABLE foo (id INTEGER)").is_err());
    }

    #[test]
    fn not_wired_hint_carries_readiness_pointer() {
        // Operators hitting NOT_WIRED need a one-step fix path.
        // Pin the readiness-script reference so the message stays
        // discoverable even as the rest of the hint evolves.
        assert!(
            NOT_WIRED_HINT.contains("check_frankensqlite_readiness.py"),
            "hint must point at the readiness checker: {NOT_WIRED_HINT}"
        );
        assert!(
            NOT_WIRED_HINT.contains("ft-kcdqp"),
            "hint must carry the bead reference: {NOT_WIRED_HINT}"
        );
    }
}
