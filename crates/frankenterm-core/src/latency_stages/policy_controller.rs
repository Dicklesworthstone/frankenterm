use serde::{Deserialize, Serialize};

use super::HitchRiskLevel;

// Four weighted loss terms plus later subtraction/hysteresis arithmetic must
// remain finite even when configuration arrives from an untrusted serializer.
const MAX_SAFE_LOSS: f64 = f64::MAX / 16.0;

// ── D3: Expected-Loss Policy Controller ────────────────────────────

/// Actions the policy controller can select.
///
/// Each action represents a runtime tuning decision with different
/// cost/benefit tradeoffs under different system states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PolicyAction {
    /// Maintain current settings — lowest cost when system is healthy.
    Hold,
    /// Tighten budgets / increase monitoring — moderate cost, reduces risk.
    Tighten,
    /// Relax budgets / reduce monitoring — saves resources in calm periods.
    Relax,
    /// Emergency shed load — expensive but prevents catastrophic hitches.
    Shed,
}

impl std::fmt::Display for PolicyAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hold => write!(f, "hold"),
            Self::Tighten => write!(f, "tighten"),
            Self::Relax => write!(f, "relax"),
            Self::Shed => write!(f, "shed"),
        }
    }
}

/// System state hypothesis for the loss matrix.
///
/// The controller considers which state the system is in,
/// weighted by the posterior probability from the hitch-risk model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SystemState {
    /// System is healthy — no action needed.
    Healthy,
    /// System is drifting — monitoring or tightening warranted.
    Drifting,
    /// System is under stress — active mitigation needed.
    Stressed,
    /// System is in crisis — shed load to prevent catastrophe.
    Critical,
}

impl std::fmt::Display for SystemState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Drifting => write!(f, "drifting"),
            Self::Stressed => write!(f, "stressed"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

/// Loss matrix entry: cost of taking `action` when the true state is `state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LossEntry {
    pub state: SystemState,
    pub action: PolicyAction,
    pub loss: f64,
}

/// Configuration for the expected-loss policy controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyControllerConfig {
    /// Loss matrix: cost of each (state, action) pair.
    /// Indexed as [state_idx * 4 + action_idx] for states and actions in enum order.
    pub loss_matrix: Vec<f64>,
    /// Safety floor: minimum probability mass assigned to Critical state.
    pub critical_floor: f64,
    /// Maximum rate of policy changes per second.
    pub max_change_rate_hz: f64,
    /// Hysteresis: don't switch action unless expected-loss improves by this fraction.
    pub hysteresis: f64,
}

impl PolicyControllerConfig {
    /// Sensible defaults with asymmetric loss (missing a crisis is much worse
    /// than over-reacting to a healthy system).
    pub fn default_asymmetric() -> Self {
        // Loss matrix: rows = states (Healthy, Drifting, Stressed, Critical)
        //              cols = actions (Hold, Tighten, Relax, Shed)
        #[rustfmt::skip]
        let loss_matrix = vec![
            // Healthy:   Hold=0, Tighten=1, Relax=0.5, Shed=5
            0.0, 1.0, 0.5, 5.0,
            // Drifting:  Hold=2, Tighten=0.5, Relax=3, Shed=4
            2.0, 0.5, 3.0, 4.0,
            // Stressed:  Hold=5, Tighten=1, Relax=8, Shed=2
            5.0, 1.0, 8.0, 2.0,
            // Critical:  Hold=10, Tighten=3, Relax=15, Shed=1
            10.0, 3.0, 15.0, 1.0,
        ];
        Self {
            loss_matrix,
            critical_floor: 0.01,
            max_change_rate_hz: 2.0,
            hysteresis: 0.05,
        }
    }
}

/// A single policy decision record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyDecision {
    /// Selected action.
    pub action: PolicyAction,
    /// Expected loss for the selected action.
    pub expected_loss: f64,
    /// State probabilities used for the decision [healthy, drifting, stressed, critical].
    pub state_probs: [f64; 4],
    /// Expected losses for all actions [hold, tighten, relax, shed].
    pub all_losses: [f64; 4],
    /// Whether hysteresis suppressed a switch.
    pub hysteresis_applied: bool,
    /// Whether the configured action-change rate limit suppressed a switch.
    pub rate_limit_applied: bool,
    /// Timestamp in microseconds.
    pub timestamp_us: u64,
}

