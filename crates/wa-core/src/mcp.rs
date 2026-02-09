//! MCP server integration for wa (feature-gated).
//!
//! This module provides a thin MCP surface that mirrors robot-mode semantics.

use std::collections::HashMap;
use std::time::Instant;

use fastmcp::prelude::*;
use fastmcp::{ResourceHandler, ResourceTemplate, ToolHandler};
use serde::{Deserialize, Serialize};

use std::path::PathBuf;
use std::sync::Arc;

use crate::Result;
use crate::accounts::AccountRecord;
use crate::approval::ApprovalStore;
use crate::caut::{CautClient, CautError, CautService};
use crate::config::{Config, PaneFilterConfig};
use crate::error::{Error, StorageError, WeztermError};
use crate::ingest::Osc133State;
use crate::patterns::{AgentType, PatternEngine};
use crate::policy::{
    ActionKind, ActorKind, InjectionResult, PaneCapabilities, PolicyDecision, PolicyEngine,
    PolicyGatedInjector, PolicyInput,
};
use crate::storage::{EventQuery, PaneReservation, SearchOptions, StorageHandle};
use crate::wezterm::{
    PaneInfo, PaneWaiter, WaitMatcher, WaitOptions, WaitResult, WeztermHandleSource,
    default_wezterm_handle,
};
use crate::workflows::{
    HandleAuthRequired, HandleClaudeCodeLimits, HandleCompaction, HandleGeminiQuota,
    HandleSessionEnd, HandleUsageLimits, PaneWorkflowLockManager, Workflow, WorkflowEngine,
    WorkflowExecutionResult, WorkflowRunner, WorkflowRunnerConfig,
};

const MCP_VERSION: &str = "v1";

const MCP_ERR_INVALID_ARGS: &str = "WA-MCP-0001";
const MCP_ERR_CONFIG: &str = "WA-MCP-0003";
const MCP_ERR_WEZTERM: &str = "WA-MCP-0004";
const MCP_ERR_STORAGE: &str = "WA-MCP-0005";
const MCP_ERR_POLICY: &str = "WA-MCP-0006";
const MCP_ERR_PANE_NOT_FOUND: &str = "WA-MCP-0007";
const MCP_ERR_WORKFLOW: &str = "WA-MCP-0008";
const MCP_ERR_TIMEOUT: &str = "WA-MCP-0009";
const MCP_ERR_NOT_IMPLEMENTED: &str = "WA-MCP-0010";
const MCP_ERR_FTS_QUERY: &str = "WA-MCP-0011";
const MCP_ERR_RESERVATION_CONFLICT: &str = "WA-MCP-0012";
const MCP_ERR_CAUT: &str = "WA-MCP-0013";

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
    #[serde(default = "default_search_limit")]
    limit: usize,
    pane: Option<u64>,
    since: Option<i64>,
    #[serde(default = "default_snippets")]
    snippets: bool,
}

fn default_search_limit() -> usize {
    20
}

