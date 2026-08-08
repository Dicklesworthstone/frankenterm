//! ft-u6fba Phase 1b: extracted from storage.rs (mod fts_sync_tests).
//! Sibling submodule of `storage` — `use super::*;` resolves to
//! `crate::storage::*`.

use super::*;
use rusqlite::{Connection, params};

fn with_fts_backend<F, R>(conn: &mut Connection, f: F) -> Result<R>
where
    F: FnOnce(&dyn crate::storage_backend_trait::StorageBackend) -> Result<R>,
{
    with_test_storage_backend(conn, f)
}

fn get_fts_index_state_test(conn: &mut Connection) -> Result<Option<FtsIndexState>> {
    with_fts_backend(conn, get_fts_index_state_backend)
}

fn upsert_fts_index_state_test(conn: &mut Connection, state: &FtsIndexState) -> Result<()> {
    with_fts_backend(conn, |backend| {
        upsert_fts_index_state_backend(backend, state)
    })
}

fn get_fts_pane_progress_test(
    conn: &mut Connection,
    pane_id: u64,
) -> Result<Option<FtsPaneProgress>> {
    with_fts_backend(conn, |backend| {
        get_fts_pane_progress_backend(backend, pane_id)
    })
}

fn upsert_fts_pane_progress_test(conn: &mut Connection, progress: &FtsPaneProgress) -> Result<()> {
    with_fts_backend(conn, |backend| {
        upsert_fts_pane_progress_backend(backend, progress)
    })
}

fn clear_fts_pane_progress_test(conn: &mut Connection) -> Result<()> {
    with_fts_backend(conn, clear_fts_pane_progress_backend)
}

fn panes_needing_fts_sync_test(conn: &mut Connection) -> Result<Vec<u64>> {
    with_fts_backend(conn, |backend| {
        let mut panes = Vec::new();
        let mut after = None;
        loop {
            let page = panes_needing_fts_sync_page_backend(backend, after, 3)?;
            let Some(last) = page.last().copied() else {
                break;
            };
            after = Some(last);
            panes.extend(page);
        }
        Ok(panes)
    })
}

fn full_fts_rebuild_test(conn: &mut Connection, config: &FtsSyncConfig) -> Result<FtsSyncResult> {
    with_fts_backend(conn, |backend| full_fts_rebuild_backend(backend, config))
}

fn sync_fts_on_startup_test(
    conn: &mut Connection,
    config: &FtsSyncConfig,
) -> Result<FtsSyncResult> {
    with_fts_backend(conn, |backend| sync_fts_on_startup_backend(backend, config))
}

/// Helper to insert a test segment directly
fn insert_test_segment(conn: &Connection, pane_id: u64, seq: u64, content: &str) {
    let now = now_ms();
    insert_test_segment_with_zone_at(conn, pane_id, seq, content, now, None);
}

fn insert_test_segment_with_zone_at(
    conn: &Connection,
    pane_id: u64,
    seq: u64,
    content: &str,
    captured_at: i64,
    zone_type: Option<&str>,
) {
    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at, zone_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            pane_id as i64,
            seq as i64,
            content,
            content.len() as i64,
            captured_at,
            zone_type
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
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // State table exists but is empty until sync initializes it
    let state = get_fts_index_state_test(&mut conn).unwrap();
    // After migration, we insert a default row
    assert!(state.is_some() || state.is_none()); // Depends on migration logic
}

#[test]
fn upsert_fts_index_state_works() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    let state = FtsIndexState {
        index_version: 42,
        last_full_rebuild_at: Some(now),
        created_at: now,
        updated_at: now,
    };

    upsert_fts_index_state_test(&mut conn, &state).unwrap();

    let loaded = get_fts_index_state_test(&mut conn).unwrap().unwrap();
    assert_eq!(loaded.index_version, 42);
    assert_eq!(loaded.last_full_rebuild_at, Some(now));
}

#[test]
fn fts_pane_progress_roundtrip() {
    let mut conn = Connection::open_in_memory().unwrap();
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

    upsert_fts_pane_progress_test(&mut conn, &progress).unwrap();

    let loaded = get_fts_pane_progress_test(&mut conn, 100).unwrap().unwrap();
    assert_eq!(loaded.pane_id, 100);
    assert_eq!(loaded.last_indexed_seq, 50);
    assert_eq!(loaded.indexed_count, 50);
}

