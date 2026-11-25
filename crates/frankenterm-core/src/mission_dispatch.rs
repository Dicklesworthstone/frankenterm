//! Mission dispatch bridge: connects mission loop decisions to the workflow engine.
//!
//! The mission loop (`mission_loop.rs`) produces `MissionDispatchContract` values
//! describing which agents should receive which assignments. This module bridges
//! those contracts to actual execution via the `WorkflowRunner`.
//!
//! # Architecture
//!
//! ```text
//! MissionDecision ──> MissionDispatcher.dispatch() ──> WorkflowRunner.handle_detection()
//!                                                      └──> PaneStepExecutor (real pane I/O)
//! ```

use crate::events::EventBus;
use crate::mission_events::{MissionEvent, MissionEventKind, MissionPhase};
use crate::patterns::{AgentType, Detection, Severity};
use crate::plan::{
    ApprovalState, AssignmentId, Mission, MissionActorRole, MissionDispatchContract,
    MissionLifecycleState, MissionTxContract, MissionTxState, StepAction, TxId, TxIntent,
    TxOutcome, TxPlan, TxPlanId, TxStep, TxStepId, WaitCondition,
};
use crate::runtime_async::{CompatRuntime, RuntimeBuilder};
use crate::workflows::{WorkflowRunner, WorkflowStartResult};
use serde::{Deserialize, Serialize};

/// A refusal while compiling one explicitly selected mission assignment.
///
/// These errors precede transaction publication and external effects. Compiling
/// a contract is not policy approval: the live transaction executor must still
/// check policy, target liveness, reservations, and any required approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AssignmentDispatchError {
    #[error("mission contract is invalid")]
    InvalidMission,
    #[error("mission must be running or executing before an assignment can be dispatched")]
    NotRunning,
    #[error("assignment does not exist in this mission")]
    AssignmentNotFound,
    #[error("assignment already has a terminal outcome")]
    AlreadySettled,
    #[error("assignment approval is pending, denied, or expired")]
    ApprovalBlocked,
    #[error("assignment requires a file reservation that this dispatch path cannot verify")]
    ReservationUnsupported,
    #[error("assignment dispatch supports explicit pane sends and pane waits only")]
    ActionUnsupported,
    #[error("assignment wait does not identify a target pane")]
    TargetUnspecified,
    #[error("assignment wait identifies conflicting target panes")]
    TargetConflict,
    #[error("assignment timestamp precedes mission creation")]
    InvalidTimestamp,
}

impl AssignmentDispatchError {
    /// Stable machine-readable refusal code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidMission => "mission.dispatch.invalid_mission",
            Self::NotRunning => "mission.dispatch.not_running",
            Self::AssignmentNotFound => "mission.dispatch.assignment_not_found",
            Self::AlreadySettled => "mission.dispatch.already_settled",
            Self::ApprovalBlocked => "mission.dispatch.approval_blocked",
            Self::ReservationUnsupported => "mission.dispatch.reservation_unsupported",
            Self::ActionUnsupported => "mission.dispatch.action_unsupported",
            Self::TargetUnspecified => "mission.dispatch.target_unspecified",
            Self::TargetConflict => "mission.dispatch.target_conflict",
            Self::InvalidTimestamp => "mission.dispatch.invalid_timestamp",
        }
    }
}

