//! br-ft-ucb1: alien-uplift §UCB1 multi-armed bandit (Auer 2002)
//! for adaptive tool/server selection in MCP transport.
//!
//! Implements UCB1 (Auer, Cesa-Bianchi, Fischer (2002). "Finite-time
//! Analysis of the Multiarmed Bandit Problem". Machine Learning,
//! 47(2-3), 235-256). UCB1 is a regret-bounded online algorithm
//! that picks the arm maximizing
//! `r̄_a + sqrt(2 ln(N) / n_a)` — the empirical mean reward plus a
//! confidence interval that shrinks as the arm is pulled more
//! often. The optimal arm is selected `O(ln T)` times more than
//! suboptimal arms over T total pulls (Auer's Theorem 1).
//!
//! # When to choose UCB1 over deterministic preference order
//!
//! `crate::mcp_client::select_server_prefers_preferred_order` today
//! picks an MCP server by static preference list. That's the right
//! default for clear hierarchies (e.g., "always prefer local socket
//! over remote HTTP") but loses out when:
//!
//! - Two servers expose the same tool with different latency
//!   distributions and the operator can't statically order them.
//! - Server health shifts over time (one degrades, another
//!   recovers) — the static list ages poorly.
//! - The fleet observes per-tool latency variation that warrants
//!   per-tool selection (e.g., server A is fast for `wa.search`
//!   but slow for `wa.get_text`).
//!
//! UCB1 adapts: each MCP tool maintains its own bandit over the
//! servers offering that tool. Reward = inverse latency or
//! success-indicator. The selected server is empirically fastest
//! over recent observations, with `O(ln T)`-bounded suboptimal
//! pulls for exploration.
//!
//! # What ships
//!
//! - [`Ucb1Bandit`] — the algorithm state (per-arm pull counts +
//!   running reward means + total pulls).
//! - [`Ucb1Snapshot`] — serde snapshot for telemetry.
//!
//! # What is deferred
//!
//! - Wiring into `crate::mcp_client::select_server_prefers_preferred_order`
//!   (the operator opt-in).
//! - Time-decay variant (UCB1 assumes stationary rewards;
//!   discounted-UCB or sliding-window-UCB tracks non-stationary
//!   regimes — Garivier & Moulines 2011).

use serde::{Deserialize, Serialize};

/// br-ft-ucb1: UCB1 multi-armed bandit (Auer 2002).
///
/// Stateful per-context selector. For MCP transport, instantiate
/// one bandit per tool name with `num_arms = number of available
/// servers offering that tool`. Pull cycle:
/// `select() → invoke server → record_reward(arm, normalized_reward)`.
///
/// Reward should be normalized to `[0.0, 1.0]` for the
/// regret-bound theorem to apply (Auer 2002 Theorem 1). Common
/// normalizations:
/// - Latency: `reward = 1.0 / (1.0 + latency_ms / typical_ms)`
///   maps fast latencies → ~1.0, slow → ~0.0.
/// - Success indicator: `reward = if ok { 1.0 } else { 0.0 }`.
/// - Combined: `success_indicator × latency_score`.
#[derive(Debug, Clone)]
pub struct Ucb1Bandit {
    arm_pulls: Vec<u64>,
    /// Running mean reward per arm (Welford-style incremental update).
    arm_rewards: Vec<f64>,
    total_pulls: u64,
}

impl Ucb1Bandit {
    /// Create a new bandit with the given number of arms.
    /// Panics if `num_arms == 0` (UCB1 requires at least one arm).
    #[must_use]
    pub fn new(num_arms: usize) -> Self {
        assert!(num_arms > 0, "Ucb1Bandit requires at least one arm");
        Self {
            arm_pulls: vec![0; num_arms],
            arm_rewards: vec![0.0; num_arms],
            total_pulls: 0,
        }
    }

    /// Number of arms.
    #[must_use]
    pub fn num_arms(&self) -> usize {
        self.arm_pulls.len()
    }

    /// Total pulls across all arms.
    #[must_use]
    pub fn total_pulls(&self) -> u64 {
        self.total_pulls
    }

    /// Pull count for a single arm.
    ///
    /// Returns 0 if `arm` is out of range (defensive — should be
    /// caught by call-site bounds check).
    #[must_use]
    pub fn pull_count(&self, arm: usize) -> u64 {
        self.arm_pulls.get(arm).copied().unwrap_or(0)
    }

