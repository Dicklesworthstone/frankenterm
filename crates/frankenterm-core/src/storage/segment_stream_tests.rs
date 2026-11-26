use super::*;

fn stream_connection() -> rusqlite::Connection {
    let connection = rusqlite::Connection::open_in_memory().expect("open stream fixture");
    initialize_schema(&connection).expect("initialize stream fixture");
    connection
        .execute_batch(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed)
             VALUES (7, 'local', 1, 1, 1), (8, 'local', 1, 1, 1);",
        )
        .expect("seed stream panes");
    connection
}

fn stream_backend() -> RusqliteBackend {
    RusqliteBackend::new(stream_connection()).expect("install backend authority")
}

fn append(backend: &dyn StorageBackend, pane_id: u64, seq: u64, at: i64) -> Segment {
    insert_output_segment_with_seq_backend(
        backend, pane_id, seq, "stream fixture", None, None, at, "test-catalog",
    )
    .expect("allocate segment")
}

fn authority(connection: &rusqlite::Connection) -> (String, i64) {
    connection
        .query_row(
            "SELECT cursor_epoch, max_segment_id FROM segment_stream_state WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read stream authority")
}

#[test]
fn segment_ids_survive_highest_deletion_full_prune_and_reopen() {
    run_storage_async_test(async {
        let directory = tempfile::tempdir().expect("stream directory");
        let path = directory.path().join("stream.db");
        let db_path = path.to_string_lossy().to_string();
        let storage = StorageHandle::new(&db_path).await.expect("open stream");
        storage
            .upsert_pane(PaneRecord {
                pane_id: 7,
                pane_uuid: None,
                domain: "local".into(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 1,
                last_seen_at: 1,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .expect("seed pane");
        let first = storage.append_segment(7, "first", None).await.unwrap();
        let second = storage.append_segment(7, "second", None).await.unwrap();
        assert!(second.id > first.id);
        let connection = rusqlite::Connection::open(&path).expect("open retention fixture");
        connection
            .execute("DELETE FROM output_segments WHERE id = ?1", [second.id])
            .expect("delete highest retained ID");
        let third = storage.append_segment(7, "third", None).await.unwrap();
        assert!(third.id > second.id, "deleted IDs cannot be allocated again");

        let cx = crate::cx::for_testing();
        let before = storage
            .get_segment_stream_snapshot_with_cx(&cx, Some(7))
            .await
            .unwrap();
        assert_eq!(storage.prune_segments_before(i64::MAX).await.unwrap(), 2);
        let empty = storage
            .get_segments_stream_page_with_cx(&cx, SegmentScanQuery::default())
            .await
            .unwrap();
        assert!(empty.segments.is_empty());
        assert_eq!(empty.snapshot.cursor_epoch, before.cursor_epoch);
        assert_eq!(empty.snapshot.max_segment_id, third.id);
        assert_eq!(empty.snapshot.tail_segment_id, 0);
        storage.shutdown().await.unwrap();
        drop(storage);
        drop(connection);

        let reopened = StorageHandle::new(&db_path).await.expect("reopen stream");
        let fourth = reopened.append_segment(7, "fourth", None).await.unwrap();
        assert!(fourth.id > third.id);
        let after = reopened
            .get_segment_stream_snapshot_with_cx(&cx, Some(7))
            .await
            .unwrap();
        assert_eq!(after.cursor_epoch, before.cursor_epoch);
        assert_eq!(after.max_segment_id, fourth.id);
        reopened.shutdown().await.unwrap();
    });
}

#[test]
fn segment_allocation_guards_abort_reuse_and_preserve_rollback() {
    let connection = stream_connection();
    let epoch = authority(&connection).0;
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    connection
        .execute(
            "INSERT INTO output_segments (id, pane_id, seq, content, content_len, captured_at)
             VALUES (50, 7, 0, 'rolledbacktoken', 15, 1)",
            [],
        )
        .unwrap();
    assert_eq!(authority(&connection).1, 50);
    connection.execute_batch("ROLLBACK").unwrap();
    assert_eq!(authority(&connection), (epoch.clone(), 0));

    connection
        .execute(
            "INSERT INTO output_segments (id, pane_id, seq, content, content_len, captured_at)
             VALUES (50, 7, 0, 'kepttoken', 9, 1)",
            [],
        )
        .unwrap();
    for sql in [
        "DELETE FROM segment_stream_state",
        "UPDATE segment_stream_state SET max_segment_id = 49",
        "UPDATE segment_stream_state SET max_segment_id = 9223372036854775808.0",
        "UPDATE segment_stream_state SET max_segment_id = 'invalid'",
        "UPDATE segment_stream_state SET cursor_epoch = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
        "INSERT OR REPLACE INTO segment_stream_state VALUES (1, 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 0)",
        "UPDATE output_segments SET id = 51 WHERE id = 50",
        "UPDATE output_segments SET rowid = 51 WHERE id = 50",
        "UPDATE output_segments SET _rowid_ = 51 WHERE id = 50",
        "UPDATE output_segments SET oid = 51 WHERE id = 50",
    ] {
        assert!(connection.execute(sql, []).is_err(), "must reject {sql}");
    }
    assert_eq!(authority(&connection), (epoch, 50));
    connection.execute("DELETE FROM output_segments", []).unwrap();
    for sql in [
        "INSERT INTO output_segments (id, pane_id, seq, content, content_len, captured_at)
         VALUES (50, 7, 0, 'reusetoken', 10, 1)",
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at)
         VALUES (7, 0, 'reusetoken', 10, 1)",
    ] {
        assert!(connection.execute(sql, []).is_err(), "must reject ID reuse");
    }
    let remaining: i64 = connection
        .query_row("SELECT COUNT(*) FROM output_segments", [], |row| row.get(0))
        .unwrap();
    let stale_fts: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM output_segments_fts WHERE output_segments_fts MATCH 'reusetoken'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!((remaining, stale_fts), (0, 0));
    let backend = RusqliteBackend::new(connection).unwrap();
    assert_eq!(append(&backend, 7, 0, 2).id, 51);
}

#[test]
fn segment_allocator_exhaustion_fails_without_mutation() {
    let backend = stream_backend();
    backend
        .execute("UPDATE segment_stream_state SET max_segment_id = 9223372036854775807")
        .unwrap();
    let error = insert_output_segment_with_seq_backend(
        &backend, 7, 0, "never stored", None, None, 1, "test-catalog",
    )
    .expect_err("exhaustion cannot wrap or reuse an ID");
    assert!(error.to_string().contains("exhausted"));
    let page = query_segments_stream_page_backend(&backend, &SegmentScanQuery::default()).unwrap();
    assert!(page.segments.is_empty());
    assert_eq!(page.snapshot.max_segment_id, i64::MAX);
    assert_eq!(page.snapshot.tail_segment_id, 0);
}

#[test]
fn segment_pages_bind_empty_and_scoped_rows_to_one_authority() {
    let backend = stream_backend();
    let first = append(&backend, 7, 0, 100);
    let other_pane = append(&backend, 8, 0, 100);
    let last = append(&backend, 7, 1, 90);
    let page = query_segments_stream_page_backend(
        &backend,
        &SegmentScanQuery {
            after_id: Some(first.id),
            pane_id: Some(7),
            limit: 1,
            ..SegmentScanQuery::default()
        },
    )
    .unwrap();
    assert_eq!(page.segments.iter().map(|row| row.id).collect::<Vec<_>>(), [last.id]);
    assert_eq!(page.snapshot.max_segment_id, last.id);
    assert_eq!(page.snapshot.tail_segment_id, last.id);
    let scoped = query_segment_stream_snapshot_backend(&backend, Some(8)).unwrap();
    assert_eq!(scoped.max_segment_id, last.id);
    assert_eq!(scoped.tail_segment_id, other_pane.id);

    for (pane_id, after_id, tail) in [
        (Some(7), Some(last.id), last.id),
        (Some(999), None, 0),
    ] {
        let empty = query_segments_stream_page_backend(
            &backend,
            &SegmentScanQuery { pane_id, after_id, ..SegmentScanQuery::default() },
        )
        .unwrap();
        assert!(empty.segments.is_empty());
        assert_eq!(empty.snapshot.cursor_epoch, page.snapshot.cursor_epoch);
        assert_eq!(empty.snapshot.max_segment_id, last.id);
        assert_eq!(empty.snapshot.tail_segment_id, tail);
    }
    for query in [
        SegmentScanQuery { after_id: Some(-1), ..SegmentScanQuery::default() },
        SegmentScanQuery { limit: SEGMENT_STREAM_PAGE_MAX + 1, ..SegmentScanQuery::default() },
    ] {
        assert!(query_segments_stream_page_backend(&backend, &query).is_err());
    }
    let plan = backend
        .query_map_cells(
            "EXPLAIN QUERY PLAN SELECT id FROM output_segments
             WHERE pane_id = ?1 AND id > ?2 ORDER BY id LIMIT 1",
            &[ToSqlValue::Integer(7), ToSqlValue::Integer(0)],
        )
        .unwrap();
    assert!(plan.iter().any(|row| {
        CellRowReader::new(row).string(3).unwrap().contains("idx_segments_pane_id")
    }));
}

#[test]
fn segment_stream_v53_upgrade_seeds_once_and_reopen_does_not_repair_missing_authority() {
    let connection = stream_connection();
    connection
        .execute(
            "INSERT INTO output_segments (id, pane_id, seq, content, content_len, captured_at)
             VALUES (83, 7, 0, 'legacy', 6, 1)",
            [],
        )
        .unwrap();
    connection
        .execute_batch(
            "DROP TRIGGER output_segments_monotonic_id_ai;
             DROP TRIGGER output_segments_id_update_guard;
             DROP TRIGGER segment_stream_state_insert_guard;
             DROP TRIGGER segment_stream_state_delete_guard;
             DROP TRIGGER segment_stream_state_update_guard;
             DROP TABLE segment_stream_state;
             DROP INDEX idx_segments_pane_id;
             PRAGMA user_version = 52;
             UPDATE ft_meta SET schema_version = 52 WHERE id = 1;
             DELETE FROM schema_version WHERE version = 53;",
        )
        .unwrap();
    initialize_schema(&connection).expect("upgrade legacy segment allocator");
    let installed = authority(&connection);
    assert_eq!(installed.1, 83);
    initialize_schema(&connection).expect("validate current authority");
    assert_eq!(authority(&connection), installed);
    connection.execute("DELETE FROM output_segments", []).unwrap();
    initialize_schema(&connection).expect("empty retention does not reset authority");
    assert_eq!(authority(&connection), installed);
    let backend = RusqliteBackend::new(connection).unwrap();
    assert_eq!(append(&backend, 7, 0, 2).id, 84);
    let connection = backend.into_connection().unwrap();

    let guard_sql: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'segment_stream_state_delete_guard'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute_batch("DROP TRIGGER segment_stream_state_delete_guard; DELETE FROM segment_stream_state;")
        .unwrap();
    connection.execute_batch(&guard_sql).unwrap();
    assert!(initialize_schema(&connection).is_err(), "current schema cannot invent an epoch");
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM segment_stream_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    let backend = RusqliteBackend::new(connection).unwrap();
    assert!(query_segment_stream_snapshot_backend(&backend, None).is_err());
    assert!(query_segments_stream_page_backend(&backend, &SegmentScanQuery::default()).is_err());
    assert!(insert_output_segment_with_seq_backend(
        &backend, 7, 1, "must fail", None, None, 3, "test-catalog",
    )
    .is_err());
}

#[cfg(unix)]
#[test]
fn fresh_stream_read_observes_replacement_and_never_creates_a_missing_database() {
    let directory = tempfile::tempdir().unwrap();
    let current = directory.path().join("current.db");
    let replacement = directory.path().join("replacement.db");
    let displaced = directory.path().join("displaced.db");
    let mut epochs = Vec::new();
    for path in [&current, &replacement] {
        let connection = rusqlite::Connection::open(path).unwrap();
        initialize_schema(&connection).unwrap();
        // No live writer is involved. DELETE mode avoids swapping unrelated
        // WAL sidecars while the old read pool deliberately holds its inode.
        connection.execute_batch("PRAGMA journal_mode = DELETE").unwrap();
        epochs.push(authority(&connection).0);
    }
    assert_ne!(epochs[0], epochs[1]);
    let provider = RusqliteStorageBackendProvider::new(1);
    let db_path = current.to_string_lossy().to_string();
    let before = with_provider_read_backend(&provider, &db_path, |backend| {
        query_segment_stream_snapshot_backend(backend, None)
    })
    .unwrap();
    assert_eq!(before.cursor_epoch, epochs[0]);
    std::fs::rename(&current, &displaced).unwrap();
    assert!(with_provider_fresh_read_backend(&provider, &db_path, |backend| {
        query_segment_stream_snapshot_backend(backend, None)
    })
    .is_err());
    assert!(!current.exists(), "a read cannot create replacement authority");
    std::fs::rename(&replacement, &current).unwrap();
    let after = with_provider_fresh_read_backend(&provider, &db_path, |backend| {
        query_segments_stream_page_backend(backend, &SegmentScanQuery::default())
    })
    .unwrap();
    assert!(after.segments.is_empty());
    assert_eq!(after.snapshot.cursor_epoch, epochs[1]);
}
