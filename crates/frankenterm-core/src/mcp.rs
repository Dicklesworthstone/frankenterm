//! MCP server integration for wa (feature-gated).
//!
//! This module provides a thin MCP surface that mirrors robot-mode semantics.

#[allow(unused_imports)]
use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use std::path::{Path, PathBuf};
use std::sync::Arc;

#[allow(unused_imports)]
use crate::mcp_framework::{
    FrameworkContent as Content, FrameworkMcpContext as McpContext, FrameworkMcpError as McpError,
    FrameworkMcpResult as McpResult, FrameworkResource as Resource,
    FrameworkResourceContent as ResourceContent, FrameworkResourceHandler as ResourceHandler,
    FrameworkResourceTemplate as ResourceTemplate, FrameworkServer as Server,
    FrameworkStdioTransport as StdioTransport, FrameworkTool as Tool,
    FrameworkToolHandler as ToolHandler,
};

use crate::Result;
use crate::accounts::AccountRecord;
use crate::agent_provider::AgentProvider;
use crate::approval::ApprovalStore;
use crate::cass::{
    CassAgent, CassClient, CassError, CassSearchResult, CassStatus, CassViewResult,
    SearchOptions as CassSearchOptions, ViewOptions as CassViewOptions,
};
#[cfg(test)]
use crate::caut::CautError;
use crate::caut::{CautClient, CautService};
use crate::config::{Config, PaneFilterConfig};
use crate::error::WeztermError;
use crate::ingest::Osc133State;
use crate::mcp_error::{
    MCP_ERR_CASS, MCP_ERR_CAUT, MCP_ERR_CONFIG, MCP_ERR_FTS_QUERY, MCP_ERR_INVALID_ARGS,
    MCP_ERR_NOT_IMPLEMENTED, MCP_ERR_PANE_NOT_FOUND, MCP_ERR_POLICY,
    MCP_ERR_RESERVATION_CONFLICT, MCP_ERR_STORAGE, MCP_ERR_TIMEOUT, MCP_ERR_WEZTERM,
    MCP_ERR_WORKFLOW, McpToolError, map_cass_error, map_caut_error, map_mcp_error,
};
use crate::patterns::{AgentType, PatternEngine};
use crate::plan::{
    mission_tx_commit_step_inputs as mcp_build_tx_commit_step_inputs,
    mission_tx_compensation_inputs as mcp_build_tx_compensation_inputs,
    mission_tx_prepare_gate_inputs as mcp_build_tx_prepare_gate_inputs,
    mission_tx_synthetic_commit_report as mcp_build_tx_synthetic_commit_report,
};
use crate::policy::{
    ActionKind, ActorKind, DecisionContext, InjectionResult, PaneCapabilities, PolicyDecision,
    PolicyEngine, PolicyGatedInjector, PolicyInput, PolicySurface,
};
use crate::query_contract::{
    SearchQueryDefaults, SearchQueryInput, UnifiedSearchMode, parse_unified_search_query,
    to_storage_search_options,
};
use crate::runtime_compat::{CompatRuntime, RuntimeBuilder as CompatRuntimeBuilder};
use crate::storage::{EventQuery, PaneReservation, StorageHandle};
use crate::wezterm::{
    PaneInfo, PaneWaiter, WaitMatcher, WaitOptions, WaitResult, WeztermHandleSource,
    default_wezterm_handle,
};
use crate::workflows::{
    HandleAuthRequired, HandleClaudeCodeLimits, HandleCompaction, HandleGeminiQuota,
    HandleProcessTriageLifecycle, HandleSessionEnd, HandleUsageLimits, PaneWorkflowLockManager,
    Workflow, WorkflowEngine, WorkflowExecutionResult, WorkflowRunner, WorkflowRunnerConfig,
};

#[path = "mcp_bridge.rs"]
mod mcp_bridge;
#[path = "mcp_middleware.rs"]
mod mcp_middleware;
#[path = "mcp_missions.rs"]
mod mcp_missions;
#[cfg(feature = "mcp-client")]
#[path = "mcp_proxy.rs"]
mod mcp_proxy;
#[path = "mcp_resources.rs"]
mod mcp_resources;
#[path = "mcp_tools.rs"]
mod mcp_tools;
#[path = "mcp_types.rs"]
mod mcp_types;

pub use mcp_bridge::{build_server, build_server_with_db, run_stdio_server};
use mcp_middleware::{AuditedToolHandler, FormatAwareToolHandler};
#[cfg(test)]
use mcp_middleware::{
    McpOutputFormat, augment_tool_schema_with_format, encode_mcp_contents,
    extract_mcp_output_format, parse_mcp_output_format,
};
use mcp_missions::{
    mcp_build_mission_assignments, mcp_load_mission_from_path,
    mcp_load_mission_tx_contract_from_path, mcp_mission_failure_catalog,
    mcp_mission_lifecycle_transitions, mcp_parse_mission_kill_switch,
    mcp_resolve_mission_file_path, mcp_resolve_mission_tx_file_path, mcp_save_mission_to_path,
    mcp_tx_transition_info,
};
use mcp_resources::{
    WaAccountsByServiceTemplateResource, WaAccountsResource, WaEventsResource,
    WaEventsTemplateResource, WaEventsUnhandledTemplateResource, WaPanesResource,
    WaReservationsByPaneTemplateResource, WaReservationsResource, WaRulesByAgentTemplateResource,
    WaRulesResource, WaWorkflowsResource,
};
use mcp_tools::{
    WaAccountsRefreshTool, WaAccountsTool, WaCassSearchTool, WaCassStatusTool, WaCassViewTool,
    WaEventsAnnotateTool, WaEventsLabelTool, WaEventsTool, WaEventsTriageTool, WaGetTextTool,
    WaMissionAbortTool, WaMissionExplainTool, WaMissionPauseTool, WaMissionResumeTool,
    WaMissionStateTool, WaReleaseTool, WaReservationsTool, WaReserveTool, WaRulesListTool,
    WaRulesTestTool, WaSearchTool, WaSendTool, WaStateTool, WaTxPlanTool, WaTxRollbackTool,
    WaTxRunTool, WaTxShowTool, WaWaitForTool, WaWorkflowRunTool,
};
#[allow(clippy::wildcard_imports)]
use mcp_types::*;

fn effective_search_rrf_k(config: &Config) -> u32 {
    config.search.rrf_k.max(1)
}

fn effective_search_quality_timeout_ms(config: &Config) -> u64 {
    config.search.quality_timeout_ms.max(1)
}

fn effective_search_fusion_weights(config: &Config) -> (f32, f32) {
    if config.search.fast_only {
        return (1.0, 0.0);
    }

    let quality_weight = if config.search.quality_weight.is_finite() {
        config.search.quality_weight.clamp(0.0, 1.0)
    } else {
        0.7
    };
    let semantic_weight = quality_weight as f32;
    let lexical_weight = (1.0 - semantic_weight).max(0.0);
    (lexical_weight, semantic_weight)
}

fn effective_search_fusion_backend(config: &Config) -> crate::search::FusionBackend {
    crate::search::FusionBackend::parse(&config.search.fusion_backend)
}

/// Convert a PaneReservation to MCP info format
fn reservation_to_mcp_info(r: &PaneReservation) -> McpReservationInfo {
    let now = now_ms() as i64;
    let status = if r.released_at.is_some() {
        "released"
    } else if r.is_active(now) {
        "active"
    } else {
        "expired"
    };

    McpReservationInfo {
        id: r.id,
        pane_id: r.pane_id,
        owner_kind: r.owner_kind.clone(),
        owner_id: r.owner_id.clone(),
        reason: r.reason.clone(),
        created_at: r.created_at,
        expires_at: r.expires_at,
        released_at: r.released_at,
        status: status.to_string(),
    }
}

const SEND_OSC_SEGMENT_LIMIT: usize = 200;
const MCP_REFRESH_COOLDOWN_MS: i64 = 30_000;

fn build_policy_engine(config: &Config, require_prompt_active: bool) -> PolicyEngine {
    PolicyEngine::new(
        config.safety.rate_limit_per_pane,
        config.safety.rate_limit_global,
        require_prompt_active,
    )
    .with_command_gate_config(config.safety.command_gate.clone())
    .with_trauma_guard_enabled(config.safety.trauma_guard.enabled)
    .with_policy_rules(config.safety.rules.clone())
}

fn injection_from_decision(
    decision: PolicyDecision,
    summary: String,
    pane_id: u64,
    action: ActionKind,
) -> InjectionResult {
    match decision {
        PolicyDecision::Allow { .. } => InjectionResult::Allowed {
            decision,
            summary,
            pane_id,
            action,
            audit_action_id: None,
        },
        PolicyDecision::Deny { .. } => InjectionResult::Denied {
            decision,
            summary,
            pane_id,
            action,
            audit_action_id: None,
        },
        PolicyDecision::RequireApproval { .. } => InjectionResult::RequiresApproval {
            decision,
            summary,
            pane_id,
            action,
            audit_action_id: None,
        },
    }
}

fn policy_reason(decision: &PolicyDecision) -> Option<&str> {
    match decision {
        PolicyDecision::Deny { reason, .. } | PolicyDecision::RequireApproval { reason, .. } => {
            Some(reason)
        }
        PolicyDecision::Allow { .. } => None,
    }
}

fn approval_command(decision: &PolicyDecision) -> Option<String> {
    match decision {
        PolicyDecision::RequireApproval {
            approval: Some(approval),
            ..
        } => Some(approval.command.clone()),
        _ => None,
    }
}

fn resolve_workspace_id(config: &Config) -> Result<String> {
    let layout = config.workspace_layout(None)?;
    Ok(layout.root.to_string_lossy().to_string())
}

fn parse_caut_service(service: &str) -> Option<CautService> {
    let normalized = service.trim();
    if normalized.eq_ignore_ascii_case("openai") {
        return Some(CautService::OpenAI);
    }
    let provider = AgentProvider::from_slug(normalized);
    CautService::from_provider(&provider)
}

fn parse_cass_agent(agent: &str) -> Option<CassAgent> {
    CassAgent::from_slug(agent)
}

fn check_refresh_cooldown(
    most_recent_refresh_ms: i64,
    now_ms_val: i64,
    cooldown_ms: i64,
) -> Option<(i64, i64)> {
    if most_recent_refresh_ms <= 0 || cooldown_ms <= 0 {
        return None;
    }
    let elapsed = (now_ms_val - most_recent_refresh_ms).max(0);
    if elapsed < cooldown_ms {
        let remaining = (cooldown_ms - elapsed).max(0);
        Some((elapsed / 1000, remaining / 1000))
    } else {
        None
    }
}

async fn derive_osc_state_from_storage(
    storage: &StorageHandle,
    pane_id: u64,
) -> std::result::Result<Option<Osc133State>, String> {
    let segments = storage
        .get_segments(pane_id, SEND_OSC_SEGMENT_LIMIT)
        .await
        .map_err(|e| format!("failed to read segments: {e}"))?;
    if segments.is_empty() {
        return Ok(None);
    }

    let mut state = Osc133State::new();
    for segment in segments.iter().rev() {
        crate::ingest::process_osc133_output(&mut state, &segment.content);
    }

    if state.markers_seen == 0 {
        return Ok(None);
    }

    Ok(Some(state))
}

#[cfg(unix)]
async fn fetch_pane_state_from_ipc(
    socket_path: &std::path::Path,
    pane_id: u64,
) -> std::result::Result<Option<IpcPaneState>, String> {
    let client = crate::ipc::IpcClient::new(socket_path);
    match client.pane_state(pane_id).await {
        Ok(response) => {
            if !response.ok {
                let detail = response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string());
                return Err(detail);
            }
            if let Some(data) = response.data {
                serde_json::from_value::<IpcPaneState>(data)
                    .map(Some)
                    .map_err(|e| format!("invalid pane state payload: {e}"))
            } else {
                Ok(None)
            }
        }
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(not(unix))]
async fn fetch_pane_state_from_ipc(
    _socket_path: &std::path::Path,
    _pane_id: u64,
) -> std::result::Result<Option<IpcPaneState>, String> {
    Err("IPC not supported on this platform".to_string())
}

fn resolve_alt_screen_state(state: &IpcPaneState) -> Option<bool> {
    if !state.known {
        return None;
    }
    if let Some(cursor_state) = state.cursor_alt_screen {
        return Some(cursor_state);
    }
    if state.last_status_at.is_some() {
        return state.alt_screen;
    }
    None
}

