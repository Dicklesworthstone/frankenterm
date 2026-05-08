//! ft-u6fba Phase 1b: extracted from storage.rs (mod timeline_correlation_tests).
//! Sibling submodule of `storage` — `use super::*;` resolves to
//! `crate::storage::*`.

use super::*;
use rusqlite::Connection;

fn create_test_event(
    id: i64,
    ts: i64,
    pane_id: u64,
    rule_id: &str,
    event_type: &str,
) -> TimelineEvent {
    TimelineEvent {
        id,
        timestamp: ts,
        pane_info: PaneInfo {
            pane_id,
            pane_uuid: None,
            domain: "local".to_string(),
            title: None,
            cwd: None,
            agent_type: None,
        },
        rule_id: rule_id.to_string(),
        event_type: event_type.to_string(),
        severity: "info".to_string(),
        confidence: 0.9,
        handled: None,
        correlations: Vec::new(),
        summary: None,
    }
}

#[test]
fn temporal_correlation_detects_close_events() {
    let events = vec![
        create_test_event(1, 1000, 1, "rule_a", "error"),
        create_test_event(2, 2000, 2, "rule_b", "error"), // Different pane, within 10s
        create_test_event(3, 3000, 1, "rule_c", "warning"),
    ];

    let correlations = detect_correlations(&events);

    // Should find temporal correlation between events in different panes
    let temporal = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Temporal)
        .collect::<Vec<_>>();

    assert!(!temporal.is_empty(), "Should detect temporal correlation");
    assert!(
        temporal
            .iter()
            .any(|c| c.event_ids.contains(&1) && c.event_ids.contains(&2)),
        "Should correlate events 1 and 2"
    );
}

#[test]
fn temporal_correlation_ignores_same_pane() {
    let events = vec![
        create_test_event(1, 1000, 1, "rule_a", "error"),
        create_test_event(2, 2000, 1, "rule_b", "error"), // Same pane
    ];

    let correlations = detect_correlations(&events);

    // Same pane events should not create temporal correlation
    let temporal = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Temporal)
        .collect::<Vec<_>>();

    assert!(
        temporal.is_empty() || !temporal.iter().any(|c| c.event_ids.len() > 1),
        "Should not correlate same-pane events"
    );
}

#[test]
fn temporal_correlation_respects_window() {
    let events = vec![
        create_test_event(1, 1000, 1, "rule_a", "error"),
        create_test_event(2, 15000, 2, "rule_b", "error"), // 14 seconds apart, outside 10s window
    ];

    let correlations = detect_correlations(&events);

    // Events too far apart should not correlate
    let temporal = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Temporal)
        .filter(|c| c.event_ids.contains(&1) && c.event_ids.contains(&2))
        .count();

    assert_eq!(
        temporal, 0,
        "Events outside 10s window should not correlate"
    );
}

#[test]
fn workflow_group_correlation_links_handled_events() {
    let mut event1 = create_test_event(1, 1000, 1, "rule_a", "error");
    event1.handled = Some(HandledInfo {
        handled_at: 2000,
        workflow_id: Some("wf-123".to_string()),
        status: "handled".to_string(),
    });

    let mut event2 = create_test_event(2, 1500, 2, "rule_b", "error");
    event2.handled = Some(HandledInfo {
        handled_at: 2100,
        workflow_id: Some("wf-123".to_string()), // Same workflow ID
        status: "handled".to_string(),
    });

    let event3 = create_test_event(3, 1200, 3, "rule_c", "warning");

    let correlations = detect_correlations(&[event1, event2, event3]);

    let workflow_corr = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::WorkflowGroup)
        .collect::<Vec<_>>();

    assert_eq!(workflow_corr.len(), 1, "Should find one workflow group");
    assert!(
        workflow_corr[0].event_ids.contains(&1) && workflow_corr[0].event_ids.contains(&2),
        "Should link events with same workflow ID"
    );
    assert!(
        !workflow_corr[0].event_ids.contains(&3),
        "Should not include unrelated event"
    );
}

#[test]
fn failover_correlation_detects_limit_then_session() {
    let events = vec![
        create_test_event(1, 1000, 1, "usage_limit", "usage_limit"),
        create_test_event(2, 30000, 2, "session_start", "session_start"), // Different pane, within 5min
    ];

    let correlations = detect_correlations(&events);

    let failover = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Failover)
        .collect::<Vec<_>>();

    assert_eq!(failover.len(), 1, "Should detect failover correlation");
    assert!(
        failover[0].event_ids.contains(&1) && failover[0].event_ids.contains(&2),
        "Should link usage_limit to session_start"
    );
}

#[test]
fn failover_correlation_ignores_same_pane() {
    let events = vec![
        create_test_event(1, 1000, 1, "usage_limit", "usage_limit"),
        create_test_event(2, 30000, 1, "session_start", "session_start"), // Same pane
    ];

    let correlations = detect_correlations(&events);

    let failover = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Failover)
        .count();

    assert_eq!(failover, 0, "Same-pane events should not be failover");
}

#[test]
fn failover_correlation_respects_5min_window() {
    let events = vec![
        create_test_event(1, 1000, 1, "usage_limit", "usage_limit"),
        create_test_event(2, 400000, 2, "session_start", "session_start"), // >5min apart
    ];

    let correlations = detect_correlations(&events);

    let failover = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Failover)
        .count();

    assert_eq!(failover, 0, "Events >5min apart should not be failover");
}

#[test]
fn correlation_confidence_values() {
    let mut event1 = create_test_event(1, 1000, 1, "rule_a", "error");
    event1.handled = Some(HandledInfo {
        handled_at: 2000,
        workflow_id: Some("wf-1".to_string()),
        status: "handled".to_string(),
    });
    let mut event2 = create_test_event(2, 1100, 2, "rule_b", "error");
    event2.handled = Some(HandledInfo {
        handled_at: 2100,
        workflow_id: Some("wf-1".to_string()),
        status: "handled".to_string(),
    });

    let correlations = detect_correlations(&[event1, event2]);

    for corr in &correlations {
        match corr.correlation_type {
            CorrelationType::Temporal => {
                assert!(
                    (corr.confidence - 0.6).abs() < 0.01,
                    "Temporal confidence should be 0.6"
                );
            }
            CorrelationType::WorkflowGroup => {
                assert!(
                    (corr.confidence - 0.95).abs() < 0.01,
                    "WorkflowGroup confidence should be 0.95"
                );
            }
            CorrelationType::Failover => {
                assert!(
                    (corr.confidence - 0.8).abs() < 0.01,
                    "Failover confidence should be 0.8"
                );
            }
            _ => {}
        }
    }
}

#[test]
fn empty_events_returns_empty_correlations() {
    let correlations = detect_correlations(&[]);
    assert!(correlations.is_empty());
}

#[test]
fn single_event_returns_empty_correlations() {
    let events = vec![create_test_event(1, 1000, 1, "rule_a", "error")];
    let correlations = detect_correlations(&events);
    assert!(correlations.is_empty());
}