fn default_snippets() -> bool {
    true
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

/// Build the MCP server with tools that have robot parity.
pub fn build_server(config: &Config) -> Result<Server> {
    build_server_with_db(config, None)
}

/// Build the MCP server with explicit db_path for tools that need storage access.
pub fn build_server_with_db(config: &Config, db_path: Option<PathBuf>) -> Result<Server> {
    let filter = config.ingest.panes.clone();
    let config = Arc::new(config.clone());
    let db_path = db_path.map(Arc::new);

    let mut builder = Server::new("wezterm-automata", crate::VERSION)
        .instructions("wa MCP server (robot parity). See docs/mcp-api-spec.md.")
        .on_startup(|| -> std::result::Result<(), std::io::Error> {
            tracing::info!("MCP server starting");
            Ok(())
        })
        .on_shutdown(|| {
            tracing::info!("MCP server shutting down");
        })
        .tool(WaStateTool::new(filter))
        .tool(WaGetTextTool)
        .tool(WaWaitForTool)
        .tool(WaRulesListTool)
        .tool(WaRulesTestTool)
        .resource(WaPanesResource::new(config.ingest.panes.clone()))
        .resource(WaWorkflowsResource::new(Arc::clone(&config)))
        .resource(WaRulesResource)
        .resource(WaRulesByAgentTemplateResource);

    if let Some(ref db_path) = db_path {
        builder = builder
            .tool(AuditedToolHandler::new(
                WaSearchTool::new(Arc::clone(db_path)),
                "wa.search",
                Arc::clone(db_path),
            ))
            .tool(AuditedToolHandler::new(
                WaEventsTool::new(Arc::clone(db_path)),
                "wa.events",
                Arc::clone(db_path),
            ))
            .tool(AuditedToolHandler::new(
                WaEventsAnnotateTool::new(Arc::clone(db_path)),
                "wa.events_annotate",
                Arc::clone(db_path),
            ))
            .tool(AuditedToolHandler::new(
                WaEventsTriageTool::new(Arc::clone(db_path)),
                "wa.events_triage",
                Arc::clone(db_path),
            ))
            .tool(AuditedToolHandler::new(
                WaEventsLabelTool::new(Arc::clone(db_path)),
                "wa.events_label",
                Arc::clone(db_path),
            ))
            .tool(AuditedToolHandler::new(
                WaReservationsTool::new(Arc::clone(db_path)),
                "wa.reservations",
                Arc::clone(db_path),
            ))
            .tool(AuditedToolHandler::new(
                WaReserveTool::new(Arc::clone(&config), Arc::clone(db_path)),
                "wa.reserve",
                Arc::clone(db_path),
            ))
            .tool(AuditedToolHandler::new(
                WaReleaseTool::new(Arc::clone(&config), Arc::clone(db_path)),
                "wa.release",
                Arc::clone(db_path),
            ))
            .tool(AuditedToolHandler::new(
                WaSendTool::new(Arc::clone(&config), Arc::clone(db_path)),
                "wa.send",
                Arc::clone(db_path),
            ))
            .tool(AuditedToolHandler::new(
                WaWorkflowRunTool::new(Arc::clone(&config), Arc::clone(db_path)),
                "wa.workflow_run",
                Arc::clone(db_path),
            ))
            .tool(AuditedToolHandler::new(
                WaAccountsTool::new(Arc::clone(db_path)),
                "wa.accounts",
                Arc::clone(db_path),
            ))
            .tool(AuditedToolHandler::new(
                WaAccountsRefreshTool::new(Arc::clone(&config), Arc::clone(db_path)),
                "wa.accounts_refresh",
                Arc::clone(db_path),
            ))
            .resource(WaEventsResource::new(Arc::clone(db_path)))
            .resource(WaEventsTemplateResource::new(Arc::clone(db_path)))
            .resource(WaEventsUnhandledTemplateResource::new(Arc::clone(db_path)))
            .resource(WaAccountsResource::new(Arc::clone(db_path)))
            .resource(WaAccountsByServiceTemplateResource::new(Arc::clone(
                db_path,
            )))
            .resource(WaReservationsResource::new(Arc::clone(db_path)))
            .resource(WaReservationsByPaneTemplateResource::new(Arc::clone(
                db_path,
            )));
    }

    let server = builder.build();

    Ok(server)
}

fn tool_output_as_resource(uri: &str, contents: Vec<Content>) -> McpResult<Vec<ResourceContent>> {
    let text = contents
        .into_iter()
        .find_map(|content| match content {
            Content::Text { text } => Some(text),
            _ => None,
        })
        .ok_or_else(|| McpError::internal_error("Tool output missing text payload"))?;

    Ok(vec![ResourceContent {
        uri: uri.to_string(),
        mime_type: Some("application/json".to_string()),
        text: Some(text),
        blob: None,
    }])
}

fn envelope_as_resource<T: Serialize>(
    uri: &str,
    envelope: McpEnvelope<T>,
) -> McpResult<Vec<ResourceContent>> {
    let text = serde_json::to_string(&envelope)
        .map_err(|e| McpError::internal_error(format!("Serialize resource payload: {e}")))?;
    Ok(vec![ResourceContent {
        uri: uri.to_string(),
        mime_type: Some("application/json".to_string()),
        text: Some(text),
        blob: None,
    }])
}

fn read_events_resource(
    ctx: &McpContext,
    db_path: &Arc<PathBuf>,
    uri: &str,
    limit: usize,
    unhandled: bool,
) -> McpResult<Vec<ResourceContent>> {
    let tool = WaEventsTool::new(Arc::clone(db_path));
    let contents = tool.call(
        ctx,
        serde_json::json!({
            "limit": limit.clamp(1, 1000),
            "unhandled": unhandled,
        }),
    )?;
    tool_output_as_resource(uri, contents)
}

fn read_accounts_resource(
    ctx: &McpContext,
    db_path: &Arc<PathBuf>,
    uri: &str,
    service: &str,
) -> McpResult<Vec<ResourceContent>> {
    let tool = WaAccountsTool::new(Arc::clone(db_path));
    let contents = tool.call(ctx, serde_json::json!({ "service": service }))?;
    tool_output_as_resource(uri, contents)
}

fn read_rules_resource(
    ctx: &McpContext,
    uri: &str,
    agent_type: Option<&str>,
) -> McpResult<Vec<ResourceContent>> {
    let args = if let Some(agent_type) = agent_type {
        serde_json::json!({ "verbose": true, "agent_type": agent_type })
    } else {
        serde_json::json!({ "verbose": true })
    };
    let tool = WaRulesListTool;
    let contents = tool.call(ctx, args)?;
    tool_output_as_resource(uri, contents)
}

fn read_reservations_resource(
    ctx: &McpContext,
    db_path: &Arc<PathBuf>,
    uri: &str,
    pane_id: Option<u64>,
) -> McpResult<Vec<ResourceContent>> {
    let tool = WaReservationsTool::new(Arc::clone(db_path));
    let args = if let Some(pane_id) = pane_id {
        serde_json::json!({ "pane_id": pane_id })
    } else {
        serde_json::Value::Null
    };
    let contents = tool.call(ctx, args)?;
    tool_output_as_resource(uri, contents)
}

struct WaPanesResource {
    filter: PaneFilterConfig,
}

impl WaPanesResource {
    fn new(filter: PaneFilterConfig) -> Self {
        Self { filter }
    }
}

impl ResourceHandler for WaPanesResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://panes".to_string(),
            name: "wa panes".to_string(),
            description: Some("Pane snapshot (same data surface as wa.state)".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "panes".to_string()],
        }
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        let tool = WaStateTool::new(self.filter.clone());
        let contents = tool.call(ctx, serde_json::Value::Null)?;
        tool_output_as_resource("wa://panes", contents)
    }
}