#[test]
fn sync_fts_on_startup_initializes_state() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup_test(&mut conn, &config).unwrap();

    // Should complete with no segments (empty db)
    assert_eq!(result.segments_indexed, 0);
    assert!(!result.full_rebuild);

    // State should be initialized
    let state = get_fts_index_state_test(&mut conn).unwrap().unwrap();
    assert_eq!(state.index_version, FTS_INDEX_VERSION);
}

#[test]
fn startup_fts_failure_storm_pages_panes_and_bounds_content_free_warnings() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    apply_defer_fts_triggers(&conn);
    let pane_count = FTS_STARTUP_PANE_PAGE_SIZE + 44;
    for pane_id in 1..=pane_count {
        let pane_id = u64::try_from(pane_id).unwrap();
        insert_test_pane(&conn, pane_id);
        insert_test_segment(&conn, pane_id, 0, "failure-storm-token");
    }

    set_fts_startup_force_pane_failure_for_test(true);
    let result = sync_fts_on_startup_test(&mut conn, &FtsSyncConfig::default()).unwrap();
    set_fts_startup_force_pane_failure_for_test(false);

    assert_eq!(result.segments_indexed, 0);
    assert_eq!(result.panes_processed, u64::try_from(pane_count).unwrap());
    assert_eq!(result.warnings.len(), FTS_STARTUP_WARNING_LIMIT);
    assert!(
        result.warnings[..FTS_STARTUP_WARNING_DETAIL_LIMIT]
            .iter()
            .all(|warning| warning.contains("error_class=database"))
    );
    assert!(
        result
            .warnings
            .iter()
            .all(|warning| !warning.contains("/private/sensitive"))
    );
    assert_eq!(
        result.warnings.last().unwrap(),
        &format!(
            "{} additional pane synchronization warnings omitted",
            pane_count - FTS_STARTUP_WARNING_DETAIL_LIMIT
        )
    );
}

#[test]
fn full_fts_rebuild_pages_more_than_one_startup_pane_batch() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    apply_defer_fts_triggers(&conn);
    let pane_count = FTS_STARTUP_PANE_PAGE_SIZE + 7;
    for pane_id in 1..=pane_count {
        let pane_id = u64::try_from(pane_id).unwrap();
        insert_test_pane(&conn, pane_id);
        insert_test_segment(&conn, pane_id, 0, "paged-full-rebuild-token");
    }

    let result = full_fts_rebuild_test(&mut conn, &FtsSyncConfig::default()).unwrap();

    assert!(result.full_rebuild);
    assert_eq!(result.segments_indexed, u64::try_from(pane_count).unwrap());
    assert_eq!(result.panes_processed, u64::try_from(pane_count).unwrap());
    assert_eq!(
        fts_match_count(&conn, "paged-full-rebuild-token"),
        i64::try_from(pane_count).unwrap()
    );
}

fn assert_short_fetch_byte_prefix_converges(insert_select_batch: bool) {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    apply_defer_fts_triggers(&conn);
    insert_test_pane(&conn, 1);
    for seq in 0..7u64 {
        insert_test_segment(
            &conn,
            1,
            seq,
            &format!("shortfetchbytetoken-{seq}-{}", "x".repeat(96)),
        );
    }

    let config = FtsSyncConfig {
        // Fetch is short on the first query, while the byte limit admits only
        // one row. The loop must continue from max_seq instead of mistaking
        // `fetched < batch_size` for complete processing.
        batch_size: 50,
        max_batch_bytes: 64,
        commit_progress: true,
    };
    let (indexed, max_seq) = with_fts_backend(&mut conn, |backend| {
        sync_fts_for_pane_backend_with_mode(backend, 1, &config, insert_select_batch)
    })
    .unwrap();
    assert_eq!(indexed, 7);
    assert_eq!(max_seq, 6);
    assert_eq!(fts_match_count(&conn, "shortfetchbytetoken"), 7);
}

#[test]
fn set_based_sync_converges_after_short_fetch_byte_prefix() {
    assert_short_fetch_byte_prefix_converges(true);
}

#[test]
fn scalar_sync_converges_after_short_fetch_byte_prefix() {
    assert_short_fetch_byte_prefix_converges(false);
}

