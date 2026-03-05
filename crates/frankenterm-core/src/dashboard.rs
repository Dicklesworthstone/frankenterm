//! Unified dashboard state aggregator for FrankenTerm (ft-3hbv9).
//!
//! Combines system health snapshots from multiple subsystems into a single
//! [`DashboardState`] suitable for TUI rendering, web API responses, and
//! robot-mode JSON output.
//!
//! # Data sources
//!
//! | Subsystem          | Snapshot type                  | Key signals                      |
//! |--------------------|--------------------------------|----------------------------------|
//! | Cost tracking      | `CostDashboardSnapshot`        | Per-provider spend, budget alerts |
//! | Quota gate         | `QuotaGateSnapshot`            | Launch verdicts, block counts    |
//! | Rate limits        | `ProviderRateLimitSummary`     | Cooldown status, limited panes   |
//! | Backpressure       | `BackpressureSnapshot`         | Queue tiers, paused panes        |
//! | Runtime health     | `RuntimeDoctorReport`          | Health checks, overall tier      |
//! | Storage pipeline   | `StoragePipelineSnapshot`      | Write lag, health tier           |
//!
//! # Usage
//!
//! ```rust,ignore
//! let mut dash = DashboardManager::new();
//! dash.update_costs(cost_tracker.dashboard_snapshot());
//! dash.update_backpressure(bp_manager.snapshot(&depths));
//! let state = dash.snapshot();
//! // Serialize for robot mode or pass to TUI renderer
//! let json = serde_json::to_string_pretty(&state).unwrap();
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::backpressure::{BackpressureSnapshot, BackpressureTier};
use crate::cost_tracker::{AlertSeverity, BudgetAlert, CostDashboardSnapshot};
use crate::quota_gate::{LaunchVerdict, QuotaGateSnapshot};
use crate::rate_limit_tracker::{ProviderRateLimitStatus, ProviderRateLimitSummary};

// =============================================================================
// System-level health
// =============================================================================

/// Coarse system health classification derived from all subsystem signals.
///
/// Ordered by severity (Green < Yellow < Red < Black) so `max()` gives the
/// worst-case across subsystems.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub enum SystemHealthTier {
    /// All subsystems nominal.
    #[default]
    Green,
    /// At least one subsystem at warning level.
    Yellow,
    /// At least one subsystem under heavy pressure.
    Red,
    /// Critical: system degraded, potential data loss or blocked operations.
    Black,
}

impl std::fmt::Display for SystemHealthTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Green => write!(f, "green"),
            Self::Yellow => write!(f, "yellow"),
            Self::Red => write!(f, "red"),
            Self::Black => write!(f, "black"),
        }
    }
}

impl From<BackpressureTier> for SystemHealthTier {
    fn from(tier: BackpressureTier) -> Self {
        match tier {
            BackpressureTier::Green => Self::Green,
            BackpressureTier::Yellow => Self::Yellow,
            BackpressureTier::Red => Self::Red,
            BackpressureTier::Black => Self::Black,
        }
    }
}

// =============================================================================
// Dashboard panels
// =============================================================================

/// Cost panel: per-provider spending and budget alerts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CostPanel {
    /// Per-provider cost summaries keyed by agent type name.
    pub providers: BTreeMap<String, ProviderCostView>,
    /// Active budget alerts (sorted by severity desc, then provider).
    pub alerts: Vec<BudgetAlertView>,
    /// Grand total cost across all providers.
    pub total_cost_usd: f64,
    /// Grand total tokens across all providers.
    pub total_tokens: u64,
    /// Number of tracked panes.
    pub pane_count: usize,
}

/// Rendered view of a provider's cost data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderCostView {
    /// Provider agent type name.
    pub agent_type: String,
    /// Total tokens consumed.
    pub tokens: u64,
    /// Total cost in USD.
    pub cost_usd: f64,
    /// Number of active panes for this provider.
    pub pane_count: usize,
    /// Budget usage as fraction (0.0–1.0+), `None` if no budget configured.
    pub budget_usage_fraction: Option<f64>,
    /// Budget limit in USD, `None` if no budget configured.
    pub budget_limit_usd: Option<f64>,
}

