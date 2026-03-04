//! Connector-aware rate-limit, quota, and cost governance for outbound actions.
//!
//! Provides per-connector and global rate limiting (token bucket), quota/budget
//! enforcement, memory-aware queue backpressure, and adaptive backoff to prevent
//! thundering-herd retries and cost explosions under connector storms.
//!
//! Part of ft-3681t.5.11.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::connector_outbound_bridge::{ConnectorAction, ConnectorActionKind};

// =============================================================================
// Governor decision
// =============================================================================

/// Outcome of a governor evaluation for a connector action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovernorVerdict {
    /// Action is allowed to proceed immediately.
    Allow,
    /// Action should be delayed (includes recommended delay).
    Throttle,
    /// Action is rejected — quota exhausted or budget exceeded.
    Reject,
}

impl std::fmt::Display for GovernorVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allow => f.write_str("allow"),
            Self::Throttle => f.write_str("throttle"),
            Self::Reject => f.write_str("reject"),
        }
    }
}

/// Reason a governor decision was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovernorReason {
    /// All limits clear.
    Clear,
    /// Per-connector rate limit hit.
    ConnectorRateLimit,
    /// Global rate limit hit.
    GlobalRateLimit,
    /// Per-connector quota exhausted.
    ConnectorQuotaExhausted,
    /// Global quota exhausted.
    GlobalQuotaExhausted,
    /// Cost budget exceeded.
    BudgetExceeded,
    /// Queue backpressure too high.
    Backpressure,
    /// Adaptive backoff active for this connector.
    AdaptiveBackoff,
}

impl std::fmt::Display for GovernorReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Clear => "clear",
            Self::ConnectorRateLimit => "connector_rate_limit",
            Self::GlobalRateLimit => "global_rate_limit",
            Self::ConnectorQuotaExhausted => "connector_quota_exhausted",
            Self::GlobalQuotaExhausted => "global_quota_exhausted",
            Self::BudgetExceeded => "budget_exceeded",
            Self::Backpressure => "backpressure",
            Self::AdaptiveBackoff => "adaptive_backoff",
        };
        f.write_str(s)
    }
}

/// Full governor decision with verdict, reason, and recommended delay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernorDecision {
    /// The verdict: allow, throttle, or reject.
    pub verdict: GovernorVerdict,
    /// Why this decision was made.
    pub reason: GovernorReason,
    /// Recommended delay before dispatch (zero for Allow).
    pub delay_ms: u64,
    /// Which connector this decision applies to.
    pub connector_id: String,
    /// Action kind that was evaluated.
    pub action_kind: String,
    /// Timestamp of the decision (millis since epoch).
    pub decided_at_ms: u64,
}

impl GovernorDecision {
    /// Create an "allow" decision.
    #[must_use]
    pub fn allow(connector_id: &str, action_kind: &str, now_ms: u64) -> Self {
        Self {
            verdict: GovernorVerdict::Allow,
            reason: GovernorReason::Clear,
            delay_ms: 0,
            connector_id: connector_id.to_string(),
            action_kind: action_kind.to_string(),
            decided_at_ms: now_ms,
        }
    }

    /// Create a "throttle" decision.
    #[must_use]
    pub fn throttle(
        connector_id: &str,
        action_kind: &str,
        reason: GovernorReason,
        delay_ms: u64,
        now_ms: u64,
    ) -> Self {
        Self {
            verdict: GovernorVerdict::Throttle,
            reason,
            delay_ms,
            connector_id: connector_id.to_string(),
            action_kind: action_kind.to_string(),
            decided_at_ms: now_ms,
        }
    }

    /// Create a "reject" decision.
    #[must_use]
    pub fn reject(
        connector_id: &str,
        action_kind: &str,
        reason: GovernorReason,
        now_ms: u64,
    ) -> Self {
        Self {
            verdict: GovernorVerdict::Reject,
            reason,
            delay_ms: 0,
            connector_id: connector_id.to_string(),
            action_kind: action_kind.to_string(),
            decided_at_ms: now_ms,
        }
    }

    /// Whether the action is allowed (possibly after a delay).
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(
            self.verdict,
            GovernorVerdict::Allow | GovernorVerdict::Throttle
        )
    }

    /// Whether the action is outright rejected.
    #[must_use]
    pub fn is_rejected(&self) -> bool {
        matches!(self.verdict, GovernorVerdict::Reject)
    }
}

// =============================================================================
// Token bucket rate limiter
// =============================================================================

/// Configuration for a token bucket rate limiter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBucketConfig {
    /// Maximum tokens (burst capacity).
    pub capacity: u64,
    /// Tokens added per refill interval.
    pub refill_rate: u64,
    /// Refill interval in milliseconds.
    pub refill_interval_ms: u64,
}

impl Default for TokenBucketConfig {
    fn default() -> Self {
        Self {
            capacity: 100,
            refill_rate: 10,
            refill_interval_ms: 1000, // 10 tokens/second
        }
    }
}

impl TokenBucketConfig {
    /// Preset for high-throughput connectors (e.g., audit logging).
    #[must_use]
    pub fn high_throughput() -> Self {
        Self {
            capacity: 500,
            refill_rate: 50,
            refill_interval_ms: 1000,
        }
    }

    /// Preset for rate-sensitive connectors (e.g., Slack, GitHub).
    #[must_use]
    pub fn rate_sensitive() -> Self {
        Self {
            capacity: 20,
            refill_rate: 5,
            refill_interval_ms: 1000,
        }
    }

    /// Preset for cost-sensitive connectors (e.g., paid APIs).
    #[must_use]
    pub fn cost_sensitive() -> Self {
        Self {
            capacity: 10,
            refill_rate: 2,
            refill_interval_ms: 1000,
        }
    }
}

/// Token bucket rate limiter with deterministic refill.
#[derive(Debug)]
pub struct TokenBucket {
    config: TokenBucketConfig,
    /// Current available tokens.
    tokens: u64,
    /// Timestamp of last refill (millis since epoch).
    last_refill_ms: u64,
}

impl TokenBucket {
    /// Create a new token bucket (starts full).
    #[must_use]
    pub fn new(config: TokenBucketConfig) -> Self {
        let tokens = config.capacity;
        Self {
            config,
            tokens,
            last_refill_ms: 0,
        }
    }

    /// Create a token bucket with a specific initial fill level.
    #[must_use]
    pub fn with_initial(config: TokenBucketConfig, initial_tokens: u64, now_ms: u64) -> Self {
        let tokens = initial_tokens.min(config.capacity);
        Self {
            config,
            tokens,
            last_refill_ms: now_ms,
        }
    }

    /// Refill tokens based on elapsed time.
    fn refill(&mut self, now_ms: u64) {
        if self.config.refill_interval_ms == 0 {
            return;
        }
        let elapsed = now_ms.saturating_sub(self.last_refill_ms);
        let intervals = elapsed / self.config.refill_interval_ms;
        if intervals > 0 {
            let added = intervals.saturating_mul(self.config.refill_rate);
            self.tokens = (self.tokens + added).min(self.config.capacity);
            self.last_refill_ms += intervals * self.config.refill_interval_ms;
        }
    }

    /// Try to consume one token. Returns true if successful.
    pub fn try_consume(&mut self, now_ms: u64) -> bool {
        self.refill(now_ms);
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }

    /// Try to consume N tokens. Returns true if all were available.
    pub fn try_consume_n(&mut self, n: u64, now_ms: u64) -> bool {
        self.refill(now_ms);
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }

    /// Current available tokens (after refill).
    #[must_use]
    pub fn available(&mut self, now_ms: u64) -> u64 {
        self.refill(now_ms);
        self.tokens
    }

    /// Time until at least one token is available (millis), or 0 if available now.
    #[must_use]
    pub fn time_until_available(&mut self, now_ms: u64) -> u64 {
        self.refill(now_ms);
        if self.tokens > 0 {
            return 0;
        }
        if self.config.refill_rate == 0 {
            return u64::MAX; // never refills
        }
        // Time until next refill interval
        let elapsed_since_last = now_ms.saturating_sub(self.last_refill_ms);
        self.config
            .refill_interval_ms
            .saturating_sub(elapsed_since_last)
    }

