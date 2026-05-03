use std::fmt;

use serde::{Deserialize, Serialize};

use super::{EnforcerSnapshot, LatencyStage, StageBudget, default_budgets};

// ── A4: Adaptive Budget Allocator ─────────────────────────────────
//
// Redistributes slack from under-budget stages to over-budget stages
// while preserving safety invariants:
// 1. Total budget conservation: sum of lane budgets = constant.
// 2. Bounded adaptation rate: per-epoch change ≤ max_adjustment_pct.
// 3. Minimum floor: no stage drops below min_budget_pct of its default.
// 4. Deterministic replay: same observations + config → same allocations.
//
// AARSP Bead: ft-2p9cb.1.4.1

/// Pressure signal for a single stage — how much headroom or deficit it has.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StagePressure {
    pub stage: LatencyStage,
    /// Observed p95 latency in microseconds (rolling window).
    pub observed_p95_us: f64,
    /// Current budget p95 target in microseconds.
    pub budget_p95_us: f64,
    /// Headroom fraction: (budget - observed) / budget. Negative means over-budget.
    pub headroom: f64,
}

impl StagePressure {
    /// Compute pressure from observation and budget.
    pub fn compute(stage: LatencyStage, observed_p95_us: f64, budget_p95_us: f64) -> Self {
        let headroom = if budget_p95_us > 0.0 {
            (budget_p95_us - observed_p95_us) / budget_p95_us
        } else {
            0.0
        };
        Self {
            stage,
            observed_p95_us,
            budget_p95_us,
            headroom,
        }
    }

    /// Is this stage under pressure (observed > budget)?
    pub fn is_over_budget(&self) -> bool {
        self.headroom < 0.0
    }

    /// How much slack (in us) this stage can donate.
    /// Returns 0.0 if under pressure.
    pub fn donatable_slack_us(&self) -> f64 {
        if self.headroom > 0.0 {
            self.budget_p95_us * self.headroom
        } else {
            0.0
        }
    }

    /// How much additional budget (in us) this stage needs.
    /// Returns 0.0 if within budget.
    pub fn deficit_us(&self) -> f64 {
        if self.headroom < 0.0 {
            self.observed_p95_us - self.budget_p95_us
        } else {
            0.0
        }
    }
}

/// Configuration for the adaptive budget allocator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdaptiveAllocatorConfig {
    /// Maximum fraction of a stage's budget that can be adjusted per epoch (0.0..1.0).
    /// E.g., 0.10 means ±10% per epoch.
    pub max_adjustment_pct: f64,
    /// Minimum fraction of the default budget that any stage can be reduced to.
    /// E.g., 0.50 means no stage goes below 50% of its default.
    pub min_budget_pct: f64,
    /// Maximum fraction above the default budget a stage can grow to.
    /// E.g., 2.0 means up to 200% of default.
    pub max_budget_pct: f64,
    /// Number of observations required before allocator starts adjusting.
    pub warmup_observations: u64,
    /// EWMA decay factor for pressure smoothing (0.0..1.0).
    /// Higher = more weight on recent observations.
    pub pressure_alpha: f64,
    /// Minimum headroom fraction to consider a stage as having donatable slack.
    /// Prevents robbing Peter to pay Paul when both are borderline.
    pub min_donor_headroom: f64,
}

impl Default for AdaptiveAllocatorConfig {
    fn default() -> Self {
        Self {
            max_adjustment_pct: 0.10,
            min_budget_pct: 0.50,
            max_budget_pct: 2.0,
            warmup_observations: 100,
            pressure_alpha: 0.3,
            min_donor_headroom: 0.15,
        }
    }
}

impl AdaptiveAllocatorConfig {
    /// Validate the configuration. Returns errors for invalid settings.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.max_adjustment_pct <= 0.0 || self.max_adjustment_pct > 1.0 {
            errors.push(format!(
                "max_adjustment_pct must be in (0.0, 1.0], got {}",
                self.max_adjustment_pct
            ));
        }
        if self.min_budget_pct <= 0.0 || self.min_budget_pct > 1.0 {
            errors.push(format!(
                "min_budget_pct must be in (0.0, 1.0], got {}",
                self.min_budget_pct
            ));
        }
        if self.max_budget_pct < 1.0 {
            errors.push(format!(
                "max_budget_pct must be >= 1.0, got {}",
                self.max_budget_pct
            ));
        }
        if self.pressure_alpha <= 0.0 || self.pressure_alpha > 1.0 {
            errors.push(format!(
                "pressure_alpha must be in (0.0, 1.0], got {}",
                self.pressure_alpha
            ));
        }
        if self.min_donor_headroom < 0.0 || self.min_donor_headroom >= 1.0 {
            errors.push(format!(
                "min_donor_headroom must be in [0.0, 1.0), got {}",
                self.min_donor_headroom
            ));
        }
        errors
    }
}

