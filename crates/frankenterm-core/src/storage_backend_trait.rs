//! Storage-backend trait substrate for the rusqlite → frankensqlite
//! migration plan.
//!
//! **Bead:** `wa-2l27x.8` (FrankenSQLite migration plan).
//! **Doctrine:** `docs/storage/storage-backend-trait.md`.
//!
//! # Why this module exists
//!
//! `wa-2l27x.8` was originally deferred until a frankensqlite Phase 5+
//! release. That upstream release prerequisite is now satisfied. The real
//! backend remains deferred on FrankenTerm's one-runtime dependency cohort
//! and transaction-ownership prerequisites.
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
//!   current implementation.
//! - A `cfg(test)` `MockBackend` for unit tests.
//! - Inline tests proving the trait is dyn-safe and the mock round-trips the
//!   supported operation flows.
//!
//! # Wired-pass scope (named follow-ups)
//!
//! - `wa-2l27x.8.cont.extract`: refactor storage.rs to consume
//!   the trait via dependency injection. StorageHandle writer and
//!   read-pool paths now flow through `RusqliteBackend` /
//!   `StorageBackend`; remaining direct rusqlite connection work is
//!   isolated to explicit backend, migration, health, sql-helper, and
//!   test modules. The extraction remains a multi-week refactor, but
//!   `storage.rs` itself is guarded against direct `Connection`
//!   regressions by `storage_l1jgo_pool_regression`.
//! - `wa-2l27x.8.cont.frankensqlite`: implement the trait against
//!   `fsqlite`. Blocked on one-runtime dependency convergence and
//!   transaction ownership.
//! - `wa-2l27x.8.cont.benchmarks`: bench the two backends side by
//!   side. Per the bead's task #4. Requires both impls.
//! - `wa-2l27x.8.cont.migration_tool`: rusqlite → frankensqlite
//!   `.db` converter. Per the bead's task #5.
//!
//! # Why a minimal trait shape
//!
//! Storage.rs is large and historically used rusqlite's full surface.
//! The trait below names the *operations* storage.rs performs
//! conceptually — execute / query_one / query_many / transaction
//! / schema_migrations — without dictating exact type signatures.
//! Implementations will need to carry their own concrete types
//! (rusqlite's `Connection`, frankensqlite's equivalent). The
//! cont.extract bead's job is to keep storage call sites on this
//! contract while backend-specific code stays inside backend or
//! explicitly scoped storage submodules.
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

#[cfg(test)]
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use serde::{Deserialize, Serialize};

/// Authoritative state of the backend connection's outer transaction boundary.
///
/// Savepoints may be nested inside [`BackendTransactionState::Transaction`].
/// Callers that create a savepoint must compare the state before and after
/// releasing it: matching states prove that the savepoint did not accidentally
/// open or close the surrounding transaction, while the successful `RELEASE`
/// proves removal of the named savepoint itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendTransactionState {
    /// No transaction is active; the connection is in autocommit mode.
    Autocommit,
    /// An explicit transaction or outermost savepoint is active.
    Transaction,
}

/// Capability the storage layer needs from its backing engine.
///
/// Each production implementor (rusqlite today, frankensqlite
/// tomorrow) exposes the same surface. Storage.rs read/write paths
/// use this trait under cont.extract.
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

    /// Configure the backend connection's busy timeout.
    ///
    /// SQLite uses this to wait for a contended lock instead of
    /// returning `SQLITE_BUSY` immediately. Backends that do not
    /// expose an equivalent should treat this as a best-effort
    /// connection knob and return an error only when the requested
    /// configuration is known to be invalid or failed.
    fn set_busy_timeout(&self, timeout: std::time::Duration) -> Result<(), BackendError>;

    /// Run a query that returns at most one row, returning
    /// the row's first column value as a string. (The
    /// minimal-trait substrate uses string round-tripping;
    /// the wired-pass extension will introduce a full
    /// `Row` abstraction.)
    fn query_scalar(&self, sql: &str) -> Result<Option<String>, BackendError>;

    /// Return the connection's outer transaction state without changing it.
    ///
    /// This is a correctness witness, not a diagnostic hint. Writer recovery
    /// uses it to decide whether a connection epoch is safe to reuse after
    /// transaction or savepoint control. Implementations must fail rather than
    /// guess when the state cannot be established authoritatively.
    fn transaction_state(&self) -> Result<BackendTransactionState, BackendError>;

    /// Dyn-safe transaction execution.
    ///
    /// The backend holds exclusive connection authority for the entire
    /// duration of `f`. Calls made after transaction admission are rejected
    /// with [`BackendError::TransactionBusy`] rather than allowed to interleave
    /// or deadlock through callback reentrancy.
    /// - If `f` returns `Ok(())`, `COMMIT` is executed.
    /// - If `f` returns `Err(error)` or panics, `ROLLBACK` is executed and the connection
    ///   is verified to return to autocommit mode before reuse.
    fn with_transaction_dyn(
        &self,
        f: &mut dyn FnMut(&mut dyn StorageTransaction) -> Result<(), BackendError>,
    ) -> Result<(), BackendError>;

    /// Execute a closure inside an exclusive transaction and return its value.
    ///
    /// The closure receives an exclusive [`StorageTransaction`] handle.
    /// Operations admitted before the transaction may finish first. Operations
    /// attempted after transaction admission are rejected and cannot interleave.
    /// - If `f` returns `Ok(val)`, the transaction commits and returns `Ok(val)`.
    /// - If `f` returns `Err(err)` or panics, the transaction rolls back before returning
    ///   the error or resuming unwind, and the connection is verified to return to autocommit.
    fn with_transaction<T, F>(&self, f: F) -> Result<T, BackendError>
    where
        F: FnOnce(&mut dyn StorageTransaction) -> Result<T, BackendError>,
        Self: Sized,
    {
        let mut output = None;
        let mut f_slot = Some(f);
        self.with_transaction_dyn(&mut |tx| {
            let f_once = f_slot
                .take()
                .ok_or(BackendError::TransactionCallbackInvokedMoreThanOnce)?;
            output = Some(f_once(tx)?);
            Ok(())
        })?;
        output.ok_or_else(|| BackendError::Other("transaction produced no output".into()))
    }

    /// Schema-version probe used by the migration runner.
    /// Returns the current `PRAGMA user_version` value.
    fn user_version(&self) -> Result<u32, BackendError>;

    /// Set the schema-version after a migration step.
    ///
    /// SQLite stores `PRAGMA user_version` as a signed 32-bit
    /// integer. Backends must reject values above
    /// [`SQLITE_USER_VERSION_MAX`] instead of relying on SQLite's
    /// truncating storage behavior.
    fn set_user_version(&self, version: u32) -> Result<(), BackendError>;

    /// Implementor name for diagnostics + telemetry. Stable
    /// per backend (e.g. `"rusqlite"`, `"frankensqlite"`,
    /// `"mock"`).
    fn backend_name(&self) -> &'static str;

    /// True when [`Self::query_row_cells`] / [`Self::query_map_cells`] and
    /// typed parameter binding preserve every SQLite storage class without
    /// routing through the lossy string-canonical defaults.
    ///
    /// Backends must override this before participating in fidelity-sensitive
    /// operations such as storage conversion. The default is conservative
    /// because the trait defaults flatten BLOB bytes to a size placeholder and
    /// cannot distinguish SQL NULL from empty TEXT.
    fn supports_lossless_cells(&self) -> bool {
        false
    }

    // ------------------------------------------------------------------
    // br-ft-qgj81 substrate-pass: multi-column row reads.
    //
    // Object-safe extension that covers storage.rs's bulk query
    // patterns (multi-column PRAGMA + 2-3 column row mappings)
    // without committing the trait to associated types. Values
    // round-trip as strings (the same cheap shape the existing
    // `query_scalar` uses) so the wired-pass call-site migration
    // can land incrementally and graduate to a typed Row
    // abstraction in a follow-up cont-bead.
    //
    // Default impls return `BackendError::Other("not yet
    // implemented")` so existing custom backends keep compiling
    // — only native backends and the cfg(test) mock need
    // overrides.
    //
    // `params` is a `&[&str]` of positional parameters bound to
    // `?N` placeholders (1-indexed per SQLite convention). The
    // wired-pass cont-bead introduces a `ToSqlValue` parameter-
    // binding trait; until then string-only is enough for the
    // PRAGMA + COUNT(*) + simple-select call sites that need
    // multi-column row reads first.
    // ------------------------------------------------------------------

    /// Run a query that returns at most one row, returning each
    /// column as a string (NULL → empty string; integer / float
    /// → decimal string; text → as-is; blob → `"<blob:N bytes>"`).
    ///
    /// `params` are positional parameters bound to `?N`
    /// placeholders. Use an empty slice for parameter-free SQL.
    fn query_row_strings(
        &self,
        _sql: &str,
        _params: &[&str],
    ) -> Result<Option<Vec<String>>, BackendError> {
        Err(BackendError::Other(format!(
            "query_row_strings not yet implemented for backend `{}`",
            self.backend_name(),
        )))
    }

    /// Run a query that may return many rows, returning each row
    /// as a `Vec<String>` per the [`Self::query_row_strings`]
    /// column-encoding rules.
    ///
    /// `params` are positional parameters bound to `?N`
    /// placeholders. Use an empty slice for parameter-free SQL.
    fn query_map_strings(
        &self,
        _sql: &str,
        _params: &[&str],
    ) -> Result<Vec<Vec<String>>, BackendError> {
        Err(BackendError::Other(format!(
            "query_map_strings not yet implemented for backend `{}`",
            self.backend_name(),
        )))
    }

    // ------------------------------------------------------------------
    // br-ft-qgj81 substrate-pass slice 2: typed parameter binding.
    //
    // Default impls route through the string-canonical encoding
    // by calling `to_canonical_string` on each parameter and
    // delegating to `query_row_strings` / `query_map_strings`.
    // Backends that gain native typed binding (RusqliteBackend
    // gets one in a follow-on slice; FrankenSQLiteBackend lands
    // with one) can override these for fidelity (NULL-vs-empty
    // distinction, blob byte-level binding, integer vs decimal-
    // text type tags).
    //
    // Object-safe: `&[ToSqlValue<'_>]` is dyn-friendly.
    // ------------------------------------------------------------------

    /// Run a query that returns at most one row, with typed
    /// parameter binding. See [`Self::query_row_strings`] for
    /// the result-shape contract.
    ///
    /// Default impl renders each parameter via
    /// [`ToSqlValue::to_canonical_string`] and delegates to
    /// `query_row_strings`. Backends should override for
    /// type-fidelity (the string-substrate flattens NULL and
    /// empty TEXT to the same encoding).
    fn query_row_typed(
        &self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Option<Vec<String>>, BackendError> {
        let strings: Vec<String> = params.iter().map(ToSqlValue::to_canonical_string).collect();
        let refs: Vec<&str> = strings.iter().map(String::as_str).collect();
        self.query_row_strings(sql, &refs)
    }

    /// Run a query that may return many rows, with typed
    /// parameter binding. See [`Self::query_map_strings`] for
    /// the result-shape contract.
    fn query_map_typed(
        &self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Vec<Vec<String>>, BackendError> {
        let strings: Vec<String> = params.iter().map(ToSqlValue::to_canonical_string).collect();
        let refs: Vec<&str> = strings.iter().map(String::as_str).collect();
        self.query_map_strings(sql, &refs)
    }

    // ------------------------------------------------------------------
    // br-ft-qgj81 substrate-pass slice 4: typed-row Cell return path.
    //
    // Default impls route through `query_row_typed` / `query_map_typed`
    // and parse each column back via `SqlCell::from_canonical_string`.
    // The parser recovers canonical INTEGER and REAL values, but this
    // path remains lossy for NULL-vs-empty-text and BLOB fidelity because
    // the canonical blob form only carries a byte count.
    //
    // Backends that gain native cell dispatch override these methods
    // for lossless round-trips. `RusqliteBackend`'s override reads
    // `rusqlite::types::Value` directly into `SqlCell` variants — see
    // the impl below.
    // ------------------------------------------------------------------

    /// Run a query that returns at most one row, with each column
    /// returned as a typed [`SqlCell`]. Default impl recovers canonical
    /// numeric cells but remains lossy for NULL / empty text and blobs
    /// (see the module-level note); native backends should override for
    /// lossless cell fidelity.
    fn query_row_cells(
        &self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Option<Vec<SqlCell>>, BackendError> {
        Ok(self.query_row_typed(sql, params)?.map(|cells| {
            cells
                .into_iter()
                .map(|raw| SqlCell::from_canonical_string(&raw))
                .collect()
        }))
    }

    /// Run a query that may return many rows, returning each row as
    /// a `Vec<SqlCell>`. Default-impl fidelity caveats apply per
    /// [`Self::query_row_cells`].
    fn query_map_cells(
        &self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Vec<Vec<SqlCell>>, BackendError> {
        Ok(self
            .query_map_typed(sql, params)?
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|raw| SqlCell::from_canonical_string(&raw))
                    .collect()
            })
            .collect())
    }

    // ------------------------------------------------------------------
    // br-ft-qgj81 substrate-pass slice 5: bulk-execute (the
    // `prepare_cached(sql) + loop execute(params)` migration target).
    //
    // The trait surface previously had no equivalent for the
    // prepare-once-execute-many pattern in storage.rs (~12 callsites,
    // mostly bulk INSERT inside a transaction). The migration guide
    // marked these "currently blocked" pending this method. Adding it
    // unblocks the full storage.rs callsite migration (ft-l1jgo).
    //
    // Object-safe: no method-level generics; `&[Vec<ToSqlValue<'_>>]`
    // dispatches cleanly through `dyn StorageBackend`.
    // ------------------------------------------------------------------

    /// Execute the same `sql` once per row of params, returning the
    /// number of param rows successfully submitted.
    ///
    /// Migration target for the rusqlite idiom:
    /// ```ignore
    /// let mut stmt = conn.prepare_cached(sql)?;
    /// for row in rows {
    ///     stmt.execute(params)?;
    /// }
    /// ```
    ///
    /// Default impl iterates [`crate::storage_backend_helpers::execute_typed`]
    /// per row, which re-prepares the statement on each iteration.
    /// Backends with a real prepared-statement cache (e.g.
    /// [`RusqliteBackend`] overrides with `prepare_cached`) avoid that
    /// cost. The override stays correctness-equivalent.
    ///
    /// Stops on the first error and returns it; the rows already
    /// submitted before the error remain submitted (each iteration is
    /// its own SQLite transaction unless the caller wraps the call in
    /// `BEGIN`/`COMMIT`). Callers that need atomic-batch semantics
    /// should wrap [`Self::execute_many`] inside a `BEGIN`/`COMMIT`
    /// pair via [`Self::execute`].
    ///
    /// Returns the count of param rows submitted, NOT the per-call
    /// SQLite `rows-affected`. The override path can't carry per-call
    /// rows-affected through `prepare_cached`'s iteration cleanly, so
    /// the public contract is the simpler "rows submitted" count.
    fn execute_many(
        &self,
        sql: &str,
        param_rows: &[Vec<ToSqlValue<'_>>],
    ) -> Result<usize, BackendError> {
        // Inline the `execute_typed` body (route through
        // `query_row_typed` and discard the result) so the default
        // impl doesn't need `Self: Sized`. Calling
        // `storage_backend_helpers::execute_typed(self, ...)` would
        // require coercing `&Self` to `&dyn StorageBackend`, which
        // is illegal in object-safe trait default methods without
        // a `Sized` bound that would in turn make this method
        // un-callable on `dyn StorageBackend`.
        let mut total = 0usize;
        for row in param_rows {
            self.query_row_typed(sql, row)?;
            total = total.saturating_add(1);
        }
        Ok(total)
    }
}

/// Owned typed cell. Mirrors [`ToSqlValue`]'s shape but in `'static`
/// form so cells flow through dyn boundaries without lifetime
/// juggling. SQLite's storage classes are NULL / INTEGER / REAL /
/// TEXT / BLOB; we keep the surface tight to those five.
///
/// Defined alongside the trait so [`StorageBackend::query_row_cells`] and
/// [`StorageBackend::query_map_cells`] can name it without a circular module
/// dependency. Re-exported by
/// [`crate::storage_backend_cells`] for consumers that want the
/// higher-level [`crate::storage_backend_cells::Row`] / `RowCells`
/// types alongside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum SqlCell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqlCell {
    /// True when the cell is a SQL NULL.
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Borrow as `i64` when the cell is an Integer.
    #[must_use]
    pub const fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// Borrow as `f64` when the cell is a Real (NOT an Integer
    /// promoted to float — call sites pass the explicit Integer
    /// variant when they want one).
    #[must_use]
    pub const fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Real(f) => Some(*f),
            _ => None,
        }
    }

    /// Borrow the text when the cell is Text.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Borrow the blob when the cell is Blob.
    #[must_use]
    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            Self::Blob(b) => Some(b.as_slice()),
            _ => None,
        }
    }

    /// Parse a cell from the canonical string encoding the default
    /// trait path uses ([`ToSqlValue::to_canonical_string`]).
    ///
    /// This recovers canonical INTEGER and REAL values so default
    /// `query_*_cells` implementations do not silently flatten numeric
    /// storage classes to Text. It cannot recover empty Text vs NULL, or
    /// Blob bytes from the `<blob:N bytes>` placeholder; native backend
    /// overrides still provide the fully lossless path.
    #[must_use]
    pub fn from_canonical_string(raw: &str) -> Self {
        if raw.is_empty() {
            // Mirrors the default path's NULL encoding: NULL renders
            // as the empty string. Distinguishing NULL from empty-
            // text needs the native override.
            return Self::Null;
        }
        if raw.starts_with("<blob:") && raw.ends_with(" bytes>") {
            return Self::Text(raw.to_string());
        }
        if let Ok(i) = raw.parse::<i64>() {
            if i.to_string() == raw {
                return Self::Integer(i);
            }
        }
        if let Ok(f) = raw.parse::<f64>() {
            if f.to_string() == raw {
                return Self::Real(f);
            }
        }
        Self::Text(raw.to_string())
    }
}

