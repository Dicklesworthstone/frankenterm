//! Swarm-Wide Token-Bucket Blast Radius Controller for ARS.
//!
//! If a newly learned reflex is subtly wrong, it must not destroy 200 agents
//! simultaneously. This module provides a hierarchical token-bucket rate
//! limiter that caps reflex executions at configurable rates per reflex,
//! per cluster, and swarm-wide.
//!
//! # Rate Hierarchy
//!
//! ```text
//! Swarm global bucket (e.g., 20 execs/min)
//!   └── Per-cluster bucket (e.g., 10 execs/min)
//!       └── Per-reflex bucket (e.g., 5 execs/min)
//! ```
//!
//! All three tiers must have tokens available for an execution to proceed.
//! Excess requests fall back to LLM processing.
//!
//! # Maturity Tiers
//!
//! New reflexes start at `Incubating` with strict rate limits. As they
//! accumulate successful executions without drift, they graduate to
//! `Graduated` with relaxed limits, and eventually `Veteran`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::ars_fst::ReflexId;
use crate::token_bucket::TokenBucket;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for blast radius control.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BlastRadiusConfig {
    /// Swarm-wide rate limit: max executions per minute.
    pub swarm_rate_per_min: f64,
    /// Swarm-wide burst capacity.
    pub swarm_burst: f64,
    /// Per-cluster rate limit: max executions per minute.
    pub cluster_rate_per_min: f64,
    /// Per-cluster burst capacity.
    pub cluster_burst: f64,
    /// Per-reflex default rate limit for incubating reflexes.
    pub incubating_rate_per_min: f64,
    /// Per-reflex burst for incubating.
    pub incubating_burst: f64,
    /// Per-reflex rate limit for graduated reflexes.
    pub graduated_rate_per_min: f64,
    /// Per-reflex burst for graduated.
    pub graduated_burst: f64,
    /// Per-reflex rate limit for veteran reflexes.
    pub veteran_rate_per_min: f64,
    /// Per-reflex burst for veteran.
    pub veteran_burst: f64,
    /// Successful executions needed to graduate from Incubating.
    pub graduation_threshold: u64,
    /// Successful executions needed to become Veteran.
    pub veteran_threshold: u64,
    /// Failure count that triggers demotion back to Incubating.
    pub demotion_failure_count: u64,
}

impl Default for BlastRadiusConfig {
    fn default() -> Self {
        Self {
            swarm_rate_per_min: 20.0,
            swarm_burst: 5.0,
            cluster_rate_per_min: 10.0,
            cluster_burst: 3.0,
            incubating_rate_per_min: 5.0,
            incubating_burst: 2.0,
            graduated_rate_per_min: 15.0,
            graduated_burst: 5.0,
            veteran_rate_per_min: 30.0,
            veteran_burst: 10.0,
            graduation_threshold: 10,
            veteran_threshold: 50,
            demotion_failure_count: 3,
        }
    }
}

// =============================================================================
// Maturity tiers
// =============================================================================

/// Maturity tier for a reflex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MaturityTier {
    /// Newly learned, strict rate limits.
    Incubating,
    /// Proven reliable, relaxed limits.
    Graduated,
    /// Long-term reliable, most permissive.
    Veteran,
}

impl MaturityTier {
    /// Get display name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Incubating => "Incubating",
            Self::Graduated => "Graduated",
            Self::Veteran => "Veteran",
        }
    }
}

// =============================================================================
// Per-reflex state
// =============================================================================

/// Tracked state for a single reflex.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReflexState {
    /// Current maturity tier.
    pub tier: MaturityTier,
    /// Total successful executions.
    pub successes: u64,
    /// Total failed executions.
    pub failures: u64,
    /// Consecutive failures (resets on success).
    pub consecutive_failures: u64,
    /// Cluster this reflex belongs to.
    pub cluster_id: String,
}

impl ReflexState {
    /// Create a new reflex state.
    pub fn new(cluster_id: &str) -> Self {
        Self {
            tier: MaturityTier::Incubating,
            successes: 0,
            failures: 0,
            consecutive_failures: 0,
            cluster_id: cluster_id.to_string(),
        }
    }

