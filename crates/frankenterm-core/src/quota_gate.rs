//! Pre-launch quota gate for pane spawning (ft-2dss0).
//!
//! Combines signals from the [`CostTracker`], [`RateLimitTracker`], and account
//! quota system to produce a [`LaunchDecision`] before every pane spawn. This
//! ensures agents never launch into exhausted providers and surfaces warnings
//! when budget or rate limits are nearing thresholds.
//!
//! # Integration
//!
//! ```text
//! Pane launch request
//!        ↓
//!  QuotaGate.evaluate()
//!        ├── CostTracker → budget alerts
//!        ├── RateLimitTracker → provider status
//!        └── AccountQuotaAdvisory → account availability
//!        ↓
//!  LaunchDecision { verdict, warnings }
//!        ↓
//!  Proceed / Warn / Block
//! ```

use crate::accounts::QuotaAvailability;
use crate::cost_tracker::{AlertSeverity, BudgetAlert, CostTracker};
use crate::patterns::AgentType;
use crate::rate_limit_tracker::{ProviderRateLimitStatus, ProviderRateLimitSummary};
use serde::{Deserialize, Serialize};

// =============================================================================
// Telemetry
// =============================================================================

/// Operational telemetry counters for the quota gate.
#[derive(Debug, Clone, Default)]
pub struct QuotaGateTelemetry {
    /// Total evaluate() calls.
    evaluations: u64,
    /// Evaluations that returned Allow.
    allowed: u64,
    /// Evaluations that returned Warn.
    warned: u64,
    /// Evaluations that returned Block.
    blocked: u64,
}

impl QuotaGateTelemetry {
    /// Create a new telemetry instance with all counters at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the current counter values.
    #[must_use]
    pub fn snapshot(&self) -> QuotaGateTelemetrySnapshot {
        QuotaGateTelemetrySnapshot {
            evaluations: self.evaluations,
            allowed: self.allowed,
            warned: self.warned,
            blocked: self.blocked,
        }
    }
}

/// Serializable snapshot of quota gate telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaGateTelemetrySnapshot {
    /// Total evaluate() calls.
    pub evaluations: u64,
    /// Evaluations that returned Allow.
    pub allowed: u64,
    /// Evaluations that returned Warn.
    pub warned: u64,
    /// Evaluations that returned Block.
    pub blocked: u64,
}

// =============================================================================
// Launch Decision
// =============================================================================

/// Verdict from the quota gate on whether a pane launch should proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchVerdict {
    /// All signals clear — proceed with launch.
    Allow,
    /// Launch is permitted, but one or more warnings are active.
    Warn,
    /// Launch is blocked — at least one blocking condition is present.
    Block,
}

/// A single warning or blocking reason from the quota gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchWarning {
    /// Source of the warning.
    pub source: WarningSource,
    /// Severity: warning (non-blocking) or critical (blocking).
    pub blocking: bool,
    /// Human-readable explanation.
    pub message: String,
}

/// Source that contributed to a launch warning or block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningSource {
    /// Budget threshold exceeded (from CostTracker).
    Budget,
    /// Rate limit active for provider (from RateLimitTracker).
    RateLimit,
    /// Account quota exhausted or low (from account system).
    AccountQuota,
}

/// Complete launch decision from the quota gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchDecision {
    /// The agent type being evaluated for launch.
    pub agent_type: String,
    /// Overall verdict.
    pub verdict: LaunchVerdict,
    /// Individual warnings and block reasons.
    pub warnings: Vec<LaunchWarning>,
}

impl LaunchDecision {
    /// Returns true when the verdict is Block.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        self.verdict == LaunchVerdict::Block
    }

    /// Returns true when the verdict is Warn.
    #[must_use]
    pub fn is_warned(&self) -> bool {
        self.verdict == LaunchVerdict::Warn
    }

    /// Returns the number of blocking reasons.
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.warnings.iter().filter(|w| w.blocking).count()
    }
}

// =============================================================================
// Input Signals
// =============================================================================

/// Quota gate input signals collected before evaluation.
///
/// All fields are optional — the gate degrades gracefully when some signals
/// are unavailable (e.g., caut is not installed, or cost tracking is disabled).
#[derive(Debug, Clone, Default)]
pub struct QuotaSignals {
    /// Budget alerts from the cost tracker (if cost tracking is enabled).
    pub budget_alerts: Vec<BudgetAlert>,
    /// Rate limit summary for the target provider (if tracking is active).
    pub rate_limit_summary: Option<ProviderRateLimitSummary>,
    /// Account quota availability (if caut integration is active).
    pub quota_availability: Option<QuotaAvailability>,
    /// Selected account quota percentage (if available).
    pub selected_quota_percent: Option<f64>,
}

