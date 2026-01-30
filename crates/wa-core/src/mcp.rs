//! MCP server integration for wa (feature-gated).
//!
//! This module provides a thin MCP surface that mirrors robot-mode semantics.

use std::time::Instant;

use fastmcp::ToolHandler;
use fastmcp::prelude::*;
use serde::{Deserialize, Serialize};

use std::path::PathBuf;
use std::sync::Arc;

use crate::Result;
use crate::config::{Config, PaneFilterConfig};
use crate::error::{Error, StorageError, WeztermError};
use crate::patterns::{AgentType, PatternEngine};
use crate::storage::{EventQuery, PaneReservation, SearchOptions, StorageHandle};
use crate::wezterm::{PaneInfo, WeztermClient};

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
    captured_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    handled_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow_id: Option<String>,
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

#[derive(Debug, Serialize)]
struct McpAccountsData {
    accounts: Vec<McpAccountInfo>,
    total: usize,
    service: String,
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
        .tool(WaRulesListTool)
        .tool(WaRulesTestTool);

    if let Some(ref db_path) = db_path {
        builder = builder
            .tool(WaSearchTool::new(Arc::clone(db_path)))
            .tool(WaEventsTool::new(Arc::clone(db_path)))
            .tool(WaReservationsTool::new(Arc::clone(db_path)))
            .tool(WaReserveTool::new(Arc::clone(db_path)))
            .tool(WaReleaseTool::new(Arc::clone(db_path)))
            .tool(WaAccountsTool::new(Arc::clone(db_path)));
    }

    let server = builder.build();

    Ok(server)
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

        if params.agent.is_some() {
            tracing::info!(
                "MCP wa.state agent filter is not yet implemented; returning unfiltered results"
            );
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result = runtime.block_on(async {
            let wezterm = WeztermClient::new();
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
            let wezterm = WeztermClient::new();
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
                        Some("Expected object with optional limit, pane, rule_id, event_type, unhandled, since".to_string()),
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

            let query = EventQuery {
                limit: Some(params.limit),
                pane_id: params.pane,
                rule_id: params.rule_id.clone(),
                event_type: params.event_type.clone(),
                unhandled_only: params.unhandled,
                since: params.since,
                until: None,
            };

            storage.get_events(query).await
        });

        match result {
            Ok(events) => {
                let total_count = events.len();
                let items: Vec<McpEventItem> = events
                    .into_iter()
                    .map(|e| {
                        // Derive pack_id from rule_id (e.g., "codex.usage.reached" -> "builtin:codex")
                        let pack_id = e.rule_id.split('.').next().map_or_else(
                            || "builtin:unknown".to_string(),
                            |agent| format!("builtin:{agent}"),
                        );
                        McpEventItem {
                            id: e.id,
                            pane_id: e.pane_id,
                            rule_id: e.rule_id,
                            pack_id,
                            event_type: e.event_type,
                            severity: e.severity,
                            confidence: e.confidence,
                            extracted: e.extracted,
                            captured_at: e.detected_at,
                            handled_at: e.handled_at,
                            workflow_id: e.handled_by_workflow_id,
                        }
                    })
                    .collect();

                let data = McpEventsData {
                    events: items,
                    total_count,
                    limit: params.limit,
                    pane_filter: params.pane,
                    rule_id_filter: params.rule_id,
                    event_type_filter: params.event_type,
                    unhandled_only: params.unhandled,
                    since_filter: params.since,
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
    db_path: Arc<PathBuf>,
}

impl WaReserveTool {
    fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
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
                    Some("Expected object with pane_id, owner_kind, owner_id (required), reason, ttl_ms".to_string()),
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
            storage
                .create_reservation(
                    params.pane_id,
                    &params.owner_kind,
                    &params.owner_id,
                    params.reason.as_deref(),
                    params.ttl_ms,
                )
                .await
        });

        match result {
            Ok(reservation) => {
                let data = McpReserveData {
                    reservation: reservation_to_mcp_info(&reservation),
                };
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                // Check if this is a conflict error
                let (code, hint) = if err.to_string().contains("already has active reservation") {
                    (
                        MCP_ERR_RESERVATION_CONFLICT,
                        Some("Use wa.reservations to check existing reservations".to_string()),
                    )
                } else {
                    map_mcp_error(&err)
                };
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

// wa.release tool - release a pane reservation
struct WaReleaseTool {
    db_path: Arc<PathBuf>,
}

impl WaReleaseTool {
    fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
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

        let db_path = Arc::clone(&self.db_path);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;
            storage.release_reservation(params.reservation_id).await
        });

        match result {
            Ok(released) => {
                let data = McpReleaseData {
                    reservation_id: params.reservation_id,
                    released,
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
