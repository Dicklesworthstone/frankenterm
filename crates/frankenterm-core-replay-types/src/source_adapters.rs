//! Read-only source adapters for swarm flight-recorder causal events.
//!
//! These adapters transform already-collected operational evidence snapshots
//! into [`crate::swarm_causal_event::SwarmCausalEvent`] values. They do not
//! shell out, contact services, repair Agent Mail, mutate git state, or run
//! proof commands; callers are responsible for collecting source snapshots.

use crate::swarm_causal_event::{
    CausalArtifactRef, CausalCorrelationKeys, CausalEventClass, CausalLinks,
    CausalPayloadSensitivity, CausalPrivacyPolicy, CausalRedactionStatus, CausalRetentionClass,
    SwarmCausalEvent, SwarmCausalEventSource, default_source_budget,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

pub const SOURCE_ADAPTER_CONTRACT_ID_V1: &str = "ft.swarm.source_adapters.v1";
const REDACTED: &str = "[redacted]";
const MAX_STRING_BYTES: usize = 2048;
const MAX_SAMPLE_ROWS: usize = 32;
const HASH_PREFIX_BYTES: usize = 12;

/// Source adapter family that produced or attempted to produce an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAdapterKind {
    PaneRuntime,
    Beads,
    Rch,
    Git,
    AgentMail,
    SourceSet,
}

impl SourceAdapterKind {
    const fn source(self) -> SwarmCausalEventSource {
        match self {
            Self::PaneRuntime => SwarmCausalEventSource::Pane,
            Self::Beads => SwarmCausalEventSource::Beads,
            Self::Rch => SwarmCausalEventSource::Rch,
            Self::Git => SwarmCausalEventSource::Git,
            Self::AgentMail => SwarmCausalEventSource::AgentMail,
            Self::SourceSet => SwarmCausalEventSource::SourceUnavailable,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::PaneRuntime => "pane_runtime",
            Self::Beads => "beads",
            Self::Rch => "rch",
            Self::Git => "git",
            Self::AgentMail => "agent_mail",
            Self::SourceSet => "source_set",
        }
    }
}

/// Shared collection context supplied by the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceAdapterContext {
    pub workspace_id: Option<String>,
    pub ingested_at_ms: u64,
    pub first_ingest_sequence: u64,
}

/// Aggregate source-set fixture/input for all read-only adapters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceAdapterSet {
    pub schema_version: u32,
    pub contract_id: String,
    pub generated_at_ms: u64,
    pub source_set_id: String,
    pub workspace_id: Option<String>,
    pub pane_runtime: Vec<PaneRuntimeSnapshot>,
    pub beads: Vec<BeadsSnapshot>,
    pub rch: Vec<RchSnapshot>,
    pub git: Vec<GitSnapshot>,
    pub agent_mail: Vec<AgentMailSnapshot>,
}

/// Adapter output plus isolated per-source failures.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceAdapterReport {
    pub events: Vec<SwarmCausalEvent>,
    pub failures: Vec<SourceAdapterFailure>,
}

/// Non-fatal adapter failure. Other snapshots in the same source set continue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceAdapterFailure {
    pub adapter: SourceAdapterKind,
    pub source_id: Option<String>,
    pub reason_code: String,
    pub detail: String,
}

/// Pane/runtime evidence already collected from runtime or storage surfaces.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneRuntimeSnapshot {
    pub source_id: String,
    pub pane_id: Option<u64>,
    pub session_id: Option<String>,
    pub collected_at_ms: u64,
    pub capture_gap_count: u64,
    pub pattern_detection_count: u64,
    pub workflow_action_count: u64,
    pub redaction: ReadPathRedactionSnapshot,
    pub reason_codes: Vec<String>,
    pub artifact_paths: Vec<String>,
    pub payload: Value,
}

/// Read-path redaction metadata carried by pane/runtime snapshots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadPathRedactionSnapshot {
    pub applied: bool,
    pub raw_text_stored: bool,
    pub policy_id: Option<String>,
}

/// Beads issue/comment/dependency evidence from JSONL or DB snapshots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BeadsSnapshot {
    pub source_id: String,
    pub issue_id: String,
    pub status: String,
    pub updated_at_ms: u64,
    pub dependency_ids: Vec<String>,
    pub comment_count: u64,
    pub dirty_tree_guard_comment_count: u64,
    pub close_reason: Option<String>,
    pub reason_codes: Vec<String>,
    pub artifact_paths: Vec<String>,
    pub payload: Value,
}

/// RCH build/proof evidence from status, JSON outputs, or log summaries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RchSnapshot {
    pub source_id: String,
    pub build_id: String,
    pub worker_id: Option<String>,
    pub command_argv: Vec<String>,
    pub started_at_ms: u64,
    pub completed_at_ms: Option<u64>,
    pub location: Option<String>,
    pub exit_code: Option<i32>,
    pub cancellation_reason: Option<String>,
    pub preflight_refusal_reason: Option<String>,
    pub artifact_paths: Vec<String>,
    pub reason_codes: Vec<String>,
    pub payload: Value,
}

