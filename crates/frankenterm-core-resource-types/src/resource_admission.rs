//! Resource-aware admission and placement planning for high-core swarm hosts.
//!
//! This module is intentionally leaf-clean: it models CPU lanes, memory, IO,
//! capture/indexing backlog, event fanout, and policy/workflow pressure without
//! depending on `frankenterm-core` runtime state. Core, Robot, and operator
//! surfaces can consume the machine-readable plan without parsing log text.

use serde::{Deserialize, Serialize};

use crate::backpressure::BackpressureTier;

/// Admission action for one proposed swarm workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePlacementAction {
    /// Admit with normal capture/indexing behavior.
    Admit,
    /// Delay until pressure drops or more capacity is available.
    Delay,
    /// Admit, but place the workload in a reduced-fidelity capture tier.
    DegradeCaptureTier,
    /// Require explicit operator approval before admission.
    RequireApproval,
    /// Reject the workload for this planning round.
    Shed,
}

impl ResourcePlacementAction {
    /// Numeric severity for deterministic max-action folding.
    #[must_use]
    pub const fn severity(self) -> u8 {
        match self {
            Self::Admit => 0,
            Self::Delay => 1,
            Self::DegradeCaptureTier => 2,
            Self::RequireApproval => 3,
            Self::Shed => 4,
        }
    }
}

/// Stable reason codes explaining admission and placement decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePlacementReasonCode {
    /// Host pressure is below every configured threshold.
    Healthy,
    /// The host has no usable CPU or memory capacity telemetry.
    MissingHostCapacity,
    /// A workload request has no usable CPU or memory requirement.
    MissingRequestCapacity,
    /// Numeric telemetry was non-finite and was treated as unsafe.
    NonFiniteTelemetry,
    /// CPU lane utilization would exceed the admission limit.
    CpuLaneSaturated,
    /// Memory reservation would exceed the admission limit.
    MemoryTierExhausted,
    /// Storage IO utilization is elevated.
    StorageIoPressure,
    /// Capture backlog is elevated.
    CaptureBacklogPressure,
    /// Search/indexing backlog is elevated.
    IndexingSaturation,
    /// Event fanout is elevated.
    EventFanoutSaturation,
    /// Policy queue pressure requires approval before proceeding.
    PolicyApprovalRequired,
    /// Workflow queue pressure is elevated.
    WorkflowQueuePressure,
    /// Discrete backpressure tier is elevated.
    BackpressureTierElevated,
    /// Mission or work priority prevented a shed decision.
    PriorityProtected,
}

/// Capture tier assigned to an admitted workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturePlacementTier {
    /// Full-fidelity hot capture and indexing.
    Hot,
    /// Reduced hot residency with compressed warm capture.
    WarmCompressed,
    /// Defer nonessential capture/indexing work.
    Deferred,
}

/// Point-in-time host resource snapshot used by the planner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HighCoreHostResourceSnapshot {
    /// Logical CPU lanes available to the swarm.
    pub logical_cpu_count: u32,
    /// CPU lanes already reserved by admitted or running work.
    pub cpu_lanes_in_use: u32,
    /// Total memory budget visible to the planner.
    pub total_memory_bytes: u64,
    /// Memory already reserved by admitted or running work.
    pub reserved_memory_bytes: u64,
    /// Storage IO utilization in [0, 1].
    pub storage_io_utilization: f64,
    /// Capture backlog ratio in [0, 1].
    pub capture_backlog_ratio: f64,
    /// Search/indexing backlog ratio in [0, 1].
    pub indexing_backlog_ratio: f64,
    /// Event fanout utilization in [0, 1].
    pub event_fanout_utilization: f64,
    /// Policy queue utilization in [0, 1].
    pub policy_queue_utilization: f64,
    /// Workflow queue utilization in [0, 1].
    pub workflow_queue_utilization: f64,
    /// Existing discrete backpressure tier.
    pub backpressure_tier: BackpressureTier,
}

