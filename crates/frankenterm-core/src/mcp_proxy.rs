//! MCP proxy composition helpers (feature: `mcp-client`).
//!
//! This module mounts remote MCP tools into the local server namespace using an
//! explicit routing policy:
//! - local tools keep existing names (`wa.*`),
//! - remote tools are mounted under `<proxy_prefix>/<server>/<tool>`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// br-ft-8na0z + br-ft-59hlx: partial-mount failure counter for
// compose_proxy_tools.
//
// In soft-fallback mode (`proxy_strict=false` AND
// `proxy_fallback_to_local=true`), ELEVEN distinct silent-skip sites
// in compose_proxy_tools below short-circuit the function after a
// tracing::warn log. The structured warn carries per-event detail
// (server, code, reason) but is invisible to in-process forensic
// verification — an operator can't answer "did all my remote
// servers mount?" without log scraping.
//
// Site breakdown:
//   - 4 PRE-LOOP early-exits (br-ft-59hlx): client-disabled,
//     discovery-failed, selection-failed, no-servers-selected.
//   - 7 IN-LOOP per-server/tool skips (br-ft-8na0z, ft-bu09o):
//     connect failed, list_tools failed, post-filter empty,
//     duplicate exposed tool name, per-tool mapping failed,
//     post-mapping empty, route-prefix collision.
//
// This counter increments at every soft-skip site so the cumulative
// bound is observable. Tracing::warn keeps the per-event detail;
// the counter answers the high-level question. Same shape as
// ft-luav8 (record_mcp_audit failure counter) and ft-0texd
// (policy clock-anomaly counter).
static MCP_PROXY_MOUNT_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Cumulative count of MCP proxy mount-failure soft-skip events
/// since process load.
///
/// Covers ELEVEN soft-skip site classes:
/// **Pre-loop (br-ft-59hlx):** mcp_client.proxy_enabled+!enabled mismatch,
/// discover_servers failure, select_proxy_servers failure, empty selection.
/// **In-loop (br-ft-8na0z, ft-bu09o):** connect failure, list_tools failure,
/// post-filter empty, duplicate exposed tool name, per-tool mapping failure,
/// post-mapping empty, route-prefix collision.
///
/// Each soft-skip event also produces a structured `tracing::warn` with
/// the precise reason; the counter is the cumulative-bound forensic anchor
/// that lets an operator quantify "did proxy composition degrade this
/// session?" without scraping logs.
#[must_use]
pub fn mcp_proxy_mount_failure_count() -> u64 {
    MCP_PROXY_MOUNT_FAILURES.load(Ordering::Relaxed)
}

/// Test helper: reset the counter so tests that simulate
/// failures can assert post-increment values without state
/// leakage between tests.
#[cfg(test)]
fn reset_mcp_proxy_mount_failure_count_for_test() {
    MCP_PROXY_MOUNT_FAILURES.store(0, Ordering::Relaxed);
}

/// Internal helper: bump the counter at every soft-skip site.
fn record_mcp_proxy_mount_failure() {
    MCP_PROXY_MOUNT_FAILURES.fetch_add(1, Ordering::Relaxed);
}

/// br-ft-eljxp: cumulative count of times `compose_proxy_tools` was
/// invoked with `proxy_enabled = true` AND `db_path = None` and
/// elected to skip the entire proxy composition because the audit
/// stream was unavailable.
///
/// Pre-fix: degraded (no-db) bridges that had `proxy_enabled = true`
/// in their config would mount remote tools through the
/// FormatAwareToolHandler-only path (line 437 of this file pre-fix),
/// bypassing the AuditedToolHandler wrapper entirely. That created
/// a cross-module fail-open path between mcp_bridge (which
/// announced "no storage, no audit") and mcp_proxy (which silently
/// fell back to an unaudited wrapper).
///
/// Post-fix: in non-strict mode, the no-db gate fires before
/// discovery / selection / mounting, bumps this counter, emits a
/// structured warn, and returns the builder unchanged. In strict
/// mode (proxy_strict OR !proxy_fallback_to_local), the gate
/// returns Err.
///
/// Operators reading this counter can detect "I started a degraded
/// bridge but the proxy config is also live — the proxy surface
/// has been silently withheld due to the audit gap" without
/// scraping log output. Same observability defect family as
/// ft-647cj / ft-luav8 / ft-8na0z / ft-0texd / ft-2fjx0 / ft-153dy.
static MCP_PROXY_UNAUDITED_DEGRADED_SKIPS: AtomicU64 = AtomicU64::new(0);

/// Cumulative count of MCP proxy compositions skipped because
/// `db_path = None` (degraded bridge) made AuditedToolHandler
/// unavailable. See [`MCP_PROXY_UNAUDITED_DEGRADED_SKIPS`].
#[must_use]
pub fn mcp_proxy_unaudited_degraded_skip_count() -> u64 {
    MCP_PROXY_UNAUDITED_DEGRADED_SKIPS.load(Ordering::Relaxed)
}

/// Test helper: reset the counter so tests can assert
/// post-increment values without state leakage between tests.
#[cfg(test)]
fn reset_mcp_proxy_unaudited_degraded_skip_count_for_test() {
    MCP_PROXY_UNAUDITED_DEGRADED_SKIPS.store(0, Ordering::Relaxed);
}

#[inline]
fn record_mcp_proxy_unaudited_degraded_skip() {
    MCP_PROXY_UNAUDITED_DEGRADED_SKIPS.fetch_add(1, Ordering::Relaxed);
}

/// br-ft-153dy + ft-sr5pq: cumulative count of unsafe remote tools
/// soft-blocked by `filter_remote_tools` when
/// `proxy_allow_mutating_tools = false` (the safe default).
///
/// Distinct from [`MCP_PROXY_MOUNT_FAILURES`]: that counter
/// tracks SERVER-level skip events (connect failure, list_tools
/// failure, mapping failure, etc.) where the server didn't
/// register at all. This counter tracks TOOL-level events that
/// succeeded at the server level — the server still mounted,
/// just with one or more tools removed by safety policy.
///
/// Forensic verification contract:
/// `mounted_tools_per_server + safety_filtered_per_server
///  == upstream_list_tools_count`. The public accessor keeps its
/// historical "destructive" name for API continuity, but this now
/// covers destructive, mutating, missing, and malformed annotation
/// blocks.
///
/// Same observability defect family as ft-luav8 / ft-8na0z /
/// ft-0texd / ft-2fjx0 / ft-647cj — make policy-driven removals
/// observable instead of implicit.
static MCP_PROXY_DESTRUCTIVE_FILTERED: AtomicU64 = AtomicU64::new(0);

/// Cumulative count of unsafe remote tools soft-blocked
/// by the proxy safety filter. See
/// [`MCP_PROXY_DESTRUCTIVE_FILTERED`] for the contract.
#[must_use]
pub fn mcp_proxy_destructive_filtered_count() -> u64 {
    MCP_PROXY_DESTRUCTIVE_FILTERED.load(Ordering::Relaxed)
}

/// Test helper: reset the counter so tests that exercise the
/// safety-filter path can assert post-increment values
/// without state leakage between tests.
#[cfg(test)]
fn reset_mcp_proxy_destructive_filtered_count_for_test() {
    MCP_PROXY_DESTRUCTIVE_FILTERED.store(0, Ordering::Relaxed);
}

/// Internal helper: bump the counter when a remote tool is
/// filtered out by the per-server safety pass.
fn record_mcp_proxy_destructive_filtered() {
    MCP_PROXY_DESTRUCTIVE_FILTERED.fetch_add(1, Ordering::Relaxed);
}

/// br-ft-wzk10: cumulative count of `RemoteProxyToolHandler::call`
/// dispatch failures across four runtime soft-block paths.
///
/// Distinct from [`MCP_PROXY_MOUNT_FAILURES`] (compose-time
/// server skip events) and [`MCP_PROXY_DESTRUCTIVE_FILTERED`]
/// (compose-time tool-level safety filtering). This counter
/// tracks RUNTIME per-call dispatch failures: the local server
/// accepted the tool registration and dispatched the call, but
/// somewhere between Cx-checkpoint and remote-response the call
/// was rejected.
///
/// Site breakdown (all in `RemoteProxyToolHandler::call`):
///   - **Path C — pre_expired**: `ctx.cx().checkpoint()` failed;
///     caller's Cx was cancelled or budget-exhausted before the
///     per-server Mutex was acquired (br-ft-xhj38 pre-flight).
///   - **Path D — lock_poisoned**: `self.client.lock()` returned
///     Err because a prior thread holding the Mutex panicked.
///     Pre-fix this site had NO tracing::warn; this counter is
///     the only forensic anchor.
///   - **Path E — remote_failed**: remote MCP server rejected
///     the call or transport failed.
///   - **Path F — decode_failed**: remote returned content the
///     local framework couldn't map back into our type surface.
///
/// Forensic verification contract:
/// `calls_attempted == calls_succeeded + mcp_proxy_call_dispatch_failure_count()`
///
/// Same observability defect family as ft-luav8 / ft-skec1 /
/// ft-8na0z / ft-153dy / ft-tpdl5 — make silent state loss visible.
static MCP_PROXY_CALL_DISPATCH_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Cumulative count of `RemoteProxyToolHandler::call` runtime
/// per-call dispatch failures. See
/// [`MCP_PROXY_CALL_DISPATCH_FAILURES`] for the four contributing
/// site classes.
#[must_use]
pub fn mcp_proxy_call_dispatch_failure_count() -> u64 {
    MCP_PROXY_CALL_DISPATCH_FAILURES.load(Ordering::Relaxed)
}