/// Git branch/status evidence captured without mutating the worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitSnapshot {
    pub source_id: String,
    pub head_commit: String,
    pub branch: Option<String>,
    pub captured_at_ms: u64,
    pub staged_paths: Vec<String>,
    pub dirty_paths: Vec<String>,
    pub branch_ahead: u64,
    pub branch_behind: u64,
    pub protected_deletion_warnings: Vec<String>,
    pub artifact_paths: Vec<String>,
    pub reason_codes: Vec<String>,
    pub payload: Value,
}

/// Agent Mail evidence collected only after the caller confirmed API health.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentMailSnapshot {
    pub source_id: String,
    pub health_state: String,
    pub thread_id: Option<String>,
    pub collected_at_ms: Option<u64>,
    pub message_count: u64,
    pub ack_count: u64,
    pub ack_required_count: u64,
    pub contact_count: u64,
    pub unavailable_reason: Option<String>,
    pub reason_codes: Vec<String>,
    pub artifact_paths: Vec<String>,
    pub payload: Value,
}

/// Parse and adapt a complete source-set JSON fixture/input.
pub fn adapt_source_set_json(input: &str, context: SourceAdapterContext) -> SourceAdapterReport {
    match serde_json::from_str::<SourceAdapterSet>(input) {
        Ok(source_set) => adapt_source_set(&source_set, context),
        Err(error) => {
            let mut report = SourceAdapterReport::default();
            let sequence = context.first_ingest_sequence;
            report.failures.push(SourceAdapterFailure {
                adapter: SourceAdapterKind::SourceSet,
                source_id: None,
                reason_code: "source_set.malformed_json".to_string(),
                detail: error.to_string(),
            });
            append_event_or_failure(
                &mut report,
                SourceAdapterKind::SourceSet,
                None,
                build_unavailable_event(
                    SourceAdapterKind::SourceSet,
                    "source_set.malformed_json",
                    &error.to_string(),
                    &[],
                    &context,
                    context.ingested_at_ms,
                    sequence,
                ),
            );
            report
        }
    }
}

/// Adapt all snapshots in a parsed source set, isolating per-source failures.
pub fn adapt_source_set(
    source_set: &SourceAdapterSet,
    mut context: SourceAdapterContext,
) -> SourceAdapterReport {
    if context.workspace_id.is_none() {
        context.workspace_id.clone_from(&source_set.workspace_id);
    }
    if context.ingested_at_ms == 0 {
        context.ingested_at_ms = source_set.generated_at_ms;
    }

    let mut report = SourceAdapterReport::default();
    let mut sequence = context.first_ingest_sequence;
    for snapshot in &source_set.pane_runtime {
        append_event_or_failure(
            &mut report,
            SourceAdapterKind::PaneRuntime,
            Some(snapshot.source_id.clone()),
            adapt_pane_runtime_snapshot(snapshot, &context, sequence),
        );
        sequence = sequence.saturating_add(1);
    }
    for snapshot in &source_set.beads {
        append_event_or_failure(
            &mut report,
            SourceAdapterKind::Beads,
            Some(snapshot.source_id.clone()),
            adapt_beads_snapshot(snapshot, &context, sequence),
        );
        sequence = sequence.saturating_add(1);
    }
    for snapshot in &source_set.rch {
        append_event_or_failure(
            &mut report,
            SourceAdapterKind::Rch,
            Some(snapshot.source_id.clone()),
            adapt_rch_snapshot(snapshot, &context, sequence),
        );
        sequence = sequence.saturating_add(1);
    }
    for snapshot in &source_set.git {
        append_event_or_failure(
            &mut report,
            SourceAdapterKind::Git,
            Some(snapshot.source_id.clone()),
            adapt_git_snapshot(snapshot, &context, sequence),
        );
        sequence = sequence.saturating_add(1);
    }
    for snapshot in &source_set.agent_mail {
        append_event_or_failure(
            &mut report,
            SourceAdapterKind::AgentMail,
            Some(snapshot.source_id.clone()),
            adapt_agent_mail_snapshot(snapshot, &context, sequence),
        );
        sequence = sequence.saturating_add(1);
    }
    report
}

/// Adapt Beads JSONL snapshots line-by-line so malformed rows do not poison the stream.
pub fn adapt_beads_jsonl(input: &str, context: SourceAdapterContext) -> SourceAdapterReport {
    let mut report = SourceAdapterReport::default();
    let mut sequence = context.first_ingest_sequence;
    for (line_index, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<BeadsSnapshot>(line) {
            Ok(snapshot) => append_event_or_failure(
                &mut report,
                SourceAdapterKind::Beads,
                Some(snapshot.source_id.clone()),
                adapt_beads_snapshot(&snapshot, &context, sequence),
            ),
            Err(error) => {
                let source_id = format!("line-{}", line_index.saturating_add(1));
                report.failures.push(SourceAdapterFailure {
                    adapter: SourceAdapterKind::Beads,
                    source_id: Some(source_id.clone()),
                    reason_code: "beads.malformed_jsonl".to_string(),
                    detail: error.to_string(),
                });
                append_event_or_failure(
                    &mut report,
                    SourceAdapterKind::Beads,
                    Some(source_id),
                    build_unavailable_event(
                        SourceAdapterKind::Beads,
                        "beads.malformed_jsonl",
                        &error.to_string(),
                        &[],
                        &context,
                        context.ingested_at_ms,
                        sequence,
                    ),
                );
            }
        }
        sequence = sequence.saturating_add(1);
    }
    report
}

