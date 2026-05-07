//! ft-u6fba Phase 1b: extracted from storage.rs (mod timeline_tests).
//! Sibling submodule of `storage` — `use super::*;` resolves to
//! `crate::storage::*`.

use super::*;

/// Helper to create a pane
fn insert_test_pane(conn: &Connection, pane_id: u64, domain: &str) {
    let now = now_ms();
    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed)
             VALUES (?1, ?2, ?3, ?4, 1)",
        params![pane_id as i64, domain, now, now],
    )
    .unwrap();
}

/// Helper to create an event
fn insert_test_event(
    conn: &Connection,
    pane_id: u64,
    rule_id: &str,
    event_type: &str,
    severity: &str,
    detected_at: i64,
) -> i64 {
    conn.execute(
        "INSERT INTO events (pane_id, rule_id, agent_type, event_type, severity,
             confidence, detected_at)
             VALUES (?1, ?2, 'claude_code', ?3, ?4, 0.9, ?5)",
        params![pane_id as i64, rule_id, event_type, severity, detected_at],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn query_test_timeline(conn: &mut Connection, query: &TimelineQuery) -> Timeline {
    with_writer_backend(conn, |backend| query_timeline_backend(backend, query)).unwrap()
}

#[test]
fn correlation_type_display_batch2() {
    assert_eq!(CorrelationType::Failover.to_string(), "failover");
    assert_eq!(CorrelationType::Temporal.to_string(), "temporal");
    assert_eq!(CorrelationType::WorkflowGroup.to_string(), "workflow_group");
}

#[test]
fn timeline_query_builder() {
    let query = TimelineQuery::new()
        .with_range(1000, 2000)
        .with_panes(vec![1, 2])
        .with_severities(vec!["critical".to_string()])
        .unhandled_only()
        .with_pagination(50, 10);

    assert_eq!(query.start, Some(1000));
    assert_eq!(query.end, Some(2000));
    assert_eq!(query.pane_ids, Some(vec![1, 2]));
    assert_eq!(query.severities, Some(vec!["critical".to_string()]));
    assert!(query.unhandled_only);
    assert_eq!(query.limit, 50);
    assert_eq!(query.offset, 10);
}

#[test]
fn empty_timeline_query() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let query = TimelineQuery::new();
    let timeline = query_test_timeline(&mut conn, &query);

    assert!(timeline.events.is_empty());
    assert!(timeline.correlations.is_empty());
    assert_eq!(timeline.total_count, 0);
    assert!(!timeline.has_more);
}

#[test]
fn timeline_with_events() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Create panes
    insert_test_pane(&conn, 1, "local");
    insert_test_pane(&conn, 2, "ssh");

    // Create events
    let now = now_ms();
    insert_test_event(
        &conn,
        1,
        "codex.usage_limit",
        "usage_limit",
        "warning",
        now - 2000,
    );
    insert_test_event(
        &conn,
        2,
        "codex.compaction",
        "compaction",
        "info",
        now - 1000,
    );
    insert_test_event(&conn, 1, "codex.error", "error", "critical", now);

    let query = TimelineQuery::new();
    let timeline = query_test_timeline(&mut conn, &query);

    assert_eq!(timeline.events.len(), 3);
    assert_eq!(timeline.total_count, 3);
    assert!(!timeline.has_more);

    // Events should be in chronological order
    assert!(timeline.events[0].timestamp <= timeline.events[1].timestamp);
    assert!(timeline.events[1].timestamp <= timeline.events[2].timestamp);

    // Pane info should be populated
    assert_eq!(timeline.events[0].pane_info.domain, "local");
    assert_eq!(timeline.events[1].pane_info.domain, "ssh");
}

#[test]
fn timeline_with_pane_filter() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1, "local");
    insert_test_pane(&conn, 2, "ssh");

    let now = now_ms();
    insert_test_event(&conn, 1, "rule1", "event1", "info", now - 1000);
    insert_test_event(&conn, 2, "rule2", "event2", "info", now);
    insert_test_event(&conn, 1, "rule3", "event3", "info", now + 1000);

    let query = TimelineQuery::new().with_panes(vec![1]);
    let timeline = query_test_timeline(&mut conn, &query);

    assert_eq!(timeline.events.len(), 2);
    assert!(timeline.events.iter().all(|e| e.pane_info.pane_id == 1));
}

