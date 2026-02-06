//! Proactive alert system: threshold monitoring and notification triggers.
//!
//! Evaluates configured alert rules against usage metrics to generate
//! alerts before users hit limits or exceed budgets.

use serde::{Deserialize, Serialize};

use crate::storage::{MetricType, StorageHandle, UsageMetricRecord};

/// Time period for aggregating alert thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertPeriod {
    /// Daily aggregation window
    Day,
    /// Weekly aggregation window
    Week,
    /// Monthly aggregation window
    Month,
}

impl AlertPeriod {
    /// Duration in milliseconds for this period.
    pub fn duration_ms(self) -> i64 {
        match self {
            AlertPeriod::Day => 86_400_000,
            AlertPeriod::Week => 604_800_000,
            AlertPeriod::Month => 2_592_000_000, // 30 days
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AlertPeriod::Day => "day",
            AlertPeriod::Week => "week",
            AlertPeriod::Month => "month",
        }
    }
}

impl std::str::FromStr for AlertPeriod {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "day" => Ok(AlertPeriod::Day),
            "week" => Ok(AlertPeriod::Week),
            "month" => Ok(AlertPeriod::Month),
            _ => Err(format!("Unknown alert period: {s}")),
        }
    }
}

impl std::fmt::Display for AlertPeriod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Severity level of a triggered alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AlertLevel {
    /// 50% of threshold reached
    Info,
    /// 75% of threshold reached
    Warning,
    /// 90% of threshold reached
    Critical,
    /// 100%+ of threshold reached
    Exceeded,
}

impl AlertLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertLevel::Info => "info",
            AlertLevel::Warning => "warning",
            AlertLevel::Critical => "critical",
            AlertLevel::Exceeded => "exceeded",
        }
    }

    /// Determine alert level from a percentage (0.0 to 1.0+).
    pub fn from_percent(percent: f64) -> Option<AlertLevel> {
        if percent >= 1.0 {
            Some(AlertLevel::Exceeded)
        } else if percent >= 0.9 {
            Some(AlertLevel::Critical)
        } else if percent >= 0.75 {
            Some(AlertLevel::Warning)
        } else if percent >= 0.5 {
            Some(AlertLevel::Info)
        } else {
            None
        }
    }
}

impl std::fmt::Display for AlertLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What metric an alert rule monitors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertMetric {
    /// Total cost in USD over a period
    Cost,
    /// Total token usage over a period
    TokenUsage,
    /// Rate limit hit frequency over a period
    RateLimitFrequency,
    /// Account balance percentage (current remaining %)
    AccountBalance,
}

impl AlertMetric {
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertMetric::Cost => "cost",
            AlertMetric::TokenUsage => "token_usage",
            AlertMetric::RateLimitFrequency => "rate_limit_frequency",
            AlertMetric::AccountBalance => "account_balance",
        }
    }
}

impl std::str::FromStr for AlertMetric {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "cost" => Ok(AlertMetric::Cost),
            "token_usage" => Ok(AlertMetric::TokenUsage),
            "rate_limit_frequency" => Ok(AlertMetric::RateLimitFrequency),
            "account_balance" => Ok(AlertMetric::AccountBalance),
            _ => Err(format!("Unknown alert metric: {s}")),
        }
    }
}

impl std::fmt::Display for AlertMetric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A configured alert rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    /// Unique rule identifier
    pub id: String,
    /// Which metric to monitor
    pub metric: AlertMetric,
    /// Threshold value (cost in USD, token count, hit count, or percent)
    pub threshold: f64,
    /// Aggregation period
    pub period: AlertPeriod,
    /// Optional agent filter (None = all agents)
    pub agent_type: Option<String>,
    /// Optional account filter (None = all accounts)
    pub account_id: Option<String>,
    /// Service name for account balance alerts (e.g. "anthropic", "openai")
    pub service: Option<String>,
    /// Whether this rule is enabled
    pub enabled: bool,
}

impl AlertRule {
    /// Create a cost alert rule.
    pub fn cost(id: impl Into<String>, threshold: f64, period: AlertPeriod) -> Self {
        Self {
            id: id.into(),
            metric: AlertMetric::Cost,
            threshold,
            period,
            agent_type: None,
            account_id: None,
            service: None,
            enabled: true,
        }
    }

    /// Create a token usage alert rule.
    pub fn token_usage(id: impl Into<String>, threshold: f64, period: AlertPeriod) -> Self {
        Self {
            id: id.into(),
            metric: AlertMetric::TokenUsage,
            threshold,
            period,
            agent_type: None,
            account_id: None,
            service: None,
            enabled: true,
        }
    }

