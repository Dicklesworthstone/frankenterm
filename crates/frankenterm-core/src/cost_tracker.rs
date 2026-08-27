//! Per-pane cost aggregation for quota-aware agent scheduling (ft-2dss0).
//!
//! Tracks API usage costs per pane and per provider, supports:
//! - Recording cost events from caut usage data
//! - Aggregating per-provider and per-pane totals
//! - Budget alerts (configurable thresholds)
//! - Serializable snapshots for Robot Mode / MCP API
//! - Proxy-based attribution estimates grouped by bead, mission, workflow, or
//!   correlation id
//!
//! Attribution estimates are operator guidance only. They are labeled
//! `proxy_estimate`, use the `pane_activity_proxy_non_attested` methodology,
//! and set `attestation_eligible = false` so they cannot be confused with
//! billing truth or release evidence.
//!
//! # Integration
//!
//! The [`CostTracker`] sits alongside the rate-limit tracker and feeds the
//! cost dashboard:
//!
//! ```text
//! caut usage data → CostTracker.record_usage()
//!                          ↓
//!                  CostTracker.provider_summary()  → Dashboard
//!                  CostTracker.pane_summary()      → Per-pane view
//!                  CostTracker.budget_alerts()     → Alerts
//!                  CostTracker.attribution_estimates()
//!                                                → Dashboard / Prometheus
//! ```

use crate::patterns::AgentType;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Stable label for proxy-based cost-attribution estimates.
pub const COST_ATTRIBUTION_ESTIMATE_LABEL: &str = "proxy_estimate";
/// Methodology tag for pane-activity proxy estimates.
pub const COST_ATTRIBUTION_METHODOLOGY: &str = "pane_activity_proxy_non_attested";

// =============================================================================
// Telemetry
// =============================================================================

/// Operational telemetry counters for the cost tracker.
///
/// All counters are plain `u64` because `CostTracker` uses `&mut self`
/// for mutation — no atomic operations needed.
#[derive(Debug, Clone, Default)]
pub struct CostTelemetry {
    /// Total record_usage calls.
    pub usages_recorded: u64,
    /// Usage records rejected before aggregation because the cost was invalid.
    pub invalid_usages_rejected: u64,
    /// Panes evicted via LRU when MAX_TRACKED_PANES exceeded.
    pub panes_evicted_lru: u64,
    /// Panes explicitly removed via remove_pane().
    pub panes_removed: u64,
    /// Budget alert evaluations performed.
    pub alert_evaluations: u64,
    /// Budget alerts triggered.
    pub alerts_triggered: u64,
}

impl CostTelemetry {
    /// Create a new telemetry instance with all counters at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the current counter values.
    #[must_use]
    pub fn snapshot(&self) -> CostTelemetrySnapshot {
        CostTelemetrySnapshot {
            usages_recorded: self.usages_recorded,
            invalid_usages_rejected: self.invalid_usages_rejected,
            panes_evicted_lru: self.panes_evicted_lru,
            panes_removed: self.panes_removed,
            alert_evaluations: self.alert_evaluations,
            alerts_triggered: self.alerts_triggered,
        }
    }

    fn record_usage_call(&mut self) {
        self.usages_recorded = self.usages_recorded.saturating_add(1);
    }

    fn record_invalid_usage_rejected(&mut self) {
        self.invalid_usages_rejected = self.invalid_usages_rejected.saturating_add(1);
    }

    fn record_pane_evicted_lru(&mut self) {
        self.panes_evicted_lru = self.panes_evicted_lru.saturating_add(1);
    }

    fn record_pane_removed(&mut self) {
        self.panes_removed = self.panes_removed.saturating_add(1);
    }

    fn record_alert_evaluation(&mut self) {
        self.alert_evaluations = self.alert_evaluations.saturating_add(1);
    }

    fn record_alert_triggered(&mut self) {
        self.alerts_triggered = self.alerts_triggered.saturating_add(1);
    }
}

/// Serializable snapshot of cost tracker telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostTelemetrySnapshot {
    /// Total record_usage calls.
    pub usages_recorded: u64,
    /// Usage records rejected before aggregation because the cost was invalid.
    pub invalid_usages_rejected: u64,
    /// Panes evicted via LRU when MAX_TRACKED_PANES exceeded.
    pub panes_evicted_lru: u64,
    /// Panes explicitly removed via remove_pane().
    pub panes_removed: u64,
    /// Budget alert evaluations performed.
    pub alert_evaluations: u64,
    /// Budget alerts triggered.
    pub alerts_triggered: u64,
}

// =============================================================================
// Configuration
// =============================================================================

/// Budget threshold configuration for a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetThreshold {
    /// Provider agent type (Codex, ClaudeCode, Gemini, etc.).
    pub agent_type: String,
    /// Maximum allowed cost in USD for this provider per period.
    pub max_cost_usd: f64,
    /// Warning threshold as a fraction of max_cost_usd (0.0..1.0).
    /// E.g., 0.8 means alert when 80% of budget is consumed.
    pub warning_fraction: f64,
}

impl BudgetThreshold {
    /// Create a new budget threshold.
    #[must_use]
    pub fn new(agent_type: impl Into<String>, max_cost_usd: f64, warning_fraction: f64) -> Self {
        Self {
            agent_type: agent_type.into(),
            max_cost_usd: Self::normalize_max_cost_usd(max_cost_usd),
            warning_fraction: Self::normalize_warning_fraction(warning_fraction),
        }
    }

    fn normalized(mut self) -> Self {
        self.max_cost_usd = Self::normalize_max_cost_usd(self.max_cost_usd);
        self.warning_fraction = Self::normalize_warning_fraction(self.warning_fraction);
        self
    }