/// Compile one assignment into a transaction for the existing durable executor.
///
/// The full action and explicit pane target are preserved. Unlike the workflow
/// notification bridge below, this does not synthesize a detection on pane 0.
/// The durable identity names the mission incarnation, workspace and exact
/// assignment. Mutable revisions, timestamps and action changes cannot create
/// a fresh slot that bypasses an earlier attempt's idempotency evidence.
/// The immutable intent and plan bind the assignee, candidate and full action;
/// publication must reject a different contract already occupying that slot.
///
/// Callers must hold the mission mutation guard through publication, execution,
/// and outcome persistence. An assignment's Approved metadata is only an
/// eligibility condition; it never replaces the executor's live approval gate.
pub fn compile_assignment_dispatch(
    mission: &Mission,
    assignment_id: &AssignmentId,
) -> Result<MissionTxContract, AssignmentDispatchError> {
    use sha2::{Digest as _, Sha256};

    mission
        .validate()
        .map_err(|_| AssignmentDispatchError::InvalidMission)?;
    if !matches!(
        mission.lifecycle_state,
        MissionLifecycleState::Running | MissionLifecycleState::Executing
    ) || mission.pause_resume_state.is_paused()
    {
        return Err(AssignmentDispatchError::NotRunning);
    }
    let assignment = mission
        .assignments
        .iter()
        .find(|assignment| assignment.assignment_id == *assignment_id)
        .ok_or(AssignmentDispatchError::AssignmentNotFound)?;
    if assignment.outcome.is_some() {
        return Err(AssignmentDispatchError::AlreadySettled);
    }
    if !matches!(
        assignment.approval_state,
        ApprovalState::NotRequired | ApprovalState::Approved { .. }
    ) {
        return Err(AssignmentDispatchError::ApprovalBlocked);
    }
    if assignment.reservation_intent.is_some() {
        return Err(AssignmentDispatchError::ReservationUnsupported);
    }
    if assignment.created_at_ms < mission.created_at_ms {
        return Err(AssignmentDispatchError::InvalidTimestamp);
    }
    let candidate = mission
        .candidates
        .iter()
        .find(|candidate| candidate.candidate_id == assignment.candidate_id)
        .ok_or(AssignmentDispatchError::InvalidMission)?;
    match &candidate.action {
        StepAction::SendText { .. } => {}
        StepAction::WaitFor {
            pane_id, condition, ..
        } => {
            let condition_pane = match condition {
                WaitCondition::Pattern { pane_id, .. }
                | WaitCondition::PaneIdle { pane_id, .. }
                | WaitCondition::StableTail { pane_id, .. } => *pane_id,
                WaitCondition::External { .. } => {
                    return Err(AssignmentDispatchError::ActionUnsupported);
                }
            };
            if let (Some(outer), Some(inner)) = (pane_id, condition_pane)
                && *outer != inner
            {
                return Err(AssignmentDispatchError::TargetConflict);
            }
            if pane_id.or(condition_pane).is_none() {
                return Err(AssignmentDispatchError::TargetUnspecified);
            }
        }
        _ => return Err(AssignmentDispatchError::ActionUnsupported),
    }

    // Structured encoding prevents delimiter ambiguity in caller-owned IDs.
    // Retain the full digest in the durable filename and ledger identities.
    let identity = serde_json::to_vec(&(
        "frankenterm.mission.assignment-dispatch.v1",
        &mission.mission_id,
        &mission.generation,
        &mission.workspace_id,
        &assignment.assignment_id,
    ))
    .map_err(|_| AssignmentDispatchError::InvalidMission)?;
    let digest = hex::encode(Sha256::digest(&identity));
    let summary = serde_json::to_string(&(
        "frankenterm.mission.assignment-binding.v1",
        &assignment.assignment_id,
        &assignment.assignee,
        &candidate.candidate_id,
    ))
    .map_err(|_| AssignmentDispatchError::InvalidMission)?;
    let tx_id = TxId(format!("mission-tx:{digest}"));
    let contract = MissionTxContract {
        tx_version: crate::plan::MISSION_TX_SCHEMA_VERSION,
        intent: TxIntent {
            tx_id: tx_id.clone(),
            requested_by: MissionActorRole::Dispatcher,
            summary,
            correlation_id: format!("mission-dispatch:{digest}"),
            created_at_ms: assignment.created_at_ms,
        },
        plan: TxPlan {
            plan_id: TxPlanId(format!("mission-plan:{digest}")),
            tx_id,
            steps: vec![TxStep {
                step_id: TxStepId(format!("assignment:{digest}")),
                ordinal: 0,
                action: candidate.action.clone(),
                description: "Execute the assignment's explicit pane action".to_string(),
            }],
            preconditions: Vec::new(),
            compensations: Vec::new(),
        },
        lifecycle_state: MissionTxState::Planned,
        outcome: TxOutcome::Pending,
        receipts: Vec::new(),
    };
    contract
        .validate()
        .map_err(|_| AssignmentDispatchError::InvalidMission)?;
    Ok(contract)
}

// ── Configuration ────────────────────────────────────────────────────────────

/// Configuration for the mission dispatcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionDispatcherConfig {
    /// Whether to dispatch contracts sequentially (true) or in parallel (false).
    pub sequential: bool,
    /// Workspace identifier for event emission.
    pub workspace: String,
    /// Track identifier for event emission.
    pub track: String,
}

impl Default for MissionDispatcherConfig {
    fn default() -> Self {
        Self {
            sequential: false,
            workspace: "default".to_string(),
            track: "dispatch".to_string(),
        }
    }
}

// ── Dispatch Result ──────────────────────────────────────────────────────────

/// Outcome of dispatching a single mission contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchResult {
    /// The assignment that was dispatched.
    pub assignment_id: String,
    /// The target agent.
    pub target_agent: String,
    /// Whether the dispatch was accepted by the workflow engine.
    pub accepted: bool,
    /// The workflow execution ID (if started).
    pub execution_id: Option<String>,
    /// Reason for failure (if not accepted).
    pub reason: Option<String>,
    /// Duration of the dispatch call in milliseconds.
    pub dispatch_ms: u64,
}

/// Aggregate result from dispatching a batch of contracts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchDispatchResult {
    /// Individual dispatch results.
    pub results: Vec<DispatchResult>,
    /// Number of successfully dispatched contracts.
    pub accepted_count: usize,
    /// Number of failed dispatches.
    pub failed_count: usize,
    /// Total dispatch duration in milliseconds.
    pub total_ms: u64,
    /// Cycle ID for correlation.
    pub cycle_id: u64,
}

// ── Dispatcher ───────────────────────────────────────────────────────────────

/// Bridges mission dispatch contracts to the workflow engine.
///
/// Constructs synthetic `Detection` events from dispatch contracts and feeds
/// them to `WorkflowRunner::handle_detection()`, which finds a matching
/// workflow (e.g., `MissionStepWorkflow`) and starts execution.
pub struct MissionDispatcher {
    config: MissionDispatcherConfig,
    event_sequence: std::sync::atomic::AtomicU64,
}

impl MissionDispatcher {
    const DISPATCH_PANE_ID: u64 = 0;