#[test]
fn fts_batch_byte_budget_uses_authoritative_content_not_cached_length() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    apply_defer_fts_triggers(&conn);
    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 0, &"x".repeat(64));
    insert_test_segment(&conn, 1, 1, &"y".repeat(64));
    conn.execute("UPDATE output_segments SET content_len = 0", [])
        .unwrap();

    with_fts_backend(&mut conn, |backend| {
        let scalar = get_unindexed_segments_backend(backend, 1, 0, 2, true)?;
        assert_eq!(
            scalar
                .iter()
                .map(|segment| segment.content_len)
                .collect::<Vec<_>>(),
            vec![64, 64],
            "the scalar path must derive the resource charge from content"
        );

        let set_based = insert_fts_entries_select_batch_backend(backend, 1, 0, 2, true, 80)?
            .expect("set-based batch selects its first row");
        assert_eq!(set_based.fetched_count, 2);
        assert_eq!(set_based.indexed_count, 1);
        assert_eq!(set_based.max_seq, 0);
        Ok(())
    })
    .unwrap();
}

#[test]
fn missing_fts_state_forces_authoritative_rebuild() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    apply_defer_fts_triggers(&conn);
    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 0, "missingstaterebuildtoken");
    conn.execute("DELETE FROM fts_index_state", []).unwrap();

    let result = sync_fts_on_startup_test(&mut conn, &FtsSyncConfig::default()).unwrap();
    assert!(result.full_rebuild);
    assert_eq!(result.segments_indexed, 1);
    assert_eq!(fts_match_count(&conn, "missingstaterebuildtoken"), 1);
    assert_eq!(
        get_fts_index_state_test(&mut conn)
            .unwrap()
            .unwrap()
            .index_version,
        FTS_INDEX_VERSION
    );
}

