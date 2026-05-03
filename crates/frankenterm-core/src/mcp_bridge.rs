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
/// surface (14 base AuditedToolHandler tools + 5 newly-gated
/// mutating tools + 7 Resource registrations = 26 entries).
/// The counter delta on a single degraded-mode build is therefore
/// `26 - 1 = 25` entries.
///
/// br-ft-p4y8d: bumped from 20 to 25 because 5 mutation-capable
/// tools (wa.tx_run, wa.tx_rollback, wa.mission_pause/resume/abort)
/// were moved out of the unconditional base catalog into the
/// db-gated AuditedToolHandler branch — pre-fix they were
/// advertised in degraded mode without any audit wrapper, which
/// violated the degraded-mode contract created by br-ft-647cj.
const MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES: u64 = 25;

/// br-ft-p4y8d: tool names registered unconditionally in the MCP
/// server (both degraded and full modes). These are read-only
/// diagnostics or use no storage at all — safe to expose without
/// the AuditedToolHandler audit wrapper. This list is the
/// authoritative degraded-mode tool catalog; the regression test
/// `degraded_server_does_not_expose_mutating_tools_ft_p4y8d` pins
/// that NO mutating tool name leaks into the degraded build.
pub(crate) const DEGRADED_MODE_BASE_TOOL_NAMES: &[&str] = &[
    "wa.state",
    "wa.wait_for",
    "wa.rules_list",
    "wa.rules_test",
    "wa.cass_search",
    "wa.cass_view",
    "wa.cass_status",
    "wa.tx_plan",
    "wa.tx_show",
    "wa.mission_state",
    "wa.mission_explain",
    "wa.get_text",
];

/// br-ft-p4y8d: mutation-capable tools that REQUIRE storage +
/// AuditedToolHandler. Pre-fix these were registered unconditionally
/// in the base catalog with NO audit wrapper — a degraded (no-db)
/// bridge would advertise/execute them while the audit trail was
/// unavailable, violating the br-ft-647cj degraded-mode expectation.
/// Now gated to the `db_path = Some(_)` branch and wrapped in
/// AuditedToolHandler so every mutation call is audit-recorded.
pub(crate) const DB_GATED_MUTATING_TOOL_NAMES: &[&str] = &[
    "wa.tx_run",
    "wa.tx_rollback",
    "wa.mission_pause",
    "wa.mission_resume",
    "wa.mission_abort",
];

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

/// Build the MCP server in degraded (no-database) mode.
///
/// br-ft-647cj: this is the EXPLICIT opt-in for the silent
/// degradation path that previously triggered when callers
/// passed `db_path=None` to [`build_server_with_db`]. Operators
/// who genuinely want a database-less server must now call this
/// function by name; the API can no longer be hit by accident.
///
/// Same surface as [`build_server_with_db`] but with a stripped
/// tool catalog (only `WaGetTextTool` registers; the 14
/// AuditedToolHandler tools + 7 storage-backed Resource
/// registrations are skipped). Bumps
/// [`MCP_BRIDGE_TOOLS_SKIPPED_NO_DB`] by
/// [`MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES`] and emits a
/// structured `tracing::warn!` listing the absent tool surface.
pub fn build_server_degraded(config: &Config) -> Result<Server> {
    build_server_inner(config, None)
}

/// Build the MCP server with tools that have robot parity.
///
/// Defaults to the **degraded (no-db) mode** — historically the
/// only path `build_server` exercised. Callers wanting the full
/// surface should use [`build_server_with_db`] with an explicit
/// `Some(path)`.
pub fn build_server(config: &Config) -> Result<Server> {
    build_server_degraded(config)
}