impl HighCoreHostResourceSnapshot {
    /// Baseline healthy host for tests and dry-run planning.
    #[must_use]
    pub const fn healthy(logical_cpu_count: u32, total_memory_bytes: u64) -> Self {
        Self {
            logical_cpu_count,
            cpu_lanes_in_use: 0,
            total_memory_bytes,
            reserved_memory_bytes: 0,
            storage_io_utilization: 0.0,
            capture_backlog_ratio: 0.0,
            indexing_backlog_ratio: 0.0,
            event_fanout_utilization: 0.0,
            policy_queue_utilization: 0.0,
            workflow_queue_utilization: 0.0,
            backpressure_tier: BackpressureTier::Green,
        }
    }
}

/// Proposed workload to admit and place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmWorkloadRequest {
    /// Stable workload identifier.
    pub stable_id: String,
    /// CPU lanes requested by this workload.
    pub requested_cpu_lanes: u32,
    /// Memory requested by this workload.
    pub requested_memory_bytes: u64,
    /// Work priority; lower numbers are more important.
    pub work_priority: u8,
    /// Whether this request protects mission-critical work.
    pub mission_critical: bool,
    /// Whether this workload can run in a reduced capture/indexing tier.
    pub can_degrade_capture: bool,
    /// Whether policy requires human approval before admission.
    pub requires_policy_approval: bool,
}

impl SwarmWorkloadRequest {
    /// Construct a request with safe defaults.
    #[must_use]
    pub fn new(
        stable_id: impl Into<String>,
        requested_cpu_lanes: u32,
        requested_memory_bytes: u64,
    ) -> Self {
        Self {
            stable_id: stable_id.into(),
            requested_cpu_lanes,
            requested_memory_bytes,
            work_priority: 5,
            mission_critical: false,
            can_degrade_capture: true,
            requires_policy_approval: false,
        }
    }

    /// Mark this request as high-priority mission work.
    #[must_use]
    pub const fn mission_critical(mut self) -> Self {
        self.mission_critical = true;
        self.work_priority = 0;
        self
    }

    /// Mark this request as requiring operator approval.
    #[must_use]
    pub const fn requires_approval(mut self) -> Self {
        self.requires_policy_approval = true;
        self
    }

    /// Mark this request as unable to degrade capture fidelity.
    #[must_use]
    pub const fn strict_capture(mut self) -> Self {
        self.can_degrade_capture = false;
        self
    }
}

/// Concrete placement target for an admitted request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePlacementTarget {
    /// First CPU lane reserved for the workload.
    pub cpu_lane_start: u32,
    /// Number of CPU lanes reserved.
    pub cpu_lane_count: u32,
    /// Memory bytes reserved.
    pub memory_reserved_bytes: u64,
    /// Capture fidelity tier assigned to the workload.
    pub capture_tier: CapturePlacementTier,
}

/// Per-plan action counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePlacementCounters {
    /// Requests admitted normally.
    pub admitted: u64,
    /// Requests delayed.
    pub delayed: u64,
    /// Requests admitted with degraded capture.
    pub degraded: u64,
    /// Requests requiring approval.
    pub approval_required: u64,
    /// Requests shed.
    pub shed: u64,
}

impl ResourcePlacementCounters {
    fn record(&mut self, action: ResourcePlacementAction) {
        match action {
            ResourcePlacementAction::Admit => self.admitted += 1,
            ResourcePlacementAction::Delay => self.delayed += 1,
            ResourcePlacementAction::DegradeCaptureTier => self.degraded += 1,
            ResourcePlacementAction::RequireApproval => self.approval_required += 1,
            ResourcePlacementAction::Shed => self.shed += 1,
        }
    }
}

/// Decision for one workload request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourcePlacementDecision {
    /// Stable workload identifier.
    pub request_id: String,
    /// Rank in deterministic planning order.
    pub rank: usize,
    /// Final action.
    pub action: ResourcePlacementAction,
    /// Stable reason codes.
    pub reason_codes: Vec<ResourcePlacementReasonCode>,
    /// Target placement for admitted or degraded workloads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<ResourcePlacementTarget>,
    /// Predicted CPU utilization after this request is considered.
    pub predicted_cpu_utilization: Option<f64>,
    /// Predicted memory utilization after this request is considered.
    pub predicted_memory_utilization: Option<f64>,
    /// Operator-facing summary, suitable for Robot/MCP output.
    pub operator_summary: String,
}

