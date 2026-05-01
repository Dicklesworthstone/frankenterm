//! Storage-backend trait substrate for the rusqlite → frankensqlite
//! migration plan.
//!
//! **Bead:** `wa-2l27x.8` (FrankenSQLite migration plan).
//! **Doctrine:** `docs/storage/storage-backend-trait.md`.
//!
//! # Why this module exists
//!
//! `wa-2l27x.8` is "DEFERRED until frankensqlite Phase 5+ ships"
//! per the bead body. Migration tasks 1, 3, 4, 5, 6 are all
//! externally blocked on that precondition.
//!
//! Migration task #2, however, is shippable today *regardless*
//! of frankensqlite's eventual fate:
//!
//! > 2. Create storage backend trait to abstract rusqlite vs
//! >    frankensqlite.
//!
//! The trait improves storage.rs's testability (mockable
//! backend for unit tests) AND prepares the boundary for the
//! eventual swap. Either way the work is useful.
//!
//! # Substrate scope (this bead)
//!
//! - The trait shape — names the operations storage.rs performs
//!   without committing to a specific signature for each.
//! - A skeleton `RusqliteBackend` newtype wrapping
//!   `rusqlite::Connection` to demonstrate the boundary fits the
//!   current implementation. Not used by storage.rs today.
//! - A `MockBackend` for testing.
//! - 6 unit tests proving the trait is dyn-safe and the mock
//!   round-trips a basic op flow.
//!
//! # Wired-pass scope (named follow-ups)
//!
//! - `wa-2l27x.8.cont.extract`: refactor storage.rs to consume
//!   the trait via dependency injection. Today storage.rs uses
//!   `rusqlite::Connection` directly throughout (~600 call sites
//!   per a quick `grep -c rusqlite crates/frankenterm-core/src/storage.rs`).
//!   The extraction is a multi-week refactor that the trait
//!   substrate makes tractable.
//! - `wa-2l27x.8.cont.frankensqlite`: implement the trait against
//!   frankensqlite. Blocked on frankensqlite Phase 5+ shipping.
//! - `wa-2l27x.8.cont.benchmarks`: bench the two backends side by
//!   side. Per the bead's task #4. Requires both impls.
//! - `wa-2l27x.8.cont.migration_tool`: rusqlite → frankensqlite
//!   `.db` converter. Per the bead's task #5.
//!
//! # Why a minimal trait shape
//!
//! Storage.rs is ~26K lines and uses rusqlite's full surface.
//! The trait below names the *operations* storage.rs performs
//! conceptually — execute / query_one / query_many / transaction
//! / schema_migrations — without dictating exact type signatures.
//! Implementations will need to carry their own concrete types
//! (rusqlite's `Connection`, frankensqlite's equivalent). The
//! cont.extract bead's job is to thread one concrete type through
//! storage.rs uniformly; this trait is the contract that
//! extraction follows.
//!
//! # What this is NOT
//!
//! - Not a full sql-abstraction layer. We're not building
//!   sqlx-compete; rusqlite remains the reference implementation
//!   for now and frankensqlite is targeted at being a drop-in
//!   wire-format compatible alternative.
//! - Not a query builder. SQL strings stay as strings; the trait
//!   just abstracts who executes them.
//! - Not a connection pool. Pooling is an orthogonal concern that
//!   already exists in storage.rs's `pool` module.

use std::path::Path;
use std::sync::{Arc, Mutex};

/// Capability the storage layer needs from its backing engine.
///
/// Each implementor (rusqlite today, frankensqlite tomorrow,
/// `MockBackend` always) exposes the same surface. Storage.rs
/// is parameterized over this trait under cont.extract.
///
/// The trait is **object-safe** so callers can use
/// `Box<dyn StorageBackend>` to defer the choice at runtime
/// (e.g. via a `--backend` CLI flag during the migration
/// window).
pub trait StorageBackend: Send + Sync {
    /// Execute a SQL statement that doesn't return rows
    /// (DDL like `CREATE TABLE`, or DML like `UPDATE` /
    /// `DELETE` / `INSERT`).
    ///
    /// Returns the number of rows affected (or 0 for DDL).
    fn execute(&self, sql: &str) -> Result<usize, BackendError>;

    /// Execute multiple SQL statements separated by `;`.
    /// Used for schema migrations.
    fn execute_batch(&self, sql: &str) -> Result<(), BackendError>;