/// Per-stage allocation state tracked by the allocator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneAllocation {
    pub stage: LatencyStage,
    /// Default (baseline) budget at p95 in microseconds.
    pub default_p95_us: f64,
    /// Current allocated budget at p95 in microseconds.
    pub current_p95_us: f64,
    /// EWMA-smoothed headroom fraction.
    pub smoothed_headroom: f64,
    /// Cumulative slack donated (positive) or received (negative) in us.
    pub cumulative_transfer_us: f64,
    /// Number of epochs where this stage was over-budget.
    pub over_budget_epochs: u64,
    /// Number of epochs where this stage donated slack.
    pub donor_epochs: u64,
}

impl LaneAllocation {
    fn new(stage: LatencyStage, default_p95_us: f64) -> Self {
        Self {
            stage,
            default_p95_us,
            current_p95_us: default_p95_us,
            smoothed_headroom: 1.0,
            cumulative_transfer_us: 0.0,
            over_budget_epochs: 0,
            donor_epochs: 0,
        }
    }
}

/// A single reallocation decision made by the allocator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocationDecision {
    /// The epoch (observation count) when this decision was made.
    pub epoch: u64,
    /// Correlation ID for replay determinism.
    pub correlation_id: String,
    /// Per-stage adjustments.
    pub adjustments: Vec<StageAdjustment>,
    /// Total slack pool before this allocation.
    pub slack_pool_before_us: f64,
    /// Total slack pool after this allocation.
    pub slack_pool_after_us: f64,
    /// Was the allocator in warmup (no-op)?
    pub warmup: bool,
    /// Reason for the allocation decision.
    pub reason: AllocationReason,
}

/// Individual stage adjustment within an allocation decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageAdjustment {
    pub stage: LatencyStage,
    /// Budget before adjustment.
    pub before_p95_us: f64,
    /// Budget after adjustment.
    pub after_p95_us: f64,
    /// Delta (positive = received slack, negative = donated).
    pub delta_us: f64,
    /// Was this adjustment clamped by rate limit?
    pub rate_clamped: bool,
    /// Was this adjustment clamped by floor/ceiling?
    pub bound_clamped: bool,
}

/// Reason code for an allocation decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllocationReason {
    /// System is in warmup, no adjustments made.
    Warmup,
    /// All stages within budget, no redistribution needed.
    AllWithinBudget,
    /// No donors available (all stages under pressure).
    NoDonors,
    /// Slack redistributed from donors to receivers.
    SlackRedistributed {
        donor_count: usize,
        receiver_count: usize,
    },
    /// Explicit reset to defaults requested.
    ResetToDefaults,
}

impl fmt::Display for AllocationReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Warmup => write!(f, "WARMUP"),
            Self::AllWithinBudget => write!(f, "ALL_WITHIN_BUDGET"),
            Self::NoDonors => write!(f, "NO_DONORS"),
            Self::SlackRedistributed {
                donor_count,
                receiver_count,
            } => write!(
                f,
                "SLACK_REDISTRIBUTED donors={} receivers={}",
                donor_count, receiver_count
            ),
            Self::ResetToDefaults => write!(f, "RESET_TO_DEFAULTS"),
        }
    }
}

/// Snapshot of the allocator state for diagnostics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocatorSnapshot {
    pub epoch: u64,
    pub total_budget_us: f64,
    pub allocated_budget_us: f64,
    pub global_slack_us: f64,
    pub lanes: Vec<LaneAllocation>,
    pub last_decision: Option<AllocationDecision>,
    pub config: AdaptiveAllocatorConfig,
}