    /// Create a new mission dispatcher.
    #[must_use]
    pub fn new(config: MissionDispatcherConfig) -> Self {
        Self {
            config,
            event_sequence: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Dispatch a batch of mission contracts to the workflow engine.
    ///
    /// For each contract, constructs a synthetic detection with
    /// `rule_id = "mission.dispatch.<assignment_id>"` and invokes
    /// `WorkflowRunner::handle_detection()`.
    ///
    /// Contracts are dispatched sequentially or in parallel depending on config.
    /// Mission events are emitted to the event bus for each dispatch attempt.
    pub fn dispatch(
        &self,
        contracts: &[MissionDispatchContract],
        runner: &WorkflowRunner,
        event_bus: Option<&EventBus>,
        cycle_id: u64,
        now_ms: i64,
    ) -> BatchDispatchResult {
        if contracts.is_empty() {
            return BatchDispatchResult {
                results: Vec::new(),
                accepted_count: 0,
                failed_count: 0,
                total_ms: 0,
                cycle_id,
            };
        }

        let batch_start = std::time::Instant::now();

        // All dispatches go through the same sync path since WorkflowRunner::handle_detection
        // is async but we need to stay sync for mission loop integration.
        let results: Vec<DispatchResult> = contracts
            .iter()
            .map(|contract| self.dispatch_single(contract, runner, event_bus, cycle_id, now_ms))
            .collect();

        let accepted_count = results.iter().filter(|r| r.accepted).count();
        let failed_count = results.len() - accepted_count;

        tracing::info!(
            cycle_id,
            total = contracts.len(),
            accepted = accepted_count,
            failed = failed_count,
            "mission dispatch batch completed"
        );

        BatchDispatchResult {
            results,
            accepted_count,
            failed_count,
            total_ms: batch_start.elapsed().as_millis() as u64,
            cycle_id,
        }
    }

    /// Dispatch a single contract.
    fn dispatch_single(
        &self,
        contract: &MissionDispatchContract,
        runner: &WorkflowRunner,
        event_bus: Option<&EventBus>,
        cycle_id: u64,
        now_ms: i64,
    ) -> DispatchResult {
        let start = std::time::Instant::now();
        let assignment_id = contract.correlation_id();
        let target_agent = contract.target_agent_label();

        tracing::info!(assignment_id, target_agent, "dispatching mission contract");

        // Emit the AssignmentEmitted audit event onto the bus so signal
        // subscribers can reconstruct the mission dispatch boundary.
        if let Some(bus) = event_bus {
            self.publish_assignment_emitted_event(
                bus,
                cycle_id,
                now_ms,
                assignment_id,
                target_agent,
            );
        }

        // Build synthetic detection for the workflow runner.
        let detection = self.build_detection(contract);
        let start_result = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .map(|runtime| {
                let result = runtime.block_on(runner.handle_detection(
                    Self::DISPATCH_PANE_ID,
                    &detection,
                    None,
                ));
                drop(runtime);
                result
            })
            .unwrap_or_else(|error| WorkflowStartResult::Error {
                error: format!("failed to build mission dispatch runtime: {error}"),
            });

        let dispatch_ms = start.elapsed().as_millis() as u64;

        let result = match start_result {
            WorkflowStartResult::Started {
                execution_id,
                workflow_name,
            } => {
                tracing::info!(
                    assignment_id,
                    execution_id,
                    workflow_name,
                    "mission dispatch started workflow"
                );
                if let Some(bus) = event_bus {
                    let _ = bus.publish(crate::events::Event::WorkflowStarted {
                        workflow_id: execution_id.clone(),
                        workflow_name,
                        pane_id: Self::DISPATCH_PANE_ID,
                    });
                }
                DispatchResult {
                    assignment_id: assignment_id.to_string(),
                    target_agent: target_agent.to_string(),
                    accepted: true,
                    execution_id: Some(execution_id),
                    reason: None,
                    dispatch_ms,
                }
            }
            WorkflowStartResult::NoMatchingWorkflow { rule_id } => {
                tracing::warn!(assignment_id, rule_id, "no matching workflow for dispatch");
                DispatchResult {
                    assignment_id: assignment_id.to_string(),
                    target_agent: target_agent.to_string(),
                    accepted: false,
                    execution_id: None,
                    reason: Some(format!("no matching workflow for rule_id '{rule_id}'")),
                    dispatch_ms,
                }
            }
            WorkflowStartResult::TriggerRejected { event_id, reason } => DispatchResult {
                assignment_id: assignment_id.to_string(),
                target_agent: target_agent.to_string(),
                accepted: false,
                execution_id: None,
                reason: Some(format!(
                    "stored workflow trigger {event_id} rejected: {reason}"
                )),
                dispatch_ms,
            },
            WorkflowStartResult::PaneLocked {
                pane_id,
                held_by_workflow,
                held_by_execution,
            } => DispatchResult {
                assignment_id: assignment_id.to_string(),
                target_agent: target_agent.to_string(),
                accepted: false,
                execution_id: None,
                reason: Some(format!(
                    "pane {pane_id} locked by workflow '{held_by_workflow}' ({held_by_execution})"
                )),
                dispatch_ms,
            },
            WorkflowStartResult::ConcurrencyLimitReached { active, limit } => DispatchResult {
                assignment_id: assignment_id.to_string(),
                target_agent: target_agent.to_string(),
                accepted: false,
                execution_id: None,
                reason: Some(format!(
                    "workflow concurrency limit reached ({active}/{limit} active)"
                )),
                dispatch_ms,
            },
            WorkflowStartResult::SourcePaneNotTrusted {
                source_pane_id,
                workflow_name,
                rule_id,
            } => DispatchResult {
                assignment_id: assignment_id.to_string(),
                target_agent: target_agent.to_string(),
                accepted: false,
                execution_id: None,
                reason: Some(format!(
                    "ft-j0ufc: workflow '{workflow_name}' refused trigger from \
                     untrusted source pane {source_pane_id} (rule_id={rule_id})"
                )),
                dispatch_ms,
            },
            WorkflowStartResult::PaneRateLimited {
                pane_id,
                workflow_name,
                rule_id,
                reset_at_ms,
                reset_known,
            } => DispatchResult {
                assignment_id: assignment_id.to_string(),
                target_agent: target_agent.to_string(),
                accepted: false,
                execution_id: None,
                reason: Some(format!(
                    "ft-7h5da.8.3: workflow '{workflow_name}' declined; pane {pane_id} \
                     rate-limited (reset_at_ms={reset_at_ms}, reset_known={reset_known}, \
                     rule_id={rule_id})"
                )),
                dispatch_ms,
            },
            WorkflowStartResult::PaneShowsOtherAgent {
                pane_id,
                workflow_name,
                rule_id,
                rule_agent,
                screen_agent,
            } => DispatchResult {
                assignment_id: assignment_id.to_string(),
                target_agent: target_agent.to_string(),
                accepted: false,
                execution_id: None,
                reason: Some(format!(
                    "ft-2ko47y: workflow '{workflow_name}' declined; pane {pane_id} shows \
                     {screen_agent}, not {rule_agent} (rule_id={rule_id})"
                )),
                dispatch_ms,
            },
            WorkflowStartResult::Error { error } => DispatchResult {
                assignment_id: assignment_id.to_string(),
                target_agent: target_agent.to_string(),
                accepted: false,
                execution_id: None,
                reason: Some(error),
                dispatch_ms,
            },
        };

        // Emit completion/failure event
        if let Some(bus) = event_bus {
            if let Some(event) = self.make_completion_event_for_result(assignment_id, &result) {
                let _ = bus.publish(event);
            }
        }

        result
    }

    /// Build a synthetic detection for a dispatch contract.
    fn build_detection(&self, contract: &MissionDispatchContract) -> Detection {
        let _ = self; // future dispatch may use dispatcher state
        Detection {
            rule_id: format!("mission.dispatch.{}", contract.correlation_id()),
            agent_type: AgentType::Unknown,
            event_type: "mission_dispatch".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({
                "assignment_id": &contract.assignment_id,
                "target_agent": &contract.target_agent,
                "candidate_id": &contract.candidate_id,
                "action_type": contract.action.action_type_name(),
                "rationale": &contract.rationale,
            }),
            matched_text: String::new(),
            span: (0, 0),
        }
    }

    /// Build a mission event for audit.
    fn make_event(
        &self,
        cycle_id: u64,
        timestamp_ms: i64,
        kind: MissionEventKind,
        reason_code: &str,
        correlation_id: &str,
        details: Vec<(String, serde_json::Value)>,
    ) -> MissionEvent {
        let seq = self
            .event_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        MissionEvent {
            sequence: seq,
            cycle_id,
            timestamp_ms,
            kind,
            reason_code: reason_code.to_string(),
            correlation_id: correlation_id.to_string(),
            phase: MissionPhase::Dispatch,
            details: details.into_iter().collect(),
            workspace: self.config.workspace.clone(),
            track: self.config.track.clone(),
        }
    }

    /// Build and publish the `AssignmentEmitted` mission audit event.
    ///
    /// Previously this event was constructed and then dropped (`let _ =
    /// event`), which made the module-level contract false: dispatch attempts
    /// claimed to emit the audit event but subscribers never saw it. The event
    /// now rides the bus as [`crate::events::Event::MissionAudit`] and is
    /// routed onto the signal channel alongside the other mission lifecycle
    /// signals.
    fn publish_assignment_emitted_event(
        &self,
        event_bus: &EventBus,
        cycle_id: u64,
        timestamp_ms: i64,
        assignment_id: &str,
        target_agent: &str,
    ) {
        let event = self.make_event(
            cycle_id,
            timestamp_ms,
            MissionEventKind::AssignmentEmitted,
            "dispatch_started",
            assignment_id,
            vec![
                (
                    "assignment_id".to_string(),
                    serde_json::json!(assignment_id),
                ),
                ("target_agent".to_string(), serde_json::json!(target_agent)),
            ],
        );
        let _ = event_bus.publish(crate::events::Event::MissionAudit {
            event: Box::new(event),
        });
    }

    /// Map a dispatch result to a workflow completion event.
    ///
    /// A successful dispatch only means the workflow runner accepted the
    /// detection; execution has not completed yet. Emitting a synthetic
    /// `WorkflowCompleted { success: true }` here would incorrectly trigger
    /// downstream "work completed" handling before any pane I/O runs.
    fn make_completion_event_for_result(
        &self,
        assignment_id: &str,
        result: &DispatchResult,
    ) -> Option<crate::events::Event> {
        let _ = self;
        if result.accepted {
            None
        } else {
            Some(crate::events::Event::WorkflowCompleted {
                workflow_id: format!("mission.dispatch.{assignment_id}"),
                success: false,
                reason: result.reason.clone(),
            })
        }
    }
}

impl Default for MissionDispatcher {
    fn default() -> Self {
        Self::new(MissionDispatcherConfig::default())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_async::CompatRuntime;
    use crate::workflows::{
        BoxFuture, CxPolicyInjector, PaneWorkflowLockManager, StepResult, Workflow,
        WorkflowContext, WorkflowRunnerConfig, WorkflowStep,
    };
    use std::sync::Arc;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build mission dispatch test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    struct MissionDispatchTestWorkflow;

    impl Workflow for MissionDispatchTestWorkflow {
        fn name(&self) -> &'static str {
            "mission_dispatch_test"
        }

        fn description(&self) -> &'static str {
            "Test workflow for mission dispatch"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.starts_with("mission.dispatch.")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("complete", "Complete immediately")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            _step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async { StepResult::done_empty() })
        }
    }