    /// Create a rate limit frequency alert rule.
    pub fn rate_limit(id: impl Into<String>, max_hits: f64, period: AlertPeriod) -> Self {
        Self {
            id: id.into(),
            metric: AlertMetric::RateLimitFrequency,
            threshold: max_hits,
            period,
            agent_type: None,
            account_id: None,
            service: None,
            enabled: true,
        }
    }

    /// Create an account balance alert rule.
    pub fn account_balance(
        id: impl Into<String>,
        min_percent: f64,
        account_id: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            metric: AlertMetric::AccountBalance,
            threshold: min_percent,
            period: AlertPeriod::Day, // period not used for balance alerts
            agent_type: None,
            account_id,
            service: None,
            enabled: true,
        }
    }

    /// Evaluate whether the current value triggers this alert.
    /// Returns the alert level if triggered, None otherwise.
    pub fn check(&self, current_value: f64) -> Option<AlertLevel> {
        if !self.enabled || self.threshold <= 0.0 {
            return None;
        }
        match self.metric {
            AlertMetric::AccountBalance => {
                // For balance alerts, threshold is min_percent_remaining
                // current_value is the current percent remaining
                // Alert when balance drops below threshold
                if current_value <= 0.0 {
                    return Some(AlertLevel::Exceeded);
                }
                let inverse_percent = 1.0 - (current_value / self.threshold);
                AlertLevel::from_percent(inverse_percent)
            }
            _ => {
                let percent = current_value / self.threshold;
                AlertLevel::from_percent(percent)
            }
        }
    }
}

/// A triggered alert ready for notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggeredAlert {
    /// The rule that triggered this alert
    pub rule_id: String,
    /// Metric being monitored
    pub metric: AlertMetric,
    /// Alert severity level
    pub level: AlertLevel,
    /// Current metric value
    pub current_value: f64,
    /// Configured threshold
    pub threshold: f64,
    /// Percentage of threshold (0.0 to 1.0+)
    pub percent_of_threshold: f64,
    /// Period this alert covers
    pub period: AlertPeriod,
    /// Timestamp when alert was evaluated (epoch ms)
    pub evaluated_at: i64,
}

impl TriggeredAlert {
    /// Human-readable summary of this alert.
    pub fn summary(&self) -> String {
        let pct = (self.percent_of_threshold * 100.0) as i64;
        match self.metric {
            AlertMetric::Cost => {
                format!(
                    "{}: {} cost ${:.2} / ${:.2} ({}%)",
                    self.level, self.period, self.current_value, self.threshold, pct
                )
            }
            AlertMetric::TokenUsage => {
                format!(
                    "{}: {} tokens {} / {} ({}%)",
                    self.level, self.period, self.current_value as i64, self.threshold as i64, pct
                )
            }
            AlertMetric::RateLimitFrequency => {
                format!(
                    "{}: {} rate limits {} / {} ({}%)",
                    self.level, self.period, self.current_value as i64, self.threshold as i64, pct
                )
            }
            AlertMetric::AccountBalance => {
                format!(
                    "{}: account balance {:.1}% (min {:.1}%)",
                    self.level, self.current_value, self.threshold
                )
            }
        }
    }
}

/// Alert monitor that evaluates rules against usage metrics.
pub struct AlertMonitor {
    rules: Vec<AlertRule>,
}

impl AlertMonitor {
    /// Create a new monitor with the given rules.
    pub fn new(rules: Vec<AlertRule>) -> Self {
        Self { rules }
    }

    /// Get the current rules.
    pub fn rules(&self) -> &[AlertRule] {
        &self.rules
    }

    /// Add a rule.
    pub fn add_rule(&mut self, rule: AlertRule) {
        self.rules.push(rule);
    }

    /// Remove a rule by ID.
    pub fn remove_rule(&mut self, id: &str) -> bool {
        let before = self.rules.len();
        self.rules.retain(|r| r.id != id);
        self.rules.len() < before
    }

    /// Evaluate all enabled rules against current metric values.
    pub async fn check_alerts(
        &self,
        storage: &StorageHandle,
    ) -> crate::error::Result<Vec<TriggeredAlert>> {
        let now = now_ms();
        let mut triggered = Vec::new();

        for rule in &self.rules {
            if !rule.enabled {
                continue;
            }

            let current_value = self.get_current_value(storage, rule, now).await?;
            if let Some(level) = rule.check(current_value) {
                let percent = if rule.threshold > 0.0 {
                    match rule.metric {
                        AlertMetric::AccountBalance => 1.0 - (current_value / rule.threshold),
                        _ => current_value / rule.threshold,
                    }
                } else {
                    0.0
                };

                triggered.push(TriggeredAlert {
                    rule_id: rule.id.clone(),
                    metric: rule.metric.clone(),
                    level,
                    current_value,
                    threshold: rule.threshold,
                    percent_of_threshold: percent,
                    period: rule.period,
                    evaluated_at: now,
                });
            }
        }

        Ok(triggered)
    }