/// Snapshot of the policy controller state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyControllerSnapshot {
    /// Current recommended action.
    pub current_action: PolicyAction,
    /// Total decisions made.
    pub total_decisions: u64,
    /// Decision counts per action [hold, tighten, relax, shed].
    pub action_counts: [u64; 4],
    /// Last expected loss.
    pub last_expected_loss: f64,
    /// Number of times hysteresis suppressed a switch.
    pub hysteresis_count: u64,
    /// Number of times the action-change rate guard suppressed a switch.
    pub rate_limit_count: u64,
}

/// The expected-loss policy controller.
///
/// Given posterior probabilities over system states, selects the action
/// that minimizes expected loss.  Incorporates hysteresis to prevent
/// flapping and a critical floor for safety.
#[derive(Debug, Clone)]
pub struct PolicyController {
    pub(super) config: PolicyControllerConfig,
    /// Current action.
    current_action: PolicyAction,
    /// Total decisions made.
    total_decisions: u64,
    /// Per-action counters [hold, tighten, relax, shed].
    action_counts: [u64; 4],
    /// Last expected loss of chosen action.
    last_expected_loss: f64,
    /// Count of hysteresis suppressions.
    hysteresis_count: u64,
    /// Count of action-change rate-limit suppressions.
    rate_limit_count: u64,
    /// Recent decisions (ring buffer).
    decisions: Vec<PolicyDecision>,
    max_decisions: usize,
    decision_head: usize,
    /// Timestamp of the last action change for rate limiting.
    last_change_us: Option<u64>,
}

impl PolicyController {
    /// Create a new controller.
    pub fn new(mut config: PolicyControllerConfig) -> Self {
        let defaults = PolicyControllerConfig::default_asymmetric();
        if config.loss_matrix.len() != 16 {
            config.loss_matrix.clone_from(&defaults.loss_matrix);
        } else {
            for (loss, default_loss) in config.loss_matrix.iter_mut().zip(&defaults.loss_matrix) {
                if !loss.is_finite() || *loss < 0.0 {
                    *loss = *default_loss;
                } else {
                    *loss = loss.min(MAX_SAFE_LOSS);
                }
            }
        }
        config.critical_floor = if config.critical_floor.is_finite() {
            config.critical_floor.clamp(0.0, 1.0)
        } else {
            defaults.critical_floor
        };
        config.max_change_rate_hz =
            if config.max_change_rate_hz.is_finite() && config.max_change_rate_hz >= 0.0 {
                config.max_change_rate_hz
            } else {
                defaults.max_change_rate_hz
            };
        config.hysteresis = if config.hysteresis.is_finite() {
            config.hysteresis.max(0.0)
        } else {
            defaults.hysteresis
        };

        Self {
            config,
            current_action: PolicyAction::Hold,
            total_decisions: 0,
            action_counts: [0; 4],
            last_expected_loss: 0.0,
            hysteresis_count: 0,
            rate_limit_count: 0,
            decisions: Vec::with_capacity(64),
            max_decisions: 100,
            decision_head: 0,
            last_change_us: None,
        }
    }

    /// Create with default asymmetric loss matrix.
    pub fn with_defaults() -> Self {
        Self::new(PolicyControllerConfig::default_asymmetric())
    }

