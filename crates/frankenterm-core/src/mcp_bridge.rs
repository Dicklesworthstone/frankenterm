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
use std::sync::atomic::{AtomicU64, Ordering};

/// br-ft-647cj: number of server tool + resource registrations
/// that get skipped when `build_server_with_db` is called with
/// `db_path=None`. The else-branch at line ~250 only registers
/// `WaGetTextTool` (1 tool) instead of the full storage-backed
/// surface (14 AuditedToolHandler tools + 7 Resource registrations =
/// 21 entries). The counter delta on a single degraded-mode
/// build is therefore `21 - 1 = 20` entries.
const MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES: u64 = 20;

/// br-ft-647cj: cumulative count of tool + resource registrations
/// skipped across all `build_server_with_db(_, None)` calls in
/// this process. Bumps by [`MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES`]
/// on every degraded-mode startup.
///
/// Operators reading `mcp_bridge_tools_skipped_no_db_count()`
/// can detect that the running MCP server is in degraded mode
/// (db_path=None) without scraping `tracing::warn` output.
/// `0` means every build call had a `db_path` and the full
/// surface is registered; non-zero is the count of skipped
/// entries (in multiples of `MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES`).
///
/// Same defect family as ft-luav8 (MCP audit-failure counter)
/// and ft-8na0z (mcp_proxy tool-mount-failure counter): make
/// degraded-startup observable instead of implicit.
static MCP_BRIDGE_TOOLS_SKIPPED_NO_DB: AtomicU64 = AtomicU64::new(0);

/// Cumulative count of MCP tool + resource registrations skipped
/// because `build_server_with_db` was called with `db_path=None`.
/// See [`MCP_BRIDGE_TOOLS_SKIPPED_NO_DB`].
#[must_use]
pub fn mcp_bridge_tools_skipped_no_db_count() -> u64 {
    MCP_BRIDGE_TOOLS_SKIPPED_NO_DB.load(Ordering::Relaxed)
}

/// Test helper: reset the counter so tests that exercise the
/// degraded-mode path can assert the post-increment value
/// without state leakage from sibling tests.
#[cfg(test)]
pub(crate) fn reset_mcp_bridge_tools_skipped_no_db_count_for_test() {
    MCP_BRIDGE_TOOLS_SKIPPED_NO_DB.store(0, Ordering::Relaxed);
}

fn record_mcp_bridge_degraded_startup() {
    MCP_BRIDGE_TOOLS_SKIPPED_NO_DB
        .fetch_add(MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES, Ordering::Relaxed);
}

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
        // br-ft-647cj: db_path=None is the degraded-mode startup
        // path. Only WaGetTextTool registers; the 14
        // AuditedToolHandler tools + 7 storage-backed resource
        // registrations from the if-branch above are silently
        // skipped (see MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES).
        // Bump the cumulative counter and emit a structured warn
        // (NOT info) at startup explicitly listing the absent
        // tool surface so operators see the gap without scraping.
        record_mcp_bridge_degraded_startup();
        tracing::warn!(
            skipped_entries = MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES,
            skipped_tools = "wa.get_text*, wa.search, wa.events, wa.events_annotate, \
                             wa.events_triage, wa.events_label, wa.reservations, \
                             wa.reserve, wa.release, wa.send, wa.workflow_run, \
                             wa.workflow_status, wa.accounts, wa.accounts_refresh",
            skipped_resources = "WaEvents*, WaAccounts*, WaReservations* (7 templates)",
            "br-ft-647cj: MCP server starting in degraded mode (db_path=None) \
             — storage-backed tool surface absent; only WaGetTextTool registered. \
             Configure a database path to enable the full tool catalog."
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// br-ft-647cj: build_server_with_db(_, None) bumps the
    /// degraded-mode counter by exactly
    /// MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES (the constant
    /// reflects the 14 AuditedToolHandler tools + 7 Resource
    /// registrations that the else-branch skips relative to the
    /// db_path=Some path, minus the 1 tool the else-branch DOES
    /// register).
    #[test]
    fn build_server_with_no_db_bumps_degraded_counter() {
        reset_mcp_bridge_tools_skipped_no_db_count_for_test();
        let before = mcp_bridge_tools_skipped_no_db_count();
        let config = Config::default();
        let _server = build_server_with_db(&config, None).expect("build server with no db");
        let after = mcp_bridge_tools_skipped_no_db_count();
        assert_eq!(
            after - before,
            MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES,
            "br-ft-647cj: degraded-mode startup must bump counter by {}",
            MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES
        );
    }

    /// br-ft-647cj: build_server_with_db(_, Some(path)) does NOT
    /// bump the degraded-mode counter — full surface is registered.
    #[test]
    fn build_server_with_db_does_not_bump_degraded_counter() {
        reset_mcp_bridge_tools_skipped_no_db_count_for_test();
        let before = mcp_bridge_tools_skipped_no_db_count();
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("ft-647cj-test.db");
        let config = Config::default();
        let _server = build_server_with_db(&config, Some(db_path))
            .expect("build server with db");
        let after = mcp_bridge_tools_skipped_no_db_count();
        assert_eq!(
            after, before,
            "br-ft-647cj: full-surface startup must NOT bump degraded counter"
        );
    }
}
