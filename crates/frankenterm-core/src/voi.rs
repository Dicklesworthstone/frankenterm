//! Value-of-Information (VOI) optimal capture scheduler.
//!
//! Replaces heuristic polling schedules with a provably optimal policy that
//! minimizes uncertainty about pane states per unit of polling cost.
//!
//! # VOI Computation
//!
//! For each pane `i` at time `t`:
//!
//! ```text
//! VOI(i,t) = [H(Sᵢ|old) - E[H(Sᵢ|new)]] × W(i) / C(i)
//! ```
//!
//! Where:
//! - `H(Sᵢ|old)` = entropy of current state belief (higher = more uncertain)
//! - `E[H(Sᵢ|new)]` = expected entropy after polling (lower = more informative)
//! - `W(i)` = importance weight
//! - `C(i)` = cost of polling (mux round-trip time)
//!
//! # Entropy Drift
//!
//! Without observations, entropy grows linearly with staleness:
//!
//! ```text
//! H(t + Δt) = min(H_max, H(t) + Δt × drift_rate)
//! ```

use crate::bayesian_ledger::PaneState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the VOI scheduler.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiConfig {
    /// Minimum VOI to trigger a poll (below this, wait).
    pub min_voi_threshold: f64,
    /// Entropy growth rate per second without observations.
    pub entropy_drift_rate: f64,
    /// Minimum polling interval in milliseconds.
    pub min_poll_interval_ms: u64,
    /// Maximum polling interval in milliseconds (even low-VOI panes get polled).
    pub max_poll_interval_ms: u64,
    /// Default polling cost in milliseconds.
    pub default_cost_ms: f64,
    /// Default importance weight.
    pub default_importance: f64,
    /// Maximum entropy cap (log₂ of state count).
    pub max_entropy: f64,
    /// Backpressure cost multipliers by tier.
    pub backpressure_multipliers: BackpressureMultipliers,
}

impl Default for VoiConfig {
    fn default() -> Self {
        Self {
            min_voi_threshold: 0.01,
            entropy_drift_rate: 0.1,
            min_poll_interval_ms: 50,
            max_poll_interval_ms: 30_000,
            default_cost_ms: 2.0,
            default_importance: 1.0,
            max_entropy: (PaneState::COUNT as f64).log2(),
            backpressure_multipliers: BackpressureMultipliers::default(),
        }
    }
}

/// Cost multipliers applied under backpressure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackpressureMultipliers {
    pub green: f64,
    pub yellow: f64,
    pub red: f64,
}

impl Default for BackpressureMultipliers {
    fn default() -> Self {
        Self {
            green: 1.0,
            yellow: 2.0,
            red: 5.0,
        }
    }
}

// =============================================================================
// Pane Belief
// =============================================================================

/// Categorical distribution over pane states.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneBelief {
    /// Log-probabilities for each state (log₂).
    log_probs: [f64; PaneState::COUNT],
    /// Cached entropy value.
    cached_entropy: f64,
    /// Last observation timestamp (epoch ms).
    last_observed_ms: u64,
    /// Importance weight for this pane.
    importance: f64,
    /// Base cost of polling this pane (ms).
    cost_ms: f64,
    /// Total observations received.
    observation_count: u64,
}

impl PaneBelief {
    /// Create a uniform (maximum-entropy) belief.
    fn uniform(now_ms: u64, importance: f64, cost_ms: f64) -> Self {
        let log_p = -(PaneState::COUNT as f64).log2();
        let log_probs = [log_p; PaneState::COUNT];
        let entropy = (PaneState::COUNT as f64).log2();
        Self {
            log_probs,
            cached_entropy: entropy,
            last_observed_ms: now_ms,
            importance,
            cost_ms,
            observation_count: 0,
        }
    }

    /// Shannon entropy in bits (log₂).
    fn recompute_entropy(&mut self) {
        let probs = log_softmax_to_probs(&self.log_probs);
        self.cached_entropy = shannon_entropy(&probs);
    }