/// Test helper: reset the counter so tests that exercise the
/// per-call dispatch-failure paths can assert post-increment
/// values without state leakage between tests.
#[cfg(test)]
fn reset_mcp_proxy_call_dispatch_failure_count_for_test() {
    MCP_PROXY_CALL_DISPATCH_FAILURES.store(0, Ordering::Relaxed);
}

/// Internal helper: bump the counter when a per-call dispatch
/// fails at any of the four sites enumerated in
/// [`MCP_PROXY_CALL_DISPATCH_FAILURES`].
fn record_mcp_proxy_call_dispatch_failure() {
    MCP_PROXY_CALL_DISPATCH_FAILURES.fetch_add(1, Ordering::Relaxed);
}

#[allow(unused_imports)]
use crate::mcp_framework::{
    FrameworkContent as Content, FrameworkMcpContext as McpContext, FrameworkMcpError as McpError,
    FrameworkMcpResult as McpResult, FrameworkServer as Server, FrameworkServerBuilder,
    FrameworkTool as Tool, FrameworkToolHandler as ToolHandler,
};

use super::mcp_middleware::{AuditedToolHandler, FormatAwareToolHandler};
use crate::Result;
use crate::config::{Config, McpClientConfig};
use crate::mcp_client::{
    ExternalServerConfig, FtMcpClient, McpClientContentItem, McpClientToolDefinition,
    discover_servers,
};

const LOG_TARGET: &str = "ft::mcp_proxy";

pub(super) fn compose_proxy_tools(
    mut builder: FrameworkServerBuilder,
    config: &Config,
    db_path: Option<Arc<PathBuf>>,
) -> Result<FrameworkServerBuilder> {
    let settings = &config.mcp_client;
    if !settings.proxy_enabled {
        return Ok(builder);
    }

    let fail_fast = settings.proxy_strict || !settings.proxy_fallback_to_local;
    if !settings.enabled {
        let message = "mcp_client.proxy_enabled requires mcp_client.enabled=true";
        if fail_fast {
            return Err(crate::error::ConfigError::ValidationError(message.to_string()).into());
        }
        // br-ft-59hlx: pre-loop silent-skip site #A (client-disabled).
        record_mcp_proxy_mount_failure();
        tracing::warn!(
            target: LOG_TARGET,
            event = "mcp_proxy_disabled_client",
            fallback_to_local = settings.proxy_fallback_to_local,
            strict = settings.proxy_strict,
            "{message}; continuing with local-only MCP server"
        );
        return Ok(builder);
    }

    // br-ft-eljxp: degraded-bridge cross-module fail-closed gate.
    // When db_path is None, the per-server mount loop's audit-wrap
    // branch (pre-fix: `if let Some(path) = db_path.as_ref() {
    // AuditedToolHandler::new(...) } else { FormatAwareToolHandler::
    // new(handler) }`) silently fell back to the unaudited wrapper.
    // That contradicted the br-ft-647cj degraded-mode contract
    // advertised by mcp_bridge — the bridge claimed "no storage, no
    // audit" but the proxy layer still mounted remote tools
    // (potentially mutation-capable) without recording calls.
    //
    // Now: if db_path is None past the mcp_client-enabled check,
    // fail closed (strict mode) or skip composition entirely
    // (non-strict mode) so no remote tool can mount unaudited. Bump
    // MCP_PROXY_UNAUDITED_DEGRADED_SKIPS so operators can detect
    // the elision without scraping log output. Placed AFTER the
    // !enabled mismatch check so config errors surface in their
    // existing dedicated path; placed BEFORE discover_servers so we
    // don't pay the discovery cost when we're about to refuse.
    if db_path.is_none() {
        let message = "mcp_client.proxy_enabled with no audit db_path: refusing to mount \
             remote proxy tools through the unaudited fallback wrapper \
             (br-ft-eljxp). Configure a database path or disable proxy.";
        if fail_fast {
            return Err(crate::error::ConfigError::ValidationError(message.to_string()).into());
        }
        record_mcp_proxy_unaudited_degraded_skip();
        tracing::warn!(
            target: LOG_TARGET,
            event = "mcp_proxy_unaudited_degraded_skip",
            fallback_to_local = settings.proxy_fallback_to_local,
            strict = settings.proxy_strict,
            "{message} Continuing with local-only MCP server."
        );
        return Ok(builder);
    }

    let discovered = match discover_servers(config) {
        Ok(servers) => servers,
        Err(err) => {
            if fail_fast {
                return Err(crate::error::ConfigError::ValidationError(format!(
                    "mcp proxy discovery failed: {}",
                    err.message
                ))
                .into());
            }
            // br-ft-59hlx: pre-loop silent-skip site #B (discovery).
            record_mcp_proxy_mount_failure();
            tracing::warn!(
                target: LOG_TARGET,
                event = "mcp_proxy_discovery_failed",
                code = err.code,
                message = %err.message,
                fallback_to_local = settings.proxy_fallback_to_local,
                strict = settings.proxy_strict,
                "Remote MCP discovery failed; continuing with local-only MCP server"
            );
            return Ok(builder);
        }
    };

    let selected = match select_proxy_servers(settings, &discovered) {
        Ok(selected) => selected,
        Err(message) => {
            let wrapped = format!("mcp proxy server selection failed: {message}");
            if fail_fast {
                return Err(crate::error::ConfigError::ValidationError(wrapped).into());
            }
            // br-ft-59hlx: pre-loop silent-skip site #C (selection).
            record_mcp_proxy_mount_failure();
            tracing::warn!(
                target: LOG_TARGET,
                event = "mcp_proxy_selection_failed",
                message = %message,
                fallback_to_local = settings.proxy_fallback_to_local,
                strict = settings.proxy_strict,
                "Remote MCP server selection failed; continuing with local-only MCP server"
            );
            return Ok(builder);
        }
    };

    if selected.is_empty() {
        let message = "no remote MCP servers selected for proxy composition";
        if fail_fast {
            return Err(crate::error::ConfigError::ValidationError(message.to_string()).into());
        }
        // br-ft-59hlx: pre-loop silent-skip site #D (empty selection).
        record_mcp_proxy_mount_failure();
        tracing::warn!(
            target: LOG_TARGET,
            event = "mcp_proxy_no_servers",
            fallback_to_local = settings.proxy_fallback_to_local,
            strict = settings.proxy_strict,
            "{message}; continuing with local-only MCP server"
        );
        return Ok(builder);
    }

    let mut mounted_tools = 0usize;
    let mut mounted_servers = 0usize;
    let mut used_route_prefixes = HashSet::new();
    let base_prefix = settings.proxy_prefix.trim();

    for server in selected {
        let server_name = server.name.clone();
        let route_prefix = format!("{base_prefix}/{}", sanitize_prefix_segment(&server_name));
        let remote = match FtMcpClient::connect_external(server, settings) {
            Ok(client) => client,
            Err(err) => {
                if fail_fast {
                    return Err(crate::error::ConfigError::ValidationError(format!(
                        "mcp proxy connect failed for server '{server_name}': {}",
                        err.message
                    ))
                    .into());
                }
                // br-ft-8na0z: silent-skip site #1 (connect).
                record_mcp_proxy_mount_failure();
                tracing::warn!(
                    target: LOG_TARGET,
                    event = "mcp_proxy_connect_failed",
                    server = %server_name,
                    code = err.code,
                    message = %err.message,
                    fallback_to_local = settings.proxy_fallback_to_local,
                    strict = settings.proxy_strict,
                    "Remote MCP connect failed; skipping server"
                );
                continue;
            }
        };

        let shared_client = Arc::new(Mutex::new(remote));
        let tools = match list_remote_tools(&shared_client, &server_name) {
            Ok(tools) => tools,
            Err(err) => {
                if fail_fast {
                    return Err(crate::error::ConfigError::ValidationError(format!(
                        "mcp proxy tool catalog failed for server '{server_name}': {}",
                        err.message
                    ))
                    .into());
                }
                // br-ft-8na0z: silent-skip site #2 (list_tools).
                record_mcp_proxy_mount_failure();
                tracing::warn!(
                    target: LOG_TARGET,
                    event = "mcp_proxy_list_tools_failed",
                    server = %server_name,
                    code = err.code,
                    message = %err.message,
                    fallback_to_local = settings.proxy_fallback_to_local,
                    strict = settings.proxy_strict,
                    "Failed to fetch remote tool catalog; skipping server"
                );
                continue;
            }
        };

        let filtered = filter_remote_tools(settings, tools);
        if filtered.is_empty() {
            // br-ft-8na0z: silent-skip site #3 (post-filter empty).
            record_mcp_proxy_mount_failure();
            tracing::warn!(
                target: LOG_TARGET,
                event = "mcp_proxy_no_tools_after_filter",
                server = %server_name,
                allow_mutating = settings.proxy_allow_mutating_tools,
                "No tools remained after proxy safety filtering; skipping server"
            );
            continue;
        }

        let unique_tools =
            unique_proxy_tools_by_exposed_name(&server_name, &route_prefix, filtered, settings)?;

        let mut mounted_handlers = Vec::new();
        for (exposed_name, tool) in unique_tools {
            let external_name = tool.name.clone();
            let handler = match RemoteProxyToolHandler::new(
                tool,
                exposed_name.clone(),
                external_name.clone(),
                server_name.clone(),
                Arc::clone(&shared_client),
            ) {
                Ok(handler) => handler,
                Err(err) => {
                    if fail_fast {
                        return Err(crate::error::ConfigError::ValidationError(format!(
                            "mcp proxy tool mapping failed for server '{server_name}' tool '{external_name}': {}",
                            err.message
                        ))
                        .into());
                    }
                    // br-ft-8na0z: silent-skip site #4 (per-tool mapping).
                    record_mcp_proxy_mount_failure();
                    tracing::warn!(
                        target: LOG_TARGET,
                        event = "mcp_proxy_tool_mapping_failed",
                        server = %server_name,
                        tool = %external_name,
                        code = err.code,
                        message = %err.message,
                        fallback_to_local = settings.proxy_fallback_to_local,
                        strict = settings.proxy_strict,
                        "Failed to map remote tool definition across local proxy seam; skipping tool"
                    );
                    continue;
                }
            };
            mounted_handlers.push((exposed_name, handler));
        }

        if mounted_handlers.is_empty() {
            // br-ft-8na0z: silent-skip site #5 (post-mapping empty).
            record_mcp_proxy_mount_failure();
            tracing::warn!(
                target: LOG_TARGET,
                event = "mcp_proxy_no_tools_after_mapping",
                server = %server_name,
                route_prefix = %route_prefix,
                fallback_to_local = settings.proxy_fallback_to_local,
                strict = settings.proxy_strict,
                "Remote MCP server had no tools that survived local seam mapping; skipping server"
            );
            continue;
        }

        if !insert_route_prefix(&mut used_route_prefixes, &route_prefix) {
            let message = format!(
                "mcp proxy route prefix collision for server '{server_name}': {route_prefix}"
            );
            if fail_fast {
                return Err(crate::error::ConfigError::ValidationError(message).into());
            }
            // br-ft-8na0z: silent-skip site #6 (route prefix collision).
            record_mcp_proxy_mount_failure();
            tracing::warn!(
                target: LOG_TARGET,
                event = "mcp_proxy_route_prefix_collision",
                server = %server_name,
                route_prefix = %route_prefix,
                fallback_to_local = settings.proxy_fallback_to_local,
                strict = settings.proxy_strict,
                "Remote MCP route prefix collision detected; skipping server"
            );
            continue;
        }

        let server_tools = mounted_handlers.len();
        // br-ft-eljxp: db_path is guaranteed Some past the no-db
        // gate at the top of this function. The previous unaudited-
        // fallback branch (FormatAwareToolHandler::new(handler) with
        // no AuditedToolHandler wrap) is dead code post-fix and has
        // been removed to prevent any future regression that would
        // re-introduce the cross-module audit bypass.
        let audit_path = db_path.as_ref().expect(
            "br-ft-eljxp: db_path must be Some past the no-db gate at \
             the top of compose_proxy_tools",
        );
        for (exposed_name, handler) in mounted_handlers {
            builder = builder.tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                handler,
                exposed_name,
                Arc::clone(audit_path),
            )));
        }

        mounted_servers += 1;
        mounted_tools += server_tools;
        tracing::info!(
            target: LOG_TARGET,
            event = "mcp_proxy_mounted_server",
            server = %server_name,
            route_prefix = %route_prefix,
            mounted_tools = server_tools,
            "Mounted remote MCP tools"
        );
    }

    if mounted_servers == 0 {
        let message = "mcp proxy composition produced zero mounted remote servers";
        if fail_fast {
            return Err(crate::error::ConfigError::ValidationError(message.to_string()).into());
        }
        tracing::warn!(
            target: LOG_TARGET,
            event = "mcp_proxy_mount_none",
            fallback_to_local = settings.proxy_fallback_to_local,
            strict = settings.proxy_strict,
            "{message}; continuing with local-only MCP server"
        );
    } else {
        tracing::info!(
            target: LOG_TARGET,
            event = "mcp_proxy_compose_complete",
            mounted_servers,
            mounted_tools,
            route_policy = "prefix",
            allow_mutating = settings.proxy_allow_mutating_tools,
            "MCP proxy composition complete"
        );
    }

    Ok(builder)
}