/// Aggregate placement plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourcePlacementPlan {
    /// Planner policy version.
    pub planner_version: u32,
    /// Host snapshot used for planning.
    pub host: HighCoreHostResourceSnapshot,
    /// Decisions in deterministic planning order.
    pub decisions: Vec<ResourcePlacementDecision>,
    /// Aggregate action counters.
    pub counters: ResourcePlacementCounters,
}

impl ResourcePlacementPlan {
    /// Find a decision by request id.
    #[must_use]
    pub fn decision_for(&self, request_id: &str) -> Option<&ResourcePlacementDecision> {
        self.decisions
            .iter()
            .find(|decision| decision.request_id == request_id)
    }
}

/// Threshold policy for [`ResourceAwarePlacementPlanner`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourcePlacementPlannerConfig {
    /// Maximum planned CPU utilization before delay.
    pub max_cpu_utilization: f64,
    /// Maximum planned memory utilization before degradation or shed.
    pub max_memory_utilization: f64,
    /// Ratio where IO pressure starts delaying new work.
    pub delay_storage_io_utilization: f64,
    /// Ratio where capture backlog starts degraded capture placement.
    pub degrade_capture_backlog_ratio: f64,
    /// Ratio where indexing backlog starts degraded capture placement.
    pub degrade_indexing_backlog_ratio: f64,
    /// Ratio where event fanout starts delaying or degrading work.
    pub delay_event_fanout_utilization: f64,
    /// Ratio where workflow queue pressure starts delaying work.
    pub delay_workflow_queue_utilization: f64,
    /// Ratio where policy queue pressure requires approval.
    pub approval_policy_queue_utilization: f64,
}

impl Default for ResourcePlacementPlannerConfig {
    fn default() -> Self {
        Self {
            max_cpu_utilization: 0.90,
            max_memory_utilization: 0.88,
            delay_storage_io_utilization: 0.80,
            degrade_capture_backlog_ratio: 0.75,
            degrade_indexing_backlog_ratio: 0.75,
            delay_event_fanout_utilization: 0.70,
            delay_workflow_queue_utilization: 0.70,
            approval_policy_queue_utilization: 0.65,
        }
    }
}

impl ResourcePlacementPlannerConfig {
    /// Validate finite ratio thresholds.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("max_cpu_utilization", self.max_cpu_utilization),
            ("max_memory_utilization", self.max_memory_utilization),
            (
                "delay_storage_io_utilization",
                self.delay_storage_io_utilization,
            ),
            (
                "degrade_capture_backlog_ratio",
                self.degrade_capture_backlog_ratio,
            ),
            (
                "degrade_indexing_backlog_ratio",
                self.degrade_indexing_backlog_ratio,
            ),
            (
                "delay_event_fanout_utilization",
                self.delay_event_fanout_utilization,
            ),
            (
                "delay_workflow_queue_utilization",
                self.delay_workflow_queue_utilization,
            ),
            (
                "approval_policy_queue_utilization",
                self.approval_policy_queue_utilization,
            ),
        ] {
            if !value.is_finite() {
                return Err(format!(
                    "resource placement config {name} must be finite, got {value}"
                ));
            }
            if !(0.0..=1.0).contains(&value) {
                return Err(format!(
                    "resource placement config {name} must be in [0, 1], got {value}"
                ));
            }
        }
        Ok(())
    }
}

/// Deterministic resource admission and placement planner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceAwarePlacementPlanner {
    config: ResourcePlacementPlannerConfig,
}

impl ResourceAwarePlacementPlanner {
    /// Create a planner from explicit config.
    #[must_use]
    pub const fn new(config: ResourcePlacementPlannerConfig) -> Self {
        Self { config }
    }

    /// Read-only access to the planner config.
    #[must_use]
    pub const fn config(&self) -> &ResourcePlacementPlannerConfig {
        &self.config
    }