#[test]
fn correlation_ids_are_unique() {
    let events = vec![
        create_test_event(1, 1000, 1, "rule_a", "error"),
        create_test_event(2, 2000, 2, "rule_b", "error"),
        create_test_event(3, 3000, 3, "rule_c", "error"),
        create_test_event(4, 4000, 4, "rule_d", "error"),
    ];

    let correlations = detect_correlations(&events);

    let ids: std::collections::HashSet<_> = correlations.iter().map(|c| &c.id).collect();
    assert_eq!(
        ids.len(),
        correlations.len(),
        "Correlation IDs should be unique"
    );
}

#[test]
fn correlation_type_display() {
    assert_eq!(CorrelationType::Failover.to_string(), "failover");
    assert_eq!(CorrelationType::Cascade.to_string(), "cascade");
    assert_eq!(CorrelationType::Temporal.to_string(), "temporal");
    assert_eq!(CorrelationType::WorkflowGroup.to_string(), "workflow_group");
    assert_eq!(CorrelationType::DedupeGroup.to_string(), "dedupe_group");
}

#[test]
fn dedupe_group_detects_same_rule_across_panes() {
    let events = vec![
        create_test_event(1, 1000, 1, "claude_code.usage.reached", "error"),
        create_test_event(2, 5000, 2, "claude_code.usage.reached", "error"),
        create_test_event(3, 8000, 3, "claude_code.usage.reached", "error"),
    ];

    let correlations = detect_correlations(&events);

    let dedupe = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::DedupeGroup)
        .collect::<Vec<_>>();

    assert_eq!(dedupe.len(), 1, "Should detect one dedupe group");
    assert_eq!(
        dedupe[0].event_ids.len(),
        3,
        "All three events should be grouped"
    );
    assert!(
        (dedupe[0].confidence - 0.7).abs() < 0.01,
        "DedupeGroup confidence should be 0.7"
    );
}

#[test]
fn dedupe_group_ignores_same_pane_only() {
    let events = vec![
        create_test_event(1, 1000, 1, "rule_a", "error"),
        create_test_event(2, 5000, 1, "rule_a", "error"), // Same pane
    ];

    let correlations = detect_correlations(&events);

    let dedupe = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::DedupeGroup)
        .count();

    assert_eq!(
        dedupe, 0,
        "Same-pane-only events should not form dedupe group"
    );
}

#[test]
fn dedupe_group_respects_window() {
    let events = vec![
        create_test_event(1, 1000, 1, "rule_a", "error"),
        create_test_event(2, 50000, 2, "rule_a", "error"), // 49s apart, outside 30s window
    ];

    let correlations = detect_correlations(&events);

    let dedupe = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::DedupeGroup)
        .count();

    assert_eq!(
        dedupe, 0,
        "Events outside 30s window should not form dedupe group"
    );
}

#[test]
fn dedupe_group_different_rules_not_grouped() {
    let events = vec![
        create_test_event(1, 1000, 1, "rule_a", "error"),
        create_test_event(2, 2000, 2, "rule_b", "error"),
    ];

    let correlations = detect_correlations(&events);

    let dedupe = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::DedupeGroup)
        .count();

    assert_eq!(dedupe, 0, "Different rule_ids should not form dedupe group");
}

#[test]
fn temporal_window_10s_boundary() {
    // Exactly at boundary: 10s apart should still correlate
    let events = vec![
        create_test_event(1, 1000, 1, "rule_a", "error"),
        create_test_event(2, 11000, 2, "rule_b", "error"), // Exactly 10s
    ];

    let correlations = detect_correlations(&events);

    let temporal = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Temporal)
        .filter(|c| c.event_ids.contains(&1) && c.event_ids.contains(&2))
        .count();

    assert_eq!(temporal, 1, "Events exactly 10s apart should correlate");
}

#[test]
fn temporal_window_just_outside() {
    // 10.001s apart should not correlate
    let events = vec![
        create_test_event(1, 1000, 1, "rule_a", "error"),
        create_test_event(2, 11002, 2, "rule_b", "error"), // >10s
    ];

    let correlations = detect_correlations(&events);

    let temporal = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Temporal)
        .filter(|c| c.event_ids.contains(&1) && c.event_ids.contains(&2))
        .count();

    assert_eq!(
        temporal, 0,
        "Events >10s apart should not temporally correlate"
    );
}

#[test]
fn failover_within_5min_window_detected() {
    // 4 minutes apart (240s) — within 5min window
    let events = vec![
        create_test_event(1, 1000, 1, "usage_limit", "usage.reached"),
        create_test_event(2, 241000, 2, "session_start", "session.start"),
    ];

    let correlations = detect_correlations(&events);

    let failover = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Failover)
        .count();

    assert_eq!(failover, 1, "Events 4min apart should detect failover");
}

#[test]
fn cascade_error_then_recovery_detected() {
    let mut event1 = create_test_event(1, 1000, 1, "rule_a", "error");
    event1.severity = "error".to_string();

    let event2 = create_test_event(2, 15000, 2, "session.resume", "session.resume");

    let correlations = detect_correlations(&[event1, event2]);

    let cascade = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Cascade)
        .collect::<Vec<_>>();

    assert_eq!(cascade.len(), 1, "Should detect cascade correlation");
    assert!(
        (cascade[0].confidence - 0.75).abs() < 0.01,
        "Cascade confidence should be 0.75"
    );
}

#[test]
fn cascade_ignores_non_error_severity() {
    let event1 = create_test_event(1, 1000, 1, "rule_a", "info");
    let event2 = create_test_event(2, 5000, 2, "session.resume", "session.resume");

    let correlations = detect_correlations(&[event1, event2]);

    let cascade = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Cascade)
        .count();

    assert_eq!(cascade, 0, "Non-error severity should not trigger cascade");
}

#[test]
fn cascade_respects_30s_window() {
    let mut event1 = create_test_event(1, 1000, 1, "rule_a", "error");
    event1.severity = "error".to_string();

    let event2 = create_test_event(2, 40000, 2, "session.resume", "session.resume");

    let correlations = detect_correlations(&[event1, event2]);

    let cascade = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Cascade)
        .count();

    assert_eq!(cascade, 0, "Events >30s apart should not cascade");
}

#[test]
fn failover_agent_type_mismatch_no_correlation() {
    let mut event1 = create_test_event(1, 1000, 1, "usage_limit", "usage.reached");
    event1.pane_info.agent_type = Some("claude_code".to_string());

    let mut event2 = create_test_event(2, 30000, 2, "session_start", "session.start");
    event2.pane_info.agent_type = Some("codex".to_string());

    let correlations = detect_correlations(&[event1, event2]);

    let failover = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Failover)
        .count();

    assert_eq!(
        failover, 0,
        "Different agent types should not create failover correlation"
    );
}

#[test]
fn failover_same_agent_type_correlates() {
    let mut event1 = create_test_event(1, 1000, 1, "usage_limit", "usage.reached");
    event1.pane_info.agent_type = Some("claude_code".to_string());

    let mut event2 = create_test_event(2, 30000, 2, "session_start", "session.start");
    event2.pane_info.agent_type = Some("claude_code".to_string());

    let correlations = detect_correlations(&[event1, event2]);

    let failover = correlations
        .iter()
        .filter(|c| c.correlation_type == CorrelationType::Failover)
        .count();

    assert_eq!(
        failover, 1,
        "Same agent type should create failover correlation"
    );
}