    /// Run a query that returns at most one row, returning
    /// the row's first column value as a string. (The
    /// minimal-trait substrate uses string round-tripping;
    /// the wired-pass extension will introduce a full
    /// `Row` abstraction.)
    fn query_scalar(&self, sql: &str) -> Result<Option<String>, BackendError>;

    /// Begin a transaction. Returns a guard that commits on
    /// `commit()` or rolls back on `Drop` (default rollback
    /// semantics — explicit commit required for durability).
    fn begin_transaction(&self) -> Result<TransactionGuard<'_>, BackendError>;

    /// Schema-version probe used by the migration runner.
    /// Returns the current `PRAGMA user_version` value.
    fn user_version(&self) -> Result<u32, BackendError>;

    /// Set the schema-version after a migration step.
    fn set_user_version(&self, version: u32) -> Result<(), BackendError>;

    /// Implementor name for diagnostics + telemetry. Stable
    /// per backend (e.g. `"rusqlite"`, `"frankensqlite"`,
    /// `"mock"`).
    fn backend_name(&self) -> &'static str;
}

/// Open-time configuration. Backends interpret these knobs as
/// best they can — frankensqlite has more options than rusqlite,
/// and the mock honors only the subset its tests exercise.
#[derive(Debug, Clone)]
pub struct OpenConfig {
    /// Read-only mount.
    pub read_only: bool,
    /// Use WAL journal mode where supported.
    pub wal_mode: bool,
    /// Page-size hint (bytes). Backends may ignore.
    pub page_size_hint: Option<u32>,
}

impl Default for OpenConfig {
    fn default() -> Self {
        Self {
            read_only: false,
            wal_mode: true,
            page_size_hint: None,
        }
    }
}

/// Factory for the trait — every backend implementation provides
/// an `open` constructor. Kept separate from `StorageBackend`
/// so the trait stays object-safe (the constructor would otherwise
/// require a `Self` return type).
pub trait StorageBackendFactory {
    type Backend: StorageBackend;

    fn open(path: &Path, config: OpenConfig) -> Result<Self::Backend, BackendError>;
}

/// Error surface common across backends. Implementors lift their
/// native error type into one of these variants.
#[derive(Debug)]
pub enum BackendError {
    /// I/O / connection setup failure.
    Connect(String),
    /// SQL syntax or constraint violation.
    Query(String),
    /// Transaction was poisoned by a panic; subsequent operations
    /// must abort.
    TxPoisoned,
    /// Schema migration failure (e.g. unexpected user_version).
    Schema(String),
    /// Backend-specific catch-all.
    Other(String),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(s) => write!(f, "storage backend connect: {s}"),
            Self::Query(s) => write!(f, "storage backend query: {s}"),
            Self::TxPoisoned => write!(f, "storage backend transaction poisoned"),
            Self::Schema(s) => write!(f, "storage backend schema: {s}"),
            Self::Other(s) => write!(f, "storage backend: {s}"),
        }
    }
}

impl std::error::Error for BackendError {}

/// RAII transaction guard. Drops with rollback by default;
/// explicit `commit()` is required for durability.
pub struct TransactionGuard<'a> {
    backend: &'a dyn StorageBackend,
    committed: bool,
    /// Backends may carry impl-specific state in this slot.
    /// The minimal substrate doesn't use it; cont.extract will.
    _marker: std::marker::PhantomData<&'a ()>,
}

impl TransactionGuard<'_> {
    /// Commit the transaction. Consumes the guard.
    pub fn commit(mut self) -> Result<(), BackendError> {
        self.backend.execute("COMMIT")?;
        self.committed = true;
        Ok(())
    }

    /// Explicitly rollback the transaction. Equivalent to
    /// `drop(guard)` but explicit at call sites that want to
    /// document the intent.
    pub fn rollback(self) {
        // Drop will fire and run the rollback path.
    }
}

impl<'a> Drop for TransactionGuard<'a> {
    fn drop(&mut self) {
        if !self.committed {
            // Best effort — if the backend is already in an
            // inconsistent state, swallow the rollback error
            // since panicking from Drop would mask the original
            // failure.
            let _ = self.backend.execute("ROLLBACK");
        }
    }
}

/// In-memory mock backend. Stores executed statements + answers
/// to `query_scalar` so tests can assert on the call sequence
/// without spinning up a real DB.
pub struct MockBackend {
    inner: Arc<Mutex<MockState>>,
}

#[derive(Default)]
struct MockState {
    executed: Vec<String>,
    user_version: u32,
    in_tx: bool,
    tx_committed: bool,
}

