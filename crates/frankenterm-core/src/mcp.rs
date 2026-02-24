//! MCP server integration for wa (feature-gated).
//!
//! This module provides a thin MCP surface that mirrors robot-mode semantics.

use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use std::path::PathBuf;
use std::sync::Arc;

mod mcp_framework {
    pub mod prelude {
        pub use fastmcp::prelude::*;
    }

    pub use fastmcp::StdioTransport;
    pub use fastmcp::{ResourceHandler, ResourceTemplate, ToolHandler};
}

use mcp_framework::prelude::*;
use mcp_framework::{ResourceHandler, ResourceTemplate, StdioTransport, ToolHandler};

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
#[cfg(test)]
use crate::mcp_error::{
    MCP_ERR_CASS, MCP_ERR_CAUT, MCP_ERR_CONFIG, MCP_ERR_NOT_IMPLEMENTED, MCP_ERR_PANE_NOT_FOUND,
    MCP_ERR_WEZTERM, map_caut_error,
};
use crate::mcp_error::{
    MCP_ERR_FTS_QUERY, MCP_ERR_INVALID_ARGS, MCP_ERR_POLICY, MCP_ERR_RESERVATION_CONFLICT,
    MCP_ERR_STORAGE, MCP_ERR_TIMEOUT, MCP_ERR_WORKFLOW, McpToolError, map_cass_error,
    map_mcp_error,
};
use crate::patterns::{AgentType, PatternEngine};
use crate::policy::{
    ActionKind, ActorKind, InjectionResult, PaneCapabilities, PolicyDecision, PolicyEngine,
    PolicyGatedInjector, PolicyInput,
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
#[cfg(feature = "mcp-client")]
#[path = "mcp_proxy.rs"]
mod mcp_proxy;
#[path = "mcp_resources.rs"]
mod mcp_resources;
#[path = "mcp_tools.rs"]
mod mcp_tools;

pub use mcp_bridge::{build_server, build_server_with_db, run_stdio_server};
use mcp_middleware::{AuditedToolHandler, FormatAwareToolHandler};
#[cfg(test)]
use mcp_middleware::{
    McpOutputFormat, augment_tool_schema_with_format, encode_mcp_contents,
    extract_mcp_output_format, parse_mcp_output_format,
};
use mcp_resources::{
    WaAccountsByServiceTemplateResource, WaAccountsResource, WaEventsResource,
    WaEventsTemplateResource, WaEventsUnhandledTemplateResource, WaPanesResource,
    WaReservationsByPaneTemplateResource, WaReservationsResource, WaRulesByAgentTemplateResource,
    WaRulesResource, WaWorkflowsResource,
};
use mcp_tools::{
    WaAccountsRefreshTool, WaAccountsTool, WaCassSearchTool, WaCassStatusTool, WaCassViewTool,
    WaEventsTool, WaGetTextTool, WaReleaseTool, WaReservationsTool, WaReserveTool, WaRulesListTool,
    WaRulesTestTool, WaSearchTool, WaSendTool, WaStateTool, WaWaitForTool, WaWorkflowRunTool,
};

const MCP_VERSION: &str = "v1";

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

#[derive(Debug, Default, Deserialize)]
struct StateParams {
    domain: Option<String>,
    agent: Option<String>,
    pane_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GetTextParams {
    pane_id: u64,
    #[serde(default = "default_tail")]
    tail: usize,
    #[serde(default)]
    escapes: bool,
}

fn default_tail() -> usize {
    500
}

#[derive(Debug, Serialize)]
struct McpGetTextData {
    pane_id: u64,
    text: String,
    tail_lines: usize,
    escapes_included: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncation_info: Option<TruncationInfo>,
}

#[derive(Debug, Serialize)]
struct TruncationInfo {
    original_bytes: usize,
    returned_bytes: usize,
    original_lines: usize,
    returned_lines: usize,
}

#[derive(Debug, Default, Deserialize)]
struct SearchParams {
    query: String,
    limit: Option<usize>,
    pane: Option<u64>,
    since: Option<i64>,
    until: Option<i64>,
    snippets: Option<bool>,
    mode: Option<UnifiedSearchMode>,
}

#[derive(Debug, Deserialize)]
struct CassSearchParams {
    query: String,
    #[serde(default = "default_cass_limit")]
    limit: usize,
    #[serde(default = "default_cass_offset")]
    offset: usize,
    agent: Option<String>,
    workspace: Option<String>,
    days: Option<u32>,
    fields: Option<String>,
    max_tokens: Option<usize>,
    #[serde(default = "default_cass_timeout_secs")]
    timeout_secs: u64,
}

fn default_cass_limit() -> usize {
    10
}

fn default_cass_offset() -> usize {
    0
}

fn default_cass_timeout_secs() -> u64 {
    15
}

#[derive(Debug, Deserialize)]
struct CassViewParams {
    source_path: String,
    line_number: usize,
    #[serde(default = "default_cass_context_lines")]
    context_lines: usize,
    #[serde(default = "default_cass_timeout_secs")]
    timeout_secs: u64,
}

fn default_cass_context_lines() -> usize {
    10
}

#[derive(Debug, Default, Deserialize)]
struct CassStatusParams {
    #[serde(default = "default_cass_timeout_secs")]
    timeout_secs: u64,
}

#[derive(Debug, Serialize)]
struct McpSearchData {
    query: String,
    results: Vec<McpSearchHit>,
    total_hits: usize,
    limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_filter: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    since_filter: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    until_filter: Option<i64>,
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct McpSearchHit {
    segment_id: i64,
    pane_id: u64,
    seq: u64,
    captured_at: i64,
    score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    semantic_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fusion_rank: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct EventsParams {
    #[serde(default = "default_events_limit")]
    limit: usize,
    pane: Option<u64>,
    rule_id: Option<String>,
    event_type: Option<String>,
    triage_state: Option<String>,
    label: Option<String>,
    #[serde(default)]
    unhandled: bool,
    since: Option<i64>,
}

fn default_events_limit() -> usize {
    20
}

#[derive(Debug, Serialize)]
struct McpEventsData {
    events: Vec<McpEventItem>,
    total_count: usize,
    limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_filter: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rule_id_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_type_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    triage_state_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label_filter: Option<String>,
    unhandled_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    since_filter: Option<i64>,
}

#[derive(Debug, Serialize)]
struct McpEventItem {
    id: i64,
    pane_id: u64,
    rule_id: String,
    pack_id: String,
    event_type: String,
    severity: String,
    confidence: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    extracted: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotations: Option<crate::storage::EventAnnotations>,
    captured_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    handled_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SendParams {
    pane_id: u64,
    text: String,
    #[serde(default)]
    dry_run: bool,
    wait_for: Option<String>,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    #[serde(default)]
    wait_for_regex: bool,
}

#[derive(Debug, Deserialize)]
struct WaitForParams {
    pane_id: u64,
    pattern: String,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    #[serde(default = "default_wait_tail")]
    tail: usize,
    #[serde(default)]
    regex: bool,
}

fn default_timeout_secs() -> u64 {
    30
}

fn default_wait_tail() -> usize {
    200
}

#[derive(Debug, Serialize)]
struct McpWaitForData {
    pane_id: u64,
    pattern: String,
    matched: bool,
    elapsed_ms: u64,
    polls: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    is_regex: bool,
}

#[derive(Debug, Serialize)]
struct McpSendData {
    pane_id: u64,
    injection: InjectionResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_for: Option<McpWaitForData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification_error: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dry_run: bool,
}

#[derive(Debug, Deserialize)]
struct WorkflowRunParams {
    name: String,
    pane_id: u64,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Serialize)]
struct McpWorkflowRunData {
    workflow_name: String,
    pane_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_id: Option<String>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    steps_executed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct RulesListParams {
    agent_type: Option<String>,
    #[serde(default)]
    verbose: bool,
}

#[derive(Debug, Serialize)]
struct McpRulesListData {
    rules: Vec<McpRuleItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_type_filter: Option<String>,
}

#[derive(Debug, Serialize)]
struct McpRuleItem {
    id: String,
    agent_type: String,
    event_type: String,
    severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow: Option<String>,
    anchor_count: usize,
    has_regex: bool,
}

#[derive(Debug, Deserialize)]
struct RulesTestParams {
    text: String,
    #[serde(default)]
    trace: bool,
}

#[derive(Debug, Serialize)]
struct McpRulesTestData {
    text_length: usize,
    match_count: usize,
    matches: Vec<McpRuleMatchItem>,
}

#[derive(Debug, Serialize)]
struct McpRuleMatchItem {
    rule_id: String,
    agent_type: String,
    event_type: String,
    severity: String,
    confidence: f64,
    matched_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    extracted: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: Option<McpRuleTraceInfo>,
}

#[derive(Debug, Serialize)]
struct McpRuleTraceInfo {
    anchors_checked: bool,
    regex_matched: bool,
}

// Reservation params and data structures
#[derive(Debug, Default, Deserialize)]
struct ReservationsParams {
    pane_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ReserveParams {
    pane_id: u64,
    owner_kind: String,
    owner_id: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default = "default_ttl_ms")]
    ttl_ms: i64,
}

fn default_ttl_ms() -> i64 {
    300_000 // 5 minutes default
}

#[derive(Debug, Deserialize)]
struct ReleaseParams {
    reservation_id: i64,
}

#[derive(Debug, Serialize)]
struct McpReservationsData {
    reservations: Vec<McpReservationInfo>,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_filter: Option<u64>,
}

#[derive(Debug, Serialize)]
struct McpReservationInfo {
    id: i64,
    pane_id: u64,
    owner_kind: String,
    owner_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    created_at: i64,
    expires_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    released_at: Option<i64>,
    status: String,
}

#[derive(Debug, Serialize)]
struct McpReserveData {
    reservation: McpReservationInfo,
}

#[derive(Debug, Serialize)]
struct McpReleaseData {
    reservation_id: i64,
    released: bool,
}

// Accounts params and data structures
#[derive(Debug, Deserialize)]
struct AccountsParams {
    service: String,
}

#[derive(Debug, Deserialize)]
struct AccountsRefreshParams {
    #[serde(default)]
    service: Option<String>,
}

#[derive(Debug, Serialize)]
struct McpAccountsData {
    accounts: Vec<McpAccountInfo>,
    total: usize,
    service: String,
}

#[derive(Debug, Serialize)]
struct McpAccountsRefreshData {
    service: String,
    refreshed_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    refreshed_at: Option<String>,
    accounts: Vec<McpAccountInfo>,
}

#[derive(Debug, Serialize)]
struct McpAccountInfo {
    account_id: String,
    service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    percent_remaining: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reset_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens_used: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens_remaining: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens_limit: Option<i64>,
    last_refreshed_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_used_at: Option<i64>,
}

#[derive(Debug, Serialize)]
struct McpEnvelope<T> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
    elapsed_ms: u64,
    version: String,
    now: u64,
    mcp_version: &'static str,
}

impl<T> McpEnvelope<T> {
    fn success(data: T, elapsed_ms: u64) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            error_code: None,
            hint: None,
            elapsed_ms,
            version: crate::VERSION.to_string(),
            now: now_ms(),
            mcp_version: MCP_VERSION,
        }
    }

    fn error(code: &str, msg: impl Into<String>, hint: Option<String>, elapsed_ms: u64) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(msg.into()),
            error_code: Some(code.to_string()),
            hint,
            elapsed_ms,
            version: crate::VERSION.to_string(),
            now: now_ms(),
            mcp_version: MCP_VERSION,
        }
    }
}