#[test]
fn correlation_serde_roundtrip() {
    let corr = Correlation {
        id: "corr-test-1".to_string(),
        event_ids: vec![1, 2, 3],
        correlation_type: CorrelationType::DedupeGroup,
        confidence: 0.7,
        description: "Test correlation".to_string(),
    };

    let json = serde_json::to_string(&corr).unwrap();
    let deserialized: Correlation = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.id, corr.id);
    assert_eq!(deserialized.event_ids, corr.event_ids);
    assert_eq!(deserialized.correlation_type, CorrelationType::DedupeGroup);
    assert!((deserialized.confidence - 0.7).abs() < f64::EPSILON);
}

#[test]
fn multiple_correlation_types_coexist() {
    // Two events: close in time, same workflow, same rule in different panes
    let mut event1 = create_test_event(1, 1000, 1, "rule_x", "error");
    event1.severity = "error".to_string();
    event1.handled = Some(HandledInfo {
        handled_at: 2000,
        workflow_id: Some("wf-99".to_string()),
        status: "handled".to_string(),
    });

    let mut event2 = create_test_event(2, 3000, 2, "rule_x", "session.resume");
    event2.handled = Some(HandledInfo {
        handled_at: 4000,
        workflow_id: Some("wf-99".to_string()),
        status: "handled".to_string(),
    });

    let correlations = detect_correlations(&[event1, event2]);

    let types: std::collections::HashSet<_> =
        correlations.iter().map(|c| c.correlation_type).collect();

    // Should have at least temporal + workflow + dedupe (and possibly cascade)
    assert!(
        types.contains(&CorrelationType::Temporal),
        "Should detect temporal correlation"
    );
    assert!(
        types.contains(&CorrelationType::WorkflowGroup),
        "Should detect workflow group"
    );
    assert!(
        types.contains(&CorrelationType::DedupeGroup),
        "Should detect dedupe group (same rule_id across panes)"
    );
}

#[test]
fn many_events_performance_no_panic() {
    // Verify detect_correlations handles a larger event set without panicking
    let events: Vec<TimelineEvent> = (0..100)
        .map(|i| create_test_event(i, i * 500, (i % 5) as u64 + 1, "rule_perf", "warning"))
        .collect();

    let correlations = detect_correlations(&events);

    // Should produce some correlations without panicking
    assert!(
        !correlations.is_empty(),
        "100 events across 5 panes should produce correlations"
    );
    // All IDs should be unique
    let ids: std::collections::HashSet<_> = correlations.iter().map(|c| &c.id).collect();
    assert_eq!(ids.len(), correlations.len());
}

// =========================================================================
// Pane Bookmark Tests
// =========================================================================

fn insert_pane_bookmark_sync(conn: &Connection, record: &PaneBookmarkRecord) -> Result<i64> {
    let tags_json = record
        .tags
        .as_ref()
        .map(|t| serde_json::to_string(t).unwrap_or_else(|_| "[]".to_string()));

    conn.query_row(
        "INSERT INTO pane_bookmarks (pane_id, alias, tags, description, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         RETURNING id",
        params![
            record.pane_id as i64,
            record.alias,
            tags_json,
            record.description,
            record.created_at,
            record.updated_at,
        ],
        |row| row.get(0),
    )
    .map_err(|e| StorageError::Database(format!("Failed to insert pane bookmark: {e}")).into())
}

fn delete_pane_bookmark_sync(conn: &Connection, alias: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "DELETE FROM pane_bookmarks WHERE alias = ?1 RETURNING 1",
            [alias],
            |_| Ok(()),
        )
        .optional()
        .map_err(|e| StorageError::Database(format!("Failed to delete pane bookmark: {e}")))?
        .is_some())
}

fn pane_bookmark_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PaneBookmarkRecord> {
    let pane_id_raw: i64 = row.get(1)?;
    let tags_raw: Option<String> = row.get(3)?;
    let tags = tags_raw.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok());
    Ok(PaneBookmarkRecord {
        id: row.get(0)?,
        pane_id: i64_to_u64_sql(pane_id_raw, 1, "pane_bookmarks.pane_id")?,
        alias: row.get(2)?,
        tags,
        description: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn query_pane_bookmark_by_alias(
    conn: &Connection,
    alias: &str,
) -> Result<Option<PaneBookmarkRecord>> {
    Ok(conn
        .query_row(
            "SELECT id, pane_id, alias, tags, description, created_at, updated_at
             FROM pane_bookmarks WHERE alias = ?1",
            [alias],
            pane_bookmark_from_row,
        )
        .optional()
        .map_err(|e| StorageError::Database(format!("Failed to query pane bookmark: {e}")))?)
}

fn list_pane_bookmarks_sync(conn: &Connection) -> Result<Vec<PaneBookmarkRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, pane_id, alias, tags, description, created_at, updated_at
             FROM pane_bookmarks ORDER BY alias ASC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to list pane bookmarks: {e}")))?;
    let rows = stmt
        .query_map([], pane_bookmark_from_row)
        .map_err(|e| StorageError::Database(format!("Failed to list pane bookmarks: {e}")))?;
    let mut bookmarks = Vec::new();
    for row in rows {
        bookmarks.push(row.map_err(|e| StorageError::Database(format!("{e}")))?);
    }
    Ok(bookmarks)
}

fn list_pane_bookmarks_by_tag_sync(
    conn: &Connection,
    tag: &str,
) -> Result<Vec<PaneBookmarkRecord>> {
    let pattern = format!("%\"{tag}\"%");
    let mut stmt = conn
        .prepare(
            "SELECT id, pane_id, alias, tags, description, created_at, updated_at
             FROM pane_bookmarks WHERE tags LIKE ?1 ORDER BY alias ASC",
        )
        .map_err(|e| {
            StorageError::Database(format!("Failed to list pane bookmarks by tag: {e}"))
        })?;
    let rows = stmt
        .query_map([pattern], pane_bookmark_from_row)
        .map_err(|e| {
            StorageError::Database(format!("Failed to list pane bookmarks by tag: {e}"))
        })?;
    let mut bookmarks = Vec::new();
    for row in rows {
        bookmarks.push(row.map_err(|e| StorageError::Database(format!("{e}")))?);
    }
    Ok(bookmarks)
}

#[test]
fn pane_bookmark_insert_and_query() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    let record = PaneBookmarkRecord {
        id: 0,
        pane_id: 42,
        alias: "build".to_string(),
        tags: Some(vec!["ci".to_string(), "important".to_string()]),
        description: Some("The build pane".to_string()),
        created_at: now,
        updated_at: now,
    };

    let id = insert_pane_bookmark_sync(&conn, &record).unwrap();
    assert!(id > 0);

    let fetched = query_pane_bookmark_by_alias(&conn, "build").unwrap();
    assert!(fetched.is_some());
    let bm = fetched.unwrap();
    assert_eq!(bm.pane_id, 42);
    assert_eq!(bm.alias, "build");
    assert_eq!(
        bm.tags,
        Some(vec!["ci".to_string(), "important".to_string()])
    );
    assert_eq!(bm.description.as_deref(), Some("The build pane"));
}