/// Rendered view of a budget alert.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetAlertView {
    /// Provider agent type name.
    pub agent_type: String,
    /// Current cost in USD.
    pub current_cost_usd: f64,
    /// Budget limit in USD.
    pub budget_limit_usd: f64,
    /// Usage fraction (0.0–1.0+).
    pub usage_fraction: f64,
    /// Alert severity.
    pub severity: String,
    /// Whether this alert would block a launch.
    pub is_blocking: bool,
}

/// Rate limit panel: per-provider rate limit status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RateLimitPanel {
    /// Per-provider rate limit summaries.
    pub providers: Vec<RateLimitProviderView>,
    /// Number of providers currently rate-limited (partial or full).
    pub limited_provider_count: usize,
    /// Total panes currently rate-limited across all providers.
    pub total_limited_panes: usize,
}

/// Rendered view of a provider's rate limit status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RateLimitProviderView {
    /// Provider agent type name.
    pub agent_type: String,
    /// Current status as a human-readable string.
    pub status: String,
    /// Whether any panes are limited.
    pub is_limited: bool,
    /// Number of limited panes.
    pub limited_pane_count: usize,
    /// Total panes tracked.
    pub total_pane_count: usize,
    /// Seconds until earliest cooldown expires.
    pub earliest_clear_secs: u64,
}

/// Backpressure panel: queue health and paused panes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackpressurePanel {
    /// Current backpressure tier.
    pub tier: String,
    /// Tier as a system health classification.
    pub health: SystemHealthTier,
    /// Capture queue utilization (0.0–1.0).
    pub capture_utilization: f64,
    /// Write queue utilization (0.0–1.0).
    pub write_utilization: f64,
    /// Number of paused panes.
    pub paused_pane_count: usize,
    /// Duration in current tier (ms).
    pub duration_in_tier_ms: u64,
    /// Total tier transitions.
    pub transitions: u64,
}

/// Quota gate panel: launch decision summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaPanel {
    /// Total evaluations performed.
    pub evaluations: u64,
    /// Launches allowed.
    pub allowed: u64,
    /// Launches warned.
    pub warned: u64,
    /// Launches blocked.
    pub blocked: u64,
    /// Block rate as a percentage (0–100).
    pub block_rate_percent: u64,
}

// =============================================================================
// Dashboard state
// =============================================================================

/// Complete dashboard state: a point-in-time snapshot of all subsystem panels.
///
/// This is the primary data structure consumed by TUI renderers, web API
/// endpoints, and robot-mode JSON output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DashboardState {
    /// Snapshot timestamp (ms since epoch).
    pub timestamp_ms: u64,
    /// Overall system health (worst-case across all panels).
    pub overall_health: SystemHealthTier,
    /// Cost tracking panel.
    pub costs: CostPanel,
    /// Rate limit panel.
    pub rate_limits: RateLimitPanel,
    /// Backpressure panel.
    pub backpressure: BackpressurePanel,
    /// Quota gate panel.
    pub quota: QuotaPanel,
    /// Telemetry counters for the dashboard manager itself.
    pub telemetry: DashboardTelemetrySnapshot,
}

// =============================================================================
// Telemetry
// =============================================================================

/// Telemetry counters for the dashboard manager.
#[derive(Debug, Default)]
pub struct DashboardTelemetry {
    pub snapshots_taken: u64,
    pub cost_updates: u64,
    pub rate_limit_updates: u64,
    pub backpressure_updates: u64,
    pub quota_updates: u64,
}

impl DashboardTelemetry {
    pub fn snapshot(&self) -> DashboardTelemetrySnapshot {
        DashboardTelemetrySnapshot {
            snapshots_taken: self.snapshots_taken,
            cost_updates: self.cost_updates,
            rate_limit_updates: self.rate_limit_updates,
            backpressure_updates: self.backpressure_updates,
            quota_updates: self.quota_updates,
        }
    }
}

/// Serializable snapshot of dashboard telemetry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DashboardTelemetrySnapshot {
    pub snapshots_taken: u64,
    pub cost_updates: u64,
    pub rate_limit_updates: u64,
    pub backpressure_updates: u64,
    pub quota_updates: u64,
}

// =============================================================================
// Manager
// =============================================================================

/// Aggregates subsystem snapshots into a unified [`DashboardState`].
///
/// The manager holds the latest snapshot from each subsystem. Call `snapshot()`
/// to produce a consistent [`DashboardState`] suitable for rendering.
#[derive(Debug)]
pub struct DashboardManager {
    cost_snapshot: Option<CostDashboardSnapshot>,
    rate_limit_summaries: Vec<ProviderRateLimitSummary>,
    backpressure_snapshot: Option<BackpressureSnapshot>,
    quota_snapshot: Option<QuotaGateSnapshot>,
    telemetry: DashboardTelemetry,
}