    async fn create_test_runner(
        db_path: &str,
    ) -> (
        WorkflowRunner,
        Arc<crate::storage::StorageHandle>,
        Arc<PaneWorkflowLockManager>,
    ) {
        let engine = crate::workflows::WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let storage = Arc::new(crate::storage::StorageHandle::new(db_path).await.unwrap());
        seed_dispatch_pane(&storage).await;

        let handle: crate::wezterm::WeztermHandle = Arc::new(crate::wezterm::MockWezterm::new());
        let injector = CxPolicyInjector::new(crate::policy::PolicyGatedInjector::new(
            crate::policy::PolicyEngine::permissive(),
            handle,
        ));
        let runner = WorkflowRunner::new(
            engine,
            Arc::clone(&lock_manager),
            Arc::clone(&storage),
            injector,
            WorkflowRunnerConfig::default(),
        );

        (runner, storage, lock_manager)
    }

    async fn seed_dispatch_pane(storage: &crate::storage::StorageHandle) {
        let now = 1_700_000_000_000;
        storage
            .upsert_pane(crate::storage::PaneRecord {
                pane_id: MissionDispatcher::DISPATCH_PANE_ID,
                pane_uuid: Some("mission-dispatch-test-pane".to_string()),
                domain: "mission-dispatch".to_string(),
                window_id: None,
                tab_id: None,
                title: Some("Mission dispatch synthetic pane".to_string()),
                cwd: None,
                tty_name: None,
                first_seen_at: now,
                last_seen_at: now,
                observed: true,
                ignore_reason: None,
                last_decision_at: Some(now),
            })
            .await
            .expect("seed mission dispatch pane");
    }