#[test]
fn pane_bookmark_alias_unique() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    let record = PaneBookmarkRecord {
        id: 0,
        pane_id: 1,
        alias: "main".to_string(),
        tags: None,
        description: None,
        created_at: now,
        updated_at: now,
    };

    insert_pane_bookmark_sync(&conn, &record).unwrap();
    let result = insert_pane_bookmark_sync(&conn, &record);
    assert!(result.is_err(), "Duplicate alias should fail");
}

#[test]
fn pane_bookmark_list_and_delete() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    for (pane, alias) in [(1, "alpha"), (2, "beta"), (3, "gamma")] {
        let record = PaneBookmarkRecord {
            id: 0,
            pane_id: pane,
            alias: alias.to_string(),
            tags: None,
            description: None,
            created_at: now,
            updated_at: now,
        };
        insert_pane_bookmark_sync(&conn, &record).unwrap();
    }

    let list = list_pane_bookmarks_sync(&conn).unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0].alias, "alpha");
    assert_eq!(list[1].alias, "beta");
    assert_eq!(list[2].alias, "gamma");

    let deleted = delete_pane_bookmark_sync(&conn, "beta").unwrap();
    assert!(deleted);

    let list2 = list_pane_bookmarks_sync(&conn).unwrap();
    assert_eq!(list2.len(), 2);

    let not_found = delete_pane_bookmark_sync(&conn, "nonexistent").unwrap();
    assert!(!not_found);
}

#[test]
fn pane_bookmark_filter_by_tag() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    let records = vec![
        PaneBookmarkRecord {
            id: 0,
            pane_id: 1,
            alias: "web".to_string(),
            tags: Some(vec!["frontend".to_string(), "prod".to_string()]),
            description: None,
            created_at: now,
            updated_at: now,
        },
        PaneBookmarkRecord {
            id: 0,
            pane_id: 2,
            alias: "api".to_string(),
            tags: Some(vec!["backend".to_string(), "prod".to_string()]),
            description: None,
            created_at: now,
            updated_at: now,
        },
        PaneBookmarkRecord {
            id: 0,
            pane_id: 3,
            alias: "test".to_string(),
            tags: Some(vec!["ci".to_string()]),
            description: None,
            created_at: now,
            updated_at: now,
        },
    ];

    for r in &records {
        insert_pane_bookmark_sync(&conn, r).unwrap();
    }

    let prod = list_pane_bookmarks_by_tag_sync(&conn, "prod").unwrap();
    assert_eq!(prod.len(), 2);

    let ci = list_pane_bookmarks_by_tag_sync(&conn, "ci").unwrap();
    assert_eq!(ci.len(), 1);
    assert_eq!(ci[0].alias, "test");

    let none = list_pane_bookmarks_by_tag_sync(&conn, "nonexistent").unwrap();
    assert!(none.is_empty());
}

#[test]
fn pane_bookmark_persists_across_restarts() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    let record = PaneBookmarkRecord {
        id: 0,
        pane_id: 99,
        alias: "persistent".to_string(),
        tags: Some(vec!["durable".to_string()]),
        description: Some("survives restart".to_string()),
        created_at: now,
        updated_at: now,
    };

    insert_pane_bookmark_sync(&conn, &record).unwrap();

    // Simulate "restart" by querying fresh (same in-memory DB)
    let fetched = query_pane_bookmark_by_alias(&conn, "persistent")
        .unwrap()
        .unwrap();
    assert_eq!(fetched.pane_id, 99);
    assert_eq!(fetched.description.as_deref(), Some("survives restart"));
}

#[test]
fn pane_bookmark_query_rejects_negative_pane_id() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    conn.execute(
        "INSERT INTO pane_bookmarks (pane_id, alias, tags, description, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            -7i64,
            "bad-bookmark",
            Option::<String>::None,
            Option::<String>::None,
            now,
            now,
        ],
    )
    .unwrap();

    let err = query_pane_bookmark_by_alias(&conn, "bad-bookmark").expect_err("negative pane id");
    let message = err.to_string();
    assert!(message.contains("pane_bookmarks.pane_id"), "{message}");
    assert!(message.contains("-7"), "{message}");
}

#[test]
fn pane_bookmark_nonexistent_alias_returns_none() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let result = query_pane_bookmark_by_alias(&conn, "ghost").unwrap();
    assert!(result.is_none());
}

#[test]
fn pane_bookmark_multiple_for_same_pane() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    for alias in ["build-1", "build-2", "build-3"] {
        insert_pane_bookmark_sync(
            &conn,
            &PaneBookmarkRecord {
                id: 0,
                pane_id: 42,
                alias: alias.to_string(),
                tags: None,
                description: None,
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();
    }

    let list = list_pane_bookmarks_sync(&conn).unwrap();
    assert_eq!(list.len(), 3);
    assert!(list.iter().all(|bm| bm.pane_id == 42));
}

#[test]
fn pane_bookmark_null_tags_vs_empty_tags() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    insert_pane_bookmark_sync(
        &conn,
        &PaneBookmarkRecord {
            id: 0,
            pane_id: 1,
            alias: "notags".to_string(),
            tags: None,
            description: None,
            created_at: now,
            updated_at: now,
        },
    )
    .unwrap();

    insert_pane_bookmark_sync(
        &conn,
        &PaneBookmarkRecord {
            id: 0,
            pane_id: 2,
            alias: "emptytags".to_string(),
            tags: Some(vec![]),
            description: None,
            created_at: now,
            updated_at: now,
        },
    )
    .unwrap();

    let no_tags = query_pane_bookmark_by_alias(&conn, "notags")
        .unwrap()
        .unwrap();
    assert!(no_tags.tags.is_none());

    let empty_tags = query_pane_bookmark_by_alias(&conn, "emptytags")
        .unwrap()
        .unwrap();
    assert_eq!(empty_tags.tags, Some(vec![]));
}

#[test]
fn pane_bookmark_case_sensitive_aliases() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    for alias in ["build", "Build", "BUILD"] {
        insert_pane_bookmark_sync(
            &conn,
            &PaneBookmarkRecord {
                id: 0,
                pane_id: 1,
                alias: alias.to_string(),
                tags: None,
                description: None,
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();
    }

    let list = list_pane_bookmarks_sync(&conn).unwrap();
    assert_eq!(list.len(), 3, "Case-different aliases should be distinct");
}

#[test]
fn pane_bookmark_nonexistent_pane_id_allowed() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    let result = insert_pane_bookmark_sync(
        &conn,
        &PaneBookmarkRecord {
            id: 0,
            pane_id: 999_999,
            alias: "phantom".to_string(),
            tags: None,
            description: None,
            created_at: now,
            updated_at: now,
        },
    );
    assert!(
        result.is_ok(),
        "Should allow bookmark for non-existent pane"
    );
}

#[test]
fn pane_bookmark_empty_description_vs_none() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    insert_pane_bookmark_sync(
        &conn,
        &PaneBookmarkRecord {
            id: 0,
            pane_id: 1,
            alias: "nodesc".to_string(),
            tags: None,
            description: None,
            created_at: now,
            updated_at: now,
        },
    )
    .unwrap();

    insert_pane_bookmark_sync(
        &conn,
        &PaneBookmarkRecord {
            id: 0,
            pane_id: 2,
            alias: "emptydesc".to_string(),
            tags: None,
            description: Some(String::new()),
            created_at: now,
            updated_at: now,
        },
    )
    .unwrap();

    let no_desc = query_pane_bookmark_by_alias(&conn, "nodesc")
        .unwrap()
        .unwrap();
    assert!(no_desc.description.is_none());

    let empty_desc = query_pane_bookmark_by_alias(&conn, "emptydesc")
        .unwrap()
        .unwrap();
    assert_eq!(empty_desc.description, Some(String::new()));
}