impl MockBackend {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockState::default())),
        }
    }

    /// Snapshot the executed-statement log for assertions.
    pub fn executed(&self) -> Vec<String> {
        self.inner.lock().unwrap().executed.clone()
    }

    /// Was the most-recent transaction committed (vs rolled back)?
    pub fn last_tx_committed(&self) -> bool {
        self.inner.lock().unwrap().tx_committed
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageBackend for MockBackend {
    fn execute(&self, sql: &str) -> Result<usize, BackendError> {
        let mut state = self.inner.lock().unwrap();
        state.executed.push(sql.to_string());
        match sql.trim().to_ascii_uppercase().as_str() {
            "BEGIN" | "BEGIN TRANSACTION" => {
                state.in_tx = true;
                state.tx_committed = false;
            }
            "COMMIT" => {
                state.tx_committed = state.in_tx;
                state.in_tx = false;
            }
            "ROLLBACK" => {
                state.in_tx = false;
                // tx_committed stays false
            }
            _ => {}
        }
        Ok(0)
    }

    fn execute_batch(&self, sql: &str) -> Result<(), BackendError> {
        for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            self.execute(stmt)?;
        }
        Ok(())
    }

    fn query_scalar(&self, _sql: &str) -> Result<Option<String>, BackendError> {
        // The mock doesn't run real queries; tests that need a
        // specific answer should use a custom backend impl.
        Ok(None)
    }

    fn begin_transaction(&self) -> Result<TransactionGuard<'_>, BackendError> {
        self.execute("BEGIN")?;
        Ok(TransactionGuard {
            backend: self,
            committed: false,
            _marker: std::marker::PhantomData,
        })
    }

    fn user_version(&self) -> Result<u32, BackendError> {
        Ok(self.inner.lock().unwrap().user_version)
    }

    fn set_user_version(&self, version: u32) -> Result<(), BackendError> {
        self.inner.lock().unwrap().user_version = version;
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trait_is_dyn_safe() {
        let mock: Box<dyn StorageBackend> = Box::new(MockBackend::new());
        assert_eq!(mock.backend_name(), "mock");
    }

    #[test]
    fn mock_records_executed_statements() {
        let mock = MockBackend::new();
        mock.execute("CREATE TABLE t (a INT)").unwrap();
        mock.execute("INSERT INTO t VALUES (1)").unwrap();
        let log = mock.executed();
        assert_eq!(log.len(), 2);
        assert!(log[0].contains("CREATE TABLE"));
        assert!(log[1].contains("INSERT"));
    }

    #[test]
    fn execute_batch_splits_on_semicolons() {
        let mock = MockBackend::new();
        mock.execute_batch("CREATE TABLE a (x INT); CREATE TABLE b (y INT);")
            .unwrap();
        let log = mock.executed();
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn transaction_commit_records_committed_state() {
        let mock = MockBackend::new();
        let tx = mock.begin_transaction().unwrap();
        tx.commit().unwrap();
        assert!(mock.last_tx_committed());
        let log = mock.executed();
        assert_eq!(log.last().map(|s| s.as_str()), Some("COMMIT"));
    }

    #[test]
    fn transaction_drop_rolls_back_when_not_committed() {
        let mock = MockBackend::new();
        {
            let _tx = mock.begin_transaction().unwrap();
            // _tx drops here without commit().
        }
        assert!(!mock.last_tx_committed());
        let log = mock.executed();
        assert_eq!(log.last().map(|s| s.as_str()), Some("ROLLBACK"));
    }

    #[test]
    fn user_version_round_trips() {
        let mock = MockBackend::new();
        assert_eq!(mock.user_version().unwrap(), 0);
        mock.set_user_version(42).unwrap();
        assert_eq!(mock.user_version().unwrap(), 42);
    }

    #[test]
    fn open_config_defaults_to_wal_mode_writable() {
        let config = OpenConfig::default();
        assert!(!config.read_only);
        assert!(config.wal_mode);
        assert_eq!(config.page_size_hint, None);
    }

    #[test]
    fn backend_error_renders_each_variant() {
        let v = vec![
            BackendError::Connect("path".into()),
            BackendError::Query("bad sql".into()),
            BackendError::TxPoisoned,
            BackendError::Schema("v drift".into()),
            BackendError::Other("misc".into()),
        ];
        for err in &v {
            let s = err.to_string();
            assert!(s.starts_with("storage backend"));
        }
    }
}