struct WaEventsResource {
    db_path: Arc<PathBuf>,
}

impl WaEventsResource {
    fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaEventsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://events".to_string(),
            name: "wa events".to_string(),
            description: Some("Recent detection events (default limit 50)".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "events".to_string()],
        }
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_events_resource(ctx, &self.db_path, "wa://events", 50, false)
    }
}

struct WaEventsTemplateResource {
    db_path: Arc<PathBuf>,
}

impl WaEventsTemplateResource {
    fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaEventsTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://events/template".to_string(),
            name: "wa events template".to_string(),
            description: Some("Template for page-sized events resource".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "events".to_string()],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "wa://events/{limit}".to_string(),
            name: "wa events (paged)".to_string(),
            description: Some("Override page size for events resource".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "events".to_string()],
        })
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_events_resource(ctx, &self.db_path, "wa://events", 50, false)
    }

    fn read_with_uri(
        &self,
        ctx: &McpContext,
        uri: &str,
        params: &HashMap<String, String>,
    ) -> McpResult<Vec<ResourceContent>> {
        let limit = params
            .get("limit")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(50)
            .clamp(1, 1000);
        read_events_resource(ctx, &self.db_path, uri, limit, false)
    }
}

struct WaEventsUnhandledTemplateResource {
    db_path: Arc<PathBuf>,
}