/// Translate a [`rusqlite::types::Value`] into a [`SqlCell`] without
/// stringifying. Used by [`RusqliteBackend::query_row_cells`] +
/// [`RusqliteBackend::query_map_cells`] to bypass the trait's lossy
/// default path.
fn rusqlite_value_to_sql_cell(v: rusqlite::types::Value) -> SqlCell {
    match v {
        rusqlite::types::Value::Null => SqlCell::Null,
        rusqlite::types::Value::Integer(i) => SqlCell::Integer(i),
        rusqlite::types::Value::Real(f) => SqlCell::Real(f),
        rusqlite::types::Value::Text(s) => SqlCell::Text(s),
        rusqlite::types::Value::Blob(b) => SqlCell::Blob(b),
    }
}

/// Encode a SQLite value as a string per the substrate's
/// canonical rules (br-ft-qgj81). Used by both
/// [`StorageBackend::query_row_strings`] and
/// [`StorageBackend::query_map_strings`].
///
/// - `NULL`    → empty string.
/// - `INTEGER` → decimal-formatted (no separator).
/// - `REAL`    → `f64::to_string` (default precision).
/// - `TEXT`    → unchanged.
/// - `BLOB`    → `<blob:N bytes>` placeholder so logs / tests
///   carry the size without dumping binary bytes.
#[must_use]
pub fn encode_sqlite_value_as_string(value: &rusqlite::types::Value) -> String {
    match value {
        rusqlite::types::Value::Null => String::new(),
        rusqlite::types::Value::Integer(i) => i.to_string(),
        rusqlite::types::Value::Real(f) => f.to_string(),
        rusqlite::types::Value::Text(s) => s.clone(),
        rusqlite::types::Value::Blob(b) => format!("<blob:{} bytes>", b.len()),
    }
}

// ============================================================================
// br-ft-qgj81 substrate-pass slice 2: ToSqlValue parameter binding.
//
// Scope item 2 of the bead. The first slice (8cb28fcd3) shipped
// `query_row_strings(sql, params: &[&str])` — sufficient for the
// PRAGMA + COUNT(*) + simple-select call sites that need
// multi-column reads against text-typed parameters. Many
// storage.rs call sites bind integer / blob / NULL parameters;
// this slice ships the typed-value abstraction backends use to
// route those bindings.
//
// Object-safe shape: `ToSqlValue` enum carries the variants
// SQLite + frankensqlite both support; backends implement the
// new methods by matching on the enum + dispatching to their
// native binding API. `query_row_typed` + `query_map_typed`
// are the typed-parameter siblings of the string-only
// methods. Default impls fall through to string-only when the
// caller passes only Text values, so backends that haven't
// migrated yet keep compiling.
// ============================================================================

/// Typed parameter value the bindings layer accepts.
///
/// Mirrors SQLite's storage classes (NULL / INTEGER / REAL /
/// TEXT / BLOB) so backends can dispatch to their native
/// `bind_*` family without a string-round-trip detour.
///
/// `Text(&str)` borrows; the wired-pass call-site migration
/// can build long-lived parameter slices on the stack without
/// allocating. `Blob(&[u8])` likewise. The substrate keeps
/// `Owned*` variants for callers that need a `'static` lifetime
/// (e.g., when params are computed dynamically in a function
/// and outlived by the query).
#[derive(Debug, Clone, PartialEq)]
pub enum ToSqlValue<'a> {
    /// SQL NULL.
    Null,
    /// 64-bit signed integer.
    Integer(i64),
    /// IEEE-754 double.
    Real(f64),
    /// Borrowed text.
    Text(&'a str),
    /// Owned text (for parameters with dynamic lifetime).
    OwnedText(String),
    /// Borrowed binary blob.
    Blob(&'a [u8]),
    /// Owned binary blob.
    OwnedBlob(Vec<u8>),
}

impl<'a> ToSqlValue<'a> {
    /// Convenience constructor: NULL.
    #[must_use]
    pub const fn null() -> Self {
        Self::Null
    }

    /// Convenience: bind a `bool` as INTEGER (0/1) — SQLite has
    /// no native bool storage class.
    #[must_use]
    pub const fn bool(b: bool) -> Self {
        if b {
            Self::Integer(1)
        } else {
            Self::Integer(0)
        }
    }

    /// Convenience: bind an `Option<i64>` (None → Null).
    #[must_use]
    pub fn optional_i64(v: Option<i64>) -> Self {
        match v {
            Some(i) => Self::Integer(i),
            None => Self::Null,
        }
    }

    /// Convenience: bind an `Option<&str>` (None → Null).
    #[must_use]
    pub fn optional_text(v: Option<&'a str>) -> Self {
        match v {
            Some(s) => Self::Text(s),
            None => Self::Null,
        }
    }

    /// Render as a `String` per the canonical encoding rules
    /// used by [`encode_sqlite_value_as_string`]. Lets the
    /// default `query_row_typed` impl on `StorageBackend`
    /// fall through to the string-only path on backends that
    /// haven't migrated yet.
    #[must_use]
    pub fn to_canonical_string(&self) -> String {
        match self {
            Self::Null => String::new(),
            Self::Integer(i) => i.to_string(),
            Self::Real(f) => f.to_string(),
            Self::Text(s) => (*s).to_string(),
            Self::OwnedText(s) => s.clone(),
            Self::Blob(b) => format!("<blob:{} bytes>", b.len()),
            Self::OwnedBlob(b) => format!("<blob:{} bytes>", b.len()),
        }
    }
}

impl<'a> From<&'a str> for ToSqlValue<'a> {
    fn from(s: &'a str) -> Self {
        Self::Text(s)
    }
}

impl From<i64> for ToSqlValue<'_> {
    fn from(i: i64) -> Self {
        Self::Integer(i)
    }
}

impl From<u32> for ToSqlValue<'_> {
    fn from(u: u32) -> Self {
        Self::Integer(i64::from(u))
    }
}

impl From<f64> for ToSqlValue<'_> {
    fn from(f: f64) -> Self {
        Self::Real(f)
    }
}

impl From<bool> for ToSqlValue<'_> {
    fn from(b: bool) -> Self {
        Self::bool(b)
    }
}

impl<'a> From<&'a [u8]> for ToSqlValue<'a> {
    fn from(b: &'a [u8]) -> Self {
        Self::Blob(b)
    }
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
    /// Backend-specific unrecoverable transaction poison.
    TxPoisoned,
    /// A transaction callback already owns the backend connection.
    TransactionBusy,
    /// SQLite ended the callback's transaction boundary before finalization.
    TransactionBoundaryLost,
    /// A backend violated the dyn callback's exactly-once contract.
    TransactionCallbackInvokedMoreThanOnce,
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
            Self::TransactionBusy => write!(f, "storage backend transaction already active"),
            Self::TransactionBoundaryLost => {
                write!(f, "storage backend transaction boundary lost")
            }
            Self::TransactionCallbackInvokedMoreThanOnce => {
                write!(f, "storage backend transaction callback invoked more than once")
            }
            Self::Schema(s) => write!(f, "storage backend schema: {s}"),
            Self::Other(s) => write!(f, "storage backend: {s}"),
        }
    }
}

impl std::error::Error for BackendError {}

/// Fail-fast admission token for a synchronous transaction callback.
///
/// Setting the flag before taking the connection mutex prevents both direct
/// callback reentrancy and the callback-spawns-a-worker-then-joins deadlock.
/// The flag is cleared on every return and unwind path.
struct TransactionAdmission<'a> {
    active: &'a AtomicBool,
}

fn callback_transaction_control_is_forbidden(context: AuthContext<'_>) -> bool {
    matches!(
        context.action,
        AuthAction::Transaction { .. } | AuthAction::Savepoint { .. }
    )
}

const AUTHORIZER_PHASE_EXTERNAL: u8 = 0;
const AUTHORIZER_PHASE_CALLBACK: u8 = 1;
const AUTHORIZER_PHASE_INTERNAL_CONTROL: u8 = 2;

/// Restores the authorizer phase even if a caller policy or SQLite wrapper
/// unexpectedly unwinds while the connection is in a privileged phase.
struct AuthorizerPhaseReset<'a> {
    phase: &'a AtomicU8,
}

impl<'a> AuthorizerPhaseReset<'a> {
    fn enter(phase: &'a AtomicU8, next: u8) -> Result<Self, BackendError> {
        phase
            .compare_exchange(
                AUTHORIZER_PHASE_EXTERNAL,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| BackendError::TxPoisoned)?;
        Ok(Self { phase })
    }
}

impl Drop for AuthorizerPhaseReset<'_> {
    fn drop(&mut self) {
        self.phase
            .store(AUTHORIZER_PHASE_EXTERNAL, Ordering::Release);
    }
}

/// Caller-supplied SQLite authorizer policy composed with the backend's scoped
/// transaction fence.
///
/// Rusqlite exposes one authorizer slot per connection and cannot recover a
/// previously installed callback. Callers that need a policy after transferring
/// a connection to [`RusqliteBackend`] must provide it to
/// [`RusqliteBackend::new_with_authorizer`]. Interior mutability may be used for
/// observability such as counters, but it must not influence the returned
/// [`Authorization`]. Decisions must not depend on time, counters, roles,
/// generations, or any other mutable state; they must remain stable for the
/// lifetime of the backend. SQLite authorizes at statement preparation time,
/// so changing a captured decision out of band could make a cached statement
/// disagree with the new policy. Recreate the backend and connection to install
/// different rules.
pub type RusqliteAuthorizerPolicy =
    dyn for<'r> Fn(AuthContext<'r>) -> Authorization + Send + Sync + 'static;

fn install_rusqlite_backend_authorizer(
    conn: &rusqlite::Connection,
    phase: Arc<AtomicU8>,
    policy: Arc<RusqliteAuthorizerPolicy>,
) -> rusqlite::Result<()> {
    conn.authorizer(Some(move |context: AuthContext<'_>| {
        let control = callback_transaction_control_is_forbidden(context);
        match (phase.load(Ordering::Acquire), control) {
            (AUTHORIZER_PHASE_CALLBACK, true) => Authorization::Deny,
            (AUTHORIZER_PHASE_INTERNAL_CONTROL, true) => Authorization::Allow,
            _ => policy(context),
        }
    }))
}

impl<'a> TransactionAdmission<'a> {
    fn acquire(active: &'a AtomicBool) -> Result<Self, BackendError> {
        active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| BackendError::TransactionBusy)?;
        Ok(Self { active })
    }
}

impl Drop for TransactionAdmission<'_> {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

/// Maximum round-trippable value for SQLite's `PRAGMA user_version`.
pub const SQLITE_USER_VERSION_MAX: u32 = i32::MAX as u32;

fn sqlite_user_version_value(version: u32) -> Result<i64, BackendError> {
    if version > SQLITE_USER_VERSION_MAX {
        return Err(BackendError::Other(format!(
            "user_version {version} exceeds SQLite signed 32-bit maximum {SQLITE_USER_VERSION_MAX}"
        )));
    }
    Ok(i64::from(version))
}

/// Exclusive transaction interface provided to closures passed to
/// [`StorageBackend::with_transaction`] or [`StorageBackend::with_transaction_dyn`].
///
/// Implementors guarantee exclusive ownership of the underlying connection
/// for the duration of the transaction.
pub trait StorageTransaction {
    /// Execute a SQL statement (DDL or DML). Returns rows affected.
    fn execute(&mut self, sql: &str) -> Result<usize, BackendError>;

    /// Execute multiple SQL statements separated by `;`.
    fn execute_batch(&mut self, sql: &str) -> Result<(), BackendError>;

    /// Run a scalar query returning the first column of the first row.
    fn query_scalar(&mut self, sql: &str) -> Result<Option<String>, BackendError>;

    /// Run a query returning at most one row as strings.
    fn query_row_strings(
        &mut self,
        sql: &str,
        params: &[&str],
    ) -> Result<Option<Vec<String>>, BackendError>;

    /// Run a query returning multiple rows as strings.
    fn query_map_strings(
        &mut self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<Vec<String>>, BackendError>;

    /// Run a query returning at most one row with typed parameters.
    fn query_row_typed(
        &mut self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Option<Vec<String>>, BackendError> {
        let strings: Vec<String> = params.iter().map(ToSqlValue::to_canonical_string).collect();
        let refs: Vec<&str> = strings.iter().map(String::as_str).collect();
        self.query_row_strings(sql, &refs)
    }

    /// Run a query returning multiple rows with typed parameters.
    fn query_map_typed(
        &mut self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Vec<Vec<String>>, BackendError> {
        let strings: Vec<String> = params.iter().map(ToSqlValue::to_canonical_string).collect();
        let refs: Vec<&str> = strings.iter().map(String::as_str).collect();
        self.query_map_strings(sql, &refs)
    }

    /// Run a query returning at most one row as typed `SqlCell`s.
    fn query_row_cells(
        &mut self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Option<Vec<SqlCell>>, BackendError> {
        let cells = self.query_row_typed(sql, params)?.map(|row| {
            row.into_iter()
                .map(|s| SqlCell::from_canonical_string(&s))
                .collect()
        });
        Ok(cells)
    }

    /// Run a query returning multiple rows as typed `SqlCell`s.
    fn query_map_cells(
        &mut self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Vec<Vec<SqlCell>>, BackendError> {
        let rows = self
            .query_map_typed(sql, params)?
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|s| SqlCell::from_canonical_string(&s))
                    .collect()
            })
            .collect();
        Ok(rows)
    }

    /// Return the current `PRAGMA user_version`.
    fn user_version(&mut self) -> Result<u32, BackendError>;

    /// Set `PRAGMA user_version`.
    fn set_user_version(&mut self, version: u32) -> Result<(), BackendError>;
}

/// Transaction handle wrapping a borrowed `rusqlite::Connection`.
pub struct RusqliteTransactionHandle<'a> {
    conn: &'a mut rusqlite::Connection,
    boundary_lost: bool,
}

impl RusqliteTransactionHandle<'_> {
    fn ensure_boundary(&mut self) -> Result<(), BackendError> {
        if self.boundary_lost || self.conn.is_autocommit() {
            self.boundary_lost = true;
            return Err(BackendError::TransactionBoundaryLost);
        }
        Ok(())
    }

    fn observe_boundary<T>(
        &mut self,
        result: Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        if self.conn.is_autocommit() {
            self.boundary_lost = true;
            if result.is_ok() {
                return Err(BackendError::TransactionBoundaryLost);
            }
        }
        result
    }

    fn run<T>(
        &mut self,
        operation: impl FnOnce(&mut rusqlite::Connection) -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        self.ensure_boundary()?;
        let result = operation(&mut *self.conn);
        self.observe_boundary(result)
    }

    fn boundary_lost(&self) -> bool {
        self.boundary_lost
    }
}