    /// Capacity of this bucket.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.config.capacity
    }

    /// Current fill ratio (0.0 to 1.0).
    #[must_use]
    pub fn fill_ratio(&mut self, now_ms: u64) -> f64 {
        self.refill(now_ms);
        if self.config.capacity == 0 {
            return 0.0;
        }
        self.tokens as f64 / self.config.capacity as f64
    }
}

// =============================================================================
// Quota tracking
// =============================================================================

/// Configuration for quota enforcement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaConfig {
    /// Maximum actions per window.
    pub max_actions: u64,
    /// Window duration in milliseconds.
    pub window_ms: u64,
    /// Warning threshold (fraction 0.0–1.0; warn at this usage level).
    pub warning_threshold: f64,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            max_actions: 10_000,
            window_ms: 3_600_000, // 1 hour
            warning_threshold: 0.8,
        }
    }
}

/// Sliding-window quota tracker.
#[derive(Debug)]
pub struct QuotaTracker {
    config: QuotaConfig,
    /// Action timestamps within the current window.
    window_actions: Vec<u64>,
    /// Total actions recorded (lifetime).
    total_actions: u64,
}

impl QuotaTracker {
    /// Create a new quota tracker.
    #[must_use]
    pub fn new(config: QuotaConfig) -> Self {
        Self {
            config,
            window_actions: Vec::new(),
            total_actions: 0,
        }
    }

    /// Expire actions outside the current window.
    fn gc(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(self.config.window_ms);
        self.window_actions.retain(|&ts| ts >= cutoff);
    }

    /// Record an action and check if within quota.
    ///
    /// Returns true if the action is within quota, false if quota exhausted.
    pub fn record(&mut self, now_ms: u64) -> bool {
        self.gc(now_ms);
        self.total_actions += 1;
        if self.window_actions.len() as u64 >= self.config.max_actions {
            return false;
        }
        self.window_actions.push(now_ms);
        true
    }

    /// Check remaining quota without recording.
    #[must_use]
    pub fn remaining(&mut self, now_ms: u64) -> u64 {
        self.gc(now_ms);
        self.config
            .max_actions
            .saturating_sub(self.window_actions.len() as u64)
    }

    /// Current usage fraction (0.0 to 1.0).
    #[must_use]
    pub fn usage_fraction(&mut self, now_ms: u64) -> f64 {
        self.gc(now_ms);
        if self.config.max_actions == 0 {
            return 1.0;
        }
        self.window_actions.len() as f64 / self.config.max_actions as f64
    }

    /// Whether usage is at or above the warning threshold.
    #[must_use]
    pub fn is_warning(&mut self, now_ms: u64) -> bool {
        self.usage_fraction(now_ms) >= self.config.warning_threshold
    }

    /// Whether quota is exhausted.
    #[must_use]
    pub fn is_exhausted(&mut self, now_ms: u64) -> bool {
        self.gc(now_ms);
        self.window_actions.len() as u64 >= self.config.max_actions
    }

    /// Total lifetime actions recorded.
    #[must_use]
    pub fn total_actions(&self) -> u64 {
        self.total_actions
    }

    /// Snapshot of current quota state.
    #[must_use]
    pub fn snapshot(&mut self, now_ms: u64) -> QuotaSnapshot {
        self.gc(now_ms);
        QuotaSnapshot {
            used: self.window_actions.len() as u64,
            max: self.config.max_actions,
            remaining: self.remaining(now_ms),
            usage_fraction: self.usage_fraction(now_ms),
            total_lifetime: self.total_actions,
            window_ms: self.config.window_ms,
        }
    }
}

/// Serializable quota state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuotaSnapshot {
    pub used: u64,
    pub max: u64,
    pub remaining: u64,
    pub usage_fraction: f64,
    pub total_lifetime: u64,
    pub window_ms: u64,
}

// =============================================================================
// Cost budget tracking
// =============================================================================

/// Configuration for cost budget enforcement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostBudgetConfig {
    /// Maximum cost per budget window (USD cents to avoid float issues).
    pub max_cost_cents: u64,
    /// Budget window in milliseconds.
    pub window_ms: u64,
    /// Warning threshold (fraction 0.0–1.0).
    pub warning_threshold: f64,
    /// Per-action cost estimates by action kind (USD cents).
    pub action_costs: BTreeMap<String, u64>,
}

impl Default for CostBudgetConfig {
    fn default() -> Self {
        let mut action_costs = BTreeMap::new();
        action_costs.insert("notify".to_string(), 1); // 1 cent per notification
        action_costs.insert("ticket".to_string(), 5); // 5 cents per ticket
        action_costs.insert("trigger_workflow".to_string(), 10); // 10 cents per workflow
        action_costs.insert("audit_log".to_string(), 0); // free
        action_costs.insert("invoke".to_string(), 5); // 5 cents generic
        action_costs.insert("credential_action".to_string(), 0); // free
        Self {
            max_cost_cents: 10_000, // $100 per window
            window_ms: 3_600_000,   // 1 hour
            warning_threshold: 0.8,
            action_costs,
        }
    }
}

/// Cost tracker for connector actions.
#[derive(Debug)]
pub struct CostBudget {
    config: CostBudgetConfig,
    /// (timestamp_ms, cost_cents) for actions within the window.
    window_records: Vec<(u64, u64)>,
    /// Total cost incurred (lifetime, cents).
    total_cost_cents: u64,
}

impl CostBudget {
    /// Create a new cost budget tracker.
    #[must_use]
    pub fn new(config: CostBudgetConfig) -> Self {
        Self {
            config,
            window_records: Vec::new(),
            total_cost_cents: 0,
        }
    }

    /// Expire records outside the current window.
    fn gc(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(self.config.window_ms);
        self.window_records.retain(|&(ts, _)| ts >= cutoff);
    }

    /// Estimate cost of an action kind (cents).
    #[must_use]
    pub fn estimate_cost(&self, action_kind: &ConnectorActionKind) -> u64 {
        let key = action_kind_str(action_kind);
        self.config.action_costs.get(key).copied().unwrap_or(1)
    }

    /// Record an action's cost.
    pub fn record(&mut self, action_kind: &ConnectorActionKind, now_ms: u64) -> u64 {
        self.gc(now_ms);
        let cost = self.estimate_cost(action_kind);
        self.window_records.push((now_ms, cost));
        self.total_cost_cents += cost;
        cost
    }

    /// Current window cost (cents).
    #[must_use]
    pub fn window_cost(&mut self, now_ms: u64) -> u64 {
        self.gc(now_ms);
        self.window_records.iter().map(|&(_, c)| c).sum()
    }

    /// Remaining budget (cents).
    #[must_use]
    pub fn remaining_cents(&mut self, now_ms: u64) -> u64 {
        self.config
            .max_cost_cents
            .saturating_sub(self.window_cost(now_ms))
    }

    /// Usage fraction (0.0–1.0).
    #[must_use]
    pub fn usage_fraction(&mut self, now_ms: u64) -> f64 {
        if self.config.max_cost_cents == 0 {
            return 1.0;
        }
        self.window_cost(now_ms) as f64 / self.config.max_cost_cents as f64
    }

    /// Whether budget is at warning level.
    #[must_use]
    pub fn is_warning(&mut self, now_ms: u64) -> bool {
        self.usage_fraction(now_ms) >= self.config.warning_threshold
    }

    /// Whether budget is exhausted.
    #[must_use]
    pub fn is_exhausted(&mut self, now_ms: u64) -> bool {
        self.window_cost(now_ms) >= self.config.max_cost_cents
    }

    /// Total lifetime cost (cents).
    #[must_use]
    pub fn total_cost_cents(&self) -> u64 {
        self.total_cost_cents
    }