    /// Make a policy decision given state probabilities.
    ///
    /// `probs` = [P(Healthy), P(Drifting), P(Stressed), P(Critical)]
    /// Must sum to ~1.0 (renormalized internally).
    pub fn decide(&mut self, probs: [f64; 4], timestamp_us: u64) -> PolicyAction {
        // Negative and non-finite values are not probabilities. Clamp them at
        // the boundary so a malformed telemetry/config payload cannot inject
        // NaNs into every expected loss or steer the minimum search by accident.
        let mut p = probs.map(|value| {
            if value.is_finite() {
                value.max(0.0)
            } else {
                0.0
            }
        });

        // Normalize using the largest component first. Summing several finite
        // `f64::MAX` inputs directly would overflow to infinity and turn every
        // normalized component into zero.
        let largest = p.iter().copied().fold(0.0_f64, f64::max);
        if largest == 0.0 {
            // An entirely unusable posterior fails safe to Critical rather
            // than making every expected loss zero and silently selecting Hold.
            p[3] = 1.0;
        } else {
            for probability in &mut p {
                *probability /= largest;
            }
            let scaled_total: f64 = p.iter().sum();
            for probability in &mut p {
                *probability /= scaled_total;
            }
        }

        // Apply the floor after normalization so non-unit caller input cannot
        // dilute the safety floor back below its configured minimum.
        if p[3] < self.config.critical_floor {
            let other_sum: f64 = p[0] + p[1] + p[2];
            if other_sum > 1e-12 {
                let scale = (1.0 - self.config.critical_floor) / other_sum;
                p[0] *= scale;
                p[1] *= scale;
                p[2] *= scale;
            } else {
                p[0] = 0.0;
                p[1] = 0.0;
                p[2] = 0.0;
            }
            p[3] = self.config.critical_floor;
        }

        // Compute expected loss for each action
        let mut all_losses = [0.0_f64; 4];
        for (action_idx, loss_slot) in all_losses.iter_mut().enumerate() {
            let mut el = 0.0;
            for (state_idx, &pi) in p.iter().enumerate() {
                el = pi.mul_add(self.config.loss_matrix[state_idx * 4 + action_idx], el);
            }
            *loss_slot = el;
        }

        // Find action with minimum expected loss
        let mut best_idx = 0_usize;
        let mut best_loss = all_losses[0];
        for (i, &loss) in all_losses.iter().enumerate().skip(1) {
            if loss < best_loss {
                best_loss = loss;
                best_idx = i;
            }
        }

        let best_action = match best_idx {
            0 => PolicyAction::Hold,
            1 => PolicyAction::Tighten,
            2 => PolicyAction::Relax,
            _ => PolicyAction::Shed,
        };

        // Apply hysteresis
        let current_idx = match self.current_action {
            PolicyAction::Hold => 0,
            PolicyAction::Tighten => 1,
            PolicyAction::Relax => 2,
            PolicyAction::Shed => 3,
        };
        let current_loss = all_losses[current_idx];
        let improvement = current_loss - best_loss;
        // Compare the relative improvement instead of multiplying the current
        // loss by an operator-provided fraction.  The quotient is bounded by
        // one because `best_loss <= current_loss`; the product could overflow
        // for an otherwise finite configuration.  A zero-loss tie is also not
        // an improvement, so retain the current action rather than flapping to
        // the enum-order winner.
        let hysteresis_applied = best_action != self.current_action
            && (current_loss == 0.0 || improvement / current_loss <= self.config.hysteresis);

        let rate_limit_applied = !hysteresis_applied
            && best_action != self.current_action
            && self.change_rate_guard_blocks(timestamp_us);

        let chosen = if hysteresis_applied {
            self.hysteresis_count = self.hysteresis_count.saturating_add(1);
            self.current_action
        } else if rate_limit_applied {
            self.rate_limit_count = self.rate_limit_count.saturating_add(1);
            self.current_action
        } else {
            if best_action != self.current_action {
                self.last_change_us = Some(timestamp_us);
            }
            self.current_action = best_action;
            best_action
        };

        let chosen_loss = all_losses[match chosen {
            PolicyAction::Hold => 0,
            PolicyAction::Tighten => 1,
            PolicyAction::Relax => 2,
            PolicyAction::Shed => 3,
        }];

        // Record
        self.total_decisions = self.total_decisions.saturating_add(1);
        self.last_expected_loss = chosen_loss;
        let action_count = &mut self.action_counts[match chosen {
            PolicyAction::Hold => 0,
            PolicyAction::Tighten => 1,
            PolicyAction::Relax => 2,
            PolicyAction::Shed => 3,
        }];
        *action_count = action_count.saturating_add(1);
        let decision = PolicyDecision {
            action: chosen,
            expected_loss: chosen_loss,
            state_probs: p,
            all_losses,
            hysteresis_applied,
            rate_limit_applied,
            timestamp_us,
        };
        if self.decisions.len() < self.max_decisions {
            self.decisions.push(decision);
        } else if self.max_decisions > 0 {
            self.decisions[self.decision_head] = decision;
            self.decision_head = (self.decision_head + 1) % self.max_decisions;
        }

        chosen
    }

