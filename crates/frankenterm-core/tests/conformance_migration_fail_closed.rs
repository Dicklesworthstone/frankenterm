//! Fail-closed coverage for future schema versions in migration tooling (ft-men4p).
//!
//! A DB whose `PRAGMA user_version` exceeds this binary's `SCHEMA_VERSION` must be
//! rejected by every migration planning/tooling entry point — never reported as
//! an empty "already current" plan. Before the ft-men4p guard, a future
//! `user_version` fell into `build_migration_plan`'s rollback branch, found no
//! known migrations above `SCHEMA_VERSION`, and returned an empty plan.
//!
//! These tests drive the public `migration_plan_for_path` entry point against a
//! real on-disk DB, so they exercise the full read-user_version -> plan path.
//! Zero-RCH; pure SQLite + the public migration API.

use std::path::{Path, PathBuf};

use frankenterm_core::storage::{SCHEMA_SQL, SCHEMA_VERSION, migration_plan_for_path};
use rusqlite::Connection;
use tempfile::TempDir;

/// Create an initialized DB file whose `PRAGMA user_version` is `user_version`.
///
/// `SCHEMA_SQL` initializes the tables (so the path is not treated as
/// uninitialized) and stamps `user_version = SCHEMA_VERSION`; the explicit
/// PRAGMA then overrides it to model an older/newer database.
fn db_with_user_version(dir: &Path, name: &str, user_version: i32) -> PathBuf {
    let path = dir.join(name);
    let conn = Connection::open(&path).expect("open temp db");
    conn.execute_batch(SCHEMA_SQL).expect("apply schema");
    conn.execute_batch(&format!("PRAGMA user_version = {user_version};"))
        .expect("set user_version");
    drop(conn);
    path
}

#[test]
fn migration_plan_for_path_rejects_any_future_user_version() {
    let dir = TempDir::new().expect("tempdir");
    // Property-style sweep: every future version, including a far-future one,
    // must fail closed rather than plan as already-current.
    for offset in [1_i32, 2, 5, 50, 1000] {
        let future = SCHEMA_VERSION + offset;
        let path = db_with_user_version(dir.path(), &format!("future-{future}.db"), future);

        let err = migration_plan_for_path(&path, SCHEMA_VERSION)
            .expect_err("a future user_version must fail closed, not plan as already-current");
        let message = err.to_string();

        // Golden on the canonical SchemaTooNew error shape: it cites the future
        // (current) version and the "newer than supported" wording.
        assert!(
            message.contains(&future.to_string()) && message.contains("newer than supported"),
            "future user_version {future} must surface the SchemaTooNew fail-closed error, \
             got: {message}"
        );
    }
}

#[test]
fn migration_plan_for_path_rejects_future_even_when_targeting_an_older_version() {
    // A future DB targeted DOWN to an older version is still refused (the guard
    // is on the source version, not the target).
    let dir = TempDir::new().expect("tempdir");
    let future = SCHEMA_VERSION + 1;
    let path = db_with_user_version(dir.path(), "future-downgrade.db", future);

    let target = (SCHEMA_VERSION - 1).max(1);
    let result = migration_plan_for_path(&path, target);
    assert!(
        result.is_err(),
        "a future DB must be refused regardless of target, got: {:?}",
        result.ok()
    );
}

#[test]
fn migration_plan_for_path_accepts_current_version_without_error() {
    // Control: a DB at exactly SCHEMA_VERSION plans cleanly — the guard must not
    // over-reject the supported version.
    let dir = TempDir::new().expect("tempdir");
    let path = db_with_user_version(dir.path(), "current.db", SCHEMA_VERSION);

    let result = migration_plan_for_path(&path, SCHEMA_VERSION);
    assert!(
        result.is_ok(),
        "a DB at the current schema version must plan, not error: {:?}",
        result.err()
    );
}