    /// Snapshot of current budget state.
    #[must_use]
    pub fn snapshot(&mut self, now_ms: u64) -> CostBudgetSnapshot {
        self.gc(now_ms);
        CostBudgetSnapshot {
            window_cost_cents: self.window_cost(now_ms),
            max_cost_cents: self.config.max_cost_cents,
            remaining_cents: self.remaining_cents(now_ms),
            usage_fraction: self.usage_fraction(now_ms),
            total_lifetime_cents: self.total_cost_cents,
            window_ms: self.config.window_ms,
        }
    }
}

/// Serializable cost budget snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CostBudgetSnapshot {
    pub window_cost_cents: u64,
    pub max_cost_cents: u64,
    pub remaining_cents: u64,
    pub usage_fraction: f64,
    pub total_lifetime_cents: u64,
    pub window_ms: u64,
}

// =============================================================================
// Adaptive backoff tracker
// =============================================================================

/// Adaptive backoff state for a single connector.
///
/// Tracks consecutive failures and computes exponential backoff with jitter
/// to prevent thundering-herd retries across connectors.
#[derive(Debug)]
pub struct AdaptiveBackoff {
    /// Base delay in milliseconds.
    base_delay_ms: u64,
    /// Maximum delay in milliseconds.
    max_delay_ms: u64,
    /// Backoff multiplier per consecutive failure.
    multiplier: f64,
    /// Consecutive failures.
    consecutive_failures: u32,
    /// Timestamp when backoff expires (millis since epoch).
    backoff_until_ms: u64,
}

impl AdaptiveBackoff {
    /// Create a new adaptive backoff.
    #[must_use]
    pub fn new(base_delay_ms: u64, max_delay_ms: u64, multiplier: f64) -> Self {
        Self {
            base_delay_ms,
            max_delay_ms,
            multiplier,
            consecutive_failures: 0,
            backoff_until_ms: 0,
        }
    }

    /// Default backoff parameters for connectors.
    #[must_use]
    pub fn connector_default() -> Self {
        Self::new(1000, 60_000, 2.0) // 1s base, 60s max, 2x per failure
    }

    /// Record a failure and compute new backoff.
    pub fn record_failure(&mut self, now_ms: u64) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let delay = self.compute_delay();
        self.backoff_until_ms = now_ms.saturating_add(delay);
    }

    /// Record a success and reset backoff.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.backoff_until_ms = 0;
    }

    /// Compute current delay based on consecutive failures.
    #[must_use]
    fn compute_delay(&self) -> u64 {
        if self.consecutive_failures == 0 {
            return 0;
        }
        let exponent = (self.consecutive_failures - 1).min(20); // cap exponent
        let delay = self.base_delay_ms as f64 * self.multiplier.powi(exponent as i32);
        (delay as u64).min(self.max_delay_ms)
    }

    /// Whether backoff is currently active.
    #[must_use]
    pub fn is_active(&self, now_ms: u64) -> bool {
        now_ms < self.backoff_until_ms
    }

    /// Remaining backoff time in milliseconds (0 if not active).
    #[must_use]
    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        self.backoff_until_ms.saturating_sub(now_ms)
    }

    /// Number of consecutive failures.
    #[must_use]
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

// =============================================================================
// Queue backpressure for connector dispatch
// =============================================================================

/// Memory-aware queue backpressure for connector action dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueBackpressureConfig {
    /// Maximum queued actions before rejecting new ones.
    pub max_queue_depth: usize,
    /// Throttle threshold (fraction of max depth; throttle above this).
    pub throttle_threshold: f64,
    /// Reject threshold (fraction of max depth; reject above this).
    pub reject_threshold: f64,
}

impl Default for QueueBackpressureConfig {
    fn default() -> Self {
        Self {
            max_queue_depth: 5000,
            throttle_threshold: 0.7,
            reject_threshold: 0.9,
        }
    }
}

/// Queue backpressure state.
#[derive(Debug)]
pub struct QueueBackpressure {
    config: QueueBackpressureConfig,
    /// Current queue depth.
    current_depth: usize,
    /// Peak observed depth.
    peak_depth: usize,
    /// Total actions enqueued.
    total_enqueued: u64,
    /// Total actions rejected due to backpressure.
    total_rejected: u64,
}

impl QueueBackpressure {
    /// Create a new queue backpressure tracker.
    #[must_use]
    pub fn new(config: QueueBackpressureConfig) -> Self {
        Self {
            config,
            current_depth: 0,
            peak_depth: 0,
            total_enqueued: 0,
            total_rejected: 0,
        }
    }

    /// Update the current queue depth.
    pub fn update_depth(&mut self, depth: usize) {
        self.current_depth = depth;
        if depth > self.peak_depth {
            self.peak_depth = depth;
        }
    }

    /// Record an enqueue.
    pub fn record_enqueue(&mut self) {
        self.current_depth += 1;
        if self.current_depth > self.peak_depth {
            self.peak_depth = self.current_depth;
        }
        self.total_enqueued += 1;
    }

    /// Record a dequeue.
    pub fn record_dequeue(&mut self) {
        self.current_depth = self.current_depth.saturating_sub(1);
    }

    /// Record a rejection.
    pub fn record_rejection(&mut self) {
        self.total_rejected += 1;
    }

    /// Current depth fraction (0.0–1.0).
    #[must_use]
    pub fn depth_fraction(&self) -> f64 {
        if self.config.max_queue_depth == 0 {
            return 1.0;
        }
        self.current_depth as f64 / self.config.max_queue_depth as f64
    }

    /// Whether queue is in throttle zone.
    #[must_use]
    pub fn should_throttle(&self) -> bool {
        self.depth_fraction() >= self.config.throttle_threshold
    }

    /// Whether queue is in reject zone.
    #[must_use]
    pub fn should_reject(&self) -> bool {
        self.depth_fraction() >= self.config.reject_threshold
    }

    /// Current depth.
    #[must_use]
    pub fn current_depth(&self) -> usize {
        self.current_depth
    }

    /// Peak observed depth.
    #[must_use]
    pub fn peak_depth(&self) -> usize {
        self.peak_depth
    }

    /// Snapshot of backpressure state.
    #[must_use]
    pub fn snapshot(&self) -> QueueBackpressureSnapshot {
        QueueBackpressureSnapshot {
            current_depth: self.current_depth,
            max_depth: self.config.max_queue_depth,
            peak_depth: self.peak_depth,
            depth_fraction: self.depth_fraction(),
            total_enqueued: self.total_enqueued,
            total_rejected: self.total_rejected,
        }
    }
}

/// Serializable queue backpressure snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueueBackpressureSnapshot {
    pub current_depth: usize,
    pub max_depth: usize,
    pub peak_depth: usize,
    pub depth_fraction: f64,
    pub total_enqueued: u64,
    pub total_rejected: u64,
}

// =============================================================================
// Per-connector governor state
// =============================================================================

/// Per-connector governor state combining rate limit, quota, cost, and backoff.
#[derive(Debug)]
pub struct ConnectorGovernorState {
    /// Connector identifier.
    connector_id: String,
    /// Token bucket rate limiter.
    rate_limiter: TokenBucket,
    /// Quota tracker.
    quota: QuotaTracker,
    /// Cost budget.
    cost: CostBudget,
    /// Adaptive backoff.
    backoff: AdaptiveBackoff,
}

impl ConnectorGovernorState {
    /// Create a new connector governor state.
    #[must_use]
    pub fn new(
        connector_id: impl Into<String>,
        rate_config: TokenBucketConfig,
        quota_config: QuotaConfig,
        cost_config: CostBudgetConfig,
    ) -> Self {
        Self {
            connector_id: connector_id.into(),
            rate_limiter: TokenBucket::new(rate_config),
            quota: QuotaTracker::new(quota_config),
            cost: CostBudget::new(cost_config),
            backoff: AdaptiveBackoff::connector_default(),
        }
    }

    /// Get the connector ID.
    #[must_use]
    pub fn connector_id(&self) -> &str {
        &self.connector_id
    }

