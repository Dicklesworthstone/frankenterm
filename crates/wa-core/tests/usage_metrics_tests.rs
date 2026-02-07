//! Tests for the usage_metrics analytics data model (wa-985.1).

use tempfile::TempDir;
use wa_core::storage::{
    DailyMetricSummary, MetricQuery, MetricType, StorageHandle, UsageMetricRecord,
};

fn temp_db() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("test.db").to_string_lossy().to_string();
    (dir, path)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn make_metric(
    metric_type: MetricType,
    agent_type: Option<&str>,
    tokens: Option<i64>,
    amount: Option<f64>,
    timestamp: i64,
) -> UsageMetricRecord {
    UsageMetricRecord {
        id: 0,
        timestamp,
        metric_type,
        pane_id: None,
        agent_type: agent_type.map(String::from),
        account_id: None,
        workflow_id: None,
        count: Some(1),
        amount,
        tokens,
        metadata: None,
        created_at: 0,
    }
}

// =========================================================================
// MetricType parsing + display
// =========================================================================

#[test]
fn metric_type_roundtrip() {
    let types = [
        MetricType::TokenUsage,
        MetricType::ApiCost,
        MetricType::ApiCall,
        MetricType::RateLimitHit,
        MetricType::WorkflowCost,
        MetricType::SessionDuration,
    ];

    for mt in &types {
        let s = mt.as_str();
        let parsed: MetricType = s.parse().expect("parse should succeed");
        assert_eq!(*mt, parsed, "round-trip failed for {s}");
        assert_eq!(mt.to_string(), s, "Display should match as_str");
    }
}

#[test]
fn metric_type_parse_unknown_returns_error() {
    let result: Result<MetricType, _> = "unknown_metric".parse();
    assert!(result.is_err());
}

#[test]
fn metric_type_serde_roundtrip() {
    let mt = MetricType::TokenUsage;
    let json = serde_json::to_string(&mt).expect("serialize");
    assert_eq!(json, "\"token_usage\"");
    let parsed: MetricType = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed, mt);
}

// =========================================================================
// Record + query basic operations
// =========================================================================