    /// Empirical mean reward for an arm. Returns 0.0 for un-pulled
    /// arms (UCB1 convention: prioritize them via infinite UCB
    /// bound, see [`Self::select`]).
    #[must_use]
    pub fn expected_reward(&self, arm: usize) -> f64 {
        self.arm_rewards.get(arm).copied().unwrap_or(0.0)
    }

    /// Select the next arm to pull via UCB1.
    ///
    /// **UCB1 initialization rule** (Auer 2002 §3): pull each arm
    /// once before invoking the UCB selection rule. While there
    /// exists an un-pulled arm, this function returns its index in
    /// arm-index order. Once every arm has been pulled at least
    /// once, the function returns the arm maximizing
    /// `r̄_a + sqrt(2 ln(N) / n_a)`.
    ///
    /// On ties (which can occur for un-pulled arms or for arms with
    /// identical UCB scores), the lowest arm-index wins for
    /// determinism — important for replay testing.
    pub fn select(&self) -> usize {
        // Initialization phase: pull each arm once first.
        for (i, &n) in self.arm_pulls.iter().enumerate() {
            if n == 0 {
                return i;
            }
        }
        // UCB phase: maximize r̄_a + sqrt(2 ln N / n_a).
        let n_total = self.total_pulls as f64;
        let ln_n = n_total.ln().max(0.0);
        let mut best_arm = 0;
        let mut best_score = f64::NEG_INFINITY;
        for (i, (&pulls, &reward)) in
            self.arm_pulls.iter().zip(self.arm_rewards.iter()).enumerate()
        {
            // Confidence interval: sqrt(2 ln N / n_a).
            let confidence = (2.0 * ln_n / pulls as f64).sqrt();
            let score = reward + confidence;
            if score > best_score {
                best_score = score;
                best_arm = i;
            }
        }
        best_arm
    }

    /// Select among a CONSTRAINED subset of arms (e.g., when some
    /// servers are circuit-broken or unhealthy). The same UCB1
    /// rule applies but only over arms where `allowed[i] = true`.
    /// Returns `None` if no allowed arm exists.
    pub fn select_constrained(&self, allowed: &[bool]) -> Option<usize> {
        if allowed.len() != self.arm_pulls.len() {
            return None;
        }
        // Initialization: any un-pulled allowed arm.
        for (i, (&n, &ok)) in self.arm_pulls.iter().zip(allowed.iter()).enumerate() {
            if ok && n == 0 {
                return Some(i);
            }
        }
        // UCB selection over allowed arms.
        let n_total = self.total_pulls as f64;
        let ln_n = n_total.ln().max(0.0);
        let mut best_arm = None;
        let mut best_score = f64::NEG_INFINITY;
        for (i, ((&pulls, &reward), &ok)) in self
            .arm_pulls
            .iter()
            .zip(self.arm_rewards.iter())
            .zip(allowed.iter())
            .enumerate()
        {
            if !ok || pulls == 0 {
                continue;
            }
            let confidence = (2.0 * ln_n / pulls as f64).sqrt();
            let score = reward + confidence;
            if score > best_score {
                best_score = score;
                best_arm = Some(i);
            }
        }
        best_arm
    }

    /// Record the observed reward for an arm. Updates the running
    /// mean via Welford's online algorithm
    /// (`mean_new = mean_old + (reward - mean_old) / n`) for
    /// numerical stability.
    ///
    /// Reward should be normalized to `[0.0, 1.0]` per Auer 2002's
    /// regret-bound theorem; values outside this range still
    /// accumulate correctly but the regret bound no longer
    /// applies. Non-finite rewards are silently dropped.
    ///
    /// Panics if `arm` is out of range.
    pub fn record_reward(&mut self, arm: usize, reward: f64) {
        if !reward.is_finite() {
            return;
        }
        assert!(arm < self.arm_pulls.len(), "arm index out of range");
        self.total_pulls = self.total_pulls.saturating_add(1);
        self.arm_pulls[arm] = self.arm_pulls[arm].saturating_add(1);
        let n = self.arm_pulls[arm] as f64;
        let mean = self.arm_rewards[arm];
        self.arm_rewards[arm] = mean + (reward - mean) / n;
    }

