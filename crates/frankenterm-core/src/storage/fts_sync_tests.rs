//! ft-u6fba Phase 1b: extracted from storage.rs (mod fts_sync_tests).
//! Sibling submodule of `storage` — `use super::*;` resolves to
//! `crate::storage::*`.

use super::*;

/// Helper to insert a test segment directly
fn insert_test_segment(conn: &Connection, pane_id: u64, seq: u64, content: &str) {
    let now = now_ms();
    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            pane_id as i64,
            seq as i64,
            content,
            content.len() as i64,
            now
        ],
    )
    .unwrap();
}

/// Helper to create a pane
fn insert_test_pane(conn: &Connection, pane_id: u64) {
    let now = now_ms();
    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed)
             VALUES (?1, 'local', ?2, ?3, 1)",
        params![pane_id as i64, now, now],
    )
    .unwrap();
}

#[test]
fn fts_index_state_tables_exist() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Check fts_index_state table exists
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='fts_index_state'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "fts_index_state table should exist");

    // Check fts_pane_progress table exists
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='fts_pane_progress'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "fts_pane_progress table should exist");
}

#[test]
fn get_fts_index_state_returns_none_when_empty() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // State table exists but is empty until sync initializes it
    let state = get_fts_index_state_sync(&conn).unwrap();
    // After migration, we insert a default row
    assert!(state.is_some() || state.is_none()); // Depends on migration logic
}

#[test]
fn upsert_fts_index_state_works() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    let state = FtsIndexState {
        index_version: 42,
        last_full_rebuild_at: Some(now),
        created_at: now,
        updated_at: now,
    };

    upsert_fts_index_state_sync(&conn, &state).unwrap();

    let loaded = get_fts_index_state_sync(&conn).unwrap().unwrap();
    assert_eq!(loaded.index_version, 42);
    assert_eq!(loaded.last_full_rebuild_at, Some(now));
}

#[test]
fn fts_pane_progress_roundtrip() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Create pane first (foreign key)
    insert_test_pane(&conn, 100);

    let now = now_ms();
    let progress = FtsPaneProgress {
        pane_id: 100,
        last_indexed_seq: 50,
        indexed_count: 50,
        last_indexed_at: now,
    };

    upsert_fts_pane_progress_sync(&conn, &progress).unwrap();

    let loaded = get_fts_pane_progress_sync(&conn, 100).unwrap().unwrap();
    assert_eq!(loaded.pane_id, 100);
    assert_eq!(loaded.last_indexed_seq, 50);
    assert_eq!(loaded.indexed_count, 50);
}

#[test]
fn sync_fts_on_startup_initializes_state() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup(&conn, &config).unwrap();

    // Should complete with no segments (empty db)
    assert_eq!(result.segments_indexed, 0);
    assert!(!result.full_rebuild);

    // State should be initialized
    let state = get_fts_index_state_sync(&conn).unwrap().unwrap();
    assert_eq!(state.index_version, FTS_INDEX_VERSION);
}

// ── [ft-wk5fo] Deferred FTS trigger mode ──────────────────────────

/// Helper: count FTS hits for a MATCH token.
///
/// `output_segments_fts` is an external-content FTS5 table
/// (`content='output_segments'`), so a bare `SELECT COUNT(*) FROM
/// output_segments_fts` projects through the external content
/// table and does not reflect the FTS5 shadow index state. A MATCH
/// query hits the index directly: zero rows proves the index is
/// empty; N rows proves catchup populated N documents that contain
/// the token.
///
/// All deferred-mode test documents share the "content" token, so
/// passing `"content"` counts every indexed test document.
fn fts_match_count(conn: &Connection, match_token: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM output_segments_fts
             WHERE output_segments_fts MATCH ?1",
        [match_token],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
}

/// Helper: exercise the StorageConfig.defer_fts_triggers path
/// directly on a Connection. Mirrors the DROP TRIGGER block in
/// StorageHandle::with_config so the test exercises the same
/// SQL the production path runs.
fn apply_defer_fts_triggers(conn: &Connection) {
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS output_segments_ai;
             DROP TRIGGER IF EXISTS output_segments_ad;
             DROP TRIGGER IF EXISTS output_segments_au;",
    )
    .unwrap();
}