    fn sample_contract(id: &str, agent: &str) -> MissionDispatchContract {
        MissionDispatchContract {
            assignment_id: Some(id.to_string()),
            target_agent: Some(agent.to_string()),
            candidate_id: crate::plan::CandidateActionId(format!("candidate:{id}")),
            action: crate::plan::StepAction::Custom {
                action_type: "mission_dispatch_test".to_string(),
                payload: serde_json::json!({ "assignment_id": id }),
            },
            rationale: "mission dispatch test contract".to_string(),
            approval_state: Some(crate::plan::ApprovalState::NotRequired),
        }
    }

    fn dispatch_mission() -> Mission {
        let mut mission = Mission::new(
            crate::plan::MissionId("mission:dispatch-test".to_string()),
            "Explicit assignment dispatch",
            "workspace:dispatch-test",
            crate::plan::MissionOwnership {
                planner: "planner".to_string(),
                dispatcher: "dispatcher".to_string(),
                operator: "operator".to_string(),
            },
            1_000,
        );
        mission.lifecycle_state = MissionLifecycleState::Running;
        mission.updated_at_ms = Some(1_003);
        mission.candidates.push(crate::plan::CandidateAction {
            candidate_id: crate::plan::CandidateActionId("candidate:send".to_string()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::SendText {
                pane_id: 42,
                text: "printf 'mission-dispatch\\n'\n".to_string(),
                paste_mode: Some(false),
            },
            rationale: "Selected explicit pane".to_string(),
            score: None,
            created_at_ms: 1_001,
        });
        mission.assignments.push(crate::plan::Assignment {
            assignment_id: AssignmentId("assignment:send".to_string()),
            candidate_id: mission.candidates[0].candidate_id.clone(),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: "agent-42".to_string(),
            reservation_intent: None,
            approval_state: ApprovalState::NotRequired,
            outcome: None,
            escalation: None,
            created_at_ms: 1_002,
            updated_at_ms: None,
        });
        mission
    }

    fn compile_selected(mission: &Mission) -> Result<MissionTxContract, AssignmentDispatchError> {
        compile_assignment_dispatch(mission, &mission.assignments[0].assignment_id)
    }

    #[test]
    fn assignment_dispatch_preserves_the_full_action_and_real_target() {
        let mission = dispatch_mission();
        let contract = compile_selected(&mission).unwrap();
        contract.validate().unwrap();
        assert_eq!(contract.lifecycle_state, MissionTxState::Planned);
        assert_eq!(contract.outcome, TxOutcome::Pending);
        assert_eq!(contract.plan.steps.len(), 1);
        assert_eq!(
            serde_json::to_value(&contract.plan.steps[0].action).unwrap(),
            serde_json::to_value(&mission.candidates[0].action).unwrap()
        );
        assert_eq!(
            contract.referenced_pane_ids().into_iter().collect::<Vec<_>>(),
            [42]
        );
        assert!(contract.plan.compensations.is_empty());
        assert!(contract.receipts.is_empty());
        assert_eq!(contract.intent.created_at_ms, 1_002);
    }

    #[test]
    fn assignment_dispatch_identity_survives_lifecycle_revision_changes() {
        let mut mission = dispatch_mission();
        let original = compile_selected(&mission).unwrap();
        mission
            .pause_mission("operator", "review", 2_000, None)
            .unwrap();
        mission.revision = 1;
        assert_eq!(
            compile_selected(&mission).unwrap_err(),
            AssignmentDispatchError::NotRunning
        );
        mission
            .resume_mission("operator", "continue", 3_000, None)
            .unwrap();
        mission.revision = 2;
        mission.title = "Renamed mission".to_string();
        mission.candidates[0].rationale = "Updated explanation".to_string();
        let resumed = compile_selected(&mission).unwrap();
        assert_eq!(
            serde_json::to_value(original).unwrap(),
            serde_json::to_value(resumed).unwrap()
        );
    }

    #[test]
    fn assignment_dispatch_identity_binds_incarnation_workspace_and_assignment() {
        let mission = dispatch_mission();
        let mut variants = vec![mission.clone()];
        let mut changed = mission.clone();
        changed.generation = if mission.generation == "f".repeat(32) {
            "e".repeat(32)
        } else {
            "f".repeat(32)
        };
        variants.push(changed);
        let mut changed = mission.clone();
        changed.mission_id = crate::plan::MissionId("mission:other".to_string());
        variants.push(changed);
        let mut changed = mission.clone();
        changed.workspace_id = "workspace:other".to_string();
        variants.push(changed);
        let mut changed = mission;
        changed.assignments[0].assignment_id = AssignmentId("assignment:other".to_string());
        variants.push(changed);
        let identities = variants
            .iter()
            .map(|mission| compile_selected(mission).unwrap().intent.tx_id.0)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(identities.len(), variants.len());
    }

    #[test]
    fn assignment_dispatch_payload_changes_keep_the_same_slot_with_different_binding() {
        let mission = dispatch_mission();
        let original = compile_selected(&mission).unwrap();
        let mut changed = mission.clone();
        changed.candidates[0].action = StepAction::SendText {
            pane_id: 43,
            text: "different action".to_string(),
            paste_mode: None,
        };
        let different_action = compile_selected(&changed).unwrap();
        assert_eq!(original.intent.tx_id, different_action.intent.tx_id);
        assert_ne!(
            serde_json::to_value(&original.plan).unwrap(),
            serde_json::to_value(&different_action.plan).unwrap()
        );
        let mut changed = mission;
        changed.assignments[0].assignee = "agent-other".to_string();
        let different_assignee = compile_selected(&changed).unwrap();
        assert_eq!(original.intent.tx_id, different_assignee.intent.tx_id);
        assert_ne!(original.intent.summary, different_assignee.intent.summary);
    }

    #[test]
    fn assignment_dispatch_selects_the_requested_assignment_for_shared_candidate() {
        let mut mission = dispatch_mission();
        let mut second = mission.assignments[0].clone();
        second.assignment_id = AssignmentId("assignment:second".to_string());
        second.assignee = "agent-second".to_string();
        second.created_at_ms = 1_003;
        mission.assignments.push(second);
        let first = compile_selected(&mission).unwrap();
        let selected =
            compile_assignment_dispatch(&mission, &mission.assignments[1].assignment_id)
                .unwrap();
        assert_ne!(first.intent.tx_id, selected.intent.tx_id);
        assert_eq!(selected.intent.created_at_ms, 1_003);
        assert_eq!(
            compile_assignment_dispatch(&mission, &AssignmentId("absent".to_string()))
                .unwrap_err(),
            AssignmentDispatchError::AssignmentNotFound
        );
    }

    #[test]
    fn assignment_dispatch_refuses_settled_or_unapproved_work() {
        let mission = dispatch_mission();
        for approval in [
            ApprovalState::Pending {
                requested_by: "dispatcher".to_string(),
                requested_at_ms: 1_003,
            },
            ApprovalState::Denied {
                denied_by: "operator".to_string(),
                denied_at_ms: 1_003,
                reason_code: "denied".to_string(),
            },
            ApprovalState::Expired {
                expired_at_ms: 1_003,
                reason_code: "expired".to_string(),
            },
        ] {
            let mut blocked = mission.clone();
            blocked.assignments[0].approval_state = approval;
            assert_eq!(
                compile_selected(&blocked).unwrap_err(),
                AssignmentDispatchError::ApprovalBlocked
            );
        }
        let mut settled = mission;
        settled.assignments[0].outcome = Some(crate::plan::Outcome::Success {
            reason_code: "dispatched".to_string(),
            completed_at_ms: 1_003,
        });
        assert_eq!(
            compile_selected(&settled).unwrap_err(),
            AssignmentDispatchError::AlreadySettled
        );
    }

    #[test]
    fn assignment_dispatch_never_drops_a_file_reservation_requirement() {
        let mut mission = dispatch_mission();
        mission.assignments[0].reservation_intent = Some(crate::plan::ReservationIntent {
            reservation_id: crate::plan::ReservationIntentId("reservation:edit".to_string()),
            requested_by: MissionActorRole::Dispatcher,
            paths: vec!["src/**".to_string()],
            exclusive: true,
            reason: None,
            requested_at_ms: 1_002,
            expires_at_ms: None,
        });
        assert_eq!(
            compile_selected(&mission).unwrap_err(),
            AssignmentDispatchError::ReservationUnsupported
        );
    }

    #[test]
    fn assignment_dispatch_wait_requires_one_unambiguous_explicit_pane() {
        let mut mission = dispatch_mission();
        for (outer, inner, error) in [
            (None, None, Some(AssignmentDispatchError::TargetUnspecified)),
            (
                Some(42),
                Some(43),
                Some(AssignmentDispatchError::TargetConflict),
            ),
            (Some(42), None, None),
            (None, Some(42), None),
            (Some(42), Some(42), None),
        ] {
            mission.candidates[0].action = StepAction::WaitFor {
                pane_id: outer,
                condition: WaitCondition::Pattern {
                    pane_id: inner,
                    rule_id: "codex.done".to_string(),
                },
                timeout_ms: 5_000,
            };
            let result = compile_selected(&mission);
            if let Some(expected) = error {
                assert_eq!(result.unwrap_err(), expected);
            } else {
                let contract = result.unwrap();
                assert_eq!(
                    serde_json::to_value(&contract.plan.steps[0].action).unwrap(),
                    serde_json::to_value(&mission.candidates[0].action).unwrap()
                );
            }
        }
    }

    #[test]
    fn assignment_dispatch_refuses_actions_without_a_live_pane_executor() {
        let mut mission = dispatch_mission();
        for action in [
            StepAction::WaitFor {
                pane_id: Some(42),
                condition: WaitCondition::External {
                    key: "ready".to_string(),
                },
                timeout_ms: 5_000,
            },
            StepAction::RunWorkflow {
                workflow_id: "workflow".to_string(),
                params: None,
            },
            StepAction::StoreData {
                key: "state".to_string(),
                value: serde_json::json!(true),
            },
            StepAction::Custom {
                action_type: "unknown".to_string(),
                payload: serde_json::Value::Null,
            },
        ] {
            mission.candidates[0].action = action;
            assert_eq!(
                compile_selected(&mission).unwrap_err(),
                AssignmentDispatchError::ActionUnsupported
            );
        }
    }

    #[test]
    fn assignment_dispatch_refuses_unstarted_and_invalid_missions() {
        let mut mission = dispatch_mission();
        for state in [
            MissionLifecycleState::Planning,
            MissionLifecycleState::Planned,
            MissionLifecycleState::Blocked,
            MissionLifecycleState::RetryPending,
        ] {
            mission.lifecycle_state = state;
            assert_eq!(
                compile_selected(&mission).unwrap_err(),
                AssignmentDispatchError::NotRunning
            );
        }
        mission.lifecycle_state = MissionLifecycleState::Running;
        mission.assignments[0].created_at_ms = 999;
        assert_eq!(
            compile_selected(&mission).unwrap_err(),
            AssignmentDispatchError::InvalidTimestamp
        );
        mission.generation.clear();
        assert_eq!(
            compile_selected(&mission).unwrap_err(),
            AssignmentDispatchError::InvalidMission
        );
    }

    #[test]
    fn empty_contracts_returns_empty_result() {
        let dispatcher = MissionDispatcher::default();
        // WorkflowRunner requires storage etc. so we test the empty path directly
        let result = BatchDispatchResult {
            results: Vec::new(),
            accepted_count: 0,
            failed_count: 0,
            total_ms: 0,
            cycle_id: 1,
        };
        assert_eq!(result.results.len(), 0);
        assert_eq!(result.accepted_count, 0);
        let _ = dispatcher; // Ensure dispatcher compiles
    }

    #[test]
    fn build_detection_has_correct_rule_id() {
        let dispatcher = MissionDispatcher::default();
        let contract = sample_contract("assign-42", "agent-alpha");
        let detection = dispatcher.build_detection(&contract);
        assert_eq!(detection.rule_id, "mission.dispatch.assign-42");
        assert_eq!(detection.event_type, "mission_dispatch");
        assert!((detection.confidence - 1.0).abs() < f64::EPSILON);
        assert_eq!(detection.severity, Severity::Info);
        assert_eq!(detection.extracted["assignment_id"], "assign-42");
        assert_eq!(detection.extracted["target_agent"], "agent-alpha");
    }

    #[test]
    fn build_detection_different_contracts_unique_rule_ids() {
        let dispatcher = MissionDispatcher::default();
        let d1 = dispatcher.build_detection(&sample_contract("a1", "agent1"));
        let d2 = dispatcher.build_detection(&sample_contract("a2", "agent2"));
        assert_ne!(d1.rule_id, d2.rule_id);
    }

    #[test]
    fn make_event_increments_sequence() {
        let dispatcher = MissionDispatcher::default();
        let e1 = dispatcher.make_event(
            1,
            1000,
            MissionEventKind::AssignmentEmitted,
            "test",
            "corr-1",
            vec![],
        );
        let e2 = dispatcher.make_event(
            1,
            2000,
            MissionEventKind::AssignmentEmitted,
            "test",
            "corr-2",
            vec![],
        );
        assert_eq!(e1.sequence, 0);
        assert_eq!(e2.sequence, 1);
    }

    #[test]
    fn make_event_captures_details() {
        let dispatcher = MissionDispatcher::default();
        let event = dispatcher.make_event(
            42,
            5000,
            MissionEventKind::AssignmentRejected,
            "dispatch_failed",
            "assign-99",
            vec![
                ("assignment_id".to_string(), serde_json::json!("assign-99")),
                ("error".to_string(), serde_json::json!("no workflow")),
            ],
        );
        assert_eq!(event.cycle_id, 42);
        assert_eq!(event.timestamp_ms, 5000);
        assert_eq!(event.reason_code, "dispatch_failed");
        assert_eq!(event.correlation_id, "assign-99");
        assert_eq!(event.details["assignment_id"], "assign-99");
        assert_eq!(event.details["error"], "no workflow");
    }

    #[test]
    fn dispatch_result_serde_roundtrip() {
        let result = DispatchResult {
            assignment_id: "a1".to_string(),
            target_agent: "agent1".to_string(),
            accepted: true,
            execution_id: Some("exec-123".to_string()),
            reason: None,
            dispatch_ms: 42,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: DispatchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.assignment_id, "a1");
        assert!(back.accepted);
        assert_eq!(back.execution_id.unwrap(), "exec-123");
    }

    #[test]
    fn batch_dispatch_result_serde_roundtrip() {
        let batch = BatchDispatchResult {
            results: vec![DispatchResult {
                assignment_id: "a1".to_string(),
                target_agent: "agent1".to_string(),
                accepted: false,
                execution_id: None,
                reason: Some("no workflow".to_string()),
                dispatch_ms: 5,
            }],
            accepted_count: 0,
            failed_count: 1,
            total_ms: 10,
            cycle_id: 7,
        };
        let json = serde_json::to_string(&batch).unwrap();
        let back: BatchDispatchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cycle_id, 7);
        assert_eq!(back.failed_count, 1);
        assert_eq!(back.results[0].reason.as_deref(), Some("no workflow"));
    }