impl StorageTransaction for RusqliteTransactionHandle<'_> {
    fn execute(&mut self, sql: &str) -> Result<usize, BackendError> {
        self.run(|conn| {
            conn.execute(sql, [])
                .map_err(|e| BackendError::Query(e.to_string()))
        })
    }

    fn execute_batch(&mut self, sql: &str) -> Result<(), BackendError> {
        self.run(|conn| {
            conn.execute_batch(sql)
                .map_err(|e| BackendError::Query(e.to_string()))
        })
    }

    fn query_scalar(&mut self, sql: &str) -> Result<Option<String>, BackendError> {
        self.run(|conn| {
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| BackendError::Query(e.to_string()))?;
            let mut rows = stmt
                .query([])
                .map_err(|e| BackendError::Query(e.to_string()))?;
            match rows
                .next()
                .map_err(|e| BackendError::Query(e.to_string()))?
            {
                Some(row) => {
                    let v: rusqlite::types::Value =
                        row.get(0).map_err(|e| BackendError::Query(e.to_string()))?;
                    Ok(Some(match v {
                        rusqlite::types::Value::Null => String::new(),
                        rusqlite::types::Value::Integer(i) => i.to_string(),
                        rusqlite::types::Value::Real(f) => f.to_string(),
                        rusqlite::types::Value::Text(s) => s,
                        rusqlite::types::Value::Blob(b) => format!("<blob:{} bytes>", b.len()),
                    }))
                }
                None => Ok(None),
            }
        })
    }

    fn query_row_strings(
        &mut self,
        sql: &str,
        params: &[&str],
    ) -> Result<Option<Vec<String>>, BackendError> {
        self.run(|conn| {
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| BackendError::Query(e.to_string()))?;
            let column_count = stmt.column_count();
            let mut rows = stmt
                .query(rusqlite::params_from_iter(params.iter().copied()))
                .map_err(|e| BackendError::Query(e.to_string()))?;
            match rows
                .next()
                .map_err(|e| BackendError::Query(e.to_string()))?
            {
                Some(row) => {
                    let mut out = Vec::with_capacity(column_count);
                    for i in 0..column_count {
                        let v: rusqlite::types::Value =
                            row.get(i).map_err(|e| BackendError::Query(e.to_string()))?;
                        out.push(encode_sqlite_value_as_string(&v));
                    }
                    Ok(Some(out))
                }
                None => Ok(None),
            }
        })
    }

    fn query_map_strings(
        &mut self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<Vec<String>>, BackendError> {
        self.run(|conn| {
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| BackendError::Query(e.to_string()))?;
            let column_count = stmt.column_count();
            let mut rows = stmt
                .query(rusqlite::params_from_iter(params.iter().copied()))
                .map_err(|e| BackendError::Query(e.to_string()))?;
            let mut out: Vec<Vec<String>> = Vec::new();
            while let Some(row) = rows
                .next()
                .map_err(|e| BackendError::Query(e.to_string()))?
            {
                let mut row_strings = Vec::with_capacity(column_count);
                for i in 0..column_count {
                    let v: rusqlite::types::Value =
                        row.get(i).map_err(|e| BackendError::Query(e.to_string()))?;
                    row_strings.push(encode_sqlite_value_as_string(&v));
                }
                out.push(row_strings);
            }
            Ok(out)
        })
    }

    fn query_row_typed(
        &mut self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Option<Vec<String>>, BackendError> {
        self.run(|conn| {
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| BackendError::Query(e.to_string()))?;
            let column_count = stmt.column_count();
            let typed_values: Vec<rusqlite::types::Value> =
                params.iter().map(to_sqlite_value).collect();
            let mut rows = stmt
                .query(rusqlite::params_from_iter(typed_values.iter()))
                .map_err(|e| BackendError::Query(e.to_string()))?;
            match rows
                .next()
                .map_err(|e| BackendError::Query(e.to_string()))?
            {
                Some(row) => {
                    let mut out = Vec::with_capacity(column_count);
                    for i in 0..column_count {
                        let v: rusqlite::types::Value =
                            row.get(i).map_err(|e| BackendError::Query(e.to_string()))?;
                        out.push(encode_sqlite_value_as_string(&v));
                    }
                    Ok(Some(out))
                }
                None => Ok(None),
            }
        })
    }

    fn query_map_typed(
        &mut self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Vec<Vec<String>>, BackendError> {
        self.run(|conn| {
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| BackendError::Query(e.to_string()))?;
            let column_count = stmt.column_count();
            let typed_values: Vec<rusqlite::types::Value> =
                params.iter().map(to_sqlite_value).collect();
            let mut rows = stmt
                .query(rusqlite::params_from_iter(typed_values.iter()))
                .map_err(|e| BackendError::Query(e.to_string()))?;
            let mut out: Vec<Vec<String>> = Vec::new();
            while let Some(row) = rows
                .next()
                .map_err(|e| BackendError::Query(e.to_string()))?
            {
                let mut row_strings = Vec::with_capacity(column_count);
                for i in 0..column_count {
                    let v: rusqlite::types::Value =
                        row.get(i).map_err(|e| BackendError::Query(e.to_string()))?;
                    row_strings.push(encode_sqlite_value_as_string(&v));
                }
                out.push(row_strings);
            }
            Ok(out)
        })
    }

    fn query_row_cells(
        &mut self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Option<Vec<SqlCell>>, BackendError> {
        self.run(|conn| {
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| BackendError::Query(e.to_string()))?;
            let column_count = stmt.column_count();
            let typed_values: Vec<rusqlite::types::Value> =
                params.iter().map(to_sqlite_value).collect();
            let mut rows = stmt
                .query(rusqlite::params_from_iter(typed_values.iter()))
                .map_err(|e| BackendError::Query(e.to_string()))?;
            match rows
                .next()
                .map_err(|e| BackendError::Query(e.to_string()))?
            {
                Some(row) => {
                    let mut out = Vec::with_capacity(column_count);
                    for i in 0..column_count {
                        let v: rusqlite::types::Value =
                            row.get(i).map_err(|e| BackendError::Query(e.to_string()))?;
                        out.push(rusqlite_value_to_sql_cell(v));
                    }
                    Ok(Some(out))
                }
                None => Ok(None),
            }
        })
    }

    fn query_map_cells(
        &mut self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Vec<Vec<SqlCell>>, BackendError> {
        self.run(|conn| {
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| BackendError::Query(e.to_string()))?;
            let column_count = stmt.column_count();
            let typed_values: Vec<rusqlite::types::Value> =
                params.iter().map(to_sqlite_value).collect();
            let mut rows = stmt
                .query(rusqlite::params_from_iter(typed_values.iter()))
                .map_err(|e| BackendError::Query(e.to_string()))?;
            let mut out: Vec<Vec<SqlCell>> = Vec::new();
            while let Some(row) = rows
                .next()
                .map_err(|e| BackendError::Query(e.to_string()))?
            {
                let mut row_cells = Vec::with_capacity(column_count);
                for i in 0..column_count {
                    let v: rusqlite::types::Value =
                        row.get(i).map_err(|e| BackendError::Query(e.to_string()))?;
                    row_cells.push(rusqlite_value_to_sql_cell(v));
                }
                out.push(row_cells);
            }
            Ok(out)
        })
    }

    fn user_version(&mut self) -> Result<u32, BackendError> {
        self.run(|conn| {
            let v: i64 = conn
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(|e| BackendError::Schema(e.to_string()))?;
            Ok(v.max(0) as u32)
        })
    }

    fn set_user_version(&mut self, version: u32) -> Result<(), BackendError> {
        let version = sqlite_user_version_value(version)?;
        self.run(|conn| {
            conn.pragma_update(None, "user_version", version)
                .map_err(|e| BackendError::Schema(e.to_string()))
        })
    }
}

/// Transaction handle wrapping a borrowed [`MockState`].
#[cfg(test)]
pub struct MockTransactionHandle<'a> {
    state: &'a mut MockState,
}

#[cfg(test)]
impl StorageTransaction for MockTransactionHandle<'_> {
    fn execute(&mut self, sql: &str) -> Result<usize, BackendError> {
        let normalized = sql.trim().to_ascii_uppercase();
        if matches!(
            normalized.split_whitespace().next(),
            Some("BEGIN" | "COMMIT" | "END" | "ROLLBACK" | "SAVEPOINT" | "RELEASE")
        ) {
            return Err(BackendError::Query(
                "transaction-control SQL is forbidden inside a scoped transaction".into(),
            ));
        }
        self.state.executed.push(sql.to_string());
        Ok(0)
    }

    fn execute_batch(&mut self, sql: &str) -> Result<(), BackendError> {
        for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            self.execute(stmt)?;
        }
        Ok(())
    }

    fn query_scalar(&mut self, _sql: &str) -> Result<Option<String>, BackendError> {
        Ok(None)
    }

    fn query_row_strings(
        &mut self,
        sql: &str,
        params: &[&str],
    ) -> Result<Option<Vec<String>>, BackendError> {
        self.state
            .queries
            .push((sql.to_string(), params.iter().map(|s| (*s).to_string()).collect()));
        Ok(self.state.row_responses.pop_front().flatten())
    }

    fn query_map_strings(
        &mut self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<Vec<String>>, BackendError> {
        self.state
            .queries
            .push((sql.to_string(), params.iter().map(|s| (*s).to_string()).collect()));
        Ok(self.state.map_responses.pop_front().unwrap_or_default())
    }

    fn user_version(&mut self) -> Result<u32, BackendError> {
        Ok(self.state.user_version)
    }

    fn set_user_version(&mut self, version: u32) -> Result<(), BackendError> {
        let _ = sqlite_user_version_value(version)?;
        self.state.user_version = version;
        Ok(())
    }
}

/// In-memory mock backend. Stores executed statements + answers
/// to `query_scalar` so tests can assert on the call sequence
/// without spinning up a real DB.
#[cfg(test)]
#[derive(Clone)]
pub struct MockBackend {
    inner: Arc<Mutex<MockState>>,
    transaction_active: Arc<AtomicBool>,
}

#[cfg(test)]
#[derive(Default)]
struct MockState {
    executed: Vec<String>,
    user_version: u32,
    in_tx: bool,
    explicit_transaction: bool,
    savepoint_depth: usize,
    tx_committed: bool,
    /// br-ft-qgj81 substrate-pass: FIFO queue of pre-loaded
    /// responses for `query_row_strings` calls. Each call pops
    /// one entry; an empty queue returns `Ok(None)`.
    row_responses: VecDeque<Option<Vec<String>>>,
    /// br-ft-qgj81 substrate-pass: FIFO queue of pre-loaded
    /// response sets for `query_map_strings` calls. Each call
    /// pops one entry; an empty queue returns `Ok(vec![])`.
    map_responses: VecDeque<Vec<Vec<String>>>,
    /// br-ft-qgj81 substrate-pass: log of `(sql, params)` pairs
    /// observed by `query_row_strings` + `query_map_strings`,
    /// so tests can assert on the call sequence.
    queries: Vec<(String, Vec<String>)>,
}

#[cfg(test)]
impl MockBackend {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockState::default())),
            transaction_active: Arc::new(AtomicBool::new(false)),
        }
    }

    fn reject_if_transaction_active(&self) -> Result<(), BackendError> {
        if self.transaction_active.load(Ordering::Acquire) {
            return Err(BackendError::TransactionBusy);
        }
        Ok(())
    }

    fn state_guard(&self) -> Result<MutexGuard<'_, MockState>, BackendError> {
        self.reject_if_transaction_active()?;
        self.inner.lock().map_err(|_| BackendError::TxPoisoned)
    }

    /// Snapshot the executed-statement log for assertions.
    pub fn executed(&self) -> Vec<String> {
        self.inner.lock().unwrap().executed.clone()
    }

    /// Was the most-recent transaction committed (vs rolled back)?
    pub fn last_tx_committed(&self) -> bool {
        self.inner.lock().unwrap().tx_committed
    }

    /// br-ft-qgj81: enqueue a response that the next
    /// `query_row_strings` call will return. `None` means "no
    /// row matched". Tests load these in the order they expect
    /// queries to fire.
    pub fn enqueue_row_response(&self, response: Option<Vec<String>>) {
        self.inner.lock().unwrap().row_responses.push_back(response);
    }

    /// br-ft-qgj81: enqueue a response that the next
    /// `query_map_strings` call will return. Empty `Vec` means
    /// "no rows matched". Tests load these in the order they
    /// expect queries to fire.
    pub fn enqueue_map_response(&self, response: Vec<Vec<String>>) {
        self.inner.lock().unwrap().map_responses.push_back(response);
    }

    /// br-ft-qgj81: snapshot the `(sql, params)` log observed
    /// by the multi-column query methods, in call order.
    pub fn observed_queries(&self) -> Vec<(String, Vec<String>)> {
        self.inner.lock().unwrap().queries.clone()
    }
}