impl WaEventsUnhandledTemplateResource {
    fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaEventsUnhandledTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://events/unhandled/template".to_string(),
            name: "wa events unhandled template".to_string(),
            description: Some("Template for unhandled events resource".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "events".to_string()],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "wa://events/unhandled/{limit}".to_string(),
            name: "wa events (unhandled)".to_string(),
            description: Some("Read only unhandled events with configurable limit".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "events".to_string(),
                "unhandled".to_string(),
            ],
        })
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_events_resource(ctx, &self.db_path, "wa://events/unhandled/50", 50, true)
    }

    fn read_with_uri(
        &self,
        ctx: &McpContext,
        uri: &str,
        params: &HashMap<String, String>,
    ) -> McpResult<Vec<ResourceContent>> {
        let limit = params
            .get("limit")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(50)
            .clamp(1, 1000);
        read_events_resource(ctx, &self.db_path, uri, limit, true)
    }
}

struct WaAccountsResource {
    db_path: Arc<PathBuf>,
}

impl WaAccountsResource {
    fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaAccountsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://accounts".to_string(),
            name: "wa accounts".to_string(),
            description: Some("Account usage snapshot (default service openai)".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "accounts".to_string()],
        }
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_accounts_resource(ctx, &self.db_path, "wa://accounts", "openai")
    }
}

struct WaAccountsByServiceTemplateResource {
    db_path: Arc<PathBuf>,
}

impl WaAccountsByServiceTemplateResource {
    fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaAccountsByServiceTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://accounts/template".to_string(),
            name: "wa accounts template".to_string(),
            description: Some("Template for service-specific account snapshots".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "accounts".to_string()],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "wa://accounts/{service}".to_string(),
            name: "wa accounts by service".to_string(),
            description: Some("Read account snapshot for a specific service".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "accounts".to_string()],
        })
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_accounts_resource(ctx, &self.db_path, "wa://accounts/openai", "openai")
    }

    fn read_with_uri(
        &self,
        ctx: &McpContext,
        uri: &str,
        params: &HashMap<String, String>,
    ) -> McpResult<Vec<ResourceContent>> {
        let service = params
            .get("service")
            .cloned()
            .unwrap_or_else(|| "openai".to_string());
        read_accounts_resource(ctx, &self.db_path, uri, &service)
    }
}

struct WaRulesResource;

impl ResourceHandler for WaRulesResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://rules".to_string(),
            name: "wa rules".to_string(),
            description: Some(
                "Rule catalog (same data surface as wa.rules_list with verbose output)".to_string(),
            ),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "rules".to_string()],
        }
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_rules_resource(ctx, "wa://rules", None)
    }
}

struct WaRulesByAgentTemplateResource;

impl ResourceHandler for WaRulesByAgentTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://rules/template".to_string(),
            name: "wa rules template".to_string(),
            description: Some("Template for rules filtered by agent type".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "rules".to_string()],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "wa://rules/{agent_type}".to_string(),
            name: "wa rules by agent".to_string(),
            description: Some("Filter rule catalog by agent type".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "rules".to_string()],
        })
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_rules_resource(ctx, "wa://rules", None)
    }

    fn read_with_uri(
        &self,
        ctx: &McpContext,
        uri: &str,
        params: &HashMap<String, String>,
    ) -> McpResult<Vec<ResourceContent>> {
        read_rules_resource(ctx, uri, params.get("agent_type").map(String::as_str))
    }
}

struct WaWorkflowsResource {
    config: Arc<Config>,
}

impl WaWorkflowsResource {
    fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ResourceHandler for WaWorkflowsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://workflows".to_string(),
            name: "wa workflows".to_string(),
            description: Some("Builtin workflow catalog and metadata".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "workflows".to_string()],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        let start = Instant::now();
        let workflows: Vec<McpWorkflowItem> = builtin_workflows(&self.config)
            .iter()
            .map(|workflow| McpWorkflowItem {
                name: workflow.name().to_string(),
                description: workflow.description().to_string(),
                step_count: workflow.step_count(),
                trigger_event_types: workflow
                    .trigger_event_types()
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                trigger_rule_ids: workflow
                    .trigger_rule_ids()
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                supported_agent_types: workflow
                    .supported_agent_types()
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                requires_pane: workflow.requires_pane(),
                requires_approval: workflow.requires_approval(),
                can_abort: workflow.can_abort(),
                destructive: workflow.is_destructive(),
            })
            .collect();