    /// Current recommended action.
    pub fn current_action(&self) -> PolicyAction {
        self.current_action
    }

    /// Total decisions made.
    pub fn total_decisions(&self) -> u64 {
        self.total_decisions
    }

    /// Snapshot of current state.
    pub fn snapshot(&self) -> PolicyControllerSnapshot {
        PolicyControllerSnapshot {
            current_action: self.current_action,
            total_decisions: self.total_decisions,
            action_counts: self.action_counts,
            last_expected_loss: self.last_expected_loss,
            hysteresis_count: self.hysteresis_count,
            rate_limit_count: self.rate_limit_count,
        }
    }

    /// Human-readable status line.
    pub fn status_line(&self) -> String {
        format!(
            "policy[{}] decisions={} loss={:.3} hyst={} rate_limited={}",
            self.current_action,
            self.total_decisions,
            self.last_expected_loss,
            self.hysteresis_count,
            self.rate_limit_count,
        )
    }

    /// Reset to initial state.
    pub fn reset(&mut self) {
        self.current_action = PolicyAction::Hold;
        self.total_decisions = 0;
        self.action_counts = [0; 4];
        self.last_expected_loss = 0.0;
        self.hysteresis_count = 0;
        self.rate_limit_count = 0;
        self.decisions.clear();
        self.decision_head = 0;
        self.last_change_us = None;
    }

    /// Recent decisions.
    pub fn recent_decisions(&self, n: usize) -> Vec<&PolicyDecision> {
        let len = self.decisions.len();
        if len == 0 || n == 0 {
            return Vec::new();
        }
        let take = n.min(len);
        let mut result = Vec::with_capacity(take);
        if len < self.max_decisions {
            let start = len.saturating_sub(take);
            for d in &self.decisions[start..] {
                result.push(d);
            }
        } else {
            for i in 0..take {
                let idx = (self.decision_head + len - take + i) % len;
                result.push(&self.decisions[idx]);
            }
        }
        result
    }

    /// Detect degradation based on controller state.
    pub fn detect_degradation(&self) -> PolicyDegradation {
        match self.current_action {
            PolicyAction::Shed => PolicyDegradation::EmergencyShed {
                total_decisions: self.total_decisions,
                last_loss: self.last_expected_loss,
            },
            PolicyAction::Tighten => PolicyDegradation::Tightening {
                expected_loss: self.last_expected_loss,
            },
            _ => PolicyDegradation::Healthy,
        }
    }

    /// Generate structured log entry.
    pub fn log_entry(&self) -> PolicyControllerLogEntry {
        PolicyControllerLogEntry {
            current_action: self.current_action,
            total_decisions: self.total_decisions,
            action_counts: self.action_counts,
            last_expected_loss: self.last_expected_loss,
            hysteresis_count: self.hysteresis_count,
            rate_limit_count: self.rate_limit_count,
            degradation: self.detect_degradation(),
        }
    }

    // ── D3 Impl: Bridge Methods and Convenience API ────────────────

    /// Decide from hitch-risk model posterior directly.
    ///
    /// Maps HitchRiskLevel to state probabilities:
    /// - Low: [0.9, 0.08, 0.01, 0.01]
    /// - Elevated: [0.3, 0.5, 0.15, 0.05]
    /// - High: [0.05, 0.15, 0.6, 0.2]
    /// - Critical: [0.01, 0.04, 0.15, 0.8]
    pub fn decide_from_risk(&mut self, level: HitchRiskLevel, timestamp_us: u64) -> PolicyAction {
        let probs = match level {
            HitchRiskLevel::Low => [0.9, 0.08, 0.01, 0.01],
            HitchRiskLevel::Elevated => [0.3, 0.5, 0.15, 0.05],
            HitchRiskLevel::High => [0.05, 0.15, 0.6, 0.2],
            HitchRiskLevel::Critical => [0.01, 0.04, 0.15, 0.8],
        };
        self.decide(probs, timestamp_us)
    }

    /// Action distribution as fractions [hold, tighten, relax, shed].
    pub fn action_distribution(&self) -> [f64; 4] {
        let total = self
            .action_counts
            .iter()
            .fold(0.0, |sum, &count| sum + count as f64);
        if total == 0.0 {
            return [0.0; 4];
        }
        [
            self.action_counts[0] as f64 / total,
            self.action_counts[1] as f64 / total,
            self.action_counts[2] as f64 / total,
            self.action_counts[3] as f64 / total,
        ]
    }

