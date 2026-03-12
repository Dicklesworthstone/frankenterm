//! Capacity governor with rch-aware heavy workload control (ft-3681t.7.3).
//!
//! Detects system pressure and routes or throttles heavy compile/test operations
//! via rch offloading to avoid local contention under swarm load.
//!
//! # Architecture
//!
//! ```text
//! PressureSignals ──► CapacityGovernor.evaluate()
//!                              │
//!           CapacityGovernorConfig ──► thresholds
//!                              ▼
//!                     GovernorDecision
//!                              │
//!              ┌───────┬───────┼───────┬────────┐
//!              ▼       ▼       ▼       ▼        ▼
//!           Allow   Throttle  Offload  Block  Override
//! ```
//!
//! Decisions are observable via [`GovernorTelemetry`] counters and
//! overrideable via [`OperatorOverride`].

use serde::{Deserialize, Serialize};

use crate::runtime_telemetry::HealthTier;

// =============================================================================
// Workload classification
// =============================================================================

/// Category of workload for capacity governance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadCategory {
    /// Heavy: cargo build, cargo test, clippy — CPU/memory intensive.
    Heavy,
    /// Medium: rch exec, linting, formatting — moderate resource use.
    Medium,
    /// Light: git status, file reads, search — minimal resource use.
    Light,
}

impl WorkloadCategory {
    /// Estimated relative resource weight (higher = heavier).
    #[must_use]
    pub fn weight(self) -> u32 {
        match self {
            Self::Heavy => 10,
            Self::Medium => 3,
            Self::Light => 1,
        }
    }
}

// =============================================================================
// Pressure signals
// =============================================================================

/// System pressure signals consumed by the governor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PressureSignals {
    /// CPU utilization ratio (0.0..1.0).
    pub cpu_utilization: f64,
    /// Memory utilization ratio (0.0..1.0).
    pub memory_utilization: f64,
    /// Number of active heavy workloads (cargo builds, tests).
    pub active_heavy_workloads: u32,
    /// Number of active medium workloads.
    pub active_medium_workloads: u32,
    /// System load average (1-minute).
    pub load_average_1m: f64,
    /// Whether rch workers are available for offloading.
    pub rch_available: bool,
    /// Number of available rch workers (0 if rch unavailable).
    pub rch_workers_available: u32,
    /// Disk I/O pressure ratio (0.0..1.0), if measurable.
    pub io_pressure: f64,
    /// Timestamp in epoch milliseconds.
    pub timestamp_ms: u64,
}

impl Default for PressureSignals {
    fn default() -> Self {
        Self {
            cpu_utilization: 0.0,
            memory_utilization: 0.0,
            active_heavy_workloads: 0,
            active_medium_workloads: 0,
            load_average_1m: 0.0,
            rch_available: false,
            rch_workers_available: 0,
            io_pressure: 0.0,
            timestamp_ms: 0,
        }
    }
}

