//! e2e: storage schema migration / upgrade flow (ft-cj3re).
//!
//! Drives the public [`initialize_schema`] entry on real (in-memory) SQLite
//! connections and asserts the upgrade contract anchored on the ft-wi24o v32
//! `segment_embeddings.embedded_at` default repair:
//!   1. a fresh DB initializes to `SCHEMA_VERSION` with the ms `embedded_at`
//!      default and no orphan / leftover rebuild state;
//!   2. re-running `initialize_schema` is an idempotent no-op;
//!   3. a DB still on the legacy seconds default (user_version 31) is repaired
//!      to ms on re-init, preserving rows;
//!   4. `initialize_schema` fails closed on a future `user_version`.
//!
//! Zero-RCH authored; runs entirely against an in-memory DB (no remote build
//! needed to author, proven via `cargo test` when the fleet is healthy).

use frankenterm_core::storage::{SCHEMA_VERSION, get_user_version, initialize_schema};
use rusqlite::Connection;

/// The `embedded_at` column DEFAULT expression for `segment_embeddings`, read
/// from `PRAGMA table_info` (column index 4), or `None` if the column is absent.
fn embedded_at_default(conn: &Connection) -> Option<String> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(segment_embeddings)")
        .expect("table_info prepare");
    let mut found = None;
    let rows = stmt
        .query_map([], |row| {
            let name: String = row.get(1)?;
            let dflt: Option<String> = row.get(4)?;
            Ok((name, dflt))
        })
        .expect("table_info query");
    for row in rows {
        let (name, dflt) = row.expect("table_info row");
        if name == "embedded_at" {
            found = dflt;
        }
    }
    found
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .expect("sqlite_master query")
        > 0
}

/// Insert one `output_segments` row (FK enforcement off so we don't have to
/// seed the whole panes→output_segments chain) and return its id.
fn seed_output_segment(conn: &Connection, seq: i64) -> i64 {
    conn.execute_batch("PRAGMA foreign_keys = OFF;")
        .expect("disable FK for seeding");
    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at)
         VALUES (1, ?1, 'x', 1, 1000)",
        [seq],
    )
    .expect("seed output_segment");
    conn.last_insert_rowid()
}

#[test]
fn fresh_db_initializes_to_head_with_ms_embedded_at_default_and_no_orphans() {
    let conn = Connection::open_in_memory().expect("in-memory sqlite");
    initialize_schema(&conn).expect("fresh init");

    // (1) fresh DB lands exactly at HEAD.
    assert_eq!(
        get_user_version(&conn).expect("user_version"),
        SCHEMA_VERSION
    );
    assert_eq!(
        SCHEMA_VERSION, 32,
        "this e2e is pinned to the v32 default repair"
    );

    // (2) segment_embeddings default is epoch ms (the v32 contract).
    let dflt = embedded_at_default(&conn).expect("embedded_at has a default");
    assert!(
        dflt.contains("1000"),
        "embedded_at default must be epoch ms (*1000), got {dflt:?}"
    );

    // (3) no leftover rebuild table and no orphan embeddings.
    assert!(
        !table_exists(&conn, "segment_embeddings_legacy"),
        "fresh init must not leave a rebuild scratch table"
    );
    let orphans: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM segment_embeddings
             WHERE segment_id NOT IN (SELECT id FROM output_segments)",
            [],
            |row| row.get(0),
        )
        .expect("orphan count");
    assert_eq!(orphans, 0, "no orphan segment_embeddings after init");

    // (2b) a default-omitting insert stores epoch ms, not seconds.
    let seg_id = seed_output_segment(&conn, 1);
    conn.execute(
        "INSERT INTO segment_embeddings (segment_id, embedder_id, dimension, vector)
         VALUES (?1, 'e', 4, X'00')",
        [seg_id],
    )
    .expect("insert omitting embedded_at");
    let stored: i64 = conn
        .query_row(
            "SELECT embedded_at FROM segment_embeddings WHERE segment_id = ?1",
            [seg_id],
            |row| row.get(0),
        )
        .expect("read defaulted embedded_at");
    assert!(
        stored >= 100_000_000_000,
        "default insert must store epoch ms (>= 1e11), got {stored}"
    );
}