    /// Query storage for the current value of a metric.
    async fn get_current_value(
        &self,
        storage: &StorageHandle,
        rule: &AlertRule,
        now: i64,
    ) -> crate::error::Result<f64> {
        let since = now - rule.period.duration_ms();

        match rule.metric {
            AlertMetric::Cost => {
                let query = crate::storage::MetricQuery {
                    metric_type: Some(MetricType::ApiCost),
                    agent_type: rule.agent_type.clone(),
                    since: Some(since),
                    ..Default::default()
                };
                let records = storage.query_usage_metrics(query).await?;
                Ok(sum_amounts(&records))
            }
            AlertMetric::TokenUsage => {
                let query = crate::storage::MetricQuery {
                    metric_type: Some(MetricType::TokenUsage),
                    agent_type: rule.agent_type.clone(),
                    since: Some(since),
                    ..Default::default()
                };
                let records = storage.query_usage_metrics(query).await?;
                Ok(sum_tokens(&records) as f64)
            }
            AlertMetric::RateLimitFrequency => {
                let query = crate::storage::MetricQuery {
                    metric_type: Some(MetricType::RateLimitHit),
                    agent_type: rule.agent_type.clone(),
                    since: Some(since),
                    ..Default::default()
                };
                let records = storage.query_usage_metrics(query).await?;
                Ok(records.len() as f64)
            }
            AlertMetric::AccountBalance => {
                // For account balance, query accounts by service
                // Returns 100.0 (no alert) if no service configured or no accounts found
                let service = match rule.service.as_deref() {
                    Some(s) => s.to_string(),
                    None => return Ok(100.0),
                };
                let accounts = storage.get_accounts_by_service(&service).await?;
                if let Some(ref target_id) = rule.account_id {
                    accounts
                        .iter()
                        .find(|a| a.account_id == *target_id)
                        .map_or(Ok(100.0), |a| Ok(a.percent_remaining))
                } else if accounts.is_empty() {
                    Ok(100.0)
                } else {
                    let total: f64 = accounts.iter().map(|a| a.percent_remaining).sum();
                    Ok(total / accounts.len() as f64)
                }
            }
        }
    }
}

fn sum_amounts(records: &[UsageMetricRecord]) -> f64 {
    records.iter().filter_map(|r| r.amount).sum()
}

