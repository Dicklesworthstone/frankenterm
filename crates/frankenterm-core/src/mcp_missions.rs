//! Mission and transaction lifecycle helpers for MCP tools.
//!
//! Extracted from `mcp.rs` as part of Wave 4A migration (ft-1fv0u).
//! Contains file I/O, state resolution, and assignment aggregation helpers
//! used by the mission/tx tool handlers.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::mcp_error::{MCP_ERR_INVALID_ARGS, McpToolError};

use super::{
    McpMissionAssignmentCounters, McpMissionAssignmentData, McpMissionFailureCatalogEntry,
    McpMissionTransitionInfo, McpTxTransitionInfo, MissionStateParams,
};

// ── Tx contract file resolution and loading ─────────────────────────────

pub(super) fn mcp_default_mission_tx_file_path(
    config: &Config,
) -> std::result::Result<PathBuf, McpToolError> {
    let layout = config
        .workspace_layout(None)
        .map_err(McpToolError::from_error)?;
    Ok(layout.ft_dir.join("mission").join("tx-active.json"))
}

pub(super) fn mcp_resolve_mission_tx_file_path(
    config: &Config,
    contract_file: Option<&str>,
) -> std::result::Result<PathBuf, McpToolError> {
    match contract_file {
        Some(path) => Ok(PathBuf::from(path)),
        None => mcp_default_mission_tx_file_path(config),
    }
}

pub(super) fn mcp_load_mission_tx_contract_from_path(
    path: &Path,
) -> std::result::Result<crate::plan::MissionTxContract, McpToolError> {
    let raw = std::fs::read_to_string(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            McpToolError::new(
                "robot.tx_not_found",
                format!("Tx contract file not found: {}", path.display()),
                Some("Pass contract_file or create .ft/mission/tx-active.json.".to_string()),
            )
        } else {
            McpToolError::new(
                "robot.tx_read_failed",
                format!("Failed to read tx contract file {}: {err}", path.display()),
                None,
            )
        }
    })?;

    let contract: crate::plan::MissionTxContract = serde_json::from_str(&raw).map_err(|err| {
        McpToolError::new(
            "robot.tx_invalid_json",
            format!("Invalid tx contract JSON in {}: {err}", path.display()),
            Some("Ensure the file matches the MissionTxContract schema.".to_string()),
        )
    })?;

    contract.validate().map_err(|err| {
        McpToolError::new(
            "robot.tx_validation_failed",
            format!("Tx contract validation failed: {err}"),
            Some("Inspect contract via wa.tx_show include_contract=true.".to_string()),
        )
    })?;

    Ok(contract)
}

pub(super) fn mcp_save_mission_tx_contract_to_path(
    path: &Path,
    contract: &crate::plan::MissionTxContract,
) -> std::result::Result<(), McpToolError> {
    let json = serde_json::to_string_pretty(contract).map_err(|err| {
        McpToolError::new(
            "robot.tx_serialize_failed",
            format!("Failed to serialize tx contract: {err}"),
            None,
        )
    })?;

    std::fs::write(path, json).map_err(|err| {
        McpToolError::new(
            "robot.tx_write_failed",
            format!("Failed to write tx contract file {}: {err}", path.display()),
            None,
        )
    })
}

pub(super) fn mcp_parse_mission_kill_switch(
    raw: Option<&str>,
) -> std::result::Result<crate::plan::MissionKillSwitchLevel, McpToolError> {
    match raw.unwrap_or("off").trim().to_ascii_lowercase().as_str() {
        "off" => Ok(crate::plan::MissionKillSwitchLevel::Off),
        "safe_mode" | "safe-mode" | "safemode" => Ok(crate::plan::MissionKillSwitchLevel::SafeMode),
        "hard_stop" | "hard-stop" | "hardstop" => Ok(crate::plan::MissionKillSwitchLevel::HardStop),
        other => Err(McpToolError::new(
            MCP_ERR_INVALID_ARGS,
            format!("Unknown kill_switch value: {other}"),
            Some("Valid values: off, safe_mode, hard_stop.".to_string()),
        )),
    }
}

pub(super) fn mcp_tx_transition_info(
    state: crate::plan::MissionTxState,
) -> Vec<McpTxTransitionInfo> {
    crate::plan::mission_tx_transition_table()
        .iter()
        .filter(|rule| rule.from == state)
        .map(|rule| McpTxTransitionInfo {
            kind: rule.via.to_string(),
            to: rule.to.to_string(),
        })
        .collect()
}