    /// Plan admission and placement for a batch of proposed workloads.
    #[must_use]
    pub fn plan(
        &self,
        host: &HighCoreHostResourceSnapshot,
        requests: &[SwarmWorkloadRequest],
    ) -> ResourcePlacementPlan {
        let mut ordered = requests.to_vec();
        ordered.sort_by(|left, right| {
            left.work_priority
                .cmp(&right.work_priority)
                .then_with(|| right.mission_critical.cmp(&left.mission_critical))
                .then_with(|| left.stable_id.cmp(&right.stable_id))
        });

        let mut cpu_lanes_used = host.cpu_lanes_in_use;
        let mut memory_reserved = host.reserved_memory_bytes;
        let mut counters = ResourcePlacementCounters::default();
        let mut decisions = Vec::with_capacity(ordered.len());

        for (rank, request) in ordered.iter().enumerate() {
            let decision = self.evaluate_one(host, request, rank + 1, cpu_lanes_used, memory_reserved);
            if matches!(
                decision.action,
                ResourcePlacementAction::Admit | ResourcePlacementAction::DegradeCaptureTier
            ) {
                cpu_lanes_used = cpu_lanes_used.saturating_add(request.requested_cpu_lanes);
                memory_reserved = memory_reserved.saturating_add(request.requested_memory_bytes);
            }
            counters.record(decision.action);
            decisions.push(decision);
        }

        ResourcePlacementPlan {
            planner_version: 1,
            host: host.clone(),
            decisions,
            counters,
        }
    }

    fn evaluate_one(
        &self,
        host: &HighCoreHostResourceSnapshot,
        request: &SwarmWorkloadRequest,
        rank: usize,
        cpu_lanes_used: u32,
        memory_reserved: u64,
    ) -> ResourcePlacementDecision {
        let mut reasons = Vec::new();
        let mut action = ResourcePlacementAction::Admit;

        if host.logical_cpu_count == 0 || host.total_memory_bytes == 0 {
            push_reason(&mut reasons, ResourcePlacementReasonCode::MissingHostCapacity);
            action = action.max(ResourcePlacementAction::Shed);
        }
        if request.requested_cpu_lanes == 0 || request.requested_memory_bytes == 0 {
            push_reason(
                &mut reasons,
                ResourcePlacementReasonCode::MissingRequestCapacity,
            );
            action = action.max(ResourcePlacementAction::Shed);
        }

        let predicted_cpu_utilization =
            utilization(cpu_lanes_used.saturating_add(request.requested_cpu_lanes), host.logical_cpu_count);
        let predicted_memory_utilization = utilization_u64(
            memory_reserved.saturating_add(request.requested_memory_bytes),
            host.total_memory_bytes,
        );

        if predicted_cpu_utilization
            .is_some_and(|utilization| utilization > self.config.max_cpu_utilization)
        {
            push_reason(&mut reasons, ResourcePlacementReasonCode::CpuLaneSaturated);
            action = action.max(ResourcePlacementAction::Delay);
        }

        if predicted_memory_utilization
            .is_some_and(|utilization| utilization > self.config.max_memory_utilization)
        {
            push_reason(
                &mut reasons,
                ResourcePlacementReasonCode::MemoryTierExhausted,
            );
            action = action.max(if request.can_degrade_capture {
                ResourcePlacementAction::DegradeCaptureTier
            } else {
                ResourcePlacementAction::Shed
            });
        }

        action = self.apply_pressure_ratio(
            action,
            &mut reasons,
            host.storage_io_utilization,
            self.config.delay_storage_io_utilization,
            ResourcePlacementReasonCode::StorageIoPressure,
            ResourcePlacementAction::Delay,
        );
        action = self.apply_pressure_ratio(
            action,
            &mut reasons,
            host.capture_backlog_ratio,
            self.config.degrade_capture_backlog_ratio,
            ResourcePlacementReasonCode::CaptureBacklogPressure,
            degraded_or_delayed(request),
        );
        action = self.apply_pressure_ratio(
            action,
            &mut reasons,
            host.indexing_backlog_ratio,
            self.config.degrade_indexing_backlog_ratio,
            ResourcePlacementReasonCode::IndexingSaturation,
            degraded_or_delayed(request),
        );
        action = self.apply_pressure_ratio(
            action,
            &mut reasons,
            host.event_fanout_utilization,
            self.config.delay_event_fanout_utilization,
            ResourcePlacementReasonCode::EventFanoutSaturation,
            degraded_or_delayed(request),
        );
        action = self.apply_pressure_ratio(
            action,
            &mut reasons,
            host.workflow_queue_utilization,
            self.config.delay_workflow_queue_utilization,
            ResourcePlacementReasonCode::WorkflowQueuePressure,
            ResourcePlacementAction::Delay,
        );
        action = self.apply_pressure_ratio(
            action,
            &mut reasons,
            host.policy_queue_utilization,
            self.config.approval_policy_queue_utilization,
            ResourcePlacementReasonCode::PolicyApprovalRequired,
            ResourcePlacementAction::RequireApproval,
        );

        match host.backpressure_tier {
            BackpressureTier::Green => {}
            BackpressureTier::Yellow => {
                push_reason(
                    &mut reasons,
                    ResourcePlacementReasonCode::BackpressureTierElevated,
                );
                action = action.max(ResourcePlacementAction::Delay);
            }
            BackpressureTier::Red => {
                push_reason(
                    &mut reasons,
                    ResourcePlacementReasonCode::BackpressureTierElevated,
                );
                action = action.max(degraded_or_delayed(request));
            }
            BackpressureTier::Black => {
                push_reason(
                    &mut reasons,
                    ResourcePlacementReasonCode::BackpressureTierElevated,
                );
                action = action.max(ResourcePlacementAction::Shed);
            }
        }

        if request.requires_policy_approval {
            push_reason(
                &mut reasons,
                ResourcePlacementReasonCode::PolicyApprovalRequired,
            );
            action = action.max(ResourcePlacementAction::RequireApproval);
        }

        if request.mission_critical && action == ResourcePlacementAction::Shed {
            push_reason(&mut reasons, ResourcePlacementReasonCode::PriorityProtected);
            action = ResourcePlacementAction::Delay;
        }

        if reasons.is_empty() {
            push_reason(&mut reasons, ResourcePlacementReasonCode::Healthy);
        }

        let placement = placement_for(action, host, request, cpu_lanes_used);
        let operator_summary = operator_summary(action, &reasons);

        ResourcePlacementDecision {
            request_id: request.stable_id.clone(),
            rank,
            action,
            reason_codes: reasons,
            placement,
            predicted_cpu_utilization,
            predicted_memory_utilization,
            operator_summary,
        }
    }

