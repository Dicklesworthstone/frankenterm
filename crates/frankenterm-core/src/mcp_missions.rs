//! Mission and transaction lifecycle helpers for MCP tools.
//!
//! Extracted from `mcp.rs` as part of Wave 4A migration (ft-1fv0u).
//! Contains file I/O, state resolution, assignment aggregation, and
//! Tx commit/compensation input builders used by the mission/tx tool handlers.

#[allow(clippy::wildcard_imports)]
use super::*;

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
            if run_state != f.as_str() {
                continue;
            }
        }
        if let Some(ref f) = filters.agent_state {
            if agent_state != f.as_str() {
                continue;
            }
        }
        if let Some(ref f) = filters.action_state {
            if action_state != f.as_str() {
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

pub(super) fn mcp_build_tx_prepare_gate_inputs(
    contract: &crate::plan::MissionTxContract,
) -> Vec<crate::plan::TxPrepareGateInput> {
    contract
        .plan
        .steps
        .iter()
        .map(|step| crate::plan::TxPrepareGateInput {
            step_id: step.step_id.clone(),
            policy_passed: true,
            policy_reason_code: None,
            reservation_available: true,
            reservation_reason_code: None,
            approval_satisfied: true,
            approval_reason_code: None,
            target_liveness: true,
            liveness_reason_code: None,
        })
        .collect()
}

pub(super) fn mcp_build_tx_commit_step_inputs(
    contract: &crate::plan::MissionTxContract,
    fail_step: Option<&str>,
    completed_at_ms: i64,
) -> Vec<crate::plan::TxCommitStepInput> {
    contract
        .plan
        .steps
        .iter()
        .map(|step| {
            let should_fail = fail_step == Some(step.step_id.0.as_str());
            crate::plan::TxCommitStepInput {
                step_id: step.step_id.clone(),
                success: !should_fail,
                reason_code: if should_fail {
                    "commit_step_failed_injected".to_string()
                } else {
                    "commit_step_succeeded".to_string()
                },
                error_code: should_fail.then(|| "FTX3999".to_string()),
                completed_at_ms,
            }
        })
        .collect()
}

pub(super) fn mcp_build_tx_compensation_inputs(
    commit_report: &crate::plan::TxCommitReport,
    fail_for_step: Option<&str>,
    completed_at_ms: i64,
) -> Vec<crate::plan::TxCompensationStepInput> {
    commit_report
        .step_results
        .iter()
        .filter(|result| result.outcome.is_committed())
        .map(|result| {
            let should_fail = fail_for_step == Some(result.step_id.0.as_str());
            crate::plan::TxCompensationStepInput {
                for_step_id: result.step_id.clone(),
                success: !should_fail,
                reason_code: if should_fail {
                    "compensation_failed_injected".to_string()
                } else {
                    "compensation_succeeded".to_string()
                },
                error_code: should_fail.then(|| "FTX4999".to_string()),
                completed_at_ms,
            }
        })
        .collect()
}

pub(super) fn mcp_build_tx_synthetic_commit_report(
    contract: &crate::plan::MissionTxContract,
    completed_at_ms: i64,
) -> crate::plan::TxCommitReport {
    let step_results = contract
        .plan
        .steps
        .iter()
        .map(|step| crate::plan::TxCommitStepResult {
            step_id: step.step_id.clone(),
            ordinal: step.ordinal,
            outcome: crate::plan::TxCommitStepOutcome::Committed {
                reason_code: "synthetic_prior_commit".to_string(),
            },
            decision_path: "rollback_synthetic_commit_report".to_string(),
            completed_at_ms,
        })
        .collect::<Vec<_>>();

    crate::plan::TxCommitReport {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        outcome: crate::plan::TxCommitOutcome::FullyCommitted,
        step_results,
        failure_boundary: None,
        committed_count: contract.plan.steps.len(),
        failed_count: 0,
        skipped_count: 0,
        decision_path: "rollback_synthetic_commit_report".to_string(),
        reason_code: "synthetic_all_committed".to_string(),
        error_code: None,
        completed_at_ms,
        receipts: Vec::new(),
    }
}