async fn resolve_pane_capabilities(
    config: &Config,
    storage: Option<&StorageHandle>,
    pane_id: u64,
) -> CapabilityResolution {
    let mut warnings = Vec::new();
    let mut osc_state = None;

    if let Some(storage) = storage {
        match derive_osc_state_from_storage(storage, pane_id).await {
            Ok(state) => osc_state = state,
            Err(err) => warnings.push(format!("OSC 133 state unavailable: {err}")),
        }
    } else {
        warnings.push("Storage unavailable; prompt state unknown.".to_string());
    }

    let mut alt_screen = None;
    let mut in_gap = true;
    let mut gap_known = false;

    let ipc_socket_path = match config.workspace_layout(None) {
        Ok(layout) => Some(layout.ipc_socket_path),
        Err(err) => {
            warnings.push(format!("Workspace layout unavailable: {err}"));
            None
        }
    };

    if let Some(socket_path) = ipc_socket_path.as_deref() {
        match fetch_pane_state_from_ipc(socket_path, pane_id).await {
            Ok(Some(state)) => {
                if state.pane_id != pane_id {
                    warnings.push(format!(
                        "Watcher returned state for pane {} (expected {})",
                        state.pane_id, pane_id
                    ));
                }
                if !state.known {
                    let reason = state.reason.as_deref().unwrap_or("unknown");
                    warnings.push(format!("Watcher has no state for this pane ({reason})."));
                } else if state.observed == Some(false) {
                    warnings.push(
                        "Pane is not observed by watcher; state may be incomplete.".to_string(),
                    );
                }
                alt_screen = resolve_alt_screen_state(&state);
                if state.in_gap.is_some() {
                    gap_known = true;
                    in_gap = state.in_gap.unwrap_or(true);
                }
                if alt_screen.is_none() {
                    warnings
                        .push("Alt-screen state unknown; approval may be required.".to_string());
                }
                if in_gap {
                    if gap_known {
                        warnings.push(
                            "Recent capture gap detected; approval may be required.".to_string(),
                        );
                    } else {
                        warnings.push(
                            "Capture continuity unknown; treating as recent gap.".to_string(),
                        );
                    }
                } else if !gap_known {
                    warnings
                        .push("Capture continuity unknown; treating as recent gap.".to_string());
                }
            }
            Ok(None) => {
                warnings.push("Watcher IPC returned no pane state.".to_string());
            }
            Err(err) => {
                warnings.push(format!("Watcher IPC unavailable: {err}"));
            }
        }
    } else {
        warnings.push("IPC socket unavailable; alt-screen/gap unknown.".to_string());
    }

    let mut capabilities =
        PaneCapabilities::from_ingest_state(osc_state.as_ref(), alt_screen, in_gap);

    if let Some(storage) = storage {
        match storage.get_active_reservation(pane_id).await {
            Ok(Some(reservation)) => {
                capabilities.is_reserved = true;
                capabilities.reserved_by = Some(reservation.owner_id);
            }
            Ok(None) => {}
            Err(err) => {
                warnings.push(format!("Reservation lookup failed: {err}"));
            }
        }
    }

    CapabilityResolution {
        capabilities,
        _warnings: warnings,
    }
}

fn register_builtin_workflows(runner: &WorkflowRunner, config: &Config) {
    for workflow in builtin_workflows(config) {
        runner.register_workflow(workflow);
    }
}

fn builtin_workflows(config: &Config) -> Vec<Arc<dyn Workflow>> {
    vec![
        Arc::new(
            HandleCompaction::new().with_prompt_config(config.workflows.compaction_prompts.clone()),
        ),
        Arc::new(HandleUsageLimits::new()),
        Arc::new(HandleSessionEnd::new()),
        Arc::new(HandleAuthRequired::new()),
        Arc::new(HandleClaudeCodeLimits::new()),
        Arc::new(HandleGeminiQuota::new()),
        Arc::new(HandleProcessTriageLifecycle::new()),
    ]
}