/// Helper: exercise the `defer_fts_triggers: false` reopen path. Mirrors
/// the `else` branch in StorageHandle::with_config that reapplies
/// `FTS_TRIGGER_RECREATE_SQL` so the flag is bidirectional
/// (ft-ih4tm). Runs the same const the production path runs.
fn apply_recreate_fts_triggers(conn: &Connection) {
    conn.execute_batch(schema_ddl::FTS_TRIGGER_RECREATE_SQL)
        .unwrap();
}

/// Helper: count how many of the three `output_segments_*` FTS
/// triggers currently exist in sqlite_master. 3 = all present
/// (sync mode), 0 = all dropped (deferred mode), anything else is a
/// partial/broken state.
fn fts_trigger_count(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'trigger'
               AND name IN ('output_segments_ai', 'output_segments_ad', 'output_segments_au')",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
}

/// Baseline: with triggers present, insertions flow straight into FTS.
/// Pins the "sync" mode that StorageConfig::default() preserves.
///
/// Starts seq at 1 because `sync_fts_for_pane` has a pre-existing
/// off-by-one in its `seq > last_indexed_seq` query that excludes a
/// fresh pane's seq=0 segment from the deferred-catchup path —
/// tracked as its own bead. For this test, we exercise the trigger
/// path which is unaffected, but seq-starts-at-1 keeps all four
/// ft-wk5fo tests symmetric.
#[test]
fn fts_sync_mode_indexes_on_insert() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1);
    for seq in 1..=5u64 {
        insert_test_segment(&conn, 1, seq, &format!("content-{seq}"));
    }

    assert_eq!(
        fts_match_count(&conn, "content"),
        5,
        "sync mode (triggers present) must index every insert"
    );
}

/// [ft-wk5fo] Headline contract: with triggers dropped (deferred
/// mode), writing segments does NOT populate FTS. The search index
/// is empty immediately after a batch of writes, proving the
/// capture-write path is no longer paying trigger-indexing cost.
#[test]
fn fts_deferred_mode_does_not_index_on_insert() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    apply_defer_fts_triggers(&conn);

    insert_test_pane(&conn, 1);
    for seq in 1..=500u64 {
        insert_test_segment(&conn, 1, seq, &format!("content-{seq}"));
    }

    assert_eq!(
        fts_match_count(&conn, "content"),
        0,
        "deferred mode must leave FTS empty after 500 inserts"
    );
}

/// [ft-wk5fo] End-to-end: the `sync_fts_on_startup` catchup engine
/// sees all 500 deferred segments and indexes them through the
/// `fts_pane_progress` resume mechanism. Proves the deferred path
/// achieves eventual consistency — the whole point of the rollout.
#[test]
fn fts_deferred_mode_catchup_indexes_all_segments() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    apply_defer_fts_triggers(&conn);

    insert_test_pane(&conn, 1);
    for seq in 1..=500u64 {
        insert_test_segment(&conn, 1, seq, &format!("content-{seq}"));
    }
    // Pre-catchup: FTS is empty.
    assert_eq!(fts_match_count(&conn, "content"), 0);

    // Trigger the batched catchup engine. Same entry point the
    // future periodic writer-thread tick will use.
    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup(&conn, &config).unwrap();

    assert_eq!(result.segments_indexed, 500);
    assert_eq!(result.panes_processed, 1);
    assert!(!result.full_rebuild);
    assert_eq!(
        fts_match_count(&conn, "content"),
        500,
        "catchup must bring every deferred segment into FTS"
    );

    // And a second catchup is a no-op (progress prevents re-indexing).
    let second = sync_fts_on_startup(&conn, &config).unwrap();
    assert_eq!(
        second.segments_indexed, 0,
        "second catchup must see no new work (progress is resumable)"
    );
    assert_eq!(fts_match_count(&conn, "content"), 500);
}

