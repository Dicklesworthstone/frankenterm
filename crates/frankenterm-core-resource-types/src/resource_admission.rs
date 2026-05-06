//! Resource-aware admission and placement planning for high-core swarm hosts.
//!
//! This module is intentionally leaf-clean: it models CPU lanes, memory, IO,
//! capture/indexing backlog, event fanout, and policy/workflow pressure without
//! depending on `frankenterm-core` runtime state. Core, Robot, and operator
//! surfaces can consume the machine-readable plan without parsing log text.

use serde::{Deserialize, Serialize};

use crate::backpressure::BackpressureTier;

const RESOURCE_TOPOLOGY_PLANNER_VERSION: u32 = 2;

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
    /// Planner configuration is invalid, so admission fails closed.
    InvalidPlannerConfig,
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
    /// Topology result comes from deterministic simulation, not live hardware measurement.
    TopologySimulated,
    /// NUMA locality or affinity would be crossed.
    NumaLocalityPenalty,
    /// No NUMA node had enough CPU capacity for the requested placement.
    NumaNodeExhausted,
    /// No NUMA node exposed the requested memory tier.
    MemoryTierMismatch,
    /// NUMA-local memory tier capacity was exhausted.
    NumaMemoryTierExhausted,
    /// IO device pressure prevented the requested placement.
    IoDeviceContention,
    /// Worker pool capacity or isolation prevented the requested placement.
    WorkerPoolContention,
    /// Search/indexing work was isolated from capture-worker pressure.
    SearchIndexingIsolation,
    /// Rebalance benefit stayed inside hysteresis, so existing placement was kept.
    RebalanceHysteresis,
    /// Work was split across NUMA nodes because no single node could host it.
    CrossNumaSplit,
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

/// Evidence source for topology-aware placement output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceTopologyEvidenceKind {
    /// Deterministic simulation from a topology snapshot.
    Simulated,
    /// Recorded live hardware evidence.
    Measured,
}

impl ResourceTopologyEvidenceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Simulated => "simulated",
            Self::Measured => "measured",
        }
    }
}

/// Workload classes understood by the topology simulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceTopologyWorkloadKind {
    /// Interactive agent pane runtime work.
    AgentPane,
    /// Capture and delta extraction workers.
    CaptureWorker,
    /// Search and indexing workers.
    SearchIndexing,
    /// Remote or local proof-lane jobs.
    ProofLaneJob,
    /// Workflow fanout and event-handler work.
    WorkflowFanout,
}

/// Memory tiers modeled by the topology simulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceMemoryTier {
    /// Slow spill or cold archive memory.
    Cold,
    /// General-purpose host memory.
    Warm,
    /// Preferred hot DRAM/HBM tier for latency-sensitive work.
    Hot,
}

impl ResourceMemoryTier {
    const fn rank(self) -> u8 {
        match self {
            Self::Cold => 0,
            Self::Warm => 1,
            Self::Hot => 2,
        }
    }

    const fn meets(self, required: Self) -> bool {
        self.rank() >= required.rank()
    }
}

/// Receipt action emitted by topology simulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceTopologyPlanAction {
    /// Keep a prior placement because rebalance benefit is too small.
    Keep,
    /// Place a new workload.
    Place,
    /// Migrate existing work to a better target.
    Migrate,
    /// Split work across more than one NUMA node.
    Split,
    /// Delay work until pressure drops.
    Delay,
    /// Shed work for this planning round.
    Shed,
}

/// NUMA-node snapshot consumed by topology simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceNumaNodeSnapshot {
    /// Stable NUMA node identifier.
    pub numa_node_id: u32,
    /// Physical socket identifier.
    pub socket_id: u32,
    /// First logical CPU lane owned by this NUMA node.
    pub cpu_lane_start: u32,
    /// Logical CPU lanes available on this NUMA node.
    pub cpu_lane_count: u32,
    /// CPU lanes already reserved by running or previously admitted work.
    pub cpu_lanes_in_use: u32,
    /// Memory tier represented by this node's memory budget.
    pub memory_tier: ResourceMemoryTier,
    /// Total memory budget on this NUMA node.
    pub total_memory_bytes: u64,
    /// Memory already reserved on this NUMA node.
    pub reserved_memory_bytes: u64,
}

impl ResourceNumaNodeSnapshot {
    /// Build a healthy node snapshot with no reserved resources.
    #[must_use]
    pub const fn healthy(
        numa_node_id: u32,
        socket_id: u32,
        cpu_lane_start: u32,
        cpu_lane_count: u32,
        memory_tier: ResourceMemoryTier,
        total_memory_bytes: u64,
    ) -> Self {
        Self {
            numa_node_id,
            socket_id,
            cpu_lane_start,
            cpu_lane_count,
            cpu_lanes_in_use: 0,
            memory_tier,
            total_memory_bytes,
            reserved_memory_bytes: 0,
        }
    }
}

/// IO-device snapshot consumed by topology simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceIoDeviceSnapshot {
    /// Stable device identifier.
    pub device_id: String,
    /// NUMA node with closest locality to this device, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attached_numa_node_id: Option<u32>,
    /// Current IO utilization in parts per thousand.
    pub utilization_per_1000: u16,
}

/// Worker-pool snapshot consumed by topology simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceWorkerPoolSnapshot {
    /// Stable pool identifier.
    pub pool_id: String,
    /// Workload kind handled by this pool.
    pub kind: ResourceTopologyWorkloadKind,
    /// NUMA node where this worker pool is pinned, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub numa_node_id: Option<u32>,
    /// Workers currently active in this pool.
    pub active_workers: u32,
    /// Maximum workers allowed in this pool.
    pub max_workers: u32,
}

/// Complete point-in-time topology snapshot for simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTopologySnapshot {
    /// NUMA nodes known to the planner.
    pub numa_nodes: Vec<ResourceNumaNodeSnapshot>,
    /// IO devices known to the planner.
    pub io_devices: Vec<ResourceIoDeviceSnapshot>,
    /// Worker pools known to the planner.
    pub worker_pools: Vec<ResourceWorkerPoolSnapshot>,
}

/// Concrete topology placement segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTopologyPlacementTarget {
    /// NUMA node that owns the reserved CPU and memory segment.
    pub numa_node_id: u32,
    /// Socket that owns the target NUMA node.
    pub socket_id: u32,
    /// First logical CPU lane reserved by this segment.
    pub cpu_lane_start: u32,
    /// Logical CPU lanes reserved by this segment.
    pub cpu_lane_count: u32,
    /// Memory reserved by this segment.
    pub memory_reserved_bytes: u64,
    /// Memory tier used by this segment.
    pub memory_tier: ResourceMemoryTier,
    /// IO device assigned to this segment, when relevant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_device_id: Option<String>,
    /// Worker pool assigned to this segment, when relevant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_pool_id: Option<String>,
}

/// Proposed topology-aware workload placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTopologyWorkloadRequest {
    /// Stable workload identifier.
    pub stable_id: String,
    /// Workload kind.
    pub kind: ResourceTopologyWorkloadKind,
    /// CPU lanes requested by this workload.
    pub requested_cpu_lanes: u32,
    /// Memory requested by this workload.
    pub requested_memory_bytes: u64,
    /// Work priority; lower numbers are more important.
    pub work_priority: u8,
    /// Whether this request protects mission-critical work.
    pub mission_critical: bool,
    /// Whether this workload may be split across NUMA nodes.
    pub can_split: bool,
    /// Preferred NUMA node affinity hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_numa_node_id: Option<u32>,
    /// Preferred IO-device affinity hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_io_device_id: Option<String>,
    /// Minimum memory tier required by this workload.
    pub required_memory_tier: ResourceMemoryTier,
    /// Existing placement, when simulating rebalance.
    pub previous_placements: Vec<ResourceTopologyPlacementTarget>,
}

impl ResourceTopologyWorkloadRequest {
    /// Construct a topology-aware workload request with safe defaults.
    #[must_use]
    pub fn new(
        stable_id: impl Into<String>,
        kind: ResourceTopologyWorkloadKind,
        requested_cpu_lanes: u32,
        requested_memory_bytes: u64,
    ) -> Self {
        Self {
            stable_id: stable_id.into(),
            kind,
            requested_cpu_lanes,
            requested_memory_bytes,
            work_priority: 5,
            mission_critical: false,
            can_split: true,
            preferred_numa_node_id: None,
            preferred_io_device_id: None,
            required_memory_tier: ResourceMemoryTier::Warm,
            previous_placements: Vec::new(),
        }
    }