    /// Record a failure on this connector.
    pub fn record_failure(&mut self, now_ms: u64) {
        self.backoff.record_failure(now_ms);
    }

    /// Record a success on this connector.
    pub fn record_success(&mut self) {
        self.backoff.record_success();
    }

    /// Per-connector snapshot.
    #[must_use]
    pub fn snapshot(&mut self, now_ms: u64) -> ConnectorGovernorSnapshot {
        ConnectorGovernorSnapshot {
            connector_id: self.connector_id.clone(),
            rate_limit_fill_ratio: self.rate_limiter.fill_ratio(now_ms),
            quota: self.quota.snapshot(now_ms),
            cost: self.cost.snapshot(now_ms),
            backoff_active: self.backoff.is_active(now_ms),
            backoff_remaining_ms: self.backoff.remaining_ms(now_ms),
            consecutive_failures: self.backoff.consecutive_failures(),
        }
    }
}

/// Serializable per-connector governor snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorGovernorSnapshot {
    pub connector_id: String,
    pub rate_limit_fill_ratio: f64,
    pub quota: QuotaSnapshot,
    pub cost: CostBudgetSnapshot,
    pub backoff_active: bool,
    pub backoff_remaining_ms: u64,
    pub consecutive_failures: u32,
}

// =============================================================================
// Main governor
// =============================================================================

/// Configuration for the connector governor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorGovernorConfig {
    /// Default rate limit config for new connectors.
    pub default_rate_limit: TokenBucketConfig,
    /// Default quota config for new connectors.
    pub default_quota: QuotaConfig,
    /// Default cost budget config for new connectors.
    pub default_cost_budget: CostBudgetConfig,
    /// Global rate limit (across all connectors).
    pub global_rate_limit: TokenBucketConfig,
    /// Global quota (across all connectors).
    pub global_quota: QuotaConfig,
    /// Queue backpressure config.
    pub queue_backpressure: QueueBackpressureConfig,
}

impl Default for ConnectorGovernorConfig {
    fn default() -> Self {
        Self {
            default_rate_limit: TokenBucketConfig::default(),
            default_quota: QuotaConfig::default(),
            default_cost_budget: CostBudgetConfig::default(),
            global_rate_limit: TokenBucketConfig {
                capacity: 1000,
                refill_rate: 100,
                refill_interval_ms: 1000,
            },
            global_quota: QuotaConfig {
                max_actions: 100_000,
                window_ms: 3_600_000,
                warning_threshold: 0.8,
            },
            queue_backpressure: QueueBackpressureConfig::default(),
        }
    }
}

/// Global connector action governor.
///
/// Evaluates every outbound connector action against per-connector and global
/// rate limits, quotas, cost budgets, queue backpressure, and adaptive backoff.
/// Returns an explainable decision (Allow/Throttle/Reject) with reason and delay.
#[derive(Debug)]
pub struct ConnectorGovernor {
    config: ConnectorGovernorConfig,
    /// Per-connector state.
    connectors: BTreeMap<String, ConnectorGovernorState>,
    /// Global rate limiter.
    global_rate: TokenBucket,
    /// Global quota tracker.
    global_quota: QuotaTracker,
    /// Queue backpressure.
    queue: QueueBackpressure,
    /// Telemetry.
    telemetry: GovernorTelemetry,
}

impl ConnectorGovernor {
    /// Create a new connector governor.
    #[must_use]
    pub fn new(config: ConnectorGovernorConfig) -> Self {
        let global_rate = TokenBucket::new(config.global_rate_limit.clone());
        let global_quota = QuotaTracker::new(config.global_quota.clone());
        let queue = QueueBackpressure::new(config.queue_backpressure.clone());
        Self {
            config,
            connectors: BTreeMap::new(),
            global_rate,
            global_quota,
            queue,
            telemetry: GovernorTelemetry::default(),
        }
    }

    /// Get or create per-connector state.
    fn ensure_connector(&mut self, connector_id: &str) -> &mut ConnectorGovernorState {
        if !self.connectors.contains_key(connector_id) {
            self.connectors.insert(
                connector_id.to_string(),
                ConnectorGovernorState::new(
                    connector_id,
                    self.config.default_rate_limit.clone(),
                    self.config.default_quota.clone(),
                    self.config.default_cost_budget.clone(),
                ),
            );
        }
        self.connectors.get_mut(connector_id).unwrap()
    }

    /// Evaluate whether a connector action should be allowed.
    ///
    /// Checks (in order of severity):
    /// 1. Queue backpressure
    /// 2. Adaptive backoff
    /// 3. Global/per-connector rate availability
    /// 4. Global/per-connector quota availability
    /// 5. Cost budget availability
    ///
    /// Side-effectful counters (token consumption + quota/cost recording) are
    /// only committed after all checks pass so rejected/throttled actions don't
    /// leak capacity from unrelated global budgets.
    pub fn evaluate(&mut self, action: &ConnectorAction, now_ms: u64) -> GovernorDecision {
        self.telemetry.evaluations += 1;
        let connector_id = action.target_connector.clone();
        let action_kind = action_kind_str(&action.action_kind);

        // 1. Queue backpressure — hard reject
        if self.queue.should_reject() {
            self.telemetry.rejections += 1;
            self.queue.record_rejection();
            return GovernorDecision::reject(
                &connector_id,
                action_kind,
                GovernorReason::Backpressure,
                now_ms,
            );
        }

        // 2. Queue throttle zone
        if self.queue.should_throttle() {
            self.telemetry.throttles += 1;
            return GovernorDecision::throttle(
                &connector_id,
                action_kind,
                GovernorReason::Backpressure,
                500, // 500ms delay for queue pressure
                now_ms,
            );
        }

        // Ensure connector exists upfront
        self.ensure_connector(&connector_id);

        // 3. Adaptive backoff — extract check result, then update telemetry
        let backoff_delay = {
            let state = self.connectors.get(&connector_id).unwrap();
            if state.backoff.is_active(now_ms) {
                Some(state.backoff.remaining_ms(now_ms))
            } else {
                None
            }
        };
        if let Some(delay) = backoff_delay {
            self.telemetry.throttles += 1;
            return GovernorDecision::throttle(
                &connector_id,
                action_kind,
                GovernorReason::AdaptiveBackoff,
                delay,
                now_ms,
            );
        }

        // 4. Global rate availability
        if self.global_rate.available(now_ms) == 0 {
            self.telemetry.throttles += 1;
            let delay = self.global_rate.time_until_available(now_ms);
            return GovernorDecision::throttle(
                &connector_id,
                action_kind,
                GovernorReason::GlobalRateLimit,
                delay,
                now_ms,
            );
        }

        // 5. Per-connector rate availability
        let rate_delay = {
            let state = self.connectors.get_mut(&connector_id).unwrap();
            if state.rate_limiter.available(now_ms) == 0 {
                Some(state.rate_limiter.time_until_available(now_ms))
            } else {
                None
            }
        };
        if let Some(delay) = rate_delay {
            self.telemetry.throttles += 1;
            return GovernorDecision::throttle(
                &connector_id,
                action_kind,
                GovernorReason::ConnectorRateLimit,
                delay,
                now_ms,
            );
        }

        // 6. Global quota availability
        if self.global_quota.is_exhausted(now_ms) {
            self.telemetry.rejections += 1;
            return GovernorDecision::reject(
                &connector_id,
                action_kind,
                GovernorReason::GlobalQuotaExhausted,
                now_ms,
            );
        }

        // 7. Per-connector quota availability
        let quota_exhausted = {
            let state = self.connectors.get_mut(&connector_id).unwrap();
            state.quota.is_exhausted(now_ms)
        };
        if quota_exhausted {
            self.telemetry.rejections += 1;
            return GovernorDecision::reject(
                &connector_id,
                action_kind,
                GovernorReason::ConnectorQuotaExhausted,
                now_ms,
            );
        }

        // 8. Cost budget availability (projected with this action).
        let budget_exceeded = {
            let state = self.connectors.get_mut(&connector_id).unwrap();
            let estimated = state.cost.estimate_cost(&action.action_kind);
            state.cost.remaining_cents(now_ms) < estimated
        };
        if budget_exceeded {
            self.telemetry.rejections += 1;
            return GovernorDecision::reject(
                &connector_id,
                action_kind,
                GovernorReason::BudgetExceeded,
                now_ms,
            );
        }

        // Commit side effects after all checks pass.
        let consumed_global = self.global_rate.try_consume(now_ms);
        debug_assert!(
            consumed_global,
            "global rate token disappeared after availability check"
        );
        let consumed_connector = {
            let state = self.connectors.get_mut(&connector_id).unwrap();
            state.rate_limiter.try_consume(now_ms)
        };
        debug_assert!(
            consumed_connector,
            "connector rate token disappeared after availability check"
        );
        let _ = self.global_quota.record(now_ms);
        {
            let state = self.connectors.get_mut(&connector_id).unwrap();
            let _ = state.quota.record(now_ms);
            let _ = state.cost.record(&action.action_kind, now_ms);
        }

        // All checks passed
        self.telemetry.allows += 1;
        GovernorDecision::allow(&connector_id, action_kind, now_ms)
    }

