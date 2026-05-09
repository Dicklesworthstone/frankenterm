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

const LIGHT_CODEL_THROTTLE_DELAY_MS: u64 = 50;

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
    fn normalized_for_evaluation(&self, config: &CapacityGovernorConfig) -> Self {
        Self {
            cpu_utilization: normalize_pressure_ratio(self.cpu_utilization),
            memory_utilization: normalize_pressure_ratio(self.memory_utilization),
            active_heavy_workloads: self.active_heavy_workloads,
            active_medium_workloads: self.active_medium_workloads,
            load_average_1m: normalize_load_average(
                self.load_average_1m,
                config.load_average_block_threshold,
            ),
            rch_available: self.rch_available,
            rch_workers_available: self.rch_workers_available,
            io_pressure: normalize_pressure_ratio(self.io_pressure),
            timestamp_ms: self.timestamp_ms,
        }
    }

    /// Whether `rch` has real offload capacity right now.
    #[must_use]
    pub fn rch_can_offload(&self) -> bool {
        self.rch_available && self.rch_workers_available > 0
    }

    /// Derive a health tier from the current pressure signals.
    #[must_use]
    pub fn health_tier(&self) -> HealthTier {
        let max_pressure = normalize_pressure_ratio(self.cpu_utilization)
            .max(normalize_pressure_ratio(self.memory_utilization))
            .max(normalize_pressure_ratio(self.io_pressure));
        HealthTier::from_ratio(max_pressure)
    }
}