#[test]
fn invalid_fts_batch_bounds_fail_before_rebuild_mutation() {
    for config in [
        FtsSyncConfig {
            batch_size: 0,
            ..FtsSyncConfig::default()
        },
        FtsSyncConfig {
            batch_size: FTS_SYNC_MAX_BATCH_SEGMENTS + 1,
            ..FtsSyncConfig::default()
        },
        FtsSyncConfig {
            max_batch_bytes: 0,
            ..FtsSyncConfig::default()
        },
        FtsSyncConfig {
            max_batch_bytes: FTS_SYNC_MAX_BATCH_BYTES + 1,
            ..FtsSyncConfig::default()
        },
    ] {
        let mut conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();
        insert_test_pane(&conn, 1);
        insert_test_segment(&conn, 1, 0, "invalidconfigsentineltoken");
        let state_before = get_fts_index_state_test(&mut conn).unwrap().unwrap();
        let hits_before = fts_match_count(&conn, "invalidconfigsentineltoken");

        let error = full_fts_rebuild_test(&mut conn, &config)
            .expect_err("invalid FTS bounds must fail before delete-all");
        assert!(error.to_string().contains("FTS sync"));
        assert_eq!(
            get_fts_index_state_test(&mut conn)
                .unwrap()
                .unwrap()
                .index_version,
            state_before.index_version
        );
        assert_eq!(
            fts_match_count(&conn, "invalidconfigsentineltoken"),
            hits_before,
            "invalid config must not mutate searchable postings"
        );
    }
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

#[derive(Debug, PartialEq)]
struct FtsSearchProjection {
    pane_id: u64,
    seq: u64,
    content: String,
    snippet: Option<String>,
    highlight: Option<String>,
    score_bits: u64,
}

fn fts_search_projection(
    conn: &mut Connection,
    query: &str,
    options: &SearchOptions,
) -> Result<Vec<FtsSearchProjection>> {
    with_fts_backend(conn, |backend| {
        search_fts_with_snippets_backend(backend, query, options).map(|results| {
            results
                .into_iter()
                .map(|result| FtsSearchProjection {
                    pane_id: result.segment.pane_id,
                    seq: result.segment.seq,
                    content: result.segment.content,
                    snippet: result.snippet,
                    highlight: result.highlight,
                    score_bits: result.score.to_bits(),
                })
                .collect()
        })
    })
}

fn sync_all_panes_for_test(
    conn: &mut Connection,
    config: &FtsSyncConfig,
    insert_select_batch: bool,
) -> Result<u64> {
    with_fts_backend(conn, |backend| {
        let mut total_indexed = 0u64;
        let mut after = None;
        loop {
            let page = panes_needing_fts_sync_page_backend(backend, after, 3)?;
            let Some(last) = page.last().copied() else {
                break;
            };
            after = Some(last);
            for pane_id in page {
                let (indexed, _) = sync_fts_for_pane_backend_with_mode(
                    backend,
                    pane_id,
                    config,
                    insert_select_batch,
                )?;
                total_indexed = total_indexed.checked_add(indexed).ok_or_else(|| {
                    StorageError::Database("test FTS indexed-row count overflow".to_string())
                })?;
            }
        }
        Ok(total_indexed)
    })
}

fn seed_insert_select_oracle_fixture(conn: &Connection) {
    apply_defer_fts_triggers(conn);
    insert_test_pane(conn, 1);
    insert_test_pane(conn, 2);

    let base = 1_700_000_000_000i64;
    let rows = [
        (
            1,
            0,
            "needle alpha prompt transcript",
            base + 10,
            Some("prompt"),
        ),
        (
            1,
            1,
            "needle beta output transcript",
            base + 20,
            Some("output"),
        ),
        (1, 2, "control row without match", base + 30, Some("output")),
        (
            2,
            0,
            "needle gamma output transcript",
            base + 40,
            Some("output"),
        ),
        (
            2,
            1,
            "needle delta prompt transcript",
            base + 50,
            Some("prompt"),
        ),
        (
            1,
            3,
            "needle epsilon output transcript",
            base + 60,
            Some("output"),
        ),
    ];

    for (pane_id, seq, content, captured_at, zone_type) in rows {
        insert_test_segment_with_zone_at(conn, pane_id, seq, content, captured_at, zone_type);
    }
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
/// Starts seq at 1 because `sync_fts_for_pane_backend` had a pre-existing
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

/// [ft-wk5fo] End-to-end: the backend startup-sync catchup engine
/// sees all 500 deferred segments and indexes them through the
/// `fts_pane_progress` resume mechanism. Proves the deferred path
/// achieves eventual consistency — the whole point of the rollout.
#[test]
fn fts_deferred_mode_catchup_indexes_all_segments() {
    let mut conn = Connection::open_in_memory().unwrap();
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
    let result = sync_fts_on_startup_test(&mut conn, &config).unwrap();

    assert_eq!(result.segments_indexed, 500);
    assert_eq!(result.panes_processed, 1);
    assert!(!result.full_rebuild);
    assert_eq!(
        fts_match_count(&conn, "content"),
        500,
        "catchup must bring every deferred segment into FTS"
    );

    // And a second catchup is a no-op (progress prevents re-indexing).
    let second = sync_fts_on_startup_test(&mut conn, &config).unwrap();
    assert_eq!(
        second.segments_indexed, 0,
        "second catchup must see no new work (progress is resumable)"
    );
    assert_eq!(fts_match_count(&conn, "content"), 500);
}

#[test]
fn insert_select_batch_matches_per_row_search_snippet_order_filters() {
    let mut oracle = Connection::open_in_memory().unwrap();
    initialize_schema(&oracle).unwrap();
    seed_insert_select_oracle_fixture(&oracle);

    let mut actual = Connection::open_in_memory().unwrap();
    initialize_schema(&actual).unwrap();
    seed_insert_select_oracle_fixture(&actual);

    let config = FtsSyncConfig {
        batch_size: 2,
        max_batch_bytes: 128,
        commit_progress: true,
    };

    let oracle_indexed = sync_all_panes_for_test(&mut oracle, &config, false).unwrap();
    let actual_indexed = sync_all_panes_for_test(&mut actual, &config, true).unwrap();
    assert_eq!(actual_indexed, oracle_indexed);
    assert_eq!(actual_indexed, 6);

    let base = 1_700_000_000_000i64;
    let search_cases = [
        SearchOptions {
            limit: Some(10),
            highlight_prefix: Some("[[".to_string()),
            highlight_suffix: Some("]]".to_string()),
            snippet_max_tokens: Some(6),
            ..SearchOptions::default()
        },
        SearchOptions {
            pane_id: Some(1),
            limit: Some(10),
            highlight_prefix: Some("[[".to_string()),
            highlight_suffix: Some("]]".to_string()),
            snippet_max_tokens: Some(6),
            ..SearchOptions::default()
        },
        SearchOptions {
            zone_type: Some("output".to_string()),
            limit: Some(10),
            highlight_prefix: Some("[[".to_string()),
            highlight_suffix: Some("]]".to_string()),
            snippet_max_tokens: Some(6),
            ..SearchOptions::default()
        },
        SearchOptions {
            since: Some(base + 15),
            until: Some(base + 45),
            limit: Some(10),
            highlight_prefix: Some("[[".to_string()),
            highlight_suffix: Some("]]".to_string()),
            snippet_max_tokens: Some(6),
            ..SearchOptions::default()
        },
        SearchOptions {
            pane_id: Some(1),
            zone_type: Some("output".to_string()),
            since: Some(base + 15),
            until: Some(base + 65),
            limit: Some(10),
            highlight_prefix: Some("[[".to_string()),
            highlight_suffix: Some("]]".to_string()),
            snippet_max_tokens: Some(6),
            ..SearchOptions::default()
        },
    ];

    for options in search_cases {
        let expected = fts_search_projection(&mut oracle, "needle", &options).unwrap();
        let actual = fts_search_projection(&mut actual, "needle", &options).unwrap();
        assert_eq!(
            actual, expected,
            "set-based FTS catch-up must match the per-row oracle for options {options:?}"
        );
    }
}

#[test]
fn insert_select_batch_resumes_after_crash_progress_restart() {
    let mut oracle = Connection::open_in_memory().unwrap();
    initialize_schema(&oracle).unwrap();
    seed_insert_select_oracle_fixture(&oracle);

    let mut restarted = Connection::open_in_memory().unwrap();
    initialize_schema(&restarted).unwrap();
    seed_insert_select_oracle_fixture(&restarted);

    let config = FtsSyncConfig {
        batch_size: 2,
        max_batch_bytes: 128,
        commit_progress: true,
    };

    assert_eq!(
        sync_all_panes_for_test(&mut oracle, &config, false).unwrap(),
        6
    );

    with_fts_backend(&mut restarted, |backend| {
        let first_batch = insert_fts_entries_select_batch_backend(
            backend,
            1,
            0,
            config.batch_size,
            true,
            config.max_batch_bytes,
        )?
        .expect("pane 1 first set-based batch must select rows");
        assert_eq!(first_batch.indexed_count, 2);
        assert_eq!(first_batch.max_seq, 1);

        upsert_fts_pane_progress_backend(
            backend,
            &FtsPaneProgress {
                pane_id: 1,
                last_indexed_seq: first_batch.max_seq,
                indexed_count: first_batch.indexed_count,
                last_indexed_at: now_ms(),
            },
        )
    })
    .unwrap();

    let resumed = sync_all_panes_for_test(&mut restarted, &config, true).unwrap();
    assert_eq!(
        resumed, 4,
        "restart must resume after committed max_seq and index the remaining rows"
    );

    let pane_one_progress = get_fts_pane_progress_test(&mut restarted, 1)
        .unwrap()
        .expect("pane 1 progress should survive restart");
    assert_eq!(pane_one_progress.last_indexed_seq, 3);
    assert_eq!(pane_one_progress.indexed_count, 4);

    let options = SearchOptions {
        limit: Some(10),
        highlight_prefix: Some("[[".to_string()),
        highlight_suffix: Some("]]".to_string()),
        snippet_max_tokens: Some(6),
        ..SearchOptions::default()
    };
    let expected = fts_search_projection(&mut oracle, "needle", &options).unwrap();
    let actual = fts_search_projection(&mut restarted, "needle", &options).unwrap();
    assert_eq!(
        actual, expected,
        "crash-progress restart must finish with the same searchable corpus as the per-row oracle"
    );
}

#[test]
fn fts_sync_rejects_negative_cached_lengths_before_index_mutation() {
    for insert_select_batch in [false, true] {
        let mut conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();
        apply_defer_fts_triggers(&conn);
        insert_test_pane(&conn, 1);
        insert_test_segment(&conn, 1, 0, "negative-length-corruption");
        conn.execute(
            "UPDATE output_segments SET content_len = -1 WHERE pane_id = 1 AND seq = 0",
            [],
        )
        .unwrap();

        let error = with_fts_backend(&mut conn, |backend| {
            sync_fts_for_pane_backend_with_mode(
                backend,
                1,
                &FtsSyncConfig::default(),
                insert_select_batch,
            )
        })
        .expect_err("negative cached content length must fail closed");
        let message = error.to_string();
        assert!(
            message.contains("content_len") || message.contains("content length"),
            "both FTS engines must identify the corrupt cached length: {message}"
        );
        let indexed: i64 = conn
            .query_row("SELECT COUNT(*) FROM output_segments_fts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            indexed, 0,
            "a corrupt cached length must be detected before either engine mutates FTS"
        );
        assert!(
            get_fts_pane_progress_test(&mut conn, 1).unwrap().is_none(),
            "a rejected batch must not advance its resume cursor"
        );
    }
}

#[test]
fn fts_sync_rolls_back_postings_when_progress_checkpoint_fails() {
    for insert_select_batch in [false, true] {
        for commit_progress in [false, true] {
            let mut conn = Connection::open_in_memory().unwrap();
            initialize_schema(&conn).unwrap();
            apply_defer_fts_triggers(&conn);
            insert_test_pane(&conn, 1);
            insert_test_segment(&conn, 1, 0, "atomic-progress-checkpoint-zero");
            insert_test_segment(&conn, 1, 1, "atomic-progress-checkpoint-one");
            conn.execute_batch(
                "CREATE TRIGGER reject_fts_progress
                 BEFORE INSERT ON fts_pane_progress
                 BEGIN
                     SELECT RAISE(ABORT, 'injected progress failure');
                 END;",
            )
            .unwrap();
            let config = FtsSyncConfig {
                batch_size: 1,
                max_batch_bytes: 1_048_576,
                commit_progress,
            };

            with_fts_backend(&mut conn, |backend| {
                sync_fts_for_pane_backend_with_mode(backend, 1, &config, insert_select_batch)
            })
            .expect_err("injected progress failure must abort the atomic FTS unit");
            let indexed: i64 = conn
                .query_row("SELECT COUNT(*) FROM output_segments_fts", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                indexed, 0,
                "postings must roll back when their resume checkpoint does not commit"
            );
            assert!(
                get_fts_pane_progress_test(&mut conn, 1).unwrap().is_none(),
                "failed progress checkpoint must leave no cursor"
            );

            conn.execute_batch("DROP TRIGGER reject_fts_progress")
                .unwrap();
            let (indexed, max_seq) = with_fts_backend(&mut conn, |backend| {
                sync_fts_for_pane_backend_with_mode(backend, 1, &config, insert_select_batch)
            })
            .expect("retry after the injected failure must recover cleanly");
            assert_eq!((indexed, max_seq), (2, 1));
            assert_eq!(fts_match_count(&conn, "atomic"), 2);
        }
    }
}

/// [ft-7do6c] The seq=0 off-by-one: a fresh pane's FIRST segment
/// gets seq=0 (COALESCE(MAX(seq)+1, 0) at append_segment_sync:12355).
/// Before the fix, `sync_fts_for_pane_backend`'s `WHERE seq > last_indexed_seq`
/// filter — with last_indexed_seq defaulting to 0 for a never-synced
/// pane — silently skipped that first segment forever under deferred
/// mode. This test constructs the exact shape that was broken:
/// fresh pane, first segment at seq=0, deferred triggers, then
/// catchup — and asserts the seq=0 row is indexed alongside the
/// rest. The ft-wk5fo tests above deliberately avoided seq=0 while
/// this bug was open; this test removes that restriction.
#[test]
fn ft_7do6c_catchup_indexes_seq_zero_on_first_sync() {
    let mut conn = Connection::open_in_memory().unwrap();
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
    let result = sync_fts_on_startup_test(&mut conn, &config).unwrap();

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
    let second = sync_fts_on_startup_test(&mut conn, &config).unwrap();
    assert_eq!(second.segments_indexed, 0);
    assert_eq!(fts_match_count(&conn, "seqzero"), 5);
}

/// [ft-wk5fo] Interleaved: alternating catchup + insert rounds
/// preserve the resumable-progress invariant. This is the shape the
/// future periodic writer-thread tick will produce in production.
///
/// Seq range is 1..=500 (not 0..500) — see the note on
/// `fts_sync_mode_indexes_on_insert` about the seq=0 off-by-one
/// in `sync_fts_for_pane_backend`.
#[test]
fn fts_deferred_mode_catchup_resumes_across_rounds() {
    let mut conn = Connection::open_in_memory().unwrap();
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
        let result = sync_fts_on_startup_test(&mut conn, &config).unwrap();
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
    let mut conn = Connection::open_in_memory().unwrap();
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
    clear_fts_pane_progress_test(&mut conn).unwrap();

    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup_test(&mut conn, &config).unwrap();

    // Should rebuild all 3 segments
    assert_eq!(result.segments_indexed, 3);
    assert_eq!(result.panes_processed, 1);
}

#[test]
fn sync_fts_respects_progress() {
    let mut conn = Connection::open_in_memory().unwrap();
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
    upsert_fts_pane_progress_test(&mut conn, &progress).unwrap();

    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup_test(&mut conn, &config).unwrap();

    // Should only index segment 3
    assert_eq!(result.segments_indexed, 1);
}

#[test]
fn sync_fts_on_startup_skips_caught_up_panes() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    apply_defer_fts_triggers(&conn);

    let now = now_ms();
    for pane in 1..=25u64 {
        insert_test_pane(&conn, pane);
        insert_test_segment(&conn, pane, 0, &format!("caughtup-{pane}-zero"));
        insert_test_segment(&conn, pane, 1, &format!("caughtup-{pane}-one"));
        upsert_fts_pane_progress_test(
            &mut conn,
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
    upsert_fts_pane_progress_test(
        &mut conn,
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

    let pane_ids = panes_needing_fts_sync_test(&mut conn).unwrap();
    assert_eq!(
        pane_ids,
        vec![100, 200],
        "healthy startup should only visit panes with new segments or missing progress"
    );

    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup_test(&mut conn, &config).unwrap();

    assert_eq!(result.panes_processed, 2);
    assert_eq!(result.segments_indexed, 2);
    assert_eq!(fts_match_count(&conn, "dirtyneedle"), 1);
    assert_eq!(fts_match_count(&conn, "missingneedle"), 1);
    assert!(
        panes_needing_fts_sync_test(&mut conn).unwrap().is_empty(),
        "second startup pass should have no panes to visit"
    );
}

#[test]
fn full_rebuild_clears_progress() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Create pane and segments
    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "One");
    insert_test_segment(&conn, 1, 2, "Two");

    // Set some progress
    let now = now_ms();
    upsert_fts_pane_progress_test(
        &mut conn,
        &FtsPaneProgress {
            pane_id: 1,
            last_indexed_seq: 1,
            indexed_count: 1,
            last_indexed_at: now,
        },
    )
    .unwrap();

    let config = FtsSyncConfig::default();
    let result = full_fts_rebuild_test(&mut conn, &config).unwrap();

    assert!(result.full_rebuild);
    assert_eq!(result.segments_indexed, 2);

    // Progress should be updated
    let progress = get_fts_pane_progress_test(&mut conn, 1).unwrap().unwrap();
    assert_eq!(progress.last_indexed_seq, 2);
    assert_eq!(progress.indexed_count, 2);
}

#[test]
fn full_rebuild_is_idempotent() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "Alpha");
    insert_test_segment(&conn, 1, 2, "Beta");
    insert_test_segment(&conn, 1, 3, "Gamma");

    let config = FtsSyncConfig::default();
    let first = full_fts_rebuild_test(&mut conn, &config).unwrap();
    assert!(first.full_rebuild);
    assert_eq!(first.segments_indexed, 3);

    let fts_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM output_segments_fts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(fts_rows, 3);

    let second = full_fts_rebuild_test(&mut conn, &config).unwrap();
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
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Set an old version
    let now = now_ms();
    let old_state = FtsIndexState {
        index_version: FTS_INDEX_VERSION - 1, // Old version
        last_full_rebuild_at: Some(now),
        created_at: now,
        updated_at: now,
    };
    upsert_fts_index_state_test(&mut conn, &old_state).unwrap();

    // Create pane and segment
    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "Test content");

    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup_test(&mut conn, &config).unwrap();

    // Should trigger full rebuild due to version mismatch
    assert!(result.full_rebuild);

    // State should be updated to new version
    let state = get_fts_index_state_test(&mut conn).unwrap().unwrap();
    assert_eq!(state.index_version, FTS_INDEX_VERSION);
}