    /// Per-action counts.
    pub fn action_counts(&self) -> [u64; 4] {
        self.action_counts
    }

    /// Count of hysteresis suppressions.
    pub fn hysteresis_count(&self) -> u64 {
        self.hysteresis_count
    }

    /// Count of action switches suppressed by the rate guard.
    pub fn rate_limit_count(&self) -> u64 {
        self.rate_limit_count
    }

    /// Last expected loss.
    pub fn last_expected_loss(&self) -> f64 {
        self.last_expected_loss
    }

    /// Update the hysteresis threshold.
    pub fn set_hysteresis(&mut self, h: f64) {
        if h.is_finite() {
            self.config.hysteresis = h.max(0.0);
        }
    }

    /// Update the critical floor.
    pub fn set_critical_floor(&mut self, floor: f64) {
        if floor.is_finite() {
            self.config.critical_floor = floor.clamp(0.0, 1.0);
        }
    }

    /// Update a single loss matrix entry.
    /// `state_idx` in 0..4 (Healthy/Drifting/Stressed/Critical),
    /// `action_idx` in 0..4 (Hold/Tighten/Relax/Shed).
    pub fn set_loss(&mut self, state_idx: usize, action_idx: usize, loss: f64) {
        if state_idx < 4 && action_idx < 4 && loss.is_finite() && loss >= 0.0 {
            self.config.loss_matrix[state_idx * 4 + action_idx] = loss.min(MAX_SAFE_LOSS);
        }
    }

    /// Number of stored decisions.
    pub fn decision_count(&self) -> usize {
        self.decisions.len()
    }

    fn change_rate_guard_blocks(&self, timestamp_us: u64) -> bool {
        let Some(last_change_us) = self.last_change_us else {
            return false;
        };
        if self.config.max_change_rate_hz == 0.0 {
            return true;
        }
        let minimum_interval_us = (1_000_000.0 / self.config.max_change_rate_hz)
            .ceil()
            .clamp(1.0, u64::MAX as f64) as u64;
        timestamp_us
            .checked_sub(last_change_us)
            .is_none_or(|elapsed| elapsed < minimum_interval_us)
    }
}

/// Degradation status for the policy controller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PolicyDegradation {
    Healthy,
    Tightening {
        expected_loss: f64,
    },
    EmergencyShed {
        total_decisions: u64,
        last_loss: f64,
    },
}

impl std::fmt::Display for PolicyDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Tightening { expected_loss } => {
                write!(f, "tightening(loss={expected_loss:.3})")
            }
            Self::EmergencyShed {
                total_decisions,
                last_loss,
            } => {
                write!(
                    f,
                    "emergency_shed(decisions={total_decisions}, loss={last_loss:.3})"
                )
            }
        }
    }
}

