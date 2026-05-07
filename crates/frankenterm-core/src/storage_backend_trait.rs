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
//! - A `cfg(test)` `MockBackend` for unit tests.
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

#[cfg(test)]
use std::collections::VecDeque;
use std::path::Path;
#[cfg(test)]
use std::sync::Arc;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Capability the storage layer needs from its backing engine.
///
/// Each production implementor (rusqlite today, frankensqlite
/// tomorrow) exposes the same surface. Storage.rs
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

impl Drop for TransactionGuard<'_> {
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
#[cfg(test)]
pub struct MockBackend {
    inner: Arc<Mutex<MockState>>,
}

#[cfg(test)]
#[derive(Default)]
struct MockState {
    executed: Vec<String>,
    user_version: u32,
    in_tx: bool,
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
        let _ = sqlite_user_version_value(version)?;
        self.inner.lock().unwrap().user_version = version;
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
        let mut state = self.inner.lock().unwrap();
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
        let mut state = self.inner.lock().unwrap();
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
        let mut state = self.inner.lock().unwrap();
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
}

impl RusqliteBackend {
    /// Wrap an existing `rusqlite::Connection`. The backend
    /// takes ownership.
    pub fn new(conn: rusqlite::Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Reclaim the wrapped `rusqlite::Connection`. Used by the
    /// pooled-backend bridge in storage.rs (br-ft-3twzm) to move
    /// a `Connection` out of a `PooledReadConn` into a temporary
    /// `RusqliteBackend` for the duration of a closure, then
    /// move the `Connection` back so the pool's Drop returns it
    /// to the LIFO.
    ///
    /// Recovers the inner `Connection` even when the wrapping
    /// mutex is poisoned. Mutex poisoning happens when a previous
    /// holder panicked while holding the lock — the trait methods
    /// avoid this by not holding the `MutexGuard` across panic
    /// boundaries, but a panic from rusqlite itself (allocation
    /// failure, internal assertion) DURING an `execute`/`prepare`
    /// call still poisons. The earlier `expect("must not be
    /// poisoned")` form turned that rare case into a double-panic
    /// inside the writer-thread Drop guard at storage.rs's
    /// `with_writer_backend` (process abort under
    /// `panic = "unwind"`, redundant under `panic = "abort"`).
    /// The poisoned `Connection` itself is intact (rusqlite
    /// doesn't keep cross-statement state in the Mutex), so
    /// returning it lets the caller decide whether to retry,
    /// reset, or discard.
    #[must_use]
    pub fn into_connection(self) -> rusqlite::Connection {
        match self.conn.into_inner() {
            Ok(conn) => conn,
            Err(poisoned) => poisoned.into_inner(),
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

impl StorageBackendFactory for RusqliteBackend {
    type Backend = Self;

    fn open(path: &Path, config: OpenConfig) -> Result<Self::Backend, BackendError> {
        Self::open_path(path, &config)
    }
}

impl StorageBackend for RusqliteBackend {
    fn execute(&self, sql: &str) -> Result<usize, BackendError> {
        let conn = self.conn.lock().map_err(|_| BackendError::TxPoisoned)?;
        conn.execute(sql, [])
            .map_err(|e| BackendError::Query(e.to_string()))
    }

    fn execute_batch(&self, sql: &str) -> Result<(), BackendError> {
        let conn = self.conn.lock().map_err(|_| BackendError::TxPoisoned)?;
        conn.execute_batch(sql)
            .map_err(|e| BackendError::Query(e.to_string()))
    }

    fn query_scalar(&self, sql: &str) -> Result<Option<String>, BackendError> {
        let conn = self.conn.lock().map_err(|_| BackendError::TxPoisoned)?;
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
        let conn = self.conn.lock().map_err(|_| BackendError::TxPoisoned)?;
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|e| BackendError::Schema(e.to_string()))?;
        Ok(v.max(0) as u32)
    }

    fn set_user_version(&self, version: u32) -> Result<(), BackendError> {
        let version = sqlite_user_version_value(version)?;
        let conn = self.conn.lock().map_err(|_| BackendError::TxPoisoned)?;
        conn.pragma_update(None, "user_version", version)
            .map_err(|e| BackendError::Schema(e.to_string()))
    }

    fn backend_name(&self) -> &'static str {
        "rusqlite"
    }

    fn query_row_strings(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<Option<Vec<String>>, BackendError> {
        let conn = self.conn.lock().map_err(|_| BackendError::TxPoisoned)?;
        let mut stmt = conn
            .prepare(sql)
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
        let conn = self.conn.lock().map_err(|_| BackendError::TxPoisoned)?;
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
        let conn = self.conn.lock().map_err(|_| BackendError::TxPoisoned)?;
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
    }

    fn query_map_typed(
        &self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Vec<Vec<String>>, BackendError> {
        let conn = self.conn.lock().map_err(|_| BackendError::TxPoisoned)?;
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
        let conn = self.conn.lock().map_err(|_| BackendError::TxPoisoned)?;
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
    }

    fn query_map_cells(
        &self,
        sql: &str,
        params: &[ToSqlValue<'_>],
    ) -> Result<Vec<Vec<SqlCell>>, BackendError> {
        let conn = self.conn.lock().map_err(|_| BackendError::TxPoisoned)?;
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
        if param_rows.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock().map_err(|_| BackendError::TxPoisoned)?;
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

    fn open_memory() -> RusqliteBackend {
        RusqliteBackend::open(":memory:", &OpenConfig::default()).expect("open in-memory rusqlite")
    }

    #[test]
    fn rusqlite_backend_opens_in_memory() {
        let backend = open_memory();
        assert_eq!(backend.backend_name(), "rusqlite");
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
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        let tx = backend.begin_transaction().unwrap();
        backend.execute("INSERT INTO t VALUES (1)").unwrap();
        backend.execute("INSERT INTO t VALUES (2)").unwrap();
        tx.commit().unwrap();
        let count = backend.query_scalar("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.as_deref(), Some("2"));
    }

    #[test]
    fn rusqlite_backend_transaction_rollback_discards_inserts() {
        let backend = open_memory();
        backend.execute("CREATE TABLE t (id INTEGER)").unwrap();
        {
            let _tx = backend.begin_transaction().unwrap();
            backend.execute("INSERT INTO t VALUES (99)").unwrap();
            // _tx drops without commit — Drop runs ROLLBACK.
        }
        let count = backend.query_scalar("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.as_deref(), Some("0"));
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
        assert_eq!(row, Some(vec!["1".to_string(), "".to_string()]));
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
        let s: ToSqlValue = "hi".into();
        assert_eq!(s, ToSqlValue::Text("hi"));
        let i: ToSqlValue = 42_i64.into();
        assert_eq!(i, ToSqlValue::Integer(42));
        let u: ToSqlValue = 100_u32.into();
        assert_eq!(u, ToSqlValue::Integer(100));
        let f: ToSqlValue = 1.5_f64.into();
        assert_eq!(f, ToSqlValue::Real(1.5));
        let b: ToSqlValue = true.into();
        assert_eq!(b, ToSqlValue::Integer(1));
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
        assert_eq!(SqlCell::from_canonical_string("3.14"), SqlCell::Real(3.14));
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
            "".to_string(),
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