impl DashboardManager {
    /// Create a new dashboard manager with no data.
    pub fn new() -> Self {
        Self {
            cost_snapshot: None,
            rate_limit_summaries: Vec::new(),
            backpressure_snapshot: None,
            quota_snapshot: None,
            telemetry: DashboardTelemetry::default(),
        }
    }

    /// Update cost tracking data.
    pub fn update_costs(&mut self, snapshot: CostDashboardSnapshot) {
        self.cost_snapshot = Some(snapshot);
        self.telemetry.cost_updates += 1;
    }

    /// Update rate limit data for all providers.
    pub fn update_rate_limits(&mut self, summaries: Vec<ProviderRateLimitSummary>) {
        self.rate_limit_summaries = summaries;
        self.telemetry.rate_limit_updates += 1;
    }

    /// Update backpressure data.
    pub fn update_backpressure(&mut self, snapshot: BackpressureSnapshot) {
        self.backpressure_snapshot = Some(snapshot);
        self.telemetry.backpressure_updates += 1;
    }

    /// Update quota gate data.
    pub fn update_quota(&mut self, snapshot: QuotaGateSnapshot) {
        self.quota_snapshot = Some(snapshot);
        self.telemetry.quota_updates += 1;
    }

    /// Produce a point-in-time [`DashboardState`] from the latest subsystem data.
    pub fn snapshot(&mut self) -> DashboardState {
        self.telemetry.snapshots_taken += 1;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);

        let costs = self.build_cost_panel();
        let rate_limits = self.build_rate_limit_panel();
        let backpressure = self.build_backpressure_panel();
        let quota = self.build_quota_panel();

        // Overall health = worst of all subsystem tiers.
        let mut overall = SystemHealthTier::Green;

        // Cost alerts contribute to health.
        if costs.alerts.iter().any(|a| a.is_blocking) {
            overall = overall.max(SystemHealthTier::Red);
        } else if !costs.alerts.is_empty() {
            overall = overall.max(SystemHealthTier::Yellow);
        }

        // Rate limits contribute to health.
        if rate_limits
            .providers
            .iter()
            .any(|p| p.status == "fully_limited")
        {
            overall = overall.max(SystemHealthTier::Red);
        } else if rate_limits.limited_provider_count > 0 {
            overall = overall.max(SystemHealthTier::Yellow);
        }

        // Backpressure tier maps directly.
        overall = overall.max(backpressure.health);

        // Quota blocks contribute to health.
        if quota.blocked > 0 && quota.block_rate_percent > 50 {
            overall = overall.max(SystemHealthTier::Red);
        } else if quota.blocked > 0 {
            overall = overall.max(SystemHealthTier::Yellow);
        }