#[test]
fn pane_bookmark_tag_with_special_chars() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    let tags = vec![
        "normal".to_string(),
        "has spaces".to_string(),
        "quote\"in".to_string(),
    ];
    insert_pane_bookmark_sync(
        &conn,
        &PaneBookmarkRecord {
            id: 0,
            pane_id: 1,
            alias: "specialtags".to_string(),
            tags: Some(tags.clone()),
            description: None,
            created_at: now,
            updated_at: now,
        },
    )
    .unwrap();

    let fetched = query_pane_bookmark_by_alias(&conn, "specialtags")
        .unwrap()
        .unwrap();
    assert_eq!(fetched.tags.unwrap(), tags);
}

#[test]
fn pane_bookmark_list_empty_db() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let list = list_pane_bookmarks_sync(&conn).unwrap();
    assert!(list.is_empty());

    let by_tag = list_pane_bookmarks_by_tag_sync(&conn, "anything").unwrap();
    assert!(by_tag.is_empty());
}

#[test]
fn migration_invariant_summary_from_report_counts_severities() {
    let report = InvariantReport {
        violations: vec![
            crate::recorder_invariants::Violation {
                kind: crate::recorder_invariants::ViolationKind::SequenceGap,
                severity: crate::recorder_invariants::ViolationSeverity::Warning,
                event_id: "evt-warning".to_string(),
                pane_id: 1,
                message: "warning".to_string(),
                event_index: 0,
            },
            crate::recorder_invariants::Violation {
                kind: crate::recorder_invariants::ViolationKind::SequenceRegression,
                severity: crate::recorder_invariants::ViolationSeverity::Error,
                event_id: "evt-error".to_string(),
                pane_id: 1,
                message: "error".to_string(),
                event_index: 1,
            },
            crate::recorder_invariants::Violation {
                kind: crate::recorder_invariants::ViolationKind::DuplicateEventId,
                severity: crate::recorder_invariants::ViolationSeverity::Critical,
                event_id: "evt-critical".to_string(),
                pane_id: 1,
                message: "critical".to_string(),
                event_index: 2,
            },
        ],
        events_checked: 3,
        panes_observed: 1,
        domains_observed: 1,
        passed: false,
        backend_kind: None,
    };

    let summary = MigrationInvariantSummary::from_report(&report);
    assert_eq!(summary.warning_count, 1);
    assert_eq!(summary.error_count, 1);
    assert_eq!(summary.critical_count, 1);
    assert!(summary.has_breakage());
}

#[test]
fn migration_rollback_classifier_prefers_data_integrity_emergency() {
    let input = MigrationRollbackClassifierInput {
        stage: MigrationStage::Import,
        confirmed_canonical_data_loss: true,
        import_digest_mismatch: true,
        ..Default::default()
    };

    let decision = classify_migration_rollback_trigger(&input);
    assert!(decision.should_rollback);
    assert_eq!(
        decision.rollback_class,
        Some(MigrationRollbackClass::DataIntegrityEmergency)
    );
    assert!(
        decision
            .triggers
            .contains(&MigrationRollbackTrigger::CanonicalDataLossConfirmed)
    );
    assert!(
        decision
            .triggers
            .contains(&MigrationRollbackTrigger::ImportDigestMismatch)
    );
}

#[test]
fn migration_rollback_classifier_immediate_on_checkpoint_or_invariant_breakage() {
    let input = MigrationRollbackClassifierInput {
        stage: MigrationStage::CheckpointSync,
        checkpoint_regression: true,
        invariants: Some(MigrationInvariantSummary {
            warning_count: 0,
            error_count: 1,
            critical_count: 0,
        }),
        ..Default::default()
    };

    let decision = classify_migration_rollback_trigger(&input);
    assert!(decision.should_rollback);
    assert_eq!(
        decision.rollback_class,
        Some(MigrationRollbackClass::Immediate)
    );
    assert!(
        decision
            .triggers
            .contains(&MigrationRollbackTrigger::CheckpointRegression)
    );
    assert!(
        decision
            .triggers
            .contains(&MigrationRollbackTrigger::InvariantErrors)
    );
}

#[test]
fn migration_rollback_classifier_requires_sustained_slo_breach_window() {
    let config = MigrationRollbackClassifierConfig {
        sustained_slo_windows: 3,
        repeated_write_failure_threshold: 3,
        repeated_index_failure_threshold: 3,
    };
    let base_input = MigrationRollbackClassifierInput {
        stage: MigrationStage::Soak,
        storage_slo: Some(MigrationStorageSloSummary {
            health_tier: StorageHealthTier::Black,
            slo_append_p95: SloStatus::Breached,
            slo_flush_p95: SloStatus::Met,
        }),
        projection_lag_breach: true,
        config,
        ..Default::default()
    };

    let not_sustained = MigrationRollbackClassifierInput {
        consecutive_slo_breach_windows: 2,
        ..base_input.clone()
    };
    let decision_not_sustained = classify_migration_rollback_trigger(&not_sustained);
    assert!(!decision_not_sustained.should_rollback);

    let sustained = MigrationRollbackClassifierInput {
        consecutive_slo_breach_windows: 3,
        ..base_input
    };
    let decision_sustained = classify_migration_rollback_trigger(&sustained);
    assert!(decision_sustained.should_rollback);
    assert_eq!(
        decision_sustained.rollback_class,
        Some(MigrationRollbackClass::PostCutover)
    );
    assert!(
        decision_sustained
            .triggers
            .contains(&MigrationRollbackTrigger::SustainedSloBreach)
    );
    assert!(
        decision_sustained
            .triggers
            .contains(&MigrationRollbackTrigger::ProjectionLagBreach)
    );
}

#[test]
fn migration_rollback_classifier_post_cutover_on_repeated_failures() {
    let input = MigrationRollbackClassifierInput {
        stage: MigrationStage::Soak,
        high_severity_write_failures: 3,
        high_severity_index_failures: 4,
        policy_audit_regression: true,
        ..Default::default()
    };

    let decision = classify_migration_rollback_trigger(&input);
    assert!(decision.should_rollback);
    assert_eq!(
        decision.rollback_class,
        Some(MigrationRollbackClass::PostCutover)
    );
    assert!(
        decision
            .triggers
            .contains(&MigrationRollbackTrigger::RepeatedWriteFailures)
    );
    assert!(
        decision
            .triggers
            .contains(&MigrationRollbackTrigger::RepeatedIndexFailures)
    );
    assert!(
        decision
            .triggers
            .contains(&MigrationRollbackTrigger::PolicyAuditRegression)
    );
}

fn default_rollback_context(
    rollback_class: MigrationRollbackClass,
    from_stage: MigrationStage,
    pre_migration_checkpoints: BTreeMap<String, RecorderOffset>,
) -> MigrationRollbackPlaybookContext {
    MigrationRollbackPlaybookContext {
        rollback_class,
        from_stage,
        pre_migration_checkpoints,
        forensic_capture: None,
        forensics_output_dir: std::env::temp_dir(),
    }
}

