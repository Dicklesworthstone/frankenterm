// Integration tests for the proactive alert system (wa-985.4)
use tempfile::TempDir;
use wa_core::alerts::{AlertLevel, AlertMonitor, AlertPeriod, AlertRule};
use wa_core::storage::{MetricType, StorageHandle, UsageMetricRecord};

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

fn make_cost_metric(amount: f64, ts: i64) -> UsageMetricRecord {
    UsageMetricRecord {
        id: 0,
        timestamp: ts,
        metric_type: MetricType::ApiCost,
        pane_id: None,
        agent_type: Some("claude_code".to_string()),
        account_id: None,
        workflow_id: None,
        count: None,
        amount: Some(amount),
        tokens: None,
        metadata: None,
        created_at: ts,
    }
}

fn make_token_metric(tokens: i64, ts: i64) -> UsageMetricRecord {
    UsageMetricRecord {
        id: 0,
        timestamp: ts,
        metric_type: MetricType::TokenUsage,
        pane_id: None,
        agent_type: Some("codex".to_string()),
        account_id: None,
        workflow_id: None,
        count: None,
        amount: None,
        tokens: Some(tokens),
        metadata: None,
        created_at: ts,
    }
}

fn make_ratelimit_metric(ts: i64) -> UsageMetricRecord {
    UsageMetricRecord {
        id: 0,
        timestamp: ts,
        metric_type: MetricType::RateLimitHit,
        pane_id: None,
        agent_type: None,
        account_id: None,
        workflow_id: None,
        count: Some(1),
        amount: None,
        tokens: None,
        metadata: None,
        created_at: ts,
    }
}

// ---- Cost alerts ----

#[test]
fn cost_alert_triggers_at_threshold() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        // Insert cost metrics totaling $40 (80% of $50)
        storage
            .record_usage_metric(make_cost_metric(25.0, ts - 1000))
            .await
            .unwrap();
        storage
            .record_usage_metric(make_cost_metric(15.0, ts - 500))
            .await
            .unwrap();

        let monitor =
            AlertMonitor::new(vec![AlertRule::cost("cost-daily", 50.0, AlertPeriod::Day)]);

        let alerts = monitor.check_alerts(&storage).await.unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].level, AlertLevel::Warning); // 80%
        assert_eq!(alerts[0].rule_id, "cost-daily");
        assert!((alerts[0].current_value - 40.0).abs() < 0.01);
    });
}

#[test]
fn cost_alert_not_triggered_below_threshold() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        // Insert $10 (20% of $50)
        storage
            .record_usage_metric(make_cost_metric(10.0, ts))
            .await
            .unwrap();

        let monitor =
            AlertMonitor::new(vec![AlertRule::cost("cost-daily", 50.0, AlertPeriod::Day)]);

        let alerts = monitor.check_alerts(&storage).await.unwrap();
        assert!(alerts.is_empty());
    });
}

#[test]
fn cost_alert_exceeded() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        storage
            .record_usage_metric(make_cost_metric(55.0, ts))
            .await
            .unwrap();

        let monitor =
            AlertMonitor::new(vec![AlertRule::cost("cost-daily", 50.0, AlertPeriod::Day)]);

        let alerts = monitor.check_alerts(&storage).await.unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].level, AlertLevel::Exceeded);
    });
}

// ---- Token alerts ----

#[test]
fn token_alert_triggers() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        storage
            .record_usage_metric(make_token_metric(50_000, ts - 1000))
            .await
            .unwrap();
        storage
            .record_usage_metric(make_token_metric(40_000, ts))
            .await
            .unwrap();

        let monitor = AlertMonitor::new(vec![AlertRule::token_usage(
            "tokens-day",
            100_000.0,
            AlertPeriod::Day,
        )]);

        let alerts = monitor.check_alerts(&storage).await.unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].level, AlertLevel::Critical); // 90%
    });
}

// ---- Rate limit alerts ----