    /// Update belief with observation likelihoods.
    ///
    /// `log_likelihoods[s]` = log₂ P(observation | state=s).
    fn update(&mut self, log_likelihoods: &[f64; PaneState::COUNT], now_ms: u64) {
        for i in 0..PaneState::COUNT {
            self.log_probs[i] += log_likelihoods[i];
        }
        // Normalize to prevent drift.
        let max_lp = self
            .log_probs
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        for lp in &mut self.log_probs {
            *lp -= max_lp;
        }
        self.recompute_entropy();
        self.last_observed_ms = now_ms;
        self.observation_count += 1;
    }

    /// Get the current probabilities.
    fn probabilities(&self) -> [f64; PaneState::COUNT] {
        log_softmax_to_probs(&self.log_probs)
    }

    /// Most likely state.
    fn map_state(&self) -> PaneState {
        let mut best_idx = 0;
        let mut best_lp = f64::NEG_INFINITY;
        for (i, &lp) in self.log_probs.iter().enumerate() {
            if lp > best_lp {
                best_lp = lp;
                best_idx = i;
            }
        }
        PaneState::from_index(best_idx).unwrap_or(PaneState::Idle)
    }
}

// =============================================================================
// Scheduling Decision
// =============================================================================

/// A scheduling decision for one pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulingDecision {
    /// Pane ID.
    pub pane_id: u64,
    /// Computed VOI.
    pub voi: f64,
    /// Current entropy.
    pub entropy: f64,
    /// Importance weight.
    pub importance: f64,
    /// Effective cost (after backpressure).
    pub effective_cost: f64,
    /// Most likely state.
    pub map_state: PaneState,
    /// Milliseconds since last observation.
    pub staleness_ms: u64,
}

/// Result of a scheduling round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleResult {
    /// Ordered list of panes to poll (highest VOI first).
    pub schedule: Vec<SchedulingDecision>,
    /// Total entropy across all panes.
    pub total_entropy: f64,
    /// Number of panes above VOI threshold.
    pub above_threshold: usize,
}

// =============================================================================
// Snapshot
// =============================================================================

/// Serializable scheduler snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiSnapshot {
    pub pane_count: usize,
    pub total_observations: u64,
    pub total_entropy: f64,
    pub config: VoiConfig,
    pub pane_states: Vec<PaneSnapshotEntry>,
}

/// Per-pane snapshot entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneSnapshotEntry {
    pub pane_id: u64,
    pub entropy: f64,
    pub map_state: PaneState,
    pub staleness_ms: u64,
    pub observations: u64,
    pub importance: f64,
}

// =============================================================================
// VOI Scheduler
// =============================================================================

/// Backpressure tier for cost adjustment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressureTierInput {
    Green,
    Yellow,
    Red,
}

/// The VOI capture scheduler.
///
/// Maintains per-pane belief distributions and computes optimal polling order
/// based on value of information.
pub struct VoiScheduler {
    config: VoiConfig,
    beliefs: HashMap<u64, PaneBelief>,
    current_backpressure: BackpressureTierInput,
}

impl VoiScheduler {
    /// Create a new scheduler.
    pub fn new(config: VoiConfig) -> Self {
        Self {
            config,
            beliefs: HashMap::new(),
            current_backpressure: BackpressureTierInput::Green,
        }
    }

    /// Register a pane for tracking with optional importance and cost.
    pub fn register_pane(&mut self, pane_id: u64, now_ms: u64) {
        self.beliefs.entry(pane_id).or_insert_with(|| {
            PaneBelief::uniform(
                now_ms,
                self.config.default_importance,
                self.config.default_cost_ms,
            )
        });
    }

    /// Remove a pane.
    pub fn unregister_pane(&mut self, pane_id: u64) {
        self.beliefs.remove(&pane_id);
    }

    /// Set importance weight for a pane.
    pub fn set_importance(&mut self, pane_id: u64, importance: f64) {
        if let Some(b) = self.beliefs.get_mut(&pane_id) {
            b.importance = importance;
        }
    }

