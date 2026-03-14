//! Handler workflow implementations for session lifecycle and auth recovery.
//!
//! Includes HandleSessionEnd, HandleProcessTriageLifecycle, HandleAuthRequired,
//! HandleClaudeCodeLimits, HandleGeminiQuota, device auth integration,
//! session resume, and safe fallback path generation.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

// ============================================================================
// HandleSessionEnd — persist structured session summaries (wa-nu4.2.2.3)
// ============================================================================

/// Persist structured session summaries when agents emit session.summary or session.end events.
///
/// Supported agents: Codex, Claude Code, Gemini.
/// Extracts available fields (session_id, tokens, cost, end_reason) from the
/// detection trigger and upserts an `AgentSessionRecord`.
pub struct HandleSessionEnd;

fn parse_i64_field(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::String(raw) => parse_number(raw.trim()),
        serde_json::Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|v| i64::try_from(v).ok())),
        _ => None,
    }
}

fn parse_cost_usd_field(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::String(raw) => {
            let normalized = raw.trim().replace(',', "");
            if normalized.is_empty() {
                None
            } else {
                normalized.parse::<f64>().ok()
            }
        }
        serde_json::Value::Number(number) => number.as_f64(),
        _ => None,
    }
}

fn new_workflow_handler_audit_context(
    action_kind: &str,
    workflow_name: &str,
    execution_id: &str,
    pane_id: u64,
    input_summary: Option<&str>,
    result: &str,
    timestamp_ms: i64,
) -> crate::policy::DecisionContext {
    let mut context = crate::policy::DecisionContext::new_audit(
        timestamp_ms,
        crate::policy::ActionKind::WorkflowRun,
        crate::policy::ActorKind::Workflow,
        crate::policy::PolicySurface::Workflow,
        Some(pane_id),
        None,
        input_summary.map(str::to_string),
        Some(execution_id.to_string()),
    );
    let determining_rule = format!("audit.{action_kind}");
    context.record_rule(
        &determining_rule,
        true,
        Some("allow"),
        Some("workflow handler audit action recorded".to_string()),
    );
    context.set_determining_rule(&determining_rule);
    context.add_evidence("stage", "workflow_audit");
    context.add_evidence("workflow_name", workflow_name);
    context.add_evidence("workflow_action_kind", action_kind);
    context.add_evidence(
        "workflow_surface",
        crate::policy::PolicySurface::Workflow.as_str(),
    );
    context.add_evidence("execution_id", execution_id);
    context.add_evidence("workflow_result", result);
    context
}

fn add_optional_evidence(
    context: &mut crate::policy::DecisionContext,
    key: &str,
    value: Option<&str>,
) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        context.add_evidence(key, value);
    }
}

fn add_repeated_evidence(
    context: &mut crate::policy::DecisionContext,
    key_prefix: &str,
    values: &[String],
) {
    context.add_evidence(format!("{key_prefix}_count"), values.len().to_string());
    for (index, value) in values.iter().enumerate() {
        context.add_evidence(format!("{key_prefix}_{index}"), value);
    }
}

fn serialize_workflow_handler_audit_context(
    context: &crate::policy::DecisionContext,
) -> Option<String> {
    match serde_json::to_string(context) {
        Ok(serialized) => Some(serialized),
        Err(error) => {
            let workflow_name = context
                .evidence
                .iter()
                .find(|entry| entry.key == "workflow_name")
                .map(|entry| entry.value.as_str())
                .unwrap_or("unknown");
            tracing::warn!(
                error = %error,
                workflow_name,
                "workflow handler audit decision-context serialization failed"
            );
            None
        }
    }
}

fn build_session_start_audit_decision_context(
    execution_id: &str,
    pane_id: u64,
    agent_type: &str,
    event_type: &str,
    rule_id: Option<&str>,
    lookup: &SessionStartCassHintsLookup,
    input_summary: &str,
    result: &str,
    timestamp_ms: i64,
) -> Option<String> {
    let mut context = new_workflow_handler_audit_context(
        "session_start_context",
        "handle_session_start_context",
        execution_id,
        pane_id,
        Some(input_summary),
        result,
        timestamp_ms,
    );
    context.add_evidence("agent_type", agent_type);
    context.add_evidence("event_type", event_type);
    add_optional_evidence(&mut context, "trigger_rule_id", rule_id);
    add_optional_evidence(&mut context, "cass_query", lookup.query.as_deref());
    add_repeated_evidence(
        &mut context,
        "cass_query_candidate",
        &lookup.query_candidates,
    );
    add_optional_evidence(&mut context, "cass_workspace", lookup.workspace.as_deref());
    add_optional_evidence(&mut context, "bead_id", lookup.bead_id.as_deref());
    add_optional_evidence(&mut context, "pane_title", lookup.pane_title.as_deref());
    add_optional_evidence(&mut context, "pane_cwd", lookup.pane_cwd.as_deref());
    add_repeated_evidence(&mut context, "cass_hint", &lookup.hints);
    add_optional_evidence(&mut context, "cass_lookup_error", lookup.error.as_deref());
    serialize_workflow_handler_audit_context(&context)
}

fn build_auth_required_audit_decision_context(
    execution_id: &str,
    pane_id: u64,
    agent_type: &str,
    event_type: &str,
    rule_id: Option<&str>,
    strategy: &AuthRecoveryStrategy,
    cass_lookup: &AuthCassHintsLookup,
    input_summary: &str,
    result: &str,
    timestamp_ms: i64,
) -> Option<String> {
    let mut context = new_workflow_handler_audit_context(
        "auth_required",
        "handle_auth_required",
        execution_id,
        pane_id,
        Some(input_summary),
        result,
        timestamp_ms,
    );
    context.add_evidence("agent_type", agent_type);
    context.add_evidence("event_type", event_type);
    add_optional_evidence(&mut context, "trigger_rule_id", rule_id);
    context.add_evidence("auth_strategy", strategy.label());
    match strategy {
        AuthRecoveryStrategy::DeviceCode { code, url } => {
            add_optional_evidence(&mut context, "auth_device_code", code.as_deref());
            add_optional_evidence(&mut context, "auth_device_url", url.as_deref());
        }
        AuthRecoveryStrategy::ApiKeyError { key_hint } => {
            add_optional_evidence(&mut context, "auth_key_hint", key_hint.as_deref());
        }
        AuthRecoveryStrategy::ManualIntervention { agent_type, hint } => {
            context.add_evidence("auth_strategy_agent_type", agent_type);
            context.add_evidence("auth_strategy_hint", hint);
        }
    }
    add_optional_evidence(&mut context, "cass_query", cass_lookup.query.as_deref());
    add_optional_evidence(
        &mut context,
        "cass_workspace",
        cass_lookup.workspace.as_deref(),
    );
    add_repeated_evidence(&mut context, "cass_hint", &cass_lookup.hints);
    add_optional_evidence(
        &mut context,
        "cass_lookup_error",
        cass_lookup.error.as_deref(),
    );
    serialize_workflow_handler_audit_context(&context)
}

fn build_recovery_plan_audit_decision_context(
    action_kind: &str,
    workflow_name: &str,
    execution_id: &str,
    pane_id: u64,
    agent_type: &str,
    event_type: &str,
    rule_id: Option<&str>,
    input_summary: &str,
    result: &str,
    primary_plan_key: &str,
    secondary_plan_key: &str,
    plan: &serde_json::Value,
    timestamp_ms: i64,
) -> Option<String> {
    let mut context = new_workflow_handler_audit_context(
        action_kind,
        workflow_name,
        execution_id,
        pane_id,
        Some(input_summary),
        result,
        timestamp_ms,
    );
    context.add_evidence("agent_type", agent_type);
    context.add_evidence("event_type", event_type);
    add_optional_evidence(&mut context, "trigger_rule_id", rule_id);
    add_optional_evidence(
        &mut context,
        primary_plan_key,
        plan.get(primary_plan_key).and_then(|value| value.as_str()),
    );
    add_optional_evidence(
        &mut context,
        secondary_plan_key,
        plan.get(secondary_plan_key)
            .and_then(|value| value.as_str()),
    );
    if let Some(safe_to_send) = plan.get("safe_to_send").and_then(|value| value.as_bool()) {
        context.add_evidence("safe_to_send", safe_to_send.to_string());
    }
    let next_steps = plan
        .get("next_steps")
        .and_then(|value| value.as_array())
        .map(|steps| {
            steps
                .iter()
                .filter_map(|step| step.as_str().map(ToString::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    add_repeated_evidence(&mut context, "next_step", &next_steps);
    serialize_workflow_handler_audit_context(&context)
}

impl HandleSessionEnd {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Build an [`AgentSessionRecord`] from a detection trigger's extracted fields.
    pub fn record_from_detection(
        pane_id: u64,
        detection: &serde_json::Value,
    ) -> crate::storage::AgentSessionRecord {
        let agent_type_str = detection
            .get("agent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let extracted = detection.get("extracted");

        let mut record = crate::storage::AgentSessionRecord::new_start(pane_id, agent_type_str);
        let now = now_ms();
        record.ended_at = Some(now);

        // Determine end_reason from event_type
        let event_type = detection
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        record.end_reason = Some(match event_type {
            "session.end" => "completed".to_string(),
            "session.summary" => "completed".to_string(),
            other => other.to_string(),
        });

        // Extract session_id (Codex resume hint, Gemini session summary)
        if let Some(ext) = extracted {
            record.session_id = ext
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);

            // Token fields (Codex session.summary)
            record.total_tokens = ext.get("total").and_then(parse_i64_field);
            record.input_tokens = ext.get("input").and_then(parse_i64_field);
            record.output_tokens = ext.get("output").and_then(parse_i64_field);
            record.cached_tokens = ext.get("cached").and_then(parse_i64_field);
            record.reasoning_tokens = ext.get("reasoning").and_then(parse_i64_field);

            // Cost field (Claude Code session.cost_summary)
            record.estimated_cost_usd = ext.get("cost").and_then(parse_cost_usd_field);

            // Model name (if present in extracted)
            record.model_name = ext
                .get("model")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
        }

        record
    }
}

impl Default for HandleSessionEnd {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleSessionEnd {
    fn name(&self) -> &'static str {
        "handle_session_end"
    }

    fn description(&self) -> &'static str {
        "Persist structured session summary when an agent session ends"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        matches!(
            detection.event_type.as_str(),
            "session.summary" | "session.end"
        )
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &["session.summary", "session.end"]
    }

    fn supported_agent_types(&self) -> &'static [&'static str] {
        &["codex", "claude_code", "gemini"]
    }

    fn requires_pane(&self) -> bool {
        true
    }

    fn requires_approval(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        false
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new(
                "extract_summary",
                "Extract structured session data from detection",
            ),
            WorkflowStep::new("persist_record", "Persist session record to database"),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let trigger = ctx.trigger().cloned().unwrap_or(serde_json::Value::Null);

        Box::pin(async move {
            match step_idx {
                // Step 0: Extract and validate detection data
                0 => {
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let event_type = trigger
                        .get("event_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");

                    tracing::info!(
                        pane_id,
                        agent_type,
                        event_type,
                        has_trigger = !trigger.is_null(),
                        "handle_session_end: extracted session data from detection"
                    );

                    StepResult::cont()
                }

                // Step 1: Build and persist the session record
                1 => {
                    let mut record = Self::record_from_detection(pane_id, &trigger);

                    // If we already have an active session for this pane, prefer updating it
                    // rather than inserting a second record. This makes duration metrics meaningful.
                    if let Ok(existing) = storage.get_sessions_for_pane(pane_id).await {
                        let want_session_id = record.session_id.as_deref();
                        let candidate = existing
                            .into_iter()
                            .filter(|s| s.ended_at.is_none() && s.agent_type == record.agent_type)
                            .filter(|s| {
                                want_session_id.is_none_or(|id| s.session_id.as_deref() == Some(id))
                            })
                            .max_by_key(|s| s.started_at);

                        if let Some(active) = candidate {
                            record.id = active.id;
                            record.started_at = active.started_at;
                            if record.session_id.is_none() {
                                record.session_id = active.session_id;
                            }
                            if record.external_id.is_none() {
                                record.external_id = active.external_id;
                            }
                            if record.external_meta.is_none() {
                                record.external_meta = active.external_meta;
                            }
                        }
                    }

                    let agent_type = record.agent_type.clone();
                    let session_id = record.session_id.clone();
                    let has_tokens = record.total_tokens.is_some();
                    let has_cost = record.estimated_cost_usd.is_some();
                    let record_for_metrics = record.clone();

                    match storage.upsert_agent_session(record).await {
                        Ok(db_id) => {
                            // Best-effort usage metrics derived from the persisted session record.
                            // If these fail, don't fail the workflow.
                            let mut metrics: Vec<crate::storage::UsageMetricRecord> = Vec::new();
                            let now = now_ms();

                            if let Some(total) = record_for_metrics.total_tokens {
                                metrics.push(crate::storage::UsageMetricRecord {
                                    id: 0,
                                    timestamp: now,
                                    metric_type: crate::storage::MetricType::TokenUsage,
                                    pane_id: Some(pane_id),
                                    agent_type: Some(record_for_metrics.agent_type.clone()),
                                    account_id: None,
                                    workflow_id: None,
                                    count: None,
                                    amount: None,
                                    tokens: Some(total),
                                    metadata: Some(
                                        serde_json::json!({
                                            "source": "workflow.handle_session_end",
                                            "session_id": record_for_metrics.session_id.clone(),
                                            "input_tokens": record_for_metrics.input_tokens,
                                            "output_tokens": record_for_metrics.output_tokens,
                                            "cached_tokens": record_for_metrics.cached_tokens,
                                            "reasoning_tokens": record_for_metrics.reasoning_tokens,
                                            "model": record_for_metrics.model_name.clone(),
                                        })
                                        .to_string(),
                                    ),
                                    created_at: now,
                                });
                            }

                            if let Some(cost) = record_for_metrics.estimated_cost_usd {
                                metrics.push(crate::storage::UsageMetricRecord {
                                    id: 0,
                                    timestamp: now,
                                    metric_type: crate::storage::MetricType::ApiCost,
                                    pane_id: Some(pane_id),
                                    agent_type: Some(record_for_metrics.agent_type.clone()),
                                    account_id: None,
                                    workflow_id: None,
                                    count: None,
                                    amount: Some(cost),
                                    tokens: None,
                                    metadata: Some(
                                        serde_json::json!({
                                            "source": "workflow.handle_session_end",
                                            "session_id": record_for_metrics.session_id.clone(),
                                            "model": record_for_metrics.model_name.clone(),
                                        })
                                        .to_string(),
                                    ),
                                    created_at: now,
                                });
                            }

                            if let Some(ended_at) = record_for_metrics.ended_at {
                                let duration_ms =
                                    ended_at.saturating_sub(record_for_metrics.started_at);
                                let duration_secs = duration_ms / 1000;
                                metrics.push(crate::storage::UsageMetricRecord {
                                    id: 0,
                                    timestamp: ended_at,
                                    metric_type: crate::storage::MetricType::SessionDuration,
                                    pane_id: Some(pane_id),
                                    agent_type: Some(record_for_metrics.agent_type.clone()),
                                    account_id: None,
                                    workflow_id: None,
                                    count: Some(duration_secs),
                                    amount: None,
                                    tokens: None,
                                    metadata: Some(
                                        serde_json::json!({
                                            "source": "workflow.handle_session_end",
                                            "session_id": record_for_metrics.session_id.clone(),
                                            "duration_ms": duration_ms,
                                        })
                                        .to_string(),
                                    ),
                                    created_at: now,
                                });
                            }

                            if !metrics.is_empty() {
                                if let Err(err) = storage.record_usage_metrics_batch(metrics).await
                                {
                                    tracing::warn!(
                                        pane_id,
                                        error = %err,
                                        "handle_session_end: failed to record usage metrics"
                                    );
                                }
                            }

                            tracing::info!(
                                pane_id,
                                db_id,
                                agent_type = %agent_type,
                                session_id = ?session_id,
                                has_tokens,
                                has_cost,
                                "handle_session_end: persisted session record"
                            );

                            StepResult::done(serde_json::json!({
                                "status": "persisted",
                                "db_id": db_id,
                                "pane_id": pane_id,
                                "agent_type": agent_type,
                                "session_id": session_id,
                                "has_tokens": has_tokens,
                                "has_cost": has_cost,
                            }))
                        }
                        Err(e) => {
                            tracing::error!(
                                pane_id,
                                error = %e,
                                "handle_session_end: failed to persist session record"
                            );
                            StepResult::abort(format!("Failed to persist session: {e}"))
                        }
                    }
                }

                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

// ============================================================================
// HandleProcessTriageLifecycle — deterministic process-triage lifecycle wiring
// ============================================================================

const PROCESS_TRIAGE_LIFECYCLE_EVENT_TYPE: &str = "process_triage.lifecycle";
const PROCESS_TRIAGE_LIFECYCLE_RULE_ID: &str = "process_triage.lifecycle";

#[derive(Debug, Clone, Copy)]
struct ProcessTriagePlanStats {
    entry_count: usize,
    auto_safe_count: usize,
    review_count: usize,
    protected_count: usize,
    has_protected_destructive: bool,
}

impl ProcessTriagePlanStats {
    fn verify_invariants(self) -> Result<(), String> {
        let total = self
            .auto_safe_count
            .saturating_add(self.review_count)
            .saturating_add(self.protected_count);
        if total != self.entry_count {
            return Err(format!(
                "triage plan counts mismatch (entries={}, auto_safe={}, review={}, protected={})",
                self.entry_count, self.auto_safe_count, self.review_count, self.protected_count
            ));
        }
        Ok(())
    }
}

/// Wire process-triage lifecycle phases into the durable workflow runner.
///
/// This workflow does not mutate pane state directly. It provides deterministic
/// orchestration and explicit abort semantics for the six lifecycle phases:
/// snapshot -> plan -> apply -> verify -> diff -> session.
pub struct HandleProcessTriageLifecycle;

impl HandleProcessTriageLifecycle {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    fn triage_plan_value(trigger: &serde_json::Value) -> Option<&serde_json::Value> {
        trigger
            .pointer("/process_triage/plan")
            .or_else(|| trigger.pointer("/extracted/triage_plan"))
            .or_else(|| trigger.get("triage_plan"))
    }

    fn snapshot_value(
        trigger: &serde_json::Value,
        pane_id: u64,
        execution_id: &str,
    ) -> serde_json::Value {
        trigger
            .pointer("/process_triage/snapshot")
            .or_else(|| trigger.pointer("/extracted/process_snapshot"))
            .or_else(|| trigger.get("process_snapshot"))
            .cloned()
            .unwrap_or_else(|| {
                serde_json::json!({
                    "status": "synthetic",
                    "pane_id": pane_id,
                    "execution_id": execution_id,
                    "captured_at_ms": now_ms(),
                })
            })
    }

    fn diff_value(trigger: &serde_json::Value, stats: ProcessTriagePlanStats) -> serde_json::Value {
        trigger
            .pointer("/process_triage/diff")
            .or_else(|| trigger.pointer("/extracted/triage_diff"))
            .or_else(|| trigger.get("triage_diff"))
            .cloned()
            .unwrap_or_else(|| {
                serde_json::json!({
                    "status": "derived",
                    "entry_count": stats.entry_count,
                    "auto_safe_count": stats.auto_safe_count,
                    "review_count": stats.review_count,
                    "protected_count": stats.protected_count,
                })
            })
    }

    fn plan_stats_from_trigger(
        trigger: &serde_json::Value,
    ) -> Result<ProcessTriagePlanStats, String> {
        let Some(plan) = Self::triage_plan_value(trigger) else {
            return Ok(ProcessTriagePlanStats {
                entry_count: 0,
                auto_safe_count: 0,
                review_count: 0,
                protected_count: 0,
                has_protected_destructive: false,
            });
        };
        Self::plan_stats(plan)
    }

    fn plan_stats(plan: &serde_json::Value) -> Result<ProcessTriagePlanStats, String> {
        let entries: Vec<&serde_json::Value> = match plan.get("entries") {
            Some(raw) => raw
                .as_array()
                .ok_or_else(|| "triage plan entries must be an array".to_string())?
                .iter()
                .collect(),
            None => Vec::new(),
        };

        let mut inferred_auto_safe = 0usize;
        let mut inferred_review = 0usize;
        let mut inferred_protected = 0usize;
        let mut has_protected_destructive = false;

        for entry in &entries {
            let category = entry
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let action = entry
                .pointer("/action/action")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            if Self::category_is_auto_safe(category) {
                inferred_auto_safe = inferred_auto_safe.saturating_add(1);
            } else if Self::category_is_protected(category) {
                inferred_protected = inferred_protected.saturating_add(1);
                if !matches!(action, "protect" | "renice" | "flag_for_review") {
                    has_protected_destructive = true;
                }
            } else {
                inferred_review = inferred_review.saturating_add(1);
            }
        }

        let read_count = |key: &str, fallback: usize| -> Result<usize, String> {
            match plan.get(key) {
                Some(raw) => raw
                    .as_u64()
                    .and_then(|v| usize::try_from(v).ok())
                    .ok_or_else(|| {
                        format!("triage plan field '{key}' must be a non-negative integer")
                    }),
                None => Ok(fallback),
            }
        };

        Ok(ProcessTriagePlanStats {
            entry_count: entries.len(),
            auto_safe_count: read_count("auto_safe_count", inferred_auto_safe)?,
            review_count: read_count("review_count", inferred_review)?,
            protected_count: read_count("protected_count", inferred_protected)?,
            has_protected_destructive,
        })
    }

    fn category_is_auto_safe(category: &str) -> bool {
        matches!(
            category,
            "zombie" | "stuck_test" | "stuck_cli" | "duplicate_build"
        )
    }

    fn category_is_protected(category: &str) -> bool {
        matches!(category, "active_agent" | "system_process")
    }

    fn session_artifact(
        trigger: &serde_json::Value,
        pane_id: u64,
        execution_id: &str,
    ) -> serde_json::Value {
        let ft_session_id = trigger
            .pointer("/process_triage/ft_session_id")
            .or_else(|| trigger.pointer("/extracted/ft_session_id"))
            .or_else(|| trigger.get("ft_session_id"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("ft-{pane_id}-{execution_id}"));

        let pt_session_id = trigger
            .pointer("/process_triage/pt_session_id")
            .or_else(|| trigger.pointer("/extracted/pt_session_id"))
            .or_else(|| trigger.get("pt_session_id"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("pt-{execution_id}"));

        let provider = trigger
            .pointer("/process_triage/provider")
            .or_else(|| trigger.pointer("/extracted/provider"))
            .or_else(|| trigger.get("provider"))
            .and_then(|v| v.as_str())
            .unwrap_or("heuristic");

        serde_json::json!({
            "ft_session_id": ft_session_id,
            "pt_session_id": pt_session_id,
            "provider": provider,
        })
    }
}

impl Default for HandleProcessTriageLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleProcessTriageLifecycle {
    fn name(&self) -> &'static str {
        "handle_process_triage_lifecycle"
    }

    fn description(&self) -> &'static str {
        "Orchestrate process triage lifecycle phases (snapshot, plan, apply, verify, diff, session)"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        detection.event_type == PROCESS_TRIAGE_LIFECYCLE_EVENT_TYPE
            || detection.rule_id == PROCESS_TRIAGE_LIFECYCLE_RULE_ID
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &[PROCESS_TRIAGE_LIFECYCLE_EVENT_TYPE]
    }

    fn trigger_rule_ids(&self) -> &'static [&'static str] {
        &[PROCESS_TRIAGE_LIFECYCLE_RULE_ID]
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new(
                "snapshot",
                "Capture process snapshot baseline for lifecycle run",
            ),
            WorkflowStep::new(
                "plan",
                "Build triage plan artifact from snapshot/provider data",
            ),
            WorkflowStep::new(
                "apply",
                "Apply triage plan actions through policy-gated semantics",
            ),
            WorkflowStep::new("verify", "Verify triage outcomes and invariant integrity"),
            WorkflowStep::new(
                "diff",
                "Produce pre/post diff summary for audit and diagnostics",
            ),
            WorkflowStep::new(
                "session",
                "Emit session correlation artifact and finalize lifecycle",
            ),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let execution_id = ctx.execution_id().to_string();
        let capabilities = ctx.capabilities().clone();
        let trigger = ctx.trigger().cloned().unwrap_or(serde_json::Value::Null);

        Box::pin(async move {
            let stats = match Self::plan_stats_from_trigger(&trigger) {
                Ok(stats) => stats,
                Err(reason) => {
                    return StepResult::abort(format!(
                        "process triage lifecycle: invalid triage plan payload: {reason}"
                    ));
                }
            };

            match step_idx {
                0 => {
                    if capabilities.alt_screen == Some(true) {
                        return StepResult::abort(
                            "process triage lifecycle: pane in alt-screen mode; refusing snapshot step",
                        );
                    }
                    if capabilities.command_running {
                        return StepResult::abort(
                            "process triage lifecycle: command currently running; refusing snapshot step",
                        );
                    }
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        "handle_process_triage_lifecycle: snapshot step completed"
                    );
                    StepResult::cont()
                }
                1 => {
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        entry_count = stats.entry_count,
                        auto_safe_count = stats.auto_safe_count,
                        review_count = stats.review_count,
                        protected_count = stats.protected_count,
                        "handle_process_triage_lifecycle: plan step completed"
                    );
                    StepResult::cont()
                }
                2 => {
                    if stats.has_protected_destructive {
                        return StepResult::abort(
                            "process triage lifecycle: protected category includes destructive action",
                        );
                    }
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        auto_safe_count = stats.auto_safe_count,
                        review_count = stats.review_count,
                        protected_count = stats.protected_count,
                        "handle_process_triage_lifecycle: apply step completed"
                    );
                    StepResult::cont()
                }
                3 => match stats.verify_invariants() {
                    Ok(()) => {
                        tracing::info!(
                            pane_id,
                            execution_id = %execution_id,
                            "handle_process_triage_lifecycle: verify step completed"
                        );
                        StepResult::cont()
                    }
                    Err(reason) => StepResult::abort(format!(
                        "process triage lifecycle: verify step failed: {reason}"
                    )),
                },
                4 => {
                    let diff = Self::diff_value(&trigger, stats);
                    if !diff.is_object() {
                        return StepResult::abort(
                            "process triage lifecycle: diff artifact must be a JSON object",
                        );
                    }
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        "handle_process_triage_lifecycle: diff step completed"
                    );
                    StepResult::cont()
                }
                5 => {
                    let snapshot = Self::snapshot_value(&trigger, pane_id, &execution_id);
                    let plan = Self::triage_plan_value(&trigger)
                        .cloned()
                        .unwrap_or_else(|| {
                            serde_json::json!({
                                "entries": [],
                                "auto_safe_count": stats.auto_safe_count,
                                "review_count": stats.review_count,
                                "protected_count": stats.protected_count,
                            })
                        });
                    let apply = trigger
                        .pointer("/process_triage/apply")
                        .or_else(|| trigger.pointer("/extracted/triage_apply"))
                        .or_else(|| trigger.get("triage_apply"))
                        .cloned()
                        .unwrap_or_else(|| {
                            serde_json::json!({
                                "status": "derived",
                                "applied_auto_safe_count": stats.auto_safe_count,
                                "requires_approval_count": stats.review_count,
                                "protected_skipped_count": stats.protected_count,
                            })
                        });
                    let verify = serde_json::json!({
                        "status": "ok",
                        "entry_count": stats.entry_count,
                        "invariants_passed": true,
                    });
                    let diff = Self::diff_value(&trigger, stats);
                    let session = Self::session_artifact(&trigger, pane_id, &execution_id);

                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        "handle_process_triage_lifecycle: session step completed"
                    );

                    StepResult::done(serde_json::json!({
                        "status": "completed",
                        "pane_id": pane_id,
                        "workflow": "handle_process_triage_lifecycle",
                        "snapshot": snapshot,
                        "plan": plan,
                        "apply": apply,
                        "verify": verify,
                        "diff": diff,
                        "session": session,
                    }))
                }
                _ => StepResult::abort(format!(
                    "process triage lifecycle: unexpected step index: {step_idx}"
                )),
            }
        })
    }
}

// ============================================================================
// HandleSessionStartContext — inject startup context from cass (ft-2l9kn)
// ============================================================================

/// Default cooldown window in milliseconds (10 minutes).
/// Session-start context injection events within this window for the same pane are suppressed.
pub const SESSION_START_CONTEXT_COOLDOWN_MS: i64 = 10 * 60 * 1000;
const SESSION_START_CASS_HINT_LIMIT: usize = 3;
const SESSION_START_CASS_TIMEOUT_SECS: u64 = 8;
const SESSION_START_CASS_LOOKBACK_DAYS: u32 = 30;
const SESSION_START_QUERY_MAX_CHARS: usize = 180;
const SESSION_START_HINT_MAX_CHARS: usize = 160;

static SESSION_START_BEAD_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:ft|wa)-[a-z0-9][a-z0-9.-]*\b").expect("valid bead id regex")
});

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionStartCassHintsLookup {
    pub query: Option<String>,
    pub query_candidates: Vec<String>,
    pub workspace: Option<String>,
    pub hints: Vec<String>,
    pub error: Option<String>,
    pub bead_id: Option<String>,
    pub pane_title: Option<String>,
    pub pane_cwd: Option<String>,
}