/// Build the MCP server with an explicit `db_path` for tools
/// that need storage access.
///
/// br-ft-647cj: passing `None` here used to silently degrade
/// the tool catalog with no telemetry. After this fix, `None`
/// is an **explicit error**: callers must either supply a real
/// path OR call [`build_server_degraded`] by name to acknowledge
/// the missing-storage shape. The silent-strip path no longer
/// exists in the public API.
pub fn build_server_with_db(config: &Config, db_path: Option<PathBuf>) -> Result<Server> {
    if db_path.is_none() {
        return Err(crate::error::Error::Runtime(format!(
            "br-ft-647cj: build_server_with_db called with db_path=None, \
             which used to silently strip {MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES} \
             tool + resource registrations from the server surface. The \
             silent-degradation path was removed; if a database-less \
             server is genuinely required, call build_server_degraded(config) \
             by name to opt in to the stripped catalog (which still bumps \
             MCP_BRIDGE_TOOLS_SKIPPED_NO_DB and warn-logs the absent tools)."
        )));
    }
    build_server_inner(config, db_path)
}

/// Internal implementation shared by [`build_server_with_db`]
/// and [`build_server_degraded`].
///
/// Public API enforces the rule that `db_path=None` is only
/// reachable via the explicit `build_server_degraded` entry —
/// this private helper does NOT enforce that invariant
/// (callers above are responsible for gating).
fn build_server_inner(config: &Config, db_path: Option<PathBuf>) -> Result<Server> {
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
        // br-ft-p4y8d: WaTxRunTool, WaTxRollbackTool, WaMissionPauseTool,
        // WaMissionResumeTool, WaMissionAbortTool moved to the
        // `if let Some(ref db_path)` branch below and wrapped in
        // AuditedToolHandler. Pre-fix they were registered here
        // unconditionally with no audit wrapper, so a degraded
        // (no-db) bridge would advertise mutation controls while
        // the audit stream was unavailable — violating the
        // br-ft-647cj degraded-mode contract. See
        // `DB_GATED_MUTATING_TOOL_NAMES` above.
        .tool(FormatAwareToolHandler::new(WaTxShowTool::new(Arc::clone(
            &config,
        ))))
        .tool(FormatAwareToolHandler::new(WaMissionStateTool::new(
            Arc::clone(&config),
        )))
        .tool(FormatAwareToolHandler::new(WaMissionExplainTool::new(
            Arc::clone(&config),
        )))
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
            // br-ft-p4y8d: mutation-capable mission/tx tools — moved
            // here from the unconditional base catalog so they're
            // (a) only registered when storage is available and
            // (b) wrapped in AuditedToolHandler so every call lands
            // in the audit stream. The tools themselves don't read
            // `db_path` (their constructors take Config + rate
            // limiter), but AuditedToolHandler needs it to write
            // the audit records. See DB_GATED_MUTATING_TOOL_NAMES.
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaTxRunTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.tx_run",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaTxRollbackTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.tx_rollback",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaMissionPauseTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.mission_pause",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaMissionResumeTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.mission_resume",
                Arc::clone(db_path),
            )))
            .tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                WaMissionAbortTool::new_with_shared_rate_limiter(
                    Arc::clone(&config),
                    Arc::clone(&shared_rate_limiter),
                ),
                "wa.mission_abort",
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
                             wa.workflow_status, wa.accounts, wa.accounts_refresh, \
                             wa.tx_run, wa.tx_rollback, wa.mission_pause, \
                             wa.mission_resume, wa.mission_abort",
            skipped_resources = "WaEvents*, WaAccounts*, WaReservations* (7 templates)",
            "br-ft-647cj + br-ft-p4y8d: MCP server starting in degraded mode \
             (db_path=None) — storage-backed tool surface absent AND mutation- \
             capable mission/tx tools withheld (no audit stream available). \
             Only the read-only/no-storage base catalog is registered. \
             Configure a database path to enable the full audited catalog."
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
    // br-ft-647cj: route db_path=None through the explicit
    // degraded-mode entry rather than the now-erroring
    // build_server_with_db(_, None) path. Operators invoking
    // `ft mcp` without --db get the same legacy behavior; the
    // build_server_with_db surface stays strict.
    let server = match db_path {
        Some(path) => build_server_with_db(config, Some(path))?,
        None => build_server_degraded(config)?,
    };
    run_framework_stdio_server(server)
        .map_err(|err| crate::error::Error::Runtime(format!("MCP stdio server failed: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// br-ft-647cj: passing `db_path=None` to `build_server_with_db`
    /// is now an explicit error. The silent-degradation path was
    /// removed; callers must use `build_server_degraded` to opt
    /// in by name.
    #[test]
    fn build_server_with_db_rejects_none_db_path() {
        let config = Config::default();
        let Err(err) = build_server_with_db(&config, None) else {
            panic!("None db_path must produce explicit error after ft-647cj");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("br-ft-647cj"),
            "error must reference the bead: {msg}"
        );
        assert!(
            msg.contains("build_server_degraded"),
            "error must point operators at the explicit-degraded entry: {msg}"
        );
    }

    /// br-ft-647cj: explicit `build_server_degraded` opt-in
    /// produces a server AND bumps the cumulative counter by the
    /// constant.
    #[test]
    fn build_server_degraded_bumps_counter() {
        reset_mcp_bridge_tools_skipped_no_db_count_for_test();
        let before = mcp_bridge_tools_skipped_no_db_count();
        let config = Config::default();
        let _server = build_server_degraded(&config).expect("explicit degraded build must succeed");
        let after = mcp_bridge_tools_skipped_no_db_count();
        assert_eq!(
            after - before,
            MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES,
            "br-ft-647cj: explicit degraded startup must bump counter by {}",
            MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES
        );
    }

    /// br-ft-647cj: `build_server_with_db(_, Some(path))` does
    /// NOT bump the degraded counter — full surface is registered.
    #[test]
    fn build_server_with_db_does_not_bump_degraded_counter() {
        reset_mcp_bridge_tools_skipped_no_db_count_for_test();
        let before = mcp_bridge_tools_skipped_no_db_count();
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("ft-647cj-test.db");
        let config = Config::default();
        let _server = build_server_with_db(&config, Some(db_path)).expect("build server with db");
        let after = mcp_bridge_tools_skipped_no_db_count();
        assert_eq!(
            after, before,
            "br-ft-647cj: full-surface startup must NOT bump degraded counter"
        );
    }

    /// br-ft-647cj: legacy `build_server(config)` continues to
    /// route through the explicit-degraded path so existing
    /// callers don't see the new Err variant. Behavior preserved.
    #[test]
    fn build_server_routes_through_explicit_degraded() {
        reset_mcp_bridge_tools_skipped_no_db_count_for_test();
        let before = mcp_bridge_tools_skipped_no_db_count();
        let config = Config::default();
        let _server = build_server(&config).expect("legacy build_server preserved");
        let after = mcp_bridge_tools_skipped_no_db_count();
        assert_eq!(
            after - before,
            MCP_BRIDGE_DEGRADED_MODE_SKIPPED_ENTRIES,
            "legacy build_server must continue to bump degraded counter"
        );
    }

    // ── br-ft-p4y8d: degraded-mode tool catalog regressions ────────────

    /// br-ft-p4y8d: NO mutation-capable mission/tx tool may appear
    /// in the degraded (no-db) MCP server catalog. Pre-fix
    /// WaTxRunTool, WaTxRollbackTool, WaMissionPauseTool,
    /// WaMissionResumeTool, and WaMissionAbortTool were registered
    /// unconditionally with no AuditedToolHandler wrapper, so a
    /// db-less bridge advertised mutation controls while the audit
    /// stream was unavailable. This test introspects the actual
    /// built Server via `Server::tools()` and asserts the contract.
    #[test]
    fn degraded_server_does_not_expose_mutating_tools_ft_p4y8d() {
        let config = Config::default();
        let server = build_server_degraded(&config).expect("degraded build must succeed");
        let tool_names: Vec<String> = server.tools().into_iter().map(|t| t.name).collect();

        for mutating in DB_GATED_MUTATING_TOOL_NAMES {
            assert!(
                !tool_names.iter().any(|n| n == mutating),
                "ft-p4y8d: degraded server must NOT expose mutating tool `{mutating}`; \
                 found in catalog: {tool_names:?}"
            );
        }
    }

    /// br-ft-p4y8d: degraded server exposes EXACTLY the documented
    /// safe base catalog (DEGRADED_MODE_BASE_TOOL_NAMES). Pins the
    /// authoritative list against accidental additions.
    #[test]
    fn degraded_server_exposes_exactly_base_catalog_ft_p4y8d() {
        let config = Config::default();
        let server = build_server_degraded(&config).expect("degraded build must succeed");
        let tool_names: Vec<String> = server.tools().into_iter().map(|t| t.name).collect();

        // Every documented base tool must be present.
        for expected in DEGRADED_MODE_BASE_TOOL_NAMES {
            assert!(
                tool_names.iter().any(|n| n == expected),
                "ft-p4y8d: degraded server must expose `{expected}`; \
                 found in catalog: {tool_names:?}"
            );
        }
        // No tool name beyond the documented base catalog should
        // appear (mcp-client proxy tools may be present under their
        // own feature gate, so we only assert this when that
        // feature is off).
        #[cfg(not(feature = "mcp-client"))]
        {
            assert_eq!(
                tool_names.len(),
                DEGRADED_MODE_BASE_TOOL_NAMES.len(),
                "ft-p4y8d: degraded catalog size must match documented base \
                 (got {}, expected {}): {tool_names:?}",
                tool_names.len(),
                DEGRADED_MODE_BASE_TOOL_NAMES.len()
            );
        }
    }

    /// br-ft-p4y8d: full-mode server (with db_path) exposes the
    /// base catalog PLUS the mutating tools. Pins the round-trip:
    /// what's withheld in degraded mode IS exposed when storage
    /// is configured.
    #[test]
    fn full_server_exposes_mutating_tools_when_db_path_set_ft_p4y8d() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("ft-p4y8d-test.db");
        let config = Config::default();
        let server =
            build_server_with_db(&config, Some(db_path)).expect("full build with db must succeed");
        let tool_names: Vec<String> = server.tools().into_iter().map(|t| t.name).collect();

        for mutating in DB_GATED_MUTATING_TOOL_NAMES {
            assert!(
                tool_names.iter().any(|n| n == mutating),
                "ft-p4y8d: full server (with db_path) must expose mutating tool \
                 `{mutating}`; found in catalog: {tool_names:?}"
            );
        }
    }

    /// br-ft-p4y8d: contract-level invariant — the degraded base
    /// catalog and the db-gated mutating list must be disjoint.
    /// This pins the data-driven contract independently of any
    /// runtime build, catching const-list edits that would
    /// re-introduce the bypass.
    #[test]
    fn degraded_and_mutating_catalogs_are_disjoint_ft_p4y8d() {
        for mutating in DB_GATED_MUTATING_TOOL_NAMES {
            assert!(
                !DEGRADED_MODE_BASE_TOOL_NAMES.contains(mutating),
                "ft-p4y8d: mutating tool `{mutating}` must not appear in \
                 DEGRADED_MODE_BASE_TOOL_NAMES (would re-introduce the bypass)"
            );
        }
    }

    /// br-ft-p4y8d: property-style sweep — for every tool name the
    /// degraded server actually exposes, that name MUST be in the
    /// documented DEGRADED_MODE_BASE_TOOL_NAMES list. Catches
    /// silent additions that drift the runtime away from the
    /// documented contract.
    #[test]
    fn every_degraded_tool_appears_in_documented_catalog_ft_p4y8d() {
        let config = Config::default();
        let server = build_server_degraded(&config).expect("degraded build must succeed");

        for tool in server.tools() {
            // mcp-client proxy may register tools under its own
            // feature gate; skip names that don't match `wa.*`.
            if !tool.name.starts_with("wa.") {
                continue;
            }
            assert!(
                DEGRADED_MODE_BASE_TOOL_NAMES.contains(&tool.name.as_str()),
                "ft-p4y8d: tool `{}` exposed in degraded mode but missing from \
                 documented DEGRADED_MODE_BASE_TOOL_NAMES — either add it to \
                 the const (if intentional) or move it to the db-gated branch",
                tool.name
            );
        }
    }
}