// =============================================================================
// Quota Gate
// =============================================================================

/// Pre-launch quota gate that evaluates whether a pane should be spawned.
///
/// The gate combines budget alerts (CostTracker), rate limit status
/// (RateLimitTracker), and account availability (accounts module) to produce
/// a [`LaunchDecision`].
///
/// # Decision Logic
///
/// The gate evaluates three independent signals. Each can contribute warnings
/// or blocks:
///
/// 1. **Budget**: Critical budget alert → Block; Warning alert → Warn
/// 2. **Rate limit**: FullyLimited → Block; PartiallyLimited → Warn
/// 3. **Account quota**: Exhausted → Block; Low → Warn
///
/// The final verdict is the most severe across all signals.
#[derive(Debug)]
pub struct QuotaGate {
    telemetry: QuotaGateTelemetry,
}

impl QuotaGate {
    /// Create a new quota gate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            telemetry: QuotaGateTelemetry::new(),
        }
    }

    /// Evaluate whether a pane launch should proceed for the given agent type.
    ///
    /// Collects signals from cost tracker, rate limit tracker, and account
    /// quota system, then produces a [`LaunchDecision`].
    pub fn evaluate(&mut self, agent_type: AgentType, signals: &QuotaSignals) -> LaunchDecision {
        self.telemetry.evaluations += 1;

        let mut warnings = Vec::new();

        // Signal 1: Budget alerts
        Self::evaluate_budget(agent_type, &signals.budget_alerts, &mut warnings);

        // Signal 2: Rate limit status
        Self::evaluate_rate_limits(signals.rate_limit_summary.as_ref(), &mut warnings);

        // Signal 3: Account quota availability
        Self::evaluate_account_quota(
            signals.quota_availability,
            signals.selected_quota_percent,
            &mut warnings,
        );

        // Compute verdict: most severe across all warnings
        let verdict = if warnings.iter().any(|w| w.blocking) {
            self.telemetry.blocked += 1;
            LaunchVerdict::Block
        } else if !warnings.is_empty() {
            self.telemetry.warned += 1;
            LaunchVerdict::Warn
        } else {
            self.telemetry.allowed += 1;
            LaunchVerdict::Allow
        };

        LaunchDecision {
            agent_type: agent_type.to_string(),
            verdict,
            warnings,
        }
    }

    /// Convenience: evaluate with a CostTracker and RateLimitTracker directly.
    ///
    /// Builds the QuotaSignals from the trackers, optionally including account
    /// quota information.
    pub fn evaluate_from_trackers(
        &mut self,
        agent_type: AgentType,
        cost_tracker: &mut CostTracker,
        rate_limit_summary: Option<ProviderRateLimitSummary>,
        quota_availability: Option<QuotaAvailability>,
        selected_quota_percent: Option<f64>,
    ) -> LaunchDecision {
        let signals = QuotaSignals {
            budget_alerts: cost_tracker.budget_alerts(),
            rate_limit_summary,
            quota_availability,
            selected_quota_percent,
        };
        self.evaluate(agent_type, &signals)
    }

    /// Get a reference to the telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &QuotaGateTelemetry {
        &self.telemetry
    }

    // =========================================================================
    // Internal evaluation helpers
    // =========================================================================

    fn evaluate_budget(
        agent_type: AgentType,
        alerts: &[BudgetAlert],
        warnings: &mut Vec<LaunchWarning>,
    ) {
        let agent_str = agent_type.to_string();
        for alert in alerts {
            if alert.agent_type != agent_str {
                continue;
            }
            match alert.severity {
                AlertSeverity::Critical => {
                    warnings.push(LaunchWarning {
                        source: WarningSource::Budget,
                        blocking: true,
                        message: format!(
                            "Budget exceeded for {}: {:.1}% of ${:.2} limit used",
                            alert.agent_type,
                            alert.usage_fraction * 100.0,
                            alert.budget_limit_usd,
                        ),
                    });
                }
                AlertSeverity::Warning => {
                    warnings.push(LaunchWarning {
                        source: WarningSource::Budget,
                        blocking: false,
                        message: format!(
                            "Budget warning for {}: {:.1}% of ${:.2} limit used",
                            alert.agent_type,
                            alert.usage_fraction * 100.0,
                            alert.budget_limit_usd,
                        ),
                    });
                }
            }
        }
    }

    fn evaluate_rate_limits(
        summary: Option<&ProviderRateLimitSummary>,
        warnings: &mut Vec<LaunchWarning>,
    ) {
        let Some(summary) = summary else { return };
        match summary.status {
            ProviderRateLimitStatus::FullyLimited => {
                warnings.push(LaunchWarning {
                    source: WarningSource::RateLimit,
                    blocking: true,
                    message: format!(
                        "All {} panes rate-limited for {}; earliest clear in {}s",
                        summary.limited_pane_count, summary.agent_type, summary.earliest_clear_secs,
                    ),
                });
            }
            ProviderRateLimitStatus::PartiallyLimited => {
                warnings.push(LaunchWarning {
                    source: WarningSource::RateLimit,
                    blocking: false,
                    message: format!(
                        "{}/{} panes rate-limited for {}; earliest clear in {}s",
                        summary.limited_pane_count,
                        summary.total_pane_count,
                        summary.agent_type,
                        summary.earliest_clear_secs,
                    ),
                });
            }
            ProviderRateLimitStatus::Clear => {}
        }
    }

    fn evaluate_account_quota(
        availability: Option<QuotaAvailability>,
        selected_percent: Option<f64>,
        warnings: &mut Vec<LaunchWarning>,
    ) {
        let Some(avail) = availability else { return };
        match avail {
            QuotaAvailability::Exhausted => {
                warnings.push(LaunchWarning {
                    source: WarningSource::AccountQuota,
                    blocking: true,
                    message: "All accounts exhausted — no eligible account for launch".to_string(),
                });
            }
            QuotaAvailability::Low => {
                let pct_msg = selected_percent
                    .map(|p| format!(" ({p:.1}% remaining)"))
                    .unwrap_or_default();
                warnings.push(LaunchWarning {
                    source: WarningSource::AccountQuota,
                    blocking: false,
                    message: format!("Account quota is low{pct_msg} — consider switching provider"),
                });
            }
            QuotaAvailability::Available => {}
        }
    }
}