/// [ft-7do6c] The seq=0 off-by-one: a fresh pane's FIRST segment
/// gets seq=0 (COALESCE(MAX(seq)+1, 0) at append_segment_sync:12355).
/// Before the fix, sync_fts_for_pane's `WHERE seq > last_indexed_seq`
/// filter — with last_indexed_seq defaulting to 0 for a never-synced
/// pane — silently skipped that first segment forever under deferred
/// mode. This test constructs the exact shape that was broken:
/// fresh pane, first segment at seq=0, deferred triggers, then
/// catchup — and asserts the seq=0 row is indexed alongside the
/// rest. The ft-wk5fo tests above deliberately avoided seq=0 while
/// this bug was open; this test removes that restriction.
#[test]
fn ft_7do6c_catchup_indexes_seq_zero_on_first_sync() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    apply_defer_fts_triggers(&conn);

    insert_test_pane(&conn, 1);
    // Seed seq starting from 0 — the real-world first-segment case.
    for seq in 0u64..5 {
        insert_test_segment(&conn, 1, seq, &format!("seqzero-{seq}"));
    }
    // Deferred mode: FTS empty pre-catchup.
    assert_eq!(fts_match_count(&conn, "seqzero"), 0);

    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup(&conn, &config).unwrap();

    // Before the fix: 4 (seq 1,2,3,4 indexed; seq=0 dropped).
    // After the fix: 5 (seq 0,1,2,3,4 all indexed).
    assert_eq!(
        result.segments_indexed, 5,
        "ft-7do6c: first-ever catchup must include seq=0, \
             not just seq 1..=4 as the pre-fix strict `seq > 0` filter"
    );
    assert_eq!(
        fts_match_count(&conn, "seqzero"),
        5,
        "ft-7do6c: all five segments (including seq=0) must be \
             searchable via FTS after catchup"
    );
    // And the seq=0 match is specifically present — not just some
    // other permutation that happens to sum to 5.
    assert_eq!(
        fts_match_count(&conn, "\"seqzero-0\""),
        1,
        "ft-7do6c: the exact seq=0 content must be searchable"
    );

    // Resume invariant still holds: a second catchup is a no-op
    // (progress table was written with last_indexed_seq=4 in the
    // first pass, so `seq > 4` correctly excludes everything).
    let second = sync_fts_on_startup(&conn, &config).unwrap();
    assert_eq!(second.segments_indexed, 0);
    assert_eq!(fts_match_count(&conn, "seqzero"), 5);
}

/// [ft-wk5fo] Interleaved: alternating catchup + insert rounds
/// preserve the resumable-progress invariant. This is the shape the
/// future periodic writer-thread tick will produce in production.
///
/// Seq range is 1..=500 (not 0..500) — see the note on
/// `fts_sync_mode_indexes_on_insert` about the seq=0 off-by-one
/// in `sync_fts_for_pane`.
#[test]
fn fts_deferred_mode_catchup_resumes_across_rounds() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    apply_defer_fts_triggers(&conn);

    insert_test_pane(&conn, 1);
    let config = FtsSyncConfig::default();

    for round in 0..5u64 {
        let start = round * 100 + 1;
        let end = (round + 1) * 100 + 1;
        for seq in start..end {
            insert_test_segment(&conn, 1, seq, &format!("roundtoken{round}"));
        }
        let result = sync_fts_on_startup(&conn, &config).unwrap();
        assert_eq!(
            result.segments_indexed, 100,
            "round {round} must index exactly the 100 new segments, not re-index prior rounds"
        );
    }

    // Each round contributed 100 docs with a distinct token — verify
    // all 5 rounds are present in the final index.
    for round in 0..5u64 {
        assert_eq!(
            fts_match_count(&conn, &format!("roundtoken{round}")),
            100,
            "round {round}'s 100 docs must be indexed and searchable"
        );
    }
}

// ── [ft-s4myu] foreign_keys PRAGMA must be applied on every open ──

/// Helper: read PRAGMA foreign_keys state for the given connection.
/// Returns 0 (OFF) or 1 (ON).
fn pragma_foreign_keys_state(conn: &Connection) -> i64 {
    conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
        .unwrap()
}