fn insert_route_prefix(used_route_prefixes: &mut HashSet<String>, route_prefix: &str) -> bool {
    used_route_prefixes.insert(route_prefix.to_ascii_lowercase())
}

fn list_remote_tools(
    client: &Arc<Mutex<FtMcpClient>>,
    server_name: &str,
) -> crate::mcp_client::McpClientResult<Vec<McpClientToolDefinition>> {
    let mut guard = client.lock().map_err(|_| {
        crate::mcp_client::McpClientError::new(
            "mcp_proxy.client_lock_poisoned",
            format!("server '{server_name}': proxy client lock poisoned"),
        )
    })?;
    guard.list_tools()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyToolSafetyBlockReason {
    MissingAnnotations,
    UnknownAnnotationShape,
    DestructiveToolBlocked,
    MutatingToolBlocked,
}

impl ProxyToolSafetyBlockReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::MissingAnnotations => "missing_annotations_blocked",
            Self::UnknownAnnotationShape => "unknown_annotations_blocked",
            Self::DestructiveToolBlocked => "destructive_tool_blocked",
            Self::MutatingToolBlocked => "mutating_tool_blocked",
        }
    }
}

fn annotation_bool(annotations: &serde_json::Map<String, serde_json::Value>, key: &str) -> bool {
    annotations
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn proxy_tool_safety_block_reason(
    tool: &McpClientToolDefinition,
) -> Option<ProxyToolSafetyBlockReason> {
    let Some(annotations) = tool.annotations.as_ref() else {
        return Some(ProxyToolSafetyBlockReason::MissingAnnotations);
    };
    let Some(annotations) = annotations.as_object() else {
        return Some(ProxyToolSafetyBlockReason::UnknownAnnotationShape);
    };

    if annotation_bool(annotations, "destructive")
        || annotation_bool(annotations, "destructiveHint")
    {
        return Some(ProxyToolSafetyBlockReason::DestructiveToolBlocked);
    }
    if annotations
        .get("readOnly")
        .and_then(serde_json::Value::as_bool)
        .is_some_and(|read_only| !read_only)
        || annotations
            .get("readOnlyHint")
            .and_then(serde_json::Value::as_bool)
            .is_some_and(|read_only| !read_only)
    {
        return Some(ProxyToolSafetyBlockReason::MutatingToolBlocked);
    }

    None
}

/// br-ft-gmt1c: a block reason is "explicit" iff the tool's
/// annotations object is well-formed and self-classifies as
/// destructive or mutating. Missing or malformed annotations are
/// NOT explicit — the operator opting into mutating tools cannot
/// know what they're admitting if the safety metadata is absent
/// or shape-broken.
fn proxy_block_reason_is_explicit(reason: ProxyToolSafetyBlockReason) -> bool {
    matches!(
        reason,
        ProxyToolSafetyBlockReason::DestructiveToolBlocked
            | ProxyToolSafetyBlockReason::MutatingToolBlocked
    )
}

fn filter_remote_tools(
    settings: &McpClientConfig,
    tools: Vec<McpClientToolDefinition>,
) -> Vec<McpClientToolDefinition> {
    let mut filtered = Vec::with_capacity(tools.len());
    for tool in tools {
        let Some(reason) = proxy_tool_safety_block_reason(&tool) else {
            // No block reason — tool is safe by annotation.
            filtered.push(tool);
            continue;
        };
        // br-ft-gmt1c: `proxy_allow_mutating_tools` only bypasses
        // EXPLICIT destructive/mutating annotations. Tools with
        // missing or malformed annotations remain blocked
        // regardless of the opt-in, because the operator cannot
        // consent to admit unsafe metadata they can't see.
        if settings.proxy_allow_mutating_tools && proxy_block_reason_is_explicit(reason) {
            filtered.push(tool);
            continue;
        }
        // br-ft-153dy: bump the cumulative counter alongside
        // the per-event tracing::warn so operators can
        // quantify the policy-driven removal blast radius
        // without scraping logs.
        record_mcp_proxy_destructive_filtered();
        tracing::warn!(
            target: LOG_TARGET,
            event = "mcp_proxy_tool_filtered",
            tool = %tool.name,
            reason = reason.as_str(),
            "Skipping unsafe remote tool due to proxy safety policy"
        );
    }
    filtered
}

fn unique_proxy_tools_by_exposed_name(
    server_name: &str,
    route_prefix: &str,
    tools: Vec<McpClientToolDefinition>,
    settings: &McpClientConfig,
) -> Result<Vec<(String, McpClientToolDefinition)>> {
    let fail_fast = settings.proxy_strict || !settings.proxy_fallback_to_local;
    let mut used_exposed_names = HashSet::new();
    let mut unique = Vec::with_capacity(tools.len());

    for tool in tools {
        let external_name = tool.name.clone();
        let exposed_name = format!("{route_prefix}/{external_name}");
        if used_exposed_names.insert(exposed_name.clone()) {
            unique.push((exposed_name, tool));
            continue;
        }

        let message = format!(
            "mcp proxy duplicate exposed tool name for server '{server_name}' tool \
             '{external_name}': {exposed_name}"
        );
        if fail_fast {
            return Err(crate::error::ConfigError::ValidationError(message).into());
        }

        // ft-bu09o: a malformed remote catalog can advertise the same tool
        // route more than once. FastMCP keeps one registration and only logs,
        // so de-duplicate before builder registration and make the skipped
        // duplicate observable.
        record_mcp_proxy_mount_failure();
        tracing::warn!(
            target: LOG_TARGET,
            event = "mcp_proxy_duplicate_exposed_tool_name",
            server = %server_name,
            tool = %external_name,
            exposed_name = %exposed_name,
            fallback_to_local = settings.proxy_fallback_to_local,
            strict = settings.proxy_strict,
            "Remote MCP server advertised a duplicate proxied tool name; skipping duplicate"
        );
    }

    Ok(unique)
}

fn select_proxy_servers(
    settings: &McpClientConfig,
    discovered: &[ExternalServerConfig],
) -> std::result::Result<Vec<ExternalServerConfig>, String> {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();

    let mut push_server = |name: &str| -> std::result::Result<(), String> {
        let name = name.trim();
        let server = discovered
            .iter()
            .find(|item| item.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("configured proxy server not found: {name}"))?;
        if server.disabled {
            return Err(format!(
                "configured proxy server is disabled: {}",
                server.name
            ));
        }

        let canonical = server.name.to_ascii_lowercase();
        if seen.insert(canonical) {
            selected.push(server.clone());
        }
        Ok(())
    };

    if !settings.proxy_servers.is_empty() {
        for server in &settings.proxy_servers {
            push_server(server)?;
        }
        return Ok(selected);
    }

    if settings.proxy_mount_all_discovered {
        for server in discovered {
            if server.disabled {
                continue;
            }
            let canonical = server.name.to_ascii_lowercase();
            if seen.insert(canonical) {
                selected.push(server.clone());
            }
        }
        return Ok(selected);
    }

    if settings.preferred_servers.is_empty() {
        return Err(
            "proxy_mount_all_discovered=false requires proxy_servers or preferred_servers"
                .to_string(),
        );
    }

    for server in &settings.preferred_servers {
        push_server(server)?;
    }

    Ok(selected)
}

fn sanitize_prefix_segment(name: &str) -> String {
    let mut value = String::with_capacity(name.len());
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            value.push(ch.to_ascii_lowercase());
        } else {
            value.push('-');
        }
    }
    let value = value.trim_matches('-');
    if value.is_empty() {
        "server".to_string()
    } else {
        value.to_string()
    }
}