#[test]
fn record_and_query_single_metric() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        let record = make_metric(
            MetricType::TokenUsage,
            Some("claude_code"),
            Some(1500),
            None,
            ts,
        );
        let id = storage.record_usage_metric(record).await.expect("record");
        assert!(id > 0);

        let results = storage
            .query_usage_metrics(MetricQuery {
                metric_type: Some(MetricType::TokenUsage),
                ..Default::default()
            })
            .await
            .expect("query");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].metric_type, MetricType::TokenUsage);
        assert_eq!(results[0].tokens, Some(1500));
        assert_eq!(results[0].agent_type.as_deref(), Some("claude_code"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn query_with_multiple_filters() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        // Insert varied metrics
        storage
            .record_usage_metric(make_metric(
                MetricType::TokenUsage,
                Some("claude_code"),
                Some(1000),
                None,
                ts - 10_000,
            ))
            .await
            .unwrap();
        storage
            .record_usage_metric(make_metric(
                MetricType::ApiCost,
                Some("codex"),
                None,
                Some(0.05),
                ts - 5_000,
            ))
            .await
            .unwrap();
        storage
            .record_usage_metric(make_metric(
                MetricType::TokenUsage,
                Some("codex"),
                Some(2000),
                None,
                ts,
            ))
            .await
            .unwrap();

        // Filter by metric type
        let token_metrics = storage
            .query_usage_metrics(MetricQuery {
                metric_type: Some(MetricType::TokenUsage),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(token_metrics.len(), 2);

        // Filter by agent
        let codex_metrics = storage
            .query_usage_metrics(MetricQuery {
                agent_type: Some("codex".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(codex_metrics.len(), 2);

        // Filter by both
        let codex_tokens = storage
            .query_usage_metrics(MetricQuery {
                metric_type: Some(MetricType::TokenUsage),
                agent_type: Some("codex".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(codex_tokens.len(), 1);
        assert_eq!(codex_tokens[0].tokens, Some(2000));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn query_with_time_range() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let base_ts = 1_000_000_000_000i64; // fixed base
        for i in 0..5 {
            storage
                .record_usage_metric(make_metric(
                    MetricType::ApiCall,
                    None,
                    None,
                    None,
                    base_ts + i * 1_000,
                ))
                .await
                .unwrap();
        }

        // Query only middle window
        let results = storage
            .query_usage_metrics(MetricQuery {
                since: Some(base_ts + 1_000),
                until: Some(base_ts + 4_000),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 3); // timestamps 1000, 2000, 3000

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn query_with_limit() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        for i in 0..10 {
            storage
                .record_usage_metric(make_metric(
                    MetricType::ApiCall,
                    None,
                    None,
                    None,
                    ts + i * 100,
                ))
                .await
                .unwrap();
        }

        let results = storage
            .query_usage_metrics(MetricQuery {
                limit: Some(3),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 3);
        // Results should be ordered by timestamp DESC
        assert!(results[0].timestamp >= results[1].timestamp);

        storage.shutdown().await.expect("shutdown");
    });
}

// =========================================================================
// Purge (retention)
// =========================================================================

#[test]
fn purge_old_metrics() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let old_ts = 1_000_000_000_000i64;
        let new_ts = 2_000_000_000_000i64;

        // Insert old and new metrics
        for i in 0..5 {
            storage
                .record_usage_metric(make_metric(
                    MetricType::TokenUsage,
                    None,
                    Some(100),
                    None,
                    old_ts + i * 100,
                ))
                .await
                .unwrap();
        }
        for i in 0..3 {
            storage
                .record_usage_metric(make_metric(
                    MetricType::TokenUsage,
                    None,
                    Some(200),
                    None,
                    new_ts + i * 100,
                ))
                .await
                .unwrap();
        }

        // Purge old
        let purged = storage.purge_usage_metrics(new_ts).await.expect("purge");
        assert_eq!(purged, 5);

        // Only new remain
        let remaining = storage
            .query_usage_metrics(MetricQuery::default())
            .await
            .unwrap();
        assert_eq!(remaining.len(), 3);

        storage.shutdown().await.expect("shutdown");
    });
}

// =========================================================================
// Daily aggregation
// =========================================================================

#[test]
fn aggregate_daily_metrics() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        // Day 1: 2 entries for claude_code
        let day1 = 1_700_000_000_000i64; // some fixed epoch ms
        storage
            .record_usage_metric(make_metric(
                MetricType::TokenUsage,
                Some("claude_code"),
                Some(1000),
                Some(0.01),
                day1,
            ))
            .await
            .unwrap();
        storage
            .record_usage_metric(make_metric(
                MetricType::TokenUsage,
                Some("claude_code"),
                Some(2000),
                Some(0.02),
                day1 + 60_000,
            ))
            .await
            .unwrap();

        // Day 1: 1 entry for codex
        storage
            .record_usage_metric(make_metric(
                MetricType::ApiCost,
                Some("codex"),
                Some(500),
                Some(0.05),
                day1 + 120_000,
            ))
            .await
            .unwrap();

        // Day 2: 1 entry
        let day2 = day1 + 86_400_000;
        storage
            .record_usage_metric(make_metric(
                MetricType::TokenUsage,
                Some("claude_code"),
                Some(3000),
                Some(0.03),
                day2,
            ))
            .await
            .unwrap();

        let summaries = storage
            .aggregate_daily_metrics(day1 - 1)
            .await
            .expect("aggregate");

        // Should have at least 3 rows (day1+claude_code, day1+codex, day2+claude_code)
        assert!(summaries.len() >= 3, "got {} summaries", summaries.len());

        // Find day1 + claude_code
        let day1_claude: Vec<&DailyMetricSummary> = summaries
            .iter()
            .filter(|s| {
                s.agent_type.as_deref() == Some("claude_code")
                    && s.day_ts == (day1 / 86_400_000) * 86_400_000
            })
            .collect();
        assert_eq!(day1_claude.len(), 1);
        assert_eq!(day1_claude[0].total_tokens, 3000);
        assert!((day1_claude[0].total_cost - 0.03).abs() < 0.001);
        assert_eq!(day1_claude[0].event_count, 2);

        storage.shutdown().await.expect("shutdown");
    });
}

// =========================================================================
// Per-agent aggregation
// =========================================================================

#[test]
fn aggregate_by_agent() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();

        // claude_code: 3 entries, 6000 tokens total, $0.06 total
        for i in 0..3 {
            storage
                .record_usage_metric(make_metric(
                    MetricType::TokenUsage,
                    Some("claude_code"),
                    Some(2000),
                    Some(0.02),
                    ts + i * 1000,
                ))
                .await
                .unwrap();
        }

        // codex: 2 entries, 1000 tokens total, $0.10 total
        for i in 0..2 {
            storage
                .record_usage_metric(make_metric(
                    MetricType::ApiCost,
                    Some("codex"),
                    Some(500),
                    Some(0.05),
                    ts + i * 1000,
                ))
                .await
                .unwrap();
        }

        let breakdowns = storage.aggregate_by_agent(ts - 1).await.expect("aggregate");

        assert_eq!(breakdowns.len(), 2);

        // Sorted by total_cost DESC, so codex ($0.10) comes first
        let codex = breakdowns.iter().find(|b| b.agent_type == "codex").unwrap();
        assert_eq!(codex.total_tokens, 1000);
        assert!((codex.total_cost - 0.10).abs() < 0.001);
        assert!((codex.avg_tokens_per_event - 500.0).abs() < 0.1);

        let claude = breakdowns
            .iter()
            .find(|b| b.agent_type == "claude_code")
            .unwrap();
        assert_eq!(claude.total_tokens, 6000);
        assert!((claude.total_cost - 0.06).abs() < 0.001);
        assert!((claude.avg_tokens_per_event - 2000.0).abs() < 0.1);

        storage.shutdown().await.expect("shutdown");
    });
}