// ── Mission (non-tx) file resolution and loading ─────────────────────────

pub(super) fn mcp_default_mission_file_path(
    config: &Config,
) -> std::result::Result<PathBuf, McpToolError> {
    let layout = config
        .workspace_layout(None)
        .map_err(McpToolError::from_error)?;
    Ok(layout.ft_dir.join("mission").join("active.json"))
}

pub(super) fn mcp_resolve_mission_file_path(
    config: &Config,
    mission_file: Option<&str>,
) -> std::result::Result<PathBuf, McpToolError> {
    match mission_file {
        Some(path) => Ok(PathBuf::from(path)),
        None => mcp_default_mission_file_path(config),
    }
}

pub(super) fn mcp_load_mission_from_path(
    path: &Path,
) -> std::result::Result<crate::plan::Mission, McpToolError> {
    let raw = std::fs::read_to_string(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            McpToolError::new(
                "robot.mission_not_found",
                format!("Mission file not found: {}", path.display()),
                Some("Pass mission_file or create .ft/mission/active.json.".to_string()),
            )
        } else {
            McpToolError::new(
                "robot.mission_read_failed",
                format!("Failed to read mission file {}: {err}", path.display()),
                None,
            )
        }
    })?;

    let mission: crate::plan::Mission = serde_json::from_str(&raw).map_err(|err| {
        McpToolError::new(
            "robot.mission_invalid_json",
            format!("Invalid mission JSON in {}: {err}", path.display()),
            Some("Ensure the file matches the Mission schema.".to_string()),
        )
    })?;

    mission.validate().map_err(|err| {
        McpToolError::new(
            "robot.mission_validation_failed",
            format!("Mission validation failed: {err}"),
            Some("Use wa.mission_explain to inspect legal transitions.".to_string()),
        )
    })?;

    Ok(mission)
}

pub(super) fn mcp_save_mission_to_path(
    path: &Path,
    mission: &crate::plan::Mission,
) -> std::result::Result<(), McpToolError> {
    let json = serde_json::to_string_pretty(mission).map_err(|err| {
        McpToolError::new(
            "robot.mission_serialize_failed",
            format!("Failed to serialize mission: {err}"),
            None,
        )
    })?;

    std::fs::write(path, json).map_err(|err| {
        McpToolError::new(
            "robot.mission_write_failed",
            format!("Failed to write mission file {}: {err}", path.display()),
            None,
        )
    })
}

pub(super) fn mcp_mission_lifecycle_transitions(
    state: crate::plan::MissionLifecycleState,
) -> Vec<McpMissionTransitionInfo> {
    crate::plan::mission_lifecycle_transition_table()
        .iter()
        .filter(|rule| rule.from == state)
        .map(|rule| McpMissionTransitionInfo {
            kind: rule.via.to_string(),
            from: rule.from.to_string(),
            to: rule.to.to_string(),
        })
        .collect()
}

pub(super) fn mcp_mission_failure_catalog() -> Vec<McpMissionFailureCatalogEntry> {
    vec![
        McpMissionFailureCatalogEntry {
            code: "PolicyDenied".to_string(),
            reason_code: "policy_denied".to_string(),
            error_code: "mission.failure.policy_denied".to_string(),
        },
        McpMissionFailureCatalogEntry {
            code: "ApprovalDenied".to_string(),
            reason_code: "approval_denied".to_string(),
            error_code: "mission.failure.approval_denied".to_string(),
        },
        McpMissionFailureCatalogEntry {
            code: "ApprovalExpired".to_string(),
            reason_code: "approval_expired".to_string(),
            error_code: "mission.failure.approval_expired".to_string(),
        },
        McpMissionFailureCatalogEntry {
            code: "DispatchFailed".to_string(),
            reason_code: "dispatch_failed".to_string(),
            error_code: "mission.failure.dispatch_failed".to_string(),
        },
        McpMissionFailureCatalogEntry {
            code: "ExecutionFailed".to_string(),
            reason_code: "execution_failed".to_string(),
            error_code: "mission.failure.execution_failed".to_string(),
        },
        McpMissionFailureCatalogEntry {
            code: "Timeout".to_string(),
            reason_code: "timeout".to_string(),
            error_code: "mission.failure.timeout".to_string(),
        },
        McpMissionFailureCatalogEntry {
            code: "KillSwitchActivated".to_string(),
            reason_code: "kill_switch".to_string(),
            error_code: "mission.failure.kill_switch".to_string(),
        },
    ]
}