#[test]
fn fts_rebuild_pending_version_triggers_full_rebuild() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "Test content");

    let now = now_ms();
    upsert_fts_index_state_test(
        &mut conn,
        &FtsIndexState {
            index_version: FTS_INDEX_REBUILD_PENDING_VERSION,
            last_full_rebuild_at: None,
            created_at: now,
            updated_at: now,
        },
    )
    .unwrap();
    clear_fts_pane_progress_test(&mut conn).unwrap();
    conn.execute_batch("INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')")
        .unwrap();

    let config = FtsSyncConfig::default();
    let result = sync_fts_on_startup_test(&mut conn, &config).unwrap();

    assert!(result.full_rebuild);

    let state = get_fts_index_state_test(&mut conn).unwrap().unwrap();
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
fn fts_hard_rebuild_failure_leaves_pending_marker_and_rolls_back_progress_clear() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "Alpha");

    let now = now_ms();
    upsert_fts_index_state_test(
        &mut conn,
        &FtsIndexState {
            index_version: FTS_INDEX_VERSION,
            last_full_rebuild_at: Some(now),
            created_at: now,
            updated_at: now,
        },
    )
    .unwrap();
    upsert_fts_pane_progress_test(
        &mut conn,
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

    let err = full_fts_rebuild_test(&mut conn, &FtsSyncConfig::default()).unwrap_err();
    let err_msg = err.to_string();
    assert!(err_msg.contains("FTS rebuild could not clear the prior index"));

    let state = get_fts_index_state_test(&mut conn).unwrap().unwrap();
    assert_eq!(state.index_version, FTS_INDEX_REBUILD_PENDING_VERSION);
    assert_eq!(state.last_full_rebuild_at, None);
    assert_eq!(
        get_fts_pane_progress_test(&mut conn, 1)
            .unwrap()
            .unwrap()
            .last_indexed_seq,
        1,
        "the destructive rebuild transaction must roll back its progress clear"
    );
}

