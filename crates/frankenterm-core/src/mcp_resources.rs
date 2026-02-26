//! MCP resource/template handlers extracted from legacy `mcp.rs`.
//!
//! This module is extraction-only and keeps resource behavior/URIs stable.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

mod mcp_resources_framework {
    pub use fastmcp::{
        Content as FrameworkContent, McpContext as FrameworkMcpContext,
        McpError as FrameworkMcpError, McpResult as FrameworkMcpResult,
        Resource as FrameworkResource, ResourceContent as FrameworkResourceContent,
        ResourceHandler as FrameworkResourceHandler, ResourceTemplate as FrameworkResourceTemplate,
        ToolHandler as FrameworkToolHandler,
    };
}

use mcp_resources_framework::{
    FrameworkContent as Content, FrameworkMcpContext as McpContext, FrameworkMcpError as McpError,
    FrameworkMcpResult as McpResult, FrameworkResource as Resource,
    FrameworkResourceContent as ResourceContent, FrameworkResourceHandler as ResourceHandler,
    FrameworkResourceTemplate as ResourceTemplate, FrameworkToolHandler as ToolHandler,
};

use super::mcp_tools::{
    WaAccountsTool, WaEventsTool, WaReservationsTool, WaRulesListTool, WaStateTool,
};
use super::{McpEnvelope, McpWorkflowItem, McpWorkflowsData, builtin_workflows, elapsed_ms};
use crate::config::{Config, PaneFilterConfig};

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

pub(super) struct WaPanesResource {
    filter: PaneFilterConfig,
}

impl WaPanesResource {
    pub(super) fn new(filter: PaneFilterConfig) -> Self {
        Self { filter }
    }
}

impl ResourceHandler for WaPanesResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://panes".to_string(),
            name: "ft panes".to_string(),
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

pub(super) struct WaEventsResource {
    db_path: Arc<PathBuf>,
}

impl WaEventsResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaEventsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://events".to_string(),
            name: "ft events".to_string(),
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

pub(super) struct WaEventsTemplateResource {
    db_path: Arc<PathBuf>,
}

impl WaEventsTemplateResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaEventsTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://events/template".to_string(),
            name: "ft events template".to_string(),
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
            name: "ft events (paged)".to_string(),
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

pub(super) struct WaEventsUnhandledTemplateResource {
    db_path: Arc<PathBuf>,
}

impl WaEventsUnhandledTemplateResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaEventsUnhandledTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://events/unhandled/template".to_string(),
            name: "ft events unhandled template".to_string(),
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
            name: "ft events (unhandled)".to_string(),
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

pub(super) struct WaAccountsResource {
    db_path: Arc<PathBuf>,
}

impl WaAccountsResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaAccountsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://accounts".to_string(),
            name: "ft accounts".to_string(),
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

pub(super) struct WaAccountsByServiceTemplateResource {
    db_path: Arc<PathBuf>,
}

impl WaAccountsByServiceTemplateResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaAccountsByServiceTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://accounts/template".to_string(),
            name: "ft accounts template".to_string(),
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
            name: "ft accounts by service".to_string(),
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

pub(super) struct WaRulesResource;

impl ResourceHandler for WaRulesResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://rules".to_string(),
            name: "ft rules".to_string(),
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

pub(super) struct WaRulesByAgentTemplateResource;

impl ResourceHandler for WaRulesByAgentTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://rules/template".to_string(),
            name: "ft rules template".to_string(),
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
            name: "ft rules by agent".to_string(),
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

pub(super) struct WaWorkflowsResource {
    config: Arc<Config>,
}

impl WaWorkflowsResource {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ResourceHandler for WaWorkflowsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://workflows".to_string(),
            name: "ft workflows".to_string(),
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

pub(super) struct WaReservationsResource {
    db_path: Arc<PathBuf>,
}

impl WaReservationsResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaReservationsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://reservations".to_string(),
            name: "ft reservations".to_string(),
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

pub(super) struct WaReservationsByPaneTemplateResource {
    db_path: Arc<PathBuf>,
}

impl WaReservationsByPaneTemplateResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaReservationsByPaneTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://reservations/template".to_string(),
            name: "ft reservations template".to_string(),
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
            name: "ft reservations by pane".to_string(),
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