#[test]
fn timeline_with_severity_filter() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1, "local");

    let now = now_ms();
    insert_test_event(&conn, 1, "rule1", "event1", "info", now - 2000);
    insert_test_event(&conn, 1, "rule2", "event2", "warning", now - 1000);
    insert_test_event(&conn, 1, "rule3", "event3", "critical", now);

    let query = TimelineQuery::new().with_severities(vec!["critical".to_string()]);
    let timeline = query_test_timeline(&mut conn, &query);

    assert_eq!(timeline.events.len(), 1);
    assert_eq!(timeline.events[0].severity, "critical");
}

#[test]
fn timeline_pagination() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1, "local");

    let now = now_ms();
    for i in 0..10 {
        insert_test_event(
            &conn,
            1,
            &format!("rule{i}"),
            "event",
            "info",
            now + i * 1000,
        );
    }

    // First page
    let query = TimelineQuery::new().with_pagination(3, 0);
    let timeline = query_test_timeline(&mut conn, &query);

    assert_eq!(timeline.events.len(), 3);
    assert_eq!(timeline.total_count, 10);
    assert!(timeline.has_more);

    // Second page
    let query = TimelineQuery::new().with_pagination(3, 3);
    let timeline = query_test_timeline(&mut conn, &query);

    assert_eq!(timeline.events.len(), 3);
    assert!(timeline.has_more);

    // Last page
    let query = TimelineQuery::new().with_pagination(3, 9);
    let timeline = query_test_timeline(&mut conn, &query);

    assert_eq!(timeline.events.len(), 1);
    assert!(!timeline.has_more);
}

#[test]
fn detect_temporal_correlations() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1, "local");
    insert_test_pane(&conn, 2, "ssh");

    let now = now_ms();
    // Events within temporal window across different panes
    insert_test_event(&conn, 1, "rule1", "event1", "info", now);
    insert_test_event(&conn, 2, "rule2", "event2", "info", now + 2000); // 2s later

    let query = TimelineQuery::new();
    let timeline = query_test_timeline(&mut conn, &query);

    // Should detect temporal correlation
    assert!(!timeline.correlations.is_empty());
    let temporal = timeline
        .correlations
        .iter()
        .find(|c| c.correlation_type == CorrelationType::Temporal);
    assert!(temporal.is_some());
}

#[test]
fn detect_failover_correlations() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1, "local");
    insert_test_pane(&conn, 2, "ssh");

    let now = now_ms();
    insert_test_event(
        &conn,
        1,
        "codex.usage.reached",
        "usage.reached",
        "critical",
        now,
    );
    insert_test_event(
        &conn,
        2,
        "codex.session.start",
        "session.start",
        "info",
        now + 10_000,
    );

    let timeline = query_test_timeline(&mut conn, &TimelineQuery::new());
    let failover = timeline
        .correlations
        .iter()
        .find(|c| c.correlation_type == CorrelationType::Failover);
    assert!(failover.is_some());
}

#[test]
fn detect_cascade_correlations() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1, "local");
    insert_test_pane(&conn, 2, "ssh");

    let now = now_ms();
    insert_test_event(
        &conn,
        1,
        "codex.error.timeout",
        "error.timeout",
        "critical",
        now,
    );
    insert_test_event(
        &conn,
        2,
        "codex.session.resume_hint",
        "session.resume_hint",
        "info",
        now + 5_000,
    );

    let timeline = query_test_timeline(&mut conn, &TimelineQuery::new());
    let cascade = timeline
        .correlations
        .iter()
        .find(|c| c.correlation_type == CorrelationType::Cascade);
    assert!(cascade.is_some());
}

#[test]
fn unhandled_only_filter() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    insert_test_pane(&conn, 1, "local");

    let now = now_ms();
    let event1_id = insert_test_event(&conn, 1, "rule1", "event1", "info", now - 1000);
    insert_test_event(&conn, 1, "rule2", "event2", "info", now);

    // Mark first event as handled
    conn.execute(
        "UPDATE events SET handled_at = ?1, handled_status = 'completed' WHERE id = ?2",
        params![now, event1_id],
    )
    .unwrap();

    let query = TimelineQuery::new().unhandled_only();
    let timeline = query_test_timeline(&mut conn, &query);

    assert_eq!(timeline.events.len(), 1);
    assert!(timeline.events[0].handled.is_none());
}
