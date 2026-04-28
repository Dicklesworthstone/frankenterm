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
    WaTxRunTool, WaTxShowTool, WaWaitForTool, WaWorkflowRunTool, WaWorkflowStatusTool,
    WaWorkflowsResource, build_mcp_shared_rate_limiter,
};
use crate::mcp_framework::{
    FrameworkServer as Server, framework_server_builder, run_framework_stdio_server,
};
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
    let shared_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
    let db_path = db_path.map(Arc::new);

    let mut builder = framework_server_builder("wezterm-automata", crate::VERSION)
        .instructions("ft MCP server (robot parity). See docs/mcp-api-spec.md.")
        .on_startup(|| -> std::result::Result<(), std::io::Error> {
            tracing::info!("MCP server starting");
            Ok(())
        })
        .on_shutdown(|| {
            tracing::info!("MCP server shutting down");
        })
        .tool(FormatAwareToolHandler::new(WaStateTool::new(
            filter,
            db_path.clone(),
        )))
        .tool(FormatAwareToolHandler::new(
            WaWaitForTool::new_with_shared_rate_limiter(
                Arc::clone(&config),
                db_path.clone(),
                Arc::clone(&shared_rate_limiter),
            ),
        ))
        .tool(FormatAwareToolHandler::new(WaRulesListTool))
        .tool(FormatAwareToolHandler::new(WaRulesTestTool))
        .tool(FormatAwareToolHandler::new(WaCassSearchTool))
        .tool(FormatAwareToolHandler::new(WaCassViewTool))
        .tool(FormatAwareToolHandler::new(WaCassStatusTool))
        .tool(FormatAwareToolHandler::new(WaTxPlanTool::new(Arc::clone(
            &config,
        ))))
        .tool(FormatAwareToolHandler::new(
            WaTxRunTool::new_with_shared_rate_limiter(
                Arc::clone(&config),
                Arc::clone(&shared_rate_limiter),
            ),
        ))
        .tool(FormatAwareToolHandler::new(
            WaTxRollbackTool::new_with_shared_rate_limiter(
                Arc::clone(&config),
                Arc::clone(&shared_rate_limiter),
            ),
        ))
        .tool(FormatAwareToolHandler::new(WaTxShowTool::new(Arc::clone(
            &config,
        ))))
        .tool(FormatAwareToolHandler::new(WaMissionStateTool::new(
            Arc::clone(&config),
        )))
        .tool(FormatAwareToolHandler::new(WaMissionExplainTool::new(
            Arc::clone(&config),
        )))
        .tool(FormatAwareToolHandler::new(
            WaMissionPauseTool::new_with_shared_rate_limiter(
                Arc::clone(&config),
                Arc::clone(&shared_rate_limiter),
            ),
        ))
        .tool(FormatAwareToolHandler::new(
            WaMissionResumeTool::new_with_shared_rate_limiter(
                Arc::clone(&config),
                Arc::clone(&shared_rate_limiter),
            ),
        ))
        .tool(FormatAwareToolHandler::new(
            WaMissionAbortTool::new_with_shared_rate_limiter(
                Arc::clone(&config),
                Arc::clone(&shared_rate_limiter),
            ),
        ))
        .resource(WaPanesResource::new(
            config.ingest.panes.clone(),
            db_path.clone(),
        ))
        .resource(WaWorkflowsResource::new(Arc::clone(&config)))
        .resource(WaRulesResource)
        .resource(WaRulesByAgentTemplateResource);

    if let Some(ref db_path) = db_path {
        builder = builder
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaGetTextTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Some(Arc::clone(db_path)),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.get_text",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaSearchTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(db_path),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.search",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaEventsTool::new(Arc::clone(db_path)),
                "wa.events",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaEventsAnnotateTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(db_path),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.events_annotate",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaEventsTriageTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(db_path),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.events_triage",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaEventsLabelTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(db_path),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.events_label",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaReservationsTool::new(Arc::clone(db_path)),
                "wa.reservations",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaReserveTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(db_path),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.reserve",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaReleaseTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(db_path),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.release",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaSendTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(db_path),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.send",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaWorkflowRunTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(db_path),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.workflow_run",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaWorkflowStatusTool::new(Arc::clone(db_path)),
                "wa.workflow_status",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaAccountsTool::new(Arc::clone(db_path)),
                "wa.accounts",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaAccountsRefreshTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(db_path),
                    Arc::clone(&shared_rate_limiter),
                ),
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
        builder = builder.tool(FormatAwareToolHandler::new(
            WaGetTextTool::new_with_shared_rate_limiter(
                Arc::clone(&config),
                None,
                Arc::clone(&shared_rate_limiter),
            ),
        ));
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
    run_framework_stdio_server(server)
        .map_err(|err| crate::error::Error::Runtime(format!("MCP stdio server failed: {err}")))
}
