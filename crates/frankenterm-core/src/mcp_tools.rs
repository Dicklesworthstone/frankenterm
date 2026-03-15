//! Extracted MCP tool handlers (strangler-fig migration slice).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[allow(unused_imports)]
use crate::mcp_framework::{
    FrameworkContent as Content, FrameworkMcpContext as McpContext, FrameworkMcpError as McpError,
    FrameworkMcpResult as McpResult, FrameworkTool as Tool, FrameworkToolHandler as ToolHandler,
};
use crate::policy::PolicySurface;

use super::mcp_types::{
    AccountsParams, AccountsRefreshParams, CassSearchParams, CassStatusParams, CassViewParams,
    EventsAnnotateParams, EventsLabelParams, EventsParams, EventsTriageParams, GetTextParams,
    McpAccountInfo, McpAccountsData, McpAccountsRefreshData, McpEnvelope, McpEventItem,
    McpEventMutationData, McpEventsData, McpGetTextData, McpMissionAssignmentCounters,
    McpMissionControlData, McpMissionExplainData, McpMissionStateData, McpPaneState,
    McpReleaseData, McpReservationInfo, McpReservationsData, McpReserveData, McpRuleItem,
    McpRuleMatchItem, McpRuleTraceInfo, McpRulesListData, McpRulesTestData, McpSearchData,
    McpSearchHit, McpSendData, McpTxPlanData, McpTxRollbackData, McpTxRunData, McpTxShowData,
    McpWaitForData, McpWorkflowRunData, MissionAbortParams, MissionExplainParams,
    MissionPauseParams, MissionResumeParams, MissionStateParams, ReleaseParams, ReservationsParams,
    ReserveParams, RulesListParams, RulesTestParams, SearchParams, SendParams, StateParams,
    TxPlanParams, TxRollbackParams, TxRunParams, TxShowParams, WaitForParams, WorkflowRunParams,
    apply_tail_truncation, now_ms,
};
#[allow(unused_imports)]
use super::{
    AccountRecord, ActionKind, ActorKind, AgentProvider, AgentType, ApprovalStore, CassAgent,
    CassClient, CassError, CassSearchOptions, CassSearchResult, CassStatus, CassViewOptions,
    CassViewResult, CautClient, CautService, CompatRuntime, CompatRuntimeBuilder, Config,
    DecisionContext, EventQuery, HandleAuthRequired, HandleClaudeCodeLimits, HandleCompaction,
    HandleGeminiQuota, HandleProcessTriageLifecycle, HandleSessionEnd, HandleUsageLimits,
    InjectionResult, MCP_ERR_CASS, MCP_ERR_CAUT, MCP_ERR_CONFIG, MCP_ERR_FTS_QUERY,
    MCP_ERR_INVALID_ARGS, MCP_ERR_NOT_IMPLEMENTED, MCP_ERR_PANE_NOT_FOUND, MCP_ERR_POLICY,
    MCP_ERR_RESERVATION_CONFLICT, MCP_ERR_STORAGE, MCP_ERR_TIMEOUT, MCP_ERR_WEZTERM,
    MCP_ERR_WORKFLOW, McpToolError, Osc133State, PaneCapabilities, PaneFilterConfig, PaneInfo,
    PaneReservation, PaneWaiter, PaneWorkflowLockManager, PatternEngine, PolicyDecision,
    PolicyEngine, PolicyGatedInjector, PolicyInput, SearchQueryDefaults, SearchQueryInput,
    StorageHandle, UnifiedSearchMode, WaitMatcher, WaitOptions, WaitResult, WeztermError,
    WeztermHandleSource, Workflow, WorkflowEngine, WorkflowExecutionResult, WorkflowRunner,
    WorkflowRunnerConfig, approval_command, build_policy_engine, builtin_workflows,
    default_wezterm_handle, effective_search_fusion_backend, effective_search_fusion_weights,
    effective_search_quality_timeout_ms, effective_search_rrf_k, elapsed_ms, envelope_to_content,
    map_cass_error, map_caut_error, map_mcp_error, mcp_build_mission_assignments,
    mcp_build_tx_commit_step_inputs, mcp_build_tx_compensation_inputs,
    mcp_build_tx_prepare_gate_inputs, mcp_build_tx_synthetic_commit_report,
    mcp_load_mission_from_path, mcp_load_mission_tx_contract_from_path,
    mcp_mission_failure_catalog, mcp_mission_lifecycle_transitions, mcp_parse_mission_kill_switch,
    mcp_resolve_mission_file_path, mcp_resolve_mission_tx_file_path, mcp_save_mission_to_path,
    mcp_tx_transition_info, parse_cass_agent, parse_caut_service, parse_unified_search_query,
    policy_reason, record_mcp_audit_sync, redact_mcp_args, reservation_to_mcp_info,
    resolve_alt_screen_state, resolve_workspace_id, to_storage_search_options,
};
use super::{
    MCP_REFRESH_COOLDOWN_MS, check_refresh_cooldown, injection_from_decision,
    register_builtin_workflows, resolve_pane_capabilities,
};

fn mcp_get_text_policy_input(
    pane_id: u64,
    domain: impl Into<String>,
    capabilities: PaneCapabilities,
    summary: &str,
) -> PolicyInput {
    PolicyInput::new(ActionKind::ReadOutput, ActorKind::Mcp)
        .with_surface(PolicySurface::Mux)
        .with_pane(pane_id)
        .with_domain(domain.into())
        .with_capabilities(capabilities)
        .with_text_summary(summary.to_string())
}

fn mcp_search_output_policy_input(summary: &str) -> PolicyInput {
    PolicyInput::new(ActionKind::SearchOutput, ActorKind::Mcp)
        .with_surface(PolicySurface::Mux)
        .with_text_summary(summary.to_string())
}

fn mcp_send_text_policy_input(
    pane_id: u64,
    domain: impl Into<String>,
    capabilities: PaneCapabilities,
    summary: &str,
    command_text: &str,
) -> PolicyInput {
    PolicyInput::new(ActionKind::SendText, ActorKind::Mcp)
        .with_surface(PolicySurface::Mux)
        .with_pane(pane_id)
        .with_domain(domain.into())
        .with_capabilities(capabilities)
        .with_text_summary(summary.to_string())
        .with_command_text(command_text.to_string())
}

fn mcp_workflow_run_policy_input(
    pane_id: u64,
    domain: impl Into<String>,
    capabilities: PaneCapabilities,
    summary: &str,
) -> PolicyInput {
    PolicyInput::new(ActionKind::WorkflowRun, ActorKind::Mcp)
        .with_surface(PolicySurface::Workflow)
        .with_pane(pane_id)
        .with_domain(domain.into())
        .with_capabilities(capabilities)
        .with_text_summary(summary.to_string())
}

fn mcp_reserve_pane_policy_input(pane_id: u64, summary: &str) -> PolicyInput {
    PolicyInput::new(ActionKind::ReservePane, ActorKind::Mcp)
        .with_surface(PolicySurface::Swarm)
        .with_pane(pane_id)
        .with_capabilities(PaneCapabilities::unknown())
        .with_text_summary(summary.to_string())
        .with_command_text("reserve_pane".to_string())
}

fn mcp_release_pane_policy_input(summary: &str, pane_id: Option<u64>) -> PolicyInput {
    let mut input = PolicyInput::new(ActionKind::ReleasePane, ActorKind::Mcp)
        .with_surface(PolicySurface::Swarm)
        .with_capabilities(PaneCapabilities::unknown())
        .with_text_summary(summary.to_string())
        .with_command_text("release_reservation".to_string());
    if let Some(pane_id) = pane_id {
        input = input.with_pane(pane_id);
    }
    input
}

fn serialize_mcp_audit_decision_context(
    context: &crate::policy::DecisionContext,
) -> Option<String> {
    serde_json::to_string(context)
        .inspect_err(
            |e| tracing::warn!(error = %e, "mcp audit decision_context serialization failed"),
        )
        .ok()
}

fn mcp_event_mutation_decision_context(
    tool_name: &str,
    action_kind: &str,
    event_id: i64,
    operation: &str,
    actor_id: Option<&str>,
    input_summary: &str,
    timestamp_ms: i64,
) -> crate::policy::DecisionContext {
    let mut context = crate::policy::DecisionContext::new_audit(
        timestamp_ms,
        crate::policy::ActionKind::ExecCommand,
        crate::policy::ActorKind::Mcp,
        PolicySurface::Mcp,
        None,
        None,
        Some(input_summary.to_string()),
        None,
    );
    let determining_rule = format!("audit.{action_kind}");
    context.record_rule(
        &determining_rule,
        true,
        Some("allow"),
        Some("MCP event mutation recorded".to_string()),
    );
    context.set_determining_rule(&determining_rule);
    context.add_evidence("stage", "event_mutation");
    context.add_evidence("tool", tool_name);
    context.add_evidence("event_action_kind", action_kind);
    context.add_evidence("event_id", event_id.to_string());
    context.add_evidence("operation", operation);
    context.add_evidence("event_surface", PolicySurface::Mcp.as_str());
    if let Some(actor_id) = actor_id {
        context.add_evidence("actor_id", actor_id);
    }
    context
}

// wa.rules_list tool
pub(super) struct WaRulesListTool;