// =========================================================================
// Edge cases
// =========================================================================

#[test]
fn empty_query_returns_empty() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let results = storage
            .query_usage_metrics(MetricQuery::default())
            .await
            .unwrap();
        assert!(results.is_empty());

        let daily = storage.aggregate_daily_metrics(0).await.unwrap();
        assert!(daily.is_empty());

        let by_agent = storage.aggregate_by_agent(0).await.unwrap();
        assert!(by_agent.is_empty());

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn metric_with_pane_id_and_workflow() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        let record = UsageMetricRecord {
            id: 0,
            timestamp: ts,
            metric_type: MetricType::WorkflowCost,
            pane_id: Some(42),
            agent_type: Some("claude_code".to_string()),
            account_id: Some("acc-123".to_string()),
            workflow_id: Some("wf-456".to_string()),
            count: Some(1),
            amount: Some(0.15),
            tokens: Some(5000),
            metadata: Some(r#"{"model":"opus"}"#.to_string()),
            created_at: 0,
        };

        let id = storage.record_usage_metric(record).await.expect("record");
        assert!(id > 0);

        let results = storage
            .query_usage_metrics(MetricQuery {
                metric_type: Some(MetricType::WorkflowCost),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.pane_id, Some(42));
        assert_eq!(r.workflow_id.as_deref(), Some("wf-456"));
        assert_eq!(r.account_id.as_deref(), Some("acc-123"));
        assert_eq!(r.metadata.as_deref(), Some(r#"{"model":"opus"}"#));
        assert_eq!(r.amount, Some(0.15));
        assert!(r.created_at > 0, "created_at should be auto-set");

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn purge_with_no_matching_records() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        storage
            .record_usage_metric(make_metric(MetricType::ApiCall, None, None, None, ts))
            .await
            .unwrap();

        // Purge with cutoff before any records
        let purged = storage.purge_usage_metrics(ts - 1000).await.unwrap();
        assert_eq!(purged, 0);

        // Record still exists
        let results = storage
            .query_usage_metrics(MetricQuery::default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn query_by_account_id() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        let mut r1 = make_metric(MetricType::ApiCall, None, None, None, ts);
        r1.account_id = Some("acc-A".to_string());
        let mut r2 = make_metric(MetricType::ApiCall, None, None, None, ts + 100);
        r2.account_id = Some("acc-B".to_string());

        storage.record_usage_metric(r1).await.unwrap();
        storage.record_usage_metric(r2).await.unwrap();

        let results = storage
            .query_usage_metrics(MetricQuery {
                account_id: Some("acc-A".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].account_id.as_deref(), Some("acc-A"));

        storage.shutdown().await.expect("shutdown");
    });
}

// =========================================================================
// Migration test
// =========================================================================

#[test]
fn schema_migration_creates_usage_metrics_table() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        // If we can record a metric, the table exists and was migrated
        let ts = now_ms();
        let id = storage
            .record_usage_metric(make_metric(MetricType::ApiCall, None, None, None, ts))
            .await
            .expect("record on fresh DB");
        assert!(id > 0);

        storage.shutdown().await.expect("shutdown");
    });
}