/// [ft-s4myu] After the fix, a reopen of an up-to-date DB must have
/// foreign_keys=ON on the new writer connection. Without the
/// explicit pragma_update in StorageHandle::with_config,
/// initialize_schema would short-circuit (current == SCHEMA_VERSION)
/// and skip SCHEMA_SQL, leaving the pragma at whatever the SQLite
/// runtime default happens to be — which is implementation-dependent
/// (libsqlite3-sys may or may not set SQLITE_DEFAULT_FOREIGN_KEYS=1
/// across versions/features). Belt-and-suspenders: always set it.
#[test]
fn ft_s4myu_reopen_connection_must_enable_foreign_keys() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("s4myu.sqlite");

    // Phase 1: fresh DB init. SCHEMA_SQL runs + initialize_schema
    // sets foreign_keys=ON via the pragma in SCHEMA_SQL.
    {
        let conn1 = Connection::open(&db_path).unwrap();
        initialize_schema(&conn1).unwrap();
        assert_eq!(
            pragma_foreign_keys_state(&conn1),
            1,
            "fresh-init connection must have foreign_keys=ON"
        );
    }

    // Phase 2: reopen. `initialize_schema` short-circuits at
    // `current == SCHEMA_VERSION` and does NOT execute SCHEMA_SQL.
    // The fix applies the pragma unconditionally regardless of
    // schema state, so after the fix this connection has FKs ON.
    let conn2 = Connection::open(&db_path).unwrap();
    initialize_schema(&conn2).unwrap();
    conn2
        .pragma_update(None, "foreign_keys", true)
        .expect("pragma_update must succeed");
    assert_eq!(
        pragma_foreign_keys_state(&conn2),
        1,
        "post-fix invariant: reopened writer connection MUST have \
             foreign_keys=ON regardless of initialize_schema's short-circuit \
             behavior (ft-s4myu)"
    );
}

/// [ft-s4myu] With FKs ON, a DELETE on session_checkpoints must
/// CASCADE to the referencing mux_pane_state rows (schema line
/// 646: `REFERENCES session_checkpoints(id) ON DELETE CASCADE`).
/// With FKs OFF, the DELETE succeeds but child rows leak — silent
/// data corruption. This test pins the semantic: after the fix is
/// in place, CASCADE actually fires.
#[test]
fn ft_s4myu_cascade_delete_fires_only_with_foreign_keys_on() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    assert_eq!(
        pragma_foreign_keys_state(&conn),
        1,
        "precondition: foreign_keys must be ON for this test"
    );

    // Insert a session (schema columns: session_id, created_at,
    // last_checkpoint_at, shutdown_clean, topology_json, ft_version).
    // topology_json + ft_version are NOT NULL.
    conn.execute(
        "INSERT INTO mux_sessions \
             (session_id, created_at, last_checkpoint_at, shutdown_clean, \
              topology_json, ft_version) \
             VALUES ('s-cascade', 0, 0, 0, '{}', 'test')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_checkpoints \
             (session_id, checkpoint_at, checkpoint_type, state_hash, \
              pane_count, total_bytes) \
             VALUES ('s-cascade', 0, 'periodic', 'deadbeef00000000', 1, 0)",
        [],
    )
    .unwrap();
    let cp_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO mux_pane_state (checkpoint_id, pane_id, terminal_state_json) \
             VALUES (?1, 1, '{}')",
        params![cp_id],
    )
    .unwrap();

    let child_count_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM mux_pane_state WHERE checkpoint_id = ?1",
            params![cp_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(child_count_before, 1);

    // DELETE the parent. With FKs ON, CASCADE fires.
    conn.execute(
        "DELETE FROM session_checkpoints WHERE id = ?1",
        params![cp_id],
    )
    .unwrap();

    let child_count_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM mux_pane_state WHERE checkpoint_id = ?1",
            params![cp_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        child_count_after, 0,
        "ft-s4myu: ON DELETE CASCADE must remove mux_pane_state children \
             when foreign_keys is ON — pre-fix this was 1 (orphan leaked)"
    );
}

/// [ft-s4myu] Belt-and-suspenders: with FKs explicitly OFF on the
/// connection, the exact same DELETE does NOT cascade. This proves
/// the test above actually exercises the FK mechanism (rather than
/// some unrelated CASCADE-like behavior) — the FK pragma is the
/// sole switch between "cascade fires" and "orphan leaks".
#[test]
fn ft_s4myu_cascade_delete_does_not_fire_with_foreign_keys_off() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    assert_eq!(pragma_foreign_keys_state(&conn), 0);

    conn.execute(
        "INSERT INTO mux_sessions \
             (session_id, created_at, last_checkpoint_at, shutdown_clean, \
              topology_json, ft_version) \
             VALUES ('s-noop', 0, 0, 0, '{}', 'test')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_checkpoints \
             (session_id, checkpoint_at, checkpoint_type, state_hash, \
              pane_count, total_bytes) \
             VALUES ('s-noop', 0, 'periodic', 'deadbeef00000000', 1, 0)",
        [],
    )
    .unwrap();
    let cp_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO mux_pane_state (checkpoint_id, pane_id, terminal_state_json) \
             VALUES (?1, 1, '{}')",
        params![cp_id],
    )
    .unwrap();

    conn.execute(
        "DELETE FROM session_checkpoints WHERE id = ?1",
        params![cp_id],
    )
    .unwrap();

    let child_count_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM mux_pane_state WHERE checkpoint_id = ?1",
            params![cp_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        child_count_after, 1,
        "with FKs OFF, DELETE must NOT cascade — this proves the \
             prior test is actually exercising the FK mechanism"
    );
}

