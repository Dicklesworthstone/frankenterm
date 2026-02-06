// Integration tests for analytics renderers and helpers (wa-985.3)
use wa_core::output::{
    AnalyticsAgentRenderer, AnalyticsDailyRenderer, AnalyticsExportRenderer, AnalyticsSummaryData,
    AnalyticsSummaryRenderer, OutputFormat, RenderContext,
};
use wa_core::storage::{AgentMetricBreakdown, DailyMetricSummary, MetricType, UsageMetricRecord};

// ---- AnalyticsSummaryData serialization ----

#[test]
fn summary_data_serde_roundtrip() {
    let data = AnalyticsSummaryData {
        period_label: "Last 7 Days".to_string(),
        total_tokens: 1_234_567,
        total_cost: 12.34,
        rate_limit_hits: 3,
        workflow_runs: 45,
    };

    let json = serde_json::to_string(&data).unwrap();
    let parsed: AnalyticsSummaryData = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.total_tokens, 1_234_567);
    assert!((parsed.total_cost - 12.34).abs() < 0.001);
    assert_eq!(parsed.rate_limit_hits, 3);
    assert_eq!(parsed.workflow_runs, 45);
    assert_eq!(parsed.period_label, "Last 7 Days");
}

// ---- Summary renderer: plain ----

#[test]
fn summary_renderer_plain() {
    let data = AnalyticsSummaryData {
        period_label: "Last 7 Days".to_string(),
        total_tokens: 50_000,
        total_cost: 5.50,
        rate_limit_hits: 2,
        workflow_runs: 10,
    };
    let ctx = RenderContext::new(OutputFormat::Plain);
    let output = AnalyticsSummaryRenderer::render(&data, &ctx);

    assert!(output.contains("Usage Analytics"));
    assert!(output.contains("Last 7 Days"));
    assert!(output.contains("50,000"));
    assert!(output.contains("$5.50"));
    assert!(output.contains("2"));
    assert!(output.contains("10"));
}

// ---- Summary renderer: json ----

#[test]
fn summary_renderer_json() {
    let data = AnalyticsSummaryData {
        period_label: "Last 30 Days".to_string(),
        total_tokens: 100_000,
        total_cost: 10.0,
        rate_limit_hits: 0,
        workflow_runs: 5,
    };
    let ctx = RenderContext::new(OutputFormat::Json);
    let output = AnalyticsSummaryRenderer::render(&data, &ctx);

    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["total_tokens"], 100_000);
    assert_eq!(parsed["period_label"], "Last 30 Days");
}

// ---- Daily renderer: plain ----

#[test]
fn daily_renderer_plain() {
    let days = vec![
        DailyMetricSummary {
            day_ts: 1_706_745_600_000, // 2024-02-01
            agent_type: None,
            total_tokens: 25_000,
            total_cost: 2.50,
            event_count: 10,
        },
        DailyMetricSummary {
            day_ts: 1_706_832_000_000, // 2024-02-02
            agent_type: None,
            total_tokens: 30_000,
            total_cost: 3.00,
            event_count: 12,
        },
    ];
    let ctx = RenderContext::new(OutputFormat::Plain);
    let output = AnalyticsDailyRenderer::render(&days, &ctx);

    assert!(output.contains("Daily Metrics"));
    assert!(output.contains("25,000"));
    assert!(output.contains("$2.50"));
    assert!(output.contains("30,000"));
    assert!(output.contains("$3.00"));
}

// ---- Daily renderer: empty ----

#[test]
fn daily_renderer_empty() {
    let ctx = RenderContext::new(OutputFormat::Plain);
    let output = AnalyticsDailyRenderer::render(&[], &ctx);
    assert!(output.contains("No daily metrics found"));
}

// ---- Daily renderer: json ----

#[test]
fn daily_renderer_json() {
    let days = vec![DailyMetricSummary {
        day_ts: 1_706_745_600_000,
        agent_type: None,
        total_tokens: 25_000,
        total_cost: 2.50,
        event_count: 10,
    }];
    let ctx = RenderContext::new(OutputFormat::Json);
    let output = AnalyticsDailyRenderer::render(&days, &ctx);

    let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0]["total_tokens"], 25_000);
}

// ---- Agent renderer: plain ----

#[test]
fn agent_renderer_plain() {
    let agents = vec![
        AgentMetricBreakdown {
            agent_type: "codex".to_string(),
            total_tokens: 75_000,
            total_cost: 7.50,
            avg_tokens_per_event: 5000.0,
        },
        AgentMetricBreakdown {
            agent_type: "claude_code".to_string(),
            total_tokens: 25_000,
            total_cost: 2.50,
            avg_tokens_per_event: 2500.0,
        },
    ];
    let ctx = RenderContext::new(OutputFormat::Plain);
    let output = AnalyticsAgentRenderer::render(&agents, &ctx);

    assert!(output.contains("Breakdown by Agent"));
    assert!(output.contains("codex"));
    assert!(output.contains("claude_code"));
    assert!(output.contains("75,000"));
    assert!(output.contains("25,000"));
    assert!(output.contains("75%"));
    assert!(output.contains("25%"));
}