        let data = McpWorkflowsData {
            total: workflows.len(),
            workflows,
        };
        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_as_resource("wa://workflows", envelope)
    }
}

struct WaReservationsResource {
    db_path: Arc<PathBuf>,
}

impl WaReservationsResource {
    fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaReservationsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://reservations".to_string(),
            name: "wa reservations".to_string(),
            description: Some(
                "Active pane reservations (same data surface as wa.reservations)".to_string(),
            ),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "reservations".to_string()],
        }
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_reservations_resource(ctx, &self.db_path, "wa://reservations", None)
    }
}

struct WaReservationsByPaneTemplateResource {
    db_path: Arc<PathBuf>,
}

impl WaReservationsByPaneTemplateResource {
    fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaReservationsByPaneTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://reservations/template".to_string(),
            name: "wa reservations template".to_string(),
            description: Some("Template for pane-filtered reservations".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "reservations".to_string()],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "wa://reservations/{pane_id}".to_string(),
            name: "wa reservations by pane".to_string(),
            description: Some("Filter reservations by pane id".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "reservations".to_string()],
        })
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_reservations_resource(ctx, &self.db_path, "wa://reservations", None)
    }

    fn read_with_uri(
        &self,
        ctx: &McpContext,
        uri: &str,
        params: &HashMap<String, String>,
    ) -> McpResult<Vec<ResourceContent>> {
        let pane_id = params
            .get("pane_id")
            .ok_or_else(|| McpError::invalid_params("Missing pane_id in resource URI"))?
            .parse::<u64>()
            .map_err(|_| McpError::invalid_params("pane_id must be an unsigned integer"))?;
        read_reservations_resource(ctx, &self.db_path, uri, Some(pane_id))
    }
}

struct WaStateTool {
    filter: PaneFilterConfig,
}