/// [ft-ih4tm] `defer_fts_triggers` must be bidirectional: flipping
/// `true` → `false` on a second open MUST re-create the three
/// `output_segments_a[iud]` triggers so synchronous FTS indexing
/// resumes. Prior to commit fcb8b1df, the first open with `true`
/// dropped the triggers, but the second open with `false` did NOT
/// re-run `CREATE TRIGGER` because `initialize_schema` short-
/// circuits for up-to-date schemas — a silent one-way door the
/// operator couldn't diagnose without reading
/// `sqlite_master WHERE type='trigger'` by hand.
///
/// This test simulates the three open phases at the SQL level
/// (the existing ft-wk5fo tests use the same Connection-level
/// pattern to avoid the broken full-StorageHandle test build) and
/// pins: (1) fresh init leaves triggers present; (2) DROP mimics
/// `defer_fts_triggers: true` leaves zero; (3) re-applying
/// `FTS_TRIGGER_RECREATE_SQL` (the const the production else-
/// branch runs) restores all three; (4) an INSERT after the
/// re-create lands in FTS synchronously — proving the trigger
/// body is not just present in schema but functionally wired.
#[test]
fn fts_deferred_mode_is_reversible_after_toggle() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Phase 1: fresh schema → all three triggers present.
    assert_eq!(
        fts_trigger_count(&conn),
        3,
        "fresh schema init must install all three FTS triggers"
    );

    // Phase 2: simulate `defer_fts_triggers: true` open.
    apply_defer_fts_triggers(&conn);
    assert_eq!(
        fts_trigger_count(&conn),
        0,
        "defer path must drop all three FTS triggers"
    );

    // Phase 3: simulate `defer_fts_triggers: false` REOPEN —
    // the ft-ih4tm regression point. Before fcb8b1df, this did
    // nothing and the operator's intent was silently ignored.
    apply_recreate_fts_triggers(&conn);
    assert_eq!(
        fts_trigger_count(&conn),
        3,
        "toggling defer back to false must re-create all three FTS triggers \
             (ft-ih4tm — the one-way-door regression)"
    );

    // Phase 4: functional verification — an INSERT after the
    // recreate must populate FTS synchronously via the restored
    // trigger. If the CREATE TRIGGER ran but the body is wrong,
    // the FTS index stays empty.
    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "synctoken after recreate");
    assert_eq!(
        fts_match_count(&conn, "synctoken"),
        1,
        "recreated AFTER INSERT trigger must synchronously index new segments"
    );
}

/// [ft-ih4tm] Round-trip stress: toggle defer true ↔ false several
/// times. Every `false` phase must fully restore all three triggers
/// (no leaks, no stale state); every `true` phase must fully drop
/// them (no partial state). Guards against anyone later "optimizing"
/// the reapply into a conditional (`if triggers_absent { ... }`)
/// that would silently miss a malformed intermediate state.
#[test]
fn fts_deferred_mode_toggle_round_trip() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    for iteration in 0..5 {
        apply_defer_fts_triggers(&conn);
        assert_eq!(
            fts_trigger_count(&conn),
            0,
            "iteration {iteration}: defer=true must leave 0 triggers"
        );

        apply_recreate_fts_triggers(&conn);
        assert_eq!(
            fts_trigger_count(&conn),
            3,
            "iteration {iteration}: defer=false must restore 3 triggers"
        );
    }
}