impl ToolHandler for WaRulesListTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.rules_list".to_string(),
            description: Some(
                "List pattern detection rules in the rule library (robot parity)".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_type": { "type": "string", "description": "Filter by agent type (codex, claude_code, gemini, wezterm)" },
                    "verbose": { "type": "boolean", "default": false, "description": "Include descriptions in output" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "rules".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: RulesListParams = if arguments.is_null() {
            RulesListParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(p) => p,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional agent_type, verbose".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let agent_filter: Option<AgentType> = match params.agent_type.as_ref() {
            Some(s) => match s.to_lowercase().as_str() {
                "codex" => Some(AgentType::Codex),
                "claude_code" => Some(AgentType::ClaudeCode),
                "gemini" => Some(AgentType::Gemini),
                "wezterm" => Some(AgentType::Wezterm),
                _ => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Unknown agent_type: {s}"),
                        Some("Valid types: codex, claude_code, gemini, wezterm".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            },
            None => None,
        };

        let engine = PatternEngine::new();
        let rules = engine.rules();

        let rule_items: Vec<McpRuleItem> = rules
            .iter()
            .filter(|rule| match agent_filter {
                Some(filter) => rule.agent_type == filter,
                None => true,
            })
            .map(|rule| McpRuleItem {
                id: rule.id.clone(),
                agent_type: rule.agent_type.to_string(),
                event_type: rule.event_type.clone(),
                severity: format!("{:?}", rule.severity).to_lowercase(),
                description: if params.verbose {
                    Some(rule.description.clone())
                } else {
                    None
                },
                workflow: rule.workflow.clone(),
                anchor_count: rule.anchors.len(),
                has_regex: rule.regex.is_some(),
            })
            .collect();

        let data = McpRulesListData {
            rules: rule_items,
            agent_type_filter: params.agent_type,
        };
        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.rules_test tool
pub(super) struct WaRulesTestTool;

impl ToolHandler for WaRulesTestTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.rules_test".to_string(),
            description: Some(
                "Test pattern detection rules against provided text (robot parity)".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Text to test pattern detection against" },
                    "trace": { "type": "boolean", "default": false, "description": "Include trace information in matches" }
                },
                "required": ["text"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "rules".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: RulesTestParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with text (required), trace".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let engine = PatternEngine::new();
        let detections = engine.detect(&params.text);

        let matches: Vec<McpRuleMatchItem> = detections
            .iter()
            .map(|d| McpRuleMatchItem {
                rule_id: d.rule_id.clone(),
                agent_type: d.agent_type.to_string(),
                event_type: d.event_type.clone(),
                severity: format!("{:?}", d.severity).to_lowercase(),
                confidence: d.confidence,
                matched_text: d.matched_text.clone(),
                extracted: if d.extracted.is_null()
                    || d.extracted
                        .as_object()
                        .is_some_and(serde_json::Map::is_empty)
                {
                    None
                } else {
                    Some(d.extracted.clone())
                },
                trace: if params.trace {
                    Some(McpRuleTraceInfo {
                        anchors_checked: true,
                        regex_matched: !d.matched_text.is_empty(),
                    })
                } else {
                    None
                },
            })
            .collect();

        let data = McpRulesTestData {
            text_length: params.text.len(),
            match_count: matches.len(),
            matches,
        };
        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.cass_search tool
pub(super) struct WaCassSearchTool;

impl ToolHandler for WaCassSearchTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.cass_search".to_string(),
            description: Some("Search coding agent session history via cass".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query string" },
                    "limit": { "type": "integer", "minimum": 0, "maximum": 1000, "default": 10, "description": "Maximum results (0 = cass default)" },
                    "offset": { "type": "integer", "minimum": 0, "default": 0, "description": "Offset into results" },
                    "agent": { "type": "string", "description": "Agent filter: codex|claude_code|gemini|cursor|aider|chatgpt" },
                    "workspace": { "type": "string", "description": "Workspace filter (cass-defined)" },
                    "days": { "type": "integer", "minimum": 0, "description": "Only sessions within the last N days" },
                    "fields": { "type": "string", "description": "Field selection (cass-defined; e.g. minimal)" },
                    "max_tokens": { "type": "integer", "minimum": 0, "description": "Max tokens per hit content (cass-defined)" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "default": 15, "description": "cass timeout override (seconds)" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "cass".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: CassSearchParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with query (required) and optional limit/offset/agent/workspace/days/fields/max_tokens/timeout_secs".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        if params.query.trim().is_empty() {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "query cannot be empty".to_string(),
                Some("Provide a non-empty search query string".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let agent: Option<CassAgent> = if let Some(ref agent_str) = params.agent {
            match parse_cass_agent(agent_str) {
                Some(agent) => Some(agent),
                None => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid agent: {agent_str}"),
                        Some(
                            "Supported: codex, claude_code, gemini, cursor, aider, chatgpt"
                                .to_string(),
                        ),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        } else {
            None
        };

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<CassSearchResult, CassError> = runtime.block_on(async {
            let client = CassClient::new().with_timeout_secs(params.timeout_secs);
            let options = CassSearchOptions {
                limit: (params.limit != 0).then_some(params.limit),
                offset: (params.offset != 0).then_some(params.offset),
                agent,
                workspace: params.workspace,
                days: params.days,
                fields: params.fields,
                max_tokens: params.max_tokens,
            };
            client.search(&params.query, &options).await
        });

        match result {
            Ok(result) => {
                let envelope = McpEnvelope::success(result, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_cass_error(&err);
                let envelope = McpEnvelope::<()>::error(
                    code,
                    format!("cass search failed: {err}"),
                    hint,
                    elapsed_ms(start),
                );
                envelope_to_content(envelope)
            }
        }
    }
}

// wa.cass_view tool
pub(super) struct WaCassViewTool;

impl ToolHandler for WaCassViewTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.cass_view".to_string(),
            description: Some("View context for a cass search hit".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "source_path": { "type": "string", "description": "Source path returned by cass search" },
                    "line_number": { "type": "integer", "minimum": 0, "description": "Line number returned by cass search" },
                    "context_lines": { "type": "integer", "minimum": 0, "default": 10, "description": "Context lines before/after match" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "default": 15, "description": "cass timeout override (seconds)" }
                },
                "required": ["source_path", "line_number"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "cass".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: CassViewParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some(
                        "Expected object with source_path, line_number, optional context_lines, timeout_secs"
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        if params.source_path.trim().is_empty() {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "source_path cannot be empty".to_string(),
                Some("Provide a valid source_path returned by cass search".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<CassViewResult, CassError> = runtime.block_on(async {
            let client = CassClient::new().with_timeout_secs(params.timeout_secs);
            let options = CassViewOptions {
                context_lines: Some(params.context_lines),
            };
            client
                .query(
                    std::path::Path::new(&params.source_path),
                    params.line_number,
                    &options,
                )
                .await
        });

        match result {
            Ok(result) => {
                let envelope = McpEnvelope::success(result, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_cass_error(&err);
                let envelope = McpEnvelope::<()>::error(
                    code,
                    format!("cass view failed: {err}"),
                    hint,
                    elapsed_ms(start),
                );
                envelope_to_content(envelope)
            }
        }
    }
}

// wa.cass_status tool
pub(super) struct WaCassStatusTool;

impl ToolHandler for WaCassStatusTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.cass_status".to_string(),
            description: Some("Check cass index status".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "default": 15, "description": "cass timeout override (seconds)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "cass".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: CassStatusParams = if arguments.is_null() {
            CassStatusParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(p) => p,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional timeout_secs".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<CassStatus, CassError> = runtime.block_on(async {
            let client = CassClient::new().with_timeout_secs(params.timeout_secs);
            client.status().await
        });

        match result {
            Ok(result) => {
                let envelope = McpEnvelope::success(result, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_cass_error(&err);
                let envelope = McpEnvelope::<()>::error(
                    code,
                    format!("cass status failed: {err}"),
                    hint,
                    elapsed_ms(start),
                );
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaStateTool {
    filter: PaneFilterConfig,
}

impl WaStateTool {
    pub(super) fn new(filter: PaneFilterConfig) -> Self {
        Self { filter }
    }
}

impl ToolHandler for WaStateTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.state".to_string(),
            description: Some("Get current pane states (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "domain": { "type": "string" },
                    "agent": { "type": "string" },
                    "pane_id": { "type": "integer", "minimum": 0 }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params = if arguments.is_null() {
            StateParams::default()
        } else {
            match serde_json::from_value::<StateParams>(arguments) {
                Ok(params) => params,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional domain/agent/pane_id".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result = runtime.block_on(async {
            let wezterm = default_wezterm_handle();
            wezterm.list_panes().await
        });

        match result {
            Ok(panes) => {
                let states: Vec<McpPaneState> = panes
                    .iter()
                    .filter(|pane| match params.pane_id {
                        Some(pane_id) => pane.pane_id == pane_id,
                        None => true,
                    })
                    .filter(|pane| match params.domain.as_ref() {
                        Some(domain) => pane.inferred_domain() == *domain,
                        None => true,
                    })
                    .filter(|pane| match params.agent.as_ref() {
                        Some(agent) => {
                            let title = pane.title.as_deref().unwrap_or("").to_lowercase();
                            let filter = agent.to_lowercase();
                            match filter.as_str() {
                                "codex" => title.contains("codex") || title.contains("openai"),
                                "claude_code" | "claude" => title.contains("claude"),
                                "gemini" => title.contains("gemini"),
                                _ => title.contains(&filter),
                            }
                        }
                        None => true,
                    })
                    .map(|pane| McpPaneState::from_pane_info(pane, &self.filter))
                    .collect();
                let envelope = McpEnvelope::success(states, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaGetTextTool {
    config: Arc<Config>,
    db_path: Option<Arc<PathBuf>>,
}

impl WaGetTextTool {
    pub(super) fn new(config: Arc<Config>, db_path: Option<Arc<PathBuf>>) -> Self {
        Self { config, db_path }
    }
}

impl ToolHandler for WaGetTextTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.get_text".to_string(),
            description: Some("Get text content from a pane (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pane_id": { "type": "integer", "minimum": 0, "description": "The pane ID to read from" },
                    "tail": { "type": "integer", "minimum": 1, "default": 500, "description": "Number of lines to return (from end)" },
                    "escapes": { "type": "boolean", "default": false, "description": "Include escape sequences" }
                },
                "required": ["pane_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: GetTextParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with pane_id (required), tail, escapes".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let config = Arc::clone(&self.config);
        let db_path = self.db_path.as_ref().map(Arc::clone);

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<McpGetTextData, McpToolError> =
            runtime.block_on(async move {
                let storage = if let Some(path) = db_path.as_ref() {
                    Some(
                        StorageHandle::new(&path.to_string_lossy())
                            .await
                            .map_err(McpToolError::from_error)?,
                    )
                } else {
                    None
                };

                let wezterm = default_wezterm_handle();
                let pane_info = wezterm
                    .get_pane(params.pane_id)
                    .await
                    .map_err(McpToolError::from_error)?;
                let domain = pane_info.inferred_domain();
                let resolution =
                    resolve_pane_capabilities(&config, storage.as_ref(), params.pane_id).await;
                let capabilities = resolution.capabilities;

                let mut engine = build_policy_engine(&config, false);
                let summary = format!("wa.get_text pane_id={}", params.pane_id);
                let mut input =
                    mcp_get_text_policy_input(params.pane_id, domain, capabilities, &summary);
                if let Some(title) = &pane_info.title {
                    input = input.with_pane_title(title.clone());
                }
                if let Some(cwd) = &pane_info.cwd {
                    input = input.with_pane_cwd(cwd.clone());
                }

                let decision = engine.authorize(&input);
                if decision.is_denied() {
                    let reason = policy_reason(&decision)
                        .unwrap_or("Read denied by policy")
                        .to_string();
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, None));
                }
                if decision.requires_approval() {
                    let mut hint = approval_command(&decision);
                    if let Some(storage) = storage.as_ref() {
                        let workspace_id =
                            resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
                        let store = ApprovalStore::new(
                            storage,
                            config.safety.approval.clone(),
                            workspace_id,
                        );
                        let updated = store
                            .attach_to_decision(decision, &input, Some(summary))
                            .await
                            .map_err(McpToolError::from_error)?;
                        hint = approval_command(&updated);
                        let reason = policy_reason(&updated)
                            .unwrap_or("Read requires approval")
                            .to_string();
                        return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                    }
                    let reason = policy_reason(&decision)
                        .unwrap_or("Read requires approval")
                        .to_string();
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                }

                let full_text = wezterm
                    .get_text(params.pane_id, params.escapes)
                    .await
                    .map_err(McpToolError::from_error)?;
                let (text, truncated, truncation_info) =
                    apply_tail_truncation(&full_text, params.tail);

                Ok(McpGetTextData {
                    pane_id: params.pane_id,
                    text: engine.redact_secrets(&text),
                    tail_lines: params.tail,
                    escapes_included: params.escapes,
                    truncated,
                    truncation_info,
                })
            });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaWaitForTool;

impl ToolHandler for WaWaitForTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.wait_for".to_string(),
            description: Some("Wait for a pattern match in pane output (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pane_id": { "type": "integer", "minimum": 0, "description": "Pane ID to wait on" },
                    "pattern": { "type": "string", "description": "Pattern to match (substring or regex)" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "default": 30, "description": "Timeout in seconds" },
                    "tail": { "type": "integer", "minimum": 0, "default": 200, "description": "Tail lines to search (0 = full buffer)" },
                    "regex": { "type": "boolean", "default": false, "description": "Treat pattern as regex" }
                },
                "required": ["pane_id", "pattern"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: WaitForParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some(
                        "Expected object with pane_id, pattern, timeout_secs, tail, regex"
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let matcher = if params.regex {
            match fancy_regex::Regex::new(&params.pattern) {
                Ok(compiled) => WaitMatcher::regex(compiled),
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid regex pattern: {err}"),
                        Some("Check the regex syntax".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        } else {
            WaitMatcher::substring(&params.pattern)
        };

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let pattern = params.pattern.clone();
        let pane_id = params.pane_id;
        let tail = params.tail;
        let timeout_secs = params.timeout_secs;
        let is_regex = params.regex;

        let result = runtime.block_on(async move {
            let wezterm = default_wezterm_handle();
            let panes = wezterm.list_panes().await?;
            if !panes.iter().any(|p| p.pane_id == pane_id) {
                return Err(WeztermError::PaneNotFound(pane_id).into());
            }

            let options = WaitOptions {
                tail_lines: tail,
                escapes: false,
                ..WaitOptions::default()
            };
            let source = WeztermHandleSource::new(Arc::clone(&wezterm));
            let waiter = PaneWaiter::new(&source).with_options(options);
            let timeout = std::time::Duration::from_secs(timeout_secs);
            waiter.wait_for(pane_id, &matcher, timeout).await
        });

        match result {
            Ok(WaitResult::Matched {
                elapsed_ms: wait_elapsed_ms,
                polls,
            }) => {
                let data = McpWaitForData {
                    pane_id,
                    pattern,
                    matched: true,
                    elapsed_ms: wait_elapsed_ms,
                    polls,
                    is_regex,
                };
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Ok(WaitResult::TimedOut {
                elapsed_ms: wait_elapsed_ms,
                polls,
                ..
            }) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_TIMEOUT,
                    format!(
                        "Timeout waiting for pattern '{pattern}' after {wait_elapsed_ms}ms ({polls} polls)"
                    ),
                    Some("Increase timeout_secs or verify the pattern.".to_string()),
                    elapsed_ms(start),
                );
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaSearchTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
}

impl WaSearchTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        Self { config, db_path }
    }
}

impl ToolHandler for WaSearchTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.search".to_string(),
            description: Some(
                "Unified lexical/semantic/hybrid search across captured pane output (CLI/robot/MCP contract)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "FTS5 search query" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 20, "description": "Maximum results" },
                    "pane": { "type": "integer", "minimum": 0, "description": "Filter by pane ID" },
                    "since": { "type": "integer", "description": "Filter by lower bound time (epoch ms, inclusive)" },
                    "until": { "type": "integer", "description": "Filter by upper bound time (epoch ms, inclusive)" },
                    "snippets": { "type": "boolean", "default": true, "description": "Include snippets in results" }
                    ,
                    "mode": { "type": "string", "enum": ["lexical", "semantic", "hybrid"], "default": "lexical", "description": "Search mode (lexical, semantic, or hybrid)" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "search".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: SearchParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some(
                        "Expected object with query (required), limit, pane, since, until, snippets, mode".to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let parsed = match parse_unified_search_query(
            SearchQueryInput {
                query: params.query,
                limit: params.limit,
                pane: params.pane,
                since: params.since,
                until: params.until,
                snippets: params.snippets,
                mode: params.mode,
                explain: None,
            },
            SearchQueryDefaults::default(),
        ) {
            Ok(parsed) => parsed,
            Err(err) => {
                let code = if err.is_query_lint_error() {
                    MCP_ERR_FTS_QUERY
                } else {
                    MCP_ERR_INVALID_ARGS
                };
                let envelope =
                    McpEnvelope::<()>::error(code, err.message(), err.hint(), elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let canonical = parsed.query;

        let requested_mode = canonical.mode;
        let search_mode = match requested_mode {
            UnifiedSearchMode::Lexical => crate::search::SearchMode::Lexical,
            UnifiedSearchMode::Semantic => crate::search::SearchMode::Semantic,
            UnifiedSearchMode::Hybrid => crate::search::SearchMode::Hybrid,
        };

        let config = Arc::clone(&self.config);
        let db_path = Arc::clone(&self.db_path);
        let query_for_storage = canonical.query.clone();
        let search_options = to_storage_search_options(&canonical);
        let snippets_enabled = canonical.snippets;
        let hybrid_rrf_k = effective_search_rrf_k(config.as_ref());
        let (hybrid_lexical_weight, hybrid_semantic_weight) =
            effective_search_fusion_weights(config.as_ref());
        let hybrid_fusion_backend = effective_search_fusion_backend(config.as_ref());
        let semantic_query = if matches!(
            requested_mode,
            UnifiedSearchMode::Semantic | UnifiedSearchMode::Hybrid
        ) {
            use crate::search::Embedder;

            let embedder = crate::search::HashEmbedder::default();
            match embedder.embed(&canonical.query) {
                Ok(vector) => Some((embedder.info().name, vector)),
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_STORAGE,
                        format!("Failed to embed query for semantic search: {err}"),
                        Some(
                            "Try mode=lexical or verify semantic embedding support in this build."
                                .to_string(),
                        ),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        } else {
            None
        };

        enum SearchExecution {
            Lexical(Vec<crate::storage::SearchResult>),
            Hybrid(crate::storage::HybridSearchBundle),
        }

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<SearchExecution, McpToolError> =
            runtime.block_on(async move {
                let storage = StorageHandle::new(&db_path.to_string_lossy())
                    .await
                    .map_err(McpToolError::from_error)?;
                let mut semantic_budget_config = storage.semantic_budget_snapshot().config;
                semantic_budget_config.max_semantic_latency_ms =
                    effective_search_quality_timeout_ms(config.as_ref());
                storage.set_semantic_budget_config(semantic_budget_config);

                let mut engine = build_policy_engine(&config, false);
                let summary = engine.redact_secrets(&query_for_storage);
                let mut input = mcp_search_output_policy_input(&summary);

                if let Some(pane_id) = search_options.pane_id {
                    let wezterm = default_wezterm_handle();
                    let pane_info = wezterm
                        .get_pane(pane_id)
                        .await
                        .map_err(McpToolError::from_error)?;
                    let domain = pane_info.inferred_domain();
                    let resolution =
                        resolve_pane_capabilities(&config, Some(&storage), pane_id).await;
                    input = input
                        .with_pane(pane_id)
                        .with_domain(domain)
                        .with_capabilities(resolution.capabilities);
                    if let Some(title) = &pane_info.title {
                        input = input.with_pane_title(title.clone());
                    }
                    if let Some(cwd) = &pane_info.cwd {
                        input = input.with_pane_cwd(cwd.clone());
                    }
                } else {
                    input = input.with_capabilities(PaneCapabilities::unknown());
                }

                let decision = engine.authorize(&input);
                if decision.is_denied() {
                    let reason = policy_reason(&decision)
                        .unwrap_or("Search denied by policy")
                        .to_string();
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, None));
                }
                if decision.requires_approval() {
                    let workspace_id =
                        resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
                    let store =
                        ApprovalStore::new(&storage, config.safety.approval.clone(), workspace_id);
                    let updated = store
                        .attach_to_decision(decision, &input, Some(summary))
                        .await
                        .map_err(McpToolError::from_error)?;
                    let reason = policy_reason(&updated)
                        .unwrap_or("Search requires approval")
                        .to_string();
                    let hint = approval_command(&updated);
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                }

                match requested_mode {
                    UnifiedSearchMode::Lexical => {
                        let results = storage
                            .search_with_results(&query_for_storage, search_options)
                            .await
                            .map_err(McpToolError::from_error)?;
                        Ok(SearchExecution::Lexical(results))
                    }
                    UnifiedSearchMode::Semantic | UnifiedSearchMode::Hybrid => {
                        let (embedder_id, query_vector) = semantic_query.ok_or_else(|| {
                            McpToolError::new(
                                MCP_ERR_STORAGE,
                                "semantic query vector missing for non-lexical wa.search mode"
                                    .to_string(),
                                None,
                            )
                        })?;

                        let bundle = storage
                            .hybrid_search_with_results(
                                &query_for_storage,
                                search_options,
                                &embedder_id,
                                &query_vector,
                                search_mode,
                                hybrid_rrf_k,
                                hybrid_lexical_weight,
                                hybrid_semantic_weight,
                                Some(hybrid_fusion_backend),
                            )
                            .await
                            .map_err(McpToolError::from_error)?;
                        Ok(SearchExecution::Hybrid(bundle))
                    }
                }
            });

        let redactor = crate::policy::Redactor::new();
        let redacted_query = redactor.redact(&canonical.query);

        match result {
            Ok(SearchExecution::Lexical(results)) => {
                let total_hits = results.len();
                let hits: Vec<McpSearchHit> = results
                    .into_iter()
                    .map(|r| McpSearchHit {
                        segment_id: r.segment.id,
                        pane_id: r.segment.pane_id,
                        seq: r.segment.seq,
                        captured_at: r.segment.captured_at,
                        score: r.score,
                        snippet: r.snippet.map(|snippet| redactor.redact(&snippet)),
                        content: if snippets_enabled {
                            None
                        } else {
                            Some(redactor.redact(&r.segment.content))
                        },
                        semantic_score: None,
                        fusion_rank: None,
                    })
                    .collect();

                let data = McpSearchData {
                    query: redacted_query.clone(),
                    results: hits,
                    total_hits,
                    limit: canonical.limit,
                    pane_filter: canonical.pane,
                    since_filter: canonical.since,
                    until_filter: canonical.until,
                    mode: canonical.mode.as_str().to_string(),
                    metrics: None,
                };
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Ok(SearchExecution::Hybrid(bundle)) => {
                let crate::storage::HybridSearchBundle {
                    mode,
                    requested_mode,
                    fallback_reason,
                    rrf_k,
                    lexical_weight,
                    semantic_weight,
                    fusion_backend,
                    lexical_candidates,
                    semantic_candidates,
                    semantic_cache_hit,
                    semantic_latency_ms,
                    semantic_rows_scanned,
                    semantic_budget_state,
                    semantic_backoff_until_ms,
                    results,
                } = bundle;
                let effective_mode = mode.clone();

                let total_hits = results.len();
                let hits: Vec<McpSearchHit> = results
                    .into_iter()
                    .map(|hit| {
                        let result = hit.result;
                        McpSearchHit {
                            segment_id: result.segment.id,
                            pane_id: result.segment.pane_id,
                            seq: result.segment.seq,
                            captured_at: result.segment.captured_at,
                            score: hit.fusion_score,
                            snippet: result.snippet.map(|snippet| redactor.redact(&snippet)),
                            content: if snippets_enabled {
                                None
                            } else {
                                Some(redactor.redact(&result.segment.content))
                            },
                            semantic_score: hit.semantic_score,
                            fusion_rank: Some(hit.fusion_rank),
                        }
                    })
                    .collect();

                let metrics = serde_json::json!({
                    "requested_mode": requested_mode,
                    "effective_mode": effective_mode,
                    "fallback_reason": fallback_reason,
                    "rrf_k": rrf_k,
                    "lexical_weight": lexical_weight,
                    "semantic_weight": semantic_weight,
                    "fusion_backend": fusion_backend,
                    "lexical_candidates": lexical_candidates,
                    "semantic_candidates": semantic_candidates,
                    "semantic_cache_hit": semantic_cache_hit,
                    "semantic_latency_ms": semantic_latency_ms,
                    "semantic_rows_scanned": semantic_rows_scanned,
                    "semantic_budget_state": semantic_budget_state,
                    "semantic_backoff_until_ms": semantic_backoff_until_ms
                });

                let data = McpSearchData {
                    query: redacted_query,
                    results: hits,
                    total_hits,
                    limit: canonical.limit,
                    pane_filter: canonical.pane,
                    since_filter: canonical.since,
                    until_filter: canonical.until,
                    mode: effective_mode,
                    metrics: Some(metrics),
                };
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaEventsTool {
    db_path: Arc<PathBuf>,
}

impl WaEventsTool {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ToolHandler for WaEventsTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.events".to_string(),
            description: Some("Get pattern detection events (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 20, "description": "Maximum results" },
                    "pane": { "type": "integer", "minimum": 0, "description": "Filter by pane ID" },
                    "rule_id": { "type": "string", "description": "Filter by rule ID (exact match)" },
                    "event_type": { "type": "string", "description": "Filter by event type" },
                    "triage_state": { "type": "string", "description": "Filter by triage state (exact match)" },
                    "label": { "type": "string", "description": "Filter by label (exact match)" },
                    "unhandled": { "type": "boolean", "default": false, "description": "Only return unhandled events" },
                    "since": { "type": "integer", "description": "Filter by time (epoch ms)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "events".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: EventsParams = if arguments.is_null() {
            EventsParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(p) => p,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional limit, pane, rule_id, event_type, triage_state, label, unhandled, since".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: crate::Result<McpEventsData> = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;

            let query = EventQuery {
                limit: Some(params.limit),
                pane_id: params.pane,
                rule_id: params.rule_id.clone(),
                event_type: params.event_type.clone(),
                triage_state: params.triage_state.clone(),
                label: params.label.clone(),
                unhandled_only: params.unhandled,
                since: params.since,
                until: None,
            };

            let events = storage.get_events(query).await?;
            let total_count = events.len();

            let mut items: Vec<McpEventItem> = Vec::with_capacity(events.len());
            for e in events {
                let pack_id = e.rule_id.split('.').next().map_or_else(
                    || "builtin:unknown".to_string(),
                    |agent| format!("builtin:{agent}"),
                );

                let annotations = match storage.get_event_annotations(e.id).await {
                    Ok(Some(a)) => Some(a),
                    Ok(None) => None,
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            event_id = e.id,
                            "Failed to load event annotations"
                        );
                        None
                    }
                };

                items.push(McpEventItem {
                    id: e.id,
                    pane_id: e.pane_id,
                    rule_id: e.rule_id,
                    pack_id,
                    event_type: e.event_type,
                    severity: e.severity,
                    confidence: e.confidence,
                    extracted: e.extracted,
                    annotations,
                    captured_at: e.detected_at,
                    handled_at: e.handled_at,
                    workflow_id: e.handled_by_workflow_id,
                });
            }

            Ok(McpEventsData {
                events: items,
                total_count,
                limit: params.limit,
                pane_filter: params.pane,
                rule_id_filter: params.rule_id,
                event_type_filter: params.event_type,
                triage_state_filter: params.triage_state,
                label_filter: params.label,
                unhandled_only: params.unhandled,
                since_filter: params.since,
            })
        });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaSendTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
}

impl WaSendTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        Self { config, db_path }
    }
}

impl ToolHandler for WaSendTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.send".to_string(),
            description: Some("Send text to a pane with policy gating (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pane_id": { "type": "integer", "minimum": 0, "description": "Pane ID to send to" },
                    "text": { "type": "string", "description": "Text to send" },
                    "dry_run": { "type": "boolean", "default": false, "description": "Preview without sending" },
                    "wait_for": { "type": "string", "description": "Wait for a pattern after sending" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "default": 30, "description": "Wait-for timeout (seconds)" },
                    "wait_for_regex": { "type": "boolean", "default": false, "description": "Treat wait_for as regex" }
                },
                "required": ["pane_id", "text"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: SendParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some(
                        "Expected object with pane_id, text, dry_run, wait_for, timeout_secs, wait_for_regex"
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let config = Arc::clone(&self.config);
        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result = runtime.block_on(async move {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;
            let wezterm = default_wezterm_handle();
            let pane_info = wezterm.get_pane(params.pane_id).await?;
            let domain = pane_info.inferred_domain();

            let resolution =
                resolve_pane_capabilities(&config, Some(&storage), params.pane_id).await;
            let capabilities = resolution.capabilities;

            let mut engine = build_policy_engine(&config, config.safety.require_prompt_active);
            let summary = engine.redact_secrets(&params.text);

            let mut input = mcp_send_text_policy_input(
                params.pane_id,
                domain,
                capabilities.clone(),
                &summary,
                &params.text,
            );

            if let Some(title) = &pane_info.title {
                input = input.with_pane_title(title.clone());
            }
            if let Some(cwd) = &pane_info.cwd {
                input = input.with_pane_cwd(cwd.clone());
            }

            if params.dry_run {
                let decision = engine.authorize(&input);
                let injection = injection_from_decision(
                    decision,
                    summary,
                    params.pane_id,
                    ActionKind::SendText,
                );
                return Ok(McpSendData {
                    pane_id: params.pane_id,
                    injection,
                    wait_for: None,
                    verification_error: None,
                    dry_run: true,
                });
            }

            let mut injector =
                PolicyGatedInjector::with_storage(engine, Arc::clone(&wezterm), storage.clone());
            let mut injection = injector
                .send_text(
                    params.pane_id,
                    &params.text,
                    ActorKind::Mcp,
                    &capabilities,
                    None,
                )
                .await;

            if let InjectionResult::RequiresApproval {
                decision,
                summary,
                pane_id,
                action,
                audit_action_id,
            } = injection
            {
                let workspace_id = resolve_workspace_id(&config)?;
                let store =
                    ApprovalStore::new(&storage, config.safety.approval.clone(), workspace_id);
                let updated = store
                    .attach_to_decision(decision, &input, Some(summary.clone()))
                    .await?;
                injection = InjectionResult::RequiresApproval {
                    decision: updated,
                    summary,
                    pane_id,
                    action,
                    audit_action_id,
                };
            }

            let mut wait_for_data = None;
            let mut verification_error = None;
            if injection.is_allowed() {
                if let Some(pattern) = params.wait_for.as_ref() {
                    let matcher = if params.wait_for_regex {
                        match fancy_regex::Regex::new(pattern) {
                            Ok(compiled) => Some(WaitMatcher::regex(compiled)),
                            Err(e) => {
                                verification_error = Some(format!("Invalid wait-for regex: {e}"));
                                None
                            }
                        }
                    } else {
                        Some(WaitMatcher::substring(pattern))
                    };

                    if let Some(matcher) = matcher {
                        let options = WaitOptions {
                            tail_lines: 200,
                            escapes: false,
                            ..WaitOptions::default()
                        };
                        let source = WeztermHandleSource::new(Arc::clone(&wezterm));
                        let waiter = PaneWaiter::new(&source).with_options(options);
                        let timeout = std::time::Duration::from_secs(params.timeout_secs);
                        match waiter.wait_for(params.pane_id, &matcher, timeout).await {
                            Ok(WaitResult::Matched { elapsed_ms, polls }) => {
                                wait_for_data = Some(McpWaitForData {
                                    pane_id: params.pane_id,
                                    pattern: pattern.clone(),
                                    matched: true,
                                    elapsed_ms,
                                    polls,
                                    is_regex: params.wait_for_regex,
                                });
                            }
                            Ok(WaitResult::TimedOut {
                                elapsed_ms, polls, ..
                            }) => {
                                wait_for_data = Some(McpWaitForData {
                                    pane_id: params.pane_id,
                                    pattern: pattern.clone(),
                                    matched: false,
                                    elapsed_ms,
                                    polls,
                                    is_regex: params.wait_for_regex,
                                });
                                verification_error =
                                    Some(format!("Timeout waiting for pattern '{pattern}'"));
                            }
                            Err(e) => {
                                verification_error = Some(format!("wait-for failed: {e}"));
                            }
                        }
                    }
                }
            }

            Ok(McpSendData {
                pane_id: params.pane_id,
                injection,
                wait_for: wait_for_data,
                verification_error,
                dry_run: false,
            })
        });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaWorkflowRunTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
}

impl WaWorkflowRunTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        Self { config, db_path }
    }
}

impl ToolHandler for WaWorkflowRunTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.workflow_run".to_string(),
            description: Some("Execute a workflow (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Workflow name" },
                    "pane_id": { "type": "integer", "minimum": 0, "description": "Target pane ID" },
                    "force": { "type": "boolean", "default": false, "description": "Force run (bypass handled guard)" },
                    "dry_run": { "type": "boolean", "default": false, "description": "Preview without executing" }
                },
                "required": ["name", "pane_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "workflow".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: WorkflowRunParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with name, pane_id, force, dry_run".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let config = Arc::clone(&self.config);
        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<McpWorkflowRunData, McpToolError> =
            runtime.block_on(async move {
                let storage = StorageHandle::new(&db_path.to_string_lossy())
                    .await
                    .map_err(McpToolError::from_error)?;
                let storage = Arc::new(storage);

                let wezterm = default_wezterm_handle();
                let pane_info = wezterm
                    .get_pane(params.pane_id)
                    .await
                    .map_err(McpToolError::from_error)?;
                let domain = pane_info.inferred_domain();

                let resolution =
                    resolve_pane_capabilities(&config, Some(storage.as_ref()), params.pane_id)
                        .await;
                let capabilities = resolution.capabilities;

                let mut policy_engine =
                    build_policy_engine(&config, config.safety.require_prompt_active);
                let summary = format!("workflow run {}", params.name);

                let mut input = mcp_workflow_run_policy_input(
                    params.pane_id,
                    domain,
                    capabilities.clone(),
                    &summary,
                );

                if let Some(title) = &pane_info.title {
                    input = input.with_pane_title(title.clone());
                }
                if let Some(cwd) = &pane_info.cwd {
                    input = input.with_pane_cwd(cwd.clone());
                }

                let decision = policy_engine.authorize(&input);
                if decision.is_denied() {
                    let reason = policy_reason(&decision)
                        .unwrap_or("Workflow denied by policy")
                        .to_string();
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, None));
                }
                if decision.requires_approval() {
                    let workspace_id =
                        resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
                    let store = ApprovalStore::new(
                        storage.as_ref(),
                        config.safety.approval.clone(),
                        workspace_id,
                    );
                    let updated = store
                        .attach_to_decision(decision, &input, Some(summary))
                        .await
                        .map_err(McpToolError::from_error)?;
                    let reason = policy_reason(&updated)
                        .unwrap_or("Workflow requires approval")
                        .to_string();
                    let hint = approval_command(&updated);
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                }

                if params.dry_run {
                    return Ok(McpWorkflowRunData {
                        workflow_name: params.name,
                        pane_id: params.pane_id,
                        execution_id: None,
                        status: "dry_run".to_string(),
                        message: Some("Dry-run: workflow not executed".to_string()),
                        result: None,
                        steps_executed: None,
                        step_index: None,
                        elapsed_ms: Some(elapsed_ms(start)),
                    });
                }

                let engine = WorkflowEngine::new(10);
                let lock_manager = Arc::new(PaneWorkflowLockManager::new());
                let injector_engine =
                    build_policy_engine(&config, config.safety.require_prompt_active);
                let injector = Arc::new(crate::runtime_compat::Mutex::new(
                    PolicyGatedInjector::with_storage(
                        injector_engine,
                        Arc::clone(&wezterm),
                        storage.as_ref().clone(),
                    ),
                ));
                let runner = WorkflowRunner::new(
                    engine,
                    lock_manager,
                    Arc::clone(&storage),
                    injector,
                    WorkflowRunnerConfig::default(),
                );
                register_builtin_workflows(&runner, &config);

                let _ = params.force;
                let workflow = runner.find_workflow_by_name(&params.name).ok_or_else(|| {
                    McpToolError::new(
                        MCP_ERR_WORKFLOW,
                        format!("Workflow '{}' not found", params.name),
                        Some(
                            "Ensure workflows are enabled or run ft watch for event-driven workflows."
                                .to_string(),
                        ),
                    )
                })?;

                let execution_id = format!("mcp-{}-{}", params.name, now_ms());
                let result = runner
                    .run_workflow(params.pane_id, workflow, &execution_id, 0)
                    .await;

                let (status, message, result_value, steps_executed, step_index) = match result {
                    WorkflowExecutionResult::Completed {
                        result,
                        steps_executed,
                        ..
                    } => ("completed", None, Some(result), Some(steps_executed), None),
                    WorkflowExecutionResult::Aborted {
                        reason, step_index, ..
                    } => ("aborted", Some(reason), None, None, Some(step_index)),
                    WorkflowExecutionResult::PolicyDenied {
                        reason, step_index, ..
                    } => ("policy_denied", Some(reason), None, None, Some(step_index)),
                    WorkflowExecutionResult::Error { error, .. } => {
                        ("error", Some(error), None, None, None)
                    }
                };

                Ok(McpWorkflowRunData {
                    workflow_name: params.name,
                    pane_id: params.pane_id,
                    execution_id: Some(execution_id),
                    status: status.to_string(),
                    message,
                    result: result_value,
                    steps_executed,
                    step_index,
                    elapsed_ms: Some(elapsed_ms(start)),
                })
            });

        match result {
            Ok(data) => {
                let status = data.status.as_str();
                if status == "completed" || status == "dry_run" {
                    let envelope = McpEnvelope::success(data, elapsed_ms(start));
                    envelope_to_content(envelope)
                } else if status == "policy_denied" {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_POLICY,
                        "Workflow denied by policy".to_string(),
                        Some("Review safety configuration or use dry_run.".to_string()),
                        elapsed_ms(start),
                    );
                    envelope_to_content(envelope)
                } else {
                    let message = data
                        .message
                        .clone()
                        .unwrap_or_else(|| "workflow failed".to_string());
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_WORKFLOW,
                        message,
                        None,
                        elapsed_ms(start),
                    );
                    envelope_to_content(envelope)
                }
            }
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaTxPlanTool {
    config: Arc<Config>,
}

impl WaTxPlanTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaTxPlanTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.tx_plan".to_string(),
            description: Some(
                "Validate and summarize mission transaction contract metadata (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "contract_file": { "type": "string", "description": "Optional path to MissionTxContract JSON (default: .ft/mission/tx-active.json)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "tx".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: TxPlanParams = if arguments.is_null() {
            TxPlanParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional contract_file".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let contract_path = match mcp_resolve_mission_tx_file_path(
            self.config.as_ref(),
            params.contract_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let contract = match mcp_load_mission_tx_contract_from_path(&contract_path) {
            Ok(contract) => contract,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let data = McpTxPlanData {
            contract_file: contract_path.display().to_string(),
            tx_id: contract.intent.tx_id.0.clone(),
            plan_id: contract.plan.plan_id.0.clone(),
            lifecycle_state: contract.lifecycle_state,
            step_count: contract.plan.steps.len(),
            precondition_count: contract.plan.preconditions.len(),
            compensation_count: contract.plan.compensations.len(),
            legal_transitions: mcp_tx_transition_info(contract.lifecycle_state),
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

pub(super) struct WaTxShowTool {
    config: Arc<Config>,
}

impl WaTxShowTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaTxShowTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.tx_show".to_string(),
            description: Some(
                "Inspect mission tx lifecycle, receipts, and legal transitions (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "contract_file": { "type": "string", "description": "Optional path to MissionTxContract JSON (default: .ft/mission/tx-active.json)" },
                    "include_contract": { "type": "boolean", "default": false, "description": "Include full contract payload in response" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "tx".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: TxShowParams = if arguments.is_null() {
            TxShowParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some(
                            "Expected object with optional contract_file, include_contract"
                                .to_string(),
                        ),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let contract_path = match mcp_resolve_mission_tx_file_path(
            self.config.as_ref(),
            params.contract_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let contract = match mcp_load_mission_tx_contract_from_path(&contract_path) {
            Ok(contract) => contract,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let data = McpTxShowData {
            contract_file: contract_path.display().to_string(),
            tx_id: contract.intent.tx_id.0.clone(),
            plan_id: contract.plan.plan_id.0.clone(),
            lifecycle_state: contract.lifecycle_state,
            outcome: contract.outcome.clone(),
            step_count: contract.plan.steps.len(),
            precondition_count: contract.plan.preconditions.len(),
            compensation_count: contract.plan.compensations.len(),
            receipt_count: contract.receipts.len(),
            legal_transitions: mcp_tx_transition_info(contract.lifecycle_state),
            contract: params.include_contract.then_some(contract),
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

pub(super) struct WaTxRunTool {
    config: Arc<Config>,
}

impl WaTxRunTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaTxRunTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.tx_run".to_string(),
            description: Some(
                "Execute deterministic tx prepare+commit and compensation on partial failure (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "contract_file": { "type": "string", "description": "Optional path to MissionTxContract JSON (default: .ft/mission/tx-active.json)" },
                    "fail_step": { "type": "string", "description": "Deterministic commit failure injection step_id" },
                    "paused": { "type": "boolean", "default": false, "description": "Treat mission as paused; commit returns pause-suspended outcome" },
                    "kill_switch": { "type": "string", "description": "off|safe_mode|hard_stop (safe-mode/hard-stop also accepted)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "tx".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: TxRunParams = if arguments.is_null() {
            TxRunParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some(
                            "Expected object with optional contract_file, fail_step, paused, kill_switch"
                                .to_string(),
                        ),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let contract_path = match mcp_resolve_mission_tx_file_path(
            self.config.as_ref(),
            params.contract_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let contract = match mcp_load_mission_tx_contract_from_path(&contract_path) {
            Ok(contract) => contract,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let kill_switch = match mcp_parse_mission_kill_switch(params.kill_switch.as_deref()) {
            Ok(level) => level,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        if let Some(fail_step_id) = params.fail_step.as_deref()
            && !contract
                .plan
                .steps
                .iter()
                .any(|step| step.step_id.0 == fail_step_id)
        {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                format!("Unknown fail_step: {fail_step_id}"),
                Some("Use step IDs from wa.tx_show(include_contract=true).".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let now_ms = i64::try_from(now_ms()).unwrap_or(0);
        let gate_inputs = mcp_build_tx_prepare_gate_inputs(&contract);
        let prepare_report = match crate::plan::evaluate_prepare_phase(
            &contract.intent.tx_id,
            &contract.plan,
            &gate_inputs,
            kill_switch,
            now_ms,
        ) {
            Ok(report) => report,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    "robot.tx_execution_failed",
                    format!("prepare phase failed: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let mut commit_report = None;
        let mut compensation_report = None;
        let mut final_state = match prepare_report.outcome {
            crate::plan::TxPrepareOutcome::AllReady => crate::plan::MissionTxState::Prepared,
            crate::plan::TxPrepareOutcome::Denied => crate::plan::MissionTxState::Failed,
            crate::plan::TxPrepareOutcome::Deferred => crate::plan::MissionTxState::Planned,
        };

        if prepare_report.outcome.commit_eligible() {
            let mut prepared_contract = contract.clone();
            prepared_contract.lifecycle_state = crate::plan::MissionTxState::Prepared;
            let commit_inputs = mcp_build_tx_commit_step_inputs(
                &prepared_contract,
                params.fail_step.as_deref(),
                now_ms,
            );
            let commit = match crate::plan::execute_commit_phase(
                &prepared_contract,
                &commit_inputs,
                kill_switch,
                params.paused,
                now_ms,
            ) {
                Ok(report) => report,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        "robot.tx_execution_failed",
                        format!("commit phase failed: {err}"),
                        None,
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            };

            final_state = commit.outcome.target_tx_state();
            if commit.failed_count > 0 && commit.committed_count > 0 {
                let mut compensating_contract = prepared_contract.clone();
                compensating_contract.lifecycle_state = crate::plan::MissionTxState::Compensating;
                compensating_contract.receipts.clone_from(&commit.receipts);
                let comp_inputs = mcp_build_tx_compensation_inputs(&commit, None, now_ms);
                let compensation = match crate::plan::execute_compensation_phase(
                    &compensating_contract,
                    &commit,
                    &comp_inputs,
                    now_ms,
                ) {
                    Ok(report) => report,
                    Err(err) => {
                        let envelope = McpEnvelope::<()>::error(
                            "robot.tx_execution_failed",
                            format!("compensation phase failed: {err}"),
                            None,
                            elapsed_ms(start),
                        );
                        return envelope_to_content(envelope);
                    }
                };
                final_state = compensation.outcome.target_tx_state();
                compensation_report = Some(compensation);
            }

            commit_report = Some(commit);
        }

        let data = McpTxRunData {
            contract_file: contract_path.display().to_string(),
            tx_id: contract.intent.tx_id.0.clone(),
            plan_id: contract.plan.plan_id.0.clone(),
            prepare_report,
            commit_report,
            compensation_report,
            final_state,
        };
        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

pub(super) struct WaTxRollbackTool {
    config: Arc<Config>,
}

impl WaTxRollbackTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaTxRollbackTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.tx_rollback".to_string(),
            description: Some(
                "Execute compensation phase for committed tx steps (robot parity)".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "contract_file": { "type": "string", "description": "Optional path to MissionTxContract JSON (default: .ft/mission/tx-active.json)" },
                    "fail_compensation_for_step": { "type": "string", "description": "Deterministic compensation failure injection step_id" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "tx".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: TxRollbackParams = if arguments.is_null() {
            TxRollbackParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some(
                            "Expected object with optional contract_file, fail_compensation_for_step"
                                .to_string(),
                        ),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let contract_path = match mcp_resolve_mission_tx_file_path(
            self.config.as_ref(),
            params.contract_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let contract = match mcp_load_mission_tx_contract_from_path(&contract_path) {
            Ok(contract) => contract,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        if let Some(step_id) = params.fail_compensation_for_step.as_deref()
            && !contract
                .plan
                .steps
                .iter()
                .any(|step| step.step_id.0 == step_id)
        {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                format!("Unknown fail_compensation_for_step: {step_id}"),
                Some("Use step IDs from wa.tx_show(include_contract=true).".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let now_ms = i64::try_from(now_ms()).unwrap_or(0);
        let commit_report = mcp_build_tx_synthetic_commit_report(&contract, now_ms);
        let comp_inputs = mcp_build_tx_compensation_inputs(
            &commit_report,
            params.fail_compensation_for_step.as_deref(),
            now_ms,
        );
        let mut compensating_contract = contract.clone();
        compensating_contract.lifecycle_state = crate::plan::MissionTxState::Compensating;
        compensating_contract
            .receipts
            .clone_from(&contract.receipts);
        let compensation_report = match crate::plan::execute_compensation_phase(
            &compensating_contract,
            &commit_report,
            &comp_inputs,
            now_ms,
        ) {
            Ok(report) => report,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    "robot.tx_execution_failed",
                    format!("rollback compensation failed: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let data = McpTxRollbackData {
            contract_file: contract_path.display().to_string(),
            tx_id: contract.intent.tx_id.0.clone(),
            plan_id: contract.plan.plan_id.0.clone(),
            final_state: compensation_report.outcome.target_tx_state(),
            compensation_report,
        };
        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

pub(super) struct WaReservationsTool {
    db_path: Arc<PathBuf>,
}

impl WaReservationsTool {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ToolHandler for WaReservationsTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.reservations".to_string(),
            description: Some("List active pane reservations (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pane_id": { "type": "integer", "minimum": 0, "description": "Filter by pane ID" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "reservations".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: ReservationsParams = if arguments.is_null() {
            ReservationsParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(p) => p,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional pane_id".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;
            storage.list_active_reservations().await
        });

        match result {
            Ok(reservations) => {
                let filtered: Vec<&PaneReservation> = reservations
                    .iter()
                    .filter(|r| match params.pane_id {
                        Some(pane_id) => r.pane_id == pane_id,
                        None => true,
                    })
                    .collect();

                let total = filtered.len();
                let items: Vec<McpReservationInfo> =
                    filtered.into_iter().map(reservation_to_mcp_info).collect();

                let data = McpReservationsData {
                    reservations: items,
                    total,
                    pane_filter: params.pane_id,
                };
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaReserveTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
}

impl WaReserveTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        Self { config, db_path }
    }
}

impl ToolHandler for WaReserveTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.reserve".to_string(),
            description: Some("Create an exclusive pane reservation (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pane_id": { "type": "integer", "minimum": 0, "description": "Pane ID to reserve" },
                    "owner_kind": { "type": "string", "description": "Kind of owner (workflow, agent, mcp, manual)" },
                    "owner_id": { "type": "string", "description": "Unique identifier for the owner" },
                    "reason": { "type": "string", "description": "Human-readable reason for reservation" },
                    "ttl_ms": { "type": "integer", "minimum": 1000, "default": 300000, "description": "Time to live in milliseconds" }
                },
                "required": ["pane_id", "owner_kind", "owner_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "reservations".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: ReserveParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some(
                        "Expected object with pane_id, owner_kind, owner_id (required), reason, ttl_ms"
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let config = Arc::clone(&self.config);
        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<McpReserveData, McpToolError> =
            runtime.block_on(async move {
                let storage = StorageHandle::new(&db_path.to_string_lossy())
                    .await
                    .map_err(McpToolError::from_error)?;

                let mut engine = build_policy_engine(&config, config.safety.require_prompt_active);
                let summary = format!("reserve pane {}", params.pane_id);
                let input = mcp_reserve_pane_policy_input(params.pane_id, &summary);

                let decision = engine.authorize(&input);
                if decision.is_denied() {
                    let reason = policy_reason(&decision)
                        .unwrap_or("Reservation denied by policy")
                        .to_string();
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, None));
                }
                if decision.requires_approval() {
                    let workspace_id =
                        resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
                    let store =
                        ApprovalStore::new(&storage, config.safety.approval.clone(), workspace_id);
                    let updated = store
                        .attach_to_decision(decision, &input, None)
                        .await
                        .map_err(McpToolError::from_error)?;
                    let reason = policy_reason(&updated)
                        .unwrap_or("Reservation requires approval")
                        .to_string();
                    let hint = approval_command(&updated);
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                }

                let reservation = storage
                    .create_reservation(
                        params.pane_id,
                        &params.owner_kind,
                        &params.owner_id,
                        params.reason.as_deref(),
                        params.ttl_ms,
                    )
                    .await
                    .map_err(McpToolError::from_error)?;

                Ok(McpReserveData {
                    reservation: reservation_to_mcp_info(&reservation),
                })
            });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = if err.message.contains("already has active reservation") {
                    (
                        MCP_ERR_RESERVATION_CONFLICT,
                        Some("Use wa.reservations to check existing reservations".to_string()),
                    )
                } else {
                    (err.code, err.hint)
                };
                let envelope = McpEnvelope::<()>::error(code, err.message, hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaReleaseTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
}

impl WaReleaseTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        Self { config, db_path }
    }
}

impl ToolHandler for WaReleaseTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.release".to_string(),
            description: Some("Release a pane reservation by ID (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "reservation_id": { "type": "integer", "description": "Reservation ID to release" }
                },
                "required": ["reservation_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "reservations".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: ReleaseParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with reservation_id (required)".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let config = Arc::clone(&self.config);
        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<McpReleaseData, McpToolError> =
            runtime.block_on(async move {
                let storage = StorageHandle::new(&db_path.to_string_lossy())
                    .await
                    .map_err(McpToolError::from_error)?;

                let active = storage
                    .list_active_reservations()
                    .await
                    .map_err(McpToolError::from_error)?;
                let pane_id = active
                    .iter()
                    .find(|r| r.id == params.reservation_id)
                    .map(|r| r.pane_id);

                let mut engine = build_policy_engine(&config, config.safety.require_prompt_active);
                let summary = format!("release reservation {}", params.reservation_id);
                let input = mcp_release_pane_policy_input(&summary, pane_id);

                let decision = engine.authorize(&input);
                if decision.is_denied() {
                    let reason = policy_reason(&decision)
                        .unwrap_or("Release denied by policy")
                        .to_string();
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, None));
                }
                if decision.requires_approval() {
                    let workspace_id =
                        resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
                    let store =
                        ApprovalStore::new(&storage, config.safety.approval.clone(), workspace_id);
                    let updated = store
                        .attach_to_decision(decision, &input, None)
                        .await
                        .map_err(McpToolError::from_error)?;
                    let reason = policy_reason(&updated)
                        .unwrap_or("Release requires approval")
                        .to_string();
                    let hint = approval_command(&updated);
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                }

                let released = storage
                    .release_reservation(params.reservation_id)
                    .await
                    .map_err(McpToolError::from_error)?;
                Ok(McpReleaseData {
                    reservation_id: params.reservation_id,
                    released,
                })
            });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaAccountsTool {
    db_path: Arc<PathBuf>,
}

impl WaAccountsTool {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ToolHandler for WaAccountsTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.accounts".to_string(),
            description: Some(
                "List accounts for a service with usage info (robot parity)".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string", "description": "Service name (openai, anthropic, google)" }
                },
                "required": ["service"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "accounts".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: AccountsParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with service (required)".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;
            storage.get_accounts_by_service(&params.service).await
        });

        match result {
            Ok(accounts) => {
                let total = accounts.len();
                let items: Vec<McpAccountInfo> = accounts
                    .into_iter()
                    .map(|a| McpAccountInfo {
                        account_id: a.account_id,
                        service: a.service,
                        name: a.name,
                        percent_remaining: a.percent_remaining,
                        reset_at: a.reset_at,
                        tokens_used: a.tokens_used,
                        tokens_remaining: a.tokens_remaining,
                        tokens_limit: a.tokens_limit,
                        last_refreshed_at: a.last_refreshed_at,
                        last_used_at: a.last_used_at,
                    })
                    .collect();

                let data = McpAccountsData {
                    accounts: items,
                    total,
                    service: params.service,
                };
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaAccountsRefreshTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
}

impl WaAccountsRefreshTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        Self { config, db_path }
    }
}

fn accounts_refresh_policy_input(summary: &str) -> PolicyInput {
    PolicyInput::new(ActionKind::ExecCommand, ActorKind::Mcp)
        .with_surface(PolicySurface::Mcp)
        .with_text_summary(summary.to_string())
        .with_command_text(summary.to_string())
}

impl ToolHandler for WaAccountsRefreshTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.accounts_refresh".to_string(),
            description: Some("Refresh account usage via caut (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string", "description": "Service name (openai)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "accounts".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: AccountsRefreshParams = if arguments.is_null() {
            AccountsRefreshParams { service: None }
        } else {
            match serde_json::from_value(arguments) {
                Ok(p) => p,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional service".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let config = Arc::clone(&self.config);
        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<McpAccountsRefreshData, McpToolError> =
            runtime.block_on(async move {
                let service = params.service.unwrap_or_else(|| "openai".to_string());
                let caut_service = parse_caut_service(&service).ok_or_else(|| {
                    McpToolError::new(
                        MCP_ERR_INVALID_ARGS,
                        format!("Unknown service: {service}"),
                        Some(format!(
                            "Supported services: {}",
                            crate::caut::CautService::supported_cli_inputs().join(", ")
                        )),
                    )
                })?;

                let storage = StorageHandle::new(&db_path.to_string_lossy())
                    .await
                    .map_err(McpToolError::from_error)?;

                let mut engine = build_policy_engine(&config, false);
                let summary = format!("caut refresh {service}");
                let input = accounts_refresh_policy_input(&summary);
                let decision = engine.authorize(&input);
                if decision.is_denied() {
                    let reason = policy_reason(&decision)
                        .unwrap_or("Refresh denied by policy")
                        .to_string();
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, None));
                }
                if decision.requires_approval() {
                    let workspace_id =
                        resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
                    let store = ApprovalStore::new(
                        &storage,
                        config.safety.approval.clone(),
                        workspace_id,
                    );
                    let updated = store
                        .attach_to_decision(decision, &input, Some(summary))
                        .await
                        .map_err(McpToolError::from_error)?;
                    let reason = policy_reason(&updated)
                        .unwrap_or("Refresh requires approval")
                        .to_string();
                    let hint = approval_command(&updated);
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                }

                if let Ok(accounts) = storage.get_accounts_by_service(&service).await {
                    let now_check = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64;
                    let most_recent = accounts.iter().map(|a| a.last_refreshed_at).max().unwrap_or(0);
                    if let Some((secs_ago, wait_secs)) =
                        check_refresh_cooldown(most_recent, now_check, MCP_REFRESH_COOLDOWN_MS)
                    {
                        return Err(McpToolError::new(
                            MCP_ERR_POLICY,
                            format!(
                                "Refresh rate limited: last refresh was {secs_ago}s ago (cooldown: {}s)",
                                MCP_REFRESH_COOLDOWN_MS / 1000
                            ),
                            Some(format!(
                                "Wait {wait_secs}s before refreshing again, or use wa.accounts to view cached data."
                            )),
                        ));
                    }
                }

                let caut = CautClient::new();
                let refresh_result = caut
                    .refresh(caut_service)
                    .await
                    .map_err(McpToolError::from_caut_error)?;

                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;

                let mut account_infos = Vec::new();
                for usage in &refresh_result.accounts {
                    let record = AccountRecord::from_caut(usage, caut_service, now_ms);
                    if let Err(e) = storage.upsert_account(record.clone()).await {
                        tracing::warn!("Failed to upsert account {}: {e}", record.account_id);
                    }
                    account_infos.push(McpAccountInfo {
                        account_id: record.account_id,
                        service: record.service,
                        name: record.name,
                        percent_remaining: record.percent_remaining,
                        reset_at: record.reset_at,
                        tokens_used: record.tokens_used,
                        tokens_remaining: record.tokens_remaining,
                        tokens_limit: record.tokens_limit,
                        last_refreshed_at: record.last_refreshed_at,
                        last_used_at: record.last_used_at,
                    });
                }

                Ok(McpAccountsRefreshData {
                    service,
                    refreshed_count: account_infos.len(),
                    refreshed_at: refresh_result.refreshed_at,
                    accounts: account_infos,
                })
            });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

// ── Mission MCP tools (ft-1i2ge.5.3) ────────────────────────────────────

// wa.mission_state tool
pub(super) struct WaMissionStateTool {
    config: Arc<Config>,
}

impl WaMissionStateTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaMissionStateTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.mission_state".to_string(),
            description: Some(
                "Query mission lifecycle state, assignments, and counters with optional filtering (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mission_file": { "type": "string", "description": "Optional path to mission JSON (default: .ft/mission/active.json)" },
                    "mission_state": { "type": "string", "description": "Filter by lifecycle state (e.g., running, paused, completed)" },
                    "run_state": { "type": "string", "description": "Filter assignments by run state (pending, succeeded, failed, cancelled)" },
                    "agent_state": { "type": "string", "description": "Filter by agent approval state (not_required, pending, approved, denied, expired)" },
                    "action_state": { "type": "string", "description": "Filter by action state (ready, blocked, completed)" },
                    "assignment_id": { "type": "string", "description": "Filter to specific assignment ID" },
                    "assignee": { "type": "string", "description": "Filter by assignee name" },
                    "limit": { "type": "integer", "minimum": 1, "description": "Max assignments to return (default: 100)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "mission".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: MissionStateParams = if arguments.is_null() {
            MissionStateParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional mission_file, filters".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let mission_path = match mcp_resolve_mission_file_path(
            self.config.as_ref(),
            params.mission_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let mission = match mcp_load_mission_from_path(&mission_path) {
            Ok(m) => m,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        // Check mission_state filter
        if let Some(ref filter_state) = params.mission_state {
            let current = mission.lifecycle_state.to_string();
            if !current.eq_ignore_ascii_case(filter_state) {
                let data = McpMissionStateData {
                    mission_file: mission_path.display().to_string(),
                    mission_id: mission.mission_id.0.clone(),
                    title: mission.title.clone(),
                    mission_hash: mission.compute_hash(),
                    lifecycle_state: current,
                    candidate_count: mission.candidates.len(),
                    assignment_count: mission.assignments.len(),
                    matched_assignment_count: 0,
                    returned_assignment_count: 0,
                    assignment_counters: McpMissionAssignmentCounters {
                        pending_approval: 0,
                        approved: 0,
                        denied: 0,
                        expired: 0,
                        succeeded: 0,
                        failed: 0,
                        cancelled: 0,
                        unresolved: 0,
                    },
                    available_transitions: mcp_mission_lifecycle_transitions(
                        mission.lifecycle_state,
                    ),
                    assignments: Vec::new(),
                };
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        }

        let (assignments, counters, matched_count) =
            mcp_build_mission_assignments(&mission, &params);
        let returned_count = assignments.len();

        let data = McpMissionStateData {
            mission_file: mission_path.display().to_string(),
            mission_id: mission.mission_id.0.clone(),
            title: mission.title.clone(),
            mission_hash: mission.compute_hash(),
            lifecycle_state: mission.lifecycle_state.to_string(),
            candidate_count: mission.candidates.len(),
            assignment_count: mission.assignments.len(),
            matched_assignment_count: matched_count,
            returned_assignment_count: returned_count,
            assignment_counters: counters,
            available_transitions: mcp_mission_lifecycle_transitions(mission.lifecycle_state),
            assignments,
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.mission_explain tool
pub(super) struct WaMissionExplainTool {
    config: Arc<Config>,
}

impl WaMissionExplainTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaMissionExplainTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.mission_explain".to_string(),
            description: Some(
                "Show legal lifecycle transitions, failure catalog, and optional assignment context (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mission_file": { "type": "string", "description": "Optional path to mission JSON (default: .ft/mission/active.json)" },
                    "assignment_id": { "type": "string", "description": "Optional assignment ID for dispatch context details" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "mission".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: MissionExplainParams = if arguments.is_null() {
            MissionExplainParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional mission_file".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let mission_path = match mcp_resolve_mission_file_path(
            self.config.as_ref(),
            params.mission_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let mission = match mcp_load_mission_from_path(&mission_path) {
            Ok(m) => m,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        // Build assignment context if requested
        let assignment_context = if let Some(ref aid) = params.assignment_id {
            let found = mission
                .assignments
                .iter()
                .find(|a| a.assignment_id.0 == *aid);
            found.map(|a| {
                serde_json::json!({
                    "assignment_id": a.assignment_id.0,
                    "candidate_id": a.candidate_id.0,
                    "assignee": a.assignee,
                    "approval_state": a.approval_state.canonical_string(),
                    "outcome": a.outcome.as_ref().map(|o| match o {
                        crate::plan::Outcome::Success { .. } => "success",
                        crate::plan::Outcome::Failed { .. } => "failed",
                        crate::plan::Outcome::Cancelled { .. } => "cancelled",
                    }),
                })
            })
        } else {
            None
        };

        let data = McpMissionExplainData {
            mission_file: mission_path.display().to_string(),
            mission_id: mission.mission_id.0.clone(),
            title: mission.title.clone(),
            lifecycle_state: mission.lifecycle_state.to_string(),
            available_transitions: mcp_mission_lifecycle_transitions(mission.lifecycle_state),
            failure_catalog: mcp_mission_failure_catalog(),
            assignment_context,
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.mission_pause tool
pub(super) struct WaMissionPauseTool {
    config: Arc<Config>,
}

impl WaMissionPauseTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaMissionPauseTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.mission_pause".to_string(),
            description: Some(
                "Pause an active mission, creating a checkpoint (robot parity)".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mission_file": { "type": "string", "description": "Optional path to mission JSON (default: .ft/mission/active.json)" },
                    "reason": { "type": "string", "description": "Reason code for the pause (required)" },
                    "requested_by": { "type": "string", "description": "Who requested the pause (default: mcp-agent)" }
                },
                "required": ["reason"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "mission".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: MissionPauseParams = match serde_json::from_value(arguments) {
            Ok(parsed) => parsed,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with reason (required)".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let reason = match &params.reason {
            Some(r) if !r.trim().is_empty() => r.clone(),
            _ => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    "reason is required and must not be empty".to_string(),
                    Some("Provide a reason code for the pause.".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let mission_path = match mcp_resolve_mission_file_path(
            self.config.as_ref(),
            params.mission_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let mut mission = match mcp_load_mission_from_path(&mission_path) {
            Ok(m) => m,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let requested_at_ms = i64::try_from(now_ms()).unwrap_or(0);
        let decision =
            match mission.pause_mission(&params.requested_by, &reason, requested_at_ms, None) {
                Ok(d) => d,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Cannot pause mission: {err}"),
                        Some("Use wa.mission_explain to see valid transitions.".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            };

        if let Err(err) = mcp_save_mission_to_path(&mission_path, &mission) {
            let envelope =
                McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
            return envelope_to_content(envelope);
        }

        let data = McpMissionControlData {
            command: "pause".to_string(),
            mission_file: mission_path.display().to_string(),
            mission_id: mission.mission_id.0.clone(),
            lifecycle_from: decision.lifecycle_from.to_string(),
            lifecycle_to: decision.lifecycle_to.to_string(),
            decision_path: decision.decision_path,
            reason_code: decision.reason_code,
            error_code: decision.error_code,
            checkpoint_id: decision.checkpoint_id,
            mission_hash: mission.compute_hash(),
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.mission_resume tool
pub(super) struct WaMissionResumeTool {
    config: Arc<Config>,
}

impl WaMissionResumeTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaMissionResumeTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.mission_resume".to_string(),
            description: Some(
                "Resume a paused mission, restoring prior lifecycle state (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mission_file": { "type": "string", "description": "Optional path to mission JSON (default: .ft/mission/active.json)" },
                    "requested_by": { "type": "string", "description": "Who requested the resume (default: mcp-agent)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "mission".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: MissionResumeParams = if arguments.is_null() {
            MissionResumeParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional mission_file".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let mission_path = match mcp_resolve_mission_file_path(
            self.config.as_ref(),
            params.mission_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let mut mission = match mcp_load_mission_from_path(&mission_path) {
            Ok(m) => m,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let requested_at_ms = i64::try_from(now_ms()).unwrap_or(0);
        let decision =
            match mission.resume_mission(&params.requested_by, "mcp_resume", requested_at_ms, None)
            {
                Ok(d) => d,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Cannot resume mission: {err}"),
                        Some("Use wa.mission_explain to see valid transitions.".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            };

        if let Err(err) = mcp_save_mission_to_path(&mission_path, &mission) {
            let envelope =
                McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
            return envelope_to_content(envelope);
        }

        let data = McpMissionControlData {
            command: "resume".to_string(),
            mission_file: mission_path.display().to_string(),
            mission_id: mission.mission_id.0.clone(),
            lifecycle_from: decision.lifecycle_from.to_string(),
            lifecycle_to: decision.lifecycle_to.to_string(),
            decision_path: decision.decision_path,
            reason_code: decision.reason_code,
            error_code: decision.error_code,
            checkpoint_id: decision.checkpoint_id,
            mission_hash: mission.compute_hash(),
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.mission_abort tool
pub(super) struct WaMissionAbortTool {
    config: Arc<Config>,
}

impl WaMissionAbortTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaMissionAbortTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.mission_abort".to_string(),
            description: Some(
                "Abort a mission, cancelling all in-flight assignments (robot parity)".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mission_file": { "type": "string", "description": "Optional path to mission JSON (default: .ft/mission/active.json)" },
                    "reason": { "type": "string", "description": "Reason code for the abort (required)" },
                    "requested_by": { "type": "string", "description": "Who requested the abort (default: mcp-agent)" },
                    "error_code": { "type": "string", "description": "Optional error code for the abort" }
                },
                "required": ["reason"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "mission".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: MissionAbortParams = match serde_json::from_value(arguments) {
            Ok(parsed) => parsed,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with reason (required)".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let reason = match &params.reason {
            Some(r) if !r.trim().is_empty() => r.clone(),
            _ => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    "reason is required and must not be empty".to_string(),
                    Some("Provide a reason code for the abort.".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let mission_path = match mcp_resolve_mission_file_path(
            self.config.as_ref(),
            params.mission_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let mut mission = match mcp_load_mission_from_path(&mission_path) {
            Ok(m) => m,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let requested_at_ms = i64::try_from(now_ms()).unwrap_or(0);
        let decision = match mission.abort_mission(
            &params.requested_by,
            &reason,
            params.error_code.clone(),
            requested_at_ms,
            None,
        ) {
            Ok(d) => d,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Cannot abort mission: {err}"),
                    Some("Use wa.mission_explain to see valid transitions.".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        if let Err(err) = mcp_save_mission_to_path(&mission_path, &mission) {
            let envelope =
                McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
            return envelope_to_content(envelope);
        }

        let data = McpMissionControlData {
            command: "abort".to_string(),
            mission_file: mission_path.display().to_string(),
            mission_id: mission.mission_id.0.clone(),
            lifecycle_from: decision.lifecycle_from.to_string(),
            lifecycle_to: decision.lifecycle_to.to_string(),
            decision_path: decision.decision_path,
            reason_code: decision.reason_code,
            error_code: decision.error_code,
            checkpoint_id: decision.checkpoint_id,
            mission_hash: mission.compute_hash(),
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.events_annotate tool (bd-2gce) — extracted from mcp.rs [ft-1fv0u]
pub(super) struct WaEventsAnnotateTool {
    db_path: Arc<PathBuf>,
}

impl WaEventsAnnotateTool {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ToolHandler for WaEventsAnnotateTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.events_annotate".to_string(),
            description: Some("Set or clear an event note (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "event_id": { "type": "integer", "minimum": 1, "description": "Event ID" },
                    "note": { "type": "string", "description": "Note text to set" },
                    "clear": { "type": "boolean", "default": false, "description": "Clear the note" },
                    "by": { "type": "string", "description": "Actor identifier (optional)" }
                },
                "required": ["event_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "events".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: EventsAnnotateParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected { event_id, note? | clear=true, by? }".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        if params.clear == params.note.is_some() {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "Invalid params: specify exactly one of note or clear".to_string(),
                Some("Example: {\"event_id\":123,\"note\":\"Investigating\"}".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: crate::Result<McpEventMutationData> = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;

            storage
                .set_event_note(params.event_id, params.note.clone(), params.by.clone())
                .await?;

            let ts = i64::try_from(now_ms()).unwrap_or(0);
            let input_summary = if params.clear {
                format!("wa.events_annotate event_id={} clear=true", params.event_id)
            } else {
                format!(
                    "wa.events_annotate event_id={} note=<redacted>",
                    params.event_id
                )
            };
            let decision_context = mcp_event_mutation_decision_context(
                "wa.events_annotate",
                "event.annotate",
                params.event_id,
                if params.clear {
                    "clear_note"
                } else {
                    "set_note"
                },
                params.by.as_deref(),
                &input_summary,
                ts,
            );
            let audit = crate::storage::AuditActionRecord {
                id: 0,
                ts,
                actor_kind: "mcp".to_string(),
                actor_id: params.by.clone(),
                correlation_id: None,
                pane_id: None,
                domain: None,
                action_kind: "event.annotate".to_string(),
                policy_decision: "allow".to_string(),
                decision_reason: Some("MCP updated event note".to_string()),
                rule_id: None,
                input_summary: Some(input_summary),
                verification_summary: None,
                decision_context: serialize_mcp_audit_decision_context(&decision_context),
                result: "success".to_string(),
            };
            let _ = storage.record_audit_action_redacted(audit).await;

            let annotations = storage
                .get_event_annotations(params.event_id)
                .await?
                .ok_or_else(|| {
                    crate::Error::Storage(crate::StorageError::Database(format!(
                        "Event {} not found",
                        params.event_id
                    )))
                })?;
            Ok(McpEventMutationData {
                event_id: params.event_id,
                changed: None,
                annotations,
            })
        });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

// wa.events_triage tool — extracted from mcp.rs [ft-1fv0u]
pub(super) struct WaEventsTriageTool {
    db_path: Arc<PathBuf>,
}

impl WaEventsTriageTool {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ToolHandler for WaEventsTriageTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.events_triage".to_string(),
            description: Some("Set or clear an event triage state (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "event_id": { "type": "integer", "minimum": 1, "description": "Event ID" },
                    "state": { "type": "string", "description": "Triage state to set" },
                    "clear": { "type": "boolean", "default": false, "description": "Clear the triage state" },
                    "by": { "type": "string", "description": "Actor identifier (optional)" }
                },
                "required": ["event_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "events".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: EventsTriageParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected { event_id, state? | clear=true, by? }".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        if params.clear == params.state.is_some() {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "Invalid params: specify exactly one of state or clear".to_string(),
                Some("Example: {\"event_id\":123,\"state\":\"investigating\"}".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: crate::Result<McpEventMutationData> = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;

            let changed = storage
                .set_event_triage_state(params.event_id, params.state.clone(), params.by.clone())
                .await?;

            let ts = i64::try_from(now_ms()).unwrap_or(0);
            let input_summary = if params.clear {
                format!("wa.events_triage event_id={} clear=true", params.event_id)
            } else {
                format!(
                    "wa.events_triage event_id={} state={}",
                    params.event_id,
                    params.state.clone().unwrap_or_default()
                )
            };
            let mut decision_context = mcp_event_mutation_decision_context(
                "wa.events_triage",
                "event.triage",
                params.event_id,
                if params.clear {
                    "clear_triage_state"
                } else {
                    "set_triage_state"
                },
                params.by.as_deref(),
                &input_summary,
                ts,
            );
            if let Some(state) = params.state.as_ref() {
                decision_context.add_evidence("state", state);
            }
            decision_context.add_evidence("changed", changed.to_string());
            let audit = crate::storage::AuditActionRecord {
                id: 0,
                ts,
                actor_kind: "mcp".to_string(),
                actor_id: params.by.clone(),
                correlation_id: None,
                pane_id: None,
                domain: None,
                action_kind: "event.triage".to_string(),
                policy_decision: "allow".to_string(),
                decision_reason: Some("MCP updated event triage".to_string()),
                rule_id: None,
                input_summary: Some(input_summary),
                verification_summary: None,
                decision_context: serialize_mcp_audit_decision_context(&decision_context),
                result: if changed {
                    "success".to_string()
                } else {
                    "noop".to_string()
                },
            };
            let _ = storage.record_audit_action_redacted(audit).await;

            let annotations = storage
                .get_event_annotations(params.event_id)
                .await?
                .ok_or_else(|| {
                    crate::Error::Storage(crate::StorageError::Database(format!(
                        "Event {} not found",
                        params.event_id
                    )))
                })?;
            Ok(McpEventMutationData {
                event_id: params.event_id,
                changed: Some(changed),
                annotations,
            })
        });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

// wa.events_label tool — extracted from mcp.rs [ft-1fv0u]
pub(super) struct WaEventsLabelTool {
    db_path: Arc<PathBuf>,
}

impl WaEventsLabelTool {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ToolHandler for WaEventsLabelTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.events_label".to_string(),
            description: Some("Add/remove/list event labels (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "event_id": { "type": "integer", "minimum": 1, "description": "Event ID" },
                    "add": { "type": "string", "description": "Label to add" },
                    "remove": { "type": "string", "description": "Label to remove" },
                    "list": { "type": "boolean", "default": false, "description": "List labels only" },
                    "by": { "type": "string", "description": "Actor identifier (optional; applies to add)" }
                },
                "required": ["event_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "events".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: EventsLabelParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected { event_id, add? | remove? | list=true, by? }".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let mut ops = 0;
        if params.add.is_some() {
            ops += 1;
        }
        if params.remove.is_some() {
            ops += 1;
        }
        if params.list {
            ops += 1;
        }
        if ops != 1 {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "Invalid params: specify exactly one of add/remove/list".to_string(),
                Some("Example: {\"event_id\":123,\"add\":\"urgent\"}".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: crate::Result<McpEventMutationData> = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;
            let ts = i64::try_from(now_ms()).unwrap_or(0);

            let changed = if let Some(label) = params.add.clone() {
                let inserted = storage
                    .add_event_label(params.event_id, label.clone(), params.by.clone())
                    .await?;
                let input_summary =
                    format!("wa.events_label event_id={} add={label}", params.event_id);

                let mut decision_context = mcp_event_mutation_decision_context(
                    "wa.events_label",
                    "event.label.add",
                    params.event_id,
                    "add_label",
                    params.by.as_deref(),
                    &input_summary,
                    ts,
                );
                decision_context.add_evidence("label", &label);
                decision_context.add_evidence("changed", inserted.to_string());
                let audit = crate::storage::AuditActionRecord {
                    id: 0,
                    ts,
                    actor_kind: "mcp".to_string(),
                    actor_id: params.by.clone(),
                    correlation_id: None,
                    pane_id: None,
                    domain: None,
                    action_kind: "event.label.add".to_string(),
                    policy_decision: "allow".to_string(),
                    decision_reason: Some("MCP added event label".to_string()),
                    rule_id: None,
                    input_summary: Some(input_summary),
                    verification_summary: None,
                    decision_context: serialize_mcp_audit_decision_context(&decision_context),
                    result: if inserted {
                        "success".to_string()
                    } else {
                        "noop".to_string()
                    },
                };
                let _ = storage.record_audit_action_redacted(audit).await;

                Some(inserted)
            } else if let Some(label) = params.remove.clone() {
                let removed = storage
                    .remove_event_label(params.event_id, label.clone())
                    .await?;
                let input_summary = format!(
                    "wa.events_label event_id={} remove={label}",
                    params.event_id
                );

                let mut decision_context = mcp_event_mutation_decision_context(
                    "wa.events_label",
                    "event.label.remove",
                    params.event_id,
                    "remove_label",
                    params.by.as_deref(),
                    &input_summary,
                    ts,
                );
                decision_context.add_evidence("label", &label);
                decision_context.add_evidence("changed", removed.to_string());
                let audit = crate::storage::AuditActionRecord {
                    id: 0,
                    ts,
                    actor_kind: "mcp".to_string(),
                    actor_id: params.by.clone(),
                    correlation_id: None,
                    pane_id: None,
                    domain: None,
                    action_kind: "event.label.remove".to_string(),
                    policy_decision: "allow".to_string(),
                    decision_reason: Some("MCP removed event label".to_string()),
                    rule_id: None,
                    input_summary: Some(input_summary),
                    verification_summary: None,
                    decision_context: serialize_mcp_audit_decision_context(&decision_context),
                    result: if removed {
                        "success".to_string()
                    } else {
                        "noop".to_string()
                    },
                };
                let _ = storage.record_audit_action_redacted(audit).await;

                Some(removed)
            } else {
                None
            };

            let annotations = storage
                .get_event_annotations(params.event_id)
                .await?
                .ok_or_else(|| {
                    crate::Error::Storage(crate::StorageError::Database(format!(
                        "Event {} not found",
                        params.event_id
                    )))
                })?;
            Ok(McpEventMutationData {
                event_id: params.event_id,
                changed,
                annotations,
            })
        });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use super::{
        ActionKind, ActorKind, CompatRuntime, CompatRuntimeBuilder, Config, Content, McpContext,
        PaneCapabilities, PaneFilterConfig, PolicySurface, StorageHandle, Tool, ToolHandler,
        WaAccountsRefreshTool, WaAccountsTool, WaCassSearchTool, WaCassStatusTool, WaCassViewTool,
        WaEventsAnnotateTool, WaEventsLabelTool, WaEventsTool, WaEventsTriageTool, WaGetTextTool,
        WaMissionAbortTool, WaMissionExplainTool, WaMissionPauseTool, WaMissionResumeTool,
        WaMissionStateTool, WaReleaseTool, WaReservationsTool, WaReserveTool, WaRulesListTool,
        WaRulesTestTool, WaSearchTool, WaSendTool, WaStateTool, WaTxPlanTool, WaTxRollbackTool,
        WaTxRunTool, WaTxShowTool, WaWaitForTool, WaWorkflowRunTool, accounts_refresh_policy_input,
        mcp_event_mutation_decision_context, mcp_get_text_policy_input,
        mcp_release_pane_policy_input, mcp_reserve_pane_policy_input,
        mcp_search_output_policy_input, mcp_send_text_policy_input, mcp_workflow_run_policy_input,
        serialize_mcp_audit_decision_context,
    };
    use crate::mcp_error::MCP_ERR_INVALID_ARGS;
    use crate::plan::{
        MISSION_TX_SCHEMA_VERSION, MissionActorRole, MissionTxContract, MissionTxState, StepAction,
        TxCompensation, TxId, TxIntent, TxOutcome, TxPlan, TxPlanId, TxPrecondition, TxStep,
        TxStepId,
    };
    use tempfile::TempDir;

    fn db_path() -> Arc<PathBuf> {
        Arc::new(PathBuf::from("/tmp/test-mcp.db"))
    }

    fn config() -> Arc<Config> {
        Arc::new(Config::default())
    }

    fn temp_db_path() -> (TempDir, Arc<PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-tools-test.db");
        (dir, Arc::new(path))
    }

    fn test_mcp_context() -> McpContext {
        McpContext::new(asupersync::Cx::for_testing(), 1)
    }

    fn seed_event(db_path: &Path) -> i64 {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy())
                .await
                .unwrap();
            // Ensure pane exists to satisfy foreign key constraint.
            storage
                .upsert_pane(crate::storage::PaneRecord {
                    pane_id: 7,
                    pane_uuid: None,
                    domain: "local".to_string(),
                    window_id: None,
                    tab_id: None,
                    title: Some("test-pane".to_string()),
                    cwd: None,
                    tty_name: None,
                    first_seen_at: 1_700_000_000_000,
                    last_seen_at: 1_700_000_000_000,
                    observed: true,
                    ignore_reason: None,
                    last_decision_at: None,
                })
                .await
                .unwrap();
            storage
                .record_event(crate::storage::StoredEvent {
                    id: 0,
                    pane_id: 7,
                    rule_id: "codex.usage.reached".to_string(),
                    agent_type: "codex".to_string(),
                    event_type: "usage_limit".to_string(),
                    severity: "warning".to_string(),
                    confidence: 0.95,
                    extracted: None,
                    matched_text: Some("Usage limit reached".to_string()),
                    segment_id: None,
                    detected_at: 1_700_000_000_000,
                    dedupe_key: None,
                    handled_at: None,
                    handled_by_workflow_id: None,
                    handled_status: None,
                })
                .await
                .unwrap()
        })
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

    fn parse_audit_decision_context(
        db_path: &Path,
        action_kind: &str,
    ) -> crate::policy::DecisionContext {
        let audit = latest_audit_action(db_path, action_kind);
        serde_json::from_str(audit.decision_context.as_deref().unwrap()).unwrap()
    }

    fn evidence<'a>(context: &'a crate::policy::DecisionContext, key: &str) -> Option<&'a str> {
        context
            .evidence
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.value.as_str())
    }

    fn sample_tx_contract(state: MissionTxState) -> MissionTxContract {
        let tx_id = TxId("tx:test".to_string());
        MissionTxContract {
            tx_version: MISSION_TX_SCHEMA_VERSION,
            intent: TxIntent {
                tx_id: tx_id.clone(),
                requested_by: MissionActorRole::Dispatcher,
                summary: "tx test".to_string(),
                correlation_id: "corr:test".to_string(),
                created_at_ms: 1_700_000_000_000,
            },
            plan: TxPlan {
                plan_id: TxPlanId("plan:test".to_string()),
                tx_id,
                steps: vec![
                    TxStep {
                        step_id: TxStepId("tx-step:1".to_string()),
                        ordinal: 1,
                        action: StepAction::SendText {
                            pane_id: 1,
                            text: "step-1".to_string(),
                            paste_mode: Some(false),
                        },
                        description: "step 1".to_string(),
                    },
                    TxStep {
                        step_id: TxStepId("tx-step:2".to_string()),
                        ordinal: 2,
                        action: StepAction::SendText {
                            pane_id: 2,
                            text: "step-2".to_string(),
                            paste_mode: Some(false),
                        },
                        description: "step 2".to_string(),
                    },
                    TxStep {
                        step_id: TxStepId("tx-step:3".to_string()),
                        ordinal: 3,
                        action: StepAction::SendText {
                            pane_id: 3,
                            text: "step-3".to_string(),
                            paste_mode: Some(true),
                        },
                        description: "step 3".to_string(),
                    },
                ],
                preconditions: vec![TxPrecondition::PromptActive { pane_id: 1 }],
                compensations: vec![
                    TxCompensation {
                        for_step_id: TxStepId("tx-step:1".to_string()),
                        action: StepAction::SendText {
                            pane_id: 1,
                            text: "undo-1".to_string(),
                            paste_mode: Some(false),
                        },
                    },
                    TxCompensation {
                        for_step_id: TxStepId("tx-step:2".to_string()),
                        action: StepAction::SendText {
                            pane_id: 2,
                            text: "undo-2".to_string(),
                            paste_mode: Some(false),
                        },
                    },
                    TxCompensation {
                        for_step_id: TxStepId("tx-step:3".to_string()),
                        action: StepAction::SendText {
                            pane_id: 3,
                            text: "undo-3".to_string(),
                            paste_mode: Some(true),
                        },
                    },
                ],
            },
            lifecycle_state: state,
            outcome: TxOutcome::Pending,
            receipts: Vec::new(),
        }
    }

    fn write_tx_contract(dir: &TempDir, state: MissionTxState) -> std::path::PathBuf {
        let path = dir.path().join("tx-contract.json");
        let contract = sample_tx_contract(state);
        std::fs::write(&path, serde_json::to_vec_pretty(&contract).unwrap()).unwrap();
        path
    }

    fn parse_json_content(contents: Vec<Content>) -> serde_json::Value {
        assert_eq!(contents.len(), 1, "expected single MCP content item");
        match &contents[0] {
            Content::Text { text } => serde_json::from_str(text).expect("valid MCP envelope json"),
            _ => panic!("expected text content"), // ubs:ignore
        }
    }

    /// Collect definitions for all 29 tools. Guarantees no panics during construction.
    fn all_definitions() -> Vec<Tool> {
        let db = db_path();
        let cfg = config();
        vec![
            WaRulesListTool.definition(),
            WaRulesTestTool.definition(),
            WaCassSearchTool.definition(),
            WaCassViewTool.definition(),
            WaCassStatusTool.definition(),
            WaStateTool::new(PaneFilterConfig::default()).definition(),
            WaGetTextTool::new(Arc::clone(&cfg), Some(Arc::clone(&db))).definition(),
            WaWaitForTool.definition(),
            WaSearchTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaEventsTool::new(Arc::clone(&db)).definition(),
            WaSendTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaWorkflowRunTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaTxPlanTool::new(Arc::clone(&cfg)).definition(),
            WaTxShowTool::new(Arc::clone(&cfg)).definition(),
            WaTxRunTool::new(Arc::clone(&cfg)).definition(),
            WaTxRollbackTool::new(Arc::clone(&cfg)).definition(),
            WaReservationsTool::new(Arc::clone(&db)).definition(),
            WaReserveTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaReleaseTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaAccountsTool::new(Arc::clone(&db)).definition(),
            WaAccountsRefreshTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaMissionStateTool::new(Arc::clone(&cfg)).definition(),
            WaMissionExplainTool::new(Arc::clone(&cfg)).definition(),
            WaMissionPauseTool::new(Arc::clone(&cfg)).definition(),
            WaMissionResumeTool::new(Arc::clone(&cfg)).definition(),
            WaMissionAbortTool::new(Arc::clone(&cfg)).definition(),
            WaEventsAnnotateTool::new(Arc::clone(&db)).definition(),
            WaEventsTriageTool::new(Arc::clone(&db)).definition(),
            WaEventsLabelTool::new(Arc::clone(&db)).definition(),
        ]
    }

    // ========================================================================
    // Tool Count Invariant
    // ========================================================================

    #[test]
    fn tool_count_is_29() {
        assert_eq!(all_definitions().len(), 29);
    }

    // ========================================================================
    // All Tool Names Are Unique
    // ========================================================================

    #[test]
    fn all_tool_names_are_unique() {
        let defs = all_definitions();
        let mut seen = std::collections::HashSet::new();
        for def in &defs {
            assert!(seen.insert(&def.name), "Duplicate tool name: {}", def.name);
        }
    }

    #[test]
    fn accounts_refresh_policy_input_uses_mcp_surface() {
        let summary = "caut refresh openai";
        let input = accounts_refresh_policy_input(summary);

        assert_eq!(input.action, ActionKind::ExecCommand);
        assert_eq!(input.actor, ActorKind::Mcp);
        assert_eq!(input.surface, PolicySurface::Mcp);
        assert_eq!(input.text_summary.as_deref(), Some(summary));
        assert_eq!(input.command_text.as_deref(), Some(summary));
    }

    #[test]
    fn mcp_tool_policy_input_helpers_use_expected_action_actor_and_surface() {
        let summary = "helper summary";

        let get_text = mcp_get_text_policy_input(7, "local", PaneCapabilities::unknown(), summary);
        assert_eq!(get_text.action, ActionKind::ReadOutput);
        assert_eq!(get_text.actor, ActorKind::Mcp);
        assert_eq!(get_text.surface, PolicySurface::Mux);
        assert_eq!(get_text.pane_id, Some(7));
        assert_eq!(get_text.text_summary.as_deref(), Some(summary));

        let search = mcp_search_output_policy_input(summary);
        assert_eq!(search.action, ActionKind::SearchOutput);
        assert_eq!(search.actor, ActorKind::Mcp);
        assert_eq!(search.surface, PolicySurface::Mux);
        assert_eq!(search.text_summary.as_deref(), Some(summary));

        let send = mcp_send_text_policy_input(
            11,
            "local",
            PaneCapabilities::unknown(),
            summary,
            "echo hi",
        );
        assert_eq!(send.action, ActionKind::SendText);
        assert_eq!(send.actor, ActorKind::Mcp);
        assert_eq!(send.surface, PolicySurface::Mux);
        assert_eq!(send.pane_id, Some(11));
        assert_eq!(send.command_text.as_deref(), Some("echo hi"));

        let workflow =
            mcp_workflow_run_policy_input(13, "local", PaneCapabilities::unknown(), summary);
        assert_eq!(workflow.action, ActionKind::WorkflowRun);
        assert_eq!(workflow.actor, ActorKind::Mcp);
        assert_eq!(workflow.surface, PolicySurface::Workflow);
        assert_eq!(workflow.pane_id, Some(13));

        let reserve = mcp_reserve_pane_policy_input(17, summary);
        assert_eq!(reserve.action, ActionKind::ReservePane);
        assert_eq!(reserve.actor, ActorKind::Mcp);
        assert_eq!(reserve.surface, PolicySurface::Swarm);
        assert_eq!(reserve.pane_id, Some(17));
        assert_eq!(reserve.command_text.as_deref(), Some("reserve_pane"));

        let release_with_pane = mcp_release_pane_policy_input(summary, Some(19));
        assert_eq!(release_with_pane.action, ActionKind::ReleasePane);
        assert_eq!(release_with_pane.actor, ActorKind::Mcp);
        assert_eq!(release_with_pane.surface, PolicySurface::Swarm);
        assert_eq!(release_with_pane.pane_id, Some(19));
        assert_eq!(
            release_with_pane.command_text.as_deref(),
            Some("release_reservation")
        );

        let release_without_pane = mcp_release_pane_policy_input(summary, None);
        assert_eq!(release_without_pane.action, ActionKind::ReleasePane);
        assert_eq!(release_without_pane.actor, ActorKind::Mcp);
        assert_eq!(release_without_pane.surface, PolicySurface::Swarm);
        assert_eq!(release_without_pane.pane_id, None);
    }

    #[test]
    fn events_annotate_audit_records_mcp_decision_context() {
        let (_dir, db_path) = temp_db_path();
        let event_id = seed_event(db_path.as_ref().as_path());
        let tool = WaEventsAnnotateTool::new(Arc::clone(&db_path));

        tool.call(
            &test_mcp_context(),
            serde_json::json!({
                "event_id": event_id,
                "note": "Investigating",
                "by": "mcp-client"
            }),
        )
        .unwrap();

        let audit = latest_audit_action(db_path.as_ref().as_path(), "event.annotate");
        assert_eq!(audit.actor_kind, "mcp");
        let context = parse_audit_decision_context(db_path.as_ref().as_path(), "event.annotate");
        let expected_event_id = event_id.to_string();
        assert_eq!(context.action, ActionKind::ExecCommand);
        assert_eq!(context.actor, ActorKind::Mcp);
        assert_eq!(context.surface, PolicySurface::Mcp);
        assert_eq!(
            context.determining_rule.as_deref(),
            Some("audit.event.annotate")
        );
        assert_eq!(evidence(&context, "tool"), Some("wa.events_annotate"));
        assert_eq!(
            evidence(&context, "event_action_kind"),
            Some("event.annotate")
        );
        assert_eq!(
            evidence(&context, "event_id"),
            Some(expected_event_id.as_str())
        );
        assert_eq!(evidence(&context, "operation"), Some("set_note"));
        assert_eq!(evidence(&context, "actor_id"), Some("mcp-client"));
    }

    #[test]
    fn events_triage_audit_records_operation_state_and_change() {
        let (_dir, db_path) = temp_db_path();
        let event_id = seed_event(db_path.as_ref().as_path());
        let tool = WaEventsTriageTool::new(Arc::clone(&db_path));

        tool.call(
            &test_mcp_context(),
            serde_json::json!({
                "event_id": event_id,
                "state": "investigating",
                "by": "mcp-client"
            }),
        )
        .unwrap();

        let audit = latest_audit_action(db_path.as_ref().as_path(), "event.triage");
        assert_eq!(audit.actor_kind, "mcp");
        let context = parse_audit_decision_context(db_path.as_ref().as_path(), "event.triage");
        let expected_event_id = event_id.to_string();
        assert_eq!(context.action, ActionKind::ExecCommand);
        assert_eq!(context.actor, ActorKind::Mcp);
        assert_eq!(context.surface, PolicySurface::Mcp);
        assert_eq!(
            context.determining_rule.as_deref(),
            Some("audit.event.triage")
        );
        assert_eq!(evidence(&context, "tool"), Some("wa.events_triage"));
        assert_eq!(
            evidence(&context, "event_action_kind"),
            Some("event.triage")
        );
        assert_eq!(
            evidence(&context, "event_id"),
            Some(expected_event_id.as_str())
        );
        assert_eq!(evidence(&context, "operation"), Some("set_triage_state"));
        assert_eq!(evidence(&context, "state"), Some("investigating"));
        assert_eq!(evidence(&context, "changed"), Some("true"));
    }

    #[test]
    fn events_label_audit_records_add_and_remove_context() {
        let (_dir, db_path) = temp_db_path();
        let event_id = seed_event(db_path.as_ref().as_path());
        let tool = WaEventsLabelTool::new(Arc::clone(&db_path));

        tool.call(
            &test_mcp_context(),
            serde_json::json!({
                "event_id": event_id,
                "add": "urgent",
                "by": "mcp-client"
            }),
        )
        .unwrap();
        let add_audit = latest_audit_action(db_path.as_ref().as_path(), "event.label.add");
        assert_eq!(add_audit.actor_kind, "mcp");
        let add_context =
            parse_audit_decision_context(db_path.as_ref().as_path(), "event.label.add");
        let expected_event_id = event_id.to_string();
        assert_eq!(add_context.action, ActionKind::ExecCommand);
        assert_eq!(add_context.actor, ActorKind::Mcp);
        assert_eq!(add_context.surface, PolicySurface::Mcp);
        assert_eq!(
            add_context.determining_rule.as_deref(),
            Some("audit.event.label.add")
        );
        assert_eq!(evidence(&add_context, "tool"), Some("wa.events_label"));
        assert_eq!(
            evidence(&add_context, "event_action_kind"),
            Some("event.label.add")
        );
        assert_eq!(
            evidence(&add_context, "event_id"),
            Some(expected_event_id.as_str())
        );
        assert_eq!(evidence(&add_context, "operation"), Some("add_label"));
        assert_eq!(evidence(&add_context, "label"), Some("urgent"));
        assert_eq!(evidence(&add_context, "changed"), Some("true"));
        assert_eq!(evidence(&add_context, "actor_id"), Some("mcp-client"));

        tool.call(
            &test_mcp_context(),
            serde_json::json!({
                "event_id": event_id,
                "remove": "urgent"
            }),
        )
        .unwrap();
        let remove_audit = latest_audit_action(db_path.as_ref().as_path(), "event.label.remove");
        assert_eq!(remove_audit.actor_kind, "mcp");
        let remove_context =
            parse_audit_decision_context(db_path.as_ref().as_path(), "event.label.remove");
        let expected_event_id = event_id.to_string();
        assert_eq!(remove_context.action, ActionKind::ExecCommand);
        assert_eq!(remove_context.actor, ActorKind::Mcp);
        assert_eq!(remove_context.surface, PolicySurface::Mcp);
        assert_eq!(
            remove_context.determining_rule.as_deref(),
            Some("audit.event.label.remove")
        );
        assert_eq!(evidence(&remove_context, "tool"), Some("wa.events_label"));
        assert_eq!(
            evidence(&remove_context, "event_action_kind"),
            Some("event.label.remove")
        );
        assert_eq!(
            evidence(&remove_context, "event_id"),
            Some(expected_event_id.as_str())
        );
        assert_eq!(evidence(&remove_context, "operation"), Some("remove_label"));
        assert_eq!(evidence(&remove_context, "label"), Some("urgent"));
        assert_eq!(evidence(&remove_context, "changed"), Some("true"));
        assert!(evidence(&remove_context, "actor_id").is_none());
    }

    // ========================================================================
    // All Tool Names Use wa. Prefix
    // ========================================================================

    #[test]
    fn all_tool_names_use_wa_prefix() {
        for def in all_definitions() {
            assert!(
                def.name.starts_with("wa."),
                "Tool {} missing wa. prefix",
                def.name
            );
        }
    }

    // ========================================================================
    // All Tools Have Descriptions
    // ========================================================================

    #[test]
    fn all_tools_have_descriptions() {
        for def in all_definitions() {
            assert!(
                def.description.is_some(),
                "Tool {} missing description",
                def.name
            );
            assert!(
                !def.description.as_ref().unwrap().is_empty(),
                "Tool {} has empty description",
                def.name
            );
        }
    }

    // ========================================================================
    // All Input Schemas Are Objects
    // ========================================================================

    #[test]
    fn all_input_schemas_are_objects() {
        for def in all_definitions() {
            let schema_type = def.input_schema.get("type").and_then(|v| v.as_str());
            assert_eq!(
                schema_type,
                Some("object"),
                "Tool {} input_schema type is {:?}, expected 'object'",
                def.name,
                schema_type
            );
        }
    }

    // ========================================================================
    // All Tools Have Version
    // ========================================================================

    #[test]
    fn all_tools_have_version() {
        for def in all_definitions() {
            assert!(def.version.is_some(), "Tool {} missing version", def.name);
        }
    }

    // ========================================================================
    // All Tools Have Tags
    // ========================================================================

    #[test]
    fn all_tools_have_wa_tag() {
        for def in all_definitions() {
            assert!(
                def.tags.contains(&"wa".to_string()),
                "Tool {} missing 'wa' tag",
                def.name
            );
        }
    }

    // ========================================================================
    // Specific Tool Name Stability
    // ========================================================================

    #[test]
    fn core_tool_names_stable() {
        let expected = [
            "wa.state",
            "wa.get_text",
            "wa.send",
            "wa.wait_for",
            "wa.search",
            "wa.events",
            "wa.rules_list",
            "wa.rules_test",
            "wa.reserve",
            "wa.release",
            "wa.reservations",
            "wa.workflow_run",
            "wa.accounts",
        ];
        let names: Vec<String> = all_definitions().iter().map(|d| d.name.clone()).collect();
        for expected_name in &expected {
            assert!(
                names.contains(&expected_name.to_string()),
                "Core tool '{}' not found in definitions",
                expected_name
            );
        }
    }

    #[test]
    fn mission_tool_names_stable() {
        let expected = [
            "wa.mission_state",
            "wa.mission_explain",
            "wa.mission_pause",
            "wa.mission_resume",
            "wa.mission_abort",
        ];
        let names: Vec<String> = all_definitions().iter().map(|d| d.name.clone()).collect();
        for expected_name in &expected {
            assert!(
                names.contains(&expected_name.to_string()),
                "Mission tool '{}' not found in definitions",
                expected_name
            );
        }
    }

    #[test]
    fn tx_tool_names_stable() {
        let expected = ["wa.tx_plan", "wa.tx_show", "wa.tx_run", "wa.tx_rollback"];
        let names: Vec<String> = all_definitions().iter().map(|d| d.name.clone()).collect();
        for expected_name in &expected {
            assert!(
                names.contains(&expected_name.to_string()),
                "Tx tool '{}' not found in definitions",
                expected_name
            );
        }
    }

    #[test]
    fn tx_show_tool_include_contract_returns_embedded_contract() {
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let tool = WaTxShowTool::new(config());

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string(),
                    "include_contract": true
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["tx_id"], "tx:test");
        assert_eq!(envelope["data"]["plan_id"], "plan:test");
        assert_eq!(envelope["data"]["lifecycle_state"], "planned");
        assert_eq!(envelope["data"]["receipt_count"], 0);
        assert_eq!(
            envelope["data"]["contract"]["plan"]["steps"]
                .as_array()
                .expect("steps array")
                .len(),
            3
        );
        assert!(
            !envelope["data"]["legal_transitions"]
                .as_array()
                .expect("transitions array")
                .is_empty()
        );
    }

    #[test]
    fn tx_run_tool_rejects_unknown_fail_step_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let tool = WaTxRunTool::new(config());

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string(),
                    "fail_step": "tx-step:missing"
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert_eq!(envelope["error"], "Unknown fail_step: tx-step:missing");
        assert_eq!(
            envelope["hint"],
            "Use step IDs from wa.tx_show(include_contract=true)."
        );
    }

    #[test]
    fn tx_run_tool_partial_failure_triggers_compensation_and_compensated_state() {
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let tool = WaTxRunTool::new(config());

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string(),
                    "fail_step": "tx-step:2"
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["prepare_report"]["outcome"], "all_ready");
        assert_eq!(
            envelope["data"]["commit_report"]["outcome"],
            "partial_failure"
        );
        assert_eq!(
            envelope["data"]["commit_report"]["failure_boundary"],
            "tx-step:2"
        );
        assert_eq!(envelope["data"]["commit_report"]["committed_count"], 1);
        assert_eq!(envelope["data"]["commit_report"]["failed_count"], 1);
        assert_eq!(
            envelope["data"]["compensation_report"]["outcome"],
            "fully_rolled_back"
        );
        assert_eq!(envelope["data"]["final_state"], "compensated");
    }

    #[test]
    fn tx_rollback_tool_returns_compensated_state_for_synthetic_commit_report() {
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract(&dir, MissionTxState::Committed);
        let tool = WaTxRollbackTool::new(config());

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string()
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["tx_id"], "tx:test");
        assert_eq!(envelope["data"]["final_state"], "compensated");
        assert_eq!(
            envelope["data"]["compensation_report"]["outcome"],
            "fully_rolled_back"
        );
        assert_eq!(
            envelope["data"]["compensation_report"]["compensated_count"],
            3
        );
    }

    #[test]
    fn tx_rollback_tool_rejects_unknown_compensation_step_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract(&dir, MissionTxState::Committed);
        let tool = WaTxRollbackTool::new(config());

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string(),
                    "fail_compensation_for_step": "tx-step:missing"
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert_eq!(
            envelope["error"],
            "Unknown fail_compensation_for_step: tx-step:missing"
        );
        assert_eq!(
            envelope["hint"],
            "Use step IDs from wa.tx_show(include_contract=true)."
        );
    }

    #[test]
    fn cass_tool_names_stable() {
        let expected = ["wa.cass_search", "wa.cass_view", "wa.cass_status"];
        let names: Vec<String> = all_definitions().iter().map(|d| d.name.clone()).collect();
        for expected_name in &expected {
            assert!(
                names.contains(&expected_name.to_string()),
                "Cass tool '{}' not found in definitions",
                expected_name
            );
        }
    }

    #[test]
    fn annotation_tool_names_stable() {
        let expected = ["wa.events_annotate", "wa.events_triage", "wa.events_label"];
        let names: Vec<String> = all_definitions().iter().map(|d| d.name.clone()).collect();
        for expected_name in &expected {
            assert!(
                names.contains(&expected_name.to_string()),
                "Annotation tool '{}' not found in definitions",
                expected_name
            );
        }
    }

    // ========================================================================
    // Key Parameter Schema Checks
    // ========================================================================

    #[test]
    fn state_tool_schema_has_domain_and_pane_id() {
        let def = WaStateTool::new(PaneFilterConfig::default()).definition();
        let props = def.input_schema.get("properties").unwrap();
        assert!(
            props.get("domain").is_some(),
            "wa.state missing 'domain' param"
        );
        assert!(
            props.get("pane_id").is_some(),
            "wa.state missing 'pane_id' param"
        );
    }

    #[test]
    fn get_text_tool_requires_pane_id() {
        let def = WaGetTextTool::new(config(), Some(db_path())).definition();
        let required = def
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("wa.get_text should have required fields");
        let has_pane_id = required.iter().any(|v| v.as_str() == Some("pane_id"));
        assert!(has_pane_id, "wa.get_text should require pane_id");
    }

    #[test]
    fn send_tool_requires_pane_id_and_text() {
        let def = WaSendTool::new(config(), db_path()).definition();
        let required = def
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("wa.send should have required fields");
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"pane_id"), "wa.send should require pane_id");
        assert!(names.contains(&"text"), "wa.send should require text");
    }

    #[test]
    fn search_tool_requires_query() {
        let def = WaSearchTool::new(config(), db_path()).definition();
        let required = def
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("wa.search should have required fields");
        let has_query = required.iter().any(|v| v.as_str() == Some("query"));
        assert!(has_query, "wa.search should require query");
    }

    #[test]
    fn reserve_tool_requires_pane_id() {
        let def = WaReserveTool::new(config(), db_path()).definition();
        let required = def
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("wa.reserve should have required fields");
        let has_pane_id = required.iter().any(|v| v.as_str() == Some("pane_id"));
        assert!(has_pane_id, "wa.reserve should require pane_id");
    }

    // ========================================================================
    // Policy input helpers — send, workflow, reserve, release
    // ========================================================================

    #[test]
    fn mcp_send_text_policy_input_fields() {
        let caps = PaneCapabilities::unknown();
        let input = mcp_send_text_policy_input(5, "local", caps, "send summary", "echo hello");
        assert_eq!(input.action, ActionKind::SendText);
        assert_eq!(input.actor, ActorKind::Mcp);
        assert_eq!(input.surface, PolicySurface::Mux);
        assert_eq!(input.pane_id, Some(5));
        assert_eq!(input.domain.as_deref(), Some("local"));
        assert_eq!(input.text_summary.as_deref(), Some("send summary"));
        assert_eq!(input.command_text.as_deref(), Some("echo hello"));
    }

    #[test]
    fn mcp_workflow_run_policy_input_fields() {
        let caps = PaneCapabilities::unknown();
        let input = mcp_workflow_run_policy_input(9, "SSH:host", caps, "run workflow");
        assert_eq!(input.action, ActionKind::WorkflowRun);
        assert_eq!(input.actor, ActorKind::Mcp);
        assert_eq!(input.surface, PolicySurface::Workflow);
        assert_eq!(input.pane_id, Some(9));
        assert_eq!(input.domain.as_deref(), Some("SSH:host"));
        assert_eq!(input.text_summary.as_deref(), Some("run workflow"));
    }

    #[test]
    fn mcp_reserve_pane_policy_input_fields() {
        let input = mcp_reserve_pane_policy_input(42, "reserve pane 42");
        assert_eq!(input.action, ActionKind::ReservePane);
        assert_eq!(input.actor, ActorKind::Mcp);
        assert_eq!(input.surface, PolicySurface::Swarm);
        assert_eq!(input.pane_id, Some(42));
        assert_eq!(input.command_text.as_deref(), Some("reserve_pane"));
    }

    #[test]
    fn mcp_release_pane_policy_input_with_pane_id() {
        let input = mcp_release_pane_policy_input("release pane 42", Some(42));
        assert_eq!(input.action, ActionKind::ReleasePane);
        assert_eq!(input.actor, ActorKind::Mcp);
        assert_eq!(input.surface, PolicySurface::Swarm);
        assert_eq!(input.pane_id, Some(42));
        assert_eq!(input.command_text.as_deref(), Some("release_reservation"));
    }

    #[test]
    fn mcp_release_pane_policy_input_without_pane_id() {
        let input = mcp_release_pane_policy_input("release all", None);
        assert_eq!(input.action, ActionKind::ReleasePane);
        assert_eq!(input.pane_id, None);
    }

    // ========================================================================
    // Event mutation decision context
    // ========================================================================

    #[test]
    fn mcp_event_mutation_decision_context_fields() {
        let context = mcp_event_mutation_decision_context(
            "wa.events_annotate",
            "events_annotate",
            123,
            "add_note",
            Some("agent-42"),
            "Annotate event 123",
            9999,
        );

        assert_eq!(context.timestamp_ms, 9999);
        assert_eq!(context.action, ActionKind::ExecCommand);
        assert_eq!(context.actor, ActorKind::Mcp);
        assert_eq!(context.surface, PolicySurface::Mcp);

        let evidence: std::collections::HashMap<String, String> = context
            .evidence
            .iter()
            .map(|e| (e.key.clone(), e.value.clone()))
            .collect();

        assert_eq!(
            evidence.get("tool").map(String::as_str),
            Some("wa.events_annotate")
        );
        assert_eq!(
            evidence.get("event_action_kind").map(String::as_str),
            Some("events_annotate")
        );
        assert_eq!(evidence.get("event_id").map(String::as_str), Some("123"));
        assert_eq!(
            evidence.get("operation").map(String::as_str),
            Some("add_note")
        );
        assert_eq!(
            evidence.get("actor_id").map(String::as_str),
            Some("agent-42")
        );
        assert_eq!(
            evidence.get("event_surface").map(String::as_str),
            Some("mcp")
        );
    }

    #[test]
    fn mcp_event_mutation_decision_context_without_actor_id() {
        let context = mcp_event_mutation_decision_context(
            "wa.events_triage",
            "events_triage",
            456,
            "set_state",
            None,
            "Triage event 456",
            1000,
        );

        let evidence: std::collections::HashMap<String, String> = context
            .evidence
            .iter()
            .map(|e| (e.key.clone(), e.value.clone()))
            .collect();

        assert!(
            evidence.get("actor_id").is_none(),
            "actor_id should be absent when None"
        );
        assert_eq!(evidence.get("event_id").map(String::as_str), Some("456"));
    }

    #[test]
    fn serialize_mcp_audit_decision_context_produces_valid_json() {
        let context = mcp_event_mutation_decision_context(
            "wa.events_label",
            "events_label",
            789,
            "add_label",
            Some("test-agent"),
            "Label event 789",
            5000,
        );
        let json = serialize_mcp_audit_decision_context(&context);
        assert!(json.is_some(), "serialization should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json.unwrap()).expect("should be valid JSON");
        assert!(parsed.is_object());
    }
}
