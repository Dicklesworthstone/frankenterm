//! MCP utility and helper functions.
//!
//! Extracted from `mcp.rs` as part of Wave 4A migration (ft-1fv0u).

#[allow(clippy::wildcard_imports)]
use super::*;

// ── Search config helpers ──────────────────────────────────────────

pub(super) fn effective_search_rrf_k(config: &Config) -> u32 {
    config.search.rrf_k.max(1)
}

pub(super) fn effective_search_quality_timeout_ms(config: &Config) -> u64 {
    config.search.quality_timeout_ms.max(1)
}

pub(super) fn effective_search_fusion_weights(config: &Config) -> (f32, f32) {
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

pub(super) fn effective_search_fusion_backend(config: &Config) -> crate::search::FusionBackend {
    crate::search::FusionBackend::parse(&config.search.fusion_backend)
}

// ── Reservation helpers ────────────────────────────────────────────

/// Convert a PaneReservation to MCP info format
pub(super) fn reservation_to_mcp_info(r: &PaneReservation) -> McpReservationInfo {
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

// ── Constants ──────────────────────────────────────────────────────

pub(super) const SEND_OSC_SEGMENT_LIMIT: usize = 200;
pub(super) const MCP_REFRESH_COOLDOWN_MS: i64 = 30_000;

// ── Policy helpers ─────────────────────────────────────────────────

pub(super) fn build_policy_engine(config: &Config, require_prompt_active: bool) -> PolicyEngine {
    PolicyEngine::new(
        config.safety.rate_limit_per_pane,
        config.safety.rate_limit_global,
        require_prompt_active,
    )
    .with_command_gate_config(config.safety.command_gate.clone())
    .with_trauma_guard_enabled(config.safety.trauma_guard.enabled)
    .with_policy_rules(config.safety.rules.clone())
}

pub(super) fn injection_from_decision(
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

pub(super) fn policy_reason(decision: &PolicyDecision) -> Option<&str> {
    match decision {
        PolicyDecision::Deny { reason, .. } | PolicyDecision::RequireApproval { reason, .. } => {
            Some(reason)
        }
        PolicyDecision::Allow { .. } => None,
    }
}

pub(super) fn approval_command(decision: &PolicyDecision) -> Option<String> {
    match decision {
        PolicyDecision::RequireApproval {
            approval: Some(approval),
            ..
        } => Some(approval.command.clone()),
        _ => None,
    }
}

// ── Config/workspace helpers ───────────────────────────────────────

pub(super) fn resolve_workspace_id(config: &Config) -> Result<String> {
    let layout = config.workspace_layout(None)?;
    Ok(layout.root.to_string_lossy().to_string())
}

pub(super) fn parse_caut_service(service: &str) -> Option<CautService> {
    let normalized = service.trim();
    if let Some(parsed) = CautService::from_cli_input(normalized) {
        return Some(parsed);
    }
    let provider = AgentProvider::from_slug(normalized);
    CautService::from_provider(&provider)
}

pub(super) fn parse_cass_agent(agent: &str) -> Option<CassAgent> {
    CassAgent::from_slug(agent)
}

pub(super) fn check_refresh_cooldown(
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

// ── IPC / pane state helpers ───────────────────────────────────────

pub(super) async fn derive_osc_state_from_storage(
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
pub(super) async fn fetch_pane_state_from_ipc(
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
pub(super) async fn fetch_pane_state_from_ipc(
    _socket_path: &std::path::Path,
    _pane_id: u64,
) -> std::result::Result<Option<IpcPaneState>, String> {
    Err("IPC not supported on this platform".to_string())
}

pub(super) fn resolve_alt_screen_state(state: &IpcPaneState) -> Option<bool> {
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

pub(super) async fn resolve_pane_capabilities(
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

// ── Workflow helpers ───────────────────────────────────────────────

pub(super) fn register_builtin_workflows(runner: &WorkflowRunner, config: &Config) {
    for workflow in builtin_workflows(config) {
        runner.register_workflow(workflow);
    }
}

pub(super) fn builtin_workflows(config: &Config) -> Vec<Arc<dyn Workflow>> {
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

// ── MCP envelope / serialization helpers ───────────────────────────

pub(super) fn envelope_to_content<T: Serialize>(
    envelope: McpEnvelope<T>,
) -> McpResult<Vec<Content>> {
    let text = serde_json::to_string(&envelope)
        .map_err(|e| McpError::internal_error(format!("Serialize MCP response: {e}")))?;
    Ok(vec![Content::Text { text }])
}

pub(super) fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// ── Audit helpers ──────────────────────────────────────────────────

/// Build a redacted summary of MCP tool arguments (keys only, no values).
pub(super) fn redact_mcp_args(tool_name: &str, args: &serde_json::Value) -> String {
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
pub(super) async fn record_mcp_audit(
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
pub(super) fn record_mcp_audit_sync(
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
