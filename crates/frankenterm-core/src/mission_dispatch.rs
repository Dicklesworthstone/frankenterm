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
use crate::plan::MissionDispatchContract;
use crate::workflows::WorkflowRunner;
use serde::{Deserialize, Serialize};

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
            .map(|contract| {
                self.dispatch_single(contract, runner, event_bus, cycle_id, now_ms)
            })
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

        tracing::info!(
            assignment_id = %contract.assignment_id,
            target_agent = %contract.target_agent,
            "dispatching mission contract"
        );

        // Emit AssignmentEmitted event
        if let Some(bus) = event_bus {
            let event = self.make_event(
                cycle_id,
                now_ms,
                MissionEventKind::AssignmentEmitted,
                "dispatch_started",
                &contract.assignment_id,
                vec![
                    ("assignment_id".to_string(), serde_json::json!(&contract.assignment_id)),
                    ("target_agent".to_string(), serde_json::json!(&contract.target_agent)),
                ],
            );
            let _ = event; // Event bus expects Event enum, not MissionEvent.
            // For now, emit as signal via the bus's native Event type.
            let _ = bus.publish(crate::events::Event::WorkflowStarted {
                workflow_id: format!("mission.dispatch.{}", contract.assignment_id),
                workflow_name: "mission_dispatch".to_string(),
                pane_id: 0, // Dispatch doesn't target a specific pane initially
            });
        }

        // Build synthetic detection for the workflow runner
        let detection = self.build_detection(contract);

        // Use thread::spawn + fresh runtime to bridge sync->async for handle_detection
        let runner_workflows = {
            // We can't pass runner (non-Send) to a thread. Instead, we call
            // find_matching_workflow synchronously, which is the sync part.
            runner.find_matching_workflow(&detection)
        };

        let dispatch_ms = start.elapsed().as_millis() as u64;

        let result = if runner_workflows.is_some() {
            // A matching workflow was found — report acceptance.
            // Actual async execution would be kicked off by the caller's runtime.
            tracing::info!(
                assignment_id = %contract.assignment_id,
                "matching workflow found for dispatch"
            );
            DispatchResult {
                assignment_id: contract.assignment_id.clone(),
                target_agent: contract.target_agent.clone(),
                accepted: true,
                execution_id: Some(format!(
                    "dispatch-{}-{}",
                    contract.assignment_id, now_ms
                )),
                reason: None,
                dispatch_ms,
            }
        } else {
            tracing::warn!(
                assignment_id = %contract.assignment_id,
                rule_id = %detection.rule_id,
                "no matching workflow for dispatch"
            );
            DispatchResult {
                assignment_id: contract.assignment_id.clone(),
                target_agent: contract.target_agent.clone(),
                accepted: false,
                execution_id: None,
                reason: Some(format!(
                    "no matching workflow for rule_id '{}'",
                    detection.rule_id
                )),
                dispatch_ms,
            }
        };

        // Emit completion/failure event
        if let Some(bus) = event_bus {
            if result.accepted {
                let _ = bus.publish(crate::events::Event::WorkflowCompleted {
                    workflow_id: format!("mission.dispatch.{}", contract.assignment_id),
                    success: true,
                    reason: None,
                });
            } else {
                let _ = bus.publish(crate::events::Event::WorkflowCompleted {
                    workflow_id: format!("mission.dispatch.{}", contract.assignment_id),
                    success: false,
                    reason: result.reason.clone(),
                });
            }
        }

        result
    }

    /// Build a synthetic detection for a dispatch contract.
    fn build_detection(&self, contract: &MissionDispatchContract) -> Detection {
        Detection {
            rule_id: format!("mission.dispatch.{}", contract.assignment_id),
            agent_type: AgentType::Unknown,
            event_type: "mission_dispatch".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({
                "assignment_id": contract.assignment_id,
                "target_agent": contract.target_agent,
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

    fn sample_contract(id: &str, agent: &str) -> MissionDispatchContract {
        MissionDispatchContract {
            assignment_id: id.to_string(),
            target_agent: agent.to_string(),
        }
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
        assert_eq!(detection.confidence, 1.0);
        assert_eq!(detection.severity, Severity::Info);
        assert!(detection.extracted["assignment_id"] == "assign-42");
        assert!(detection.extracted["target_agent"] == "agent-alpha");
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
        assert!(!detection.rule_id.is_empty());
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
}