/// Adapt one pane/runtime snapshot.
pub fn adapt_pane_runtime_snapshot(
    snapshot: &PaneRuntimeSnapshot,
    context: &SourceAdapterContext,
    ingest_sequence: u64,
) -> Result<SwarmCausalEvent, SourceAdapterFailure> {
    let Some(pane_id) = snapshot.pane_id else {
        return build_unavailable_event(
            SourceAdapterKind::PaneRuntime,
            "pane.missing_pane_id",
            "pane/runtime snapshot omitted pane_id",
            &snapshot.artifact_paths,
            context,
            snapshot.collected_at_ms,
            ingest_sequence,
        );
    };
    let (artifacts, omitted_artifacts) = safe_artifact_refs(&snapshot.artifact_paths);
    let mut payload = json!({
        "adapter": SourceAdapterKind::PaneRuntime.label(),
        "source_id_hash": short_hash(&snapshot.source_id),
        "pane_id": pane_id,
        "session_id": snapshot.session_id,
        "capture_gap_count": snapshot.capture_gap_count,
        "pattern_detection_count": snapshot.pattern_detection_count,
        "workflow_action_count": snapshot.workflow_action_count,
        "read_path_redaction": {
            "applied": snapshot.redaction.applied,
            "raw_text_stored": snapshot.redaction.raw_text_stored,
            "policy_id": snapshot.redaction.policy_id,
        },
        "reason_codes": capped_string_values(&snapshot.reason_codes),
        "details": snapshot.payload.clone(),
    });
    insert_omitted_artifact_count(&mut payload, omitted_artifacts);
    let mut correlation = base_correlation(context);
    correlation.pane_id = Some(pane_id);
    correlation.session_id.clone_from(&snapshot.session_id);
    build_validated_event(
        SourceAdapterKind::PaneRuntime,
        event_class_for_pane(snapshot),
        snapshot.collected_at_ms,
        context.ingested_at_ms,
        ingest_sequence,
        correlation,
        CausalPayloadSensitivity::UserText,
        CausalRetentionClass::Standard,
        artifacts,
        payload,
    )
}

/// Adapt one Beads snapshot.
pub fn adapt_beads_snapshot(
    snapshot: &BeadsSnapshot,
    context: &SourceAdapterContext,
    ingest_sequence: u64,
) -> Result<SwarmCausalEvent, SourceAdapterFailure> {
    if snapshot.issue_id.trim().is_empty() {
        return build_unavailable_event(
            SourceAdapterKind::Beads,
            "beads.missing_issue_id",
            "Beads snapshot omitted issue_id",
            &snapshot.artifact_paths,
            context,
            snapshot.updated_at_ms,
            ingest_sequence,
        );
    }
    let (artifacts, omitted_artifacts) = safe_artifact_refs(&snapshot.artifact_paths);
    let mut payload = json!({
        "adapter": SourceAdapterKind::Beads.label(),
        "source_id_hash": short_hash(&snapshot.source_id),
        "issue_id": snapshot.issue_id,
        "status": snapshot.status,
        "dependency_ids": capped_string_values(&snapshot.dependency_ids),
        "comment_count": snapshot.comment_count,
        "dirty_tree_guard_comment_count": snapshot.dirty_tree_guard_comment_count,
        "close_reason": snapshot.close_reason,
        "reason_codes": capped_string_values(&snapshot.reason_codes),
        "details": snapshot.payload.clone(),
    });
    insert_omitted_artifact_count(&mut payload, omitted_artifacts);
    let mut correlation = base_correlation(context);
    correlation.bead_id = Some(snapshot.issue_id.clone());
    build_validated_event(
        SourceAdapterKind::Beads,
        event_class_for_beads(snapshot),
        snapshot.updated_at_ms,
        context.ingested_at_ms,
        ingest_sequence,
        correlation,
        CausalPayloadSensitivity::Structural,
        CausalRetentionClass::Proof,
        artifacts,
        payload,
    )
}

