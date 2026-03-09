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

/// Build structured `DecisionContext` metadata for MCP audit entries.
fn mcp_audit_decision_context(
    tool_name: &str,
    action_kind: &str,
    decision: &str,
    result: &str,
    error_code: Option<&str>,
    elapsed_ms: u64,
) -> Option<String> {
    let mut context = DecisionContext::new_audit(
        i64::try_from(now_ms()).unwrap_or(0),
        ActionKind::ExecCommand,
        ActorKind::Mcp,
        PolicySurface::Mcp,
        None,
        None,
        Some(format!("mcp audit for {tool_name}")),
        None,
    );
    let determining_rule = format!("audit.{action_kind}");
    context.record_rule(
        &determining_rule,
        true,
        Some(decision),
        Some(format!("MCP tool audit recorded with result={result}")),
    );
    context.set_determining_rule(&determining_rule);
    context.add_evidence("stage", "mcp_audit");
    context.add_evidence("tool", tool_name);
    context.add_evidence("mcp_action_kind", action_kind);
    context.add_evidence("mcp_surface", PolicySurface::Mcp.as_str());
    context.add_evidence("policy_decision", decision);
    context.add_evidence("result", result);
    context.add_evidence("elapsed_ms", elapsed_ms.to_string());
    if let Some(error_code) = error_code {
        context.add_evidence("error_code", error_code);
    }
    serde_json::to_string(&context).ok()
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
    let action_kind = format!("mcp.{tool_name}");
    let audit = crate::storage::AuditActionRecord {
        id: 0,
        ts,
        actor_kind: "mcp".to_string(),
        actor_id: None,
        correlation_id: None,
        pane_id: None,
        domain: None,
        action_kind: action_kind.clone(),
        policy_decision: decision.to_string(),
        decision_reason: error_code.map(|c| format!("error_code={c}")),
        rule_id: None,
        input_summary: Some(format!("{input_summary} elapsed_ms={elapsed_ms}")),
        verification_summary: None,
        decision_context: mcp_audit_decision_context(
            tool_name,
            &action_kind,
            decision,
            result,
            error_code,
            elapsed_ms,
        ),
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
    db_path: &Path,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    // ========================================================================
    // check_refresh_cooldown Tests
    // ========================================================================

    #[test]
    fn cooldown_not_triggered_when_no_prior_refresh() {
        // most_recent_refresh_ms <= 0 means no prior refresh
        assert_eq!(check_refresh_cooldown(0, 100_000, 30_000), None);
        assert_eq!(check_refresh_cooldown(-1, 100_000, 30_000), None);
    }

    #[test]
    fn cooldown_triggered_within_period() {
        // Refreshed 10 seconds ago, cooldown is 30 seconds
        let result = check_refresh_cooldown(90_000, 100_000, 30_000);
        assert!(result.is_some());
        let (elapsed_s, remaining_s) = result.unwrap();
        assert_eq!(elapsed_s, 10); // 10_000ms / 1000
        assert_eq!(remaining_s, 20); // (30_000 - 10_000) / 1000
    }

    #[test]
    fn cooldown_not_triggered_after_period() {
        // Refreshed 60 seconds ago, cooldown is 30 seconds
        assert_eq!(check_refresh_cooldown(40_000, 100_000, 30_000), None);
    }

    #[test]
    fn cooldown_boundary_exact_expiry() {
        // Refreshed exactly cooldown_ms ago — should not trigger
        assert_eq!(check_refresh_cooldown(70_000, 100_000, 30_000), None);
    }

    #[test]
    fn cooldown_just_before_expiry() {
        // Refreshed 29_999ms ago, cooldown is 30_000ms — still in cooldown
        let result = check_refresh_cooldown(70_001, 100_000, 30_000);
        assert!(result.is_some());
    }

    // ========================================================================
    // redact_mcp_args Tests
    // ========================================================================

    #[test]
    fn redact_args_empty_object() {
        let args = serde_json::json!({});
        assert_eq!(redact_mcp_args("wa.pane.list", &args), "mcp:wa.pane.list");
    }

    #[test]
    fn redact_args_with_keys() {
        let args = serde_json::json!({"pane_id": 42, "text": "secret"});
        let result = redact_mcp_args("wa.pane.send_text", &args);
        assert!(result.starts_with("mcp:wa.pane.send_text keys=["));
        assert!(result.contains("pane_id"));
        assert!(result.contains("text"));
        // Values should NOT appear
        assert!(!result.contains("42"));
        assert!(!result.contains("secret"));
    }

    #[test]
    fn redact_args_non_object() {
        // Arrays and scalars have no keys
        let args = serde_json::json!([1, 2, 3]);
        assert_eq!(redact_mcp_args("wa.test", &args), "mcp:wa.test");

        let args = serde_json::json!("string");
        assert_eq!(redact_mcp_args("wa.test", &args), "mcp:wa.test");

        let args = serde_json::json!(null);
        assert_eq!(redact_mcp_args("wa.test", &args), "mcp:wa.test");
    }

    #[test]
    fn redact_args_single_key() {
        let args = serde_json::json!({"pane_id": 1});
        let result = redact_mcp_args("wa.pane.get", &args);
        assert_eq!(result, "mcp:wa.pane.get keys=[pane_id]");
    }

    // ========================================================================
    // resolve_alt_screen_state Tests
    // ========================================================================

    fn make_ipc_state(pane_id: u64, known: bool) -> IpcPaneState {
        IpcPaneState {
            pane_id,
            known,
            observed: None,
            alt_screen: None,
            last_status_at: None,
            in_gap: None,
            cursor_alt_screen: None,
            reason: None,
        }
    }

    #[test]
    fn alt_screen_unknown_state_returns_none() {
        let state = make_ipc_state(1, false);
        assert_eq!(resolve_alt_screen_state(&state), None);
    }

    #[test]
    fn alt_screen_cursor_alt_screen_preferred() {
        let mut state = make_ipc_state(1, true);
        state.cursor_alt_screen = Some(true);
        state.alt_screen = Some(false); // cursor_alt_screen should take precedence
        state.last_status_at = Some(100);
        assert_eq!(resolve_alt_screen_state(&state), Some(true));
    }

    #[test]
    fn alt_screen_fallback_to_alt_screen_field() {
        let mut state = make_ipc_state(1, true);
        state.alt_screen = Some(true);
        state.last_status_at = Some(100);
        assert_eq!(resolve_alt_screen_state(&state), Some(true));
    }

    #[test]
    fn alt_screen_no_status_at_returns_none() {
        let mut state = make_ipc_state(1, true);
        state.alt_screen = Some(true);
        // No last_status_at → should return None
        assert_eq!(resolve_alt_screen_state(&state), None);
    }

    // ========================================================================
    // effective_search_fusion_weights Tests
    // ========================================================================

    #[test]
    fn fusion_weights_fast_only() {
        let mut config = Config::default();
        config.search.fast_only = true;
        let (lexical, semantic) = effective_search_fusion_weights(&config);
        assert!((lexical - 1.0).abs() < f32::EPSILON);
        assert!((semantic - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn fusion_weights_default() {
        let config = Config::default();
        let (lexical, semantic) = effective_search_fusion_weights(&config);
        // Default quality_weight should be between 0 and 1
        assert!(lexical >= 0.0 && lexical <= 1.0);
        assert!(semantic >= 0.0 && semantic <= 1.0);
        assert!((lexical + semantic - 1.0).abs() < 0.01);
    }

    #[test]
    fn fusion_weights_nan_fallback() {
        let mut config = Config::default();
        config.search.quality_weight = f64::NAN;
        let (lexical, semantic) = effective_search_fusion_weights(&config);
        // NaN should fall back to 0.7 quality_weight
        assert!((semantic - 0.7).abs() < 0.01);
        assert!((lexical - 0.3).abs() < 0.01);
    }

    #[test]
    fn fusion_weights_clamped_high() {
        let mut config = Config::default();
        config.search.quality_weight = 5.0; // Above 1.0
        let (lexical, semantic) = effective_search_fusion_weights(&config);
        assert!((semantic - 1.0).abs() < f32::EPSILON);
        assert!((lexical - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn fusion_weights_clamped_low() {
        let mut config = Config::default();
        config.search.quality_weight = -1.0; // Below 0.0
        let (lexical, semantic) = effective_search_fusion_weights(&config);
        assert!((semantic - 0.0).abs() < f32::EPSILON);
        assert!((lexical - 1.0).abs() < f32::EPSILON);
    }

    // ========================================================================
    // effective_search_rrf_k Tests
    // ========================================================================

    #[test]
    fn rrf_k_minimum_is_one() {
        let mut config = Config::default();
        config.search.rrf_k = 0;
        assert_eq!(effective_search_rrf_k(&config), 1);
    }

    #[test]
    fn rrf_k_preserves_valid_value() {
        let mut config = Config::default();
        config.search.rrf_k = 60;
        assert_eq!(effective_search_rrf_k(&config), 60);
    }

    // ========================================================================
    // effective_search_quality_timeout_ms Tests
    // ========================================================================

    #[test]
    fn quality_timeout_minimum_is_one() {
        let mut config = Config::default();
        config.search.quality_timeout_ms = 0;
        assert_eq!(effective_search_quality_timeout_ms(&config), 1);
    }

    #[test]
    fn quality_timeout_preserves_valid_value() {
        let mut config = Config::default();
        config.search.quality_timeout_ms = 5000;
        assert_eq!(effective_search_quality_timeout_ms(&config), 5000);
    }

    // ========================================================================
    // policy_reason Tests
    // ========================================================================

    #[test]
    fn policy_reason_allow_returns_none() {
        let decision = PolicyDecision::Allow {
            rule_id: None,
            context: None,
        };
        assert_eq!(policy_reason(&decision), None);
    }

    #[test]
    fn policy_reason_deny_returns_reason() {
        let decision = PolicyDecision::Deny {
            reason: "rate limited".to_string(),
            rule_id: None,
            context: None,
        };
        assert_eq!(policy_reason(&decision), Some("rate limited"));
    }

    #[test]
    fn policy_reason_require_approval_returns_reason() {
        let decision = PolicyDecision::RequireApproval {
            reason: "dangerous command".to_string(),
            rule_id: None,
            approval: None,
            context: None,
        };
        assert_eq!(policy_reason(&decision), Some("dangerous command"));
    }

    // ========================================================================
    // approval_command Tests
    // ========================================================================

    #[test]
    fn approval_command_no_approval() {
        let decision = PolicyDecision::RequireApproval {
            reason: "test".to_string(),
            rule_id: None,
            approval: None,
            context: None,
        };
        assert_eq!(approval_command(&decision), None);
    }

    #[test]
    fn approval_command_with_approval() {
        let decision = PolicyDecision::RequireApproval {
            reason: "test".to_string(),
            rule_id: None,
            approval: Some(crate::policy::ApprovalRequest {
                allow_once_code: "ABC123".to_string(),
                allow_once_full_hash: "deadbeef".to_string(),
                expires_at: 9999999999,
                summary: "test approval".to_string(),
                command: "ft approve --pane 1".to_string(),
            }),
            context: None,
        };
        assert_eq!(
            approval_command(&decision),
            Some("ft approve --pane 1".to_string())
        );
    }

    #[test]
    fn approval_command_allow_returns_none() {
        let decision = PolicyDecision::Allow {
            rule_id: None,
            context: None,
        };
        assert_eq!(approval_command(&decision), None);
    }

    #[test]
    fn approval_command_deny_returns_none() {
        let decision = PolicyDecision::Deny {
            reason: "nope".to_string(),
            rule_id: None,
            context: None,
        };
        assert_eq!(approval_command(&decision), None);
    }

    // ========================================================================
    // elapsed_ms Tests
    // ========================================================================

    #[test]
    fn elapsed_ms_returns_positive() {
        let start = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let ms = elapsed_ms(start);
        assert!(ms >= 4, "should be at least ~5ms, got {ms}");
    }

    // ========================================================================
    // SEND_OSC_SEGMENT_LIMIT and MCP_REFRESH_COOLDOWN_MS
    // ========================================================================

    #[test]
    fn constants_have_expected_values() {
        assert_eq!(SEND_OSC_SEGMENT_LIMIT, 200);
        assert_eq!(MCP_REFRESH_COOLDOWN_MS, 30_000);
    }

    fn temp_db_path() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("mcp-helpers-audit.db");
        (dir, db_path)
    }

    fn latest_audit_action(db_path: &Path, action_kind: &str) -> crate::storage::AuditActionRecord {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy())
                .await
                .unwrap();
            let mut rows = storage
                .get_audit_actions(crate::storage::AuditQuery {
                    limit: Some(1),
                    action_kind: Some(action_kind.to_string()),
                    ..crate::storage::AuditQuery::default()
                })
                .await
                .unwrap();
            assert!(!rows.is_empty(), "missing audit row for {action_kind}");
            rows.remove(0)
        })
    }

    fn evidence<'a>(context: &'a DecisionContext, key: &str) -> Option<&'a str> {
        context
            .evidence
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.value.as_str())
    }

    #[test]
    fn mcp_helpers_audit_decision_context_tracks_surface_and_error_code() {
        let ctx_json = mcp_audit_decision_context(
            "wa.accounts_refresh",
            "mcp.wa.accounts_refresh",
            "deny",
            "error",
            Some("MCP_TIMEOUT"),
            77,
        )
        .expect("decision context should serialize");
        let ctx: DecisionContext =
            serde_json::from_str(&ctx_json).expect("decision context should parse");

        assert_eq!(ctx.action, ActionKind::ExecCommand);
        assert_eq!(ctx.actor, ActorKind::Mcp);
        assert_eq!(ctx.surface, PolicySurface::Mcp);
        assert_eq!(
            ctx.text_summary.as_deref(),
            Some("mcp audit for wa.accounts_refresh")
        );
        assert_eq!(ctx.capabilities, PaneCapabilities::default());
        assert_eq!(
            ctx.determining_rule.as_deref(),
            Some("audit.mcp.wa.accounts_refresh")
        );
        assert_eq!(evidence(&ctx, "stage"), Some("mcp_audit"));
        assert_eq!(evidence(&ctx, "tool"), Some("wa.accounts_refresh"));
        assert_eq!(
            evidence(&ctx, "mcp_action_kind"),
            Some("mcp.wa.accounts_refresh")
        );
        assert_eq!(evidence(&ctx, "policy_decision"), Some("deny"));
        assert_eq!(evidence(&ctx, "result"), Some("error"));
        assert_eq!(evidence(&ctx, "elapsed_ms"), Some("77"));
        assert_eq!(evidence(&ctx, "error_code"), Some("MCP_TIMEOUT"));
    }

    #[test]
    fn mcp_helpers_record_mcp_audit_persists_structured_decision_context() {
        let (_dir, db_path) = temp_db_path();
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage should initialize");
            record_mcp_audit(
                &storage,
                "wa.rules_list",
                "mcp:wa.rules_list".to_string(),
                "allow",
                "success",
                None,
                12,
            )
            .await;
        });

        let audit = latest_audit_action(&db_path, "mcp.wa.rules_list");
        assert_eq!(audit.actor_kind, "mcp");
        let context: DecisionContext = serde_json::from_str(
            audit
                .decision_context
                .as_deref()
                .expect("decision context should be recorded"),
        )
        .expect("decision context should parse");
        assert_eq!(context.action, ActionKind::ExecCommand);
        assert_eq!(context.actor, ActorKind::Mcp);
        assert_eq!(context.surface, PolicySurface::Mcp);
        assert_eq!(
            context.determining_rule.as_deref(),
            Some("audit.mcp.wa.rules_list")
        );
        assert_eq!(evidence(&context, "stage"), Some("mcp_audit"));
        assert_eq!(evidence(&context, "tool"), Some("wa.rules_list"));
        assert_eq!(
            evidence(&context, "mcp_action_kind"),
            Some("mcp.wa.rules_list")
        );
        assert_eq!(evidence(&context, "policy_decision"), Some("allow"));
        assert_eq!(evidence(&context, "result"), Some("success"));
        assert_eq!(evidence(&context, "elapsed_ms"), Some("12"));
    }
}
