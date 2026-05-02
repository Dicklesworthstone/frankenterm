//! ft-u6fba Phase 1b: extracted from storage.rs (mod timeline_integration_tests).
//! Sibling submodule of `storage` — `use super::*;` resolves to
//! `crate::storage::*`.

use super::*;

fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    super::run_storage_async_test(future);
}

fn make_pane(pane_id: u64, now: i64) -> PaneRecord {
    PaneRecord {
        pane_id,
        pane_uuid: Some(format!("uuid-{pane_id}")),
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: Some(format!("pane-{pane_id}")),
        cwd: Some("/tmp/test".to_string()),
        tty_name: None,
        first_seen_at: now,
        last_seen_at: now,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

fn make_event(
    pane_id: u64,
    rule_id: &str,
    event_type: &str,
    severity: &str,
    detected_at: i64,
) -> StoredEvent {
    StoredEvent {
        id: 0,
        pane_id,
        rule_id: rule_id.to_string(),
        agent_type: "claude_code".to_string(),
        event_type: event_type.to_string(),
        severity: severity.to_string(),
        confidence: 0.9,
        extracted: None,
        matched_text: None,
        segment_id: None,
        detected_at,
        dedupe_key: None,
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    }
}

#[test]
fn timeline_empty_db_returns_empty() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let query = TimelineQuery::new();
        let timeline = handle.get_timeline(query).await.unwrap();

        assert!(timeline.events.is_empty());
        assert!(timeline.correlations.is_empty());
        assert_eq!(timeline.total_count, 0);
        assert!(!timeline.has_more);

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_single_event_no_correlations() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 1);
        assert!(timeline.correlations.is_empty());
        assert_eq!(timeline.total_count, 1);

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_temporal_correlation_across_panes() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        // Two events within 10s across different panes
        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(2, "rule_b", "warning", "warning", now + 3000))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 2);
        let temporal = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Temporal)
            .count();
        assert!(temporal > 0, "Should detect temporal correlation");

        // Events should have correlation refs attached
        let event_with_refs = timeline
            .events
            .iter()
            .filter(|e| !e.correlations.is_empty())
            .count();
        assert!(event_with_refs > 0, "Events should have correlation refs");

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_failover_correlation_integration() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        // Usage limit in pane 1, session start in pane 2 within 5 minutes
        handle
            .record_event(make_event(
                1,
                "usage_limit",
                "usage.reached",
                "warning",
                now,
            ))
            .await
            .unwrap();
        handle
            .record_event(make_event(
                2,
                "session_start",
                "session.start",
                "info",
                now + 120_000,
            ))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        let failover = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Failover)
            .count();
        assert_eq!(failover, 1, "Should detect failover correlation");

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_pagination_offset_limit() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();

        // Insert 10 events
        for i in 0..10 {
            handle
                .record_event(make_event(
                    1,
                    &format!("rule_{i}"),
                    "info",
                    "info",
                    now + i * 1000,
                ))
                .await
                .unwrap();
        }

        // Page 1: first 3
        let query = TimelineQuery {
            limit: 3,
            offset: 0,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let page1 = handle.get_timeline(query).await.unwrap();
        assert_eq!(page1.events.len(), 3);
        assert_eq!(page1.total_count, 10);
        assert!(page1.has_more);

        // Page 2: next 3
        let query = TimelineQuery {
            limit: 3,
            offset: 3,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let page2 = handle.get_timeline(query).await.unwrap();
        assert_eq!(page2.events.len(), 3);
        assert!(page2.has_more);

        // Page 4: last 1
        let query = TimelineQuery {
            limit: 3,
            offset: 9,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let page4 = handle.get_timeline(query).await.unwrap();
        assert_eq!(page4.events.len(), 1);
        assert!(!page4.has_more);

        // Beyond range
        let query = TimelineQuery {
            limit: 3,
            offset: 15,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let beyond = handle.get_timeline(query).await.unwrap();
        assert!(beyond.events.is_empty());
        assert!(!beyond.has_more);

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_filter_by_severity() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();

        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(1, "rule_b", "warning", "warning", now + 1000))
            .await
            .unwrap();
        handle
            .record_event(make_event(1, "rule_c", "info", "info", now + 2000))
            .await
            .unwrap();

        let query = TimelineQuery {
            severities: Some(vec!["error".to_string()]),
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        assert_eq!(timeline.events.len(), 1);
        assert_eq!(timeline.events[0].severity, "error");

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_filter_by_pane_id() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(2, "rule_b", "error", "error", now + 1000))
            .await
            .unwrap();

        let query = TimelineQuery {
            pane_ids: Some(vec![1]),
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        assert_eq!(timeline.events.len(), 1);
        assert_eq!(timeline.events[0].pane_info.pane_id, 1);

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_filter_by_time_range() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();

        for i in 0..5 {
            handle
                .record_event(make_event(
                    1,
                    &format!("rule_{i}"),
                    "info",
                    "info",
                    now + i * 60_000,
                ))
                .await
                .unwrap();
        }

        // Only events in first 2 minutes
        let query = TimelineQuery {
            start: Some(now),
            end: Some(now + 120_000),
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        assert_eq!(timeline.events.len(), 3); // t=0, t=60s, t=120s

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_unhandled_only_filter() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();

        // Insert event then mark it as handled via the proper API
        let event_id = handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .mark_event_handled(event_id, Some("wf-1".to_string()), "handled")
            .await
            .unwrap();

        handle
            .record_event(make_event(1, "rule_b", "warning", "warning", now + 5000))
            .await
            .unwrap();

        let query = TimelineQuery {
            unhandled_only: true,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        assert_eq!(timeline.events.len(), 1);
        assert!(timeline.events[0].handled.is_none());

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_events_same_timestamp_handled_gracefully() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        // Three events at the exact same timestamp
        for i in 0..3 {
            handle
                .record_event(make_event(
                    (i % 2) as u64 + 1,
                    &format!("rule_{i}"),
                    "error",
                    "error",
                    now,
                ))
                .await
                .unwrap();
        }

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 3);
        // Should detect temporal correlation (same timestamp, different panes)
        let temporal = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Temporal)
            .count();
        assert!(
            temporal > 0,
            "Same-timestamp cross-pane events should correlate"
        );

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_correlation_refs_attached_to_events() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        // Two events that should correlate temporally
        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(2, "rule_b", "warning", "warning", now + 2000))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        // At least one event should have correlation refs
        let has_refs = timeline.events.iter().any(|e| !e.correlations.is_empty());
        assert!(
            has_refs,
            "Correlated events should have CorrelationRef attached"
        );

        // Verify ref IDs match top-level correlation IDs
        for event in &timeline.events {
            for cref in &event.correlations {
                assert!(
                    timeline.correlations.iter().any(|c| c.id == cref.id),
                    "Event correlation ref ID should match a top-level correlation"
                );
            }
        }

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_correlations_disabled_returns_empty() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        // Events that would normally correlate
        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(2, "rule_b", "error", "error", now + 1000))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 2);
        assert!(
            timeline.correlations.is_empty(),
            "Correlations should be empty when disabled"
        );

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_query_performance_many_events() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        // Create 5 panes
        for p in 1..=5 {
            handle.upsert_pane(make_pane(p, now)).await.unwrap();
        }

        // Insert 200 events across panes
        for i in 0..200 {
            let pane = (i % 5) as u64 + 1;
            handle
                .record_event(make_event(
                    pane,
                    &format!("rule_{}", i % 10),
                    "detection",
                    if i % 3 == 0 { "error" } else { "warning" },
                    now + i * 500,
                ))
                .await
                .unwrap();
        }

        // Time the query
        let start = std::time::Instant::now();
        let query = TimelineQuery {
            include_correlations: true,
            limit: 100,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(timeline.events.len(), 100);
        assert_eq!(timeline.total_count, 200);
        assert!(timeline.has_more);
        assert!(
            !timeline.correlations.is_empty(),
            "Should find correlations among 200 events"
        );
        // Performance budget: query should complete in <500ms (generous for CI)
        assert!(
            elapsed.as_millis() < 500,
            "Timeline query took {}ms, expected <500ms",
            elapsed.as_millis()
        );

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_workflow_group_integration() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        // Two events handled by same workflow (must use mark_event_handled API)
        let eid1 = handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .mark_event_handled(eid1, Some("wf-test-1".to_string()), "handled")
            .await
            .unwrap();

        let eid2 = handle
            .record_event(make_event(2, "rule_b", "error", "error", now + 5000))
            .await
            .unwrap();
        handle
            .mark_event_handled(eid2, Some("wf-test-1".to_string()), "handled")
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        let workflow = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::WorkflowGroup)
            .collect::<Vec<_>>();
        assert_eq!(workflow.len(), 1, "Should detect workflow group");
        assert_eq!(workflow[0].event_ids.len(), 2);
        assert!(
            (workflow[0].confidence - 0.95).abs() < 0.01,
            "Workflow confidence should be 0.95"
        );

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_serde_roundtrip() {
    run_async_test(async {
        let timeline = Timeline {
            start: 1000,
            end: 5000,
            events: vec![TimelineEvent {
                id: 1,
                timestamp: 1000,
                pane_info: PaneInfo {
                    pane_id: 1,
                    pane_uuid: Some("uuid-1".to_string()),
                    agent_type: Some("claude_code".to_string()),
                    domain: "local".to_string(),
                    cwd: Some("/tmp".to_string()),
                    title: Some("test".to_string()),
                },
                rule_id: "rule_a".to_string(),
                event_type: "error".to_string(),
                severity: "error".to_string(),
                confidence: 0.9,
                handled: None,
                correlations: vec![CorrelationRef {
                    id: "corr-1".to_string(),
                    correlation_type: CorrelationType::Temporal,
                }],
                summary: Some("Test event".to_string()),
            }],
            correlations: vec![Correlation {
                id: "corr-1".to_string(),
                event_ids: vec![1, 2],
                correlation_type: CorrelationType::Temporal,
                confidence: 0.6,
                description: "Test correlation".to_string(),
            }],
            total_count: 1,
            has_more: false,
        };

        let json = serde_json::to_string(&timeline).unwrap();
        let deserialized: Timeline = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.events.len(), 1);
        assert_eq!(deserialized.correlations.len(), 1);
        assert_eq!(deserialized.total_count, 1);
        assert!(!deserialized.has_more);
        assert_eq!(deserialized.events[0].correlations.len(), 1);
        assert_eq!(
            deserialized.correlations[0].correlation_type,
            CorrelationType::Temporal
        );
    });
}