/// Structured log entry for the policy controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyControllerLogEntry {
    pub current_action: PolicyAction,
    pub total_decisions: u64,
    pub action_counts: [u64; 4],
    pub last_expected_loss: f64,
    pub hysteresis_count: u64,
    pub rate_limit_count: u64,
    pub degradation: PolicyDegradation,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_distribution_and_counters_handle_saturation() {
        let mut controller = PolicyController::with_defaults();
        controller.set_hysteresis(2.0);
        controller.total_decisions = u64::MAX - 1;
        controller.hysteresis_count = u64::MAX - 1;
        controller.action_counts = [u64::MAX; 4];

        let action = controller.decide([0.0, 0.0, 0.0, 1.0], 1);

        assert_eq!(action, PolicyAction::Hold);
        assert_eq!(controller.total_decisions, u64::MAX);
        assert_eq!(controller.hysteresis_count, u64::MAX);
        assert_eq!(controller.action_counts, [u64::MAX; 4]);
        assert_eq!(
            controller.action_distribution().map(f64::to_bits),
            [0.25f64.to_bits(); 4]
        );
    }

    #[test]
    fn malformed_config_and_posterior_fail_safe_without_panicking() {
        let mut controller = PolicyController::new(PolicyControllerConfig {
            loss_matrix: vec![f64::NAN],
            critical_floor: f64::INFINITY,
            max_change_rate_hz: f64::NAN,
            hysteresis: f64::NEG_INFINITY,
        });

        let action = controller.decide([f64::NAN, f64::NEG_INFINITY, -1.0, f64::INFINITY], 1);

        assert_eq!(action, PolicyAction::Shed);
        let decision = controller
            .recent_decisions(1)
            .into_iter()
            .next()
            .expect("the fail-safe decision is retained");
        assert_eq!(
            decision.state_probs.map(f64::to_bits),
            [
                0.0f64.to_bits(),
                0.0f64.to_bits(),
                0.0f64.to_bits(),
                1.0f64.to_bits()
            ]
        );
        assert!(decision.all_losses.iter().all(|loss| loss.is_finite()));
    }

    #[test]
    fn malformed_entry_in_full_loss_matrix_uses_matching_default() {
        let defaults = PolicyControllerConfig::default_asymmetric();
        let mut config = defaults.clone();
        config.loss_matrix[7] = f64::NAN;

        let controller = PolicyController::new(config);

        assert_eq!(controller.config.loss_matrix.len(), 16);
        assert_eq!(
            controller.config.loss_matrix[7].to_bits(),
            defaults.loss_matrix[7].to_bits()
        );
    }

    #[test]
    fn critical_floor_survives_unnormalized_large_input() {
        let mut controller = PolicyController::with_defaults();

        let _ = controller.decide([100.0, 0.0, 0.0, 0.0], 1);

        let decision = controller
            .recent_decisions(1)
            .into_iter()
            .next()
            .expect("decision is retained");
        assert!(decision.state_probs[3] >= controller.config.critical_floor);
        assert!((decision.state_probs.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn finite_extreme_losses_cannot_overflow_expected_loss() {
        let mut controller = PolicyController::new(PolicyControllerConfig {
            loss_matrix: vec![f64::MAX; 16],
            ..PolicyControllerConfig::default_asymmetric()
        });

        let _ = controller.decide([f64::MAX; 4], 1);

        let decision = controller
            .recent_decisions(1)
            .into_iter()
            .next()
            .expect("decision is retained");
        assert!(decision.all_losses.iter().all(|loss| loss.is_finite()));
        assert!(decision.expected_loss.is_finite());
    }

    #[test]
    fn extreme_hysteresis_remains_finite_and_suppresses_a_switch() {
        let mut controller = PolicyController::with_defaults();
        controller.set_hysteresis(f64::MAX);

        let action = controller.decide([0.0, 0.0, 0.0, 1.0], 1);

        assert_eq!(action, PolicyAction::Hold);
        let decision = controller
            .recent_decisions(1)
            .into_iter()
            .next()
            .expect("decision is retained");
        assert!(decision.hysteresis_applied);
        assert!(decision.all_losses.iter().all(|loss| loss.is_finite()));
    }

    #[test]
    fn action_change_rate_guard_suppresses_fast_switch_and_allows_later_switch() {
        let mut controller = PolicyController::with_defaults();
        assert_eq!(
            controller.decide([0.0, 0.0, 0.0, 1.0], 100),
            PolicyAction::Shed
        );
        assert_eq!(
            controller.decide([1.0, 0.0, 0.0, 0.0], 200),
            PolicyAction::Shed
        );
        assert!(
            controller
                .recent_decisions(1)
                .first()
                .is_some_and(|decision| decision.rate_limit_applied)
        );
        assert_eq!(controller.rate_limit_count(), 1);

        assert_eq!(
            controller.decide([1.0, 0.0, 0.0, 0.0], 500_100),
            PolicyAction::Hold
        );
        assert_eq!(controller.rate_limit_count(), 1);
    }

    #[test]
    fn backwards_timestamp_cannot_bypass_action_change_rate_guard() {
        let mut controller = PolicyController::with_defaults();
        assert_eq!(
            controller.decide([0.0, 0.0, 0.0, 1.0], 1_000),
            PolicyAction::Shed
        );
        assert_eq!(
            controller.decide([1.0, 0.0, 0.0, 0.0], 999),
            PolicyAction::Shed
        );
        assert_eq!(controller.rate_limit_count(), 1);
    }
}