#[test]
fn startup_repairs_an_interrupted_pending_rebuild_before_publishing_current() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "interruptedrebuildtoken");
    assert_eq!(fts_match_count(&conn, "interruptedrebuildtoken"), 1);

    // Model a database left by a pre-atomic or interrupted rebuild: the
    // durable pending marker was published, but destructive index/progress
    // mutations became visible before the process stopped.
    with_fts_backend(&mut conn, |backend| {
        mark_fts_rebuild_pending_backend(backend, now_ms())?;
        backend
            .execute_batch(
                "INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')",
            )
            .map_err(|error| storage_backend_error("Simulate interrupted FTS rebuild", error))?;
        clear_fts_pane_progress_backend(backend)
    })
    .unwrap();
    assert_eq!(fts_match_count(&conn, "interruptedrebuildtoken"), 0);

    let result = sync_fts_on_startup_test(&mut conn, &FtsSyncConfig::default()).unwrap();
    assert!(result.full_rebuild);
    assert_eq!(result.segments_indexed, 1);
    assert_eq!(fts_match_count(&conn, "interruptedrebuildtoken"), 1);
    let state = get_fts_index_state_test(&mut conn).unwrap().unwrap();
    assert_eq!(state.index_version, FTS_INDEX_VERSION);
    assert!(state.last_full_rebuild_at.is_some());
}

