//! Proactive alert system: threshold monitoring and notification triggers.
//!
//! Evaluates configured alert rules against usage metrics to generate
//! alerts before users hit limits or exceed budgets.

use serde::{Deserialize, Serialize};

use crate::caut::{CautClient, CautService};
use crate::error::RuntimeOperationSource;
#[cfg(test)]
use crate::storage::UsageMetricRecord;
use crate::storage::{MetricType, StorageHandle};

/// A watcher account_balance rule treats account data older than this as
/// unavailable rather than as the current balance.
const ACCOUNT_BALANCE_MAX_AGE_MS: i64 = 15 * 60 * 1000;

/// The watcher asks caut again once a balance rule's account data is this old,
/// and runs caut at most this often per service, whether or not the last run
/// succeeded. Two more attempts fit before the data ages out.
const ACCOUNT_REFRESH_AFTER_MS: i64 = 5 * 60 * 1000;

/// The fraction of a rule's threshold a watcher alert's value must clear,
/// beyond a level's boundary, before the alert falls back to a lower level or
/// recovers. Escalation needs no margin.
const ALERT_RECOVERY_MARGIN: f64 = 0.05;

fn alert_check_cancelled_error(detail: impl Into<String>) -> crate::Error {
    crate::Error::RuntimeOperation {
        operation: "alerts.check_alerts",
        source: RuntimeOperationSource::Cancelled(detail.into()),
    }
}

/// Time period for aggregating alert thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertPeriod {
    /// Daily aggregation window
    #[serde(alias = "day")]
    Day,
    /// Weekly aggregation window
    #[serde(alias = "week")]
    Week,
    /// Monthly aggregation window
    #[serde(alias = "month")]
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

    /// Determine alert level for a low-watermark threshold based on the
    /// fraction of the configured minimum that is still remaining.
    fn from_low_watermark_ratio(remaining_ratio: f64) -> Option<AlertLevel> {
        if remaining_ratio <= 0.0 {
            Some(AlertLevel::Exceeded)
        } else if remaining_ratio <= 0.5 {
            Some(AlertLevel::Critical)
        } else if remaining_ratio <= 0.75 {
            Some(AlertLevel::Warning)
        } else if remaining_ratio < 1.0 {
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
    #[serde(alias = "cost")]
    Cost,
    /// Total token usage over a period
    #[serde(alias = "token_usage")]
    TokenUsage,
    /// Rate limit hit frequency over a period
    #[serde(alias = "rate_limit_frequency")]
    RateLimitFrequency,
    /// Account balance percentage (current remaining %)
    #[serde(alias = "account_balance")]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// An operator-selected pane supplies both the metric scope and the event's
/// notification identity. Account rules observe the explicitly named account;
/// their pane is the operator-selected notification target, not inferred from
/// an account name or a recent unrelated event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatcherAlertRule {
    pub pane_id: u64,
    pub id: String,
    pub metric: AlertMetric,
    pub threshold: f64,
    /// The rolling window a cost, token or rate-limit total covers. Required
    /// for those metrics and refused for `account_balance`, which reads the
    /// account's current remaining percentage. Being part of the rule's
    /// config hash, an ignored period would re-emit an active alert on edit.
    pub period: Option<AlertPeriod>,
    pub agent_type: Option<String>,
    pub account_id: Option<String>,
    pub service: Option<String>,
    #[serde(default = "alert_rule_enabled")]
    pub enabled: bool,
}

const fn alert_rule_enabled() -> bool {
    true
}

impl WatcherAlertRule {
    fn as_rule(&self) -> AlertRule {
        AlertRule {
            id: self.id.clone(),
            metric: self.metric.clone(),
            threshold: self.threshold,
            // Only account_balance has no period, and it never reads one; this
            // matches AlertRule::account_balance.
            period: self.period.unwrap_or(AlertPeriod::Day),
            agent_type: self.agent_type.clone(),
            account_id: self.account_id.clone(),
            service: self.service.clone(),
            enabled: self.enabled,
        }
    }
}

/// Optional watcher alerts. Disabled by default; ordinary watcher startup
/// performs no alert queries or spawns an alert task.
///
/// ```toml
/// [alerts]
/// enabled = true
/// check_interval_secs = 60
/// repeat_after_secs = 1800
///
/// [[alerts.rules]]
/// id = "agent-token-budget"
/// pane_id = 7
/// metric = "token_usage"
/// threshold = 1000000
/// period = "day"
/// ```
///
/// Events are always retained in the ordinary event history. Desktop/webhook
/// delivery uses `[notifications]`, including its filters, mutes and cooldowns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatcherAlertConfig {
    pub enabled: bool,
    pub check_interval_secs: u64,
    pub repeat_after_secs: u64,
    pub rules: Vec<WatcherAlertRule>,
}

impl Default for WatcherAlertConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            check_interval_secs: 60,
            repeat_after_secs: 1800,
            rules: Vec::new(),
        }
    }
}