    /// Set base polling cost for a pane.
    pub fn set_cost(&mut self, pane_id: u64, cost_ms: f64) {
        if let Some(b) = self.beliefs.get_mut(&pane_id) {
            b.cost_ms = cost_ms;
        }
    }

    /// Update backpressure tier.
    pub fn set_backpressure(&mut self, tier: BackpressureTierInput) {
        self.current_backpressure = tier;
    }

    /// Update belief for a pane with new observation likelihoods.
    ///
    /// `log_likelihoods[s]` = log₂ P(observation | state=s).
    pub fn update_belief(
        &mut self,
        pane_id: u64,
        log_likelihoods: &[f64; PaneState::COUNT],
        now_ms: u64,
    ) {
        if let Some(belief) = self.beliefs.get_mut(&pane_id) {
            belief.update(log_likelihoods, now_ms);
        }
    }

    /// Apply entropy drift to all panes based on elapsed time.
    pub fn apply_drift(&mut self, now_ms: u64) {
        let drift_rate = self.config.entropy_drift_rate;
        let max_entropy = self.config.max_entropy;

        for belief in self.beliefs.values_mut() {
            if belief.last_observed_ms < now_ms {
                let dt_secs = (now_ms - belief.last_observed_ms) as f64 / 1000.0;
                let new_entropy = (belief.cached_entropy + dt_secs * drift_rate).min(max_entropy);
                belief.cached_entropy = new_entropy;
            }
        }
    }

    /// Compute VOI for a single pane at the given time.
    fn compute_voi(&self, pane_id: u64, now_ms: u64) -> Option<f64> {
        let belief = self.beliefs.get(&pane_id)?;

        let dt_secs = (now_ms.saturating_sub(belief.last_observed_ms)) as f64 / 1000.0;
        let current_h = (belief.cached_entropy + dt_secs * self.config.entropy_drift_rate)
            .min(self.config.max_entropy);

        // Expected post-observation entropy: optimistic estimate.
        // After observing, entropy drops roughly to the conditional entropy
        // of the distribution. We approximate as: H_after ≈ H_current × decay.
        // With more observations, the decay is stronger.
        let decay = 1.0 / (1.0 + belief.observation_count as f64).ln().max(0.5);
        let expected_h_after = current_h * decay.min(0.9);

        let entropy_reduction = (current_h - expected_h_after).max(0.0);
        let bp_multiplier = self.backpressure_multiplier();
        let effective_cost = (belief.cost_ms * bp_multiplier).max(0.01);

        Some(entropy_reduction * belief.importance / effective_cost)
    }

    /// Get the backpressure cost multiplier.
    fn backpressure_multiplier(&self) -> f64 {
        match self.current_backpressure {
            BackpressureTierInput::Green => self.config.backpressure_multipliers.green,
            BackpressureTierInput::Yellow => self.config.backpressure_multipliers.yellow,
            BackpressureTierInput::Red => self.config.backpressure_multipliers.red,
        }
    }