    /// Record a connector action outcome (success/failure) for adaptive backoff.
    pub fn record_outcome(&mut self, connector_id: &str, success: bool, now_ms: u64) {
        let state = self.ensure_connector(connector_id);
        if success {
            state.record_success();
        } else {
            state.record_failure(now_ms);
        }
    }

    /// Update queue depth for backpressure tracking.
    pub fn update_queue_depth(&mut self, depth: usize) {
        self.queue.update_depth(depth);
    }

    /// Record an enqueue to the dispatch queue.
    pub fn record_enqueue(&mut self) {
        self.queue.record_enqueue();
    }

    /// Record a dequeue from the dispatch queue.
    pub fn record_dequeue(&mut self) {
        self.queue.record_dequeue();
    }

    /// Get per-connector state (immutable).
    #[must_use]
    pub fn get_connector(&self, connector_id: &str) -> Option<&ConnectorGovernorState> {
        self.connectors.get(connector_id)
    }

    /// Get registered connector IDs.
    #[must_use]
    pub fn connector_ids(&self) -> Vec<&str> {
        self.connectors.keys().map(|s| s.as_str()).collect()
    }

    /// Full governor snapshot.
    #[must_use]
    pub fn snapshot(&mut self, now_ms: u64) -> GovernorSnapshot {
        let connectors: Vec<ConnectorGovernorSnapshot> = self
            .connectors
            .values_mut()
            .map(|s| s.snapshot(now_ms))
            .collect();
        GovernorSnapshot {
            global_rate_fill_ratio: self.global_rate.fill_ratio(now_ms),
            global_quota: self.global_quota.snapshot(now_ms),
            queue: self.queue.snapshot(),
            connectors,
            telemetry: self.telemetry.snapshot(),
        }
    }
}

// =============================================================================
// Governor telemetry
// =============================================================================

#[derive(Debug, Default)]
struct GovernorTelemetry {
    evaluations: u64,
    allows: u64,
    throttles: u64,
    rejections: u64,
}

impl GovernorTelemetry {
    fn snapshot(&self) -> GovernorTelemetrySnapshot {
        GovernorTelemetrySnapshot {
            evaluations: self.evaluations,
            allows: self.allows,
            throttles: self.throttles,
            rejections: self.rejections,
        }
    }
}

/// Serializable governor telemetry snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GovernorTelemetrySnapshot {
    pub evaluations: u64,
    pub allows: u64,
    pub throttles: u64,
    pub rejections: u64,
}

/// Full governor snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernorSnapshot {
    pub global_rate_fill_ratio: f64,
    pub global_quota: QuotaSnapshot,
    pub queue: QueueBackpressureSnapshot,
    pub connectors: Vec<ConnectorGovernorSnapshot>,
    pub telemetry: GovernorTelemetrySnapshot,
}

// =============================================================================
// Helpers
// =============================================================================