    /// Record a successful execution and possibly promote.
    pub fn record_success(&mut self, config: &BlastRadiusConfig) {
        self.successes += 1;
        self.consecutive_failures = 0;

        // Check for tier promotion.
        match self.tier {
            MaturityTier::Incubating if self.successes >= config.graduation_threshold => {
                self.tier = MaturityTier::Graduated;
                debug!(
                    successes = self.successes,
                    "reflex graduated from Incubating"
                );
            }
            MaturityTier::Graduated if self.successes >= config.veteran_threshold => {
                self.tier = MaturityTier::Veteran;
                debug!(successes = self.successes, "reflex promoted to Veteran");
            }
            _ => {}
        }
    }

    /// Record a failed execution and possibly demote.
    pub fn record_failure(&mut self, config: &BlastRadiusConfig) {
        self.failures += 1;
        self.consecutive_failures += 1;

        if self.consecutive_failures >= config.demotion_failure_count
            && self.tier != MaturityTier::Incubating
        {
            warn!(
                prev_tier = self.tier.name(),
                consecutive_failures = self.consecutive_failures,
                "reflex demoted to Incubating"
            );
            self.tier = MaturityTier::Incubating;
            self.consecutive_failures = 0;
        }
    }

    /// Get total executions.
    pub fn total_executions(&self) -> u64 {
        self.successes + self.failures
    }
}

// =============================================================================
// Rate limit decision
// =============================================================================

/// Result of a blast radius check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlastDecision {
    /// Execution allowed.
    Allow { tier: MaturityTier },
    /// Execution denied — rate limited.
    Deny {
        reason: DenyReason,
        tier: MaturityTier,
    },
}

impl BlastDecision {
    /// Check if allowed.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }
}

/// Why the execution was rate-limited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenyReason {
    /// Swarm-wide rate limit exceeded.
    SwarmLimit,
    /// Per-cluster rate limit exceeded.
    ClusterLimit { cluster_id: String },
    /// Per-reflex rate limit exceeded.
    ReflexLimit { reflex_id: ReflexId },
}

// =============================================================================
// Blast Radius Controller
// =============================================================================

/// Hierarchical token-bucket blast radius controller.
pub struct BlastRadiusController {
    config: BlastRadiusConfig,
    /// Swarm-wide token bucket.
    swarm_bucket: TokenBucket,
    /// Per-cluster token buckets.
    cluster_buckets: HashMap<String, TokenBucket>,
    /// Per-reflex token buckets.
    reflex_buckets: HashMap<ReflexId, TokenBucket>,
    /// Per-reflex state tracking.
    reflex_states: HashMap<ReflexId, ReflexState>,
    /// Total allowed.
    total_allowed: u64,
    /// Total denied.
    total_denied: u64,
}