    fn normalize_max_cost_usd(max_cost_usd: f64) -> f64 {
        if max_cost_usd.is_finite() && max_cost_usd > 0.0 {
            max_cost_usd
        } else {
            0.0
        }
    }

    fn normalize_warning_fraction(warning_fraction: f64) -> f64 {
        if warning_fraction.is_finite() {
            warning_fraction.clamp(0.0, 1.0)
        } else {
            1.0
        }
    }
}

/// Configuration for the cost tracker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostTrackerConfig {
    /// Per-provider budget thresholds.
    pub budgets: Vec<BudgetThreshold>,
}

impl CostTrackerConfig {
    /// Return a copy of this config with all numeric thresholds finite and bounded.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.budgets = self
            .budgets
            .into_iter()
            .map(BudgetThreshold::normalized)
            .collect();
        self
    }
}

// =============================================================================
// Cost data types
// =============================================================================

// Canonical value in TuningConfig::PolicyTuning.
const MAX_TRACKED_PANES: usize = crate::tuning_config::PolicyTuning::DEFAULT_COST_TRACKER_MAX_PANES;

/// A single usage cost record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageCostRecord {
    /// Pane where the usage occurred.
    pub pane_id: u64,
    /// Agent type (maps to LLM provider).
    pub agent_type: String,
    /// Tokens consumed in this usage event.
    pub tokens_used: u64,
    /// Estimated cost in USD for this usage event.
    pub cost_usd: f64,
    /// When this usage was recorded (epoch milliseconds).
    pub recorded_at_ms: i64,
}

/// Per-pane accumulated cost state.
#[derive(Debug, Clone)]
struct PaneCostState {
    agent_type: AgentType,
    /// Total tokens used by this pane.
    total_tokens: u64,
    /// Total cost in USD accumulated by this pane.
    total_cost_usd: f64,
    /// Number of usage records.
    record_count: u64,
    /// Last update timestamp (epoch ms).
    last_updated_ms: i64,
}

impl PaneCostState {
    fn new(agent_type: AgentType) -> Self {
        Self {
            agent_type,
            total_tokens: 0,
            total_cost_usd: 0.0,
            record_count: 0,
            last_updated_ms: 0,
        }
    }

    fn record(&mut self, tokens: u64, cost_usd: f64, at_ms: i64) {
        self.total_tokens = self.total_tokens.saturating_add(tokens);
        self.total_cost_usd += cost_usd;
        self.record_count = self.record_count.saturating_add(1);
        if at_ms > self.last_updated_ms {
            self.last_updated_ms = at_ms;
        }
    }
}

// =============================================================================
// Summaries (serializable output)
// =============================================================================

/// Per-provider cost summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCostSummary {
    /// Provider agent type name.
    pub agent_type: String,
    /// Total tokens consumed across all panes for this provider.
    pub total_tokens: u64,
    /// Total cost in USD across all panes for this provider.
    pub total_cost_usd: f64,
    /// Number of active panes for this provider.
    pub pane_count: usize,
    /// Total usage records for this provider.
    pub record_count: u64,
}

/// Per-pane cost summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneCostSummary {
    /// Pane ID.
    pub pane_id: u64,
    /// Agent type for this pane.
    pub agent_type: String,
    /// Total tokens consumed by this pane.
    pub total_tokens: u64,
    /// Total cost in USD for this pane.
    pub total_cost_usd: f64,
    /// Number of usage records.
    pub record_count: u64,
    /// Last update timestamp (epoch ms).
    pub last_updated_ms: i64,
}

/// Attribution target for proxy-based cost estimates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostAttributionKind {
    /// Attribute the estimate to a Beads issue id.
    Bead,
    /// Attribute the estimate to a mission id.
    Mission,
    /// Attribute the estimate to a workflow id when no mission id exists.
    Workflow,
    /// Attribute the estimate to a correlation id when it is the only stable key.
    Correlation,
}

impl CostAttributionKind {
    /// Stable snake_case label used in JSON and Prometheus labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bead => "bead",
            Self::Mission => "mission",
            Self::Workflow => "workflow",
            Self::Correlation => "correlation",
        }
    }
}

/// Activity proxy sample for estimating cost attribution.
///
/// This is intentionally estimate-only. The values may be derived from pane
/// bytes, active seconds, detection counts, or token proxies; they are not a
/// billing source of truth and must not feed release-attestation slots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostAttributionProxyRecord {
    /// Target namespace for this estimate.
    pub kind: CostAttributionKind,
    /// Bead, mission, workflow, or correlation id.
    pub attribution_id: String,
    /// Optional pane that produced the activity.
    pub pane_id: Option<u64>,
    /// Captured output bytes used as an activity proxy.
    pub bytes_captured: u64,
    /// Active-state seconds used as an activity proxy.
    pub active_seconds: f64,
    /// Detection/event count used as an activity proxy.
    pub detection_count: u64,
    /// Estimated token count, when a token proxy is available.
    pub estimated_tokens: u64,
    /// Estimated USD cost derived from proxy methodology.
    pub estimated_cost_usd: f64,
    /// When this estimate was recorded (epoch milliseconds).
    pub recorded_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CostAttributionKey {
    kind: CostAttributionKind,
    attribution_id: String,
}

#[derive(Debug, Clone, Default)]
struct CostAttributionState {
    pane_ids: BTreeSet<u64>,
    bytes_captured: u64,
    active_seconds: f64,
    detection_count: u64,
    estimated_tokens: u64,
    estimated_cost_usd: f64,
    record_count: u64,
    last_updated_ms: i64,
}