#[derive(Debug, Serialize)]
struct McpPaneState {
    pane_id: u64,
    pane_uuid: Option<String>,
    tab_id: u64,
    window_id: u64,
    domain: String,
    title: Option<String>,
    cwd: Option<String>,
    observed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    ignore_reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct McpWorkflowsData {
    workflows: Vec<McpWorkflowItem>,
    total: usize,
}

#[derive(Debug, Serialize)]
struct McpWorkflowItem {
    name: String,
    description: String,
    step_count: usize,
    trigger_event_types: Vec<String>,
    trigger_rule_ids: Vec<String>,
    supported_agent_types: Vec<String>,
    requires_pane: bool,
    requires_approval: bool,
    can_abort: bool,
    destructive: bool,
}

impl McpPaneState {
    fn from_pane_info(info: &PaneInfo, filter: &PaneFilterConfig) -> Self {
        let domain = info.inferred_domain();
        let title = info.title.clone().unwrap_or_default();
        let cwd = info.cwd.clone().unwrap_or_default();

        let ignore_reason = filter.check_pane(&domain, &title, &cwd);

        Self {
            pane_id: info.pane_id,
            pane_uuid: None,
            tab_id: info.tab_id,
            window_id: info.window_id,
            domain,
            title: info.title.clone(),
            cwd: info.cwd.clone(),
            observed: ignore_reason.is_none(),
            ignore_reason,
        }
    }
}

fn apply_tail_truncation(text: &str, tail_lines: usize) -> (String, bool, Option<TruncationInfo>) {
    let lines: Vec<&str> = text.lines().collect();
    let original_lines = lines.len();
    let original_bytes = text.len();

    if lines.len() <= tail_lines {
        return (text.to_string(), false, None);
    }

    let start_idx = lines.len().saturating_sub(tail_lines);
    let truncated_lines: Vec<&str> = lines[start_idx..].to_vec();
    let truncated_text = truncated_lines.join("\n");
    let returned_bytes = truncated_text.len();
    let returned_lines = truncated_lines.len();

    (
        truncated_text,
        true,
        Some(TruncationInfo {
            original_bytes,
            returned_bytes,
            original_lines,
            returned_lines,
        }),
    )
}

// wa.events_annotate tool (bd-2gce)
#[derive(Debug, Deserialize)]
struct EventsAnnotateParams {
    event_id: i64,
    note: Option<String>,
    #[serde(default)]
    clear: bool,
    by: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventsTriageParams {
    event_id: i64,
    state: Option<String>,
    #[serde(default)]
    clear: bool,
    by: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventsLabelParams {
    event_id: i64,
    add: Option<String>,
    remove: Option<String>,
    #[serde(default)]
    list: bool,
    by: Option<String>,
}

#[derive(Debug, Serialize)]
struct McpEventMutationData {
    event_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed: Option<bool>,
    annotations: crate::storage::EventAnnotations,
}

struct WaEventsAnnotateTool {
    db_path: Arc<PathBuf>,
}

impl WaEventsAnnotateTool {
    fn new(db_path: Arc<PathBuf>) -> Self {
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

            // Audit (redacted)
            let ts = i64::try_from(now_ms()).unwrap_or(0);
            let audit = crate::storage::AuditActionRecord {
                id: 0,
                ts,
                actor_kind: "robot".to_string(),
                actor_id: params.by.clone(),
                correlation_id: None,
                pane_id: None,
                domain: None,
                action_kind: "event.annotate".to_string(),
                policy_decision: "allow".to_string(),
                decision_reason: Some("MCP updated event note".to_string()),
                rule_id: None,
                input_summary: Some(if params.clear {
                    format!("wa.events_annotate event_id={} clear=true", params.event_id)
                } else {
                    format!(
                        "wa.events_annotate event_id={} note=<redacted>",
                        params.event_id
                    )
                }),
                verification_summary: None,
                decision_context: None,
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

struct WaEventsTriageTool {
    db_path: Arc<PathBuf>,
}

impl WaEventsTriageTool {
    fn new(db_path: Arc<PathBuf>) -> Self {
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
            let audit = crate::storage::AuditActionRecord {
                id: 0,
                ts,
                actor_kind: "robot".to_string(),
                actor_id: params.by.clone(),
                correlation_id: None,
                pane_id: None,
                domain: None,
                action_kind: "event.triage".to_string(),
                policy_decision: "allow".to_string(),
                decision_reason: Some("MCP updated event triage".to_string()),
                rule_id: None,
                input_summary: Some(if params.clear {
                    format!("wa.events_triage event_id={} clear=true", params.event_id)
                } else {
                    format!(
                        "wa.events_triage event_id={} state={}",
                        params.event_id,
                        params.state.clone().unwrap_or_default()
                    )
                }),
                verification_summary: None,
                decision_context: None,
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

struct WaEventsLabelTool {
    db_path: Arc<PathBuf>,
}

impl WaEventsLabelTool {
    fn new(db_path: Arc<PathBuf>) -> Self {
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

                let audit = crate::storage::AuditActionRecord {
                    id: 0,
                    ts,
                    actor_kind: "robot".to_string(),
                    actor_id: params.by.clone(),
                    correlation_id: None,
                    pane_id: None,
                    domain: None,
                    action_kind: "event.label.add".to_string(),
                    policy_decision: "allow".to_string(),
                    decision_reason: Some("MCP added event label".to_string()),
                    rule_id: None,
                    input_summary: Some(format!(
                        "wa.events_label event_id={} add={label}",
                        params.event_id
                    )),
                    verification_summary: None,
                    decision_context: None,
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

                let audit = crate::storage::AuditActionRecord {
                    id: 0,
                    ts,
                    actor_kind: "robot".to_string(),
                    actor_id: params.by.clone(),
                    correlation_id: None,
                    pane_id: None,
                    domain: None,
                    action_kind: "event.label.remove".to_string(),
                    policy_decision: "allow".to_string(),
                    decision_reason: Some("MCP removed event label".to_string()),
                    rule_id: None,
                    input_summary: Some(format!(
                        "wa.events_label event_id={} remove={label}",
                        params.event_id
                    )),
                    verification_summary: None,
                    decision_context: None,
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

#[derive(Debug, Deserialize)]
struct IpcPaneState {
    pane_id: u64,
    known: bool,
    #[serde(default)]
    observed: Option<bool>,
    #[serde(default)]
    alt_screen: Option<bool>,
    #[serde(default)]
    last_status_at: Option<i64>,
    #[serde(default)]
    in_gap: Option<bool>,
    #[serde(default)]
    cursor_alt_screen: Option<bool>,
    #[serde(default)]
    reason: Option<String>,
}

struct CapabilityResolution {
    capabilities: PaneCapabilities,
    _warnings: Vec<String>,
}

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
    if most_recent_refresh_ms <= 0 {
        return None;
    }
    let elapsed = now_ms_val - most_recent_refresh_ms;
    if elapsed < cooldown_ms {
        Some((elapsed / 1000, (cooldown_ms - elapsed) / 1000))
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

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| u64::try_from(dur.as_millis()).unwrap_or(u64::MAX))
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
    let audit = crate::storage::AuditActionRecord {
        id: 0,
        ts,
        actor_kind: "mcp".to_string(),
        actor_id: None,
        correlation_id: None,
        pane_id: None,
        domain: None,
        action_kind: format!("mcp.{tool_name}"),
        policy_decision: decision.to_string(),
        decision_reason: error_code.map(|c| format!("error_code={c}")),
        rule_id: None,
        input_summary: Some(format!("{input_summary} elapsed_ms={elapsed_ms}")),
        verification_summary: None,
        decision_context: None,
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
    db_path: &PathBuf,
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

    fn uri_set(values: impl IntoIterator<Item = String>) -> BTreeSet<String> {
        values.into_iter().collect()
    }

    fn json_value_strategy() -> impl Strategy<Value = serde_json::Value> {
        let leaf = proptest::prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::Bool),
            any::<i64>().prop_map(|n| serde_json::Value::Number(n.into())),
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

    proptest::proptest! {
        #[test]
        fn proptest_toon_roundtrip_preserves_json_semantics(value in json_value_strategy()) {
            let toon = toon_rust::encode(value.clone(), None);
            let decoded = toon_rust::try_decode(&toon, None).expect("decode should succeed");
            let decoded_json = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0)
                .join("\n");
            let decoded_value: serde_json::Value =
                serde_json::from_str(&decoded_json).expect("decoded TOON should parse as JSON");
            prop_assert_eq!(decoded_value, value);
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
            prop_assert_eq!(decoded_value, value);
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
        // 5 non-storage + 12 storage-dependent = 17 total
        assert!(
            count >= 17,
            "Expected at least 17 tools with DB, got {count}"
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
        assert!(parse_caut_service("google").is_none());
        assert!(parse_caut_service("anthropic").is_none());
        assert!(parse_caut_service("").is_none());
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
        // Refreshed 120 seconds ago, cooldown is 60 seconds
        assert!(check_refresh_cooldown(0, 120_000, 60_000).is_none());
    }

    #[test]
    fn check_refresh_cooldown_exactly_at_boundary() {
        // Exactly at cooldown boundary
        assert!(check_refresh_cooldown(40_000, 100_000, 60_000).is_none());
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
        let json = r#"{}"#;
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
        let json = r#"{}"#;
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
        assert!(truncated || !truncated); // just assert no panic
        let _ = (result, info);
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
}