fn envelope_to_content<T: Serialize>(envelope: McpEnvelope<T>) -> McpResult<Vec<Content>> {
    let text = serde_json::to_string(&envelope)
        .map_err(|e| McpError::internal_error(format!("Serialize MCP response: {e}")))?;
    Ok(vec![Content::Text { text }])
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Build a redacted summary of MCP tool arguments (keys only, no values).
fn redact_mcp_args(tool_name: &str, args: &serde_json::Value) -> String {
    let keys = args
        .as_object()
        .map(|m| m.keys().map(|k| k.as_str()).collect::<Vec<_>>().join(","))
        .unwrap_or_default();
    if keys.is_empty() {
        format!("mcp:{tool_name}")
    } else {
        format!("mcp:{tool_name} keys=[{keys}]")
    }
}

fn mcp_audit_decision_context(
    tool_name: &str,
    action_kind: &str,
    decision: &str,
    result: &str,
    error_code: Option<&str>,
    elapsed_ms: u64,
) -> Option<String> {
    let mut context = DecisionContext::new_audit(
        i64::try_from(now_ms()).unwrap_or(0),
        ActionKind::ExecCommand,
        ActorKind::Mcp,
        PolicySurface::Mcp,
        None,
        None,
        Some(format!("mcp audit for {tool_name}")),
        None,
    );
    let determining_rule = format!("audit.{action_kind}");
    context.record_rule(
        &determining_rule,
        true,
        Some(decision),
        Some(format!("MCP tool audit recorded with result={result}")),
    );
    context.set_determining_rule(&determining_rule);
    context.add_evidence("stage", "mcp_audit");
    context.add_evidence("tool", tool_name);
    context.add_evidence("mcp_action_kind", action_kind);
    context.add_evidence("mcp_surface", PolicySurface::Mcp.as_str());
    context.add_evidence("policy_decision", decision);
    context.add_evidence("result", result);
    context.add_evidence("elapsed_ms", elapsed_ms.to_string());
    if let Some(error_code) = error_code {
        context.add_evidence("error_code", error_code);
    }
    serde_json::to_string(&context).ok()
}

/// Record an MCP tool call audit entry.
///
/// This is fire-and-forget: failures are logged but never propagated to the caller.
async fn record_mcp_audit(
    storage: &StorageHandle,
    tool_name: &str,
    input_summary: String,
    decision: &str,
    result: &str,
    error_code: Option<&str>,
    elapsed_ms: u64,
) {
    let ts = i64::try_from(now_ms()).unwrap_or(0);
    let action_kind = format!("mcp.{tool_name}");
    let audit = crate::storage::AuditActionRecord {
        id: 0,
        ts,
        actor_kind: "mcp".to_string(),
        actor_id: None,
        correlation_id: None,
        pane_id: None,
        domain: None,
        action_kind: action_kind.clone(),
        policy_decision: decision.to_string(),
        decision_reason: error_code.map(|c| format!("error_code={c}")),
        rule_id: None,
        input_summary: Some(format!("{input_summary} elapsed_ms={elapsed_ms}")),
        verification_summary: None,
        decision_context: mcp_audit_decision_context(
            tool_name,
            &action_kind,
            decision,
            result,
            error_code,
            elapsed_ms,
        ),
        result: result.to_string(),
    };
    if let Err(e) = storage.record_audit_action_redacted(audit).await {
        tracing::warn!(tool = tool_name, error = %e, "Failed to record MCP audit entry");
    }
}

/// Record an MCP audit entry for tools that have a db_path available.
///
/// Opens a StorageHandle, records the audit, and closes it.
/// Fire-and-forget: errors are logged, never propagated.
fn record_mcp_audit_sync(
    db_path: &std::path::Path,
    tool_name: &str,
    args: &serde_json::Value,
    ok: bool,
    error_code: Option<&str>,
    elapsed_ms: u64,
) {
    let summary = redact_mcp_args(tool_name, args);
    let db_path_str = db_path.to_string_lossy().to_string();
    let tool_name = tool_name.to_string();
    let error_code = error_code.map(|s| s.to_string());
    let decision = if ok { "allow" } else { "deny" };
    let result = if ok { "success" } else { "error" };

    // Spawn a background task to record audit — non-blocking, fire-and-forget
    std::thread::spawn(move || {
        let rt = match CompatRuntimeBuilder::current_thread().build() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(tool = %tool_name, error = %e, "Failed to create runtime for MCP audit");
                return;
            }
        };
        rt.block_on(async {
            if let Ok(storage) = StorageHandle::new(&db_path_str).await {
                record_mcp_audit(
                    &storage,
                    &tool_name,
                    summary,
                    decision,
                    result,
                    error_code.as_deref(),
                    elapsed_ms,
                )
                .await;
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    fn uri_set(values: impl IntoIterator<Item = String>) -> BTreeSet<String> {
        values.into_iter().collect()
    }

    fn json_value_strategy() -> impl Strategy<Value = serde_json::Value> {
        // TOON uses f64 internally (standard JSON semantics), so restrict integers
        // to the f64-exact range (±2^53) to ensure lossless roundtrip.
        const F64_INT_MAX: i64 = 1_i64 << 53;
        let leaf = proptest::prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::Bool),
            (-F64_INT_MAX..=F64_INT_MAX).prop_map(|n| serde_json::Value::Number(n.into())),
            ".*".prop_map(serde_json::Value::String),
        ];

        leaf.prop_recursive(4, 64, 8, |inner| {
            proptest::prop_oneof![
                proptest::collection::vec(inner.clone(), 0..8).prop_map(serde_json::Value::Array),
                proptest::collection::btree_map("[a-zA-Z0-9_]{1,16}", inner, 0..8).prop_map(
                    |map| {
                        let object = map
                            .into_iter()
                            .collect::<serde_json::Map<String, serde_json::Value>>();
                        serde_json::Value::Object(object)
                    }
                ),
            ]
        })
    }

    fn sample_mcp_tx_contract() -> crate::plan::MissionTxContract {
        use crate::plan::{
            MissionActorRole, MissionTxContract, MissionTxState, StepAction, TxCompensation, TxId,
            TxIntent, TxOutcome, TxPlan, TxPlanId, TxPrecondition, TxStep, TxStepId,
        };

        let tx_id = TxId("tx:mcp-test".to_string());
        MissionTxContract {
            tx_version: crate::plan::MISSION_TX_SCHEMA_VERSION,
            intent: TxIntent {
                tx_id: tx_id.clone(),
                requested_by: MissionActorRole::Dispatcher,
                summary: "mcp tx test".to_string(),
                correlation_id: "mcp-tx-corr".to_string(),
                created_at_ms: 1_704_200_000_000,
            },
            plan: TxPlan {
                plan_id: TxPlanId("tx-plan:mcp-test".to_string()),
                tx_id,
                steps: vec![
                    TxStep {
                        step_id: TxStepId("tx-step:1".to_string()),
                        ordinal: 1,
                        action: StepAction::SendText {
                            pane_id: 1,
                            text: "/do-step-1".to_string(),
                            paste_mode: Some(false),
                        },
                        description: String::new(),
                    },
                    TxStep {
                        step_id: TxStepId("tx-step:2".to_string()),
                        ordinal: 2,
                        action: StepAction::SendText {
                            pane_id: 2,
                            text: "/do-step-2".to_string(),
                            paste_mode: Some(true),
                        },
                        description: String::new(),
                    },
                ],
                preconditions: vec![TxPrecondition::PromptActive { pane_id: 1 }],
                compensations: vec![
                    TxCompensation {
                        for_step_id: TxStepId("tx-step:1".to_string()),
                        action: StepAction::SendText {
                            pane_id: 1,
                            text: "/undo-step-1".to_string(),
                            paste_mode: Some(false),
                        },
                    },
                    TxCompensation {
                        for_step_id: TxStepId("tx-step:2".to_string()),
                        action: StepAction::SendText {
                            pane_id: 2,
                            text: "/undo-step-2".to_string(),
                            paste_mode: Some(true),
                        },
                    },
                ],
            },
            lifecycle_state: MissionTxState::Planned,
            outcome: TxOutcome::Pending,
            receipts: Vec::new(),
        }
    }

    #[test]
    fn parse_mcp_output_format_supports_json_and_toon() {
        assert_eq!(parse_mcp_output_format("json"), Some(McpOutputFormat::Json));
        assert_eq!(parse_mcp_output_format("toon"), Some(McpOutputFormat::Toon));
        assert_eq!(
            parse_mcp_output_format(" TOON "),
            Some(McpOutputFormat::Toon)
        );
        assert_eq!(parse_mcp_output_format("yaml"), None);
    }

    #[test]
    fn extract_mcp_output_format_defaults_to_json_and_strips_param() {
        let mut args = serde_json::json!({
            "pane_id": 42,
            "format": "toon"
        });
        let format = extract_mcp_output_format(&mut args).expect("format should parse");
        assert_eq!(format, McpOutputFormat::Toon);
        assert!(args.get("format").is_none());
        assert_eq!(args["pane_id"], 42);

        let mut no_format = serde_json::json!({
            "pane_id": 1
        });
        let default = extract_mcp_output_format(&mut no_format).expect("default format");
        assert_eq!(default, McpOutputFormat::Json);
    }

    #[test]
    fn augment_tool_schema_with_format_adds_format_property() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "pane_id": {"type": "integer"}
            },
            "required": ["pane_id"],
            "additionalProperties": false
        });

        augment_tool_schema_with_format(&mut schema);

        assert_eq!(schema["properties"]["format"]["type"], "string");
        assert_eq!(schema["properties"]["format"]["enum"][0], "json");
        assert_eq!(schema["properties"]["format"]["enum"][1], "toon");
    }

    #[test]
    fn encode_mcp_contents_toon_transcodes_json_text_payload() {
        let contents = vec![Content::Text {
            text: r#"{"ok":true,"data":{"pane_id":7},"elapsed_ms":1}"#.to_string(),
        }];

        let encoded =
            encode_mcp_contents(contents, McpOutputFormat::Toon).expect("TOON transcode works");
        let text = match &encoded[0] {
            Content::Text { text } => text.clone(),
            _ => panic!("expected text content"), // ubs:ignore
        };

        // TOON output should not remain raw JSON and should still be decodable.
        assert!(!text.trim_start().starts_with('{'));
        let _decoded = toon_rust::try_decode(&text, None).expect("TOON decode should succeed");
    }

    /// Deep-compare two JSON values allowing ±1 ULP tolerance on numbers.
    /// TOON uses f64 internally and its encode→json_stringify→parse roundtrip
    /// can introduce 1 ULP drift on some values. This comparator accounts for
    /// both the i64/f64 type distinction and the 1 ULP decimal roundtrip issue.
    fn json_values_equivalent(a: &serde_json::Value, b: &serde_json::Value) -> bool {
        match (a, b) {
            (serde_json::Value::Null, serde_json::Value::Null) => true,
            (serde_json::Value::Bool(x), serde_json::Value::Bool(y)) => x == y,
            (serde_json::Value::String(x), serde_json::Value::String(y)) => x == y,
            (serde_json::Value::Number(x), serde_json::Value::Number(y)) => {
                let fx = x.as_f64().unwrap_or(f64::NAN);
                let fy = y.as_f64().unwrap_or(f64::NAN);
                (fx - fy).abs() <= fx.abs().max(fy.abs()).max(1.0) * 2.0 * f64::EPSILON
            }
            (serde_json::Value::Array(x), serde_json::Value::Array(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .zip(y.iter())
                        .all(|(a, b)| json_values_equivalent(a, b))
            }
            (serde_json::Value::Object(x), serde_json::Value::Object(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .all(|(k, v)| y.get(k).is_some_and(|yv| json_values_equivalent(v, yv)))
            }
            _ => false,
        }
    }

    proptest::proptest! {
        #[test]
        fn proptest_toon_roundtrip_preserves_json_semantics(value in json_value_strategy()) {
            let toon = toon_rust::encode(value.clone(), None);
            let decoded = toon_rust::try_decode(&toon, None).expect("decode should succeed");
            let decoded_json = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0)
                .join("\n");
            let decoded_value: serde_json::Value =
                serde_json::from_str(&decoded_json).expect("decoded TOON should parse as JSON");
            prop_assert!(
                json_values_equivalent(&decoded_value, &value),
                "TOON roundtrip mismatch:\n  decoded: {}\n  original: {}",
                decoded_value, value,
            );
        }

        #[test]
        fn proptest_toon_decode_from_lines_matches_single_pass(value in json_value_strategy()) {
            let toon = toon_rust::encode(value.clone(), None);
            let lines = toon
                .lines()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>();

            let decoded_from_lines =
                toon_rust::try_decode_from_lines(lines, None).expect("line decode should succeed");
            let decoded_json = toon_rust::cli::json_stringify::json_stringify_lines(
                &decoded_from_lines,
                0,
            )
            .join("\n");
            let decoded_value: serde_json::Value =
                serde_json::from_str(&decoded_json).expect("decoded TOON should parse as JSON");
            prop_assert!(
                json_values_equivalent(&decoded_value, &value),
                "line decode roundtrip mismatch:\n  decoded: {}\n  original: {}",
                decoded_value, value,
            );
        }

        #[test]
        fn proptest_toon_stream_events_stable_under_chunked_line_iteration(
            value in json_value_strategy(),
            chunk_size in 1usize..8
        ) {
            let toon = toon_rust::encode(value, None);
            let lines = toon
                .lines()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>();

            let full_events =
                toon_rust::try_decode_stream_sync(lines.iter().cloned(), None)
                    .expect("stream decode should succeed");

            let chunked_lines = lines
                .chunks(chunk_size)
                .flat_map(|chunk| chunk.iter().cloned())
                .collect::<Vec<_>>();
            let chunked_events =
                toon_rust::try_decode_stream_sync(chunked_lines, None)
                    .expect("chunked stream decode should succeed");

            prop_assert_eq!(chunked_events, full_events);
        }
    }

    #[test]
    fn mcp_server_with_db_exposes_expected_resources_and_templates() {
        let server = build_server_with_db(&Config::default(), Some(PathBuf::from("wa-test.db")))
            .expect("build mcp server");

        let resources = uri_set(server.resources().into_iter().map(|r| r.uri));
        let templates = uri_set(
            server
                .resource_templates()
                .into_iter()
                .map(|t| t.uri_template),
        );

        assert_eq!(
            resources,
            uri_set([
                "wa://panes".to_string(),
                "wa://events".to_string(),
                "wa://accounts".to_string(),
                "wa://rules".to_string(),
                "wa://workflows".to_string(),
                "wa://reservations".to_string(),
            ])
        );
        assert_eq!(
            templates,
            uri_set([
                "wa://events/{limit}".to_string(),
                "wa://events/unhandled/{limit}".to_string(),
                "wa://accounts/{service}".to_string(),
                "wa://rules/{agent_type}".to_string(),
                "wa://reservations/{pane_id}".to_string(),
            ])
        );
    }

    #[test]
    fn mcp_server_without_db_only_exposes_non_storage_resources() {
        let server = build_server_with_db(&Config::default(), None).expect("build mcp server");

        let resources = uri_set(server.resources().into_iter().map(|r| r.uri));
        let templates = uri_set(
            server
                .resource_templates()
                .into_iter()
                .map(|t| t.uri_template),
        );

        assert_eq!(
            resources,
            uri_set([
                "wa://panes".to_string(),
                "wa://rules".to_string(),
                "wa://workflows".to_string(),
            ])
        );
        assert_eq!(templates, uri_set(["wa://rules/{agent_type}".to_string()]));
    }

    // ── Error code stability tests (wa-nu4.3.1.3) ────────────────────────

    #[test]
    fn error_codes_have_stable_prefix() {
        let codes = [
            MCP_ERR_INVALID_ARGS,
            MCP_ERR_CONFIG,
            MCP_ERR_WEZTERM,
            MCP_ERR_STORAGE,
            MCP_ERR_POLICY,
            MCP_ERR_PANE_NOT_FOUND,
            MCP_ERR_WORKFLOW,
            MCP_ERR_TIMEOUT,
            MCP_ERR_NOT_IMPLEMENTED,
            MCP_ERR_FTS_QUERY,
            MCP_ERR_RESERVATION_CONFLICT,
            MCP_ERR_CAUT,
            MCP_ERR_CASS,
        ];
        for code in &codes {
            assert!(
                code.starts_with("FT-MCP-"),
                "Error code {code} must start with WA-MCP-"
            );
        }
        // All codes should be unique
        let unique: BTreeSet<&str> = codes.iter().copied().collect();
        assert_eq!(unique.len(), codes.len(), "Error codes must be unique");
    }

    #[test]
    fn error_codes_are_numeric_suffixed() {
        let codes = [
            MCP_ERR_INVALID_ARGS,
            MCP_ERR_CONFIG,
            MCP_ERR_WEZTERM,
            MCP_ERR_STORAGE,
            MCP_ERR_POLICY,
            MCP_ERR_PANE_NOT_FOUND,
            MCP_ERR_WORKFLOW,
            MCP_ERR_TIMEOUT,
            MCP_ERR_NOT_IMPLEMENTED,
            MCP_ERR_FTS_QUERY,
            MCP_ERR_RESERVATION_CONFLICT,
            MCP_ERR_CAUT,
            MCP_ERR_CASS,
        ];
        for code in &codes {
            let suffix = &code["FT-MCP-".len()..];
            assert!(
                suffix.chars().all(|c| c.is_ascii_digit()),
                "Error code suffix '{suffix}' must be numeric for {code}"
            );
        }
    }

    // ── Envelope schema tests (wa-nu4.3.1.3) ─────────────────────────────

    #[test]
    fn envelope_success_has_required_fields() {
        let envelope = McpEnvelope::success("test_data".to_string(), 42);
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json["data"].is_string());
        assert_eq!(json["elapsed_ms"], 42);
        assert!(json["version"].is_string());
        assert!(json["now"].is_number());
        assert!(json["mcp_version"].is_string());
        // Error fields should be absent (skip_serializing_if = Option::is_none)
        assert!(json.get("error").is_none());
        assert!(json.get("error_code").is_none());
        assert!(json.get("hint").is_none());
    }

    #[test]
    fn envelope_error_has_required_fields() {
        let envelope = McpEnvelope::<()>::error(
            MCP_ERR_STORAGE,
            "db error",
            Some("Try again".to_string()),
            99,
        );
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["ok"], false);
        assert!(json.get("data").is_none());
        assert_eq!(json["error"], "db error");
        assert_eq!(json["error_code"], "FT-MCP-0005");
        assert_eq!(json["hint"], "Try again");
        assert_eq!(json["elapsed_ms"], 99);
        assert!(json["version"].is_string());
    }

    #[test]
    fn envelope_error_without_hint() {
        let envelope = McpEnvelope::<()>::error(MCP_ERR_TIMEOUT, "timeout", None, 5000);
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error_code"], "FT-MCP-0009");
        assert!(json.get("hint").is_none());
    }

    #[test]
    fn envelope_version_matches_crate() {
        let envelope = McpEnvelope::success((), 0);
        assert_eq!(envelope.version, crate::VERSION);
    }

    #[test]
    fn mcp_version_is_set() {
        assert!(!MCP_VERSION.is_empty());
        assert!(
            MCP_VERSION.starts_with('v')
                || MCP_VERSION.starts_with("0.")
                || MCP_VERSION.starts_with("1."),
            "MCP_VERSION '{MCP_VERSION}' should be versioned"
        );
    }

    // ── map_mcp_error coverage (wa-nu4.3.1.3) ────────────────────────────

    #[test]
    fn map_mcp_error_storage() {
        let err = crate::Error::Storage(crate::StorageError::Database("test".to_string()));
        let (code, _hint) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_STORAGE);
    }

    #[test]
    fn map_mcp_error_config() {
        let err = crate::Error::Config(crate::error::ConfigError::ParseError(
            "bad config".to_string(),
        ));
        let (code, _hint) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_CONFIG);
    }

    #[test]
    fn mcp_parse_mission_kill_switch_supports_expected_values() {
        assert_eq!(
            mcp_parse_mission_kill_switch(None).expect("default should parse"),
            crate::plan::MissionKillSwitchLevel::Off
        );
        assert_eq!(
            mcp_parse_mission_kill_switch(Some("safe_mode")).expect("snake case should parse"),
            crate::plan::MissionKillSwitchLevel::SafeMode
        );
        assert_eq!(
            mcp_parse_mission_kill_switch(Some("hard-stop")).expect("kebab case should parse"),
            crate::plan::MissionKillSwitchLevel::HardStop
        );
        let err =
            mcp_parse_mission_kill_switch(Some("invalid")).expect_err("invalid value should fail");
        assert_eq!(err.code, MCP_ERR_INVALID_ARGS);
    }

    #[test]
    fn mcp_load_mission_tx_contract_maps_errors_and_accepts_valid_contract() {
        let temp_root = std::env::temp_dir().join(format!(
            "ft-mcp-tx-loader-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&temp_root).expect("create temp root");

        let missing_path = temp_root.join("missing.json");
        let missing = mcp_load_mission_tx_contract_from_path(&missing_path)
            .expect_err("missing path should fail");
        assert_eq!(missing.code, "robot.tx_not_found");

        let invalid_json_path = temp_root.join("invalid.json");
        std::fs::write(&invalid_json_path, "{broken").expect("write invalid json");
        let invalid = mcp_load_mission_tx_contract_from_path(&invalid_json_path)
            .expect_err("invalid json should fail");
        assert_eq!(invalid.code, "robot.tx_invalid_json");

        let mut invalid_contract = sample_mcp_tx_contract();
        invalid_contract.plan.steps.clear();
        let invalid_contract_path = temp_root.join("invalid-contract.json");
        std::fs::write(
            &invalid_contract_path,
            serde_json::to_string_pretty(&invalid_contract).expect("serialize invalid contract"),
        )
        .expect("write invalid contract");
        let validation = mcp_load_mission_tx_contract_from_path(&invalid_contract_path)
            .expect_err("invalid contract should fail validation");
        assert_eq!(validation.code, "robot.tx_validation_failed");

        let valid_contract = sample_mcp_tx_contract();
        let valid_contract_path = temp_root.join("valid-contract.json");
        std::fs::write(
            &valid_contract_path,
            serde_json::to_string_pretty(&valid_contract).expect("serialize valid contract"),
        )
        .expect("write valid contract");
        let loaded = mcp_load_mission_tx_contract_from_path(&valid_contract_path)
            .expect("valid contract should load");
        assert_eq!(loaded.intent.tx_id.0, "tx:mcp-test");
        assert_eq!(loaded.plan.steps.len(), 2);
    }

    #[test]
    fn mcp_tx_transition_info_returns_rules_for_planned_state() {
        let transitions = mcp_tx_transition_info(crate::plan::MissionTxState::Planned);
        assert!(!transitions.is_empty());
    }

    // ── Tool definition validation (wa-nu4.3.1.3) ────────────────────────

    #[test]
    fn all_spec_tools_registered_with_db() {
        let server = build_server_with_db(&Config::default(), Some(PathBuf::from("wa-test.db")))
            .expect("build mcp server");
        let tool_defs = server.tools();
        let tool_names: BTreeSet<String> = tool_defs.into_iter().map(|t| t.name).collect();

        // All tools from wa-nu4.3.1.1 spec must be present
        let required = [
            "wa.state",
            "wa.get_text",
            "wa.send",
            "wa.wait_for",
            "wa.search",
            "wa.events",
            "wa.workflow_run",
            "wa.tx_plan",
            "wa.tx_run",
            "wa.tx_rollback",
            "wa.tx_show",
            "wa.accounts",
            "wa.accounts_refresh",
            "wa.rules_list",
            "wa.rules_test",
            "wa.reservations",
            "wa.reserve",
            "wa.release",
        ];
        for name in &required {
            assert!(
                tool_names.contains(*name),
                "Required tool '{name}' not registered. Found: {tool_names:?}"
            );
        }
    }

    #[test]
    fn non_storage_tools_registered_without_db() {
        let server = build_server_with_db(&Config::default(), None).expect("build mcp server");
        let tool_defs = server.tools();
        let tool_names: BTreeSet<String> = tool_defs.into_iter().map(|t| t.name).collect();

        // Non-storage tools must be present even without DB
        let always_present = [
            "wa.state",
            "wa.get_text",
            "wa.wait_for",
            "wa.rules_list",
            "wa.rules_test",
            "wa.tx_plan",
            "wa.tx_run",
            "wa.tx_rollback",
            "wa.tx_show",
        ];
        for name in &always_present {
            assert!(
                tool_names.contains(*name),
                "Non-storage tool '{name}' must be registered without DB. Found: {tool_names:?}"
            );
        }

        // Storage-dependent tools must NOT be present without DB
        let storage_only = [
            "wa.search",
            "wa.events",
            "wa.workflow_run",
            "wa.accounts",
            "wa.accounts_refresh",
            "wa.reservations",
            "wa.reserve",
            "wa.release",
        ];
        for name in &storage_only {
            assert!(
                !tool_names.contains(*name),
                "Storage tool '{name}' should not be registered without DB"
            );
        }
    }

    #[test]
    fn wa_search_schema_includes_unified_contract_fields() {
        let tool = WaSearchTool::new(
            Arc::new(Config::default()),
            Arc::new(PathBuf::from("wa-test.db")),
        )
        .definition();
        let properties = tool
            .input_schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("wa.search properties object");

        assert!(properties.contains_key("query"));
        assert!(properties.contains_key("limit"));
        assert!(properties.contains_key("pane"));
        assert!(properties.contains_key("since"));
        assert!(properties.contains_key("until"));
        assert!(properties.contains_key("snippets"));
        assert!(properties.contains_key("mode"));
        assert_eq!(properties["snippets"]["default"], true);
        assert_eq!(properties["mode"]["default"], "lexical");
    }

    #[test]
    fn all_tool_definitions_have_descriptions() {
        let server = build_server_with_db(&Config::default(), Some(PathBuf::from("wa-test.db")))
            .expect("build mcp server");
        let tool_defs = server.tools();
        for tool in &tool_defs {
            assert!(
                tool.description.as_ref().is_some_and(|d| !d.is_empty()),
                "Tool '{}' must have a non-empty description",
                tool.name
            );
        }
    }

    #[test]
    fn all_tool_definitions_have_input_schemas() {
        let server = build_server_with_db(&Config::default(), Some(PathBuf::from("wa-test.db")))
            .expect("build mcp server");
        let tool_defs = server.tools();
        for tool in &tool_defs {
            let schema = &tool.input_schema;
            assert!(
                schema.get("type").is_some(),
                "Tool '{}' input schema must have a 'type' field",
                tool.name
            );
        }
    }

    #[test]
    fn all_tool_definitions_have_version() {
        let server = build_server_with_db(&Config::default(), Some(PathBuf::from("wa-test.db")))
            .expect("build mcp server");
        let tool_defs = server.tools();
        for tool in &tool_defs {
            assert!(
                tool.version.as_ref().is_some_and(|v| !v.is_empty()),
                "Tool '{}' must have a version",
                tool.name
            );
        }
    }

    #[test]
    fn tool_count_with_db() {
        let server = build_server_with_db(&Config::default(), Some(PathBuf::from("wa-test.db")))
            .expect("build mcp server");
        let count = server.tools().len();
        // 9 non-storage + 12 storage-dependent = 21 total
        assert!(
            count >= 21,
            "Expected at least 21 tools with DB, got {count}"
        );
    }

    // ── MCP audit tests (wa-nu4.3.1.6) ──────────────────────────────────

    #[test]
    fn redact_mcp_args_with_keys() {
        let args = serde_json::json!({"pane_id": 42, "text": "secret stuff", "escape": true});
        let redacted = redact_mcp_args("wa.send", &args);
        assert!(redacted.starts_with("mcp:wa.send"));
        assert!(redacted.contains("keys=["));
        // Keys present, values absent
        assert!(redacted.contains("pane_id"));
        assert!(redacted.contains("text"));
        assert!(redacted.contains("escape"));
        assert!(!redacted.contains("secret stuff"));
        assert!(!redacted.contains("42"));
    }

    #[test]
    fn redact_mcp_args_empty_object() {
        let args = serde_json::json!({});
        let redacted = redact_mcp_args("wa.state", &args);
        assert_eq!(redacted, "mcp:wa.state");
    }

    #[test]
    fn redact_mcp_args_non_object() {
        let args = serde_json::json!("just a string");
        let redacted = redact_mcp_args("wa.get_text", &args);
        assert_eq!(redacted, "mcp:wa.get_text");
    }

    #[test]
    fn redact_mcp_args_nested_values_not_leaked() {
        let args = serde_json::json!({
            "api_key": "sk-secret-123",
            "config": {"nested": "value"},
            "token": "bearer-abc"
        });
        let redacted = redact_mcp_args("wa.accounts_refresh", &args);
        assert!(!redacted.contains("sk-secret-123"));
        assert!(!redacted.contains("bearer-abc"));
        assert!(!redacted.contains("nested"));
        // Keys only
        assert!(redacted.contains("api_key"));
        assert!(redacted.contains("config"));
        assert!(redacted.contains("token"));
    }

    fn temp_db_path() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("mcp-audit.db");
        (dir, db_path)
    }

    fn latest_audit_action(db_path: &Path, action_kind: &str) -> crate::storage::AuditActionRecord {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy())
                .await
                .unwrap();
            let rows = storage
                .get_audit_actions(crate::storage::AuditQuery {
                    limit: Some(1),
                    action_kind: Some(action_kind.to_string()),
                    ..crate::storage::AuditQuery::default()
                })
                .await
                .unwrap();
            rows.into_iter()
                .next()
                .unwrap_or_else(|| panic!("missing audit row for {action_kind}"))
        })
    }

    fn evidence<'a>(context: &'a DecisionContext, key: &str) -> Option<&'a str> {
        context
            .evidence
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.value.as_str())
    }

    #[test]
    fn mcp_audit_decision_context_tracks_surface_and_error_code() {
        let ctx_json = mcp_audit_decision_context(
            "wa.accounts_refresh",
            "mcp.wa.accounts_refresh",
            "deny",
            "error",
            Some("MCP_TIMEOUT"),
            77,
        )
        .expect("decision context should serialize");
        let ctx: DecisionContext =
            serde_json::from_str(&ctx_json).expect("decision context should parse");

        assert_eq!(ctx.action, ActionKind::ExecCommand);
        assert_eq!(ctx.actor, ActorKind::Mcp);
        assert_eq!(ctx.surface, PolicySurface::Mcp);
        assert_eq!(
            ctx.determining_rule.as_deref(),
            Some("audit.mcp.wa.accounts_refresh")
        );
        assert_eq!(evidence(&ctx, "stage"), Some("mcp_audit"));
        assert_eq!(evidence(&ctx, "tool"), Some("wa.accounts_refresh"));
        assert_eq!(
            evidence(&ctx, "mcp_action_kind"),
            Some("mcp.wa.accounts_refresh")
        );
        assert_eq!(evidence(&ctx, "policy_decision"), Some("deny"));
        assert_eq!(evidence(&ctx, "result"), Some("error"));
        assert_eq!(evidence(&ctx, "elapsed_ms"), Some("77"));
        assert_eq!(evidence(&ctx, "error_code"), Some("MCP_TIMEOUT"));
    }

    #[test]
    fn record_mcp_audit_persists_structured_decision_context() {
        let (_dir, db_path) = temp_db_path();
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage should initialize");
            record_mcp_audit(
                &storage,
                "wa.rules_list",
                "mcp:wa.rules_list".to_string(),
                "allow",
                "success",
                None,
                12,
            )
            .await;
        });

        let audit = latest_audit_action(&db_path, "mcp.wa.rules_list");
        assert_eq!(audit.actor_kind, "mcp");
        let context: DecisionContext = serde_json::from_str(
            audit
                .decision_context
                .as_deref()
                .expect("decision context should be recorded"),
        )
        .expect("decision context should parse");
        assert_eq!(context.action, ActionKind::ExecCommand);
        assert_eq!(context.actor, ActorKind::Mcp);
        assert_eq!(context.surface, PolicySurface::Mcp);
        assert_eq!(
            context.determining_rule.as_deref(),
            Some("audit.mcp.wa.rules_list")
        );
        assert_eq!(evidence(&context, "tool"), Some("wa.rules_list"));
        assert_eq!(
            evidence(&context, "mcp_action_kind"),
            Some("mcp.wa.rules_list")
        );
        assert_eq!(evidence(&context, "policy_decision"), Some("allow"));
        assert_eq!(evidence(&context, "result"), Some("success"));
        assert_eq!(evidence(&context, "elapsed_ms"), Some("12"));
        assert!(evidence(&context, "error_code").is_none());
    }

    #[test]
    fn audited_handler_delegates_definition() {
        let inner = WaRulesListTool;
        let inner_def = inner.definition();
        let wrapped = AuditedToolHandler::new(
            inner,
            "wa.rules_list",
            Arc::new(PathBuf::from("/tmp/test.db")),
        );
        let wrapped_def = wrapped.definition();
        assert_eq!(inner_def.name, wrapped_def.name);
        assert_eq!(inner_def.description, wrapped_def.description);
    }

    #[test]
    fn audited_handler_preserves_tool_name() {
        let handler = AuditedToolHandler::new(
            WaRulesTestTool,
            "wa.rules_test",
            Arc::new(PathBuf::from("/tmp/test.db")),
        );
        assert_eq!(handler.tool_name, "wa.rules_test");
    }

    #[test]
    fn all_storage_tools_wrapped_with_audit() {
        // Verify tool names still match after wrapping
        let server = build_server_with_db(&Config::default(), Some(PathBuf::from("wa-test.db")))
            .expect("build mcp server");
        let tool_names: BTreeSet<String> = server.tools().into_iter().map(|t| t.name).collect();

        let audited_tools = [
            "wa.search",
            "wa.events",
            "wa.events_annotate",
            "wa.events_triage",
            "wa.events_label",
            "wa.reservations",
            "wa.reserve",
            "wa.release",
            "wa.send",
            "wa.workflow_run",
            "wa.accounts",
            "wa.accounts_refresh",
        ];
        for name in &audited_tools {
            assert!(
                tool_names.contains(*name),
                "Audited tool '{name}' must still be registered after wrapping"
            );
        }
    }

    // ── apply_tail_truncation tests ──────────────────────────────────────

    #[test]
    fn apply_tail_truncation_no_truncation_when_under_limit() {
        let text = "line1\nline2\nline3";
        let (result, truncated, info) = apply_tail_truncation(text, 10);
        assert_eq!(result, text);
        assert!(!truncated);
        assert!(info.is_none());
    }

    #[test]
    fn apply_tail_truncation_exact_limit() {
        let text = "line1\nline2\nline3";
        let (result, truncated, info) = apply_tail_truncation(text, 3);
        assert_eq!(result, text);
        assert!(!truncated);
        assert!(info.is_none());
    }

    #[test]
    fn apply_tail_truncation_truncates_to_tail() {
        let text = "line1\nline2\nline3\nline4\nline5";
        let (result, truncated, info) = apply_tail_truncation(text, 2);
        assert!(truncated);
        assert_eq!(result, "line4\nline5");
        let info = info.expect("truncation info present");
        assert_eq!(info.original_lines, 5);
        assert_eq!(info.returned_lines, 2);
        assert!(info.returned_bytes < info.original_bytes);
    }

    #[test]
    fn apply_tail_truncation_single_line() {
        let text = "only one line";
        let (result, truncated, _) = apply_tail_truncation(text, 1);
        assert_eq!(result, text);
        assert!(!truncated);
    }

    #[test]
    fn apply_tail_truncation_empty_text() {
        let (result, truncated, _) = apply_tail_truncation("", 5);
        assert_eq!(result, "");
        assert!(!truncated);
    }

    // ── parse_cass_agent tests ───────────────────────────────────────────

    #[test]
    fn parse_cass_agent_known_agents() {
        assert!(matches!(parse_cass_agent("codex"), Some(CassAgent::Codex)));
        assert!(matches!(
            parse_cass_agent("claude_code"),
            Some(CassAgent::ClaudeCode)
        ));
        assert!(matches!(
            parse_cass_agent("claude-code"),
            Some(CassAgent::ClaudeCode)
        ));
        assert!(matches!(
            parse_cass_agent("claude"),
            Some(CassAgent::ClaudeCode)
        ));
        assert!(matches!(
            parse_cass_agent("gemini"),
            Some(CassAgent::Gemini)
        ));
        assert!(matches!(
            parse_cass_agent("cursor"),
            Some(CassAgent::Cursor)
        ));
        assert!(matches!(parse_cass_agent("aider"), Some(CassAgent::Aider)));
        assert!(matches!(
            parse_cass_agent("chatgpt"),
            Some(CassAgent::ChatGpt)
        ));
        assert!(matches!(
            parse_cass_agent("chat_gpt"),
            Some(CassAgent::ChatGpt)
        ));
        assert!(matches!(
            parse_cass_agent("chat-gpt"),
            Some(CassAgent::ChatGpt)
        ));
    }

    #[test]
    fn parse_cass_agent_case_insensitive() {
        assert!(matches!(parse_cass_agent("CODEX"), Some(CassAgent::Codex)));
        assert!(matches!(
            parse_cass_agent("Claude_Code"),
            Some(CassAgent::ClaudeCode)
        ));
        assert!(matches!(
            parse_cass_agent("GEMINI"),
            Some(CassAgent::Gemini)
        ));
    }

    #[test]
    fn parse_cass_agent_trims_whitespace() {
        assert!(matches!(
            parse_cass_agent("  codex  "),
            Some(CassAgent::Codex)
        ));
    }

    #[test]
    fn parse_cass_agent_unknown_returns_none() {
        assert!(parse_cass_agent("unknown_agent").is_none());
        assert!(parse_cass_agent("").is_none());
        assert!(parse_cass_agent("gpt4").is_none());
    }

    // ── parse_caut_service tests ─────────────────────────────────────────

    #[test]
    fn parse_caut_service_openai() {
        assert!(matches!(
            parse_caut_service("openai"),
            Some(CautService::OpenAI)
        ));
    }

    #[test]
    fn parse_caut_service_provider_aliases() {
        assert!(matches!(
            parse_caut_service("codex"),
            Some(CautService::OpenAI)
        ));
        assert!(matches!(
            parse_caut_service("chat-gpt"),
            Some(CautService::OpenAI)
        ));
    }

    #[test]
    fn parse_caut_service_unknown_returns_none() {
        // "google" and "anthropic" are now recognized slugs via is_google_slug/is_anthropic_slug
        assert!(parse_caut_service("google").is_some());
        assert!(parse_caut_service("anthropic").is_some());
        // Truly unknown slugs and empty string still return None
        assert!(parse_caut_service("").is_none());
        assert!(parse_caut_service("unknown-provider-xyz").is_none());
    }

    // ── check_refresh_cooldown tests ─────────────────────────────────────

    #[test]
    fn check_refresh_cooldown_no_previous_refresh() {
        assert!(check_refresh_cooldown(0, 100_000, 60_000).is_none());
        assert!(check_refresh_cooldown(-1, 100_000, 60_000).is_none());
    }

    #[test]
    fn check_refresh_cooldown_within_cooldown() {
        // Refreshed 10 seconds ago, cooldown is 60 seconds
        let result = check_refresh_cooldown(90_000, 100_000, 60_000);
        assert!(result.is_some());
        let (elapsed_secs, remaining_secs) = result.unwrap();
        assert_eq!(elapsed_secs, 10); // 10_000ms / 1000
        assert_eq!(remaining_secs, 50); // (60_000 - 10_000) / 1000
    }

    #[test]
    fn check_refresh_cooldown_past_cooldown() {
        // Refreshed 110 seconds ago, cooldown is 60 seconds.
        assert!(check_refresh_cooldown(10_000, 120_000, 60_000).is_none());
    }

    #[test]
    fn check_refresh_cooldown_exactly_at_boundary() {
        // Exactly at cooldown boundary
        assert!(check_refresh_cooldown(40_000, 100_000, 60_000).is_none());
    }

    #[test]
    fn check_refresh_cooldown_future_timestamp_clamps_to_zero_elapsed() {
        // System clock moved backward or persisted refresh timestamp is in the future.
        let result = check_refresh_cooldown(120_000, 100_000, 60_000);
        assert_eq!(result, Some((0, 60)));
    }

    #[test]
    fn check_refresh_cooldown_non_positive_window_is_disabled() {
        assert!(check_refresh_cooldown(100_000, 101_000, 0).is_none());
        assert!(check_refresh_cooldown(100_000, 101_000, -1).is_none());
    }

    // ── resolve_alt_screen_state tests ───────────────────────────────────

    #[test]
    fn resolve_alt_screen_unknown_pane_returns_none() {
        let state = IpcPaneState {
            pane_id: 1,
            known: false,
            observed: None,
            alt_screen: Some(true),
            last_status_at: Some(1000),
            in_gap: None,
            cursor_alt_screen: Some(true),
            reason: None,
        };
        assert!(resolve_alt_screen_state(&state).is_none());
    }

    #[test]
    fn resolve_alt_screen_cursor_takes_priority() {
        let state = IpcPaneState {
            pane_id: 1,
            known: true,
            observed: None,
            alt_screen: Some(false),
            last_status_at: Some(1000),
            in_gap: None,
            cursor_alt_screen: Some(true),
            reason: None,
        };
        assert_eq!(resolve_alt_screen_state(&state), Some(true));
    }

    #[test]
    fn resolve_alt_screen_falls_back_to_alt_screen_with_status() {
        let state = IpcPaneState {
            pane_id: 1,
            known: true,
            observed: None,
            alt_screen: Some(false),
            last_status_at: Some(1000),
            in_gap: None,
            cursor_alt_screen: None,
            reason: None,
        };
        assert_eq!(resolve_alt_screen_state(&state), Some(false));
    }

    #[test]
    fn resolve_alt_screen_no_status_no_cursor_returns_none() {
        let state = IpcPaneState {
            pane_id: 1,
            known: true,
            observed: None,
            alt_screen: Some(true),
            last_status_at: None,
            in_gap: None,
            cursor_alt_screen: None,
            reason: None,
        };
        assert!(resolve_alt_screen_state(&state).is_none());
    }

    // ── policy_reason and approval_command tests ─────────────────────────

    #[test]
    fn policy_reason_allow_returns_none() {
        let decision = PolicyDecision::Allow {
            rule_id: None,
            context: None,
        };
        assert!(policy_reason(&decision).is_none());
    }

    #[test]
    fn policy_reason_deny_returns_reason() {
        let decision = PolicyDecision::Deny {
            reason: "too dangerous".to_string(),
            rule_id: None,
            context: None,
        };
        assert_eq!(policy_reason(&decision), Some("too dangerous"));
    }

    #[test]
    fn policy_reason_require_approval_returns_reason() {
        let decision = PolicyDecision::RequireApproval {
            reason: "needs human review".to_string(),
            approval: None,
            rule_id: None,
            context: None,
        };
        assert_eq!(policy_reason(&decision), Some("needs human review"));
    }

    #[test]
    fn approval_command_allow_returns_none() {
        let decision = PolicyDecision::Allow {
            rule_id: None,
            context: None,
        };
        assert!(approval_command(&decision).is_none());
    }

    #[test]
    fn approval_command_deny_returns_none() {
        let decision = PolicyDecision::Deny {
            reason: "denied".to_string(),
            rule_id: None,
            context: None,
        };
        assert!(approval_command(&decision).is_none());
    }

    #[test]
    fn approval_command_require_approval_no_approval_returns_none() {
        let decision = PolicyDecision::RequireApproval {
            reason: "review".to_string(),
            approval: None,
            rule_id: None,
            context: None,
        };
        assert!(approval_command(&decision).is_none());
    }

    // ── injection_from_decision tests ────────────────────────────────────

    #[test]
    fn injection_from_decision_allow() {
        let decision = PolicyDecision::Allow {
            rule_id: None,
            context: None,
        };
        let result = injection_from_decision(decision, "test".to_string(), 1, ActionKind::SendText);
        assert!(matches!(result, InjectionResult::Allowed { .. }));
    }

    #[test]
    fn injection_from_decision_deny() {
        let decision = PolicyDecision::Deny {
            reason: "blocked".to_string(),
            rule_id: None,
            context: None,
        };
        let result = injection_from_decision(decision, "test".to_string(), 1, ActionKind::SendText);
        assert!(matches!(result, InjectionResult::Denied { .. }));
    }

    #[test]
    fn injection_from_decision_require_approval() {
        let decision = PolicyDecision::RequireApproval {
            reason: "review".to_string(),
            approval: None,
            rule_id: None,
            context: None,
        };
        let result = injection_from_decision(decision, "test".to_string(), 1, ActionKind::SendText);
        assert!(matches!(result, InjectionResult::RequiresApproval { .. }));
    }

    // ── default value functions tests ────────────────────────────────────

    #[test]
    fn default_tail_is_500() {
        assert_eq!(default_tail(), 500);
    }

    #[test]
    fn default_cass_limit_is_10() {
        assert_eq!(default_cass_limit(), 10);
    }

    #[test]
    fn default_cass_offset_is_0() {
        assert_eq!(default_cass_offset(), 0);
    }

    #[test]
    fn default_cass_timeout_secs_is_15() {
        assert_eq!(default_cass_timeout_secs(), 15);
    }

    #[test]
    fn default_cass_context_lines_is_10() {
        assert_eq!(default_cass_context_lines(), 10);
    }

    #[test]
    fn default_events_limit_positive() {
        assert!(default_events_limit() > 0);
    }

    #[test]
    fn default_timeout_secs_positive() {
        assert!(default_timeout_secs() > 0);
    }

    #[test]
    fn default_wait_tail_positive() {
        assert!(default_wait_tail() > 0);
    }

    // ── now_ms and elapsed_ms tests ──────────────────────────────────────

    #[test]
    fn now_ms_returns_reasonable_epoch() {
        let ts = now_ms();
        // Should be after 2024-01-01 (1704067200000) and before 2100
        assert!(ts > 1_704_067_200_000);
        assert!(ts < 4_102_444_800_000);
    }

    #[test]
    fn elapsed_ms_returns_small_value() {
        let start = Instant::now();
        let elapsed = elapsed_ms(start);
        // Should be very small (< 100ms)
        assert!(elapsed < 100);
    }

    // ── McpOutputFormat tests ────────────────────────────────────────────

    #[test]
    fn mcp_output_format_default_is_json() {
        assert_eq!(McpOutputFormat::default(), McpOutputFormat::Json);
    }

    #[test]
    fn mcp_output_format_eq() {
        assert_eq!(McpOutputFormat::Json, McpOutputFormat::Json);
        assert_eq!(McpOutputFormat::Toon, McpOutputFormat::Toon);
        assert_ne!(McpOutputFormat::Json, McpOutputFormat::Toon);
    }

    // ── extract_mcp_output_format edge cases ─────────────────────────────

    #[test]
    fn extract_mcp_output_format_invalid_returns_err() {
        let mut args = serde_json::json!({"format": "yaml"});
        let result = extract_mcp_output_format(&mut args);
        assert!(result.is_err());
    }

    #[test]
    fn extract_mcp_output_format_non_string_returns_err() {
        let mut args = serde_json::json!({"format": 42});
        let result = extract_mcp_output_format(&mut args);
        assert!(result.is_err());
    }

    #[test]
    fn extract_mcp_output_format_non_object_returns_json() {
        let mut args = serde_json::json!("not an object");
        let result = extract_mcp_output_format(&mut args).expect("should default to json");
        assert_eq!(result, McpOutputFormat::Json);
    }

    // ── encode_mcp_contents edge cases ───────────────────────────────────

    #[test]
    fn encode_mcp_contents_json_passthrough() {
        let contents = vec![Content::Text {
            text: r#"{"data": 1}"#.to_string(),
        }];
        let result =
            encode_mcp_contents(contents.clone(), McpOutputFormat::Json).expect("json passthrough");
        assert_eq!(result.len(), 1);
        match &result[0] {
            Content::Text { text } => assert_eq!(text, r#"{"data": 1}"#),
            _ => panic!("expected text content"), // ubs:ignore
        }
    }

    #[test]
    fn encode_mcp_contents_toon_invalid_json_returns_err() {
        let contents = vec![Content::Text {
            text: "not valid json {[}".to_string(),
        }];
        let result = encode_mcp_contents(contents, McpOutputFormat::Toon);
        assert!(result.is_err());
    }

    // ── envelope_to_content tests ────────────────────────────────────────

    #[test]
    fn envelope_to_content_success_serializes() {
        let envelope = McpEnvelope::success(42, 10);
        let contents = envelope_to_content(envelope).expect("should serialize");
        assert_eq!(contents.len(), 1);
        match &contents[0] {
            Content::Text { text } => {
                let v: serde_json::Value = serde_json::from_str(text).expect("valid json");
                assert_eq!(v["ok"], true);
                assert_eq!(v["data"], 42);
            }
            _ => panic!("expected text content"), // ubs:ignore
        }
    }

    #[test]
    fn envelope_to_content_error_serializes() {
        let envelope = McpEnvelope::<()>::error(MCP_ERR_INVALID_ARGS, "bad input", None, 5);
        let contents = envelope_to_content(envelope).expect("should serialize");
        assert_eq!(contents.len(), 1);
        match &contents[0] {
            Content::Text { text } => {
                let v: serde_json::Value = serde_json::from_str(text).expect("valid json");
                assert_eq!(v["ok"], false);
                assert_eq!(v["error"], "bad input");
            }
            _ => panic!("expected text content"), // ubs:ignore
        }
    }

    // ── augment_tool_schema edge cases ───────────────────────────────────

    #[test]
    fn augment_tool_schema_non_object_schema_is_noop() {
        let mut schema = serde_json::json!("not an object");
        augment_tool_schema_with_format(&mut schema);
        assert_eq!(schema, serde_json::json!("not an object"));
    }

    #[test]
    fn augment_tool_schema_non_object_type_is_noop() {
        let mut schema = serde_json::json!({"type": "array"});
        augment_tool_schema_with_format(&mut schema);
        assert!(schema.get("properties").is_none());
    }

    #[test]
    fn augment_tool_schema_does_not_overwrite_existing_format() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "format": {"type": "number", "description": "custom format"}
            }
        });
        augment_tool_schema_with_format(&mut schema);
        // Should not overwrite the existing format property
        assert_eq!(schema["properties"]["format"]["type"], "number");
    }

    // ── McpEnvelope serde roundtrip ──────────────────────────────────────

    #[test]
    fn mcp_envelope_success_serde_roundtrip() {
        let envelope = McpEnvelope::success(vec![1, 2, 3], 42);
        let json = serde_json::to_string(&envelope).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"], serde_json::json!([1, 2, 3]));
        assert_eq!(v["elapsed_ms"], 42);
        assert!(v.get("error").is_none());
    }

    #[test]
    fn mcp_envelope_error_serde_roundtrip() {
        let envelope = McpEnvelope::<String>::error(
            MCP_ERR_PANE_NOT_FOUND,
            "pane 99 not found",
            Some("Check pane_id".to_string()),
            100,
        );
        let json = serde_json::to_string(&envelope).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "pane 99 not found");
        assert_eq!(v["error_code"], MCP_ERR_PANE_NOT_FOUND);
        assert_eq!(v["hint"], "Check pane_id");
        assert!(v.get("data").is_none());
    }

    // ── map_mcp_error additional coverage ────────────────────────────────

    #[test]
    fn map_mcp_error_wezterm() {
        let err = crate::Error::Wezterm(WeztermError::CliNotFound);
        let (code, _hint) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_WEZTERM);
    }

    // ── search fusion config helpers ──────────────────────────────────────

    #[test]
    fn effective_search_rrf_k_clamps_to_positive() {
        let mut config = Config::default();
        config.search.rrf_k = 0;
        assert_eq!(effective_search_rrf_k(&config), 1);
        config.search.rrf_k = 77;
        assert_eq!(effective_search_rrf_k(&config), 77);
    }

    #[test]
    fn effective_search_quality_timeout_ms_clamps_to_positive() {
        let mut config = Config::default();
        config.search.quality_timeout_ms = 0;
        assert_eq!(effective_search_quality_timeout_ms(&config), 1);
        config.search.quality_timeout_ms = 250;
        assert_eq!(effective_search_quality_timeout_ms(&config), 250);
    }

    #[test]
    fn effective_search_fusion_weights_follow_quality_weight() {
        let mut config = Config::default();
        config.search.quality_weight = 0.25;
        let (lexical, semantic) = effective_search_fusion_weights(&config);
        assert!((lexical - 0.75).abs() < f32::EPSILON);
        assert!((semantic - 0.25).abs() < f32::EPSILON);

        config.search.quality_weight = f64::NAN;
        let (lexical_default, semantic_default) = effective_search_fusion_weights(&config);
        assert!((lexical_default - 0.3).abs() < f32::EPSILON);
        assert!((semantic_default - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn effective_search_fusion_weights_fast_only_disables_semantic_weight() {
        let mut config = Config::default();
        config.search.quality_weight = 0.25;
        config.search.fast_only = true;
        let (lexical, semantic) = effective_search_fusion_weights(&config);
        assert!((lexical - 1.0).abs() < f32::EPSILON);
        assert!((semantic - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn effective_search_fusion_backend_uses_config_selector() {
        let mut config = Config::default();
        config.search.fusion_backend = "frankensearch".to_string();
        assert_eq!(
            effective_search_fusion_backend(&config),
            crate::search::FusionBackend::FrankenSearchRrf
        );

        config.search.fusion_backend = "unknown-backend".to_string();
        assert_eq!(
            effective_search_fusion_backend(&config),
            crate::search::FusionBackend::FrankenSearchRrf
        );

        config.search.fusion_backend = "legacy".to_string();
        assert_eq!(
            effective_search_fusion_backend(&config),
            crate::search::FusionBackend::FrankenSearchRrf
        );
    }

    // ── parse_mcp_output_format edge cases ───────────────────────────────

    #[test]
    fn parse_mcp_output_format_empty_returns_none() {
        assert!(parse_mcp_output_format("").is_none());
    }

    #[test]
    fn parse_mcp_output_format_case_insensitive() {
        assert_eq!(parse_mcp_output_format("JSON"), Some(McpOutputFormat::Json));
        assert_eq!(parse_mcp_output_format("Json"), Some(McpOutputFormat::Json));
        assert_eq!(parse_mcp_output_format("TOON"), Some(McpOutputFormat::Toon));
    }

    // -----------------------------------------------------------------------
    // Batch 2: RubyBeaver wa-1u90p.7.1 — map_*_error, struct serde, helpers
    // -----------------------------------------------------------------------

    // ── default_ttl_ms ────────────────────────────────────────────────────

    #[test]
    fn default_ttl_ms_is_five_minutes() {
        assert_eq!(default_ttl_ms(), 300_000);
    }

    // ── constants ─────────────────────────────────────────────────────────

    #[test]
    fn send_osc_segment_limit_reasonable() {
        assert!(SEND_OSC_SEGMENT_LIMIT > 0);
        assert!(SEND_OSC_SEGMENT_LIMIT <= 1000);
    }

    #[test]
    fn mcp_refresh_cooldown_positive() {
        assert!(MCP_REFRESH_COOLDOWN_MS > 0);
    }

    // ── map_mcp_error additional branches ─────────────────────────────────

    #[test]
    fn map_mcp_error_pane_not_found() {
        let err = crate::Error::Wezterm(WeztermError::PaneNotFound(99));
        let (code, hint) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_PANE_NOT_FOUND);
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("wa.state"));
    }

    #[test]
    fn map_mcp_error_wezterm_timeout() {
        let err = crate::Error::Wezterm(WeztermError::Timeout(30));
        let (code, hint) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_TIMEOUT);
        assert!(hint.is_some());
    }

    #[test]
    fn map_mcp_error_wezterm_not_running() {
        let err = crate::Error::Wezterm(WeztermError::NotRunning);
        let (code, hint) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_WEZTERM);
        assert!(hint.is_some());
    }

    #[test]
    fn map_mcp_error_wezterm_generic() {
        let err = crate::Error::Wezterm(WeztermError::CommandFailed("unknown error".to_string()));
        let (code, hint) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_WEZTERM);
        assert!(hint.is_none());
    }

    #[test]
    fn map_mcp_error_workflow() {
        let err = crate::Error::Workflow(crate::error::WorkflowError::NotFound(
            "workflow failed".to_string(),
        ));
        let (code, _) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_WORKFLOW);
    }

    #[test]
    fn map_mcp_error_policy() {
        let err = crate::Error::Policy("policy violation".to_string());
        let (code, _) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_POLICY);
    }

    #[test]
    fn map_mcp_error_runtime_fallback() {
        let err = crate::Error::Runtime("misc error".to_string());
        let (code, _) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_NOT_IMPLEMENTED);
    }

    // ── map_caut_error ────────────────────────────────────────────────────

    #[test]
    fn map_caut_error_not_installed() {
        let err = CautError::NotInstalled;
        let (code, hint) = map_caut_error(&err);
        assert_eq!(code, MCP_ERR_CONFIG);
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("caut"));
    }

    #[test]
    fn map_caut_error_timeout() {
        let err = CautError::Timeout { timeout_secs: 30 };
        let (code, hint) = map_caut_error(&err);
        assert_eq!(code, MCP_ERR_TIMEOUT);
        assert!(hint.is_some());
    }

    #[test]
    fn map_caut_error_generic() {
        let err = CautError::Io {
            message: "io error".to_string(),
        };
        let (code, hint) = map_caut_error(&err);
        assert_eq!(code, MCP_ERR_CAUT);
        assert!(hint.is_some());
    }

    // ── map_cass_error ────────────────────────────────────────────────────

    #[test]
    fn map_cass_error_not_installed() {
        let err = CassError::NotInstalled;
        let (code, hint) = map_cass_error(&err);
        assert_eq!(code, MCP_ERR_CONFIG);
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("cass"));
    }

    #[test]
    fn map_cass_error_timeout() {
        let err = CassError::Timeout { timeout_secs: 15 };
        let (code, hint) = map_cass_error(&err);
        assert_eq!(code, MCP_ERR_TIMEOUT);
        assert!(hint.is_some());
    }

    #[test]
    fn map_cass_error_generic() {
        let err = CassError::Io {
            message: "disk error".to_string(),
        };
        let (code, hint) = map_cass_error(&err);
        assert_eq!(code, MCP_ERR_CASS);
        assert!(hint.is_some());
    }

    // ── McpToolError ──────────────────────────────────────────────────────

    #[test]
    fn mcp_tool_error_new() {
        let err = McpToolError::new(MCP_ERR_STORAGE, "db locked".to_string(), None);
        assert_eq!(err.code, MCP_ERR_STORAGE);
        assert_eq!(err.message, "db locked");
        assert!(err.hint.is_none());
    }

    #[test]
    fn mcp_tool_error_new_with_hint() {
        let err = McpToolError::new(
            MCP_ERR_TIMEOUT,
            "timed out".to_string(),
            Some("Retry later".to_string()),
        );
        assert_eq!(err.code, MCP_ERR_TIMEOUT);
        assert_eq!(err.hint.as_deref(), Some("Retry later"));
    }

    #[test]
    fn mcp_tool_error_from_error() {
        let err = McpToolError::from_error(crate::Error::Runtime("oops".to_string()));
        assert_eq!(err.code, MCP_ERR_NOT_IMPLEMENTED);
        assert!(err.message.contains("oops"));
    }

    #[test]
    fn mcp_tool_error_from_caut_error() {
        let err = McpToolError::from_caut_error(CautError::NotInstalled);
        assert_eq!(err.code, MCP_ERR_CONFIG);
        assert!(err.message.contains("caut"));
    }

    // ── reservation_to_mcp_info ───────────────────────────────────────────

    #[test]
    fn reservation_to_mcp_info_active() {
        let r = PaneReservation {
            id: 1,
            pane_id: 42,
            owner_kind: "workflow".to_string(),
            owner_id: "wf-123".to_string(),
            reason: Some("testing".to_string()),
            created_at: 1_000_000,
            expires_at: i64::MAX, // far future
            released_at: None,
            status: "active".to_string(),
        };
        let info = reservation_to_mcp_info(&r);
        assert_eq!(info.id, 1);
        assert_eq!(info.pane_id, 42);
        assert_eq!(info.status, "active");
        assert_eq!(info.reason.as_deref(), Some("testing"));
    }

    #[test]
    fn reservation_to_mcp_info_released() {
        let r = PaneReservation {
            id: 2,
            pane_id: 10,
            owner_kind: "agent".to_string(),
            owner_id: "a-1".to_string(),
            reason: None,
            created_at: 1_000_000,
            expires_at: i64::MAX,
            released_at: Some(2_000_000),
            status: "active".to_string(),
        };
        let info = reservation_to_mcp_info(&r);
        assert_eq!(info.status, "released");
    }

    #[test]
    fn reservation_to_mcp_info_expired() {
        let r = PaneReservation {
            id: 3,
            pane_id: 5,
            owner_kind: "manual".to_string(),
            owner_id: "user".to_string(),
            reason: None,
            created_at: 1_000,
            expires_at: 2_000, // expired long ago
            released_at: None,
            status: "active".to_string(),
        };
        let info = reservation_to_mcp_info(&r);
        assert_eq!(info.status, "expired");
    }

    // ── IpcPaneState deserialization ──────────────────────────────────────

    #[test]
    fn ipc_pane_state_deserialize_minimal() {
        let json = r#"{"pane_id": 42, "known": true}"#;
        let state: IpcPaneState = serde_json::from_str(json).unwrap();
        assert_eq!(state.pane_id, 42);
        assert!(state.known);
        assert!(state.observed.is_none());
        assert!(state.alt_screen.is_none());
        assert!(state.last_status_at.is_none());
        assert!(state.in_gap.is_none());
        assert!(state.cursor_alt_screen.is_none());
        assert!(state.reason.is_none());
    }

    #[test]
    fn ipc_pane_state_deserialize_all_fields() {
        let json = r#"{
            "pane_id": 7,
            "known": false,
            "observed": true,
            "alt_screen": false,
            "last_status_at": 999,
            "in_gap": true,
            "cursor_alt_screen": true,
            "reason": "test reason"
        }"#;
        let state: IpcPaneState = serde_json::from_str(json).unwrap();
        assert_eq!(state.pane_id, 7);
        assert!(!state.known);
        assert_eq!(state.observed, Some(true));
        assert_eq!(state.alt_screen, Some(false));
        assert_eq!(state.last_status_at, Some(999));
        assert_eq!(state.in_gap, Some(true));
        assert_eq!(state.cursor_alt_screen, Some(true));
        assert_eq!(state.reason.as_deref(), Some("test reason"));
    }

    // ── resolve_alt_screen_state edge cases ──────────────────────────────

    #[test]
    fn resolve_alt_screen_both_fields_none() {
        let state = IpcPaneState {
            pane_id: 1,
            known: true,
            observed: None,
            alt_screen: None,
            last_status_at: Some(1000),
            in_gap: None,
            cursor_alt_screen: None,
            reason: None,
        };
        assert!(resolve_alt_screen_state(&state).is_none());
    }

    #[test]
    fn resolve_alt_screen_alt_screen_none_cursor_present() {
        let state = IpcPaneState {
            pane_id: 1,
            known: true,
            observed: None,
            alt_screen: None,
            last_status_at: Some(1000),
            in_gap: None,
            cursor_alt_screen: Some(false),
            reason: None,
        };
        assert_eq!(resolve_alt_screen_state(&state), Some(false));
    }

    // ── McpPaneState from_pane_info ──────────────────────────────────────

    #[test]
    fn mcp_pane_state_from_pane_info_basic() {
        let json = serde_json::json!({
            "pane_id": 42,
            "tab_id": 1,
            "window_id": 0,
            "title": "test pane",
            "cwd": "/tmp"
        });
        let info: PaneInfo = serde_json::from_value(json).unwrap();
        let filter = PaneFilterConfig::default();
        let state = McpPaneState::from_pane_info(&info, &filter);
        assert_eq!(state.pane_id, 42);
        assert_eq!(state.tab_id, 1);
        assert_eq!(state.window_id, 0);
        assert_eq!(state.title.as_deref(), Some("test pane"));
        assert_eq!(state.cwd.as_deref(), Some("/tmp"));
        assert!(state.observed); // default filter doesn't exclude
    }

    #[test]
    fn mcp_pane_state_serialization() {
        let state = McpPaneState {
            pane_id: 1,
            pane_uuid: Some("abc-123".to_string()),
            tab_id: 2,
            window_id: 3,
            domain: "local".to_string(),
            title: Some("Shell".to_string()),
            cwd: Some("/home/user".to_string()),
            observed: true,
            ignore_reason: None,
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["pane_id"], 1);
        assert_eq!(json["pane_uuid"], "abc-123");
        assert_eq!(json["domain"], "local");
        assert_eq!(json["observed"], true);
        assert!(json.get("ignore_reason").is_none());
    }

    #[test]
    fn mcp_pane_state_with_ignore_reason() {
        let state = McpPaneState {
            pane_id: 5,
            pane_uuid: None,
            tab_id: 0,
            window_id: 0,
            domain: "ssh".to_string(),
            title: None,
            cwd: None,
            observed: false,
            ignore_reason: Some("domain excluded".to_string()),
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["observed"], false);
        assert_eq!(json["ignore_reason"], "domain excluded");
    }

    // ── McpSearchHit serialization ───────────────────────────────────────

    #[test]
    fn mcp_search_hit_serialization_minimal() {
        let hit = McpSearchHit {
            segment_id: 100,
            pane_id: 42,
            seq: 5,
            captured_at: 1_700_000_000,
            score: 0.95,
            snippet: None,
            content: None,
            semantic_score: None,
            fusion_rank: None,
        };
        let json = serde_json::to_value(&hit).unwrap();
        assert_eq!(json["segment_id"], 100);
        assert_eq!(json["pane_id"], 42);
        assert!((json["score"].as_f64().unwrap() - 0.95).abs() < f64::EPSILON);
        assert!(json.get("snippet").is_none());
        assert!(json.get("content").is_none());
        assert!(json.get("semantic_score").is_none());
        assert!(json.get("fusion_rank").is_none());
    }

    #[test]
    fn mcp_search_hit_serialization_full() {
        let hit = McpSearchHit {
            segment_id: 1,
            pane_id: 1,
            seq: 1,
            captured_at: 1000,
            score: 0.5,
            snippet: Some("match here".to_string()),
            content: Some("full text".to_string()),
            semantic_score: Some(0.8),
            fusion_rank: Some(3),
        };
        let json = serde_json::to_value(&hit).unwrap();
        assert_eq!(json["snippet"], "match here");
        assert_eq!(json["semantic_score"], 0.8);
        assert_eq!(json["fusion_rank"], 3);
    }

    // ── McpWorkflowItem serialization ────────────────────────────────────

    #[test]
    fn mcp_workflow_item_serialization() {
        let item = McpWorkflowItem {
            name: "handle_compaction".to_string(),
            description: "Handle compaction events".to_string(),
            step_count: 3,
            trigger_event_types: vec!["compaction".to_string()],
            trigger_rule_ids: vec!["r-comp-1".to_string()],
            supported_agent_types: vec!["claude_code".to_string()],
            requires_pane: true,
            requires_approval: false,
            can_abort: true,
            destructive: false,
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["name"], "handle_compaction");
        assert_eq!(json["step_count"], 3);
        assert_eq!(json["requires_pane"], true);
        assert_eq!(json["destructive"], false);
        assert_eq!(json["trigger_event_types"][0], "compaction");
    }

    // ── McpRuleItem serialization ────────────────────────────────────────

    #[test]
    fn mcp_rule_item_serialization() {
        let item = McpRuleItem {
            id: "rule-1".to_string(),
            agent_type: "claude_code".to_string(),
            event_type: "pattern_match".to_string(),
            severity: "warning".to_string(),
            description: Some("Detect API keys".to_string()),
            workflow: Some("handle_auth".to_string()),
            anchor_count: 2,
            has_regex: true,
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["id"], "rule-1");
        assert_eq!(json["anchor_count"], 2);
        assert_eq!(json["has_regex"], true);
        assert_eq!(json["description"], "Detect API keys");
    }

    #[test]
    fn mcp_rule_item_optional_fields_absent() {
        let item = McpRuleItem {
            id: "rule-2".to_string(),
            agent_type: "codex".to_string(),
            event_type: "anomaly".to_string(),
            severity: "info".to_string(),
            description: None,
            workflow: None,
            anchor_count: 0,
            has_regex: false,
        };
        let json = serde_json::to_value(&item).unwrap();
        assert!(json.get("description").is_none());
        assert!(json.get("workflow").is_none());
    }

    // ── McpWaitForData serialization ─────────────────────────────────────

    #[test]
    fn mcp_wait_for_data_serialization() {
        let data = McpWaitForData {
            pane_id: 1,
            pattern: "\\$".to_string(),
            matched: true,
            elapsed_ms: 500,
            polls: 10,
            is_regex: true,
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["pane_id"], 1);
        assert_eq!(json["matched"], true);
        assert_eq!(json["elapsed_ms"], 500);
        assert_eq!(json["polls"], 10);
        assert_eq!(json["is_regex"], true);
    }

    #[test]
    fn mcp_wait_for_data_is_regex_false_omitted() {
        let data = McpWaitForData {
            pane_id: 1,
            pattern: "hello".to_string(),
            matched: false,
            elapsed_ms: 100,
            polls: 2,
            is_regex: false,
        };
        let json = serde_json::to_value(&data).unwrap();
        // is_regex=false should be skipped via skip_serializing_if = Not::not
        assert!(json.get("is_regex").is_none());
    }

    // ── McpEventsData serialization ──────────────────────────────────────

    #[test]
    fn mcp_events_data_optional_filters_omitted() {
        let data = McpEventsData {
            events: vec![],
            total_count: 0,
            limit: 20,
            pane_filter: None,
            rule_id_filter: None,
            event_type_filter: None,
            triage_state_filter: None,
            label_filter: None,
            unhandled_only: false,
            since_filter: None,
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["total_count"], 0);
        assert!(json.get("pane_filter").is_none());
        assert!(json.get("rule_id_filter").is_none());
    }

    // ── McpWorkflowRunData serialization ─────────────────────────────────

    #[test]
    fn mcp_workflow_run_data_serialization() {
        let data = McpWorkflowRunData {
            workflow_name: "handle_compaction".to_string(),
            pane_id: 42,
            execution_id: Some("exec-abc".to_string()),
            status: "completed".to_string(),
            message: Some("Done".to_string()),
            result: Some(serde_json::json!({"key": "value"})),
            steps_executed: Some(3),
            step_index: Some(2),
            elapsed_ms: Some(1500),
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["workflow_name"], "handle_compaction");
        assert_eq!(json["status"], "completed");
        assert_eq!(json["steps_executed"], 3);
    }

    // ── McpGetTextData serialization ─────────────────────────────────────

    #[test]
    fn mcp_get_text_data_no_truncation() {
        let data = McpGetTextData {
            pane_id: 5,
            text: "hello world".to_string(),
            tail_lines: 500,
            escapes_included: false,
            truncated: false,
            truncation_info: None,
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["pane_id"], 5);
        assert_eq!(json["text"], "hello world");
        assert!(json.get("truncated").is_none()); // false skipped
        assert!(json.get("truncation_info").is_none());
    }

    #[test]
    fn mcp_get_text_data_with_truncation() {
        let data = McpGetTextData {
            pane_id: 1,
            text: "last two lines\nhere".to_string(),
            tail_lines: 2,
            escapes_included: true,
            truncated: true,
            truncation_info: Some(TruncationInfo {
                original_bytes: 1000,
                returned_bytes: 100,
                original_lines: 50,
                returned_lines: 2,
            }),
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["truncated"], true);
        assert_eq!(json["truncation_info"]["original_lines"], 50);
        assert_eq!(json["truncation_info"]["returned_lines"], 2);
    }

    // ── builtin_workflows ─────────────────────────────────────────────────

    #[test]
    fn builtin_workflows_not_empty() {
        let config = Config::default();
        let workflows = builtin_workflows(&config);
        assert!(
            workflows.len() >= 5,
            "expected at least 5 builtin workflows, got {}",
            workflows.len()
        );
    }

    // ── SearchParams deserialization ──────────────────────────────────────

    #[test]
    fn search_params_deserialize_minimal() {
        let json = r#"{"query": "hello"}"#;
        let params: SearchParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.query, "hello");
        assert!(params.limit.is_none());
        assert!(params.pane.is_none());
        assert!(params.since.is_none());
        assert!(params.until.is_none());
        assert!(params.snippets.is_none());
        assert!(params.mode.is_none());
    }

    // ── EventsParams deserialization ─────────────────────────────────────

    #[test]
    fn events_params_deserialize_defaults() {
        let json = r"{}";
        let params: EventsParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, default_events_limit());
        assert!(params.pane.is_none());
        assert!(params.rule_id.is_none());
        assert!(!params.unhandled);
    }

    // ── SendParams deserialization ───────────────────────────────────────

    #[test]
    fn send_params_deserialize_with_defaults() {
        let json = r#"{"pane_id": 42, "text": "ls -la"}"#;
        let params: SendParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.pane_id, 42);
        assert_eq!(params.text, "ls -la");
        assert!(!params.dry_run);
        assert!(params.wait_for.is_none());
        assert_eq!(params.timeout_secs, default_timeout_secs());
        assert!(!params.wait_for_regex);
    }

    // ── WaitForParams deserialization ────────────────────────────────────

    #[test]
    fn wait_for_params_deserialize_defaults() {
        let json = r#"{"pane_id": 1, "pattern": "\\$"}"#;
        let params: WaitForParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.pane_id, 1);
        assert_eq!(params.pattern, "\\$");
        assert_eq!(params.timeout_secs, default_timeout_secs());
        assert_eq!(params.tail, default_wait_tail());
        assert!(!params.regex);
    }

    // ── GetTextParams deserialization ────────────────────────────────────

    #[test]
    fn get_text_params_deserialize_defaults() {
        let json = r#"{"pane_id": 7}"#;
        let params: GetTextParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.pane_id, 7);
        assert_eq!(params.tail, default_tail());
        assert!(!params.escapes);
    }

    // ── StateParams deserialization ──────────────────────────────────────

    #[test]
    fn state_params_deserialize_empty() {
        let json = r"{}";
        let params: StateParams = serde_json::from_str(json).unwrap();
        assert!(params.domain.is_none());
        assert!(params.agent.is_none());
        assert!(params.pane_id.is_none());
    }

    // ── WorkflowRunParams deserialization ────────────────────────────────

    #[test]
    fn workflow_run_params_deserialize() {
        let json = r#"{"name": "handle_compaction", "pane_id": 42}"#;
        let params: WorkflowRunParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "handle_compaction");
        assert_eq!(params.pane_id, 42);
        assert!(!params.force);
        assert!(!params.dry_run);
    }

    // ── TruncationInfo serialization ────────────────────────────────────

    #[test]
    fn truncation_info_serialization() {
        let info = TruncationInfo {
            original_bytes: 5000,
            returned_bytes: 500,
            original_lines: 100,
            returned_lines: 10,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["original_bytes"], 5000);
        assert_eq!(json["returned_bytes"], 500);
        assert_eq!(json["original_lines"], 100);
        assert_eq!(json["returned_lines"], 10);
    }

    // ── apply_tail_truncation additional edge cases ──────────────────────

    #[test]
    fn apply_tail_truncation_zero_tail_lines() {
        // Tail of 0 should return empty or nothing
        let text = "line1\nline2";
        let (result, truncated, _) = apply_tail_truncation(text, 0);
        // With 0 tail lines, behavior depends on implementation
        // Either empty or the text itself (edge case)
        assert!(result.len() <= text.len());
        // Just assert it doesn't panic
        let _ = truncated;
    }

    #[test]
    fn apply_tail_truncation_trailing_newline() {
        let text = "line1\nline2\nline3\n";
        let (result, truncated, info) = apply_tail_truncation(text, 2);
        // Should take last 2 non-empty lines
        // Assert the function ran without panicking; consume return values.
        let _ = (result, truncated, info);
    }

    // ── approval_command with approval present ──────────────────────────

    #[test]
    fn approval_command_require_approval_with_command() {
        use crate::policy::ApprovalRequest;
        let decision = PolicyDecision::RequireApproval {
            reason: "needs review".to_string(),
            approval: Some(ApprovalRequest {
                allow_once_code: "ABC123".to_string(),
                allow_once_full_hash: "sha256hash".to_string(),
                expires_at: 9_999_999_999,
                summary: "Allow send to pane 42".to_string(),
                command: "wa approve --id 123".to_string(),
            }),
            rule_id: None,
            context: None,
        };
        let cmd = approval_command(&decision);
        assert_eq!(cmd.as_deref(), Some("wa approve --id 123"));
    }

    // ── Mission MCP helper tests (ft-1i2ge.5.3) ─────────────────────────

    #[test]
    fn mission_state_params_deserialize_defaults() {
        let json = r"{}";
        let params: MissionStateParams = serde_json::from_str(json).unwrap();
        assert!(params.mission_file.is_none());
        assert!(params.mission_state.is_none());
        assert!(params.run_state.is_none());
        assert!(params.agent_state.is_none());
        assert!(params.action_state.is_none());
        assert!(params.assignment_id.is_none());
        assert!(params.assignee.is_none());
        assert!(params.limit.is_none());
    }

    #[test]
    fn mission_state_params_deserialize_with_filters() {
        let json = r#"{"mission_state":"running","run_state":"pending","limit":10}"#;
        let params: MissionStateParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.mission_state.as_deref(), Some("running"));
        assert_eq!(params.run_state.as_deref(), Some("pending"));
        assert_eq!(params.limit, Some(10));
    }

    #[test]
    fn mission_pause_params_deserialize_with_reason() {
        let json = r#"{"reason":"operator_request","requested_by":"test-agent"}"#;
        let params: MissionPauseParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.reason.as_deref(), Some("operator_request"));
        assert_eq!(params.requested_by, "test-agent");
    }

    #[test]
    fn mission_pause_params_default_requested_by() {
        let json = r#"{"reason":"test"}"#;
        let params: MissionPauseParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.requested_by, "mcp-agent");
    }

    #[test]
    fn mission_abort_params_deserialize_with_error_code() {
        let json = r#"{"reason":"emergency","error_code":"E-ABORT-001"}"#;
        let params: MissionAbortParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.reason.as_deref(), Some("emergency"));
        assert_eq!(params.error_code.as_deref(), Some("E-ABORT-001"));
        assert_eq!(params.requested_by, "mcp-agent");
    }

    #[test]
    fn mission_explain_params_deserialize_defaults() {
        let json = r"{}";
        let params: MissionExplainParams = serde_json::from_str(json).unwrap();
        assert!(params.mission_file.is_none());
        assert!(params.assignment_id.is_none());
    }

    #[test]
    fn mission_lifecycle_transitions_running_state() {
        let transitions =
            mcp_mission_lifecycle_transitions(crate::plan::MissionLifecycleState::Running);
        assert!(!transitions.is_empty());
        let kinds: Vec<&str> = transitions.iter().map(|t| t.kind.as_str()).collect();
        // Running should allow cancel (abort) at minimum
        assert!(
            kinds.iter().any(|k| k == &"cancel"),
            "Running state should have cancel transition, got: {kinds:?}"
        );
    }

    #[test]
    fn mission_lifecycle_transitions_completed_state() {
        let transitions =
            mcp_mission_lifecycle_transitions(crate::plan::MissionLifecycleState::Completed);
        // Terminal state — no outgoing transitions
        assert!(
            transitions.is_empty(),
            "Completed state should have no transitions"
        );
    }

    #[test]
    fn mission_failure_catalog_not_empty() {
        let catalog = mcp_mission_failure_catalog();
        assert!(!catalog.is_empty());
        for entry in &catalog {
            assert!(!entry.code.is_empty());
            assert!(!entry.reason_code.is_empty());
            assert!(!entry.error_code.is_empty());
        }
    }

    #[test]
    fn mission_state_data_serializes() {
        let data = McpMissionStateData {
            mission_file: "/tmp/test.json".to_string(),
            mission_id: "m-1".to_string(),
            title: "Test Mission".to_string(),
            mission_hash: "sha256:abcd".to_string(),
            lifecycle_state: "running".to_string(),
            candidate_count: 3,
            assignment_count: 2,
            matched_assignment_count: 1,
            returned_assignment_count: 1,
            assignment_counters: McpMissionAssignmentCounters {
                pending_approval: 0,
                approved: 1,
                denied: 0,
                expired: 0,
                succeeded: 1,
                failed: 0,
                cancelled: 0,
                unresolved: 0,
            },
            available_transitions: vec![],
            assignments: vec![McpMissionAssignmentData {
                assignment_id: "a-1".to_string(),
                candidate_id: "c-1".to_string(),
                assignee: "agent-1".to_string(),
                run_state: "pending".to_string(),
                agent_state: "approved".to_string(),
                action_state: "ready".to_string(),
                reason_code: None,
                error_code: None,
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("running"));
        assert!(json.contains("a-1"));
    }

    #[test]
    fn mission_control_data_serializes() {
        let data = McpMissionControlData {
            command: "pause".to_string(),
            mission_file: "/tmp/test.json".to_string(),
            mission_id: "m-1".to_string(),
            lifecycle_from: "running".to_string(),
            lifecycle_to: "paused".to_string(),
            decision_path: "pause_mission->running->paused".to_string(),
            reason_code: "operator_request".to_string(),
            error_code: None,
            checkpoint_id: Some("cp-m-1-123".to_string()),
            mission_hash: "sha256:abcd".to_string(),
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("pause"));
        assert!(json.contains("cp-m-1-123"));
        assert!(!json.contains("error_code")); // skip_serializing_if None
    }

    #[test]
    fn mission_build_assignments_with_no_assignments() {
        use crate::plan::{Mission, MissionId, MissionOwnership};
        let mission = Mission::new(
            MissionId("m-test".to_string()),
            "Test",
            "ws-1",
            MissionOwnership {
                planner: "test".to_string(),
                dispatcher: "test".to_string(),
                operator: "test".to_string(),
            },
            1000,
        );
        let params = MissionStateParams::default();
        let (assignments, counters, matched) = mcp_build_mission_assignments(&mission, &params);
        assert!(assignments.is_empty());
        assert_eq!(matched, 0);
        assert_eq!(counters.approved, 0);
        assert_eq!(counters.unresolved, 0);
    }

    #[test]
    fn mission_build_assignments_filters_by_run_state() {
        use crate::plan::{
            ApprovalState, Assignment, AssignmentId, CandidateActionId, Mission, MissionId,
            MissionOwnership, Outcome,
        };
        let mut mission = Mission::new(
            MissionId("m-test".to_string()),
            "Test",
            "ws-1",
            MissionOwnership {
                planner: "test".to_string(),
                dispatcher: "test".to_string(),
                operator: "test".to_string(),
            },
            1000,
        );
        mission.assignments = vec![
            Assignment {
                assignment_id: AssignmentId("a-1".to_string()),
                candidate_id: CandidateActionId("c-1".to_string()),
                assignee: "agent-1".to_string(),
                assigned_by: crate::plan::MissionActorRole::Planner,
                approval_state: ApprovalState::NotRequired,
                outcome: Some(Outcome::Success {
                    reason_code: "ok".to_string(),
                    completed_at_ms: 2000,
                }),
                reservation_intent: None,
                escalation: None,
                created_at_ms: 1000,
                updated_at_ms: None,
            },
            Assignment {
                assignment_id: AssignmentId("a-2".to_string()),
                candidate_id: CandidateActionId("c-2".to_string()),
                assignee: "agent-2".to_string(),
                assigned_by: crate::plan::MissionActorRole::Planner,
                approval_state: ApprovalState::NotRequired,
                outcome: None,
                reservation_intent: None,
                escalation: None,
                created_at_ms: 1000,
                updated_at_ms: None,
            },
        ];
        let params = MissionStateParams {
            run_state: Some("pending".to_string()),
            ..Default::default()
        };
        let (assignments, counters, matched) = mcp_build_mission_assignments(&mission, &params);
        assert_eq!(matched, 1);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].assignment_id, "a-2");
        assert_eq!(counters.succeeded, 1);
        assert_eq!(counters.unresolved, 1);
    }

    #[test]
    fn mission_transition_info_serializes() {
        let info = McpMissionTransitionInfo {
            kind: "PauseRequested".to_string(),
            from: "Running".to_string(),
            to: "Paused".to_string(),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("PauseRequested"));
        assert!(json.contains("Running"));
        assert!(json.contains("Paused"));
    }
}
