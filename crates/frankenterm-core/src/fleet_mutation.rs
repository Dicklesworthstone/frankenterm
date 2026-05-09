//! Shared transaction substrate for daemon-mediated fleet mutations.
//!
//! `ft robot profile apply`, `ft robot fleet scale`, and
//! `ft robot fleet rebalance` need the same safety properties before
//! any of them should perform live daemon/mux mutations:
//!
//! - policy/approval preflight before side effects;
//! - idempotent replay for retried Robot/MCP requests;
//! - dry-run receipts that use the same plan as commit;
//! - no silent partial success; and
//! - compensation receipts for already-committed steps when a later
//!   step fails.
//!
//! This module is intentionally pure. It defines the plan, receipt,
//! idempotency, and executor contracts that the daemon-side profile,
//! scale, and rebalance handlers can share. Live mux spawning/stopping
//! is supplied by an implementation of [`FleetMutationExecutor`].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::robot_idempotency::MutationKey;

/// One daemon-side fleet/profile mutation action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetMutationAction {
    /// Spawn an agent pane from a resolved profile/program.
    SpawnAgent {
        profile_name: String,
        program: String,
        cwd: Option<String>,
        domain: Option<String>,
        env: BTreeMap<String, String>,
    },
    /// Stop an existing agent pane.
    StopAgent {
        pane_id: u64,
        reason: String,
    },
    /// Reassign one work item from one agent to another.
    ReassignWork {
        work_item_id: String,
        from_agent: Option<String>,
        to_agent: String,
        reason: String,
    },
}

impl FleetMutationAction {
    /// Stable, human-readable action family.
    #[must_use]
    pub const fn family(&self) -> &'static str {
        match self {
            Self::SpawnAgent { .. } => "spawn_agent",
            Self::StopAgent { .. } => "stop_agent",
            Self::ReassignWork { .. } => "reassign_work",
        }
    }

    /// Stable target identifier used in receipts and forensic logs.
    #[must_use]
    pub fn target_id(&self) -> String {
        match self {
            Self::SpawnAgent {
                profile_name,
                program,
                ..
            } => format!("profile:{profile_name}:program:{program}"),
            Self::StopAgent { pane_id, .. } => format!("pane:{pane_id}"),
            Self::ReassignWork { work_item_id, .. } => format!("work:{work_item_id}"),
        }
    }
}

/// Policy/approval decision attached to a mutation step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum FleetMutationPolicyDecision {
    Allow,
    Deny {
        reason_code: String,
        message: String,
    },
    RequireApproval {
        approval_id: String,
        reason_code: String,
        message: String,
    },
}

impl FleetMutationPolicyDecision {
    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// A single planned mutation step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMutationStep {
    pub step_id: String,
    pub action: FleetMutationAction,
    pub policy: FleetMutationPolicyDecision,
    /// Optional action that compensates this step after it has
    /// succeeded and a later step fails.
    pub compensation: Option<FleetMutationAction>,
}

impl FleetMutationStep {
    #[must_use]
    pub fn mutation_key(&self, plan_id: &str) -> MutationKey {
        let fingerprint = stable_json(self);
        MutationKey::derive(
            "fleet_mutation_step",
            &format!("{plan_id}:{}:{fingerprint}", self.step_id),
        )
    }
}

/// A complete mutation plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMutationPlan {
    pub plan_id: String,
    pub idempotency_key: MutationKey,
    pub dry_run: bool,
    pub steps: Vec<FleetMutationStep>,
}

impl FleetMutationPlan {
    /// Build a plan and derive a content-addressed idempotency key
    /// from the stable plan payload.
    #[must_use]
    pub fn new(plan_id: impl Into<String>, dry_run: bool, steps: Vec<FleetMutationStep>) -> Self {
        let plan_id = plan_id.into();
        let fingerprint = stable_json(&steps);
        let idempotency_key = MutationKey::derive(
            "fleet_mutation_plan",
            &format!("{plan_id}:dry_run={dry_run}:{fingerprint}"),
        );
        Self {
            plan_id,
            idempotency_key,
            dry_run,
            steps,
        }
    }

