//! Regression coverage for br-ft-3twzm / ft-l1jgo read-pool migration.
//!
//! The br-ft-3twzm fix migrates 9 reader paths in storage.rs to
//! `pooled_backend`, restoring the connection-pool contract ft-bhyxz
//! introduced. Direct in-process verification
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
//! body inside each `pooled_backend` call is byte-identical
//! to the pre-migration body — only the backend acquisition path
//! changed.
//!
//! The structural verification below keeps the ft-l1jgo acceptance
//! invariant pinned: `storage.rs` must not grow direct
//! `rusqlite::Connection` references again.

use std::fs;

#[test]
fn storage_rs_keeps_direct_connection_refs_out_of_handle_surface() {
    let storage_path = format!("{}/src/storage.rs", env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(&storage_path)
        .unwrap_or_else(|err| panic!("read {storage_path}: {err}"));

    for forbidden in [
        "rusqlite::Connection",
        "rusqlite::{Connection",
        "Connection::open",
        "Connection::open_in_memory",
        "&Connection",
        "&mut Connection",
        "Deref<Target = Connection>",
    ] {
        assert!(
            !source.contains(forbidden),
            "storage.rs must route direct Connection access through StorageBackend; found {forbidden:?}"
        );
    }
}