    /// Reset the bandit to its initial state (all arms un-pulled).
    /// Useful for cycle-based regimes where reward distributions
    /// shift periodically (a coarser-grained alternative to the
    /// proper sliding-window-UCB variant).
    pub fn reset(&mut self) {
        for p in self.arm_pulls.iter_mut() {
            *p = 0;
        }
        for r in self.arm_rewards.iter_mut() {
            *r = 0.0;
        }
        self.total_pulls = 0;
    }

    /// Snapshot for telemetry / mcp-doctor reports.
    #[must_use]
    pub fn snapshot(&self) -> Ucb1Snapshot {
        Ucb1Snapshot {
            num_arms: self.arm_pulls.len() as u32,
            total_pulls: self.total_pulls,
            arm_pulls: self.arm_pulls.clone(),
            arm_rewards: self.arm_rewards.clone(),
        }
    }
}

/// br-ft-ucb1: serde snapshot for telemetry / mcp-doctor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ucb1Snapshot {
    pub num_arms: u32,
    pub total_pulls: u64,
    pub arm_pulls: Vec<u64>,
    pub arm_rewards: Vec<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "at least one arm")]
    fn zero_arm_bandit_panics() {
        let _ = Ucb1Bandit::new(0);
    }

    #[test]
    fn empty_bandit_selects_first_arm_initialization_rule() {
        // br-ft-ucb1 initialization rule (Auer 2002 §3): pull each
        // arm once before applying the UCB selection rule. The
        // function returns un-pulled arms in arm-index order.
        let b = Ucb1Bandit::new(3);
        assert_eq!(b.select(), 0, "first un-pulled arm wins");
    }

    #[test]
    fn initialization_phase_walks_every_arm_in_order() {
        let mut b = Ucb1Bandit::new(3);
        assert_eq!(b.select(), 0);
        b.record_reward(0, 0.5);
        assert_eq!(b.select(), 1);
        b.record_reward(1, 0.5);
        assert_eq!(b.select(), 2);
        b.record_reward(2, 0.5);
        // After all arms pulled once, UCB phase begins.
        assert_eq!(b.total_pulls(), 3);
    }

    #[test]
    fn ucb_selection_favors_high_reward_arm() {
        // Pull each arm once with disparate rewards, then verify
        // subsequent select() prefers the high-reward arm.
        let mut b = Ucb1Bandit::new(3);
        b.record_reward(0, 0.9);
        b.record_reward(1, 0.5);
        b.record_reward(2, 0.1);
        // Initialization done; UCB phase. arm 0 has highest reward
        // and same pull count, so its UCB score wins.
        assert_eq!(b.select(), 0);
    }

    #[test]
    fn ucb_explores_under_pulled_arm_eventually() {
        // br-ft-ucb1 exploration: an arm pulled few times has a
        // larger confidence bound. Even with lower mean reward,
        // it can win selection if confidence dominates.
        let mut b = Ucb1Bandit::new(2);
        // Pull arm 0 many times with mediocre reward.
        for _ in 0..50 {
            b.record_reward(0, 0.5);
        }
        // Pull arm 1 once with low reward (50 + 1 = 51 total pulls).
        b.record_reward(1, 0.0);
        // Compute UCB scores manually:
        // ln(51) ≈ 3.93
        // arm 0: 0.5 + sqrt(2 × 3.93 / 50) ≈ 0.5 + 0.397 = 0.897
        // arm 1: 0.0 + sqrt(2 × 3.93 / 1) ≈ 0.0 + 2.804 = 2.804
        // arm 1 wins via exploration bonus.
        assert_eq!(b.select(), 1, "UCB exploration must select arm 1 despite lower reward");
    }

    #[test]
    fn convergence_picks_optimal_arm_most_often() {
        // br-ft-ucb1 regret bound (Auer 2002 Theorem 1): suboptimal
        // arms are pulled O(ln T) times. After many pulls, the
        // optimal arm dominates the pull distribution.
        let mut b = Ucb1Bandit::new(3);
        // Deterministic rewards: arm 0 = 0.8, arm 1 = 0.5, arm 2 = 0.2.
        for _ in 0..1000 {
            let arm = b.select();
            let reward = match arm {
                0 => 0.8,
                1 => 0.5,
                _ => 0.2,
            };
            b.record_reward(arm, reward);
        }
        let p0 = b.pull_count(0);
        let p1 = b.pull_count(1);
        let p2 = b.pull_count(2);
        assert!(
            p0 > p1 && p0 > p2,
            "br-ft-ucb1 convergence: arm 0 (highest reward) must be pulled most often \
             (got p0={p0} p1={p1} p2={p2})"
        );
        // Regret-bound sanity: arm 0 should dominate, suboptimal
        // arms should be ≤ a small fraction of total.
        assert!(
            p0 as f64 / 1000.0 > 0.7,
            "arm 0 should dominate (>70% of pulls); got {p0}/1000"
        );
    }

    #[test]
    fn select_constrained_skips_disallowed_arms() {
        let mut b = Ucb1Bandit::new(3);
        // All 3 arms pulled once with disparate rewards.
        b.record_reward(0, 0.9);
        b.record_reward(1, 0.5);
        b.record_reward(2, 0.1);
        // Constrain to arms 1 and 2 (e.g., arm 0 is circuit-broken).
        let allowed = [false, true, true];
        let selected = b.select_constrained(&allowed).expect("at least one allowed");
        assert!(selected == 1 || selected == 2);
        assert_ne!(selected, 0, "disallowed arm 0 must not be selected");
    }

    #[test]
    fn select_constrained_returns_none_when_no_arm_allowed() {
        let b = Ucb1Bandit::new(3);
        let allowed = [false, false, false];
        assert_eq!(b.select_constrained(&allowed), None);
    }

    #[test]
    fn select_constrained_mismatched_length_returns_none() {
        let b = Ucb1Bandit::new(3);
        let allowed = [true, true]; // wrong length
        assert_eq!(b.select_constrained(&allowed), None);
    }

    #[test]
    fn record_reward_drops_non_finite() {
        let mut b = Ucb1Bandit::new(2);
        b.record_reward(0, f64::NAN);
        b.record_reward(0, f64::INFINITY);
        b.record_reward(0, f64::NEG_INFINITY);
        assert_eq!(b.pull_count(0), 0);
        assert_eq!(b.total_pulls(), 0);
        b.record_reward(0, 0.5);
        assert_eq!(b.pull_count(0), 1);
    }

    #[test]
    fn reset_clears_all_state() {
        let mut b = Ucb1Bandit::new(3);
        for i in 0..3 {
            b.record_reward(i, 0.5);
        }
        assert_eq!(b.total_pulls(), 3);
        b.reset();
        assert_eq!(b.total_pulls(), 0);
        for i in 0..3 {
            assert_eq!(b.pull_count(i), 0);
            assert_eq!(b.expected_reward(i), 0.0);
        }
    }

    #[test]
    fn welford_running_mean_is_correct() {
        // Welford incremental update should give the same mean as
        // batch arithmetic for a simple sequence.
        let mut b = Ucb1Bandit::new(1);
        let rewards = [0.1, 0.5, 0.9, 0.3, 0.7];
        for r in rewards {
            b.record_reward(0, r);
        }
        let expected_mean = rewards.iter().sum::<f64>() / rewards.len() as f64; // 0.5
        let observed = b.expected_reward(0);
        assert!(
            (observed - expected_mean).abs() < 1e-9,
            "Welford running mean should equal batch mean: expected {expected_mean}, got {observed}"
        );
    }

    #[test]
    fn snapshot_carries_documented_fields() {
        let mut b = Ucb1Bandit::new(3);
        b.record_reward(0, 0.5);
        b.record_reward(1, 0.7);
        let snap = b.snapshot();
        assert_eq!(snap.num_arms, 3);
        assert_eq!(snap.total_pulls, 2);
        assert_eq!(snap.arm_pulls, vec![1, 1, 0]);
        assert!((snap.arm_rewards[0] - 0.5).abs() < 1e-9);
        assert!((snap.arm_rewards[1] - 0.7).abs() < 1e-9);
        assert!((snap.arm_rewards[2] - 0.0).abs() < 1e-9);
    }

    #[test]
    fn snapshot_serde_roundtrips() {
        let mut b = Ucb1Bandit::new(2);
        b.record_reward(0, 0.5);
        b.record_reward(1, 0.3);
        let snap = b.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let parsed: Ucb1Snapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, snap);
    }
}