/// Convert `ConnectorActionKind` to a string key.
fn action_kind_str(kind: &ConnectorActionKind) -> &'static str {
    match kind {
        ConnectorActionKind::Notify => "notify",
        ConnectorActionKind::Ticket => "ticket",
        ConnectorActionKind::TriggerWorkflow => "trigger_workflow",
        ConnectorActionKind::AuditLog => "audit_log",
        ConnectorActionKind::Invoke => "invoke",
        ConnectorActionKind::CredentialAction => "credential_action",
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_action(connector: &str, kind: ConnectorActionKind) -> ConnectorAction {
        ConnectorAction {
            target_connector: connector.to_string(),
            action_kind: kind,
            correlation_id: format!("corr-{connector}"),
            params: serde_json::json!({"test": true}),
            created_at_ms: 1000,
        }
    }

    // ---- GovernorVerdict ----

    #[test]
    fn verdict_display() {
        assert_eq!(GovernorVerdict::Allow.to_string(), "allow");
        assert_eq!(GovernorVerdict::Throttle.to_string(), "throttle");
        assert_eq!(GovernorVerdict::Reject.to_string(), "reject");
    }

    #[test]
    fn verdict_serde_roundtrip() {
        for v in [
            GovernorVerdict::Allow,
            GovernorVerdict::Throttle,
            GovernorVerdict::Reject,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: GovernorVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn reason_display() {
        assert_eq!(GovernorReason::Clear.to_string(), "clear");
        assert_eq!(
            GovernorReason::ConnectorRateLimit.to_string(),
            "connector_rate_limit"
        );
        assert_eq!(
            GovernorReason::AdaptiveBackoff.to_string(),
            "adaptive_backoff"
        );
    }

    #[test]
    fn reason_serde_roundtrip() {
        let reasons = [
            GovernorReason::Clear,
            GovernorReason::ConnectorRateLimit,
            GovernorReason::GlobalRateLimit,
            GovernorReason::ConnectorQuotaExhausted,
            GovernorReason::GlobalQuotaExhausted,
            GovernorReason::BudgetExceeded,
            GovernorReason::Backpressure,
            GovernorReason::AdaptiveBackoff,
        ];
        for r in reasons {
            let json = serde_json::to_string(&r).unwrap();
            let back: GovernorReason = serde_json::from_str(&json).unwrap();
            assert_eq!(r, back);
        }
    }

    // ---- GovernorDecision ----

    #[test]
    fn decision_allow() {
        let d = GovernorDecision::allow("slack", "notify", 1000);
        assert!(d.is_allowed());
        assert!(!d.is_rejected());
        assert_eq!(d.delay_ms, 0);
        assert_eq!(d.verdict, GovernorVerdict::Allow);
        assert_eq!(d.reason, GovernorReason::Clear);
    }

    #[test]
    fn decision_throttle() {
        let d = GovernorDecision::throttle(
            "slack",
            "notify",
            GovernorReason::AdaptiveBackoff,
            5000,
            1000,
        );
        assert!(d.is_allowed());
        assert!(!d.is_rejected());
        assert_eq!(d.delay_ms, 5000);
    }

    #[test]
    fn decision_reject() {
        let d = GovernorDecision::reject("slack", "notify", GovernorReason::BudgetExceeded, 1000);
        assert!(!d.is_allowed());
        assert!(d.is_rejected());
    }

    #[test]
    fn decision_serde_roundtrip() {
        let d = GovernorDecision::throttle(
            "github",
            "ticket",
            GovernorReason::GlobalRateLimit,
            200,
            5000,
        );
        let json = serde_json::to_string(&d).unwrap();
        let back: GovernorDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(back.verdict, d.verdict);
        assert_eq!(back.reason, d.reason);
        assert_eq!(back.delay_ms, d.delay_ms);
        assert_eq!(back.connector_id, d.connector_id);
    }

    // ---- TokenBucket ----

    #[test]
    fn token_bucket_starts_full() {
        let config = TokenBucketConfig {
            capacity: 10,
            refill_rate: 1,
            refill_interval_ms: 1000,
        };
        let mut bucket = TokenBucket::new(config);
        assert_eq!(bucket.available(0), 10);
    }

    #[test]
    fn token_bucket_consume_and_refill() {
        let config = TokenBucketConfig {
            capacity: 5,
            refill_rate: 2,
            refill_interval_ms: 1000,
        };
        let mut bucket = TokenBucket::with_initial(config, 0, 0);
        assert_eq!(bucket.available(0), 0);
        assert!(!bucket.try_consume(0));

        // After 1 second: 2 tokens
        assert_eq!(bucket.available(1000), 2);
        assert!(bucket.try_consume(1000));
        assert_eq!(bucket.available(1000), 1);

        // After 3 more seconds: 1 + 6 = 7, capped at 5
        assert_eq!(bucket.available(4000), 5);
    }

    #[test]
    fn token_bucket_consume_n() {
        let config = TokenBucketConfig {
            capacity: 10,
            refill_rate: 5,
            refill_interval_ms: 1000,
        };
        let mut bucket = TokenBucket::new(config);
        assert!(bucket.try_consume_n(5, 0));
        assert_eq!(bucket.available(0), 5);
        assert!(!bucket.try_consume_n(6, 0));
        assert_eq!(bucket.available(0), 5); // unchanged
        assert!(bucket.try_consume_n(5, 0));
        assert_eq!(bucket.available(0), 0);
    }

    #[test]
    fn token_bucket_time_until_available() {
        let config = TokenBucketConfig {
            capacity: 10,
            refill_rate: 1,
            refill_interval_ms: 1000,
        };
        let mut bucket = TokenBucket::with_initial(config, 0, 0);
        assert_eq!(bucket.time_until_available(0), 1000);
        assert_eq!(bucket.time_until_available(500), 500);
        assert_eq!(bucket.time_until_available(1000), 0); // refill happened
    }

    #[test]
    fn token_bucket_fill_ratio() {
        let config = TokenBucketConfig {
            capacity: 100,
            refill_rate: 10,
            refill_interval_ms: 1000,
        };
        let mut bucket = TokenBucket::new(config);
        assert!((bucket.fill_ratio(0) - 1.0).abs() < f64::EPSILON);
        bucket.try_consume_n(50, 0);
        assert!((bucket.fill_ratio(0) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn token_bucket_presets() {
        let ht = TokenBucketConfig::high_throughput();
        assert_eq!(ht.capacity, 500);
        let rs = TokenBucketConfig::rate_sensitive();
        assert_eq!(rs.capacity, 20);
        let cs = TokenBucketConfig::cost_sensitive();
        assert_eq!(cs.capacity, 10);
    }

    // ---- QuotaTracker ----

    #[test]
    fn quota_tracks_within_window() {
        let config = QuotaConfig {
            max_actions: 5,
            window_ms: 10_000,
            warning_threshold: 0.8,
        };
        let mut qt = QuotaTracker::new(config);
        for i in 0..5 {
            assert!(qt.record(i * 1000));
        }
        assert!(!qt.record(5000)); // 6th action, over quota
        assert_eq!(qt.remaining(5000), 0);
    }

    #[test]
    fn quota_window_expiration() {
        let config = QuotaConfig {
            max_actions: 3,
            window_ms: 5000,
            warning_threshold: 0.8,
        };
        let mut qt = QuotaTracker::new(config);
        assert!(qt.record(1000));
        assert!(qt.record(2000));
        assert!(qt.record(3000));
        assert!(!qt.record(4000)); // over

        // Actions at 1000, 2000, 3000 expire at 6000+
        assert_eq!(qt.remaining(7000), 1); // action at 2000 still in window
        assert_eq!(qt.remaining(9000), 3); // all expired
    }

    #[test]
    fn quota_warning_threshold() {
        let config = QuotaConfig {
            max_actions: 10,
            window_ms: 60_000,
            warning_threshold: 0.5,
        };
        let mut qt = QuotaTracker::new(config);
        for i in 0..4 {
            qt.record(i * 1000);
        }
        assert!(!qt.is_warning(5000)); // 4/10 = 0.4 < 0.5

        qt.record(5000); // 5/10 = 0.5
        assert!(qt.is_warning(6000));
    }

    #[test]
    fn quota_snapshot_serde_roundtrip() {
        let snap = QuotaSnapshot {
            used: 50,
            max: 100,
            remaining: 50,
            usage_fraction: 0.5,
            total_lifetime: 200,
            window_ms: 3_600_000,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: QuotaSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    // ---- CostBudget ----

    #[test]
    fn cost_budget_tracks_costs() {
        let mut config = CostBudgetConfig::default();
        config.max_cost_cents = 100;
        let mut cb = CostBudget::new(config);

        // Notify = 1 cent
        let cost = cb.record(&ConnectorActionKind::Notify, 1000);
        assert_eq!(cost, 1);
        assert_eq!(cb.window_cost(1000), 1);
        assert_eq!(cb.remaining_cents(1000), 99);
    }

    #[test]
    fn cost_budget_exhaustion() {
        let mut config = CostBudgetConfig::default();
        config.max_cost_cents = 10;
        let mut cb = CostBudget::new(config);

        // TriggerWorkflow = 10 cents each
        cb.record(&ConnectorActionKind::TriggerWorkflow, 1000);
        assert!(cb.is_exhausted(1000));
        assert_eq!(cb.remaining_cents(1000), 0);
    }

    #[test]
    fn cost_budget_window_expiration() {
        let mut config = CostBudgetConfig::default();
        config.max_cost_cents = 20;
        config.window_ms = 5000;
        let mut cb = CostBudget::new(config);

        cb.record(&ConnectorActionKind::TriggerWorkflow, 1000); // 10 cents at t=1000
        cb.record(&ConnectorActionKind::TriggerWorkflow, 2000); // 10 cents at t=2000
        assert!(cb.is_exhausted(3000)); // 20 cents total

        // t=7000: action at t=1000 expired (cutoff = 7000-5000 = 2000)
        assert_eq!(cb.window_cost(7000), 10); // only t=2000 remains
        assert!(!cb.is_exhausted(7000));
    }

    #[test]
    fn cost_budget_warning_threshold() {
        let mut config = CostBudgetConfig::default();
        config.max_cost_cents = 100;
        config.warning_threshold = 0.5;
        let mut cb = CostBudget::new(config);

        // Record 50 notifies (1 cent each)
        for i in 0..50 {
            cb.record(&ConnectorActionKind::Notify, i * 100);
        }
        assert!(cb.is_warning(5000)); // 50/100 = 0.5
    }

    #[test]
    fn cost_budget_snapshot_serde() {
        let snap = CostBudgetSnapshot {
            window_cost_cents: 50,
            max_cost_cents: 100,
            remaining_cents: 50,
            usage_fraction: 0.5,
            total_lifetime_cents: 200,
            window_ms: 3_600_000,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: CostBudgetSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    // ---- AdaptiveBackoff ----

    #[test]
    fn backoff_starts_inactive() {
        let b = AdaptiveBackoff::connector_default();
        assert!(!b.is_active(0));
        assert_eq!(b.consecutive_failures(), 0);
        assert_eq!(b.remaining_ms(0), 0);
    }

    #[test]
    fn backoff_exponential() {
        let mut b = AdaptiveBackoff::new(1000, 60_000, 2.0);
        // First failure: 1000ms backoff
        b.record_failure(0);
        assert!(b.is_active(0));
        assert_eq!(b.remaining_ms(0), 1000);
        assert!(!b.is_active(1000)); // expired

        // Second failure: 2000ms
        b.record_failure(1000);
        assert_eq!(b.remaining_ms(1000), 2000);

        // Third failure: 4000ms
        b.record_failure(3000);
        assert_eq!(b.remaining_ms(3000), 4000);
    }

    #[test]
    fn backoff_capped_at_max() {
        let mut b = AdaptiveBackoff::new(1000, 5000, 2.0);
        for i in 0..10 {
            b.record_failure(i * 10000);
        }
        // Should be capped at 5000ms
        assert!(b.remaining_ms(90000) <= 5000);
    }

    #[test]
    fn backoff_resets_on_success() {
        let mut b = AdaptiveBackoff::new(1000, 60_000, 2.0);
        b.record_failure(0);
        b.record_failure(1000);
        assert_eq!(b.consecutive_failures(), 2);

        b.record_success();
        assert_eq!(b.consecutive_failures(), 0);
        assert!(!b.is_active(5000));
    }

    // ---- QueueBackpressure ----

    #[test]
    fn queue_backpressure_zones() {
        let config = QueueBackpressureConfig {
            max_queue_depth: 100,
            throttle_threshold: 0.7,
            reject_threshold: 0.9,
        };
        let mut qb = QueueBackpressure::new(config);

        qb.update_depth(50);
        assert!(!qb.should_throttle());
        assert!(!qb.should_reject());

        qb.update_depth(70);
        assert!(qb.should_throttle());
        assert!(!qb.should_reject());

        qb.update_depth(90);
        assert!(qb.should_throttle());
        assert!(qb.should_reject());
    }

    #[test]
    fn queue_backpressure_enqueue_dequeue() {
        let mut qb = QueueBackpressure::new(QueueBackpressureConfig::default());
        qb.record_enqueue();
        qb.record_enqueue();
        assert_eq!(qb.current_depth(), 2);
        assert_eq!(qb.peak_depth(), 2);

        qb.record_dequeue();
        assert_eq!(qb.current_depth(), 1);
        assert_eq!(qb.peak_depth(), 2); // peak unchanged
    }

    #[test]
    fn queue_backpressure_snapshot_serde() {
        let snap = QueueBackpressureSnapshot {
            current_depth: 50,
            max_depth: 100,
            peak_depth: 75,
            depth_fraction: 0.5,
            total_enqueued: 200,
            total_rejected: 10,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: QueueBackpressureSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    // ---- ConnectorGovernor ----

    #[test]
    fn governor_allows_normal_actions() {
        let mut gov = ConnectorGovernor::new(ConnectorGovernorConfig::default());
        let action = sample_action("slack", ConnectorActionKind::Notify);
        let decision = gov.evaluate(&action, 1000);
        assert_eq!(decision.verdict, GovernorVerdict::Allow);
        assert_eq!(decision.reason, GovernorReason::Clear);
    }

    #[test]
    fn governor_per_connector_rate_limit() {
        let mut config = ConnectorGovernorConfig::default();
        config.default_rate_limit = TokenBucketConfig {
            capacity: 3,
            refill_rate: 1,
            refill_interval_ms: 1000,
        };
        let mut gov = ConnectorGovernor::new(config);
        let action = sample_action("slack", ConnectorActionKind::Notify);

        // Consume all 3 tokens
        for _ in 0..3 {
            let d = gov.evaluate(&action, 1000);
            assert_eq!(d.verdict, GovernorVerdict::Allow);
        }

        // 4th should be throttled
        let d = gov.evaluate(&action, 1000);
        assert_eq!(d.verdict, GovernorVerdict::Throttle);
        assert_eq!(d.reason, GovernorReason::ConnectorRateLimit);
    }

    #[test]
    fn governor_global_rate_limit() {
        let mut config = ConnectorGovernorConfig::default();
        config.global_rate_limit = TokenBucketConfig {
            capacity: 2,
            refill_rate: 1,
            refill_interval_ms: 1000,
        };
        let mut gov = ConnectorGovernor::new(config);

        // 2 different connectors exhaust global limit
        let a1 = sample_action("slack", ConnectorActionKind::Notify);
        let a2 = sample_action("github", ConnectorActionKind::Ticket);
        assert_eq!(gov.evaluate(&a1, 1000).verdict, GovernorVerdict::Allow);
        assert_eq!(gov.evaluate(&a2, 1000).verdict, GovernorVerdict::Allow);

        // 3rd from any connector is throttled
        let a3 = sample_action("datadog", ConnectorActionKind::AuditLog);
        let d = gov.evaluate(&a3, 1000);
        assert_eq!(d.verdict, GovernorVerdict::Throttle);
        assert_eq!(d.reason, GovernorReason::GlobalRateLimit);
    }

    #[test]
    fn governor_connector_throttle_does_not_consume_global_rate_capacity() {
        let mut config = ConnectorGovernorConfig::default();
        config.global_rate_limit = TokenBucketConfig {
            capacity: 2,
            refill_rate: 0,
            refill_interval_ms: 1000,
        };
        config.default_rate_limit = TokenBucketConfig {
            capacity: 1,
            refill_rate: 0,
            refill_interval_ms: 1000,
        };
        let mut gov = ConnectorGovernor::new(config);

        let slack = sample_action("slack", ConnectorActionKind::Notify);
        let github = sample_action("github", ConnectorActionKind::Notify);

        assert_eq!(gov.evaluate(&slack, 1000).verdict, GovernorVerdict::Allow);
        let second_slack = gov.evaluate(&slack, 1000);
        assert_eq!(second_slack.verdict, GovernorVerdict::Throttle);
        assert_eq!(second_slack.reason, GovernorReason::ConnectorRateLimit);

        // This would be incorrectly throttled by global rate if connector-throttled
        // actions consumed global tokens.
        assert_eq!(gov.evaluate(&github, 1000).verdict, GovernorVerdict::Allow);
    }

    #[test]
    fn governor_quota_exhaustion_rejects() {
        let mut config = ConnectorGovernorConfig::default();
        config.default_quota = QuotaConfig {
            max_actions: 2,
            window_ms: 60_000,
            warning_threshold: 0.5,
        };
        let mut gov = ConnectorGovernor::new(config);
        let action = sample_action("slack", ConnectorActionKind::Notify);

        assert_eq!(gov.evaluate(&action, 1000).verdict, GovernorVerdict::Allow);
        assert_eq!(gov.evaluate(&action, 2000).verdict, GovernorVerdict::Allow);

        let d = gov.evaluate(&action, 3000);
        assert_eq!(d.verdict, GovernorVerdict::Reject);
        assert_eq!(d.reason, GovernorReason::ConnectorQuotaExhausted);
    }

    #[test]
    fn governor_cost_budget_rejects() {
        let mut cost_config = CostBudgetConfig::default();
        cost_config.max_cost_cents = 5;
        let mut config = ConnectorGovernorConfig::default();
        config.default_cost_budget = cost_config;
        let mut gov = ConnectorGovernor::new(config);

        // Ticket = 5 cents, fills budget
        let action = sample_action("jira", ConnectorActionKind::Ticket);
        assert_eq!(gov.evaluate(&action, 1000).verdict, GovernorVerdict::Allow);

        // Next action should be rejected (budget exhausted)
        let d = gov.evaluate(&action, 2000);
        assert_eq!(d.verdict, GovernorVerdict::Reject);
        assert_eq!(d.reason, GovernorReason::BudgetExceeded);
    }

    #[test]
    fn governor_cost_budget_rejects_projected_overrun() {
        let mut cost_config = CostBudgetConfig::default();
        cost_config.max_cost_cents = 9;
        let mut config = ConnectorGovernorConfig::default();
        config.default_cost_budget = cost_config;
        let mut gov = ConnectorGovernor::new(config);

        // Ticket costs 5 cents.
        let action = sample_action("jira", ConnectorActionKind::Ticket);
        assert_eq!(gov.evaluate(&action, 1000).verdict, GovernorVerdict::Allow);

        // Remaining budget is 4, so a second ticket must be rejected.
        let second = gov.evaluate(&action, 2000);
        assert_eq!(second.verdict, GovernorVerdict::Reject);
        assert_eq!(second.reason, GovernorReason::BudgetExceeded);
    }

    #[test]
    fn governor_backpressure_reject() {
        let mut config = ConnectorGovernorConfig::default();
        config.queue_backpressure = QueueBackpressureConfig {
            max_queue_depth: 100,
            throttle_threshold: 0.7,
            reject_threshold: 0.9,
        };
        let mut gov = ConnectorGovernor::new(config);
        gov.update_queue_depth(95);

        let action = sample_action("slack", ConnectorActionKind::Notify);
        let d = gov.evaluate(&action, 1000);
        assert_eq!(d.verdict, GovernorVerdict::Reject);
        assert_eq!(d.reason, GovernorReason::Backpressure);
    }

    #[test]
    fn governor_backpressure_throttle() {
        let mut config = ConnectorGovernorConfig::default();
        config.queue_backpressure = QueueBackpressureConfig {
            max_queue_depth: 100,
            throttle_threshold: 0.7,
            reject_threshold: 0.9,
        };
        let mut gov = ConnectorGovernor::new(config);
        gov.update_queue_depth(75);

        let action = sample_action("slack", ConnectorActionKind::Notify);
        let d = gov.evaluate(&action, 1000);
        assert_eq!(d.verdict, GovernorVerdict::Throttle);
        assert_eq!(d.reason, GovernorReason::Backpressure);
    }

    #[test]
    fn governor_adaptive_backoff_throttles() {
        let mut gov = ConnectorGovernor::new(ConnectorGovernorConfig::default());

        // Record failures
        gov.record_outcome("slack", false, 1000);
        gov.record_outcome("slack", false, 1100);

        // Action during backoff
        let action = sample_action("slack", ConnectorActionKind::Notify);
        let d = gov.evaluate(&action, 1200);
        assert_eq!(d.verdict, GovernorVerdict::Throttle);
        assert_eq!(d.reason, GovernorReason::AdaptiveBackoff);
        assert!(d.delay_ms > 0);
    }

    #[test]
    fn governor_backoff_clears_on_success() {
        let mut gov = ConnectorGovernor::new(ConnectorGovernorConfig::default());
        gov.record_outcome("slack", false, 1000);
        gov.record_outcome("slack", true, 5000);

        let action = sample_action("slack", ConnectorActionKind::Notify);
        let d = gov.evaluate(&action, 5000);
        assert_eq!(d.verdict, GovernorVerdict::Allow);
    }

    #[test]
    fn governor_different_connectors_independent() {
        let mut config = ConnectorGovernorConfig::default();
        config.default_rate_limit = TokenBucketConfig {
            capacity: 1,
            refill_rate: 0,
            refill_interval_ms: 1000,
        };
        let mut gov = ConnectorGovernor::new(config);

        let slack = sample_action("slack", ConnectorActionKind::Notify);
        let github = sample_action("github", ConnectorActionKind::Ticket);

        assert_eq!(gov.evaluate(&slack, 1000).verdict, GovernorVerdict::Allow);
        assert_eq!(gov.evaluate(&github, 1000).verdict, GovernorVerdict::Allow);

        // Each connector's 2nd action is throttled independently
        assert_eq!(
            gov.evaluate(&slack, 1000).verdict,
            GovernorVerdict::Throttle
        );
        assert_eq!(
            gov.evaluate(&github, 1000).verdict,
            GovernorVerdict::Throttle
        );
    }

    #[test]
    fn governor_telemetry_snapshot() {
        let mut gov = ConnectorGovernor::new(ConnectorGovernorConfig::default());
        let action = sample_action("slack", ConnectorActionKind::Notify);
        gov.evaluate(&action, 1000);

        let snap = gov.snapshot(1000);
        assert_eq!(snap.telemetry.evaluations, 1);
        assert_eq!(snap.telemetry.allows, 1);
        assert_eq!(snap.telemetry.throttles, 0);
        assert_eq!(snap.telemetry.rejections, 0);
    }

    #[test]
    fn governor_telemetry_serde_roundtrip() {
        let snap = GovernorTelemetrySnapshot {
            evaluations: 100,
            allows: 80,
            throttles: 15,
            rejections: 5,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: GovernorTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn governor_connector_ids_sorted() {
        let mut gov = ConnectorGovernor::new(ConnectorGovernorConfig::default());
        let actions = [
            sample_action("github", ConnectorActionKind::Ticket),
            sample_action("slack", ConnectorActionKind::Notify),
            sample_action("datadog", ConnectorActionKind::AuditLog),
        ];
        for a in &actions {
            gov.evaluate(a, 1000);
        }
        let ids = gov.connector_ids();
        assert_eq!(ids, vec!["datadog", "github", "slack"]); // BTreeMap = sorted
    }

    #[test]
    fn governor_global_quota_rejects() {
        let mut config = ConnectorGovernorConfig::default();
        config.global_quota = QuotaConfig {
            max_actions: 2,
            window_ms: 60_000,
            warning_threshold: 0.5,
        };
        let mut gov = ConnectorGovernor::new(config);
        let a1 = sample_action("slack", ConnectorActionKind::Notify);
        let a2 = sample_action("github", ConnectorActionKind::Ticket);
        let a3 = sample_action("datadog", ConnectorActionKind::AuditLog);

        assert_eq!(gov.evaluate(&a1, 1000).verdict, GovernorVerdict::Allow);
        assert_eq!(gov.evaluate(&a2, 2000).verdict, GovernorVerdict::Allow);

        let d = gov.evaluate(&a3, 3000);
        assert_eq!(d.verdict, GovernorVerdict::Reject);
        assert_eq!(d.reason, GovernorReason::GlobalQuotaExhausted);
    }

    #[test]
    fn governor_connector_quota_reject_does_not_consume_global_quota() {
        let mut config = ConnectorGovernorConfig::default();
        config.global_quota = QuotaConfig {
            max_actions: 2,
            window_ms: 60_000,
            warning_threshold: 0.5,
        };
        config.default_quota = QuotaConfig {
            max_actions: 1,
            window_ms: 60_000,
            warning_threshold: 0.5,
        };
        let mut gov = ConnectorGovernor::new(config);

        let slack = sample_action("slack", ConnectorActionKind::Notify);
        let github = sample_action("github", ConnectorActionKind::Notify);

        assert_eq!(gov.evaluate(&slack, 1000).verdict, GovernorVerdict::Allow);
        let second_slack = gov.evaluate(&slack, 2000);
        assert_eq!(second_slack.verdict, GovernorVerdict::Reject);
        assert_eq!(second_slack.reason, GovernorReason::ConnectorQuotaExhausted);

        // Should still be allowed because only one global quota slot was actually used.
        assert_eq!(gov.evaluate(&github, 3000).verdict, GovernorVerdict::Allow);
    }

    // ---- Stress test ----

    #[test]
    fn governor_stress_many_connectors() {
        let mut gov = ConnectorGovernor::new(ConnectorGovernorConfig::default());
        for i in 0..100 {
            let action = sample_action(&format!("connector-{i}"), ConnectorActionKind::Notify);
            let d = gov.evaluate(&action, i * 100);
            assert!(d.is_allowed());
        }
        let snap = gov.snapshot(10000);
        assert_eq!(snap.connectors.len(), 100);
        assert_eq!(snap.telemetry.evaluations, 100);
    }

    #[test]
    fn governor_stress_rapid_fire_single_connector() {
        let mut config = ConnectorGovernorConfig::default();
        config.default_rate_limit = TokenBucketConfig {
            capacity: 50,
            refill_rate: 10,
            refill_interval_ms: 1000,
        };
        let mut gov = ConnectorGovernor::new(config);
        let action = sample_action("slack", ConnectorActionKind::Notify);

        let mut allowed = 0u64;
        let mut throttled = 0u64;
        for i in 0..100 {
            let d = gov.evaluate(&action, 1000 + i * 10);
            match d.verdict {
                GovernorVerdict::Allow => allowed += 1,
                GovernorVerdict::Throttle => throttled += 1,
                GovernorVerdict::Reject => {}
            }
        }
        // Should get ~50 allowed (bucket capacity) and ~50 throttled
        assert!(allowed >= 40, "expected ~50 allows, got {allowed}");
        assert!(throttled >= 40, "expected ~50 throttles, got {throttled}");
    }
}