    /// Build a plan with a caller-supplied idempotency key.
    #[must_use]
    pub fn with_client_key(
        plan_id: impl Into<String>,
        dry_run: bool,
        steps: Vec<FleetMutationStep>,
        client_key: &str,
    ) -> Self {
        Self {
            plan_id: plan_id.into(),
            idempotency_key: MutationKey::from_client("fleet_mutation_plan", client_key),
            dry_run,
            steps,
        }
    }
}

/// Executor-provided output for a mutation or compensation action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FleetMutationStepOutput {
    pub pane_id: Option<u64>,
    pub work_item_id: Option<String>,
    pub details: BTreeMap<String, String>,
}

/// Executor failure with stable machine code plus operator message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMutationExecutionError {
    pub error_code: String,
    pub message: String,
}

impl FleetMutationExecutionError {
    #[must_use]
    pub fn new(error_code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error_code: error_code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for FleetMutationExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.error_code, self.message)
    }
}

impl std::error::Error for FleetMutationExecutionError {}

/// Live side-effect adapter supplied by the daemon/mux layer.
pub trait FleetMutationExecutor {
    fn execute_step(
        &mut self,
        step: &FleetMutationStep,
    ) -> Result<FleetMutationStepOutput, FleetMutationExecutionError>;

    fn compensate_step(
        &mut self,
        original: &FleetMutationStep,
        compensation: &FleetMutationAction,
    ) -> Result<FleetMutationStepOutput, FleetMutationExecutionError>;
}

/// Terminal status for a whole plan receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetMutationReceiptStatus {
    DryRun,
    Succeeded,
    Denied,
    ApprovalRequired,
    Failed,
    Compensated,
    CompensationFailed,
}

/// Terminal status for one step receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetMutationStepStatus {
    Planned,
    Skipped,
    Succeeded,
    Failed,
    Compensated,
}

/// Terminal status for compensating a successful step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetMutationCompensationStatus {
    Succeeded,
    Failed,
    NotAvailable,
}

/// Compensation evidence attached to a previously succeeded step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMutationCompensationReceipt {
    pub status: FleetMutationCompensationStatus,
    pub action: Option<FleetMutationAction>,
    pub output: Option<FleetMutationStepOutput>,
    pub error: Option<FleetMutationExecutionError>,
}

/// Receipt for one mutation step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMutationStepReceipt {
    pub step_id: String,
    pub action: FleetMutationAction,
    pub target_id: String,
    pub status: FleetMutationStepStatus,
    pub policy: FleetMutationPolicyDecision,
    pub idempotency_key: MutationKey,
    pub output: Option<FleetMutationStepOutput>,
    pub error: Option<FleetMutationExecutionError>,
    pub compensation: Option<FleetMutationCompensationReceipt>,
}

/// Durable receipt for a whole mutation plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMutationReceipt {
    pub plan_id: String,
    pub idempotency_key: MutationKey,
    pub dry_run: bool,
    pub status: FleetMutationReceiptStatus,
    pub idempotent_replay: bool,
    pub completed_count: usize,
    pub failed_count: usize,
    pub compensated_count: usize,
    pub skipped_count: usize,
    pub steps: Vec<FleetMutationStepReceipt>,
}

impl FleetMutationReceipt {
    fn replay(mut self) -> Self {
        self.idempotent_replay = true;
        self
    }
}

/// Plan-level validation error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetMutationPlanError {
    EmptyPlan,
    EmptyStepId {
        step_index: usize,
    },
    DuplicateStepId {
        step_id: String,
        first_index: usize,
        duplicate_index: usize,
    },
}

impl std::fmt::Display for FleetMutationPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPlan => f.write_str("fleet mutation plan must contain at least one step"),
            Self::EmptyStepId { step_index } => {
                write!(f, "fleet mutation step at index {step_index} has an empty step_id")
            }
            Self::DuplicateStepId {
                step_id,
                first_index,
                duplicate_index,
            } => write!(
                f,
                "fleet mutation step_id `{step_id}` appears at indexes {first_index} and {duplicate_index}"
            ),
        }
    }
}