fn sample_forensic_capture() -> MigrationForensicCaptureContext {
    MigrationForensicCaptureContext {
        source_state: MigrationForensicBackendState {
            health: true,
            head_offset: Some(RecorderOffset {
                segment_id: 1,
                byte_offset: 512,
                ordinal: 32,
            }),
            last_checkpoint: Some(RecorderOffset {
                segment_id: 1,
                byte_offset: 480,
                ordinal: 30,
            }),
        },
        target_state: MigrationForensicBackendState {
            health: false,
            head_offset: Some(RecorderOffset {
                segment_id: 7,
                byte_offset: 1024,
                ordinal: 41,
            }),
            last_checkpoint: Some(RecorderOffset {
                segment_id: 7,
                byte_offset: 1000,
                ordinal: 40,
            }),
        },
        migration_checkpoint: MigrationForensicMigrationCheckpoint {
            last_completed_stage: MigrationStage::ProjectionRebuild,
            manifest: "manifest_sha256:abc123".to_string(),
        },
        corruption_detail: MigrationForensicCorruptionDetail {
            location: "target.events.segment_7".to_string(),
            affected_ordinals: vec![39, 40, 41],
            detail: "checksum mismatch at ordinal 41".to_string(),
        },
    }
}

#[test]
fn test_tier1_rollback_unquiesces_source() {
    let mut state = MigrationRollbackExecutionState {
        migration_active: true,
        backend_selector: RecorderBackendKind::FrankenSqlite,
        target_has_partial_data: true,
        source_health: true,
        index_health: true,
        ..Default::default()
    };
    let context = default_rollback_context(
        MigrationRollbackClass::Immediate,
        MigrationStage::CheckpointSync,
        BTreeMap::new(),
    );

    let report = execute_migration_rollback_playbook(&mut state, &context).unwrap();
    assert!(!state.migration_active);
    assert!(!report.migration_active);
}

#[test]
fn test_tier1_rollback_clears_target() {
    let mut state = MigrationRollbackExecutionState {
        migration_active: true,
        backend_selector: RecorderBackendKind::FrankenSqlite,
        target_has_partial_data: true,
        source_health: true,
        index_health: true,
        ..Default::default()
    };
    let context = default_rollback_context(
        MigrationRollbackClass::Immediate,
        MigrationStage::Import,
        BTreeMap::new(),
    );

    let report = execute_migration_rollback_playbook(&mut state, &context).unwrap();
    assert!(!state.target_has_partial_data);
    assert!(report.target_cleared);
}

#[test]
fn test_tier1_rollback_restores_backend_selector() {
    let mut state = MigrationRollbackExecutionState {
        migration_active: true,
        backend_selector: RecorderBackendKind::FrankenSqlite,
        target_has_partial_data: true,
        source_health: true,
        index_health: true,
        ..Default::default()
    };
    let context = default_rollback_context(
        MigrationRollbackClass::Immediate,
        MigrationStage::ProjectionRebuild,
        BTreeMap::new(),
    );

    let report = execute_migration_rollback_playbook(&mut state, &context).unwrap();
    assert_eq!(state.backend_selector, RecorderBackendKind::AppendLog);
    assert_eq!(report.backend_selector, RecorderBackendKind::AppendLog);
}

#[test]
fn test_tier2_rollback_triggers_projection_rebuild() {
    let mut state = MigrationRollbackExecutionState {
        migration_active: false,
        backend_selector: RecorderBackendKind::FrankenSqlite,
        target_has_partial_data: false,
        source_health: true,
        index_health: true,
        ..Default::default()
    };
    let context = default_rollback_context(
        MigrationRollbackClass::PostCutover,
        MigrationStage::Soak,
        BTreeMap::new(),
    );

    let report = execute_migration_rollback_playbook(&mut state, &context).unwrap();
    assert!(state.projection_rebuild_triggered);
    assert!(report.projection_rebuild_triggered);
}

#[test]
fn test_tier2_rollback_resets_checkpoints() {
    let mut state = MigrationRollbackExecutionState {
        migration_active: false,
        backend_selector: RecorderBackendKind::FrankenSqlite,
        target_has_partial_data: false,
        source_health: true,
        index_health: true,
        checkpoints: BTreeMap::from([(
            "lexical".to_string(),
            RecorderOffset {
                segment_id: 9,
                byte_offset: 999,
                ordinal: 999,
            },
        )]),
        ..Default::default()
    };
    let pre_migration = BTreeMap::from([
        (
            "lexical".to_string(),
            RecorderOffset {
                segment_id: 1,
                byte_offset: 10,
                ordinal: 10,
            },
        ),
        (
            "semantic".to_string(),
            RecorderOffset {
                segment_id: 1,
                byte_offset: 20,
                ordinal: 20,
            },
        ),
    ]);
    let context = default_rollback_context(
        MigrationRollbackClass::PostCutover,
        MigrationStage::Soak,
        pre_migration.clone(),
    );

    let report = execute_migration_rollback_playbook(&mut state, &context).unwrap();
    assert_eq!(state.checkpoints, pre_migration);
    assert!(report.checkpoints_reset);
}

#[test]
fn test_rollback_verifies_source_health() {
    let mut tier1_state = MigrationRollbackExecutionState {
        migration_active: true,
        backend_selector: RecorderBackendKind::FrankenSqlite,
        target_has_partial_data: true,
        source_health: false,
        index_health: true,
        ..Default::default()
    };
    let tier1_context = default_rollback_context(
        MigrationRollbackClass::Immediate,
        MigrationStage::CheckpointSync,
        BTreeMap::new(),
    );
    let tier1_err =
        execute_migration_rollback_playbook(&mut tier1_state, &tier1_context).unwrap_err();
    assert_eq!(
        tier1_err,
        MigrationRollbackExecutionError::SourceHealthFailed {
            tier: MigrationRollbackClass::Immediate
        }
    );

    let mut tier2_state = MigrationRollbackExecutionState {
        migration_active: false,
        backend_selector: RecorderBackendKind::FrankenSqlite,
        target_has_partial_data: false,
        source_health: true,
        index_health: false,
        ..Default::default()
    };
    let tier2_context = default_rollback_context(
        MigrationRollbackClass::PostCutover,
        MigrationStage::Soak,
        BTreeMap::new(),
    );
    let tier2_err =
        execute_migration_rollback_playbook(&mut tier2_state, &tier2_context).unwrap_err();
    assert_eq!(
        tier2_err,
        MigrationRollbackExecutionError::IndexHealthFailedPostCutover
    );
}

#[test]
fn test_tier3_freezes_writes() {
    let temp = tempfile::tempdir().unwrap();
    let forensic_capture = sample_forensic_capture();
    let context = MigrationRollbackPlaybookContext {
        rollback_class: MigrationRollbackClass::DataIntegrityEmergency,
        from_stage: MigrationStage::Activate,
        pre_migration_checkpoints: BTreeMap::new(),
        forensic_capture: Some(forensic_capture),
        forensics_output_dir: temp.path().to_path_buf(),
    };
    let mut state = MigrationRollbackExecutionState::default();

    let report = execute_migration_rollback_playbook(&mut state, &context).unwrap();

    assert!(state.recorder_writes_blocked());
    assert!(report.emergency_freeze_active);
}

