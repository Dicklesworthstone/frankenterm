//! Regression-doc placeholder for br-ft-3twzm.
//!
//! The br-ft-3twzm fix migrates 9 reader paths in storage.rs to
//! `pooled_rusqlite_backend`, restoring the connection-pool
//! contract ft-bhyxz introduced. Direct in-process verification
//! that "the pool is hit, not RusqliteBackend::open" requires
//! either:
//!   - Instrumenting `rusqlite::Connection::open` (not exposed to
//!     downstream crates).
//!   - Wrapping `PooledReadConn::acquire` in a counter (substrate
//!     change beyond the scope of this fix).
//!
//! The migrated reader paths are exercised by their existing unit
//! / integration tests:
//!   - `embedding_stats`, `get_embedding`, `get_unembedded_segments`,
//!     `store_embedding` — covered by `tests/proptest_storage*.rs`
//!     + the storage handle's own #[cfg(test)] module.
//!   - `get_saved_search_by_name`, `list_saved_searches` — covered
//!     by storage's saved-search test cluster.
//!   - `get_gaps`, `retention_cleanup_count`, `segment_time_range` —
//!     covered by storage's reader-path tests.
//!
//! All those tests pass after the migration because the closure
//! body inside each `pooled_rusqlite_backend` call is byte-identical
//! to the pre-migration body — only the backend acquisition path
//! changed.
//!
//! The structural verification this file would have shipped (no
//! `open_rusqlite_backend` callers remain in storage.rs) is a
//! grep at commit time:
//!
//! ```text
//! $ grep -c 'open_rusqlite_backend' crates/frankenterm-core/src/storage.rs
//! 1   # only the helper definition remains; all 9 call sites
//!     # migrated to pooled_rusqlite_backend
//! ```
//!
//! See br-ft-3twzm for the full fix description; this stub file
//! exists so a future agent searching the test suite for the bead
//! ID finds the cross-reference.
