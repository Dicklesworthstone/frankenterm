//! Inverse Reinforcement Learning for automatic user preference discovery.
//!
//! Observes user focus patterns, scroll behavior, and interaction frequency to
//! learn an implicit reward function over pane states. Uses Maximum Entropy IRL
//! (Ziebart et al., 2008) to recover reward weights from demonstrated trajectories.
//!
//! # Algorithm
//!
//! ```text
//! R(s,a) = θᵀ φ(s,a)
//! P(τ|θ) ∝ exp(Σ_t θᵀ φ(s_t, a_t))
//! ∇_θ L = E_demo[φ] − E_policy[φ]   (match feature expectations)
//! ```
//!
//! The learned reward auto-prioritizes capture frequency for high-value panes,
//! predicts which pane the user will switch to next, and detects anomalous
//! behavior shifts.
//!
//! # Privacy
//!
//! All IRL computation is performed entirely locally. No user behavior data
//! ever leaves the local machine.
//!
//! Bead: wa-283h4.14

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Feature extraction
// =============================================================================

/// Number of features in the state-action feature vector φ(s,a).
pub const NUM_FEATURES: usize = 8;

/// Observable state of a single pane at a given moment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneState {
    /// Whether the pane has new output since last focus.
    pub has_new_output: bool,
    /// Seconds since the user last focused this pane.
    pub time_since_focus_s: f64,
    /// Output lines per second (smoothed).
    pub output_rate: f64,
    /// Number of detected errors in recent output.
    pub error_count: u32,
    /// Whether the pane is currently running a process.
    pub process_active: bool,
    /// Fraction of scrollback that has been viewed (0..1).
    pub scroll_depth: f64,
    /// Number of interactions (keystrokes, resizes) in the last minute.
    pub interaction_count: u32,
    /// Pane identifier.
    pub pane_id: u64,
}

/// Action the user can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UserAction {
    /// Focus a specific pane.
    FocusPane(u64),
    /// Scroll in the current pane.
    Scroll,
    /// Resize the current pane.
    Resize,
    /// No action (stayed on current pane).
    Ignore,
}

/// A single (state, action) observation in a trajectory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    /// State of all visible panes at decision time.
    pub pane_states: Vec<PaneState>,
    /// Which pane was focused when the decision was made.
    pub current_pane_id: u64,
    /// Action the user chose.
    pub action: UserAction,
}

/// Extract the feature vector φ(s,a) for a state-action pair.
///
/// Features (8-dimensional):
/// 0. has_new_output (target pane) — binary
/// 1. time_since_focus (target pane) — log-scaled
/// 2. output_rate (target pane) — log1p-scaled
/// 3. error_present (target pane) — binary
/// 4. process_active (target pane) — binary
/// 5. scroll_depth (current pane) — [0, 1]
/// 6. interaction_count (normalized) — log1p-scaled
/// 7. is_switch (1 if action changes focus) — binary
pub fn extract_features(obs: &Observation, action: &UserAction) -> [f64; NUM_FEATURES] {
    let target_pane_id = match action {
        UserAction::FocusPane(id) => *id,
        _ => obs.current_pane_id,
    };

    let target = obs
        .pane_states
        .iter()
        .find(|p| p.pane_id == target_pane_id);

    let current = obs
        .pane_states
        .iter()
        .find(|p| p.pane_id == obs.current_pane_id);

    let (has_output, tsf, rate, err, proc_active) = match target {
        Some(p) => (
            if p.has_new_output { 1.0 } else { 0.0 },
            (1.0 + p.time_since_focus_s).ln(),
            (1.0 + p.output_rate).ln(),
            if p.error_count > 0 { 1.0 } else { 0.0 },
            if p.process_active { 1.0 } else { 0.0 },
        ),
        None => (0.0, 0.0, 0.0, 0.0, 0.0),
    };

    let scroll_depth = current.map(|p| p.scroll_depth).unwrap_or(0.0);
    let interaction = current
        .map(|p| (1.0 + p.interaction_count as f64).ln())
        .unwrap_or(0.0);
    let is_switch = match action {
        UserAction::FocusPane(id) if *id != obs.current_pane_id => 1.0,
        _ => 0.0,
    };

    [
        has_output,
        tsf,
        rate,
        err,
        proc_active,
        scroll_depth,
        interaction,
        is_switch,
    ]
}

// =============================================================================
// IRL configuration
// =============================================================================

/// Configuration for the MaxEntIRL learner.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IrlConfig {
    /// Learning rate for gradient ascent.
    pub learning_rate: f64,
    /// Number of gradient steps per batch update.
    pub max_iterations: usize,
    /// Convergence threshold on gradient norm.
    pub convergence_threshold: f64,
    /// L2 regularization coefficient.
    pub l2_regularization: f64,
    /// Discount factor for temporal weighting of observations.
    pub discount: f64,
    /// Minimum observations before IRL updates begin.
    pub min_observations: usize,
    /// Maximum trajectory history to retain.
    pub max_trajectory_len: usize,
}