impl CostAttributionState {
    fn record(&mut self, sample: &CostAttributionProxyRecord) {
        if let Some(pane_id) = sample.pane_id {
            self.pane_ids.insert(pane_id);
        }
        self.bytes_captured = self.bytes_captured.saturating_add(sample.bytes_captured);
        self.active_seconds = finite_saturating_add(self.active_seconds, sample.active_seconds);
        self.detection_count = self.detection_count.saturating_add(sample.detection_count);
        self.estimated_tokens = self
            .estimated_tokens
            .saturating_add(sample.estimated_tokens);
        self.estimated_cost_usd =
            finite_saturating_add(self.estimated_cost_usd, sample.estimated_cost_usd);
        self.record_count = self.record_count.saturating_add(1);
        if sample.recorded_at_ms > self.last_updated_ms {
            self.last_updated_ms = sample.recorded_at_ms;
        }
    }
}

/// Aggregated, explicitly labeled cost-attribution estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostAttributionEstimateSummary {
    /// Target namespace for this estimate.
    pub kind: CostAttributionKind,
    /// Bead, mission, workflow, or correlation id.
    pub attribution_id: String,
    /// Stable label that keeps this out of actual-billing and attestation paths.
    pub estimate_label: String,
    /// Human/machine-readable methodology tag.
    pub methodology: String,
    /// Always false: proxy estimates are operator guidance, not proof artifacts.
    pub attestation_eligible: bool,
    /// Distinct panes contributing samples.
    pub pane_count: usize,
    /// Total captured bytes used as an activity proxy.
    pub bytes_captured: u64,
    /// Total active-state seconds used as an activity proxy.
    pub active_seconds: f64,
    /// Total detection/event count used as an activity proxy.
    pub detection_count: u64,
    /// Estimated tokens from proxy sources.
    pub estimated_tokens: u64,
    /// Estimated USD cost from proxy sources.
    pub estimated_cost_usd: f64,
    /// Number of proxy samples folded into this estimate.
    pub record_count: u64,
    /// Last sample timestamp (epoch milliseconds).
    pub last_updated_ms: i64,
}

/// Budget alert for a provider exceeding a cost threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetAlert {
    /// Provider agent type name.
    pub agent_type: String,
    /// Current total cost in USD.
    pub current_cost_usd: f64,
    /// Budget limit in USD.
    pub budget_limit_usd: f64,
    /// Usage fraction (0.0..inf, >1.0 means over budget).
    pub usage_fraction: f64,
    /// Severity: "warning" (above warning threshold) or "critical" (above budget).
    pub severity: AlertSeverity,
}

/// Alert severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    /// Above the warning threshold but below the budget limit.
    Warning,
    /// At or above the budget limit.
    Critical,
}

/// Full cost dashboard snapshot — all providers, panes, and alerts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostDashboardSnapshot {
    /// Per-provider summaries.
    pub providers: Vec<ProviderCostSummary>,
    /// Per-pane summaries (sorted by pane_id).
    pub panes: Vec<PaneCostSummary>,
    /// Active budget alerts.
    pub alerts: Vec<BudgetAlert>,
    /// Grand total cost across all providers.
    pub grand_total_cost_usd: f64,
    /// Grand total tokens across all providers.
    pub grand_total_tokens: u64,
}

// =============================================================================
// Tracker
// =============================================================================

/// Tracks per-pane and per-provider API usage costs.
///
/// Thread-safe usage: wrap in `Arc<Mutex<CostTracker>>` for concurrent access.
#[derive(Debug)]
pub struct CostTracker {
    panes: BTreeMap<u64, PaneCostState>,
    attribution_estimates: BTreeMap<CostAttributionKey, CostAttributionState>,
    /// Insertion order for LRU eviction when MAX_TRACKED_PANES exceeded.
    pane_order: VecDeque<u64>,
    /// Budget configuration.
    config: CostTrackerConfig,
    /// Operational telemetry counters.
    telemetry: CostTelemetry,
}