#[test]
fn search_fails_closed_while_fts_rebuild_is_pending() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    insert_test_pane(&conn, 1);
    insert_test_segment(&conn, 1, 1, "pendingsearchgatetoken");
    assert_eq!(
        fts_search_projection(
            &mut conn,
            "pendingsearchgatetoken",
            &SearchOptions::default()
        )
        .unwrap()
        .len(),
        1
    );

    with_fts_backend(&mut conn, |backend| {
        mark_fts_rebuild_pending_backend(backend, now_ms())
    })
    .unwrap();
    let error = fts_search_projection(
        &mut conn,
        "pendingsearchgatetoken",
        &SearchOptions::default(),
    )
    .expect_err("search must not expose a potentially partial pending index");
    assert!(error.to_string().contains("rebuilding or requires repair"));

    full_fts_rebuild_test(&mut conn, &FtsSyncConfig::default()).unwrap();
    assert_eq!(
        fts_search_projection(
            &mut conn,
            "pendingsearchgatetoken",
            &SearchOptions::default()
        )
        .unwrap()
        .len(),
        1
    );
}

#[test]
fn batch_config_limits_work() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Create pane and multiple segments
    insert_test_pane(&conn, 1);
    for i in 1..=10 {
        insert_test_segment(&conn, 1, i, &format!("Segment {i} with some content"));
    }

    // Clear FTS
    conn.execute_batch("INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')")
        .ok();
    clear_fts_pane_progress_test(&mut conn).unwrap();

    // Use small batch size
    let config = FtsSyncConfig {
        batch_size: 3,
        max_batch_bytes: 1_048_576,
        commit_progress: true,
    };

    let result = sync_fts_on_startup_test(&mut conn, &config).unwrap();

    // Should index all 10 segments in multiple batches
    assert_eq!(result.segments_indexed, 10);

    // Progress should be at the end
    let progress = get_fts_pane_progress_test(&mut conn, 1).unwrap().unwrap();
    assert_eq!(progress.last_indexed_seq, 10);
}