impl PressureSignals {
    /// Derive a health tier from the current pressure signals.
    #[must_use]
    pub fn health_tier(&self) -> HealthTier {
        let max_pressure = self
            .cpu_utilization
            .max(self.memory_utilization)
            .max(self.io_pressure);
        HealthTier::from_ratio(max_pressure)
    }
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the capacity governor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapacityGovernorConfig {
    /// Maximum concurrent heavy workloads before throttling.
    pub max_concurrent_heavy: u32,
    /// Maximum concurrent medium workloads before throttling.
    pub max_concurrent_medium: u32,
    /// CPU utilization threshold for throttling (0.0..1.0).
    pub cpu_throttle_threshold: f64,
    /// CPU utilization threshold for blocking (0.0..1.0).
    pub cpu_block_threshold: f64,
    /// Memory utilization threshold for throttling (0.0..1.0).
    pub memory_throttle_threshold: f64,
    /// Memory utilization threshold for blocking (0.0..1.0).
    pub memory_block_threshold: f64,
    /// Throttle delay in milliseconds for heavy workloads.
    pub heavy_throttle_delay_ms: u64,
    /// Throttle delay in milliseconds for medium workloads.
    pub medium_throttle_delay_ms: u64,
    /// Whether to prefer rch offloading over local throttling.
    pub prefer_rch_offload: bool,
    /// Maximum load average before blocking new heavy workloads.
    pub load_average_block_threshold: f64,
}

impl Default for CapacityGovernorConfig {
    fn default() -> Self {
        Self {
            max_concurrent_heavy: 2,
            max_concurrent_medium: 6,
            cpu_throttle_threshold: 0.80,
            cpu_block_threshold: 0.95,
            memory_throttle_threshold: 0.85,
            memory_block_threshold: 0.95,
            heavy_throttle_delay_ms: 5_000,
            medium_throttle_delay_ms: 1_000,
            prefer_rch_offload: true,
            load_average_block_threshold: 12.0,
        }
    }
}

// =============================================================================
// Decisions
// =============================================================================

/// Governor decision for a workload request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum GovernorDecision {
    /// Allow the workload to proceed immediately.
    Allow {
        reason: String,
    },
    /// Throttle: delay execution by the specified duration.
    Throttle {
        delay_ms: u64,
        reason: String,
    },
    /// Offload: redirect to rch for remote execution.
    Offload {
        reason: String,
    },
    /// Block: reject the workload entirely.
    Block {
        reason: String,
    },
    /// Operator override: allow regardless of pressure.
    Override {
        operator: String,
        original_decision: Box<GovernorDecision>,
    },
}

impl GovernorDecision {
    /// Whether this decision permits the workload to proceed (possibly delayed).
    #[must_use]
    pub fn is_permitted(&self) -> bool {
        !matches!(self, Self::Block { .. })
    }

    /// The reason string for this decision.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Allow { reason }
            | Self::Throttle { reason, .. }
            | Self::Offload { reason }
            | Self::Block { reason } => reason,
            Self::Override { .. } => "operator override",
        }
    }
}

// =============================================================================
// Operator override
// =============================================================================

/// An operator override that forces workloads through regardless of pressure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorOverride {
    /// Operator identity (agent name or user).
    pub operator: String,
    /// Optional workload category filter (None = all categories).
    pub category: Option<WorkloadCategory>,
    /// Override expiry timestamp in epoch milliseconds (0 = no expiry).
    pub expires_ms: u64,
    /// Reason for the override.
    pub reason: String,
}

impl OperatorOverride {
    /// Whether this override is still active at the given timestamp.
    #[must_use]
    pub fn is_active(&self, now_ms: u64) -> bool {
        self.expires_ms == 0 || now_ms < self.expires_ms
    }

    /// Whether this override applies to the given workload category.
    #[must_use]
    pub fn applies_to(&self, category: WorkloadCategory) -> bool {
        self.category.is_none() || self.category == Some(category)
    }
}

// =============================================================================
// Telemetry
// =============================================================================

/// Telemetry counters for governor decisions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GovernorTelemetry {
    pub evaluations: u64,
    pub allowed: u64,
    pub throttled: u64,
    pub offloaded: u64,
    pub blocked: u64,
    pub overrides: u64,
    pub last_evaluation_ms: u64,
}

impl GovernorTelemetry {
    fn record(&mut self, decision: &GovernorDecision, now_ms: u64) {
        self.evaluations += 1;
        self.last_evaluation_ms = now_ms;
        match decision {
            GovernorDecision::Allow { .. } => self.allowed += 1,
            GovernorDecision::Throttle { .. } => self.throttled += 1,
            GovernorDecision::Offload { .. } => self.offloaded += 1,
            GovernorDecision::Block { .. } => self.blocked += 1,
            GovernorDecision::Override { .. } => self.overrides += 1,
        }
    }
}

// =============================================================================
// Governor
// =============================================================================