    fn apply_pressure_ratio(
        &self,
        action: ResourcePlacementAction,
        reasons: &mut Vec<ResourcePlacementReasonCode>,
        observed: f64,
        threshold: f64,
        reason: ResourcePlacementReasonCode,
        pressure_action: ResourcePlacementAction,
    ) -> ResourcePlacementAction {
        if !observed.is_finite() {
            push_reason(reasons, ResourcePlacementReasonCode::NonFiniteTelemetry);
            return action.max(ResourcePlacementAction::Shed);
        }
        if observed >= threshold {
            push_reason(reasons, reason);
            action.max(pressure_action)
        } else {
            action
        }
    }
}

impl Default for ResourceAwarePlacementPlanner {
    fn default() -> Self {
        Self::new(ResourcePlacementPlannerConfig::default())
    }
}

fn utilization(used: u32, total: u32) -> Option<f64> {
    (total > 0).then(|| f64::from(used) / f64::from(total))
}

fn utilization_u64(used: u64, total: u64) -> Option<f64> {
    (total > 0).then(|| used as f64 / total as f64)
}

fn degraded_or_delayed(request: &SwarmWorkloadRequest) -> ResourcePlacementAction {
    if request.can_degrade_capture {
        ResourcePlacementAction::DegradeCaptureTier
    } else {
        ResourcePlacementAction::Delay
    }
}