/// Adapt one RCH snapshot.
pub fn adapt_rch_snapshot(
    snapshot: &RchSnapshot,
    context: &SourceAdapterContext,
    ingest_sequence: u64,
) -> Result<SwarmCausalEvent, SourceAdapterFailure> {
    if snapshot.build_id.trim().is_empty() {
        return build_unavailable_event(
            SourceAdapterKind::Rch,
            "rch.missing_build_id",
            "RCH snapshot omitted build_id",
            &snapshot.artifact_paths,
            context,
            snapshot.started_at_ms,
            ingest_sequence,
        );
    }
    let (artifacts, omitted_artifacts) = safe_artifact_refs(&snapshot.artifact_paths);
    let mut payload = json!({
        "adapter": SourceAdapterKind::Rch.label(),
        "source_id_hash": short_hash(&snapshot.source_id),
        "build_id": snapshot.build_id,
        "worker_id": snapshot.worker_id,
        "command_argv": capped_string_values(&snapshot.command_argv),
        "started_at_ms": snapshot.started_at_ms,
        "completed_at_ms": snapshot.completed_at_ms,
        "location": snapshot.location,
        "exit_code": snapshot.exit_code,
        "cancellation_reason": snapshot.cancellation_reason,
        "preflight_refusal_reason": snapshot.preflight_refusal_reason,
        "reason_codes": capped_string_values(&snapshot.reason_codes),
        "details": snapshot.payload.clone(),
    });
    insert_omitted_artifact_count(&mut payload, omitted_artifacts);
    let mut correlation = base_correlation(context);
    correlation.rch_build_id = Some(snapshot.build_id.clone());
    correlation.rch_worker_id.clone_from(&snapshot.worker_id);
    build_validated_event(
        SourceAdapterKind::Rch,
        event_class_for_rch(snapshot),
        snapshot.started_at_ms,
        context.ingested_at_ms,
        ingest_sequence,
        correlation,
        CausalPayloadSensitivity::Structural,
        CausalRetentionClass::Proof,
        artifacts,
        payload,
    )
}

/// Adapt one git snapshot.
pub fn adapt_git_snapshot(
    snapshot: &GitSnapshot,
    context: &SourceAdapterContext,
    ingest_sequence: u64,
) -> Result<SwarmCausalEvent, SourceAdapterFailure> {
    if snapshot.head_commit.trim().is_empty() {
        return build_unavailable_event(
            SourceAdapterKind::Git,
            "git.missing_head_commit",
            "git snapshot omitted head_commit",
            &snapshot.artifact_paths,
            context,
            snapshot.captured_at_ms,
            ingest_sequence,
        );
    }
    let (artifacts, omitted_artifacts) = safe_artifact_refs(&snapshot.artifact_paths);
    let mut payload = json!({
        "adapter": SourceAdapterKind::Git.label(),
        "source_id_hash": short_hash(&snapshot.source_id),
        "head_commit": snapshot.head_commit,
        "branch": snapshot.branch,
        "branch_ahead": snapshot.branch_ahead,
        "branch_behind": snapshot.branch_behind,
        "staged_path_count": snapshot.staged_paths.len(),
        "dirty_path_count": snapshot.dirty_paths.len(),
        "protected_deletion_warning_count": snapshot.protected_deletion_warnings.len(),
        "staged_paths_sample": capped_string_values(&snapshot.staged_paths),
        "dirty_paths_sample": capped_string_values(&snapshot.dirty_paths),
        "protected_deletion_warnings_sample": capped_string_values(&snapshot.protected_deletion_warnings),
        "reason_codes": capped_string_values(&snapshot.reason_codes),
        "details": snapshot.payload.clone(),
    });
    insert_omitted_artifact_count(&mut payload, omitted_artifacts);
    let mut correlation = base_correlation(context);
    correlation.git_commit = Some(snapshot.head_commit.clone());
    correlation.git_branch.clone_from(&snapshot.branch);
    build_validated_event(
        SourceAdapterKind::Git,
        event_class_for_git(snapshot),
        snapshot.captured_at_ms,
        context.ingested_at_ms,
        ingest_sequence,
        correlation,
        CausalPayloadSensitivity::Structural,
        CausalRetentionClass::Proof,
        artifacts,
        payload,
    )
}

/// Adapt one Agent Mail snapshot.
pub fn adapt_agent_mail_snapshot(
    snapshot: &AgentMailSnapshot,
    context: &SourceAdapterContext,
    ingest_sequence: u64,
) -> Result<SwarmCausalEvent, SourceAdapterFailure> {
    if snapshot.health_state.eq_ignore_ascii_case("unavailable") {
        return build_unavailable_event(
            SourceAdapterKind::AgentMail,
            "agent_mail.unavailable",
            snapshot
                .unavailable_reason
                .as_deref()
                .unwrap_or("Agent Mail API unavailable"),
            &snapshot.artifact_paths,
            context,
            snapshot.collected_at_ms.unwrap_or(context.ingested_at_ms),
            ingest_sequence,
        );
    }
    let Some(thread_id) = snapshot
        .thread_id
        .as_ref()
        .filter(|id| !id.trim().is_empty())
    else {
        return build_unavailable_event(
            SourceAdapterKind::AgentMail,
            "agent_mail.missing_thread_id",
            "Agent Mail snapshot omitted thread_id while marked available",
            &snapshot.artifact_paths,
            context,
            snapshot.collected_at_ms.unwrap_or(context.ingested_at_ms),
            ingest_sequence,
        );
    };
    let (artifacts, omitted_artifacts) = safe_artifact_refs(&snapshot.artifact_paths);
    let mut payload = json!({
        "adapter": SourceAdapterKind::AgentMail.label(),
        "source_id_hash": short_hash(&snapshot.source_id),
        "health_state": snapshot.health_state,
        "message_count": snapshot.message_count,
        "ack_count": snapshot.ack_count,
        "ack_required_count": snapshot.ack_required_count,
        "contact_count": snapshot.contact_count,
        "reason_codes": capped_string_values(&snapshot.reason_codes),
        "details": snapshot.payload.clone(),
    });
    insert_omitted_artifact_count(&mut payload, omitted_artifacts);
    let mut correlation = base_correlation(context);
    correlation.thread_id = Some(thread_id.clone());
    build_validated_event(
        SourceAdapterKind::AgentMail,
        CausalEventClass::SourcePass,
        snapshot.collected_at_ms.unwrap_or(context.ingested_at_ms),
        context.ingested_at_ms,
        ingest_sequence,
        correlation,
        CausalPayloadSensitivity::Structural,
        CausalRetentionClass::Proof,
        artifacts,
        payload,
    )
}

