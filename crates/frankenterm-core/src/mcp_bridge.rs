//! MCP server bridge/wiring for the legacy MCP module.
//!
//! This stays as a thin extraction-only layer to reduce `mcp.rs` size while
//! preserving behavior and registration order.

use super::{
    AuditedToolHandler, Config, FormatAwareToolHandler, Result,
    WaAccountsByServiceTemplateResource, WaAccountsRefreshTool, WaAccountsResource, WaAccountsTool,
    WaCassSearchTool, WaCassStatusTool, WaCassViewTool, WaEventsAnnotateTool, WaEventsLabelTool,
    WaEventsResource, WaEventsTemplateResource, WaEventsTool, WaEventsTriageTool,
    WaEventsUnhandledTemplateResource, WaGetTextTool, WaMissionAbortTool, WaMissionExplainTool,
    WaMissionPauseTool, WaMissionResumeTool, WaMissionStateTool, WaPanesResource, WaReleaseTool,
    WaReservationsByPaneTemplateResource, WaReservationsResource, WaReservationsTool,
    WaReserveTool, WaRulesByAgentTemplateResource, WaRulesListTool, WaRulesResource,
    WaRulesTestTool, WaSearchTool, WaSendTool, WaStateTool, WaTxPlanTool, WaTxRollbackTool,
    WaTxRunTool, WaTxShowTool, WaWaitForTool, WaWorkflowRunTool, WaWorkflowsResource,
};
use crate::mcp_framework::{FrameworkServer as Server, FrameworkStdioTransport as StdioTransport};
use std::path::PathBuf;
use std::sync::Arc;

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
        .instructions("ft MCP server (robot parity). See docs/mcp-api-spec.md.")
        .on_startup(|| -> std::result::Result<(), std::io::Error> {
            tracing::info!("MCP server starting");
            Ok(())
        })
        .on_shutdown(|| {
            tracing::info!("MCP server shutting down");
        })
        .tool(FormatAwareToolHandler::new(WaStateTool::new(filter)))
        .tool(FormatAwareToolHandler::new(WaWaitForTool))
        .tool(FormatAwareToolHandler::new(WaRulesListTool))
        .tool(FormatAwareToolHandler::new(WaRulesTestTool))
        .tool(FormatAwareToolHandler::new(WaCassSearchTool))
        .tool(FormatAwareToolHandler::new(WaCassViewTool))
        .tool(FormatAwareToolHandler::new(WaCassStatusTool))
        .tool(FormatAwareToolHandler::new(WaTxPlanTool::new(Arc::clone(
            &config,
        ))))
        .tool(FormatAwareToolHandler::new(WaTxRunTool::new(Arc::clone(
            &config,
        ))))
        .tool(FormatAwareToolHandler::new(WaTxRollbackTool::new(
            Arc::clone(&config),
        )))
        .tool(FormatAwareToolHandler::new(WaTxShowTool::new(Arc::clone(
            &config,
        ))))
        .tool(FormatAwareToolHandler::new(WaMissionStateTool::new(
            Arc::clone(&config),
        )))
        .tool(FormatAwareToolHandler::new(WaMissionExplainTool::new(
            Arc::clone(&config),
        )))
        .tool(FormatAwareToolHandler::new(WaMissionPauseTool::new(
            Arc::clone(&config),
        )))
        .tool(FormatAwareToolHandler::new(WaMissionResumeTool::new(
            Arc::clone(&config),
        )))
        .tool(FormatAwareToolHandler::new(WaMissionAbortTool::new(
            Arc::clone(&config),
        )))
        .resource(WaPanesResource::new(config.ingest.panes.clone()))
        .resource(WaWorkflowsResource::new(Arc::clone(&config)))
        .resource(WaRulesResource)
        .resource(WaRulesByAgentTemplateResource);

    if let Some(ref db_path) = db_path {
        builder = builder
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaGetTextTool::new(Arc::clone(&config), Some(Arc::clone(db_path))),
                "wa.get_text",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaSearchTool::new(Arc::clone(&config), Arc::clone(db_path)),
                "wa.search",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaEventsTool::new(Arc::clone(db_path)),
                "wa.events",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaEventsAnnotateTool::new(Arc::clone(db_path)),
                "wa.events_annotate",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaEventsTriageTool::new(Arc::clone(db_path)),
                "wa.events_triage",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaEventsLabelTool::new(Arc::clone(db_path)),
                "wa.events_label",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaReservationsTool::new(Arc::clone(db_path)),
                "wa.reservations",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaReserveTool::new(Arc::clone(&config), Arc::clone(db_path)),
                "wa.reserve",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaReleaseTool::new(Arc::clone(&config), Arc::clone(db_path)),
                "wa.release",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaSendTool::new(Arc::clone(&config), Arc::clone(db_path)),
                "wa.send",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaWorkflowRunTool::new(Arc::clone(&config), Arc::clone(db_path)),
                "wa.workflow_run",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaAccountsTool::new(Arc::clone(db_path)),
                "wa.accounts",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaAccountsRefreshTool::new(Arc::clone(&config), Arc::clone(db_path)),
                "wa.accounts_refresh",
                Arc::clone(db_path),
            )))
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
    } else {
        builder = builder.tool(FormatAwareToolHandler::new(WaGetTextTool::new(
            Arc::clone(&config),
            None,
        )));
    }

    #[cfg(feature = "mcp-client")]
    {
        builder = super::mcp_proxy::compose_proxy_tools(builder, config.as_ref(), db_path.clone())?;
    }

    let server = builder.build();

    Ok(server)
}

/// Build and run the MCP server over stdio transport.
///
/// This keeps transport details inside `frankenterm-core` so callers don't
/// need a direct `fastmcp` dependency.
pub fn run_stdio_server(config: &Config, db_path: Option<PathBuf>) -> Result<()> {
    let server = build_server_with_db(config, db_path)?;
    let transport = StdioTransport::stdio();
    server.run_transport(transport)
}