fn placement_for(
    action: ResourcePlacementAction,
    host: &HighCoreHostResourceSnapshot,
    request: &SwarmWorkloadRequest,
    cpu_lanes_used: u32,
) -> Option<ResourcePlacementTarget> {
    let capture_tier = match action {
        ResourcePlacementAction::Admit => CapturePlacementTier::Hot,
        ResourcePlacementAction::DegradeCaptureTier => {
            if host.backpressure_tier >= BackpressureTier::Red || host.capture_backlog_ratio >= 0.95 {
                CapturePlacementTier::Deferred
            } else {
                CapturePlacementTier::WarmCompressed
            }
        }
        ResourcePlacementAction::Delay
        | ResourcePlacementAction::RequireApproval
        | ResourcePlacementAction::Shed => return None,
    };

    Some(ResourcePlacementTarget {
        cpu_lane_start: cpu_lanes_used,
        cpu_lane_count: request.requested_cpu_lanes,
        memory_reserved_bytes: request.requested_memory_bytes,
        capture_tier,
    })
}

fn push_reason(
    reasons: &mut Vec<ResourcePlacementReasonCode>,
    reason: ResourcePlacementReasonCode,
) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

fn operator_summary(
    action: ResourcePlacementAction,
    reasons: &[ResourcePlacementReasonCode],
) -> String {
    let reasons = reasons
        .iter()
        .map(|reason| format!("{reason:?}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("action={action:?};reasons={reasons}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1_073_741_824;

    fn host_64c_256gib() -> HighCoreHostResourceSnapshot {
        HighCoreHostResourceSnapshot::healthy(64, 256 * GIB)
    }

    fn request(id: &str, cpu: u32, memory_gib: u64) -> SwarmWorkloadRequest {
        SwarmWorkloadRequest::new(id, cpu, memory_gib * GIB)
    }

    #[test]
    fn healthy_host_admits_requests_in_deterministic_priority_order() {
        let planner = ResourceAwarePlacementPlanner::default();
        let mut medium = request("medium", 4, 8);
        medium.work_priority = 5;
        let critical = request("critical", 8, 16).mission_critical();
        let mut important = request("important", 2, 4);
        important.work_priority = 1;

        let plan = planner.plan(&host_64c_256gib(), &[medium, critical, important]);

        assert_eq!(plan.counters.admitted, 3);
        assert_eq!(plan.decisions[0].request_id, "critical");
        assert_eq!(plan.decisions[1].request_id, "important");
        assert_eq!(plan.decisions[2].request_id, "medium");
        assert_eq!(plan.decisions[0].placement.as_ref().unwrap().cpu_lane_start, 0);
        assert_eq!(plan.decisions[1].placement.as_ref().unwrap().cpu_lane_start, 8);
        assert_eq!(plan.decisions[2].placement.as_ref().unwrap().cpu_lane_start, 10);
    }

    #[test]
    fn cpu_overcommit_delays_lower_priority_work() {
        let planner = ResourceAwarePlacementPlanner::default();
        let mut host = host_64c_256gib();
        host.cpu_lanes_in_use = 56;
        let delayed = request("background", 8, 4);

        let plan = planner.plan(&host, &[delayed]);
        let decision = plan.decision_for("background").unwrap();

        assert_eq!(decision.action, ResourcePlacementAction::Delay);
        assert!(
            decision
                .reason_codes
                .contains(&ResourcePlacementReasonCode::CpuLaneSaturated)
        );
        assert!(decision.placement.is_none());
    }

    #[test]
    fn memory_pressure_degrades_capture_when_supported() {
        let planner = ResourceAwarePlacementPlanner::default();
        let mut host = host_64c_256gib();
        host.reserved_memory_bytes = 220 * GIB;
        let req = request("capture-heavy", 2, 16);

        let plan = planner.plan(&host, &[req]);
        let decision = plan.decision_for("capture-heavy").unwrap();

        assert_eq!(decision.action, ResourcePlacementAction::DegradeCaptureTier);
        assert!(
            decision
                .reason_codes
                .contains(&ResourcePlacementReasonCode::MemoryTierExhausted)
        );
        assert_eq!(
            decision.placement.as_ref().unwrap().capture_tier,
            CapturePlacementTier::WarmCompressed
        );
    }

    #[test]
    fn event_storm_pressure_degrades_degradable_work() {
        let planner = ResourceAwarePlacementPlanner::default();
        let mut host = host_64c_256gib();
        host.event_fanout_utilization = 0.95;

        let plan = planner.plan(&host, &[request("event-storm", 2, 4)]);
        let decision = plan.decision_for("event-storm").unwrap();

        assert_eq!(decision.action, ResourcePlacementAction::DegradeCaptureTier);
        assert!(
            decision
                .reason_codes
                .contains(&ResourcePlacementReasonCode::EventFanoutSaturation)
        );
    }

    #[test]
    fn indexing_saturation_degrades_capture_and_records_summary() {
        let planner = ResourceAwarePlacementPlanner::default();
        let mut host = host_64c_256gib();
        host.indexing_backlog_ratio = 0.80;

        let plan = planner.plan(&host, &[request("indexing", 2, 4)]);
        let decision = plan.decision_for("indexing").unwrap();

        assert_eq!(decision.action, ResourcePlacementAction::DegradeCaptureTier);
        assert!(
            decision
                .reason_codes
                .contains(&ResourcePlacementReasonCode::IndexingSaturation)
        );
        assert!(decision.operator_summary.contains("IndexingSaturation"));
    }

    #[test]
    fn policy_pressure_requires_approval() {
        let planner = ResourceAwarePlacementPlanner::default();
        let mut host = host_64c_256gib();
        host.policy_queue_utilization = 0.90;

        let plan = planner.plan(&host, &[request("approval", 2, 4)]);
        let decision = plan.decision_for("approval").unwrap();

        assert_eq!(decision.action, ResourcePlacementAction::RequireApproval);
        assert_eq!(plan.counters.approval_required, 1);
        assert!(
            decision
                .reason_codes
                .contains(&ResourcePlacementReasonCode::PolicyApprovalRequired)
        );
    }

    #[test]
    fn mission_critical_work_is_delayed_not_shed_under_black_backpressure() {
        let planner = ResourceAwarePlacementPlanner::default();
        let mut host = host_64c_256gib();
        host.backpressure_tier = BackpressureTier::Black;

        let plan = planner.plan(&host, &[request("mission", 2, 4).mission_critical()]);
        let decision = plan.decision_for("mission").unwrap();

        assert_eq!(decision.action, ResourcePlacementAction::Delay);
        assert!(
            decision
                .reason_codes
                .contains(&ResourcePlacementReasonCode::PriorityProtected)
        );
    }

    #[test]
    fn recovery_after_pressure_drops_admits_same_request() {
        let planner = ResourceAwarePlacementPlanner::default();
        let mut pressured = host_64c_256gib();
        pressured.capture_backlog_ratio = 0.90;
        let req = request("recoverable", 2, 4).strict_capture();

        let delayed = planner.plan(&pressured, std::slice::from_ref(&req));
        let recovered = planner.plan(&host_64c_256gib(), &[req]);

        assert_eq!(
            delayed.decision_for("recoverable").unwrap().action,
            ResourcePlacementAction::Delay
        );
        assert_eq!(
            recovered.decision_for("recoverable").unwrap().action,
            ResourcePlacementAction::Admit
        );
    }

    #[test]
    fn nonfinite_telemetry_fails_closed() {
        let planner = ResourceAwarePlacementPlanner::default();
        let mut host = host_64c_256gib();
        host.storage_io_utilization = f64::NAN;

        let plan = planner.plan(&host, &[request("bad-telemetry", 2, 4)]);
        let decision = plan.decision_for("bad-telemetry").unwrap();

        assert_eq!(decision.action, ResourcePlacementAction::Shed);
        assert!(
            decision
                .reason_codes
                .contains(&ResourcePlacementReasonCode::NonFiniteTelemetry)
        );
    }

    #[test]
    fn plan_serde_preserves_machine_readable_reasons() {
        let planner = ResourceAwarePlacementPlanner::default();
        let mut host = host_64c_256gib();
        host.workflow_queue_utilization = 0.75;
        let plan = planner.plan(&host, &[request("workflow", 2, 4)]);

        let json = serde_json::to_string_pretty(&plan).unwrap();
        assert!(json.contains("\"workflow_queue_pressure\""));

        let restored: ResourcePlacementPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, plan);
    }
}