/// The adaptive budget allocator.
///
/// Redistributes latency slack from consistently under-budget stages to
/// stages experiencing pressure, using EWMA-smoothed signals and bounded
/// per-epoch adjustment rates.
///
/// # Invariants
///
/// 1. **Conservation**: `Σ lane.current_p95_us` is constant across epochs.
/// 2. **Bounded rate**: Per-epoch change ≤ `max_adjustment_pct * default_budget`.
/// 3. **Floor/ceiling**: `min_budget_pct * default ≤ current ≤ max_budget_pct * default`.
/// 4. **Determinism**: Same observation sequence → same allocation history.
///
/// # Not thread-safe
///
/// Caller provides synchronization if shared across threads.
#[derive(Debug, Clone)]
pub struct AdaptiveAllocator {
    config: AdaptiveAllocatorConfig,
    lanes: Vec<LaneAllocation>,
    total_budget_us: f64,
    epoch: u64,
    decisions: Vec<AllocationDecision>,
    max_decisions: usize,
}

impl AdaptiveAllocator {
    /// Create a new allocator from stage budgets and configuration.
    pub fn new(stage_budgets: &[StageBudget], config: AdaptiveAllocatorConfig) -> Self {
        let lanes: Vec<LaneAllocation> = stage_budgets
            .iter()
            .filter(|b| !b.stage.is_aggregate())
            .map(|b| LaneAllocation::new(b.stage, b.p95_us))
            .collect();
        let total_budget_us: f64 = lanes.iter().map(|l| l.current_p95_us).sum();
        Self {
            config,
            lanes,
            total_budget_us,
            epoch: 0,
            decisions: Vec::new(),
            max_decisions: 1000,
        }
    }

    /// Create an allocator with default budgets and configuration.
    pub fn with_defaults() -> Self {
        Self::new(&default_budgets(), AdaptiveAllocatorConfig::default())
    }

    /// Current epoch count.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Total conserved budget.
    pub fn total_budget_us(&self) -> f64 {
        self.total_budget_us
    }

    /// Current global slack: total_budget - Σ current allocations.
    /// Should be ≈ 0.0 due to conservation, but floating point may drift.
    pub fn global_slack_us(&self) -> f64 {
        let allocated: f64 = self.lanes.iter().map(|l| l.current_p95_us).sum();
        self.total_budget_us - allocated
    }

    /// Get current allocation for a stage.
    pub fn allocation(&self, stage: LatencyStage) -> Option<&LaneAllocation> {
        self.lanes.iter().find(|l| l.stage == stage)
    }

    /// Get all lane allocations.
    pub fn lanes(&self) -> &[LaneAllocation] {
        &self.lanes
    }

    /// Get the last N allocation decisions (most recent first).
    pub fn recent_decisions(&self, n: usize) -> &[AllocationDecision] {
        let start = self.decisions.len().saturating_sub(n);
        &self.decisions[start..]
    }