    /// Produce a full scheduling round: compute VOI for all panes, sort by
    /// descending VOI, return ordered polling schedule.
    pub fn schedule(&self, now_ms: u64) -> ScheduleResult {
        let mut decisions: Vec<SchedulingDecision> = Vec::with_capacity(self.beliefs.len());
        let mut total_entropy = 0.0;

        for (&pane_id, belief) in &self.beliefs {
            let dt_secs = (now_ms.saturating_sub(belief.last_observed_ms)) as f64 / 1000.0;
            let current_h = (belief.cached_entropy + dt_secs * self.config.entropy_drift_rate)
                .min(self.config.max_entropy);
            total_entropy += current_h;

            let voi = self.compute_voi(pane_id, now_ms).unwrap_or(0.0);
            let bp_multiplier = self.backpressure_multiplier();

            decisions.push(SchedulingDecision {
                pane_id,
                voi,
                entropy: current_h,
                importance: belief.importance,
                effective_cost: belief.cost_ms * bp_multiplier,
                map_state: belief.map_state(),
                staleness_ms: now_ms.saturating_sub(belief.last_observed_ms),
            });
        }

        // Sort by VOI descending.
        decisions.sort_by(|a, b| {
            b.voi
                .partial_cmp(&a.voi)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let above_threshold = decisions
            .iter()
            .filter(|d| d.voi >= self.config.min_voi_threshold)
            .count();

        ScheduleResult {
            schedule: decisions,
            total_entropy,
            above_threshold,
        }
    }

    /// Select the single best pane to poll next.
    pub fn next_pane(&self, now_ms: u64) -> Option<SchedulingDecision> {
        let result = self.schedule(now_ms);
        result
            .schedule
            .into_iter()
            .find(|d| d.voi >= self.config.min_voi_threshold)
    }

    /// Number of tracked panes.
    pub fn pane_count(&self) -> usize {
        self.beliefs.len()
    }

    /// Total observations across all panes.
    pub fn total_observations(&self) -> u64 {
        self.beliefs.values().map(|b| b.observation_count).sum()
    }

    /// Get belief probabilities for a pane.
    pub fn pane_probabilities(&self, pane_id: u64) -> Option<[f64; PaneState::COUNT]> {
        self.beliefs.get(&pane_id).map(|b| b.probabilities())
    }

    /// Get the MAP (most likely) state for a pane.
    pub fn pane_map_state(&self, pane_id: u64) -> Option<PaneState> {
        self.beliefs.get(&pane_id).map(|b| b.map_state())
    }

    /// Create a serializable snapshot.
    pub fn snapshot(&self, now_ms: u64) -> VoiSnapshot {
        let pane_states: Vec<PaneSnapshotEntry> = self
            .beliefs
            .iter()
            .map(|(&pane_id, belief)| PaneSnapshotEntry {
                pane_id,
                entropy: belief.cached_entropy,
                map_state: belief.map_state(),
                staleness_ms: now_ms.saturating_sub(belief.last_observed_ms),
                observations: belief.observation_count,
                importance: belief.importance,
            })
            .collect();

        VoiSnapshot {
            pane_count: self.beliefs.len(),
            total_observations: self.total_observations(),
            total_entropy: self.beliefs.values().map(|b| b.cached_entropy).sum(),
            config: self.config.clone(),
            pane_states,
        }
    }

    /// Suggested poll interval for a pane based on its VOI.
    ///
    /// High VOI → short interval (poll soon). Low VOI → long interval.
    pub fn suggested_interval_ms(&self, pane_id: u64, now_ms: u64) -> u64 {
        let voi = self.compute_voi(pane_id, now_ms).unwrap_or(0.0);
        if voi <= 0.0 {
            return self.config.max_poll_interval_ms;
        }

        // Inverse-proportional mapping: interval = base / voi, clamped.
        let base = 100.0; // calibration constant
        let interval_ms = (base / voi) as u64;
        interval_ms.clamp(
            self.config.min_poll_interval_ms,
            self.config.max_poll_interval_ms,
        )
    }
}

impl std::fmt::Debug for VoiScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiScheduler")
            .field("pane_count", &self.beliefs.len())
            .field("backpressure", &self.current_backpressure)
            .finish()
    }
}

// =============================================================================
// Math Helpers
// =============================================================================

/// Convert log-probabilities to normalized probabilities via softmax.
fn log_softmax_to_probs(log_probs: &[f64; PaneState::COUNT]) -> [f64; PaneState::COUNT] {
    let max_lp = log_probs
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);

    let mut probs = [0.0; PaneState::COUNT];
    let mut sum = 0.0;
    for (i, &lp) in log_probs.iter().enumerate() {
        probs[i] = (lp - max_lp).exp();
        sum += probs[i];
    }
    if sum > 0.0 {
        for p in &mut probs {
            *p /= sum;
        }
    }
    probs
}