impl WaStateTool {
    fn new(filter: PaneFilterConfig) -> Self {
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

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
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

// wa.get_text tool
struct WaGetTextTool;

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

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result = runtime.block_on(async {
            let wezterm = default_wezterm_handle();
            wezterm.get_text(params.pane_id, params.escapes).await
        });

        match result {
            Ok(full_text) => {
                let (text, truncated, truncation_info) =
                    apply_tail_truncation(&full_text, params.tail);

                let data = McpGetTextData {
                    pane_id: params.pane_id,
                    text,
                    tail_lines: params.tail,
                    escapes_included: params.escapes,
                    truncated,
                    truncation_info,
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

// wa.wait_for tool
struct WaWaitForTool;

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

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
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

// wa.send tool
struct WaSendTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
}

impl WaSendTool {
    fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
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

            let mut input = PolicyInput::new(ActionKind::SendText, ActorKind::Mcp)
                .with_pane(params.pane_id)
                .with_domain(domain)
                .with_capabilities(capabilities.clone())
                .with_text_summary(summary.clone())
                .with_command_text(&params.text);

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

// wa.search tool
struct WaSearchTool {
    db_path: Arc<PathBuf>,
}

impl WaSearchTool {
    fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ToolHandler for WaSearchTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.search".to_string(),
            description: Some(
                "Full-text search across captured pane output (robot parity)".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "FTS5 search query" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 20, "description": "Maximum results" },
                    "pane": { "type": "integer", "minimum": 0, "description": "Filter by pane ID" },
                    "since": { "type": "integer", "description": "Filter by time (epoch ms)" },
                    "snippets": { "type": "boolean", "default": true, "description": "Include snippets in results" }
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
                        "Expected object with query (required), limit, pane, since, snippets"
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let db_path = Arc::clone(&self.db_path);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;

            let options = SearchOptions {
                limit: Some(params.limit),
                pane_id: params.pane,
                since: params.since,
                until: None,
                include_snippets: Some(params.snippets),
                snippet_max_tokens: Some(30),
                highlight_prefix: Some(">>".to_string()),
                highlight_suffix: Some("<<".to_string()),
            };

            storage.search_with_results(&params.query, options).await
        });

        match result {
            Ok(results) => {
                let total_hits = results.len();
                let hits: Vec<McpSearchHit> = results
                    .into_iter()
                    .map(|r| McpSearchHit {
                        segment_id: r.segment.id,
                        pane_id: r.segment.pane_id,
                        seq: r.segment.seq,
                        captured_at: r.segment.captured_at,
                        score: r.score,
                        snippet: r.snippet,
                        content: if params.snippets {
                            None
                        } else {
                            Some(r.segment.content)
                        },
                    })
                    .collect();

                let data = McpSearchData {
                    query: params.query,
                    results: hits,
                    total_hits,
                    limit: params.limit,
                    pane_filter: params.pane,
                    since_filter: params.since,
                };
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = match &err {
                    Error::Storage(StorageError::FtsQueryError(_)) => (
                        MCP_ERR_FTS_QUERY,
                        Some("Check FTS5 query syntax. Supported: words, \"phrases\", prefix*, AND/OR/NOT".to_string()),
                    ),
                    _ => map_mcp_error(&err),
                };
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

// wa.events tool
struct WaEventsTool {
    db_path: Arc<PathBuf>,
}

impl WaEventsTool {
    fn new(db_path: Arc<PathBuf>) -> Self {
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
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
                // Derive pack_id from rule_id (e.g., "codex.usage.reached" -> "builtin:codex")
                let pack_id = e.rule_id.split('.').next().map_or_else(
                    || "builtin:unknown".to_string(),
                    |agent| format!("builtin:{agent}"),
                );

                let annotations = match storage.get_event_annotations(e.id).await {
                    Ok(Some(a)) => Some(a),
                    Ok(None) => None,
                    Err(err) => {
                        tracing::warn!(error = %err, event_id = e.id, "Failed to load event annotations");
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
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

// wa.workflow_run tool
struct WaWorkflowRunTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
}

impl WaWorkflowRunTool {
    fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
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

                let mut input = PolicyInput::new(ActionKind::WorkflowRun, ActorKind::Mcp)
                    .with_pane(params.pane_id)
                    .with_domain(domain)
                    .with_capabilities(capabilities.clone())
                    .with_text_summary(summary.clone());

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
                let injector =
                    Arc::new(tokio::sync::Mutex::new(PolicyGatedInjector::with_storage(
                        injector_engine,
                        Arc::clone(&wezterm),
                        storage.as_ref().clone(),
                    )));
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
                        "Ensure workflows are enabled or run wa watch for event-driven workflows."
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

// wa.rules_list tool
struct WaRulesListTool;

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

        // Parse agent_type filter if provided
        let agent_filter: Option<AgentType> =
            params
                .agent_type
                .as_ref()
                .and_then(|s| match s.to_lowercase().as_str() {
                    "codex" => Some(AgentType::Codex),
                    "claude_code" => Some(AgentType::ClaudeCode),
                    "gemini" => Some(AgentType::Gemini),
                    "wezterm" => Some(AgentType::Wezterm),
                    _ => None,
                });

        // Create pattern engine to get rules
        let engine = PatternEngine::new();
        let rules = engine.rules();

        // Filter and transform rules
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
struct WaRulesTestTool;

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

        // Create pattern engine and run detection
        let engine = PatternEngine::new();
        let detections = engine.detect(&params.text);

        // Convert detections to MCP format
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

// wa.reservations tool - list active pane reservations
struct WaReservationsTool {
    db_path: Arc<PathBuf>,
}

impl WaReservationsTool {
    fn new(db_path: Arc<PathBuf>) -> Self {
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
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

// wa.reserve tool - create a pane reservation
struct WaReserveTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
}

impl WaReserveTool {
    fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<McpReserveData, McpToolError> =
            runtime.block_on(async move {
                let storage = StorageHandle::new(&db_path.to_string_lossy())
                    .await
                    .map_err(McpToolError::from_error)?;

                let mut engine = build_policy_engine(&config, config.safety.require_prompt_active);
                let mut input = PolicyInput::new(ActionKind::ReservePane, ActorKind::Mcp)
                    .with_pane(params.pane_id)
                    .with_capabilities(PaneCapabilities::unknown())
                    .with_text_summary(format!("reserve pane {}", params.pane_id));
                input = input.with_command_text("reserve_pane");

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
                // Check if this is a conflict error
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

// wa.release tool - release a pane reservation
struct WaReleaseTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
}

impl WaReleaseTool {
    fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
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
                let mut input = PolicyInput::new(ActionKind::ReleasePane, ActorKind::Mcp)
                    .with_capabilities(PaneCapabilities::unknown())
                    .with_text_summary(format!("release reservation {}", params.reservation_id));
                if let Some(pane_id) = pane_id {
                    input = input.with_pane(pane_id);
                }
                input = input.with_command_text("release_reservation");

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

// wa.accounts tool - list accounts by service
struct WaAccountsTool {
    db_path: Arc<PathBuf>,
}

impl WaAccountsTool {
    fn new(db_path: Arc<PathBuf>) -> Self {
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
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

// wa.accounts_refresh tool - refresh account usage via caut
struct WaAccountsRefreshTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
}

impl WaAccountsRefreshTool {
    fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        Self { config, db_path }
    }
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<McpAccountsRefreshData, McpToolError> =
            runtime.block_on(async move {
            let service = params
                .service
                .unwrap_or_else(|| "openai".to_string());
            let caut_service = parse_caut_service(&service).ok_or_else(|| {
                McpToolError::new(
                    MCP_ERR_INVALID_ARGS,
                    format!("Unknown service: {service}"),
                    Some("Supported services: openai".to_string()),
                )
            })?;

            let storage = StorageHandle::new(&db_path.to_string_lossy())
                .await
                .map_err(McpToolError::from_error)?;

            let mut engine = build_policy_engine(&config, false);
            let summary = format!("caut refresh {service}");
            let input = PolicyInput::new(ActionKind::ExecCommand, ActorKind::Mcp)
                .with_text_summary(summary.clone())
                .with_command_text(summary.clone());
            let decision = engine.authorize(&input);
            if decision.is_denied() {
                let reason = policy_reason(&decision)
                    .unwrap_or("Refresh denied by policy")
                    .to_string();
                return Err(McpToolError::new(MCP_ERR_POLICY, reason, None));
            }
            if decision.requires_approval() {
                let workspace_id = resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
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
                let most_recent = accounts
                    .iter()
                    .map(|a| a.last_refreshed_at)
                    .max()
                    .unwrap_or(0);
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

const SEND_OSC_SEGMENT_LIMIT: usize = 200;
const MCP_REFRESH_COOLDOWN_MS: i64 = 30_000;

struct McpToolError {
    code: &'static str,
    message: String,
    hint: Option<String>,
}

impl McpToolError {
    fn new(code: &'static str, message: String, hint: Option<String>) -> Self {
        Self {
            code,
            message,
            hint,
        }
    }

    fn from_error(err: Error) -> Self {
        let (code, hint) = map_mcp_error(&err);
        Self {
            code,
            message: err.to_string(),
            hint,
        }
    }

    fn from_caut_error(err: CautError) -> Self {
        let (code, hint) = map_caut_error(&err);
        Self {
            code,
            message: err.to_string(),
            hint,
        }
    }
}

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
    match service {
        "openai" => Some(CautService::OpenAI),
        _ => None,
    }
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
    ]
}

fn map_caut_error(error: &CautError) -> (&'static str, Option<String>) {
    match error {
        CautError::NotInstalled => (
            MCP_ERR_CONFIG,
            Some("Install caut and ensure it is on PATH.".to_string()),
        ),
        CautError::Timeout { .. } => (
            MCP_ERR_TIMEOUT,
            Some("Retry the refresh or increase caut timeout.".to_string()),
        ),
        _ => (MCP_ERR_CAUT, Some(error.remediation().summary.to_string())),
    }
}

fn map_mcp_error(error: &Error) -> (&'static str, Option<String>) {
    match error {
        Error::Wezterm(WeztermError::PaneNotFound(_)) => (
            MCP_ERR_PANE_NOT_FOUND,
            Some("Use wa.state to list available panes.".to_string()),
        ),
        Error::Wezterm(WeztermError::Timeout(_)) => (
            MCP_ERR_TIMEOUT,
            Some("Increase timeout or ensure WezTerm is responsive.".to_string()),
        ),
        Error::Wezterm(WeztermError::NotRunning) => {
            (MCP_ERR_WEZTERM, Some("Is WezTerm running?".to_string()))
        }
        Error::Wezterm(WeztermError::CliNotFound) => (
            MCP_ERR_WEZTERM,
            Some("Install WezTerm and ensure it is in PATH.".to_string()),
        ),
        Error::Wezterm(_) => (MCP_ERR_WEZTERM, None),
        Error::Config(_) => (MCP_ERR_CONFIG, None),
        Error::Storage(_) => (MCP_ERR_STORAGE, None),
        Error::Workflow(_) => (MCP_ERR_WORKFLOW, None),
        Error::Policy(_) => (MCP_ERR_POLICY, None),
        _ => (MCP_ERR_NOT_IMPLEMENTED, None),
    }
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

// ── MCP audit recording (wa-nu4.3.1.6) ──────────────────────────────────

/// Wrapper that records an audit entry for every tool call.
///
/// Wraps any `ToolHandler` and intercepts `call()` to record:
/// - tool name and redacted argument keys
/// - success/failure outcome
/// - error code (if any)
/// - elapsed time
struct AuditedToolHandler<T: ToolHandler> {
    inner: T,
    tool_name: String,
    db_path: Arc<PathBuf>,
}

impl<T: ToolHandler> AuditedToolHandler<T> {
    fn new(inner: T, tool_name: impl Into<String>, db_path: Arc<PathBuf>) -> Self {
        Self {
            inner,
            tool_name: tool_name.into(),
            db_path,
        }
    }
}

impl<T: ToolHandler> ToolHandler for AuditedToolHandler<T> {
    fn definition(&self) -> Tool {
        self.inner.definition()
    }

    fn call(&self, ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let raw_args = arguments.clone();
        let result = self.inner.call(ctx, arguments);

        // Extract ok/error_code from the envelope in the result
        let (ok, error_code) = match &result {
            Ok(contents) => {
                let parsed = contents.first().and_then(|c| match c {
                    Content::Text { text } => serde_json::from_str::<serde_json::Value>(text).ok(),
                    _ => None,
                });
                let is_ok = parsed
                    .as_ref()
                    .and_then(|v| v.get("ok")?.as_bool())
                    .unwrap_or(true);
                let code = if !is_ok {
                    parsed.and_then(|v| v.get("error_code")?.as_str().map(String::from))
                } else {
                    None
                };
                (is_ok, code)
            }
            Err(_) => (false, Some("MCP_INTERNAL".to_string())),
        };

        record_mcp_audit_sync(
            &self.db_path,
            &self.tool_name,
            &raw_args,
            ok,
            error_code.as_deref(),
            elapsed_ms(start),
        );

        result
    }
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
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
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
    use std::collections::BTreeSet;

    fn uri_set(values: impl IntoIterator<Item = String>) -> BTreeSet<String> {
        values.into_iter().collect()
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
        ];
        for code in &codes {
            assert!(
                code.starts_with("WA-MCP-"),
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
        ];
        for code in &codes {
            let suffix = &code["WA-MCP-".len()..];
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
        assert_eq!(json["error_code"], "WA-MCP-0005");
        assert_eq!(json["hint"], "Try again");
        assert_eq!(json["elapsed_ms"], 99);
        assert!(json["version"].is_string());
    }

    #[test]
    fn envelope_error_without_hint() {
        let envelope = McpEnvelope::<()>::error(MCP_ERR_TIMEOUT, "timeout", None, 5000);
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error_code"], "WA-MCP-0009");
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
}