fn sum_tokens(records: &[UsageMetricRecord]) -> i64 {
    records.iter().filter_map(|r| r.tokens).sum()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_level_from_percent_thresholds() {
        assert_eq!(AlertLevel::from_percent(0.0), None);
        assert_eq!(AlertLevel::from_percent(0.49), None);
        assert_eq!(AlertLevel::from_percent(0.5), Some(AlertLevel::Info));
        assert_eq!(AlertLevel::from_percent(0.74), Some(AlertLevel::Info));
        assert_eq!(AlertLevel::from_percent(0.75), Some(AlertLevel::Warning));
        assert_eq!(AlertLevel::from_percent(0.89), Some(AlertLevel::Warning));
        assert_eq!(AlertLevel::from_percent(0.9), Some(AlertLevel::Critical));
        assert_eq!(AlertLevel::from_percent(0.99), Some(AlertLevel::Critical));
        assert_eq!(AlertLevel::from_percent(1.0), Some(AlertLevel::Exceeded));
        assert_eq!(AlertLevel::from_percent(1.5), Some(AlertLevel::Exceeded));
    }

    #[test]
    fn alert_level_ordering() {
        assert!(AlertLevel::Info < AlertLevel::Warning);
        assert!(AlertLevel::Warning < AlertLevel::Critical);
        assert!(AlertLevel::Critical < AlertLevel::Exceeded);
    }

    #[test]
    fn cost_rule_check() {
        let rule = AlertRule::cost("cost-daily", 50.0, AlertPeriod::Day);
        assert_eq!(rule.check(24.0), None); // 48%
        assert_eq!(rule.check(25.0), Some(AlertLevel::Info)); // 50%
        assert_eq!(rule.check(37.5), Some(AlertLevel::Warning)); // 75%
        assert_eq!(rule.check(45.0), Some(AlertLevel::Critical)); // 90%
        assert_eq!(rule.check(50.0), Some(AlertLevel::Exceeded)); // 100%
        assert_eq!(rule.check(60.0), Some(AlertLevel::Exceeded)); // 120%
    }

    #[test]
    fn token_usage_rule_check() {
        let rule = AlertRule::token_usage("tokens-week", 100_000.0, AlertPeriod::Week);
        assert_eq!(rule.check(49_000.0), None);
        assert_eq!(rule.check(50_000.0), Some(AlertLevel::Info));
        assert_eq!(rule.check(100_000.0), Some(AlertLevel::Exceeded));
    }

    #[test]
    fn rate_limit_rule_check() {
        let rule = AlertRule::rate_limit("ratelimit-day", 10.0, AlertPeriod::Day);
        assert_eq!(rule.check(4.0), None);
        assert_eq!(rule.check(5.0), Some(AlertLevel::Info));
        assert_eq!(rule.check(10.0), Some(AlertLevel::Exceeded));
    }

    #[test]
    fn account_balance_rule_check() {
        let rule = AlertRule::account_balance("balance-low", 20.0, None);
        // threshold=20%, current=100% → not triggered
        assert_eq!(rule.check(100.0), None);
        // current=15% → triggered (below 20%)
        assert_eq!(rule.check(10.0), Some(AlertLevel::Info));
        // current=5% → critical
        assert_eq!(rule.check(2.0), Some(AlertLevel::Critical));
        // current=0% → exceeded
        assert_eq!(rule.check(0.0), Some(AlertLevel::Exceeded));
    }

    #[test]
    fn disabled_rule_never_triggers() {
        let mut rule = AlertRule::cost("cost-daily", 50.0, AlertPeriod::Day);
        rule.enabled = false;
        assert_eq!(rule.check(100.0), None);
    }

    #[test]
    fn zero_threshold_never_triggers() {
        let rule = AlertRule::cost("zero", 0.0, AlertPeriod::Day);
        assert_eq!(rule.check(100.0), None);
    }

    #[test]
    fn alert_period_roundtrip() {
        for period in [AlertPeriod::Day, AlertPeriod::Week, AlertPeriod::Month] {
            let s = period.as_str();
            let parsed: AlertPeriod = s.parse().unwrap();
            assert_eq!(parsed, period);
            assert_eq!(period.to_string(), s);
        }
    }

    #[test]
    fn alert_metric_roundtrip() {
        for metric in [
            AlertMetric::Cost,
            AlertMetric::TokenUsage,
            AlertMetric::RateLimitFrequency,
            AlertMetric::AccountBalance,
        ] {
            let s = metric.as_str();
            let parsed: AlertMetric = s.parse().unwrap();
            assert_eq!(parsed, metric);
            assert_eq!(metric.to_string(), s);
        }
    }

    #[test]
    fn triggered_alert_summary_cost() {
        let alert = TriggeredAlert {
            rule_id: "cost-daily".to_string(),
            metric: AlertMetric::Cost,
            level: AlertLevel::Warning,
            current_value: 37.50,
            threshold: 50.0,
            percent_of_threshold: 0.75,
            period: AlertPeriod::Day,
            evaluated_at: 0,
        };
        let summary = alert.summary();
        assert!(summary.contains("warning"));
        assert!(summary.contains("37.50"));
        assert!(summary.contains("50.00"));
        assert!(summary.contains("75%"));
    }

    #[test]
    fn triggered_alert_summary_token() {
        let alert = TriggeredAlert {
            rule_id: "tokens-week".to_string(),
            metric: AlertMetric::TokenUsage,
            level: AlertLevel::Critical,
            current_value: 90_000.0,
            threshold: 100_000.0,
            percent_of_threshold: 0.9,
            period: AlertPeriod::Week,
            evaluated_at: 0,
        };
        let summary = alert.summary();
        assert!(summary.contains("critical"));
        assert!(summary.contains("90000"));
        assert!(summary.contains("100000"));
    }

    #[test]
    fn monitor_add_remove_rules() {
        let mut monitor = AlertMonitor::new(vec![
            AlertRule::cost("r1", 50.0, AlertPeriod::Day),
            AlertRule::token_usage("r2", 100_000.0, AlertPeriod::Week),
        ]);
        assert_eq!(monitor.rules().len(), 2);

        monitor.add_rule(AlertRule::rate_limit("r3", 10.0, AlertPeriod::Day));
        assert_eq!(monitor.rules().len(), 3);

        assert!(monitor.remove_rule("r2"));
        assert_eq!(monitor.rules().len(), 2);

        assert!(!monitor.remove_rule("nonexistent"));
        assert_eq!(monitor.rules().len(), 2);
    }

    #[test]
    fn alert_period_duration_ms() {
        assert_eq!(AlertPeriod::Day.duration_ms(), 86_400_000);
        assert_eq!(AlertPeriod::Week.duration_ms(), 604_800_000);
        assert_eq!(AlertPeriod::Month.duration_ms(), 2_592_000_000);
    }
}