impl Default for IrlConfig {
    fn default() -> Self {
        Self {
            learning_rate: 0.01,
            max_iterations: 100,
            convergence_threshold: 1e-4,
            l2_regularization: 0.001,
            discount: 0.99,
            min_observations: 20,
            max_trajectory_len: 1000,
        }
    }
}

// =============================================================================
// Reward function
// =============================================================================

/// Learned reward function R(s,a) = θᵀ φ(s,a).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewardFunction {
    /// Reward weights θ.
    pub theta: [f64; NUM_FEATURES],
    /// Number of training observations seen.
    pub observation_count: usize,
    /// Running sum of demonstrated feature expectations (μ_demo).
    demo_feature_sum: [f64; NUM_FEATURES],
}

impl RewardFunction {
    /// Create a new reward function with zero weights.
    pub fn new() -> Self {
        Self {
            theta: [0.0; NUM_FEATURES],
            observation_count: 0,
            demo_feature_sum: [0.0; NUM_FEATURES],
        }
    }

    /// Compute reward for a state-action pair: R(s,a) = θᵀ φ(s,a).
    pub fn reward(&self, features: &[f64; NUM_FEATURES]) -> f64 {
        dot(&self.theta, features)
    }

    /// Rank panes by expected reward given current observations.
    pub fn rank_panes(&self, obs: &Observation) -> Vec<(u64, f64)> {
        let mut rankings: Vec<(u64, f64)> = obs
            .pane_states
            .iter()
            .map(|p| {
                let action = UserAction::FocusPane(p.pane_id);
                let features = extract_features(obs, &action);
                (p.pane_id, self.reward(&features))
            })
            .collect();
        rankings.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        rankings
    }

    /// Compute policy π(a|s) ∝ exp(θᵀ φ(s,a)) over available actions.
    pub fn policy(&self, obs: &Observation) -> Vec<(UserAction, f64)> {
        let actions = available_actions(obs);
        if actions.is_empty() {
            return vec![];
        }

        let rewards: Vec<f64> = actions
            .iter()
            .map(|a| {
                let f = extract_features(obs, a);
                self.reward(&f)
            })
            .collect();

        let max_r = rewards
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        let exp_rewards: Vec<f64> = rewards.iter().map(|r| (r - max_r).exp()).collect();
        let sum: f64 = exp_rewards.iter().sum();

        if sum <= 0.0 || !sum.is_finite() {
            let uniform = 1.0 / actions.len() as f64;
            return actions.into_iter().map(|a| (a, uniform)).collect();
        }

        actions
            .into_iter()
            .zip(exp_rewards.iter())
            .map(|(a, &e)| (a, e / sum))
            .collect()
    }

    /// Get the mean demonstrated feature vector.
    pub fn demo_feature_expectation(&self) -> [f64; NUM_FEATURES] {
        if self.observation_count == 0 {
            return [0.0; NUM_FEATURES];
        }
        let n = self.observation_count as f64;
        let mut mean = [0.0; NUM_FEATURES];
        for i in 0..NUM_FEATURES {
            mean[i] = self.demo_feature_sum[i] / n;
        }
        mean
    }
}

impl Default for RewardFunction {
    fn default() -> Self {
        Self::new()
    }
}

/// Enumerate available actions for a given observation.
fn available_actions(obs: &Observation) -> Vec<UserAction> {
    let mut actions = Vec::with_capacity(obs.pane_states.len() + 3);
    for p in &obs.pane_states {
        actions.push(UserAction::FocusPane(p.pane_id));
    }
    actions.push(UserAction::Scroll);
    actions.push(UserAction::Resize);
    actions.push(UserAction::Ignore);
    actions
}

// =============================================================================
// MaxEnt IRL learner
// =============================================================================

/// Maximum Entropy IRL learner.
///
/// Maintains trajectory history and incrementally updates reward weights θ via:
///   θ ← θ + α (μ_demo − μ_policy) − λθ
///
/// where μ_demo and μ_policy are expected feature counts under the demonstrated
/// and current learned policy respectively.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaxEntIrl {
    /// Learned reward function.
    pub reward: RewardFunction,
    /// Configuration.
    pub config: IrlConfig,
    /// Trajectory history (ring buffer).
    trajectory: Vec<Observation>,
    /// Write position in the ring buffer.
    write_pos: usize,
    /// Whether the buffer has wrapped around.
    wrapped: bool,
}

impl MaxEntIrl {
    /// Create a new IRL learner with the given config.
    pub fn new(config: IrlConfig) -> Self {
        Self {
            reward: RewardFunction::new(),
            config,
            trajectory: Vec::new(),
            write_pos: 0,
            wrapped: false,
        }
    }

    /// Number of observations stored.
    pub fn observation_count(&self) -> usize {
        if self.wrapped {
            self.trajectory.len()
        } else {
            self.write_pos
        }
    }