impl std::error::Error for FleetMutationPlanError {}

/// In-memory receipt ledger keyed by plan idempotency key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FleetMutationLedger {
    receipts: BTreeMap<String, FleetMutationReceipt>,
}

impl FleetMutationLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn get(&self, key: &MutationKey) -> Option<&FleetMutationReceipt> {
        self.receipts.get(key.as_str())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.receipts.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty()
    }

    /// Execute a plan or return the cached receipt for an idempotent replay.
    pub fn execute_plan<E: FleetMutationExecutor>(
        &mut self,
        plan: &FleetMutationPlan,
        executor: &mut E,
    ) -> Result<FleetMutationReceipt, FleetMutationPlanError> {
        validate_plan(plan)?;
        if let Some(receipt) = self.receipts.get(plan.idempotency_key.as_str()) {
            return Ok(receipt.clone().replay());
        }

        let receipt = if plan.dry_run {
            dry_run_receipt(plan)
        } else if let Some(status) = preflight_block_status(plan) {
            preflight_blocked_receipt(plan, status)
        } else {
            commit_receipt(plan, executor)
        };

        self.receipts
            .insert(plan.idempotency_key.as_str().to_string(), receipt.clone());
        Ok(receipt)
    }
}

fn validate_plan(plan: &FleetMutationPlan) -> Result<(), FleetMutationPlanError> {
    if plan.steps.is_empty() {
        return Err(FleetMutationPlanError::EmptyPlan);
    }

    let mut seen = BTreeMap::<&str, usize>::new();
    for (index, step) in plan.steps.iter().enumerate() {
        if step.step_id.is_empty() {
            return Err(FleetMutationPlanError::EmptyStepId { step_index: index });
        }
        if let Some(first_index) = seen.insert(step.step_id.as_str(), index) {
            return Err(FleetMutationPlanError::DuplicateStepId {
                step_id: step.step_id.clone(),
                first_index,
                duplicate_index: index,
            });
        }
    }
    Ok(())
}

fn preflight_block_status(plan: &FleetMutationPlan) -> Option<FleetMutationReceiptStatus> {
    if plan
        .steps
        .iter()
        .any(|step| matches!(step.policy, FleetMutationPolicyDecision::Deny { .. }))
    {
        Some(FleetMutationReceiptStatus::Denied)
    } else if plan.steps.iter().any(|step| {
        matches!(
            step.policy,
            FleetMutationPolicyDecision::RequireApproval { .. }
        )
    }) {
        Some(FleetMutationReceiptStatus::ApprovalRequired)
    } else {
        None
    }
}

fn dry_run_receipt(plan: &FleetMutationPlan) -> FleetMutationReceipt {
    let steps = plan
        .steps
        .iter()
        .map(|step| step_receipt(plan, step, FleetMutationStepStatus::Planned, None, None))
        .collect::<Vec<_>>();
    receipt_with_counts(plan, FleetMutationReceiptStatus::DryRun, false, steps)
}

fn preflight_blocked_receipt(
    plan: &FleetMutationPlan,
    status: FleetMutationReceiptStatus,
) -> FleetMutationReceipt {
    let steps = plan
        .steps
        .iter()
        .map(|step| step_receipt(plan, step, FleetMutationStepStatus::Skipped, None, None))
        .collect::<Vec<_>>();
    receipt_with_counts(plan, status, false, steps)
}