impl Default for QuotaGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of the quota gate state for dashboard rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaGateSnapshot {
    /// Telemetry counters.
    pub telemetry: QuotaGateTelemetrySnapshot,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_new_is_empty() {
        let gate = QuotaGate::new();
        let snap = gate.telemetry().snapshot();
        assert_eq!(snap.evaluations, 0);
        assert_eq!(snap.allowed, 0);
        assert_eq!(snap.warned, 0);
        assert_eq!(snap.blocked, 0);
    }

    #[test]
    fn allow_when_all_signals_clear() {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![],
            rate_limit_summary: Some(ProviderRateLimitSummary {
                agent_type: "codex".to_string(),
                status: ProviderRateLimitStatus::Clear,
                limited_pane_count: 0,
                total_pane_count: 5,
                earliest_clear_secs: 0,
                total_events: 0,
            }),
            quota_availability: Some(QuotaAvailability::Available),
            selected_quota_percent: Some(85.0),
        };

        let decision = gate.evaluate(AgentType::Codex, &signals);
        assert_eq!(decision.verdict, LaunchVerdict::Allow);
        assert!(decision.warnings.is_empty());
        assert!(!decision.is_blocked());
    }

    #[test]
    fn allow_with_no_signals() {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals::default();

        let decision = gate.evaluate(AgentType::Codex, &signals);
        assert_eq!(decision.verdict, LaunchVerdict::Allow);
    }

    #[test]
    fn warn_on_budget_warning() {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![BudgetAlert {
                agent_type: "codex".to_string(),
                severity: AlertSeverity::Warning,
                budget_limit_usd: 10.0,
                current_cost_usd: 8.5,
                usage_fraction: 0.85,
            }],
            rate_limit_summary: None,
            quota_availability: None,
            selected_quota_percent: None,
        };

        let decision = gate.evaluate(AgentType::Codex, &signals);
        assert_eq!(decision.verdict, LaunchVerdict::Warn);
        assert_eq!(decision.warnings.len(), 1);
        assert!(!decision.warnings[0].blocking);
        assert_eq!(decision.warnings[0].source, WarningSource::Budget);
    }

    #[test]
    fn block_on_budget_critical() {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![BudgetAlert {
                agent_type: "codex".to_string(),
                severity: AlertSeverity::Critical,
                budget_limit_usd: 10.0,
                current_cost_usd: 12.0,
                usage_fraction: 1.2,
            }],
            rate_limit_summary: None,
            quota_availability: None,
            selected_quota_percent: None,
        };

        let decision = gate.evaluate(AgentType::Codex, &signals);
        assert_eq!(decision.verdict, LaunchVerdict::Block);
        assert!(decision.is_blocked());
        assert_eq!(decision.block_count(), 1);
    }

    #[test]
    fn block_on_fully_limited() {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![],
            rate_limit_summary: Some(ProviderRateLimitSummary {
                agent_type: "codex".to_string(),
                status: ProviderRateLimitStatus::FullyLimited,
                limited_pane_count: 3,
                total_pane_count: 3,
                earliest_clear_secs: 120,
                total_events: 5,
            }),
            quota_availability: None,
            selected_quota_percent: None,
        };

        let decision = gate.evaluate(AgentType::Codex, &signals);
        assert_eq!(decision.verdict, LaunchVerdict::Block);
        assert_eq!(decision.warnings[0].source, WarningSource::RateLimit);
    }

    #[test]
    fn warn_on_partially_limited() {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![],
            rate_limit_summary: Some(ProviderRateLimitSummary {
                agent_type: "codex".to_string(),
                status: ProviderRateLimitStatus::PartiallyLimited,
                limited_pane_count: 1,
                total_pane_count: 3,
                earliest_clear_secs: 60,
                total_events: 1,
            }),
            quota_availability: None,
            selected_quota_percent: None,
        };

        let decision = gate.evaluate(AgentType::Codex, &signals);
        assert_eq!(decision.verdict, LaunchVerdict::Warn);
        assert!(!decision.is_blocked());
    }

    #[test]
    fn block_on_accounts_exhausted() {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![],
            rate_limit_summary: None,
            quota_availability: Some(QuotaAvailability::Exhausted),
            selected_quota_percent: None,
        };

        let decision = gate.evaluate(AgentType::Codex, &signals);
        assert_eq!(decision.verdict, LaunchVerdict::Block);
        assert_eq!(decision.warnings[0].source, WarningSource::AccountQuota);
    }

    #[test]
    fn warn_on_low_quota() {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![],
            rate_limit_summary: None,
            quota_availability: Some(QuotaAvailability::Low),
            selected_quota_percent: Some(3.5),
        };

        let decision = gate.evaluate(AgentType::Codex, &signals);
        assert_eq!(decision.verdict, LaunchVerdict::Warn);
        assert!(decision.warnings[0].message.contains("3.5%"));
    }

    #[test]
    fn multiple_blocks_combine() {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![BudgetAlert {
                agent_type: "codex".to_string(),
                severity: AlertSeverity::Critical,
                budget_limit_usd: 10.0,
                current_cost_usd: 15.0,
                usage_fraction: 1.5,
            }],
            rate_limit_summary: Some(ProviderRateLimitSummary {
                agent_type: "codex".to_string(),
                status: ProviderRateLimitStatus::FullyLimited,
                limited_pane_count: 3,
                total_pane_count: 3,
                earliest_clear_secs: 300,
                total_events: 5,
            }),
            quota_availability: Some(QuotaAvailability::Exhausted),
            selected_quota_percent: None,
        };

        let decision = gate.evaluate(AgentType::Codex, &signals);
        assert_eq!(decision.verdict, LaunchVerdict::Block);
        assert_eq!(decision.block_count(), 3);
        assert_eq!(decision.warnings.len(), 3);
    }

    #[test]
    fn budget_alert_for_different_provider_ignored() {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![BudgetAlert {
                agent_type: "gemini".to_string(),
                severity: AlertSeverity::Critical,
                budget_limit_usd: 10.0,
                current_cost_usd: 12.0,
                usage_fraction: 1.2,
            }],
            rate_limit_summary: None,
            quota_availability: None,
            selected_quota_percent: None,
        };

        // Evaluating for Codex should ignore Gemini's alert
        let decision = gate.evaluate(AgentType::Codex, &signals);
        assert_eq!(decision.verdict, LaunchVerdict::Allow);
    }

    #[test]
    fn telemetry_counts_verdicts() {
        let mut gate = QuotaGate::new();
        let clear = QuotaSignals::default();
        let warning = QuotaSignals {
            quota_availability: Some(QuotaAvailability::Low),
            selected_quota_percent: Some(5.0),
            ..Default::default()
        };
        let blocked = QuotaSignals {
            quota_availability: Some(QuotaAvailability::Exhausted),
            ..Default::default()
        };

        gate.evaluate(AgentType::Codex, &clear);
        gate.evaluate(AgentType::Codex, &warning);
        gate.evaluate(AgentType::Codex, &blocked);

        let snap = gate.telemetry().snapshot();
        assert_eq!(snap.evaluations, 3);
        assert_eq!(snap.allowed, 1);
        assert_eq!(snap.warned, 1);
        assert_eq!(snap.blocked, 1);
    }

    #[test]
    fn verdict_ordering() {
        assert!(LaunchVerdict::Allow < LaunchVerdict::Warn);
        assert!(LaunchVerdict::Warn < LaunchVerdict::Block);
    }

    #[test]
    fn launch_decision_serde_roundtrip() {
        let decision = LaunchDecision {
            agent_type: "codex".to_string(),
            verdict: LaunchVerdict::Warn,
            warnings: vec![LaunchWarning {
                source: WarningSource::Budget,
                blocking: false,
                message: "Budget warning".to_string(),
            }],
        };

        let json = serde_json::to_string(&decision).unwrap();
        let deserialized: LaunchDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.verdict, decision.verdict);
        assert_eq!(deserialized.warnings.len(), decision.warnings.len());
        assert_eq!(deserialized.agent_type, decision.agent_type);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let snap = QuotaGateTelemetrySnapshot {
            evaluations: 42,
            allowed: 30,
            warned: 10,
            blocked: 2,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: QuotaGateTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, deserialized);
    }

    #[test]
    fn evaluate_from_trackers_builds_signals() {
        let mut gate = QuotaGate::new();
        let mut cost_tracker = CostTracker::new();

        let decision = gate.evaluate_from_trackers(
            AgentType::Codex,
            &mut cost_tracker,
            None,
            Some(QuotaAvailability::Available),
            Some(90.0),
        );
        assert_eq!(decision.verdict, LaunchVerdict::Allow);
    }

    #[test]
    fn evaluate_latency_under_10ms() {
        use std::time::Instant;

        let mut gate = QuotaGate::new();
        // Build a realistic worst-case signal set (all three signals active)
        let signals = QuotaSignals {
            budget_alerts: vec![
                BudgetAlert {
                    agent_type: "codex".to_string(),
                    severity: AlertSeverity::Warning,
                    budget_limit_usd: 100.0,
                    current_cost_usd: 85.0,
                    usage_fraction: 0.85,
                },
                BudgetAlert {
                    agent_type: "claude_code".to_string(),
                    severity: AlertSeverity::Critical,
                    budget_limit_usd: 50.0,
                    current_cost_usd: 60.0,
                    usage_fraction: 1.2,
                },
            ],
            rate_limit_summary: Some(ProviderRateLimitSummary {
                agent_type: "codex".to_string(),
                status: ProviderRateLimitStatus::PartiallyLimited,
                limited_pane_count: 2,
                total_pane_count: 5,
                earliest_clear_secs: 120,
                total_events: 3,
            }),
            quota_availability: Some(QuotaAvailability::Low),
            selected_quota_percent: Some(4.5),
        };

        // Warm up
        gate.evaluate(AgentType::Codex, &signals);

        // Measure 1000 evaluations
        let start = Instant::now();
        for _ in 0..1000 {
            let _ = gate.evaluate(AgentType::Codex, &signals);
        }
        let elapsed = start.elapsed();
        let per_eval_us = elapsed.as_micros() as f64 / 1000.0;

        // Acceptance criterion: <10ms per evaluation (generous bound)
        // In practice this should be <100us
        assert!(
            per_eval_us < 10_000.0,
            "Quota gate evaluation too slow: {per_eval_us:.1}us per eval (limit: 10000us)"
        );
    }

    #[test]
    fn quota_gate_snapshot() {
        let mut gate = QuotaGate::new();
        gate.evaluate(AgentType::Codex, &QuotaSignals::default());

        let snapshot = QuotaGateSnapshot {
            telemetry: gate.telemetry().snapshot(),
        };
        assert_eq!(snapshot.telemetry.evaluations, 1);

        let json = serde_json::to_string(&snapshot).unwrap();
        let deserialized: QuotaGateSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.telemetry.evaluations, 1);
    }
}