struct RemoteProxyToolHandler {
    definition: Tool,
    exposed_name: String,
    external_name: String,
    server_name: String,
    client: Arc<Mutex<FtMcpClient>>,
}

impl RemoteProxyToolHandler {
    fn new(
        definition: McpClientToolDefinition,
        exposed_name: String,
        external_name: String,
        server_name: String,
        client: Arc<Mutex<FtMcpClient>>,
    ) -> crate::mcp_client::McpClientResult<Self> {
        let mut definition = definition.into_framework()?;
        definition.name.clone_from(&exposed_name);
        Ok(Self {
            definition,
            exposed_name,
            external_name,
            server_name,
            client,
        })
    }
}

impl ToolHandler for RemoteProxyToolHandler {
    fn definition(&self) -> Tool {
        self.definition.clone()
    }

    fn call(&self, ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        // br-ft-xhj38: pre-flight Cx checkpoint BEFORE acquiring
        // the per-server Mutex. Two reasons:
        //
        // 1. **Audit-completeness**: matches the option-A++ pattern
        //    that ft-ymn10 just shipped at mcp_middleware.rs:185-202.
        //    A caller whose Cx is already cancelled or budget-
        //    exhausted gets a typed error before we touch the
        //    remote, instead of having the deadline silently
        //    violated by an unbounded `guard.call_tool(...)`.
        //
        // 2. **Head-of-line blocking**: the per-server Mutex on
        //    `shared_client` serializes all proxy calls to that
        //    server. Without this checkpoint, a pre-expired Cx
        //    would still acquire the Mutex and run the (likely-
        //    long) remote call, blocking every other in-flight
        //    request on the same server. The checkpoint short-
        //    circuits BEFORE the lock so the next caller's wait
        //    time isn't bounded by an already-doomed request.
        //
        // Mid-call hangs (the bigger problem when the remote
        // server itself is hung) require ToolHandler trait
        // surgery to wrap call_tool in tokio::time::timeout —
        // tracked separately under ft-bd3vr (blocked on fastmcp
        // upstream API) and the option-B paragraph in this
        // bead. This commit ships option-A++ alone, which is
        // bounded and immediately useful.
        if let Err(cx_err) = ctx.cx().checkpoint() {
            // br-ft-wzk10 site C: pre-flight Cx checkpoint failed.
            record_mcp_proxy_call_dispatch_failure();
            tracing::warn!(
                target: LOG_TARGET,
                event = "mcp_proxy_route_pre_expired",
                route = "remote",
                server = %self.server_name,
                tool = %self.exposed_name,
                cx_err = %cx_err,
                elapsed_ms = start.elapsed().as_millis(),
                "br-ft-xhj38: proxy call short-circuited before \
                 Mutex acquire because Cx pre-flight checkpoint failed"
            );
            return Err(McpError::internal_error(format!(
                "Cx pre-flight checkpoint failed before proxy route '{}' dispatch: {cx_err}",
                self.exposed_name
            )));
        }

        let mut guard = self.client.lock().map_err(|_| {
            // br-ft-wzk10 site D: per-server Mutex<FtMcpClient> was
            // poisoned (a prior thread holding the lock panicked).
            // Pre-fix this site had NO tracing::warn — the counter
            // bump + the new structured warn below are the only
            // forensic anchors.
            record_mcp_proxy_call_dispatch_failure();
            tracing::warn!(
                target: LOG_TARGET,
                event = "mcp_proxy_route_lock_poisoned",
                route = "remote",
                server = %self.server_name,
                tool = %self.exposed_name,
                elapsed_ms = start.elapsed().as_millis(),
                "br-ft-wzk10: per-server FtMcpClient Mutex poisoned; \
                 proxy call rejected"
            );
            McpError::internal_error(format!(
                "proxy route '{}' failed: remote client lock poisoned",
                self.exposed_name
            ))
        })?;

        match guard.call_tool(&self.external_name, arguments) {
            Ok(content) => {
                let content = content
                    .into_iter()
                    .map(McpClientContentItem::into_framework)
                    .collect::<crate::mcp_client::McpClientResult<Vec<Content>>>()
                    .map_err(|err| {
                        // br-ft-wzk10 site F: remote returned content
                        // the local framework couldn't map back into
                        // our type surface.
                        record_mcp_proxy_call_dispatch_failure();
                        tracing::warn!(
                            target: LOG_TARGET,
                            event = "mcp_proxy_route_decode_failed",
                            route = "remote",
                            server = %self.server_name,
                            tool = %self.exposed_name,
                            code = err.code,
                            message = %err.message,
                            elapsed_ms = start.elapsed().as_millis(),
                            "Remote MCP proxy tool returned content that could not be mapped back into the local framework surface"
                        );
                        McpError::tool_error(format!("[{}] {}", err.code, err.message))
                    })?;
                tracing::info!(
                    target: LOG_TARGET,
                    event = "mcp_proxy_route",
                    route = "remote",
                    server = %self.server_name,
                    tool = %self.exposed_name,
                    elapsed_ms = start.elapsed().as_millis(),
                    "Executed proxied remote MCP tool"
                );
                Ok(content)
            }
            Err(err) => {
                // br-ft-wzk10 site E: remote MCP server rejected
                // the call or transport failed.
                record_mcp_proxy_call_dispatch_failure();
                tracing::warn!(
                    target: LOG_TARGET,
                    event = "mcp_proxy_route_failed",
                    route = "remote",
                    server = %self.server_name,
                    tool = %self.exposed_name,
                    code = err.code,
                    message = %err.message,
                    elapsed_ms = start.elapsed().as_millis(),
                    "Remote MCP proxy tool failed"
                );
                Err(McpError::tool_error(format!(
                    "[{}] {}",
                    err.code, err.message
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Config, ExternalServerConfig, McpClientConfig, McpClientToolDefinition, Server,
        compose_proxy_tools, filter_remote_tools, insert_route_prefix,
        mcp_proxy_destructive_filtered_count, reset_mcp_proxy_destructive_filtered_count_for_test,
        sanitize_prefix_segment, select_proxy_servers,
    };
    use proptest::prelude::*;
    use std::collections::HashMap;
    use std::collections::HashSet;

    fn make_server(name: &str, disabled: bool) -> ExternalServerConfig {
        ExternalServerConfig {
            name: name.to_string(),
            command: "python3".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            disabled,
        }
    }

    fn make_safe_tool(name: &str) -> McpClientToolDefinition {
        McpClientToolDefinition {
            name: name.to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: Vec::new(),
            annotations: Some(serde_json::json!({"destructive": false, "readOnly": true})),
        }
    }

    #[test]
    fn select_proxy_servers_mount_all_filters_disabled() {
        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_mount_all_discovered: true,
            ..McpClientConfig::default()
        };
        let discovered = vec![
            make_server("alpha", false),
            make_server("beta", true),
            make_server("gamma", false),
        ];

        let selected = select_proxy_servers(&settings, &discovered).expect("select servers");
        let names: Vec<String> = selected.into_iter().map(|item| item.name).collect();
        assert_eq!(names, vec!["alpha".to_string(), "gamma".to_string()]);
    }

    #[test]
    fn select_proxy_servers_uses_explicit_order() {
        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_mount_all_discovered: false,
            proxy_servers: vec!["gamma".to_string(), "alpha".to_string()],
            ..McpClientConfig::default()
        };
        let discovered = vec![
            make_server("alpha", false),
            make_server("gamma", false),
            make_server("zeta", false),
        ];

        let selected = select_proxy_servers(&settings, &discovered).expect("select servers");
        let names: Vec<String> = selected.into_iter().map(|item| item.name).collect();
        assert_eq!(names, vec!["gamma".to_string(), "alpha".to_string()]);
    }

    #[test]
    fn select_proxy_servers_trims_explicit_names() {
        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_mount_all_discovered: false,
            proxy_servers: vec!["  gamma  ".to_string()],
            ..McpClientConfig::default()
        };
        let discovered = vec![make_server("gamma", false)];

        let selected = select_proxy_servers(&settings, &discovered).expect("select servers");
        let names: Vec<String> = selected.into_iter().map(|item| item.name).collect();
        assert_eq!(names, vec!["gamma".to_string()]);
    }

    #[test]
    fn select_proxy_servers_rejects_missing_explicit_server() {
        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_mount_all_discovered: false,
            proxy_servers: vec!["delta".to_string()],
            ..McpClientConfig::default()
        };
        let discovered = vec![make_server("alpha", false)];

        let err = select_proxy_servers(&settings, &discovered).unwrap_err();
        assert!(err.contains("configured proxy server not found"));
    }

    #[test]
    fn sanitize_prefix_segment_normalizes_symbols() {
        assert_eq!(sanitize_prefix_segment("GitHub Copilot"), "github-copilot");
        assert_eq!(sanitize_prefix_segment("___"), "___");
        assert_eq!(sanitize_prefix_segment(" / "), "server");
    }

    #[test]
    fn unique_proxy_tools_by_exposed_name_skips_duplicate_and_counts_ft_bu09o() {
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_mount_failure_count_for_test();
        let settings = McpClientConfig {
            proxy_strict: false,
            proxy_fallback_to_local: true,
            ..McpClientConfig::default()
        };
        let tools = vec![
            make_safe_tool("same"),
            make_safe_tool("same"),
            make_safe_tool("other"),
        ];

        let unique =
            super::unique_proxy_tools_by_exposed_name("srv", "remote/srv", tools, &settings)
                .expect("soft duplicate handling should keep first route");
        let mounted_names: Vec<&str> = unique
            .iter()
            .map(|(exposed_name, _)| exposed_name.as_str())
            .collect();

        assert_eq!(mounted_names, vec!["remote/srv/same", "remote/srv/other"]);
        assert_eq!(
            super::mcp_proxy_mount_failure_count(),
            1,
            "ft-bu09o: skipped duplicate exposed tool names must be observable"
        );
    }

    #[test]
    fn unique_proxy_tools_by_exposed_name_fails_fast_on_duplicate_ft_bu09o() {
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_mount_failure_count_for_test();
        let settings = McpClientConfig {
            proxy_strict: true,
            proxy_fallback_to_local: false,
            ..McpClientConfig::default()
        };
        let tools = vec![make_safe_tool("same"), make_safe_tool("same")];

        let err = super::unique_proxy_tools_by_exposed_name("srv", "remote/srv", tools, &settings)
            .expect_err("strict duplicate handling must fail composition");
        assert!(err.to_string().contains("duplicate exposed tool name"));
        assert_eq!(
            super::mcp_proxy_mount_failure_count(),
            0,
            "strict mode returns an error instead of recording a soft skip"
        );
    }

    #[test]
    fn filter_remote_tools_blocks_destructive_by_default() {
        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_allow_mutating_tools: false,
            ..McpClientConfig::default()
        };
        let safe = McpClientToolDefinition {
            name: "safe".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(serde_json::json!({"destructive": false})),
        };
        let destructive = McpClientToolDefinition {
            name: "drop_db".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(serde_json::json!({"destructive": true})),
        };

        let filtered = locked_filter_remote_tools(&settings, vec![safe, destructive]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "safe");
    }