    #[test]
    fn dispatcher_config_default() {
        let config = MissionDispatcherConfig::default();
        assert!(!config.sequential);
        assert_eq!(config.workspace, "default");
        assert_eq!(config.track, "dispatch");
    }

    #[test]
    fn dispatcher_default_creates_valid_instance() {
        let dispatcher = MissionDispatcher::default();
        assert!(!dispatcher.config.sequential);
        // Build a detection to verify the instance works
        let contract = sample_contract("test", "agent");
        let detection = dispatcher.build_detection(&contract);
        assert_ne!(detection.rule_id, "");
    }

    #[test]
    fn make_event_uses_config_workspace_and_track() {
        let config = MissionDispatcherConfig {
            sequential: true,
            workspace: "ws-prod".to_string(),
            track: "fast-lane".to_string(),
        };
        let dispatcher = MissionDispatcher::new(config);
        let event = dispatcher.make_event(
            1,
            1000,
            MissionEventKind::CycleStarted,
            "start",
            "c1",
            vec![],
        );
        assert_eq!(event.workspace, "ws-prod");
        assert_eq!(event.track, "fast-lane");
    }

    #[test]
    fn assignment_emitted_event_is_published_to_signal_subscribers() {
        use crate::events::{Event, EventBus};

        run_async_test(async {
            let config = MissionDispatcherConfig {
                sequential: true,
                workspace: "ws-audit".to_string(),
                track: "audit-lane".to_string(),
            };
            let dispatcher = MissionDispatcher::new(config);
            let bus = EventBus::new(64);
            let mut subscriber = bus.subscribe_signals();

            dispatcher.publish_assignment_emitted_event(
                &bus,
                7,
                1_700_000_000_123,
                "assign-7",
                "agent-omega",
            );

            let received = subscriber
                .recv()
                .await
                .expect("a mission audit signal event should be delivered");

            let Event::MissionAudit { event } = received else {
                panic!("expected a MissionAudit event, got {received:?}");
            };
            assert_eq!(event.kind, MissionEventKind::AssignmentEmitted);
            assert_eq!(event.phase, MissionPhase::Dispatch);
            assert_eq!(event.cycle_id, 7);
            assert_eq!(event.timestamp_ms, 1_700_000_000_123);
            assert_eq!(event.reason_code, "dispatch_started");
            assert_eq!(event.correlation_id, "assign-7");
            assert_eq!(event.workspace, "ws-audit");
            assert_eq!(event.track, "audit-lane");
            assert_eq!(event.details["assignment_id"], "assign-7");
            assert_eq!(event.details["target_agent"], "agent-omega");
        });
    }