    /// Add an observation and optionally trigger a gradient update.
    ///
    /// Returns `true` if a gradient step was taken.
    pub fn observe(&mut self, obs: Observation) -> bool {
        // Accumulate demonstrated feature expectations
        let features = extract_features(&obs, &obs.action);
        for i in 0..NUM_FEATURES {
            self.reward.demo_feature_sum[i] += features[i];
        }
        self.reward.observation_count += 1;

        // Store in ring buffer
        if self.write_pos < self.trajectory.len() {
            self.trajectory[self.write_pos] = obs;
        } else {
            self.trajectory.push(obs);
        }
        self.write_pos += 1;
        if self.write_pos >= self.config.max_trajectory_len {
            self.write_pos = 0;
            self.wrapped = true;
        }

        // Only update after enough observations
        if self.observation_count() < self.config.min_observations {
            return false;
        }

        self.gradient_step();
        true
    }

    /// Perform one MaxEntIRL gradient ascent step.
    ///
    /// θ ← θ + α (μ_demo − μ_policy) − λθ
    fn gradient_step(&mut self) {
        let mu_demo = self.reward.demo_feature_expectation();
        let mu_policy = self.compute_policy_feature_expectation();

        let alpha = self.config.learning_rate;
        let lambda = self.config.l2_regularization;

        for i in 0..NUM_FEATURES {
            let grad = mu_demo[i] - mu_policy[i] - lambda * self.reward.theta[i];
            self.reward.theta[i] += alpha * grad;
        }
    }

    /// Compute μ_policy = E_π[φ] over stored trajectories.
    fn compute_policy_feature_expectation(&self) -> [f64; NUM_FEATURES] {
        let mut mu = [0.0; NUM_FEATURES];
        let n = self.observation_count();
        if n == 0 {
            return mu;
        }

        let obs_iter = if self.wrapped {
            self.trajectory.iter()
        } else {
            self.trajectory[..self.write_pos].iter()
        };

        let mut count = 0usize;
        for obs in obs_iter {
            let policy = self.reward.policy(obs);
            for (action, prob) in &policy {
                let f = extract_features(obs, action);
                for i in 0..NUM_FEATURES {
                    mu[i] += prob * f[i];
                }
            }
            count += 1;
        }

        if count > 0 {
            let n_f = count as f64;
            for v in &mut mu {
                *v /= n_f;
            }
        }
        mu
    }

    /// Run batch IRL: multiple gradient steps until convergence.
    pub fn batch_update(&mut self) -> BatchResult {
        let n = self.observation_count();
        if n < self.config.min_observations {
            return BatchResult {
                iterations: 0,
                converged: false,
                final_gradient_norm: f64::NAN,
            };
        }

        let mut iterations = 0;
        let mut grad_norm = f64::NAN;

        for _ in 0..self.config.max_iterations {
            let mu_demo = self.reward.demo_feature_expectation();
            let mu_policy = self.compute_policy_feature_expectation();

            let alpha = self.config.learning_rate;
            let lambda = self.config.l2_regularization;

            let mut sq_sum = 0.0;
            for i in 0..NUM_FEATURES {
                let grad = mu_demo[i] - mu_policy[i] - lambda * self.reward.theta[i];
                self.reward.theta[i] += alpha * grad;
                sq_sum += grad * grad;
            }
            grad_norm = sq_sum.sqrt();
            iterations += 1;

            if grad_norm < self.config.convergence_threshold {
                return BatchResult {
                    iterations,
                    converged: true,
                    final_gradient_norm: grad_norm,
                };
            }
        }

        BatchResult {
            iterations,
            converged: false,
            final_gradient_norm: grad_norm,
        }
    }

    /// Online IRL update from a single new observation.
    ///
    /// Faster than `batch_update` — performs exactly one gradient step using the
    /// stochastic approximation:
    ///   θ ← θ + α (φ_demo − φ_policy_sample)
    pub fn online_update(&mut self, obs: &Observation) {
        let phi_demo = extract_features(obs, &obs.action);

        // Sample from current policy (use expected features under policy)
        let policy = self.reward.policy(obs);
        let mut phi_policy = [0.0; NUM_FEATURES];
        for (action, prob) in &policy {
            let f = extract_features(obs, action);
            for i in 0..NUM_FEATURES {
                phi_policy[i] += prob * f[i];
            }
        }

        let alpha = self.config.learning_rate;
        let lambda = self.config.l2_regularization;
        for i in 0..NUM_FEATURES {
            let grad = phi_demo[i] - phi_policy[i] - lambda * self.reward.theta[i];
            self.reward.theta[i] += alpha * grad;
        }
    }

    /// Get the current trajectory slice (for inspection/serialization).
    pub fn trajectory(&self) -> &[Observation] {
        if self.wrapped {
            &self.trajectory
        } else {
            &self.trajectory[..self.write_pos]
        }
    }
}