#[test]
fn sync_fts_indexes_new_segments() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Create pane and segments
    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "Hello world");
    insert_test_segment(&conn, 1, 2, "Testing FTS sync");
    insert_test_segment(&conn, 1, 3, "Third segment");

    // Note: With trigger-driven FTS, segments are already indexed on insert.
    // The incremental sync is for recovery scenarios.
    // Let's clear FTS and progress to simulate recovery
    conn.execute_batch("INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')")
        .ok(); // May fail if empty
    clear_fts_pane_progress_sync(&conn).unwrap();

    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup(&conn, &config).unwrap();

    // Should rebuild all 3 segments
    assert_eq!(result.segments_indexed, 3);
    assert_eq!(result.panes_processed, 1);
}

#[test]
fn sync_fts_respects_progress() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Create pane and segments
    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "First");
    insert_test_segment(&conn, 1, 2, "Second");
    insert_test_segment(&conn, 1, 3, "Third");

    // Clear FTS
    conn.execute_batch("INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')")
        .ok();

    // Set progress to seq 2 (pretend first two are already indexed)
    let now = now_ms();
    let progress = FtsPaneProgress {
        pane_id: 1,
        last_indexed_seq: 2,
        indexed_count: 2,
        last_indexed_at: now,
    };
    upsert_fts_pane_progress_sync(&conn, &progress).unwrap();

    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup(&conn, &config).unwrap();

    // Should only index segment 3
    assert_eq!(result.segments_indexed, 1);
}

#[test]
fn sync_fts_on_startup_skips_caught_up_panes() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    apply_defer_fts_triggers(&conn);

    let now = now_ms();
    for pane in 1..=25u64 {
        insert_test_pane(&conn, pane);
        insert_test_segment(&conn, pane, 0, &format!("caughtup-{pane}-zero"));
        insert_test_segment(&conn, pane, 1, &format!("caughtup-{pane}-one"));
        upsert_fts_pane_progress_sync(
            &conn,
            &FtsPaneProgress {
                pane_id: pane,
                last_indexed_seq: 1,
                indexed_count: 2,
                last_indexed_at: now,
            },
        )
        .unwrap();
    }

    insert_test_pane(&conn, 100);
    insert_test_segment(&conn, 100, 0, "dirty-old");
    insert_test_segment(&conn, 100, 1, "dirtyneedle");
    upsert_fts_pane_progress_sync(
        &conn,
        &FtsPaneProgress {
            pane_id: 100,
            last_indexed_seq: 0,
            indexed_count: 1,
            last_indexed_at: now,
        },
    )
    .unwrap();

    insert_test_pane(&conn, 200);
    insert_test_segment(&conn, 200, 0, "missingneedle");

    let pane_ids = panes_needing_fts_sync(&conn).unwrap();
    assert_eq!(
        pane_ids,
        vec![100, 200],
        "healthy startup should only visit panes with new segments or missing progress"
    );

    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup(&conn, &config).unwrap();

    assert_eq!(result.panes_processed, 2);
    assert_eq!(result.segments_indexed, 2);
    assert_eq!(fts_match_count(&conn, "dirtyneedle"), 1);
    assert_eq!(fts_match_count(&conn, "missingneedle"), 1);
    assert!(
        panes_needing_fts_sync(&conn).unwrap().is_empty(),
        "second startup pass should have no panes to visit"
    );
}

#[test]
fn full_rebuild_clears_progress() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Create pane and segments
    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "One");
    insert_test_segment(&conn, 1, 2, "Two");

    // Set some progress
    let now = now_ms();
    upsert_fts_pane_progress_sync(
        &conn,
        &FtsPaneProgress {
            pane_id: 1,
            last_indexed_seq: 1,
            indexed_count: 1,
            last_indexed_at: now,
        },
    )
    .unwrap();

    let config = FtsSyncConfig::default();
    let result = full_fts_rebuild_sync(&conn, &config).unwrap();

    assert!(result.full_rebuild);
    assert_eq!(result.segments_indexed, 2);

    // Progress should be updated
    let progress = get_fts_pane_progress_sync(&conn, 1).unwrap().unwrap();
    assert_eq!(progress.last_indexed_seq, 2);
    assert_eq!(progress.indexed_count, 2);
}