    /// Mark this request as high-priority mission work.
    #[must_use]
    pub const fn mission_critical(mut self) -> Self {
        self.mission_critical = true;
        self.work_priority = 0;
        self
    }

    /// Disable cross-NUMA splitting for this request.
    #[must_use]
    pub const fn no_split(mut self) -> Self {
        self.can_split = false;
        self
    }

    /// Prefer one NUMA node.
    #[must_use]
    pub const fn prefer_numa_node(mut self, numa_node_id: u32) -> Self {
        self.preferred_numa_node_id = Some(numa_node_id);
        self
    }

    /// Prefer one IO device.
    #[must_use]
    pub fn prefer_io_device(mut self, device_id: impl Into<String>) -> Self {
        self.preferred_io_device_id = Some(device_id.into());
        self
    }

    /// Require a minimum memory tier.
    #[must_use]
    pub const fn require_memory_tier(mut self, memory_tier: ResourceMemoryTier) -> Self {
        self.required_memory_tier = memory_tier;
        self
    }

    /// Attach an existing placement for rebalance simulation.
    #[must_use]
    pub fn with_previous_placement(mut self, placement: ResourceTopologyPlacementTarget) -> Self {
        self.previous_placements = vec![placement];
        self
    }
}

/// Integer cost and risk estimate for a topology simulation receipt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTopologyCostEstimate {
    /// Penalty from crossing NUMA or affinity boundaries.
    pub locality_penalty_per_1000: u16,
    /// IO pressure observed on the selected path.
    pub io_pressure_per_1000: u16,
    /// Worker-pool pressure observed on the selected path.
    pub worker_pool_pressure_per_1000: u16,
    /// Estimated migration cost in deterministic planner units.
    pub migration_cost_units: u32,
    /// Aggregate simulated risk in parts per thousand.
    pub total_risk_per_1000: u16,
}

/// Per-plan topology action counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTopologyPlanCounters {
    /// Existing placements kept.
    pub kept: u64,
    /// New placements selected.
    pub placed: u64,
    /// Existing placements migrated.
    pub migrated: u64,
    /// Workloads split across NUMA nodes.
    pub split: u64,
    /// Workloads delayed.
    pub delayed: u64,
    /// Workloads shed.
    pub shed: u64,
}

impl ResourceTopologyPlanCounters {
    fn record(&mut self, action: ResourceTopologyPlanAction) {
        match action {
            ResourceTopologyPlanAction::Keep => self.kept += 1,
            ResourceTopologyPlanAction::Place => self.placed += 1,
            ResourceTopologyPlanAction::Migrate => self.migrated += 1,
            ResourceTopologyPlanAction::Split => self.split += 1,
            ResourceTopologyPlanAction::Delay => self.delayed += 1,
            ResourceTopologyPlanAction::Shed => self.shed += 1,
        }
    }
}

/// Deterministic topology simulation receipt for one workload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTopologySimulationReceipt {
    /// Stable workload identifier.
    pub request_id: String,
    /// Rank in deterministic planning order.
    pub rank: usize,
    /// Workload kind.
    pub workload_kind: ResourceTopologyWorkloadKind,
    /// Simulated action.
    pub action: ResourceTopologyPlanAction,
    /// Stable reason codes.
    pub reason_codes: Vec<ResourcePlacementReasonCode>,
    /// Evidence source for this receipt.
    pub evidence_kind: ResourceTopologyEvidenceKind,
    /// Existing placement considered by rebalance simulation.
    pub from_placements: Vec<ResourceTopologyPlacementTarget>,
    /// Target placement segments selected by simulation.
    pub to_placements: Vec<ResourceTopologyPlacementTarget>,
    /// Simulated cost estimate.
    pub cost: ResourceTopologyCostEstimate,
    /// Operator-facing summary, suitable for Robot/MCP output.
    pub operator_summary: String,
}

/// Aggregate topology simulation plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTopologySimulationPlan {
    /// Planner policy version.
    pub planner_version: u32,
    /// Evidence source for all receipts in this plan.
    pub evidence_kind: ResourceTopologyEvidenceKind,
    /// Topology snapshot used for planning.
    pub topology: ResourceTopologySnapshot,
    /// Receipts in deterministic planning order.
    pub receipts: Vec<ResourceTopologySimulationReceipt>,
    /// Aggregate action counters.
    pub counters: ResourceTopologyPlanCounters,
    /// Compact operator-facing summary.
    pub operator_summary: String,
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

/// Operator-facing aggregate summary for a placement plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePlacementPlanSummary {
    /// Planner policy version.
    pub planner_version: u32,
    /// Number of requests considered.
    pub total_requests: u64,
    /// Most restrictive action in the plan.
    pub highest_action: ResourcePlacementAction,
    /// Unique reason codes across all decisions, in first-observed order.
    pub reason_codes: Vec<ResourcePlacementReasonCode>,
    /// Aggregate action counters.
    pub counters: ResourcePlacementCounters,
    /// Compact operator-facing summary, suitable for Robot/MCP output.
    pub operator_summary: String,
}

impl ResourcePlacementPlan {
    /// Find a decision by request id.
    #[must_use]
    pub fn decision_for(&self, request_id: &str) -> Option<&ResourcePlacementDecision> {
        self.decisions
            .iter()
            .find(|decision| decision.request_id == request_id)
    }