// =========================================================================
// wa-985.5: Comprehensive analytics tests
// =========================================================================

#[test]
fn batch_record_then_aggregate_daily() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        // Insert a batch of metrics across 2 days
        let day1 = 1_700_000_000_000i64;
        let day2 = day1 + 86_400_000;
        let batch = vec![
            make_metric(
                MetricType::TokenUsage,
                Some("codex"),
                Some(1000),
                Some(0.10),
                day1,
            ),
            make_metric(
                MetricType::TokenUsage,
                Some("codex"),
                Some(2000),
                Some(0.20),
                day1 + 60_000,
            ),
            make_metric(
                MetricType::ApiCost,
                Some("claude_code"),
                None,
                Some(0.50),
                day1 + 120_000,
            ),
            make_metric(
                MetricType::TokenUsage,
                Some("codex"),
                Some(3000),
                Some(0.30),
                day2,
            ),
            make_metric(
                MetricType::ApiCost,
                Some("claude_code"),
                None,
                Some(0.75),
                day2 + 60_000,
            ),
        ];

        let inserted = storage.record_usage_metrics_batch(batch).await.unwrap();
        assert_eq!(inserted, 5);

        // Aggregate daily should show both days
        let daily = storage.aggregate_daily_metrics(day1 - 1).await.unwrap();
        assert!(
            daily.len() >= 2,
            "should have summaries for 2+ day/agent combos"
        );

        // Aggregate by agent should show both agents
        let by_agent = storage.aggregate_by_agent(day1 - 1).await.unwrap();
        assert_eq!(by_agent.len(), 2);
        let codex = by_agent.iter().find(|b| b.agent_type == "codex").unwrap();
        assert_eq!(codex.total_tokens, 6000); // 1000 + 2000 + 3000

        let claude = by_agent
            .iter()
            .find(|b| b.agent_type == "claude_code")
            .unwrap();
        assert!((claude.total_cost - 1.25).abs() < 0.01); // 0.50 + 0.75

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn batch_record_empty_is_noop() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let inserted = storage.record_usage_metrics_batch(vec![]).await.unwrap();
        assert_eq!(inserted, 0);

        let results = storage
            .query_usage_metrics(MetricQuery::default())
            .await
            .unwrap();
        assert!(results.is_empty());

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn all_metric_types_record_and_query() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        let types = [
            MetricType::TokenUsage,
            MetricType::ApiCost,
            MetricType::ApiCall,
            MetricType::RateLimitHit,
            MetricType::WorkflowCost,
            MetricType::SessionDuration,
        ];

        for (i, mt) in types.iter().enumerate() {
            let mut record = make_metric(*mt, None, None, None, ts + i as i64 * 100);
            record.tokens = Some((i + 1) as i64 * 100);
            storage.record_usage_metric(record).await.unwrap();
        }

        // Query each type individually
        for mt in &types {
            let results = storage
                .query_usage_metrics(MetricQuery {
                    metric_type: Some(*mt),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(results.len(), 1, "should find 1 record for {}", mt.as_str());
            assert_eq!(results[0].metric_type, *mt);
        }

        // Query all
        let all = storage
            .query_usage_metrics(MetricQuery::default())
            .await
            .unwrap();
        assert_eq!(all.len(), 6);

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn aggregation_with_null_agent_type() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        // Records with no agent_type
        storage
            .record_usage_metric(make_metric(MetricType::ApiCall, None, None, None, ts))
            .await
            .unwrap();
        storage
            .record_usage_metric(make_metric(MetricType::ApiCall, None, None, None, ts + 100))
            .await
            .unwrap();
        // Record with agent_type
        storage
            .record_usage_metric(make_metric(
                MetricType::TokenUsage,
                Some("codex"),
                Some(500),
                None,
                ts + 200,
            ))
            .await
            .unwrap();

        let daily = storage.aggregate_daily_metrics(ts - 1).await.unwrap();
        // Should have entries for NULL agent and codex
        assert!(daily.len() >= 2);

        let by_agent = storage.aggregate_by_agent(ts - 1).await.unwrap();
        // at least codex should appear (NULL agent may or may not depending on SQL)
        assert!(!by_agent.is_empty());

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn aggregation_with_null_tokens_and_amount() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        // Record with only count, no tokens or amount
        let record = UsageMetricRecord {
            id: 0,
            timestamp: ts,
            metric_type: MetricType::RateLimitHit,
            pane_id: Some(1),
            agent_type: Some("codex".to_string()),
            account_id: None,
            workflow_id: None,
            count: Some(1),
            amount: None,
            tokens: None,
            metadata: None,
            created_at: 0,
        };
        storage.record_usage_metric(record).await.unwrap();

        let daily = storage.aggregate_daily_metrics(ts - 1).await.unwrap();
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].total_tokens, 0); // NULL coalesces to 0
        assert!((daily[0].total_cost - 0.0).abs() < 0.001);
        assert_eq!(daily[0].event_count, 1);

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn metric_query_default_is_unfiltered() {
    let query = MetricQuery::default();
    assert!(query.metric_type.is_none());
    assert!(query.agent_type.is_none());
    assert!(query.account_id.is_none());
    assert!(query.since.is_none());
    assert!(query.until.is_none());
    assert!(query.limit.is_none());
}