impl CostTracker {
    /// Create a new cost tracker with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self {
            panes: BTreeMap::new(),
            attribution_estimates: BTreeMap::new(),
            pane_order: VecDeque::new(),
            config: CostTrackerConfig::default(),
            telemetry: CostTelemetry::new(),
        }
    }

    /// Create a new cost tracker with specific budget configuration.
    #[must_use]
    pub fn with_config(config: CostTrackerConfig) -> Self {
        Self {
            panes: BTreeMap::new(),
            attribution_estimates: BTreeMap::new(),
            pane_order: VecDeque::new(),
            config: config.normalized(),
            telemetry: CostTelemetry::new(),
        }
    }

    /// Record a usage cost event for a pane.
    pub fn record_usage(
        &mut self,
        pane_id: u64,
        agent_type: AgentType,
        tokens: u64,
        cost_usd: f64,
        at_ms: i64,
    ) {
        self.telemetry.record_usage_call();

        let current_cost = self
            .panes
            .get(&pane_id)
            .map_or(0.0, |state| state.total_cost_usd);
        if !cost_usd.is_finite() || cost_usd < 0.0 || !(current_cost + cost_usd).is_finite() {
            self.telemetry.record_invalid_usage_rejected();
            return;
        }

        // Evict oldest pane if at capacity
        if !self.panes.contains_key(&pane_id) && self.panes.len() >= MAX_TRACKED_PANES {
            if let Some(oldest_id) = self.pane_order.pop_front() {
                self.panes.remove(&oldest_id);
                self.telemetry.record_pane_evicted_lru();
            }
        }

        let state = self
            .panes
            .entry(pane_id)
            .or_insert_with(|| PaneCostState::new(agent_type));
        state.agent_type = agent_type;
        state.record(tokens, cost_usd, at_ms);
        self.touch_pane_order(pane_id);
    }

    /// Record a proxy-based cost-attribution estimate.
    ///
    /// These estimates are deliberately separated from [`Self::record_usage`]:
    /// they can power operator panels and Prometheus-style gauges, but they are
    /// not billing truth and are never attestation-eligible.
    pub fn record_attribution_estimate(&mut self, sample: CostAttributionProxyRecord) {
        let attribution_id = sample.attribution_id.trim();
        if attribution_id.is_empty()
            || !sample.active_seconds.is_finite()
            || sample.active_seconds < 0.0
            || !sample.estimated_cost_usd.is_finite()
            || sample.estimated_cost_usd < 0.0
        {
            self.telemetry.record_invalid_usage_rejected();
            return;
        }

        let key = CostAttributionKey {
            kind: sample.kind,
            attribution_id: attribution_id.to_string(),
        };
        self.attribution_estimates
            .entry(key)
            .or_default()
            .record(&sample);
    }

    /// Get cost summary for a specific provider.
    #[must_use]
    pub fn provider_summary(&self, agent_type: AgentType) -> ProviderCostSummary {
        let mut total_tokens = 0u64;
        let mut total_cost = 0.0f64;
        let mut pane_count = 0usize;
        let mut record_count = 0u64;

        for state in self.panes.values() {
            if state.agent_type == agent_type {
                total_tokens = total_tokens.saturating_add(state.total_tokens);
                total_cost += state.total_cost_usd;
                pane_count += 1;
                record_count = record_count.saturating_add(state.record_count);
            }
        }

        ProviderCostSummary {
            agent_type: agent_type.to_string(),
            total_tokens,
            total_cost_usd: total_cost,
            pane_count,
            record_count,
        }
    }

    /// Get cost summaries for all tracked providers.
    #[must_use]
    pub fn all_provider_summaries(&self) -> Vec<ProviderCostSummary> {
        let mut seen = Vec::new();
        for state in self.panes.values() {
            if !seen.contains(&state.agent_type) {
                seen.push(state.agent_type);
            }
        }
        seen.into_iter()
            .map(|at| self.provider_summary(at))
            .collect()
    }

    /// Get cost summary for a specific pane.
    #[must_use]
    pub fn pane_summary(&self, pane_id: u64) -> Option<PaneCostSummary> {
        self.panes.get(&pane_id).map(|state| PaneCostSummary {
            pane_id,
            agent_type: state.agent_type.to_string(),
            total_tokens: state.total_tokens,
            total_cost_usd: state.total_cost_usd,
            record_count: state.record_count,
            last_updated_ms: state.last_updated_ms,
        })
    }

    /// Get cost summaries for all tracked panes (sorted by pane_id).
    #[must_use]
    pub fn all_pane_summaries(&self) -> Vec<PaneCostSummary> {
        self.panes
            .iter()
            .map(|(&pane_id, state)| PaneCostSummary {
                pane_id,
                agent_type: state.agent_type.to_string(),
                total_tokens: state.total_tokens,
                total_cost_usd: state.total_cost_usd,
                record_count: state.record_count,
                last_updated_ms: state.last_updated_ms,
            })
            .collect()
    }

    /// Return proxy-based cost-attribution estimates sorted by kind and id.
    #[must_use]
    pub fn attribution_estimates(&self) -> Vec<CostAttributionEstimateSummary> {
        self.attribution_estimates
            .iter()
            .map(|(key, state)| CostAttributionEstimateSummary {
                kind: key.kind,
                attribution_id: key.attribution_id.clone(),
                estimate_label: COST_ATTRIBUTION_ESTIMATE_LABEL.to_string(),
                methodology: COST_ATTRIBUTION_METHODOLOGY.to_string(),
                attestation_eligible: false,
                pane_count: state.pane_ids.len(),
                bytes_captured: state.bytes_captured,
                active_seconds: state.active_seconds,
                detection_count: state.detection_count,
                estimated_tokens: state.estimated_tokens,
                estimated_cost_usd: state.estimated_cost_usd,
                record_count: state.record_count,
                last_updated_ms: state.last_updated_ms,
            })
            .collect()
    }

    /// Evaluate budget thresholds and return active alerts.
    #[must_use]
    pub fn budget_alerts(&mut self) -> Vec<BudgetAlert> {
        self.telemetry.record_alert_evaluation();
        let summaries = self.all_provider_summaries();
        let mut alerts = Vec::new();

        for threshold in &self.config.budgets {
            let matching = summaries
                .iter()
                .find(|s| s.agent_type == threshold.agent_type);
            if let Some(summary) = matching {
                if threshold.max_cost_usd > 0.0 {
                    let fraction =
                        finite_usage_fraction(summary.total_cost_usd, threshold.max_cost_usd);
                    if fraction >= 1.0 {
                        self.telemetry.record_alert_triggered();
                        alerts.push(BudgetAlert {
                            agent_type: threshold.agent_type.clone(),
                            current_cost_usd: summary.total_cost_usd,
                            budget_limit_usd: threshold.max_cost_usd,
                            usage_fraction: fraction,
                            severity: AlertSeverity::Critical,
                        });
                    } else if fraction >= threshold.warning_fraction {
                        self.telemetry.record_alert_triggered();
                        alerts.push(BudgetAlert {
                            agent_type: threshold.agent_type.clone(),
                            current_cost_usd: summary.total_cost_usd,
                            budget_limit_usd: threshold.max_cost_usd,
                            usage_fraction: fraction,
                            severity: AlertSeverity::Warning,
                        });
                    }
                }
            }
        }

        alerts
    }

    /// Generate a full dashboard snapshot.
    #[must_use]
    pub fn dashboard_snapshot(&mut self) -> CostDashboardSnapshot {
        let providers = self.all_provider_summaries();
        let panes = self.all_pane_summaries();
        let alerts = self.budget_alerts();

        let grand_total_cost_usd: f64 = providers.iter().map(|p| p.total_cost_usd).sum();
        let grand_total_tokens: u64 = providers.iter().map(|p| p.total_tokens).sum();

        CostDashboardSnapshot {
            providers,
            panes,
            alerts,
            grand_total_cost_usd,
            grand_total_tokens,
        }
    }

    /// Grand total cost across all panes.
    #[must_use]
    pub fn grand_total_cost(&self) -> f64 {
        self.panes.values().map(|s| s.total_cost_usd).sum()
    }

    /// Grand total tokens across all panes.
    #[must_use]
    pub fn grand_total_tokens(&self) -> u64 {
        self.panes
            .values()
            .map(|s| s.total_tokens)
            .fold(0u64, u64::saturating_add)
    }

    /// Remove a pane from tracking (e.g., when pane is closed).
    pub fn remove_pane(&mut self, pane_id: u64) {
        if self.panes.remove(&pane_id).is_some() {
            self.telemetry.record_pane_removed();
        }
        self.pane_order.retain(|&id| id != pane_id);
    }

    /// Total number of tracked panes.
    #[must_use]
    pub fn tracked_pane_count(&self) -> usize {
        self.panes.len()
    }

    /// Access the operational telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &CostTelemetry {
        &self.telemetry
    }

    /// Update budget configuration.
    pub fn set_config(&mut self, config: CostTrackerConfig) {
        self.config = config.normalized();
    }

    fn touch_pane_order(&mut self, pane_id: u64) {
        if let Some(pos) = self.pane_order.iter().position(|&id| id == pane_id) {
            self.pane_order.remove(pos);
        }
        self.pane_order.push_back(pane_id);
    }
}