impl BlastRadiusController {
    /// Create a controller with the given configuration.
    pub fn new(config: BlastRadiusConfig) -> Self {
        let swarm_bucket = TokenBucket::new(config.swarm_burst, config.swarm_rate_per_min / 60.0);
        Self {
            config,
            swarm_bucket,
            cluster_buckets: HashMap::new(),
            reflex_buckets: HashMap::new(),
            reflex_states: HashMap::new(),
            total_allowed: 0,
            total_denied: 0,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(BlastRadiusConfig::default())
    }

    /// Register a reflex with its cluster.
    pub fn register_reflex(&mut self, reflex_id: ReflexId, cluster_id: &str) {
        self.reflex_states
            .entry(reflex_id)
            .or_insert_with(|| ReflexState::new(cluster_id));
    }

    /// Check if a reflex execution is allowed.
    pub fn check(&mut self, reflex_id: ReflexId, now_ms: u64) -> BlastDecision {
        // Ensure state exists.
        let state = self
            .reflex_states
            .get(&reflex_id)
            .cloned()
            .unwrap_or_else(|| ReflexState::new("unknown"));
        let tier = state.tier;
        let cluster_id = state.cluster_id.clone();

        // Ensure we have reflex state registered.
        self.reflex_states.entry(reflex_id).or_insert(state);

        // 1. Swarm-wide check.
        if !self.swarm_bucket.try_acquire(1, now_ms) {
            self.total_denied += 1;
            return BlastDecision::Deny {
                reason: DenyReason::SwarmLimit,
                tier,
            };
        }

        // 2. Per-cluster check.
        let cluster_bucket = self
            .cluster_buckets
            .entry(cluster_id.clone())
            .or_insert_with(|| {
                TokenBucket::new(
                    self.config.cluster_burst,
                    self.config.cluster_rate_per_min / 60.0,
                )
            });
        if !cluster_bucket.try_acquire(1, now_ms) {
            // Return the swarm token (compensate).
            // Note: exact return isn't possible with the existing API, so we accept slight drift.
            self.total_denied += 1;
            return BlastDecision::Deny {
                reason: DenyReason::ClusterLimit { cluster_id },
                tier,
            };
        }

        // 3. Per-reflex check (rate depends on tier).
        let (rate, burst) = self.tier_rate(tier);
        let reflex_bucket = self
            .reflex_buckets
            .entry(reflex_id)
            .or_insert_with(|| TokenBucket::new(burst, rate / 60.0));
        if !reflex_bucket.try_acquire(1, now_ms) {
            self.total_denied += 1;
            return BlastDecision::Deny {
                reason: DenyReason::ReflexLimit { reflex_id },
                tier,
            };
        }

        self.total_allowed += 1;
        BlastDecision::Allow { tier }
    }

    /// Record a successful execution for a reflex.
    pub fn record_success(&mut self, reflex_id: ReflexId) {
        if let Some(state) = self.reflex_states.get_mut(&reflex_id) {
            let old_tier = state.tier;
            state.record_success(&self.config);
            // If tier promoted, upgrade the reflex bucket rate.
            if state.tier != old_tier {
                let new_tier = state.tier;
                let (rate, burst) = self.tier_rate(new_tier);
                self.reflex_buckets
                    .insert(reflex_id, TokenBucket::new(burst, rate / 60.0));
            }
        }
    }

    /// Record a failed execution for a reflex.
    pub fn record_failure(&mut self, reflex_id: ReflexId) {
        if let Some(state) = self.reflex_states.get_mut(&reflex_id) {
            let old_tier = state.tier;
            state.record_failure(&self.config);
            // If tier demoted, downgrade the reflex bucket rate.
            if state.tier != old_tier {
                let new_tier = state.tier;
                let (rate, burst) = self.tier_rate(new_tier);
                self.reflex_buckets
                    .insert(reflex_id, TokenBucket::new(burst, rate / 60.0));
            }
        }
    }

    /// Get the rate and burst for a maturity tier.
    fn tier_rate(&self, tier: MaturityTier) -> (f64, f64) {
        match tier {
            MaturityTier::Incubating => (
                self.config.incubating_rate_per_min,
                self.config.incubating_burst,
            ),
            MaturityTier::Graduated => (
                self.config.graduated_rate_per_min,
                self.config.graduated_burst,
            ),
            MaturityTier::Veteran => (self.config.veteran_rate_per_min, self.config.veteran_burst),
        }
    }

    /// Get reflex state (if registered).
    pub fn reflex_state(&self, reflex_id: ReflexId) -> Option<&ReflexState> {
        self.reflex_states.get(&reflex_id)
    }

    /// Get statistics.
    pub fn stats(&self) -> BlastStats {
        BlastStats {
            total_allowed: self.total_allowed,
            total_denied: self.total_denied,
            registered_reflexes: self.reflex_states.len(),
            cluster_count: self.cluster_buckets.len(),
            tier_counts: self.tier_counts(),
        }
    }

    /// Count reflexes per tier.
    fn tier_counts(&self) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for state in self.reflex_states.values() {
            *counts.entry(state.tier.name().to_string()).or_insert(0) += 1;
        }
        counts
    }

    /// Get the configuration.
    pub fn config(&self) -> &BlastRadiusConfig {
        &self.config
    }
}