// ---- Agent renderer: empty ----

#[test]
fn agent_renderer_empty() {
    let ctx = RenderContext::new(OutputFormat::Plain);
    let output = AnalyticsAgentRenderer::render(&[], &ctx);
    assert!(output.contains("No agent metrics found"));
}

// ---- Agent renderer: json ----

#[test]
fn agent_renderer_json() {
    let agents = vec![AgentMetricBreakdown {
        agent_type: "codex".to_string(),
        total_tokens: 100_000,
        total_cost: 10.0,
        avg_tokens_per_event: 5000.0,
    }];
    let ctx = RenderContext::new(OutputFormat::Json);
    let output = AnalyticsAgentRenderer::render(&agents, &ctx);

    let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0]["agent_type"], "codex");
    assert_eq!(parsed[0]["total_tokens"], 100_000);
}

// ---- Export renderer: CSV ----

#[test]
fn export_csv_format() {
    let metrics = vec![
        UsageMetricRecord {
            id: 1,
            timestamp: 1_700_000_000_000,
            metric_type: MetricType::TokenUsage,
            pane_id: None,
            agent_type: Some("codex".to_string()),
            account_id: None,
            workflow_id: None,
            count: None,
            amount: None,
            tokens: Some(5000),
            metadata: None,
            created_at: 1_700_000_000_000,
        },
        UsageMetricRecord {
            id: 2,
            timestamp: 1_700_000_001_000,
            metric_type: MetricType::ApiCost,
            pane_id: None,
            agent_type: Some("claude_code".to_string()),
            account_id: Some("acc-1".to_string()),
            workflow_id: None,
            count: None,
            amount: Some(0.50),
            tokens: None,
            metadata: None,
            created_at: 1_700_000_001_000,
        },
    ];

    let csv = AnalyticsExportRenderer::render_csv(&metrics);

    // Check header
    assert!(csv.starts_with("timestamp,metric_type,agent_type,account_id,tokens,amount,count\n"));

    // Check rows
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 3); // header + 2 rows
    assert!(lines[1].contains("1700000000000"));
    assert!(lines[1].contains("token_usage"));
    assert!(lines[1].contains("codex"));
    assert!(lines[1].contains("5000"));
    assert!(lines[2].contains("api_cost"));
    assert!(lines[2].contains("claude_code"));
    assert!(lines[2].contains("acc-1"));
    assert!(lines[2].contains("0.5000"));
}

// ---- Export renderer: JSON ----

#[test]
fn export_json_format() {
    let metrics = vec![UsageMetricRecord {
        id: 1,
        timestamp: 1_700_000_000_000,
        metric_type: MetricType::TokenUsage,
        pane_id: None,
        agent_type: Some("codex".to_string()),
        account_id: None,
        workflow_id: None,
        count: None,
        amount: None,
        tokens: Some(5000),
        metadata: None,
        created_at: 1_700_000_000_000,
    }];

    let json = AnalyticsExportRenderer::render_json(&metrics);
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0]["tokens"], 5000);
}

// ---- Export renderer: empty ----

#[test]
fn export_csv_empty() {
    let csv = AnalyticsExportRenderer::render_csv(&[]);
    assert_eq!(
        csv,
        "timestamp,metric_type,agent_type,account_id,tokens,amount,count\n"
    );
}

#[test]
fn export_json_empty() {
    let json = AnalyticsExportRenderer::render_json(&[]);
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert!(parsed.is_empty());
}

// ---- Summary with zeros ----

#[test]
fn summary_renderer_zeros() {
    let data = AnalyticsSummaryData {
        period_label: "Last 1 Day".to_string(),
        total_tokens: 0,
        total_cost: 0.0,
        rate_limit_hits: 0,
        workflow_runs: 0,
    };
    let ctx = RenderContext::new(OutputFormat::Plain);
    let output = AnalyticsSummaryRenderer::render(&data, &ctx);

    assert!(output.contains("0"));
    assert!(output.contains("$0.00"));
}

// ---- Agent percentage with single agent ----

#[test]
fn agent_renderer_single_agent_100_pct() {
    let agents = vec![AgentMetricBreakdown {
        agent_type: "codex".to_string(),
        total_tokens: 50_000,
        total_cost: 5.0,
        avg_tokens_per_event: 1000.0,
    }];
    let ctx = RenderContext::new(OutputFormat::Plain);
    let output = AnalyticsAgentRenderer::render(&agents, &ctx);
    assert!(output.contains("100%"));
}