    /// Process a set of pressure observations and produce an allocation decision.
    ///
    /// This is the core method. Call it once per epoch (e.g., every N observations
    /// or every T seconds).
    ///
    /// # Determinism
    ///
    /// Given the same sequence of `pressures` and `correlation_id`, the allocator
    /// produces identical decisions regardless of wall-clock time.
    pub fn allocate(
        &mut self,
        pressures: &[StagePressure],
        correlation_id: &str,
    ) -> AllocationDecision {
        self.epoch += 1;

        // Update EWMA headroom for each observed stage.
        for pressure in pressures {
            if let Some(lane) = self.lanes.iter_mut().find(|l| l.stage == pressure.stage) {
                lane.smoothed_headroom = lane.smoothed_headroom.mul_add(
                    1.0 - self.config.pressure_alpha,
                    pressure.headroom * self.config.pressure_alpha,
                );
                if pressure.is_over_budget() {
                    lane.over_budget_epochs += 1;
                }
            }
        }

        // During warmup, return no-op decision.
        if self.epoch <= self.config.warmup_observations {
            let decision = AllocationDecision {
                epoch: self.epoch,
                correlation_id: correlation_id.to_string(),
                adjustments: Vec::new(),
                slack_pool_before_us: self.global_slack_us(),
                slack_pool_after_us: self.global_slack_us(),
                warmup: true,
                reason: AllocationReason::Warmup,
            };
            self.push_decision(decision.clone());
            return decision;
        }

        // Classify lanes into donors (excess headroom) and receivers (over-budget).
        let mut donors: Vec<usize> = Vec::new();
        let mut receivers: Vec<usize> = Vec::new();

        for (i, lane) in self.lanes.iter().enumerate() {
            if lane.smoothed_headroom >= self.config.min_donor_headroom {
                donors.push(i);
            } else if lane.smoothed_headroom < 0.0 {
                receivers.push(i);
            }
        }

        let slack_before = self.global_slack_us();

        // No receivers — all within budget.
        if receivers.is_empty() {
            let decision = AllocationDecision {
                epoch: self.epoch,
                correlation_id: correlation_id.to_string(),
                adjustments: Vec::new(),
                slack_pool_before_us: slack_before,
                slack_pool_after_us: slack_before,
                warmup: false,
                reason: AllocationReason::AllWithinBudget,
            };
            self.push_decision(decision.clone());
            return decision;
        }

        // No donors — can't help.
        if donors.is_empty() {
            let decision = AllocationDecision {
                epoch: self.epoch,
                correlation_id: correlation_id.to_string(),
                adjustments: Vec::new(),
                slack_pool_before_us: slack_before,
                slack_pool_after_us: slack_before,
                warmup: false,
                reason: AllocationReason::NoDonors,
            };
            self.push_decision(decision.clone());
            return decision;
        }

        // Compute available slack from donors and total deficit from receivers.
        let mut available_slack = 0.0_f64;
        for &idx in &donors {
            let lane = &self.lanes[idx];
            let max_donate = lane.default_p95_us * self.config.max_adjustment_pct;
            let floor = lane.default_p95_us * self.config.min_budget_pct;
            let actual_donate = max_donate.min(lane.current_p95_us - floor).max(0.0);
            available_slack += actual_donate;
        }

        let mut total_deficit = 0.0_f64;
        for &idx in &receivers {
            let lane = &self.lanes[idx];
            // Deficit = how much more this lane needs.
            let deficit = (-lane.smoothed_headroom) * lane.current_p95_us;
            let max_receive = lane.default_p95_us * self.config.max_adjustment_pct;
            let ceiling = lane.default_p95_us * self.config.max_budget_pct;
            let room = ceiling - lane.current_p95_us;
            total_deficit += deficit.min(max_receive).min(room).max(0.0);
        }

        // Scale: if deficit > available, proportionally reduce.
        let scale = if total_deficit > 0.0 {
            (available_slack / total_deficit).min(1.0)
        } else {
            0.0
        };

        let mut adjustments = Vec::new();

        // Donate from donors.
        let mut donated_total = 0.0_f64;
        for &idx in &donors {
            let lane = &mut self.lanes[idx];
            let max_donate = lane.default_p95_us * self.config.max_adjustment_pct;
            let floor = lane.default_p95_us * self.config.min_budget_pct;
            let actual_donate = max_donate.min(lane.current_p95_us - floor).max(0.0);
            // Scale donation proportionally to how much is needed.
            let donate = if available_slack > 0.0 {
                actual_donate * (total_deficit * scale / available_slack).min(1.0)
            } else {
                0.0
            };
            if donate > 0.0 {
                let before = lane.current_p95_us;
                lane.current_p95_us -= donate;
                let rate_clamped = donate >= max_donate;
                let bound_clamped = lane.current_p95_us <= floor;
                if bound_clamped {
                    lane.current_p95_us = floor;
                }
                let actual_delta = before - lane.current_p95_us;
                lane.cumulative_transfer_us -= actual_delta;
                lane.donor_epochs += 1;
                donated_total += actual_delta;
                adjustments.push(StageAdjustment {
                    stage: lane.stage,
                    before_p95_us: before,
                    after_p95_us: lane.current_p95_us,
                    delta_us: -actual_delta,
                    rate_clamped,
                    bound_clamped,
                });
            }
        }

        // Distribute to receivers proportionally to deficit.
        let mut remaining = donated_total;
        for &idx in &receivers {
            let lane = &mut self.lanes[idx];
            let deficit = (-lane.smoothed_headroom) * lane.current_p95_us;
            let max_receive = lane.default_p95_us * self.config.max_adjustment_pct;
            let ceiling = lane.default_p95_us * self.config.max_budget_pct;
            let room = ceiling - lane.current_p95_us;
            let want = deficit.min(max_receive).min(room).max(0.0);

            let give = if total_deficit > 0.0 {
                (want / total_deficit * donated_total).min(remaining)
            } else {
                0.0
            };

            if give > 0.0 {
                let before = lane.current_p95_us;
                lane.current_p95_us += give;
                let rate_clamped = give >= max_receive;
                let bound_clamped = lane.current_p95_us >= ceiling;
                if bound_clamped {
                    lane.current_p95_us = ceiling;
                }
                let actual_give = lane.current_p95_us - before;
                lane.cumulative_transfer_us += actual_give;
                remaining -= actual_give;
                adjustments.push(StageAdjustment {
                    stage: lane.stage,
                    before_p95_us: before,
                    after_p95_us: lane.current_p95_us,
                    delta_us: actual_give,
                    rate_clamped,
                    bound_clamped,
                });
            }
        }

        let decision = AllocationDecision {
            epoch: self.epoch,
            correlation_id: correlation_id.to_string(),
            adjustments,
            slack_pool_before_us: slack_before,
            slack_pool_after_us: self.global_slack_us(),
            warmup: false,
            reason: AllocationReason::SlackRedistributed {
                donor_count: donors.len(),
                receiver_count: receivers.len(),
            },
        };
        self.push_decision(decision.clone());
        decision
    }

