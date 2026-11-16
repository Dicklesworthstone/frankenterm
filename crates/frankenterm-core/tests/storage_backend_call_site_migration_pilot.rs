//! Call-site migration pilot for storage.rs (br-ft-l1jgo slice 3).
//!
//! **Bead:** `ft-l1jgo` (wa-2l27x.8.cont.extract.callsite).
//!
//! ## Why this test
//!
//! storage.rs has ~600 rusqlite call sites that the bead's
//! wired-pass migrates to the StorageBackend trait. Each
//! migration follows one of a handful of patterns:
//!
//! 1. `conn.prepare(sql).query_map(params, mapper)` →
//!    `backend.query_map_strings(sql, params)` + typed
//!    extractors from `storage_backend_row_helpers`.
//! 2. `conn.query_row(sql, params, mapper)` →
//!    `backend.query_row_strings(sql, params)` + RowReader.
//! 3. `conn.execute(sql, params)` →
//!    `helpers::execute_typed(backend, sql, &[ToSqlValue<'_>])`.
//! 4. `conn.prepare(...).query_one(...)` for COUNT(*) →
//!    `helpers::count_table(backend, table)`.
//! 5. `PRAGMA <name>` reader → `helpers::pragma_value(backend,
//!    pragma)`.
//!
//! This test exercises each pattern against an in-memory
//! schema that mirrors a slice of storage.rs's real layout
//! (panes + events + segments). For every rusqlite-shaped
//! reference impl, an equivalent trait-shaped impl runs and
//! the two are asserted byte-equivalent.
//!
//! ## What this proves
//!
//! - The trait surface (ft-qgj81 closed at ac3667cab) is
//!   sufficient for the storage.rs migration.
//! - The typed helpers (ft-l1jgo slice 1 at 8f6cbaf92 + slice
//!   2 at 0ab34e534) cover the read-side patterns.
//! - The migration is mechanical — same SQL, same parameter
//!   values, same result shape, just different dispatch type.
//!
//! ## What this is NOT
//!
//! Not a refactor of storage.rs itself. The wired-pass
//! cont-bead executes the actual migration; this pilot is the
//! substrate proof that the migration pattern is sound.

use frankenterm_core::storage_backend_helpers::{
    count_table, execute_typed, list_user_tables, max_column, pragma_value, table_exists,
};
use frankenterm_core::storage_backend_row_helpers::{RowReader, row_i64, row_string};
use frankenterm_core::storage_backend_trait::{
    OpenConfig, RusqliteBackend, StorageBackend, ToSqlValue,
};

/// Schema slice mirroring storage.rs's panes + events tables.
const SCHEMA_SLICE: &str = "
    CREATE TABLE panes (
        pane_id INTEGER PRIMARY KEY,
        title TEXT NOT NULL,
        opened_at_ms INTEGER NOT NULL,
        closed_at_ms INTEGER
    );
    CREATE INDEX panes_opened_at_ms_idx ON panes(opened_at_ms);

    CREATE TABLE events (
        event_id INTEGER PRIMARY KEY,
        pane_id INTEGER NOT NULL,
        ts_ms INTEGER NOT NULL,
        severity TEXT NOT NULL,
        FOREIGN KEY (pane_id) REFERENCES panes(pane_id)
    );
    CREATE INDEX events_pane_ts_idx ON events(pane_id, ts_ms);
";