fn append_event_or_failure(
    report: &mut SourceAdapterReport,
    adapter: SourceAdapterKind,
    source_id: Option<String>,
    result: Result<SwarmCausalEvent, SourceAdapterFailure>,
) {
    match result {
        Ok(event) => report.events.push(event),
        Err(mut failure) => {
            failure.adapter = adapter;
            if failure.source_id.is_none() {
                failure.source_id = source_id;
            }
            report.failures.push(failure);
        }
    }
}

fn build_validated_event(
    adapter: SourceAdapterKind,
    event_class: CausalEventClass,
    occurred_at_ms: u64,
    ingested_at_ms: u64,
    ingest_sequence: u64,
    correlation: CausalCorrelationKeys,
    sensitivity: CausalPayloadSensitivity,
    retention_class: CausalRetentionClass,
    artifacts: Vec<CausalArtifactRef>,
    payload: Value,
) -> Result<SwarmCausalEvent, SourceAdapterFailure> {
    let source = adapter.source();
    let budget = default_source_budget(source, retention_class);
    let prepared = prepare_payload_for_budget(payload, budget.max_payload_bytes);
    let privacy_policy = if prepared.redaction_status == CausalRedactionStatus::HashOnly {
        CausalPrivacyPolicy::for_source(source, retention_class, prepared.redaction_status)
            .with_omitted_payload_ref(
                prepared.omitted_payload_hash,
                prepared.omitted_payload_bytes,
            )
    } else {
        CausalPrivacyPolicy::for_source(source, retention_class, prepared.redaction_status)
    };
    SwarmCausalEvent::new_with_privacy_policy(
        event_id_for(adapter, ingest_sequence, &prepared.event_seed),
        source,
        event_class,
        occurred_at_ms,
        occurred_at_ms.max(ingested_at_ms),
        ingest_sequence,
        correlation,
        CausalLinks::default(),
        sensitivity,
        prepared.redaction_status,
        retention_class,
        privacy_policy,
        artifacts,
        prepared.payload,
    )
    .map_err(|error| SourceAdapterFailure {
        adapter,
        source_id: None,
        reason_code: "source_adapter.event_validation_failed".to_string(),
        detail: error.to_string(),
    })
}

fn build_unavailable_event(
    adapter: SourceAdapterKind,
    reason_code: &str,
    detail: &str,
    artifact_paths: &[String],
    context: &SourceAdapterContext,
    occurred_at_ms: u64,
    ingest_sequence: u64,
) -> Result<SwarmCausalEvent, SourceAdapterFailure> {
    let (artifacts, omitted_artifacts) = safe_artifact_refs(artifact_paths);
    let mut payload = json!({
        "adapter": adapter.label(),
        "reason": reason_code,
        "detail": detail,
        "reason_codes": [reason_code],
    });
    insert_omitted_artifact_count(&mut payload, omitted_artifacts);
    let mut correlation = base_correlation(context);
    correlation.command_id = Some(format!("{}-unavailable", adapter.label()));
    let budget = default_source_budget(
        SwarmCausalEventSource::SourceUnavailable,
        CausalRetentionClass::Proof,
    );
    let prepared = prepare_payload_for_budget(payload, budget.max_payload_bytes);
    SwarmCausalEvent::new_with_privacy_policy(
        event_id_for(adapter, ingest_sequence, reason_code),
        SwarmCausalEventSource::SourceUnavailable,
        unavailable_event_class(adapter),
        occurred_at_ms,
        occurred_at_ms.max(context.ingested_at_ms),
        ingest_sequence,
        correlation,
        CausalLinks::default(),
        CausalPayloadSensitivity::Structural,
        CausalRedactionStatus::Unavailable,
        CausalRetentionClass::Proof,
        CausalPrivacyPolicy::for_source(
            SwarmCausalEventSource::SourceUnavailable,
            CausalRetentionClass::Proof,
            CausalRedactionStatus::Unavailable,
        ),
        artifacts,
        prepared.payload,
    )
    .map_err(|error| SourceAdapterFailure {
        adapter,
        source_id: None,
        reason_code: "source_adapter.unavailable_event_validation_failed".to_string(),
        detail: error.to_string(),
    })
}

fn base_correlation(context: &SourceAdapterContext) -> CausalCorrelationKeys {
    CausalCorrelationKeys {
        workspace_id: context.workspace_id.clone(),
        ..Default::default()
    }
}