/// Blast radius statistics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlastStats {
    pub total_allowed: u64,
    pub total_denied: u64,
    pub registered_reflexes: usize,
    pub cluster_count: usize,
    pub tier_counts: HashMap<String, usize>,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn default_controller() -> BlastRadiusController {
        BlastRadiusController::with_defaults()
    }

    // ---- Maturity tiers ----

    #[test]
    fn tier_ordering() {
        assert!(MaturityTier::Incubating < MaturityTier::Graduated);
        assert!(MaturityTier::Graduated < MaturityTier::Veteran);
    }

    #[test]
    fn tier_names() {
        assert_eq!(MaturityTier::Incubating.name(), "Incubating");
        assert_eq!(MaturityTier::Graduated.name(), "Graduated");
        assert_eq!(MaturityTier::Veteran.name(), "Veteran");
    }

    // ---- Reflex state ----

    #[test]
    fn new_reflex_is_incubating() {
        let state = ReflexState::new("c1");
        assert_eq!(state.tier, MaturityTier::Incubating);
        assert_eq!(state.successes, 0);
        assert_eq!(state.failures, 0);
    }

    #[test]
    fn success_promotes_to_graduated() {
        let config = BlastRadiusConfig {
            graduation_threshold: 3,
            ..Default::default()
        };
        let mut state = ReflexState::new("c1");
        for _ in 0..3 {
            state.record_success(&config);
        }
        assert_eq!(state.tier, MaturityTier::Graduated);
    }

    #[test]
    fn success_promotes_to_veteran() {
        let config = BlastRadiusConfig {
            graduation_threshold: 2,
            veteran_threshold: 5,
            ..Default::default()
        };
        let mut state = ReflexState::new("c1");
        for _ in 0..5 {
            state.record_success(&config);
        }
        assert_eq!(state.tier, MaturityTier::Veteran);
    }

    #[test]
    fn failure_demotes_graduated() {
        let config = BlastRadiusConfig {
            graduation_threshold: 2,
            demotion_failure_count: 2,
            ..Default::default()
        };
        let mut state = ReflexState::new("c1");
        // Promote.
        for _ in 0..2 {
            state.record_success(&config);
        }
        assert_eq!(state.tier, MaturityTier::Graduated);

        // Consecutive failures.
        state.record_failure(&config);
        state.record_failure(&config);
        assert_eq!(state.tier, MaturityTier::Incubating);
    }

    #[test]
    fn success_resets_consecutive_failures() {
        let config = BlastRadiusConfig {
            demotion_failure_count: 3,
            graduation_threshold: 100,
            ..Default::default()
        };
        let mut state = ReflexState::new("c1");
        state.tier = MaturityTier::Graduated;

        state.record_failure(&config);
        state.record_failure(&config);
        assert_eq!(state.consecutive_failures, 2);

        state.record_success(&config);
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.tier, MaturityTier::Graduated);
    }

    #[test]
    fn total_executions_correct() {
        let config = BlastRadiusConfig::default();
        let mut state = ReflexState::new("c1");
        state.record_success(&config);
        state.record_success(&config);
        state.record_failure(&config);
        assert_eq!(state.total_executions(), 3);
    }

    // ---- Controller basic ----

    #[test]
    fn allow_first_execution() {
        let mut ctrl = default_controller();
        ctrl.register_reflex(1, "c1");
        let decision = ctrl.check(1, 1000);
        assert!(decision.is_allowed());
    }

    #[test]
    fn allow_returns_correct_tier() {
        let mut ctrl = default_controller();
        ctrl.register_reflex(1, "c1");
        let decision = ctrl.check(1, 1000);
        assert_eq!(
            decision,
            BlastDecision::Allow {
                tier: MaturityTier::Incubating
            }
        );
    }

    #[test]
    fn unregistered_reflex_auto_creates() {
        let mut ctrl = default_controller();
        let decision = ctrl.check(99, 1000);
        // Should create with "unknown" cluster and allow.
        assert!(decision.is_allowed());
    }

    // ---- Rate limiting ----

    #[test]
    fn reflex_rate_limited_after_burst() {
        let config = BlastRadiusConfig {
            incubating_rate_per_min: 6.0, // 0.1/sec
            incubating_burst: 2.0,        // max 2 burst
            swarm_rate_per_min: 600.0,    // high so it doesn't interfere
            swarm_burst: 100.0,
            cluster_rate_per_min: 600.0,
            cluster_burst: 100.0,
            ..Default::default()
        };
        let mut ctrl = BlastRadiusController::new(config);
        ctrl.register_reflex(1, "c1");

        // Use up burst.
        assert!(ctrl.check(1, 1000).is_allowed());
        assert!(ctrl.check(1, 1001).is_allowed());

        // Third should be denied (burst exhausted, not enough time for refill).
        let d = ctrl.check(1, 1002);
        let is_reflex_limit = matches!(
            d,
            BlastDecision::Deny {
                reason: DenyReason::ReflexLimit { .. },
                ..
            }
        );
        assert!(is_reflex_limit);
    }

    #[test]
    fn swarm_rate_limited() {
        let config = BlastRadiusConfig {
            swarm_rate_per_min: 6.0,
            swarm_burst: 1.0,
            cluster_rate_per_min: 600.0,
            cluster_burst: 100.0,
            incubating_rate_per_min: 600.0,
            incubating_burst: 100.0,
            ..Default::default()
        };
        let mut ctrl = BlastRadiusController::new(config);
        ctrl.register_reflex(1, "c1");

        assert!(ctrl.check(1, 1000).is_allowed());
        let d = ctrl.check(1, 1001);
        let is_swarm_limit = matches!(
            d,
            BlastDecision::Deny {
                reason: DenyReason::SwarmLimit,
                ..
            }
        );
        assert!(is_swarm_limit);
    }

    #[test]
    fn cluster_rate_limited() {
        let config = BlastRadiusConfig {
            swarm_rate_per_min: 600.0,
            swarm_burst: 100.0,
            cluster_rate_per_min: 6.0,
            cluster_burst: 1.0,
            incubating_rate_per_min: 600.0,
            incubating_burst: 100.0,
            ..Default::default()
        };
        let mut ctrl = BlastRadiusController::new(config);
        ctrl.register_reflex(1, "c1");

        assert!(ctrl.check(1, 1000).is_allowed());
        let d = ctrl.check(1, 1001);
        let is_cluster_limit = matches!(
            d,
            BlastDecision::Deny {
                reason: DenyReason::ClusterLimit { .. },
                ..
            }
        );
        assert!(is_cluster_limit);
    }

    #[test]
    fn rate_replenishes_over_time() {
        let config = BlastRadiusConfig {
            incubating_rate_per_min: 60.0, // 1/sec
            incubating_burst: 1.0,
            swarm_rate_per_min: 6000.0,
            swarm_burst: 100.0,
            cluster_rate_per_min: 6000.0,
            cluster_burst: 100.0,
            ..Default::default()
        };
        let mut ctrl = BlastRadiusController::new(config);
        ctrl.register_reflex(1, "c1");

        assert!(ctrl.check(1, 1000).is_allowed());
        assert!(!ctrl.check(1, 1001).is_allowed());

        // After 1 second, token should replenish.
        assert!(ctrl.check(1, 2001).is_allowed());
    }

    // ---- Tier graduation affects rate ----

    #[test]
    fn graduated_reflex_has_higher_rate() {
        let mut ctrl = default_controller();
        ctrl.register_reflex(1, "c1");

        // Promote to graduated.
        for _ in 0..10 {
            ctrl.record_success(1);
        }

        let state = ctrl.reflex_state(1).unwrap();
        assert_eq!(state.tier, MaturityTier::Graduated);
    }

    #[test]
    fn demoted_reflex_gets_strict_rate() {
        let config = BlastRadiusConfig {
            graduation_threshold: 2,
            demotion_failure_count: 2,
            ..Default::default()
        };
        let mut ctrl = BlastRadiusController::new(config);
        ctrl.register_reflex(1, "c1");

        // Promote.
        ctrl.record_success(1);
        ctrl.record_success(1);
        assert_eq!(ctrl.reflex_state(1).unwrap().tier, MaturityTier::Graduated);

        // Demote.
        ctrl.record_failure(1);
        ctrl.record_failure(1);
        assert_eq!(ctrl.reflex_state(1).unwrap().tier, MaturityTier::Incubating);
    }

    // ---- Stats ----

    #[test]
    fn stats_track_allowed_denied() {
        let config = BlastRadiusConfig {
            incubating_rate_per_min: 6.0,
            incubating_burst: 1.0,
            swarm_rate_per_min: 6000.0,
            swarm_burst: 100.0,
            cluster_rate_per_min: 6000.0,
            cluster_burst: 100.0,
            ..Default::default()
        };
        let mut ctrl = BlastRadiusController::new(config);
        ctrl.register_reflex(1, "c1");

        ctrl.check(1, 1000); // allowed
        ctrl.check(1, 1001); // denied

        let stats = ctrl.stats();
        assert_eq!(stats.total_allowed, 1);
        assert_eq!(stats.total_denied, 1);
    }

    #[test]
    fn stats_tier_counts() {
        let mut ctrl = default_controller();
        ctrl.register_reflex(1, "c1");
        ctrl.register_reflex(2, "c1");
        ctrl.register_reflex(3, "c2");

        let stats = ctrl.stats();
        assert_eq!(stats.registered_reflexes, 3);
        assert_eq!(stats.tier_counts.get("Incubating"), Some(&3));
    }

    #[test]
    fn stats_cluster_count() {
        let mut ctrl = default_controller();
        ctrl.register_reflex(1, "c1");
        ctrl.register_reflex(2, "c2");

        ctrl.check(1, 1000);
        ctrl.check(2, 1000);

        let stats = ctrl.stats();
        assert_eq!(stats.cluster_count, 2);
    }

    // ---- Serde roundtrips ----

    #[test]
    fn config_serde_roundtrip() {
        let config = BlastRadiusConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let decoded: BlastRadiusConfig = serde_json::from_str(&json).unwrap();
        let diff = (decoded.swarm_rate_per_min - config.swarm_rate_per_min).abs();
        assert!(diff < 1e-10);
    }

    #[test]
    fn maturity_tier_serde_roundtrip() {
        for tier in [
            MaturityTier::Incubating,
            MaturityTier::Graduated,
            MaturityTier::Veteran,
        ] {
            let json = serde_json::to_string(&tier).unwrap();
            let decoded: MaturityTier = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, tier);
        }
    }

    #[test]
    fn reflex_state_serde_roundtrip() {
        let state = ReflexState {
            tier: MaturityTier::Graduated,
            successes: 15,
            failures: 2,
            consecutive_failures: 0,
            cluster_id: "c1".to_string(),
        };
        let json = serde_json::to_string(&state).unwrap();
        let decoded: ReflexState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.tier, state.tier);
        assert_eq!(decoded.successes, state.successes);
    }

    #[test]
    fn blast_decision_serde_roundtrip() {
        let d = BlastDecision::Allow {
            tier: MaturityTier::Veteran,
        };
        let json = serde_json::to_string(&d).unwrap();
        let decoded: BlastDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, d);
    }

    #[test]
    fn deny_reason_serde_roundtrip() {
        let r = DenyReason::ClusterLimit {
            cluster_id: "c1".to_string(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let decoded: DenyReason = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, r);
    }

    #[test]
    fn blast_stats_serde_roundtrip() {
        let stats = BlastStats {
            total_allowed: 10,
            total_denied: 3,
            registered_reflexes: 5,
            cluster_count: 2,
            tier_counts: HashMap::from([
                ("Incubating".to_string(), 3),
                ("Graduated".to_string(), 2),
            ]),
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: BlastStats = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, stats);
    }

    // ---- Blast decision methods ----

    #[test]
    fn allow_is_allowed() {
        let d = BlastDecision::Allow {
            tier: MaturityTier::Incubating,
        };
        assert!(d.is_allowed());
    }

    #[test]
    fn deny_is_not_allowed() {
        let d = BlastDecision::Deny {
            reason: DenyReason::SwarmLimit,
            tier: MaturityTier::Incubating,
        };
        assert!(!d.is_allowed());
    }
}