#[test]
fn timeline_dedupe_group_integration() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();
        handle.upsert_pane(make_pane(3, now)).await.unwrap();

        // Same rule firing across 3 panes within 30s
        handle
            .record_event(make_event(
                1,
                "claude_code.usage.reached",
                "usage",
                "warning",
                now,
            ))
            .await
            .unwrap();
        handle
            .record_event(make_event(
                2,
                "claude_code.usage.reached",
                "usage",
                "warning",
                now + 10_000,
            ))
            .await
            .unwrap();
        handle
            .record_event(make_event(
                3,
                "claude_code.usage.reached",
                "usage",
                "warning",
                now + 20_000,
            ))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        let dedupe = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::DedupeGroup)
            .collect::<Vec<_>>();
        assert_eq!(dedupe.len(), 1, "Should detect dedupe group across 3 panes");
        assert_eq!(dedupe[0].event_ids.len(), 3);

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_events_ordered_chronologically() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();

        // Insert events out of order
        handle
            .record_event(make_event(1, "rule_c", "info", "info", now + 5000))
            .await
            .unwrap();
        handle
            .record_event(make_event(1, "rule_a", "info", "info", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(1, "rule_b", "info", "info", now + 2000))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 3);
        assert!(
            timeline.events[0].timestamp <= timeline.events[1].timestamp
                && timeline.events[1].timestamp <= timeline.events[2].timestamp,
            "Events should be in chronological order"
        );

        handle.shutdown().await.unwrap();
    });
}