fn event_class_for_pane(snapshot: &PaneRuntimeSnapshot) -> CausalEventClass {
    if snapshot.capture_gap_count > 0 {
        CausalEventClass::EvidenceUnavailable
    } else {
        CausalEventClass::SourcePass
    }
}

fn event_class_for_beads(snapshot: &BeadsSnapshot) -> CausalEventClass {
    if snapshot.dirty_tree_guard_comment_count > 0 {
        CausalEventClass::DirtyTreeContamination
    } else {
        CausalEventClass::SourcePass
    }
}

fn event_class_for_rch(snapshot: &RchSnapshot) -> CausalEventClass {
    if snapshot.preflight_refusal_reason.is_some()
        || snapshot.cancellation_reason.is_some()
        || snapshot.location.as_deref() == Some("local")
        || snapshot.exit_code.is_some_and(|code| code != 0)
    {
        CausalEventClass::InfrastructureFailure
    } else {
        CausalEventClass::SourcePass
    }
}

fn event_class_for_git(snapshot: &GitSnapshot) -> CausalEventClass {
    if !snapshot.dirty_paths.is_empty() || !snapshot.protected_deletion_warnings.is_empty() {
        CausalEventClass::DirtyTreeContamination
    } else {
        CausalEventClass::SourcePass
    }
}

const fn unavailable_event_class(adapter: SourceAdapterKind) -> CausalEventClass {
    match adapter {
        SourceAdapterKind::AgentMail => CausalEventClass::CommunicationOutage,
        SourceAdapterKind::Rch => CausalEventClass::InfrastructureFailure,
        SourceAdapterKind::Git | SourceAdapterKind::Beads | SourceAdapterKind::PaneRuntime => {
            CausalEventClass::EvidenceUnavailable
        }
        SourceAdapterKind::SourceSet => CausalEventClass::SourceFailure,
    }
}

#[derive(Debug)]
struct PreparedPayload {
    payload: Value,
    redaction_status: CausalRedactionStatus,
    omitted_payload_hash: String,
    omitted_payload_bytes: usize,
    event_seed: String,
}

fn prepare_payload_for_budget(payload: Value, max_payload_bytes: usize) -> PreparedPayload {
    let transform = redact_value(payload);
    let transformed_bytes = serialized_value_len(&transform.value);
    let event_seed = transform
        .value
        .get("adapter")
        .and_then(Value::as_str)
        .unwrap_or("adapter")
        .to_string();
    if transformed_bytes > max_payload_bytes {
        let omitted_hash = sha256_hex(serde_json::to_string(&transform.value).unwrap_or_default());
        let payload = json!({
            "summary": "payload omitted after source budget",
            "reason": "source_adapter.payload_hash_only",
            "omitted_payload_bytes": transformed_bytes,
        });
        return PreparedPayload {
            payload,
            redaction_status: CausalRedactionStatus::HashOnly,
            omitted_payload_hash: omitted_hash,
            omitted_payload_bytes: transformed_bytes,
            event_seed,
        };
    }
    PreparedPayload {
        payload: transform.value,
        redaction_status: if transform.truncated {
            CausalRedactionStatus::Truncated
        } else if transform.redacted {
            CausalRedactionStatus::Redacted
        } else {
            CausalRedactionStatus::NotRequired
        },
        omitted_payload_hash: String::new(),
        omitted_payload_bytes: 0,
        event_seed,
    }
}

#[derive(Debug)]
struct RedactedValue {
    value: Value,
    redacted: bool,
    truncated: bool,
}

fn redact_value(value: Value) -> RedactedValue {
    match value {
        Value::String(value) => redact_string(value),
        Value::Array(values) => redact_array(values),
        Value::Object(values) => redact_object(values),
        Value::Null | Value::Bool(_) | Value::Number(_) => RedactedValue {
            value,
            redacted: false,
            truncated: false,
        },
    }
}

fn redact_array(values: Vec<Value>) -> RedactedValue {
    let original_len = values.len();
    let mut redacted = false;
    let mut truncated = original_len > MAX_SAMPLE_ROWS;
    let mut out = Vec::with_capacity(original_len.min(MAX_SAMPLE_ROWS));
    for value in values.into_iter().take(MAX_SAMPLE_ROWS) {
        let item = redact_value(value);
        redacted |= item.redacted;
        truncated |= item.truncated;
        out.push(item.value);
    }
    if original_len > MAX_SAMPLE_ROWS {
        out.push(json!({
            "omitted_count": original_len - MAX_SAMPLE_ROWS,
            "reason": "source_adapter.array_sample_truncated",
        }));
    }
    RedactedValue {
        value: Value::Array(out),
        redacted,
        truncated,
    }
}

fn redact_object(values: Map<String, Value>) -> RedactedValue {
    let mut redacted = false;
    let mut truncated = false;
    let mut out = Map::new();
    for (key, value) in values {
        if key_looks_secret_like(&key) {
            redacted = true;
            out.insert(key, Value::String(REDACTED.to_string()));
        } else {
            let item = redact_value(value);
            redacted |= item.redacted;
            truncated |= item.truncated;
            out.insert(key, item.value);
        }
    }
    RedactedValue {
        value: Value::Object(out),
        redacted,
        truncated,
    }
}