#[cfg(test)]
impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl StorageBackend for MockBackend {
    fn execute(&self, sql: &str) -> Result<usize, BackendError> {
        let mut state = self.state_guard()?;
        state.executed.push(sql.to_string());
        let normalized = sql.trim().to_ascii_uppercase();
        match normalized.as_str() {
            "BEGIN" | "BEGIN TRANSACTION" | "BEGIN IMMEDIATE" => {
                state.in_tx = true;
                state.explicit_transaction = true;
                state.tx_committed = false;
            }
            "COMMIT" => {
                state.tx_committed = state.in_tx;
                state.in_tx = false;
                state.explicit_transaction = false;
                state.savepoint_depth = 0;
            }
            "ROLLBACK" => {
                state.in_tx = false;
                state.explicit_transaction = false;
                state.savepoint_depth = 0;
                // tx_committed stays false
            }
            _ => {}
        }
        if normalized.starts_with("SAVEPOINT ") {
            state.in_tx = true;
            state.savepoint_depth = state.savepoint_depth.saturating_add(1);
            state.tx_committed = false;
        } else if normalized.starts_with("RELEASE ") {
            state.savepoint_depth = state.savepoint_depth.saturating_sub(1);
            state.in_tx = state.explicit_transaction || state.savepoint_depth != 0;
        }
        Ok(0)
    }

    fn execute_batch(&self, sql: &str) -> Result<(), BackendError> {
        self.reject_if_transaction_active()?;
        for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            self.execute(stmt)?;
        }
        Ok(())
    }

    fn set_busy_timeout(&self, timeout: std::time::Duration) -> Result<(), BackendError> {
        self.state_guard()?
            .executed
            .push(format!("busy_timeout_ms: {}", timeout.as_millis()));
        Ok(())
    }

    fn query_scalar(&self, _sql: &str) -> Result<Option<String>, BackendError> {
        let _guard = self.state_guard()?;
        // The mock doesn't run real queries; tests that need a
        // specific answer should use a custom backend impl.
        Ok(None)
    }

    fn transaction_state(&self) -> Result<BackendTransactionState, BackendError> {
        Ok(if self.state_guard()?.in_tx {
            BackendTransactionState::Transaction
        } else {
            BackendTransactionState::Autocommit
        })
    }

    fn with_transaction_dyn(
        &self,
        f: &mut dyn FnMut(&mut dyn StorageTransaction) -> Result<(), BackendError>,
    ) -> Result<(), BackendError> {
        let admission = TransactionAdmission::acquire(&self.transaction_active)?;
        let mut guard = self.inner.lock().map_err(|_| BackendError::TxPoisoned)?;
        if guard.in_tx {
            return Err(BackendError::Other(
                "cannot start transaction: mock connection is not in autocommit mode".into(),
            ));
        }

        guard.in_tx = true;
        guard.tx_committed = false;
        guard.executed.push("BEGIN".to_string());
        let user_version_before = guard.user_version;

        let mut tx_handle = MockTransactionHandle { state: &mut *guard };

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut tx_handle)));
        drop(tx_handle);

        match result {
            Ok(Ok(())) => {
                guard.in_tx = false;
                guard.tx_committed = true;
                guard.executed.push("COMMIT".to_string());
                Ok(())
            }
            Ok(Err(err)) => {
                guard.in_tx = false;
                guard.tx_committed = false;
                guard.user_version = user_version_before;
                guard.executed.push("ROLLBACK".to_string());
                Err(err)
            }
            Err(panic_payload) => {
                guard.in_tx = false;
                guard.tx_committed = false;
                guard.user_version = user_version_before;
                guard.executed.push("ROLLBACK".to_string());
                drop(guard);
                drop(admission);
                std::panic::resume_unwind(panic_payload);
            }
        }
    }

    fn user_version(&self) -> Result<u32, BackendError> {
        Ok(self.state_guard()?.user_version)
    }

    fn set_user_version(&self, version: u32) -> Result<(), BackendError> {
        let _ = sqlite_user_version_value(version)?;
        self.state_guard()?.user_version = version;
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "mock"
    }

    fn query_row_strings(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<Option<Vec<String>>, BackendError> {
        let mut state = self.state_guard()?;
        state.queries.push((
            sql.to_string(),
            params.iter().map(|p| (*p).to_string()).collect(),
        ));
        Ok(state.row_responses.pop_front().unwrap_or(None))
    }

    fn query_map_strings(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<Vec<String>>, BackendError> {
        let mut state = self.state_guard()?;
        state.queries.push((
            sql.to_string(),
            params.iter().map(|p| (*p).to_string()).collect(),
        ));
        Ok(state.map_responses.pop_front().unwrap_or_default())
    }

    /// MockBackend's bulk-execute override records each param row
    /// in the executed log so tests can assert "we called this SQL
    /// N times with these params" without juggling FIFO responses.
    /// Each iteration is logged with the same SQL prefixed by
    /// `"execute_many: "` so the log distinguishes batch calls
    /// from one-shot `execute` calls.
    fn execute_many(
        &self,
        sql: &str,
        param_rows: &[Vec<ToSqlValue<'_>>],
    ) -> Result<usize, BackendError> {
        let mut state = self.state_guard()?;
        for row in param_rows {
            let canonical_params: Vec<String> =
                row.iter().map(ToSqlValue::to_canonical_string).collect();
            // Encode as `execute_many: <sql>` so test assertions can
            // separate batch from single-shot writes in the executed log.
            state.executed.push(format!(
                "execute_many: {sql} | params=[{}]",
                canonical_params.join(", ")
            ));
        }
        Ok(param_rows.len())
    }
}

/// Real rusqlite-backed implementation of [`StorageBackend`].
///
/// **Bead:** ft-mbs4e (wa-2l27x.8.cont.extract — first slice).
///
/// Wraps `rusqlite::Connection` behind a `Mutex` so the trait's
/// `&self` shape is honored across calls. Storage.rs's existing
/// pool layer (in storage.rs's `pool` module) handles the
/// per-thread connection check-out / check-in dance; this
/// backend is the per-connection wrapper.
///
/// This is the *first slice* of cont.extract: it proves the
/// trait holds the load for actual rusqlite usage. Threading
/// the trait through every storage.rs call site is the bulk of
/// cont.extract and lands incrementally.
pub struct RusqliteBackend {
    conn: Mutex<rusqlite::Connection>,
    transaction_active: AtomicBool,
    authorizer_phase: Arc<AtomicU8>,
    authorizer_policy: Arc<RusqliteAuthorizerPolicy>,
    quarantined: AtomicBool,
    #[cfg(test)]
    fail_next_rollback: AtomicBool,
}

impl RusqliteBackend {
    /// Wrap an existing `rusqlite::Connection` and take ownership of both the
    /// connection and its single SQLite authorizer slot.
    ///
    /// Any authorizer installed before this call is replaced immediately.
    /// Call [`Self::new_with_authorizer`] instead when the transferred
    /// connection must retain an application policy alongside the backend's
    /// transaction-control fence.
    pub fn new(conn: rusqlite::Connection) -> Self {
        Self::new_with_authorizer(conn, |_| Authorization::Allow)
    }

    /// Wrap a connection while preserving an explicit application authorizer.
    ///
    /// During a scoped transaction callback, the backend's denial of caller-
    /// issued outer transaction and savepoint control takes precedence. The
    /// backend's own outer BEGIN/COMMIT/ROLLBACK are admitted in a separate
    /// internal phase. Every unrelated action is delegated to `policy` in all
    /// phases, and external transaction control is delegated normally.
    pub fn new_with_authorizer<F>(conn: rusqlite::Connection, policy: F) -> Self
    where
        F: for<'r> Fn(AuthContext<'r>) -> Authorization + Send + Sync + 'static,
    {
        let authorizer_phase = Arc::new(AtomicU8::new(AUTHORIZER_PHASE_EXTERNAL));
        let authorizer_policy: Arc<RusqliteAuthorizerPolicy> = Arc::new(policy);
        install_rusqlite_backend_authorizer(
            &conn,
            Arc::clone(&authorizer_phase),
            Arc::clone(&authorizer_policy),
        )
        .unwrap_or_else(|error| panic!("failed to install storage authorizer: {error}"));

        Self {
            conn: Mutex::new(conn),
            transaction_active: AtomicBool::new(false),
            authorizer_phase,
            authorizer_policy,
            quarantined: AtomicBool::new(false),
            #[cfg(test)]
            fail_next_rollback: AtomicBool::new(false),
        }
    }

    /// Reclaim the wrapped `rusqlite::Connection`. Used by the
    /// read-pool and cfg(test) loan paths when an existing concrete
    /// rusqlite handle must temporarily cross the [`StorageBackend`]
    /// trait boundary and then be returned to its owner.
    ///
    /// An ordinary mutex poison is recoverable only when SQLite authoritatively
    /// reports autocommit. Reclaiming a quarantined or transaction-active
    /// connection panics instead of leaking an unsafe handle through this
    /// legacy infallible API.
    #[must_use]
    pub fn into_connection(self) -> rusqlite::Connection {
        self.try_into_connection().unwrap_or_else(|_| {
            panic!("refusing to reclaim quarantined or transaction-active storage connection")
        })
    }

    /// Reclaim the connection only when its backend epoch is proven reusable.
    ///
    /// On error the rejected connection is dropped, which lets SQLite close
    /// and roll back without exposing an indeterminate epoch to a caller or
    /// pool. Use this fallible form at cleanup boundaries that must preserve an
    /// already-caught callback panic.
    pub fn try_into_connection(self) -> Result<rusqlite::Connection, BackendError> {
        let quarantined = self.quarantined.load(Ordering::Acquire);
        let authorizer_policy = Arc::clone(&self.authorizer_policy);
        let conn = match self.conn.into_inner() {
            Ok(conn) => conn,
            Err(poisoned) => poisoned.into_inner(),
        };
        if quarantined || !conn.is_autocommit() {
            return Err(BackendError::TxPoisoned);
        }
        // Remove the backend phase machine while retaining the caller's
        // explicitly transferred policy on the reclaimed raw connection.
        conn.authorizer(Some(move |context: AuthContext<'_>| {
            authorizer_policy(context)
        }))
        .map_err(|error| {
            BackendError::Other(format!(
                "failed to restore caller authorizer while reclaiming connection: {error}"
            ))
        })?;
        conn.flush_prepared_statement_cache();
        Ok(conn)
    }

    fn reject_if_transaction_active(&self) -> Result<(), BackendError> {
        if self.transaction_active.load(Ordering::Acquire) {
            return Err(BackendError::TransactionBusy);
        }
        Ok(())
    }

    fn lock_connection(&self) -> Result<MutexGuard<'_, rusqlite::Connection>, BackendError> {
        if self.quarantined.load(Ordering::Acquire) {
            return Err(BackendError::TxPoisoned);
        }

        let guard = match self.conn.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                if !guard.is_autocommit() {
                    self.quarantined.store(true, Ordering::Release);
                    return Err(BackendError::TxPoisoned);
                }
                self.conn.clear_poison();
                guard
            }
        };

        if self.quarantined.load(Ordering::Acquire) {
            return Err(BackendError::TxPoisoned);
        }
        Ok(guard)
    }

    fn conn_guard(&self) -> Result<MutexGuard<'_, rusqlite::Connection>, BackendError> {
        self.reject_if_transaction_active()?;
        self.lock_connection()
    }

    fn quarantine(&self) {
        self.quarantined.store(true, Ordering::Release);
    }

    fn rollback_callback_transaction(
        &self,
        conn: &rusqlite::Connection,
    ) -> Result<(), BackendError> {
        if conn.is_autocommit() {
            return Ok(());
        }
        #[cfg(test)]
        if self.fail_next_rollback.swap(false, Ordering::AcqRel) {
            return Err(BackendError::Query(
                "injected transaction rollback failure".into(),
            ));
        }
        self.execute_backend_transaction_control(conn, "ROLLBACK")
            .map(|_| ())
    }

    /// Execute one backend-owned outer transaction-control statement while
    /// preventing a caller policy from blocking the backend's cleanup and
    /// finalization. Only transaction/savepoint authorizer actions receive the
    /// internal allow; every unrelated action still delegates to the policy.
    fn execute_backend_transaction_control(
        &self,
        conn: &rusqlite::Connection,
        sql: &'static str,
    ) -> Result<usize, BackendError> {
        conn.flush_prepared_statement_cache();
        let phase = AuthorizerPhaseReset::enter(
            self.authorizer_phase.as_ref(),
            AUTHORIZER_PHASE_INTERNAL_CONTROL,
        )?;
        // This is a cleanup-and-rethrow boundary, not panic recovery. Elevated
        // control uses uncached `Connection::execute`; catching here guarantees
        // the phase reset and symmetric cache flush before any unexpected
        // unwind can release the connection mutex.
        let execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            conn.execute(sql, [])
        }));
        drop(phase);
        conn.flush_prepared_statement_cache();
        match execution {
            Ok(result) => {
                result.map_err(|error| BackendError::Query(format!("{sql} failed: {error}")))
            }
            Err(panic_payload) => std::panic::resume_unwind(panic_payload),
        }
    }

    fn reinstall_owned_authorizer(
        &self,
        conn: &rusqlite::Connection,
    ) -> Result<(), BackendError> {
        install_rusqlite_backend_authorizer(
            conn,
            Arc::clone(&self.authorizer_phase),
            Arc::clone(&self.authorizer_policy),
        )
        .map_err(|error| {
            BackendError::Other(format!(
                "failed to reinstall backend-owned storage authorizer: {error}"
            ))
        })?;
        conn.flush_prepared_statement_cache();
        Ok(())
    }

    #[cfg(test)]
    fn inject_next_rollback_failure(&self) {
        self.fail_next_rollback.store(true, Ordering::Release);
    }

    /// Borrow the wrapped connection for legacy rusqlite-only helpers.
    ///
    /// Keep this crate-private: new storage call sites should use the
    /// [`StorageBackend`] trait surface, not reach through to rusqlite. The
    /// callback must not replace/remove the SQLite authorizer or execute SQL
    /// under a replacement hook. The backend defensively reinstalls its exact
    /// composed hook afterward (including on unwind) so an accidental slot
    /// mutation cannot escape the loan and weaken a later scoped transaction.
    pub(crate) fn with_connection<F, R>(&self, f: F) -> Result<R, BackendError>
    where
        F: FnOnce(&rusqlite::Connection) -> R,
    {
        let conn = self.conn_guard()?;
        let callback_result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&conn)));
        let reinstall_result = self.reinstall_owned_authorizer(&conn);
        if callback_result.is_err() && !conn.is_autocommit() {
            let rollback_result = self.rollback_callback_transaction(&conn);
            if rollback_result.is_err() || !conn.is_autocommit() {
                self.quarantine();
            }
        }
        let reusable_after_cleanup = conn.is_autocommit();
        drop(conn);
        if let Err(error) = reinstall_result {
            self.quarantine();
            return match callback_result {
                Err(panic_payload) => std::panic::resume_unwind(panic_payload),
                Ok(_) => Err(error),
            };
        }
        match callback_result {
            Ok(value) => Ok(value),
            Err(panic_payload) => {
                if !reusable_after_cleanup {
                    self.quarantine();
                }
                std::panic::resume_unwind(panic_payload)
            }
        }
    }

    /// Open a fresh connection at UTF-8 `path` with the given config.
    /// `path = ":memory:"` for in-memory.
    pub fn open(path: &str, config: &OpenConfig) -> Result<Self, BackendError> {
        Self::open_path(Path::new(path), config)
    }

    /// Open a fresh connection at filesystem `path` with the given config.
    pub fn open_path(path: &Path, config: &OpenConfig) -> Result<Self, BackendError> {
        let flags = if config.read_only {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
        };
        let conn = rusqlite::Connection::open_with_flags(path, flags)
            .map_err(|e| BackendError::Connect(e.to_string()))?;
        if config.wal_mode && !config.read_only {
            conn.pragma_update(None, "journal_mode", "WAL")
                .map_err(|e| BackendError::Connect(format!("WAL pragma: {e}")))?;
        }
        if let Some(page_size) = config.page_size_hint {
            conn.pragma_update(None, "page_size", page_size as i64)
                .map_err(|e| BackendError::Connect(format!("page_size pragma: {e}")))?;
        }
        Ok(Self::new(conn))
    }
}

#[cfg(test)]
pub(crate) fn with_test_storage_backend<F, R>(
    conn: &mut rusqlite::Connection,
    f: F,
) -> crate::error::Result<R>
where
    F: FnOnce(&dyn StorageBackend) -> crate::error::Result<R>,
{
    let placeholder = rusqlite::Connection::open_in_memory().map_err(|err| {
        crate::error::StorageError::Database(format!(
            "failed to create temporary placeholder backend for storage test loan: {err}"
        ))
    })?;
    let original = std::mem::replace(conn, placeholder);
    let backend = RusqliteBackend::new(original);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&backend)));
    let mut callback_leaked_transaction = false;
    let cleanup = match backend.transaction_state() {
        Ok(BackendTransactionState::Autocommit) => Ok(()),
        Ok(BackendTransactionState::Transaction) => {
            callback_leaked_transaction = true;
            backend.execute("ROLLBACK").map(|_| ())
        }
        Err(error) => Err(error),
    };
    let restoration_failed = match backend.try_into_connection() {
        Ok(restored) => {
            let placeholder = std::mem::replace(conn, restored);
            drop(placeholder);
            false
        }
        Err(_) => true,
    };

    if cleanup.is_err() || restoration_failed {
        return match result {
            Err(panic_payload) => std::panic::resume_unwind(panic_payload),
            Ok(_) => Err(crate::error::StorageError::BackendEpochPoisoned.into()),
        };
    }
    match result {
        Ok(Ok(_)) if callback_leaked_transaction => Err(crate::error::StorageError::Database(
            "test storage callback returned success with an open transaction; the loan was rolled back"
                .to_string(),
        )
        .into()),
        Ok(result) => result,
        Err(panic_payload) => std::panic::resume_unwind(panic_payload),
    }
}

impl StorageBackendFactory for RusqliteBackend {
    type Backend = Self;

    fn open(path: &Path, config: OpenConfig) -> Result<Self::Backend, BackendError> {
        Self::open_path(path, &config)
    }
}

impl StorageBackend for RusqliteBackend {
    fn execute(&self, sql: &str) -> Result<usize, BackendError> {
        let conn = self.conn_guard()?;
        conn.execute(sql, [])
            .map_err(|e| BackendError::Query(e.to_string()))
    }

    fn execute_batch(&self, sql: &str) -> Result<(), BackendError> {
        let conn = self.conn_guard()?;
        conn.execute_batch(sql)
            .map_err(|e| BackendError::Query(e.to_string()))
    }

    fn set_busy_timeout(&self, timeout: std::time::Duration) -> Result<(), BackendError> {
        let conn = self.conn_guard()?;
        conn.busy_timeout(timeout)
            .map_err(|e| BackendError::Query(format!("busy_timeout: {e}")))
    }