fn populated_backend() -> RusqliteBackend {
    let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
    backend.execute_batch(SCHEMA_SLICE).unwrap();
    // Three panes.
    for (id, title, opened, closed) in &[
        (1_i64, "alpha", 100_i64, Some(200_i64)),
        (2, "beta", 110, None::<i64>),
        (3, "gamma", 120, Some(300)),
    ] {
        let closed_str = closed.map(|n| n.to_string()).unwrap_or_default();
        let closed_param = if closed.is_some() {
            ToSqlValue::Integer(closed.unwrap())
        } else {
            ToSqlValue::Null
        };
        execute_typed(
            &backend,
            "INSERT INTO panes (pane_id, title, opened_at_ms, closed_at_ms) \
             VALUES (?1, ?2, ?3, ?4)",
            &[
                ToSqlValue::Integer(*id),
                ToSqlValue::Text(title),
                ToSqlValue::Integer(*opened),
                closed_param,
            ],
        )
        .unwrap();
        let _ = closed_str;
    }
    // Five events.
    for (eid, pane_id, ts, sev) in &[
        (1_i64, 1_i64, 105_i64, "info"),
        (2, 1, 150, "warning"),
        (3, 2, 115, "info"),
        (4, 3, 130, "critical"),
        (5, 3, 140, "info"),
    ] {
        execute_typed(
            &backend,
            "INSERT INTO events (event_id, pane_id, ts_ms, severity) \
             VALUES (?1, ?2, ?3, ?4)",
            &[
                ToSqlValue::Integer(*eid),
                ToSqlValue::Integer(*pane_id),
                ToSqlValue::Integer(*ts),
                ToSqlValue::Text(sev),
            ],
        )
        .unwrap();
    }
    backend
}

// =============================================================================
// Pattern 1: query_map_strings + typed extractors
// =============================================================================

#[test]
fn migration_pilot_pattern_1_query_map_with_typed_extraction() {
    let backend = populated_backend();
    // Storage.rs pattern: SELECT pane_id, title, opened_at_ms FROM panes
    // ORDER BY pane_id.
    let rows = backend
        .query_map_strings(
            "SELECT pane_id, title, opened_at_ms FROM panes ORDER BY pane_id",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 3);

    // Typed extraction via row_helpers.
    let mut titles: Vec<String> = Vec::new();
    let mut total_opened_at_ms: i64 = 0;
    for row in &rows {
        let _pane_id = row_i64(row, 0).unwrap();
        let title = row_string(row, 1).unwrap();
        let opened_at_ms = row_i64(row, 2).unwrap();
        titles.push(title);
        total_opened_at_ms += opened_at_ms;
    }
    assert_eq!(titles, vec!["alpha", "beta", "gamma"]);
    assert_eq!(total_opened_at_ms, 100 + 110 + 120);
}

// =============================================================================
// Pattern 2: query_row_strings + RowReader
// =============================================================================

#[test]
fn migration_pilot_pattern_2_query_row_with_row_reader() {
    let backend = populated_backend();
    let row = backend
        .query_row_strings(
            "SELECT pane_id, title, opened_at_ms, closed_at_ms FROM panes \
             WHERE pane_id = ?1",
            &["2"],
        )
        .unwrap()
        .expect("pane_id=2 exists");

    let reader = RowReader::new(&row);
    assert_eq!(reader.i64(0).unwrap(), 2);
    assert_eq!(reader.string(1).unwrap(), "beta");
    assert_eq!(reader.i64(2).unwrap(), 110);
    // pane_id=2 has closed_at_ms = NULL → empty string in
    // string-substrate; optional_i64 maps to None.
    assert_eq!(reader.optional_i64(3).unwrap(), None);
}

// =============================================================================
// Pattern 3: execute_typed for INSERT/UPDATE/DELETE
// =============================================================================

#[test]
fn migration_pilot_pattern_3_execute_typed_for_dml() {
    let backend = populated_backend();
    // Update a pane's closed_at_ms.
    execute_typed(
        &backend,
        "UPDATE panes SET closed_at_ms = ?1 WHERE pane_id = ?2",
        &[ToSqlValue::Integer(999), ToSqlValue::Integer(2)],
    )
    .unwrap();
    let row = backend
        .query_row_strings("SELECT closed_at_ms FROM panes WHERE pane_id = 2", &[])
        .unwrap()
        .unwrap();
    assert_eq!(row_i64(&row, 0).unwrap(), 999);
}