#[test]
fn metric_record_serde_roundtrip() {
    let record = UsageMetricRecord {
        id: 42,
        timestamp: 1_700_000_000_000,
        metric_type: MetricType::TokenUsage,
        pane_id: Some(3),
        agent_type: Some("codex".to_string()),
        account_id: Some("acc-1".to_string()),
        workflow_id: Some("wf-1".to_string()),
        count: Some(10),
        amount: Some(1.23),
        tokens: Some(5000),
        metadata: Some(r#"{"model":"gpt4"}"#.to_string()),
        created_at: 1_700_000_001_000,
    };

    let json = serde_json::to_string(&record).unwrap();
    let parsed: UsageMetricRecord = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.id, 42);
    assert_eq!(parsed.metric_type, MetricType::TokenUsage);
    assert_eq!(parsed.pane_id, Some(3));
    assert_eq!(parsed.agent_type.as_deref(), Some("codex"));
    assert_eq!(parsed.tokens, Some(5000));
    assert!((parsed.amount.unwrap() - 1.23).abs() < 0.001);
}

#[test]
fn daily_summary_serde_roundtrip() {
    let summary = DailyMetricSummary {
        day_ts: 1_700_000_000_000,
        agent_type: Some("claude_code".to_string()),
        total_tokens: 50_000,
        total_cost: 5.50,
        event_count: 25,
    };

    let json = serde_json::to_string(&summary).unwrap();
    let parsed: DailyMetricSummary = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.total_tokens, 50_000);
    assert!((parsed.total_cost - 5.50).abs() < 0.001);
    assert_eq!(parsed.event_count, 25);
}

#[test]
fn agent_breakdown_serde_roundtrip() {
    use wa_core::storage::AgentMetricBreakdown;

    let breakdown = AgentMetricBreakdown {
        agent_type: "codex".to_string(),
        total_tokens: 100_000,
        total_cost: 10.0,
        avg_tokens_per_event: 5000.0,
    };

    let json = serde_json::to_string(&breakdown).unwrap();
    let parsed: AgentMetricBreakdown = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.agent_type, "codex");
    assert_eq!(parsed.total_tokens, 100_000);
    assert!((parsed.avg_tokens_per_event - 5000.0).abs() < 0.1);
}

#[test]
fn purge_all_metrics() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        for i in 0..10 {
            storage
                .record_usage_metric(make_metric(
                    MetricType::ApiCall,
                    None,
                    None,
                    None,
                    ts + i * 100,
                ))
                .await
                .unwrap();
        }

        // Purge everything
        let purged = storage.purge_usage_metrics(ts + 10_000).await.unwrap();
        assert_eq!(purged, 10);

        let remaining = storage
            .query_usage_metrics(MetricQuery::default())
            .await
            .unwrap();
        assert!(remaining.is_empty());

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn query_results_ordered_desc_by_timestamp() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let base = 1_000_000_000_000i64;
        for i in 0..5 {
            storage
                .record_usage_metric(make_metric(
                    MetricType::TokenUsage,
                    None,
                    Some(i * 100),
                    None,
                    base + i * 1000,
                ))
                .await
                .unwrap();
        }

        let results = storage
            .query_usage_metrics(MetricQuery::default())
            .await
            .unwrap();
        assert_eq!(results.len(), 5);

        // Verify DESC ordering
        for window in results.windows(2) {
            assert!(
                window[0].timestamp >= window[1].timestamp,
                "results should be DESC by timestamp"
            );
        }

        storage.shutdown().await.expect("shutdown");
    });
}