/// Inject startup context hints when a session starts.
///
/// This workflow runs on `session.start` events, performs a bounded cass lookup
/// for related historical sessions, persists an audit decision, and injects a
/// startup prompt into the pane.
pub struct HandleSessionStartContext {
    cooldown_ms: i64,
}

impl HandleSessionStartContext {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cooldown_ms: SESSION_START_CONTEXT_COOLDOWN_MS,
        }
    }

    /// Create with a custom cooldown (useful for tests).
    #[allow(dead_code)]
    #[must_use]
    pub fn with_cooldown_ms(cooldown_ms: i64) -> Self {
        Self { cooldown_ms }
    }

    fn compact_whitespace(input: &str) -> String {
        input.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn truncate_chars(input: &str, max_chars: usize) -> String {
        input.chars().take(max_chars).collect()
    }

    fn quote_for_shell_command(value: &str) -> String {
        serde_json::to_string(value).unwrap_or_else(|_| {
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        })
    }

    fn push_candidate(candidates: &mut Vec<String>, candidate: Option<String>) {
        let Some(raw) = candidate else {
            return;
        };
        let compact = Self::compact_whitespace(raw.trim());
        if compact.is_empty() {
            return;
        }
        let truncated = Self::truncate_chars(&compact, SESSION_START_QUERY_MAX_CHARS);
        if candidates
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&truncated))
        {
            return;
        }
        candidates.push(truncated);
    }

    fn extract_bead_id(input: &str) -> Option<String> {
        SESSION_START_BEAD_ID_RE
            .find(input)
            .map(|m| m.as_str().to_ascii_lowercase())
    }

    fn pane_tail_context(path: &str) -> Option<String> {
        let components: Vec<String> = std::path::Path::new(path)
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(value) => Some(value.to_string_lossy().to_string()),
                _ => None,
            })
            .collect();

        if components.is_empty() {
            return None;
        }

        let mut tail: Vec<String> = components.into_iter().rev().take(2).collect();
        tail.reverse();
        Some(tail.join("/"))
    }

    fn workspace_for_pane(pane: &crate::storage::PaneRecord) -> Option<String> {
        let cwd = pane.cwd.as_deref()?;
        Self::workspace_from_cwd(cwd)
    }

    fn workspace_from_cwd(cwd: &str) -> Option<String> {
        let parsed = crate::wezterm::CwdInfo::parse(cwd);
        if parsed.is_remote || parsed.path.is_empty() {
            return None;
        }
        Some(parsed.path)
    }

    fn cass_agent_from_trigger(trigger: &serde_json::Value) -> Option<CassAgent> {
        trigger
            .get("agent_type")
            .and_then(|v| v.as_str())
            .and_then(CassAgent::from_slug)
    }

    fn query_candidates(
        trigger: &serde_json::Value,
        pane: Option<&crate::storage::PaneRecord>,
    ) -> (Vec<String>, Option<String>) {
        let mut candidates = Vec::new();

        let title = pane
            .and_then(|record| record.title.as_deref())
            .map(ToString::to_string);
        let cwd = pane
            .and_then(|record| record.cwd.as_deref())
            .map(ToString::to_string);
        let matched_text = trigger
            .get("matched_text")
            .and_then(|v| v.as_str())
            .map(ToString::to_string);
        let extracted_message = trigger
            .get("extracted")
            .and_then(|value| value.get("message"))
            .and_then(|value| value.as_str())
            .map(ToString::to_string);

        let bead_id = title
            .as_deref()
            .and_then(Self::extract_bead_id)
            .or_else(|| matched_text.as_deref().and_then(Self::extract_bead_id))
            .or_else(|| extracted_message.as_deref().and_then(Self::extract_bead_id));

        Self::push_candidate(&mut candidates, bead_id.clone());

        Self::push_candidate(&mut candidates, title);
        Self::push_candidate(
            &mut candidates,
            cwd.and_then(|raw| Self::workspace_from_cwd(&raw))
                .and_then(|workspace| Self::pane_tail_context(&workspace)),
        );
        Self::push_candidate(&mut candidates, extracted_message);
        Self::push_candidate(&mut candidates, matched_text);

        let model = trigger
            .get("extracted")
            .and_then(|value| value.get("model"))
            .and_then(|value| value.as_str())
            .map(ToString::to_string);
        Self::push_candidate(&mut candidates, model);

        let agent_type = trigger
            .get("agent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("agent");
        Self::push_candidate(
            &mut candidates,
            Some(format!("{agent_type} session startup context")),
        );

        (candidates, bead_id)
    }

    fn format_cass_hint(hit: &CassSearchHit) -> Option<String> {
        let snippet = hit
            .content
            .as_deref()
            .map(Self::compact_whitespace)
            .unwrap_or_default();
        if snippet.is_empty() {
            return None;
        }

        let source_path = hit
            .source_path
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("unknown");
        let line_suffix = hit
            .line_number
            .map_or_else(String::new, |line| format!(":{line}"));
        let compact_snippet = Self::truncate_chars(&snippet, SESSION_START_HINT_MAX_CHARS);

        Some(format!("{source_path}{line_suffix} - {compact_snippet}"))
    }

    async fn lookup_cass_hints(
        storage: &StorageHandle,
        pane_id: u64,
        trigger: &serde_json::Value,
    ) -> SessionStartCassHintsLookup {
        let pane = match storage.get_pane(pane_id).await {
            Ok(record) => record,
            Err(error) => {
                return SessionStartCassHintsLookup {
                    query: None,
                    query_candidates: vec![],
                    workspace: None,
                    hints: vec![],
                    error: Some(format!("pane_lookup_failed: {error}")),
                    bead_id: None,
                    pane_title: None,
                    pane_cwd: None,
                };
            }
        };

        let (query_candidates, bead_id) = Self::query_candidates(trigger, pane.as_ref());
        let Some(first_query) = query_candidates.first().cloned() else {
            return SessionStartCassHintsLookup {
                query: None,
                query_candidates,
                workspace: None,
                hints: vec![],
                error: Some("no_query_candidates".to_string()),
                bead_id,
                pane_title: pane.as_ref().and_then(|record| record.title.clone()),
                pane_cwd: pane.as_ref().and_then(|record| record.cwd.clone()),
            };
        };

        let workspace = pane.as_ref().and_then(Self::workspace_for_pane);
        let options = SearchOptions {
            limit: Some(SESSION_START_CASS_HINT_LIMIT),
            offset: None,
            agent: Self::cass_agent_from_trigger(trigger),
            workspace: workspace.clone(),
            days: Some(SESSION_START_CASS_LOOKBACK_DAYS),
            fields: Some("minimal".to_string()),
            max_tokens: Some(220),
        };

        let cass = CassClient::new().with_timeout_secs(SESSION_START_CASS_TIMEOUT_SECS);
        let mut last_error: Option<String> = None;

        for query in &query_candidates {
            match cass.search(query, &options).await {
                Ok(result) => {
                    let hints = result
                        .hits
                        .iter()
                        .filter_map(Self::format_cass_hint)
                        .take(SESSION_START_CASS_HINT_LIMIT)
                        .collect::<Vec<_>>();
                    if !hints.is_empty() {
                        return SessionStartCassHintsLookup {
                            query: Some(query.clone()),
                            query_candidates,
                            workspace,
                            hints,
                            error: None,
                            bead_id,
                            pane_title: pane.as_ref().and_then(|record| record.title.clone()),
                            pane_cwd: pane.as_ref().and_then(|record| record.cwd.clone()),
                        };
                    }
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                }
            }
        }

        SessionStartCassHintsLookup {
            query: Some(first_query),
            query_candidates,
            workspace,
            hints: vec![],
            error: last_error,
            bead_id,
            pane_title: pane.as_ref().and_then(|record| record.title.clone()),
            pane_cwd: pane.as_ref().and_then(|record| record.cwd.clone()),
        }
    }

    pub fn build_context_prompt(
        trigger: &serde_json::Value,
        cass_lookup: &SessionStartCassHintsLookup,
    ) -> String {
        let agent_type = trigger
            .get("agent_type")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        let model = trigger
            .get("extracted")
            .and_then(|value| value.get("model"))
            .and_then(|value| value.as_str());

        let mut lines = vec![
            "Session startup context bootstrap.".to_string(),
            format!("Agent type: {agent_type}"),
        ];
        if let Some(model_name) = model {
            lines.push(format!("Model: {model_name}"));
        }
        if let Some(bead_id) = cass_lookup.bead_id.as_deref() {
            lines.push(format!("Detected bead/task hint: {bead_id}"));
        }
        if let Some(title) = cass_lookup.pane_title.as_deref() {
            lines.push(format!("Pane title: {title}"));
        }
        if let Some(cwd) = cass_lookup.pane_cwd.as_deref() {
            lines.push(format!("Pane cwd: {cwd}"));
        }

        if !cass_lookup.hints.is_empty() {
            lines.push(String::new());
            lines.push("Related fixes from past sessions (cass):".to_string());
            for hint in &cass_lookup.hints {
                lines.push(format!("- {hint}"));
            }
        } else if let Some(error) = cass_lookup.error.as_deref() {
            lines.push(String::new());
            lines.push(format!("Cass lookup unavailable: {error}"));
        } else {
            lines.push(String::new());
            lines.push("No direct cass matches found yet.".to_string());
        }

        if let Some(query) = cass_lookup.query.as_deref() {
            lines.push(String::new());
            lines.push(format!("Cass query: {query}"));
            let quoted_query = Self::quote_for_shell_command(query);
            lines.push(format!(
                "Try: ft robot cass search {quoted_query} --limit 3"
            ));
        }
        if let Some(workspace) = cass_lookup.workspace.as_deref() {
            lines.push(format!("Cass workspace filter: {workspace}"));
        }

        lines.push(String::new());
        lines.push(
            "Keep AGENTS.md + README.md + current bead acceptance criteria in focus.".to_string(),
        );
        lines.push("If uncertain on next work, run: bv --robot-triage".to_string());

        let mut prompt = lines.join("\n");
        prompt.push('\n');
        prompt
    }
}

impl Default for HandleSessionStartContext {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleSessionStartContext {
    fn name(&self) -> &'static str {
        "handle_session_start_context"
    }