fn redact_string(value: String) -> RedactedValue {
    if looks_secret_like(&value) {
        return RedactedValue {
            value: Value::String(REDACTED.to_string()),
            redacted: true,
            truncated: false,
        };
    }
    let (value, truncated) = truncate_string(&value);
    RedactedValue {
        value: Value::String(value),
        redacted: false,
        truncated,
    }
}

fn truncate_string(value: &str) -> (String, bool) {
    if value.len() <= MAX_STRING_BYTES {
        return (value.to_string(), false);
    }
    let mut end = MAX_STRING_BYTES;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (format!("{}...[truncated]", &value[..end]), true)
}

fn capped_string_values(values: &[String]) -> Vec<String> {
    values.iter().take(MAX_SAMPLE_ROWS).cloned().collect()
}

fn safe_artifact_refs(paths: &[String]) -> (Vec<CausalArtifactRef>, usize) {
    let mut omitted = 0usize;
    let refs = paths
        .iter()
        .filter_map(|path| {
            let path = path.trim();
            if is_safe_artifact_path(path) {
                Some(CausalArtifactRef {
                    kind: artifact_kind(path),
                    uri: path.to_string(),
                    sha256: None,
                    size_bytes: None,
                })
            } else {
                omitted = omitted.saturating_add(1);
                None
            }
        })
        .collect();
    (refs, omitted)
}

fn artifact_kind(path: &str) -> String {
    let kind = path
        .rsplit_once('.')
        .map_or("artifact", |(_, extension)| extension)
        .chars()
        .filter(|char| char.is_ascii_alphanumeric() || *char == '_' || *char == '-')
        .collect::<String>()
        .to_ascii_lowercase();
    if kind.is_empty() {
        "artifact".to_string()
    } else {
        kind
    }
}

fn is_safe_artifact_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.split('/').any(|part| part.is_empty() || part == "..")
        && !looks_secret_like(path)
}

fn insert_omitted_artifact_count(payload: &mut Value, count: usize) {
    if count == 0 {
        return;
    }
    if let Value::Object(values) = payload {
        values.insert(
            "omitted_unsafe_artifact_count".to_string(),
            Value::from(count as u64),
        );
    }
}

fn event_id_for(adapter: SourceAdapterKind, ingest_sequence: u64, seed: &str) -> String {
    format!(
        "source-{}-{}-{}",
        adapter.label(),
        ingest_sequence,
        short_hash(seed)
    )
}

fn short_hash(value: &str) -> String {
    let digest = sha256_hex(value);
    digest.chars().take(HASH_PREFIX_BYTES).collect()
}