    fn query_scalar(&self, sql: &str) -> Result<Option<String>, BackendError> {
        let conn = self.conn_guard()?;
        let mut stmt = conn
            .prepare_cached(sql)
            .map_err(|e| BackendError::Query(e.to_string()))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| BackendError::Query(e.to_string()))?;
        match rows
            .next()
            .map_err(|e| BackendError::Query(e.to_string()))?
        {
            Some(row) => {
                let v: rusqlite::types::Value =
                    row.get(0).map_err(|e| BackendError::Query(e.to_string()))?;
                Ok(Some(match v {
                    rusqlite::types::Value::Null => String::new(),
                    rusqlite::types::Value::Integer(i) => i.to_string(),
                    rusqlite::types::Value::Real(f) => f.to_string(),
                    rusqlite::types::Value::Text(s) => s,
                    rusqlite::types::Value::Blob(b) => format!("<blob:{} bytes>", b.len()),
                }))
            }
            None => Ok(None),
        }
    }

    fn transaction_state(&self) -> Result<BackendTransactionState, BackendError> {
        let conn = self.conn_guard()?;
        Ok(if conn.is_autocommit() {
            BackendTransactionState::Autocommit
        } else {
            BackendTransactionState::Transaction
        })
    }

    fn with_transaction_dyn(
        &self,
        f: &mut dyn FnMut(&mut dyn StorageTransaction) -> Result<(), BackendError>,
    ) -> Result<(), BackendError> {
        let admission = TransactionAdmission::acquire(&self.transaction_active)?;
        let mut guard = self.lock_connection()?;
        if !guard.is_autocommit() {
            return Err(BackendError::Other(
                "cannot start transaction: connection is not in autocommit mode".into(),
            ));
        }

        if let Err(error) = self.execute_backend_transaction_control(&guard, "BEGIN") {
            if !guard.is_autocommit() {
                self.quarantine();
                return Err(BackendError::TxPoisoned);
            }
            return Err(error);
        }

        // SQLite invokes the authorizer when a statement is prepared, not on
        // every execution. Statements cached while the callback fence was
        // inactive must therefore be finalized before the fence is enabled.
        // The connection mutex excludes concurrent prepares across both the
        // cache flush and gate transition.
        guard.flush_prepared_statement_cache();
        let callback_phase =
            match AuthorizerPhaseReset::enter(
                self.authorizer_phase.as_ref(),
                AUTHORIZER_PHASE_CALLBACK,
            ) {
                Ok(phase) => phase,
                Err(error) => {
                    self.quarantine();
                    return Err(error);
                }
            };
        let mut tx_handle = RusqliteTransactionHandle {
            conn: &mut guard,
            boundary_lost: false,
        };
        let callback_result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut tx_handle)));

        let handle_boundary_lost = tx_handle.boundary_lost();
        drop(tx_handle);
        drop(callback_phase);
        // Keep the inverse transition honest too: future code may add cached
        // callback statements whose prepare-time denial should not survive
        // after the transaction fence is disabled.
        guard.flush_prepared_statement_cache();
        let boundary_lost = handle_boundary_lost || guard.is_autocommit();
        let callback_requested_commit = matches!(&callback_result, Ok(Ok(())));

        let finalization = if callback_requested_commit && !boundary_lost {
            match self.execute_backend_transaction_control(&guard, "COMMIT") {
                Ok(_) if guard.is_autocommit() => Ok(()),
                Ok(_) => {
                    let _ = self.rollback_callback_transaction(&guard);
                    self.quarantine();
                    Err(BackendError::TxPoisoned)
                }
                Err(commit_error) => {
                    let _ = self.rollback_callback_transaction(&guard);
                    if guard.is_autocommit() {
                        Err(commit_error)
                    } else {
                        self.quarantine();
                        Err(BackendError::TxPoisoned)
                    }
                }
            }
        } else {
            let _ = self.rollback_callback_transaction(&guard);
            if !guard.is_autocommit() {
                self.quarantine();
                Err(BackendError::TxPoisoned)
            } else if boundary_lost {
                Err(BackendError::TransactionBoundaryLost)
            } else {
                Ok(())
            }
        };

        drop(guard);
        drop(admission);

        match callback_result {
            Ok(Ok(())) => finalization,
            Ok(Err(callback_error)) => match finalization {
                Ok(()) => Err(callback_error),
                Err(finalization_error) => Err(finalization_error),
            },
            Err(panic_payload) => {
                // This is a cleanup-and-rethrow boundary, not panic recovery.
                // The connection guard is deliberately gone before unwinding,
                // so the original payload cannot poison the backend mutex.
                std::panic::resume_unwind(panic_payload)
            }
        }
    }

    fn user_version(&self) -> Result<u32, BackendError> {
        let conn = self.conn_guard()?;
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|e| BackendError::Schema(e.to_string()))?;
        Ok(v.max(0) as u32)
    }

    fn set_user_version(&self, version: u32) -> Result<(), BackendError> {
        let version = sqlite_user_version_value(version)?;
        let conn = self.conn_guard()?;
        conn.pragma_update(None, "user_version", version)
            .map_err(|e| BackendError::Schema(e.to_string()))
    }

    fn backend_name(&self) -> &'static str {
        "rusqlite"
    }

    fn supports_lossless_cells(&self) -> bool {
        true
    }

    fn query_row_strings(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<Option<Vec<String>>, BackendError> {
        let conn = self.conn_guard()?;
        let mut stmt = conn
            .prepare_cached(sql)
            .map_err(|e| BackendError::Query(e.to_string()))?;
        let column_count = stmt.column_count();
        // rusqlite::params_from_iter accepts an IntoIterator over
        // anything that implements ToSql. &str does, so this
        // compiles cleanly + binds positional params 1-indexed.
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params.iter().copied()))
            .map_err(|e| BackendError::Query(e.to_string()))?;
        match rows
            .next()
            .map_err(|e| BackendError::Query(e.to_string()))?
        {
            Some(row) => {
                let mut out = Vec::with_capacity(column_count);
                for i in 0..column_count {
                    let v: rusqlite::types::Value =
                        row.get(i).map_err(|e| BackendError::Query(e.to_string()))?;
                    out.push(encode_sqlite_value_as_string(&v));
                }
                Ok(Some(out))
            }
            None => Ok(None),
        }
    }

    fn query_map_strings(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<Vec<String>>, BackendError> {
        let conn = self.conn_guard()?;
        let mut stmt = conn
            .prepare_cached(sql)
            .map_err(|e| BackendError::Query(e.to_string()))?;
        let column_count = stmt.column_count();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params.iter().copied()))
            .map_err(|e| BackendError::Query(e.to_string()))?;
        let mut out: Vec<Vec<String>> = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| BackendError::Query(e.to_string()))?
        {
            let mut row_strings = Vec::with_capacity(column_count);
            for i in 0..column_count {
                let v: rusqlite::types::Value =
                    row.get(i).map_err(|e| BackendError::Query(e.to_string()))?;
                row_strings.push(encode_sqlite_value_as_string(&v));
            }
            out.push(row_strings);
        }
        Ok(out)
    }

    // br-ft-qgj81 slice 3: native ToSqlValue overrides.
    //
    // Default `query_row_typed` / `query_map_typed` on the trait fall
    // through `ToSqlValue::to_canonical_string` → `query_row_strings`
    // which (a) round-trips integers/reals through their `to_string()`
    // form (lossy for floats with tail digits) and (b) replaces blob
    // contents with the sentinel `"<blob:N bytes>"`. Both bugs surface
    // the moment a caller binds a real BLOB or a precision-sensitive
    // f64 — so RusqliteBackend overrides both methods to dispatch
    // ToSqlValue → rusqlite::types::Value directly, preserving every
    // SQLite storage class on the wire.
    fn query_row_typed(
        &self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Option<Vec<String>>, BackendError> {
        let conn = self.conn_guard()?;
        let mut stmt = conn
            .prepare_cached(sql)
            .map_err(|e| BackendError::Query(e.to_string()))?;
        let column_count = stmt.column_count();
        let typed_values: Vec<rusqlite::types::Value> =
            params.iter().map(to_sqlite_value).collect();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(typed_values.iter()))
            .map_err(|e| BackendError::Query(e.to_string()))?;
        match rows
            .next()
            .map_err(|e| BackendError::Query(e.to_string()))?
        {
            Some(row) => {
                let mut out = Vec::with_capacity(column_count);
                for i in 0..column_count {
                    let v: rusqlite::types::Value =
                        row.get(i).map_err(|e| BackendError::Query(e.to_string()))?;
                    out.push(encode_sqlite_value_as_string(&v));
                }
                Ok(Some(out))
            }
            None => Ok(None),
        }
    }

    fn query_map_typed(
        &self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Vec<Vec<String>>, BackendError> {
        let conn = self.conn_guard()?;
        let mut stmt = conn
            .prepare_cached(sql)
            .map_err(|e| BackendError::Query(e.to_string()))?;
        let column_count = stmt.column_count();
        let typed_values: Vec<rusqlite::types::Value> =
            params.iter().map(to_sqlite_value).collect();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(typed_values.iter()))
            .map_err(|e| BackendError::Query(e.to_string()))?;
        let mut out: Vec<Vec<String>> = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| BackendError::Query(e.to_string()))?
        {
            let mut row_strings = Vec::with_capacity(column_count);
            for i in 0..column_count {
                let v: rusqlite::types::Value =
                    row.get(i).map_err(|e| BackendError::Query(e.to_string()))?;
                row_strings.push(encode_sqlite_value_as_string(&v));
            }
            out.push(row_strings);
        }
        Ok(out)
    }

    // br-ft-qgj81 slice 4: native cell-typed return overrides.
    //
    // The trait's default `query_row_cells` / `query_map_cells` route
    // through `query_row_typed` (string-canonical) → `from_canonical_string`,
    // which still cannot recover empty TEXT or blob bytes. The overrides
    // below read `rusqlite::types::Value` directly and lift each storage
    // class into its matching `SqlCell` variant without a string detour,
    // fixing the same lossiness `query_row_typed`'s override fixed for
    // the *parameter* path.
    fn query_row_cells(
        &self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Option<Vec<SqlCell>>, BackendError> {
        let conn = self.conn_guard()?;
        let mut stmt = conn
            .prepare_cached(sql)
            .map_err(|e| BackendError::Query(e.to_string()))?;
        let column_count = stmt.column_count();
        let typed_values: Vec<rusqlite::types::Value> =
            params.iter().map(to_sqlite_value).collect();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(typed_values.iter()))
            .map_err(|e| BackendError::Query(e.to_string()))?;
        match rows
            .next()
            .map_err(|e| BackendError::Query(e.to_string()))?
        {
            Some(row) => {
                let mut out = Vec::with_capacity(column_count);
                for i in 0..column_count {
                    let v: rusqlite::types::Value =
                        row.get(i).map_err(|e| BackendError::Query(e.to_string()))?;
                    out.push(rusqlite_value_to_sql_cell(v));
                }
                Ok(Some(out))
            }
            None => Ok(None),
        }
    }

    fn query_map_cells(
        &self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Vec<Vec<SqlCell>>, BackendError> {
        let conn = self.conn_guard()?;
        let mut stmt = conn
            .prepare_cached(sql)
            .map_err(|e| BackendError::Query(e.to_string()))?;
        let column_count = stmt.column_count();
        let typed_values: Vec<rusqlite::types::Value> =
            params.iter().map(to_sqlite_value).collect();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(typed_values.iter()))
            .map_err(|e| BackendError::Query(e.to_string()))?;
        let mut out: Vec<Vec<SqlCell>> = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| BackendError::Query(e.to_string()))?
        {
            let mut row_cells = Vec::with_capacity(column_count);
            for i in 0..column_count {
                let v: rusqlite::types::Value =
                    row.get(i).map_err(|e| BackendError::Query(e.to_string()))?;
                row_cells.push(rusqlite_value_to_sql_cell(v));
            }
            out.push(row_cells);
        }
        Ok(out)
    }

    /// Native bulk-execute via `prepare_cached`. Locks the mutex
    /// once, prepares the statement once, iterates the param rows
    /// binding each, and drops the prepared statement on exit. The
    /// trait's default impl re-prepares on every iteration, which
    /// loses the rusqlite prepared-statement cache; this override
    /// preserves it.
    ///
    /// Stops on the first error and returns it. Already-submitted
    /// rows remain submitted (each iteration is autocommit unless
    /// the caller wraps in `BEGIN`/`COMMIT`).
    fn execute_many(
        &self,
        sql: &str,
        param_rows: &[Vec<ToSqlValue<'_>>],
    ) -> Result<usize, BackendError> {
        let conn = self.conn_guard()?;
        if param_rows.is_empty() {
            return Ok(0);
        }
        let mut stmt = conn
            .prepare_cached(sql)
            .map_err(|e| BackendError::Query(e.to_string()))?;
        let mut total = 0usize;
        for row in param_rows {
            let typed_values: Vec<rusqlite::types::Value> =
                row.iter().map(to_sqlite_value).collect();
            stmt.execute(rusqlite::params_from_iter(typed_values.iter()))
                .map_err(|e| BackendError::Query(e.to_string()))?;
            total = total.saturating_add(1);
        }
        Ok(total)
    }
}