#[test]
fn test_tier3_captures_forensic_bundle() {
    let temp = tempfile::tempdir().unwrap();
    let context = MigrationRollbackPlaybookContext {
        rollback_class: MigrationRollbackClass::DataIntegrityEmergency,
        from_stage: MigrationStage::Activate,
        pre_migration_checkpoints: BTreeMap::new(),
        forensic_capture: Some(sample_forensic_capture()),
        forensics_output_dir: temp.path().to_path_buf(),
    };
    let mut state = MigrationRollbackExecutionState::default();

    let report = execute_migration_rollback_playbook(&mut state, &context).unwrap();
    let path = report.forensic_bundle_path.unwrap();

    assert!(path.exists());
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(name.starts_with("forensics_"));
    assert!(name.ends_with(".json"));
}

#[test]
fn test_tier3_bundle_contains_source_and_target_state() {
    let temp = tempfile::tempdir().unwrap();
    let forensic_capture = sample_forensic_capture();
    let context = MigrationRollbackPlaybookContext {
        rollback_class: MigrationRollbackClass::DataIntegrityEmergency,
        from_stage: MigrationStage::Activate,
        pre_migration_checkpoints: BTreeMap::new(),
        forensic_capture: Some(forensic_capture.clone()),
        forensics_output_dir: temp.path().to_path_buf(),
    };
    let mut state = MigrationRollbackExecutionState::default();

    let report = execute_migration_rollback_playbook(&mut state, &context).unwrap();
    let bytes = std::fs::read(report.forensic_bundle_path.unwrap()).unwrap();
    let bundle: MigrationForensicBundle = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(bundle.source_state, forensic_capture.source_state);
    assert_eq!(bundle.target_state, forensic_capture.target_state);
}

#[test]
fn test_tier3_bundle_contains_migration_checkpoint() {
    let temp = tempfile::tempdir().unwrap();
    let forensic_capture = sample_forensic_capture();
    let context = MigrationRollbackPlaybookContext {
        rollback_class: MigrationRollbackClass::DataIntegrityEmergency,
        from_stage: MigrationStage::Activate,
        pre_migration_checkpoints: BTreeMap::new(),
        forensic_capture: Some(forensic_capture.clone()),
        forensics_output_dir: temp.path().to_path_buf(),
    };
    let mut state = MigrationRollbackExecutionState::default();

    let report = execute_migration_rollback_playbook(&mut state, &context).unwrap();
    let bytes = std::fs::read(report.forensic_bundle_path.unwrap()).unwrap();
    let bundle: MigrationForensicBundle = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        bundle.migration_checkpoint,
        forensic_capture.migration_checkpoint
    );
}

#[test]
fn test_tier3_remains_frozen_until_manual_reenable() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = MigrationRollbackExecutionState::default();
    let emergency_context = MigrationRollbackPlaybookContext {
        rollback_class: MigrationRollbackClass::DataIntegrityEmergency,
        from_stage: MigrationStage::Activate,
        pre_migration_checkpoints: BTreeMap::new(),
        forensic_capture: Some(sample_forensic_capture()),
        forensics_output_dir: temp.path().to_path_buf(),
    };

    execute_migration_rollback_playbook(&mut state, &emergency_context).unwrap();
    assert!(state.recorder_writes_blocked());

    let immediate_context = default_rollback_context(
        MigrationRollbackClass::Immediate,
        MigrationStage::CheckpointSync,
        BTreeMap::new(),
    );
    execute_migration_rollback_playbook(&mut state, &immediate_context).unwrap();
    assert!(state.recorder_writes_blocked());

    state.manual_reenable_recorder_writes();
    assert!(!state.recorder_writes_blocked());
}

// =========================================================================
// Migration v20 → v21: Session persistence tables + wa_meta → ft_meta
// =========================================================================

/// Helper: create a v20 database with wa_meta populated
fn create_v20_database() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
        .unwrap();

    // Create minimal schema at v20 (only tables needed for migration testing)
    conn.execute_batch(
        r"
            CREATE TABLE schema_version (
                version INTEGER NOT NULL,
                applied_at INTEGER NOT NULL,
                description TEXT
            );
            CREATE TABLE wa_meta (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                min_compatible_wa TEXT NOT NULL,
                created_by_wa TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE panes (
                pane_id INTEGER PRIMARY KEY,
                pane_uuid TEXT,
                domain TEXT NOT NULL DEFAULT 'local',
                window_id INTEGER,
                tab_id INTEGER,
                title TEXT,
                cwd TEXT,
                tty_name TEXT,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                observed INTEGER NOT NULL DEFAULT 1,
                ignore_reason TEXT,
                last_decision_at INTEGER
            );
            INSERT INTO schema_version (version, applied_at, description)
                VALUES (20, 1700000000000, 'test v20');
            INSERT INTO wa_meta (id, schema_version, min_compatible_wa, created_by_wa, created_at)
                VALUES (1, 20, '0.1.0', '0.1.0', 1700000000000);
            PRAGMA user_version = 20;
            ",
    )
    .unwrap();
    conn
}

#[test]
fn migrate_v20_to_v21_creates_session_tables() {
    let conn = create_v20_database();
    let plan = build_migration_plan(20, 21).unwrap();
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].migration_version, 21);

    apply_migration_plan(&conn, &plan).unwrap();

    // Verify ft_meta exists with migrated data
    let (sv, min_ft, created_ft): (i32, String, String) = conn
        .query_row(
            "SELECT schema_version, min_compatible_ft, created_by_ft FROM ft_meta WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(sv, 20); // wa_meta had schema_version=20
    assert_eq!(min_ft, "0.1.0");
    assert_eq!(created_ft, "0.1.0");

    // Verify wa_meta was dropped
    assert!(!table_exists(&conn, "wa_meta").unwrap());

    // Verify session tables exist
    assert!(table_exists(&conn, "mux_sessions").unwrap());
    assert!(table_exists(&conn, "session_checkpoints").unwrap());
    assert!(table_exists(&conn, "mux_pane_state").unwrap());

    // Verify user_version updated
    assert_eq!(get_user_version(&conn).unwrap(), 21);
}

#[test]
fn migrate_v20_to_v21_idempotent_on_v21_db() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    assert_eq!(get_user_version(&conn).unwrap(), SCHEMA_VERSION);

    // Running migration on already-v21 db is a no-op
    let plan = build_migration_plan(21, 21).unwrap();
    assert!(plan.steps.is_empty());
}

#[test]
fn migrate_v21_session_tables_foreign_keys() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Insert a session
    conn.execute(
        "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version) \
             VALUES ('sess-1', 1700000000000, '{}', '0.1.0')",
        [],
    )
    .unwrap();

    // Insert a checkpoint referencing the session
    conn.execute(
        "INSERT INTO session_checkpoints \
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes) \
             VALUES ('sess-1', 1700000001000, 'periodic', 'hash123', 2, 1024)",
        [],
    )
    .unwrap();

    let checkpoint_id: i64 = conn
        .query_row(
            "SELECT id FROM session_checkpoints WHERE session_id = 'sess-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    // Insert pane state referencing the checkpoint
    conn.execute(
        "INSERT INTO mux_pane_state \
             (checkpoint_id, pane_id, terminal_state_json) \
             VALUES (?1, 1, '{}')",
        params![checkpoint_id],
    )
    .unwrap();

    // Verify cascade delete: removing session cascades to checkpoints and pane state
    conn.execute("DELETE FROM mux_sessions WHERE session_id = 'sess-1'", [])
        .unwrap();

    let checkpoint_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(checkpoint_count, 0, "Checkpoints should cascade-delete");

    let pane_state_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!(pane_state_count, 0, "Pane state should cascade-delete");
}