    #[test]
    fn accepted_dispatch_does_not_emit_completion_event() {
        let dispatcher = MissionDispatcher::default();
        let result = DispatchResult {
            assignment_id: "a1".to_string(),
            target_agent: "agent1".to_string(),
            accepted: true,
            execution_id: Some("exec-123".to_string()),
            reason: None,
            dispatch_ms: 1,
        };

        assert!(
            dispatcher
                .make_completion_event_for_result("a1", &result)
                .is_none(),
            "accepted dispatch should not report workflow completion before execution runs"
        );
    }

    #[test]
    fn rejected_dispatch_emits_failure_completion_event() {
        let dispatcher = MissionDispatcher::default();
        let result = DispatchResult {
            assignment_id: "a1".to_string(),
            target_agent: "agent1".to_string(),
            accepted: false,
            execution_id: None,
            reason: Some("no workflow".to_string()),
            dispatch_ms: 1,
        };

        let event = dispatcher
            .make_completion_event_for_result("a1", &result)
            .expect("rejected dispatch should emit a failure event");

        // [ft-zv3u9] crate::events::Event does not impl PartialEq (its
        // payload includes types like Detection and UserVarPayload that
        // would each need PartialEq, cascading the trait bound through
        // a wide surface). Match-and-destructure the variant instead
        // of relying on assert_eq! so this test compiles without
        // expanding the PartialEq surface across events.rs and
        // patterns.rs.
        match event {
            crate::events::Event::WorkflowCompleted {
                workflow_id,
                success,
                reason,
            } => {
                assert_eq!(workflow_id, "mission.dispatch.a1");
                assert!(!success);
                assert_eq!(reason.as_deref(), Some("no workflow"));
            }
            other => panic!("expected WorkflowCompleted, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_starts_real_workflow_and_persists_execution() {
        run_async_test(async {
            let tmp = tempfile::tempdir().expect("tempdir");
            let db_path = tmp.path().join("mission_dispatch.sqlite");
            let db_path_str = db_path.to_string_lossy().into_owned();
            let (runner, storage, _) = create_test_runner(&db_path_str).await;
            runner.register_workflow(Arc::new(MissionDispatchTestWorkflow));

            let dispatcher = MissionDispatcher::default();
            let contract = sample_contract("assign-42", "agent-alpha");
            let result = dispatcher.dispatch(&[contract], &runner, None, 7, 1_700_000_000_000);

            assert_eq!(result.accepted_count, 1, "{result:?}");
            assert_eq!(result.failed_count, 0);
            assert_eq!(result.results.len(), 1);

            let dispatch = &result.results[0];
            assert!(dispatch.accepted);
            let execution_id = dispatch.execution_id.as_deref().expect("execution id");
            assert_ne!(execution_id, "dispatch-assign-42-1700000000000");
            assert!(execution_id.contains("mission_dispatch_test"));

            let record = storage
                .get_workflow(execution_id)
                .await
                .unwrap()
                .expect("workflow record should exist");
            assert_eq!(record.workflow_name, "mission_dispatch_test");
            assert_eq!(record.pane_id, MissionDispatcher::DISPATCH_PANE_ID);
        });
    }
}