    #[test]
    fn filter_remote_tools_blocks_mutating_read_only_annotations_by_default() {
        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_allow_mutating_tools: false,
            ..McpClientConfig::default()
        };
        let read_only = McpClientToolDefinition {
            name: "read_file".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(serde_json::json!({"destructive": false, "readOnly": true})),
        };
        let mutating_read_only_false = McpClientToolDefinition {
            name: "write_file".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(serde_json::json!({"destructive": false, "readOnly": false})),
        };
        let mutating_hint_false = McpClientToolDefinition {
            name: "create_ticket".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(serde_json::json!({"destructive": false, "readOnlyHint": false})),
        };
        let destructive_hint = McpClientToolDefinition {
            name: "drop_cache".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(serde_json::json!({"destructiveHint": true, "readOnly": true})),
        };

        let filtered = locked_filter_remote_tools(
            &settings,
            vec![
                read_only,
                mutating_read_only_false,
                mutating_hint_false,
                destructive_hint,
            ],
        );

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "read_file");
    }

    #[test]
    fn filter_remote_tools_blocks_missing_or_malformed_annotations_by_default() {
        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_allow_mutating_tools: false,
            ..McpClientConfig::default()
        };
        let missing_annotations = McpClientToolDefinition {
            name: "unknown_missing".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: None,
        };
        let malformed_annotations = McpClientToolDefinition {
            name: "unknown_malformed".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(serde_json::json!("read-only")),
        };

        let filtered =
            locked_filter_remote_tools(&settings, vec![missing_annotations, malformed_annotations]);

        assert!(filtered.is_empty());
    }

    /// br-ft-153dy: filtering one destructive tool bumps the
    /// new cumulative counter by exactly 1.
    #[test]
    fn filter_remote_tools_bumps_destructive_counter() {
        let _guard = proxy_counter_test_lock();
        reset_mcp_proxy_destructive_filtered_count_for_test();
        let before = mcp_proxy_destructive_filtered_count();

        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_allow_mutating_tools: false,
            ..McpClientConfig::default()
        };
        let destructive = McpClientToolDefinition {
            name: "drop_db".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(serde_json::json!({"destructive": true})),
        };

        let _ = filter_remote_tools(&settings, vec![destructive]);
        let after = mcp_proxy_destructive_filtered_count();
        assert_eq!(
            after - before,
            1,
            "br-ft-153dy: each filtered destructive tool must bump the counter by 1"
        );
    }

    /// br-ft-153dy: filtering N destructive tools across one
    /// call bumps by exactly N. Catches any future refactor that
    /// drops the counter into a per-server-loop instead of a
    /// per-tool one.
    #[test]
    fn filter_remote_tools_destructive_counter_matches_filter_count() {
        let _guard = proxy_counter_test_lock();
        reset_mcp_proxy_destructive_filtered_count_for_test();
        let before = mcp_proxy_destructive_filtered_count();

        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_allow_mutating_tools: false,
            ..McpClientConfig::default()
        };
        let make_destructive = |n: u32| McpClientToolDefinition {
            name: format!("drop_db_{n}"),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(serde_json::json!({"destructive": true})),
        };
        let make_safe = |n: u32| McpClientToolDefinition {
            name: format!("read_{n}"),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(serde_json::json!({"destructive": false})),
        };

        let mixed = vec![
            make_destructive(1),
            make_safe(1),
            make_destructive(2),
            make_destructive(3),
            make_safe(2),
            make_destructive(4),
            make_destructive(5),
        ];
        let filtered = filter_remote_tools(&settings, mixed);
        assert_eq!(filtered.len(), 2, "two safe tools survive the filter");

        let after = mcp_proxy_destructive_filtered_count();
        assert_eq!(
            after - before,
            5,
            "br-ft-153dy: 5 destructive tools must increment the counter by 5"
        );
    }

    /// br-ft-153dy: when the operator opts into mutating tools
    /// (`proxy_allow_mutating_tools = true`), no filtering
    /// happens and the counter stays untouched.
    #[test]
    fn filter_remote_tools_allow_mutating_does_not_bump_counter() {
        let _guard = proxy_counter_test_lock();
        reset_mcp_proxy_destructive_filtered_count_for_test();
        let before = mcp_proxy_destructive_filtered_count();

        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_allow_mutating_tools: true,
            ..McpClientConfig::default()
        };
        let destructive = McpClientToolDefinition {
            name: "drop_db".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(serde_json::json!({"destructive": true})),
        };
        let _ = filter_remote_tools(&settings, vec![destructive]);
        let after = mcp_proxy_destructive_filtered_count();
        assert_eq!(
            after, before,
            "br-ft-153dy: allow_mutating_tools=true must NOT bump destructive counter"
        );
    }

    #[test]
    fn insert_route_prefix_detects_case_insensitive_collisions() {
        let mut used = HashSet::new();
        assert!(insert_route_prefix(&mut used, "remote/github-copilot"));
        assert!(!insert_route_prefix(&mut used, "REMOTE/GitHub-Copilot"));
    }

    #[test]
    fn compose_proxy_tools_selection_error_falls_back_when_non_strict() {
        let mut config = Config::default();
        config.mcp_client.enabled = true;
        config.mcp_client.proxy_enabled = true;
        config.mcp_client.proxy_strict = false;
        config.mcp_client.proxy_fallback_to_local = true;
        config.mcp_client.include_default_paths = false;
        config.mcp_client.proxy_mount_all_discovered = false;
        config.mcp_client.proxy_servers = vec!["missing".to_string()];

        let builder = Server::new("test", "0.0.0");
        let result = compose_proxy_tools(builder, &config, None);
        assert!(result.is_ok());
    }

    #[test]
    fn compose_proxy_tools_selection_error_fails_when_strict() {
        let mut config = Config::default();
        config.mcp_client.enabled = true;
        config.mcp_client.proxy_enabled = true;
        config.mcp_client.proxy_strict = true;
        config.mcp_client.proxy_fallback_to_local = false;
        config.mcp_client.include_default_paths = false;
        config.mcp_client.proxy_mount_all_discovered = false;
        config.mcp_client.proxy_servers = vec!["missing".to_string()];

        let builder = Server::new("test", "0.0.0");
        let result = compose_proxy_tools(builder, &config, None);
        assert!(result.is_err());
    }

    // ========================================================================
    // sanitize_prefix_segment edge cases
    // ========================================================================

    #[test]
    fn sanitize_prefix_segment_empty_returns_server() {
        assert_eq!(sanitize_prefix_segment(""), "server");
    }

    #[test]
    fn sanitize_prefix_segment_whitespace_only_returns_server() {
        assert_eq!(sanitize_prefix_segment("   "), "server");
    }

    #[test]
    fn sanitize_prefix_segment_special_chars_replaced() {
        assert_eq!(sanitize_prefix_segment("my.server@v2"), "my-server-v2");
    }

    #[test]
    fn sanitize_prefix_segment_preserves_hyphens_and_underscores() {
        assert_eq!(sanitize_prefix_segment("my-server_v2"), "my-server_v2");
    }

    #[test]
    fn sanitize_prefix_segment_lowercases() {
        assert_eq!(sanitize_prefix_segment("MyServer"), "myserver");
    }

    #[test]
    fn sanitize_prefix_segment_trims_leading_trailing_hyphens() {
        // Special chars at boundaries become hyphens, which are trimmed
        assert_eq!(sanitize_prefix_segment("..name.."), "name");
    }

    // ========================================================================
    // filter_remote_tools edge cases
    // ========================================================================

    #[test]
    fn filter_remote_tools_allows_mutating_when_configured() {
        // br-ft-gmt1c: `proxy_allow_mutating_tools` admits tools
        // with EXPLICIT destructive/mutating annotations, but NOT
        // tools with missing/malformed annotation metadata. The
        // operator can't consent to admit unsafe metadata they
        // can't see.
        let mut settings = McpClientConfig::default();
        settings.proxy_allow_mutating_tools = true;

        let tools = vec![
            // Explicit destructive: admitted under the opt-in.
            McpClientToolDefinition {
                name: "drop_db".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: Some(serde_json::json!({"destructive": true})),
            },
            // Explicit mutating: admitted under the opt-in.
            McpClientToolDefinition {
                name: "write_file".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: Some(serde_json::json!({"readOnly": false})),
            },
            // Tool with no safety problem (readOnly:true) — passes.
            McpClientToolDefinition {
                name: "list_tables".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: Some(serde_json::json!({"readOnly": true})),
            },
        ];

        let filtered = locked_filter_remote_tools(&settings, tools);
        let names: Vec<&str> = filtered.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"drop_db"), "explicit destructive admitted");
        assert!(names.contains(&"write_file"), "explicit mutating admitted");
        assert!(names.contains(&"list_tables"), "safe tool admitted");
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn filter_remote_tools_still_blocks_missing_annotations_under_opt_in_ft_gmt1c() {
        // br-ft-gmt1c: even with proxy_allow_mutating_tools=true,
        // a tool with annotations=None must remain blocked. The
        // operator cannot consent to admit unsafe metadata they
        // can't see.
        let mut settings = McpClientConfig::default();
        settings.proxy_allow_mutating_tools = true;

        let tools = vec![McpClientToolDefinition {
            name: "no_annotations".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: Vec::new(),
            annotations: None,
        }];

        let filtered = locked_filter_remote_tools(&settings, tools);
        assert!(
            filtered.is_empty(),
            "missing-annotations tools must remain blocked even with opt-in"
        );
    }

    #[test]
    fn filter_remote_tools_still_blocks_malformed_annotations_under_opt_in_ft_gmt1c() {
        // br-ft-gmt1c: a tool with annotations that are not a JSON
        // object (e.g. an array, a string, a number) must remain
        // blocked even when the operator opted into mutating tools.
        let mut settings = McpClientConfig::default();
        settings.proxy_allow_mutating_tools = true;

        let tools = vec![McpClientToolDefinition {
            name: "weird_shape".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: Vec::new(),
            annotations: Some(serde_json::json!(["not", "an", "object"])),
        }];

        let filtered = locked_filter_remote_tools(&settings, tools);
        assert!(
            filtered.is_empty(),
            "malformed-annotations tools must remain blocked even with opt-in"
        );
    }

    #[test]
    fn filter_remote_tools_empty_input() {
        let settings = McpClientConfig::default();
        let filtered = locked_filter_remote_tools(&settings, Vec::new());
        assert!(filtered.is_empty());
    }

    // ========================================================================
    // select_proxy_servers edge cases
    // ========================================================================

    #[test]
    fn select_proxy_servers_deduplicates_case_insensitive() {
        let settings = McpClientConfig {
            proxy_servers: vec!["Morph".to_string(), "morph".to_string()],
            ..McpClientConfig::default()
        };
        let discovered = vec![make_server("Morph", false)];
        let selected = select_proxy_servers(&settings, &discovered).unwrap();
        assert_eq!(selected.len(), 1, "duplicate names should be deduped");
    }

    #[test]
    fn select_proxy_servers_skips_disabled_in_mount_all() {
        let settings = McpClientConfig {
            proxy_mount_all_discovered: true,
            ..McpClientConfig::default()
        };
        let discovered = vec![
            make_server("enabled-one", false),
            make_server("disabled-one", true),
            make_server("enabled-two", false),
        ];
        let selected = select_proxy_servers(&settings, &discovered).unwrap();
        assert_eq!(selected.len(), 2);
        assert!(selected.iter().all(|s| !s.disabled));
    }

    #[test]
    fn select_proxy_servers_preferred_falls_back_to_discovered() {
        let settings = McpClientConfig {
            proxy_mount_all_discovered: false,
            preferred_servers: vec!["alpha".to_string(), "beta".to_string()],
            ..McpClientConfig::default()
        };
        let discovered = vec![
            make_server("alpha", false),
            make_server("beta", false),
            make_server("gamma", false),
        ];
        let selected = select_proxy_servers(&settings, &discovered).unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].name, "alpha");
        assert_eq!(selected[1].name, "beta");
    }

    #[test]
    fn select_proxy_servers_no_config_returns_error() {
        let settings = McpClientConfig {
            proxy_mount_all_discovered: false,
            preferred_servers: Vec::new(),
            proxy_servers: Vec::new(),
            ..McpClientConfig::default()
        };
        let discovered = vec![make_server("server-a", false)];
        let result = select_proxy_servers(&settings, &discovered);
        assert!(result.is_err());
    }

    // ========================================================================
    // insert_route_prefix
    // ========================================================================

    #[test]
    fn insert_route_prefix_first_insert_succeeds() {
        let mut used = HashSet::new();
        assert!(insert_route_prefix(&mut used, "remote/my-tool"));
    }

    #[test]
    fn insert_route_prefix_duplicate_returns_false() {
        let mut used = HashSet::new();
        insert_route_prefix(&mut used, "remote/my-tool");
        assert!(!insert_route_prefix(&mut used, "remote/my-tool"));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn prop_sanitize_prefix_segment_output_is_lowercase_and_bounded(
            raw in "[A-Za-z0-9 _./@:-]{0,48}",
        ) {
            let sanitized = sanitize_prefix_segment(&raw);
            prop_assert!(!sanitized.is_empty());
            prop_assert_eq!(&sanitized, &sanitized.to_ascii_lowercase());
            prop_assert!(sanitized.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'));
        }

        #[test]
        fn prop_insert_route_prefix_is_case_insensitive_once(
            segment in "[A-Za-z0-9/_-]{1,32}",
        ) {
            let lower = segment.to_ascii_lowercase();
            let upper = segment.to_ascii_uppercase();
            let mut used = HashSet::new();

            prop_assert!(insert_route_prefix(&mut used, &lower));
            prop_assert!(!insert_route_prefix(&mut used, &upper));
            prop_assert_eq!(used.len(), 1);
        }

        #[test]
        fn prop_filter_remote_tools_matches_destructive_policy(
            safe_name in "[A-Za-z0-9_.-]{1,16}",
            destructive_name in "[A-Za-z0-9_.-]{1,16}",
        ) {
            prop_assume!(safe_name != destructive_name);

            let safe = McpClientToolDefinition {
                name: safe_name.clone(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: Some(serde_json::json!({"destructive": false})),
            };
            let destructive = McpClientToolDefinition {
                name: destructive_name.clone(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: Some(serde_json::json!({"destructive": true})),
            };

            let mut settings = McpClientConfig::default();
            settings.proxy_allow_mutating_tools = false;
            let filtered = locked_filter_remote_tools(&settings, vec![safe.clone(), destructive.clone()]);
            prop_assert_eq!(filtered.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec![safe_name.as_str()]);

            settings.proxy_allow_mutating_tools = true;
            let unfiltered = locked_filter_remote_tools(&settings, vec![safe, destructive]);
            prop_assert_eq!(unfiltered.len(), 2);
        }

        #[test]
        fn prop_filter_remote_tools_blocks_mutating_annotations_by_default(
            name in "[A-Za-z0-9_.-]{1,16}",
            use_hint in any::<bool>(),
            destructive_hint in any::<bool>(),
        ) {
            let annotations = if destructive_hint {
                serde_json::json!({
                    "destructiveHint": true,
                    "readOnly": true,
                })
            } else if use_hint {
                serde_json::json!({
                    "destructive": false,
                    "readOnlyHint": false,
                })
            } else {
                serde_json::json!({
                    "destructive": false,
                    "readOnly": false,
                })
            };
            let tool = McpClientToolDefinition {
                name: name.clone(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: Some(annotations),
            };

            let mut settings = McpClientConfig::default();
            settings.proxy_allow_mutating_tools = false;
            prop_assert!(locked_filter_remote_tools(&settings, vec![tool.clone()]).is_empty());

            settings.proxy_allow_mutating_tools = true;
            let unfiltered = locked_filter_remote_tools(&settings, vec![tool]);
            prop_assert_eq!(unfiltered.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec![name.as_str()]);
        }

        #[test]
        fn prop_destructive_counter_delta_is_serialized_by_test_lock(count in 0usize..=16) {
            let _guard = proxy_counter_test_lock();
            reset_mcp_proxy_destructive_filtered_count_for_test();
            let settings = McpClientConfig {
                enabled: true,
                proxy_enabled: true,
                proxy_allow_mutating_tools: false,
                ..McpClientConfig::default()
            };
            let tools: Vec<McpClientToolDefinition> = (0..count)
                .map(|idx| McpClientToolDefinition {
                    name: format!("drop_db_{idx}"),
                    description: None,
                    input_schema: serde_json::json!({"type": "object"}),
                    output_schema: None,
                    icon: None,
                    version: None,
                    tags: Vec::new(),
                    annotations: Some(serde_json::json!({"destructive": true})),
                })
                .collect();

            let filtered = filter_remote_tools(&settings, tools);

            prop_assert!(filtered.is_empty());
            prop_assert_eq!(mcp_proxy_destructive_filtered_count(), count as u64);
        }
    }

    // ========================================================================
    // br-ft-8na0z: mcp_proxy partial-mount failure counter.
    //
    // Counter is process-wide; tests serialize via a Mutex guard so
    // concurrent execution doesn't race on the global state.
    // ========================================================================

    fn proxy_counter_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn locked_filter_remote_tools(
        settings: &McpClientConfig,
        tools: Vec<McpClientToolDefinition>,
    ) -> Vec<McpClientToolDefinition> {
        let _guard = proxy_counter_test_lock();
        filter_remote_tools(settings, tools)
    }

    #[test]
    fn mcp_proxy_mount_failure_counter_starts_at_zero_after_reset() {
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_mount_failure_count_for_test();
        assert_eq!(super::mcp_proxy_mount_failure_count(), 0);
    }

    #[test]
    fn mcp_proxy_mount_failure_counter_increments_per_helper_call() {
        // Direct test of the helper invoked at the six silent-skip
        // call sites (connect, list_tools, post-filter empty,
        // per-tool mapping, post-mapping empty, route collision).
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_mount_failure_count_for_test();
        super::record_mcp_proxy_mount_failure();
        assert_eq!(super::mcp_proxy_mount_failure_count(), 1);
        super::record_mcp_proxy_mount_failure();
        super::record_mcp_proxy_mount_failure();
        super::record_mcp_proxy_mount_failure();
        super::record_mcp_proxy_mount_failure();
        super::record_mcp_proxy_mount_failure();
        // 6 sites × 1 server with all silent failures = 6 bumps.
        assert_eq!(super::mcp_proxy_mount_failure_count(), 6);
    }

    #[test]
    fn mcp_proxy_mount_failure_counter_unchanged_when_proxy_disabled() {
        // Negative test: compose_proxy_tools with
        // `proxy_enabled=false` returns early before any
        // silent-skip site can fire. Counter must remain at 0.
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_mount_failure_count_for_test();

        let mut config = Config::default();
        config.mcp_client.proxy_enabled = false;
        let builder = crate::mcp_framework::framework_server_builder("test", "0.0.0");
        let _ = compose_proxy_tools(builder, &config, None).expect("disabled proxy must succeed");
        assert_eq!(
            super::mcp_proxy_mount_failure_count(),
            0,
            "ft-8na0z: proxy_enabled=false short-circuits before silent-skip sites; \
             counter must stay zero"
        );
    }

    /// [ft-59hlx] Site #A: proxy_enabled=true with mcp_client.enabled=false
    /// is a soft-fallback early-exit that previously bypassed the counter.
    /// Verify the counter now bumps on this pre-loop path.
    #[test]
    fn mcp_proxy_mount_failure_counter_bumps_on_client_disabled_mismatch() {
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_mount_failure_count_for_test();

        let mut config = Config::default();
        config.mcp_client.proxy_enabled = true;
        config.mcp_client.enabled = false;
        // Soft-fallback (default): proxy_strict=false AND fallback_to_local=true.
        let builder = crate::mcp_framework::framework_server_builder("test", "0.0.0");
        let _ = compose_proxy_tools(builder, &config, None).expect("soft-fallback must succeed");

        assert_eq!(
            super::mcp_proxy_mount_failure_count(),
            1,
            "ft-59hlx: proxy_enabled+!enabled mismatch is a pre-loop \
             silent-skip; counter must bump exactly once"
        );
    }

    /// [ft-59hlx] Site #D: proxy_enabled with empty proxy_servers list AND
    /// proxy_mount_all_discovered=false produces an empty selected vec —
    /// [ft-59hlx] Site #D: proxy_enabled with empty proxy_servers list AND
    /// proxy_mount_all_discovered=false produces an empty selected vec —
    /// another pre-loop early-exit that previously bypassed the counter.
    ///
    /// br-ft-eljxp: a Some(db_path) is now required to reach the
    /// empty-selection path; the new no-db gate (line ~252) refuses
    /// proxy composition before discovery/selection when db_path is
    /// None. Pass a tempdir path so this test still exercises the
    /// ft-59hlx site #D rather than the ft-eljxp gate.
    #[test]
    fn mcp_proxy_mount_failure_counter_bumps_on_empty_selection() {
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_mount_failure_count_for_test();

        let mut config = Config::default();
        config.mcp_client.enabled = true;
        config.mcp_client.proxy_enabled = true;
        config.mcp_client.proxy_servers = Vec::new();
        config.mcp_client.proxy_mount_all_discovered = false;
        // Soft-fallback (default).
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = std::sync::Arc::new(dir.path().join("ft-eljxp-test.db"));
        let builder = crate::mcp_framework::framework_server_builder("test", "0.0.0");
        let _ = compose_proxy_tools(builder, &config, Some(db_path))
            .expect("soft-fallback must succeed");

        assert_eq!(
            super::mcp_proxy_mount_failure_count(),
            1,
            "ft-59hlx: empty selection is a pre-loop silent-skip; \
             counter must bump exactly once"
        );
    }

    /// [ft-59hlx] Multiple early-exit invocations accumulate. Run two
    /// distinct pre-loop early-exit paths back-to-back and assert the
    /// counter records both events.
    ///
    /// br-ft-eljxp: site #D requires Some(db_path) to bypass the new
    /// no-db gate; site #A (client-disabled) fires before the no-db
    /// gate per the documented order, so it can stay None.
    #[test]
    fn mcp_proxy_mount_failure_counter_accumulates_across_pre_loop_paths() {
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_mount_failure_count_for_test();

        // Site #A — client-disabled mismatch (fires before the
        // no-db gate; db_path can stay None).
        let mut config_a = Config::default();
        config_a.mcp_client.proxy_enabled = true;
        config_a.mcp_client.enabled = false;
        let builder_a = crate::mcp_framework::framework_server_builder("a", "0.0.0");
        let _ = compose_proxy_tools(builder_a, &config_a, None).expect("a");

        // Site #D — empty selection (requires Some(db_path) to
        // bypass the new br-ft-eljxp no-db gate).
        let mut config_d = Config::default();
        config_d.mcp_client.enabled = true;
        config_d.mcp_client.proxy_enabled = true;
        config_d.mcp_client.proxy_servers = Vec::new();
        config_d.mcp_client.proxy_mount_all_discovered = false;
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = std::sync::Arc::new(dir.path().join("ft-eljxp-test.db"));
        let builder_d = crate::mcp_framework::framework_server_builder("d", "0.0.0");
        let _ = compose_proxy_tools(builder_d, &config_d, Some(db_path)).expect("d");

        assert_eq!(
            super::mcp_proxy_mount_failure_count(),
            2,
            "ft-59hlx: pre-loop silent-skips accumulate (one per soft-fallback \
             event); counter == sum of all pre-loop short-circuits"
        );
    }

    // ========================================================================
    // br-ft-wzk10: mcp_proxy per-call dispatch-failure counter
    //
    // Distinct from MCP_PROXY_MOUNT_FAILURES (compose-time) and
    // MCP_PROXY_DESTRUCTIVE_FILTERED (compose-time tool filter).
    // This counter tracks RUNTIME per-call dispatch failures across
    // four sites in RemoteProxyToolHandler::call:
    //   C — pre-flight Cx checkpoint failed
    //   D — per-server Mutex<FtMcpClient> poisoned
    //   E — call_tool returned Err from remote
    //   F — content decode mapping failed
    //
    // The simplest unit-level pin exercises the helper directly;
    // full integration tests for sites C/D/E/F require mocked Cx
    // cancellation, panic-injection, and fastmcp Client fixtures
    // respectively. The helper exhaustiveness test below is the
    // load-bearing assertion that the counter substrate is sound.
    // ========================================================================

    #[test]
    fn mcp_proxy_call_dispatch_failure_counter_starts_at_zero_after_reset() {
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_call_dispatch_failure_count_for_test();
        assert_eq!(super::mcp_proxy_call_dispatch_failure_count(), 0);
    }

    #[test]
    fn mcp_proxy_call_dispatch_failure_counter_increments_per_helper_call() {
        // Direct test of the helper invoked at the four soft-block
        // call sites in RemoteProxyToolHandler::call (C/D/E/F).
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_call_dispatch_failure_count_for_test();

        // Simulate four dispatch failures across the four sites.
        super::record_mcp_proxy_call_dispatch_failure();
        super::record_mcp_proxy_call_dispatch_failure();
        super::record_mcp_proxy_call_dispatch_failure();
        super::record_mcp_proxy_call_dispatch_failure();

        assert_eq!(
            super::mcp_proxy_call_dispatch_failure_count(),
            4,
            "br-ft-wzk10: each helper call must bump the counter by exactly 1; \
             4 calls (one per soft-block site C/D/E/F) → counter == 4"
        );
    }

    #[test]
    fn mcp_proxy_call_dispatch_failure_counter_independent_from_mount_and_destructive_counters() {
        // Pin counter independence: bumping one of the three proxy
        // counters must not affect the other two.
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_mount_failure_count_for_test();
        super::reset_mcp_proxy_destructive_filtered_count_for_test();
        super::reset_mcp_proxy_call_dispatch_failure_count_for_test();

        super::record_mcp_proxy_call_dispatch_failure();
        super::record_mcp_proxy_call_dispatch_failure();

        assert_eq!(super::mcp_proxy_call_dispatch_failure_count(), 2);
        assert_eq!(
            super::mcp_proxy_mount_failure_count(),
            0,
            "br-ft-wzk10: dispatch-failure bumps must NOT spill into \
             the compose-time mount-failure counter"
        );
        assert_eq!(
            super::mcp_proxy_destructive_filtered_count(),
            0,
            "br-ft-wzk10: dispatch-failure bumps must NOT spill into \
             the compose-time destructive-filter counter"
        );
    }

    // ── br-ft-eljxp: degraded-bridge cross-module fail-closed gate ────

    /// br-ft-eljxp: in non-strict mode, `compose_proxy_tools` with
    /// `proxy_enabled = true` and `db_path = None` must skip the
    /// composition entirely (returning the unchanged builder) and
    /// bump the dedicated unaudited-degraded skip counter.
    /// Pre-fix the per-server mount loop's audit-wrap branch
    /// silently fell back to FormatAwareToolHandler::new(handler)
    /// — bypassing AuditedToolHandler — when db_path was None.
    #[test]
    fn compose_proxy_tools_no_db_soft_skips_with_counter_bump_ft_eljxp() {
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_unaudited_degraded_skip_count_for_test();

        let mut config = Config::default();
        config.mcp_client.enabled = true;
        config.mcp_client.proxy_enabled = true;
        config.mcp_client.proxy_strict = false;
        config.mcp_client.proxy_fallback_to_local = true;
        // Even with discoverable servers, the gate fires before
        // discover_servers is called.
        config.mcp_client.proxy_servers = vec!["alpha".to_string()];

        let builder = crate::mcp_framework::framework_server_builder("test", "0.0.0");
        let _ = compose_proxy_tools(builder, &config, None)
            .expect("non-strict no-db must soft-skip with Ok");

        assert_eq!(
            super::mcp_proxy_unaudited_degraded_skip_count(),
            1,
            "ft-eljxp: non-strict no-db path must bump the dedicated counter exactly once"
        );
    }

    /// br-ft-eljxp: in strict mode (proxy_strict OR
    /// !proxy_fallback_to_local), `compose_proxy_tools` with
    /// `proxy_enabled = true` and `db_path = None` must return
    /// Err. Counter does NOT bump on the strict path — strict
    /// mode is a config validation error, not a runtime soft-skip.
    #[test]
    fn compose_proxy_tools_no_db_strict_returns_err_ft_eljxp() {
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_unaudited_degraded_skip_count_for_test();

        let mut config = Config::default();
        config.mcp_client.enabled = true;
        config.mcp_client.proxy_enabled = true;
        config.mcp_client.proxy_strict = true;
        config.mcp_client.proxy_fallback_to_local = false;

        let builder = crate::mcp_framework::framework_server_builder("test", "0.0.0");
        let result = compose_proxy_tools(builder, &config, None);
        let err = match result {
            Ok(_) => panic!("strict no-db must return Err"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("br-ft-eljxp"),
            "ft-eljxp: error must reference the bead breadcrumb; got {msg}"
        );
        assert!(
            msg.contains("audit db_path"),
            "ft-eljxp: error must explain the audit gap; got {msg}"
        );
        assert_eq!(
            super::mcp_proxy_unaudited_degraded_skip_count(),
            0,
            "ft-eljxp: strict path returns Err; counter must NOT bump"
        );
    }

    /// br-ft-eljxp: with `proxy_enabled = false`, the no-db gate
    /// is unreachable (the early proxy_enabled check returns Ok
    /// first). Counter must stay zero. Pins the gate ordering
    /// against accidental drift.
    #[test]
    fn compose_proxy_tools_no_db_with_proxy_disabled_does_not_bump_ft_eljxp() {
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_unaudited_degraded_skip_count_for_test();

        let mut config = Config::default();
        config.mcp_client.enabled = true;
        config.mcp_client.proxy_enabled = false;

        let builder = crate::mcp_framework::framework_server_builder("test", "0.0.0");
        let _ = compose_proxy_tools(builder, &config, None).expect("disabled proxy is always Ok");

        assert_eq!(
            super::mcp_proxy_unaudited_degraded_skip_count(),
            0,
            "ft-eljxp: proxy_enabled=false short-circuits before the no-db gate; \
             counter must stay zero"
        );
    }

    /// br-ft-eljxp: with `proxy_enabled = true` and Some(db_path),
    /// the no-db gate does NOT fire. Counter must stay zero.
    /// Round-trip pin: full-mode bridges must reach discovery /
    /// selection / mount paths normally.
    #[test]
    fn compose_proxy_tools_with_db_path_does_not_bump_no_db_counter_ft_eljxp() {
        let _guard = proxy_counter_test_lock();
        super::reset_mcp_proxy_unaudited_degraded_skip_count_for_test();

        let mut config = Config::default();
        config.mcp_client.enabled = true;
        config.mcp_client.proxy_enabled = true;
        // Empty selection path is fine — we only care that the
        // no-db gate doesn't fire when db_path is Some.
        config.mcp_client.proxy_servers = Vec::new();
        config.mcp_client.proxy_mount_all_discovered = false;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = std::sync::Arc::new(dir.path().join("ft-eljxp-test.db"));

        let builder = crate::mcp_framework::framework_server_builder("test", "0.0.0");
        let _ = compose_proxy_tools(builder, &config, Some(db_path))
            .expect("Some(db_path) must reach the empty-selection soft path");

        assert_eq!(
            super::mcp_proxy_unaudited_degraded_skip_count(),
            0,
            "ft-eljxp: Some(db_path) must NOT bump the no-db counter \
             (gate is gated on db_path.is_none())"
        );
    }

    /// br-ft-eljxp: property-style sweep over the four corners of
    /// (proxy_enabled, db_path) × (strict, soft). Pins the gate
    /// behavior across the entire 4-way matrix:
    ///   - proxy_enabled=false × any: Ok, no counter bump
    ///   - proxy_enabled=true × Some(db): Ok, no counter bump
    ///   - proxy_enabled=true × None × strict: Err, no counter bump
    ///   - proxy_enabled=true × None × soft: Ok, +1 counter bump
    #[test]
    fn compose_proxy_tools_no_db_gate_property_sweep_ft_eljxp() {
        for &proxy_enabled in &[false, true] {
            for &has_db in &[false, true] {
                for &strict in &[false, true] {
                    let _guard = proxy_counter_test_lock();
                    super::reset_mcp_proxy_unaudited_degraded_skip_count_for_test();

                    let mut config = Config::default();
                    config.mcp_client.enabled = true;
                    config.mcp_client.proxy_enabled = proxy_enabled;
                    config.mcp_client.proxy_strict = strict;
                    config.mcp_client.proxy_fallback_to_local = !strict;

                    let dir = tempfile::tempdir().expect("tempdir");
                    let db_path = if has_db {
                        Some(std::sync::Arc::new(dir.path().join("test.db")))
                    } else {
                        None
                    };

                    let builder = crate::mcp_framework::framework_server_builder("sweep", "0.0.0");
                    let result = compose_proxy_tools(builder, &config, db_path);
                    let bump = super::mcp_proxy_unaudited_degraded_skip_count();

                    match (proxy_enabled, has_db, strict) {
                        (false, _, _) => {
                            assert!(
                                result.is_ok(),
                                "ft-eljxp matrix({proxy_enabled},{has_db},{strict}): proxy_enabled=false must be Ok"
                            );
                            assert_eq!(bump, 0, "ft-eljxp: no bump when proxy disabled");
                        }
                        (true, true, _) => {
                            // The soft empty-selection path may bump
                            // mount_failure but NOT the no-db counter.
                            assert!(
                                result.is_ok() || result.is_err(),
                                "ft-eljxp matrix({proxy_enabled},{has_db},{strict}): outcome depends on selection; bypass-irrelevant"
                            );
                            assert_eq!(bump, 0, "ft-eljxp: Some(db) never bumps no-db counter");
                        }
                        (true, false, true) => {
                            assert!(
                                result.is_err(),
                                "ft-eljxp matrix({proxy_enabled},{has_db},{strict}): no-db strict must be Err"
                            );
                            assert_eq!(bump, 0, "ft-eljxp: strict Err does NOT bump counter");
                        }
                        (true, false, false) => {
                            assert!(
                                result.is_ok(),
                                "ft-eljxp matrix({proxy_enabled},{has_db},{strict}): no-db soft must be Ok"
                            );
                            assert_eq!(bump, 1, "ft-eljxp: soft no-db bumps counter once");
                        }
                    }
                }
            }
        }
    }
}