/// Translate a [`ToSqlValue`] into a [`rusqlite::types::Value`] for
/// native parameter binding. Used by [`RusqliteBackend::query_row_typed`] and
/// [`RusqliteBackend::query_map_typed`] to bypass the trait's string round-trip
/// default impl.
///
/// Borrowed `Text(&str)` and `Blob(&[u8])` are cloned into owned
/// `rusqlite::types::Value::{Text, Blob}` because `params_from_iter`
/// takes ownership of each bound value. The clone cost is negligible
/// next to the SQLite query path.
fn to_sqlite_value(v: &ToSqlValue<'_>) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match v {
        ToSqlValue::Null => Value::Null,
        ToSqlValue::Integer(i) => Value::Integer(*i),
        ToSqlValue::Real(f) => Value::Real(*f),
        ToSqlValue::Text(s) => Value::Text((*s).to_string()),
        ToSqlValue::OwnedText(s) => Value::Text(s.clone()),
        ToSqlValue::Blob(b) => Value::Blob((*b).to_vec()),
        ToSqlValue::OwnedBlob(b) => Value::Blob(b.clone()),
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
    fn rusqlite_into_connection_recovers_from_poisoned_mutex() {
        // The writer-thread Drop guard at storage.rs's
        // `with_writer_backend` calls `into_connection()` even when
        // the wrapped closure panicked. If the panic happened
        // INSIDE a held `MutexGuard` (rusqlite-internal panic
        // during `execute`/`prepare`/etc.), the mutex is poisoned;
        // the earlier `expect("must not be poisoned")` form turned
        // that into a double-panic during unwinding. This test
        // poisons the mutex deliberately and pins the new
        // poison-recovering behavior.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let backend = std::sync::Arc::new(RusqliteBackend::new(conn));

        // Poison the mutex by panicking inside a held lock from
        // another thread. After this, `Mutex::into_inner` would
        // return `Err(PoisonError<Connection>)`.
        let backend_for_poisoner = std::sync::Arc::clone(&backend);
        let _ = std::thread::spawn(move || {
            let _guard = backend_for_poisoner.conn.lock().unwrap();
            panic!("simulate rusqlite-internal panic while holding the lock");
        })
        .join();

        // Pull the backend out of the Arc so we can call the
        // consuming `into_connection`. The Arc is unique now (the
        // poisoner thread dropped its clone on unwind).
        let backend = std::sync::Arc::try_unwrap(backend)
            .unwrap_or_else(|_| panic!("Arc must be unique after poisoner thread joined"));

        // The pre-fix `expect()` would have panicked here. The
        // post-fix recovery path returns the Connection.
        let recovered = backend.into_connection();
        // Connection is functional — we can run a trivial query.
        let one: i64 = recovered
            .query_row("SELECT 1", [], |row| row.get(0))
            .expect("recovered connection must execute");
        assert_eq!(one, 1);
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

    // ── execute_many: bulk-execute migration target (br-ft-qgj81 slice 5) ──

    #[test]
    fn execute_many_empty_param_rows_is_a_zero_op() {
        let mock = MockBackend::new();
        let count = mock
            .execute_many("INSERT INTO t (a) VALUES (?1)", &[])
            .unwrap();
        assert_eq!(count, 0);
        assert!(mock.executed().is_empty(), "no calls should be logged");
    }

    #[test]
    fn execute_many_iterates_param_rows_and_logs_each() {
        let mock = MockBackend::new();
        let rows = vec![
            vec![ToSqlValue::Integer(1), ToSqlValue::Text("alpha")],
            vec![ToSqlValue::Integer(2), ToSqlValue::Text("beta")],
            vec![ToSqlValue::Integer(3), ToSqlValue::Null],
        ];
        let count = mock
            .execute_many("INSERT INTO t (id, name) VALUES (?1, ?2)", &rows)
            .unwrap();
        assert_eq!(count, 3, "submitted row count must equal param_rows.len()");

        let log = mock.executed();
        assert_eq!(log.len(), 3);
        // Each entry begins with the `execute_many:` prefix so test
        // assertions can split batch from one-shot writes.
        for entry in &log {
            assert!(
                entry.starts_with("execute_many: INSERT INTO t"),
                "log entry must carry execute_many prefix: {entry:?}"
            );
        }
        // Each entry carries its row's params in canonical form.
        assert!(log[0].contains("alpha"));
        assert!(log[1].contains("beta"));
        // ToSqlValue::Null encodes to "" in canonical form; we just
        // check that the third entry is structurally present.
        assert!(log[2].contains("INSERT INTO t"));
    }

    #[test]
    fn rusqlite_execute_many_native_override_persists_all_rows() {
        // Pin the RusqliteBackend native override against a real
        // in-memory DB. This is the override that uses `prepare_cached`
        // for the prepare-once optimization; the trait-level default
        // would also persist all rows but re-prepare each time.
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute("CREATE TABLE bulk_pin (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();

        let rows = vec![
            vec![ToSqlValue::Integer(1), ToSqlValue::Text("alpha")],
            vec![ToSqlValue::Integer(2), ToSqlValue::Text("beta")],
            vec![ToSqlValue::Integer(3), ToSqlValue::Text("gamma")],
        ];
        let count = backend
            .execute_many("INSERT INTO bulk_pin (id, name) VALUES (?1, ?2)", &rows)
            .unwrap();
        assert_eq!(count, 3);

        // All three rows must be readable via the trait surface.
        let all = backend
            .query_map_typed("SELECT id, name FROM bulk_pin ORDER BY id", &[])
            .unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0][0], "1");
        assert_eq!(all[0][1], "alpha");
        assert_eq!(all[2][1], "gamma");
    }

    #[test]
    fn rusqlite_execute_many_stops_on_first_error() {
        // SQLite UNIQUE constraint violation aborts the batch; the
        // public contract is "stops on first error and returns it".
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute("CREATE TABLE bulk_unique (id INTEGER PRIMARY KEY, name TEXT UNIQUE)")
            .unwrap();

        let rows = vec![
            vec![ToSqlValue::Integer(1), ToSqlValue::Text("alpha")],
            vec![ToSqlValue::Integer(2), ToSqlValue::Text("alpha")], // duplicate "alpha"
            vec![ToSqlValue::Integer(3), ToSqlValue::Text("gamma")], // never reached
        ];
        let result =
            backend.execute_many("INSERT INTO bulk_unique (id, name) VALUES (?1, ?2)", &rows);
        assert!(matches!(result, Err(BackendError::Query(_))));

        // First row was committed before the second triggered the
        // UNIQUE violation (each iteration is autocommit). The third
        // never ran.
        let all = backend
            .query_map_typed("SELECT id FROM bulk_unique", &[])
            .unwrap();
        assert_eq!(all.len(), 1, "only the first row should persist");
    }

    #[test]
    fn rusqlite_execute_many_inside_explicit_transaction_is_atomic() {
        // Wrapping execute_many in BEGIN/COMMIT gives atomic-batch
        // semantics: a constraint violation rolls back ALL rows
        // (including ones that already executed).
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute("CREATE TABLE bulk_atomic (id INTEGER PRIMARY KEY, name TEXT UNIQUE)")
            .unwrap();

        backend.execute("BEGIN").unwrap();
        let rows = vec![
            vec![ToSqlValue::Integer(1), ToSqlValue::Text("alpha")],
            vec![ToSqlValue::Integer(2), ToSqlValue::Text("alpha")],
        ];
        let result =
            backend.execute_many("INSERT INTO bulk_atomic (id, name) VALUES (?1, ?2)", &rows);
        assert!(result.is_err());
        backend.execute("ROLLBACK").unwrap();

        let all = backend
            .query_map_typed("SELECT id FROM bulk_atomic", &[])
            .unwrap();
        assert!(
            all.is_empty(),
            "rollback must undo the row that succeeded before the error"
        );
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
        mock.with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (1)")?;
            Ok::<(), BackendError>(())
        })
        .unwrap();
        assert!(mock.last_tx_committed());
        let log = mock.executed();
        assert_eq!(log.first().map(|s| s.as_str()), Some("BEGIN"));
        assert_eq!(log.last().map(|s| s.as_str()), Some("COMMIT"));
    }

    #[test]
    fn transaction_error_rolls_back_when_not_committed() {
        let mock = MockBackend::new();
        let res = mock.with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (1)")?;
            Err::<(), _>(BackendError::Other("test error".into()))
        });
        assert!(res.is_err());
        assert!(!mock.last_tx_committed());
        let log = mock.executed();
        assert_eq!(log.first().map(|s| s.as_str()), Some("BEGIN"));
        assert_eq!(log.last().map(|s| s.as_str()), Some("ROLLBACK"));
    }

    #[test]
    fn transaction_panic_rolls_back_and_resumes_unwind() {
        let mock = MockBackend::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = mock.with_transaction::<(), _>(|tx| {
                tx.execute("INSERT INTO t VALUES (1)")?;
                panic!("test panic in mock transaction");
            });
        }));
        assert!(result.is_err());
        assert!(!mock.inner.is_poisoned());
        assert!(!mock.last_tx_committed());
        let log = mock.executed();
        assert_eq!(log.first().map(|s| s.as_str()), Some("BEGIN"));
        assert_eq!(log.last().map(|s| s.as_str()), Some("ROLLBACK"));
    }

    #[test]
    fn mock_transaction_rollback_restores_user_version() {
        let mock = MockBackend::new();
        mock.set_user_version(7).unwrap();
        let result = mock.with_transaction(|tx| {
            tx.set_user_version(99)?;
            Err::<(), _>(BackendError::Other("abort version update".into()))
        });
        assert!(result.is_err());
        assert_eq!(mock.user_version().unwrap(), 7);
        assert!(!mock.last_tx_committed());
    }

    #[test]
    fn test_backend_loan_restores_original_connection_after_panic() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE preserved (value INTEGER); INSERT INTO preserved VALUES (7);")
            .unwrap();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = with_test_storage_backend(&mut conn, |backend| -> crate::error::Result<()> {
                backend.execute("BEGIN").unwrap();
                backend
                    .execute("INSERT INTO preserved VALUES (99)")
                    .unwrap();
                panic!("panic while connection is loaned with an open transaction");
            });
        }));
        let payload = panic.expect_err("loan callback panic must resume");
        assert_eq!(
            payload.downcast_ref::<&'static str>().copied(),
            Some("panic while connection is loaned with an open transaction")
        );

        let values: Vec<i64> = conn
            .prepare("SELECT value FROM preserved ORDER BY value")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(values, vec![7]);
        assert!(conn.is_autocommit());
    }

    #[test]
    fn test_backend_loan_rejects_success_with_a_leaked_transaction() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE preserved (value INTEGER); INSERT INTO preserved VALUES (7);")
            .unwrap();

        let result = with_test_storage_backend(&mut conn, |backend| -> crate::error::Result<()> {
            backend.execute("BEGIN").unwrap();
            backend
                .execute("INSERT INTO preserved VALUES (99)")
                .unwrap();
            Ok(())
        });
        let error = result.expect_err("successful callback must not hide an open transaction");
        assert!(error
            .to_string()
            .contains("returned success with an open transaction"));
        assert!(conn.is_autocommit());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM preserved", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "leaked transaction must be rolled back");
    }

    #[test]
    fn user_version_round_trips() {
        let mock = MockBackend::new();
        assert_eq!(mock.user_version().unwrap(), 0);
        mock.set_user_version(42).unwrap();
        assert_eq!(mock.user_version().unwrap(), 42);
    }

    #[test]
    fn user_version_rejects_values_outside_sqlite_range() {
        let mock = MockBackend::new();
        mock.set_user_version(SQLITE_USER_VERSION_MAX).unwrap();

        let err = mock
            .set_user_version(SQLITE_USER_VERSION_MAX + 1)
            .unwrap_err();
        assert!(matches!(err, BackendError::Other(_)));
        assert_eq!(mock.user_version().unwrap(), SQLITE_USER_VERSION_MAX);
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
            BackendError::TransactionBusy,
            BackendError::TransactionBoundaryLost,
            BackendError::TransactionCallbackInvokedMoreThanOnce,
            BackendError::Schema("v drift".into()),
            BackendError::Other("misc".into()),
        ];
        for err in &v {
            let s = err.to_string();
            assert!(s.starts_with("storage backend"));
        }
    }

    // ========================================================================
    // ft-mbs4e: RusqliteBackend — proves the trait holds the load for
    // actual rusqlite usage. In-memory db; full storage.rs refactor
    // remains in cont.extract.
    // ========================================================================

    struct DoubleInvokeBackend {
        inner: MockBackend,
    }

    impl StorageBackend for DoubleInvokeBackend {
        fn execute(&self, sql: &str) -> Result<usize, BackendError> {
            self.inner.execute(sql)
        }

        fn execute_batch(&self, sql: &str) -> Result<(), BackendError> {
            self.inner.execute_batch(sql)
        }

        fn set_busy_timeout(
            &self,
            timeout: std::time::Duration,
        ) -> Result<(), BackendError> {
            self.inner.set_busy_timeout(timeout)
        }

        fn query_scalar(&self, sql: &str) -> Result<Option<String>, BackendError> {
            self.inner.query_scalar(sql)
        }

        fn transaction_state(&self) -> Result<BackendTransactionState, BackendError> {
            self.inner.transaction_state()
        }

        fn with_transaction_dyn(
            &self,
            f: &mut dyn FnMut(&mut dyn StorageTransaction) -> Result<(), BackendError>,
        ) -> Result<(), BackendError> {
            self.inner.with_transaction_dyn(&mut |tx| {
                f(tx)?;
                f(tx)
            })
        }

        fn user_version(&self) -> Result<u32, BackendError> {
            self.inner.user_version()
        }

        fn set_user_version(&self, version: u32) -> Result<(), BackendError> {
            self.inner.set_user_version(version)
        }

        fn backend_name(&self) -> &'static str {
            "double-invoke-test"
        }
    }

    fn open_memory() -> RusqliteBackend {
        RusqliteBackend::open(":memory:", &OpenConfig::default()).expect("open in-memory rusqlite")
    }

    #[test]
    fn rusqlite_backend_opens_in_memory() {
        let backend = open_memory();
        assert_eq!(backend.backend_name(), "rusqlite");
    }

    #[test]
    fn generic_transaction_callback_second_invocation_forces_rollback() {
        let backend = DoubleInvokeBackend {
            inner: MockBackend::new(),
        };
        let result = backend.with_transaction(|tx| {
            tx.set_user_version(41)?;
            Ok::<(), BackendError>(())
        });
        assert!(matches!(
            result,
            Err(BackendError::TransactionCallbackInvokedMoreThanOnce)
        ));
        assert_eq!(backend.user_version().unwrap(), 0);
        assert!(!backend.inner.last_tx_committed());
    }

    #[test]
    fn rusqlite_backend_executes_ddl_and_dml() {
        let backend = open_memory();
        backend
            .execute("CREATE TABLE t (id INTEGER, val TEXT)")
            .unwrap();
        let n = backend
            .execute("INSERT INTO t (id, val) VALUES (1, 'hello')")
            .unwrap();
        assert_eq!(n, 1);
        let n = backend
            .execute("INSERT INTO t (id, val) VALUES (2, 'world')")
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn rusqlite_backend_query_scalar_returns_first_column() {
        let backend = open_memory();
        backend
            .execute("CREATE TABLE t (id INTEGER, val TEXT)")
            .unwrap();
        backend
            .execute("INSERT INTO t VALUES (42, 'answer')")
            .unwrap();
        let got = backend
            .query_scalar("SELECT id FROM t WHERE val = 'answer'")
            .unwrap();
        assert_eq!(got.as_deref(), Some("42"));
        let got_str = backend
            .query_scalar("SELECT val FROM t WHERE id = 42")
            .unwrap();
        assert_eq!(got_str.as_deref(), Some("answer"));
    }

    #[test]
    fn rusqlite_backend_query_scalar_returns_none_on_empty() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        let got = backend.query_scalar("SELECT id FROM t").unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn rusqlite_backend_user_version_round_trips() {
        let backend = open_memory();
        assert_eq!(backend.user_version().unwrap(), 0);
        backend.set_user_version(7).unwrap();
        assert_eq!(backend.user_version().unwrap(), 7);
        backend.set_user_version(13).unwrap();
        assert_eq!(backend.user_version().unwrap(), 13);
    }

    #[test]
    fn rusqlite_backend_busy_timeout_round_trips() {
        let backend = open_memory();
        backend
            .set_busy_timeout(std::time::Duration::from_millis(1234))
            .unwrap();
        let got = backend.query_scalar("PRAGMA busy_timeout").unwrap();
        assert_eq!(got.as_deref(), Some("1234"));
    }

    #[test]
    fn rusqlite_backend_recovers_poisoned_connection_lock() {
        let backend = open_memory();
        backend
            .execute_batch("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = backend
                .conn
                .lock()
                .expect("connection lock should start clean");
            panic!("poison backend connection lock");
        }));
        assert!(poisoned.is_err());
        assert!(backend.conn.is_poisoned());

        backend.execute("INSERT INTO t VALUES (2)").unwrap();
        assert!(!backend.conn.is_poisoned());
        let count = backend.query_scalar("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.as_deref(), Some("2"));

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = backend
                .conn
                .lock()
                .expect("connection lock should be clean after recovery");
            panic!("poison backend connection lock again");
        }));
        assert!(poisoned.is_err());
        assert!(backend.conn.is_poisoned());

        assert!(
            backend
                .with_connection(|conn| conn.is_autocommit())
                .unwrap()
        );
        assert!(!backend.conn.is_poisoned());
    }

    #[test]
    fn rusqlite_backend_rejects_user_version_outside_sqlite_range() {
        let backend = open_memory();
        backend.set_user_version(SQLITE_USER_VERSION_MAX).unwrap();

        let err = backend
            .set_user_version(SQLITE_USER_VERSION_MAX + 1)
            .unwrap_err();
        assert!(matches!(err, BackendError::Other(_)));
        assert_eq!(backend.user_version().unwrap(), SQLITE_USER_VERSION_MAX);
    }

    #[test]
    fn rusqlite_backend_transaction_commit_persists_inserts() {
        let backend = open_memory();
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Autocommit
        );
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        backend
            .with_transaction(|tx| {
                tx.execute("INSERT INTO t VALUES (1)")?;
                tx.execute("INSERT INTO t VALUES (2)")?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Autocommit
        );
        let count = backend.query_scalar("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.as_deref(), Some("2"));
    }

    #[test]
    fn transaction_state_witness_tracks_nested_and_outermost_savepoints() {
        let backend = open_memory();
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Autocommit
        );

        backend.execute("BEGIN").unwrap();
        backend.execute("SAVEPOINT nested_unit").unwrap();
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Transaction
        );
        backend.execute("RELEASE SAVEPOINT nested_unit").unwrap();
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Transaction,
            "releasing a nested savepoint must preserve its outer transaction"
        );
        backend.execute("ROLLBACK").unwrap();
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Autocommit
        );

        backend.execute("SAVEPOINT outermost_unit").unwrap();
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Transaction
        );
        backend.execute("RELEASE SAVEPOINT outermost_unit").unwrap();
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Autocommit,
            "releasing an outermost savepoint must restore autocommit"
        );
    }

    #[test]
    fn rusqlite_backend_transaction_rollback_discards_inserts() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        let res: Result<(), _> = backend.with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (99)")?;
            Err(BackendError::Other("abort transaction".into()))
        });
        assert!(res.is_err());
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Autocommit
        );
        let count = backend.query_scalar("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.as_deref(), Some("0"));
    }

    #[test]
    fn rusqlite_backend_transaction_panic_rolls_back_and_restores_autocommit() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = backend.with_transaction::<(), _>(|tx| {
                tx.execute("INSERT INTO t VALUES (99)")?;
                panic!("mutation panic inside transaction");
            });
        }));
        assert!(result.is_err());
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Autocommit
        );
        let count = backend.query_scalar("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.as_deref(), Some("0"));
    }

    #[test]
    fn rusqlite_transaction_authorizer_denies_control_sql_and_rolls_back_prior_writes() {
        for control_sql in [
            "/* leading comment */ COMMIT",
            "-- leading comment\nROLLBACK",
            "END TRANSACTION",
            "BEGIN IMMEDIATE",
            "SAVEPOINT nested",
            "RELEASE SAVEPOINT nested",
            "ROLLBACK TO SAVEPOINT nested",
        ] {
            let backend = open_memory();
            backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
            let mut ordinary_insert_admitted = false;

            let result = backend.with_transaction(|tx| {
                tx.execute("INSERT INTO t VALUES (1)")?;
                ordinary_insert_admitted = true;
                tx.execute(control_sql)?;
                tx.execute("INSERT INTO t VALUES (2)")?;
                Ok(())
            });

            assert!(ordinary_insert_admitted, "ordinary SQL must be authorized");
            let message = match result {
                Err(BackendError::Query(message)) => message,
                other => panic!(
                    "SQLite's parser must deny transaction control `{control_sql}`: {other:?}"
                ),
            };
            let normalized_message = message.to_ascii_lowercase();
            assert!(
                normalized_message.contains("not authorized")
                    || normalized_message.contains("authorization denied"),
                "`{control_sql}` must fail at the authorizer, not for an incidental semantic reason: {message}"
            );
            assert_eq!(
                backend.query_scalar("SELECT COUNT(*) FROM t").unwrap().as_deref(),
                Some("0"),
                "a denied `{control_sql}` must roll back writes that preceded it"
            );

            backend.execute("INSERT INTO t VALUES (3)").unwrap();
            assert_eq!(
                backend.query_scalar("SELECT COUNT(*) FROM t").unwrap().as_deref(),
                Some("1"),
                "ordinary SQL must remain usable after denying `{control_sql}`"
            );
        }
    }

    #[test]
    fn scoped_transaction_authorizer_classifies_every_control_action() {
        use rusqlite::hooks::TransactionOperation;

        for action in [
            AuthAction::Transaction {
                operation: TransactionOperation::Begin,
            },
            AuthAction::Transaction {
                operation: TransactionOperation::Release,
            },
            AuthAction::Transaction {
                operation: TransactionOperation::Rollback,
            },
            AuthAction::Savepoint {
                operation: TransactionOperation::Begin,
                savepoint_name: "nested",
            },
            AuthAction::Savepoint {
                operation: TransactionOperation::Release,
                savepoint_name: "nested",
            },
            AuthAction::Savepoint {
                operation: TransactionOperation::Rollback,
                savepoint_name: "nested",
            },
        ] {
            assert!(callback_transaction_control_is_forbidden(AuthContext {
                action,
                database_name: None,
                accessor: None,
            }));
        }
        assert!(!callback_transaction_control_is_forbidden(AuthContext {
            action: AuthAction::Insert { table_name: "t" },
            database_name: Some("main"),
            accessor: None,
        }));
    }

    #[test]
    fn rusqlite_backend_composes_caller_authorizer_with_transaction_fence() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let backend = RusqliteBackend::new_with_authorizer(conn, |context| {
            if matches!(context.action, AuthAction::Delete { .. }) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        });
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        let callback_result = backend.with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (1)")?;
            tx.execute("DELETE FROM t")?;
            Ok(())
        });
        assert!(matches!(callback_result, Err(BackendError::Query(_))));
        assert_eq!(
            backend.query_scalar("SELECT COUNT(*) FROM t").unwrap().as_deref(),
            Some("0"),
            "the caller policy must be delegated to inside the callback and roll back prior writes"
        );

        backend.execute("INSERT INTO t VALUES (2)").unwrap();
        let deletion = backend.execute("DELETE FROM t");
        assert!(matches!(deletion, Err(BackendError::Query(_))));
        assert_eq!(
            backend.query_scalar("SELECT COUNT(*) FROM t").unwrap().as_deref(),
            Some("1"),
            "scoped transaction setup must not erase the caller's authorizer"
        );
    }

    #[test]
    fn rusqlite_backend_internal_control_bypasses_only_caller_control_denials() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let backend = RusqliteBackend::new_with_authorizer(conn, |context| {
            if callback_transaction_control_is_forbidden(context) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        });
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();

        assert!(matches!(backend.execute("BEGIN"), Err(BackendError::Query(_))));
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Autocommit,
            "caller-issued transaction control must remain subject to caller policy"
        );

        backend
            .with_transaction(|tx| {
                tx.execute("INSERT INTO t VALUES (1)")?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            backend.query_scalar("SELECT COUNT(*) FROM t").unwrap().as_deref(),
            Some("1"),
            "backend-owned BEGIN and COMMIT must remain available to the scoped transaction"
        );

        let callback_error = backend.with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (2)")?;
            Err::<(), _>(BackendError::Other("roll back under caller policy".into()))
        });
        assert!(matches!(callback_error, Err(BackendError::Other(_))));
        assert_eq!(
            backend.query_scalar("SELECT COUNT(*) FROM t").unwrap().as_deref(),
            Some("1"),
            "backend-owned ROLLBACK must remain available after a callback error"
        );

        let callback_panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = backend.with_transaction::<(), _>(|tx| {
                tx.execute("INSERT INTO t VALUES (3)")?;
                panic!("panic under transaction-denying caller policy");
            });
        }));
        let payload = callback_panic.expect_err("callback panic must resume");
        assert_eq!(
            payload.downcast_ref::<&'static str>().copied(),
            Some("panic under transaction-denying caller policy")
        );
        assert_eq!(
            backend.query_scalar("SELECT COUNT(*) FROM t").unwrap().as_deref(),
            Some("1"),
            "backend-owned ROLLBACK must remain available before panic resume"
        );
        assert!(matches!(backend.execute("BEGIN"), Err(BackendError::Query(_))));
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Autocommit,
            "the external caller policy must be restored after every scoped exit"
        );
    }

    #[test]
    fn rusqlite_authorizer_phase_transitions_flush_cached_authorizations() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
        let read_prepares = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let policy_reads = Arc::clone(&read_prepares);
        let backend = RusqliteBackend::new_with_authorizer(conn, move |context| {
            if matches!(context.action, AuthAction::Read { .. }) {
                policy_reads.fetch_add(1, Ordering::AcqRel);
            }
            Authorization::Allow
        });
        const QUERY: &str = "SELECT id FROM t WHERE id = 1";

        assert_eq!(backend.query_scalar(QUERY).unwrap().as_deref(), Some("1"));
        let first_prepare_count = read_prepares.load(Ordering::Acquire);
        assert!(first_prepare_count > 0);

        backend
            .with_transaction(|tx| {
                tx.execute("INSERT INTO t VALUES (2)")?;
                Ok(())
            })
            .unwrap();
        let count_after_transition = read_prepares.load(Ordering::Acquire);
        assert_eq!(backend.query_scalar(QUERY).unwrap().as_deref(), Some("1"));
        assert!(
            read_prepares.load(Ordering::Acquire) > count_after_transition,
            "the identical cached query must be re-authorized after a callback phase transition"
        );
    }

    #[test]
    fn rusqlite_raw_connection_loan_cannot_disable_later_transaction_fence() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        backend
            .with_connection(|conn| {
                conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                    .unwrap();
            })
            .unwrap();

        let result = backend.with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (1)")?;
            tx.execute("COMMIT")?;
            Ok(())
        });
        assert!(matches!(result, Err(BackendError::Query(_))));
        assert_eq!(
            backend.query_scalar("SELECT COUNT(*) FROM t").unwrap().as_deref(),
            Some("0"),
            "a raw loan must not disable the authorizer fence for a later callback"
        );
    }

    #[test]
    fn rusqlite_raw_connection_loan_panic_restores_fence_before_resume() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = backend.with_connection(|conn| {
                conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                    .unwrap();
                panic!("panic after replacing raw-loan authorizer");
            });
        }));
        let payload = panic_result.expect_err("raw loan panic must resume");
        assert_eq!(
            payload.downcast_ref::<&'static str>().copied(),
            Some("panic after replacing raw-loan authorizer")
        );

        let result = backend.with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (1)")?;
            tx.execute("COMMIT")?;
            Ok(())
        });
        assert!(matches!(result, Err(BackendError::Query(_))));
        assert_eq!(
            backend.query_scalar("SELECT COUNT(*) FROM t").unwrap().as_deref(),
            Some("0"),
            "panic cleanup must reinstall the fence before resuming the payload"
        );
    }

    #[test]
    fn rusqlite_raw_connection_loan_panic_rolls_back_an_open_transaction() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<(), BackendError> = backend.with_connection(|conn| {
                conn.execute_batch("BEGIN; INSERT INTO t VALUES (1);")
                    .unwrap();
                panic!("panic with open raw-loan transaction");
            });
        }));
        let payload = panic_result.expect_err("raw loan panic must resume");
        assert_eq!(
            payload.downcast_ref::<&'static str>().copied(),
            Some("panic with open raw-loan transaction")
        );
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Autocommit
        );
        assert_eq!(
            backend.query_scalar("SELECT COUNT(*) FROM t").unwrap().as_deref(),
            Some("0"),
            "panic cleanup must roll back a raw-loan transaction before reuse"
        );
        backend.execute("INSERT INTO t VALUES (2)").unwrap();
    }

    #[test]
    fn rusqlite_raw_loan_rollback_failure_preserves_panic_and_quarantines() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        backend.inject_next_rollback_failure();
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<(), BackendError> = backend.with_connection(|conn| {
                conn.execute_batch("BEGIN; INSERT INTO t VALUES (1);")
                    .unwrap();
                panic!("raw-loan panic before injected rollback failure");
            });
        }));
        let payload = panic_result.expect_err("raw loan panic must resume");
        assert_eq!(
            payload.downcast_ref::<&'static str>().copied(),
            Some("raw-loan panic before injected rollback failure")
        );
        assert!(matches!(
            backend.query_scalar("SELECT COUNT(*) FROM t"),
            Err(BackendError::TxPoisoned)
        ));
    }

    #[test]
    fn rusqlite_reclaimed_connection_retains_the_caller_policy() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let backend = RusqliteBackend::new_with_authorizer(conn, |context| {
            if matches!(context.action, AuthAction::Delete { .. }) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        });
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        backend.execute("INSERT INTO t VALUES (1)").unwrap();

        let conn = backend.try_into_connection().unwrap();
        assert!(conn.execute("DELETE FROM t", []).is_err());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn rusqlite_transaction_batch_cannot_rollback_then_write_in_autocommit() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();

        let result = backend.with_transaction(|tx| {
            tx.execute_batch(
                "INSERT INTO t VALUES (1); ROLLBACK; INSERT INTO t VALUES (2);",
            )?;
            Ok(())
        });

        assert!(matches!(result, Err(BackendError::Query(_))));
        assert_eq!(
            backend.query_scalar("SELECT COUNT(*) FROM t").unwrap().as_deref(),
            Some("0"),
            "the batch prefix must be rolled back and its suffix must never reach autocommit"
        );
    }

    #[test]
    fn rusqlite_transaction_cached_control_statement_cannot_bypass_authorizer() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        {
            let conn = backend.conn.lock().unwrap();
            let statement = conn
                .prepare_cached("COMMIT")
                .expect("prime transaction-control cache while the callback fence is inactive");
            drop(statement);
        }

        let result = backend.with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (1)")?;
            tx.execute("COMMIT")?;
            tx.execute("INSERT INTO t VALUES (2)")?;
            Ok(())
        });

        assert!(matches!(result, Err(BackendError::Query(_))));
        assert_eq!(
            backend.query_scalar("SELECT COUNT(*) FROM t").unwrap().as_deref(),
            Some("0")
        );
    }

    #[test]
    fn rusqlite_automatic_rollback_is_sticky_and_blocks_later_callback_writes() {
        let backend = open_memory();
        backend
            .execute("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        backend.execute("INSERT INTO t VALUES (1)").unwrap();

        let result = backend.with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (2)")?;
            assert!(matches!(
                tx.execute("INSERT OR ROLLBACK INTO t VALUES (1)"),
                Err(BackendError::Query(_))
            ));
            assert!(matches!(
                tx.execute("INSERT INTO t VALUES (3)"),
                Err(BackendError::TransactionBoundaryLost)
            ));
            Ok(())
        });

        assert!(matches!(
            result,
            Err(BackendError::TransactionBoundaryLost)
        ));
        assert_eq!(
            backend.query_scalar("SELECT COUNT(*) FROM t").unwrap().as_deref(),
            Some("1"),
            "SQLite's automatic rollback must discard the earlier callback write"
        );
        backend.execute("INSERT INTO t VALUES (4)").unwrap();
    }

    #[test]
    fn rusqlite_deferred_constraint_commit_failure_rolls_back_and_reuses_connection() {
        let backend = open_memory();
        backend
            .execute_batch(
                "PRAGMA foreign_keys = ON; \
                 CREATE TABLE parent (id INTEGER PRIMARY KEY); \
                 CREATE TABLE child (parent_id INTEGER, \
                     FOREIGN KEY(parent_id) REFERENCES parent(id) \
                     DEFERRABLE INITIALLY DEFERRED);",
            )
            .unwrap();

        let result = backend.with_transaction(|tx| {
            tx.execute("INSERT INTO child VALUES (99)")?;
            Ok(())
        });
        assert!(matches!(result, Err(BackendError::Query(_))));
        assert_eq!(
            backend
                .query_scalar("SELECT COUNT(*) FROM child")
                .unwrap()
                .as_deref(),
            Some("0")
        );
        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Autocommit
        );

        backend.execute("INSERT INTO parent VALUES (99)").unwrap();
        backend.execute("INSERT INTO child VALUES (99)").unwrap();
    }

    #[test]
    fn rusqlite_rollback_failure_quarantines_every_connection_surface() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        backend.inject_next_rollback_failure();

        let result = backend.with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (1)")?;
            Err::<(), _>(BackendError::Other("force rollback".into()))
        });
        assert!(matches!(result, Err(BackendError::TxPoisoned)));

        assert!(matches!(
            backend.execute("INSERT INTO t VALUES (2)"),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.execute_batch("SELECT 1;"),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.set_busy_timeout(std::time::Duration::from_millis(1)),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.query_scalar("SELECT COUNT(*) FROM t"),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.query_row_strings("SELECT id FROM t", &[]),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.query_map_strings("SELECT id FROM t", &[]),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.query_row_typed("SELECT id FROM t", &[]),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.query_map_typed("SELECT id FROM t", &[]),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.query_row_cells("SELECT id FROM t", &[]),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.query_map_cells("SELECT id FROM t", &[]),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.execute_many(
                "INSERT INTO t VALUES (?1)",
                &[vec![ToSqlValue::Integer(2)]],
            ),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.execute_many("INSERT INTO t VALUES (?1)", &[]),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.transaction_state(),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.user_version(),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.set_user_version(1),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.with_transaction(|_tx| Ok::<(), BackendError>(())),
            Err(BackendError::TxPoisoned)
        ));
        assert!(matches!(
            backend.with_connection(rusqlite::Connection::is_autocommit),
            Err(BackendError::TxPoisoned)
        ));

        let reclaim = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = backend.into_connection();
        }));
        let payload = reclaim.expect_err("quarantined connection must not escape");
        assert_eq!(
            payload.downcast_ref::<&'static str>().copied(),
            Some("refusing to reclaim quarantined or transaction-active storage connection")
        );
    }

    #[test]
    fn rusqlite_rollback_failure_during_panic_preserves_payload_and_quarantines() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        backend.inject_next_rollback_failure();

        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = backend.with_transaction::<(), _>(|tx| {
                tx.execute("INSERT INTO t VALUES (1)")?;
                panic!("original transaction panic payload");
            });
        }));
        let payload = panic_result.expect_err("callback panic must resume");
        assert_eq!(
            payload.downcast_ref::<&'static str>().copied(),
            Some("original transaction panic payload")
        );
        assert!(matches!(
            backend.query_scalar("SELECT COUNT(*) FROM t"),
            Err(BackendError::TxPoisoned)
        ));
    }

    #[test]
    fn rusqlite_backend_transaction_rejects_same_thread_reentrancy_then_reuses() {
        let (result_sender, result_receiver) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let backend = open_memory();
            let result = (|| -> Result<(), BackendError> {
                backend.execute("CREATE TABLE t (id INTEGER, origin TEXT)")?;
                backend.with_transaction(|tx| {
                    tx.execute("INSERT INTO t VALUES (1, 'transaction')")?;
                    if !matches!(
                        backend.execute("INSERT INTO t VALUES (2, 'reentrant')"),
                        Err(BackendError::TransactionBusy)
                    ) {
                        return Err(BackendError::Other(
                            "same-thread reentrant operation was not rejected".into(),
                        ));
                    }
                    tx.execute("INSERT INTO t VALUES (3, 'transaction')")?;
                    Ok(())
                })?;
                backend.execute("INSERT INTO t VALUES (4, 'post_transaction')")?;
                let count = backend.query_scalar("SELECT COUNT(*) FROM t")?;
                if count.as_deref() != Some("3") {
                    return Err(BackendError::Other(
                        "same-thread reentrancy test observed an unexpected row count".into(),
                    ));
                }
                Ok(())
            })();
            let _ = result_sender.send(result);
        });

        let result = result_receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("same-thread reentrancy must fail fast instead of deadlocking");
        worker.join().expect("reentrancy worker must not panic");
        result.unwrap();
    }

    #[test]
    fn rusqlite_backend_transaction_rejects_joined_child_reentrancy_without_deadlock() {
        let backend = Arc::new(open_memory());
        backend
            .execute("CREATE TABLE t (id INTEGER, origin TEXT)")
            .unwrap();

        let child_backend = Arc::clone(&backend);
        let (child_result_sender, child_result_receiver) = std::sync::mpsc::sync_channel(1);
        let mut child_handle = None;
        let transaction_result = backend.with_transaction(|tx| {
                tx.execute("INSERT INTO t VALUES (1, 'tx_step1')")?;
                let child = std::thread::spawn(move || {
                    let result = child_backend.execute("INSERT INTO t VALUES (100, 'child')");
                    let _ = child_result_sender.send(result);
                });
                child_handle = Some(child);
                match child_result_receiver.recv_timeout(std::time::Duration::from_secs(5)) {
                    Ok(Err(BackendError::TransactionBusy)) => {}
                    Ok(other) => {
                        return Err(BackendError::Other(format!(
                            "joined child returned an unexpected result: {other:?}"
                        )));
                    }
                    Err(_) => {
                        return Err(BackendError::Other(
                            "joined child blocked on the transaction-owned connection".into(),
                        ));
                    }
                }
                tx.execute("INSERT INTO t VALUES (2, 'tx_step2')")?;
                Ok(())
            });
        child_handle
            .expect("transaction must spawn its child")
            .join()
            .expect("joined child must not panic");
        transaction_result.unwrap();

        assert_eq!(
            backend.transaction_state().unwrap(),
            BackendTransactionState::Autocommit
        );
        backend
            .execute("INSERT INTO t VALUES (100, 'post_transaction')")
            .unwrap();

        let rows = backend
            .query_map_strings("SELECT id, origin FROM t ORDER BY rowid", &[])
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], "1");
        assert_eq!(rows[0][1], "tx_step1");
        assert_eq!(rows[1][0], "2");
        assert_eq!(rows[1][1], "tx_step2");
        assert_eq!(rows[2][0], "100");
        assert_eq!(rows[2][1], "post_transaction");
    }

    #[test]
    fn rusqlite_backend_transaction_rejects_non_autocommit_start() {
        let backend = open_memory();
        backend.execute("BEGIN").unwrap();
        let res: Result<(), _> = backend.with_transaction(|tx| {
            tx.execute("SELECT 1")?;
            Ok(())
        });
        assert!(res.is_err());
        backend.execute("ROLLBACK").unwrap();
    }

    #[test]
    fn rusqlite_backend_works_via_dyn_dispatch() {
        // The defining property cont.extract relies on: storage.rs
        // can hold a `Box<dyn StorageBackend>` and dispatch through
        // the trait without knowing which concrete impl is behind it.
        let backend: Box<dyn StorageBackend> = Box::new(open_memory());
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        backend.execute("INSERT INTO t VALUES (1)").unwrap();
        let count = backend.query_scalar("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.as_deref(), Some("1"));
        assert_eq!(backend.backend_name(), "rusqlite");
    }

    #[test]
    fn rusqlite_backend_execute_batch_runs_migration_script() {
        let backend = open_memory();
        backend
            .execute_batch(
                "CREATE TABLE a (x INT); \
                 CREATE TABLE b (y INT); \
                 INSERT INTO a VALUES (1); \
                 INSERT INTO b VALUES (2);",
            )
            .unwrap();
        let a_count = backend.query_scalar("SELECT COUNT(*) FROM a").unwrap();
        assert_eq!(a_count.as_deref(), Some("1"));
        let b_count = backend.query_scalar("SELECT COUNT(*) FROM b").unwrap();
        assert_eq!(b_count.as_deref(), Some("1"));
    }

    // ----------------------------------------------------------------
    // br-ft-qgj81 substrate-pass: multi-column row reads.
    // ----------------------------------------------------------------

    #[test]
    fn rusqlite_query_row_strings_returns_columns_in_order() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute_batch(
                "CREATE TABLE p (id INT, name TEXT, weight REAL); \
                 INSERT INTO p VALUES (1, 'alpha', 1.5);",
            )
            .unwrap();
        let row = backend
            .query_row_strings("SELECT id, name, weight FROM p WHERE id = ?1", &["1"])
            .unwrap();
        assert_eq!(
            row,
            Some(vec![
                "1".to_string(),
                "alpha".to_string(),
                "1.5".to_string()
            ])
        );
    }

    #[test]
    fn rusqlite_query_row_strings_returns_none_when_no_match() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend.execute_batch("CREATE TABLE p (id INT);").unwrap();
        let row = backend
            .query_row_strings("SELECT id FROM p WHERE id = ?1", &["999"])
            .unwrap();
        assert_eq!(row, None);
    }

    #[test]
    fn rusqlite_query_row_strings_encodes_null_as_empty() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute_batch(
                "CREATE TABLE p (id INT, name TEXT); \
                 INSERT INTO p VALUES (1, NULL);",
            )
            .unwrap();
        let row = backend
            .query_row_strings("SELECT id, name FROM p WHERE id = 1", &[])
            .unwrap();
        assert_eq!(row, Some(vec!["1".to_string(), String::new()]));
    }

    #[test]
    fn rusqlite_query_row_strings_encodes_blob_with_size() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute_batch(
                "CREATE TABLE p (id INT, blob BLOB); \
                 INSERT INTO p VALUES (1, x'DEADBEEF');",
            )
            .unwrap();
        let row = backend
            .query_row_strings("SELECT id, blob FROM p WHERE id = 1", &[])
            .unwrap();
        assert_eq!(
            row,
            Some(vec!["1".to_string(), "<blob:4 bytes>".to_string()])
        );
    }

    #[test]
    fn rusqlite_query_map_strings_returns_all_rows_in_order() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute_batch(
                "CREATE TABLE p (id INT, name TEXT); \
                 INSERT INTO p VALUES (1, 'alpha'); \
                 INSERT INTO p VALUES (2, 'beta'); \
                 INSERT INTO p VALUES (3, 'gamma');",
            )
            .unwrap();
        let rows = backend
            .query_map_strings("SELECT id, name FROM p ORDER BY id", &[])
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], vec!["1".to_string(), "alpha".to_string()]);
        assert_eq!(rows[1], vec!["2".to_string(), "beta".to_string()]);
        assert_eq!(rows[2], vec!["3".to_string(), "gamma".to_string()]);
    }

    #[test]
    fn rusqlite_query_map_strings_returns_empty_on_no_match() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend.execute_batch("CREATE TABLE p (id INT);").unwrap();
        let rows = backend
            .query_map_strings("SELECT id FROM p WHERE id > ?1", &["100"])
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn rusqlite_query_row_strings_pragma_use_case() {
        // The bead's example use case: multi-column PRAGMA queries
        // (e.g., `PRAGMA wal_checkpoint(FULL)` returns 3 columns).
        // Just verify the trait surface accepts a multi-column
        // PRAGMA result without exploding.
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        let row = backend
            .query_row_strings("PRAGMA wal_checkpoint(FULL)", &[])
            .unwrap();
        // Result columns are (busy, log, checkpointed); shape
        // depends on journal mode but column count is 3.
        let row = row.expect("PRAGMA wal_checkpoint always returns one row");
        assert_eq!(row.len(), 3);
    }

    #[test]
    fn mock_query_row_strings_pops_pre_loaded_response() {
        let mock = MockBackend::new();
        mock.enqueue_row_response(Some(vec!["1".to_string(), "alpha".to_string()]));
        mock.enqueue_row_response(None);
        let r1 = mock.query_row_strings("SELECT * FROM p", &["a"]).unwrap();
        assert_eq!(r1, Some(vec!["1".to_string(), "alpha".to_string()]));
        let r2 = mock.query_row_strings("SELECT * FROM p", &[]).unwrap();
        assert_eq!(r2, None);
        // Empty queue: defaults to None.
        let r3 = mock.query_row_strings("SELECT * FROM p", &[]).unwrap();
        assert_eq!(r3, None);
        // Observed-queries log carries (sql, params) for all 3 calls.
        let observed = mock.observed_queries();
        assert_eq!(observed.len(), 3);
        assert_eq!(observed[0].0, "SELECT * FROM p");
        assert_eq!(observed[0].1, vec!["a".to_string()]);
    }

    #[test]
    fn mock_query_map_strings_pops_pre_loaded_response() {
        let mock = MockBackend::new();
        mock.enqueue_map_response(vec![
            vec!["1".to_string(), "alpha".to_string()],
            vec!["2".to_string(), "beta".to_string()],
        ]);
        let r1 = mock.query_map_strings("SELECT * FROM p", &[]).unwrap();
        assert_eq!(r1.len(), 2);
        assert_eq!(r1[0], vec!["1".to_string(), "alpha".to_string()]);
        // Empty queue: defaults to vec![].
        let r2 = mock.query_map_strings("SELECT * FROM p", &[]).unwrap();
        assert!(r2.is_empty());
    }

    #[test]
    fn encode_sqlite_value_as_string_handles_all_variants() {
        use rusqlite::types::Value;
        assert_eq!(encode_sqlite_value_as_string(&Value::Null), "");
        assert_eq!(encode_sqlite_value_as_string(&Value::Integer(42)), "42");
        assert_eq!(encode_sqlite_value_as_string(&Value::Real(1.5)), "1.5");
        assert_eq!(
            encode_sqlite_value_as_string(&Value::Text("hi".to_string())),
            "hi"
        );
        assert_eq!(
            encode_sqlite_value_as_string(&Value::Blob(vec![0, 1, 2, 3])),
            "<blob:4 bytes>"
        );
    }

    #[test]
    fn dyn_storage_backend_supports_multi_column_methods() {
        // Object-safety check: the new methods land on
        // `Box<dyn StorageBackend>` without breaking dispatch.
        let backend: Box<dyn StorageBackend> = Box::new(MockBackend::new());
        let _row = backend.query_row_strings("SELECT 1", &[]);
        let _rows = backend.query_map_strings("SELECT 1", &[]);
    }

    // ----------------------------------------------------------------
    // br-ft-qgj81 substrate-pass slice 2: ToSqlValue + typed query
    // methods.
    // ----------------------------------------------------------------

    #[test]
    fn tosql_value_canonical_string_matches_substrate_encoding() {
        assert_eq!(ToSqlValue::Null.to_canonical_string(), "");
        assert_eq!(ToSqlValue::Integer(42).to_canonical_string(), "42");
        assert_eq!(ToSqlValue::Real(1.5).to_canonical_string(), "1.5");
        assert_eq!(ToSqlValue::Text("hi").to_canonical_string(), "hi");
        assert_eq!(
            ToSqlValue::OwnedText("hello".to_string()).to_canonical_string(),
            "hello"
        );
        assert_eq!(
            ToSqlValue::Blob(&[0, 1, 2, 3]).to_canonical_string(),
            "<blob:4 bytes>"
        );
        assert_eq!(
            ToSqlValue::OwnedBlob(vec![0; 1024]).to_canonical_string(),
            "<blob:1024 bytes>"
        );
    }

    #[test]
    fn tosql_value_bool_helper_emits_integer_0_or_1() {
        assert_eq!(ToSqlValue::bool(true), ToSqlValue::Integer(1));
        assert_eq!(ToSqlValue::bool(false), ToSqlValue::Integer(0));
    }

    #[test]
    fn tosql_value_optional_helpers_route_none_to_null() {
        assert_eq!(ToSqlValue::optional_i64(Some(42)), ToSqlValue::Integer(42));
        assert_eq!(ToSqlValue::optional_i64(None), ToSqlValue::Null);
        assert_eq!(
            ToSqlValue::optional_text(Some("hi")),
            ToSqlValue::Text("hi")
        );
        assert_eq!(ToSqlValue::optional_text(None), ToSqlValue::Null);
    }

    #[test]
    fn tosql_value_from_impls_cover_native_types() {
        let string_value: ToSqlValue = "hi".into();
        assert_eq!(string_value, ToSqlValue::Text("hi"));
        let integer_value: ToSqlValue = 42_i64.into();
        assert_eq!(integer_value, ToSqlValue::Integer(42));
        let unsigned_value: ToSqlValue = 100_u32.into();
        assert_eq!(unsigned_value, ToSqlValue::Integer(100));
        let float_value: ToSqlValue = 1.5_f64.into();
        assert_eq!(float_value, ToSqlValue::Real(1.5));
        let bool_value: ToSqlValue = true.into();
        assert_eq!(bool_value, ToSqlValue::Integer(1));
        let raw: &[u8] = &[0, 1, 2];
        let blob: ToSqlValue = raw.into();
        assert_eq!(blob, ToSqlValue::Blob(&[0, 1, 2]));
    }

    #[test]
    fn query_row_typed_default_routes_through_string_path_on_rusqlite() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute_batch(
                "CREATE TABLE p (id INT, name TEXT); \
                 INSERT INTO p VALUES (1, 'alpha');",
            )
            .unwrap();
        let row = backend
            .query_row_typed(
                "SELECT id, name FROM p WHERE id = ?1",
                &[ToSqlValue::Integer(1)],
            )
            .unwrap();
        // Default impl renders Integer(1) → "1" via
        // to_canonical_string + delegates to query_row_strings.
        // RusqliteBackend's query_row_strings binds "1" as TEXT
        // — SQLite's affinity-based comparison still matches
        // the INT column's value, so this round-trips.
        assert_eq!(row, Some(vec!["1".to_string(), "alpha".to_string()]));
    }

    #[test]
    fn query_map_typed_default_routes_through_string_path_on_rusqlite() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute_batch(
                "CREATE TABLE p (id INT, name TEXT); \
                 INSERT INTO p VALUES (1, 'alpha'), (2, 'beta');",
            )
            .unwrap();
        let rows = backend
            .query_map_typed(
                "SELECT id, name FROM p WHERE id <= ?1 ORDER BY id",
                &[ToSqlValue::Integer(2)],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn query_row_typed_dispatches_through_box_dyn() {
        let backend: Box<dyn StorageBackend> = Box::new(MockBackend::new());
        let _row = backend.query_row_typed("SELECT 1", &[ToSqlValue::Null]);
    }

    #[test]
    fn tosql_value_text_borrows_lifetime_correctly() {
        // The borrow checker enforces that ToSqlValue::Text<'a>
        // can't outlive its borrow source. Compile-time check
        // via a runtime sanity test on the values.
        let owned = "hello".to_string();
        let v = ToSqlValue::Text(owned.as_str());
        match v {
            ToSqlValue::Text(s) => assert_eq!(s, "hello"),
            _ => panic!("expected Text"),
        }
    }

    // ========================================================================
    // br-ft-qgj81 slice 4: SqlCell + query_row_cells / query_map_cells
    // ========================================================================

    #[test]
    fn sql_cell_typed_accessors_only_match_their_variant() {
        assert!(SqlCell::Null.is_null());
        assert!(!SqlCell::Integer(0).is_null());

        assert_eq!(SqlCell::Integer(7).as_i64(), Some(7));
        assert_eq!(SqlCell::Real(7.5).as_i64(), None);
        assert_eq!(SqlCell::Real(7.5).as_f64(), Some(7.5));
        assert_eq!(SqlCell::Text("hi".into()).as_text(), Some("hi"));
        assert_eq!(SqlCell::Blob(vec![0xff]).as_blob(), Some(&[0xff][..]));
    }

    #[test]
    fn sql_cell_from_canonical_string_recovers_numeric_values() {
        // Empty string flattens to NULL (matches the
        // to_canonical_string contract).
        assert!(SqlCell::from_canonical_string("").is_null());
        assert_eq!(SqlCell::from_canonical_string("42"), SqlCell::Integer(42));
        assert_eq!(
            SqlCell::from_canonical_string("-9223372036854775808"),
            SqlCell::Integer(i64::MIN)
        );
        assert_eq!(
            SqlCell::from_canonical_string("3.125"),
            SqlCell::Real(3.125)
        );
        assert_eq!(
            SqlCell::from_canonical_string("hello"),
            SqlCell::Text("hello".into())
        );
        assert_eq!(
            SqlCell::from_canonical_string("<blob:4 bytes>"),
            SqlCell::Text("<blob:4 bytes>".into())
        );
        assert_eq!(
            SqlCell::from_canonical_string("0042"),
            SqlCell::Text("0042".into())
        );
    }

    #[test]
    fn sql_cell_serde_uses_kind_value_tagged_form() {
        let cells = vec![
            SqlCell::Null,
            SqlCell::Integer(7),
            SqlCell::Real(2.5),
            SqlCell::Text("hi".into()),
            SqlCell::Blob(vec![0xff]),
        ];
        for cell in &cells {
            let s = serde_json::to_string(cell).unwrap();
            let parsed: SqlCell = serde_json::from_str(&s).unwrap();
            assert_eq!(&parsed, cell);
        }
        let v: serde_json::Value = serde_json::to_value(&cells[1]).unwrap();
        assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("integer"));
        assert_eq!(v.get("value").and_then(|x| x.as_i64()), Some(7));
    }

    #[test]
    fn rusqlite_query_row_cells_preserves_each_storage_class() {
        // Native override on RusqliteBackend reads
        // rusqlite::types::Value directly; INTEGER / REAL / TEXT /
        // BLOB / NULL all round-trip into their matching SqlCell
        // variants without a string detour.
        let backend = open_memory();
        let row = backend
            .query_row_cells("SELECT NULL, 42, 3.5, 'text', x'cafe'", &[])
            .unwrap()
            .expect("row");
        assert_eq!(row.len(), 5);
        assert!(matches!(row[0], SqlCell::Null));
        assert!(matches!(row[1], SqlCell::Integer(42)));
        match &row[2] {
            SqlCell::Real(f) => assert!((f - 3.5).abs() < f64::EPSILON),
            other => panic!("expected Real, got {other:?}"),
        }
        match &row[3] {
            SqlCell::Text(s) => assert_eq!(s, "text"),
            other => panic!("expected Text, got {other:?}"),
        }
        match &row[4] {
            SqlCell::Blob(b) => assert_eq!(b, &vec![0xca, 0xfe]),
            other => panic!("expected Blob, got {other:?}"),
        }
    }

    #[test]
    fn rusqlite_query_row_cells_distinguishes_null_from_empty_text() {
        // Default-path lossiness: from_canonical_string("") returns
        // SqlCell::Null. The native override must keep NULL and
        // empty-Text distinct.
        let backend = open_memory();
        let row = backend
            .query_row_cells("SELECT NULL, ''", &[])
            .unwrap()
            .expect("row");
        assert!(row[0].is_null());
        match &row[1] {
            SqlCell::Text(s) => assert!(s.is_empty()),
            other => panic!("expected empty Text, got {other:?}"),
        }
    }

    #[test]
    fn rusqlite_query_row_cells_preserves_blob_bytes() {
        // The default path replaces blob bodies with the literal
        // sentinel "<blob:N bytes>"; the native override must
        // return the actual bytes.
        let backend = open_memory();
        backend
            .execute_batch(
                "CREATE TABLE bin (b BLOB); \
                 INSERT INTO bin VALUES (x'deadbeef');",
            )
            .unwrap();
        let row = backend
            .query_row_cells("SELECT b FROM bin", &[])
            .unwrap()
            .expect("row");
        match &row[0] {
            SqlCell::Blob(b) => assert_eq!(b, &vec![0xde, 0xad, 0xbe, 0xef]),
            other => panic!("expected Blob, got {other:?}"),
        }
    }

    #[test]
    fn rusqlite_query_row_cells_preserves_f64_tail_precision() {
        // The default path renders f64 via to_string(); some tail
        // digits round on the way out. The native override must
        // hand back the exact f64 SQLite stored.
        let backend = open_memory();
        let exact: f64 = 1.0 / 3.0;
        let row = backend
            .query_row_cells("SELECT ?1", &[ToSqlValue::Real(exact)])
            .unwrap()
            .expect("row");
        match &row[0] {
            SqlCell::Real(f) => assert!(
                (f - exact).abs() < 1e-15,
                "expected exact f64 round-trip; got delta {}",
                (f - exact).abs()
            ),
            other => panic!("expected Real, got {other:?}"),
        }
    }

    #[test]
    fn rusqlite_query_row_cells_returns_none_on_empty_match() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (x INT)").unwrap();
        let row = backend.query_row_cells("SELECT x FROM t", &[]).unwrap();
        assert!(row.is_none());
    }

    #[test]
    fn rusqlite_query_map_cells_returns_one_row_per_match() {
        let backend = open_memory();
        backend
            .execute_batch(
                "CREATE TABLE t (id INTEGER, body TEXT); \
                 INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c');",
            )
            .unwrap();
        let rows = backend
            .query_map_cells("SELECT id, body FROM t ORDER BY id", &[])
            .unwrap();
        assert_eq!(rows.len(), 3);
        for (idx, row) in rows.iter().enumerate() {
            assert_eq!(row.len(), 2);
            assert!(matches!(row[0], SqlCell::Integer(_)));
            let want_id = (idx as i64) + 1;
            assert_eq!(row[0].as_i64(), Some(want_id));
        }
    }

    #[test]
    fn rusqlite_query_map_cells_empty_table_returns_empty_vec() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (x INT)").unwrap();
        let rows = backend.query_map_cells("SELECT x FROM t", &[]).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn query_row_cells_default_impl_recovers_numeric_cells_on_mock() {
        // Mock backend doesn't override query_row_cells, so the
        // default impl runs: query_row_typed → query_row_strings
        // → from_canonical_string. Mock's enqueued strings come
        // back through the canonical parser.
        let mock = MockBackend::new();
        mock.enqueue_row_response(Some(vec![
            String::new(),
            "42".to_string(),
            "3.5".to_string(),
            "hello".to_string(),
            "<blob:4 bytes>".to_string(),
        ]));
        let row = mock
            .query_row_cells("SELECT a, b, c", &[])
            .unwrap()
            .expect("row");
        assert!(row[0].is_null());
        assert_eq!(row[1], SqlCell::Integer(42));
        assert_eq!(row[2], SqlCell::Real(3.5));
        assert_eq!(row[3], SqlCell::Text("hello".into()));
        assert_eq!(row[4], SqlCell::Text("<blob:4 bytes>".into()));
    }

    #[test]
    fn query_map_cells_default_impl_recovers_numeric_cells_on_mock() {
        let mock = MockBackend::new();
        mock.enqueue_map_response(vec![
            vec!["1".to_string(), "1.5".to_string(), "alpha".to_string()],
            vec!["2".to_string(), "2.5".to_string(), "beta".to_string()],
        ]);
        let rows = mock.query_map_cells("SELECT a, b, c", &[]).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], SqlCell::Integer(1));
        assert_eq!(rows[0][1], SqlCell::Real(1.5));
        assert_eq!(rows[0][2], SqlCell::Text("alpha".into()));
        assert_eq!(rows[1][0], SqlCell::Integer(2));
        assert_eq!(rows[1][1], SqlCell::Real(2.5));
        assert_eq!(rows[1][2], SqlCell::Text("beta".into()));
    }

    #[test]
    fn query_row_cells_dispatches_through_box_dyn() {
        let backend: Box<dyn StorageBackend> = Box::new(MockBackend::new());
        // Dyn dispatch hits the default impl (mock doesn't override).
        let _row = backend.query_row_cells("SELECT 1", &[]);
    }
}