        DashboardState {
            timestamp_ms: now_ms,
            overall_health: overall,
            costs,
            rate_limits,
            backpressure,
            quota,
            telemetry: self.telemetry.snapshot(),
        }
    }

    /// Telemetry counters.
    pub fn telemetry(&self) -> &DashboardTelemetry {
        &self.telemetry
    }

    // ─── Panel builders ──────────────────────────────────────────────

    fn build_cost_panel(&self) -> CostPanel {
        let Some(snap) = &self.cost_snapshot else {
            return CostPanel {
                providers: BTreeMap::new(),
                alerts: Vec::new(),
                total_cost_usd: 0.0,
                total_tokens: 0,
                pane_count: 0,
            };
        };

        let mut providers = BTreeMap::new();
        for p in &snap.providers {
            providers.insert(
                p.agent_type.clone(),
                ProviderCostView {
                    agent_type: p.agent_type.clone(),
                    tokens: p.total_tokens,
                    cost_usd: p.total_cost_usd,
                    pane_count: p.pane_count,
                    budget_usage_fraction: None,
                    budget_limit_usd: None,
                },
            );
        }

        // Enrich providers with budget data from alerts.
        for alert in &snap.alerts {
            if let Some(pv) = providers.get_mut(&alert.agent_type) {
                pv.budget_usage_fraction = Some(alert.usage_fraction);
                pv.budget_limit_usd = Some(alert.budget_limit_usd);
            }
        }

        let alerts = snap.alerts.iter().map(budget_alert_to_view).collect();

        CostPanel {
            providers,
            alerts,
            total_cost_usd: snap.grand_total_cost_usd,
            total_tokens: snap.grand_total_tokens,
            pane_count: snap.panes.len(),
        }
    }

    fn build_rate_limit_panel(&self) -> RateLimitPanel {
        let providers: Vec<RateLimitProviderView> = self
            .rate_limit_summaries
            .iter()
            .map(rate_limit_to_view)
            .collect();

        let limited_provider_count = providers.iter().filter(|p| p.is_limited).count();
        let total_limited_panes: usize = providers.iter().map(|p| p.limited_pane_count).sum();

        RateLimitPanel {
            providers,
            limited_provider_count,
            total_limited_panes,
        }
    }

    fn build_backpressure_panel(&self) -> BackpressurePanel {
        let Some(snap) = &self.backpressure_snapshot else {
            return BackpressurePanel {
                tier: "green".to_string(),
                health: SystemHealthTier::Green,
                capture_utilization: 0.0,
                write_utilization: 0.0,
                paused_pane_count: 0,
                duration_in_tier_ms: 0,
                transitions: 0,
            };
        };

        let capture_util = if snap.capture_capacity > 0 {
            snap.capture_depth as f64 / snap.capture_capacity as f64
        } else {
            0.0
        };
        let write_util = if snap.write_capacity > 0 {
            snap.write_depth as f64 / snap.write_capacity as f64
        } else {
            0.0
        };

        BackpressurePanel {
            tier: format!("{:?}", snap.tier).to_lowercase(),
            health: SystemHealthTier::from(snap.tier),
            capture_utilization: capture_util,
            write_utilization: write_util,
            paused_pane_count: snap.paused_panes.len(),
            duration_in_tier_ms: snap.duration_in_tier_ms,
            transitions: snap.transitions,
        }
    }

    fn build_quota_panel(&self) -> QuotaPanel {
        let Some(snap) = &self.quota_snapshot else {
            return QuotaPanel {
                evaluations: 0,
                allowed: 0,
                warned: 0,
                blocked: 0,
                block_rate_percent: 0,
            };
        };

        let t = &snap.telemetry;
        let block_rate = (t.blocked * 100).checked_div(t.evaluations).unwrap_or(0);

        QuotaPanel {
            evaluations: t.evaluations,
            allowed: t.allowed,
            warned: t.warned,
            blocked: t.blocked,
            block_rate_percent: block_rate,
        }
    }
}

impl Default for DashboardManager {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Conversion helpers
// =============================================================================

fn budget_alert_to_view(alert: &BudgetAlert) -> BudgetAlertView {
    let is_blocking = alert.severity == AlertSeverity::Critical;
    BudgetAlertView {
        agent_type: alert.agent_type.clone(),
        current_cost_usd: alert.current_cost_usd,
        budget_limit_usd: alert.budget_limit_usd,
        usage_fraction: alert.usage_fraction,
        severity: format!("{:?}", alert.severity).to_lowercase(),
        is_blocking,
    }
}

fn rate_limit_to_view(summary: &ProviderRateLimitSummary) -> RateLimitProviderView {
    let is_limited = summary.status != ProviderRateLimitStatus::Clear;
    let status = match summary.status {
        ProviderRateLimitStatus::Clear => "clear",
        ProviderRateLimitStatus::PartiallyLimited => "partially_limited",
        ProviderRateLimitStatus::FullyLimited => "fully_limited",
    };
    RateLimitProviderView {
        agent_type: summary.agent_type.clone(),
        status: status.to_string(),
        is_limited,
        limited_pane_count: summary.limited_pane_count,
        total_pane_count: summary.total_pane_count,
        earliest_clear_secs: summary.earliest_clear_secs,
    }
}

// =============================================================================
// Convenience: From subsystem snapshots
// =============================================================================

impl DashboardState {
    /// Compute the worst-case launch verdict based on current dashboard state.
    pub fn worst_launch_verdict(&self) -> LaunchVerdict {
        if self.quota.blocked > 0 && self.quota.evaluations > 0 {
            let recent_block_rate = self.quota.block_rate_percent;
            if recent_block_rate > 50 {
                return LaunchVerdict::Block;
            }
        }
        if self.overall_health >= SystemHealthTier::Red {
            return LaunchVerdict::Block;
        }
        if self.overall_health >= SystemHealthTier::Yellow {
            return LaunchVerdict::Warn;
        }
        LaunchVerdict::Allow
    }