    /// Reset all lane allocations to their defaults.
    pub fn reset(&mut self) -> AllocationDecision {
        self.epoch += 1;
        let mut adjustments = Vec::new();
        for lane in &mut self.lanes {
            if (lane.current_p95_us - lane.default_p95_us).abs() > 1e-6 {
                adjustments.push(StageAdjustment {
                    stage: lane.stage,
                    before_p95_us: lane.current_p95_us,
                    after_p95_us: lane.default_p95_us,
                    delta_us: lane.default_p95_us - lane.current_p95_us,
                    rate_clamped: false,
                    bound_clamped: false,
                });
                lane.current_p95_us = lane.default_p95_us;
                lane.smoothed_headroom = 1.0;
                lane.cumulative_transfer_us = 0.0;
            }
        }
        let decision = AllocationDecision {
            epoch: self.epoch,
            correlation_id: String::new(),
            adjustments,
            slack_pool_before_us: self.global_slack_us(),
            slack_pool_after_us: 0.0,
            warmup: false,
            reason: AllocationReason::ResetToDefaults,
        };
        self.push_decision(decision.clone());
        decision
    }

    /// Get a diagnostic snapshot.
    pub fn snapshot(&self) -> AllocatorSnapshot {
        AllocatorSnapshot {
            epoch: self.epoch,
            total_budget_us: self.total_budget_us,
            allocated_budget_us: self.lanes.iter().map(|l| l.current_p95_us).sum(),
            global_slack_us: self.global_slack_us(),
            lanes: self.lanes.clone(),
            last_decision: self.decisions.last().cloned(),
            config: self.config.clone(),
        }
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        let over_budget: Vec<String> = self
            .lanes
            .iter()
            .filter(|l| l.smoothed_headroom < 0.0)
            .map(|l| format!("{}", l.stage))
            .collect();
        if over_budget.is_empty() {
            format!(
                "allocator=NOMINAL epoch={} slack={:.1}us",
                self.epoch,
                self.global_slack_us()
            )
        } else {
            format!(
                "allocator=REDISTRIBUTING epoch={} pressure=[{}] slack={:.1}us",
                self.epoch,
                over_budget.join(", "),
                self.global_slack_us()
            )
        }
    }

    fn push_decision(&mut self, decision: AllocationDecision) {
        self.decisions.push(decision);
        // Bounded history.
        if self.decisions.len() > self.max_decisions {
            self.decisions.drain(0..self.decisions.len() / 2);
        }
    }

    /// Extract pressure signals from an EnforcerSnapshot.
    ///
    /// This bridges the BudgetEnforcer output into the allocator input,
    /// using observed p95 from the enforcer's percentile estimates.
    pub fn pressures_from_snapshot(snapshot: &EnforcerSnapshot) -> Vec<StagePressure> {
        snapshot
            .stages
            .iter()
            .filter(|ss| !ss.stage.is_aggregate())
            .map(|ss| {
                let observed_p95 = ss.percentiles.p95_us.unwrap_or(0.0);
                StagePressure::compute(ss.stage, observed_p95, ss.budget.p95_us)
            })
            .collect()
    }