fn commit_receipt<E: FleetMutationExecutor>(
    plan: &FleetMutationPlan,
    executor: &mut E,
) -> FleetMutationReceipt {
    let mut receipts = Vec::with_capacity(plan.steps.len());
    let mut succeeded = Vec::new();

    for step in &plan.steps {
        match executor.execute_step(step) {
            Ok(output) => {
                succeeded.push(receipts.len());
                receipts.push(step_receipt(
                    plan,
                    step,
                    FleetMutationStepStatus::Succeeded,
                    Some(output),
                    None,
                ));
            }
            Err(err) => {
                receipts.push(step_receipt(
                    plan,
                    step,
                    FleetMutationStepStatus::Failed,
                    None,
                    Some(err),
                ));
                compensate_successes(plan, executor, &mut receipts, &succeeded);
                let status = if receipts
                    .iter()
                    .any(|receipt| compensation_failed(receipt.compensation.as_ref()))
                {
                    FleetMutationReceiptStatus::CompensationFailed
                } else if receipts.iter().any(|receipt| receipt.compensation.is_some()) {
                    FleetMutationReceiptStatus::Compensated
                } else {
                    FleetMutationReceiptStatus::Failed
                };
                return receipt_with_counts(plan, status, false, receipts);
            }
        }
    }

    receipt_with_counts(plan, FleetMutationReceiptStatus::Succeeded, false, receipts)
}

fn compensate_successes<E: FleetMutationExecutor>(
    plan: &FleetMutationPlan,
    executor: &mut E,
    receipts: &mut [FleetMutationStepReceipt],
    succeeded_indexes: &[usize],
) {
    for index in succeeded_indexes.iter().rev().copied() {
        let Some(receipt) = receipts.get_mut(index) else {
            continue;
        };
        let Some(step) = plan.steps.get(index) else {
            continue;
        };
        let Some(compensation) = step.compensation.as_ref() else {
            receipt.compensation = Some(FleetMutationCompensationReceipt {
                status: FleetMutationCompensationStatus::NotAvailable,
                action: None,
                output: None,
                error: None,
            });
            continue;
        };

        match executor.compensate_step(step, compensation) {
            Ok(output) => {
                receipt.status = FleetMutationStepStatus::Compensated;
                receipt.compensation = Some(FleetMutationCompensationReceipt {
                    status: FleetMutationCompensationStatus::Succeeded,
                    action: Some(compensation.clone()),
                    output: Some(output),
                    error: None,
                });
            }
            Err(err) => {
                receipt.compensation = Some(FleetMutationCompensationReceipt {
                    status: FleetMutationCompensationStatus::Failed,
                    action: Some(compensation.clone()),
                    output: None,
                    error: Some(err),
                });
            }
        }
    }
}

fn step_receipt(
    plan: &FleetMutationPlan,
    step: &FleetMutationStep,
    status: FleetMutationStepStatus,
    output: Option<FleetMutationStepOutput>,
    error: Option<FleetMutationExecutionError>,
) -> FleetMutationStepReceipt {
    FleetMutationStepReceipt {
        step_id: step.step_id.clone(),
        action: step.action.clone(),
        target_id: step.action.target_id(),
        status,
        policy: step.policy.clone(),
        idempotency_key: step.mutation_key(&plan.plan_id),
        output,
        error,
        compensation: None,
    }
}

fn receipt_with_counts(
    plan: &FleetMutationPlan,
    status: FleetMutationReceiptStatus,
    idempotent_replay: bool,
    steps: Vec<FleetMutationStepReceipt>,
) -> FleetMutationReceipt {
    let completed_count = steps
        .iter()
        .filter(|step| matches!(step.status, FleetMutationStepStatus::Succeeded))
        .count();
    let failed_count = steps
        .iter()
        .filter(|step| matches!(step.status, FleetMutationStepStatus::Failed))
        .count();
    let compensated_count = steps
        .iter()
        .filter(|step| matches!(step.status, FleetMutationStepStatus::Compensated))
        .count();
    let skipped_count = steps
        .iter()
        .filter(|step| matches!(step.status, FleetMutationStepStatus::Skipped))
        .count();

    FleetMutationReceipt {
        plan_id: plan.plan_id.clone(),
        idempotency_key: plan.idempotency_key.clone(),
        dry_run: plan.dry_run,
        status,
        idempotent_replay,
        completed_count,
        failed_count,
        compensated_count,
        skipped_count,
        steps,
    }
}

fn compensation_failed(compensation: Option<&FleetMutationCompensationReceipt>) -> bool {
    compensation.is_some_and(|receipt| receipt.status == FleetMutationCompensationStatus::Failed)
}