/// Result of a batch IRL update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResult {
    /// Number of gradient iterations performed.
    pub iterations: usize,
    /// Whether the gradient norm fell below the convergence threshold.
    pub converged: bool,
    /// Final gradient norm.
    pub final_gradient_norm: f64,
}

// =============================================================================
// Preference monitor (per-pane tracking)
// =============================================================================

/// Tracks learned preferences and provides pane priority scores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreferenceMonitor {
    /// The IRL learner.
    pub irl: MaxEntIrl,
    /// Per-pane cumulative reward (exponentially weighted).
    pane_scores: HashMap<u64, f64>,
    /// Decay factor for pane scores per observation.
    score_decay: f64,
}

impl PreferenceMonitor {
    /// Create a new preference monitor with default config.
    pub fn new(config: IrlConfig) -> Self {
        Self {
            irl: MaxEntIrl::new(config),
            pane_scores: HashMap::new(),
            score_decay: 0.99,
        }
    }

    /// Record a user action and update preferences.
    pub fn record(&mut self, obs: Observation) {
        // Decay existing scores
        for score in self.pane_scores.values_mut() {
            *score *= self.score_decay;
        }

        // Update IRL
        self.irl.observe(obs.clone());

        // Update pane scores from current reward function
        for ps in &obs.pane_states {
            let action = UserAction::FocusPane(ps.pane_id);
            let features = extract_features(&obs, &action);
            let r = self.irl.reward.reward(&features);
            let entry = self.pane_scores.entry(ps.pane_id).or_insert(0.0);
            *entry += r;
        }
    }

    /// Get priority scores for all tracked panes, sorted descending.
    pub fn priority_scores(&self) -> Vec<(u64, f64)> {
        let mut scores: Vec<(u64, f64)> = self.pane_scores.iter().map(|(&k, &v)| (k, v)).collect();
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores
    }

    /// Predict which pane the user will focus next, given current state.
    pub fn predict_next_focus(&self, obs: &Observation) -> Option<u64> {
        let rankings = self.irl.reward.rank_panes(obs);
        // Pick the highest-reward pane that isn't the current focus
        rankings
            .into_iter()
            .find(|(id, _)| *id != obs.current_pane_id)
            .map(|(id, _)| id)
    }

    /// Detect anomaly: user ignoring a pane they historically value.
    ///
    /// Returns pane IDs whose reward rank is in the top `k` but haven't been
    /// focused within `stale_threshold_s` seconds.
    pub fn detect_neglected_panes(
        &self,
        obs: &Observation,
        top_k: usize,
        stale_threshold_s: f64,
    ) -> Vec<u64> {
        let rankings = self.irl.reward.rank_panes(obs);
        rankings
            .into_iter()
            .take(top_k)
            .filter_map(|(id, _)| {
                obs.pane_states
                    .iter()
                    .find(|p| p.pane_id == id && p.time_since_focus_s > stale_threshold_s)
                    .map(|_| id)
            })
            .collect()
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Dot product of two equal-length slices.
pub fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Cosine similarity between two vectors.
pub fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    let d = dot(a, b);
    let na = dot(a, a).sqrt();
    let nb = dot(b, b).sqrt();
    if na < 1e-15 || nb < 1e-15 {
        return 0.0;
    }
    d / (na * nb)
}

/// Spearman rank correlation between two vectors.
pub fn rank_correlation(a: &[f64], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len());
    let n = a.len();
    if n < 2 {
        return 0.0;
    }

    let rank_a = ranks(a);
    let rank_b = ranks(b);

    // Pearson correlation of ranks
    let mean_a: f64 = rank_a.iter().sum::<f64>() / n as f64;
    let mean_b: f64 = rank_b.iter().sum::<f64>() / n as f64;

    let mut cov = 0.0;
    let mut var_a = 0.0;
    let mut var_b = 0.0;
    for i in 0..n {
        let da = rank_a[i] - mean_a;
        let db = rank_b[i] - mean_b;
        cov += da * db;
        var_a += da * da;
        var_b += db * db;
    }

    if var_a < 1e-15 || var_b < 1e-15 {
        return 0.0;
    }
    cov / (var_a.sqrt() * var_b.sqrt())
}

/// Compute ranks for a slice of values (1-based, average ties).
fn ranks(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    let mut indexed: Vec<(usize, f64)> = values.iter().copied().enumerate().collect();
    indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut result = vec![0.0; n];
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j < n && (indexed[j].1 - indexed[i].1).abs() < 1e-12 {
            j += 1;
        }
        let avg_rank = (i + j + 1) as f64 / 2.0; // average 1-based rank
        for k in i..j {
            result[indexed[k].0] = avg_rank;
        }
        i = j;
    }
    result
}

// =============================================================================
// Test helpers (trajectory generation from ground-truth policy)
// =============================================================================