    fn description(&self) -> &'static str {
        "Inject startup context hints from cass when a session starts"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        detection.event_type == "session.start"
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &["session.start"]
    }

    fn supported_agent_types(&self) -> &'static [&'static str] {
        &["claude_code", "codex", "gemini"]
    }

    fn requires_pane(&self) -> bool {
        true
    }

    fn requires_approval(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        false
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new(
                "check_cooldown",
                "Skip if startup context was recently injected for this pane",
            ),
            WorkflowStep::new(
                "inject_startup_context",
                "Lookup cass hints and inject startup context prompt",
            ),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let trigger = ctx.trigger().cloned().unwrap_or(serde_json::Value::Null);
        let execution_id = ctx.execution_id().to_string();
        let cooldown_ms = self.cooldown_ms;

        Box::pin(async move {
            match step_idx {
                // Step 0: Check cooldown to prevent repeated session-start injection spam.
                0 => {
                    let since = now_ms() - cooldown_ms;
                    let query = crate::storage::AuditQuery {
                        pane_id: Some(pane_id),
                        action_kind: Some("session_start_context".to_string()),
                        since: Some(since),
                        limit: Some(1),
                        ..Default::default()
                    };

                    match storage.get_audit_actions(query).await {
                        Ok(recent) if !recent.is_empty() => {
                            tracing::info!(
                                pane_id,
                                last_context_ts = recent[0].ts,
                                "handle_session_start_context: within cooldown, skipping"
                            );
                            StepResult::done(serde_json::json!({
                                "status": "cooldown_skipped",
                                "pane_id": pane_id,
                                "last_context_ts": recent[0].ts,
                            }))
                        }
                        Ok(_) => StepResult::cont(),
                        Err(error) => {
                            tracing::warn!(
                                pane_id,
                                error = %error,
                                "handle_session_start_context: cooldown check failed, proceeding"
                            );
                            StepResult::cont()
                        }
                    }
                }

                // Step 1: Query cass, record audit decision, and inject startup prompt.
                1 => {
                    let lookup = Self::lookup_cass_hints(&storage, pane_id, &trigger).await;
                    let prompt = Self::build_context_prompt(&trigger, &lookup);
                    let result = if lookup.hints.is_empty() {
                        if lookup.error.is_some() {
                            "lookup_error"
                        } else {
                            "no_hints"
                        }
                    } else {
                        "hints_injected"
                    };

                    let rule_id = trigger
                        .get("rule_id")
                        .and_then(|value| value.as_str())
                        .map(ToString::to_string);
                    let event_type = trigger
                        .get("event_type")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown");
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown");
                    let timestamp_ms = now_ms();
                    let input_summary = format!("Session start context injection for {agent_type}");
                    let decision_context = build_session_start_audit_decision_context(
                        &execution_id,
                        pane_id,
                        agent_type,
                        event_type,
                        rule_id.as_deref(),
                        &lookup,
                        &input_summary,
                        result,
                        timestamp_ms,
                    );

                    let audit = crate::storage::AuditActionRecord {
                        id: 0,
                        ts: timestamp_ms,
                        actor_kind: "workflow".to_string(),
                        actor_id: Some(execution_id.clone()),
                        correlation_id: None,
                        pane_id: Some(pane_id),
                        domain: None,
                        action_kind: "session_start_context".to_string(),
                        policy_decision: "allow".to_string(),
                        decision_reason: None,
                        rule_id,
                        input_summary: Some(input_summary),
                        verification_summary: None,
                        decision_context,
                        result: result.to_string(),
                    };

                    match storage.record_audit_action(audit).await {
                        Ok(audit_id) => {
                            tracing::info!(
                                pane_id,
                                audit_id,
                                result,
                                "handle_session_start_context: recorded session-start context decision"
                            );
                            StepResult::send_text(prompt)
                        }
                        Err(error) => {
                            tracing::error!(
                                pane_id,
                                error = %error,
                                "handle_session_start_context: failed to record audit decision"
                            );
                            StepResult::abort(format!(
                                "Failed to record session_start_context decision: {error}"
                            ))
                        }
                    }
                }

                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

// ============================================================================
// HandleOnErrorCassSearch — search cass for past fixes on error detection (ft-2l9kn)
// ============================================================================

/// Default cooldown window in milliseconds (3 minutes).
/// Error-triggered cass lookup events within this window for the same pane are suppressed.
pub const ON_ERROR_CASS_COOLDOWN_MS: i64 = 3 * 60 * 1000;
const ON_ERROR_CASS_HINT_LIMIT: usize = 3;
const ON_ERROR_CASS_TIMEOUT_SECS: u64 = 6;
const ON_ERROR_CASS_LOOKBACK_DAYS: u32 = 30;
const ON_ERROR_CASS_QUERY_MAX_CHARS: usize = 200;
const ON_ERROR_CASS_HINT_MAX_CHARS: usize = 180;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OnErrorCassHintsLookup {
    pub query: Option<String>,
    pub query_candidates: Vec<String>,
    pub workspace: Option<String>,
    pub hints: Vec<String>,
    pub error: Option<String>,
    pub error_text: Option<String>,
    pub rule_id: Option<String>,
}

/// Search cass for past fixes when an error pattern is detected in pane output.
///
/// This workflow triggers on `error.*` event types, performs a bounded cass lookup
/// for similar past errors and their resolutions, records an audit decision, and
/// logs the hints. If cass returns matches, a brief summary is injected as a
/// comment into the pane so the agent can leverage past solutions.
pub struct HandleOnErrorCassSearch {
    cooldown_ms: i64,
}

impl HandleOnErrorCassSearch {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cooldown_ms: ON_ERROR_CASS_COOLDOWN_MS,
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn with_cooldown_ms(cooldown_ms: i64) -> Self {
        Self { cooldown_ms }
    }

    fn compact_whitespace(input: &str) -> String {
        input.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn truncate_chars(input: &str, max_chars: usize) -> String {
        input.chars().take(max_chars).collect()
    }

    fn extract_error_text(trigger: &serde_json::Value) -> Option<String> {
        // Try matched_text first, then extracted.message, then extracted.error
        trigger
            .get("matched_text")
            .and_then(|v| v.as_str())
            .or_else(|| {
                trigger
                    .get("extracted")
                    .and_then(|v| v.get("message"))
                    .and_then(|v| v.as_str())
            })
            .or_else(|| {
                trigger
                    .get("extracted")
                    .and_then(|v| v.get("error"))
                    .and_then(|v| v.as_str())
            })
            .map(|s| {
                Self::truncate_chars(
                    &Self::compact_whitespace(s.trim()),
                    ON_ERROR_CASS_QUERY_MAX_CHARS,
                )
            })
    }

    fn cass_agent_from_trigger(trigger: &serde_json::Value) -> Option<CassAgent> {
        trigger
            .get("agent_type")
            .and_then(|v| v.as_str())
            .and_then(CassAgent::from_slug)
    }

    fn workspace_from_pane(pane: Option<&crate::storage::PaneRecord>) -> Option<String> {
        let cwd = pane?.cwd.as_deref()?;
        let parsed = crate::wezterm::CwdInfo::parse(cwd);
        if parsed.is_remote || parsed.path.is_empty() {
            return None;
        }
        Some(parsed.path)
    }

    fn query_candidates(
        trigger: &serde_json::Value,
        pane: Option<&crate::storage::PaneRecord>,
    ) -> Vec<String> {
        let mut candidates = Vec::new();

        // Primary: the error text itself
        if let Some(error_text) = Self::extract_error_text(trigger) {
            if !error_text.is_empty() {
                candidates.push(error_text);
            }
        }

        // Secondary: rule_id for structural matching
        if let Some(rule_id) = trigger.get("rule_id").and_then(|v| v.as_str()) {
            let rule_query = Self::truncate_chars(rule_id, ON_ERROR_CASS_QUERY_MAX_CHARS);
            if !candidates
                .iter()
                .any(|c| c.eq_ignore_ascii_case(&rule_query))
            {
                candidates.push(rule_query);
            }
        }

        // Tertiary: pane title for broader context
        if let Some(title) = pane.and_then(|p| p.title.as_deref()) {
            let title_trimmed = Self::truncate_chars(
                &Self::compact_whitespace(title.trim()),
                ON_ERROR_CASS_QUERY_MAX_CHARS,
            );
            if !title_trimmed.is_empty()
                && !candidates
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(&title_trimmed))
            {
                candidates.push(title_trimmed);
            }
        }

        candidates
    }

    fn format_cass_hint(hit: &CassSearchHit) -> Option<String> {
        let snippet = hit
            .content
            .as_deref()
            .map(Self::compact_whitespace)
            .unwrap_or_default();
        if snippet.is_empty() {
            return None;
        }
        let source_path = hit
            .source_path
            .as_deref()
            .filter(|v| !v.is_empty())
            .unwrap_or("unknown");
        let line_suffix = hit
            .line_number
            .map_or_else(String::new, |line| format!(":{line}"));
        let compact = Self::truncate_chars(&snippet, ON_ERROR_CASS_HINT_MAX_CHARS);
        Some(format!("{source_path}{line_suffix} - {compact}"))
    }

    async fn lookup_cass_hints(
        storage: &StorageHandle,
        pane_id: u64,
        trigger: &serde_json::Value,
    ) -> OnErrorCassHintsLookup {
        let pane = match storage.get_pane(pane_id).await {
            Ok(record) => record,
            Err(error) => {
                return OnErrorCassHintsLookup {
                    error: Some(format!("pane_lookup_failed: {error}")),
                    ..Default::default()
                };
            }
        };

        let query_candidates = Self::query_candidates(trigger, pane.as_ref());
        let Some(first_query) = query_candidates.first().cloned() else {
            return OnErrorCassHintsLookup {
                query_candidates,
                error: Some("no_query_candidates".to_string()),
                error_text: Self::extract_error_text(trigger),
                rule_id: trigger
                    .get("rule_id")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string),
                ..Default::default()
            };
        };

        let workspace = Self::workspace_from_pane(pane.as_ref());
        let options = SearchOptions {
            limit: Some(ON_ERROR_CASS_HINT_LIMIT),
            offset: None,
            agent: Self::cass_agent_from_trigger(trigger),
            workspace: workspace.clone(),
            days: Some(ON_ERROR_CASS_LOOKBACK_DAYS),
            fields: Some("minimal".to_string()),
            max_tokens: Some(250),
        };

        let cass = CassClient::new().with_timeout_secs(ON_ERROR_CASS_TIMEOUT_SECS);
        let mut last_error: Option<String> = None;

        for query in &query_candidates {
            match cass.search(query, &options).await {
                Ok(result) => {
                    let hints = result
                        .hits
                        .iter()
                        .filter_map(Self::format_cass_hint)
                        .take(ON_ERROR_CASS_HINT_LIMIT)
                        .collect::<Vec<_>>();
                    if !hints.is_empty() {
                        return OnErrorCassHintsLookup {
                            query: Some(query.clone()),
                            query_candidates,
                            workspace,
                            hints,
                            error: None,
                            error_text: Self::extract_error_text(trigger),
                            rule_id: trigger
                                .get("rule_id")
                                .and_then(|v| v.as_str())
                                .map(ToString::to_string),
                        };
                    }
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                }
            }
        }

        OnErrorCassHintsLookup {
            query: Some(first_query),
            query_candidates,
            workspace,
            hints: vec![],
            error: last_error,
            error_text: Self::extract_error_text(trigger),
            rule_id: trigger
                .get("rule_id")
                .and_then(|v| v.as_str())
                .map(ToString::to_string),
        }
    }
}

impl Default for HandleOnErrorCassSearch {
    fn default() -> Self {
        Self::new()
    }
}

fn build_on_error_cass_audit_decision_context(
    execution_id: &str,
    pane_id: u64,
    agent_type: &str,
    event_type: &str,
    rule_id: Option<&str>,
    lookup: &OnErrorCassHintsLookup,
    input_summary: &str,
    result: &str,
    timestamp_ms: i64,
) -> Option<String> {
    let mut context = new_workflow_handler_audit_context(
        "on_error_cass_search",
        "handle_on_error_cass_search",
        execution_id,
        pane_id,
        Some(input_summary),
        result,
        timestamp_ms,
    );
    context.add_evidence("agent_type", agent_type);
    context.add_evidence("event_type", event_type);
    add_optional_evidence(&mut context, "trigger_rule_id", rule_id);
    add_optional_evidence(&mut context, "cass_query", lookup.query.as_deref());
    add_repeated_evidence(
        &mut context,
        "cass_query_candidate",
        &lookup.query_candidates,
    );
    add_optional_evidence(&mut context, "cass_workspace", lookup.workspace.as_deref());
    add_optional_evidence(&mut context, "error_text", lookup.error_text.as_deref());
    add_optional_evidence(&mut context, "error_rule_id", lookup.rule_id.as_deref());
    add_repeated_evidence(&mut context, "cass_hint", &lookup.hints);
    add_optional_evidence(&mut context, "cass_lookup_error", lookup.error.as_deref());
    serialize_workflow_handler_audit_context(&context)
}

impl Workflow for HandleOnErrorCassSearch {
    fn name(&self) -> &'static str {
        "handle_on_error_cass_search"
    }

    fn description(&self) -> &'static str {
        "Search cass for past fixes when an error pattern is detected"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        let et = &detection.event_type;
        et.starts_with("error.") || et == "mux.error"
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &[
            "error.network",
            "error.timeout",
            "error.overloaded",
            "mux.error",
        ]
    }

    fn supported_agent_types(&self) -> &'static [&'static str] {
        &["claude_code", "codex", "gemini"]
    }

    fn requires_pane(&self) -> bool {
        true
    }

    fn requires_approval(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        false
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new(
                "check_cooldown",
                "Skip if on-error cass search was recently performed for this pane",
            ),
            WorkflowStep::new(
                "search_cass_for_error",
                "Search cass for similar past errors and inject hints",
            ),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let trigger = ctx.trigger().cloned().unwrap_or(serde_json::Value::Null);
        let execution_id = ctx.execution_id().to_string();
        let cooldown_ms = self.cooldown_ms;

        Box::pin(async move {
            match step_idx {
                // Step 0: Check cooldown
                0 => {
                    let since = now_ms() - cooldown_ms;
                    let query = crate::storage::AuditQuery {
                        pane_id: Some(pane_id),
                        action_kind: Some("on_error_cass_search".to_string()),
                        since: Some(since),
                        limit: Some(1),
                        ..Default::default()
                    };

                    match storage.get_audit_actions(query).await {
                        Ok(recent) if !recent.is_empty() => {
                            tracing::info!(
                                pane_id,
                                last_search_ts = recent[0].ts,
                                "handle_on_error_cass_search: within cooldown, skipping"
                            );
                            StepResult::done(serde_json::json!({
                                "status": "cooldown_skipped",
                                "pane_id": pane_id,
                                "last_search_ts": recent[0].ts,
                            }))
                        }
                        Ok(_) => StepResult::cont(),
                        Err(error) => {
                            tracing::warn!(
                                pane_id,
                                error = %error,
                                "handle_on_error_cass_search: cooldown check failed, proceeding"
                            );
                            StepResult::cont()
                        }
                    }
                }

                // Step 1: Search cass and log/inject hints
                1 => {
                    let lookup = Self::lookup_cass_hints(&storage, pane_id, &trigger).await;
                    let result_label = if lookup.hints.is_empty() {
                        if lookup.error.is_some() {
                            "lookup_error"
                        } else {
                            "no_hints"
                        }
                    } else {
                        "hints_found"
                    };

                    let rule_id = trigger
                        .get("rule_id")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                    let event_type = trigger
                        .get("event_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let timestamp_ms = now_ms();

                    let error_preview = lookup.error_text.as_deref().unwrap_or("unknown error");
                    let input_summary =
                        format!("On-error cass search for {agent_type}: {error_preview}");
                    let decision_context = build_on_error_cass_audit_decision_context(
                        &execution_id,
                        pane_id,
                        agent_type,
                        event_type,
                        rule_id.as_deref(),
                        &lookup,
                        &input_summary,
                        result_label,
                        timestamp_ms,
                    );

                    let audit = crate::storage::AuditActionRecord {
                        id: 0,
                        ts: timestamp_ms,
                        actor_kind: "workflow".to_string(),
                        actor_id: Some(execution_id.clone()),
                        correlation_id: None,
                        pane_id: Some(pane_id),
                        domain: None,
                        action_kind: "on_error_cass_search".to_string(),
                        policy_decision: "allow".to_string(),
                        decision_reason: None,
                        rule_id,
                        input_summary: Some(input_summary),
                        verification_summary: None,
                        decision_context,
                        result: result_label.to_string(),
                    };

                    if let Err(error) = storage.record_audit_action(audit).await {
                        tracing::error!(
                            pane_id,
                            error = %error,
                            "handle_on_error_cass_search: failed to record audit"
                        );
                        return StepResult::abort(format!(
                            "Failed to record on_error_cass_search audit: {error}"
                        ));
                    }

                    if lookup.hints.is_empty() {
                        tracing::info!(
                            pane_id,
                            result = result_label,
                            "handle_on_error_cass_search: no past fixes found"
                        );
                        StepResult::done(serde_json::json!({
                            "status": result_label,
                            "pane_id": pane_id,
                            "error_text": lookup.error_text,
                        }))
                    } else {
                        tracing::info!(
                            pane_id,
                            hint_count = lookup.hints.len(),
                            "handle_on_error_cass_search: found past fixes"
                        );
                        StepResult::done(serde_json::json!({
                            "status": "hints_found",
                            "pane_id": pane_id,
                            "hint_count": lookup.hints.len(),
                            "hints": lookup.hints,
                            "query": lookup.query,
                        }))
                    }
                }

                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

// ============================================================================
// HandleSwarmLearningIndex — index completed sessions into cass (ft-2l9kn)
// ============================================================================

/// Cooldown window in milliseconds (15 minutes).
/// Prevents re-indexing if a recent session from the same pane already triggered it.
pub const SWARM_LEARNING_INDEX_COOLDOWN_MS: i64 = 15 * 60 * 1000;
const SWARM_LEARNING_INDEX_TIMEOUT_SECS: u64 = 30;

/// Trigger cass index refresh when an agent session completes.
///
/// This is the "swarm learning" component of the cass integration: when an agent
/// finishes working on a problem, the session is indexed so future agents can
/// search for and benefit from the solution.
///
/// Fires on `session.end` and `session.summary` events after the cooldown window.
pub struct HandleSwarmLearningIndex {
    cooldown_ms: i64,
}

impl HandleSwarmLearningIndex {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cooldown_ms: SWARM_LEARNING_INDEX_COOLDOWN_MS,
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn with_cooldown_ms(cooldown_ms: i64) -> Self {
        Self { cooldown_ms }
    }

    fn workspace_from_pane(pane: Option<&crate::storage::PaneRecord>) -> Option<String> {
        let cwd = pane?.cwd.as_deref()?;
        let parsed = crate::wezterm::CwdInfo::parse(cwd);
        if parsed.is_remote || parsed.path.is_empty() {
            return None;
        }
        Some(parsed.path)
    }
}

impl Default for HandleSwarmLearningIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleSwarmLearningIndex {
    fn name(&self) -> &'static str {
        "handle_swarm_learning_index"
    }

    fn description(&self) -> &'static str {
        "Index completed agent sessions into cass for swarm learning"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        let et = &detection.event_type;
        et == "session.end" || et == "session.summary"
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &["session.end", "session.summary"]
    }

    fn supported_agent_types(&self) -> &'static [&'static str] {
        &["claude_code", "codex", "gemini"]
    }

    fn requires_pane(&self) -> bool {
        true
    }

    fn requires_approval(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        false
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new(
                "check_cooldown",
                "Skip if cass index was recently triggered for this pane",
            ),
            WorkflowStep::new(
                "trigger_cass_index",
                "Trigger cass index refresh for the session workspace",
            ),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let trigger = ctx.trigger().cloned().unwrap_or(serde_json::Value::Null);
        let execution_id = ctx.execution_id().to_string();
        let cooldown_ms = self.cooldown_ms;

        Box::pin(async move {
            match step_idx {
                // Step 0: Check cooldown
                0 => {
                    let since = now_ms() - cooldown_ms;
                    let query = crate::storage::AuditQuery {
                        pane_id: Some(pane_id),
                        action_kind: Some("swarm_learning_index".to_string()),
                        since: Some(since),
                        limit: Some(1),
                        ..Default::default()
                    };

                    match storage.get_audit_actions(query).await {
                        Ok(recent) if !recent.is_empty() => {
                            tracing::info!(
                                pane_id,
                                last_index_ts = recent[0].ts,
                                "handle_swarm_learning_index: within cooldown, skipping"
                            );
                            StepResult::done(serde_json::json!({
                                "status": "cooldown_skipped",
                                "pane_id": pane_id,
                                "last_index_ts": recent[0].ts,
                            }))
                        }
                        Ok(_) => StepResult::cont(),
                        Err(error) => {
                            tracing::warn!(
                                pane_id,
                                error = %error,
                                "handle_swarm_learning_index: cooldown check failed, proceeding"
                            );
                            StepResult::cont()
                        }
                    }
                }

                // Step 1: Trigger cass index
                1 => {
                    let pane = storage.get_pane(pane_id).await.ok().flatten();
                    let workspace = Self::workspace_from_pane(pane.as_ref());
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");

                    let cass = CassClient::new()
                        .with_timeout_secs(SWARM_LEARNING_INDEX_TIMEOUT_SECS);
                    let result_label = match cass
                        .trigger_index(workspace.as_deref())
                        .await
                    {
                        Ok(index_result) => {
                            let sessions = index_result
                                .sessions_indexed
                                .unwrap_or(0);
                            let new_sessions = index_result
                                .new_sessions
                                .unwrap_or(0);
                            tracing::info!(
                                pane_id,
                                sessions,
                                new_sessions,
                                "handle_swarm_learning_index: cass index complete"
                            );
                            "index_complete"
                        }
                        Err(error) => {
                            tracing::warn!(
                                pane_id,
                                error = %error,
                                "handle_swarm_learning_index: cass index failed"
                            );
                            "index_error"
                        }
                    };

                    let timestamp_ms = now_ms();
                    let input_summary = format!(
                        "Swarm learning index for {agent_type} session end"
                    );
                    let mut context = new_workflow_handler_audit_context(
                        "swarm_learning_index",
                        "handle_swarm_learning_index",
                        &execution_id,
                        pane_id,
                        Some(&input_summary),
                        result_label,
                        timestamp_ms,
                    );
                    context.add_evidence("agent_type", agent_type);
                    add_optional_evidence(
                        &mut context,
                        "workspace",
                        workspace.as_deref(),
                    );

                    let audit = crate::storage::AuditActionRecord {
                        id: 0,
                        ts: timestamp_ms,
                        actor_kind: "workflow".to_string(),
                        actor_id: Some(execution_id),
                        correlation_id: None,
                        pane_id: Some(pane_id),
                        domain: None,
                        action_kind: "swarm_learning_index".to_string(),
                        policy_decision: "allow".to_string(),
                        decision_reason: None,
                        rule_id: None,
                        input_summary: Some(input_summary),
                        verification_summary: None,
                        decision_context: serialize_workflow_handler_audit_context(
                            &context,
                        ),
                        result: result_label.to_string(),
                    };

                    if let Err(error) = storage.record_audit_action(audit).await {
                        tracing::error!(
                            pane_id,
                            error = %error,
                            "handle_swarm_learning_index: audit recording failed"
                        );
                    }

                    StepResult::done(serde_json::json!({
                        "status": result_label,
                        "pane_id": pane_id,
                        "workspace": workspace,
                    }))
                }

                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

// ============================================================================
// HandleAuthRequired — centralize auth recovery (wa-nu4.2.2.4)
// ============================================================================

/// Default cooldown window in milliseconds (5 minutes).
/// Auth events within this window for the same pane are suppressed.
pub const AUTH_COOLDOWN_MS: i64 = 5 * 60 * 1000;
const AUTH_CASS_HINT_LIMIT: usize = 3;
const AUTH_CASS_TIMEOUT_SECS: u64 = 8;
const AUTH_CASS_LOOKBACK_DAYS: u32 = 30;
const AUTH_CASS_QUERY_MAX_CHARS: usize = 160;
const AUTH_CASS_HINT_MAX_CHARS: usize = 140;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthCassHintsLookup {
    pub query: Option<String>,
    pub workspace: Option<String>,
    pub hints: Vec<String>,
    pub error: Option<String>,
}

/// Recovery strategy for an auth-required event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "strategy")]
pub enum AuthRecoveryStrategy {
    /// Device code auth: user must enter code in browser.
    DeviceCode {
        code: Option<String>,
        url: Option<String>,
    },
    /// API key error: environment variable needs fixing.
    ApiKeyError { key_hint: Option<String> },
    /// Generic auth prompt requiring manual intervention.
    ManualIntervention { agent_type: String, hint: String },
}

impl AuthRecoveryStrategy {
    /// Determine recovery strategy from a detection trigger.
    pub fn from_detection(trigger: &serde_json::Value) -> Self {
        let event_type = trigger
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let rule_id = trigger
            .get("rule_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let extracted = trigger.get("extracted");
        let agent_type = trigger
            .get("agent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        if event_type == "auth.device_code" || rule_id.contains("device_code") {
            let code = extracted
                .and_then(|e| e.get("code"))
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            let url = extracted
                .and_then(|e| e.get("url"))
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            Self::DeviceCode { code, url }
        } else if event_type == "auth.error" || rule_id.contains("api_key") {
            let key_hint = extracted
                .and_then(|e| e.get("key_name"))
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            Self::ApiKeyError { key_hint }
        } else {
            Self::ManualIntervention {
                agent_type: agent_type.to_string(),
                hint: format!("Auth required for {agent_type}; manual login may be needed"),
            }
        }
    }

    /// Human-readable label for the strategy.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::DeviceCode { .. } => "device_code",
            Self::ApiKeyError { .. } => "api_key_error",
            Self::ManualIntervention { .. } => "manual_intervention",
        }
    }
}

/// Centralize auth-required events into a single workflow that selects the
/// correct recovery strategy, records the outcome, and avoids spamming.
pub struct HandleAuthRequired {
    cooldown_ms: i64,
}

impl HandleAuthRequired {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cooldown_ms: AUTH_COOLDOWN_MS,
        }
    }

    /// Create with a custom cooldown (useful for testing or configuration).
    #[allow(dead_code)]
    #[must_use]
    pub fn with_cooldown_ms(cooldown_ms: i64) -> Self {
        Self { cooldown_ms }
    }

    fn cass_agent_from_trigger(trigger: &serde_json::Value) -> Option<CassAgent> {
        trigger
            .get("agent_type")
            .and_then(|v| v.as_str())
            .and_then(CassAgent::from_slug)
    }

    fn compact_whitespace(input: &str) -> String {
        input.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn truncate_chars(input: &str, max_chars: usize) -> String {
        input.chars().take(max_chars).collect()
    }

    pub fn normalized_cass_query(trigger: &serde_json::Value) -> Option<String> {
        let candidates = [
            trigger.get("matched_text").and_then(|v| v.as_str()),
            trigger
                .get("extracted")
                .and_then(|v| v.get("message"))
                .and_then(|v| v.as_str()),
            trigger
                .get("extracted")
                .and_then(|v| v.get("error"))
                .and_then(|v| v.as_str()),
        ];

        for raw in candidates.into_iter().flatten() {
            let compact = Self::compact_whitespace(raw);
            if compact.is_empty() {
                continue;
            }
            return Some(Self::truncate_chars(&compact, AUTH_CASS_QUERY_MAX_CHARS));
        }

        let event_type = trigger
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let agent_type = trigger
            .get("agent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("agent");
        let fallback = format!("{agent_type} {event_type} auth error");
        Some(Self::truncate_chars(&fallback, AUTH_CASS_QUERY_MAX_CHARS))
    }

    fn workspace_for_pane(pane: &crate::storage::PaneRecord) -> Option<String> {
        let cwd = pane.cwd.as_deref()?;
        let parsed = crate::wezterm::CwdInfo::parse(cwd);
        if parsed.is_remote || parsed.path.is_empty() {
            return None;
        }
        Some(parsed.path)
    }

    fn format_cass_hint(hit: &CassSearchHit) -> Option<String> {
        let snippet = hit
            .content
            .as_deref()
            .map(Self::compact_whitespace)
            .unwrap_or_default();
        if snippet.is_empty() {
            return None;
        }

        let source_path = hit
            .source_path
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown");
        let line_suffix = hit
            .line_number
            .map_or_else(String::new, |line| format!(":{line}"));
        let compact_snippet = Self::truncate_chars(&snippet, AUTH_CASS_HINT_MAX_CHARS);

        Some(format!("{source_path}{line_suffix} - {compact_snippet}"))
    }

    async fn lookup_cass_hints(
        storage: &StorageHandle,
        pane_id: u64,
        trigger: &serde_json::Value,
    ) -> AuthCassHintsLookup {
        let query = Self::normalized_cass_query(trigger);
        let Some(query_value) = query.clone() else {
            return AuthCassHintsLookup::default();
        };

        let pane = match storage.get_pane(pane_id).await {
            Ok(record) => record,
            Err(error) => {
                return AuthCassHintsLookup {
                    query,
                    workspace: None,
                    hints: Vec::new(),
                    error: Some(format!("pane_lookup_failed: {error}")),
                };
            }
        };

        let workspace = pane.as_ref().and_then(Self::workspace_for_pane);

        let options = SearchOptions {
            limit: Some(AUTH_CASS_HINT_LIMIT),
            offset: None,
            agent: Self::cass_agent_from_trigger(trigger),
            workspace: workspace.clone(),
            days: Some(AUTH_CASS_LOOKBACK_DAYS),
            fields: Some("minimal".to_string()),
            max_tokens: Some(180),
        };

        let cass = CassClient::new().with_timeout_secs(AUTH_CASS_TIMEOUT_SECS);
        match cass.search(&query_value, &options).await {
            Ok(result) => {
                let hints = result
                    .hits
                    .iter()
                    .filter_map(Self::format_cass_hint)
                    .take(AUTH_CASS_HINT_LIMIT)
                    .collect();
                AuthCassHintsLookup {
                    query,
                    workspace,
                    hints,
                    error: None,
                }
            }
            Err(error) => AuthCassHintsLookup {
                query,
                workspace,
                hints: Vec::new(),
                error: Some(error.to_string()),
            },
        }
    }

    pub fn build_recovery_prompt(
        strategy: &AuthRecoveryStrategy,
        trigger: &serde_json::Value,
        cass_lookup: &AuthCassHintsLookup,
    ) -> String {
        let mut lines = vec![
            "Auth recovery needed for this pane.".to_string(),
            format!("Strategy: {}", strategy.label()),
        ];

        match strategy {
            AuthRecoveryStrategy::DeviceCode { code, url } => {
                if let Some(device_code) = code {
                    lines.push(format!("Device code: {device_code}"));
                }
                if let Some(login_url) = url {
                    lines.push(format!("Login URL: {login_url}"));
                }
                lines.push("Complete device auth in browser, then continue.".to_string());
            }
            AuthRecoveryStrategy::ApiKeyError { key_hint } => {
                if let Some(key_name) = key_hint {
                    lines.push(format!("Check API key variable: {key_name}"));
                } else {
                    lines.push("Check API key configuration for this agent.".to_string());
                }
                lines.push("Fix credentials, then retry the previous command.".to_string());
            }
            AuthRecoveryStrategy::ManualIntervention { hint, .. } => {
                lines.push(hint.clone());
            }
        }

        if !cass_lookup.hints.is_empty() {
            lines.push(String::new());
            lines.push("Related fixes from past sessions (cass):".to_string());
            for hint in &cass_lookup.hints {
                lines.push(format!("- {hint}"));
            }
        } else if let Some(error) = cass_lookup.error.as_deref() {
            lines.push(String::new());
            lines.push(format!("Cass lookup unavailable: {error}"));
        }

        if let Some(query) = cass_lookup.query.as_deref() {
            lines.push(String::new());
            lines.push(format!("Cass query: {query}"));
        }
        if let Some(workspace) = cass_lookup.workspace.as_deref() {
            lines.push(format!("Cass workspace filter: {workspace}"));
        }
        if let Some(matched_text) = trigger.get("matched_text").and_then(|v| v.as_str()) {
            let compact = Self::truncate_chars(&Self::compact_whitespace(matched_text), 120);
            if !compact.is_empty() {
                lines.push(format!("Detected text: {compact}"));
            }
        }

        let mut prompt = lines.join("\n");
        prompt.push('\n');
        prompt
    }
}

impl Default for HandleAuthRequired {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleAuthRequired {
    fn name(&self) -> &'static str {
        "handle_auth_required"
    }

    fn description(&self) -> &'static str {
        "Centralize auth-required events with strategy selection and cooldown"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        matches!(
            detection.event_type.as_str(),
            "auth.device_code" | "auth.error"
        )
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &["auth.device_code", "auth.error"]
    }

    fn supported_agent_types(&self) -> &'static [&'static str] {
        &["codex", "claude_code", "gemini"]
    }

    fn requires_pane(&self) -> bool {
        true
    }

    fn requires_approval(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        false
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new(
                "check_cooldown",
                "Skip if auth was recently handled for this pane",
            ),
            WorkflowStep::new("classify_auth", "Determine auth type and recovery strategy"),
            WorkflowStep::new(
                "record_and_plan",
                "Record auth event and produce recovery plan",
            ),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let trigger = ctx.trigger().cloned().unwrap_or(serde_json::Value::Null);
        let execution_id = ctx.execution_id().to_string();
        let cooldown_ms = self.cooldown_ms;

        Box::pin(async move {
            match step_idx {
                // Step 0: Check cooldown — query audit log for recent auth events
                0 => {
                    let since = now_ms() - cooldown_ms;
                    let query = crate::storage::AuditQuery {
                        pane_id: Some(pane_id),
                        action_kind: Some("auth_required".to_string()),
                        since: Some(since),
                        limit: Some(1),
                        ..Default::default()
                    };

                    match storage.get_audit_actions(query).await {
                        Ok(recent) if !recent.is_empty() => {
                            tracing::info!(
                                pane_id,
                                last_auth_ts = recent[0].ts,
                                "handle_auth_required: within cooldown, skipping"
                            );
                            StepResult::done(serde_json::json!({
                                "status": "cooldown_skipped",
                                "pane_id": pane_id,
                                "last_auth_ts": recent[0].ts,
                            }))
                        }
                        Ok(_) => {
                            tracing::debug!(
                                pane_id,
                                "handle_auth_required: no recent auth events, proceeding"
                            );
                            StepResult::cont()
                        }
                        Err(e) => {
                            // Non-fatal: if we can't check cooldown, proceed anyway
                            tracing::warn!(
                                pane_id,
                                error = %e,
                                "handle_auth_required: cooldown check failed, proceeding"
                            );
                            StepResult::cont()
                        }
                    }
                }

                // Step 1: Classify the auth event
                1 => {
                    let strategy = AuthRecoveryStrategy::from_detection(&trigger);
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let event_type = trigger
                        .get("event_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");

                    tracing::info!(
                        pane_id,
                        agent_type,
                        event_type,
                        strategy = strategy.label(),
                        "handle_auth_required: classified auth event"
                    );

                    StepResult::cont()
                }

                // Step 2: Record audit event, gather cass hints, and inject recovery prompt
                2 => {
                    let strategy = AuthRecoveryStrategy::from_detection(&trigger);
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let rule_id = trigger
                        .get("rule_id")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                    let event_type = trigger
                        .get("event_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let cass_lookup = Self::lookup_cass_hints(&storage, pane_id, &trigger).await;
                    let cass_hint_count = cass_lookup.hints.len();
                    let recovery_prompt =
                        Self::build_recovery_prompt(&strategy, &trigger, &cass_lookup);
                    let timestamp_ms = now_ms();
                    let input_summary =
                        format!("Auth required for {agent_type}: {}", strategy.label());
                    let decision_context = build_auth_required_audit_decision_context(
                        &execution_id,
                        pane_id,
                        agent_type,
                        event_type,
                        rule_id.as_deref(),
                        &strategy,
                        &cass_lookup,
                        &input_summary,
                        "recorded",
                        timestamp_ms,
                    );

                    // Record the auth event in the audit log
                    let audit = crate::storage::AuditActionRecord {
                        id: 0,
                        ts: timestamp_ms,
                        actor_kind: "workflow".to_string(),
                        actor_id: Some(execution_id.clone()),
                        correlation_id: None,
                        pane_id: Some(pane_id),
                        domain: None,
                        action_kind: "auth_required".to_string(),
                        policy_decision: "allow".to_string(),
                        decision_reason: None,
                        rule_id,
                        input_summary: Some(input_summary),
                        verification_summary: None,
                        decision_context,
                        result: "recorded".to_string(),
                    };

                    match storage.record_audit_action(audit).await {
                        Ok(audit_id) => {
                            tracing::info!(
                                pane_id,
                                audit_id,
                                strategy = strategy.label(),
                                "handle_auth_required: recorded auth event"
                            );
                            tracing::info!(
                                pane_id,
                                audit_id,
                                event_type,
                                cass_hint_count,
                                "handle_auth_required: injecting auth recovery prompt"
                            );
                            StepResult::send_text(recovery_prompt)
                        }
                        Err(e) => {
                            tracing::error!(
                                pane_id,
                                error = %e,
                                "handle_auth_required: failed to record auth event"
                            );
                            StepResult::abort(format!("Failed to record auth event: {e}"))
                        }
                    }
                }

                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

// ============================================================================
// HandleClaudeCodeLimits — safe-pause on Claude Code usage/rate limits
// (wa-03j, wa-nu4.2.2.1)
// ============================================================================

/// Default cooldown window in milliseconds (10 minutes).
/// Usage-limit events within this window for the same pane are suppressed.
const CLAUDE_CODE_LIMITS_COOLDOWN_MS: i64 = 10 * 60 * 1000;

/// Handle Claude Code usage-limit and rate-limit events.
///
/// Unlike the Codex-specific [`HandleUsageLimits`], this workflow does **not**
/// attempt account rotation or automated exit. Instead it:
///   1. Guards against unsafe pane states.
///   2. Applies a cooldown so repeated limit events don't spam actions.
///   3. Classifies the limit type (usage warning, usage reached, rate limit).
///   4. Persists an audit record and produces a recovery plan the operator
///      can act on (wait for reset, switch accounts manually, etc.).
pub struct HandleClaudeCodeLimits {
    pub cooldown_ms: i64,
}

impl HandleClaudeCodeLimits {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cooldown_ms: CLAUDE_CODE_LIMITS_COOLDOWN_MS,
        }
    }

    /// Create with a custom cooldown (useful for testing).
    #[allow(dead_code)]
    #[must_use]
    pub fn with_cooldown_ms(cooldown_ms: i64) -> Self {
        Self { cooldown_ms }
    }

    /// Classify the limit type from a detection trigger.
    pub fn classify_limit(trigger: &serde_json::Value) -> (&'static str, Option<String>) {
        let event_type = trigger
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let extracted = trigger.get("extracted");

        let reset_time = extracted
            .and_then(|e| e.get("reset_time"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .or_else(|| {
                extracted
                    .and_then(|e| e.get("retry_after"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
            });

        let limit_type = match event_type {
            "usage.warning" => "usage_warning",
            "usage.reached" => "usage_reached",
            "rate_limit.detected" => "rate_limit_detected",
            _ => "unknown_limit",
        };

        (limit_type, reset_time)
    }

    /// Build a recovery plan JSON object for the operator.
    pub fn build_recovery_plan(
        limit_type: &str,
        reset_time: Option<&str>,
        pane_id: u64,
    ) -> serde_json::Value {
        let next_steps = match limit_type {
            "usage_warning" => vec![
                "Save current work and commit progress",
                "Consider wrapping up the current task",
                "If approaching hard limit, start a new session",
            ],
            "usage_reached" => {
                let mut steps = vec![
                    "Session has hit its usage limit",
                    "Do not send further input to avoid wasted tokens",
                ];
                if reset_time.is_some() {
                    steps.push("Wait for the limit to reset (see reset_time)");
                }
                steps.push("Or start a new Claude Code session manually");
                steps
            }
            "rate_limit_detected" => {
                let mut steps = vec![
                    "Provider rate limit detected for this session",
                    "Do not send further input until throttling clears",
                ];
                if reset_time.is_some() {
                    steps.push("Wait until retry window expires (see reset_time)");
                }
                steps.push("Or switch to another account/session manually");
                steps
            }
            _ => vec!["Unknown limit type; check pane output for details"],
        };

        serde_json::json!({
            "limit_type": limit_type,
            "pane_id": pane_id,
            "reset_time": reset_time,
            "next_steps": next_steps,
            "safe_to_send": limit_type == "usage_warning",
        })
    }
}

impl Default for HandleClaudeCodeLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleClaudeCodeLimits {
    fn name(&self) -> &'static str {
        "handle_claude_code_limits"
    }

    fn description(&self) -> &'static str {
        "Safe-pause on Claude Code usage/rate limits with recovery plan"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        detection.agent_type == crate::patterns::AgentType::ClaudeCode
            && matches!(
                detection.event_type.as_str(),
                "usage.warning" | "usage.reached" | "rate_limit.detected"
            )
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &["usage.warning", "usage.reached", "rate_limit.detected"]
    }

    fn supported_agent_types(&self) -> &'static [&'static str] {
        &["claude_code"]
    }

    fn requires_pane(&self) -> bool {
        true
    }

    fn requires_approval(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        false
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new("check_guards", "Validate pane state allows interaction"),
            WorkflowStep::new(
                "check_cooldown",
                "Skip if usage limit was recently handled for this pane",
            ),
            WorkflowStep::new(
                "classify_and_record",
                "Classify limit type, record audit event, and build recovery plan",
            ),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let trigger = ctx.trigger().cloned().unwrap_or(serde_json::Value::Null);
        let execution_id = ctx.execution_id().to_string();
        let cooldown_ms = self.cooldown_ms;
        let caps = ctx.capabilities().to_owned();

        Box::pin(async move {
            match step_idx {
                // Step 0: Guard checks
                0 => {
                    if caps.alt_screen == Some(true) {
                        return StepResult::abort("Pane is in alt-screen mode");
                    }
                    if caps.command_running {
                        return StepResult::abort("Command is running in pane");
                    }
                    tracing::debug!(pane_id, "handle_claude_code_limits: guard checks passed");
                    StepResult::cont()
                }

                // Step 1: Cooldown check
                1 => {
                    let since = now_ms() - cooldown_ms;
                    let query = crate::storage::AuditQuery {
                        pane_id: Some(pane_id),
                        action_kind: Some("claude_code_usage_limit".to_string()),
                        since: Some(since),
                        limit: Some(1),
                        ..Default::default()
                    };

                    match storage.get_audit_actions(query).await {
                        Ok(recent) if !recent.is_empty() => {
                            tracing::info!(
                                pane_id,
                                last_limit_ts = recent[0].ts,
                                "handle_claude_code_limits: within cooldown, skipping"
                            );
                            StepResult::done(serde_json::json!({
                                "status": "cooldown_skipped",
                                "pane_id": pane_id,
                                "last_limit_ts": recent[0].ts,
                            }))
                        }
                        Ok(_) => {
                            tracing::debug!(
                                pane_id,
                                "handle_claude_code_limits: no recent limit events, proceeding"
                            );
                            StepResult::cont()
                        }
                        Err(e) => {
                            tracing::warn!(
                                pane_id,
                                error = %e,
                                "handle_claude_code_limits: cooldown check failed, proceeding"
                            );
                            StepResult::cont()
                        }
                    }
                }

                // Step 2: Classify limit, record audit event, build plan
                2 => {
                    let (limit_type, reset_time) = Self::classify_limit(&trigger);
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("claude_code");
                    let rule_id = trigger
                        .get("rule_id")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);

                    let plan =
                        Self::build_recovery_plan(limit_type, reset_time.as_deref(), pane_id);
                    let event_type = trigger
                        .get("event_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let timestamp_ms = now_ms();
                    let input_summary = format!("Claude Code {limit_type} on pane {pane_id}");
                    let decision_context = build_recovery_plan_audit_decision_context(
                        "claude_code_usage_limit",
                        "handle_claude_code_limits",
                        &execution_id,
                        pane_id,
                        agent_type,
                        event_type,
                        rule_id.as_deref(),
                        &input_summary,
                        "recorded",
                        "limit_type",
                        "reset_time",
                        &plan,
                        timestamp_ms,
                    );

                    // Record the limit event in the audit log
                    let audit = crate::storage::AuditActionRecord {
                        id: 0,
                        ts: timestamp_ms,
                        actor_kind: "workflow".to_string(),
                        actor_id: Some(execution_id.clone()),
                        correlation_id: None,
                        pane_id: Some(pane_id),
                        domain: None,
                        action_kind: "claude_code_usage_limit".to_string(),
                        policy_decision: "allow".to_string(),
                        decision_reason: None,
                        rule_id,
                        input_summary: Some(input_summary),
                        verification_summary: None,
                        decision_context,
                        result: "recorded".to_string(),
                    };

                    match storage.record_audit_action(audit).await {
                        Ok(audit_id) => {
                            tracing::info!(
                                pane_id,
                                audit_id,
                                limit_type,
                                reset_time = ?reset_time,
                                "handle_claude_code_limits: recorded usage limit event"
                            );

                            StepResult::done(serde_json::json!({
                                "status": "recorded",
                                "pane_id": pane_id,
                                "agent_type": agent_type,
                                "limit_type": limit_type,
                                "reset_time": reset_time,
                                "recovery_plan": plan,
                                "audit_id": audit_id,
                            }))
                        }
                        Err(e) => {
                            tracing::error!(
                                pane_id,
                                error = %e,
                                "handle_claude_code_limits: failed to record limit event"
                            );
                            StepResult::abort(format!("Failed to record usage limit event: {e}"))
                        }
                    }
                }

                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

// ============================================================================
// HandleGeminiQuota — safe-pause on Gemini usage/quota limits (wa-smm)
// ============================================================================

/// Default cooldown window in milliseconds (10 minutes).
const GEMINI_QUOTA_COOLDOWN_MS: i64 = 10 * 60 * 1000;

/// Handle Gemini usage-limit and quota events.
///
/// Similar to [`HandleClaudeCodeLimits`], this workflow does not attempt
/// automated account rotation. It:
///   1. Guards against unsafe pane states.
///   2. Applies a cooldown to avoid spamming on repeated events.
///   3. Classifies the quota type (warning vs reached/rate-limited).
///   4. Persists an audit record and produces a recovery plan.
pub struct HandleGeminiQuota {
    pub cooldown_ms: i64,
}

impl HandleGeminiQuota {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cooldown_ms: GEMINI_QUOTA_COOLDOWN_MS,
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn with_cooldown_ms(cooldown_ms: i64) -> Self {
        Self { cooldown_ms }
    }

    pub fn classify_quota(trigger: &serde_json::Value) -> (&'static str, Option<String>) {
        let event_type = trigger
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let extracted = trigger.get("extracted");

        let remaining = extracted
            .and_then(|e| e.get("remaining"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string);

        let quota_type = match event_type {
            "usage.warning" => "quota_warning",
            "usage.reached" => "quota_reached",
            "rate_limit.detected" => "quota_reached",
            _ => "unknown_quota",
        };

        (quota_type, remaining)
    }

    pub fn build_recovery_plan(
        quota_type: &str,
        remaining_pct: Option<&str>,
        pane_id: u64,
    ) -> serde_json::Value {
        let next_steps = match quota_type {
            "quota_warning" => vec![
                "Save current work and commit progress",
                "Consider switching to a non-Pro model if available",
                "Check quota reset time in Google AI Studio",
            ],
            "quota_reached" => vec![
                "Gemini Pro models quota is exhausted",
                "Do not send further input to avoid wasted requests",
                "Switch to a non-Pro model or wait for quota reset",
                "Or start a new session with a different Google account",
            ],
            _ => vec!["Unknown quota type; check pane output for details"],
        };

        serde_json::json!({
            "quota_type": quota_type,
            "pane_id": pane_id,
            "remaining_pct": remaining_pct,
            "next_steps": next_steps,
            "safe_to_send": quota_type == "quota_warning",
        })
    }
}

impl Default for HandleGeminiQuota {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleGeminiQuota {
    fn name(&self) -> &'static str {
        "handle_gemini_quota"
    }

    fn description(&self) -> &'static str {
        "Safe-pause on Gemini quota/usage limits with recovery plan"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        detection.agent_type == crate::patterns::AgentType::Gemini
            && matches!(
                detection.event_type.as_str(),
                "usage.warning" | "usage.reached" | "rate_limit.detected"
            )
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &["usage.warning", "usage.reached", "rate_limit.detected"]
    }

    fn supported_agent_types(&self) -> &'static [&'static str] {
        &["gemini"]
    }

    fn requires_pane(&self) -> bool {
        true
    }

    fn requires_approval(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        false
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new("check_guards", "Validate pane state allows interaction"),
            WorkflowStep::new(
                "check_cooldown",
                "Skip if quota event was recently handled for this pane",
            ),
            WorkflowStep::new(
                "classify_and_record",
                "Classify quota type, record audit event, and build recovery plan",
            ),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let trigger = ctx.trigger().cloned().unwrap_or(serde_json::Value::Null);
        let execution_id = ctx.execution_id().to_string();
        let cooldown_ms = self.cooldown_ms;
        let caps = ctx.capabilities().clone();

        Box::pin(async move {
            match step_idx {
                0 => {
                    if caps.alt_screen == Some(true) {
                        return StepResult::abort("Pane is in alt-screen mode");
                    }
                    if caps.command_running {
                        return StepResult::abort("Command is running in pane");
                    }
                    tracing::debug!(pane_id, "handle_gemini_quota: guard checks passed");
                    StepResult::cont()
                }

                1 => {
                    let since = now_ms() - cooldown_ms;
                    let query = crate::storage::AuditQuery {
                        pane_id: Some(pane_id),
                        action_kind: Some("gemini_quota_limit".to_string()),
                        since: Some(since),
                        limit: Some(1),
                        ..Default::default()
                    };

                    match storage.get_audit_actions(query).await {
                        Ok(recent) if !recent.is_empty() => {
                            tracing::info!(
                                pane_id,
                                last_quota_ts = recent[0].ts,
                                "handle_gemini_quota: within cooldown, skipping"
                            );
                            StepResult::done(serde_json::json!({
                                "status": "cooldown_skipped",
                                "pane_id": pane_id,
                                "last_quota_ts": recent[0].ts,
                            }))
                        }
                        Ok(_) => {
                            tracing::debug!(
                                pane_id,
                                "handle_gemini_quota: no recent quota events, proceeding"
                            );
                            StepResult::cont()
                        }
                        Err(e) => {
                            tracing::warn!(
                                pane_id,
                                error = %e,
                                "handle_gemini_quota: cooldown check failed, proceeding"
                            );
                            StepResult::cont()
                        }
                    }
                }

                2 => {
                    let (quota_type, remaining_pct) = Self::classify_quota(&trigger);
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("gemini");
                    let rule_id = trigger
                        .get("rule_id")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);

                    let plan =
                        Self::build_recovery_plan(quota_type, remaining_pct.as_deref(), pane_id);
                    let event_type = trigger
                        .get("event_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let timestamp_ms = now_ms();
                    let input_summary = format!("Gemini {quota_type} on pane {pane_id}");
                    let decision_context = build_recovery_plan_audit_decision_context(
                        "gemini_quota_limit",
                        "handle_gemini_quota",
                        &execution_id,
                        pane_id,
                        agent_type,
                        event_type,
                        rule_id.as_deref(),
                        &input_summary,
                        "recorded",
                        "quota_type",
                        "remaining_pct",
                        &plan,
                        timestamp_ms,
                    );

                    let audit = crate::storage::AuditActionRecord {
                        id: 0,
                        ts: timestamp_ms,
                        actor_kind: "workflow".to_string(),
                        actor_id: Some(execution_id.clone()),
                        correlation_id: None,
                        pane_id: Some(pane_id),
                        domain: None,
                        action_kind: "gemini_quota_limit".to_string(),
                        policy_decision: "allow".to_string(),
                        decision_reason: None,
                        rule_id,
                        input_summary: Some(input_summary),
                        verification_summary: None,
                        decision_context,
                        result: "recorded".to_string(),
                    };

                    match storage.record_audit_action(audit).await {
                        Ok(audit_id) => {
                            tracing::info!(
                                pane_id,
                                audit_id,
                                quota_type,
                                remaining_pct = ?remaining_pct,
                                "handle_gemini_quota: recorded quota event"
                            );

                            StepResult::done(serde_json::json!({
                                "status": "recorded",
                                "pane_id": pane_id,
                                "agent_type": agent_type,
                                "quota_type": quota_type,
                                "remaining_pct": remaining_pct,
                                "recovery_plan": plan,
                                "audit_id": audit_id,
                            }))
                        }
                        Err(e) => {
                            tracing::error!(
                                pane_id,
                                error = %e,
                                "handle_gemini_quota: failed to record quota event"
                            );
                            StepResult::abort(format!("Failed to record quota event: {e}"))
                        }
                    }
                }

                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

// ============================================================================
// Device Auth Workflow Step (wa-nu4.1.3.6)
// ============================================================================
//
// Integrates browser-based OpenAI device auth into the usage-limit failover
// workflow. This step:
//   1. Validates the device code format
//   2. Initializes a BrowserContext
//   3. Runs the OpenAiDeviceAuthFlow via Playwright
//   4. Returns a structured result mapping to workflow step outcomes

/// Result of executing the device auth workflow step.
///
/// Maps browser automation outcomes to workflow-level concepts.
#[cfg(feature = "browser")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status")]
pub enum DeviceAuthStepOutcome {
    /// Device auth completed successfully; profile is now authenticated.
    #[serde(rename = "authenticated")]
    Authenticated {
        /// Wall-clock time the flow took (ms).
        elapsed_ms: u64,
        /// Account used for auth.
        account: String,
    },

    /// Interactive bootstrap is required (password/MFA).
    ///
    /// The workflow should transition to the safe fallback path
    /// (wa-nu4.1.3.8) rather than retrying.
    #[serde(rename = "bootstrap_required")]
    BootstrapRequired {
        /// Why interactive login is needed.
        reason: String,
        /// Account that needs bootstrap.
        account: String,
        /// Path to failure artifacts, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        artifacts_dir: Option<std::path::PathBuf>,
    },

    /// Auth step failed with an error.
    #[serde(rename = "failed")]
    Failed {
        /// Human-readable error description.
        error: String,
        /// Error classification for programmatic handling.
        #[serde(skip_serializing_if = "Option::is_none")]
        error_kind: Option<String>,
        /// Path to failure artifacts, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        artifacts_dir: Option<std::path::PathBuf>,
    },
}

/// Execute the Playwright-based device auth flow as a workflow step.
///
/// This function:
/// 1. Validates the device code format (returns early on invalid codes)
/// 2. Initializes a BrowserContext from the given data directory
/// 3. Runs OpenAiDeviceAuthFlow with the persistent browser profile
/// 4. Maps the result to a [`DeviceAuthStepOutcome`]
///
/// # Arguments
///
/// * `device_code` - The device code from the Codex pane (e.g., "ABCD-EFGH").
/// * `account` - Account identifier for profile selection (e.g., "default").
/// * `data_dir` - Data directory containing browser profiles (typically `.ft/`).
/// * `artifacts_dir` - Optional directory for failure artifacts (screenshots, etc.).
/// * `headless` - Whether to run the browser in headless mode.
///
/// # Safety
///
/// - The device code is validated but **never logged** (secret material).
/// - Only the code format is checked, not the code content.
/// - Failure artifacts contain redacted DOM, never session tokens.
#[cfg(feature = "browser")]
pub fn execute_device_auth_step(
    device_code: &str,
    account: &str,
    data_dir: &std::path::Path,
    artifacts_dir: Option<&std::path::Path>,
    headless: bool,
) -> DeviceAuthStepOutcome {
    use crate::browser::openai_device::{AuthFlowResult, OpenAiDeviceAuthFlow};
    use crate::browser::{BrowserConfig, BrowserContext};

    // Step 1: Validate device code format before touching the browser
    if !validate_device_code(device_code) {
        return DeviceAuthStepOutcome::Failed {
            error: "Invalid device code format".into(),
            error_kind: Some("invalid_code".into()),
            artifacts_dir: None,
        };
    }

    // Step 2: Initialize browser context
    let config = BrowserConfig {
        headless,
        ..Default::default()
    };
    let mut ctx = BrowserContext::new(config, data_dir);

    if let Err(e) = ctx.ensure_ready() {
        return DeviceAuthStepOutcome::Failed {
            error: format!("Browser initialization failed: {e}"),
            error_kind: Some("browser_not_ready".into()),
            artifacts_dir: None,
        };
    }

    // Step 3: Run the device auth flow
    let mut flow = OpenAiDeviceAuthFlow::with_defaults();
    if let Some(dir) = artifacts_dir {
        flow = flow.with_artifacts(dir);
    }

    let result = flow.execute(&ctx, device_code, account, None);

    // Step 4: Map browser result to workflow outcome
    match result {
        AuthFlowResult::Success { elapsed_ms } => {
            tracing::info!(
                account = %account,
                elapsed_ms,
                "Device auth step: success"
            );
            // NOTE: device_code intentionally NOT logged
            DeviceAuthStepOutcome::Authenticated {
                elapsed_ms,
                account: account.to_string(),
            }
        }
        AuthFlowResult::InteractiveBootstrapRequired {
            reason,
            artifacts_dir: art_dir,
        } => {
            tracing::warn!(
                account = %account,
                reason = %reason,
                "Device auth step: interactive bootstrap required"
            );
            DeviceAuthStepOutcome::BootstrapRequired {
                reason,
                account: account.to_string(),
                artifacts_dir: art_dir,
            }
        }
        AuthFlowResult::Failed {
            error,
            kind,
            artifacts_dir: art_dir,
        } => {
            tracing::error!(
                account = %account,
                error = %error,
                kind = ?kind,
                "Device auth step: failed"
            );
            DeviceAuthStepOutcome::Failed {
                error,
                error_kind: Some(format!("{kind:?}")),
                artifacts_dir: art_dir,
            }
        }
    }
}

/// Convert a [`DeviceAuthStepOutcome`] to a [`StepResult`] for workflow integration.
///
/// Mapping:
/// - `Authenticated` → `StepResult::Continue` (proceed to resume step)
/// - `BootstrapRequired` → `StepResult::Abort` (enter fallback path)
/// - `Failed` → `StepResult::Abort` with error details
#[cfg(feature = "browser")]
pub fn device_auth_outcome_to_step_result(outcome: &DeviceAuthStepOutcome) -> StepResult {
    match outcome {
        DeviceAuthStepOutcome::Authenticated { .. } => {
            let json = serde_json::to_value(outcome).unwrap_or_default();
            StepResult::Done { result: json }
        }
        DeviceAuthStepOutcome::BootstrapRequired {
            reason, account, ..
        } => StepResult::Abort {
            reason: format!("Interactive bootstrap required for account '{account}': {reason}"),
        },
        DeviceAuthStepOutcome::Failed { error, .. } => StepResult::Abort {
            reason: format!("Device auth failed: {error}"),
        },
    }
}

// ============================================================================
// Resume Session Step (wa-nu4.1.3.7)
// ============================================================================
//
// After device auth completes, resume the Codex session with the saved
// session ID and send "proceed." to continue. Verifies the session is
// ready by waiting for a stable output marker.

/// Configuration for the resume session step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResumeSessionConfig {
    /// Template for the resume command. `{session_id}` is replaced at runtime.
    pub resume_command_template: String,
    /// Text to send after session resumes (triggers continuation).
    pub proceed_text: String,
    /// Wait for pane output to stabilize after sending the resume command (ms).
    pub post_resume_stable_ms: u64,
    /// Wait for pane output to stabilize after sending proceed (ms).
    pub post_proceed_stable_ms: u64,
    /// Maximum wait time for the resume command to take effect (ms).
    pub resume_timeout_ms: u64,
    /// Maximum wait time for the proceed signal to be acknowledged (ms).
    pub proceed_timeout_ms: u64,
}

impl Default for ResumeSessionConfig {
    fn default() -> Self {
        Self {
            resume_command_template: "cod resume {session_id}\n".to_string(),
            proceed_text: "proceed.\n".to_string(),
            post_resume_stable_ms: 3_000,
            post_proceed_stable_ms: 5_000,
            resume_timeout_ms: 30_000,
            proceed_timeout_ms: 30_000,
        }
    }
}

/// Outcome of the resume session step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum ResumeSessionOutcome {
    /// Session resumed and ready for continued interaction.
    #[serde(rename = "ready")]
    Ready {
        /// Session ID that was resumed.
        session_id: String,
    },

    /// Resume command sent but could not verify ready state within timeout.
    ///
    /// This is a soft failure — the session may still be resuming.
    #[serde(rename = "timeout")]
    VerifyTimeout {
        /// Session ID attempted.
        session_id: String,
        /// Which phase timed out ("resume" or "proceed").
        phase: String,
        /// Time waited (ms).
        waited_ms: u64,
    },

    /// Session could not be resumed due to an error.
    #[serde(rename = "failed")]
    Failed {
        /// Human-readable error description.
        error: String,
    },
}

/// Format the resume command for a given session ID.
///
/// Replaces `{session_id}` in the template with the actual session ID.
///
/// # Panics
///
/// None — if `{session_id}` is not in the template, the template is returned as-is.
#[allow(clippy::literal_string_with_formatting_args)]
const SESSION_ID_TOKEN: &str = "{session_id}";

#[must_use]
pub fn format_resume_command(session_id: &str, config: &ResumeSessionConfig) -> String {
    config
        .resume_command_template
        .replace(SESSION_ID_TOKEN, session_id)
}

/// Validate a session ID for resume.
///
/// Session IDs are hex UUIDs (e.g., "a1b2c3d4-e5f6-7890-abcd-ef1234567890").
/// Returns true if the ID has at least 8 hex characters separated by hyphens.
#[must_use]
pub fn validate_session_id(session_id: &str) -> bool {
    let trimmed = session_id.trim();
    if trimmed.len() < 8 {
        return false;
    }
    // Must contain only hex chars and hyphens
    trimmed.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Build the resume StepResult (sends resume command, waits for stable tail).
///
/// This returns a `StepResult::SendText` with a `StableTail` wait condition
/// so the workflow runner handles the policy-gated injection and wait.
#[must_use]
pub fn build_resume_step_result(session_id: &str, config: &ResumeSessionConfig) -> StepResult {
    let command = format_resume_command(session_id, config);
    StepResult::send_text_and_wait(
        command,
        WaitCondition::stable_tail(config.post_resume_stable_ms),
        config.resume_timeout_ms,
    )
}

/// Build the proceed StepResult (sends proceed text, waits for stable tail).
///
/// This returns a `StepResult::SendText` with a `StableTail` wait condition
/// so the workflow runner handles the policy-gated injection and wait.
#[must_use]
pub fn build_proceed_step_result(config: &ResumeSessionConfig) -> StepResult {
    StepResult::send_text_and_wait(
        config.proceed_text.clone(),
        WaitCondition::stable_tail(config.post_proceed_stable_ms),
        config.proceed_timeout_ms,
    )
}

/// Convert a [`ResumeSessionOutcome`] to a [`StepResult`] for workflow integration.
///
/// Mapping:
/// - `Ready` → `StepResult::Done` (workflow complete)
/// - `VerifyTimeout` → `StepResult::Done` with timeout info (non-fatal)
/// - `Failed` → `StepResult::Abort`
#[must_use]
pub fn resume_outcome_to_step_result(outcome: &ResumeSessionOutcome) -> StepResult {
    match outcome {
        ResumeSessionOutcome::Ready { .. } => {
            let json = serde_json::to_value(outcome).unwrap_or_default();
            StepResult::Done { result: json }
        }
        ResumeSessionOutcome::VerifyTimeout { .. } => {
            // Timeouts are soft failures — report but don't abort.
            // The session may still be resuming; let the caller decide.
            let json = serde_json::to_value(outcome).unwrap_or_default();
            StepResult::Done { result: json }
        }
        ResumeSessionOutcome::Failed { error } => StepResult::Abort {
            reason: format!("Resume session failed: {error}"),
        },
    }
}

// ============================================================================
// Safe Fallback Path (wa-nu4.1.3.8)
// ============================================================================

/// Why the safe fallback path was entered.
///
/// Captures the blocking condition that prevents full automated failover.
/// All variants carry enough context to build a structured next-step plan
/// without exposing secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum FallbackReason {
    /// Browser auth returned NeedsHuman (password, MFA, SSO).
    #[serde(rename = "needs_human_auth")]
    NeedsHumanAuth {
        /// Which account triggered the interactive requirement.
        account: String,
        /// Human-readable explanation (already redacted by caller).
        detail: String,
    },
    /// Failover is disabled in configuration.
    #[serde(rename = "failover_disabled")]
    FailoverDisabled,
    /// A required external tool is missing (caut, Playwright, etc.).
    #[serde(rename = "tool_missing")]
    ToolMissing {
        /// Name of the missing tool.
        tool: String,
    },
    /// Policy denied the injection (alt-screen, recent gap, unknown state).
    #[serde(rename = "policy_denied")]
    PolicyDenied {
        /// Policy rule that denied.
        rule: String,
    },
    /// All configured accounts have reached usage limits.
    #[serde(rename = "all_accounts_exhausted")]
    AllAccountsExhausted {
        /// Number of accounts checked.
        accounts_checked: u32,
    },
    /// A catch-all for unexpected blocking conditions.
    #[serde(rename = "other")]
    Other {
        /// Human-readable description.
        detail: String,
    },
}

impl std::fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedsHumanAuth { account, detail } => {
                write!(
                    f,
                    "Interactive auth required for account {account}: {detail}"
                )
            }
            Self::FailoverDisabled => write!(f, "Account failover is disabled in configuration"),
            Self::ToolMissing { tool } => write!(f, "Required tool not found: {tool}"),
            Self::PolicyDenied { rule } => write!(f, "Policy denied injection: {rule}"),
            Self::AllAccountsExhausted { accounts_checked } => {
                write!(
                    f,
                    "All {accounts_checked} configured account(s) at usage limit"
                )
            }
            Self::Other { detail } => write!(f, "{detail}"),
        }
    }
}

/// A structured next-step plan persisted when the safe fallback path activates.
///
/// This plan enables both human operators and downstream agents to understand
/// what happened and how to recover, without leaking secrets.
///
/// # Redaction
///
/// The caller is responsible for passing already-redacted values for any field
/// that might contain sensitive data (session IDs are opaque hashes, not tokens).
/// Use [`crate::policy::Redactor`] before constructing this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackNextStepPlan {
    /// Schema version for forward compatibility.
    pub version: u32,

    /// Why the fallback was entered.
    pub reason: FallbackReason,

    /// Pane ID where the usage-limit event was detected.
    pub pane_id: u64,

    /// Explicit steps the operator must take to recover.
    ///
    /// Each entry is a human-readable instruction, e.g.:
    /// - "Run `ft auth bootstrap --account openai-team` in a terminal"
    /// - "Wait for usage-limit reset (estimated 2024-03-15T12:00:00Z)"
    pub operator_steps: Vec<String>,

    /// When it is safe to retry automated failover (epoch ms), if known.
    ///
    /// Derived from the reset time parsed from the usage-limit transcript.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<i64>,

    /// Resume session ID, if available (opaque identifier, not a secret).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_session_id: Option<String>,

    /// Non-secret account identifier that was in use when the limit was hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,

    /// Suggested CLI commands the operator can run to resume or inspect.
    ///
    /// e.g., `["ft auth bootstrap --account openai-team", "ft events --pane 42"]`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_commands: Vec<String>,

    /// Timestamp when the plan was created (epoch ms).
    pub created_at_ms: i64,
}

impl FallbackNextStepPlan {
    /// Current schema version.
    pub const CURRENT_VERSION: u32 = 1;
}

/// Build a [`FallbackNextStepPlan`] for the "needs human auth" scenario.
///
/// This is the most common fallback: device auth returned `BootstrapRequired`
/// because password/MFA/SSO is needed.
#[must_use]
pub fn build_needs_human_auth_plan(
    pane_id: u64,
    account: &str,
    detail: &str,
    resume_session_id: Option<&str>,
    retry_after_ms: Option<i64>,
    now_ms: i64,
) -> FallbackNextStepPlan {
    let mut operator_steps = vec![format!(
        "Run `ft auth bootstrap --account {account}` to complete interactive login"
    )];

    if let Some(session_id) = resume_session_id {
        operator_steps.push(format!("After auth, resume with: cod resume {session_id}"));
    }

    if retry_after_ms.is_some() {
        operator_steps.push(
            "Alternatively, wait for the usage-limit reset and retry automatically".to_string(),
        );
    }

    let mut suggested_commands = vec![format!("ft auth bootstrap --account {account}")];
    suggested_commands.push(format!("ft events --pane {pane_id}"));

    FallbackNextStepPlan {
        version: FallbackNextStepPlan::CURRENT_VERSION,
        reason: FallbackReason::NeedsHumanAuth {
            account: account.to_string(),
            detail: detail.to_string(),
        },
        pane_id,
        operator_steps,
        retry_after_ms,
        resume_session_id: resume_session_id.map(ToString::to_string),
        account_id: Some(account.to_string()),
        suggested_commands,
        created_at_ms: now_ms,
    }
}

/// Build a [`FallbackNextStepPlan`] for the "failover disabled" scenario.
#[must_use]
pub fn build_failover_disabled_plan(
    pane_id: u64,
    resume_session_id: Option<&str>,
    retry_after_ms: Option<i64>,
    now_ms: i64,
) -> FallbackNextStepPlan {
    let mut operator_steps = vec![
        "Account failover is disabled. Enable it in ft config or handle manually.".to_string(),
    ];

    if let Some(session_id) = resume_session_id {
        operator_steps.push(format!("Resume manually with: cod resume {session_id}"));
    }

    if retry_after_ms.is_some() {
        operator_steps
            .push("Wait for the usage-limit reset time, then the session can retry.".to_string());
    }

    let mut suggested_commands = vec![format!("ft events --pane {pane_id}")];
    suggested_commands.push("ft config show".to_string());

    FallbackNextStepPlan {
        version: FallbackNextStepPlan::CURRENT_VERSION,
        reason: FallbackReason::FailoverDisabled,
        pane_id,
        operator_steps,
        retry_after_ms,
        resume_session_id: resume_session_id.map(ToString::to_string),
        account_id: None,
        suggested_commands,
        created_at_ms: now_ms,
    }
}

/// Build a [`FallbackNextStepPlan`] for the "tool missing" scenario.
#[must_use]
pub fn build_tool_missing_plan(pane_id: u64, tool: &str, now_ms: i64) -> FallbackNextStepPlan {
    FallbackNextStepPlan {
        version: FallbackNextStepPlan::CURRENT_VERSION,
        reason: FallbackReason::ToolMissing {
            tool: tool.to_string(),
        },
        pane_id,
        operator_steps: vec![
            format!("Install the required tool: {tool}"),
            "Re-run the workflow after installation.".to_string(),
        ],
        retry_after_ms: None,
        resume_session_id: None,
        account_id: None,
        suggested_commands: vec![format!("ft events --pane {pane_id}")],
        created_at_ms: now_ms,
    }
}

/// Build a [`FallbackNextStepPlan`] for the "all accounts exhausted" scenario.
#[must_use]
pub fn build_all_accounts_exhausted_plan(
    pane_id: u64,
    accounts_checked: u32,
    resume_session_id: Option<&str>,
    retry_after_ms: Option<i64>,
    now_ms: i64,
) -> FallbackNextStepPlan {
    let mut operator_steps = vec![format!(
        "All {accounts_checked} configured account(s) are at their usage limit."
    )];

    if retry_after_ms.is_some() {
        operator_steps.push(
            "Wait for usage-limit reset, then failover will retry automatically.".to_string(),
        );
    } else {
        operator_steps.push(
            "Check account limits with `ft accounts status` and add or rotate accounts."
                .to_string(),
        );
    }

    let mut suggested_commands = vec![
        "ft accounts status".to_string(),
        format!("ft events --pane {pane_id}"),
    ];

    if let Some(session_id) = resume_session_id {
        suggested_commands.push(format!("cod resume {session_id}"));
    }

    FallbackNextStepPlan {
        version: FallbackNextStepPlan::CURRENT_VERSION,
        reason: FallbackReason::AllAccountsExhausted { accounts_checked },
        pane_id,
        operator_steps,
        retry_after_ms,
        resume_session_id: resume_session_id.map(ToString::to_string),
        account_id: None,
        suggested_commands,
        created_at_ms: now_ms,
    }
}

/// Convert a [`FallbackNextStepPlan`] to a [`StepResult`] for workflow integration.
///
/// The plan is serialized into the `Done` result so it persists in step logs.
/// The workflow marks the originating event as **paused** (not failed), signalling
/// that automation stopped intentionally and recovery is documented.
#[must_use]
pub fn fallback_plan_to_step_result(plan: &FallbackNextStepPlan) -> StepResult {
    let mut result = serde_json::to_value(plan).unwrap_or_default();
    // Tag the result so downstream consumers can distinguish fallback from success.
    if let serde_json::Value::Object(ref mut map) = result {
        map.insert("fallback".to_string(), serde_json::Value::Bool(true));
    }
    StepResult::Done { result }
}

/// The handled-status string used when an event enters the safe fallback path.
///
/// Events marked with this status are excluded from `--unhandled` queries
/// (because `handled_at` is set) but carry distinct semantics from "completed".
pub const FALLBACK_HANDLED_STATUS: &str = "paused";

/// Check whether a [`StepResult::Done`] result represents a fallback plan
/// (as opposed to a normal successful completion).
#[must_use]
pub fn is_fallback_result(result: &StepResult) -> bool {
    match result {
        StepResult::Done { result } => result
            .as_object()
            .and_then(|m| m.get("fallback"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_audit_decision_context(serialized: Option<String>) -> crate::policy::DecisionContext {
        serde_json::from_str(&serialized.expect("decision context should serialize"))
            .expect("decision context should deserialize")
    }

    fn evidence_map(
        context: &crate::policy::DecisionContext,
    ) -> std::collections::HashMap<String, String> {
        context
            .evidence
            .iter()
            .map(|entry| (entry.key.clone(), entry.value.clone()))
            .collect()
    }

    // ========================================================================
    // HandleSessionEnd::record_from_detection
    // ========================================================================

    #[test]
    fn record_from_detection_extracts_all_fields() {
        let detection = serde_json::json!({
            "agent_type": "codex",
            "event_type": "session.summary",
            "extracted": {
                "session_id": "abc-123",
                "total": "10,000",
                "input": "5,000",
                "output": "3,000",
                "cached": "1,500",
                "reasoning": "500",
                "cost": "0.42",
                "model": "o3-pro"
            }
        });

        let record = HandleSessionEnd::record_from_detection(42, &detection);
        assert_eq!(record.pane_id, 42);
        assert_eq!(record.agent_type, "codex");
        assert_eq!(record.session_id.as_deref(), Some("abc-123"));
        assert_eq!(record.total_tokens, Some(10_000));
        assert_eq!(record.input_tokens, Some(5_000));
        assert_eq!(record.output_tokens, Some(3_000));
        assert_eq!(record.cached_tokens, Some(1_500));
        assert_eq!(record.reasoning_tokens, Some(500));
        assert!((record.estimated_cost_usd.unwrap() - 0.42).abs() < f64::EPSILON);
        assert_eq!(record.model_name.as_deref(), Some("o3-pro"));
        assert_eq!(record.end_reason.as_deref(), Some("completed"));
        assert!(record.ended_at.is_some());
    }

    #[test]
    fn record_from_detection_handles_missing_extracted() {
        let detection = serde_json::json!({
            "agent_type": "gemini",
            "event_type": "session.end"
        });

        let record = HandleSessionEnd::record_from_detection(10, &detection);
        assert_eq!(record.pane_id, 10);
        assert_eq!(record.agent_type, "gemini");
        assert_eq!(record.end_reason.as_deref(), Some("completed"));
        assert!(record.session_id.is_none());
        assert!(record.total_tokens.is_none());
        assert!(record.model_name.is_none());
    }

    #[test]
    fn record_from_detection_unknown_event_type() {
        let detection = serde_json::json!({
            "event_type": "session.crash"
        });

        let record = HandleSessionEnd::record_from_detection(1, &detection);
        assert_eq!(record.agent_type, "unknown");
        assert_eq!(record.end_reason.as_deref(), Some("session.crash"));
    }

    #[test]
    fn record_from_detection_partial_token_fields() {
        let detection = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "session.summary",
            "extracted": {
                "total": "1000",
                "cost": "not_a_number"
            }
        });

        let record = HandleSessionEnd::record_from_detection(5, &detection);
        assert_eq!(record.total_tokens, Some(1000));
        assert!(record.input_tokens.is_none());
        assert!(record.estimated_cost_usd.is_none()); // "not_a_number" fails parse
    }

    #[test]
    fn record_from_detection_accepts_numeric_json_fields() {
        let detection = serde_json::json!({
            "agent_type": "codex",
            "event_type": "session.summary",
            "extracted": {
                "total": 10_000,
                "input": 5_000,
                "output": 3_000,
                "cached": 1_500,
                "reasoning": 500,
                "cost": 0.42
            }
        });

        let record = HandleSessionEnd::record_from_detection(7, &detection);
        assert_eq!(record.total_tokens, Some(10_000));
        assert_eq!(record.input_tokens, Some(5_000));
        assert_eq!(record.output_tokens, Some(3_000));
        assert_eq!(record.cached_tokens, Some(1_500));
        assert_eq!(record.reasoning_tokens, Some(500));
        assert!(
            record
                .estimated_cost_usd
                .is_some_and(|v| (v - 0.42).abs() < f64::EPSILON)
        );
    }

    #[test]
    fn record_from_detection_parses_cost_with_thousands_separator() {
        let detection = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "session.summary",
            "extracted": {
                "cost": "1,234.56"
            }
        });

        let record = HandleSessionEnd::record_from_detection(8, &detection);
        assert!(
            record
                .estimated_cost_usd
                .is_some_and(|v| (v - 1234.56).abs() < f64::EPSILON)
        );
    }

    // ========================================================================
    // HandleSessionEnd trait methods
    // ========================================================================

    #[test]
    fn handle_session_end_name_and_description() {
        let h = HandleSessionEnd::new();
        assert_eq!(h.name(), "handle_session_end");
        assert!(!h.description().is_empty());
    }

    #[test]
    fn handle_session_end_trigger_event_types() {
        let h = HandleSessionEnd::new();
        let types = h.trigger_event_types();
        assert!(types.contains(&"session.summary"));
        assert!(types.contains(&"session.end"));
    }

    #[test]
    fn handle_session_end_supported_agent_types() {
        let h = HandleSessionEnd::new();
        let agents = h.supported_agent_types();
        assert!(agents.contains(&"codex"));
        assert!(agents.contains(&"claude_code"));
        assert!(agents.contains(&"gemini"));
    }

    #[test]
    fn handle_session_end_is_not_destructive() {
        let h = HandleSessionEnd::new();
        assert!(!h.is_destructive());
        assert!(!h.requires_approval());
        assert!(h.requires_pane());
    }

    #[test]
    fn handle_session_end_has_two_steps() {
        let h = HandleSessionEnd::new();
        let steps = h.steps();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].name, "extract_summary");
        assert_eq!(steps[1].name, "persist_record");
    }

    // ========================================================================
    // HandleSessionStartContext
    // ========================================================================

    #[test]
    fn handle_session_start_context_metadata() {
        let workflow = HandleSessionStartContext::new();
        assert_eq!(workflow.name(), "handle_session_start_context");
        assert_eq!(workflow.trigger_event_types(), ["session.start"]);
        assert!(!workflow.is_destructive());
        assert!(!workflow.requires_approval());
        assert!(workflow.requires_pane());
    }

    #[test]
    fn handle_session_start_context_handles_session_start_only() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleSessionStartContext::new();
        let session_start = Detection {
            rule_id: "claude_code.banner".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "session.start".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({"model":"claude-opus-4-6"}),
            matched_text: "Claude Code v1.0".to_string(),
            span: (0, 0),
        };
        assert!(workflow.handles(&session_start));

        let auth_error = Detection {
            event_type: "auth.error".to_string(),
            ..session_start
        };
        assert!(!workflow.handles(&auth_error));
    }

    #[test]
    fn handle_session_start_context_query_candidates_prefer_bead_id() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "session.start",
            "matched_text": "Claude Code v1.0",
            "extracted": {"model": "claude-opus-4-6"},
        });
        let pane = crate::storage::PaneRecord {
            pane_id: 77,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some("Working on ft-2l9kn cass integration".to_string()),
            cwd: Some("/Users/jemanuel/projects/frankenterm".to_string()),
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };

        let (candidates, bead_id) =
            HandleSessionStartContext::query_candidates(&trigger, Some(&pane));
        assert_eq!(bead_id.as_deref(), Some("ft-2l9kn"));
        assert!(
            candidates.iter().any(|entry| entry == "ft-2l9kn"),
            "expected bead id query candidate in {candidates:?}"
        );
    }

    #[test]
    fn handle_session_start_context_build_prompt_includes_hints() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "session.start",
            "extracted": {"model": "claude-opus-4-6"},
        });
        let lookup = SessionStartCassHintsLookup {
            query: Some("ft-2l9kn".to_string()),
            query_candidates: vec!["ft-2l9kn".to_string()],
            workspace: Some("/Users/jemanuel/projects/frankenterm".to_string()),
            hints: vec!["/tmp/session.md:42 - use rch for cargo".to_string()],
            error: None,
            bead_id: Some("ft-2l9kn".to_string()),
            pane_title: Some("ft-2l9kn".to_string()),
            pane_cwd: Some("/Users/jemanuel/projects/frankenterm".to_string()),
        };

        let prompt = HandleSessionStartContext::build_context_prompt(&trigger, &lookup);
        assert!(prompt.contains("Session startup context bootstrap."));
        assert!(prompt.contains("Related fixes from past sessions (cass):"));
        assert!(prompt.contains("ft robot cass search \"ft-2l9kn\" --limit 3"));
    }

    #[test]
    fn handle_session_start_context_build_prompt_no_hints_still_guides() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "session.start",
        });
        let lookup = SessionStartCassHintsLookup {
            query: Some("claude_code session startup context".to_string()),
            query_candidates: vec!["claude_code session startup context".to_string()],
            workspace: None,
            hints: vec![],
            error: None,
            bead_id: None,
            pane_title: None,
            pane_cwd: None,
        };

        let prompt = HandleSessionStartContext::build_context_prompt(&trigger, &lookup);
        assert!(prompt.contains("No direct cass matches found yet."));
        assert!(
            prompt.contains(
                "Keep AGENTS.md + README.md + current bead acceptance criteria in focus."
            )
        );
    }

    #[test]
    fn handle_session_start_context_build_prompt_escapes_query_quotes() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "session.start",
        });
        let lookup = SessionStartCassHintsLookup {
            query: Some("fix \"quoted\" regression".to_string()),
            query_candidates: vec!["fix \"quoted\" regression".to_string()],
            workspace: None,
            hints: vec![],
            error: None,
            bead_id: None,
            pane_title: None,
            pane_cwd: None,
        };

        let prompt = HandleSessionStartContext::build_context_prompt(&trigger, &lookup);
        assert!(
            prompt
                .contains("Try: ft robot cass search \"fix \\\"quoted\\\" regression\" --limit 3"),
            "prompt must include shell-safe quoted query: {prompt}"
        );
    }

    #[test]
    fn session_start_audit_decision_context_is_typed_and_preserves_lookup_fields() {
        let lookup = SessionStartCassHintsLookup {
            query: Some("ft-3681t.6.1".to_string()),
            query_candidates: vec![
                "ft-3681t.6.1".to_string(),
                "policy workflow audit".to_string(),
            ],
            workspace: Some("/Users/jemanuel/projects/frankenterm".to_string()),
            hints: vec!["notes/session.md:12 - keep workflow surface typed".to_string()],
            error: None,
            bead_id: Some("ft-3681t.6.1".to_string()),
            pane_title: Some("ft-3681t.6.1 policy work".to_string()),
            pane_cwd: Some("/Users/jemanuel/projects/frankenterm".to_string()),
        };

        let context = parse_audit_decision_context(build_session_start_audit_decision_context(
            "exec-1",
            42,
            "claude_code",
            "session.start",
            Some("claude_code.banner"),
            &lookup,
            "Session start context injection for claude_code",
            "hints_injected",
            1234,
        ));
        let evidence = evidence_map(&context);

        assert_eq!(context.timestamp_ms, 1234);
        assert_eq!(context.action, crate::policy::ActionKind::WorkflowRun);
        assert_eq!(context.actor, crate::policy::ActorKind::Workflow);
        assert_eq!(context.surface, crate::policy::PolicySurface::Workflow);
        assert_eq!(context.workflow_id.as_deref(), Some("exec-1"));
        assert_eq!(
            context.determining_rule.as_deref(),
            Some("audit.session_start_context")
        );
        assert_eq!(
            evidence.get("workflow_name").map(String::as_str),
            Some("handle_session_start_context")
        );
        assert_eq!(
            evidence
                .get("cass_query_candidate_count")
                .map(String::as_str),
            Some("2")
        );
        assert_eq!(
            evidence.get("cass_hint_0").map(String::as_str),
            Some("notes/session.md:12 - keep workflow surface typed")
        );
        assert_eq!(
            evidence.get("bead_id").map(String::as_str),
            Some("ft-3681t.6.1")
        );
    }

    #[test]
    fn auth_required_audit_decision_context_includes_strategy_and_cass_details() {
        let strategy = AuthRecoveryStrategy::DeviceCode {
            code: Some("ABCD-EFGH".to_string()),
            url: Some("https://example.test/device".to_string()),
        };
        let cass_lookup = AuthCassHintsLookup {
            query: Some("claude_code auth error".to_string()),
            workspace: Some("/Users/jemanuel/projects/frankenterm".to_string()),
            hints: vec!["notes/auth.md:9 - retry after browser login".to_string()],
            error: None,
        };

        let context = parse_audit_decision_context(build_auth_required_audit_decision_context(
            "exec-2",
            7,
            "claude_code",
            "auth.device_code",
            Some("claude_code.auth.device_code"),
            &strategy,
            &cass_lookup,
            "Auth required for claude_code: device_code",
            "recorded",
            5678,
        ));
        let evidence = evidence_map(&context);

        assert_eq!(
            context.determining_rule.as_deref(),
            Some("audit.auth_required")
        );
        assert_eq!(
            evidence.get("auth_strategy").map(String::as_str),
            Some("device_code")
        );
        assert_eq!(
            evidence.get("auth_device_code").map(String::as_str),
            Some("ABCD-EFGH")
        );
        assert_eq!(
            evidence.get("auth_device_url").map(String::as_str),
            Some("https://example.test/device")
        );
        assert_eq!(
            evidence.get("cass_hint_count").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn claude_limit_audit_decision_context_tracks_recovery_plan_fields() {
        let plan = HandleClaudeCodeLimits::build_recovery_plan(
            "usage_reached",
            Some("2026-03-07T05:00:00Z"),
            99,
        );

        let context = parse_audit_decision_context(build_recovery_plan_audit_decision_context(
            "claude_code_usage_limit",
            "handle_claude_code_limits",
            "exec-3",
            99,
            "claude_code",
            "usage.reached",
            Some("claude_code.usage.reached"),
            "Claude Code usage_reached on pane 99",
            "recorded",
            "limit_type",
            "reset_time",
            &plan,
            9012,
        ));
        let evidence = evidence_map(&context);

        assert_eq!(
            context.determining_rule.as_deref(),
            Some("audit.claude_code_usage_limit")
        );
        assert_eq!(
            evidence.get("limit_type").map(String::as_str),
            Some("usage_reached")
        );
        assert_eq!(
            evidence.get("reset_time").map(String::as_str),
            Some("2026-03-07T05:00:00Z")
        );
        assert_eq!(
            evidence.get("safe_to_send").map(String::as_str),
            Some("false")
        );
        assert_eq!(
            evidence.get("next_step_count").map(String::as_str),
            Some("4")
        );
    }

    #[test]
    fn gemini_quota_audit_decision_context_tracks_remaining_pct() {
        let plan = HandleGeminiQuota::build_recovery_plan("quota_warning", Some("12%"), 11);

        let context = parse_audit_decision_context(build_recovery_plan_audit_decision_context(
            "gemini_quota_limit",
            "handle_gemini_quota",
            "exec-4",
            11,
            "gemini",
            "usage.warning",
            Some("gemini.usage.warning"),
            "Gemini quota_warning on pane 11",
            "recorded",
            "quota_type",
            "remaining_pct",
            &plan,
            3456,
        ));
        let evidence = evidence_map(&context);

        assert_eq!(
            context.determining_rule.as_deref(),
            Some("audit.gemini_quota_limit")
        );
        assert_eq!(
            evidence.get("quota_type").map(String::as_str),
            Some("quota_warning")
        );
        assert_eq!(
            evidence.get("remaining_pct").map(String::as_str),
            Some("12%")
        );
        assert_eq!(
            evidence.get("safe_to_send").map(String::as_str),
            Some("true")
        );
    }

    // ========================================================================
    // ResumeSessionConfig defaults
    // ========================================================================

    #[test]
    fn resume_session_config_default() {
        let config = ResumeSessionConfig::default();
        assert!(config.resume_command_template.contains("{session_id}"));
        assert_eq!(config.proceed_text, "proceed.\n");
        assert_eq!(config.post_resume_stable_ms, 3_000);
        assert_eq!(config.post_proceed_stable_ms, 5_000);
        assert_eq!(config.resume_timeout_ms, 30_000);
        assert_eq!(config.proceed_timeout_ms, 30_000);
    }

    // ========================================================================
    // format_resume_command
    // ========================================================================

    #[test]
    fn format_resume_command_substitutes_session_id() {
        let config = ResumeSessionConfig::default();
        let cmd = format_resume_command("abc-def-123", &config);
        assert!(cmd.contains("abc-def-123"));
        assert!(!cmd.contains("{session_id}"));
    }

    #[test]
    fn format_resume_command_custom_template() {
        let config = ResumeSessionConfig {
            resume_command_template: "resume --session={session_id} --force\n".to_string(),
            ..Default::default()
        };
        let cmd = format_resume_command("sess-42", &config);
        assert_eq!(cmd, "resume --session=sess-42 --force\n");
    }

    #[test]
    fn format_resume_command_no_placeholder() {
        let config = ResumeSessionConfig {
            resume_command_template: "static_command\n".to_string(),
            ..Default::default()
        };
        let cmd = format_resume_command("ignored", &config);
        assert_eq!(cmd, "static_command\n");
    }

    // ========================================================================
    // validate_session_id
    // ========================================================================

    #[test]
    fn validate_session_id_valid_uuid() {
        assert!(validate_session_id("a1b2c3d4-e5f6-7890-abcd-ef1234567890"));
    }

    #[test]
    fn validate_session_id_short_hex() {
        assert!(validate_session_id("abcdef12"));
    }

    #[test]
    fn validate_session_id_too_short() {
        assert!(!validate_session_id("abc"));
        assert!(!validate_session_id("1234567")); // 7 chars
    }

    #[test]
    fn validate_session_id_exactly_eight() {
        assert!(validate_session_id("12345678"));
    }

    #[test]
    fn validate_session_id_non_hex_chars() {
        assert!(!validate_session_id("abcdefg1-2345-6789")); // 'g' is not hex
    }

    #[test]
    fn validate_session_id_empty() {
        assert!(!validate_session_id(""));
    }

    #[test]
    fn validate_session_id_whitespace_trimmed() {
        assert!(validate_session_id("  abcdef12  "));
    }

    #[test]
    fn validate_session_id_special_chars() {
        assert!(!validate_session_id("abc!def@12345678"));
    }

    // ========================================================================
    // build_resume_step_result
    // ========================================================================

    #[test]
    fn build_resume_step_result_is_send_text() {
        let config = ResumeSessionConfig::default();
        let result = build_resume_step_result("sess-123", &config);
        match &result {
            StepResult::SendText {
                text,
                wait_for,
                wait_timeout_ms,
            } => {
                assert!(text.contains("sess-123"));
                assert!(wait_for.is_some());
                assert_eq!(*wait_timeout_ms, Some(config.resume_timeout_ms));
            }
            other => panic!("Expected SendText, got {other:?}"),
        }
    }

    // ========================================================================
    // build_proceed_step_result
    // ========================================================================

    #[test]
    fn build_proceed_step_result_sends_proceed_text() {
        let config = ResumeSessionConfig::default();
        let result = build_proceed_step_result(&config);
        match &result {
            StepResult::SendText {
                text,
                wait_for,
                wait_timeout_ms,
            } => {
                assert_eq!(text, "proceed.\n");
                assert!(wait_for.is_some());
                assert_eq!(*wait_timeout_ms, Some(config.proceed_timeout_ms));
            }
            other => panic!("Expected SendText, got {other:?}"),
        }
    }

    // ========================================================================
    // resume_outcome_to_step_result
    // ========================================================================

    #[test]
    fn resume_outcome_ready_returns_done() {
        let outcome = ResumeSessionOutcome::Ready {
            session_id: "sess-1".to_string(),
        };
        let result = resume_outcome_to_step_result(&outcome);
        match &result {
            StepResult::Done { result } => {
                assert_eq!(result["status"], "ready");
                assert_eq!(result["session_id"], "sess-1");
            }
            other => panic!("Expected Done, got {other:?}"),
        }
    }

    #[test]
    fn resume_outcome_timeout_returns_done() {
        let outcome = ResumeSessionOutcome::VerifyTimeout {
            session_id: "sess-2".to_string(),
            phase: "resume".to_string(),
            waited_ms: 30_000,
        };
        let result = resume_outcome_to_step_result(&outcome);
        match &result {
            StepResult::Done { result } => {
                assert_eq!(result["status"], "timeout");
                assert_eq!(result["phase"], "resume");
            }
            other => panic!("Expected Done, got {other:?}"),
        }
    }

    #[test]
    fn resume_outcome_failed_returns_abort() {
        let outcome = ResumeSessionOutcome::Failed {
            error: "connection lost".to_string(),
        };
        let result = resume_outcome_to_step_result(&outcome);
        match &result {
            StepResult::Abort { reason } => {
                assert!(reason.contains("connection lost"));
            }
            other => panic!("Expected Abort, got {other:?}"),
        }
    }

    // ========================================================================
    // FallbackReason Display
    // ========================================================================

    #[test]
    fn fallback_reason_needs_human_auth_display() {
        let reason = FallbackReason::NeedsHumanAuth {
            account: "openai-team".to_string(),
            detail: "MFA required".to_string(),
        };
        let s = reason.to_string();
        assert!(s.contains("openai-team"));
        assert!(s.contains("MFA required"));
    }

    #[test]
    fn fallback_reason_failover_disabled_display() {
        let reason = FallbackReason::FailoverDisabled;
        assert!(reason.to_string().contains("disabled"));
    }

    #[test]
    fn fallback_reason_tool_missing_display() {
        let reason = FallbackReason::ToolMissing {
            tool: "playwright".to_string(),
        };
        assert!(reason.to_string().contains("playwright"));
    }

    #[test]
    fn fallback_reason_policy_denied_display() {
        let reason = FallbackReason::PolicyDenied {
            rule: "alt_screen_active".to_string(),
        };
        assert!(reason.to_string().contains("alt_screen_active"));
    }

    #[test]
    fn fallback_reason_all_accounts_exhausted_display() {
        let reason = FallbackReason::AllAccountsExhausted {
            accounts_checked: 3,
        };
        let s = reason.to_string();
        assert!(s.contains("3"));
        assert!(s.contains("limit"));
    }

    #[test]
    fn fallback_reason_other_display() {
        let reason = FallbackReason::Other {
            detail: "unexpected condition".to_string(),
        };
        assert_eq!(reason.to_string(), "unexpected condition");
    }

    // ========================================================================
    // FallbackReason serde roundtrip
    // ========================================================================

    #[test]
    fn fallback_reason_serde_roundtrip() {
        let variants = vec![
            FallbackReason::NeedsHumanAuth {
                account: "acct".to_string(),
                detail: "mfa".to_string(),
            },
            FallbackReason::FailoverDisabled,
            FallbackReason::ToolMissing {
                tool: "caut".to_string(),
            },
            FallbackReason::PolicyDenied {
                rule: "rule1".to_string(),
            },
            FallbackReason::AllAccountsExhausted {
                accounts_checked: 5,
            },
            FallbackReason::Other {
                detail: "misc".to_string(),
            },
        ];

        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let back: FallbackReason = serde_json::from_str(&json).unwrap();
            assert_eq!(variant.to_string(), back.to_string());
        }
    }

    // ========================================================================
    // build_needs_human_auth_plan
    // ========================================================================

    #[test]
    fn build_needs_human_auth_plan_basic() {
        let plan = build_needs_human_auth_plan(42, "openai-team", "MFA needed", None, None, 1000);
        assert_eq!(plan.version, FallbackNextStepPlan::CURRENT_VERSION);
        assert_eq!(plan.pane_id, 42);
        assert_eq!(plan.created_at_ms, 1000);
        assert!(plan.resume_session_id.is_none());
        assert!(plan.retry_after_ms.is_none());
        assert_eq!(plan.account_id.as_deref(), Some("openai-team"));
        assert!(!plan.operator_steps.is_empty());
        assert!(plan.operator_steps[0].contains("bootstrap"));
    }

    #[test]
    fn build_needs_human_auth_plan_with_session_and_retry() {
        let plan = build_needs_human_auth_plan(
            42,
            "openai-team",
            "MFA needed",
            Some("sess-abc"),
            Some(999_999),
            2000,
        );
        assert_eq!(plan.resume_session_id.as_deref(), Some("sess-abc"));
        assert_eq!(plan.retry_after_ms, Some(999_999));
        // Should have extra operator steps for resume and retry
        assert!(plan.operator_steps.len() >= 3);
        assert!(plan.operator_steps.iter().any(|s| s.contains("resume")));
    }

    // ========================================================================
    // build_failover_disabled_plan
    // ========================================================================

    #[test]
    fn build_failover_disabled_plan_basic() {
        let plan = build_failover_disabled_plan(10, None, None, 3000);
        assert_eq!(plan.pane_id, 10);
        assert!(plan.account_id.is_none());
        match &plan.reason {
            FallbackReason::FailoverDisabled => {}
            other => panic!("Expected FailoverDisabled, got {other:?}"),
        }
        assert!(plan.suggested_commands.iter().any(|s| s.contains("config")));
    }

    #[test]
    fn build_failover_disabled_plan_with_resume() {
        let plan = build_failover_disabled_plan(10, Some("sess-xyz"), None, 3000);
        assert_eq!(plan.resume_session_id.as_deref(), Some("sess-xyz"));
        assert!(plan.operator_steps.iter().any(|s| s.contains("resume")));
    }

    // ========================================================================
    // build_tool_missing_plan
    // ========================================================================

    #[test]
    fn build_tool_missing_plan_basic() {
        let plan = build_tool_missing_plan(99, "playwright", 5000);
        assert_eq!(plan.pane_id, 99);
        assert_eq!(plan.created_at_ms, 5000);
        match &plan.reason {
            FallbackReason::ToolMissing { tool } => assert_eq!(tool, "playwright"),
            other => panic!("Expected ToolMissing, got {other:?}"),
        }
        assert!(plan.operator_steps[0].contains("playwright"));
        assert!(plan.resume_session_id.is_none());
        assert!(plan.retry_after_ms.is_none());
    }

    // ========================================================================
    // build_all_accounts_exhausted_plan
    // ========================================================================

    #[test]
    fn build_all_accounts_exhausted_plan_no_retry() {
        let plan = build_all_accounts_exhausted_plan(7, 3, None, None, 6000);
        assert_eq!(plan.pane_id, 7);
        match &plan.reason {
            FallbackReason::AllAccountsExhausted { accounts_checked } => {
                assert_eq!(*accounts_checked, 3);
            }
            other => panic!("Expected AllAccountsExhausted, got {other:?}"),
        }
        assert!(plan.operator_steps.iter().any(|s| s.contains("accounts")));
    }

    #[test]
    fn build_all_accounts_exhausted_plan_with_retry_and_session() {
        let plan = build_all_accounts_exhausted_plan(7, 2, Some("sess-z"), Some(100_000), 6000);
        assert_eq!(plan.retry_after_ms, Some(100_000));
        assert_eq!(plan.resume_session_id.as_deref(), Some("sess-z"));
        assert!(plan.suggested_commands.iter().any(|s| s.contains("resume")));
    }

    // ========================================================================
    // fallback_plan_to_step_result
    // ========================================================================

    #[test]
    fn fallback_plan_to_step_result_is_done_with_fallback_tag() {
        let plan = build_tool_missing_plan(1, "caut", 1000);
        let result = fallback_plan_to_step_result(&plan);
        match &result {
            StepResult::Done { result } => {
                assert_eq!(result["fallback"], true);
                assert_eq!(result["version"], 1);
                assert_eq!(result["pane_id"], 1);
            }
            other => panic!("Expected Done, got {other:?}"),
        }
    }

    #[test]
    fn fallback_plan_preserves_reason_in_json() {
        let plan = build_needs_human_auth_plan(5, "acct", "pw", None, None, 100);
        let result = fallback_plan_to_step_result(&plan);
        if let StepResult::Done { result } = &result {
            assert_eq!(result["reason"]["kind"], "needs_human_auth");
            assert_eq!(result["reason"]["account"], "acct");
        }
    }

    // ========================================================================
    // is_fallback_result
    // ========================================================================

    #[test]
    fn is_fallback_result_true_for_fallback_plan() {
        let plan = build_tool_missing_plan(1, "caut", 1000);
        let result = fallback_plan_to_step_result(&plan);
        assert!(is_fallback_result(&result));
    }

    #[test]
    fn is_fallback_result_false_for_normal_done() {
        let result = StepResult::Done {
            result: serde_json::json!({"status": "ok"}),
        };
        assert!(!is_fallback_result(&result));
    }

    #[test]
    fn is_fallback_result_false_for_abort() {
        let result = StepResult::Abort {
            reason: "error".to_string(),
        };
        assert!(!is_fallback_result(&result));
    }

    #[test]
    fn is_fallback_result_false_for_continue() {
        assert!(!is_fallback_result(&StepResult::Continue));
    }

    #[test]
    fn is_fallback_result_false_for_done_without_fallback_key() {
        let result = StepResult::Done {
            result: serde_json::json!({}),
        };
        assert!(!is_fallback_result(&result));
    }

    #[test]
    fn is_fallback_result_false_for_done_with_false_fallback() {
        let result = StepResult::Done {
            result: serde_json::json!({"fallback": false}),
        };
        assert!(!is_fallback_result(&result));
    }

    // ========================================================================
    // FallbackNextStepPlan serde roundtrip
    // ========================================================================

    #[test]
    fn fallback_plan_serde_roundtrip() {
        let plan = build_needs_human_auth_plan(
            42,
            "acct-1",
            "MFA needed",
            Some("sess-42"),
            Some(999_999),
            12345,
        );
        let json = serde_json::to_string(&plan).unwrap();
        let back: FallbackNextStepPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, plan.version);
        assert_eq!(back.pane_id, plan.pane_id);
        assert_eq!(back.created_at_ms, plan.created_at_ms);
        assert_eq!(back.resume_session_id, plan.resume_session_id);
        assert_eq!(back.retry_after_ms, plan.retry_after_ms);
        assert_eq!(back.operator_steps.len(), plan.operator_steps.len());
    }

    // ========================================================================
    // ResumeSessionOutcome serde roundtrip
    // ========================================================================

    #[test]
    fn resume_session_outcome_serde_roundtrip() {
        let variants: Vec<ResumeSessionOutcome> = vec![
            ResumeSessionOutcome::Ready {
                session_id: "s1".to_string(),
            },
            ResumeSessionOutcome::VerifyTimeout {
                session_id: "s2".to_string(),
                phase: "proceed".to_string(),
                waited_ms: 30_000,
            },
            ResumeSessionOutcome::Failed {
                error: "conn err".to_string(),
            },
        ];

        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let back: ResumeSessionOutcome = serde_json::from_str(&json).unwrap();
            // Verify the status tag survives roundtrip
            let original_json: serde_json::Value = serde_json::from_str(&json).unwrap();
            let back_json = serde_json::to_value(&back).unwrap();
            assert_eq!(original_json["status"], back_json["status"]);
        }
    }

    // ========================================================================
    // FALLBACK_HANDLED_STATUS constant
    // ========================================================================

    #[test]
    fn fallback_handled_status_is_paused() {
        assert_eq!(FALLBACK_HANDLED_STATUS, "paused");
    }

    // ========================================================================
    // HandleProcessTriageLifecycle tests
    // ========================================================================

    #[test]
    fn handle_process_triage_lifecycle_metadata() {
        let workflow = HandleProcessTriageLifecycle::new();
        assert_eq!(workflow.name(), "handle_process_triage_lifecycle");
        assert_eq!(
            workflow.description(),
            "Orchestrate process triage lifecycle phases (snapshot, plan, apply, verify, diff, session)"
        );
        assert!(!workflow.is_destructive());
        assert!(!workflow.requires_approval());
        assert_eq!(workflow.steps().len(), 6);
    }

    #[test]
    fn handle_process_triage_lifecycle_step_names() {
        let workflow = HandleProcessTriageLifecycle::new();
        let steps = workflow.steps();
        assert_eq!(steps[0].name, "snapshot");
        assert_eq!(steps[1].name, "plan");
        assert_eq!(steps[2].name, "apply");
        assert_eq!(steps[3].name, "verify");
        assert_eq!(steps[4].name, "diff");
        assert_eq!(steps[5].name, "session");
    }

    #[test]
    fn handle_process_triage_lifecycle_handles_event_type() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleProcessTriageLifecycle::new();

        let triage_event = Detection {
            rule_id: "process_triage.lifecycle".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "process_triage.lifecycle".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "triage".to_string(),
            span: (0, 0),
        };
        assert!(workflow.handles(&triage_event));
    }

    #[test]
    fn handle_process_triage_lifecycle_handles_rule_id() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleProcessTriageLifecycle::new();

        let by_rule = Detection {
            rule_id: "process_triage.lifecycle".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "other.event_type".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "triage".to_string(),
            span: (0, 0),
        };
        assert!(workflow.handles(&by_rule));
    }

    #[test]
    fn handle_process_triage_lifecycle_rejects_unrelated_events() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleProcessTriageLifecycle::new();

        let unrelated = Detection {
            rule_id: "claude_code.session.end".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "session.end".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "end".to_string(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&unrelated));
    }

    #[test]
    fn handle_process_triage_lifecycle_trigger_event_types() {
        let workflow = HandleProcessTriageLifecycle::new();
        let types = workflow.trigger_event_types();
        assert_eq!(types, &["process_triage.lifecycle"]);
    }

    #[test]
    fn handle_process_triage_lifecycle_trigger_rule_ids() {
        let workflow = HandleProcessTriageLifecycle::new();
        let rule_ids = workflow.trigger_rule_ids();
        assert_eq!(rule_ids, &["process_triage.lifecycle"]);
    }

    // ========================================================================
    // HandleAuthRequired tests
    // ========================================================================

    #[test]
    fn handle_auth_required_metadata() {
        let workflow = HandleAuthRequired::new();
        assert_eq!(workflow.name(), "handle_auth_required");
        assert_eq!(
            workflow.description(),
            "Centralize auth-required events with strategy selection and cooldown"
        );
        assert!(!workflow.is_destructive());
        assert!(!workflow.requires_approval());
        assert!(workflow.requires_pane());
        assert_eq!(workflow.steps().len(), 3);
    }

    #[test]
    fn handle_auth_required_step_names() {
        let workflow = HandleAuthRequired::new();
        let steps = workflow.steps();
        assert_eq!(steps[0].name, "check_cooldown");
        assert_eq!(steps[1].name, "classify_auth");
        assert_eq!(steps[2].name, "record_and_plan");
    }

    #[test]
    fn handle_auth_required_handles_auth_events() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleAuthRequired::new();

        let device_code = Detection {
            rule_id: "claude_code.auth.device_code".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "auth.device_code".to_string(),
            severity: Severity::Warning,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "device code".to_string(),
            span: (0, 0),
        };
        assert!(workflow.handles(&device_code));

        let auth_error = Detection {
            event_type: "auth.error".to_string(),
            ..device_code.clone()
        };
        assert!(workflow.handles(&auth_error));
    }

    #[test]
    fn handle_auth_required_rejects_non_auth_events() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleAuthRequired::new();

        let session_event = Detection {
            rule_id: "claude_code.session.start".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "session.start".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "session start".to_string(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&session_event));

        let error_event = Detection {
            event_type: "error.network".to_string(),
            ..session_event
        };
        assert!(!workflow.handles(&error_event));
    }

    #[test]
    fn handle_auth_required_supported_agent_types() {
        let workflow = HandleAuthRequired::new();
        let types = workflow.supported_agent_types();
        assert!(types.contains(&"codex"));
        assert!(types.contains(&"claude_code"));
        assert!(types.contains(&"gemini"));
    }

    #[test]
    fn handle_auth_required_with_cooldown() {
        let workflow = HandleAuthRequired::with_cooldown_ms(2000);
        assert_eq!(workflow.cooldown_ms, 2000);
        assert_eq!(workflow.name(), "handle_auth_required");
    }

    #[test]
    fn handle_auth_required_default_cooldown() {
        let workflow = HandleAuthRequired::default();
        assert_eq!(workflow.cooldown_ms, AUTH_COOLDOWN_MS);
        assert_eq!(workflow.cooldown_ms, 5 * 60 * 1000);
    }

    // ========================================================================
    // HandleClaudeCodeLimits tests
    // ========================================================================

    #[test]
    fn handle_claude_code_limits_metadata() {
        let workflow = HandleClaudeCodeLimits::new();
        assert_eq!(workflow.name(), "handle_claude_code_limits");
        assert_eq!(
            workflow.description(),
            "Safe-pause on Claude Code usage/rate limits with recovery plan"
        );
        assert!(!workflow.is_destructive());
        assert!(!workflow.requires_approval());
        assert!(workflow.requires_pane());
        assert_eq!(workflow.steps().len(), 3);
    }

    #[test]
    fn handle_claude_code_limits_step_names() {
        let workflow = HandleClaudeCodeLimits::new();
        let steps = workflow.steps();
        assert_eq!(steps[0].name, "check_guards");
        assert_eq!(steps[1].name, "check_cooldown");
        assert_eq!(steps[2].name, "classify_and_record");
    }

    #[test]
    fn handle_claude_code_limits_handles_claude_code_events() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleClaudeCodeLimits::new();

        let usage_reached = Detection {
            rule_id: "claude_code.usage.reached".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "Usage limit reached".to_string(),
            span: (0, 0),
        };
        assert!(workflow.handles(&usage_reached));

        let usage_warning = Detection {
            event_type: "usage.warning".to_string(),
            ..usage_reached.clone()
        };
        assert!(workflow.handles(&usage_warning));

        let rate_limit = Detection {
            event_type: "rate_limit.detected".to_string(),
            ..usage_reached.clone()
        };
        assert!(workflow.handles(&rate_limit));
    }

    #[test]
    fn handle_claude_code_limits_rejects_non_claude_code_agent() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleClaudeCodeLimits::new();

        // Same event type but from Codex — should NOT match
        let codex_usage = Detection {
            rule_id: "codex.usage.reached".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "Usage limit".to_string(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&codex_usage));
    }

    #[test]
    fn handle_claude_code_limits_rejects_unrelated_event_types() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleClaudeCodeLimits::new();

        let session_end = Detection {
            rule_id: "claude_code.session.end".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "session.end".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "Session ended".to_string(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&session_end));
    }

    #[test]
    fn handle_claude_code_limits_with_cooldown() {
        let workflow = HandleClaudeCodeLimits::with_cooldown_ms(3000);
        assert_eq!(workflow.cooldown_ms, 3000);
    }

    #[test]
    fn handle_claude_code_limits_default_cooldown() {
        let workflow = HandleClaudeCodeLimits::default();
        assert_eq!(workflow.cooldown_ms, CLAUDE_CODE_LIMITS_COOLDOWN_MS);
        assert_eq!(workflow.cooldown_ms, 10 * 60 * 1000);
    }

    #[test]
    fn handle_claude_code_limits_classify_usage_reached() {
        let trigger = serde_json::json!({
            "event_type": "usage.reached",
            "extracted": {"reset_time": "2026-03-14T12:00:00Z"}
        });
        let (limit_type, reset_time) = HandleClaudeCodeLimits::classify_limit(&trigger);
        assert_eq!(limit_type, "usage_reached");
        assert_eq!(reset_time.as_deref(), Some("2026-03-14T12:00:00Z"));
    }

    #[test]
    fn handle_claude_code_limits_classify_rate_limit() {
        let trigger = serde_json::json!({
            "event_type": "rate_limit.detected",
            "extracted": {"retry_after": "30s"}
        });
        let (limit_type, reset_time) = HandleClaudeCodeLimits::classify_limit(&trigger);
        assert_eq!(limit_type, "rate_limit_detected");
        assert_eq!(reset_time.as_deref(), Some("30s"));
    }

    #[test]
    fn handle_claude_code_limits_classify_unknown() {
        let trigger = serde_json::json!({"event_type": "some.other"});
        let (limit_type, _) = HandleClaudeCodeLimits::classify_limit(&trigger);
        assert_eq!(limit_type, "unknown_limit");
    }

    // ========================================================================
    // HandleGeminiQuota tests
    // ========================================================================

    #[test]
    fn handle_gemini_quota_metadata() {
        let workflow = HandleGeminiQuota::new();
        assert_eq!(workflow.name(), "handle_gemini_quota");
        assert_eq!(
            workflow.description(),
            "Safe-pause on Gemini quota/usage limits with recovery plan"
        );
        assert!(!workflow.is_destructive());
        assert!(!workflow.requires_approval());
        assert!(workflow.requires_pane());
        assert_eq!(workflow.steps().len(), 3);
    }

    #[test]
    fn handle_gemini_quota_step_names() {
        let workflow = HandleGeminiQuota::new();
        let steps = workflow.steps();
        assert_eq!(steps[0].name, "check_guards");
        assert_eq!(steps[1].name, "check_cooldown");
        assert_eq!(steps[2].name, "classify_and_record");
    }

    #[test]
    fn handle_gemini_quota_handles_gemini_events() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleGeminiQuota::new();

        let quota_reached = Detection {
            rule_id: "gemini.usage.reached".to_string(),
            agent_type: AgentType::Gemini,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "Quota exhausted".to_string(),
            span: (0, 0),
        };
        assert!(workflow.handles(&quota_reached));

        let quota_warning = Detection {
            event_type: "usage.warning".to_string(),
            ..quota_reached.clone()
        };
        assert!(workflow.handles(&quota_warning));

        let rate_limit = Detection {
            event_type: "rate_limit.detected".to_string(),
            ..quota_reached.clone()
        };
        assert!(workflow.handles(&rate_limit));
    }

    #[test]
    fn handle_gemini_quota_rejects_non_gemini_agent() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleGeminiQuota::new();

        let claude_usage = Detection {
            rule_id: "claude_code.usage.reached".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "Usage limit".to_string(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&claude_usage));
    }

    #[test]
    fn handle_gemini_quota_with_cooldown() {
        let workflow = HandleGeminiQuota::with_cooldown_ms(7000);
        assert_eq!(workflow.cooldown_ms, 7000);
    }

    #[test]
    fn handle_gemini_quota_default_cooldown() {
        let workflow = HandleGeminiQuota::default();
        assert_eq!(workflow.cooldown_ms, GEMINI_QUOTA_COOLDOWN_MS);
        assert_eq!(workflow.cooldown_ms, 10 * 60 * 1000);
    }

    #[test]
    fn handle_gemini_quota_classify_quota_reached() {
        let trigger = serde_json::json!({
            "event_type": "usage.reached",
            "extracted": {"remaining": "0%"}
        });
        let (quota_type, remaining) = HandleGeminiQuota::classify_quota(&trigger);
        assert_eq!(quota_type, "quota_reached");
        assert_eq!(remaining.as_deref(), Some("0%"));
    }

    #[test]
    fn handle_gemini_quota_classify_quota_warning() {
        let trigger = serde_json::json!({
            "event_type": "usage.warning",
            "extracted": {"remaining": "15%"}
        });
        let (quota_type, remaining) = HandleGeminiQuota::classify_quota(&trigger);
        assert_eq!(quota_type, "quota_warning");
        assert_eq!(remaining.as_deref(), Some("15%"));
    }

    #[test]
    fn handle_gemini_quota_classify_unknown() {
        let trigger = serde_json::json!({"event_type": "some.other"});
        let (quota_type, remaining) = HandleGeminiQuota::classify_quota(&trigger);
        assert_eq!(quota_type, "unknown_quota");
        assert!(remaining.is_none());
    }

    // ========================================================================
    // HandleOnErrorCassSearch tests (ft-2l9kn)
    // ========================================================================

    #[test]
    fn handle_on_error_cass_search_metadata() {
        let workflow = HandleOnErrorCassSearch::new();
        assert_eq!(workflow.name(), "handle_on_error_cass_search");
        assert!(!workflow.is_destructive());
        assert!(!workflow.requires_approval());
        assert!(workflow.requires_pane());
        assert_eq!(workflow.steps().len(), 2);
    }

    #[test]
    fn handle_on_error_cass_search_handles_error_events_only() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleOnErrorCassSearch::new();
        let base = Detection {
            rule_id: "claude_code.error.network".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "error.network".to_string(),
            severity: Severity::Warning,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "Connection refused".to_string(),
            span: (0, 0),
        };
        assert!(workflow.handles(&base));

        let timeout = Detection {
            rule_id: "claude_code.error.timeout".to_string(),
            event_type: "error.timeout".to_string(),
            ..base.clone()
        };
        assert!(workflow.handles(&timeout));

        let overloaded = Detection {
            rule_id: "claude_code.error.overloaded".to_string(),
            event_type: "error.overloaded".to_string(),
            ..base.clone()
        };
        assert!(workflow.handles(&overloaded));

        let mux_error = Detection {
            event_type: "mux.error".to_string(),
            ..base.clone()
        };
        assert!(workflow.handles(&mux_error));

        // Should NOT handle session.start
        let session_start = Detection {
            event_type: "session.start".to_string(),
            ..base.clone()
        };
        assert!(!workflow.handles(&session_start));

        // Should NOT handle auth.error (auth has its own handler)
        let auth_error = Detection {
            event_type: "auth.error".to_string(),
            ..base
        };
        assert!(!workflow.handles(&auth_error));
    }

    #[test]
    fn handle_on_error_extract_error_text_from_matched_text() {
        let trigger = serde_json::json!({
            "matched_text": "Connection refused: could not connect to host",
            "agent_type": "claude_code",
        });
        let text = HandleOnErrorCassSearch::extract_error_text(&trigger);
        assert_eq!(
            text.as_deref(),
            Some("Connection refused: could not connect to host")
        );
    }

    #[test]
    fn handle_on_error_extract_error_text_from_extracted_message() {
        let trigger = serde_json::json!({
            "extracted": {"message": "Request timed out after 30s"},
            "agent_type": "claude_code",
        });
        let text = HandleOnErrorCassSearch::extract_error_text(&trigger);
        assert_eq!(text.as_deref(), Some("Request timed out after 30s"));
    }

    #[test]
    fn handle_on_error_extract_error_text_from_extracted_error() {
        let trigger = serde_json::json!({
            "extracted": {"error": "API overloaded, try again later"},
            "agent_type": "codex",
        });
        let text = HandleOnErrorCassSearch::extract_error_text(&trigger);
        assert_eq!(
            text.as_deref(),
            Some("API overloaded, try again later")
        );
    }

    #[test]
    fn handle_on_error_extract_error_text_prefers_matched_text() {
        let trigger = serde_json::json!({
            "matched_text": "matched text",
            "extracted": {"message": "extracted message", "error": "extracted error"},
        });
        let text = HandleOnErrorCassSearch::extract_error_text(&trigger);
        assert_eq!(text.as_deref(), Some("matched text"));
    }

    #[test]
    fn handle_on_error_query_candidates_include_error_and_rule() {
        let trigger = serde_json::json!({
            "matched_text": "Connection refused",
            "rule_id": "claude_code.error.network",
            "agent_type": "claude_code",
        });
        let pane = crate::storage::PaneRecord {
            pane_id: 42,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some("Working on ft-3681t".to_string()),
            cwd: Some("/Users/jemanuel/projects/frankenterm".to_string()),
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };

        let candidates = HandleOnErrorCassSearch::query_candidates(&trigger, Some(&pane));
        assert!(
            candidates.iter().any(|c| c == "Connection refused"),
            "expected error text in {candidates:?}"
        );
        assert!(
            candidates
                .iter()
                .any(|c| c == "claude_code.error.network"),
            "expected rule_id in {candidates:?}"
        );
        assert!(
            candidates.iter().any(|c| c.contains("ft-3681t")),
            "expected pane title in {candidates:?}"
        );
    }

    #[test]
    fn handle_on_error_query_candidates_dedup_case_insensitive() {
        let trigger = serde_json::json!({
            "matched_text": "ERROR.NETWORK",
            "rule_id": "error.network",
        });
        let candidates = HandleOnErrorCassSearch::query_candidates(&trigger, None);
        let lowercase_count = candidates
            .iter()
            .filter(|c| c.eq_ignore_ascii_case("error.network"))
            .count();
        assert!(
            lowercase_count <= 1,
            "expected no case-insensitive duplicates in {candidates:?}"
        );
    }

    #[test]
    fn handle_on_error_format_cass_hint_basic() {
        let hit = CassSearchHit {
            content: Some(
                "Fix: use rch exec -- cargo test instead of direct cargo".to_string(),
            ),
            source_path: Some("/tmp/session.jsonl".to_string()),
            line_number: Some(42),
            ..Default::default()
        };
        let hint = HandleOnErrorCassSearch::format_cass_hint(&hit);
        assert!(hint.is_some());
        let h = hint.unwrap();
        assert!(h.contains("/tmp/session.jsonl:42"));
        assert!(h.contains("use rch exec"));
    }

    #[test]
    fn handle_on_error_format_cass_hint_empty_content_returns_none() {
        let hit = CassSearchHit {
            content: Some(String::new()),
            source_path: Some("/tmp/session.jsonl".to_string()),
            line_number: Some(1),
            ..Default::default()
        };
        assert!(HandleOnErrorCassSearch::format_cass_hint(&hit).is_none());
    }

    #[test]
    fn on_error_cass_hints_lookup_serde_roundtrip() {
        let lookup = OnErrorCassHintsLookup {
            query: Some("Connection refused".to_string()),
            query_candidates: vec![
                "Connection refused".to_string(),
                "claude_code.error.network".to_string(),
            ],
            workspace: Some("/Users/jemanuel/projects/frankenterm".to_string()),
            hints: vec!["/tmp/s.md:5 - check firewall settings".to_string()],
            error: None,
            error_text: Some("Connection refused".to_string()),
            rule_id: Some("claude_code.error.network".to_string()),
        };
        let json = serde_json::to_string(&lookup).unwrap();
        let back: OnErrorCassHintsLookup = serde_json::from_str(&json).unwrap();
        assert_eq!(back.query, lookup.query);
        assert_eq!(back.hints.len(), 1);
        assert_eq!(back.error_text, lookup.error_text);
        assert_eq!(back.rule_id, lookup.rule_id);
    }

    #[test]
    fn on_error_cass_audit_decision_context_preserves_fields() {
        let lookup = OnErrorCassHintsLookup {
            query: Some("timeout error".to_string()),
            query_candidates: vec!["timeout error".to_string()],
            workspace: Some("/data/project".to_string()),
            hints: vec!["fix: increase timeout to 60s".to_string()],
            error: None,
            error_text: Some("Request timed out after 30s".to_string()),
            rule_id: Some("claude_code.error.timeout".to_string()),
        };

        let context_json = build_on_error_cass_audit_decision_context(
            "exec-err-1",
            55,
            "claude_code",
            "error.timeout",
            Some("claude_code.error.timeout"),
            &lookup,
            "On-error cass search for claude_code: Request timed out after 30s",
            "hints_found",
            9999,
        );

        assert!(context_json.is_some(), "context serialization must succeed");
        let context = parse_audit_decision_context(context_json);
        let evidence = evidence_map(&context);

        assert_eq!(context.timestamp_ms, 9999);
        assert_eq!(
            evidence.get("workflow_name").map(String::as_str),
            Some("handle_on_error_cass_search")
        );
        assert_eq!(
            evidence.get("agent_type").map(String::as_str),
            Some("claude_code")
        );
        assert_eq!(
            evidence.get("event_type").map(String::as_str),
            Some("error.timeout")
        );
        assert_eq!(
            evidence.get("error_text").map(String::as_str),
            Some("Request timed out after 30s")
        );
        assert_eq!(
            evidence.get("cass_hint_0").map(String::as_str),
            Some("fix: increase timeout to 60s")
        );
    }

    #[test]
    fn handle_on_error_cass_search_with_cooldown() {
        let workflow = HandleOnErrorCassSearch::with_cooldown_ms(5000);
        assert_eq!(workflow.cooldown_ms, 5000);
        assert_eq!(workflow.name(), "handle_on_error_cass_search");
    }

    #[test]
    fn handle_on_error_cass_search_default() {
        let workflow = HandleOnErrorCassSearch::default();
        assert_eq!(workflow.cooldown_ms, ON_ERROR_CASS_COOLDOWN_MS);
    }

    // ========================================================================
    // HandleSwarmLearningIndex tests (ft-2l9kn)
    // ========================================================================

    #[test]
    fn handle_swarm_learning_index_metadata() {
        let workflow = HandleSwarmLearningIndex::new();
        assert_eq!(workflow.name(), "handle_swarm_learning_index");
        assert_eq!(
            workflow.description(),
            "Index completed agent sessions into cass for swarm learning"
        );
        assert!(!workflow.is_destructive());
        assert!(!workflow.requires_approval());
        assert!(workflow.requires_pane());
        assert_eq!(workflow.steps().len(), 2);
    }

    #[test]
    fn handle_swarm_learning_index_step_names() {
        let workflow = HandleSwarmLearningIndex::new();
        let steps = workflow.steps();
        assert_eq!(steps[0].name, "check_cooldown");
        assert_eq!(steps[1].name, "trigger_cass_index");
    }

    #[test]
    fn handle_swarm_learning_index_handles_session_end_events() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleSwarmLearningIndex::new();

        let session_end = Detection {
            rule_id: "claude_code.session.end".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "session.end".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "Session ended".to_string(),
            span: (0, 0),
        };
        assert!(workflow.handles(&session_end));

        let session_summary = Detection {
            rule_id: "codex.session.summary".to_string(),
            agent_type: AgentType::Codex,
            event_type: "session.summary".to_string(),
            ..session_end.clone()
        };
        assert!(workflow.handles(&session_summary));
    }

    #[test]
    fn handle_swarm_learning_index_rejects_non_session_events() {
        use crate::patterns::{AgentType, Detection, Severity};

        let workflow = HandleSwarmLearningIndex::new();

        let error_event = Detection {
            rule_id: "claude_code.error.network".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "error.network".to_string(),
            severity: Severity::Warning,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "Connection refused".to_string(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&error_event));

        let session_start = Detection {
            event_type: "session.start".to_string(),
            ..error_event.clone()
        };
        assert!(!workflow.handles(&session_start));

        let auth_event = Detection {
            event_type: "auth.required".to_string(),
            ..error_event
        };
        assert!(!workflow.handles(&auth_event));
    }

    #[test]
    fn handle_swarm_learning_index_trigger_event_types() {
        let workflow = HandleSwarmLearningIndex::new();
        let types = workflow.trigger_event_types();
        assert_eq!(types.len(), 2);
        assert!(types.contains(&"session.end"));
        assert!(types.contains(&"session.summary"));
    }

    #[test]
    fn handle_swarm_learning_index_supported_agent_types() {
        let workflow = HandleSwarmLearningIndex::new();
        let types = workflow.supported_agent_types();
        assert!(types.contains(&"claude_code"));
        assert!(types.contains(&"codex"));
        assert!(types.contains(&"gemini"));
    }

    #[test]
    fn handle_swarm_learning_index_with_cooldown() {
        let workflow = HandleSwarmLearningIndex::with_cooldown_ms(5000);
        assert_eq!(workflow.cooldown_ms, 5000);
        assert_eq!(workflow.name(), "handle_swarm_learning_index");
    }

    #[test]
    fn handle_swarm_learning_index_default_cooldown() {
        let workflow = HandleSwarmLearningIndex::default();
        assert_eq!(workflow.cooldown_ms, SWARM_LEARNING_INDEX_COOLDOWN_MS);
        assert_eq!(workflow.cooldown_ms, 15 * 60 * 1000);
    }

    #[test]
    fn handle_swarm_learning_index_workspace_from_local_pane() {
        let pane = crate::storage::PaneRecord {
            pane_id: 10,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some("Working on ft-2l9kn".to_string()),
            cwd: Some("/Users/jemanuel/projects/frankenterm".to_string()),
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };

        let workspace = HandleSwarmLearningIndex::workspace_from_pane(Some(&pane));
        assert_eq!(
            workspace.as_deref(),
            Some("/Users/jemanuel/projects/frankenterm")
        );
    }

    #[test]
    fn handle_swarm_learning_index_workspace_from_file_uri_pane() {
        let pane = crate::storage::PaneRecord {
            pane_id: 11,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: None,
            cwd: Some("file:///home/user/project".to_string()),
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };

        let workspace = HandleSwarmLearningIndex::workspace_from_pane(Some(&pane));
        assert_eq!(workspace.as_deref(), Some("/home/user/project"));
    }

    #[test]
    fn handle_swarm_learning_index_workspace_from_remote_pane_returns_none() {
        let pane = crate::storage::PaneRecord {
            pane_id: 12,
            pane_uuid: None,
            domain: "SSH:remote-host".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: None,
            cwd: Some("file://remote-host/home/user/project".to_string()),
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };

        let workspace = HandleSwarmLearningIndex::workspace_from_pane(Some(&pane));
        assert!(
            workspace.is_none(),
            "remote pane should not produce a workspace, got: {workspace:?}"
        );
    }

    #[test]
    fn handle_swarm_learning_index_workspace_from_empty_cwd_returns_none() {
        let pane = crate::storage::PaneRecord {
            pane_id: 13,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: None,
            cwd: Some(String::new()),
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };

        let workspace = HandleSwarmLearningIndex::workspace_from_pane(Some(&pane));
        assert!(
            workspace.is_none(),
            "empty cwd should not produce workspace, got: {workspace:?}"
        );
    }

    #[test]
    fn handle_swarm_learning_index_workspace_from_none_pane() {
        let workspace = HandleSwarmLearningIndex::workspace_from_pane(None);
        assert!(workspace.is_none());
    }

    #[test]
    fn handle_swarm_learning_index_workspace_from_no_cwd() {
        let pane = crate::storage::PaneRecord {
            pane_id: 14,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("test".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };

        let workspace = HandleSwarmLearningIndex::workspace_from_pane(Some(&pane));
        assert!(
            workspace.is_none(),
            "pane with no cwd should not produce workspace"
        );
    }
}