/// Build assignment data from a Mission, with optional filtering.
pub(super) fn mcp_build_mission_assignments(
    mission: &crate::plan::Mission,
    filters: &MissionStateParams,
) -> (
    Vec<McpMissionAssignmentData>,
    McpMissionAssignmentCounters,
    usize,
) {
    use crate::plan::{ApprovalState, Outcome};

    let mut counters = McpMissionAssignmentCounters {
        pending_approval: 0,
        approved: 0,
        denied: 0,
        expired: 0,
        succeeded: 0,
        failed: 0,
        cancelled: 0,
        unresolved: 0,
    };

    let limit = filters.limit.unwrap_or(100);

    let mut matched = Vec::new();
    for assignment in &mission.assignments {
        // Compute derived states
        let run_state = match &assignment.outcome {
            Some(Outcome::Success { .. }) => "succeeded",
            Some(Outcome::Failed { .. }) => "failed",
            Some(Outcome::Cancelled { .. }) => "cancelled",
            None => "pending",
        };

        let agent_state = match &assignment.approval_state {
            ApprovalState::NotRequired => "not_required",
            ApprovalState::Pending { .. } => "pending",
            ApprovalState::Approved { .. } => "approved",
            ApprovalState::Denied { .. } => "denied",
            ApprovalState::Expired { .. } => "expired",
        };

        let action_state = if run_state != "pending" {
            "completed"
        } else if matches!(
            assignment.approval_state,
            ApprovalState::Pending { .. }
                | ApprovalState::Denied { .. }
                | ApprovalState::Expired { .. }
        ) {
            "blocked"
        } else {
            "ready"
        };

        // Update counters
        match &assignment.approval_state {
            ApprovalState::Pending { .. } => counters.pending_approval += 1,
            ApprovalState::Approved { .. } | ApprovalState::NotRequired => counters.approved += 1,
            ApprovalState::Denied { .. } => counters.denied += 1,
            ApprovalState::Expired { .. } => counters.expired += 1,
        }
        match &assignment.outcome {
            Some(Outcome::Success { .. }) => counters.succeeded += 1,
            Some(Outcome::Failed { .. }) => counters.failed += 1,
            Some(Outcome::Cancelled { .. }) => counters.cancelled += 1,
            None => counters.unresolved += 1,
        }

        // Apply filters
        if let Some(ref f) = filters.run_state {
            if run_state != *f {
                continue;
            }
        }
        if let Some(ref f) = filters.agent_state {
            if agent_state != *f {
                continue;
            }
        }
        if let Some(ref f) = filters.action_state {
            if action_state != *f {
                continue;
            }
        }
        if let Some(ref f) = filters.assignment_id {
            if assignment.assignment_id.0 != *f {
                continue;
            }
        }
        if let Some(ref f) = filters.assignee {
            if assignment.assignee != *f {
                continue;
            }
        }

        let reason_code = assignment.outcome.as_ref().and_then(|o| match o {
            Outcome::Failed { reason_code, .. } => Some(reason_code.clone()),
            Outcome::Cancelled { reason_code, .. } => Some(reason_code.clone()),
            Outcome::Success { .. } => None,
        });
        let error_code = assignment.outcome.as_ref().and_then(|o| match o {
            Outcome::Failed { error_code, .. } => Some(error_code.clone()),
            Outcome::Success { .. } | Outcome::Cancelled { .. } => None,
        });

        matched.push(McpMissionAssignmentData {
            assignment_id: assignment.assignment_id.0.clone(),
            candidate_id: assignment.candidate_id.0.clone(),
            assignee: assignment.assignee.clone(),
            run_state: run_state.to_string(),
            agent_state: agent_state.to_string(),
            action_state: action_state.to_string(),
            reason_code,
            error_code,
        });
    }

    let matched_count = matched.len();
    matched.truncate(limit);

    (matched, counters, matched_count)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        MCP_ERR_INVALID_ARGS, MissionStateParams, mcp_build_mission_assignments,
        mcp_load_mission_from_path, mcp_load_mission_tx_contract_from_path,
        mcp_mission_failure_catalog, mcp_mission_lifecycle_transitions,
        mcp_parse_mission_kill_switch, mcp_resolve_mission_file_path,
        mcp_resolve_mission_tx_file_path, mcp_save_mission_to_path, mcp_tx_transition_info,
    };
    use crate::config::Config;
    use crate::plan::{
        ApprovalState, Assignment, AssignmentId, CandidateActionId, MissionActorRole, MissionId,
        MissionKillSwitchLevel, MissionLifecycleState, MissionOwnership, MissionTxContract,
        MissionTxState, Outcome, StepAction, TxId, TxIntent, TxOutcome, TxPlan, TxPlanId, TxStep,
        TxStepId,
    };

    // ── Helpers ──────────────────────────────────────────────────────────

    fn make_ownership() -> MissionOwnership {
        MissionOwnership {
            planner: "planner-a".to_string(),
            dispatcher: "dispatcher-a".to_string(),
            operator: "operator-a".to_string(),
        }
    }

    fn make_mission_with_assignments(assignments: Vec<Assignment>) -> crate::plan::Mission {
        let mut mission = crate::plan::Mission::new(
            MissionId("mission:test".to_string()),
            "Test Mission",
            "ws-test",
            make_ownership(),
            1_700_000_000_000,
        );
        mission.assignments = assignments;
        mission
    }

    fn make_assignment(
        id: &str,
        assignee: &str,
        approval: ApprovalState,
        outcome: Option<Outcome>,
    ) -> Assignment {
        Assignment {
            assignment_id: AssignmentId(id.to_string()),
            candidate_id: CandidateActionId("candidate:abc".to_string()),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: assignee.to_string(),
            reservation_intent: None,
            approval_state: approval,
            outcome,
            escalation: None,
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: None,
        }
    }

    fn make_tx_contract(step_count: usize) -> MissionTxContract {
        let steps: Vec<TxStep> = (0..step_count)
            .map(|i| TxStep {
                step_id: TxStepId(format!("step-{i}")),
                ordinal: i,
                action: StepAction::SendText {
                    pane_id: 1,
                    text: format!("echo step {i}"),
                    paste_mode: None,
                },
                description: String::new(),
            })
            .collect();
        MissionTxContract {
            tx_version: 1,
            intent: TxIntent {
                tx_id: TxId("tx-001".to_string()),
                requested_by: MissionActorRole::Operator,
                summary: "test tx".to_string(),
                correlation_id: "corr-1".to_string(),
                created_at_ms: 1_700_000_000_000,
            },
            plan: TxPlan {
                plan_id: TxPlanId("plan-001".to_string()),
                tx_id: TxId("tx-001".to_string()),
                steps,
                preconditions: Vec::new(),
                compensations: Vec::new(),
            },
            lifecycle_state: MissionTxState::Planned,
            outcome: TxOutcome::Pending,
            receipts: Vec::new(),
        }
    }

    fn empty_filters() -> MissionStateParams {
        MissionStateParams {
            mission_file: None,
            mission_state: None,
            run_state: None,
            agent_state: None,
            action_state: None,
            assignment_id: None,
            assignee: None,
            limit: None,
        }
    }

    // ========================================================================
    // mcp_parse_mission_kill_switch Tests
    // ========================================================================

    #[test]
    fn kill_switch_none_defaults_to_off() {
        let level = mcp_parse_mission_kill_switch(None).unwrap();
        assert_eq!(level, MissionKillSwitchLevel::Off);
    }

    #[test]
    fn kill_switch_off() {
        let level = mcp_parse_mission_kill_switch(Some("off")).unwrap();
        assert_eq!(level, MissionKillSwitchLevel::Off);
    }

    #[test]
    fn kill_switch_safe_mode_variants() {
        for variant in &["safe_mode", "safe-mode", "safemode"] {
            let level = mcp_parse_mission_kill_switch(Some(variant)).unwrap();
            assert_eq!(
                level,
                MissionKillSwitchLevel::SafeMode,
                "failed for {variant}"
            );
        }
    }

    #[test]
    fn kill_switch_hard_stop_variants() {
        for variant in &["hard_stop", "hard-stop", "hardstop"] {
            let level = mcp_parse_mission_kill_switch(Some(variant)).unwrap();
            assert_eq!(
                level,
                MissionKillSwitchLevel::HardStop,
                "failed for {variant}"
            );
        }
    }

    #[test]
    fn kill_switch_case_insensitive() {
        let level = mcp_parse_mission_kill_switch(Some("SAFE_MODE")).unwrap();
        assert_eq!(level, MissionKillSwitchLevel::SafeMode);
    }

    #[test]
    fn kill_switch_with_whitespace() {
        let level = mcp_parse_mission_kill_switch(Some("  hard_stop  ")).unwrap();
        assert_eq!(level, MissionKillSwitchLevel::HardStop);
    }

    #[test]
    fn kill_switch_invalid_returns_error() {
        let err = mcp_parse_mission_kill_switch(Some("bogus")).unwrap_err();
        assert_eq!(err.code, MCP_ERR_INVALID_ARGS);
        assert!(err.message.contains("bogus"));
        assert!(err.hint.as_ref().unwrap().contains("off"));
    }

    // ========================================================================
    // mcp_mission_failure_catalog Tests
    // ========================================================================

    #[test]
    fn failure_catalog_has_seven_entries() {
        let catalog = mcp_mission_failure_catalog();
        assert_eq!(catalog.len(), 7);
    }

    #[test]
    fn failure_catalog_codes_are_unique() {
        let catalog = mcp_mission_failure_catalog();
        let mut codes = std::collections::HashSet::new();
        for entry in &catalog {
            assert!(codes.insert(&entry.code), "duplicate code: {}", entry.code);
        }
    }

    #[test]
    fn failure_catalog_error_codes_have_prefix() {
        let catalog = mcp_mission_failure_catalog();
        for entry in &catalog {
            assert!(
                entry.error_code.starts_with("mission.failure."),
                "error_code {} missing prefix",
                entry.error_code
            );
        }
    }

    #[test]
    fn failure_catalog_includes_kill_switch() {
        let catalog = mcp_mission_failure_catalog();
        assert!(catalog.iter().any(|e| e.code == "KillSwitchActivated"));
    }

    // ========================================================================
    // mcp_tx_transition_info Tests
    // ========================================================================

    #[test]
    fn tx_transition_from_planned_has_entries() {
        let transitions = mcp_tx_transition_info(MissionTxState::Planned);
        assert!(
            !transitions.is_empty(),
            "Planned state should have transitions"
        );
    }

    #[test]
    fn tx_transition_committed_includes_compensating() {
        let transitions = mcp_tx_transition_info(MissionTxState::Committed);
        assert!(
            transitions
                .iter()
                .any(|t| t.kind == "compensate" && t.to == "compensating"),
            "Committed txs should advertise the rollback path"
        );
    }

    #[test]
    fn tx_transition_compensating_includes_both_success_terminal_states() {
        let transitions = mcp_tx_transition_info(MissionTxState::Compensating);
        assert!(
            transitions
                .iter()
                .any(|t| t.kind == "complete" && t.to == "compensated"),
            "Compensating txs should advertise the compensated terminal state"
        );
        assert!(
            transitions
                .iter()
                .any(|t| t.kind == "complete" && t.to == "rolled_back"),
            "Compensating txs should advertise the rolled_back terminal state"
        );
    }

    // ========================================================================
    // mcp_mission_lifecycle_transitions Tests
    // ========================================================================

    #[test]
    fn lifecycle_transitions_from_planning_has_entries() {
        let transitions = mcp_mission_lifecycle_transitions(MissionLifecycleState::Planning);
        assert!(
            !transitions.is_empty(),
            "Planning state should have transitions"
        );
    }

    #[test]
    fn lifecycle_transitions_completed_is_terminal() {
        let transitions = mcp_mission_lifecycle_transitions(MissionLifecycleState::Completed);
        // Completed is a terminal state - should have no outgoing transitions
        assert!(
            transitions.is_empty(),
            "Completed should be terminal, got {} transitions",
            transitions.len()
        );
    }

    #[test]
    fn lifecycle_transition_entries_have_from_field() {
        let transitions = mcp_mission_lifecycle_transitions(MissionLifecycleState::Running);
        for t in &transitions {
            assert!(!t.from.is_empty());
            assert!(!t.to.is_empty());
            assert!(!t.kind.is_empty());
        }
    }

    // ========================================================================
    // mcp_build_mission_assignments Tests
    // ========================================================================

    #[test]
    fn assignments_empty_mission() {
        let mission = make_mission_with_assignments(vec![]);
        let (matched, counters, total) = mcp_build_mission_assignments(&mission, &empty_filters());
        assert!(matched.is_empty());
        assert_eq!(total, 0);
        assert_eq!(counters.approved, 0);
        assert_eq!(counters.unresolved, 0);
    }

    #[test]
    fn assignments_single_pending() {
        let a = make_assignment("a1", "agent-1", ApprovalState::NotRequired, None);
        let mission = make_mission_with_assignments(vec![a]);
        let (matched, counters, total) = mcp_build_mission_assignments(&mission, &empty_filters());
        assert_eq!(total, 1);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].run_state, "pending");
        assert_eq!(matched[0].agent_state, "not_required");
        assert_eq!(matched[0].action_state, "ready");
        assert_eq!(counters.approved, 1); // NotRequired counts as approved
        assert_eq!(counters.unresolved, 1); // No outcome yet
    }

    #[test]
    fn assignments_succeeded_outcome() {
        let a = make_assignment(
            "a1",
            "agent-1",
            ApprovalState::Approved {
                approved_by: "op".to_string(),
                approved_at_ms: 1,
                approval_code_hash: "hash".to_string(),
            },
            Some(Outcome::Success {
                reason_code: "done".to_string(),
                completed_at_ms: 2,
            }),
        );
        let mission = make_mission_with_assignments(vec![a]);
        let (matched, counters, _) = mcp_build_mission_assignments(&mission, &empty_filters());
        assert_eq!(matched[0].run_state, "succeeded");
        assert_eq!(matched[0].action_state, "completed");
        assert_eq!(counters.succeeded, 1);
        assert_eq!(counters.approved, 1);
    }

    #[test]
    fn assignments_failed_outcome_has_codes() {
        let a = make_assignment(
            "a1",
            "agent-1",
            ApprovalState::NotRequired,
            Some(Outcome::Failed {
                reason_code: "timeout".to_string(),
                error_code: "E001".to_string(),
                completed_at_ms: 3,
            }),
        );
        let mission = make_mission_with_assignments(vec![a]);
        let (matched, counters, _) = mcp_build_mission_assignments(&mission, &empty_filters());
        assert_eq!(matched[0].run_state, "failed");
        assert_eq!(matched[0].reason_code.as_deref(), Some("timeout"));
        assert_eq!(matched[0].error_code.as_deref(), Some("E001"));
        assert_eq!(counters.failed, 1);
    }

    #[test]
    fn assignments_pending_approval_is_blocked() {
        let a = make_assignment(
            "a1",
            "agent-1",
            ApprovalState::Pending {
                requested_by: "op".to_string(),
                requested_at_ms: 1,
            },
            None,
        );
        let mission = make_mission_with_assignments(vec![a]);
        let (matched, counters, _) = mcp_build_mission_assignments(&mission, &empty_filters());
        assert_eq!(matched[0].action_state, "blocked");
        assert_eq!(matched[0].agent_state, "pending");
        assert_eq!(counters.pending_approval, 1);
    }

    #[test]
    fn assignments_filter_by_run_state() {
        let a1 = make_assignment("a1", "agent-1", ApprovalState::NotRequired, None);
        let a2 = make_assignment(
            "a2",
            "agent-2",
            ApprovalState::NotRequired,
            Some(Outcome::Success {
                reason_code: "ok".to_string(),
                completed_at_ms: 1,
            }),
        );
        let mission = make_mission_with_assignments(vec![a1, a2]);
        let mut filters = empty_filters();
        filters.run_state = Some("succeeded".to_string());
        let (matched, _, total) = mcp_build_mission_assignments(&mission, &filters);
        assert_eq!(total, 1);
        assert_eq!(matched[0].assignment_id, "a2");
    }

    #[test]
    fn assignments_filter_by_assignee() {
        let a1 = make_assignment("a1", "agent-1", ApprovalState::NotRequired, None);
        let a2 = make_assignment("a2", "agent-2", ApprovalState::NotRequired, None);
        let mission = make_mission_with_assignments(vec![a1, a2]);
        let mut filters = empty_filters();
        filters.assignee = Some("agent-2".to_string());
        let (matched, _, total) = mcp_build_mission_assignments(&mission, &filters);
        assert_eq!(total, 1);
        assert_eq!(matched[0].assignee, "agent-2");
    }

    #[test]
    fn assignments_limit_truncates() {
        let assignments: Vec<Assignment> = (0..10)
            .map(|i| {
                make_assignment(
                    &format!("a{i}"),
                    &format!("agent-{i}"),
                    ApprovalState::NotRequired,
                    None,
                )
            })
            .collect();
        let mission = make_mission_with_assignments(assignments);
        let mut filters = empty_filters();
        filters.limit = Some(3);
        let (matched, _, total) = mcp_build_mission_assignments(&mission, &filters);
        assert_eq!(total, 10);
        assert_eq!(matched.len(), 3);
    }

    #[test]
    fn assignments_counters_track_all_states() {
        let a1 = make_assignment(
            "a1",
            "ag1",
            ApprovalState::NotRequired,
            Some(Outcome::Success {
                reason_code: "ok".to_string(),
                completed_at_ms: 1,
            }),
        );
        let a2 = make_assignment(
            "a2",
            "ag2",
            ApprovalState::NotRequired,
            Some(Outcome::Failed {
                reason_code: "err".to_string(),
                error_code: "E1".to_string(),
                completed_at_ms: 2,
            }),
        );
        let a3 = make_assignment(
            "a3",
            "ag3",
            ApprovalState::Denied {
                denied_by: "op".to_string(),
                denied_at_ms: 3,
                reason_code: "no".to_string(),
            },
            None,
        );
        let a4 = make_assignment(
            "a4",
            "ag4",
            ApprovalState::NotRequired,
            Some(Outcome::Cancelled {
                reason_code: "abort".to_string(),
                completed_at_ms: 4,
            }),
        );
        let mission = make_mission_with_assignments(vec![a1, a2, a3, a4]);
        let (_, counters, _) = mcp_build_mission_assignments(&mission, &empty_filters());
        assert_eq!(counters.succeeded, 1);
        assert_eq!(counters.failed, 1);
        assert_eq!(counters.denied, 1);
        assert_eq!(counters.cancelled, 1);
        assert_eq!(counters.approved, 3); // NotRequired counts as approved
        assert_eq!(counters.unresolved, 1); // a3 has no outcome
    }

    // ========================================================================
    // mission_tx_prepare_gate_inputs Tests
    // ========================================================================

    #[test]
    fn prepare_gate_inputs_match_step_count() {
        let contract = make_tx_contract(3);
        let inputs = crate::plan::mission_tx_prepare_gate_inputs(&contract);
        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0].step_id, TxStepId("step-0".to_string()));
        assert_eq!(inputs[2].step_id, TxStepId("step-2".to_string()));
    }

    #[test]
    fn prepare_gate_inputs_all_pass_by_default() {
        let contract = make_tx_contract(2);
        let inputs = crate::plan::mission_tx_prepare_gate_inputs(&contract);
        for input in &inputs {
            assert!(input.policy_passed);
            assert!(input.reservation_available);
            assert!(input.approval_satisfied);
            assert!(input.target_liveness);
        }
    }

    // ========================================================================
    // mission_tx_commit_step_inputs Tests
    // ========================================================================

    #[test]
    fn commit_step_inputs_all_succeed() {
        let contract = make_tx_contract(3);
        let inputs = crate::plan::mission_tx_commit_step_inputs(&contract, None, 999);
        assert_eq!(inputs.len(), 3);
        for input in &inputs {
            assert!(input.success);
            assert_eq!(input.reason_code, "commit_step_succeeded");
            assert!(input.error_code.is_none());
            assert_eq!(input.completed_at_ms, 999);
        }
    }

    #[test]
    fn commit_step_inputs_one_fails() {
        let contract = make_tx_contract(3);
        let inputs = crate::plan::mission_tx_commit_step_inputs(&contract, Some("step-1"), 1000);
        assert!(inputs[0].success);
        assert!(!inputs[1].success);
        assert_eq!(inputs[1].reason_code, "commit_step_failed_injected");
        assert_eq!(inputs[1].error_code.as_deref(), Some("FTX3999"));
        assert!(inputs[2].success);
    }

    // ========================================================================
    // mission_tx_synthetic_commit_report Tests
    // ========================================================================

    #[test]
    fn synthetic_commit_report_basic() {
        let contract = make_tx_contract(2);
        let report = crate::plan::mission_tx_synthetic_commit_report(&contract, 5000);
        assert_eq!(report.tx_id, TxId("tx-001".to_string()));
        assert_eq!(report.plan_id, TxPlanId("plan-001".to_string()));
        assert_eq!(report.committed_count, 2);
        assert_eq!(report.failed_count, 0);
        assert_eq!(report.skipped_count, 0);
        assert_eq!(report.completed_at_ms, 5000);
        assert!(report.error_code.is_none());
    }

    #[test]
    fn synthetic_commit_report_all_steps_committed() {
        let contract = make_tx_contract(3);
        let report = crate::plan::mission_tx_synthetic_commit_report(&contract, 100);
        assert_eq!(report.step_results.len(), 3);
        for result in &report.step_results {
            assert!(result.outcome.is_committed());
            assert_eq!(result.decision_path, "rollback_synthetic_commit_report");
        }
    }

    // ========================================================================
    // mission_tx_compensation_inputs Tests
    // ========================================================================

    #[test]
    fn compensation_inputs_from_committed_report() {
        let contract = make_tx_contract(2);
        let report = crate::plan::mission_tx_synthetic_commit_report(&contract, 100);
        let inputs = crate::plan::mission_tx_compensation_inputs(&report, None, 200);
        assert_eq!(inputs.len(), 2);
        for input in &inputs {
            assert!(input.success);
            assert_eq!(input.reason_code, "compensation_succeeded");
            assert!(input.error_code.is_none());
            assert_eq!(input.completed_at_ms, 200);
        }
    }

    #[test]
    fn compensation_inputs_one_fails() {
        let contract = make_tx_contract(3);
        let report = crate::plan::mission_tx_synthetic_commit_report(&contract, 100);
        let inputs = crate::plan::mission_tx_compensation_inputs(&report, Some("step-1"), 300);
        assert!(inputs[0].success);
        assert!(!inputs[1].success);
        assert_eq!(inputs[1].reason_code, "compensation_failed_injected");
        assert_eq!(inputs[1].error_code.as_deref(), Some("FTX4999"));
        assert!(inputs[2].success);
    }

    // ========================================================================
    // File I/O functions (temp dir based)
    // ========================================================================

    #[test]
    fn load_tx_contract_not_found() {
        let path = Path::new("/nonexistent/tx-active.json");
        let err = mcp_load_mission_tx_contract_from_path(path).unwrap_err();
        assert_eq!(err.code, "robot.tx_not_found");
        assert!(err.hint.is_some());
    }

    #[test]
    fn load_tx_contract_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx.json");
        std::fs::write(&path, "not json").unwrap();
        let err = mcp_load_mission_tx_contract_from_path(&path).unwrap_err();
        assert_eq!(err.code, "robot.tx_invalid_json");
    }

    #[test]
    fn load_tx_contract_empty_steps_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx.json");
        // Serialize a real contract to get correct serde format, then empty its steps
        let mut contract = make_tx_contract(1);
        contract.plan.steps.clear();
        let json = serde_json::to_string(&contract).unwrap();
        std::fs::write(&path, json).unwrap();
        let err = mcp_load_mission_tx_contract_from_path(&path).unwrap_err();
        assert_eq!(err.code, "robot.tx_validation_failed");
    }

    #[test]
    fn load_mission_not_found() {
        let path = Path::new("/nonexistent/active.json");
        let err = mcp_load_mission_from_path(path).unwrap_err();
        assert_eq!(err.code, "robot.mission_not_found");
    }

    #[test]
    fn load_mission_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mission.json");
        std::fs::write(&path, "garbage").unwrap();
        let err = mcp_load_mission_from_path(&path).unwrap_err();
        assert_eq!(err.code, "robot.mission_invalid_json");
    }

    #[test]
    fn save_and_load_mission_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mission.json");
        let mission = make_mission_with_assignments(vec![]);
        mcp_save_mission_to_path(&path, &mission).unwrap();
        let loaded = mcp_load_mission_from_path(&path).unwrap();
        assert_eq!(loaded.mission_id.0, mission.mission_id.0);
        assert_eq!(loaded.title, mission.title);
    }

    #[test]
    fn resolve_tx_file_path_explicit() {
        // When explicit path is given, it is used directly
        let result = mcp_resolve_mission_tx_file_path(&Config::default(), Some("/tmp/my-tx.json"));
        // With explicit path, Config doesn't matter
        assert_eq!(result.unwrap(), PathBuf::from("/tmp/my-tx.json"));
    }

    #[test]
    fn resolve_mission_file_path_explicit() {
        let result =
            mcp_resolve_mission_file_path(&Config::default(), Some("/tmp/my-mission.json"));
        assert_eq!(result.unwrap(), PathBuf::from("/tmp/my-mission.json"));
    }
}