/// Capacity governor that evaluates pressure signals against configurable
/// thresholds to produce allow/throttle/offload/block decisions.
pub struct CapacityGovernor {
    config: CapacityGovernorConfig,
    overrides: Vec<OperatorOverride>,
    telemetry: GovernorTelemetry,
    /// History of recent decisions for audit trail.
    decision_log: Vec<GovernorDecisionEntry>,
    max_log_entries: usize,
}

/// A logged governor decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernorDecisionEntry {
    pub timestamp_ms: u64,
    pub category: WorkloadCategory,
    pub decision: GovernorDecision,
    pub pressure_tier: HealthTier,
}

impl CapacityGovernor {
    /// Create a new governor with the given configuration.
    pub fn new(config: CapacityGovernorConfig) -> Self {
        Self {
            config,
            overrides: Vec::new(),
            telemetry: GovernorTelemetry::default(),
            decision_log: Vec::new(),
            max_log_entries: 1000,
        }
    }

    /// Create a governor with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(CapacityGovernorConfig::default())
    }

    /// Evaluate a workload request against current pressure signals.
    pub fn evaluate(
        &mut self,
        category: WorkloadCategory,
        signals: &PressureSignals,
    ) -> GovernorDecision {
        let now_ms = signals.timestamp_ms;

        // Check for active operator overrides first.
        self.overrides.retain(|o| o.is_active(now_ms));
        if let Some(ovr) = self
            .overrides
            .iter()
            .find(|o| o.applies_to(category) && o.is_active(now_ms))
        {
            let original = self.compute_decision(category, signals);
            let decision = GovernorDecision::Override {
                operator: ovr.operator.clone(),
                original_decision: Box::new(original),
            };
            self.record_decision(now_ms, category, &decision, signals);
            return decision;
        }

        let decision = self.compute_decision(category, signals);
        self.record_decision(now_ms, category, &decision, signals);
        decision
    }

    /// Add an operator override.
    pub fn add_override(&mut self, ovr: OperatorOverride) {
        self.overrides.push(ovr);
    }

    /// Remove all overrides for the given operator.
    pub fn remove_overrides(&mut self, operator: &str) {
        self.overrides.retain(|o| o.operator != operator);
    }

    /// Get the current telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &GovernorTelemetry {
        &self.telemetry
    }

    /// Get recent decision log entries.
    #[must_use]
    pub fn decision_log(&self) -> &[GovernorDecisionEntry] {
        &self.decision_log
    }

    /// Get the current configuration.
    #[must_use]
    pub fn config(&self) -> &CapacityGovernorConfig {
        &self.config
    }

    fn compute_decision(
        &self,
        category: WorkloadCategory,
        signals: &PressureSignals,
    ) -> GovernorDecision {
        // Block conditions: extreme pressure.
        if signals.cpu_utilization >= self.config.cpu_block_threshold
            || signals.memory_utilization >= self.config.memory_block_threshold
        {
            return GovernorDecision::Block {
                reason: format!(
                    "extreme pressure: cpu={:.0}% mem={:.0}%",
                    signals.cpu_utilization * 100.0,
                    signals.memory_utilization * 100.0,
                ),
            };
        }

        if signals.load_average_1m >= self.config.load_average_block_threshold
            && category == WorkloadCategory::Heavy
        {
            return GovernorDecision::Block {
                reason: format!(
                    "load average {:.1} exceeds threshold {:.1}",
                    signals.load_average_1m, self.config.load_average_block_threshold,
                ),
            };
        }

        // Concurrency limits for heavy workloads.
        if category == WorkloadCategory::Heavy
            && signals.active_heavy_workloads >= self.config.max_concurrent_heavy
        {
            if self.config.prefer_rch_offload && signals.rch_available {
                return GovernorDecision::Offload {
                    reason: format!(
                        "heavy concurrency limit ({}/{}), rch available ({} workers)",
                        signals.active_heavy_workloads,
                        self.config.max_concurrent_heavy,
                        signals.rch_workers_available,
                    ),
                };
            }
            return GovernorDecision::Throttle {
                delay_ms: self.config.heavy_throttle_delay_ms,
                reason: format!(
                    "heavy concurrency limit ({}/{})",
                    signals.active_heavy_workloads, self.config.max_concurrent_heavy,
                ),
            };
        }

        // Concurrency limits for medium workloads.
        if category == WorkloadCategory::Medium
            && signals.active_medium_workloads >= self.config.max_concurrent_medium
        {
            return GovernorDecision::Throttle {
                delay_ms: self.config.medium_throttle_delay_ms,
                reason: format!(
                    "medium concurrency limit ({}/{})",
                    signals.active_medium_workloads, self.config.max_concurrent_medium,
                ),
            };
        }

        // CPU/memory throttling for heavy workloads.
        if category == WorkloadCategory::Heavy {
            if signals.cpu_utilization >= self.config.cpu_throttle_threshold
                || signals.memory_utilization >= self.config.memory_throttle_threshold
            {
                if self.config.prefer_rch_offload && signals.rch_available {
                    return GovernorDecision::Offload {
                        reason: format!(
                            "elevated pressure: cpu={:.0}% mem={:.0}%, rch available",
                            signals.cpu_utilization * 100.0,
                            signals.memory_utilization * 100.0,
                        ),
                    };
                }
                return GovernorDecision::Throttle {
                    delay_ms: self.config.heavy_throttle_delay_ms,
                    reason: format!(
                        "elevated pressure: cpu={:.0}% mem={:.0}%",
                        signals.cpu_utilization * 100.0,
                        signals.memory_utilization * 100.0,
                    ),
                };
            }
        }

        GovernorDecision::Allow {
            reason: "within capacity".to_string(),
        }
    }

    fn record_decision(
        &mut self,
        now_ms: u64,
        category: WorkloadCategory,
        decision: &GovernorDecision,
        signals: &PressureSignals,
    ) {
        self.telemetry.record(decision, now_ms);
        let entry = GovernorDecisionEntry {
            timestamp_ms: now_ms,
            category,
            decision: decision.clone(),
            pressure_tier: signals.health_tier(),
        };
        self.decision_log.push(entry);
        if self.decision_log.len() > self.max_log_entries {
            let excess = self.decision_log.len() - self.max_log_entries;
            self.decision_log.drain(..excess);
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn default_signals() -> PressureSignals {
        PressureSignals {
            cpu_utilization: 0.3,
            memory_utilization: 0.4,
            active_heavy_workloads: 0,
            active_medium_workloads: 0,
            load_average_1m: 2.0,
            rch_available: false,
            rch_workers_available: 0,
            io_pressure: 0.1,
            timestamp_ms: 1000,
        }
    }

    #[test]
    fn allow_light_workload_under_low_pressure() {
        let mut gov = CapacityGovernor::with_defaults();
        let decision = gov.evaluate(WorkloadCategory::Light, &default_signals());
        assert!(matches!(decision, GovernorDecision::Allow { .. }));
        assert!(decision.is_permitted());
        assert_eq!(gov.telemetry().evaluations, 1);
        assert_eq!(gov.telemetry().allowed, 1);
    }

    #[test]
    fn allow_heavy_workload_under_low_pressure() {
        let mut gov = CapacityGovernor::with_defaults();
        let decision = gov.evaluate(WorkloadCategory::Heavy, &default_signals());
        assert!(matches!(decision, GovernorDecision::Allow { .. }));
    }

    #[test]
    fn block_under_extreme_cpu_pressure() {
        let mut gov = CapacityGovernor::with_defaults();
        let mut signals = default_signals();
        signals.cpu_utilization = 0.96;
        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        assert!(matches!(decision, GovernorDecision::Block { .. }));
        assert!(!decision.is_permitted());
        assert_eq!(gov.telemetry().blocked, 1);
    }

    #[test]
    fn block_under_extreme_memory_pressure() {
        let mut gov = CapacityGovernor::with_defaults();
        let mut signals = default_signals();
        signals.memory_utilization = 0.96;
        let decision = gov.evaluate(WorkloadCategory::Light, &signals);
        assert!(matches!(decision, GovernorDecision::Block { .. }));
    }

    #[test]
    fn block_heavy_under_high_load_average() {
        let mut gov = CapacityGovernor::with_defaults();
        let mut signals = default_signals();
        signals.load_average_1m = 15.0;
        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        assert!(matches!(decision, GovernorDecision::Block { .. }));
    }

    #[test]
    fn light_allowed_under_high_load_average() {
        let mut gov = CapacityGovernor::with_defaults();
        let mut signals = default_signals();
        signals.load_average_1m = 15.0;
        let decision = gov.evaluate(WorkloadCategory::Light, &signals);
        // Light workloads not blocked by load average alone
        assert!(matches!(decision, GovernorDecision::Allow { .. }));
    }

    #[test]
    fn throttle_heavy_at_concurrency_limit_no_rch() {
        let mut gov = CapacityGovernor::with_defaults();
        let mut signals = default_signals();
        signals.active_heavy_workloads = 2;
        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        assert!(matches!(decision, GovernorDecision::Throttle { .. }));
        assert_eq!(gov.telemetry().throttled, 1);
    }

    #[test]
    fn offload_heavy_at_concurrency_limit_with_rch() {
        let mut gov = CapacityGovernor::with_defaults();
        let mut signals = default_signals();
        signals.active_heavy_workloads = 2;
        signals.rch_available = true;
        signals.rch_workers_available = 3;
        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        assert!(matches!(decision, GovernorDecision::Offload { .. }));
        assert_eq!(gov.telemetry().offloaded, 1);
    }

    #[test]
    fn throttle_medium_at_concurrency_limit() {
        let mut gov = CapacityGovernor::with_defaults();
        let mut signals = default_signals();
        signals.active_medium_workloads = 6;
        let decision = gov.evaluate(WorkloadCategory::Medium, &signals);
        assert!(matches!(decision, GovernorDecision::Throttle { .. }));
    }

    #[test]
    fn offload_heavy_under_elevated_pressure_with_rch() {
        let mut gov = CapacityGovernor::with_defaults();
        let mut signals = default_signals();
        signals.cpu_utilization = 0.85;
        signals.rch_available = true;
        signals.rch_workers_available = 2;
        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        assert!(matches!(decision, GovernorDecision::Offload { .. }));
    }

    #[test]
    fn throttle_heavy_under_elevated_pressure_no_rch() {
        let mut gov = CapacityGovernor::with_defaults();
        let mut signals = default_signals();
        signals.cpu_utilization = 0.85;
        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        assert!(matches!(decision, GovernorDecision::Throttle { .. }));
    }

    #[test]
    fn operator_override_bypasses_block() {
        let mut gov = CapacityGovernor::with_defaults();
        gov.add_override(OperatorOverride {
            operator: "admin".to_string(),
            category: None,
            expires_ms: 0,
            reason: "emergency deploy".to_string(),
        });
        let mut signals = default_signals();
        signals.cpu_utilization = 0.99;
        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        assert!(matches!(decision, GovernorDecision::Override { .. }));
        assert_eq!(gov.telemetry().overrides, 1);
        if let GovernorDecision::Override {
            original_decision, ..
        } = &decision
        {
            assert!(matches!(**original_decision, GovernorDecision::Block { .. }));
        }
    }

    #[test]
    fn expired_override_does_not_apply() {
        let mut gov = CapacityGovernor::with_defaults();
        gov.add_override(OperatorOverride {
            operator: "admin".to_string(),
            category: None,
            expires_ms: 500,
            reason: "temporary".to_string(),
        });
        let mut signals = default_signals();
        signals.cpu_utilization = 0.99;
        signals.timestamp_ms = 1000;
        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        assert!(matches!(decision, GovernorDecision::Block { .. }));
    }

    #[test]
    fn category_filtered_override() {
        let mut gov = CapacityGovernor::with_defaults();
        gov.add_override(OperatorOverride {
            operator: "admin".to_string(),
            category: Some(WorkloadCategory::Heavy),
            expires_ms: 0,
            reason: "allow heavy only".to_string(),
        });
        let mut signals = default_signals();
        signals.cpu_utilization = 0.99;
        // Heavy gets override
        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        assert!(matches!(decision, GovernorDecision::Override { .. }));
        // Light does NOT get override
        let decision = gov.evaluate(WorkloadCategory::Light, &signals);
        assert!(matches!(decision, GovernorDecision::Block { .. }));
    }

    #[test]
    fn remove_overrides_by_operator() {
        let mut gov = CapacityGovernor::with_defaults();
        gov.add_override(OperatorOverride {
            operator: "admin".to_string(),
            category: None,
            expires_ms: 0,
            reason: "test".to_string(),
        });
        gov.remove_overrides("admin");
        let mut signals = default_signals();
        signals.cpu_utilization = 0.99;
        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        assert!(matches!(decision, GovernorDecision::Block { .. }));
    }

    #[test]
    fn decision_log_records_entries() {
        let mut gov = CapacityGovernor::with_defaults();
        let signals = default_signals();
        gov.evaluate(WorkloadCategory::Heavy, &signals);
        gov.evaluate(WorkloadCategory::Light, &signals);
        assert_eq!(gov.decision_log().len(), 2);
        assert_eq!(gov.decision_log()[0].category, WorkloadCategory::Heavy);
        assert_eq!(gov.decision_log()[1].category, WorkloadCategory::Light);
    }

    #[test]
    fn decision_log_truncates_at_max() {
        let config = CapacityGovernorConfig::default();
        let mut gov = CapacityGovernor::new(config);
        gov.max_log_entries = 3;
        let signals = default_signals();
        for _ in 0..5 {
            gov.evaluate(WorkloadCategory::Light, &signals);
        }
        assert_eq!(gov.decision_log().len(), 3);
        assert_eq!(gov.telemetry().evaluations, 5);
    }

    #[test]
    fn pressure_signals_health_tier() {
        let mut signals = PressureSignals::default();
        assert_eq!(signals.health_tier(), HealthTier::Green);

        signals.cpu_utilization = 0.6;
        assert_eq!(signals.health_tier(), HealthTier::Yellow);

        signals.cpu_utilization = 0.9;
        assert_eq!(signals.health_tier(), HealthTier::Red);

        signals.memory_utilization = 0.96;
        assert_eq!(signals.health_tier(), HealthTier::Black);
    }

    #[test]
    fn workload_category_weights() {
        assert!(WorkloadCategory::Heavy.weight() > WorkloadCategory::Medium.weight());
        assert!(WorkloadCategory::Medium.weight() > WorkloadCategory::Light.weight());
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = CapacityGovernorConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let restored: CapacityGovernorConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, restored);
    }

    #[test]
    fn governor_telemetry_serde_roundtrip() {
        let mut telem = GovernorTelemetry::default();
        telem.evaluations = 10;
        telem.allowed = 5;
        telem.throttled = 2;
        telem.offloaded = 1;
        telem.blocked = 1;
        telem.overrides = 1;
        let json = serde_json::to_string(&telem).unwrap();
        let restored: GovernorTelemetry = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.evaluations, 10);
        assert_eq!(restored.offloaded, 1);
    }

    #[test]
    fn decision_reason_extraction() {
        let d = GovernorDecision::Allow {
            reason: "ok".to_string(),
        };
        assert_eq!(d.reason(), "ok");
        let d = GovernorDecision::Block {
            reason: "full".to_string(),
        };
        assert_eq!(d.reason(), "full");
        let d = GovernorDecision::Override {
            operator: "admin".to_string(),
            original_decision: Box::new(GovernorDecision::Block {
                reason: "full".to_string(),
            }),
        };
        assert_eq!(d.reason(), "operator override");
    }
}