    /// Summarize the plan without requiring operators to inspect every decision.
    #[must_use]
    pub fn summary(&self) -> ResourcePlacementPlanSummary {
        let mut highest_action = ResourcePlacementAction::Admit;
        let mut reason_codes = Vec::new();

        for decision in &self.decisions {
            highest_action = highest_action.max(decision.action);
            for reason in &decision.reason_codes {
                push_reason(&mut reason_codes, *reason);
            }
        }

        if reason_codes.is_empty() {
            push_reason(&mut reason_codes, ResourcePlacementReasonCode::Healthy);
        }

        let operator_summary = operator_summary(highest_action, &reason_codes);
        ResourcePlacementPlanSummary {
            planner_version: self.planner_version,
            total_requests: saturating_usize_to_u64(self.decisions.len()),
            highest_action,
            reason_codes,
            counters: self.counters,
            operator_summary,
        }
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
    /// Minimum simulated risk reduction required before migrating existing work.
    pub rebalance_hysteresis_min_benefit_per_1000: u16,
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
            rebalance_hysteresis_min_benefit_per_1000: 120,
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
        if let Err(error) = self.config.validate() {
            return invalid_config_plan(host, requests, &error);
        }

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
            let decision =
                self.evaluate_one(host, request, rank + 1, cpu_lanes_used, memory_reserved);
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

    /// Simulate topology-aware placement and rebalance receipts.
    #[must_use]
    pub fn simulate_topology_rebalance(
        &self,
        topology: &ResourceTopologySnapshot,
        requests: &[ResourceTopologyWorkloadRequest],
    ) -> ResourceTopologySimulationPlan {
        let evidence_kind = ResourceTopologyEvidenceKind::Simulated;
        let mut ordered = requests.to_vec();
        ordered.sort_by(|left, right| {
            left.work_priority
                .cmp(&right.work_priority)
                .then_with(|| right.mission_critical.cmp(&left.mission_critical))
                .then_with(|| left.stable_id.cmp(&right.stable_id))
        });

        let mut mutable_topology = topology.clone();
        sort_topology_snapshot(&mut mutable_topology);

        let mut counters = ResourceTopologyPlanCounters::default();
        let mut receipts = Vec::with_capacity(ordered.len());

        for (rank, request) in ordered.iter().enumerate() {
            let receipt = if let Err(error) = self.config.validate() {
                topology_terminal_receipt(
                    request,
                    rank + 1,
                    ResourceTopologyPlanAction::Shed,
                    vec![
                        ResourcePlacementReasonCode::TopologySimulated,
                        ResourcePlacementReasonCode::InvalidPlannerConfig,
                    ],
                    ResourceTopologyCostEstimate {
                        total_risk_per_1000: 1000,
                        ..ResourceTopologyCostEstimate::default()
                    },
                    &error,
                )
            } else {
                self.evaluate_topology_one(&mutable_topology, request, rank + 1)
            };

            apply_topology_receipt(&mut mutable_topology, &receipt);
            counters.record(receipt.action);
            receipts.push(receipt);
        }

        ResourceTopologySimulationPlan {
            planner_version: RESOURCE_TOPOLOGY_PLANNER_VERSION,
            evidence_kind,
            topology: topology.clone(),
            receipts,
            counters,
            operator_summary: topology_plan_operator_summary(evidence_kind, counters),
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
            push_reason(
                &mut reasons,
                ResourcePlacementReasonCode::MissingHostCapacity,
            );
            action = action.max(ResourcePlacementAction::Shed);
        }
        if request.requested_cpu_lanes == 0 || request.requested_memory_bytes == 0 {
            push_reason(
                &mut reasons,
                ResourcePlacementReasonCode::MissingRequestCapacity,
            );
            action = action.max(ResourcePlacementAction::Shed);
        }

        let predicted_cpu_utilization = utilization(
            cpu_lanes_used.saturating_add(request.requested_cpu_lanes),
            host.logical_cpu_count,
        );
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

        action = Self::apply_pressure_ratio(
            action,
            &mut reasons,
            host.storage_io_utilization,
            self.config.delay_storage_io_utilization,
            ResourcePlacementReasonCode::StorageIoPressure,
            ResourcePlacementAction::Delay,
        );
        action = Self::apply_pressure_ratio(
            action,
            &mut reasons,
            host.capture_backlog_ratio,
            self.config.degrade_capture_backlog_ratio,
            ResourcePlacementReasonCode::CaptureBacklogPressure,
            degraded_or_delayed(request),
        );
        action = Self::apply_pressure_ratio(
            action,
            &mut reasons,
            host.indexing_backlog_ratio,
            self.config.degrade_indexing_backlog_ratio,
            ResourcePlacementReasonCode::IndexingSaturation,
            degraded_or_delayed(request),
        );
        action = Self::apply_pressure_ratio(
            action,
            &mut reasons,
            host.event_fanout_utilization,
            self.config.delay_event_fanout_utilization,
            ResourcePlacementReasonCode::EventFanoutSaturation,
            degraded_or_delayed(request),
        );
        action = Self::apply_pressure_ratio(
            action,
            &mut reasons,
            host.workflow_queue_utilization,
            self.config.delay_workflow_queue_utilization,
            ResourcePlacementReasonCode::WorkflowQueuePressure,
            ResourcePlacementAction::Delay,
        );
        action = Self::apply_pressure_ratio(
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

    fn evaluate_topology_one(
        &self,
        topology: &ResourceTopologySnapshot,
        request: &ResourceTopologyWorkloadRequest,
        rank: usize,
    ) -> ResourceTopologySimulationReceipt {
        if request.requested_cpu_lanes == 0 || request.requested_memory_bytes == 0 {
            return topology_terminal_receipt(
                request,
                rank,
                ResourceTopologyPlanAction::Shed,
                vec![
                    ResourcePlacementReasonCode::TopologySimulated,
                    ResourcePlacementReasonCode::MissingRequestCapacity,
                ],
                ResourceTopologyCostEstimate {
                    total_risk_per_1000: 1000,
                    ..ResourceTopologyCostEstimate::default()
                },
                "missing request capacity",
            );
        }

        if topology.numa_nodes.is_empty() {
            return topology_terminal_receipt(
                request,
                rank,
                if request.mission_critical {
                    ResourceTopologyPlanAction::Delay
                } else {
                    ResourceTopologyPlanAction::Shed
                },
                vec![
                    ResourcePlacementReasonCode::TopologySimulated,
                    ResourcePlacementReasonCode::MissingHostCapacity,
                ],
                ResourceTopologyCostEstimate {
                    total_risk_per_1000: 1000,
                    ..ResourceTopologyCostEstimate::default()
                },
                "missing topology capacity",
            );
        }

        let mut capacity_view = topology.clone();
        sort_topology_snapshot(&mut capacity_view);
        release_topology_targets(&mut capacity_view, &request.previous_placements);

        if let Some(candidate) = self.best_topology_candidate(&capacity_view, request) {
            return self.receipt_from_candidate(request, rank, candidate);
        }

        if let Some(split) = self.split_topology_candidate(&capacity_view, request) {
            let operator_summary = topology_operator_summary(
                ResourceTopologyPlanAction::Split,
                ResourceTopologyEvidenceKind::Simulated,
                &split.reason_codes,
                &split.cost,
            );
            return ResourceTopologySimulationReceipt {
                request_id: request.stable_id.clone(),
                rank,
                workload_kind: request.kind,
                action: ResourceTopologyPlanAction::Split,
                reason_codes: split.reason_codes,
                evidence_kind: ResourceTopologyEvidenceKind::Simulated,
                from_placements: request.previous_placements.clone(),
                to_placements: split.targets,
                cost: split.cost,
                operator_summary,
            };
        }

        let reasons = topology_unplaceable_reasons(
            &capacity_view,
            request,
            ratio_threshold_per_1000(self.config.delay_storage_io_utilization),
        );
        let should_delay = request.mission_critical
            || reasons.iter().any(|reason| {
                matches!(
                    reason,
                    ResourcePlacementReasonCode::IoDeviceContention
                        | ResourcePlacementReasonCode::NumaMemoryTierExhausted
                        | ResourcePlacementReasonCode::MemoryTierMismatch
                        | ResourcePlacementReasonCode::WorkerPoolContention
                )
            });
        let action = if should_delay {
            ResourceTopologyPlanAction::Delay
        } else {
            ResourceTopologyPlanAction::Shed
        };

        topology_terminal_receipt(
            request,
            rank,
            action,
            reasons,
            ResourceTopologyCostEstimate {
                total_risk_per_1000: 1000,
                ..ResourceTopologyCostEstimate::default()
            },
            "no simulated topology target available",
        )
    }

    fn receipt_from_candidate(
        &self,
        request: &ResourceTopologyWorkloadRequest,
        rank: usize,
        candidate: TopologyCandidate,
    ) -> ResourceTopologySimulationReceipt {
        let mut action = if request.previous_placements.is_empty() {
            ResourceTopologyPlanAction::Place
        } else if same_topology_placements(
            &request.previous_placements,
            std::slice::from_ref(&candidate.target),
        ) {
            ResourceTopologyPlanAction::Keep
        } else {
            ResourceTopologyPlanAction::Migrate
        };

        let mut reason_codes = candidate.reason_codes;
        let mut to_placements = vec![candidate.target];
        let mut cost = candidate.cost;

        if action == ResourceTopologyPlanAction::Migrate {
            let previous_cost = topology_existing_cost(&request.previous_placements);
            let benefit = previous_cost
                .total_risk_per_1000
                .saturating_sub(cost.total_risk_per_1000);
            if benefit < self.config.rebalance_hysteresis_min_benefit_per_1000 {
                push_reason(
                    &mut reason_codes,
                    ResourcePlacementReasonCode::RebalanceHysteresis,
                );
                action = ResourceTopologyPlanAction::Keep;
                to_placements.clone_from(&request.previous_placements);
                cost = previous_cost;
            } else {
                cost.migration_cost_units = topology_migration_cost_units(request);
            }
        }

        let operator_summary = topology_operator_summary(
            action,
            ResourceTopologyEvidenceKind::Simulated,
            &reason_codes,
            &cost,
        );

        ResourceTopologySimulationReceipt {
            request_id: request.stable_id.clone(),
            rank,
            workload_kind: request.kind,
            action,
            reason_codes,
            evidence_kind: ResourceTopologyEvidenceKind::Simulated,
            from_placements: request.previous_placements.clone(),
            to_placements,
            cost,
            operator_summary,
        }
    }

    fn best_topology_candidate(
        &self,
        topology: &ResourceTopologySnapshot,
        request: &ResourceTopologyWorkloadRequest,
    ) -> Option<TopologyCandidate> {
        let mut candidates = topology
            .numa_nodes
            .iter()
            .filter_map(|node| self.topology_candidate_for_node(topology, request, node))
            .collect::<Vec<_>>();

        candidates.sort_by(|left, right| {
            left.cost
                .total_risk_per_1000
                .cmp(&right.cost.total_risk_per_1000)
                .then_with(|| left.target.numa_node_id.cmp(&right.target.numa_node_id))
                .then_with(|| left.target.cpu_lane_start.cmp(&right.target.cpu_lane_start))
                .then_with(|| left.target.io_device_id.cmp(&right.target.io_device_id))
                .then_with(|| left.target.worker_pool_id.cmp(&right.target.worker_pool_id))
        });
        candidates.into_iter().next()
    }

    fn topology_candidate_for_node(
        &self,
        topology: &ResourceTopologySnapshot,
        request: &ResourceTopologyWorkloadRequest,
        node: &ResourceNumaNodeSnapshot,
    ) -> Option<TopologyCandidate> {
        if !node.memory_tier.meets(request.required_memory_tier) {
            return None;
        }

        let predicted_cpu = utilization(
            node.cpu_lanes_in_use
                .saturating_add(request.requested_cpu_lanes),
            node.cpu_lane_count,
        )?;
        if predicted_cpu > self.config.max_cpu_utilization {
            return None;
        }

        let predicted_memory = utilization_u64(
            node.reserved_memory_bytes
                .saturating_add(request.requested_memory_bytes),
            node.total_memory_bytes,
        )?;
        if predicted_memory > self.config.max_memory_utilization {
            return None;
        }

        let mut reason_codes = vec![ResourcePlacementReasonCode::TopologySimulated];
        let mut locality_penalty_per_1000 = if request
            .preferred_numa_node_id
            .is_some_and(|preferred| preferred != node.numa_node_id)
        {
            push_reason(
                &mut reason_codes,
                ResourcePlacementReasonCode::NumaLocalityPenalty,
            );
            200
        } else {
            0
        };

        let io_device = select_topology_io_device(
            topology,
            request,
            node,
            ratio_threshold_per_1000(self.config.delay_storage_io_utilization),
        )?;
        if io_device
            .device()
            .and_then(|device| device.attached_numa_node_id)
            .is_some_and(|attached| attached != node.numa_node_id)
        {
            push_reason(
                &mut reason_codes,
                ResourcePlacementReasonCode::NumaLocalityPenalty,
            );
            locality_penalty_per_1000 = locality_penalty_per_1000.max(150);
        }

        let worker_pool = select_topology_worker_pool(topology, request, node)?;
        if request.kind == ResourceTopologyWorkloadKind::SearchIndexing
            && worker_pool
                .pool()
                .is_some_and(|pool| pool.numa_node_id == Some(node.numa_node_id))
        {
            push_reason(
                &mut reason_codes,
                ResourcePlacementReasonCode::SearchIndexingIsolation,
            );
        }

        let io_pressure_per_1000 = io_device
            .device()
            .map_or(0, |device| device.utilization_per_1000.min(1000));
        let worker_pool_pressure_per_1000 = worker_pool
            .pool()
            .map_or(0, topology_worker_pool_pressure_per_1000);
        let cpu_pressure_per_1000 = utilization_per_1000_u32(
            node.cpu_lanes_in_use
                .saturating_add(request.requested_cpu_lanes),
            node.cpu_lane_count,
        );
        let memory_pressure_per_1000 = utilization_per_1000_u64(
            node.reserved_memory_bytes
                .saturating_add(request.requested_memory_bytes),
            node.total_memory_bytes,
        );
        let total_risk_per_1000 = topology_total_risk_per_1000(
            cpu_pressure_per_1000,
            memory_pressure_per_1000,
            io_pressure_per_1000,
            worker_pool_pressure_per_1000,
            locality_penalty_per_1000,
        );
        let cost = ResourceTopologyCostEstimate {
            locality_penalty_per_1000,
            io_pressure_per_1000,
            worker_pool_pressure_per_1000,
            migration_cost_units: 0,
            total_risk_per_1000,
        };

        Some(TopologyCandidate {
            target: ResourceTopologyPlacementTarget {
                numa_node_id: node.numa_node_id,
                socket_id: node.socket_id,
                cpu_lane_start: node.cpu_lane_start.saturating_add(node.cpu_lanes_in_use),
                cpu_lane_count: request.requested_cpu_lanes,
                memory_reserved_bytes: request.requested_memory_bytes,
                memory_tier: node.memory_tier,
                io_device_id: io_device.into_device_id(),
                worker_pool_id: worker_pool.into_pool_id(),
            },
            cost,
            reason_codes,
        })
    }

    fn split_topology_candidate(
        &self,
        topology: &ResourceTopologySnapshot,
        request: &ResourceTopologyWorkloadRequest,
    ) -> Option<TopologySplitCandidate> {
        if !request.can_split || request.requested_cpu_lanes <= 1 {
            return None;
        }

        let mut remaining_cpu = request.requested_cpu_lanes;
        let mut remaining_memory = request.requested_memory_bytes;
        let mut targets = Vec::new();
        let mut cpu_pressure_peak = 0;
        let mut memory_pressure_peak = 0;

        for node in &topology.numa_nodes {
            if !node.memory_tier.meets(request.required_memory_tier) {
                continue;
            }
            let available_cpu = topology_available_cpu_lanes(node, self.config.max_cpu_utilization);
            let available_memory =
                topology_available_memory_bytes(node, self.config.max_memory_utilization);
            if available_cpu == 0 || available_memory == 0 {
                continue;
            }

            let cpu_segment = remaining_cpu.min(available_cpu);
            let desired_memory = if cpu_segment == remaining_cpu {
                remaining_memory
            } else {
                div_ceil_u64(
                    request
                        .requested_memory_bytes
                        .saturating_mul(u64::from(cpu_segment)),
                    u64::from(request.requested_cpu_lanes),
                )
            };
            let memory_segment = desired_memory.min(remaining_memory).min(available_memory);
            if cpu_segment == 0 || memory_segment == 0 {
                continue;
            }

            targets.push(ResourceTopologyPlacementTarget {
                numa_node_id: node.numa_node_id,
                socket_id: node.socket_id,
                cpu_lane_start: node.cpu_lane_start.saturating_add(node.cpu_lanes_in_use),
                cpu_lane_count: cpu_segment,
                memory_reserved_bytes: memory_segment,
                memory_tier: node.memory_tier,
                io_device_id: None,
                worker_pool_id: None,
            });

            cpu_pressure_peak = cpu_pressure_peak.max(utilization_per_1000_u32(
                node.cpu_lanes_in_use.saturating_add(cpu_segment),
                node.cpu_lane_count,
            ));
            memory_pressure_peak = memory_pressure_peak.max(utilization_per_1000_u64(
                node.reserved_memory_bytes.saturating_add(memory_segment),
                node.total_memory_bytes,
            ));
            remaining_cpu = remaining_cpu.saturating_sub(cpu_segment);
            remaining_memory = remaining_memory.saturating_sub(memory_segment);
            if remaining_cpu == 0 && remaining_memory == 0 {
                break;
            }
        }

        if remaining_cpu == 0 && remaining_memory == 0 && targets.len() > 1 {
            let cost = ResourceTopologyCostEstimate {
                locality_penalty_per_1000: 250,
                total_risk_per_1000: topology_total_risk_per_1000(
                    cpu_pressure_peak,
                    memory_pressure_peak,
                    0,
                    0,
                    250,
                ),
                ..ResourceTopologyCostEstimate::default()
            };
            Some(TopologySplitCandidate {
                targets,
                cost,
                reason_codes: vec![
                    ResourcePlacementReasonCode::TopologySimulated,
                    ResourcePlacementReasonCode::CrossNumaSplit,
                ],
            })
        } else {
            None
        }
    }

    fn apply_pressure_ratio(
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

#[derive(Debug, Clone)]
struct TopologyCandidate {
    target: ResourceTopologyPlacementTarget,
    cost: ResourceTopologyCostEstimate,
    reason_codes: Vec<ResourcePlacementReasonCode>,
}

#[derive(Debug, Clone)]
struct TopologySplitCandidate {
    targets: Vec<ResourceTopologyPlacementTarget>,
    cost: ResourceTopologyCostEstimate,
    reason_codes: Vec<ResourcePlacementReasonCode>,
}

#[derive(Debug, Clone)]
enum TopologyIoSelection {
    NotRequired,
    Selected(ResourceIoDeviceSnapshot),
}

impl TopologyIoSelection {
    const fn device(&self) -> Option<&ResourceIoDeviceSnapshot> {
        match self {
            Self::NotRequired => None,
            Self::Selected(device) => Some(device),
        }
    }

    fn into_device_id(self) -> Option<String> {
        match self {
            Self::NotRequired => None,
            Self::Selected(device) => Some(device.device_id),
        }
    }
}

#[derive(Debug, Clone)]
enum TopologyWorkerPoolSelection {
    NotRequired,
    Selected(ResourceWorkerPoolSnapshot),
}

impl TopologyWorkerPoolSelection {
    const fn pool(&self) -> Option<&ResourceWorkerPoolSnapshot> {
        match self {
            Self::NotRequired => None,
            Self::Selected(pool) => Some(pool),
        }
    }

    fn into_pool_id(self) -> Option<String> {
        match self {
            Self::NotRequired => None,
            Self::Selected(pool) => Some(pool.pool_id),
        }
    }
}

fn topology_terminal_receipt(
    request: &ResourceTopologyWorkloadRequest,
    rank: usize,
    action: ResourceTopologyPlanAction,
    reason_codes: Vec<ResourcePlacementReasonCode>,
    cost: ResourceTopologyCostEstimate,
    detail: &str,
) -> ResourceTopologySimulationReceipt {
    let mut operator_summary = topology_operator_summary(
        action,
        ResourceTopologyEvidenceKind::Simulated,
        &reason_codes,
        &cost,
    );
    if !detail.is_empty() {
        operator_summary.push_str(";detail=");
        operator_summary.push_str(detail);
    }

    ResourceTopologySimulationReceipt {
        request_id: request.stable_id.clone(),
        rank,
        workload_kind: request.kind,
        action,
        reason_codes,
        evidence_kind: ResourceTopologyEvidenceKind::Simulated,
        from_placements: request.previous_placements.clone(),
        to_placements: Vec::new(),
        cost,
        operator_summary,
    }
}

fn topology_plan_operator_summary(
    evidence_kind: ResourceTopologyEvidenceKind,
    counters: ResourceTopologyPlanCounters,
) -> String {
    format!(
        "source={};kept={};placed={};migrated={};split={};delayed={};shed={}",
        evidence_kind.as_str(),
        counters.kept,
        counters.placed,
        counters.migrated,
        counters.split,
        counters.delayed,
        counters.shed
    )
}

fn topology_operator_summary(
    action: ResourceTopologyPlanAction,
    evidence_kind: ResourceTopologyEvidenceKind,
    reasons: &[ResourcePlacementReasonCode],
    cost: &ResourceTopologyCostEstimate,
) -> String {
    let reasons = reasons
        .iter()
        .map(|reason| format!("{reason:?}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "source={};action={action:?};risk_per_1000={};reasons={reasons}",
        evidence_kind.as_str(),
        cost.total_risk_per_1000
    )
}

fn topology_unplaceable_reasons(
    topology: &ResourceTopologySnapshot,
    request: &ResourceTopologyWorkloadRequest,
    io_pressure_threshold_per_1000: u16,
) -> Vec<ResourcePlacementReasonCode> {
    let mut reasons = vec![ResourcePlacementReasonCode::TopologySimulated];

    if topology.numa_nodes.is_empty() {
        push_reason(
            &mut reasons,
            ResourcePlacementReasonCode::MissingHostCapacity,
        );
        return reasons;
    }

    if !topology
        .numa_nodes
        .iter()
        .any(|node| node.memory_tier.meets(request.required_memory_tier))
    {
        push_reason(
            &mut reasons,
            ResourcePlacementReasonCode::MemoryTierMismatch,
        );
    } else if topology.numa_nodes.iter().all(|node| {
        !node.memory_tier.meets(request.required_memory_tier)
            || node
                .reserved_memory_bytes
                .saturating_add(request.requested_memory_bytes)
                > node.total_memory_bytes
    }) {
        push_reason(
            &mut reasons,
            ResourcePlacementReasonCode::NumaMemoryTierExhausted,
        );
    }

    if topology.numa_nodes.iter().all(|node| {
        node.cpu_lanes_in_use
            .saturating_add(request.requested_cpu_lanes)
            > node.cpu_lane_count
    }) {
        push_reason(&mut reasons, ResourcePlacementReasonCode::NumaNodeExhausted);
    }

    if topology_workload_requires_io(request.kind) {
        let any_io = matching_io_devices(topology, request, None)
            .into_iter()
            .any(|device| device.utilization_per_1000 < io_pressure_threshold_per_1000);
        if !any_io {
            push_reason(
                &mut reasons,
                ResourcePlacementReasonCode::IoDeviceContention,
            );
        }
    }

    if topology_workload_requires_worker_pool(request.kind)
        && !topology
            .worker_pools
            .iter()
            .any(|pool| pool.kind == request.kind && pool.active_workers < pool.max_workers)
    {
        push_reason(
            &mut reasons,
            ResourcePlacementReasonCode::WorkerPoolContention,
        );
    }

    if reasons.len() == 1 {
        push_reason(&mut reasons, ResourcePlacementReasonCode::NumaNodeExhausted);
    }

    reasons
}

fn select_topology_io_device(
    topology: &ResourceTopologySnapshot,
    request: &ResourceTopologyWorkloadRequest,
    node: &ResourceNumaNodeSnapshot,
    pressure_threshold_per_1000: u16,
) -> Option<TopologyIoSelection> {
    if !topology_workload_requires_io(request.kind) && request.preferred_io_device_id.is_none() {
        return Some(TopologyIoSelection::NotRequired);
    }

    let mut devices = matching_io_devices(topology, request, Some(node.numa_node_id));
    devices.sort_by(|left, right| {
        left.utilization_per_1000
            .cmp(&right.utilization_per_1000)
            .then_with(|| left.device_id.cmp(&right.device_id))
    });

    let device = devices.into_iter().next()?;
    (device.utilization_per_1000 < pressure_threshold_per_1000)
        .then_some(TopologyIoSelection::Selected(device))
}

fn matching_io_devices(
    topology: &ResourceTopologySnapshot,
    request: &ResourceTopologyWorkloadRequest,
    node_id: Option<u32>,
) -> Vec<ResourceIoDeviceSnapshot> {
    topology
        .io_devices
        .iter()
        .filter(|device| {
            request
                .preferred_io_device_id
                .as_ref()
                .is_none_or(|preferred| preferred == &device.device_id)
        })
        .filter(|device| {
            node_id.is_none_or(|id| {
                device
                    .attached_numa_node_id
                    .is_none_or(|attached| attached == id)
            })
        })
        .cloned()
        .collect()
}

fn select_topology_worker_pool(
    topology: &ResourceTopologySnapshot,
    request: &ResourceTopologyWorkloadRequest,
    node: &ResourceNumaNodeSnapshot,
) -> Option<TopologyWorkerPoolSelection> {
    if !topology_workload_requires_worker_pool(request.kind) {
        return Some(TopologyWorkerPoolSelection::NotRequired);
    }

    let mut pools = topology
        .worker_pools
        .iter()
        .filter(|pool| pool.kind == request.kind)
        .filter(|pool| pool.active_workers < pool.max_workers)
        .filter(|pool| {
            pool.numa_node_id
                .is_none_or(|pool_node_id| pool_node_id == node.numa_node_id)
        })
        .cloned()
        .collect::<Vec<_>>();

    pools.sort_by(|left, right| {
        topology_worker_pool_pressure_per_1000(left)
            .cmp(&topology_worker_pool_pressure_per_1000(right))
            .then_with(|| left.pool_id.cmp(&right.pool_id))
    });

    pools
        .into_iter()
        .next()
        .map(TopologyWorkerPoolSelection::Selected)
}

fn topology_workload_requires_io(kind: ResourceTopologyWorkloadKind) -> bool {
    matches!(
        kind,
        ResourceTopologyWorkloadKind::CaptureWorker
            | ResourceTopologyWorkloadKind::SearchIndexing
            | ResourceTopologyWorkloadKind::ProofLaneJob
    )
}

fn topology_workload_requires_worker_pool(kind: ResourceTopologyWorkloadKind) -> bool {
    matches!(
        kind,
        ResourceTopologyWorkloadKind::CaptureWorker
            | ResourceTopologyWorkloadKind::SearchIndexing
            | ResourceTopologyWorkloadKind::ProofLaneJob
            | ResourceTopologyWorkloadKind::WorkflowFanout
    )
}

fn topology_worker_pool_pressure_per_1000(pool: &ResourceWorkerPoolSnapshot) -> u16 {
    utilization_per_1000_u32(pool.active_workers, pool.max_workers)
}

fn topology_total_risk_per_1000(
    cpu_pressure_per_1000: u16,
    memory_pressure_per_1000: u16,
    io_pressure_per_1000: u16,
    worker_pool_pressure_per_1000: u16,
    locality_penalty_per_1000: u16,
) -> u16 {
    let total = u32::from(cpu_pressure_per_1000) / 3
        + u32::from(memory_pressure_per_1000) / 3
        + u32::from(io_pressure_per_1000) / 5
        + u32::from(worker_pool_pressure_per_1000) / 5
        + u32::from(locality_penalty_per_1000);
    u16::try_from(total.min(1000)).unwrap_or(1000)
}

fn topology_available_cpu_lanes(node: &ResourceNumaNodeSnapshot, threshold: f64) -> u32 {
    let usable = (f64::from(node.cpu_lane_count) * threshold.clamp(0.0, 1.0)).floor() as u32;
    usable.saturating_sub(node.cpu_lanes_in_use)
}

fn topology_available_memory_bytes(node: &ResourceNumaNodeSnapshot, threshold: f64) -> u64 {
    let usable = (node.total_memory_bytes as f64 * threshold.clamp(0.0, 1.0)).floor() as u64;
    usable.saturating_sub(node.reserved_memory_bytes)
}

fn ratio_threshold_per_1000(value: f64) -> u16 {
    (value.clamp(0.0, 1.0) * 1000.0).round() as u16
}

fn utilization_per_1000_u32(used: u32, total: u32) -> u16 {
    if total == 0 {
        return 1000;
    }
    let ratio = u64::from(used).saturating_mul(1000) / u64::from(total);
    u16::try_from(ratio.min(1000)).unwrap_or(1000)
}

fn utilization_per_1000_u64(used: u64, total: u64) -> u16 {
    if total == 0 {
        return 1000;
    }
    let ratio = used.saturating_mul(1000) / total;
    u16::try_from(ratio.min(1000)).unwrap_or(1000)
}

fn div_ceil_u64(numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 {
        return numerator;
    }
    numerator / denominator + u64::from(numerator % denominator != 0)
}

fn topology_existing_cost(
    placements: &[ResourceTopologyPlacementTarget],
) -> ResourceTopologyCostEstimate {
    if placements.is_empty() {
        return ResourceTopologyCostEstimate::default();
    }

    let locality_penalty_per_1000 = if placements.len() > 1 { 250 } else { 0 };
    let memory_pressure_per_1000 = placements
        .iter()
        .map(|placement| match placement.memory_tier {
            ResourceMemoryTier::Hot => 250,
            ResourceMemoryTier::Warm => 450,
            ResourceMemoryTier::Cold => 750,
        })
        .max()
        .unwrap_or(0);
    let total_risk_per_1000 = topology_total_risk_per_1000(
        500,
        memory_pressure_per_1000,
        0,
        0,
        locality_penalty_per_1000,
    );

    ResourceTopologyCostEstimate {
        locality_penalty_per_1000,
        total_risk_per_1000,
        ..ResourceTopologyCostEstimate::default()
    }
}

fn topology_migration_cost_units(request: &ResourceTopologyWorkloadRequest) -> u32 {
    let memory_gib = request.requested_memory_bytes / 1_073_741_824;
    request
        .requested_cpu_lanes
        .saturating_mul(10)
        .saturating_add(u32::try_from(memory_gib).unwrap_or(u32::MAX))
}

fn apply_topology_receipt(
    topology: &mut ResourceTopologySnapshot,
    receipt: &ResourceTopologySimulationReceipt,
) {
    match receipt.action {
        ResourceTopologyPlanAction::Keep
        | ResourceTopologyPlanAction::Delay
        | ResourceTopologyPlanAction::Shed => {}
        ResourceTopologyPlanAction::Migrate => {
            release_topology_targets(topology, &receipt.from_placements);
            reserve_topology_targets(topology, &receipt.to_placements);
        }
        ResourceTopologyPlanAction::Place | ResourceTopologyPlanAction::Split => {
            reserve_topology_targets(topology, &receipt.to_placements);
        }
    }
}

fn reserve_topology_targets(
    topology: &mut ResourceTopologySnapshot,
    targets: &[ResourceTopologyPlacementTarget],
) {
    for target in targets {
        if let Some(node) = topology_node_mut(topology, target.numa_node_id) {
            node.cpu_lanes_in_use = node.cpu_lanes_in_use.saturating_add(target.cpu_lane_count);
            node.reserved_memory_bytes = node
                .reserved_memory_bytes
                .saturating_add(target.memory_reserved_bytes);
        }
        if let Some(pool_id) = &target.worker_pool_id {
            if let Some(pool) = topology
                .worker_pools
                .iter_mut()
                .find(|pool| &pool.pool_id == pool_id)
            {
                pool.active_workers = pool.active_workers.saturating_add(1);
            }
        }
    }
}

fn release_topology_targets(
    topology: &mut ResourceTopologySnapshot,
    targets: &[ResourceTopologyPlacementTarget],
) {
    for target in targets {
        if let Some(node) = topology_node_mut(topology, target.numa_node_id) {
            node.cpu_lanes_in_use = node.cpu_lanes_in_use.saturating_sub(target.cpu_lane_count);
            node.reserved_memory_bytes = node
                .reserved_memory_bytes
                .saturating_sub(target.memory_reserved_bytes);
        }
        if let Some(pool_id) = &target.worker_pool_id {
            if let Some(pool) = topology
                .worker_pools
                .iter_mut()
                .find(|pool| &pool.pool_id == pool_id)
            {
                pool.active_workers = pool.active_workers.saturating_sub(1);
            }
        }
    }
}

fn topology_node_mut(
    topology: &mut ResourceTopologySnapshot,
    numa_node_id: u32,
) -> Option<&mut ResourceNumaNodeSnapshot> {
    topology
        .numa_nodes
        .iter_mut()
        .find(|node| node.numa_node_id == numa_node_id)
}

fn sort_topology_snapshot(topology: &mut ResourceTopologySnapshot) {
    topology
        .numa_nodes
        .sort_by_key(|node| (node.numa_node_id, node.socket_id, node.cpu_lane_start));
    topology.io_devices.sort_by(|left, right| {
        left.device_id
            .cmp(&right.device_id)
            .then_with(|| left.attached_numa_node_id.cmp(&right.attached_numa_node_id))
    });
    topology.worker_pools.sort_by(|left, right| {
        left.pool_id
            .cmp(&right.pool_id)
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.numa_node_id.cmp(&right.numa_node_id))
    });
}

fn same_topology_placements(
    left: &[ResourceTopologyPlacementTarget],
    right: &[ResourceTopologyPlacementTarget],
) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort_by_key(topology_placement_sort_key);
    right.sort_by_key(topology_placement_sort_key);
    left == right
}

fn topology_placement_sort_key(
    placement: &ResourceTopologyPlacementTarget,
) -> (u32, u32, u32, u32, Option<String>, Option<String>) {
    (
        placement.numa_node_id,
        placement.socket_id,
        placement.cpu_lane_start,
        placement.cpu_lane_count,
        placement.io_device_id.clone(),
        placement.worker_pool_id.clone(),
    )
}

fn utilization(used: u32, total: u32) -> Option<f64> {
    (total > 0).then(|| f64::from(used) / f64::from(total))
}

fn utilization_u64(used: u64, total: u64) -> Option<f64> {
    (total > 0).then(|| used as f64 / total as f64)
}

fn saturating_usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn invalid_config_plan(
    host: &HighCoreHostResourceSnapshot,
    requests: &[SwarmWorkloadRequest],
    error: &str,
) -> ResourcePlacementPlan {
    let mut ordered = requests.to_vec();
    ordered.sort_by(|left, right| {
        left.work_priority
            .cmp(&right.work_priority)
            .then_with(|| right.mission_critical.cmp(&left.mission_critical))
            .then_with(|| left.stable_id.cmp(&right.stable_id))
    });

    let mut counters = ResourcePlacementCounters::default();
    let mut decisions = Vec::with_capacity(ordered.len());
    let reason_codes = vec![ResourcePlacementReasonCode::InvalidPlannerConfig];

    for (rank, request) in ordered.iter().enumerate() {
        counters.record(ResourcePlacementAction::Shed);
        let mut summary = operator_summary(ResourcePlacementAction::Shed, &reason_codes);
        summary.push_str(";config_error=");
        summary.push_str(error);
        decisions.push(ResourcePlacementDecision {
            request_id: request.stable_id.clone(),
            rank: rank + 1,
            action: ResourcePlacementAction::Shed,
            reason_codes: reason_codes.clone(),
            placement: None,
            predicted_cpu_utilization: utilization(
                host.cpu_lanes_in_use
                    .saturating_add(request.requested_cpu_lanes),
                host.logical_cpu_count,
            ),
            predicted_memory_utilization: utilization_u64(
                host.reserved_memory_bytes
                    .saturating_add(request.requested_memory_bytes),
                host.total_memory_bytes,
            ),
            operator_summary: summary,
        });
    }

    ResourcePlacementPlan {
        planner_version: 1,
        host: host.clone(),
        decisions,
        counters,
    }
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
            if host.backpressure_tier >= BackpressureTier::Red || host.capture_backlog_ratio >= 0.95
            {
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

    fn topology_two_node() -> ResourceTopologySnapshot {
        ResourceTopologySnapshot {
            numa_nodes: vec![
                ResourceNumaNodeSnapshot::healthy(0, 0, 0, 32, ResourceMemoryTier::Hot, 128 * GIB),
                ResourceNumaNodeSnapshot::healthy(
                    1,
                    1,
                    32,
                    32,
                    ResourceMemoryTier::Warm,
                    128 * GIB,
                ),
            ],
            io_devices: vec![
                ResourceIoDeviceSnapshot {
                    device_id: "nvme0".to_string(),
                    attached_numa_node_id: Some(0),
                    utilization_per_1000: 200,
                },
                ResourceIoDeviceSnapshot {
                    device_id: "nvme1".to_string(),
                    attached_numa_node_id: Some(1),
                    utilization_per_1000: 200,
                },
            ],
            worker_pools: vec![
                ResourceWorkerPoolSnapshot {
                    pool_id: "capture-0".to_string(),
                    kind: ResourceTopologyWorkloadKind::CaptureWorker,
                    numa_node_id: Some(0),
                    active_workers: 1,
                    max_workers: 4,
                },
                ResourceWorkerPoolSnapshot {
                    pool_id: "search-1".to_string(),
                    kind: ResourceTopologyWorkloadKind::SearchIndexing,
                    numa_node_id: Some(1),
                    active_workers: 1,
                    max_workers: 4,
                },
                ResourceWorkerPoolSnapshot {
                    pool_id: "proof-0".to_string(),
                    kind: ResourceTopologyWorkloadKind::ProofLaneJob,
                    numa_node_id: Some(0),
                    active_workers: 0,
                    max_workers: 2,
                },
            ],
        }
    }

    fn topology_request(
        id: &str,
        kind: ResourceTopologyWorkloadKind,
        cpu: u32,
        memory_gib: u64,
    ) -> ResourceTopologyWorkloadRequest {
        ResourceTopologyWorkloadRequest::new(id, kind, cpu, memory_gib * GIB)
    }

    fn topology_target(
        numa_node_id: u32,
        socket_id: u32,
        cpu_lane_start: u32,
        cpu_lane_count: u32,
        memory_gib: u64,
        memory_tier: ResourceMemoryTier,
    ) -> ResourceTopologyPlacementTarget {
        ResourceTopologyPlacementTarget {
            numa_node_id,
            socket_id,
            cpu_lane_start,
            cpu_lane_count,
            memory_reserved_bytes: memory_gib * GIB,
            memory_tier,
            io_device_id: None,
            worker_pool_id: None,
        }
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
        assert_eq!(
            plan.decisions[0].placement.as_ref().unwrap().cpu_lane_start,
            0
        );
        assert_eq!(
            plan.decisions[1].placement.as_ref().unwrap().cpu_lane_start,
            8
        );
        assert_eq!(
            plan.decisions[2].placement.as_ref().unwrap().cpu_lane_start,
            10
        );
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
    fn plan_summary_folds_highest_action_counters_and_unique_reasons() {
        let planner = ResourceAwarePlacementPlanner::default();
        let mut host = host_64c_256gib();
        host.capture_backlog_ratio = 0.90;

        let plan = planner.plan(
            &host,
            &[
                request("capture", 2, 4),
                request("approval", 2, 4).requires_approval(),
            ],
        );
        let summary = plan.summary();

        assert_eq!(summary.total_requests, 2);
        assert_eq!(
            summary.highest_action,
            ResourcePlacementAction::RequireApproval
        );
        assert_eq!(summary.counters.degraded, 1);
        assert_eq!(summary.counters.approval_required, 1);
        assert!(
            summary
                .reason_codes
                .contains(&ResourcePlacementReasonCode::CaptureBacklogPressure)
        );
        assert!(
            summary
                .reason_codes
                .contains(&ResourcePlacementReasonCode::PolicyApprovalRequired)
        );
        assert!(summary.operator_summary.contains("RequireApproval"));
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
    fn invalid_planner_config_fails_closed_for_all_requests() {
        let config = ResourcePlacementPlannerConfig {
            max_cpu_utilization: f64::NAN,
            ..ResourcePlacementPlannerConfig::default()
        };
        let planner = ResourceAwarePlacementPlanner::new(config);

        let plan = planner.plan(
            &host_64c_256gib(),
            &[
                request("background", 2, 4),
                request("mission", 2, 4).mission_critical(),
            ],
        );
        let summary = plan.summary();

        assert_eq!(plan.counters.shed, 2);
        assert_eq!(summary.highest_action, ResourcePlacementAction::Shed);
        assert_eq!(summary.counters.shed, 2);
        assert!(
            summary
                .reason_codes
                .contains(&ResourcePlacementReasonCode::InvalidPlannerConfig)
        );

        for decision in &plan.decisions {
            assert_eq!(decision.action, ResourcePlacementAction::Shed);
            assert!(decision.placement.is_none());
            assert!(
                decision
                    .reason_codes
                    .contains(&ResourcePlacementReasonCode::InvalidPlannerConfig)
            );
            assert!(decision.operator_summary.contains("InvalidPlannerConfig"));
            assert!(decision.operator_summary.contains("max_cpu_utilization"));
        }
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

    #[test]
    fn topology_simulation_places_on_preferred_numa_node() {
        let planner = ResourceAwarePlacementPlanner::default();
        let request = topology_request("pane-a", ResourceTopologyWorkloadKind::AgentPane, 4, 8)
            .prefer_numa_node(1);

        let plan = planner.simulate_topology_rebalance(&topology_two_node(), &[request]);
        let receipt = &plan.receipts[0];

        assert_eq!(receipt.action, ResourceTopologyPlanAction::Place);
        assert_eq!(receipt.to_placements[0].numa_node_id, 1);
        assert_eq!(
            receipt.evidence_kind,
            ResourceTopologyEvidenceKind::Simulated
        );
        assert!(receipt.operator_summary.contains("source=simulated"));
    }

    #[test]
    fn topology_memory_tier_exhaustion_delays_hot_request() {
        let planner = ResourceAwarePlacementPlanner::default();
        let mut topology = topology_two_node();
        topology.numa_nodes[0].reserved_memory_bytes = 124 * GIB;
        let request = topology_request("hot-pane", ResourceTopologyWorkloadKind::AgentPane, 2, 8)
            .require_memory_tier(ResourceMemoryTier::Hot)
            .no_split();

        let plan = planner.simulate_topology_rebalance(&topology, &[request]);
        let receipt = &plan.receipts[0];

        assert_eq!(receipt.action, ResourceTopologyPlanAction::Delay);
        assert!(
            receipt
                .reason_codes
                .contains(&ResourcePlacementReasonCode::NumaMemoryTierExhausted)
        );
        assert!(receipt.to_placements.is_empty());
    }

    #[test]
    fn topology_io_device_contention_delays_capture_worker() {
        let planner = ResourceAwarePlacementPlanner::default();
        let mut topology = topology_two_node();
        topology.io_devices[0].utilization_per_1000 = 950;
        let request = topology_request(
            "capture-a",
            ResourceTopologyWorkloadKind::CaptureWorker,
            2,
            4,
        )
        .prefer_numa_node(0)
        .prefer_io_device("nvme0")
        .no_split();

        let plan = planner.simulate_topology_rebalance(&topology, &[request]);
        let receipt = &plan.receipts[0];

        assert_eq!(receipt.action, ResourceTopologyPlanAction::Delay);
        assert!(
            receipt
                .reason_codes
                .contains(&ResourcePlacementReasonCode::IoDeviceContention)
        );
    }

    #[test]
    fn topology_search_indexing_uses_isolated_worker_pool() {
        let planner = ResourceAwarePlacementPlanner::default();
        let request = topology_request(
            "search-a",
            ResourceTopologyWorkloadKind::SearchIndexing,
            2,
            4,
        );

        let plan = planner.simulate_topology_rebalance(&topology_two_node(), &[request]);
        let receipt = &plan.receipts[0];

        assert_eq!(receipt.action, ResourceTopologyPlanAction::Place);
        assert_eq!(receipt.to_placements[0].numa_node_id, 1);
        assert_eq!(
            receipt.to_placements[0].worker_pool_id.as_deref(),
            Some("search-1")
        );
        assert!(
            receipt
                .reason_codes
                .contains(&ResourcePlacementReasonCode::SearchIndexingIsolation)
        );
    }

    #[test]
    fn topology_rebalance_hysteresis_keeps_existing_placement() {
        let planner = ResourceAwarePlacementPlanner::new(ResourcePlacementPlannerConfig {
            rebalance_hysteresis_min_benefit_per_1000: 400,
            ..ResourcePlacementPlannerConfig::default()
        });
        let mut topology = topology_two_node();
        topology.numa_nodes[0].cpu_lanes_in_use = 4;
        topology.numa_nodes[0].reserved_memory_bytes = 8 * GIB;
        let previous = topology_target(0, 0, 0, 4, 8, ResourceMemoryTier::Hot);
        let request = topology_request("pane-a", ResourceTopologyWorkloadKind::AgentPane, 4, 8)
            .prefer_numa_node(1)
            .with_previous_placement(previous.clone());

        let plan = planner.simulate_topology_rebalance(&topology, &[request]);
        let receipt = &plan.receipts[0];

        assert_eq!(receipt.action, ResourceTopologyPlanAction::Keep);
        assert_eq!(receipt.to_placements, vec![previous]);
        assert!(
            receipt
                .reason_codes
                .contains(&ResourcePlacementReasonCode::RebalanceHysteresis)
        );
    }

    #[test]
    fn topology_rebalance_migrates_when_benefit_clears_hysteresis() {
        let planner = ResourceAwarePlacementPlanner::new(ResourcePlacementPlannerConfig {
            rebalance_hysteresis_min_benefit_per_1000: 1,
            ..ResourcePlacementPlannerConfig::default()
        });
        let mut topology = topology_two_node();
        topology.numa_nodes[0].cpu_lanes_in_use = 4;
        topology.numa_nodes[0].reserved_memory_bytes = 8 * GIB;
        let previous = topology_target(0, 0, 0, 4, 8, ResourceMemoryTier::Hot);
        let request = topology_request("pane-a", ResourceTopologyWorkloadKind::AgentPane, 4, 8)
            .prefer_numa_node(1)
            .with_previous_placement(previous);

        let plan = planner.simulate_topology_rebalance(&topology, &[request]);
        let receipt = &plan.receipts[0];

        assert_eq!(receipt.action, ResourceTopologyPlanAction::Migrate);
        assert_eq!(receipt.to_placements[0].numa_node_id, 1);
        assert!(receipt.cost.migration_cost_units > 0);
    }

    #[test]
    fn topology_can_split_across_numa_nodes_when_single_node_is_too_small() {
        let planner = ResourceAwarePlacementPlanner::default();
        let request = topology_request("wide-pane", ResourceTopologyWorkloadKind::AgentPane, 48, 4);

        let plan = planner.simulate_topology_rebalance(&topology_two_node(), &[request]);
        let receipt = &plan.receipts[0];

        assert_eq!(receipt.action, ResourceTopologyPlanAction::Split);
        assert_eq!(receipt.to_placements.len(), 2);
        assert!(
            receipt
                .reason_codes
                .contains(&ResourcePlacementReasonCode::CrossNumaSplit)
        );
    }

    #[test]
    fn topology_missing_capacity_sheds_noncritical_work() {
        let planner = ResourceAwarePlacementPlanner::default();
        let topology = ResourceTopologySnapshot {
            numa_nodes: Vec::new(),
            io_devices: Vec::new(),
            worker_pools: Vec::new(),
        };
        let request = topology_request("orphan", ResourceTopologyWorkloadKind::AgentPane, 1, 1);

        let plan = planner.simulate_topology_rebalance(&topology, &[request]);
        let receipt = &plan.receipts[0];

        assert_eq!(receipt.action, ResourceTopologyPlanAction::Shed);
        assert!(
            receipt
                .reason_codes
                .contains(&ResourcePlacementReasonCode::MissingHostCapacity)
        );
    }

    #[test]
    fn topology_plan_has_stable_serialized_fixture() {
        let planner = ResourceAwarePlacementPlanner::default();
        let topology = ResourceTopologySnapshot {
            numa_nodes: vec![ResourceNumaNodeSnapshot::healthy(
                0,
                0,
                0,
                8,
                ResourceMemoryTier::Warm,
                16 * GIB,
            )],
            io_devices: Vec::new(),
            worker_pools: Vec::new(),
        };
        let request = topology_request("pane-a", ResourceTopologyWorkloadKind::AgentPane, 1, 1);

        let plan = planner.simulate_topology_rebalance(&topology, &[request]);
        let json = serde_json::to_string(&plan).unwrap();
        let golden = concat!(
            "{\"planner_version\":2,\"evidence_kind\":\"simulated\",\"topology\":",
            "{\"numa_nodes\":[{\"numa_node_id\":0,\"socket_id\":0,\"cpu_lane_start\":0,",
            "\"cpu_lane_count\":8,\"cpu_lanes_in_use\":0,\"memory_tier\":\"warm\",",
            "\"total_memory_bytes\":17179869184,\"reserved_memory_bytes\":0}],",
            "\"io_devices\":[],\"worker_pools\":[]},\"receipts\":[{\"request_id\":\"pane-a\",",
            "\"rank\":1,\"workload_kind\":\"agent_pane\",\"action\":\"place\",",
            "\"reason_codes\":[\"topology_simulated\"],\"evidence_kind\":\"simulated\",",
            "\"from_placements\":[],\"to_placements\":[{\"numa_node_id\":0,\"socket_id\":0,",
            "\"cpu_lane_start\":0,\"cpu_lane_count\":1,\"memory_reserved_bytes\":1073741824,",
            "\"memory_tier\":\"warm\"}],\"cost\":{\"locality_penalty_per_1000\":0,",
            "\"io_pressure_per_1000\":0,\"worker_pool_pressure_per_1000\":0,",
            "\"migration_cost_units\":0,\"total_risk_per_1000\":61},",
            "\"operator_summary\":\"source=simulated;action=Place;risk_per_1000=61;",
            "reasons=TopologySimulated\"}],\"counters\":{\"kept\":0,\"placed\":1,",
            "\"migrated\":0,\"split\":0,\"delayed\":0,\"shed\":0},",
            "\"operator_summary\":\"source=simulated;kept=0;placed=1;migrated=0;",
            "split=0;delayed=0;shed=0\"}"
        );

        assert_eq!(json, golden);
        let restored: ResourceTopologySimulationPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, plan);
    }
}