fn sha256_hex(input: impl AsRef<[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_ref());
    hex_bytes(&hasher.finalize())
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn serialized_value_len(value: &Value) -> usize {
    serde_json::to_string(value).unwrap_or_default().len()
}

fn key_looks_secret_like(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("authorization")
        || lower.contains("password")
        || lower.contains("passwd")
        || lower.contains("secret")
        || lower.contains("token")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("private_key")
}

fn looks_secret_like(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("authorization: bearer ")
        || lower.contains("bearer sk-")
        || lower.contains("bearer ghp_")
        || lower.contains("password=")
        || lower.contains("passwd=")
        || lower.contains("secret=")
        || lower.contains("token=")
        || lower.contains("api_key=")
        || lower.contains("apikey=")
        || lower.contains("private_key=")
        || lower.contains("aws_secret_access_key=")
        || lower.contains("sk-live-")
        || lower.contains("ghp_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm_causal_event::CausalPayloadOmissionReason;

    const VALID_SOURCE_SET: &str =
        include_str!("../../../fixtures/flight-recorder/source-adapters/valid-source-set.json");

    fn context() -> SourceAdapterContext {
        SourceAdapterContext {
            workspace_id: Some("frankenterm".to_string()),
            ingested_at_ms: 1_778_000_100_000,
            first_ingest_sequence: 10,
        }
    }

    #[test]
    fn fixture_adapts_all_source_kinds() {
        let report = adapt_source_set_json(VALID_SOURCE_SET, context());
        assert_eq!(report.failures, Vec::new());
        assert_eq!(report.events.len(), 5);
        assert_eq!(report.events[0].source, SwarmCausalEventSource::Pane);
        assert_eq!(report.events[1].source, SwarmCausalEventSource::Beads);
        assert_eq!(report.events[2].source, SwarmCausalEventSource::Rch);
        assert_eq!(report.events[3].source, SwarmCausalEventSource::Git);
        assert_eq!(report.events[4].source, SwarmCausalEventSource::AgentMail);
        assert!(report.events.iter().all(|event| event.validate().is_ok()));
    }

    #[test]
    fn fixture_outputs_bounded_redacted_events_with_artifacts() {
        let report = adapt_source_set_json(VALID_SOURCE_SET, context());
        let pane = &report.events[0];
        assert_eq!(
            pane.privacy.redaction_status,
            CausalRedactionStatus::Redacted
        );
        assert_eq!(pane.correlation.pane_id, Some(42));
        assert_eq!(pane.event_class, CausalEventClass::EvidenceUnavailable);
        assert_eq!(pane.payload["details"]["env"]["API_TOKEN"], REDACTED);
        assert!(pane.privacy.payload_bytes <= pane.privacy.max_payload_bytes);

        let git = &report.events[3];
        assert_eq!(git.event_class, CausalEventClass::DirtyTreeContamination);
        assert_eq!(git.correlation.git_branch.as_deref(), Some("main"));
        assert_eq!(
            git.payload["omitted_unsafe_artifact_count"],
            Value::from(1_u64)
        );
        assert_eq!(git.artifacts.len(), 1);
        assert_eq!(
            git.artifacts[0].uri,
            "tests/e2e/logs/git-status-ft-ogr3n2.json"
        );
    }

    #[test]
    fn agent_mail_unavailable_becomes_source_unavailable_event() {
        let snapshot = AgentMailSnapshot {
            source_id: "mail-red".to_string(),
            health_state: "unavailable".to_string(),
            unavailable_reason: Some("database unavailable after retry".to_string()),
            reason_codes: vec!["agent_mail.database_error".to_string()],
            ..Default::default()
        };
        let event = adapt_agent_mail_snapshot(&snapshot, &context(), 99).unwrap();
        assert_eq!(event.source, SwarmCausalEventSource::SourceUnavailable);
        assert_eq!(event.event_class, CausalEventClass::CommunicationOutage);
        assert_eq!(event.payload["reason"], "agent_mail.unavailable");
        assert_eq!(
            event.privacy.redaction_status,
            CausalRedactionStatus::Unavailable
        );
    }

    #[test]
    fn missing_required_source_keys_emit_unavailable_events() {
        let rch = RchSnapshot {
            source_id: "missing-build".to_string(),
            preflight_refusal_reason: Some("no admissible workers".to_string()),
            ..Default::default()
        };
        let event = adapt_rch_snapshot(&rch, &context(), 21).unwrap();
        assert_eq!(event.source, SwarmCausalEventSource::SourceUnavailable);
        assert_eq!(event.payload["reason"], "rch.missing_build_id");

        let beads = BeadsSnapshot {
            source_id: "missing-issue".to_string(),
            ..Default::default()
        };
        let event = adapt_beads_snapshot(&beads, &context(), 22).unwrap();
        assert_eq!(event.source, SwarmCausalEventSource::SourceUnavailable);
        assert_eq!(event.payload["reason"], "beads.missing_issue_id");
    }

    #[test]
    fn malformed_beads_jsonl_isolated_from_valid_rows() {
        let valid = serde_json::to_string(&BeadsSnapshot {
            source_id: "issue-row".to_string(),
            issue_id: "ft-abc12.3".to_string(),
            status: "closed".to_string(),
            updated_at_ms: 1_778_000_000_000,
            artifact_paths: vec![".beads/issues.jsonl".to_string()],
            ..Default::default()
        })
        .unwrap();
        let input = format!("{valid}\n{{not-json}}\n");
        let report = adapt_beads_jsonl(&input, context());
        assert_eq!(report.events.len(), 2);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.events[0].source, SwarmCausalEventSource::Beads);
        assert_eq!(
            report.events[1].source,
            SwarmCausalEventSource::SourceUnavailable
        );
        assert_eq!(report.events[1].payload["reason"], "beads.malformed_jsonl");
    }

    #[test]
    fn oversize_payload_degrades_to_hash_only_reference() {
        let snapshot = BeadsSnapshot {
            source_id: "oversize".to_string(),
            issue_id: "ft-big.1".to_string(),
            status: "open".to_string(),
            updated_at_ms: 1_778_000_000_000,
            payload: json!({
                "comments": vec!["x".repeat(4096); 40],
            }),
            ..Default::default()
        };
        let event = adapt_beads_snapshot(&snapshot, &context(), 55).unwrap();
        assert_eq!(
            event.privacy.redaction_status,
            CausalRedactionStatus::HashOnly
        );
        assert_eq!(
            event.privacy.omission_reason,
            Some(CausalPayloadOmissionReason::HashOnlyByPolicy)
        );
        assert!(
            event
                .privacy
                .omitted_payload_hash_sha256
                .as_ref()
                .is_some_and(|hash| hash.len() == 64)
        );
    }

    #[test]
    fn malformed_source_set_reports_failure_event() {
        let report = adapt_source_set_json("{not-json}", context());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.events.len(), 1);
        assert_eq!(
            report.events[0].source,
            SwarmCausalEventSource::SourceUnavailable
        );
        assert_eq!(
            report.events[0].payload["reason"],
            "source_set.malformed_json"
        );
    }

    #[test]
    fn fixture_contract_id_is_checked_by_test() {
        let source_set: SourceAdapterSet = serde_json::from_str(VALID_SOURCE_SET).unwrap();
        assert_eq!(source_set.contract_id, SOURCE_ADAPTER_CONTRACT_ID_V1);
        assert_eq!(source_set.schema_version, 1);
    }
}
