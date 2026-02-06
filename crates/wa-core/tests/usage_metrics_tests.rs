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