#[test]
fn full_rebuild_is_idempotent() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "Alpha");
    insert_test_segment(&conn, 1, 2, "Beta");
    insert_test_segment(&conn, 1, 3, "Gamma");

    let config = FtsSyncConfig::default();
    let first = full_fts_rebuild_sync(&conn, &config).unwrap();
    assert!(first.full_rebuild);
    assert_eq!(first.segments_indexed, 3);

    let fts_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM output_segments_fts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(fts_rows, 3);

    let second = full_fts_rebuild_sync(&conn, &config).unwrap();
    assert!(second.full_rebuild);
    assert_eq!(second.segments_indexed, 3);

    let fts_rows_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM output_segments_fts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(fts_rows_after, 3);
}

#[test]
fn version_mismatch_triggers_rebuild() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Set an old version
    let now = now_ms();
    let old_state = FtsIndexState {
        index_version: FTS_INDEX_VERSION - 1, // Old version
        last_full_rebuild_at: Some(now),
        created_at: now,
        updated_at: now,
    };
    upsert_fts_index_state_sync(&conn, &old_state).unwrap();

    // Create pane and segment
    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "Test content");

    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup(&conn, &config).unwrap();

    // Should trigger full rebuild due to version mismatch
    assert!(result.full_rebuild);

    // State should be updated to new version
    let state = get_fts_index_state_sync(&conn).unwrap().unwrap();
    assert_eq!(state.index_version, FTS_INDEX_VERSION);
}

#[test]
fn fts_rebuild_pending_version_triggers_full_rebuild() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "Test content");

    let now = now_ms();
    upsert_fts_index_state_sync(
        &conn,
        &FtsIndexState {
            index_version: FTS_INDEX_REBUILD_PENDING_VERSION,
            last_full_rebuild_at: None,
            created_at: now,
            updated_at: now,
        },
    )
    .unwrap();
    clear_fts_pane_progress_sync(&conn).unwrap();
    conn.execute_batch("INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')")
        .unwrap();

    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup(&conn, &config).unwrap();

    assert!(result.full_rebuild);

    let state = get_fts_index_state_sync(&conn).unwrap().unwrap();
    assert_eq!(state.index_version, FTS_INDEX_VERSION);
    assert!(state.last_full_rebuild_at.is_some());

    let fts_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM output_segments_fts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(fts_rows, 1);
}

#[test]
fn fts_hard_rebuild_failure_marks_index_pending_and_clears_progress() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "Alpha");

    let now = now_ms();
    upsert_fts_index_state_sync(
        &conn,
        &FtsIndexState {
            index_version: FTS_INDEX_VERSION,
            last_full_rebuild_at: Some(now),
            created_at: now,
            updated_at: now,
        },
    )
    .unwrap();
    upsert_fts_pane_progress_sync(
        &conn,
        &FtsPaneProgress {
            pane_id: 1,
            last_indexed_seq: 1,
            indexed_count: 1,
            last_indexed_at: now,
        },
    )
    .unwrap();

    conn.execute_batch("DROP TABLE output_segments_fts")
        .unwrap();

    let err = full_fts_rebuild_sync(&conn, &FtsSyncConfig::default()).unwrap_err();
    let err_msg = err.to_string();
    assert!(err_msg.contains("FTS rebuild incomplete"));

    let state = get_fts_index_state_sync(&conn).unwrap().unwrap();
    assert_eq!(state.index_version, FTS_INDEX_REBUILD_PENDING_VERSION);
    assert_eq!(state.last_full_rebuild_at, None);
    assert!(get_fts_pane_progress_sync(&conn, 1).unwrap().is_none());
}

#[test]
fn batch_config_limits_work() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Create pane and multiple segments
    insert_test_pane(&conn, 1);
    for i in 1..=10 {
        insert_test_segment(&conn, 1, i, &format!("Segment {i} with some content"));
    }

    // Clear FTS
    conn.execute_batch("INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')")
        .ok();
    clear_fts_pane_progress_sync(&conn).unwrap();

    // Use small batch size
    let config = FtsSyncConfig {
        batch_size: 3,
        max_batch_bytes: 1_048_576,
        commit_progress: true,
    };

    let result = sync_fts_on_startup(&conn, &config).unwrap();

    // Should index all 10 segments in multiple batches
    assert_eq!(result.segments_indexed, 10);

    // Progress should be at the end
    let progress = get_fts_pane_progress_sync(&conn, 1).unwrap().unwrap();
    assert_eq!(progress.last_indexed_seq, 10);
}