#[test]
fn re_running_initialize_schema_is_idempotent_noop() {
    let conn = Connection::open_in_memory().expect("in-memory sqlite");
    initialize_schema(&conn).expect("fresh init");
    let version = get_user_version(&conn).expect("user_version");
    let default = embedded_at_default(&conn);

    // Second init on an up-to-date DB short-circuits; nothing changes.
    initialize_schema(&conn).expect("re-init is a no-op");
    assert_eq!(
        get_user_version(&conn).expect("user_version"),
        version,
        "user_version unchanged on re-run"
    );
    assert_eq!(
        embedded_at_default(&conn),
        default,
        "embedded_at default unchanged on re-run"
    );
    assert!(!table_exists(&conn, "segment_embeddings_legacy"));
}

#[test]
fn upgrade_from_seconds_default_at_v31_repairs_to_ms_preserving_rows() {
    let conn = Connection::open_in_memory().expect("in-memory sqlite");
    initialize_schema(&conn).expect("fresh init");

    // Simulate a DB upgraded through v22/v23: an output segment, then
    // segment_embeddings reverted to the LEGACY seconds default carrying a
    // v30-normalized (ms) value, with user_version stamped back to 31 (pre-v32).
    let seg_id = seed_output_segment(&conn, 9);
    conn.execute_batch(
        "DROP TABLE segment_embeddings;
         CREATE TABLE segment_embeddings (
             segment_id INTEGER NOT NULL REFERENCES output_segments(id) ON DELETE CASCADE,
             embedder_id TEXT NOT NULL,
             dimension INTEGER NOT NULL,
             vector BLOB NOT NULL,
             embedded_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
             PRIMARY KEY (segment_id, embedder_id)
         );
         CREATE INDEX IF NOT EXISTS idx_segment_embeddings_embedder
             ON segment_embeddings(embedder_id);",
    )
    .expect("revert to legacy seconds-default table");
    conn.execute(
        "INSERT INTO segment_embeddings (segment_id, embedder_id, dimension, vector, embedded_at)
         VALUES (?1, 'e', 4, X'00', 1700000000000)",
        [seg_id],
    )
    .expect("seed ms-valued row under the seconds default");
    conn.execute_batch("PRAGMA user_version = 31;")
        .expect("stamp pre-v32 version");
    assert!(
        !embedded_at_default(&conn).unwrap().contains("1000"),
        "fixture must start on the legacy seconds default"
    );

    // Re-init applies exactly v32.
    initialize_schema(&conn).expect("upgrade 31 -> 32");

    assert_eq!(
        get_user_version(&conn).expect("user_version"),
        SCHEMA_VERSION
    );
    assert!(
        embedded_at_default(&conn).unwrap().contains("1000"),
        "v32 must repair the default to epoch ms"
    );
    let kept: i64 = conn
        .query_row(
            "SELECT embedded_at FROM segment_embeddings WHERE segment_id = ?1",
            [seg_id],
            |row| row.get(0),
        )
        .expect("row preserved");
    assert_eq!(
        kept, 1_700_000_000_000,
        "existing ms row must survive the v32 rebuild"
    );
    assert!(
        !table_exists(&conn, "segment_embeddings_legacy"),
        "v32 rebuild must leave no orphan legacy table"
    );
}

#[test]
fn initialize_schema_fails_closed_on_future_version() {
    let conn = Connection::open_in_memory().expect("in-memory sqlite");
    initialize_schema(&conn).expect("fresh init");

    // A DB stamped beyond this binary's known schema must NOT be silently
    // migrated/downgraded — fail closed.
    conn.execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1))
        .expect("stamp future version");
    assert!(
        initialize_schema(&conn).is_err(),
        "initialize_schema must fail closed when user_version > SCHEMA_VERSION"
    );
}