#[test]
fn migrate_v21_checkpoint_type_constraint() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    conn.execute(
        "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version) \
             VALUES ('sess-1', 1700000000000, '{}', '0.1.0')",
        [],
    )
    .unwrap();

    // Valid types should succeed
    for valid_type in &["periodic", "event", "shutdown", "startup"] {
        conn.execute(
                &format!(
                    "INSERT INTO session_checkpoints \
                     (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes) \
                     VALUES ('sess-1', 1700000001000, '{valid_type}', 'hash', 1, 100)"
                ),
                [],
            )
            .unwrap();
    }

    // Invalid type should fail
    let result = conn.execute(
        "INSERT INTO session_checkpoints \
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes) \
             VALUES ('sess-1', 1700000002000, 'invalid_type', 'hash', 1, 100)",
        [],
    );
    assert!(
        result.is_err(),
        "Invalid checkpoint_type should be rejected by CHECK constraint"
    );
}

#[test]
fn migrate_v21_ft_meta_columns_renamed() {
    let conn = create_v20_database();
    let plan = build_migration_plan(20, 21).unwrap();
    apply_migration_plan(&conn, &plan).unwrap();

    // Verify old wa_meta column names don't exist
    assert!(!table_exists(&conn, "wa_meta").unwrap());

    // Verify new ft_meta columns are accessible
    let meta = load_ft_meta(&conn).unwrap().expect("ft_meta should exist");
    assert_eq!(meta.min_compatible_ft, "0.1.0");
    assert_eq!(meta.created_by_ft, "0.1.0");
    assert_eq!(meta.created_at, 1_700_000_000_000);
}

#[test]
fn migrate_v21_rollback_restores_wa_meta() {
    let conn = create_v20_database();

    // Migrate up to v21
    let up_plan = build_migration_plan(20, 21).unwrap();
    apply_migration_plan(&conn, &up_plan).unwrap();
    assert!(!table_exists(&conn, "wa_meta").unwrap());
    assert!(table_exists(&conn, "ft_meta").unwrap());

    // Roll back to v20
    let down_plan = build_migration_plan(21, 20).unwrap();
    apply_migration_plan(&conn, &down_plan).unwrap();

    // wa_meta should be restored with data from ft_meta
    assert!(table_exists(&conn, "wa_meta").unwrap());
    assert!(!table_exists(&conn, "ft_meta").unwrap());

    let (sv, min_wa, created_wa): (i32, String, String) = conn
        .query_row(
            "SELECT schema_version, min_compatible_wa, created_by_wa FROM wa_meta WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(sv, 20);
    assert_eq!(min_wa, "0.1.0");
    assert_eq!(created_wa, "0.1.0");

    // Session tables should be dropped
    assert!(!table_exists(&conn, "mux_sessions").unwrap());
    assert!(!table_exists(&conn, "session_checkpoints").unwrap());
    assert!(!table_exists(&conn, "mux_pane_state").unwrap());

    assert_eq!(get_user_version(&conn).unwrap(), 20);
}

/// Helper: create a v22 database with the legacy segment_embeddings schema.
fn create_v22_database_with_legacy_embeddings() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
        .unwrap();

    conn.execute_batch(
        r"
            CREATE TABLE schema_version (
                version INTEGER NOT NULL,
                applied_at INTEGER NOT NULL,
                description TEXT
            );
            CREATE TABLE ft_meta (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                min_compatible_ft TEXT NOT NULL,
                created_by_ft TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE panes (
                pane_id INTEGER PRIMARY KEY,
                domain TEXT NOT NULL DEFAULT 'local',
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                observed INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE output_segments (
                id INTEGER PRIMARY KEY,
                pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
                seq INTEGER NOT NULL,
                content TEXT NOT NULL,
                content_len INTEGER NOT NULL,
                captured_at INTEGER NOT NULL
            );
            -- Legacy parent table referenced by migration v22
            CREATE TABLE segments (
                id INTEGER PRIMARY KEY
            );
            CREATE TABLE segment_embeddings (
                segment_id INTEGER PRIMARY KEY REFERENCES segments(id) ON DELETE CASCADE,
                embedder_id TEXT NOT NULL,
                dimension INTEGER NOT NULL,
                vector BLOB NOT NULL,
                embedded_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
            );
            CREATE INDEX idx_segment_embeddings_embedder ON segment_embeddings(embedder_id);

            INSERT INTO schema_version (version, applied_at, description)
                VALUES (22, 1700000000000, 'test v22');
            INSERT INTO ft_meta (id, schema_version, min_compatible_ft, created_by_ft, created_at)
                VALUES (1, 22, '0.1.0', '0.1.0', 1700000000000);
            INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed)
                VALUES (1, 'local', 1700000000000, 1700000000000, 1);
            INSERT INTO output_segments (id, pane_id, seq, content, content_len, captured_at)
                VALUES (1, 1, 0, 'legacy segment', 14, 1700000000000);
            INSERT INTO segments (id) VALUES (1);
            INSERT INTO segment_embeddings (segment_id, embedder_id, dimension, vector, embedded_at)
                VALUES (1, 'hash', 8, X'0102', 1700000000);
            PRAGMA user_version = 22;
            ",
    )
    .unwrap();

    conn
}

#[test]
fn migrate_v22_to_v23_repairs_segment_embeddings_schema() {
    let conn = create_v22_database_with_legacy_embeddings();
    let plan = build_migration_plan(22, 23).unwrap();
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].migration_version, 23);

    apply_migration_plan(&conn, &plan).unwrap();

    assert_eq!(get_user_version(&conn).unwrap(), 23);
    assert!(segment_embeddings_table_is_canonical(&conn).unwrap());

    let migrated_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM segment_embeddings", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(migrated_count, 1);

    let (embedder_id, vector): (String, Vec<u8>) = conn
        .query_row(
            "SELECT embedder_id, vector FROM segment_embeddings WHERE segment_id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(embedder_id, "hash");
    assert_eq!(vector, vec![1u8, 2u8]);
}

#[test]
fn migrate_v22_to_v23_creates_embeddings_table_when_missing() {
    let conn = create_v22_database_with_legacy_embeddings();
    conn.execute_batch(
        r"
            DROP INDEX IF EXISTS idx_segment_embeddings_embedder;
            DROP TABLE IF EXISTS segment_embeddings;
            ",
    )
    .unwrap();

    let plan = build_migration_plan(22, 23).unwrap();
    apply_migration_plan(&conn, &plan).unwrap();

    assert_eq!(get_user_version(&conn).unwrap(), 23);
    assert!(table_exists(&conn, "segment_embeddings").unwrap());
    assert!(segment_embeddings_table_is_canonical(&conn).unwrap());
}