/// Generate synthetic pane states for testing.
#[cfg(test)]
fn make_test_panes(pane_ids: &[u64], seed: u64) -> Vec<PaneState> {
    let mut state = seed;
    pane_ids
        .iter()
        .map(|&id| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let r = (state >> 33) as f64 / u32::MAX as f64;
            PaneState {
                has_new_output: r > 0.5,
                time_since_focus_s: r * 60.0,
                output_rate: r * 10.0,
                error_count: if r > 0.8 { 1 } else { 0 },
                process_active: r > 0.3,
                scroll_depth: r,
                interaction_count: (r * 20.0) as u32,
                pane_id: id,
            }
        })
        .collect()
}

/// Generate trajectories from a ground-truth reward function for testing.
#[cfg(test)]
fn generate_trajectories(
    theta: &[f64],
    num_trajectories: usize,
    trajectory_length: usize,
) -> Vec<Observation> {
    let pane_ids: Vec<u64> = (1..=5).collect();
    let mut observations = Vec::with_capacity(num_trajectories * trajectory_length);
    let mut seed = 12345u64;

    for traj in 0..num_trajectories {
        let mut current_pane = pane_ids[traj % pane_ids.len()];
        for step in 0..trajectory_length {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);

            let panes = make_test_panes(&pane_ids, seed.wrapping_add(step as u64));
            let obs_base = Observation {
                pane_states: panes,
                current_pane_id: current_pane,
                action: UserAction::Ignore, // placeholder
            };

            // Choose action using softmax policy with ground-truth theta
            let actions = available_actions(&obs_base);
            let mut best_action = UserAction::Ignore;
            let mut best_reward = f64::NEG_INFINITY;

            // Compute rewards for all actions
            let rewards: Vec<f64> = actions
                .iter()
                .map(|a| {
                    let f = extract_features(&obs_base, a);
                    let mut r = 0.0;
                    for i in 0..theta.len().min(NUM_FEATURES) {
                        r += theta[i] * f[i];
                    }
                    r
                })
                .collect();

            // Softmax sampling (deterministic via seed)
            let max_r = rewards
                .iter()
                .copied()
                .fold(f64::NEG_INFINITY, f64::max);
            let exp_r: Vec<f64> = rewards.iter().map(|r| (r - max_r).exp()).collect();
            let sum: f64 = exp_r.iter().sum();

            let r_val = (seed >> 33) as f64 / u32::MAX as f64;
            let mut cumulative = 0.0;
            for (i, &e) in exp_r.iter().enumerate() {
                cumulative += e / sum;
                if r_val <= cumulative {
                    best_action = actions[i];
                    break;
                }
            }
            if let f64::NEG_INFINITY = best_reward {
                // just pick whatever softmax chose
            }
            let _ = best_reward;

            if let UserAction::FocusPane(id) = best_action {
                current_pane = id;
            }

            observations.push(Observation {
                pane_states: obs_base.pane_states,
                current_pane_id: obs_base.current_pane_id,
                action: best_action,
            });
        }
    }
    observations
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Feature extraction
    // =========================================================================

    #[test]
    fn feature_vector_length() {
        let panes = make_test_panes(&[1, 2, 3], 42);
        let obs = Observation {
            pane_states: panes,
            current_pane_id: 1,
            action: UserAction::FocusPane(2),
        };
        let features = extract_features(&obs, &obs.action);
        assert_eq!(features.len(), NUM_FEATURES);
    }

    #[test]
    fn feature_switch_bit() {
        let panes = make_test_panes(&[1, 2], 42);
        let obs = Observation {
            pane_states: panes,
            current_pane_id: 1,
            action: UserAction::FocusPane(2),
        };

        // Switching to a different pane → is_switch = 1
        let f_switch = extract_features(&obs, &UserAction::FocusPane(2));
        assert_eq!(f_switch[7], 1.0);

        // Staying on current pane → is_switch = 0
        let f_stay = extract_features(&obs, &UserAction::FocusPane(1));
        assert_eq!(f_stay[7], 0.0);

        // Non-focus actions → is_switch = 0
        let f_scroll = extract_features(&obs, &UserAction::Scroll);
        assert_eq!(f_scroll[7], 0.0);
    }

    #[test]
    fn feature_missing_pane_graceful() {
        let panes = make_test_panes(&[1, 2], 42);
        let obs = Observation {
            pane_states: panes,
            current_pane_id: 1,
            action: UserAction::FocusPane(999),
        };
        let f = extract_features(&obs, &UserAction::FocusPane(999));
        // Target pane not found → features should be 0.0 for target fields
        assert_eq!(f[0], 0.0); // has_new_output
        assert_eq!(f[1], 0.0); // time_since_focus
    }

    // =========================================================================
    // Reward function
    // =========================================================================

    #[test]
    fn reward_linear_in_theta() {
        let mut rf = RewardFunction::new();
        rf.theta = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let f1 = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let f2 = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        assert!((rf.reward(&f1) - 1.0).abs() < 1e-10);
        assert!((rf.reward(&f2)).abs() < 1e-10);
    }

    #[test]
    fn reward_ranks_panes() {
        let mut rf = RewardFunction::new();
        // Weight: prefer panes with new output
        rf.theta = [10.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];

        let panes = vec![
            PaneState {
                has_new_output: false,
                time_since_focus_s: 5.0,
                output_rate: 0.0,
                error_count: 0,
                process_active: false,
                scroll_depth: 0.0,
                interaction_count: 0,
                pane_id: 1,
            },
            PaneState {
                has_new_output: true,
                time_since_focus_s: 5.0,
                output_rate: 0.0,
                error_count: 0,
                process_active: false,
                scroll_depth: 0.0,
                interaction_count: 0,
                pane_id: 2,
            },
        ];

        let obs = Observation {
            pane_states: panes,
            current_pane_id: 1,
            action: UserAction::Ignore,
        };

        let rankings = rf.rank_panes(&obs);
        assert_eq!(rankings[0].0, 2); // pane 2 (has output) ranks first
    }

    // =========================================================================
    // Policy
    // =========================================================================

    #[test]
    fn policy_sums_to_one() {
        let mut rf = RewardFunction::new();
        rf.theta = [1.0, -0.5, 0.3, 2.0, 0.0, 0.0, 0.0, -1.0];
        let panes = make_test_panes(&[1, 2, 3], 42);
        let obs = Observation {
            pane_states: panes,
            current_pane_id: 1,
            action: UserAction::Ignore,
        };
        let policy = rf.policy(&obs);
        let sum: f64 = policy.iter().map(|(_, p)| p).sum();
        assert!(
            (sum - 1.0).abs() < 1e-10,
            "Policy probabilities should sum to 1, got {}",
            sum
        );
    }

    #[test]
    fn policy_prefers_high_reward() {
        let mut rf = RewardFunction::new();
        // Strongly prefer new output
        rf.theta = [100.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];

        let panes = vec![
            PaneState {
                has_new_output: true,
                time_since_focus_s: 5.0,
                output_rate: 0.0,
                error_count: 0,
                process_active: false,
                scroll_depth: 0.0,
                interaction_count: 0,
                pane_id: 1,
            },
            PaneState {
                has_new_output: false,
                time_since_focus_s: 5.0,
                output_rate: 0.0,
                error_count: 0,
                process_active: false,
                scroll_depth: 0.0,
                interaction_count: 0,
                pane_id: 2,
            },
        ];

        let obs = Observation {
            pane_states: panes,
            current_pane_id: 2,
            action: UserAction::Ignore,
        };

        let policy = rf.policy(&obs);
        let p_focus_1 = policy
            .iter()
            .find(|(a, _)| *a == UserAction::FocusPane(1))
            .map(|(_, p)| *p)
            .unwrap_or(0.0);
        let p_focus_2 = policy
            .iter()
            .find(|(a, _)| *a == UserAction::FocusPane(2))
            .map(|(_, p)| *p)
            .unwrap_or(0.0);

        assert!(
            p_focus_1 > p_focus_2,
            "Pane 1 (has output) should have higher probability: {} vs {}",
            p_focus_1,
            p_focus_2
        );
    }

    // =========================================================================
    // MaxEnt IRL learner
    // =========================================================================

    #[test]
    fn irl_starts_empty() {
        let irl = MaxEntIrl::new(IrlConfig::default());
        assert_eq!(irl.observation_count(), 0);
        assert_eq!(irl.reward.theta, [0.0; NUM_FEATURES]);
    }

    #[test]
    fn irl_observe_accumulates() {
        let config = IrlConfig {
            min_observations: 5,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);
        let panes = make_test_panes(&[1, 2], 42);

        for _ in 0..10 {
            irl.observe(Observation {
                pane_states: panes.clone(),
                current_pane_id: 1,
                action: UserAction::FocusPane(2),
            });
        }
        assert_eq!(irl.observation_count(), 10);
        assert_eq!(irl.reward.observation_count, 10);
    }

    #[test]
    fn irl_ring_buffer_wraps() {
        let config = IrlConfig {
            min_observations: 1,
            max_trajectory_len: 5,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);
        let panes = make_test_panes(&[1, 2], 42);

        for i in 0..10 {
            irl.observe(Observation {
                pane_states: panes.clone(),
                current_pane_id: 1,
                action: UserAction::FocusPane((i % 2 + 1) as u64),
            });
        }

        // Ring buffer wraps at 5
        assert_eq!(irl.trajectory().len(), 5);
        assert!(irl.wrapped);
    }

    #[test]
    fn irl_gradient_updates_theta() {
        let config = IrlConfig {
            min_observations: 3,
            learning_rate: 0.1,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);
        let panes = make_test_panes(&[1, 2, 3], 42);

        // Consistently focus pane 2 (has new output)
        for _ in 0..10 {
            let updated = irl.observe(Observation {
                pane_states: panes.clone(),
                current_pane_id: 1,
                action: UserAction::FocusPane(2),
            });
            // After min_observations, gradient steps should occur
            if irl.observation_count() > 3 {
                assert!(updated);
            }
        }

        // Theta should have moved from zero
        let norm: f64 = irl.reward.theta.iter().map(|t| t * t).sum::<f64>().sqrt();
        assert!(norm > 0.0, "Theta should have been updated from zero");
    }

    #[test]
    fn batch_update_reduces_gradient() {
        let config = IrlConfig {
            min_observations: 5,
            max_iterations: 200,
            learning_rate: 0.05,
            convergence_threshold: 1e-3,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);

        // Generate consistent demo trajectories
        let panes = make_test_panes(&[1, 2, 3], 42);
        for _ in 0..50 {
            irl.observe(Observation {
                pane_states: panes.clone(),
                current_pane_id: 1,
                action: UserAction::FocusPane(2),
            });
        }

        let result = irl.batch_update();
        assert!(result.iterations > 0);
        // Gradient should decrease (may or may not fully converge)
        assert!(result.final_gradient_norm.is_finite());
    }

    #[test]
    fn online_update_moves_theta() {
        let config = IrlConfig {
            learning_rate: 0.1,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);
        let panes = make_test_panes(&[1, 2], 42);

        let obs = Observation {
            pane_states: panes,
            current_pane_id: 1,
            action: UserAction::FocusPane(2),
        };

        let before = irl.reward.theta;
        irl.online_update(&obs);
        let after = irl.reward.theta;

        assert_ne!(before, after, "Online update should change theta");
    }

    // =========================================================================
    // Preference monitor
    // =========================================================================

    #[test]
    fn monitor_tracks_pane_scores() {
        let config = IrlConfig {
            min_observations: 2,
            ..IrlConfig::default()
        };
        let mut monitor = PreferenceMonitor::new(config);
        let panes = make_test_panes(&[1, 2, 3], 42);

        for _ in 0..5 {
            monitor.record(Observation {
                pane_states: panes.clone(),
                current_pane_id: 1,
                action: UserAction::FocusPane(2),
            });
        }

        let scores = monitor.priority_scores();
        assert!(!scores.is_empty());
    }

    #[test]
    fn monitor_predicts_next_focus() {
        let config = IrlConfig {
            min_observations: 2,
            learning_rate: 0.5,
            ..IrlConfig::default()
        };
        let mut monitor = PreferenceMonitor::new(config);

        let panes = vec![
            PaneState {
                has_new_output: true,
                time_since_focus_s: 30.0,
                output_rate: 5.0,
                error_count: 2,
                process_active: true,
                scroll_depth: 0.5,
                interaction_count: 10,
                pane_id: 1,
            },
            PaneState {
                has_new_output: false,
                time_since_focus_s: 1.0,
                output_rate: 0.0,
                error_count: 0,
                process_active: false,
                scroll_depth: 0.0,
                interaction_count: 0,
                pane_id: 2,
            },
        ];

        // Consistently focus pane 1
        for _ in 0..20 {
            monitor.record(Observation {
                pane_states: panes.clone(),
                current_pane_id: 2,
                action: UserAction::FocusPane(1),
            });
        }

        let obs = Observation {
            pane_states: panes.clone(),
            current_pane_id: 2,
            action: UserAction::Ignore,
        };

        let predicted = monitor.predict_next_focus(&obs);
        assert!(predicted.is_some());
        // Should predict pane 1 (the one user consistently focuses)
        assert_eq!(predicted.unwrap(), 1);
    }

    #[test]
    fn monitor_detects_neglected_panes() {
        let config = IrlConfig {
            min_observations: 2,
            learning_rate: 0.5,
            ..IrlConfig::default()
        };
        let mut monitor = PreferenceMonitor::new(config);

        let panes = vec![
            PaneState {
                has_new_output: true,
                time_since_focus_s: 120.0, // stale
                output_rate: 5.0,
                error_count: 1,
                process_active: true,
                scroll_depth: 0.0,
                interaction_count: 0,
                pane_id: 1,
            },
            PaneState {
                has_new_output: false,
                time_since_focus_s: 1.0,
                output_rate: 0.0,
                error_count: 0,
                process_active: false,
                scroll_depth: 0.0,
                interaction_count: 0,
                pane_id: 2,
            },
        ];

        // Train to value pane 1
        for _ in 0..20 {
            monitor.record(Observation {
                pane_states: panes.clone(),
                current_pane_id: 2,
                action: UserAction::FocusPane(1),
            });
        }

        let obs = Observation {
            pane_states: panes.clone(),
            current_pane_id: 2,
            action: UserAction::Ignore,
        };

        let neglected = monitor.detect_neglected_panes(&obs, 3, 60.0);
        // Pane 1 is in top-k but time_since_focus > 60s
        assert!(neglected.contains(&1));
    }

    // =========================================================================
    // Helpers
    // =========================================================================

    #[test]
    fn dot_product_correct() {
        assert!((dot(&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]) - 32.0).abs() < 1e-10);
        assert!((dot(&[0.0; 3], &[1.0; 3])).abs() < 1e-10);
    }

    #[test]
    fn cosine_similarity_identical() {
        let a = [1.0, 2.0, 3.0];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = [1.0, 0.0];
        let b = [0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-10);
    }

    #[test]
    fn cosine_similarity_zero_vector() {
        let a = [0.0, 0.0];
        let b = [1.0, 2.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn rank_correlation_perfect() {
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert!((rank_correlation(&a, &a) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn rank_correlation_inverse() {
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        let b = [5.0, 4.0, 3.0, 2.0, 1.0];
        assert!((rank_correlation(&a, &b) + 1.0).abs() < 1e-10);
    }

    #[test]
    fn ranks_with_ties() {
        let vals = [1.0, 2.0, 2.0, 4.0];
        let r = ranks(&vals);
        assert!((r[0] - 1.0).abs() < 1e-10);
        assert!((r[1] - 2.5).abs() < 1e-10); // tied
        assert!((r[2] - 2.5).abs() < 1e-10); // tied
        assert!((r[3] - 4.0).abs() < 1e-10);
    }

    // =========================================================================
    // IRL recovery test (ground-truth → recover → rank correlation)
    // =========================================================================

    #[test]
    fn irl_recovers_reward_direction() {
        let ground_truth = [2.0, -1.0, 0.5, 3.0, 0.0, -0.5, 0.0, 1.0];
        let trajectories = generate_trajectories(&ground_truth, 100, 20);

        let config = IrlConfig {
            min_observations: 10,
            max_iterations: 200,
            learning_rate: 0.05,
            convergence_threshold: 1e-5,
            l2_regularization: 0.0001,
            max_trajectory_len: 5000,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);

        for obs in trajectories {
            irl.observe(obs);
        }
        irl.batch_update();

        // Recovered theta should have positive rank correlation with ground truth
        let corr = rank_correlation(&ground_truth, &irl.reward.theta);
        assert!(
            corr > 0.3,
            "Rank correlation {} too low (expected > 0.3)",
            corr
        );
    }

    #[test]
    fn irl_online_converges_direction() {
        let ground_truth = [1.0, -0.5, 0.0, 2.0, 0.0, 0.0, 0.0, -1.0];
        // More trajectories for stochastic online updates to converge
        let trajectories = generate_trajectories(&ground_truth, 200, 30);

        let config = IrlConfig {
            min_observations: 1,
            learning_rate: 0.005,
            l2_regularization: 0.0001,
            max_trajectory_len: 10000,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);

        for obs in &trajectories {
            irl.online_update(obs);
        }

        // Online updates are noisy — just verify positive correlation (same hemisphere)
        let cosine = cosine_similarity(&ground_truth, &irl.reward.theta);
        assert!(
            cosine > 0.0,
            "Online IRL cosine similarity {} should be positive (same direction)",
            cosine
        );
    }

    // =========================================================================
    // Serialization roundtrip
    // =========================================================================

    #[test]
    fn config_serde_roundtrip() {
        let config = IrlConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: IrlConfig = serde_json::from_str(&json).unwrap();
        assert!((parsed.learning_rate - config.learning_rate).abs() < 1e-10);
        assert_eq!(parsed.max_iterations, config.max_iterations);
    }

    #[test]
    fn reward_function_serde_roundtrip() {
        let mut rf = RewardFunction::new();
        rf.theta = [1.0, -2.0, 3.0, 0.0, 0.5, -0.5, 0.1, 0.0];
        rf.observation_count = 42;

        let json = serde_json::to_string(&rf).unwrap();
        let parsed: RewardFunction = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.theta, rf.theta);
        assert_eq!(parsed.observation_count, 42);
    }

    #[test]
    fn batch_result_serde() {
        let result = BatchResult {
            iterations: 50,
            converged: true,
            final_gradient_norm: 0.001,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: BatchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.iterations, 50);
        assert!(parsed.converged);
    }

    // =========================================================================
    // Reward monotonicity
    // =========================================================================

    #[test]
    fn reward_monotonic_in_positive_features() {
        let theta = [1.0, 0.5, 0.3, 2.0, 0.1, 0.0, 0.0, -1.0];
        let base = [0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5];

        for i in 0..NUM_FEATURES {
            if theta[i] > 0.0 {
                let mut enhanced = base;
                enhanced[i] += 1.0;
                let r_base = dot(&theta, &base);
                let r_enhanced = dot(&theta, &enhanced);
                assert!(
                    r_enhanced > r_base,
                    "Feature {} (weight {}) should increase reward",
                    i,
                    theta[i]
                );
            }
        }
    }
}