#[test]
fn rate_limit_alert_triggers() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        // Insert 8 rate limit events (80% of 10)
        for i in 0..8 {
            storage
                .record_usage_metric(make_ratelimit_metric(ts - i * 100))
                .await
                .unwrap();
        }

        let monitor = AlertMonitor::new(vec![AlertRule::rate_limit(
            "rl-day",
            10.0,
            AlertPeriod::Day,
        )]);

        let alerts = monitor.check_alerts(&storage).await.unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].level, AlertLevel::Warning); // 80%
    });
}

// ---- Multiple rules ----

#[test]
fn multiple_rules_evaluated() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        // Cost: $45 (90% of $50)
        storage
            .record_usage_metric(make_cost_metric(45.0, ts))
            .await
            .unwrap();
        // Tokens: 50K (50% of 100K)
        storage
            .record_usage_metric(make_token_metric(50_000, ts))
            .await
            .unwrap();

        let monitor = AlertMonitor::new(vec![
            AlertRule::cost("cost-daily", 50.0, AlertPeriod::Day),
            AlertRule::token_usage("tokens-daily", 100_000.0, AlertPeriod::Day),
        ]);

        let alerts = monitor.check_alerts(&storage).await.unwrap();
        assert_eq!(alerts.len(), 2);

        let cost_alert = alerts.iter().find(|a| a.rule_id == "cost-daily").unwrap();
        assert_eq!(cost_alert.level, AlertLevel::Critical);

        let token_alert = alerts.iter().find(|a| a.rule_id == "tokens-daily").unwrap();
        assert_eq!(token_alert.level, AlertLevel::Info);
    });
}

// ---- Disabled rules ----

#[test]
fn disabled_rule_not_evaluated() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        storage
            .record_usage_metric(make_cost_metric(100.0, ts))
            .await
            .unwrap();

        let mut rule = AlertRule::cost("cost-daily", 50.0, AlertPeriod::Day);
        rule.enabled = false;
        let monitor = AlertMonitor::new(vec![rule]);

        let alerts = monitor.check_alerts(&storage).await.unwrap();
        assert!(alerts.is_empty());
    });
}

// ---- Empty database ----

#[test]
fn no_metrics_no_alerts() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let monitor = AlertMonitor::new(vec![
            AlertRule::cost("cost-daily", 50.0, AlertPeriod::Day),
            AlertRule::token_usage("tokens-daily", 100_000.0, AlertPeriod::Day),
        ]);

        let alerts = monitor.check_alerts(&storage).await.unwrap();
        assert!(alerts.is_empty());
    });
}

// ---- Old metrics outside window ----

#[test]
fn old_metrics_outside_window_not_counted() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        // Insert an old metric (2 days ago, outside daily window)
        let old_ts = ts - 2 * 86_400_000;
        storage
            .record_usage_metric(make_cost_metric(100.0, old_ts))
            .await
            .unwrap();

        // Insert a recent metric within window
        storage
            .record_usage_metric(make_cost_metric(10.0, ts))
            .await
            .unwrap();

        let monitor =
            AlertMonitor::new(vec![AlertRule::cost("cost-daily", 50.0, AlertPeriod::Day)]);

        let alerts = monitor.check_alerts(&storage).await.unwrap();
        // Only $10 in window, 20% — no alert
        assert!(alerts.is_empty());
    });
}

// ---- Triggered alert summary ----

#[test]
fn triggered_alert_has_correct_summary() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        storage
            .record_usage_metric(make_cost_metric(45.0, ts))
            .await
            .unwrap();

        let monitor =
            AlertMonitor::new(vec![AlertRule::cost("cost-daily", 50.0, AlertPeriod::Day)]);

        let alerts = monitor.check_alerts(&storage).await.unwrap();
        assert_eq!(alerts.len(), 1);

        let summary = alerts[0].summary();
        assert!(summary.contains("critical"));
        assert!(summary.contains("45.00"));
        assert!(summary.contains("50.00"));
        assert!(summary.contains("90%"));
    });
}

// ---- Account balance (no service = no alert) ----

#[test]
fn account_balance_no_service_returns_no_alert() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let rule = AlertRule::account_balance("bal-low", 20.0, None);
        let monitor = AlertMonitor::new(vec![rule]);

        let alerts = monitor.check_alerts(&storage).await.unwrap();
        // No service configured, returns 100.0, no alert
        assert!(alerts.is_empty());
    });
}