    /// Generate updated StageBudgets reflecting current allocations.
    ///
    /// Returns budgets with p95 adjusted to the allocator's current values.
    /// Other percentiles (p50, p99, p999) are scaled proportionally so
    /// the monotonic invariant is preserved.
    pub fn adjusted_budgets(&self) -> Vec<StageBudget> {
        self.lanes
            .iter()
            .map(|lane| {
                let ratio = if lane.default_p95_us > 0.0 {
                    lane.current_p95_us / lane.default_p95_us
                } else {
                    1.0
                };
                // Find the original budget from defaults.
                let defaults = default_budgets();
                let orig = defaults
                    .iter()
                    .find(|b| b.stage == lane.stage)
                    .copied()
                    .unwrap_or(StageBudget {
                        stage: lane.stage,
                        p50_us: lane.default_p95_us * 0.5,
                        p95_us: lane.default_p95_us,
                        p99_us: lane.default_p95_us * 2.0,
                        p999_us: lane.default_p95_us * 5.0,
                    });
                StageBudget {
                    stage: lane.stage,
                    p50_us: orig.p50_us * ratio,
                    p95_us: lane.current_p95_us,
                    p99_us: orig.p99_us * ratio,
                    p999_us: orig.p999_us * ratio,
                }
            })
            .collect()
    }

    /// Check allocator health — detects potential instability.
    pub fn current_degradation(&self) -> AllocatorDegradation {
        // Check for oscillation: if many lanes flip between donor/receiver rapidly.
        let oscillating = self
            .lanes
            .iter()
            .filter(|l| l.donor_epochs > 0 && l.over_budget_epochs > 0)
            .count();

        if oscillating > self.lanes.len() / 2 {
            return AllocatorDegradation::Oscillating {
                lane_count: oscillating,
            };
        }

        // Check conservation drift.
        let drift = self.global_slack_us().abs();
        if drift > 1.0 {
            return AllocatorDegradation::ConservationDrift { drift_us: drift };
        }

        // Check if too many lanes are at their floor.
        let at_floor = self
            .lanes
            .iter()
            .filter(|l| {
                l.default_p95_us
                    .mul_add(-self.config.min_budget_pct, l.current_p95_us)
                    .abs()
                    < 1e-6
            })
            .count();

        if at_floor > self.lanes.len() / 2 {
            return AllocatorDegradation::FloorSaturation {
                lane_count: at_floor,
            };
        }

        AllocatorDegradation::Healthy
    }

    /// Is the allocator in a healthy state?
    pub fn is_healthy(&self) -> bool {
        matches!(self.current_degradation(), AllocatorDegradation::Healthy)
    }

    /// Generate a structured log entry for the most recent allocation decision.
    pub fn last_log_entry(&self) -> Option<AllocationLogEntry> {
        self.decisions.last().map(|d| AllocationLogEntry {
            epoch: d.epoch,
            correlation_id: d.correlation_id.clone(),
            reason: d.reason.to_string(),
            adjustment_count: d.adjustments.len(),
            total_donated_us: d
                .adjustments
                .iter()
                .filter(|a| a.delta_us < 0.0)
                .map(|a| -a.delta_us)
                .sum(),
            total_received_us: d
                .adjustments
                .iter()
                .filter(|a| a.delta_us > 0.0)
                .map(|a| a.delta_us)
                .sum(),
            conservation_error_us: self.global_slack_us(),
            degradation: self.current_degradation(),
        })
    }
}

/// Degradation states for the adaptive allocator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AllocatorDegradation {
    /// All invariants hold, allocator operating normally.
    Healthy,
    /// Multiple lanes oscillating between donor and receiver roles.
    Oscillating { lane_count: usize },
    /// Budget conservation invariant has drifted beyond tolerance.
    ConservationDrift { drift_us: f64 },
    /// Too many lanes pinned at their minimum floor.
    FloorSaturation { lane_count: usize },
}

impl fmt::Display for AllocatorDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::Oscillating { lane_count } => {
                write!(f, "OSCILLATING lanes={}", lane_count)
            }
            Self::ConservationDrift { drift_us } => {
                write!(f, "CONSERVATION_DRIFT drift={:.3}us", drift_us)
            }
            Self::FloorSaturation { lane_count } => {
                write!(f, "FLOOR_SATURATION lanes={}", lane_count)
            }
        }
    }
}

/// Structured log entry for an allocation epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocationLogEntry {
    pub epoch: u64,
    pub correlation_id: String,
    pub reason: String,
    pub adjustment_count: usize,
    pub total_donated_us: f64,
    pub total_received_us: f64,
    pub conservation_error_us: f64,
    pub degradation: AllocatorDegradation,
}