fn stable_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|err| format!("serde-error:{err}"))
}

/// Convenience assertion for callers reconstructing receipts from durable
/// storage: every compensation must refer to a step that actually reached a
/// side-effect state.
#[must_use]
pub fn receipt_has_no_orphan_compensation(receipt: &FleetMutationReceipt) -> bool {
    let committed: BTreeSet<&str> = receipt
        .steps
        .iter()
        .filter(|step| {
            matches!(
                step.status,
                FleetMutationStepStatus::Succeeded | FleetMutationStepStatus::Compensated
            ) || step.compensation.is_some()
        })
        .map(|step| step.step_id.as_str())
        .collect();

    receipt.steps.iter().all(|step| {
        step.compensation.is_none() || committed.contains(step.step_id.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeExecutor {
        calls: Vec<String>,
        fail_step: Option<String>,
        fail_compensation_for: Option<String>,
        next_pane_id: u64,
    }

    impl FakeExecutor {
        fn new() -> Self {
            Self {
                next_pane_id: 100,
                ..Self::default()
            }
        }
    }

    impl FleetMutationExecutor for FakeExecutor {
        fn execute_step(
            &mut self,
            step: &FleetMutationStep,
        ) -> Result<FleetMutationStepOutput, FleetMutationExecutionError> {
            self.calls.push(format!("execute:{}", step.step_id));
            if self.fail_step.as_deref() == Some(step.step_id.as_str()) {
                return Err(FleetMutationExecutionError::new(
                    "fake.step_failed",
                    format!("step {} failed", step.step_id),
                ));
            }
            let mut output = FleetMutationStepOutput::default();
            if matches!(step.action, FleetMutationAction::SpawnAgent { .. }) {
                output.pane_id = Some(self.next_pane_id);
                self.next_pane_id += 1;
            }
            Ok(output)
        }

        fn compensate_step(
            &mut self,
            original: &FleetMutationStep,
            _compensation: &FleetMutationAction,
        ) -> Result<FleetMutationStepOutput, FleetMutationExecutionError> {
            self.calls.push(format!("compensate:{}", original.step_id));
            if self.fail_compensation_for.as_deref() == Some(original.step_id.as_str()) {
                return Err(FleetMutationExecutionError::new(
                    "fake.compensation_failed",
                    format!("compensation for {} failed", original.step_id),
                ));
            }
            Ok(FleetMutationStepOutput::default())
        }
    }

    fn spawn_step(step_id: &str) -> FleetMutationStep {
        FleetMutationStep {
            step_id: step_id.to_string(),
            action: FleetMutationAction::SpawnAgent {
                profile_name: "default".to_string(),
                program: "codex".to_string(),
                cwd: Some("/repo".to_string()),
                domain: None,
                env: BTreeMap::new(),
            },
            policy: FleetMutationPolicyDecision::Allow,
            compensation: Some(FleetMutationAction::StopAgent {
                pane_id: 100,
                reason: "rollback spawned agent".to_string(),
            }),
        }
    }

    fn stop_step(step_id: &str, pane_id: u64) -> FleetMutationStep {
        FleetMutationStep {
            step_id: step_id.to_string(),
            action: FleetMutationAction::StopAgent {
                pane_id,
                reason: "scale down".to_string(),
            },
            policy: FleetMutationPolicyDecision::Allow,
            compensation: None,
        }
    }

    #[test]
    fn dry_run_returns_plan_without_executor_side_effects() {
        let plan = FleetMutationPlan::new("plan-a", true, vec![spawn_step("spawn-1")]);
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let receipt = ledger
            .execute_plan(&plan, &mut executor)
            .expect("dry-run receipt");

        assert_eq!(receipt.status, FleetMutationReceiptStatus::DryRun);
        assert_eq!(receipt.steps[0].status, FleetMutationStepStatus::Planned);
        assert!(executor.calls.is_empty());
        assert_eq!(receipt.completed_count, 0);
    }

    #[test]
    fn policy_denial_prevents_all_side_effects() {
        let mut denied = spawn_step("spawn-denied");
        denied.policy = FleetMutationPolicyDecision::Deny {
            reason_code: "policy.forbidden".to_string(),
            message: "spawning blocked".to_string(),
        };
        let plan = FleetMutationPlan::new("plan-denied", false, vec![denied, spawn_step("later")]);
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let receipt = ledger
            .execute_plan(&plan, &mut executor)
            .expect("denied receipt");

        assert_eq!(receipt.status, FleetMutationReceiptStatus::Denied);
        assert_eq!(receipt.skipped_count, 2);
        assert!(executor.calls.is_empty());
    }

    #[test]
    fn idempotent_replay_returns_cached_receipt_without_reexecution() {
        let plan = FleetMutationPlan::new("plan-replay", false, vec![spawn_step("spawn-1")]);
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let first = ledger.execute_plan(&plan, &mut executor).expect("first");
        assert_eq!(first.status, FleetMutationReceiptStatus::Succeeded);
        assert_eq!(executor.calls, vec!["execute:spawn-1"]);

        let replay = ledger.execute_plan(&plan, &mut executor).expect("replay");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.steps, first.steps);
        assert_eq!(executor.calls, vec!["execute:spawn-1"]);
    }

    #[test]
    fn partial_failure_compensates_successes_in_reverse_order() {
        let plan = FleetMutationPlan::new(
            "plan-comp",
            false,
            vec![
                spawn_step("spawn-1"),
                spawn_step("spawn-2"),
                stop_step("stop-fails", 42),
            ],
        );
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();
        executor.fail_step = Some("stop-fails".to_string());

        let receipt = ledger.execute_plan(&plan, &mut executor).expect("receipt");

        assert_eq!(receipt.status, FleetMutationReceiptStatus::Compensated);
        assert_eq!(receipt.failed_count, 1);
        assert_eq!(receipt.compensated_count, 2);
        assert_eq!(
            executor.calls,
            vec![
                "execute:spawn-1",
                "execute:spawn-2",
                "execute:stop-fails",
                "compensate:spawn-2",
                "compensate:spawn-1",
            ]
        );
        assert!(receipt_has_no_orphan_compensation(&receipt));
    }

    #[test]
    fn compensation_failure_is_explicit_in_plan_status() {
        let plan =
            FleetMutationPlan::new("plan-comp-fail", false, vec![spawn_step("spawn-1"), stop_step("fail", 7)]);
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();
        executor.fail_step = Some("fail".to_string());
        executor.fail_compensation_for = Some("spawn-1".to_string());

        let receipt = ledger.execute_plan(&plan, &mut executor).expect("receipt");

        assert_eq!(receipt.status, FleetMutationReceiptStatus::CompensationFailed);
        let compensation = receipt.steps[0]
            .compensation
            .as_ref()
            .expect("compensation receipt");
        assert_eq!(compensation.status, FleetMutationCompensationStatus::Failed);
    }

    #[test]
    fn duplicate_step_ids_are_rejected_before_execution() {
        let plan = FleetMutationPlan::new(
            "plan-dup",
            false,
            vec![spawn_step("same"), stop_step("same", 1)],
        );
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let err = ledger
            .execute_plan(&plan, &mut executor)
            .expect_err("duplicate step ids should fail");

        assert!(matches!(
            err,
            FleetMutationPlanError::DuplicateStepId { .. }
        ));
        assert!(executor.calls.is_empty());
    }

    #[test]
    fn receipt_roundtrip_preserves_resume_evidence() {
        let plan = FleetMutationPlan::new("plan-roundtrip", false, vec![spawn_step("spawn-1")]);
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();
        let receipt = ledger.execute_plan(&plan, &mut executor).expect("receipt");

        let encoded = serde_json::to_string(&receipt).expect("encode receipt");
        let decoded: FleetMutationReceipt =
            serde_json::from_str(&encoded).expect("decode receipt");

        assert_eq!(decoded, receipt);
        assert!(receipt_has_no_orphan_compensation(&decoded));
    }
}