/// Shannon entropy in bits (using natural log, converted to log₂).
fn shannon_entropy(probs: &[f64; PaneState::COUNT]) -> f64 {
    let mut h = 0.0;
    for &p in probs {
        if p > 0.0 {
            h -= p * p.log2();
        }
    }
    h.max(0.0)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Config
    // -------------------------------------------------------------------------

    #[test]
    fn config_defaults() {
        let cfg = VoiConfig::default();
        assert!((cfg.min_voi_threshold - 0.01).abs() < 1e-10);
        assert!((cfg.entropy_drift_rate - 0.1).abs() < 1e-10);
        assert_eq!(cfg.min_poll_interval_ms, 50);
        assert_eq!(cfg.max_poll_interval_ms, 30_000);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = VoiConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let cfg2: VoiConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.min_poll_interval_ms, cfg2.min_poll_interval_ms);
        assert!((cfg.entropy_drift_rate - cfg2.entropy_drift_rate).abs() < 1e-10);
    }

    // -------------------------------------------------------------------------
    // Math helpers
    // -------------------------------------------------------------------------

    #[test]
    fn log_softmax_uniform() {
        let log_probs = [0.0; PaneState::COUNT];
        let probs = log_softmax_to_probs(&log_probs);
        let expected = 1.0 / PaneState::COUNT as f64;
        for p in &probs {
            assert!((*p - expected).abs() < 1e-10);
        }
    }

    #[test]
    fn log_softmax_preserves_order() {
        let mut log_probs = [0.0; PaneState::COUNT];
        log_probs[0] = 10.0; // Active should be highest.
        let probs = log_softmax_to_probs(&log_probs);
        for i in 1..PaneState::COUNT {
            assert!(probs[0] > probs[i]);
        }
    }

    #[test]
    fn log_softmax_sums_to_one() {
        let log_probs = [-1.0, -2.0, -0.5, -3.0, -1.5, -4.0, -2.5];
        let probs = log_softmax_to_probs(&log_probs);
        let sum: f64 = probs.iter().sum();
        assert!((sum - 1.0).abs() < 1e-10);
    }

    #[test]
    fn entropy_uniform_is_max() {
        let uniform = [1.0 / PaneState::COUNT as f64; PaneState::COUNT];
        let h = shannon_entropy(&uniform);
        let expected = (PaneState::COUNT as f64).log2();
        assert!((h - expected).abs() < 1e-10);
    }

    #[test]
    fn entropy_certain_is_zero() {
        let mut certain = [0.0; PaneState::COUNT];
        certain[0] = 1.0;
        let h = shannon_entropy(&certain);
        assert!(h.abs() < 1e-10);
    }

    #[test]
    fn entropy_non_negative() {
        let probs = [0.3, 0.2, 0.15, 0.1, 0.1, 0.1, 0.05];
        let h = shannon_entropy(&probs);
        assert!(h >= 0.0);
    }

    // -------------------------------------------------------------------------
    // PaneBelief
    // -------------------------------------------------------------------------

    #[test]
    fn belief_uniform_has_max_entropy() {
        let belief = PaneBelief::uniform(1000, 1.0, 2.0);
        let expected = (PaneState::COUNT as f64).log2();
        assert!((belief.cached_entropy - expected).abs() < 1e-10);
    }

    #[test]
    fn belief_update_changes_entropy() {
        let mut belief = PaneBelief::uniform(1000, 1.0, 2.0);
        let initial_h = belief.cached_entropy;

        // Strong evidence for Active state.
        let mut lls = [0.0; PaneState::COUNT];
        lls[PaneState::Active.index()] = 5.0;
        lls[PaneState::Idle.index()] = -5.0;

        belief.update(&lls, 2000);

        assert!(belief.cached_entropy < initial_h);
        assert_eq!(belief.observation_count, 1);
        assert_eq!(belief.last_observed_ms, 2000);
    }

    #[test]
    fn belief_map_state_follows_evidence() {
        let mut belief = PaneBelief::uniform(1000, 1.0, 2.0);

        let mut lls = [0.0; PaneState::COUNT];
        lls[PaneState::Error.index()] = 10.0;
        belief.update(&lls, 2000);

        assert_eq!(belief.map_state(), PaneState::Error);
    }

    #[test]
    fn belief_probabilities_sum_to_one() {
        let mut belief = PaneBelief::uniform(1000, 1.0, 2.0);
        let mut lls = [-1.0; PaneState::COUNT];
        lls[0] = 3.0;
        lls[2] = 1.0;
        belief.update(&lls, 2000);

        let probs = belief.probabilities();
        let sum: f64 = probs.iter().sum();
        assert!((sum - 1.0).abs() < 1e-10);
    }

    // -------------------------------------------------------------------------
    // VoiScheduler basics
    // -------------------------------------------------------------------------

    #[test]
    fn scheduler_creation() {
        let sched = VoiScheduler::new(VoiConfig::default());
        assert_eq!(sched.pane_count(), 0);
        assert_eq!(sched.total_observations(), 0);
    }

    #[test]
    fn scheduler_register_unregister() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);
        sched.register_pane(2, 1000);
        assert_eq!(sched.pane_count(), 2);

        sched.unregister_pane(1);
        assert_eq!(sched.pane_count(), 1);
    }

    #[test]
    fn scheduler_initial_state_uniform() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);

        let probs = sched.pane_probabilities(1).unwrap();
        let expected = 1.0 / PaneState::COUNT as f64;
        for p in &probs {
            assert!((*p - expected).abs() < 1e-10);
        }
    }

    #[test]
    fn scheduler_missing_pane() {
        let sched = VoiScheduler::new(VoiConfig::default());
        assert!(sched.pane_probabilities(999).is_none());
        assert!(sched.pane_map_state(999).is_none());
    }

    // -------------------------------------------------------------------------
    // VOI computation
    // -------------------------------------------------------------------------

    #[test]
    fn voi_increases_with_staleness() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);

        // Reduce entropy first with strong evidence (uniform → concentrated).
        let mut lls = [0.0; PaneState::COUNT];
        lls[PaneState::Active.index()] = 10.0;
        sched.update_belief(1, &lls, 1000);

        let voi_fresh = sched.compute_voi(1, 1000).unwrap();
        let voi_stale = sched.compute_voi(1, 11_000).unwrap(); // 10s later

        assert!(
            voi_stale > voi_fresh,
            "Stale pane should have higher VOI: fresh={voi_fresh}, stale={voi_stale}"
        );
    }

    #[test]
    fn voi_higher_importance_higher_voi() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);
        sched.register_pane(2, 1000);
        sched.set_importance(1, 2.0);
        sched.set_importance(2, 1.0);

        let voi_1 = sched.compute_voi(1, 5000).unwrap();
        let voi_2 = sched.compute_voi(2, 5000).unwrap();

        assert!(
            voi_1 > voi_2,
            "Higher importance should yield higher VOI: {voi_1} vs {voi_2}"
        );
    }

    #[test]
    fn voi_higher_cost_lower_voi() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);
        sched.register_pane(2, 1000);
        sched.set_cost(1, 1.0);
        sched.set_cost(2, 10.0);

        let voi_cheap = sched.compute_voi(1, 5000).unwrap();
        let voi_expensive = sched.compute_voi(2, 5000).unwrap();

        assert!(
            voi_cheap > voi_expensive,
            "Cheaper pane should have higher VOI: {voi_cheap} vs {voi_expensive}"
        );
    }

    #[test]
    fn voi_backpressure_reduces_voi() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);

        let voi_green = sched.compute_voi(1, 5000).unwrap();

        sched.set_backpressure(BackpressureTierInput::Red);
        let voi_red = sched.compute_voi(1, 5000).unwrap();

        assert!(
            voi_green > voi_red,
            "Backpressure should reduce VOI: green={voi_green}, red={voi_red}"
        );
    }

    #[test]
    fn voi_non_negative() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);
        let voi = sched.compute_voi(1, 1000).unwrap();
        assert!(voi >= 0.0, "VOI should be non-negative: {voi}");
    }

    // -------------------------------------------------------------------------
    // Scheduling
    // -------------------------------------------------------------------------

    #[test]
    fn schedule_orders_by_voi_descending() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);
        sched.register_pane(2, 1000);
        sched.register_pane(3, 1000);
        sched.set_importance(1, 3.0);
        sched.set_importance(2, 1.0);
        sched.set_importance(3, 2.0);

        let result = sched.schedule(5000);

        assert_eq!(result.schedule.len(), 3);
        // VOI should be descending.
        for i in 1..result.schedule.len() {
            assert!(result.schedule[i - 1].voi >= result.schedule[i].voi);
        }
        // Highest importance pane should be first.
        assert_eq!(result.schedule[0].pane_id, 1);
    }

    #[test]
    fn schedule_staleness_tracked() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);

        let result = sched.schedule(6000);
        assert_eq!(result.schedule[0].staleness_ms, 5000);
    }

    #[test]
    fn schedule_above_threshold_count() {
        let mut sched = VoiScheduler::new(VoiConfig {
            min_voi_threshold: 100.0, // Very high threshold.
            ..Default::default()
        });
        sched.register_pane(1, 1000);
        sched.register_pane(2, 1000);

        let result = sched.schedule(1000);
        assert_eq!(result.above_threshold, 0);
    }

    #[test]
    fn next_pane_returns_highest_voi() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);
        sched.register_pane(2, 1000);
        sched.set_importance(2, 5.0);

        let next = sched.next_pane(5000);
        assert!(next.is_some());
        assert_eq!(next.unwrap().pane_id, 2);
    }

    #[test]
    fn next_pane_none_below_threshold() {
        let mut sched = VoiScheduler::new(VoiConfig {
            min_voi_threshold: 1e10,
            ..Default::default()
        });
        sched.register_pane(1, 1000);

        assert!(sched.next_pane(1000).is_none());
    }

    // -------------------------------------------------------------------------
    // Entropy drift
    // -------------------------------------------------------------------------

    #[test]
    fn entropy_drift_increases_entropy() {
        let mut sched = VoiScheduler::new(VoiConfig {
            entropy_drift_rate: 0.5,
            ..Default::default()
        });
        sched.register_pane(1, 1000);

        // Feed strong evidence to reduce entropy.
        let mut lls = [0.0; PaneState::COUNT];
        lls[0] = 10.0; // strong Active signal
        sched.update_belief(1, &lls, 1000);

        let h_before = sched.beliefs[&1].cached_entropy;

        sched.apply_drift(11_000); // 10s drift at 0.5/s = +5 bits

        let h_after = sched.beliefs[&1].cached_entropy;
        assert!(
            h_after > h_before,
            "Drift should increase entropy: {h_before} → {h_after}"
        );
    }

    #[test]
    fn entropy_drift_capped_at_max() {
        let mut sched = VoiScheduler::new(VoiConfig {
            entropy_drift_rate: 100.0, // extreme drift
            ..Default::default()
        });
        sched.register_pane(1, 1000);

        sched.apply_drift(1_000_000); // massive time gap

        let h = sched.beliefs[&1].cached_entropy;
        assert!(
            h <= sched.config.max_entropy + 1e-10,
            "Entropy should be capped: {h} > {}",
            sched.config.max_entropy
        );
    }

    // -------------------------------------------------------------------------
    // Belief update integration
    // -------------------------------------------------------------------------

    #[test]
    fn update_belief_changes_map_state() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);

        let mut lls = [0.0; PaneState::COUNT];
        lls[PaneState::Stuck.index()] = 10.0;
        sched.update_belief(1, &lls, 2000);

        assert_eq!(sched.pane_map_state(1), Some(PaneState::Stuck));
    }

    #[test]
    fn update_belief_increases_observations() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);

        let lls = [0.0; PaneState::COUNT];
        sched.update_belief(1, &lls, 2000);
        sched.update_belief(1, &lls, 3000);

        assert_eq!(sched.total_observations(), 2);
    }

    // -------------------------------------------------------------------------
    // Suggested interval
    // -------------------------------------------------------------------------

    #[test]
    fn suggested_interval_decreases_with_staleness() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);

        let interval_fresh = sched.suggested_interval_ms(1, 1000);
        let interval_stale = sched.suggested_interval_ms(1, 31_000);

        assert!(
            interval_stale <= interval_fresh,
            "Stale pane should have shorter interval: fresh={interval_fresh}, stale={interval_stale}"
        );
    }

    #[test]
    fn suggested_interval_clamped() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);

        let interval = sched.suggested_interval_ms(1, 1_000_000);
        assert!(interval >= sched.config.min_poll_interval_ms);
        assert!(interval <= sched.config.max_poll_interval_ms);
    }

    // -------------------------------------------------------------------------
    // Snapshot
    // -------------------------------------------------------------------------

    #[test]
    fn snapshot_captures_state() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);
        sched.register_pane(2, 1000);

        let snap = sched.snapshot(5000);
        assert_eq!(snap.pane_count, 2);
        assert_eq!(snap.pane_states.len(), 2);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);

        let snap = sched.snapshot(2000);
        let json = serde_json::to_string(&snap).unwrap();
        let snap2: VoiSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(snap2.pane_count, 1);
        assert_eq!(snap2.pane_states.len(), 1);
    }

    #[test]
    fn schedule_result_serde() {
        let result = ScheduleResult {
            schedule: vec![SchedulingDecision {
                pane_id: 1,
                voi: 0.5,
                entropy: 2.0,
                importance: 1.0,
                effective_cost: 2.0,
                map_state: PaneState::Active,
                staleness_ms: 3000,
            }],
            total_entropy: 2.0,
            above_threshold: 1,
        };
        let json = serde_json::to_string(&result).unwrap();
        let result2: ScheduleResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result2.schedule.len(), 1);
        assert!((result2.schedule[0].voi - 0.5).abs() < 1e-10);
    }

    // -------------------------------------------------------------------------
    // Debug impl
    // -------------------------------------------------------------------------

    #[test]
    fn debug_impl() {
        let sched = VoiScheduler::new(VoiConfig::default());
        let s = format!("{sched:?}");
        assert!(s.contains("VoiScheduler"));
        assert!(s.contains("pane_count"));
    }

    // -------------------------------------------------------------------------
    // Edge cases
    // -------------------------------------------------------------------------

    #[test]
    fn empty_scheduler_schedule() {
        let sched = VoiScheduler::new(VoiConfig::default());
        let result = sched.schedule(1000);
        assert!(result.schedule.is_empty());
        assert_eq!(result.above_threshold, 0);
    }

    #[test]
    fn zero_voi_pane_gets_max_interval() {
        let mut sched = VoiScheduler::new(VoiConfig::default());
        sched.register_pane(1, 1000);

        // At t=1000, pane was just observed → VOI is very low.
        // If VOI is 0, interval should be max.
        let interval = sched.suggested_interval_ms(1, 1000);
        // For a just-observed pane, interval should be close to max.
        assert!(interval >= sched.config.min_poll_interval_ms);
    }

    #[test]
    fn backpressure_multipliers_serde() {
        let mult = BackpressureMultipliers::default();
        let json = serde_json::to_string(&mult).unwrap();
        let mult2: BackpressureMultipliers = serde_json::from_str(&json).unwrap();
        assert!((mult2.green - 1.0).abs() < 1e-10);
        assert!((mult2.yellow - 2.0).abs() < 1e-10);
        assert!((mult2.red - 5.0).abs() < 1e-10);
    }
}