    /// True if any subsystem is at Red or Black tier.
    pub fn has_critical_alerts(&self) -> bool {
        self.overall_health >= SystemHealthTier::Red
    }

    /// Number of currently limited providers.
    pub fn limited_provider_count(&self) -> usize {
        self.rate_limits.limited_provider_count
    }

    /// Number of paused panes due to backpressure.
    pub fn paused_pane_count(&self) -> usize {
        self.backpressure.paused_pane_count
    }

    /// Produce a compact summary suitable for status bars and one-line displays.
    pub fn summary_line(&self) -> String {
        let health = &self.overall_health;
        let providers = self.costs.providers.len();
        let limited = self.rate_limits.limited_provider_count;
        let paused = self.backpressure.paused_pane_count;
        let blocked = self.quota.blocked;

        let mut parts = vec![format!("health={health}")];

        if providers > 0 {
            parts.push(format!(
                "cost=${:.2}/{}tok",
                self.costs.total_cost_usd, self.costs.total_tokens
            ));
        }

        if limited > 0 {
            parts.push(format!("rate_limited={limited}"));
        }

        if paused > 0 {
            parts.push(format!("paused={paused}"));
        }

        if blocked > 0 {
            parts.push(format!("blocked={blocked}"));
        }

        parts.join(" ")
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backpressure::BackpressureTier;
    use crate::cost_tracker::{
        AlertSeverity, BudgetAlert, CostDashboardSnapshot, PaneCostSummary, ProviderCostSummary,
    };
    use crate::quota_gate::{QuotaGateSnapshot, QuotaGateTelemetrySnapshot};
    use crate::rate_limit_tracker::{ProviderRateLimitStatus, ProviderRateLimitSummary};

    fn sample_cost_snapshot() -> CostDashboardSnapshot {
        CostDashboardSnapshot {
            providers: vec![
                ProviderCostSummary {
                    agent_type: "codex".to_string(),
                    total_tokens: 50_000,
                    total_cost_usd: 45.0,
                    pane_count: 3,
                    record_count: 100,
                },
                ProviderCostSummary {
                    agent_type: "claude_code".to_string(),
                    total_tokens: 20_000,
                    total_cost_usd: 15.0,
                    pane_count: 2,
                    record_count: 50,
                },
            ],
            panes: vec![
                PaneCostSummary {
                    pane_id: 1,
                    agent_type: "codex".to_string(),
                    total_tokens: 30_000,
                    total_cost_usd: 25.0,
                    record_count: 60,
                    last_updated_ms: 1000,
                },
                PaneCostSummary {
                    pane_id: 2,
                    agent_type: "codex".to_string(),
                    total_tokens: 20_000,
                    total_cost_usd: 20.0,
                    record_count: 40,
                    last_updated_ms: 2000,
                },
            ],
            alerts: vec![BudgetAlert {
                agent_type: "codex".to_string(),
                current_cost_usd: 45.0,
                budget_limit_usd: 50.0,
                usage_fraction: 0.9,
                severity: AlertSeverity::Warning,
            }],
            grand_total_cost_usd: 60.0,
            grand_total_tokens: 70_000,
        }
    }

    fn sample_rate_limits() -> Vec<ProviderRateLimitSummary> {
        vec![
            ProviderRateLimitSummary {
                agent_type: "codex".to_string(),
                status: ProviderRateLimitStatus::Clear,
                limited_pane_count: 0,
                total_pane_count: 3,
                earliest_clear_secs: 0,
                total_events: 0,
            },
            ProviderRateLimitSummary {
                agent_type: "gemini".to_string(),
                status: ProviderRateLimitStatus::PartiallyLimited,
                limited_pane_count: 1,
                total_pane_count: 2,
                earliest_clear_secs: 30,
                total_events: 2,
            },
        ]
    }

    fn sample_backpressure() -> BackpressureSnapshot {
        BackpressureSnapshot {
            tier: BackpressureTier::Yellow,
            timestamp_epoch_ms: 1_000_000,
            capture_depth: 800,
            capture_capacity: 1000,
            write_depth: 200,
            write_capacity: 1000,
            duration_in_tier_ms: 5000,
            transitions: 3,
            paused_panes: vec![5],
        }
    }

    fn sample_quota() -> QuotaGateSnapshot {
        QuotaGateSnapshot {
            telemetry: QuotaGateTelemetrySnapshot {
                evaluations: 100,
                allowed: 80,
                warned: 15,
                blocked: 5,
            },
        }
    }

    // ── Basic snapshot tests ─────────────────────────────────────────

    #[test]
    fn empty_manager_produces_green_snapshot() {
        let mut mgr = DashboardManager::new();
        let state = mgr.snapshot();
        assert_eq!(state.overall_health, SystemHealthTier::Green);
        assert_eq!(state.costs.pane_count, 0);
        assert_eq!(state.rate_limits.limited_provider_count, 0);
        assert_eq!(state.backpressure.paused_pane_count, 0);
        assert_eq!(state.quota.evaluations, 0);
    }

    #[test]
    fn full_snapshot_with_all_data() {
        let mut mgr = DashboardManager::new();
        mgr.update_costs(sample_cost_snapshot());
        mgr.update_rate_limits(sample_rate_limits());
        mgr.update_backpressure(sample_backpressure());
        mgr.update_quota(sample_quota());

        let state = mgr.snapshot();

        // Cost panel
        assert_eq!(state.costs.providers.len(), 2);
        assert!(state.costs.providers.contains_key("codex"));
        assert!(state.costs.providers.contains_key("claude_code"));
        assert!((state.costs.total_cost_usd - 60.0).abs() < 1e-6);
        assert_eq!(state.costs.total_tokens, 70_000);
        assert_eq!(state.costs.alerts.len(), 1);
        assert_eq!(state.costs.alerts[0].severity, "warning");

        // Rate limit panel
        assert_eq!(state.rate_limits.providers.len(), 2);
        assert_eq!(state.rate_limits.limited_provider_count, 1);
        assert_eq!(state.rate_limits.total_limited_panes, 1);

        // Backpressure panel
        assert_eq!(state.backpressure.tier, "yellow");
        assert_eq!(state.backpressure.health, SystemHealthTier::Yellow);
        assert!((state.backpressure.capture_utilization - 0.8).abs() < 1e-6);
        assert_eq!(state.backpressure.paused_pane_count, 1);

        // Quota panel
        assert_eq!(state.quota.evaluations, 100);
        assert_eq!(state.quota.allowed, 80);
        assert_eq!(state.quota.blocked, 5);
        assert_eq!(state.quota.block_rate_percent, 5);

        // Overall health: Yellow from backpressure + rate limits
        assert_eq!(state.overall_health, SystemHealthTier::Yellow);
    }

    #[test]
    fn critical_budget_alert_raises_health_to_red() {
        let mut mgr = DashboardManager::new();
        let mut cost = sample_cost_snapshot();
        cost.alerts = vec![BudgetAlert {
            agent_type: "codex".to_string(),
            current_cost_usd: 60.0,
            budget_limit_usd: 50.0,
            usage_fraction: 1.2,
            severity: AlertSeverity::Critical,
        }];
        mgr.update_costs(cost);

        let state = mgr.snapshot();
        assert_eq!(state.overall_health, SystemHealthTier::Red);
        assert!(state.costs.alerts[0].is_blocking);
    }

    #[test]
    fn fully_limited_rate_limit_raises_health_to_red() {
        let mut mgr = DashboardManager::new();
        mgr.update_rate_limits(vec![ProviderRateLimitSummary {
            agent_type: "codex".to_string(),
            status: ProviderRateLimitStatus::FullyLimited,
            limited_pane_count: 5,
            total_pane_count: 5,
            earliest_clear_secs: 120,
            total_events: 10,
        }]);

        let state = mgr.snapshot();
        assert_eq!(state.overall_health, SystemHealthTier::Red);
    }

    #[test]
    fn black_backpressure_raises_health_to_black() {
        let mut mgr = DashboardManager::new();
        mgr.update_backpressure(BackpressureSnapshot {
            tier: BackpressureTier::Black,
            timestamp_epoch_ms: 1_000_000,
            capture_depth: 990,
            capture_capacity: 1000,
            write_depth: 950,
            write_capacity: 1000,
            duration_in_tier_ms: 10_000,
            transitions: 5,
            paused_panes: vec![1, 2, 3, 4, 5],
        });

        let state = mgr.snapshot();
        assert_eq!(state.overall_health, SystemHealthTier::Black);
        assert_eq!(state.backpressure.paused_pane_count, 5);
    }

    #[test]
    fn high_block_rate_raises_health_to_red() {
        let mut mgr = DashboardManager::new();
        mgr.update_quota(QuotaGateSnapshot {
            telemetry: QuotaGateTelemetrySnapshot {
                evaluations: 10,
                allowed: 2,
                warned: 2,
                blocked: 6,
            },
        });

        let state = mgr.snapshot();
        assert_eq!(state.overall_health, SystemHealthTier::Red);
        assert_eq!(state.quota.block_rate_percent, 60);
    }

    // ── Telemetry tests ──────────────────────────────────────────────

    #[test]
    fn telemetry_counts_updates() {
        let mut mgr = DashboardManager::new();
        mgr.update_costs(sample_cost_snapshot());
        mgr.update_costs(sample_cost_snapshot());
        mgr.update_rate_limits(sample_rate_limits());
        mgr.update_backpressure(sample_backpressure());
        mgr.update_quota(sample_quota());

        let _ = mgr.snapshot();
        let _ = mgr.snapshot();

        let t = mgr.telemetry().snapshot();
        assert_eq!(t.cost_updates, 2);
        assert_eq!(t.rate_limit_updates, 1);
        assert_eq!(t.backpressure_updates, 1);
        assert_eq!(t.quota_updates, 1);
        assert_eq!(t.snapshots_taken, 2);
    }

    #[test]
    fn telemetry_included_in_snapshot() {
        let mut mgr = DashboardManager::new();
        let _ = mgr.snapshot();
        let state = mgr.snapshot();
        assert_eq!(state.telemetry.snapshots_taken, 2);
    }

    // ── Serde roundtrip ──────────────────────────────────────────────

    #[test]
    fn dashboard_state_serde_roundtrip() {
        let mut mgr = DashboardManager::new();
        mgr.update_costs(sample_cost_snapshot());
        mgr.update_rate_limits(sample_rate_limits());
        mgr.update_backpressure(sample_backpressure());
        mgr.update_quota(sample_quota());
        let state = mgr.snapshot();

        let json = serde_json::to_string(&state).expect("serialize");
        let deser: DashboardState = serde_json::from_str(&json).expect("deserialize");

        // Compare everything except timestamp (varies).
        assert_eq!(deser.overall_health, state.overall_health);
        assert_eq!(deser.costs, state.costs);
        assert_eq!(deser.rate_limits, state.rate_limits);
        assert_eq!(deser.backpressure, state.backpressure);
        assert_eq!(deser.quota, state.quota);
        assert_eq!(deser.telemetry, state.telemetry);
    }

    // ── Convenience methods ──────────────────────────────────────────

    #[test]
    fn worst_launch_verdict_reflects_health() {
        let mut mgr = DashboardManager::new();
        let state = mgr.snapshot();
        assert_eq!(state.worst_launch_verdict(), LaunchVerdict::Allow);

        // Yellow health → Warn
        mgr.update_backpressure(sample_backpressure()); // Yellow
        let state = mgr.snapshot();
        assert_eq!(state.worst_launch_verdict(), LaunchVerdict::Warn);

        // Red health → Block
        mgr.update_backpressure(BackpressureSnapshot {
            tier: BackpressureTier::Red,
            timestamp_epoch_ms: 1_000_000,
            capture_depth: 900,
            capture_capacity: 1000,
            write_depth: 900,
            write_capacity: 1000,
            duration_in_tier_ms: 5000,
            transitions: 4,
            paused_panes: vec![1, 2, 3],
        });
        let state = mgr.snapshot();
        assert_eq!(state.worst_launch_verdict(), LaunchVerdict::Block);
    }

    #[test]
    fn has_critical_alerts_detects_red_and_above() {
        let mut mgr = DashboardManager::new();
        let state = mgr.snapshot();
        assert!(!state.has_critical_alerts());

        mgr.update_backpressure(BackpressureSnapshot {
            tier: BackpressureTier::Red,
            timestamp_epoch_ms: 1_000_000,
            capture_depth: 900,
            capture_capacity: 1000,
            write_depth: 0,
            write_capacity: 1000,
            duration_in_tier_ms: 0,
            transitions: 0,
            paused_panes: vec![],
        });
        let state = mgr.snapshot();
        assert!(state.has_critical_alerts());
    }

    // ── Budget enrichment ────────────────────────────────────────────

    #[test]
    fn provider_cost_view_enriched_with_budget_data() {
        let mut mgr = DashboardManager::new();
        mgr.update_costs(sample_cost_snapshot());
        let state = mgr.snapshot();

        // Codex has a budget alert → enriched.
        let codex = &state.costs.providers["codex"];
        assert_eq!(codex.budget_usage_fraction, Some(0.9));
        assert_eq!(codex.budget_limit_usd, Some(50.0));

        // ClaudeCode has no alert → no budget data.
        let claude = &state.costs.providers["claude_code"];
        assert_eq!(claude.budget_usage_fraction, None);
        assert_eq!(claude.budget_limit_usd, None);
    }

    // ── Edge cases ───────────────────────────────────────────────────

    #[test]
    fn zero_capacity_queues_produce_zero_utilization() {
        let mut mgr = DashboardManager::new();
        mgr.update_backpressure(BackpressureSnapshot {
            tier: BackpressureTier::Green,
            timestamp_epoch_ms: 0,
            capture_depth: 0,
            capture_capacity: 0,
            write_depth: 0,
            write_capacity: 0,
            duration_in_tier_ms: 0,
            transitions: 0,
            paused_panes: vec![],
        });
        let state = mgr.snapshot();
        assert!((state.backpressure.capture_utilization - 0.0).abs() < 1e-6);
        assert!((state.backpressure.write_utilization - 0.0).abs() < 1e-6);
    }

    #[test]
    fn zero_evaluations_produce_zero_block_rate() {
        let mut mgr = DashboardManager::new();
        mgr.update_quota(QuotaGateSnapshot {
            telemetry: QuotaGateTelemetrySnapshot {
                evaluations: 0,
                allowed: 0,
                warned: 0,
                blocked: 0,
            },
        });
        let state = mgr.snapshot();
        assert_eq!(state.quota.block_rate_percent, 0);
    }

    // ── SystemHealthTier ordering ────────────────────────────────────

    #[test]
    fn health_tier_ordering() {
        assert!(SystemHealthTier::Green < SystemHealthTier::Yellow);
        assert!(SystemHealthTier::Yellow < SystemHealthTier::Red);
        assert!(SystemHealthTier::Red < SystemHealthTier::Black);
    }

    #[test]
    fn health_tier_display() {
        assert_eq!(SystemHealthTier::Green.to_string(), "green");
        assert_eq!(SystemHealthTier::Yellow.to_string(), "yellow");
        assert_eq!(SystemHealthTier::Red.to_string(), "red");
        assert_eq!(SystemHealthTier::Black.to_string(), "black");
    }

    // ── Summary line ────────────────────────────────────────────────

    #[test]
    fn summary_line_empty_manager() {
        let mut mgr = DashboardManager::new();
        let state = mgr.snapshot();
        assert_eq!(state.summary_line(), "health=green");
    }

    #[test]
    fn summary_line_with_all_data() {
        let mut mgr = DashboardManager::new();
        mgr.update_costs(sample_cost_snapshot());
        mgr.update_rate_limits(vec![ProviderRateLimitSummary {
            agent_type: "codex".to_string(),
            status: ProviderRateLimitStatus::PartiallyLimited,
            limited_pane_count: 2,
            total_pane_count: 5,
            earliest_clear_secs: 30,
            total_events: 3,
        }]);
        mgr.update_backpressure(BackpressureSnapshot {
            tier: BackpressureTier::Yellow,
            timestamp_epoch_ms: 0,
            capture_depth: 500,
            capture_capacity: 1000,
            write_depth: 0,
            write_capacity: 1000,
            duration_in_tier_ms: 0,
            transitions: 0,
            paused_panes: vec![1, 2],
        });
        mgr.update_quota(QuotaGateSnapshot {
            telemetry: QuotaGateTelemetrySnapshot {
                evaluations: 10,
                allowed: 7,
                warned: 1,
                blocked: 2,
            },
        });
        let state = mgr.snapshot();
        let line = state.summary_line();
        assert!(line.contains("health=yellow"), "got: {line}");
        assert!(line.contains("cost=$60.00/70000tok"), "got: {line}");
        assert!(line.contains("rate_limited=1"), "got: {line}");
        assert!(line.contains("paused=2"), "got: {line}");
        assert!(line.contains("blocked=2"), "got: {line}");
    }
}
