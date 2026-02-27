//! Extracted MCP tool handlers (strangler-fig migration slice).

#[allow(clippy::wildcard_imports)]
use super::*;

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
                let mut input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Mcp)
                    .with_pane(params.pane_id)
                    .with_domain(domain)
                    .with_capabilities(capabilities)
                    .with_text_summary(summary.clone());
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
                let mut input = PolicyInput::new(ActionKind::SearchOutput, ActorKind::Mcp)
                    .with_text_summary(summary.clone());

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
            if matches!(commit.outcome, crate::plan::TxCommitOutcome::PartialFailure) {
                let mut compensating_contract = prepared_contract.clone();
                compensating_contract.lifecycle_state = crate::plan::MissionTxState::Compensating;
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
            let found = mission.assignments.iter().find(|a| a.assignment_id.0 == *aid);
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
        let decision = match mission.pause_mission(
            &params.requested_by,
            &reason,
            requested_at_ms,
            None,
        ) {
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
        let decision = match mission.resume_mission(
            &params.requested_by,
            "mcp_resume",
            requested_at_ms,
            None,
        ) {
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