impl Default for CostTracker {
    fn default() -> Self {
        Self::new()
    }
}

fn finite_usage_fraction(current_cost_usd: f64, max_cost_usd: f64) -> f64 {
    let fraction = current_cost_usd / max_cost_usd;
    if fraction.is_finite() {
        fraction
    } else {
        f64::MAX
    }
}

fn finite_saturating_add(current: f64, delta: f64) -> f64 {
    let next = current + delta;
    if next.is_finite() { next } else { f64::MAX }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn tracker_new_is_empty() {
        let tracker = CostTracker::new();
        assert_eq!(tracker.tracked_pane_count(), 0);
        assert!(tracker.grand_total_cost() < f64::EPSILON);
        assert_eq!(tracker.grand_total_tokens(), 0);
    }

    #[test]
    fn record_usage_creates_pane_state() {
        let mut tracker = CostTracker::new();
        tracker.record_usage(1, AgentType::Codex, 1000, 0.03, 100);
        assert_eq!(tracker.tracked_pane_count(), 1);
        assert!((tracker.grand_total_cost() - 0.03).abs() < f64::EPSILON);
        assert_eq!(tracker.grand_total_tokens(), 1000);
    }

    #[test]
    fn record_usage_accumulates() {
        let mut tracker = CostTracker::new();
        tracker.record_usage(1, AgentType::Codex, 1000, 0.03, 100);
        tracker.record_usage(1, AgentType::Codex, 2000, 0.06, 200);
        assert_eq!(tracker.tracked_pane_count(), 1);
        assert!((tracker.grand_total_cost() - 0.09).abs() < 1e-10);
        assert_eq!(tracker.grand_total_tokens(), 3000);
    }

    #[test]
    fn attribution_estimates_group_by_bead_and_stay_non_attested() {
        let mut tracker = CostTracker::new();

        tracker.record_attribution_estimate(CostAttributionProxyRecord {
            kind: CostAttributionKind::Bead,
            attribution_id: " ft-123 ".to_string(),
            pane_id: Some(7),
            bytes_captured: 2_048,
            active_seconds: 12.5,
            detection_count: 3,
            estimated_tokens: 800,
            estimated_cost_usd: 0.04,
            recorded_at_ms: 100,
        });
        tracker.record_attribution_estimate(CostAttributionProxyRecord {
            kind: CostAttributionKind::Bead,
            attribution_id: "ft-123".to_string(),
            pane_id: Some(8),
            bytes_captured: 1_024,
            active_seconds: 7.5,
            detection_count: 2,
            estimated_tokens: 400,
            estimated_cost_usd: 0.02,
            recorded_at_ms: 200,
        });

        let estimates = tracker.attribution_estimates();
        assert_eq!(estimates.len(), 1);
        let estimate = &estimates[0];
        assert_eq!(estimate.kind, CostAttributionKind::Bead);
        assert_eq!(estimate.attribution_id, "ft-123");
        assert_eq!(estimate.estimate_label, "proxy_estimate");
        assert_eq!(estimate.methodology, "pane_activity_proxy_non_attested");
        assert!(!estimate.attestation_eligible);
        assert_eq!(estimate.pane_count, 2);
        assert_eq!(estimate.bytes_captured, 3_072);
        assert!((estimate.active_seconds - 20.0).abs() < f64::EPSILON);
        assert_eq!(estimate.detection_count, 5);
        assert_eq!(estimate.estimated_tokens, 1_200);
        assert!((estimate.estimated_cost_usd - 0.06).abs() < f64::EPSILON);
        assert_eq!(estimate.record_count, 2);
        assert_eq!(estimate.last_updated_ms, 200);
    }

    #[test]
    fn attribution_estimates_reject_blank_or_non_finite_samples() {
        let mut tracker = CostTracker::new();

        tracker.record_attribution_estimate(CostAttributionProxyRecord {
            kind: CostAttributionKind::Mission,
            attribution_id: " ".to_string(),
            pane_id: Some(1),
            bytes_captured: 1,
            active_seconds: 1.0,
            detection_count: 1,
            estimated_tokens: 1,
            estimated_cost_usd: 0.01,
            recorded_at_ms: 1,
        });
        tracker.record_attribution_estimate(CostAttributionProxyRecord {
            kind: CostAttributionKind::Mission,
            attribution_id: "mission-a".to_string(),
            pane_id: Some(1),
            bytes_captured: 1,
            active_seconds: f64::NAN,
            detection_count: 1,
            estimated_tokens: 1,
            estimated_cost_usd: 0.01,
            recorded_at_ms: 1,
        });
        tracker.record_attribution_estimate(CostAttributionProxyRecord {
            kind: CostAttributionKind::Mission,
            attribution_id: "mission-a".to_string(),
            pane_id: Some(1),
            bytes_captured: 1,
            active_seconds: 1.0,
            detection_count: 1,
            estimated_tokens: 1,
            estimated_cost_usd: f64::INFINITY,
            recorded_at_ms: 1,
        });

        assert_eq!(tracker.attribution_estimates(), [] as [cost_tracker::CostAttributionEstimateSummary; 0]);
        assert_eq!(tracker.telemetry().invalid_usages_rejected, 3);
    }

    #[test]
    fn multiple_panes_accumulate() {
        let mut tracker = CostTracker::new();
        tracker.record_usage(1, AgentType::Codex, 1000, 0.03, 100);
        tracker.record_usage(2, AgentType::ClaudeCode, 500, 0.015, 100);
        tracker.record_usage(3, AgentType::Codex, 2000, 0.06, 200);
        assert_eq!(tracker.tracked_pane_count(), 3);
        assert!((tracker.grand_total_cost() - 0.105).abs() < 1e-10);
        assert_eq!(tracker.grand_total_tokens(), 3500);
    }

    #[test]
    fn provider_summary_isolates_by_type() {
        let mut tracker = CostTracker::new();
        tracker.record_usage(1, AgentType::Codex, 1000, 0.03, 100);
        tracker.record_usage(2, AgentType::ClaudeCode, 500, 0.015, 100);
        tracker.record_usage(3, AgentType::Codex, 2000, 0.06, 200);

        let codex = tracker.provider_summary(AgentType::Codex);
        assert_eq!(codex.pane_count, 2);
        assert_eq!(codex.total_tokens, 3000);
        assert!((codex.total_cost_usd - 0.09).abs() < 1e-10);
        assert_eq!(codex.record_count, 2);

        let claude = tracker.provider_summary(AgentType::ClaudeCode);
        assert_eq!(claude.pane_count, 1);
        assert_eq!(claude.total_tokens, 500);
        assert!((claude.total_cost_usd - 0.015).abs() < 1e-10);
    }

    #[test]
    fn pane_summary_returns_none_for_unknown() {
        let tracker = CostTracker::new();
        assert!(tracker.pane_summary(999).is_none());
    }

    #[test]
    fn pane_summary_returns_data() {
        let mut tracker = CostTracker::new();
        tracker.record_usage(42, AgentType::Gemini, 5000, 0.10, 300);

        let summary = tracker.pane_summary(42).unwrap();
        assert_eq!(summary.pane_id, 42);
        assert_eq!(summary.total_tokens, 5000);
        assert!((summary.total_cost_usd - 0.10).abs() < f64::EPSILON);
        assert_eq!(summary.record_count, 1);
        assert_eq!(summary.last_updated_ms, 300);
    }

    #[test]
    fn remove_pane_decreases_count() {
        let mut tracker = CostTracker::new();
        tracker.record_usage(1, AgentType::Codex, 1000, 0.03, 100);
        tracker.record_usage(2, AgentType::Codex, 2000, 0.06, 200);
        assert_eq!(tracker.tracked_pane_count(), 2);

        tracker.remove_pane(1);
        assert_eq!(tracker.tracked_pane_count(), 1);
        assert!((tracker.grand_total_cost() - 0.06).abs() < f64::EPSILON);
    }

    #[test]
    fn remove_nonexistent_pane_is_noop() {
        let mut tracker = CostTracker::new();
        tracker.remove_pane(999);
        assert_eq!(tracker.telemetry().panes_removed, 0);
    }

    #[test]
    fn lru_eviction_at_capacity() {
        let mut tracker = CostTracker::new();
        // Fill to capacity
        for i in 0..MAX_TRACKED_PANES as u64 {
            tracker.record_usage(i, AgentType::Codex, 100, 0.001, i as i64);
        }
        assert_eq!(tracker.tracked_pane_count(), MAX_TRACKED_PANES);

        // One more triggers eviction
        tracker.record_usage(
            MAX_TRACKED_PANES as u64,
            AgentType::Codex,
            100,
            0.001,
            MAX_TRACKED_PANES as i64,
        );
        assert_eq!(tracker.tracked_pane_count(), MAX_TRACKED_PANES);
        assert_eq!(tracker.telemetry().panes_evicted_lru, 1);

        // Oldest pane (0) should be evicted
        assert!(tracker.pane_summary(0).is_none());
        // Newest pane should exist
        assert!(tracker.pane_summary(MAX_TRACKED_PANES as u64).is_some());
    }

    #[test]
    fn budget_alert_warning() {
        let config = CostTrackerConfig {
            budgets: vec![BudgetThreshold::new("codex", 10.0, 0.8)],
        };
        let mut tracker = CostTracker::with_config(config);
        // 85% of budget
        tracker.record_usage(1, AgentType::Codex, 100_000, 8.5, 100);

        let alerts = tracker.budget_alerts();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, AlertSeverity::Warning);
        assert!((alerts[0].usage_fraction - 0.85).abs() < 1e-10);
    }

    #[test]
    fn budget_alert_critical() {
        let config = CostTrackerConfig {
            budgets: vec![BudgetThreshold::new("codex", 10.0, 0.8)],
        };
        let mut tracker = CostTracker::with_config(config);
        // Over budget
        tracker.record_usage(1, AgentType::Codex, 200_000, 12.0, 100);

        let alerts = tracker.budget_alerts();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, AlertSeverity::Critical);
        assert!(alerts[0].usage_fraction >= 1.0);
    }

    #[test]
    fn budget_no_alert_when_under_threshold() {
        let config = CostTrackerConfig {
            budgets: vec![BudgetThreshold::new("codex", 10.0, 0.8)],
        };
        let mut tracker = CostTracker::with_config(config);
        // 50% of budget — no alert
        tracker.record_usage(1, AgentType::Codex, 50_000, 5.0, 100);

        let alerts = tracker.budget_alerts();
        assert!(alerts.is_empty());
    }

    #[test]
    fn budget_no_alert_without_config() {
        let mut tracker = CostTracker::new();
        tracker.record_usage(1, AgentType::Codex, 1_000_000, 100.0, 100);

        let alerts = tracker.budget_alerts();
        assert!(alerts.is_empty());
    }

    #[test]
    fn dashboard_snapshot_aggregates_all() {
        let config = CostTrackerConfig {
            budgets: vec![
                BudgetThreshold::new("codex", 10.0, 0.8),
                BudgetThreshold::new("claude_code", 20.0, 0.9),
            ],
        };
        let mut tracker = CostTracker::with_config(config);
        tracker.record_usage(1, AgentType::Codex, 1000, 0.03, 100);
        tracker.record_usage(2, AgentType::ClaudeCode, 500, 0.015, 100);
        tracker.record_usage(3, AgentType::Codex, 2000, 0.06, 200);

        let snapshot = tracker.dashboard_snapshot();
        assert_eq!(snapshot.providers.len(), 2);
        assert_eq!(snapshot.panes.len(), 3);
        assert!(snapshot.alerts.is_empty()); // costs too low for alerts
        assert!((snapshot.grand_total_cost_usd - 0.105).abs() < 1e-10);
        assert_eq!(snapshot.grand_total_tokens, 3500);
    }

    #[test]
    fn all_provider_summaries_covers_all_types() {
        let mut tracker = CostTracker::new();
        tracker.record_usage(1, AgentType::Codex, 100, 0.01, 100);
        tracker.record_usage(2, AgentType::ClaudeCode, 200, 0.02, 100);
        tracker.record_usage(3, AgentType::Gemini, 300, 0.03, 100);

        let summaries = tracker.all_provider_summaries();
        assert_eq!(summaries.len(), 3);
    }

    #[test]
    fn all_pane_summaries_sorted_by_id() {
        let mut tracker = CostTracker::new();
        tracker.record_usage(42, AgentType::Codex, 100, 0.01, 100);
        tracker.record_usage(7, AgentType::Codex, 200, 0.02, 100);
        tracker.record_usage(99, AgentType::Codex, 300, 0.03, 100);

        let summaries = tracker.all_pane_summaries();
        assert_eq!(summaries.len(), 3);
        // BTreeMap guarantees sorted order
        assert_eq!(summaries[0].pane_id, 7);
        assert_eq!(summaries[1].pane_id, 42);
        assert_eq!(summaries[2].pane_id, 99);
    }

    #[test]
    fn telemetry_counts_operations() {
        let mut tracker = CostTracker::new();
        tracker.record_usage(1, AgentType::Codex, 100, 0.01, 100);
        tracker.record_usage(2, AgentType::Codex, 200, 0.02, 200);
        tracker.remove_pane(1);

        let snap = tracker.telemetry().snapshot();
        assert_eq!(snap.usages_recorded, 2);
        assert_eq!(snap.panes_removed, 1);
    }

    #[test]
    fn set_config_changes_thresholds() {
        let mut tracker = CostTracker::new();
        tracker.record_usage(1, AgentType::Codex, 100_000, 9.0, 100);

        // No config → no alerts
        assert!(tracker.budget_alerts().is_empty());

        // Set config → now alerts fire
        tracker.set_config(CostTrackerConfig {
            budgets: vec![BudgetThreshold::new("codex", 10.0, 0.8)],
        });
        let alerts = tracker.budget_alerts();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, AlertSeverity::Warning);
    }

    #[test]
    fn agent_type_update_on_re_registration() {
        let mut tracker = CostTracker::new();
        tracker.record_usage(1, AgentType::Codex, 100, 0.01, 100);
        // Same pane, different agent type
        tracker.record_usage(1, AgentType::ClaudeCode, 200, 0.02, 200);

        let summary = tracker.pane_summary(1).unwrap();
        assert_eq!(summary.agent_type, AgentType::ClaudeCode.to_string());
        assert_eq!(summary.total_tokens, 300);
        assert!((summary.total_cost_usd - 0.03).abs() < 1e-10);
    }

    #[test]
    fn cost_telemetry_snapshot_serde_roundtrip() {
        let snap = CostTelemetrySnapshot {
            usages_recorded: 42,
            invalid_usages_rejected: 0,
            panes_evicted_lru: 3,
            panes_removed: 1,
            alert_evaluations: 10,
            alerts_triggered: 2,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: CostTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, deserialized);
    }

    #[test]
    fn dashboard_snapshot_serde_roundtrip() {
        let config = CostTrackerConfig {
            budgets: vec![BudgetThreshold::new("codex", 10.0, 0.8)],
        };
        let mut tracker = CostTracker::with_config(config);
        tracker.record_usage(1, AgentType::Codex, 1000, 9.0, 100);

        let snapshot = tracker.dashboard_snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        let deserialized: CostDashboardSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.providers.len(), snapshot.providers.len());
        assert_eq!(deserialized.panes.len(), snapshot.panes.len());
        assert_eq!(deserialized.alerts.len(), snapshot.alerts.len());
    }

    #[test]
    fn warning_fraction_clamped() {
        let threshold = BudgetThreshold::new("codex", 10.0, 1.5);
        assert!((threshold.warning_fraction - 1.0).abs() < f64::EPSILON);

        let threshold = BudgetThreshold::new("codex", 10.0, -0.5);
        assert!(threshold.warning_fraction.abs() < f64::EPSILON);
    }

    #[test]
    fn budget_threshold_new_normalizes_invalid_numeric_limits() {
        for max_cost_usd in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let threshold = BudgetThreshold::new("codex", max_cost_usd, f64::NAN);
            assert!(threshold.max_cost_usd.abs() < f64::EPSILON);
            assert!((threshold.warning_fraction - 1.0).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn deserialized_budget_config_is_normalized_on_update() {
        let mut tracker = CostTracker::new();
        tracker.record_usage(1, AgentType::Codex, 100_000, 1.0, 100);
        tracker.set_config(CostTrackerConfig {
            budgets: vec![BudgetThreshold {
                agent_type: "codex".to_string(),
                max_cost_usd: f64::NAN,
                warning_fraction: f64::NAN,
            }],
        });

        let alerts = tracker.budget_alerts();
        assert!(alerts.is_empty());
    }

    proptest! {
        #[test]
        fn budget_threshold_alerts_keep_finite_fractions(
            max_cost_usd in any::<f64>(),
            warning_fraction in any::<f64>(),
            cost_usd in 0.0f64..1.0e9,
        ) {
            let config = CostTrackerConfig {
                budgets: vec![BudgetThreshold {
                    agent_type: "codex".to_string(),
                    max_cost_usd,
                    warning_fraction,
                }],
            };
            let mut tracker = CostTracker::with_config(config);
            tracker.record_usage(1, AgentType::Codex, 100_000, cost_usd, 100);

            for alert in tracker.budget_alerts() {
                prop_assert!(alert.budget_limit_usd.is_finite());
                prop_assert!(alert.budget_limit_usd > 0.0);
                prop_assert!(alert.usage_fraction.is_finite());
                prop_assert!(alert.usage_fraction >= 0.0);
            }
        }

        #[test]
        fn ft_nqy7d_telemetry_counters_saturate_at_u64_max(
            delta in 0u64..=3,
        ) {
            let mut tracker = CostTracker::with_config(CostTrackerConfig {
                budgets: vec![BudgetThreshold::new("codex", 1.0, 0.5)],
            });
            for pane_id in 0..MAX_TRACKED_PANES as u64 {
                tracker.record_usage(pane_id, AgentType::Codex, 1, 0.01, pane_id as i64);
            }

            let baseline = u64::MAX - delta;
            tracker.telemetry = CostTelemetry {
                usages_recorded: baseline,
                invalid_usages_rejected: baseline,
                panes_evicted_lru: baseline,
                panes_removed: baseline,
                alert_evaluations: baseline,
                alerts_triggered: baseline,
            };

            tracker.record_usage(42, AgentType::Codex, 1, f64::NAN, 1);
            tracker.record_usage(MAX_TRACKED_PANES as u64 + 1, AgentType::Codex, 1, 2.0, 2);
            tracker.remove_pane(MAX_TRACKED_PANES as u64 + 1);
            let _ = tracker.budget_alerts();

            let snapshot = tracker.telemetry().snapshot();
            prop_assert_eq!(snapshot.usages_recorded, baseline.saturating_add(2));
            prop_assert_eq!(snapshot.invalid_usages_rejected, baseline.saturating_add(1));
            prop_assert_eq!(snapshot.panes_evicted_lru, baseline.saturating_add(1));
            prop_assert_eq!(snapshot.panes_removed, baseline.saturating_add(1));
            prop_assert_eq!(snapshot.alert_evaluations, baseline.saturating_add(1));
            prop_assert_eq!(snapshot.alerts_triggered, baseline.saturating_add(1));
        }
    }

    #[test]
    fn zero_budget_produces_no_alert() {
        let config = CostTrackerConfig {
            budgets: vec![BudgetThreshold::new("codex", 0.0, 0.8)],
        };
        let mut tracker = CostTracker::with_config(config);
        tracker.record_usage(1, AgentType::Codex, 1000, 5.0, 100);

        // Zero budget → division by zero guard → no alert
        let alerts = tracker.budget_alerts();
        assert!(alerts.is_empty());
    }
}