impl WatcherAlertConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(5..=3600).contains(&self.check_interval_secs) {
            return Err("alerts.check_interval_secs must be in 5..=3600".to_string());
        }
        if !(self.check_interval_secs..=86_400).contains(&self.repeat_after_secs) {
            return Err(
                "alerts.repeat_after_secs must be at least the check interval and at most 86400"
                    .to_string(),
            );
        }
        if self.rules.len() > 64 {
            return Err("alerts.rules supports at most 64 rules".to_string());
        }
        if self.enabled && !self.rules.iter().any(|rule| rule.enabled) {
            return Err("alerts.enabled requires at least one enabled rule".to_string());
        }
        let mut ids = std::collections::HashSet::new();
        for rule in &self.rules {
            if rule.id.is_empty()
                || rule.id.len() > 64
                || !rule
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                || !ids.insert(rule.id.as_str())
            {
                return Err(
                    "alerts.rules requires unique 1..=64 byte alphanumeric, '_' or '-' IDs"
                        .to_string(),
                );
            }
            if i64::try_from(rule.pane_id).is_err() {
                return Err(format!("alerts rule {} has an oversized pane_id", rule.id));
            }
            if !rule.threshold.is_finite() || rule.threshold <= 0.0 {
                return Err(format!(
                    "alerts rule {} requires a finite positive threshold",
                    rule.id
                ));
            }
            for value in [&rule.agent_type, &rule.account_id, &rule.service]
                .into_iter()
                .flatten()
            {
                if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                    return Err(format!(
                        "alerts rule {} has an invalid scope filter",
                        rule.id
                    ));
                }
            }
            if rule.metric == AlertMetric::AccountBalance {
                if rule.threshold > 100.0 || rule.service.is_none() || rule.account_id.is_none() {
                    return Err(format!(
                        "alerts rule {} account_balance requires service, account_id \
                         and a threshold in (0, 100]",
                        rule.id
                    ));
                }
                if rule
                    .service
                    .as_deref()
                    .and_then(CautService::from_cli_input)
                    .is_none()
                {
                    // Accounts are stored and refreshed under caut's service
                    // names, so any other name could never match.
                    return Err(format!(
                        "alerts rule {} account_balance service must be one of: {}",
                        rule.id,
                        CautService::supported_cli_inputs().join(", ")
                    ));
                }
                if rule.agent_type.is_some() {
                    return Err(format!(
                        "alerts rule {} account_balance does not accept agent_type",
                        rule.id
                    ));
                }
                if rule.period.is_some() {
                    return Err(format!(
                        "alerts rule {} account_balance does not accept period: it checks \
                         the account's current remaining percentage",
                        rule.id
                    ));
                }
            } else {
                if rule.period.is_none() {
                    return Err(format!(
                        "alerts rule {} requires period = \"day\", \"week\" or \"month\"",
                        rule.id
                    ));
                }
                if rule.service.is_some() || rule.account_id.is_some() {
                    // Pane-scoped usage rows are recorded without an account,
                    // and account-attributed rows have no pane, so an account
                    // filter on a pane total could never match and the rule
                    // would never fire.
                    return Err(format!(
                        "alerts rule {} service and account_id are only supported for \
                         account_balance; recorded pane usage carries no account",
                        rule.id
                    ));
                }
            }
        }
        Ok(())
    }
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
                // Account balance thresholds are lower bounds rather than
                // upper bounds: alert as soon as remaining balance drops below
                // the configured minimum, then escalate as the minimum is
                // further depleted.
                AlertLevel::from_low_watermark_ratio(current_value / self.threshold)
            }
            _ => {
                let percent = current_value / self.threshold;
                AlertLevel::from_percent(percent)
            }
        }
    }

    /// The level `current_value` reaches when [`ALERT_RECOVERY_MARGIN`] of the
    /// threshold is counted against it. A watcher alert falls back to a lower
    /// level only when the value is below that level's boundary by more than
    /// the margin, so a total hovering at a boundary does not alternate
    /// between alert and recovery as rows age out and arrive.
    fn check_with_recovery_margin(&self, current_value: f64) -> Option<AlertLevel> {
        if !self.enabled || self.threshold <= 0.0 {
            return None;
        }
        let ratio = current_value / self.threshold;
        match self.metric {
            AlertMetric::AccountBalance => {
                AlertLevel::from_low_watermark_ratio(ratio - ALERT_RECOVERY_MARGIN)
            }
            _ => AlertLevel::from_percent(ratio + ALERT_RECOVERY_MARGIN),
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
    /// Percentage of threshold (0.0 to 1.0+). Account balance alerts store
    /// the deficit relative to the configured minimum (0.0 = at threshold,
    /// 1.0 = fully depleted).
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
                    "{}: account balance {:.1}% (min {:.1}%) ({}% depleted)",
                    self.level, self.current_value, self.threshold, pct
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
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.check_alerts_with_cx(&cx, storage).await
    }

    /// Cx-first [`Self::check_alerts`] (ft-xbnl0.2.3). Threads
    /// caller `&Cx` through the rule-evaluation loop via
    /// `cx.checkpoint()?` at the top of each iteration. Each
    /// rule queries a bounded aggregate on its own period
    /// window — a long rule list against a
    /// busy DB can take seconds. Per-rule checkpoint lets an
    /// operator cancel a slow alert sweep cleanly; triggered
    /// alerts from rules already evaluated are discarded on
    /// cancel (cancellation returns Err, not partial Vec).
    pub async fn check_alerts_with_cx(
        &self,
        cx: &crate::cx::Cx,
        storage: &StorageHandle,
    ) -> crate::error::Result<Vec<TriggeredAlert>> {
        cx.checkpoint()
            .map_err(|err| alert_check_cancelled_error(format!("pre-start: {err}")))?;

        let now = now_ms();
        let mut triggered = Vec::new();

        for rule in &self.rules {
            if !rule.enabled {
                continue;
            }

            cx.checkpoint()
                .map_err(|err| alert_check_cancelled_error(format!("rule '{}': {err}", rule.id)))?;

            let current_value =
                Self::get_current_value_with_cx(cx, storage, rule, now, None).await?;
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

    /// Cx-first storage query for the current value of a metric.
    ///
    /// Routes every storage call through its cx-first sibling
    /// (`aggregate_usage_metric_with_cx`, `get_accounts_by_service_with_cx`)
    /// so alert evaluation under a cancelled parent cx bails at the
    /// earliest boundary rather than completing a full metric scan.
    async fn get_current_value_with_cx(
        cx: &crate::cx::Cx,
        storage: &StorageHandle,
        rule: &AlertRule,
        now: i64,
        pane_id: Option<u64>,
    ) -> crate::error::Result<f64> {
        let since = now.saturating_sub(rule.period.duration_ms()).max(0);

        match rule.metric {
            AlertMetric::Cost | AlertMetric::TokenUsage | AlertMetric::RateLimitFrequency => {
                let metric_type = match rule.metric {
                    AlertMetric::Cost => MetricType::ApiCost,
                    AlertMetric::TokenUsage => MetricType::TokenUsage,
                    AlertMetric::RateLimitFrequency => MetricType::RateLimitHit,
                    AlertMetric::AccountBalance => unreachable!(),
                };
                let query = crate::storage::MetricQuery {
                    metric_type: Some(metric_type),
                    agent_type: rule.agent_type.clone(),
                    account_id: rule.account_id.clone(),
                    since: Some(since),
                    until: Some(now.saturating_add(1)),
                    ..Default::default()
                };
                storage
                    .aggregate_usage_metric_with_cx(cx, query, pane_id)
                    .await
            }
            AlertMetric::AccountBalance => {
                // Accounts are stored under caut's canonical service name, so
                // an alias such as "claude" must look up "anthropic".
                let service = match rule.service.as_deref() {
                    Some(s) => CautService::from_cli_input(s)
                        .map_or_else(|| s.to_string(), |service| service.as_str().to_string()),
                    None => return Ok(100.0),
                };
                if let Some(account_id) = rule.account_id.as_deref() {
                    let account = storage
                        .get_account_with_cx(cx, &service, account_id)
                        .await?;
                    // caut reports no percentage for some accounts (an auth
                    // failure, for example). That is missing evidence, not 0%.
                    if let Some(account) = account
                        && let Some(percent) = account.percent_remaining
                    {
                        if pane_id.is_some()
                            && (!percent.is_finite()
                                || !(0.0..=100.0).contains(&percent)
                                || account.last_refreshed_at > now
                                || now.saturating_sub(account.last_refreshed_at)
                                    > ACCOUNT_BALANCE_MAX_AGE_MS)
                        {
                            return Err(alert_evaluation_unavailable(
                                "account evidence is stale or invalid",
                            ));
                        }
                        return Ok(percent);
                    }
                    return if pane_id.is_some() {
                        Err(alert_evaluation_unavailable(
                            "configured account is unavailable or reports no balance",
                        ))
                    } else {
                        Ok(100.0)
                    };
                }
                let accounts = storage
                    .get_accounts_by_service_with_cx(cx, &service)
                    .await?;
                let known: Vec<f64> = accounts
                    .iter()
                    .filter_map(|a| a.percent_remaining)
                    .collect();
                if known.is_empty() {
                    if pane_id.is_some() && !accounts.is_empty() {
                        return Err(alert_evaluation_unavailable(
                            "no account of the service reports a balance",
                        ));
                    }
                    Ok(100.0)
                } else {
                    Ok(known.iter().sum::<f64>() / known.len() as f64)
                }
            }
        }
    }
}

fn alert_evaluation_unavailable(reason: &str) -> crate::Error {
    crate::StorageError::Database(format!("usage alert unavailable: {reason}")).into()
}

/// One bounded evaluation turn. Unavailable evidence does not clear an alert.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WatcherAlertSweep {
    pub emitted: usize,
    pub unavailable: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatcherAlertState {
    version: u8,
    // Numeric metadata survives configured string redaction. These are not
    // operator text and must not become a different rule after persistence.
    config_sha256: [u8; 32],
    level: Option<u8>,
}

fn alert_level_code(level: AlertLevel) -> u8 {
    match level {
        AlertLevel::Info => 1,
        AlertLevel::Warning => 2,
        AlertLevel::Critical => 3,
        AlertLevel::Exceeded => 4,
    }
}

fn alert_level_from_code(code: u8) -> crate::Result<AlertLevel> {
    match code {
        1 => Ok(AlertLevel::Info),
        2 => Ok(AlertLevel::Warning),
        3 => Ok(AlertLevel::Critical),
        4 => Ok(AlertLevel::Exceeded),
        _ => Err(alert_evaluation_unavailable("prior alert level is invalid")),
    }
}

/// Evaluates configured rules and records transitions before publishing them
/// through the watcher's ordinary detection/notification pipeline. The newest
/// durable event carries the state, so restart preserves cooldown and a
/// recorded recovery permits an identical later alert to start a new episode.
pub struct WatcherAlertMonitor {
    config: WatcherAlertConfig,
    /// The unavailable reason last warned per rule ID, or per account refresh
    /// key (which contains a space, so it never collides with a rule ID). A
    /// persistent condition, such as a caut that is not installed, warns once
    /// per change instead of on every sweep.
    unavailable_warned: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// Refreshes the accounts named by account_balance rules.
    caut: CautClient,
    /// When caut last ran, per canonical service name.
    account_refresh_attempts: std::sync::Mutex<std::collections::HashMap<&'static str, i64>>,
}

impl WatcherAlertMonitor {
    pub fn new(config: WatcherAlertConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            unavailable_warned: std::sync::Mutex::new(std::collections::HashMap::new()),
            caut: CautClient::new(),
            account_refresh_attempts: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    #[cfg(test)]
    fn with_caut_client(mut self, caut: CautClient) -> Self {
        self.caut = caut;
        self
    }

    /// Keeps the accounts named by enabled account_balance rules fresh.
    ///
    /// Nothing else in `ft watch` refreshes accounts, and data older than
    /// [`ACCOUNT_BALANCE_MAX_AGE_MS`] is unavailable, so without this a balance
    /// rule stopped working minutes after the last manual `ft accounts
    /// refresh`. caut runs under the sweep's Cx (cancelling it kills caut) and
    /// its own timeout, at most once per service per
    /// [`ACCOUNT_REFRESH_AFTER_MS`]. A failed refresh leaves the stored record
    /// to age out, so the rule reports unavailable instead of guessing.
    async fn refresh_balance_accounts_with_cx(
        &self,
        cx: &crate::cx::Cx,
        storage: &StorageHandle,
        now: i64,
    ) -> crate::Result<()> {
        let mut wanted: Vec<(CautService, Vec<&str>)> = Vec::new();
        for rule in self
            .config
            .rules
            .iter()
            .filter(|rule| rule.enabled && rule.metric == AlertMetric::AccountBalance)
        {
            let (Some(service), Some(account_id)) = (
                rule.service
                    .as_deref()
                    .and_then(CautService::from_cli_input),
                rule.account_id.as_deref(),
            ) else {
                continue;
            };
            match wanted.iter_mut().find(|(known, _)| *known == service) {
                Some((_, account_ids)) => account_ids.push(account_id),
                None => wanted.push((service, vec![account_id])),
            }
        }
        for (service, account_ids) in wanted {
            cx.checkpoint()
                .map_err(|error| alert_check_cancelled_error(error.to_string()))?;
            let mut stale = false;
            for account_id in account_ids {
                let refreshed_at = storage
                    .get_account_with_cx(cx, service.as_str(), account_id)
                    .await?
                    .map_or(i64::MIN, |account| account.last_refreshed_at);
                stale |= now.saturating_sub(refreshed_at) >= ACCOUNT_REFRESH_AFTER_MS;
            }
            if !stale {
                continue;
            }
            {
                let mut attempts = self
                    .account_refresh_attempts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if attempts
                    .get(service.as_str())
                    .is_some_and(|at| now.saturating_sub(*at) < ACCOUNT_REFRESH_AFTER_MS)
                {
                    continue;
                }
                attempts.insert(service.as_str(), now);
            }
            let warn_key = format!("account refresh {}", service.as_str());
            match self.caut.refresh_cx(cx, service).await {
                Ok(refresh) => {
                    let mut stored = 0_usize;
                    for usage in &refresh.accounts {
                        // An entry without an ID or name matches no rule. One
                        // without a percentage (an expired login, for example)
                        // is stored as an unknown balance, which no rule
                        // reads as 0%.
                        if usage.id.is_none() && usage.name.is_none() {
                            continue;
                        }
                        let record = crate::accounts::AccountRecord::from_caut(usage, service, now);
                        storage.upsert_account_with_cx(cx, record).await?;
                        stored += 1;
                    }
                    if self.note_available(&warn_key) {
                        tracing::info!(
                            service = service.as_str(),
                            "Usage alert account refresh available again"
                        );
                    }
                    tracing::debug!(
                        service = service.as_str(),
                        reported = refresh.accounts.len(),
                        stored,
                        "Usage alert account refresh completed"
                    );
                }
                Err(error) => {
                    cx.checkpoint()
                        .map_err(|error| alert_check_cancelled_error(error.to_string()))?;
                    if self.note_unavailable(&warn_key, &error.to_string()) {
                        tracing::warn!(service = service.as_str(), %error,
                            "Usage alert account refresh failed; balance rules report \
                             unavailable once their data is stale");
                    } else {
                        tracing::debug!(service = service.as_str(), %error,
                            "Usage alert account refresh still failing");
                    }
                }
            }
        }
        Ok(())
    }

    /// Records an unavailable evaluation. Returns true when the reason is new
    /// for this rule and deserves a warning.
    fn note_unavailable(&self, rule_id: &str, reason: &str) -> bool {
        self.unavailable_warned.lock().map_or(true, |mut warned| {
            warned
                .insert(rule_id.to_string(), reason.to_string())
                .as_deref()
                != Some(reason)
        })
    }

    /// Records a successful evaluation. Returns true when the rule had been
    /// reported unavailable.
    fn note_available(&self, rule_id: &str) -> bool {
        self.unavailable_warned
            .lock()
            .is_ok_and(|mut warned| warned.remove(rule_id).is_some())
    }

    pub async fn evaluate_with_cx(
        &self,
        cx: &crate::cx::Cx,
        storage: &StorageHandle,
        event_bus: &crate::events::EventBus,
    ) -> crate::Result<WatcherAlertSweep> {
        self.evaluate_at_with_cx(cx, storage, event_bus, now_ms())
            .await
    }

    async fn evaluate_at_with_cx(
        &self,
        cx: &crate::cx::Cx,
        storage: &StorageHandle,
        event_bus: &crate::events::EventBus,
        now: i64,
    ) -> crate::Result<WatcherAlertSweep> {
        let mut sweep = WatcherAlertSweep::default();
        if !self.config.enabled {
            return Ok(sweep);
        }
        match self
            .refresh_balance_accounts_with_cx(cx, storage, now)
            .await
        {
            Ok(()) => {
                self.note_available("account refresh");
            }
            Err(error) => {
                // Each balance rule still evaluates, and reports unavailable
                // if its data has aged out.
                cx.checkpoint()
                    .map_err(|error| alert_check_cancelled_error(error.to_string()))?;
                if self.note_unavailable("account refresh", &error.to_string()) {
                    tracing::warn!(%error, "Usage alert account refresh unavailable");
                }
            }
        }
        for rule in self.config.rules.iter().filter(|rule| rule.enabled) {
            cx.checkpoint()
                .map_err(|error| alert_check_cancelled_error(error.to_string()))?;
            match self
                .evaluate_rule_with_cx(cx, storage, event_bus, rule, now)
                .await
            {
                Ok(emitted) => {
                    sweep.emitted += usize::from(emitted);
                    if self.note_available(&rule.id) {
                        tracing::info!(rule = %rule.id, pane_id = rule.pane_id,
                            "Usage alert evaluation available again");
                    }
                }
                Err(error) => {
                    cx.checkpoint()
                        .map_err(|error| alert_check_cancelled_error(error.to_string()))?;
                    sweep.unavailable += 1;
                    if self.note_unavailable(&rule.id, &error.to_string()) {
                        tracing::warn!(rule = %rule.id, pane_id = rule.pane_id, %error,
                            "Usage alert evaluation unavailable; retaining prior state \
                             (repeats are logged at debug)");
                    } else {
                        tracing::debug!(rule = %rule.id, pane_id = rule.pane_id, %error,
                            "Usage alert evaluation still unavailable");
                    }
                }
            }
        }
        Ok(sweep)
    }

    async fn evaluate_rule_with_cx(
        &self,
        cx: &crate::cx::Cx,
        storage: &StorageHandle,
        event_bus: &crate::events::EventBus,
        configured: &WatcherAlertRule,
        now: i64,
    ) -> crate::Result<bool> {
        use crate::patterns::{AgentType, Detection, Severity};
        use crate::storage::{EventQuery, StoredEvent};
        use sha2::{Digest, Sha256};

        let pane = storage
            .get_pane_with_cx(cx, configured.pane_id)
            .await?
            .filter(|pane| pane.observed)
            .ok_or_else(|| alert_evaluation_unavailable("configured pane is not observed"))?;
        let config_json = serde_json::to_vec(configured)
            .map_err(|_| alert_evaluation_unavailable("rule serialization failed"))?;
        let config_sha256: [u8; 32] = Sha256::digest(&config_json).into();
        let event_rule_id = format!("wezterm.usage_alert.{}", configured.id);
        let previous = storage
            .get_events_with_cx(
                cx,
                EventQuery {
                    pane_id: Some(configured.pane_id),
                    rule_id: Some(event_rule_id.clone()),
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .await?
            .into_iter()
            .next();
        let previous_id = previous.as_ref().map_or(0, |event| event.id);
        let mut previous_level = None;
        let mut same_config = true;
        if let Some(previous) = &previous {
            if previous.detected_at > now {
                return Err(alert_evaluation_unavailable(
                    "clock precedes prior alert state",
                ));
            }
            let state: WatcherAlertState = previous
                .extracted
                .as_ref()
                .and_then(|extracted| extracted.get("alert_state"))
                .and_then(|state| serde_json::from_value(state.clone()).ok())
                .filter(|state: &WatcherAlertState| state.version == 1)
                .ok_or_else(|| alert_evaluation_unavailable("prior alert state is invalid"))?;
            same_config = state.config_sha256 == config_sha256;
            previous_level = state.level.map(alert_level_from_code).transpose()?;
        }
        let rule = configured.as_rule();
        let current_value = AlertMonitor::get_current_value_with_cx(
            cx,
            storage,
            &rule,
            now,
            Some(configured.pane_id),
        )
        .await?;
        let mut level = rule.check(current_value);
        if same_config
            && let Some(previous) = previous_level
            && level < Some(previous)
        {
            // Hysteresis: fall back only as far as the recovery margin
            // allows, and never above the level already recorded.
            level = rule
                .check_with_recovery_margin(current_value)
                .min(Some(previous))
                .max(level);
        }
        if level.is_none() && previous_level.is_none() {
            return Ok(false);
        }
        if same_config
            && level == previous_level
            && previous.as_ref().is_some_and(|event| {
                now.saturating_sub(event.detected_at)
                    < i64::try_from(self.config.repeat_after_secs * 1000).unwrap_or(i64::MAX)
            })
        {
            return Ok(false);
        }
        let (event_type, severity, summary) = if let Some(level) = level {
            let alert = TriggeredAlert {
                rule_id: rule.id.clone(),
                metric: rule.metric.clone(),
                level,
                current_value,
                threshold: rule.threshold,
                percent_of_threshold: if rule.metric == AlertMetric::AccountBalance {
                    1.0 - current_value / rule.threshold
                } else {
                    current_value / rule.threshold
                },
                period: rule.period,
                evaluated_at: now,
            };
            let severity = match level {
                AlertLevel::Info => Severity::Info,
                AlertLevel::Warning => Severity::Warning,
                AlertLevel::Critical | AlertLevel::Exceeded => Severity::Critical,
            };
            ("usage.alert", severity, alert.summary())
        } else {
            (
                "usage.recovered",
                Severity::Info,
                format!(
                    "{} recovered: {} is {:.2}",
                    rule.id, rule.metric, current_value
                ),
            )
        };
        let state = WatcherAlertState {
            version: 1,
            config_sha256,
            level: level.map(alert_level_code),
        };
        let extracted = serde_json::json!({
            "alert_name": rule.id,
            "summary": summary,
            "metric": rule.metric.as_str(),
            "current_value": current_value,
            "threshold": rule.threshold,
            "period": configured.period.map(AlertPeriod::as_str),
            "previous_event_id": previous_id,
            "alert_state": state,
        });
        let detection = Detection {
            rule_id: event_rule_id,
            agent_type: AgentType::Wezterm,
            event_type: event_type.to_string(),
            severity,
            confidence: 1.0,
            extracted,
            matched_text: String::new(),
            span: (0, 0),
        };
        let stored = StoredEvent {
            id: 0,
            pane_id: configured.pane_id,
            rule_id: detection.rule_id.clone(),
            agent_type: detection.agent_type.to_string(),
            event_type: detection.event_type.clone(),
            severity: match severity {
                Severity::Info => "info",
                Severity::Warning => "warning",
                Severity::Critical => "critical",
            }
            .to_string(),
            confidence: detection.confidence,
            extracted: Some(detection.extracted.clone()),
            matched_text: None,
            segment_id: None,
            detected_at: now,
            dedupe_key: Some(format!(
                "usage_alert:{}:{previous_id}:{}",
                hex::encode(config_sha256),
                level.map_or("recovered", AlertLevel::as_str),
            )),
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        let outcome = storage.record_event_outcome_with_cx(cx, stored).await?;
        if let Some(event_id) = outcome.inserted_event_id() {
            // The event row is already durable. The bus only feeds live
            // subscribers, so delivering to none of them is not a failure.
            let _ = event_bus.publish(crate::events::Event::PatternDetected {
                pane_id: configured.pane_id,
                pane_uuid: pane.pane_uuid,
                detection,
                event_id: Some(event_id),
            });
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
fn sum_amounts(records: &[UsageMetricRecord]) -> f64 {
    records.iter().filter_map(|r| r.amount).sum()
}

#[cfg(test)]
fn sum_tokens(records: &[UsageMetricRecord]) -> i64 {
    records.iter().filter_map(|r| r.tokens).sum()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watcher_config() -> WatcherAlertConfig {
        WatcherAlertConfig {
            enabled: true,
            check_interval_secs: 5,
            repeat_after_secs: 60,
            rules: vec![WatcherAlertRule {
                pane_id: 7,
                id: "build-budget".to_string(),
                metric: AlertMetric::Cost,
                threshold: 100.0,
                period: Some(AlertPeriod::Day),
                agent_type: Some("claude_code".to_string()),
                account_id: None,
                service: None,
                enabled: true,
            }],
        }
    }

    async fn watcher_storage() -> (tempfile::TempDir, StorageHandle) {
        watcher_storage_with_config(crate::storage::StorageConfig::default()).await
    }

    async fn watcher_storage_with_config(
        config: crate::storage::StorageConfig,
    ) -> (tempfile::TempDir, StorageHandle) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("alerts.db");
        let storage = StorageHandle::with_config(path.to_str().unwrap(), config)
            .await
            .unwrap();
        storage
            .upsert_pane(crate::storage::PaneRecord {
                pane_id: 7,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: Some(1),
                tab_id: Some(1),
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .unwrap();
        (directory, storage)
    }

    fn watcher_cost(timestamp: i64, amount: f64) -> UsageMetricRecord {
        UsageMetricRecord {
            id: 0,
            timestamp,
            metric_type: MetricType::ApiCost,
            pane_id: Some(7),
            agent_type: Some("claude_code".to_string()),
            // As in production: pane-scoped usage rows carry no account.
            account_id: None,
            workflow_id: None,
            count: None,
            amount: Some(amount),
            tokens: None,
            metadata: None,
            created_at: timestamp,
        }
    }

    #[test]
    fn watcher_alert_config_requires_explicit_bounded_valid_scopes() {
        let text = r#"
[alerts]
enabled = true
check_interval_secs = 5
repeat_after_secs = 60
[[alerts.rules]]
id = "tokens"
pane_id = 7
metric = "token_usage"
threshold = 100000
period = "day"
"#;
        let config = crate::config::Config::from_toml(text).unwrap();
        assert!(config.alerts.enabled);
        assert!(config.alerts.rules[0].enabled);
        assert!(crate::config::Config::from_toml(&text.replace("pane_id = 7", "")).is_err());
        assert!(
            crate::config::Config::from_toml(
                &text.replace("threshold = 100000", "threshold = nan")
            )
            .is_err()
        );
        assert!(
            crate::config::Config::from_toml(
                &text.replace("metric = \"token_usage\"", "metric = \"unknown\"")
            )
            .is_err()
        );
        assert!(!crate::config::Config::default().alerts.enabled);
        assert!(
            crate::config::Config::default()
                .diff_for_hot_reload(&config)
                .forbidden
                .iter()
                .any(|change| change.name == "alerts")
        );

        let mut invalid = watcher_config();
        invalid.rules.push(invalid.rules[0].clone());
        assert!(invalid.validate().is_err());
        invalid = watcher_config();
        invalid.rules[0].pane_id = u64::MAX;
        assert!(invalid.validate().is_err());
        invalid = watcher_config();
        invalid.rules[0].metric = AlertMetric::AccountBalance;
        assert!(
            invalid.validate().is_err(),
            "account service must be explicit"
        );
        invalid.rules[0].service = Some("anthropic".to_string());
        invalid.rules[0].agent_type = None;
        invalid.rules[0].account_id = Some("build".to_string());
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .contains("does not accept period"),
            "a balance has no window, so a period would only perturb its config hash"
        );
        invalid.rules[0].period = None;
        assert!(invalid.validate().is_ok());
        invalid.rules[0].service = Some("claude".to_string());
        assert!(invalid.validate().is_ok(), "caut's aliases are accepted");
        invalid.rules[0].service = Some("anthropic-api".to_string());
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .contains("service must be one of"),
            "no stored or refreshed account could ever match another name"
        );
        invalid.rules[0].service = Some("anthropic".to_string());
        invalid.rules[0].account_id = None;
        assert!(invalid.validate().is_err(), "account ID must be explicit");
        // Pane usage rows carry no account, so this filter could never match.
        invalid = watcher_config();
        invalid.rules[0].account_id = Some("build".to_string());
        assert!(invalid.validate().is_err(), "a pane total takes no account");
        invalid = watcher_config();
        invalid.rules[0].period = None;
        assert!(
            invalid.validate().unwrap_err().contains("requires period"),
            "a pane total needs its window"
        );
    }

    /// A balance rule is written without `period`, and a total keeps the exact
    /// serialized form it had when `period` was not optional, so the config
    /// hash of an already-recorded alert does not change.
    #[test]
    fn watcher_alert_period_is_optional_only_where_it_is_unused() {
        let text = r#"
[alerts]
enabled = true
[[alerts.rules]]
id = "balance"
pane_id = 7
metric = "account_balance"
threshold = 20.0
service = "anthropic"
account_id = "my-account"
"#;
        let config = crate::config::Config::from_toml(text).unwrap();
        assert_eq!(config.alerts.rules[0].period, None);
        let with_period = text.replace("threshold = 20.0", "threshold = 20.0\nperiod = \"day\"");
        assert!(crate::config::Config::from_toml(&with_period).is_err());

        let total = serde_json::to_value(&watcher_config().rules[0]).unwrap();
        assert_eq!(
            total["period"],
            serde_json::to_value(AlertPeriod::Day).unwrap(),
            "Some(period) must hash exactly as the bare period did"
        );
    }

    #[test]
    fn a_persistent_unavailable_reason_warns_once_per_change() {
        let monitor = WatcherAlertMonitor::new(watcher_config()).unwrap();
        assert!(monitor.note_unavailable("build-budget", "account evidence is stale"));
        // The same condition on later sweeps does not warn again.
        assert!(!monitor.note_unavailable("build-budget", "account evidence is stale"));
        assert!(!monitor.note_unavailable("build-budget", "account evidence is stale"));
        // Another rule, or a different reason, does.
        assert!(monitor.note_unavailable("other-rule", "account evidence is stale"));
        assert!(monitor.note_unavailable("build-budget", "configured pane is not observed"));
        // Recovery is reported once, and a later failure warns afresh.
        assert!(monitor.note_available("build-budget"));
        assert!(!monitor.note_available("build-budget"));
        assert!(monitor.note_unavailable("build-budget", "configured pane is not observed"));
    }

    #[test]
    fn watcher_alerts_persist_scoped_episodes_restart_cooldown_and_retrigger() {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_directory, storage) = watcher_storage().await;
            let cx = crate::cx::Cx::for_testing();
            let bus = crate::events::EventBus::new(16);
            let mut subscriber = bus.subscribe_detections();
            let day = AlertPeriod::Day.duration_ms();
            let now = day * 2;
            let mut other_pane = watcher_cost(now, 500.0);
            other_pane.pane_id = Some(8);
            let mut other_agent = watcher_cost(now, 500.0);
            other_agent.agent_type = Some("codex".to_string());
            storage
                .record_usage_metrics_batch(vec![
                    watcher_cost(now - 1, 80.0),
                    watcher_cost(now - day - 1, 500.0),
                    watcher_cost(now + 1, 500.0),
                    other_pane,
                    other_agent,
                ])
                .await
                .unwrap();
            let monitor = WatcherAlertMonitor::new(watcher_config()).unwrap();
            assert_eq!(
                monitor
                    .evaluate_at_with_cx(&cx, &storage, &bus, now)
                    .await
                    .unwrap()
                    .emitted,
                1
            );
            let first = match subscriber.try_recv().unwrap().unwrap() {
                crate::events::Event::PatternDetected {
                    pane_id, detection, ..
                } => {
                    assert_eq!(pane_id, 7);
                    assert_eq!(detection.severity, crate::patterns::Severity::Warning);
                    assert_eq!(detection.extracted["current_value"], 80.0);
                    detection
                }
                other => panic!("unexpected event: {other:?}"),
            };
            let events = storage
                .get_events(crate::storage::EventQuery::default())
                .await
                .unwrap();
            assert_eq!(events.len(), 1);
            let rendered = crate::event_templates::render_event(&events[0]);
            assert!(rendered.summary.contains("$80.00 / $100.00"));
            assert!(!rendered.summary.contains("Unknown event"));

            // A fresh monitor has no in-memory cooldown to rely on. The exact
            // same sample remains below its durable reminder deadline.
            let restarted = WatcherAlertMonitor::new(watcher_config()).unwrap();
            assert_eq!(
                restarted
                    .evaluate_at_with_cx(&cx, &storage, &bus, now)
                    .await
                    .unwrap()
                    .emitted,
                0
            );
            assert!(subscriber.try_recv().is_none());

            let recovered_at = now + day + 2;
            assert_eq!(
                restarted
                    .evaluate_at_with_cx(&cx, &storage, &bus, recovered_at)
                    .await
                    .unwrap()
                    .emitted,
                1
            );
            match subscriber.try_recv().unwrap().unwrap() {
                crate::events::Event::PatternDetected { detection, .. } => {
                    assert_eq!(detection.event_type, "usage.recovered");
                }
                other => panic!("unexpected event: {other:?}"),
            }
            storage
                .record_usage_metric(watcher_cost(recovered_at + 1, 80.0))
                .await
                .unwrap();
            assert_eq!(
                restarted
                    .evaluate_at_with_cx(&cx, &storage, &bus, recovered_at + 1)
                    .await
                    .unwrap()
                    .emitted,
                1
            );
            let second = match subscriber.try_recv().unwrap().unwrap() {
                crate::events::Event::PatternDetected { detection, .. } => detection,
                other => panic!("unexpected event: {other:?}"),
            };
            assert_ne!(
                crate::events::event_identity_key(&first, 7, None),
                crate::events::event_identity_key(&second, 7, None),
                "a real new episode must also survive the notification deduper"
            );
            assert_eq!(
                restarted
                    .evaluate_at_with_cx(&cx, &storage, &bus, recovered_at + 60_000)
                    .await
                    .unwrap()
                    .emitted,
                0
            );
            assert_eq!(
                restarted
                    .evaluate_at_with_cx(&cx, &storage, &bus, recovered_at + 60_001)
                    .await
                    .unwrap()
                    .emitted,
                1
            );
            assert_eq!(
                storage
                    .get_events(crate::storage::EventQuery::default())
                    .await
                    .unwrap()
                    .len(),
                4
            );
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn watcher_alerts_refuse_missing_stale_corrupt_and_cancelled_evidence() {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (directory, storage) = watcher_storage().await;
            let cx = crate::cx::Cx::for_testing();
            let bus = crate::events::EventBus::new(16);
            let now = 200_000_000;
            let mut config = watcher_config();
            config.rules[0].metric = AlertMetric::AccountBalance;
            config.rules[0].threshold = 20.0;
            config.rules[0].period = None;
            config.rules[0].agent_type = None;
            config.rules[0].service = Some("anthropic".to_string());
            config.rules[0].account_id = Some("build".to_string());
            // A caut that cannot run leaves only the evidence written below.
            let monitor = WatcherAlertMonitor::new(config).unwrap().with_caut_client(
                CautClient::new().with_binary("/ft-un0d6b/nonexistent-caut-binary"),
            );
            assert_eq!(
                monitor
                    .evaluate_at_with_cx(&cx, &storage, &bus, now)
                    .await
                    .unwrap()
                    .unavailable,
                1
            );
            let mut account = crate::accounts::AccountRecord {
                id: 0,
                account_id: "build".to_string(),
                service: "anthropic".to_string(),
                name: None,
                percent_remaining: Some(10.0),
                reset_at: None,
                tokens_used: None,
                tokens_remaining: None,
                tokens_limit: None,
                last_refreshed_at: now,
                last_used_at: None,
                created_at: now,
                updated_at: now,
            };
            storage.upsert_account(account.clone()).await.unwrap();
            assert_eq!(
                monitor
                    .evaluate_at_with_cx(&cx, &storage, &bus, now)
                    .await
                    .unwrap()
                    .emitted,
                1
            );
            for unavailable_at in [now - 1, now + 900_001] {
                assert_eq!(
                    monitor
                        .evaluate_at_with_cx(&cx, &storage, &bus, unavailable_at)
                        .await
                        .unwrap()
                        .unavailable,
                    1
                );
            }
            assert_eq!(
                storage
                    .get_events(crate::storage::EventQuery::default())
                    .await
                    .unwrap()
                    .len(),
                1,
                "stale or regressed evidence must not clear or repeat an alert"
            );
            account.percent_remaining = Some(100.0);
            account.last_refreshed_at = now + 900_002;
            account.updated_at = now + 900_002;
            storage.upsert_account(account).await.unwrap();
            assert_eq!(
                monitor
                    .evaluate_at_with_cx(&cx, &storage, &bus, now + 900_002)
                    .await
                    .unwrap()
                    .emitted,
                1
            );

            let cancelled = crate::cx::Cx::for_testing();
            cancelled.set_cancel_requested(true);
            assert!(
                monitor
                    .evaluate_at_with_cx(&cancelled, &storage, &bus, now + 900_003)
                    .await
                    .is_err()
            );
            let disabled = WatcherAlertMonitor::new(WatcherAlertConfig::default()).unwrap();
            assert_eq!(
                disabled
                    .evaluate_at_with_cx(&cancelled, &storage, &bus, now)
                    .await
                    .unwrap(),
                WatcherAlertSweep::default()
            );
            let connection =
                rusqlite::Connection::open(directory.path().join("alerts.db")).unwrap();
            connection
                .execute("UPDATE events SET extracted = '{}'", [])
                .unwrap();
            assert_eq!(
                monitor
                    .evaluate_at_with_cx(&cx, &storage, &bus, now + 900_003)
                    .await
                    .unwrap()
                    .unavailable,
                1
            );
            drop(connection);
            storage.shutdown().await.unwrap();
        });
    }

    /// ft-un0d6b item 3: a rolling total that slips just under a boundary as an
    /// old row ages out keeps its alert instead of notifying a recovery, and
    /// recovers once it is below the boundary by more than the margin.
    #[test]
    fn watcher_alert_holds_its_level_within_the_recovery_margin() {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_directory, storage) = watcher_storage().await;
            let cx = crate::cx::Cx::for_testing();
            let bus = crate::events::EventBus::new(16);
            let day = AlertPeriod::Day.duration_ms();
            let now = day * 2;
            storage
                .record_usage_metrics_batch(vec![
                    watcher_cost(now - day + 10, 4.0),
                    watcher_cost(now - day + 20, 3.0),
                    watcher_cost(now, 44.0),
                ])
                .await
                .unwrap();
            let monitor = WatcherAlertMonitor::new(watcher_config()).unwrap();
            let sweep = |at| monitor.evaluate_at_with_cx(&cx, &storage, &bus, at);

            // 51% of the threshold: Info.
            assert_eq!(sweep(now).await.unwrap().emitted, 1);
            // 47%: under the 50% boundary, but within the 5% margin.
            assert_eq!(sweep(now + 11).await.unwrap().emitted, 0);
            // 44%: more than the margin below it.
            assert_eq!(sweep(now + 21).await.unwrap().emitted, 1);
            let events = storage
                .get_events(crate::storage::EventQuery::default())
                .await
                .unwrap();
            let types: Vec<_> = events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect();
            assert_eq!(types.len(), 2);
            assert!(types.contains(&"usage.alert") && types.contains(&"usage.recovered"));
            storage.shutdown().await.unwrap();
        });

        // A balance rule's margin runs the other way: it must rise clear of
        // its minimum.
        let balance = AlertRule::account_balance("low", 20.0, None);
        assert_eq!(balance.check(20.5), None);
        assert_eq!(
            balance.check_with_recovery_margin(20.5),
            Some(AlertLevel::Info)
        );
        assert_eq!(balance.check_with_recovery_margin(21.5), None);
    }

    /// ft-un0d6b item 1: the watcher refreshes the accounts its balance rules
    /// name through a real caut process, with no manual `ft accounts refresh`.
    /// It stores no invented 0% for an entry that reports no percentage, and
    /// runs caut at most once per refresh interval per service, even while
    /// caut fails.
    #[cfg(unix)]
    #[test]
    fn watcher_refreshes_balance_accounts_through_caut_without_inventing_balances() {
        use crate::runtime_async::CompatRuntime;
        use std::os::unix::fs::PermissionsExt as _;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (directory, storage) = watcher_storage().await;
            let calls = directory.path().join("caut-calls.log");
            let fake = directory.path().join("caut");
            let write_fake = |body: &str| {
                let script = format!(
                    "#!/bin/sh\necho \"$@\" >> '{}'\n{body}\n",
                    calls.display()
                );
                std::fs::write(&fake, script).unwrap();
                std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            };
            let call_count =
                || std::fs::read_to_string(&calls).map_or(0, |log| log.lines().count());
            write_fake(
                r#"printf '%s' '{"accounts":[{"id":"build","percent_remaining":12.0},{"id":"expired"},{"percent_remaining":50.0}]}'"#,
            );
            let cx = crate::cx::Cx::for_testing();
            let bus = crate::events::EventBus::new(16);
            let now = 200_000_000;
            let mut config = watcher_config();
            config.rules[0].metric = AlertMetric::AccountBalance;
            config.rules[0].threshold = 20.0;
            config.rules[0].period = None;
            config.rules[0].agent_type = None;
            // An alias: caut stores the account under "anthropic".
            config.rules[0].service = Some("claude".to_string());
            config.rules[0].account_id = Some("build".to_string());
            let monitor = WatcherAlertMonitor::new(config).unwrap().with_caut_client(
                CautClient::new().with_binary(fake.to_string_lossy().into_owned()),
            );

            let sweep = monitor
                .evaluate_at_with_cx(&cx, &storage, &bus, now)
                .await
                .unwrap();
            assert_eq!((sweep.emitted, sweep.unavailable), (1, 0));
            assert_eq!(
                std::fs::read_to_string(&calls).unwrap(),
                "usage --provider claude --format json\n"
            );
            let accounts = storage.get_accounts_by_service("anthropic").await.unwrap();
            let stored: Vec<(&str, Option<f64>)> = accounts
                .iter()
                .map(|a| (a.account_id.as_str(), a.percent_remaining))
                .collect();
            assert_eq!(
                stored,
                vec![("build", Some(12.0)), ("expired", None)],
                "an entry without an ID or name is not stored, and one without a \
                 percentage is stored as unknown, not 0%"
            );
            assert_eq!(accounts[0].last_refreshed_at, now);

            // Fresh data needs no caut.
            monitor
                .evaluate_at_with_cx(&cx, &storage, &bus, now + 60_000)
                .await
                .unwrap();
            assert_eq!(call_count(), 1);

            // Once the data is old enough, caut runs again. Its failure leaves
            // the last good record, which is still within its freshness window.
            write_fake("exit 3");
            let later = now + ACCOUNT_REFRESH_AFTER_MS;
            let sweep = monitor
                .evaluate_at_with_cx(&cx, &storage, &bus, later)
                .await
                .unwrap();
            assert_eq!(call_count(), 2);
            assert_eq!(sweep.unavailable, 0);
            // A failing caut is not run again on every sweep.
            monitor
                .evaluate_at_with_cx(&cx, &storage, &bus, later + 60_000)
                .await
                .unwrap();
            assert_eq!(call_count(), 2);
            // When the last good data ages out, the rule reports unavailable
            // instead of the old balance.
            let sweep = monitor
                .evaluate_at_with_cx(&cx, &storage, &bus, now + ACCOUNT_BALANCE_MAX_AGE_MS + 1)
                .await
                .unwrap();
            assert_eq!(call_count(), 3);
            assert_eq!(sweep.unavailable, 1);

            // The login expires: caut now reports the account without a
            // percentage. That fresh record is an unknown balance. The rule
            // reports unavailable; it neither alerts at 0% nor keeps 12%.
            write_fake(r#"printf '%s' '{"accounts":[{"id":"build"}]}'"#);
            let expired_at = now + ACCOUNT_BALANCE_MAX_AGE_MS + 1 + ACCOUNT_REFRESH_AFTER_MS;
            let sweep = monitor
                .evaluate_at_with_cx(&cx, &storage, &bus, expired_at)
                .await
                .unwrap();
            assert_eq!(call_count(), 4);
            assert_eq!((sweep.emitted, sweep.unavailable), (0, 1));
            let build = storage
                .get_account("anthropic", "build")
                .await
                .unwrap()
                .expect("the expired account is still stored");
            assert_eq!(build.percent_remaining, None);
            assert_eq!(build.last_refreshed_at, expired_at);
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn watcher_alert_cooldown_survives_custom_redaction_and_restart() {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut app = crate::config::Config::default();
            app.safety.redaction.patterns =
                vec!["[a-f0-9]{32,}".to_string(), "warning".to_string()];
            let (_directory, storage) =
                watcher_storage_with_config(crate::storage::StorageConfig::from(&app)).await;
            let cx = crate::cx::Cx::for_testing();
            let bus = crate::events::EventBus::new(16);
            let now = 200_000_000;
            storage
                .record_usage_metric(watcher_cost(now, 80.0))
                .await
                .unwrap();
            let config = watcher_config();
            let monitor = WatcherAlertMonitor::new(config.clone()).unwrap();
            assert_eq!(
                monitor
                    .evaluate_at_with_cx(&cx, &storage, &bus, now)
                    .await
                    .unwrap()
                    .emitted,
                1
            );
            let restarted = WatcherAlertMonitor::new(config).unwrap();
            assert_eq!(
                restarted
                    .evaluate_at_with_cx(&cx, &storage, &bus, now + 5_000)
                    .await
                    .unwrap()
                    .emitted,
                0
            );
            let events = storage
                .get_events(crate::storage::EventQuery::default())
                .await
                .unwrap();
            assert_eq!(events.len(), 1);
            let extracted = events[0].extracted.as_ref().unwrap();
            assert!(
                extracted["summary"]
                    .as_str()
                    .unwrap()
                    .contains("[REDACTED]")
            );
            assert_eq!(
                extracted["alert_state"]["config_sha256"]
                    .as_array()
                    .unwrap()
                    .len(),
                32
            );
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn usage_aggregate_refuses_missing_cost_and_token_evidence() {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_directory, storage) = watcher_storage().await;
            let cx = crate::cx::Cx::for_testing();
            let bus = crate::events::EventBus::new(16);
            let monitor = WatcherAlertMonitor::new(watcher_config()).unwrap();
            let now = 200_000_000;
            storage
                .record_usage_metric(watcher_cost(now, 80.0))
                .await
                .unwrap();
            assert_eq!(
                monitor
                    .evaluate_at_with_cx(&cx, &storage, &bus, now)
                    .await
                    .unwrap()
                    .emitted,
                1
            );

            for metric_type in [MetricType::ApiCost, MetricType::TokenUsage] {
                let mut missing = watcher_cost(now, 0.0);
                missing.metric_type = metric_type;
                missing.amount = None;
                missing.tokens = None;
                storage.record_usage_metric(missing).await.unwrap();
                let query = crate::storage::MetricQuery {
                    metric_type: Some(metric_type),
                    since: Some(now - 1),
                    until: Some(now + 1),
                    ..Default::default()
                };
                let error = storage
                    .aggregate_usage_metric_with_cx(&cx, query, Some(7))
                    .await
                    .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("missing, invalid or overflowing")
                );
            }
            let sweep = monitor
                .evaluate_at_with_cx(&cx, &storage, &bus, now + 60_000)
                .await
                .unwrap();
            assert_eq!(sweep.unavailable, 1);
            assert_eq!(sweep.emitted, 0);
            assert_eq!(
                storage
                    .get_events(crate::storage::EventQuery::default())
                    .await
                    .unwrap()
                    .len(),
                1,
                "missing evidence must preserve the active alert"
            );
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn usage_aggregate_is_scalar_bounded_and_counts_recorded_rate_limit_hits() {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (directory, storage) = watcher_storage().await;
            let cx = crate::cx::Cx::for_testing();
            let mut metric = watcher_cost(10, 0.0);
            metric.metric_type = MetricType::RateLimitHit;
            metric.count = Some(7);
            storage.record_usage_metric(metric).await.unwrap();
            let query = crate::storage::MetricQuery {
                metric_type: Some(MetricType::RateLimitHit),
                since: Some(0),
                until: Some(20),
                ..Default::default()
            };
            assert_eq!(
                storage
                    .aggregate_usage_metric_with_cx(&cx, query.clone(), Some(7))
                    .await
                    .unwrap(),
                7.0
            );
            let mut occurrence = watcher_cost(10, 0.0);
            occurrence.metric_type = MetricType::RateLimitHit;
            occurrence.count = None;
            storage.record_usage_metric(occurrence).await.unwrap();
            assert_eq!(
                storage
                    .aggregate_usage_metric_with_cx(&cx, query, Some(7))
                    .await
                    .unwrap(),
                8.0,
                "an uncounted rate-limit record is one occurrence"
            );
            let connection =
                rusqlite::Connection::open(directory.path().join("alerts.db")).unwrap();
            connection
                .execute_batch(
                    "WITH RECURSIVE n(value) AS (
                         SELECT 1 UNION ALL SELECT value + 1 FROM n WHERE value < 100001
                     )
                     INSERT INTO usage_metrics(timestamp, metric_type, pane_id, amount, created_at)
                     SELECT 10, 'api_cost', 7, 1.0, 10 FROM n",
                )
                .unwrap();
            let query = crate::storage::MetricQuery {
                metric_type: Some(MetricType::ApiCost),
                since: Some(0),
                until: Some(20),
                ..Default::default()
            };
            let error = storage
                .aggregate_usage_metric_with_cx(&cx, query, Some(7))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("exceeds 100000"));
            let monitor = WatcherAlertMonitor::new(watcher_config()).unwrap();
            let bus = crate::events::EventBus::new(16);
            let sweep = monitor
                .evaluate_at_with_cx(&cx, &storage, &bus, 10)
                .await
                .unwrap();
            assert_eq!(sweep.unavailable, 1);
            assert_eq!(
                sweep.emitted, 0,
                "source overflow must never publish a partial aggregate"
            );
            assert!(
                storage
                    .get_events(crate::storage::EventQuery::default())
                    .await
                    .unwrap()
                    .is_empty()
            );
            drop(connection);
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn alert_check_cancelled_error_uses_structured_runtime_operation() {
        let error = alert_check_cancelled_error("rule 'budget': cancelled");
        match error {
            crate::Error::RuntimeOperation { operation, source } => {
                assert_eq!(operation, "alerts.check_alerts");
                match source {
                    RuntimeOperationSource::Cancelled(detail) => {
                        assert_eq!(detail, "rule 'budget': cancelled");
                    }
                    other => panic!("unexpected runtime source: {other:?}"),
                }
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// ft-xbnl0.2.3 Cx-first: `check_alerts_with_cx` must match
    /// `check_alerts` on a manager with 0 enabled rules — both
    /// return empty Vec without touching storage. Uses raw
    /// block_on because alerts::tests is sync fixtures.
    #[test]
    fn check_alerts_with_cx_empty_rules_matches_legacy() {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async {
                let db_path =
                    std::env::temp_dir().join(format!("wa_alerts_cx_{}.db", std::process::id()));
                let db_str = db_path.to_string_lossy().to_string();
                let _ = std::fs::remove_file(&db_path);
                let storage = StorageHandle::new(&db_str).await.expect("storage");

                let monitor = AlertMonitor::new(Vec::new()); // no rules
                let cx = crate::cx::for_request();

                let legacy: Vec<TriggeredAlert> =
                    monitor.check_alerts(&storage).await.expect("legacy");
                let cx_first: Vec<TriggeredAlert> = monitor
                    .check_alerts_with_cx(&cx, &storage)
                    .await
                    .expect("cx-first");

                assert_eq!(legacy.len(), cx_first.len());
                assert_eq!(cx_first.len(), 0);

                storage.shutdown().await.expect("shutdown");
                let _ = std::fs::remove_file(&db_path);
            });
        }));
        crate::runtime_async::test_support::finish_test_runtime(runtime, result);
    }

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
    fn alert_level_from_low_watermark_ratio_thresholds() {
        assert_eq!(AlertLevel::from_low_watermark_ratio(1.1), None);
        assert_eq!(AlertLevel::from_low_watermark_ratio(1.0), None);
        assert_eq!(
            AlertLevel::from_low_watermark_ratio(0.99),
            Some(AlertLevel::Info)
        );
        assert_eq!(
            AlertLevel::from_low_watermark_ratio(0.76),
            Some(AlertLevel::Info)
        );
        assert_eq!(
            AlertLevel::from_low_watermark_ratio(0.75),
            Some(AlertLevel::Warning)
        );
        assert_eq!(
            AlertLevel::from_low_watermark_ratio(0.51),
            Some(AlertLevel::Warning)
        );
        assert_eq!(
            AlertLevel::from_low_watermark_ratio(0.5),
            Some(AlertLevel::Critical)
        );
        assert_eq!(
            AlertLevel::from_low_watermark_ratio(0.01),
            Some(AlertLevel::Critical)
        );
        assert_eq!(
            AlertLevel::from_low_watermark_ratio(0.0),
            Some(AlertLevel::Exceeded)
        );
        assert_eq!(
            AlertLevel::from_low_watermark_ratio(-0.1),
            Some(AlertLevel::Exceeded)
        );
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
        // current=20% → still at threshold, not yet below the minimum
        assert_eq!(rule.check(20.0), None);
        // current=19% → just below threshold, info
        assert_eq!(rule.check(19.0), Some(AlertLevel::Info));
        // current=15% → 75% of the minimum remaining, warning
        assert_eq!(rule.check(15.0), Some(AlertLevel::Warning));
        // current=10% → half the minimum remaining, critical
        assert_eq!(rule.check(10.0), Some(AlertLevel::Critical));
        // current=5% → still critical
        assert_eq!(rule.check(5.0), Some(AlertLevel::Critical));
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

    // -----------------------------------------------------------------------
    // Batch 14 — PearlHeron wa-1u90p.7.1
    // -----------------------------------------------------------------------

    // ---- AlertPeriod / AlertMetric parse errors ----

    #[test]
    fn alert_period_parse_unknown_errors() {
        let err: Result<AlertPeriod, _> = "hourly".parse();
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("hourly"));
    }

    #[test]
    fn alert_metric_parse_unknown_errors() {
        let err: Result<AlertMetric, _> = "latency".parse();
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("latency"));
    }

    // ---- AlertLevel display ----

    #[test]
    fn alert_level_as_str_display_consistency() {
        for level in [
            AlertLevel::Info,
            AlertLevel::Warning,
            AlertLevel::Critical,
            AlertLevel::Exceeded,
        ] {
            assert_eq!(level.as_str(), level.to_string());
        }
    }

    // ---- AlertLevel from_percent boundaries ----

    #[test]
    fn alert_level_from_percent_negative() {
        assert_eq!(AlertLevel::from_percent(-0.1), None);
    }

    // ---- TriggeredAlert summary: rate limit ----

    #[test]
    fn triggered_alert_summary_rate_limit() {
        let alert = TriggeredAlert {
            rule_id: "rl-daily".to_string(),
            metric: AlertMetric::RateLimitFrequency,
            level: AlertLevel::Exceeded,
            current_value: 15.0,
            threshold: 10.0,
            percent_of_threshold: 1.5,
            period: AlertPeriod::Day,
            evaluated_at: 0,
        };
        let summary = alert.summary();
        assert!(summary.contains("exceeded"));
        assert!(summary.contains("rate limits"));
        assert!(summary.contains("15"));
        assert!(summary.contains("150%"));
    }

    // ---- TriggeredAlert summary: account balance ----

    #[test]
    fn triggered_alert_summary_account_balance() {
        let alert = TriggeredAlert {
            rule_id: "bal-low".to_string(),
            metric: AlertMetric::AccountBalance,
            level: AlertLevel::Critical,
            current_value: 5.0,
            threshold: 20.0,
            percent_of_threshold: 0.75,
            period: AlertPeriod::Day,
            evaluated_at: 0,
        };
        let summary = alert.summary();
        assert!(summary.contains("critical"));
        assert!(summary.contains("balance"));
        assert!(summary.contains("5.0%"));
        assert!(summary.contains("20.0%"));
    }

    // ---- AlertRule constructors set expected fields ----

    #[test]
    fn alert_rule_cost_constructor() {
        let rule = AlertRule::cost("c1", 100.0, AlertPeriod::Week);
        assert_eq!(rule.id, "c1");
        assert_eq!(rule.metric, AlertMetric::Cost);
        assert!((rule.threshold - 100.0).abs() < f64::EPSILON);
        assert_eq!(rule.period, AlertPeriod::Week);
        assert!(rule.agent_type.is_none());
        assert!(rule.account_id.is_none());
        assert!(rule.enabled);
    }

    #[test]
    fn alert_rule_token_usage_constructor() {
        let rule = AlertRule::token_usage("t1", 50_000.0, AlertPeriod::Month);
        assert_eq!(rule.metric, AlertMetric::TokenUsage);
        assert_eq!(rule.period, AlertPeriod::Month);
    }

    #[test]
    fn alert_rule_rate_limit_constructor() {
        let rule = AlertRule::rate_limit("r1", 20.0, AlertPeriod::Day);
        assert_eq!(rule.metric, AlertMetric::RateLimitFrequency);
        assert!((rule.threshold - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn alert_rule_account_balance_constructor() {
        let rule = AlertRule::account_balance("b1", 10.0, Some("acct-1".to_string()));
        assert_eq!(rule.metric, AlertMetric::AccountBalance);
        assert_eq!(rule.account_id.as_deref(), Some("acct-1"));
    }

    // ---- AlertMetric / AlertPeriod serde roundtrip ----

    #[test]
    fn alert_metric_serde_roundtrip() {
        for metric in [
            AlertMetric::Cost,
            AlertMetric::TokenUsage,
            AlertMetric::RateLimitFrequency,
            AlertMetric::AccountBalance,
        ] {
            let json = serde_json::to_string(&metric).unwrap();
            let restored: AlertMetric = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, metric);
        }
    }

    #[test]
    fn alert_period_serde_roundtrip() {
        for period in [AlertPeriod::Day, AlertPeriod::Week, AlertPeriod::Month] {
            let json = serde_json::to_string(&period).unwrap();
            let restored: AlertPeriod = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, period);
        }
    }

    // ---- AlertLevel serde roundtrip ----

    #[test]
    fn alert_level_serde_roundtrip() {
        for level in [
            AlertLevel::Info,
            AlertLevel::Warning,
            AlertLevel::Critical,
            AlertLevel::Exceeded,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            let restored: AlertLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, level);
        }
    }

    // ---- Negative threshold ----

    #[test]
    fn negative_threshold_never_triggers() {
        let rule = AlertRule::cost("neg", -10.0, AlertPeriod::Day);
        assert_eq!(rule.check(100.0), None);
    }

    // ---- sum helpers ----

    #[test]
    fn sum_amounts_empty() {
        assert!((sum_amounts(&[]) - 0.0).abs() < f64::EPSILON);
    }

    fn make_usage_record(
        id: i64,
        metric_type: MetricType,
        amount: Option<f64>,
        tokens: Option<i64>,
    ) -> UsageMetricRecord {
        UsageMetricRecord {
            id,
            timestamp: 0,
            metric_type,
            pane_id: None,
            agent_type: None,
            account_id: None,
            workflow_id: None,
            count: None,
            amount,
            tokens,
            metadata: None,
            created_at: 0,
        }
    }

    #[test]
    fn sum_amounts_with_nones() {
        let records = vec![
            make_usage_record(0, MetricType::ApiCost, Some(1.5), None),
            make_usage_record(1, MetricType::ApiCost, None, None),
            make_usage_record(2, MetricType::ApiCost, Some(2.5), None),
        ];
        assert!((sum_amounts(&records) - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn sum_tokens_with_nones() {
        let records = vec![
            make_usage_record(0, MetricType::TokenUsage, None, Some(100)),
            make_usage_record(1, MetricType::TokenUsage, None, None),
            make_usage_record(2, MetricType::TokenUsage, None, Some(250)),
        ];
        assert_eq!(sum_tokens(&records), 350);
    }

    // ---- now_ms sanity ----

    #[test]
    fn now_ms_is_positive() {
        assert!(now_ms() > 0);
    }
}