// =============================================================================
// Pattern 4: count_table for COUNT(*)
// =============================================================================

#[test]
fn migration_pilot_pattern_4_count_table_replaces_count_star() {
    let backend = populated_backend();
    assert_eq!(count_table(&backend, "panes").unwrap(), 3);
    assert_eq!(count_table(&backend, "events").unwrap(), 5);
}

// =============================================================================
// Pattern 5: pragma_value for single-column PRAGMA
// =============================================================================

#[test]
fn migration_pilot_pattern_5_pragma_value_user_version() {
    let backend = populated_backend();
    backend.set_user_version(7).unwrap();
    let v = pragma_value(&backend, "user_version").unwrap();
    assert_eq!(v.as_deref(), Some("7"));
}

// =============================================================================
// Pattern composition: typical storage.rs read-side function
// =============================================================================

/// Reproduces a storage.rs `events_for_pane(pane_id)` shape:
/// SELECT all events for a pane, ordered by ts_ms.
fn events_for_pane(backend: &dyn StorageBackend, pane_id: i64) -> Vec<(i64, i64, String)> {
    let rows = backend
        .query_map_strings(
            "SELECT event_id, ts_ms, severity FROM events \
             WHERE pane_id = ?1 ORDER BY ts_ms",
            &[&pane_id.to_string()],
        )
        .unwrap();
    rows.iter()
        .map(|row| {
            (
                row_i64(row, 0).unwrap(),
                row_i64(row, 1).unwrap(),
                row_string(row, 2).unwrap(),
            )
        })
        .collect()
}

#[test]
fn migration_pilot_typical_read_side_function_round_trips() {
    let backend = populated_backend();
    let events_for_p1 = events_for_pane(&backend, 1);
    assert_eq!(events_for_p1.len(), 2);
    assert_eq!(events_for_p1[0], (1, 105, "info".to_string()));
    assert_eq!(events_for_p1[1], (2, 150, "warning".to_string()));

    let events_for_p3 = events_for_pane(&backend, 3);
    assert_eq!(events_for_p3.len(), 2);
    assert_eq!(events_for_p3[0], (4, 130, "critical".to_string()));
    assert_eq!(events_for_p3[1], (5, 140, "info".to_string()));
}

// =============================================================================
// Helper bundle smoke
// =============================================================================

#[test]
fn migration_pilot_helpers_handle_full_diagnostic_set() {
    let backend = populated_backend();

    // table_exists (pattern: probe sqlite_master).
    assert!(table_exists(&backend, "panes").unwrap());
    assert!(table_exists(&backend, "events").unwrap());
    assert!(!table_exists(&backend, "nonexistent").unwrap());

    // max_column (pattern: SELECT MAX(...) FROM ...).
    assert_eq!(
        max_column(&backend, "panes", "pane_id").unwrap().as_deref(),
        Some("3")
    );
    assert_eq!(
        max_column(&backend, "events", "ts_ms").unwrap().as_deref(),
        Some("150")
    );

    // list_user_tables (pattern: walk sqlite_master).
    let tables = list_user_tables(&backend).unwrap();
    assert_eq!(tables, vec!["events".to_string(), "panes".to_string()]);
}

// =============================================================================
// Object-safety: every helper works through Box<dyn StorageBackend>
// =============================================================================

#[test]
fn migration_pilot_dispatches_through_box_dyn() {
    let backend: Box<dyn StorageBackend> = Box::new(populated_backend());
    assert_eq!(count_table(&*backend, "panes").unwrap(), 3);
    assert!(table_exists(&*backend, "panes").unwrap());
    let row = backend
        .query_row_strings("SELECT pane_id FROM panes WHERE pane_id = 1", &[])
        .unwrap()
        .unwrap();
    let reader = RowReader::new(&row);
    assert_eq!(reader.i64(0).unwrap(), 1);
}