// =========================================================================
// wa-985.5: Integration test: record → aggregate → render
// =========================================================================

#[test]
fn integration_record_aggregate_render_summary() {
    use wa_core::output::{
        AnalyticsSummaryData, AnalyticsSummaryRenderer, OutputFormat, RenderContext,
    };

    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        let batch = vec![
            make_metric(
                MetricType::TokenUsage,
                Some("codex"),
                Some(10_000),
                Some(1.0),
                ts,
            ),
            make_metric(
                MetricType::TokenUsage,
                Some("claude_code"),
                Some(20_000),
                Some(2.0),
                ts + 100,
            ),
            make_metric(
                MetricType::RateLimitHit,
                Some("codex"),
                None,
                None,
                ts + 200,
            ),
            make_metric(MetricType::WorkflowCost, None, None, Some(0.5), ts + 300),
        ];
        storage.record_usage_metrics_batch(batch).await.unwrap();

        // Query all and build summary data
        let all = storage
            .query_usage_metrics(MetricQuery {
                since: Some(ts - 1),
                ..Default::default()
            })
            .await
            .unwrap();

        let total_tokens: i64 = all.iter().filter_map(|r| r.tokens).sum();
        let total_cost: f64 = all.iter().filter_map(|r| r.amount).sum();
        let rate_limits = all
            .iter()
            .filter(|r| r.metric_type == MetricType::RateLimitHit)
            .count();
        let workflows = all
            .iter()
            .filter(|r| r.metric_type == MetricType::WorkflowCost)
            .count();

        let summary = AnalyticsSummaryData {
            period_label: "Test Period".to_string(),
            total_tokens,
            total_cost,
            rate_limit_hits: rate_limits as i64,
            workflow_runs: workflows as i64,
        };

        assert_eq!(summary.total_tokens, 30_000);
        assert!((summary.total_cost - 3.5).abs() < 0.01);
        assert_eq!(summary.rate_limit_hits, 1);
        assert_eq!(summary.workflow_runs, 1);

        // Render as JSON
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = AnalyticsSummaryRenderer::render(&summary, &ctx);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["total_tokens"], 30_000);

        // Render as plain
        let ctx_plain = RenderContext::new(OutputFormat::Plain);
        let plain_output = AnalyticsSummaryRenderer::render(&summary, &ctx_plain);
        assert!(plain_output.contains("30,000"));
        assert!(plain_output.contains("$3.50"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn integration_record_aggregate_render_daily() {
    use wa_core::output::{AnalyticsDailyRenderer, OutputFormat, RenderContext};

    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        let batch = vec![
            make_metric(
                MetricType::TokenUsage,
                Some("codex"),
                Some(5000),
                Some(0.50),
                ts,
            ),
            make_metric(
                MetricType::TokenUsage,
                Some("codex"),
                Some(3000),
                Some(0.30),
                ts + 1000,
            ),
        ];
        storage.record_usage_metrics_batch(batch).await.unwrap();

        let daily = storage.aggregate_daily_metrics(ts - 1).await.unwrap();
        assert!(!daily.is_empty());

        // Render as JSON
        let ctx = RenderContext::new(OutputFormat::Json);
        let output = AnalyticsDailyRenderer::render(&daily, &ctx);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert!(!parsed.is_empty());
        assert!(parsed[0].get("total_tokens").is_some());

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn integration_record_aggregate_render_by_agent() {
    use wa_core::output::{AnalyticsAgentRenderer, OutputFormat, RenderContext};

    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        let batch = vec![
            make_metric(
                MetricType::TokenUsage,
                Some("codex"),
                Some(10_000),
                Some(1.0),
                ts,
            ),
            make_metric(
                MetricType::TokenUsage,
                Some("claude_code"),
                Some(5_000),
                Some(0.50),
                ts + 100,
            ),
        ];
        storage.record_usage_metrics_batch(batch).await.unwrap();

        let by_agent = storage.aggregate_by_agent(ts - 1).await.unwrap();
        assert_eq!(by_agent.len(), 2);

        // Render as plain — should include percentages
        let ctx = RenderContext::new(OutputFormat::Plain);
        let output = AnalyticsAgentRenderer::render(&by_agent, &ctx);
        assert!(output.contains("codex"));
        assert!(output.contains("claude_code"));
        assert!(output.contains("%")); // percentage column

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn integration_record_aggregate_export_csv() {
    use wa_core::output::AnalyticsExportRenderer;

    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = 1_700_000_000_000i64;
        let batch = vec![
            make_metric(MetricType::TokenUsage, Some("codex"), Some(5000), None, ts),
            make_metric(
                MetricType::ApiCost,
                Some("claude_code"),
                None,
                Some(1.25),
                ts + 1000,
            ),
        ];
        storage.record_usage_metrics_batch(batch).await.unwrap();

        let results = storage
            .query_usage_metrics(MetricQuery {
                since: Some(ts - 1),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 2);

        // Export as CSV
        let csv = AnalyticsExportRenderer::render_csv(&results);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 rows
        assert!(lines[0].starts_with("timestamp,metric_type"));

        // Export as JSON
        let json = AnalyticsExportRenderer::render_json(&results);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 2);

        storage.shutdown().await.expect("shutdown");
    });
}

// =========================================================================
// wa-985.5: Alert integration (record → check alerts → verify)
// =========================================================================

#[test]
fn integration_record_metrics_then_check_alerts() {
    use wa_core::alerts::{AlertLevel, AlertMonitor, AlertPeriod, AlertRule};

    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();

        // Record cost metrics totaling $45 (90% of $50 threshold)
        let batch = vec![
            make_metric(MetricType::ApiCost, Some("codex"), None, Some(20.0), ts),
            make_metric(
                MetricType::ApiCost,
                Some("claude_code"),
                None,
                Some(25.0),
                ts + 100,
            ),
        ];
        storage.record_usage_metrics_batch(batch).await.unwrap();

        // Record token metrics totaling 80,000 (80% of 100K threshold)
        let batch2 = vec![
            make_metric(
                MetricType::TokenUsage,
                Some("codex"),
                Some(50_000),
                None,
                ts + 200,
            ),
            make_metric(
                MetricType::TokenUsage,
                Some("claude_code"),
                Some(30_000),
                None,
                ts + 300,
            ),
        ];
        storage.record_usage_metrics_batch(batch2).await.unwrap();

        let monitor = AlertMonitor::new(vec![
            AlertRule::cost("cost-50", 50.0, AlertPeriod::Day),
            AlertRule::token_usage("tokens-100k", 100_000.0, AlertPeriod::Day),
        ]);

        let alerts = monitor.check_alerts(&storage).await.unwrap();
        assert_eq!(alerts.len(), 2);

        let cost_alert = alerts.iter().find(|a| a.rule_id == "cost-50").unwrap();
        assert_eq!(cost_alert.level, AlertLevel::Critical); // 90%
        assert!((cost_alert.current_value - 45.0).abs() < 0.01);
        assert!(cost_alert.summary().contains("critical"));

        let token_alert = alerts.iter().find(|a| a.rule_id == "tokens-100k").unwrap();
        assert_eq!(token_alert.level, AlertLevel::Warning); // 80%

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn integration_purge_removes_old_metrics_from_alert_window() {
    use wa_core::alerts::{AlertMonitor, AlertPeriod, AlertRule};

    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ts = now_ms();
        let old_ts = ts - 2 * 86_400_000; // 2 days ago

        // Record old expensive metrics
        storage
            .record_usage_metric(make_metric(
                MetricType::ApiCost,
                None,
                None,
                Some(100.0),
                old_ts,
            ))
            .await
            .unwrap();
        // Record recent small metric
        storage
            .record_usage_metric(make_metric(MetricType::ApiCost, None, None, Some(5.0), ts))
            .await
            .unwrap();

        // Before purge: old metrics are outside daily window anyway
        let monitor =
            AlertMonitor::new(vec![AlertRule::cost("daily-cost", 50.0, AlertPeriod::Day)]);
        let alerts_before = monitor.check_alerts(&storage).await.unwrap();
        // Only $5 in daily window — no alert
        assert!(alerts_before.is_empty());

        // Purge old metrics
        let purged = storage.purge_usage_metrics(ts - 86_400_000).await.unwrap();
        assert_eq!(purged, 1);

        // After purge: still only $5 — no alert
        let alerts_after = monitor.check_alerts(&storage).await.unwrap();
        assert!(alerts_after.is_empty());

        storage.shutdown().await.expect("shutdown");
    });
}