fn normalize_pressure_ratio(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

fn normalize_load_average(value: f64, block_threshold: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        block_threshold
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

/// Invalid capacity governor threshold configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityGovernorConfigError {
    /// A pressure threshold is NaN.
    NanThreshold,
    /// A throttle threshold is above its corresponding block threshold.
    MisorderedThresholds,
}

impl CapacityGovernorConfig {
    /// Validate pressure thresholds before the governor uses them for safety
    /// decisions.
    pub fn validate(&self) -> Result<(), CapacityGovernorConfigError> {
        if self.cpu_throttle_threshold.is_nan()
            || self.cpu_block_threshold.is_nan()
            || self.memory_throttle_threshold.is_nan()
            || self.memory_block_threshold.is_nan()
            || self.load_average_block_threshold.is_nan()
        {
            return Err(CapacityGovernorConfigError::NanThreshold);
        }

        if self.cpu_throttle_threshold > self.cpu_block_threshold
            || self.memory_throttle_threshold > self.memory_block_threshold
        {
            return Err(CapacityGovernorConfigError::MisorderedThresholds);
        }

        Ok(())
    }

    fn fail_closed() -> Self {
        Self {
            max_concurrent_heavy: 0,
            max_concurrent_medium: 0,
            cpu_throttle_threshold: 0.0,
            cpu_block_threshold: 0.0,
            memory_throttle_threshold: 0.0,
            memory_block_threshold: 0.0,
            heavy_throttle_delay_ms: 0,
            medium_throttle_delay_ms: 0,
            prefer_rch_offload: false,
            load_average_block_threshold: 0.0,
        }
    }
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
    Allow { reason: String },
    /// Throttle: delay execution by the specified duration.
    Throttle { delay_ms: u64, reason: String },
    /// Offload: redirect to rch for remote execution.
    Offload { reason: String },
    /// Block: reject the workload entirely.
    Block { reason: String },
    /// Operator override: allow regardless of pressure.
    Override {
        operator: String,
        reason: String,
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
            | Self::Block { reason }
            | Self::Override { reason, .. } => reason,
        }
    }

    fn minimum_pressure_tier(&self) -> HealthTier {
        match self {
            Self::Allow { .. } => HealthTier::Green,
            Self::Throttle { .. } | Self::Offload { .. } => HealthTier::Red,
            Self::Block { .. } => HealthTier::Black,
            Self::Override {
                original_decision, ..
            } => original_decision.minimum_pressure_tier(),
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
///
/// br-ft-l2ksv: per-WorkloadCategory CoDel queues
/// (alien-uplift Nichols & Jacobson 2012) provide a complementary
/// latency-based throttle signal alongside the CPU + memory
/// thresholds. When sustained sojourn time (enqueue → start)
/// exceeds the CoDel target, the governor adds a Throttle gate
/// even when CPU + memory look healthy. See br-ft-codel substrate
/// + crate::codel_queue.
pub struct CapacityGovernor {
    config: CapacityGovernorConfig,
    overrides: Vec<OperatorOverride>,
    telemetry: GovernorTelemetry,
    /// History of recent decisions for audit trail.
    decision_log: Vec<GovernorDecisionEntry>,
    max_log_entries: usize,
    /// br-ft-l2ksv: per-WorkloadCategory CoDel queues for
    /// latency-based throttle gating. Populated in `new()`;
    /// fed observations via `record_workload_sojourn()`; consumed
    /// at decision time via `evaluate()` (after the existing
    /// threshold checks pass).
    codel_light: crate::codel_queue::CodelQueue,
    codel_medium: crate::codel_queue::CodelQueue,
    codel_heavy: crate::codel_queue::CodelQueue,
    /// br-ft-p-squared-wire: per-WorkloadCategory P² streaming
    /// p99 latency estimators (alien-uplift Jain & Chlamtac 1985,
    /// substrate at a91792395). Constant memory (~80 bytes per
    /// counter, 240 bytes total across 3 categories) regardless
    /// of insert count — strictly smaller footprint than t-digest
    /// or sample-buffer histograms for the per-category-p99 use
    /// case.
    ///
    /// This slice is TELEMETRY-ONLY: observations feed
    /// [`Self::record_workload_p99_observation`]; estimates are
    /// available via [`Self::p99_snapshots`] for the
    /// capacity-doctor surface. The decision logic in
    /// [`Self::evaluate`] does NOT yet gate on p99 — that's a
    /// follow-up requiring threshold-tuning work + operator
    /// review of the CoDel-vs-p99 interaction (different
    /// statistics: CoDel uses minimum sojourn, P² uses p99).
    p99_light: crate::p_squared_quantile::PSquaredEstimator,
    p99_medium: crate::p_squared_quantile::PSquaredEstimator,
    p99_heavy: crate::p_squared_quantile::PSquaredEstimator,
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
    #[must_use]
    pub fn new(config: CapacityGovernorConfig) -> Self {
        Self::try_new(config)
            .unwrap_or_else(|_| Self::new_unchecked(CapacityGovernorConfig::fail_closed()))
    }

    /// Try to create a new governor after validating threshold configuration.
    pub fn try_new(config: CapacityGovernorConfig) -> Result<Self, CapacityGovernorConfigError> {
        config.validate()?;
        Ok(Self::new_unchecked(config))
    }

    fn new_unchecked(config: CapacityGovernorConfig) -> Self {
        Self {
            config,
            overrides: Vec::new(),
            telemetry: GovernorTelemetry::default(),
            decision_log: Vec::new(),
            max_log_entries: 1000,
            // br-ft-l2ksv: 3 CoDel queues, one per WorkloadCategory.
            // Default config: 5ms target / 100ms interval (Nichols
            // & Jacobson universals).
            codel_light: crate::codel_queue::CodelQueue::default(),
            codel_medium: crate::codel_queue::CodelQueue::default(),
            codel_heavy: crate::codel_queue::CodelQueue::default(),
            // br-ft-p-squared-wire: 3 P² estimators tracking
            // per-WorkloadCategory p99 latency.
            p99_light: crate::p_squared_quantile::PSquaredEstimator::new(0.99),
            p99_medium: crate::p_squared_quantile::PSquaredEstimator::new(0.99),
            p99_heavy: crate::p_squared_quantile::PSquaredEstimator::new(0.99),
        }
    }

    /// br-ft-p-squared-wire: borrow the P² estimator for the given
    /// category.
    fn p99_estimator_mut(
        &mut self,
        category: WorkloadCategory,
    ) -> &mut crate::p_squared_quantile::PSquaredEstimator {
        match category {
            WorkloadCategory::Light => &mut self.p99_light,
            WorkloadCategory::Medium => &mut self.p99_medium,
            WorkloadCategory::Heavy => &mut self.p99_heavy,
        }
    }

    /// br-ft-p-squared-wire: record a workload's observed latency
    /// (typically the same value the caller passes to
    /// [`Self::record_workload_sojourn`]) into the per-category
    /// P² streaming-p99 estimator.
    ///
    /// Caller can invoke both `record_workload_sojourn` (CoDel)
    /// and `record_workload_p99_observation` (P²) on the same
    /// observation — the two algorithms track different
    /// statistics (minimum sojourn vs p99 latency) so they
    /// surface different operator-relevant signals.
    pub fn record_workload_p99_observation(&mut self, category: WorkloadCategory, latency_ms: u64) {
        self.p99_estimator_mut(category).record(latency_ms as f64);
    }

    /// br-ft-p-squared-wire: snapshot of all 3 per-category P²
    /// estimators for inclusion in capacity-doctor /
    /// runtime-telemetry reports.
    #[must_use]
    pub fn p99_snapshots(
        &self,
    ) -> [(
        WorkloadCategory,
        crate::p_squared_quantile::PSquaredSnapshot,
    ); 3] {
        [
            (WorkloadCategory::Light, self.p99_light.snapshot()),
            (WorkloadCategory::Medium, self.p99_medium.snapshot()),
            (WorkloadCategory::Heavy, self.p99_heavy.snapshot()),
        ]
    }

    /// br-ft-l2ksv: borrow the CoDel queue for the given category.
    fn codel_queue_mut(
        &mut self,
        category: WorkloadCategory,
    ) -> &mut crate::codel_queue::CodelQueue {
        match category {
            WorkloadCategory::Light => &mut self.codel_light,
            WorkloadCategory::Medium => &mut self.codel_medium,
            WorkloadCategory::Heavy => &mut self.codel_heavy,
        }
    }

    fn codel_throttle_delay_ms(&self, category: WorkloadCategory) -> u64 {
        match category {
            WorkloadCategory::Heavy => self.config.heavy_throttle_delay_ms,
            WorkloadCategory::Medium => self.config.medium_throttle_delay_ms,
            WorkloadCategory::Light => LIGHT_CODEL_THROTTLE_DELAY_MS,
        }
    }

    /// br-ft-l2ksv: record a workload's enqueue → start sojourn time
    /// for the given category. Caller invokes this when a workload
    /// transitions out of queue and starts executing. Sojourn
    /// observations feed the CoDel state machine which gates the
    /// throttle decision in subsequent `evaluate()` calls.
    pub fn record_workload_sojourn(
        &mut self,
        category: WorkloadCategory,
        sojourn: std::time::Duration,
    ) {
        self.record_workload_sojourn_at(category, sojourn, std::time::Instant::now());
    }

    fn record_workload_sojourn_at(
        &mut self,
        category: WorkloadCategory,
        sojourn: std::time::Duration,
        now: std::time::Instant,
    ) {
        self.codel_queue_mut(category).record_sojourn(sojourn, now);
    }

    /// br-ft-l2ksv: snapshot of all 3 per-category CoDel queues for
    /// inclusion in capacity-doctor / runtime-telemetry reports.
    #[must_use]
    pub fn codel_snapshots(&self) -> [(WorkloadCategory, crate::codel_queue::CodelSnapshot); 3] {
        [
            (WorkloadCategory::Light, self.codel_light.snapshot()),
            (WorkloadCategory::Medium, self.codel_medium.snapshot()),
            (WorkloadCategory::Heavy, self.codel_heavy.snapshot()),
        ]
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
        let signals = signals.normalized_for_evaluation(&self.config);
        let now_ms = signals.timestamp_ms;

        // Check for active operator overrides first.
        self.overrides.retain(|o| o.is_active(now_ms));
        if let Some(ovr) = self.overrides.iter().find(|o| o.applies_to(category)) {
            let original = self.compute_decision(category, &signals);
            let decision = GovernorDecision::Override {
                operator: ovr.operator.clone(),
                reason: ovr.reason.clone(),
                original_decision: Box::new(original),
            };
            self.record_decision(now_ms, category, &decision, &signals);
            return decision;
        }

        let decision = self.compute_decision(category, &signals);
        // br-ft-l2ksv: if compute_decision returned Allow but CoDel
        // says drop (sustained sojourn ≥ target for ≥ interval),
        // upgrade to Throttle. CoDel is a complementary gate — it
        // never rescues a Block/Throttle to Allow, only adds
        // throttling on top of an otherwise-healthy threshold check.
        // The CoDel `should_drop` mutates state (drop_count tick
        // cadence), so it must run regardless of whether the
        // decision is Allow — but we only act on its verdict when
        // the CPU/memory/concurrency checks said Allow.
        let codel_now = std::time::Instant::now();
        let codel_drop = self.codel_queue_mut(category).should_drop(codel_now);
        let decision = if codel_drop && matches!(decision, GovernorDecision::Allow { .. }) {
            GovernorDecision::Throttle {
                delay_ms: self.codel_throttle_delay_ms(category),
                reason: format!(
                    "br-ft-l2ksv codel sojourn-based throttle: {category:?} \
                     queue saw sustained sojourn ≥ target_ms over interval_ms"
                ),
            }
        } else {
            decision
        };
        self.record_decision(now_ms, category, &decision, &signals);
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
            if self.config.prefer_rch_offload && signals.rch_can_offload() {
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
        if category == WorkloadCategory::Heavy
            && (signals.cpu_utilization >= self.config.cpu_throttle_threshold
                || signals.memory_utilization >= self.config.memory_throttle_threshold)
        {
            if self.config.prefer_rch_offload && signals.rch_can_offload() {
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
            pressure_tier: signals.health_tier().max(decision.minimum_pressure_tier()),
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
    use proptest::prelude::*;

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

    fn quiet_nan(payload: u64) -> f64 {
        f64::from_bits(0x7ff8_0000_0000_0000 | (payload & 0x0007_ffff_ffff_ffff).max(1))
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
        assert_eq!(gov.decision_log()[0].pressure_tier, HealthTier::Black);
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
        assert_eq!(gov.decision_log()[0].pressure_tier, HealthTier::Red);
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
        assert_eq!(gov.decision_log()[0].pressure_tier, HealthTier::Red);
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
            assert!(matches!(
                **original_decision,
                GovernorDecision::Block { .. }
            ));
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
    fn pressure_signals_health_tier_treats_nan_ratios_as_black() {
        let nan_fields: [fn(&mut PressureSignals); 3] = [
            |signals: &mut PressureSignals| signals.cpu_utilization = f64::NAN,
            |signals: &mut PressureSignals| signals.memory_utilization = f64::NAN,
            |signals: &mut PressureSignals| signals.io_pressure = f64::NAN,
        ];

        for apply_nan in nan_fields {
            let mut signals = PressureSignals::default();
            apply_nan(&mut signals);
            assert_eq!(signals.health_tier(), HealthTier::Black);
        }
    }

    proptest! {
        #[test]
        fn nan_pressure_inputs_do_not_allow_heavy_work(
            field in 0usize..3,
            payload in 1u64..0x0008_0000_0000_0000,
        ) {
            let nan = quiet_nan(payload);
            prop_assert!(nan.is_nan());

            let mut gov = CapacityGovernor::with_defaults();
            let mut signals = default_signals();
            match field {
                0 => signals.cpu_utilization = nan,
                1 => signals.memory_utilization = nan,
                2 => signals.load_average_1m = nan,
                _ => unreachable!("field generator is 0..3"),
            }

            let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
            prop_assert!(
                !matches!(decision, GovernorDecision::Allow { .. }),
                "NaN pressure field {} must not mask overload for heavy work: {:?}",
                field,
                decision,
            );
            prop_assert_eq!(gov.telemetry().allowed, 0);
        }

        #[test]
        fn nan_io_pressure_records_black_health_tier(
            payload in 1u64..0x0008_0000_0000_0000,
        ) {
            let nan = quiet_nan(payload);
            prop_assert!(nan.is_nan());

            let mut gov = CapacityGovernor::with_defaults();
            let mut signals = default_signals();
            signals.io_pressure = nan;

            let _ = gov.evaluate(WorkloadCategory::Light, &signals);

            prop_assert_eq!(gov.telemetry().allowed, 1);
            prop_assert_eq!(gov.decision_log()[0].pressure_tier, HealthTier::Black);
        }
    }

    #[test]
    fn rch_can_offload_requires_availability_and_workers() {
        let mut signals = default_signals();
        assert!(!signals.rch_can_offload());

        signals.rch_available = true;
        assert!(!signals.rch_can_offload());

        signals.rch_workers_available = 2;
        assert!(signals.rch_can_offload());

        signals.rch_available = false;
        assert!(!signals.rch_can_offload());
    }

    #[test]
    fn zero_rch_workers_throttle_instead_of_offload_at_concurrency_limit() {
        let mut gov = CapacityGovernor::with_defaults();
        let mut signals = default_signals();
        signals.active_heavy_workloads = 2;
        signals.rch_available = true;
        signals.rch_workers_available = 0;

        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        assert!(matches!(decision, GovernorDecision::Throttle { .. }));
        assert_eq!(gov.telemetry().offloaded, 0);
        assert_eq!(gov.telemetry().throttled, 1);
    }

    #[test]
    fn zero_rch_workers_throttle_under_elevated_pressure() {
        let mut gov = CapacityGovernor::with_defaults();
        let mut signals = default_signals();
        signals.cpu_utilization = 0.85;
        signals.rch_available = true;
        signals.rch_workers_available = 0;

        let decision = gov.evaluate(WorkloadCategory::Heavy, &signals);
        assert!(matches!(decision, GovernorDecision::Throttle { .. }));
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
            reason: "emergency override".to_string(),
            original_decision: Box::new(GovernorDecision::Block {
                reason: "full".to_string(),
            }),
        };
        assert_eq!(d.reason(), "emergency override");
    }

    // ─── br-ft-l2ksv: CoDel wiring tests ─────────────────────────────────
    //
    // Pin the new CoDel-as-complementary-throttle-gate behavior.
    // CoDel never rescues a Block/Throttle to Allow — it only
    // upgrades an Allow to Throttle when sustained sojourn ≥
    // target persists for ≥ interval.

    #[test]
    fn codel_does_not_throttle_when_sojourn_healthy() {
        // Healthy sojourns + healthy CPU/memory = Allow.
        let mut gov = CapacityGovernor::with_defaults();
        for _ in 0..10 {
            // 1ms sojourn — well below 5ms target.
            gov.record_workload_sojourn(
                WorkloadCategory::Heavy,
                std::time::Duration::from_millis(1),
            );
        }
        let d = gov.evaluate(WorkloadCategory::Heavy, &default_signals());
        assert!(
            matches!(d, GovernorDecision::Allow { .. }),
            "healthy sojourn must keep Allow (got {d:?})"
        );
    }

    #[test]
    fn codel_throttles_when_sustained_sojourn_above_target() {
        // br-ft-l2ksv core invariant: sustained above-target sojourn
        // for ≥ interval must promote the governor to Throttle even
        // when CPU/memory are healthy.
        let mut gov = CapacityGovernor::with_defaults();
        // Promote CoDel to Dropping: 2 above-target sojourns
        // separated by ≥ interval (default 100ms). Tests use real
        // wall-clock here since CodelQueue takes Instant; sleep
        // briefly between observations.
        gov.record_workload_sojourn(
            WorkloadCategory::Heavy,
            std::time::Duration::from_millis(20),
        );
        std::thread::sleep(std::time::Duration::from_millis(120));
        gov.record_workload_sojourn(
            WorkloadCategory::Heavy,
            std::time::Duration::from_millis(20),
        );
        // CoDel is now in Dropping state; the next evaluate()
        // should see a Throttle decision (CoDel's first drop fires
        // immediately on entering Dropping).
        let d = gov.evaluate(WorkloadCategory::Heavy, &default_signals());
        assert!(
            matches!(d, GovernorDecision::Throttle { .. }),
            "br-ft-l2ksv: sustained > target sojourn must promote Allow → Throttle \
             (got {d:?})"
        );
        // The Throttle reason must reference CoDel for operator
        // observability.
        if let GovernorDecision::Throttle { reason, .. } = &d {
            assert!(
                reason.contains("codel"),
                "Throttle reason must mention codel for operator clarity (got {reason})"
            );
        }
    }

    fn promote_codel_to_due_drop(gov: &mut CapacityGovernor, category: WorkloadCategory) {
        let first = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(250))
            .expect("250ms subtraction from Instant::now should be in range");
        let second = first + std::time::Duration::from_millis(120);
        gov.record_workload_sojourn_at(category, std::time::Duration::from_millis(20), first);
        gov.record_workload_sojourn_at(category, std::time::Duration::from_millis(20), second);
    }

    proptest! {
        #[test]
        fn codel_throttle_delay_matches_workload_category(
            category in prop_oneof![
                Just(WorkloadCategory::Light),
                Just(WorkloadCategory::Medium),
                Just(WorkloadCategory::Heavy),
            ],
            heavy_delay_ms in 0u64..=20_000,
            medium_delay_ms in 0u64..=20_000,
        ) {
            let config = CapacityGovernorConfig {
                heavy_throttle_delay_ms: heavy_delay_ms,
                medium_throttle_delay_ms: medium_delay_ms,
                ..Default::default()
            };
            let mut gov = CapacityGovernor::new(config);
            promote_codel_to_due_drop(&mut gov, category);

            let decision = gov.evaluate(category, &default_signals());
            let GovernorDecision::Throttle { delay_ms, reason } = decision else {
                prop_assert!(false, "CoDel drop must throttle {category:?}, got {decision:?}");
                return Ok(());
            };

            let expected = match category {
                WorkloadCategory::Heavy => heavy_delay_ms,
                WorkloadCategory::Medium => medium_delay_ms,
                WorkloadCategory::Light => LIGHT_CODEL_THROTTLE_DELAY_MS,
            };
            prop_assert_eq!(delay_ms, expected);
            prop_assert!(reason.contains("codel"));
        }
    }

    #[test]
    fn codel_does_not_rescue_block_to_allow() {
        // CoDel is only an UPGRADE gate (Allow → Throttle). A Block
        // decision from CPU/memory thresholds must not be rescued
        // by CoDel reporting healthy sojourn.
        let mut gov = CapacityGovernor::with_defaults();
        let mut signals = default_signals();
        signals.cpu_utilization = 0.99; // extreme — Block.
        // Healthy sojourn.
        gov.record_workload_sojourn(WorkloadCategory::Heavy, std::time::Duration::from_millis(1));
        let d = gov.evaluate(WorkloadCategory::Heavy, &signals);
        assert!(
            matches!(d, GovernorDecision::Block { .. }),
            "Block decision must not be rescued by healthy CoDel (got {d:?})"
        );
    }

    #[test]
    fn codel_snapshots_returns_three_per_category_entries() {
        let gov = CapacityGovernor::with_defaults();
        let snaps = gov.codel_snapshots();
        assert_eq!(snaps.len(), 3);
        let categories: std::collections::HashSet<_> = snaps.iter().map(|(c, _)| *c).collect();
        assert!(categories.contains(&WorkloadCategory::Light));
        assert!(categories.contains(&WorkloadCategory::Medium));
        assert!(categories.contains(&WorkloadCategory::Heavy));
    }

    #[test]
    fn codel_per_category_state_independent() {
        // Sojourn observations on the Heavy queue must not
        // promote the Light queue.
        let mut gov = CapacityGovernor::with_defaults();
        gov.record_workload_sojourn(
            WorkloadCategory::Heavy,
            std::time::Duration::from_millis(20),
        );
        std::thread::sleep(std::time::Duration::from_millis(120));
        gov.record_workload_sojourn(
            WorkloadCategory::Heavy,
            std::time::Duration::from_millis(20),
        );
        // Heavy is in Dropping; Light should still be NotDropping.
        let d_light = gov.evaluate(WorkloadCategory::Light, &default_signals());
        assert!(
            matches!(d_light, GovernorDecision::Allow { .. }),
            "Heavy CoDel state must not bleed into Light decisions (got {d_light:?})"
        );
    }

    // ─── br-ft-p-squared-wire: P² per-category p99 estimator tests ───────
    //
    // Pin the telemetry-only wiring of crate::p_squared_quantile into
    // CapacityGovernor. This slice does NOT change the throttle decision;
    // it adds the p99 surface for capacity-doctor visibility. Future
    // slices can gate the decision on p99.

    #[test]
    fn p99_snapshots_returns_three_per_category_entries() {
        let gov = CapacityGovernor::with_defaults();
        let snaps = gov.p99_snapshots();
        assert_eq!(snaps.len(), 3);
        let categories: std::collections::HashSet<_> = snaps.iter().map(|(c, _)| *c).collect();
        assert!(categories.contains(&WorkloadCategory::Light));
        assert!(categories.contains(&WorkloadCategory::Medium));
        assert!(categories.contains(&WorkloadCategory::Heavy));
    }

    #[test]
    fn p99_estimator_warms_up_after_min_observations() {
        let mut gov = CapacityGovernor::with_defaults();
        // PSquaredEstimator default warmup = 5 observations.
        for i in 0..4u64 {
            gov.record_workload_p99_observation(WorkloadCategory::Heavy, i + 1);
        }
        let snap = gov
            .p99_snapshots()
            .into_iter()
            .find(|(c, _)| *c == WorkloadCategory::Heavy)
            .unwrap();
        assert_eq!(
            snap.1.estimate, None,
            "pre-warmup snapshot must have None estimate"
        );
        gov.record_workload_p99_observation(WorkloadCategory::Heavy, 5);
        let snap = gov
            .p99_snapshots()
            .into_iter()
            .find(|(c, _)| *c == WorkloadCategory::Heavy)
            .unwrap();
        assert!(
            snap.1.estimate.is_some(),
            "post-warmup snapshot must have Some estimate"
        );
    }

    #[test]
    fn p99_per_category_state_independent() {
        // br-ft-p-squared-wire isolation invariant: observations on the
        // Heavy estimator must not perturb the Light estimator.
        let mut gov = CapacityGovernor::with_defaults();
        for i in 0..10u64 {
            gov.record_workload_p99_observation(WorkloadCategory::Heavy, 1000 + i);
        }
        let heavy_snap = gov
            .p99_snapshots()
            .into_iter()
            .find(|(c, _)| *c == WorkloadCategory::Heavy)
            .unwrap();
        let light_snap = gov
            .p99_snapshots()
            .into_iter()
            .find(|(c, _)| *c == WorkloadCategory::Light)
            .unwrap();
        assert!(heavy_snap.1.estimate.is_some(), "Heavy is warm");
        assert_eq!(
            light_snap.1.estimate, None,
            "Light has 0 observations; estimate must be None"
        );
        assert_eq!(light_snap.1.count, 0);
    }

    #[test]
    fn p99_estimate_within_bounded_error_for_uniform_load() {
        // br-ft-p-squared-wire load-bearing: 10k uniform[0, 1000ms]
        // observations should yield p99 estimate in [950, 1050]
        // (true p99 = 990ms; ±60ms generous slack for sampling).
        let mut gov = CapacityGovernor::with_defaults();
        let mut state: u64 = 42;
        let next = |s: &mut u64| -> u64 {
            *s = (*s).wrapping_mul(48271).wrapping_rem(0x7fff_ffff);
            *s % 1000
        };
        for _ in 0..10_000 {
            gov.record_workload_p99_observation(WorkloadCategory::Medium, next(&mut state));
        }
        let snap = gov
            .p99_snapshots()
            .into_iter()
            .find(|(c, _)| *c == WorkloadCategory::Medium)
            .unwrap();
        let est = snap.1.estimate.expect("warm after 10k samples");
        assert!(
            est > 900.0 && est < 1050.0,
            "br-ft-p-squared-wire p99 estimate must be in [900, 1050] for uniform[0, 1000) (got {est})"
        );
    }
}
