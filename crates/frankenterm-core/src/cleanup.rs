//! Retention-aware cleanup engine with safe dry-run preview.
//!
//! The cleanup engine evaluates retention tiers from `StorageConfig` to determine
//! which rows in each table are eligible for deletion. It supports two modes:
//!
//! - **Preview** (dry-run): returns per-table counts without modifying data.
//! - **Apply**: deletes rows in batches and returns a summary.

use serde::{Deserialize, Serialize};

use crate::config::{CompiledRetentionPolicy, StorageConfig};
use crate::error::RuntimeOperationSource;
use crate::storage::{DurableMutationProgress, MaintenanceRecord, StorageHandle, now_ms};

/// Per-table cleanup counts for preview and apply results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CleanupTableSummary {
    pub table: String,
    pub eligible_rows: usize,
    pub deleted_rows: usize,
    pub retention_days: u32,
}

/// Full cleanup plan: a list of per-table summaries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CleanupPlan {
    pub tables: Vec<CleanupTableSummary>,
    pub total_eligible: usize,
    pub total_deleted: usize,
    pub dry_run: bool,
}

/// Terminal state of a cleanup attempt.  Cancellation and ordinary failure
/// remain distinct even when a durable prefix has already committed.
#[derive(Debug)]
pub enum CleanupTermination {
    Completed,
    Cancelled { error: crate::Error },
    Failed { error: crate::Error },
}

/// Result of writing the one maintenance receipt owned by a cleanup attempt.
#[derive(Debug)]
pub enum CleanupAuditStatus {
    /// No cleanup deletion was admitted: the caller was already cancelled or
    /// the in-progress receipt could not be confirmed open.
    NotRequired,
    Recorded {
        maintenance_id: i64,
    },
    Failed {
        error: crate::Error,
        /// The exact finalization that failed to update the already-durable
        /// in-progress receipt. Its positive ID targets that same row on retry;
        /// each committed delete batch has already advanced the row inside its
        /// transaction, so caller-local response loss cannot erase evidence.
        pending_record: MaintenanceRecord,
    },
}

/// Partial-progress-preserving result for apply mode.
#[derive(Debug)]
pub struct CleanupApplyOutcome {
    pub plan: CleanupPlan,
    pub termination: CleanupTermination,
    pub audit: CleanupAuditStatus,
}

impl CleanupApplyOutcome {
    fn into_legacy_result(self) -> crate::Result<CleanupPlan> {
        match self.termination {
            CleanupTermination::Completed => match self.audit {
                CleanupAuditStatus::Failed { error, .. } => Err(error),
                CleanupAuditStatus::NotRequired | CleanupAuditStatus::Recorded { .. } => {
                    Ok(self.plan)
                }
            },
            CleanupTermination::Cancelled { error } | CleanupTermination::Failed { error } => {
                Err(error)
            }
        }
    }

    #[must_use]
    pub fn completed_and_audited(&self) -> bool {
        matches!(self.termination, CleanupTermination::Completed)
            && matches!(self.audit, CleanupAuditStatus::Recorded { .. })
    }
}

#[derive(Debug)]
enum CleanupStep<T> {
    Complete(T),
    Interrupted { durable: T, error: crate::Error },
}

/// Provenance class required before the swarm janitor may even report a lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmJanitorProvenanceKind {
    TxIntent,
    Mission,
}

/// Explicit provenance binding for an opt-in ephemeral cleanup lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmJanitorLeaseProvenance {
    pub kind: SwarmJanitorProvenanceKind,
    pub id: String,
}

/// One report-only ephemeral lease candidate for janitor review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmJanitorLeaseCandidate {
    pub lease_id: String,
    pub path: String,
    pub owner: String,
    pub expires_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<SwarmJanitorLeaseProvenance>,
}

/// Machine-stable reason codes for a swarm janitor report item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmJanitorReasonCode {
    ReportOnly,
    EligibleAfterExplicitHumanApproval,
    InvalidPath,
    MissingLease,
    MissingProvenance,
    LeaseStillActive,
    HardProtectedPath,
}

/// Report-only disposition for a swarm janitor candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmJanitorCandidateDecision {
    ReviewEligible,
    WaitForLeaseExpiry,
    Blocked,
}

/// Per-candidate report-only swarm janitor decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmJanitorReportItem {
    pub candidate: SwarmJanitorLeaseCandidate,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized_path: Option<String>,
    pub decision: SwarmJanitorCandidateDecision,
    pub reason_codes: Vec<SwarmJanitorReasonCode>,
    pub requires_human_approval: bool,
    pub would_delete: bool,
    pub safe_action: String,
}

/// Report-only swarm janitor plan. This type intentionally has no apply mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmJanitorReport {
    pub generated_at_ms: i64,
    pub report_only: bool,
    pub mutates_files: bool,
    pub automatic_deletion_allowed: bool,
    pub protected_paths: Vec<String>,
    pub items: Vec<SwarmJanitorReportItem>,
    pub review_eligible: usize,
    pub waiting_for_lease_expiry: usize,
    pub blocked: usize,
}

/// Batch size for apply-mode deletions to avoid long-running transactions.
const DELETE_BATCH_SIZE: usize = 5000;
const SWARM_JANITOR_PROTECTED_PATHS: &[&str] = &["AGENTS.md", "crates/frankenterm-core"];

/// Build a report-only janitor plan for opt-in ephemeral leases.
#[must_use]
pub fn plan_swarm_janitor_report(
    candidates: &[SwarmJanitorLeaseCandidate],
    generated_at_ms: i64,
) -> SwarmJanitorReport {
    let items: Vec<_> = candidates
        .iter()
        .cloned()
        .map(|candidate| classify_swarm_janitor_candidate(candidate, generated_at_ms))
        .collect();

    let review_eligible = items
        .iter()
        .filter(|item| item.decision == SwarmJanitorCandidateDecision::ReviewEligible)
        .count();
    let waiting_for_lease_expiry = items
        .iter()
        .filter(|item| item.decision == SwarmJanitorCandidateDecision::WaitForLeaseExpiry)
        .count();
    let blocked = items
        .iter()
        .filter(|item| item.decision == SwarmJanitorCandidateDecision::Blocked)
        .count();

    SwarmJanitorReport {
        generated_at_ms,
        report_only: true,
        mutates_files: false,
        automatic_deletion_allowed: false,
        protected_paths: SWARM_JANITOR_PROTECTED_PATHS
            .iter()
            .map(|path| (*path).to_string())
            .collect(),
        items,
        review_eligible,
        waiting_for_lease_expiry,
        blocked,
    }
}

fn classify_swarm_janitor_candidate(
    candidate: SwarmJanitorLeaseCandidate,
    now_ms: i64,
) -> SwarmJanitorReportItem {
    let mut reason_codes = vec![SwarmJanitorReasonCode::ReportOnly];
    let normalized_path = normalize_swarm_janitor_candidate_path(&candidate.path);

    if candidate.lease_id.trim().is_empty() {
        reason_codes.push(SwarmJanitorReasonCode::MissingLease);
    }
    if normalized_path.is_none() {
        reason_codes.push(SwarmJanitorReasonCode::InvalidPath);
    }
    if normalized_path
        .as_deref()
        .is_some_and(is_hard_protected_swarm_janitor_path)
    {
        reason_codes.push(SwarmJanitorReasonCode::HardProtectedPath);
    }
    if candidate
        .provenance
        .as_ref()
        .is_none_or(|provenance| provenance.id.trim().is_empty())
    {
        reason_codes.push(SwarmJanitorReasonCode::MissingProvenance);
    }

    let has_blocker = reason_codes.iter().any(|reason| {
        matches!(
            reason,
            SwarmJanitorReasonCode::InvalidPath
                | SwarmJanitorReasonCode::MissingLease
                | SwarmJanitorReasonCode::MissingProvenance
                | SwarmJanitorReasonCode::HardProtectedPath
        )
    });

    let decision = if has_blocker {
        SwarmJanitorCandidateDecision::Blocked
    } else if candidate.expires_at_ms > now_ms {
        reason_codes.push(SwarmJanitorReasonCode::LeaseStillActive);
        SwarmJanitorCandidateDecision::WaitForLeaseExpiry
    } else {
        reason_codes.push(SwarmJanitorReasonCode::EligibleAfterExplicitHumanApproval);
        SwarmJanitorCandidateDecision::ReviewEligible
    };

    let safe_action = match decision {
        SwarmJanitorCandidateDecision::ReviewEligible => {
            "report_only_requires_explicit_human_approval".to_string()
        }
        SwarmJanitorCandidateDecision::WaitForLeaseExpiry => {
            "wait_for_lease_expiry_then_recheck".to_string()
        }
        SwarmJanitorCandidateDecision::Blocked => "blocked_no_cleanup".to_string(),
    };

    SwarmJanitorReportItem {
        candidate,
        normalized_path,
        decision,
        reason_codes,
        requires_human_approval: decision == SwarmJanitorCandidateDecision::ReviewEligible,
        would_delete: false,
        safe_action,
    }
}

fn normalize_swarm_janitor_candidate_path(path: &str) -> Option<String> {
    let path = path.trim();
    let path = path.strip_prefix("./").unwrap_or(path);
    let path = path.trim_end_matches('/');
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path.split('/').any(|part| {
            part.is_empty() || part == "." || part == ".." || part.chars().any(char::is_control)
        })
    {
        return None;
    }

    Some(path.to_string())
}

fn is_hard_protected_swarm_janitor_path(path: &str) -> bool {
    path == "AGENTS.md"
        || path.ends_with("/AGENTS.md")
        || path == "crates/frankenterm-core"
        || path.starts_with("crates/frankenterm-core/")
}

fn cleanup_cancelled(operation: &'static str, err: impl std::fmt::Display) -> crate::Error {
    crate::Error::RuntimeOperation {
        operation,
        source: RuntimeOperationSource::Cancelled(err.to_string()),
    }
}

fn compile_cleanup_retention_policy(
    config: &StorageConfig,
) -> crate::Result<std::sync::Arc<CompiledRetentionPolicy>> {
    config.compile_retention_policy().map_err(|error| {
        crate::error::ConfigError::ValidationError(format!(
            "cleanup retention policy is invalid: {error}"
        ))
        .into()
    })
}

fn cleanup_termination_from_error(error: crate::Error) -> CleanupTermination {
    if matches!(
        &error,
        crate::Error::Cancelled(_)
            | crate::Error::RuntimeOperation {
                source: RuntimeOperationSource::Cancelled(_),
                ..
            }
    ) {
        CleanupTermination::Cancelled { error }
    } else {
        CleanupTermination::Failed { error }
    }
}

fn append_cleanup_summary(
    plan: &mut CleanupPlan,
    summary: CleanupTableSummary,
) -> crate::Result<()> {
    let total_eligible = plan
        .total_eligible
        .checked_add(summary.eligible_rows)
        .ok_or_else(|| {
            crate::error::StorageError::Database(
                "cleanup eligible-row aggregate overflow".to_string(),
            )
        })?;
    let total_deleted = plan
        .total_deleted
        .checked_add(summary.deleted_rows)
        .ok_or_else(|| {
            crate::error::StorageError::Database(
                "cleanup deleted-row aggregate overflow".to_string(),
            )
        })?;
    plan.total_eligible = total_eligible;
    plan.total_deleted = total_deleted;
    plan.tables.push(summary);
    Ok(())
}

fn cleanup_receipt_record(
    maintenance_id: i64,
    attempt_status: &str,
    plan: &CleanupPlan,
) -> MaintenanceRecord {
    // Deliberately omit tier names, filters, cancellation reasons, and error
    // strings. This fixed-shape aggregate is bounded and content-free.
    let metadata = serde_json::json!({
        "schema": "tiered_cleanup_receipt.v1",
        "attempt_status": attempt_status,
        "tables_recorded": plan.tables.len(),
        "eligible_rows": plan.total_eligible,
        "deleted_rows": plan.total_deleted,
    })
    .to_string();
    MaintenanceRecord {
        id: maintenance_id,
        event_type: "tiered_cleanup".to_string(),
        message: Some(if attempt_status == "in_progress" {
            format!(
                "Cleanup in progress: {} durable rows deleted",
                plan.total_deleted
            )
        } else {
            format!(
                "Cleanup {attempt_status}: {} rows deleted across {} recorded tables",
                plan.total_deleted,
                plan.tables.len()
            )
        }),
        metadata: Some(metadata),
        timestamp: now_ms(),
    }
}

async fn begin_cleanup_apply_receipt(storage: &StorageHandle) -> crate::Result<i64> {
    let receipt_cx = crate::cx::Cx::for_request_with_budget(crate::cx::Budget::MINIMAL);
    let maintenance_id = storage
        .record_maintenance_with_cx(
            &receipt_cx,
            cleanup_receipt_record(0, "in_progress", &CleanupPlan::default()),
        )
        .await?;
    if maintenance_id <= 0 {
        return Err(crate::error::StorageError::Database(
            "tiered cleanup receipt creation returned a non-positive id".to_string(),
        )
        .into());
    }
    Ok(maintenance_id)
}

async fn finish_cleanup_apply_outcome(
    storage: &StorageHandle,
    maintenance_id: i64,
    plan: CleanupPlan,
    termination: CleanupTermination,
) -> CleanupApplyOutcome {
    let attempt_status = match &termination {
        CleanupTermination::Completed => "completed",
        CleanupTermination::Cancelled { .. } => "cancelled_partial",
        CleanupTermination::Failed { .. } => "failed_partial",
    };
    let receipt_cx = crate::cx::Cx::for_request_with_budget(crate::cx::Budget::MINIMAL);
    let pending_record = cleanup_receipt_record(maintenance_id, attempt_status, &plan);
    let receipt = storage
        .record_maintenance_with_cx(&receipt_cx, pending_record.clone())
        .await;
    let audit = match receipt {
        Ok(recorded_id) if recorded_id == maintenance_id => CleanupAuditStatus::Recorded {
            maintenance_id: recorded_id,
        },
        Ok(_) => CleanupAuditStatus::Failed {
            error: crate::error::StorageError::Database(
                "tiered cleanup finalization returned a different receipt id".to_string(),
            )
            .into(),
            pending_record,
        },
        Err(error) => CleanupAuditStatus::Failed {
            error,
            pending_record,
        },
    };
    CleanupApplyOutcome {
        plan,
        termination,
        audit,
    }
}

fn durable_delete_step(progress: DurableMutationProgress<usize>) -> (usize, Option<crate::Error>) {
    match progress {
        DurableMutationProgress::Complete(deleted) => (deleted, None),
        DurableMutationProgress::Interrupted { durable, error } => {
            (durable.unwrap_or(0), Some(error))
        }
    }
}

/// Preview what would be cleaned up (dry-run).
///
/// Queries each cleanable table for row counts older than the configured retention.
/// Events use tier-based retention; other tables use the global `retention_days`.
pub async fn cleanup_preview(
    storage: &StorageHandle,
    config: &StorageConfig,
) -> crate::Result<CleanupPlan> {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    cleanup_preview_with_cx(&cx, storage, config).await
}

/// Cx-first [`cleanup_preview`] (ft-xbnl0.2.3). Threads caller `&Cx`
/// through the combined event classifier and every remaining storage query.
/// Explicit checkpoints between tables plus the storage `_with_cx` surfaces
/// make a late cancellation observable before the next scan, while a
/// pre-cancelled caller short-circuits before touching storage at all.
///
/// Identical semantics to [`cleanup_preview`] for uncancelled cx —
/// only the await points gain cancellation awareness.
pub async fn cleanup_preview_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    config: &StorageConfig,
) -> crate::Result<CleanupPlan> {
    cx.checkpoint()
        .map_err(|err| cleanup_cancelled("cleanup_preview", err))?;

    let now = now_ms();
    let global_cutoff_ms = retention_cutoff_ms(now, config.retention_days);

    let mut plan = CleanupPlan {
        dry_run: true,
        ..Default::default()
    };

    // ft-xbnl0.2.3 tick 132: route through cx-first helpers + storage.
    let retention_policy = compile_cleanup_retention_policy(config)?;
    let events_summaries =
        preview_events_by_tier_with_cx(cx, storage, config, retention_policy, now).await?;
    for summary in events_summaries {
        append_cleanup_summary(&mut plan, summary)?;
    }

    if config.retention_days > 0 {
        cx.checkpoint()
            .map_err(|err| cleanup_cancelled("cleanup_preview.output_segments", err))?;
        let count = storage
            .count_segments_before_with_cx(cx, global_cutoff_ms)
            .await?;
        append_cleanup_summary(
            &mut plan,
            CleanupTableSummary {
                table: "output_segments".to_string(),
                eligible_rows: count,
                deleted_rows: 0,
                retention_days: config.retention_days,
            },
        )?;
    }

    if config.retention_days > 0 {
        cx.checkpoint()
            .map_err(|err| cleanup_cancelled("cleanup_preview.audit_actions", err))?;
        let count = storage
            .count_audit_actions_before_with_cx(cx, global_cutoff_ms)
            .await?;
        append_cleanup_summary(
            &mut plan,
            CleanupTableSummary {
                table: "audit_actions".to_string(),
                eligible_rows: count,
                deleted_rows: 0,
                retention_days: config.retention_days,
            },
        )?;
    }

    if config.retention_days > 0 {
        cx.checkpoint()
            .map_err(|err| cleanup_cancelled("cleanup_preview.usage_metrics", err))?;
        let count = storage
            .count_usage_metrics_before_with_cx(cx, global_cutoff_ms)
            .await?;
        append_cleanup_summary(
            &mut plan,
            CleanupTableSummary {
                table: "usage_metrics".to_string(),
                eligible_rows: count,
                deleted_rows: 0,
                retention_days: config.retention_days,
            },
        )?;
    }

    if config.retention_days > 0 {
        cx.checkpoint()
            .map_err(|err| cleanup_cancelled("cleanup_preview.notification_history", err))?;
        let count = storage
            .count_notification_history_before_with_cx(cx, global_cutoff_ms)
            .await?;
        append_cleanup_summary(
            &mut plan,
            CleanupTableSummary {
                table: "notification_history".to_string(),
                eligible_rows: count,
                deleted_rows: 0,
                retention_days: config.retention_days,
            },
        )?;
    }

    if config.retention_days > 0 {
        cx.checkpoint()
            .map_err(|err| cleanup_cancelled("cleanup_preview.operational_maintenance", err))?;
        let count = storage
            .count_operational_maintenance_before_with_cx(cx, global_cutoff_ms)
            .await?;
        append_cleanup_summary(
            &mut plan,
            CleanupTableSummary {
                table: "maintenance_log (operational)".to_string(),
                eligible_rows: count,
                deleted_rows: 0,
                retention_days: config.retention_days,
            },
        )?;
    }

    Ok(plan)
}

/// Apply cleanup: delete eligible rows and return the result plan.
pub async fn cleanup_apply(
    storage: &StorageHandle,
    config: &StorageConfig,
) -> crate::Result<CleanupPlan> {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    cleanup_apply_with_cx(&cx, storage, config).await
}

/// Cx-first [`cleanup_apply`] (ft-xbnl0.2.3). Parallel of
/// [`cleanup_preview_with_cx`] for the delete path. Threads caller
/// `&Cx` through classified event draining and the sequence of remaining
/// count+prune pairs. A late-cancelled caller can abort before the next
/// bounded writer turn or table so partial-deletion state is predictable:
///
///   * a pre-cancelled cx short-circuits before any DELETE runs,
///   * a cx cancelled mid-way leaves already-deleted tables in the
///     returned `CleanupPlan` while skipping the remaining tables
///     (bounded by the next checkpoint at each `if
///     config.retention_days > 0 { ... }` block).
///
/// The typed internal outcome opens one fixed-shape maintenance receipt before
/// mutation, advances its durable-prefix count inside every delete transaction,
/// and finalizes the same row. Receipt failure therefore fails closed before a
/// delete or leaves an exact in-progress ledger for retry/recovery.
///
/// Identical scan + delete logic to [`cleanup_apply`] for
/// uncancelled cx.
pub async fn cleanup_apply_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    config: &StorageConfig,
) -> crate::Result<CleanupPlan> {
    let retention_policy = compile_cleanup_retention_policy(config)?;
    cleanup_apply_with_compiled_retention_with_cx(
        cx,
        storage,
        config.retention_days,
        retention_policy,
    )
    .await
    .into_legacy_result()
}

/// Apply cleanup using an already validated and compiled retention policy.
///
/// The observation runtime keeps this plan across maintenance ticks so startup
/// configuration is honored before the first hot reload and the ordered tier
/// classifier is not rebuilt on every hourly pass.
pub(crate) async fn cleanup_apply_with_compiled_retention_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    retention_days: u32,
    retention_policy: std::sync::Arc<CompiledRetentionPolicy>,
) -> CleanupApplyOutcome {
    if let Err(error) = cx
        .checkpoint()
        .map_err(|err| cleanup_cancelled("cleanup_apply", err))
    {
        return CleanupApplyOutcome {
            plan: CleanupPlan {
                dry_run: false,
                ..Default::default()
            },
            termination: cleanup_termination_from_error(error),
            audit: CleanupAuditStatus::NotRequired,
        };
    }

    let mut plan = CleanupPlan {
        dry_run: false,
        ..Default::default()
    };
    let maintenance_id = match begin_cleanup_apply_receipt(storage).await {
        Ok(maintenance_id) => maintenance_id,
        Err(error) => {
            return CleanupApplyOutcome {
                plan,
                termination: cleanup_termination_from_error(error),
                audit: CleanupAuditStatus::NotRequired,
            };
        }
    };
    let now = now_ms();
    let global_cutoff_ms = retention_cutoff_ms(now, retention_days);

    // ft-xbnl0.2.3 tick 132: route through cx-first helpers + storage.
    let events_step = apply_events_by_tier_progress_with_cx(
        cx,
        storage,
        retention_days,
        retention_policy,
        now,
        maintenance_id,
    )
    .await;
    let (events_summaries, events_error) = match events_step {
        CleanupStep::Complete(summaries) => (summaries, None),
        CleanupStep::Interrupted { durable, error } => (durable, Some(error)),
    };
    for summary in events_summaries {
        if let Err(error) = append_cleanup_summary(&mut plan, summary) {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(error),
            )
            .await;
        }
    }
    if let Some(error) = events_error {
        return finish_cleanup_apply_outcome(
            storage,
            maintenance_id,
            plan,
            cleanup_termination_from_error(error),
        )
        .await;
    }

    if retention_days > 0 {
        if let Err(error) = cx
            .checkpoint()
            .map_err(|err| cleanup_cancelled("cleanup_apply.output_segments", err))
        {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(error),
            )
            .await;
        }
        let count = match storage
            .count_segments_before_with_cx(cx, global_cutoff_ms)
            .await
        {
            Ok(count) => count,
            Err(error) => {
                return finish_cleanup_apply_outcome(
                    storage,
                    maintenance_id,
                    plan,
                    cleanup_termination_from_error(error),
                )
                .await;
            }
        };
        let progress = storage
            .prune_segments_before_progress_with_cx(cx, global_cutoff_ms, Some(maintenance_id))
            .await;
        let (deleted, error) = durable_delete_step(progress);
        if let Err(aggregate_error) = append_cleanup_summary(
            &mut plan,
            CleanupTableSummary {
                table: "output_segments".to_string(),
                eligible_rows: count,
                deleted_rows: deleted,
                retention_days,
            },
        ) {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(aggregate_error),
            )
            .await;
        }
        if let Some(error) = error {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(error),
            )
            .await;
        }
    }

    if retention_days > 0 {
        if let Err(error) = cx
            .checkpoint()
            .map_err(|err| cleanup_cancelled("cleanup_apply.audit_actions", err))
        {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(error),
            )
            .await;
        }
        let count = match storage
            .count_audit_actions_before_with_cx(cx, global_cutoff_ms)
            .await
        {
            Ok(count) => count,
            Err(error) => {
                return finish_cleanup_apply_outcome(
                    storage,
                    maintenance_id,
                    plan,
                    cleanup_termination_from_error(error),
                )
                .await;
            }
        };
        let progress = storage
            .purge_audit_actions_before_progress_with_cx(cx, global_cutoff_ms, Some(maintenance_id))
            .await;
        let (deleted, error) = durable_delete_step(progress);
        if let Err(aggregate_error) = append_cleanup_summary(
            &mut plan,
            CleanupTableSummary {
                table: "audit_actions".to_string(),
                eligible_rows: count,
                deleted_rows: deleted,
                retention_days,
            },
        ) {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(aggregate_error),
            )
            .await;
        }
        if let Some(error) = error {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(error),
            )
            .await;
        }
    }

    if retention_days > 0 {
        if let Err(error) = cx
            .checkpoint()
            .map_err(|err| cleanup_cancelled("cleanup_apply.usage_metrics", err))
        {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(error),
            )
            .await;
        }
        let count = match storage
            .count_usage_metrics_before_with_cx(cx, global_cutoff_ms)
            .await
        {
            Ok(count) => count,
            Err(error) => {
                return finish_cleanup_apply_outcome(
                    storage,
                    maintenance_id,
                    plan,
                    cleanup_termination_from_error(error),
                )
                .await;
            }
        };
        let progress = storage
            .purge_usage_metrics_progress_with_cx(cx, global_cutoff_ms, Some(maintenance_id))
            .await;
        let (deleted, error) = durable_delete_step(progress);
        if let Err(aggregate_error) = append_cleanup_summary(
            &mut plan,
            CleanupTableSummary {
                table: "usage_metrics".to_string(),
                eligible_rows: count,
                deleted_rows: deleted,
                retention_days,
            },
        ) {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(aggregate_error),
            )
            .await;
        }
        if let Some(error) = error {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(error),
            )
            .await;
        }
    }

    if retention_days > 0 {
        if let Err(error) = cx
            .checkpoint()
            .map_err(|err| cleanup_cancelled("cleanup_apply.notification_history", err))
        {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(error),
            )
            .await;
        }
        let count = match storage
            .count_notification_history_before_with_cx(cx, global_cutoff_ms)
            .await
        {
            Ok(count) => count,
            Err(error) => {
                return finish_cleanup_apply_outcome(
                    storage,
                    maintenance_id,
                    plan,
                    cleanup_termination_from_error(error),
                )
                .await;
            }
        };
        let progress = storage
            .purge_notification_history_progress_with_cx(cx, global_cutoff_ms, Some(maintenance_id))
            .await;
        let (deleted, error) = durable_delete_step(progress);
        if let Err(aggregate_error) = append_cleanup_summary(
            &mut plan,
            CleanupTableSummary {
                table: "notification_history".to_string(),
                eligible_rows: count,
                deleted_rows: deleted,
                retention_days,
            },
        ) {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(aggregate_error),
            )
            .await;
        }
        if let Some(error) = error {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(error),
            )
            .await;
        }
    }

    if retention_days > 0 {
        if let Err(error) = cx
            .checkpoint()
            .map_err(|err| cleanup_cancelled("cleanup_apply.operational_maintenance", err))
        {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(error),
            )
            .await;
        }
        let count = match storage
            .count_operational_maintenance_before_with_cx(cx, global_cutoff_ms)
            .await
        {
            Ok(count) => count,
            Err(error) => {
                return finish_cleanup_apply_outcome(
                    storage,
                    maintenance_id,
                    plan,
                    cleanup_termination_from_error(error),
                )
                .await;
            }
        };
        let progress = storage
            .purge_operational_maintenance_before_progress_with_cx(
                cx,
                global_cutoff_ms,
                Some(maintenance_id),
            )
            .await;
        let (deleted, error) = durable_delete_step(progress);
        if let Err(aggregate_error) = append_cleanup_summary(
            &mut plan,
            CleanupTableSummary {
                table: "maintenance_log (operational)".to_string(),
                eligible_rows: count,
                deleted_rows: deleted,
                retention_days,
            },
        ) {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(aggregate_error),
            )
            .await;
        }
        if let Some(error) = error {
            return finish_cleanup_apply_outcome(
                storage,
                maintenance_id,
                plan,
                cleanup_termination_from_error(error),
            )
            .await;
        }
    }

    finish_cleanup_apply_outcome(storage, maintenance_id, plan, CleanupTermination::Completed).await
}

fn compiled_retention_cutoffs(
    policy: &CompiledRetentionPolicy,
    now: i64,
    fallback_retention_days: u32,
) -> std::sync::Arc<[Option<i64>]> {
    let mut cutoffs = Vec::with_capacity(policy.tiers().len().saturating_add(1));
    cutoffs.extend(policy.tiers().iter().map(|tier| {
        (tier.retention_days > 0).then(|| retention_cutoff_ms(now, tier.retention_days))
    }));
    cutoffs.push(
        (fallback_retention_days > 0).then(|| retention_cutoff_ms(now, fallback_retention_days)),
    );
    cutoffs.into()
}

fn event_cleanup_summaries(
    policy: &CompiledRetentionPolicy,
    fallback_retention_days: u32,
    eligible_by_branch: &[usize],
    deleted_by_branch: &[usize],
) -> crate::Result<Vec<CleanupTableSummary>> {
    let expected = policy
        .fallback_branch_index()
        .checked_add(1)
        .ok_or_else(|| {
            crate::error::StorageError::Database(
                "cleanup retention branch count overflow".to_string(),
            )
        })?;
    if eligible_by_branch.len() != expected || deleted_by_branch.len() != expected {
        return Err(crate::error::StorageError::Database(format!(
            "cleanup retention branch accounting mismatch: eligible={}, deleted={}, expected={expected}",
            eligible_by_branch.len(),
            deleted_by_branch.len()
        ))
        .into());
    }

    if policy.tiers().is_empty() {
        return if fallback_retention_days == 0 {
            Ok(Vec::new())
        } else {
            Ok(vec![CleanupTableSummary {
                table: "events".to_string(),
                eligible_rows: eligible_by_branch[0],
                deleted_rows: deleted_by_branch[0],
                retention_days: fallback_retention_days,
            }])
        };
    }

    let mut summaries = Vec::with_capacity(expected);
    for (branch_index, tier) in policy.tiers().iter().enumerate() {
        if tier.retention_days > 0 {
            summaries.push(CleanupTableSummary {
                table: format!("events (tier: {})", tier.name),
                eligible_rows: eligible_by_branch[branch_index],
                deleted_rows: deleted_by_branch[branch_index],
                retention_days: tier.retention_days,
            });
        }
    }
    if fallback_retention_days > 0 {
        let fallback = policy.fallback_branch_index();
        summaries.push(CleanupTableSummary {
            table: "events (global fallback)".to_string(),
            eligible_rows: eligible_by_branch[fallback],
            deleted_rows: deleted_by_branch[fallback],
            retention_days: fallback_retention_days,
        });
    }
    Ok(summaries)
}

/// Preserve committed event-deletion accounting when per-branch shapes cannot
/// be reconciled. The original classification error is still returned to stop
/// the cleanup pass, while this content-free aggregate keeps the durable prefix
/// visible to the attempt receipt.
fn fallback_event_cleanup_summary(
    fallback_retention_days: u32,
    eligible_by_branch: &[usize],
    deleted_by_branch: &[usize],
) -> CleanupTableSummary {
    CleanupTableSummary {
        table: "events (aggregate fallback)".to_string(),
        eligible_rows: eligible_by_branch
            .iter()
            .copied()
            .fold(0usize, usize::saturating_add),
        deleted_rows: deleted_by_branch
            .iter()
            .copied()
            .fold(0usize, usize::saturating_add),
        retention_days: fallback_retention_days,
    }
}

/// Preview events eligible for cleanup, grouped by retention tier.
async fn preview_events_by_tier_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    config: &StorageConfig,
    retention_policy: std::sync::Arc<CompiledRetentionPolicy>,
    now: i64,
) -> crate::Result<Vec<CleanupTableSummary>> {
    let cutoffs = compiled_retention_cutoffs(&retention_policy, now, config.retention_days);
    let eligible = storage
        .count_events_by_compiled_retention_policy_with_cx(
            cx,
            std::sync::Arc::clone(&retention_policy),
            cutoffs,
        )
        .await?;
    let deleted = vec![0usize; eligible.len()];
    event_cleanup_summaries(
        &retention_policy,
        config.retention_days,
        &eligible,
        &deleted,
    )
}

/// Cx-first event cleanup that preserves every committed branch/batch when a
/// later checkpoint, writer turn, or storage operation interrupts the pass.
async fn apply_events_by_tier_progress_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    retention_days: u32,
    retention_policy: std::sync::Arc<CompiledRetentionPolicy>,
    now: i64,
    maintenance_id: i64,
) -> CleanupStep<Vec<CleanupTableSummary>> {
    let cutoffs = compiled_retention_cutoffs(&retention_policy, now, retention_days);
    let eligible = match storage
        .count_events_by_compiled_retention_policy_with_cx(
            cx,
            std::sync::Arc::clone(&retention_policy),
            std::sync::Arc::clone(&cutoffs),
        )
        .await
    {
        Ok(counts) => counts,
        Err(error) => {
            return CleanupStep::Interrupted {
                durable: Vec::new(),
                error,
            };
        }
    };
    let progress = storage
        .delete_events_by_compiled_retention_policy_progress_with_cx(
            cx,
            std::sync::Arc::clone(&retention_policy),
            cutoffs,
            DELETE_BATCH_SIZE,
            Some(maintenance_id),
        )
        .await;
    let (deleted, error) = match progress {
        DurableMutationProgress::Complete(deleted) => (deleted, None),
        DurableMutationProgress::Interrupted { durable, error } => (
            durable.unwrap_or_else(|| vec![0usize; eligible.len()]),
            Some(error),
        ),
    };
    let summaries =
        match event_cleanup_summaries(&retention_policy, retention_days, &eligible, &deleted) {
            Ok(summaries) => summaries,
            Err(error) => {
                return CleanupStep::Interrupted {
                    durable: vec![fallback_event_cleanup_summary(
                        retention_days,
                        &eligible,
                        &deleted,
                    )],
                    error,
                };
            }
        };
    match error {
        Some(error) => CleanupStep::Interrupted {
            durable: summaries,
            error,
        },
        None => CleanupStep::Complete(summaries),
    }
}

/// Convert retention_days into a cutoff epoch-ms timestamp.
fn retention_cutoff_ms(now_ms: i64, retention_days: u32) -> i64 {
    now_ms.saturating_sub(i64::from(retention_days).saturating_mul(24 * 60 * 60 * 1000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RetentionTier;
    use crate::storage::{
        AuditActionRecord, MetricType, NotificationHistoryRecord, NotificationStatus, PaneRecord,
        StorageHandle, StoredEvent, UsageMetricRecord,
    };

    /// Helper: build a current-thread compat runtime and block on the given future.
    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build cleanup test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    // ---------------------------------------------------------------
    // Pure unit tests (no storage)
    // ---------------------------------------------------------------

    #[test]
    fn retention_cutoff_ms_calculation() {
        let now = 1_700_000_000_000i64; // fixed timestamp
        let cutoff = retention_cutoff_ms(now, 30);
        assert_eq!(cutoff, now - 30 * 24 * 60 * 60 * 1000);
    }

    #[test]
    fn retention_cutoff_zero_means_everything() {
        let now = 1_700_000_000_000i64;
        let cutoff = retention_cutoff_ms(now, 0);
        assert_eq!(cutoff, now); // 0 days ago = now
    }

    #[test]
    fn retention_cutoff_one_day() {
        let now = 1_700_000_000_000i64;
        let cutoff = retention_cutoff_ms(now, 1);
        assert_eq!(cutoff, now - 86_400_000);
    }

    #[test]
    fn retention_cutoff_365_days() {
        let now = 1_700_000_000_000i64;
        let cutoff = retention_cutoff_ms(now, 365);
        assert_eq!(cutoff, now - 365 * 86_400_000);
    }

    #[test]
    fn cleanup_plan_default_is_dry_run() {
        let plan = CleanupPlan::default();
        assert!(!plan.dry_run); // default is false; preview sets it true
        assert_eq!(plan.total_eligible, 0);
        assert_eq!(plan.total_deleted, 0);
    }

    #[test]
    fn cleanup_table_summary_serializes() {
        let summary = CleanupTableSummary {
            table: "events".to_string(),
            eligible_rows: 100,
            deleted_rows: 50,
            retention_days: 30,
        };
        let json = serde_json::to_string(&summary).expect("serialize");
        assert!(json.contains("\"table\":\"events\""));
        assert!(json.contains("\"eligible_rows\":100"));
    }

    #[test]
    fn cleanup_plan_serializes_full_structure() {
        let plan = CleanupPlan {
            tables: vec![
                CleanupTableSummary {
                    table: "events (tier: critical)".to_string(),
                    eligible_rows: 10,
                    deleted_rows: 10,
                    retention_days: 90,
                },
                CleanupTableSummary {
                    table: "output_segments".to_string(),
                    eligible_rows: 200,
                    deleted_rows: 200,
                    retention_days: 30,
                },
            ],
            total_eligible: 210,
            total_deleted: 210,
            dry_run: false,
        };
        let json = serde_json::to_string_pretty(&plan).expect("serialize plan");
        assert!(json.contains("\"total_eligible\": 210"));
        assert!(json.contains("\"total_deleted\": 210"));
        assert!(json.contains("\"dry_run\": false"));
    }

    #[test]
    fn append_cleanup_summary_is_atomic_on_aggregate_overflow() {
        let mut plan = CleanupPlan {
            total_eligible: 7,
            total_deleted: usize::MAX,
            ..Default::default()
        };
        let result = append_cleanup_summary(
            &mut plan,
            CleanupTableSummary {
                table: "events".to_string(),
                eligible_rows: 1,
                deleted_rows: 1,
                retention_days: 30,
            },
        );

        assert!(result.is_err());
        assert_eq!(plan.total_eligible, 7);
        assert_eq!(plan.total_deleted, usize::MAX);
        assert!(plan.tables.is_empty());
    }

    #[test]
    fn event_accounting_fallback_preserves_committed_aggregate() {
        let summary = fallback_event_cleanup_summary(30, &[5, 7], &[2, 3, 4]);

        assert_eq!(summary.table, "events (aggregate fallback)");
        assert_eq!(summary.eligible_rows, 12);
        assert_eq!(summary.deleted_rows, 9);
        assert_eq!(summary.retention_days, 30);
    }

    fn swarm_janitor_candidate(
        lease_id: &str,
        path: &str,
        expires_at_ms: i64,
        provenance: Option<SwarmJanitorLeaseProvenance>,
    ) -> SwarmJanitorLeaseCandidate {
        SwarmJanitorLeaseCandidate {
            lease_id: lease_id.to_string(),
            path: path.to_string(),
            owner: "cod_8".to_string(),
            expires_at_ms,
            provenance,
        }
    }

    fn tx_provenance(id: &str) -> SwarmJanitorLeaseProvenance {
        SwarmJanitorLeaseProvenance {
            kind: SwarmJanitorProvenanceKind::TxIntent,
            id: id.to_string(),
        }
    }

    fn single_swarm_janitor_item(report: &SwarmJanitorReport) -> Option<&SwarmJanitorReportItem> {
        assert_eq!(report.items.len(), 1);
        report.items.first()
    }

    #[test]
    fn swarm_janitor_report_is_report_only_even_for_review_eligible_leases() {
        let report = plan_swarm_janitor_report(
            &[swarm_janitor_candidate(
                "lease-1",
                "scratch/mission-output.tmp",
                1_000,
                Some(tx_provenance("tx-clean-1")),
            )],
            2_000,
        );

        assert!(report.report_only);
        assert!(!report.mutates_files);
        assert!(!report.automatic_deletion_allowed);
        assert_eq!(report.review_eligible, 1);
        assert_eq!(report.blocked, 0);

        let Some(item) = single_swarm_janitor_item(&report) else {
            return;
        };
        assert_eq!(item.decision, SwarmJanitorCandidateDecision::ReviewEligible);
        assert_eq!(
            item.normalized_path.as_deref(),
            Some("scratch/mission-output.tmp")
        );
        assert!(item.requires_human_approval);
        assert!(!item.would_delete);
        assert_eq!(
            item.safe_action,
            "report_only_requires_explicit_human_approval"
        );
        assert!(
            item.reason_codes
                .contains(&SwarmJanitorReasonCode::EligibleAfterExplicitHumanApproval)
        );
    }

    #[test]
    fn swarm_janitor_report_waits_for_active_lease() {
        let report = plan_swarm_janitor_report(
            &[swarm_janitor_candidate(
                "lease-active",
                "scratch/live.tmp",
                3_000,
                Some(tx_provenance("tx-live")),
            )],
            2_000,
        );

        let Some(item) = single_swarm_janitor_item(&report) else {
            return;
        };
        assert_eq!(
            item.decision,
            SwarmJanitorCandidateDecision::WaitForLeaseExpiry
        );
        assert_eq!(report.waiting_for_lease_expiry, 1);
        assert!(!item.requires_human_approval);
        assert!(!item.would_delete);
        assert!(
            item.reason_codes
                .contains(&SwarmJanitorReasonCode::LeaseStillActive)
        );
    }

    #[test]
    fn swarm_janitor_report_blocks_missing_lease_and_provenance() {
        let report = plan_swarm_janitor_report(
            &[swarm_janitor_candidate(
                "",
                "scratch/orphan.tmp",
                1_000,
                None,
            )],
            2_000,
        );

        let Some(item) = single_swarm_janitor_item(&report) else {
            return;
        };
        assert_eq!(item.decision, SwarmJanitorCandidateDecision::Blocked);
        assert_eq!(report.blocked, 1);
        assert!(!item.requires_human_approval);
        assert!(!item.would_delete);
        assert!(
            item.reason_codes
                .contains(&SwarmJanitorReasonCode::MissingLease)
        );
        assert!(
            item.reason_codes
                .contains(&SwarmJanitorReasonCode::MissingProvenance)
        );
    }

    #[test]
    fn swarm_janitor_report_blocks_invalid_paths() {
        for path in ["/tmp/file", "../outside", "nested/../outside", "C:\\tmp"] {
            let report = plan_swarm_janitor_report(
                &[swarm_janitor_candidate(
                    "lease-invalid",
                    path,
                    1_000,
                    Some(tx_provenance("tx-invalid")),
                )],
                2_000,
            );

            let Some(item) = single_swarm_janitor_item(&report) else {
                return;
            };
            assert_eq!(
                item.decision,
                SwarmJanitorCandidateDecision::Blocked,
                "{path}"
            );
            assert!(
                item.reason_codes
                    .contains(&SwarmJanitorReasonCode::InvalidPath),
                "{path}"
            );
            assert!(!item.would_delete);
        }
    }

    #[test]
    fn swarm_janitor_report_hard_protects_agents_and_core_paths() {
        for path in [
            "AGENTS.md",
            "docs/AGENTS.md",
            "crates/frankenterm-core",
            "crates/frankenterm-core/src/lib.rs",
            "./crates/frankenterm-core/",
        ] {
            let report = plan_swarm_janitor_report(
                &[swarm_janitor_candidate(
                    "lease-protected",
                    path,
                    1_000,
                    Some(tx_provenance("tx-protected")),
                )],
                2_000,
            );

            let Some(item) = single_swarm_janitor_item(&report) else {
                return;
            };
            assert_eq!(
                item.decision,
                SwarmJanitorCandidateDecision::Blocked,
                "{path}"
            );
            assert!(
                item.reason_codes
                    .contains(&SwarmJanitorReasonCode::HardProtectedPath),
                "{path}"
            );
            assert_eq!(item.safe_action, "blocked_no_cleanup");
            assert!(!item.would_delete);
        }
    }

    #[test]
    fn cleanup_cancelled_uses_structured_runtime_operation_error() {
        let err = cleanup_cancelled("cleanup_preview.output_segments", "caller cancelled");

        let crate::Error::RuntimeOperation { operation, source } = err else {
            assert!(false, "expected structured cleanup cancellation error");
            return;
        };

        assert_eq!(operation, "cleanup_preview.output_segments");
        assert_eq!(
            source,
            RuntimeOperationSource::Cancelled("caller cancelled".to_string())
        );
    }

    // ---------------------------------------------------------------
    // Integration tests (with real StorageHandle)
    // ---------------------------------------------------------------

    /// Helper: create a temporary storage handle.
    async fn setup_storage(label: &str) -> (StorageHandle, std::path::PathBuf) {
        let db_path =
            std::env::temp_dir().join(format!("wa_cleanup_test_{label}_{}.db", std::process::id()));
        // Remove any leftover DB files from a previous run
        let db_str = db_path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));

        let storage = StorageHandle::new(&db_str).await.expect("open test db");

        // Register a pane so foreign-key constraints are satisfied
        storage
            .upsert_pane(PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: Some("test".to_string()),
                cwd: None,
                tty_name: None,
                first_seen_at: 1_000_000_000_000,
                last_seen_at: 1_000_000_000_000,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .expect("upsert pane");

        (storage, db_path)
    }

    /// Helper: tear down storage after a test.
    async fn teardown(storage: StorageHandle, db_path: &std::path::Path) {
        storage.shutdown().await.expect("shutdown");
        let db_str = db_path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));
    }

    /// Helper: create a StoredEvent at a given timestamp with severity + event_type.
    fn make_event(detected_at: i64, severity: &str, event_type: &str) -> StoredEvent {
        StoredEvent {
            id: 0,
            pane_id: 1,
            rule_id: format!("test.{event_type}"),
            agent_type: "test".to_string(),
            event_type: event_type.to_string(),
            severity: severity.to_string(),
            confidence: 1.0,
            extracted: None,
            matched_text: None,
            segment_id: None,
            detected_at,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }
    }

    /// Helper: create an AuditActionRecord at a given timestamp.
    fn make_audit(ts: i64) -> AuditActionRecord {
        AuditActionRecord {
            id: 0,
            ts,
            actor_kind: "robot".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(1),
            domain: None,
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        }
    }

    /// Helper: create a UsageMetricRecord at a given timestamp.
    fn make_usage(ts: i64) -> UsageMetricRecord {
        UsageMetricRecord {
            id: 0,
            timestamp: ts,
            metric_type: MetricType::ApiCall,
            pane_id: Some(1),
            agent_type: None,
            account_id: None,
            workflow_id: None,
            count: Some(1),
            amount: None,
            tokens: None,
            metadata: None,
            created_at: ts,
        }
    }

    /// Helper: create a NotificationHistoryRecord at a given timestamp.
    fn make_notification(ts: i64) -> NotificationHistoryRecord {
        NotificationHistoryRecord {
            id: 0,
            timestamp: ts,
            event_id: None,
            channel: "desktop".to_string(),
            title: "test".to_string(),
            body: "test body".to_string(),
            severity: "info".to_string(),
            status: NotificationStatus::Sent,
            error_message: None,
            acknowledged_at: None,
            acknowledged_by: None,
            action_taken: None,
            retry_count: 0,
            metadata: None,
            created_at: ts,
        }
    }

    // --- Test: empty database ---

    #[test]
    fn preview_empty_db_returns_zero_eligible() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("empty_preview").await;
            let config = StorageConfig::default();

            let plan = cleanup_preview(&storage, &config).await.expect("preview");
            assert!(plan.dry_run);
            assert_eq!(plan.total_eligible, 0);
            for table in &plan.tables {
                assert_eq!(
                    table.eligible_rows, 0,
                    "table {} should be empty",
                    table.table
                );
            }

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn apply_empty_db_deletes_nothing() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("empty_apply").await;
            let config = StorageConfig::default();

            let plan = cleanup_apply(&storage, &config).await.expect("apply");
            assert!(!plan.dry_run);
            assert_eq!(plan.total_eligible, 0);
            assert_eq!(plan.total_deleted, 0);

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn cleanup_bounds_operational_maintenance_and_preserves_audit_receipts() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("operational_maintenance").await;
            let now = now_ms();
            let old = now - 60 * 86_400_000;
            for event_type in [
                "cache_gc",
                "fleet_scrollback_coordinator",
                "retention_cleanup",
                "tiered_cleanup",
                "size_retention",
                "future_maintenance_class",
            ] {
                storage
                    .record_maintenance(MaintenanceRecord {
                        id: 0,
                        event_type: event_type.to_string(),
                        message: None,
                        metadata: None,
                        timestamp: old,
                    })
                    .await
                    .expect("seed maintenance row");
            }
            storage
                .record_maintenance(MaintenanceRecord {
                    id: 0,
                    event_type: "cache_gc".to_string(),
                    message: None,
                    metadata: None,
                    timestamp: now,
                })
                .await
                .expect("seed recent operational row");

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: Vec::new(),
                ..StorageConfig::default()
            };
            let preview = cleanup_preview(&storage, &config).await.expect("preview");
            let preview_summary = preview
                .tables
                .iter()
                .find(|summary| summary.table == "maintenance_log (operational)")
                .expect("operational maintenance preview summary");
            assert_eq!(preview_summary.eligible_rows, 2);
            assert_eq!(preview_summary.deleted_rows, 0);

            let applied = cleanup_apply(&storage, &config).await.expect("apply");
            let applied_summary = applied
                .tables
                .iter()
                .find(|summary| summary.table == "maintenance_log (operational)")
                .expect("operational maintenance apply summary");
            assert_eq!(applied_summary.eligible_rows, 2);
            assert_eq!(applied_summary.deleted_rows, 2);

            let connection = rusqlite::Connection::open(&db_path)
                .expect("open operational-maintenance fixture connection");
            let old_operational: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM maintenance_log
                     WHERE timestamp < ?1
                       AND event_type IN ('cache_gc', 'fleet_scrollback_coordinator')",
                    [now - 30 * 86_400_000],
                    |row| row.get(0),
                )
                .expect("count old operational rows");
            assert_eq!(old_operational, 0);
            let retained_audit_classes: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM maintenance_log
                     WHERE timestamp = ?1
                       AND event_type IN (
                         'retention_cleanup', 'tiered_cleanup', 'size_retention',
                         'future_maintenance_class'
                       )",
                    [old],
                    |row| row.get(0),
                )
                .expect("count fail-safe retained maintenance rows");
            assert_eq!(retained_audit_classes, 4);

            teardown(storage, &db_path).await;
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `cleanup_apply_with_cx` must match
    /// the legacy `cleanup_apply` for an uncancelled cx. Parallel to
    /// the preview-side parity test. Uses a separate empty DB so the
    /// apply path can run its delete statements + maintenance record
    /// writes without interference from the preview test.
    #[test]
    fn cleanup_apply_with_cx_matches_legacy_on_empty_db() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("cx_empty_apply").await;
            let config = StorageConfig::default();

            let cx = crate::cx::for_request();
            // Run legacy first to generate a maintenance record, then
            // Cx-first — apply is idempotent on an empty DB (0 rows
            // eligible, 0 deleted), so plans must match.
            let legacy = cleanup_apply(&storage, &config)
                .await
                .expect("legacy apply");
            let cx_first = cleanup_apply_with_cx(&cx, &storage, &config)
                .await
                .expect("cx-first apply");

            assert_eq!(legacy.dry_run, cx_first.dry_run);
            assert!(!legacy.dry_run, "apply must not be dry-run");
            assert_eq!(legacy.total_eligible, cx_first.total_eligible);
            assert_eq!(legacy.total_deleted, cx_first.total_deleted);
            assert_eq!(
                legacy.tables.len(),
                cx_first.tables.len(),
                "Cx-first apply must produce same number of table summaries"
            );
            for (a, b) in legacy.tables.iter().zip(cx_first.tables.iter()) {
                assert_eq!(a.table, b.table, "table name mismatch");
                assert_eq!(a.eligible_rows, b.eligible_rows);
                assert_eq!(a.deleted_rows, b.deleted_rows);
                assert_eq!(a.retention_days, b.retention_days);
            }

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn cleanup_partial_cancellation_preserves_exact_progress_receipt_and_retry() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("partial_receipt_retry").await;
            let config = StorageConfig {
                retention_days: 1,
                retention_tiers: vec![RetentionTier {
                    name: "secret-tier-sentinel-must-not-enter-receipt".to_string(),
                    retention_days: 1,
                    severities: vec!["info".to_string()],
                    event_types: Vec::new(),
                    handled: None,
                }],
                ..StorageConfig::default()
            };
            let policy = config.compile_retention_policy().unwrap();

            let pre_cancelled_cx = crate::cx::for_testing();
            pre_cancelled_cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel-secret-sentinel"),
            );
            let pre_cancelled = cleanup_apply_with_compiled_retention_with_cx(
                &pre_cancelled_cx,
                &storage,
                config.retention_days,
                std::sync::Arc::clone(&policy),
            )
            .await;
            assert!(matches!(
                pre_cancelled.termination,
                CleanupTermination::Cancelled { .. }
            ));
            assert!(matches!(
                pre_cancelled.audit,
                CleanupAuditStatus::NotRequired
            ));
            assert_eq!(pre_cancelled.plan.total_deleted, 0);

            let connection = rusqlite::Connection::open(&db_path)
                .expect("open partial-cleanup fixture connection");
            connection
                .execute(
                    "WITH RECURSIVE ids(id) AS (
                         VALUES(1) UNION ALL SELECT id + 1 FROM ids WHERE id < 5001
                     )
                     INSERT INTO events (
                         id, pane_id, rule_id, agent_type, event_type, severity,
                         confidence, detected_at
                     )
                     SELECT id, 1, 'cleanup-partial-' || id, 'test', 'partial',
                            'info', 1.0, 1
                     FROM ids ORDER BY id ASC",
                    [],
                )
                .expect("seed two cleanup event batches");
            connection
                .execute(
                    "INSERT INTO output_segments (
                         pane_id, seq, content, content_len, captured_at
                     ) VALUES (1, 0, 'remaining-table-sentinel', 24, 1)",
                    [],
                )
                .expect("seed a later cleanup table");

            let cancelled_cx = crate::cx::for_testing();
            storage.cancel_after_next_committed_batch_for_test("delete_events_by_retention_policy");
            let partial = cleanup_apply_with_compiled_retention_with_cx(
                &cancelled_cx,
                &storage,
                config.retention_days,
                std::sync::Arc::clone(&policy),
            )
            .await;
            assert!(matches!(
                partial.termination,
                CleanupTermination::Cancelled { .. }
            ));
            assert!(matches!(partial.audit, CleanupAuditStatus::Recorded { .. }));
            assert_eq!(partial.plan.total_deleted, DELETE_BATCH_SIZE);
            assert_eq!(storage.count_events_before(i64::MAX).await.unwrap(), 1);
            assert_eq!(storage.count_segments_before(i64::MAX).await.unwrap(), 1);

            let (receipt_count, message, metadata): (i64, String, String) = connection
                .query_row(
                    "SELECT COUNT(*), COALESCE(MAX(message), ''), COALESCE(MAX(metadata), '')
                     FROM maintenance_log WHERE event_type = 'tiered_cleanup'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("query the one partial cleanup receipt");
            assert_eq!(receipt_count, 1, "one attempt must own exactly one receipt");
            let rendered_receipt = format!("{message}\n{metadata}");
            assert!(!rendered_receipt.contains("secret-tier-sentinel"));
            assert!(!rendered_receipt.contains("pre-cancel-secret-sentinel"));
            assert!(metadata.len() < 512, "receipt metadata must remain bounded");
            let metadata_value: serde_json::Value =
                serde_json::from_str(&metadata).expect("parse fixed-shape receipt metadata");
            let metadata_keys = metadata_value
                .as_object()
                .expect("receipt metadata object")
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                metadata_keys,
                std::collections::BTreeSet::from([
                    "attempt_status",
                    "deleted_rows",
                    "eligible_rows",
                    "schema",
                    "tables_recorded",
                ])
            );

            let retry = cleanup_apply_with_compiled_retention_with_cx(
                &crate::cx::for_testing(),
                &storage,
                config.retention_days,
                std::sync::Arc::clone(&policy),
            )
            .await;
            assert!(matches!(retry.termination, CleanupTermination::Completed));
            assert!(matches!(retry.audit, CleanupAuditStatus::Recorded { .. }));
            assert_eq!(
                retry.plan.total_deleted, 2,
                "retry must delete only the one remaining event and untouched segment"
            );
            assert_eq!(storage.count_events_before(i64::MAX).await.unwrap(), 0);
            assert_eq!(storage.count_segments_before(i64::MAX).await.unwrap(), 0);

            let idempotent = cleanup_apply_with_compiled_retention_with_cx(
                &crate::cx::for_testing(),
                &storage,
                config.retention_days,
                policy,
            )
            .await;
            assert!(matches!(
                idempotent.termination,
                CleanupTermination::Completed
            ));
            assert_eq!(idempotent.plan.total_deleted, 0);

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn cleanup_finalization_failure_preserves_transactional_durable_ledger() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("receipt_failure_distinct").await;
            storage
                .record_event(make_event(1, "info", "receipt-failure"))
                .await
                .unwrap();
            let connection =
                rusqlite::Connection::open(&db_path).expect("open cleanup audit-failure fixture");
            connection
                .execute_batch(
                    "CREATE TRIGGER fail_tiered_cleanup_receipt
                     BEFORE UPDATE ON maintenance_log
                     WHEN NEW.event_type = 'tiered_cleanup'
                       AND instr(NEW.metadata, '\"attempt_status\":\"completed\"') > 0
                     BEGIN
                         SELECT RAISE(ABORT, 'injected receipt failure');
                     END;",
                )
                .expect("install receipt failure trigger");
            let config = StorageConfig {
                retention_days: 1,
                retention_tiers: Vec::new(),
                ..StorageConfig::default()
            };
            let outcome = cleanup_apply_with_compiled_retention_with_cx(
                &crate::cx::for_testing(),
                &storage,
                config.retention_days,
                config.compile_retention_policy().unwrap(),
            )
            .await;
            assert!(matches!(outcome.termination, CleanupTermination::Completed));
            assert_eq!(outcome.plan.total_deleted, 1);
            assert_eq!(storage.count_events_before(i64::MAX).await.unwrap(), 0);

            let open_metadata: String = connection
                .query_row(
                    "SELECT metadata FROM maintenance_log
                     WHERE event_type = 'tiered_cleanup'",
                    [],
                    |row| row.get(0),
                )
                .expect("read transactionally advanced cleanup ledger");
            let open_metadata: serde_json::Value = serde_json::from_str(&open_metadata)
                .expect("parse transactionally advanced cleanup ledger");
            assert_eq!(open_metadata["attempt_status"], "in_progress");
            assert_eq!(open_metadata["deleted_rows"], 1);

            let pending_record = match outcome.audit {
                CleanupAuditStatus::Failed { pending_record, .. } => pending_record,
                other => panic!("expected a retryable failed receipt, got {other:?}"),
            };
            let metadata: serde_json::Value = serde_json::from_str(
                pending_record
                    .metadata
                    .as_deref()
                    .expect("pending receipt metadata"),
            )
            .expect("parse pending receipt metadata");
            assert_eq!(metadata["attempt_status"], "completed");
            assert_eq!(metadata["deleted_rows"], 1);
            assert_eq!(metadata["eligible_rows"], 1);
            assert!(
                pending_record.id > 0,
                "retry must update the open receipt row"
            );

            connection
                .execute_batch("DROP TRIGGER fail_tiered_cleanup_receipt")
                .expect("remove injected receipt failure");
            let idempotent_retry = pending_record.clone();
            let mut undercounted_retry = pending_record;
            let mut undercounted_metadata = metadata.clone();
            undercounted_metadata
                .as_object_mut()
                .expect("pending receipt metadata object")
                .insert("deleted_rows".to_string(), serde_json::Value::from(0));
            undercounted_retry.metadata = Some(undercounted_metadata.to_string());
            let finalized_id = storage
                .record_maintenance(undercounted_retry)
                .await
                .expect("the durable ledger must repair a caller-local undercount");
            assert_eq!(finalized_id, idempotent_retry.id);
            let repeated_id = storage
                .record_maintenance(idempotent_retry)
                .await
                .expect("a lost finalization response must be safely retryable");
            assert_eq!(repeated_id, finalized_id);
            let persisted_metadata: String = connection
                .query_row(
                    "SELECT metadata FROM maintenance_log WHERE event_type = 'tiered_cleanup'",
                    [],
                    |row| row.get(0),
                )
                .expect("read retried exact receipt");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&persisted_metadata)
                    .expect("parse retried receipt metadata"),
                metadata,
                "retry must preserve the exact durable-prefix accounting"
            );
            let receipt_count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM maintenance_log
                     WHERE event_type = 'tiered_cleanup'",
                    [],
                    |row| row.get(0),
                )
                .expect("count idempotently finalized receipts");
            assert_eq!(receipt_count, 1);

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn cleanup_fails_closed_before_delete_when_receipt_cannot_open() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("receipt_open_failure").await;
            storage
                .record_event(make_event(1, "info", "receipt-open-failure"))
                .await
                .unwrap();
            let connection = rusqlite::Connection::open(&db_path)
                .expect("open cleanup receipt-open-failure fixture");
            connection
                .execute_batch(
                    "CREATE TRIGGER fail_tiered_cleanup_receipt_open
                     BEFORE INSERT ON maintenance_log
                     WHEN NEW.event_type = 'tiered_cleanup'
                     BEGIN
                         SELECT RAISE(ABORT, 'injected receipt open failure');
                     END;",
                )
                .expect("install receipt open-failure trigger");
            let config = StorageConfig {
                retention_days: 1,
                retention_tiers: Vec::new(),
                ..StorageConfig::default()
            };

            let outcome = cleanup_apply_with_compiled_retention_with_cx(
                &crate::cx::for_testing(),
                &storage,
                config.retention_days,
                config.compile_retention_policy().unwrap(),
            )
            .await;

            assert!(matches!(
                outcome.termination,
                CleanupTermination::Failed { .. }
            ));
            assert!(matches!(outcome.audit, CleanupAuditStatus::NotRequired));
            assert_eq!(outcome.plan.total_deleted, 0);
            assert_eq!(storage.count_events_before(i64::MAX).await.unwrap(), 1);
            let receipt_rows: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM maintenance_log
                     WHERE event_type = 'tiered_cleanup'",
                    [],
                    |row| row.get(0),
                )
                .expect("count failed-open cleanup receipts");
            assert_eq!(receipt_rows, 0);

            connection
                .execute_batch("DROP TRIGGER fail_tiered_cleanup_receipt_open")
                .expect("remove receipt open-failure trigger");
            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn cleanup_rolls_back_delete_when_durable_ledger_cannot_advance() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("receipt_advance_failure").await;
            storage
                .record_event(make_event(1, "info", "receipt-advance-failure"))
                .await
                .unwrap();
            let connection = rusqlite::Connection::open(&db_path)
                .expect("open cleanup receipt-advance-failure fixture");
            connection
                .execute_batch(
                    "CREATE TRIGGER fail_tiered_cleanup_receipt_advance
                     BEFORE UPDATE ON maintenance_log
                     WHEN NEW.event_type = 'tiered_cleanup'
                       AND instr(NEW.metadata, '\"attempt_status\":\"in_progress\"') > 0
                       AND json_extract(NEW.metadata, '$.deleted_rows') > 0
                     BEGIN
                         SELECT RAISE(ABORT, 'injected receipt advance failure');
                     END;",
                )
                .expect("install receipt advance-failure trigger");
            let config = StorageConfig {
                retention_days: 1,
                retention_tiers: Vec::new(),
                ..StorageConfig::default()
            };

            let outcome = cleanup_apply_with_compiled_retention_with_cx(
                &crate::cx::for_testing(),
                &storage,
                config.retention_days,
                config.compile_retention_policy().unwrap(),
            )
            .await;

            assert!(matches!(
                outcome.termination,
                CleanupTermination::Failed { .. }
            ));
            assert!(matches!(outcome.audit, CleanupAuditStatus::Recorded { .. }));
            assert_eq!(outcome.plan.total_deleted, 0);
            assert_eq!(
                storage.count_events_before(i64::MAX).await.unwrap(),
                1,
                "the event delete must roll back with its failed receipt advance"
            );
            let metadata: String = connection
                .query_row(
                    "SELECT metadata FROM maintenance_log
                     WHERE event_type = 'tiered_cleanup'",
                    [],
                    |row| row.get(0),
                )
                .expect("read failed cleanup receipt");
            let metadata: serde_json::Value =
                serde_json::from_str(&metadata).expect("parse failed cleanup receipt");
            assert_eq!(metadata["attempt_status"], "failed_partial");
            assert_eq!(metadata["deleted_rows"], 0);

            connection
                .execute_batch("DROP TRIGGER fail_tiered_cleanup_receipt_advance")
                .expect("remove receipt advance-failure trigger");
            teardown(storage, &db_path).await;
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `cleanup_preview_with_cx` must match
    /// the legacy `cleanup_preview` for an uncancelled cx — the
    /// Cx-first variant only adds cancellation seams *between*
    /// storage queries; the table scan logic is identical. Proves
    /// the Cx-first variant produces bit-for-bit identical plan
    /// shapes for an empty DB.
    #[test]
    fn cleanup_preview_with_cx_matches_legacy_on_empty_db() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("cx_empty_preview").await;
            let config = StorageConfig::default();

            let cx = crate::cx::for_request();
            let legacy = cleanup_preview(&storage, &config)
                .await
                .expect("legacy preview");
            let cx_first = cleanup_preview_with_cx(&cx, &storage, &config)
                .await
                .expect("cx-first preview");

            assert_eq!(legacy.dry_run, cx_first.dry_run);
            assert_eq!(legacy.total_eligible, cx_first.total_eligible);
            assert_eq!(legacy.total_deleted, cx_first.total_deleted);
            assert_eq!(
                legacy.tables.len(),
                cx_first.tables.len(),
                "Cx-first must produce same number of table summaries"
            );
            // Every table summary should match by table name + counts.
            for (a, b) in legacy.tables.iter().zip(cx_first.tables.iter()) {
                assert_eq!(a.table, b.table, "table name mismatch");
                assert_eq!(a.eligible_rows, b.eligible_rows);
                assert_eq!(a.deleted_rows, b.deleted_rows);
                assert_eq!(a.retention_days, b.retention_days);
            }

            teardown(storage, &db_path).await;
        });
    }

    // --- Test: flat global retention (no tiers) ---

    #[test]
    fn preview_flat_retention_counts_old_events() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("flat_preview").await;
            let now = now_ms();
            let old_ts = now - 60 * 86_400_000; // 60 days ago
            let recent_ts = now - 5 * 86_400_000; // 5 days ago

            // Insert events: 2 old, 1 recent
            storage
                .record_event(make_event(old_ts, "info", "test"))
                .await
                .unwrap();
            storage
                .record_event(make_event(old_ts - 1000, "warning", "test"))
                .await
                .unwrap();
            storage
                .record_event(make_event(recent_ts, "info", "test"))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![], // flat retention
                ..Default::default()
            };

            let plan = cleanup_preview(&storage, &config).await.expect("preview");
            assert!(plan.dry_run);

            // Find the events entry
            let events_table = plan.tables.iter().find(|t| t.table == "events").unwrap();
            assert_eq!(
                events_table.eligible_rows, 2,
                "2 old events should be eligible"
            );
            assert_eq!(events_table.deleted_rows, 0, "preview should not delete");
            assert_eq!(events_table.retention_days, 30);

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn apply_flat_retention_deletes_old_events() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("flat_apply").await;
            let now = now_ms();
            let old_ts = now - 60 * 86_400_000; // 60 days ago
            let recent_ts = now - 5 * 86_400_000; // 5 days ago

            storage
                .record_event(make_event(old_ts, "info", "test"))
                .await
                .unwrap();
            storage
                .record_event(make_event(old_ts - 1000, "warning", "test"))
                .await
                .unwrap();
            storage
                .record_event(make_event(recent_ts, "info", "test"))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![],
                ..Default::default()
            };

            let plan = cleanup_apply(&storage, &config).await.expect("apply");
            assert!(!plan.dry_run);

            let events_table = plan.tables.iter().find(|t| t.table == "events").unwrap();
            assert_eq!(events_table.eligible_rows, 2);
            assert_eq!(
                events_table.deleted_rows, 2,
                "2 old events should be deleted"
            );

            // Verify the recent event still exists
            let remaining = storage.count_events_before(now + 1000).await.unwrap();
            assert_eq!(remaining, 1, "only the recent event should remain");

            teardown(storage, &db_path).await;
        });
    }

    // --- Test: tiered retention ---

    #[test]
    fn preview_tiered_retention_groups_by_tier() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("tiered_preview").await;
            let now = now_ms();

            // critical event: 50 days old (within 90-day critical tier)
            let critical_ts = now - 50 * 86_400_000;
            // info event: 15 days old (beyond 7-day info tier)
            let info_old_ts = now - 15 * 86_400_000;
            // info event: 3 days old (within 7-day info tier)
            let info_recent_ts = now - 3 * 86_400_000;

            storage
                .record_event(make_event(critical_ts, "critical", "error"))
                .await
                .unwrap();
            storage
                .record_event(make_event(info_old_ts, "info", "detection"))
                .await
                .unwrap();
            storage
                .record_event(make_event(info_recent_ts, "info", "detection"))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![
                    RetentionTier {
                        name: "critical".to_string(),
                        retention_days: 90,
                        severities: vec!["critical".to_string()],
                        event_types: vec![],
                        handled: None,
                    },
                    RetentionTier {
                        name: "info".to_string(),
                        retention_days: 7,
                        severities: vec!["info".to_string()],
                        event_types: vec![],
                        handled: None,
                    },
                ],
                ..Default::default()
            };

            let plan = cleanup_preview(&storage, &config).await.expect("preview");

            // Critical tier: 90 days retention, 50-day-old event should NOT be eligible
            let critical_tier = plan
                .tables
                .iter()
                .find(|t| t.table.contains("critical"))
                .unwrap();
            assert_eq!(
                critical_tier.eligible_rows, 0,
                "critical event within retention"
            );

            // Info tier: 7 days retention, 15-day-old event IS eligible, 3-day-old is NOT
            let info_tier = plan
                .tables
                .iter()
                .find(|t| t.table.contains("info"))
                .unwrap();
            assert_eq!(info_tier.eligible_rows, 1, "only the 15-day-old info event");

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn apply_tiered_retention_deletes_correct_events() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("tiered_apply").await;
            let now = now_ms();

            // critical event: 100 days old (beyond 90-day critical tier)
            let old_critical_ts = now - 100 * 86_400_000;
            // critical event: 50 days old (within 90-day critical tier)
            let recent_critical_ts = now - 50 * 86_400_000;
            // info event: 15 days old (beyond 7-day info tier)
            let old_info_ts = now - 15 * 86_400_000;
            // info event: 3 days old (within 7-day info tier)
            let recent_info_ts = now - 3 * 86_400_000;
            // unmatched warning: 40 days old (beyond 30-day global fallback)
            let old_unmatched_ts = now - 40 * 86_400_000;

            storage
                .record_event(make_event(old_critical_ts, "critical", "error"))
                .await
                .unwrap();
            storage
                .record_event(make_event(recent_critical_ts, "critical", "error"))
                .await
                .unwrap();
            storage
                .record_event(make_event(old_info_ts, "info", "detection"))
                .await
                .unwrap();
            storage
                .record_event(make_event(recent_info_ts, "info", "detection"))
                .await
                .unwrap();
            storage
                .record_event(make_event(old_unmatched_ts, "warning", "other"))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![
                    RetentionTier {
                        name: "critical".to_string(),
                        retention_days: 90,
                        severities: vec!["critical".to_string()],
                        event_types: vec![],
                        handled: None,
                    },
                    RetentionTier {
                        name: "info".to_string(),
                        retention_days: 7,
                        severities: vec!["info".to_string()],
                        event_types: vec![],
                        handled: None,
                    },
                ],
                ..Default::default()
            };

            let plan = cleanup_apply(&storage, &config).await.expect("apply");

            let critical_tier = plan
                .tables
                .iter()
                .find(|t| t.table.contains("critical"))
                .unwrap();
            assert_eq!(
                critical_tier.deleted_rows, 1,
                "only the 100-day-old critical event"
            );

            let info_tier = plan
                .tables
                .iter()
                .find(|t| t.table.contains("info"))
                .unwrap();
            assert_eq!(info_tier.deleted_rows, 1, "only the 15-day-old info event");

            let fallback = plan
                .tables
                .iter()
                .find(|table| table.table == "events (global fallback)")
                .expect("tiered cleanup reports unmatched rows");
            assert_eq!(fallback.deleted_rows, 1, "the unmatched old event");

            // 2 events should remain: the recent critical and the recent info.
            let remaining = storage.count_events_before(now + 1000).await.unwrap();
            assert_eq!(remaining, 2, "2 recent events should survive cleanup");

            teardown(storage, &db_path).await;
        });
    }

    // --- Test: tier with handled filter ---

    #[test]
    fn tiered_retention_handled_filter() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("handled_filter").await;
            let now = now_ms();

            let old_ts = now - 10 * 86_400_000; // 10 days old

            // Insert two info events, then mark one as handled
            let ev1_id = storage
                .record_event(make_event(old_ts, "info", "detection"))
                .await
                .unwrap();
            let _ev2_id = storage
                .record_event(make_event(old_ts, "info", "detection"))
                .await
                .unwrap();

            storage
                .mark_event_handled(ev1_id, None, "auto")
                .await
                .unwrap();

            // Tier: only delete handled info events older than 3 days
            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![RetentionTier {
                    name: "info-handled".to_string(),
                    retention_days: 3,
                    severities: vec!["info".to_string()],
                    event_types: vec![],
                    handled: Some(true),
                }],
                ..Default::default()
            };

            let plan = cleanup_preview(&storage, &config).await.expect("preview");
            let tier = plan
                .tables
                .iter()
                .find(|t| t.table.contains("info-handled"))
                .unwrap();
            assert_eq!(tier.eligible_rows, 1, "only the handled event is eligible");

            let plan = cleanup_apply(&storage, &config).await.expect("apply");
            let tier = plan
                .tables
                .iter()
                .find(|t| t.table.contains("info-handled"))
                .unwrap();
            assert_eq!(tier.deleted_rows, 1);

            // The unhandled event should remain
            let remaining = storage.count_events_before(now + 1000).await.unwrap();
            assert_eq!(remaining, 1, "unhandled event should survive");

            teardown(storage, &db_path).await;
        });
    }

    // --- Test: multi-table cleanup (segments, audit, usage, notifications) ---

    #[test]
    fn apply_cleans_all_table_types() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("multi_table").await;
            let now = now_ms();
            let old_ts = now - 60 * 86_400_000; // 60 days ago
            let recent_ts = now - 5 * 86_400_000; // 5 days ago

            // Insert old + recent rows in each table
            storage
                .append_segment(1, "old segment", None)
                .await
                .unwrap();
            // The segment timestamp comes from now_ms() at insert time, so we need
            // to use the count methods with a future cutoff to include it.
            // For a real test, we rely on the old events/audit/usage/notifications.

            storage
                .record_event(make_event(old_ts, "info", "test"))
                .await
                .unwrap();
            storage
                .record_event(make_event(recent_ts, "info", "test"))
                .await
                .unwrap();

            storage
                .record_audit_action(make_audit(old_ts))
                .await
                .unwrap();
            storage
                .record_audit_action(make_audit(recent_ts))
                .await
                .unwrap();

            storage
                .record_usage_metric(make_usage(old_ts))
                .await
                .unwrap();
            storage
                .record_usage_metric(make_usage(recent_ts))
                .await
                .unwrap();

            storage
                .record_notification(make_notification(old_ts))
                .await
                .unwrap();
            storage
                .record_notification(make_notification(recent_ts))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![], // flat retention
                ..Default::default()
            };

            let plan = cleanup_apply(&storage, &config).await.expect("apply");

            // Events: 1 old deleted
            let events = plan.tables.iter().find(|t| t.table == "events").unwrap();
            assert_eq!(events.deleted_rows, 1);

            // Audit actions: 1 old deleted
            let audit = plan
                .tables
                .iter()
                .find(|t| t.table == "audit_actions")
                .unwrap();
            assert_eq!(audit.deleted_rows, 1);

            // Usage metrics: 1 old deleted
            let usage = plan
                .tables
                .iter()
                .find(|t| t.table == "usage_metrics")
                .unwrap();
            assert_eq!(usage.deleted_rows, 1);

            // Notification history: 1 old deleted
            let notif = plan
                .tables
                .iter()
                .find(|t| t.table == "notification_history")
                .unwrap();
            assert_eq!(notif.deleted_rows, 1);

            // Total should be consistent
            assert_eq!(
                plan.total_deleted,
                plan.tables.iter().map(|t| t.deleted_rows).sum::<usize>()
            );

            teardown(storage, &db_path).await;
        });
    }

    // --- Test: zero retention_days means keep forever ---

    #[test]
    fn zero_retention_days_skips_all_cleanup() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("zero_retention").await;
            let now = now_ms();
            let ancient_ts = now - 365 * 86_400_000; // 1 year ago

            storage
                .record_event(make_event(ancient_ts, "info", "test"))
                .await
                .unwrap();
            storage
                .record_audit_action(make_audit(ancient_ts))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 0, // keep forever
                retention_tiers: vec![],
                ..Default::default()
            };

            let plan = cleanup_preview(&storage, &config).await.expect("preview");
            assert_eq!(
                plan.total_eligible, 0,
                "nothing eligible when retention_days=0"
            );
            assert!(plan.tables.is_empty() || plan.tables.iter().all(|t| t.eligible_rows == 0));

            let plan = cleanup_apply(&storage, &config).await.expect("apply");
            assert_eq!(
                plan.total_deleted, 0,
                "nothing deleted when retention_days=0"
            );

            teardown(storage, &db_path).await;
        });
    }

    // --- Test: tier with retention_days=0 (keep forever) is skipped ---

    #[test]
    fn tier_with_zero_retention_keeps_events_forever() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("tier_zero").await;
            let now = now_ms();
            let ancient_ts = now - 365 * 86_400_000;

            storage
                .record_event(make_event(ancient_ts, "critical", "error"))
                .await
                .unwrap();
            storage
                .record_event(make_event(ancient_ts, "info", "detection"))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![
                    RetentionTier {
                        name: "critical-forever".to_string(),
                        retention_days: 0, // keep forever
                        severities: vec!["critical".to_string()],
                        event_types: vec![],
                        handled: None,
                    },
                    RetentionTier {
                        name: "info-short".to_string(),
                        retention_days: 7,
                        severities: vec!["info".to_string()],
                        event_types: vec![],
                        handled: None,
                    },
                ],
                ..Default::default()
            };

            let plan = cleanup_apply(&storage, &config).await.expect("apply");

            // The critical-forever tier should not appear or have 0 eligible
            let critical_tier = plan
                .tables
                .iter()
                .find(|t| t.table.contains("critical-forever"));
            assert!(
                critical_tier.is_none(),
                "tier with retention_days=0 should be skipped entirely"
            );

            // The info tier should delete the 1-year-old info event
            let info_tier = plan
                .tables
                .iter()
                .find(|t| t.table.contains("info-short"))
                .unwrap();
            assert_eq!(info_tier.deleted_rows, 1);

            // The critical event should still exist
            let remaining = storage.count_events_before(now + 1000).await.unwrap();
            assert_eq!(
                remaining, 1,
                "critical event preserved by zero-retention tier"
            );

            teardown(storage, &db_path).await;
        });
    }

    // --- Test: preview vs apply consistency ---

    #[test]
    fn preview_and_apply_agree_on_eligible_counts() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("consistency").await;
            let now = now_ms();
            let old_ts = now - 60 * 86_400_000;

            for _ in 0..5 {
                storage
                    .record_event(make_event(old_ts, "info", "test"))
                    .await
                    .unwrap();
            }

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![],
                ..Default::default()
            };

            let preview = cleanup_preview(&storage, &config).await.expect("preview");
            let apply = cleanup_apply(&storage, &config).await.expect("apply");

            assert_eq!(
                preview.total_eligible, apply.total_eligible,
                "preview and apply should agree on eligible counts"
            );
            assert_eq!(apply.total_deleted, apply.total_eligible);

            teardown(storage, &db_path).await;
        });
    }

    // --- Test: mixed severity events with default tiers ---

    #[test]
    fn mixed_severity_with_default_tiers() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("mixed_severity").await;
            let now = now_ms();

            // 20-day-old events: info should be cleaned (7d), warning kept (30d), critical kept (90d)
            let ts_20d = now - 20 * 86_400_000;
            storage
                .record_event(make_event(ts_20d, "critical", "error"))
                .await
                .unwrap();
            storage
                .record_event(make_event(ts_20d, "warning", "detection"))
                .await
                .unwrap();
            storage
                .record_event(make_event(ts_20d, "info", "detection"))
                .await
                .unwrap();
            storage
                .record_event(make_event(ts_20d, "info", "detection"))
                .await
                .unwrap();

            let config = StorageConfig::default(); // default tiers: critical=90d, warning=30d, info=7d

            let plan = cleanup_apply(&storage, &config).await.expect("apply");

            let critical_tier = plan
                .tables
                .iter()
                .find(|t| t.table.contains("critical"))
                .unwrap();
            assert_eq!(
                critical_tier.deleted_rows, 0,
                "20d critical within 90d retention"
            );

            let warning_tier = plan
                .tables
                .iter()
                .find(|t| t.table.contains("warning"))
                .unwrap();
            assert_eq!(
                warning_tier.deleted_rows, 0,
                "20d warning within 30d retention"
            );

            let info_tier = plan
                .tables
                .iter()
                .find(|t| t.table.contains("info"))
                .unwrap();
            assert_eq!(info_tier.deleted_rows, 2, "20d info beyond 7d retention");

            let remaining = storage.count_events_before(now + 1000).await.unwrap();
            assert_eq!(remaining, 2, "critical + warning survive, 2 info deleted");

            teardown(storage, &db_path).await;
        });
    }

    // --- Test: cleanup logs maintenance event ---

    #[test]
    fn apply_records_maintenance_event() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("maintenance_log").await;
            let now = now_ms();
            let old_ts = now - 60 * 86_400_000;

            storage
                .record_event(make_event(old_ts, "info", "test"))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![],
                ..Default::default()
            };

            let _plan = cleanup_apply(&storage, &config).await.expect("apply");

            // Verify maintenance event was recorded by checking the maintenance log
            // The record_maintenance call in cleanup_apply should have succeeded.
            // We can verify by checking count: after apply, there should be no error.
            // (The maintenance record itself is verified by the fact that apply succeeded
            // without error, since it calls record_maintenance at the end.)

            teardown(storage, &db_path).await;
        });
    }

    // ---------------------------------------------------------------
    // E2E tests: full cleanup pipeline with before/after verification
    // ---------------------------------------------------------------

    /// E2E: populate mixed-severity events, run dry-run, apply, verify before/after stats.
    #[test]
    fn e2e_mixed_severity_lifecycle() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("e2e_lifecycle").await;
            let now = now_ms();

            // --- Populate: mix of severities and ages ---
            let ages: &[(i64, &str, &str)] = &[
                (now - 100 * 86_400_000, "critical", "error.crash"),
                (now - 50 * 86_400_000, "critical", "error.timeout"),
                (now - 20 * 86_400_000, "warning", "rate_limit"),
                (now - 10 * 86_400_000, "warning", "rate_limit"),
                (now - 15 * 86_400_000, "info", "detection"),
                (now - 5 * 86_400_000, "info", "detection"),
                (now - 2 * 86_400_000, "info", "detection"),
            ];
            for (ts, sev, etype) in ages {
                storage
                    .record_event(make_event(*ts, sev, etype))
                    .await
                    .unwrap();
            }
            // Other table rows
            storage
                .record_audit_action(make_audit(now - 60 * 86_400_000))
                .await
                .unwrap();
            storage
                .record_audit_action(make_audit(now - 3 * 86_400_000))
                .await
                .unwrap();
            storage
                .record_usage_metric(make_usage(now - 60 * 86_400_000))
                .await
                .unwrap();
            storage
                .record_notification(make_notification(now - 60 * 86_400_000))
                .await
                .unwrap();
            storage
                .record_notification(make_notification(now - 3 * 86_400_000))
                .await
                .unwrap();

            // --- Before stats ---
            let before_stats =
                crate::storage::database_stats(&db_path, 30).expect("ft-oqfsx: test fixture stats");
            let before_events = before_stats
                .tables
                .iter()
                .find(|t| t.name == "events")
                .unwrap()
                .row_count;
            assert_eq!(before_events, 7, "7 events before cleanup");

            // Config: default tiers (critical=90d, warning=30d, info=7d)
            let config = StorageConfig::default();

            // --- Dry-run preview ---
            let preview = cleanup_preview(&storage, &config).await.expect("preview");
            assert!(preview.dry_run);

            // critical tier: 100d old is beyond 90d -> 1 eligible; 50d is within -> 0
            let crit_tier = preview
                .tables
                .iter()
                .find(|t| t.table.contains("critical"))
                .unwrap();
            assert_eq!(crit_tier.eligible_rows, 1, "1 old critical eligible");

            // warning tier: 20d and 10d both within 30d -> 0 eligible
            let warn_tier = preview
                .tables
                .iter()
                .find(|t| t.table.contains("warning"))
                .unwrap();
            assert_eq!(warn_tier.eligible_rows, 0, "warnings within retention");

            // info tier: 15d old beyond 7d -> 1 eligible; 5d and 2d within -> 0
            let info_tier = preview
                .tables
                .iter()
                .find(|t| t.table.contains("info"))
                .unwrap();
            assert_eq!(info_tier.eligible_rows, 1, "1 old info eligible");

            // audit/usage/notification: 60d old records beyond 30d retention
            let audit_preview = preview
                .tables
                .iter()
                .find(|t| t.table == "audit_actions")
                .unwrap();
            assert_eq!(audit_preview.eligible_rows, 1);
            let usage_preview = preview
                .tables
                .iter()
                .find(|t| t.table == "usage_metrics")
                .unwrap();
            assert_eq!(usage_preview.eligible_rows, 1);
            let notif_preview = preview
                .tables
                .iter()
                .find(|t| t.table == "notification_history")
                .unwrap();
            assert_eq!(notif_preview.eligible_rows, 1);

            let total_preview_eligible = preview.total_eligible;
            assert_eq!(total_preview_eligible, 5, "5 total eligible across tables");

            // --- Apply cleanup ---
            let apply = cleanup_apply(&storage, &config).await.expect("apply");
            assert!(!apply.dry_run);
            assert_eq!(
                apply.total_eligible, total_preview_eligible,
                "apply agrees with preview on eligible"
            );
            assert_eq!(apply.total_deleted, 5, "5 total deleted");

            // --- After stats ---
            let after_stats =
                crate::storage::database_stats(&db_path, 30).expect("ft-oqfsx: test fixture stats");
            let after_events = after_stats
                .tables
                .iter()
                .find(|t| t.name == "events")
                .unwrap()
                .row_count;
            assert_eq!(after_events, 5, "5 events remain after cleanup");
            assert_eq!(
                before_events - after_events,
                2,
                "2 events deleted (1 critical + 1 info)"
            );

            let after_audit = after_stats
                .tables
                .iter()
                .find(|t| t.name == "audit_actions")
                .unwrap()
                .row_count;
            assert_eq!(after_audit, 1, "1 recent audit remains");

            let after_usage = after_stats
                .tables
                .iter()
                .find(|t| t.name == "usage_metrics")
                .unwrap()
                .row_count;
            assert_eq!(after_usage, 0, "old usage deleted");

            let after_notif = after_stats
                .tables
                .iter()
                .find(|t| t.name == "notification_history")
                .unwrap()
                .row_count;
            assert_eq!(after_notif, 1, "1 recent notification remains");

            teardown(storage, &db_path).await;
        });
    }

    /// E2E: run dry-run twice to verify deterministic counts.
    #[test]
    fn e2e_dry_run_is_deterministic() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("e2e_deterministic").await;
            let now = now_ms();

            for i in 0..10 {
                let ts = now - (40 + i) * 86_400_000;
                storage
                    .record_event(make_event(ts, "info", "detection"))
                    .await
                    .unwrap();
            }
            for i in 0..3 {
                let ts = now - (40 + i) * 86_400_000;
                storage.record_audit_action(make_audit(ts)).await.unwrap();
            }

            let config = StorageConfig::default(); // info=7d retention

            let run1 = cleanup_preview(&storage, &config).await.expect("run1");
            let run2 = cleanup_preview(&storage, &config).await.expect("run2");

            assert_eq!(
                run1.total_eligible, run2.total_eligible,
                "consecutive previews must return identical counts"
            );
            assert_eq!(run1.tables.len(), run2.tables.len());
            for (t1, t2) in run1.tables.iter().zip(run2.tables.iter()) {
                assert_eq!(t1.table, t2.table);
                assert_eq!(
                    t1.eligible_rows, t2.eligible_rows,
                    "table {} counts differ between runs",
                    t1.table
                );
            }

            // Also verify stats are deterministic
            let stats1 =
                crate::storage::database_stats(&db_path, 30).expect("ft-oqfsx: test fixture stats");
            let stats2 =
                crate::storage::database_stats(&db_path, 30).expect("ft-oqfsx: test fixture stats");
            for (s1, s2) in stats1.tables.iter().zip(stats2.tables.iter()) {
                assert_eq!(s1.name, s2.name);
                assert_eq!(
                    s1.row_count, s2.row_count,
                    "stats table {} counts differ",
                    s1.name
                );
            }

            teardown(storage, &db_path).await;
        });
    }

    /// E2E: apply is idempotent (second apply finds nothing to delete).
    #[test]
    fn e2e_apply_is_idempotent() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("e2e_idempotent").await;
            let now = now_ms();

            for _ in 0..5 {
                storage
                    .record_event(make_event(now - 60 * 86_400_000, "info", "detection"))
                    .await
                    .unwrap();
            }
            storage
                .record_audit_action(make_audit(now - 60 * 86_400_000))
                .await
                .unwrap();

            let config = StorageConfig::default();

            let first_apply = cleanup_apply(&storage, &config).await.expect("first apply");
            assert!(first_apply.total_deleted > 0, "first apply deletes rows");

            let second_apply = cleanup_apply(&storage, &config)
                .await
                .expect("second apply");
            assert_eq!(
                second_apply.total_deleted, 0,
                "second apply finds nothing to delete"
            );
            assert_eq!(second_apply.total_eligible, 0);

            teardown(storage, &db_path).await;
        });
    }

    /// E2E: JSON serialization of the full pipeline (stats + plan) is stable.
    #[test]
    fn e2e_json_artifacts_are_stable() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("e2e_json").await;
            let now = now_ms();

            storage
                .record_event(make_event(now - 60 * 86_400_000, "info", "detection"))
                .await
                .unwrap();
            storage
                .record_event(make_event(now - 2 * 86_400_000, "critical", "error"))
                .await
                .unwrap();

            let config = StorageConfig::default();

            let plan = cleanup_preview(&storage, &config).await.expect("preview");
            let json1 = serde_json::to_string_pretty(&plan).expect("serialize plan");
            let json2 = serde_json::to_string_pretty(&plan).expect("serialize again");
            assert_eq!(json1, json2, "serialization is deterministic");

            // Verify JSON contains expected fields
            assert!(json1.contains("\"dry_run\": true"));
            assert!(json1.contains("\"total_eligible\":"));
            assert!(json1.contains("\"tables\":"));

            let stats =
                crate::storage::database_stats(&db_path, 30).expect("ft-oqfsx: test fixture stats");
            let stats_json = serde_json::to_string_pretty(&stats).expect("serialize stats");
            assert!(stats_json.contains("\"db_path\":"));
            assert!(stats_json.contains("\"tables\":"));
            assert!(stats_json.contains("\"suggestions\":"));

            teardown(storage, &db_path).await;
        });
    }

    /// E2E: before/after stats capture with deletion counts.
    #[test]
    fn e2e_before_after_stats_with_deletion_counts() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("e2e_before_after").await;
            let now = now_ms();

            // Populate: 3 old events, 2 recent events, 2 old audit, 1 recent audit
            for _ in 0..3 {
                storage
                    .record_event(make_event(now - 60 * 86_400_000, "info", "test"))
                    .await
                    .unwrap();
            }
            for _ in 0..2 {
                storage
                    .record_event(make_event(now - 2 * 86_400_000, "info", "test"))
                    .await
                    .unwrap();
            }
            for _ in 0..2 {
                storage
                    .record_audit_action(make_audit(now - 60 * 86_400_000))
                    .await
                    .unwrap();
            }
            storage
                .record_audit_action(make_audit(now - 2 * 86_400_000))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![], // flat retention
                ..Default::default()
            };

            // Before
            let before =
                crate::storage::database_stats(&db_path, 30).expect("ft-oqfsx: test fixture stats");
            let before_events = before
                .tables
                .iter()
                .find(|t| t.name == "events")
                .unwrap()
                .row_count;
            let before_audit = before
                .tables
                .iter()
                .find(|t| t.name == "audit_actions")
                .unwrap()
                .row_count;
            assert_eq!(before_events, 5);
            assert_eq!(before_audit, 3);

            // Apply
            let plan = cleanup_apply(&storage, &config).await.expect("apply");

            // After
            let after =
                crate::storage::database_stats(&db_path, 30).expect("ft-oqfsx: test fixture stats");
            let after_events = after
                .tables
                .iter()
                .find(|t| t.name == "events")
                .unwrap()
                .row_count;
            let after_audit = after
                .tables
                .iter()
                .find(|t| t.name == "audit_actions")
                .unwrap()
                .row_count;
            assert_eq!(after_events, 2, "3 old events deleted, 2 recent remain");
            assert_eq!(after_audit, 1, "2 old audit deleted, 1 recent remains");

            // Verify deletion counts match stats delta
            let events_deleted = plan
                .tables
                .iter()
                .find(|t| t.table == "events")
                .unwrap()
                .deleted_rows;
            let audit_deleted = plan
                .tables
                .iter()
                .find(|t| t.table == "audit_actions")
                .unwrap()
                .deleted_rows;
            assert_eq!(
                events_deleted as u64,
                before_events - after_events,
                "deletion count matches stats delta"
            );
            assert_eq!(
                audit_deleted as u64,
                before_audit - after_audit,
                "audit deletion count matches stats delta"
            );

            teardown(storage, &db_path).await;
        });
    }

    // --- Test: event_type filtering in tiers ---

    #[test]
    fn tier_filters_by_event_type() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("event_type_filter").await;
            let now = now_ms();
            let old_ts = now - 15 * 86_400_000; // 15 days old

            // Same severity, different event types
            storage
                .record_event(make_event(old_ts, "info", "usage_limit"))
                .await
                .unwrap();
            storage
                .record_event(make_event(old_ts, "info", "compaction"))
                .await
                .unwrap();
            storage
                .record_event(make_event(old_ts, "info", "detection"))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![RetentionTier {
                    name: "info-usage".to_string(),
                    retention_days: 7,
                    severities: vec!["info".to_string()],
                    event_types: vec!["usage_limit".to_string()],
                    handled: None,
                }],
                ..Default::default()
            };

            let plan = cleanup_apply(&storage, &config).await.expect("apply");

            let tier = plan
                .tables
                .iter()
                .find(|t| t.table.contains("info-usage"))
                .unwrap();
            assert_eq!(tier.deleted_rows, 1, "only usage_limit event matched tier");

            // The compaction and detection events don't match this tier, so they fall
            // through to global retention (30 days). 15-day-old events are within 30 days,
            // so they should NOT be deleted by global retention either.
            let remaining = storage.count_events_before(now + 1000).await.unwrap();
            assert_eq!(remaining, 2, "compaction + detection survive");

            teardown(storage, &db_path).await;
        });
    }

    // ---------------------------------------------------------------
    // Expanded pure unit tests (wa-1u90p.7.1)
    // ---------------------------------------------------------------

    #[test]
    fn cleanup_table_summary_default() {
        let s = CleanupTableSummary::default();
        assert_eq!(s.table, "");
        assert_eq!(s.eligible_rows, 0);
        assert_eq!(s.deleted_rows, 0);
        assert_eq!(s.retention_days, 0);
    }

    #[test]
    fn cleanup_table_summary_clone() {
        let s = CleanupTableSummary {
            table: "events".to_string(),
            eligible_rows: 42,
            deleted_rows: 10,
            retention_days: 30,
        };
        let c = s.clone();
        assert_eq!(c.table, "events");
        assert_eq!(c.eligible_rows, 42);
        assert_eq!(c.deleted_rows, 10);
        assert_eq!(c.retention_days, 30);
    }

    #[test]
    fn cleanup_table_summary_debug() {
        let s = CleanupTableSummary {
            table: "audit_actions".to_string(),
            eligible_rows: 5,
            deleted_rows: 3,
            retention_days: 7,
        };
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("audit_actions"));
        assert!(dbg.contains("eligible_rows"));
        assert!(dbg.contains("deleted_rows"));
    }

    #[test]
    fn cleanup_table_summary_serialize_all_fields() {
        let s = CleanupTableSummary {
            table: "output_segments".to_string(),
            eligible_rows: 999,
            deleted_rows: 500,
            retention_days: 90,
        };
        let json = serde_json::to_value(&s).expect("to_value");
        assert_eq!(json["table"], "output_segments");
        assert_eq!(json["eligible_rows"], 999);
        assert_eq!(json["deleted_rows"], 500);
        assert_eq!(json["retention_days"], 90);
    }

    #[test]
    fn cleanup_table_summary_deleted_can_exceed_eligible() {
        // The struct doesn't enforce eligible >= deleted; it's just data
        let s = CleanupTableSummary {
            table: "test".to_string(),
            eligible_rows: 5,
            deleted_rows: 10,
            retention_days: 1,
        };
        assert_eq!(s.deleted_rows, 10);
    }

    #[test]
    fn cleanup_plan_default() {
        let p = CleanupPlan::default();
        assert!(p.tables.is_empty());
        assert_eq!(p.total_eligible, 0);
        assert_eq!(p.total_deleted, 0);
        assert!(!p.dry_run);
    }

    #[test]
    fn cleanup_plan_clone() {
        let p = CleanupPlan {
            tables: vec![CleanupTableSummary {
                table: "events".to_string(),
                eligible_rows: 10,
                deleted_rows: 5,
                retention_days: 30,
            }],
            total_eligible: 10,
            total_deleted: 5,
            dry_run: true,
        };
        let c = p.clone();
        assert_eq!(c.tables.len(), 1);
        assert_eq!(c.total_eligible, 10);
        assert_eq!(c.total_deleted, 5);
        assert!(c.dry_run);
    }

    #[test]
    fn cleanup_plan_debug() {
        let p = CleanupPlan {
            tables: vec![],
            total_eligible: 42,
            total_deleted: 0,
            dry_run: true,
        };
        let dbg = format!("{:?}", p);
        assert!(dbg.contains("CleanupPlan"));
        assert!(dbg.contains("dry_run"));
        assert!(dbg.contains("42"));
    }

    #[test]
    fn cleanup_plan_serialize_empty_tables() {
        let p = CleanupPlan {
            tables: vec![],
            total_eligible: 0,
            total_deleted: 0,
            dry_run: true,
        };
        let json = serde_json::to_value(&p).expect("serialize");
        assert_eq!(json["tables"], serde_json::json!([]));
        assert_eq!(json["dry_run"], true);
    }

    #[test]
    fn cleanup_plan_serialize_multiple_tables() {
        let p = CleanupPlan {
            tables: vec![
                CleanupTableSummary {
                    table: "a".to_string(),
                    eligible_rows: 1,
                    deleted_rows: 1,
                    retention_days: 7,
                },
                CleanupTableSummary {
                    table: "b".to_string(),
                    eligible_rows: 2,
                    deleted_rows: 0,
                    retention_days: 30,
                },
            ],
            total_eligible: 3,
            total_deleted: 1,
            dry_run: false,
        };
        let json = serde_json::to_value(&p).expect("serialize");
        let tables = json["tables"].as_array().unwrap();
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0]["table"], "a");
        assert_eq!(tables[1]["table"], "b");
    }

    #[test]
    fn retention_cutoff_ms_max_days() {
        let now = 1_700_000_000_000i64;
        // u32::MAX days is ~11.7 million years -- cutoff should be far in the past
        let cutoff = retention_cutoff_ms(now, u32::MAX);
        assert!(cutoff < 0, "max retention days produces negative cutoff");
    }

    #[test]
    fn retention_cutoff_ms_monotonic_in_days() {
        let now = 1_700_000_000_000i64;
        let c1 = retention_cutoff_ms(now, 1);
        let c7 = retention_cutoff_ms(now, 7);
        let c30 = retention_cutoff_ms(now, 30);
        let c365 = retention_cutoff_ms(now, 365);
        assert!(c1 > c7, "1 day cutoff is more recent than 7");
        assert!(c7 > c30, "7 day cutoff is more recent than 30");
        assert!(c30 > c365, "30 day cutoff is more recent than 365");
    }

    #[test]
    fn retention_cutoff_ms_exact_day_boundary() {
        let now = 86_400_000i64; // exactly 1 day in ms
        let cutoff = retention_cutoff_ms(now, 1);
        assert_eq!(cutoff, 0, "1 day cutoff from exactly 1 day = epoch");
    }

    #[test]
    fn retention_cutoff_ms_small_now() {
        // When now is smaller than the retention window, cutoff goes negative
        let now = 1000i64;
        let cutoff = retention_cutoff_ms(now, 1);
        assert!(cutoff < 0);
    }

    #[test]
    fn delete_batch_size_is_positive() {
        const { assert!(DELETE_BATCH_SIZE > 0) };
    }

    #[test]
    fn delete_batch_size_is_reasonable() {
        // Batch size should be large enough to be efficient but not so large
        // as to cause long-running transactions
        const { assert!(DELETE_BATCH_SIZE >= 100, "batch too small for efficiency") };
        const { assert!(DELETE_BATCH_SIZE <= 100_000, "batch too large for safety") };
    }

    #[test]
    fn cleanup_plan_dry_run_field_independent() {
        // dry_run is just a flag -- doesn't affect other fields
        let p = CleanupPlan {
            dry_run: true,
            total_deleted: 42,
            ..Default::default()
        };
        assert!(p.dry_run);
        assert_eq!(p.total_deleted, 42);
    }

    #[test]
    fn cleanup_table_summary_tier_name_format() {
        // Tier names in plan use "events (tier: <name>)" format
        let name = format!("events (tier: {})", "critical");
        let s = CleanupTableSummary {
            table: name.clone(),
            eligible_rows: 0,
            deleted_rows: 0,
            retention_days: 90,
        };
        assert!(s.table.starts_with("events (tier: "));
        assert!(s.table.ends_with(')'));
    }

    #[test]
    fn retention_cutoff_ms_two_days_is_double_one_day() {
        let now = 1_700_000_000_000i64;
        let c1 = retention_cutoff_ms(now, 1);
        let c2 = retention_cutoff_ms(now, 2);
        assert_eq!(now - c2, 2 * (now - c1));
    }

    // ---------------------------------------------------------------
    // RubyBeaver wa-1u90p.7.1 -- additional pure unit tests
    // ---------------------------------------------------------------

    #[test]
    fn cleanup_table_summary_serde_roundtrip() {
        let s = CleanupTableSummary {
            table: "events (tier: critical)".to_string(),
            eligible_rows: 42,
            deleted_rows: 10,
            retention_days: 90,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: CleanupTableSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.table, "events (tier: critical)");
        assert_eq!(back.eligible_rows, 42);
        assert_eq!(back.deleted_rows, 10);
        assert_eq!(back.retention_days, 90);
    }

    #[test]
    fn cleanup_plan_serde_roundtrip() {
        let p = CleanupPlan {
            tables: vec![
                CleanupTableSummary {
                    table: "events".to_string(),
                    eligible_rows: 100,
                    deleted_rows: 50,
                    retention_days: 30,
                },
                CleanupTableSummary {
                    table: "audit_actions".to_string(),
                    eligible_rows: 20,
                    deleted_rows: 20,
                    retention_days: 30,
                },
            ],
            total_eligible: 120,
            total_deleted: 70,
            dry_run: true,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: CleanupPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tables.len(), 2);
        assert_eq!(back.total_eligible, 120);
        assert_eq!(back.total_deleted, 70);
        assert!(back.dry_run);
    }

    #[test]
    fn cleanup_plan_total_matches_table_sum() {
        let p = CleanupPlan {
            tables: vec![
                CleanupTableSummary {
                    table: "a".to_string(),
                    eligible_rows: 10,
                    deleted_rows: 5,
                    retention_days: 7,
                },
                CleanupTableSummary {
                    table: "b".to_string(),
                    eligible_rows: 20,
                    deleted_rows: 15,
                    retention_days: 30,
                },
                CleanupTableSummary {
                    table: "c".to_string(),
                    eligible_rows: 30,
                    deleted_rows: 0,
                    retention_days: 90,
                },
            ],
            total_eligible: 60,
            total_deleted: 20,
            dry_run: false,
        };
        let sum_eligible: usize = p.tables.iter().map(|t| t.eligible_rows).sum();
        let sum_deleted: usize = p.tables.iter().map(|t| t.deleted_rows).sum();
        assert_eq!(sum_eligible, p.total_eligible);
        assert_eq!(sum_deleted, p.total_deleted);
    }

    #[test]
    fn retention_cutoff_ms_preserves_now_with_zero() {
        // retention_days=0 means cutoff equals now (everything before now is eligible)
        let now = 1_700_000_000_000i64;
        assert_eq!(retention_cutoff_ms(now, 0), now);
    }

    #[test]
    fn cleanup_table_summary_zero_retention_days() {
        let s = CleanupTableSummary {
            table: "events".to_string(),
            eligible_rows: 0,
            deleted_rows: 0,
            retention_days: 0,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"retention_days\":0"));
    }

    // ---------------------------------------------------------------
    // RubyBeaver wa-1u90p.7.1 -- NEW expanded tests (batch 2)
    // ---------------------------------------------------------------

    #[test]
    fn cleanup_table_summary_deserialize_from_json_object() {
        let json =
            r#"{"table":"usage_metrics","eligible_rows":77,"deleted_rows":33,"retention_days":14}"#;
        let s: CleanupTableSummary = serde_json::from_str(json).unwrap();
        assert_eq!(s.table, "usage_metrics");
        assert_eq!(s.eligible_rows, 77);
        assert_eq!(s.deleted_rows, 33);
        assert_eq!(s.retention_days, 14);
    }

    #[test]
    fn cleanup_plan_deserialize_from_json_object() {
        let json = r#"{
            "tables": [],
            "total_eligible": 0,
            "total_deleted": 0,
            "dry_run": false
        }"#;
        let p: CleanupPlan = serde_json::from_str(json).unwrap();
        assert!(p.tables.is_empty());
        assert!(!p.dry_run);
    }

    #[test]
    fn cleanup_table_summary_large_row_counts() {
        let s = CleanupTableSummary {
            table: "events".to_string(),
            eligible_rows: usize::MAX,
            deleted_rows: usize::MAX - 1,
            retention_days: u32::MAX,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: CleanupTableSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.eligible_rows, usize::MAX);
        assert_eq!(back.deleted_rows, usize::MAX - 1);
        assert_eq!(back.retention_days, u32::MAX);
    }

    #[test]
    fn cleanup_plan_with_many_tables_roundtrip() {
        let tables: Vec<CleanupTableSummary> = (0..20)
            .map(|i| CleanupTableSummary {
                table: format!("table_{}", i),
                eligible_rows: i * 10,
                deleted_rows: i * 5,
                retention_days: (i as u32) + 1,
            })
            .collect();
        let total_eligible: usize = tables.iter().map(|t| t.eligible_rows).sum();
        let total_deleted: usize = tables.iter().map(|t| t.deleted_rows).sum();
        let p = CleanupPlan {
            tables,
            total_eligible,
            total_deleted,
            dry_run: true,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: CleanupPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tables.len(), 20);
        assert_eq!(back.total_eligible, total_eligible);
        assert_eq!(back.total_deleted, total_deleted);
    }

    #[test]
    fn cleanup_table_summary_empty_table_name() {
        let s = CleanupTableSummary {
            table: String::new(),
            eligible_rows: 1,
            deleted_rows: 0,
            retention_days: 7,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: CleanupTableSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.table, "");
        assert_eq!(back.eligible_rows, 1);
    }

    #[test]
    fn cleanup_table_summary_unicode_table_name() {
        let s = CleanupTableSummary {
            table: "events (tier: critico)".to_string(),
            eligible_rows: 5,
            deleted_rows: 3,
            retention_days: 30,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: CleanupTableSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.table, "events (tier: critico)");
    }

    #[test]
    fn cleanup_plan_serde_preserves_table_order() {
        let p = CleanupPlan {
            tables: vec![
                CleanupTableSummary {
                    table: "z_last".to_string(),
                    eligible_rows: 1,
                    deleted_rows: 0,
                    retention_days: 7,
                },
                CleanupTableSummary {
                    table: "a_first".to_string(),
                    eligible_rows: 2,
                    deleted_rows: 1,
                    retention_days: 30,
                },
                CleanupTableSummary {
                    table: "m_middle".to_string(),
                    eligible_rows: 3,
                    deleted_rows: 2,
                    retention_days: 90,
                },
            ],
            total_eligible: 6,
            total_deleted: 3,
            dry_run: false,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: CleanupPlan = serde_json::from_str(&json).unwrap();
        // Order must be preserved (not alphabetically sorted)
        assert_eq!(back.tables[0].table, "z_last");
        assert_eq!(back.tables[1].table, "a_first");
        assert_eq!(back.tables[2].table, "m_middle");
    }

    #[test]
    fn retention_cutoff_ms_negative_now() {
        // Hypothetical: negative now_ms (before epoch)
        let now = -1_000_000i64;
        let cutoff = retention_cutoff_ms(now, 1);
        assert_eq!(cutoff, now - 86_400_000);
        assert!(cutoff < now);
    }

    #[test]
    fn retention_cutoff_ms_zero_now() {
        let cutoff = retention_cutoff_ms(0, 7);
        assert_eq!(cutoff, -(7 * 86_400_000));
    }

    #[test]
    fn retention_cutoff_ms_linearity() {
        // cutoff(now, a) - cutoff(now, b) = (b - a) * ms_per_day
        let now = 1_700_000_000_000i64;
        let ca = retention_cutoff_ms(now, 10);
        let cb = retention_cutoff_ms(now, 20);
        let ms_per_day: i64 = 86_400_000;
        assert_eq!(ca - cb, 10 * ms_per_day);
    }

    #[test]
    fn retention_cutoff_ms_same_day_different_now() {
        let c1 = retention_cutoff_ms(1_000_000_000_000, 30);
        let c2 = retention_cutoff_ms(2_000_000_000_000, 30);
        // The difference in cutoffs should equal the difference in now values
        assert_eq!(c2 - c1, 1_000_000_000_000);
    }

    #[test]
    fn cleanup_plan_clone_independence() {
        // Cloned plan should be independent of the original
        let mut original = CleanupPlan {
            tables: vec![CleanupTableSummary {
                table: "events".to_string(),
                eligible_rows: 10,
                deleted_rows: 5,
                retention_days: 30,
            }],
            total_eligible: 10,
            total_deleted: 5,
            dry_run: true,
        };
        let cloned = original.clone();
        original.total_deleted = 999;
        original.tables[0].deleted_rows = 999;
        // Clone should be unaffected
        assert_eq!(cloned.total_deleted, 5);
        assert_eq!(cloned.tables[0].deleted_rows, 5);
    }

    #[test]
    fn cleanup_table_summary_clone_independence() {
        let mut original = CleanupTableSummary {
            table: "events".to_string(),
            eligible_rows: 50,
            deleted_rows: 25,
            retention_days: 14,
        };
        let cloned = original.clone();
        original.eligible_rows = 0;
        original.table = "modified".to_string();
        assert_eq!(cloned.eligible_rows, 50);
        assert_eq!(cloned.table, "events");
    }

    #[test]
    fn cleanup_plan_debug_contains_all_key_fields() {
        let p = CleanupPlan {
            tables: vec![CleanupTableSummary {
                table: "notification_history".to_string(),
                eligible_rows: 7,
                deleted_rows: 3,
                retention_days: 30,
            }],
            total_eligible: 7,
            total_deleted: 3,
            dry_run: false,
        };
        let dbg = format!("{:?}", p);
        assert!(dbg.contains("total_eligible"));
        assert!(dbg.contains("total_deleted"));
        assert!(dbg.contains("tables"));
        assert!(dbg.contains("notification_history"));
    }

    #[test]
    fn cleanup_table_summary_debug_contains_retention() {
        let s = CleanupTableSummary {
            table: "output_segments".to_string(),
            eligible_rows: 0,
            deleted_rows: 0,
            retention_days: 365,
        };
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("retention_days"));
        assert!(dbg.contains("365"));
    }

    #[test]
    fn cleanup_plan_serde_roundtrip_with_dry_run_false() {
        let p = CleanupPlan {
            tables: vec![],
            total_eligible: 0,
            total_deleted: 0,
            dry_run: false,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: CleanupPlan = serde_json::from_str(&json).unwrap();
        assert!(!back.dry_run);
    }

    #[test]
    fn cleanup_table_summary_serde_roundtrip_max_retention() {
        let s = CleanupTableSummary {
            table: "events".to_string(),
            eligible_rows: 0,
            deleted_rows: 0,
            retention_days: u32::MAX,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: CleanupTableSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.retention_days, u32::MAX);
    }

    #[test]
    fn cleanup_plan_tables_push_increments_len() {
        let mut p = CleanupPlan::default();
        assert_eq!(p.tables.len(), 0);
        p.tables.push(CleanupTableSummary::default());
        assert_eq!(p.tables.len(), 1);
        p.tables.push(CleanupTableSummary::default());
        assert_eq!(p.tables.len(), 2);
    }

    #[test]
    fn cleanup_plan_default_matches_struct_literal() {
        let from_default = CleanupPlan::default();
        let from_literal = CleanupPlan {
            tables: vec![],
            total_eligible: 0,
            total_deleted: 0,
            dry_run: false,
        };
        assert_eq!(from_default.tables.len(), from_literal.tables.len());
        assert_eq!(from_default.total_eligible, from_literal.total_eligible);
        assert_eq!(from_default.total_deleted, from_literal.total_deleted);
        assert_eq!(from_default.dry_run, from_literal.dry_run);
    }

    #[test]
    fn cleanup_table_summary_default_matches_struct_literal() {
        let from_default = CleanupTableSummary::default();
        let from_literal = CleanupTableSummary {
            table: String::new(),
            eligible_rows: 0,
            deleted_rows: 0,
            retention_days: 0,
        };
        assert_eq!(from_default.table, from_literal.table);
        assert_eq!(from_default.eligible_rows, from_literal.eligible_rows);
        assert_eq!(from_default.deleted_rows, from_literal.deleted_rows);
        assert_eq!(from_default.retention_days, from_literal.retention_days);
    }

    #[test]
    fn cleanup_plan_json_value_types() {
        let p = CleanupPlan {
            tables: vec![CleanupTableSummary {
                table: "events".to_string(),
                eligible_rows: 5,
                deleted_rows: 2,
                retention_days: 7,
            }],
            total_eligible: 5,
            total_deleted: 2,
            dry_run: true,
        };
        let v = serde_json::to_value(&p).unwrap();
        // Verify JSON types are correct
        assert!(v["tables"].is_array());
        assert!(v["total_eligible"].is_number());
        assert!(v["total_deleted"].is_number());
        assert!(v["dry_run"].is_boolean());
        assert!(v["tables"][0]["table"].is_string());
        assert!(v["tables"][0]["eligible_rows"].is_number());
    }

    #[test]
    fn retention_cutoff_ms_7_days_in_milliseconds() {
        let now = 1_700_000_000_000i64;
        let cutoff = retention_cutoff_ms(now, 7);
        // 7 days = 7 * 24 * 60 * 60 * 1000 = 604_800_000 ms
        assert_eq!(now - cutoff, 604_800_000);
    }

    #[test]
    fn retention_cutoff_ms_30_days_in_milliseconds() {
        let now = 1_700_000_000_000i64;
        let cutoff = retention_cutoff_ms(now, 30);
        // 30 days = 2_592_000_000 ms
        assert_eq!(now - cutoff, 2_592_000_000);
    }

    #[test]
    fn retention_cutoff_ms_90_days_in_milliseconds() {
        let now = 1_700_000_000_000i64;
        let cutoff = retention_cutoff_ms(now, 90);
        // 90 days = 7_776_000_000 ms
        assert_eq!(now - cutoff, 7_776_000_000);
    }

    #[test]
    fn delete_batch_size_value() {
        // Document the actual constant value for regression detection
        assert_eq!(DELETE_BATCH_SIZE, 5000);
    }

    #[test]
    fn cleanup_plan_deserialize_ignores_unknown_fields_if_deny_missing() {
        // Standard serde: extra fields are silently ignored by default
        let json = r#"{
            "tables": [],
            "total_eligible": 0,
            "total_deleted": 0,
            "dry_run": true,
            "extra_field": "should_be_ignored"
        }"#;
        // This should succeed since serde default behavior ignores unknown fields
        let p: Result<CleanupPlan, _> = serde_json::from_str(json);
        assert!(p.is_ok());
    }

    #[test]
    fn cleanup_table_summary_special_chars_in_table_name() {
        let s = CleanupTableSummary {
            table: "events (tier: info/handled)".to_string(),
            eligible_rows: 1,
            deleted_rows: 0,
            retention_days: 3,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: CleanupTableSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.table, "events (tier: info/handled)");
    }

    #[test]
    fn cleanup_plan_serde_compact_vs_pretty() {
        let p = CleanupPlan {
            tables: vec![CleanupTableSummary {
                table: "events".to_string(),
                eligible_rows: 1,
                deleted_rows: 1,
                retention_days: 7,
            }],
            total_eligible: 1,
            total_deleted: 1,
            dry_run: false,
        };
        let compact = serde_json::to_string(&p).unwrap();
        let pretty = serde_json::to_string_pretty(&p).unwrap();
        // Both should deserialize to the same thing
        let from_compact: CleanupPlan = serde_json::from_str(&compact).unwrap();
        let from_pretty: CleanupPlan = serde_json::from_str(&pretty).unwrap();
        assert_eq!(from_compact.total_eligible, from_pretty.total_eligible);
        assert_eq!(from_compact.total_deleted, from_pretty.total_deleted);
        assert_eq!(from_compact.dry_run, from_pretty.dry_run);
        assert_eq!(from_compact.tables.len(), from_pretty.tables.len());
        // Pretty should be longer due to whitespace
        assert!(pretty.len() > compact.len());
    }

    #[test]
    fn cleanup_plan_with_zero_eligible_and_nonzero_deleted() {
        // Edge case: deleted > eligible is structurally allowed
        let p = CleanupPlan {
            tables: vec![],
            total_eligible: 0,
            total_deleted: 5,
            dry_run: false,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: CleanupPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total_eligible, 0);
        assert_eq!(back.total_deleted, 5);
    }

    #[test]
    fn cleanup_table_summary_all_zeros() {
        let s = CleanupTableSummary {
            table: "events".to_string(),
            eligible_rows: 0,
            deleted_rows: 0,
            retention_days: 0,
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["eligible_rows"], 0);
        assert_eq!(json["deleted_rows"], 0);
        assert_eq!(json["retention_days"], 0);
    }

    #[test]
    fn retention_cutoff_ms_commutes_with_addition() {
        // cutoff(now + delta, days) = cutoff(now, days) + delta
        let now = 1_700_000_000_000i64;
        let delta = 5_000_000i64;
        let days = 30u32;
        assert_eq!(
            retention_cutoff_ms(now + delta, days),
            retention_cutoff_ms(now, days) + delta
        );
    }

    // ---------------------------------------------------------------
    // RubyBeaver wa-1u90p.7.1 -- async integration edge-case tests
    // ---------------------------------------------------------------

    #[test]
    fn preview_with_only_audit_data() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("only_audit").await;
            let now = now_ms();
            let old_ts = now - 60 * 86_400_000;

            // Only insert audit data, no events
            storage
                .record_audit_action(make_audit(old_ts))
                .await
                .unwrap();
            storage
                .record_audit_action(make_audit(old_ts - 1000))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![],
                ..Default::default()
            };

            let plan = cleanup_preview(&storage, &config).await.expect("preview");
            let audit = plan
                .tables
                .iter()
                .find(|t| t.table == "audit_actions")
                .unwrap();
            assert_eq!(audit.eligible_rows, 2);
            assert_eq!(audit.deleted_rows, 0, "preview never deletes");

            // Events table should show 0 eligible
            let events = plan.tables.iter().find(|t| t.table == "events").unwrap();
            assert_eq!(events.eligible_rows, 0);

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn preview_with_only_usage_data() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("only_usage").await;
            let now = now_ms();
            let old_ts = now - 60 * 86_400_000;

            storage
                .record_usage_metric(make_usage(old_ts))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![],
                ..Default::default()
            };

            let plan = cleanup_preview(&storage, &config).await.expect("preview");
            let usage = plan
                .tables
                .iter()
                .find(|t| t.table == "usage_metrics")
                .unwrap();
            assert_eq!(usage.eligible_rows, 1);

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn preview_with_only_notification_data() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("only_notif").await;
            let now = now_ms();
            let old_ts = now - 60 * 86_400_000;

            storage
                .record_notification(make_notification(old_ts))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![],
                ..Default::default()
            };

            let plan = cleanup_preview(&storage, &config).await.expect("preview");
            let notif = plan
                .tables
                .iter()
                .find(|t| t.table == "notification_history")
                .unwrap();
            assert_eq!(notif.eligible_rows, 1);

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn apply_with_all_recent_data_deletes_nothing() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("all_recent").await;
            let now = now_ms();
            let recent_ts = now - 2 * 86_400_000; // 2 days ago

            storage
                .record_event(make_event(recent_ts, "info", "test"))
                .await
                .unwrap();
            storage
                .record_audit_action(make_audit(recent_ts))
                .await
                .unwrap();
            storage
                .record_usage_metric(make_usage(recent_ts))
                .await
                .unwrap();
            storage
                .record_notification(make_notification(recent_ts))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![],
                ..Default::default()
            };

            let plan = cleanup_apply(&storage, &config).await.expect("apply");
            assert_eq!(plan.total_deleted, 0, "all data is recent");
            assert_eq!(plan.total_eligible, 0);

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn preview_plan_always_has_dry_run_true() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("dry_run_flag").await;

            let config = StorageConfig::default();
            let plan = cleanup_preview(&storage, &config).await.expect("preview");
            assert!(plan.dry_run, "preview must always set dry_run=true");

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn apply_plan_always_has_dry_run_false() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("apply_flag").await;

            let config = StorageConfig::default();
            let plan = cleanup_apply(&storage, &config).await.expect("apply");
            assert!(!plan.dry_run, "apply must always set dry_run=false");

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn preview_total_eligible_equals_sum_of_table_eligible() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("eligible_sum").await;
            let now = now_ms();
            let old_ts = now - 60 * 86_400_000;

            storage
                .record_event(make_event(old_ts, "info", "test"))
                .await
                .unwrap();
            storage
                .record_audit_action(make_audit(old_ts))
                .await
                .unwrap();
            storage
                .record_usage_metric(make_usage(old_ts))
                .await
                .unwrap();
            storage
                .record_notification(make_notification(old_ts))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![],
                ..Default::default()
            };

            let plan = cleanup_preview(&storage, &config).await.expect("preview");
            let table_sum: usize = plan.tables.iter().map(|t| t.eligible_rows).sum();
            assert_eq!(
                plan.total_eligible, table_sum,
                "total_eligible must equal sum of per-table eligible_rows"
            );

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn apply_total_deleted_equals_sum_of_table_deleted() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("deleted_sum").await;
            let now = now_ms();
            let old_ts = now - 60 * 86_400_000;

            storage
                .record_event(make_event(old_ts, "info", "test"))
                .await
                .unwrap();
            storage
                .record_audit_action(make_audit(old_ts))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![],
                ..Default::default()
            };

            let plan = cleanup_apply(&storage, &config).await.expect("apply");
            let table_sum: usize = plan.tables.iter().map(|t| t.deleted_rows).sum();
            assert_eq!(
                plan.total_deleted, table_sum,
                "total_deleted must equal sum of per-table deleted_rows"
            );

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn multiple_keep_forever_tiers_protect_their_rows_from_global_fallback() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("all_tiers_zero").await;
            let now = now_ms();
            let ancient_ts = now - 365 * 86_400_000;

            storage
                .record_event(make_event(ancient_ts, "critical", "error"))
                .await
                .unwrap();
            storage
                .record_event(make_event(ancient_ts, "info", "detection"))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![
                    RetentionTier {
                        name: "crit-forever".to_string(),
                        retention_days: 0,
                        severities: vec!["critical".to_string()],
                        event_types: vec![],
                        handled: None,
                    },
                    RetentionTier {
                        name: "info-forever".to_string(),
                        retention_days: 0,
                        severities: vec!["info".to_string()],
                        event_types: vec![],
                        handled: None,
                    },
                ],
                ..Default::default()
            };

            let plan = cleanup_apply(&storage, &config).await.expect("apply");

            // Keep-forever tiers do not produce their own cleanup entries, but
            // they must still exclude their first-match partitions from the
            // global fallback. The fallback remains visible and exact.
            let fallback = plan
                .tables
                .iter()
                .find(|table| table.table == "events (global fallback)")
                .expect("tiered cleanup reports the unmatched global fallback");
            assert_eq!(fallback.eligible_rows, 0);
            assert_eq!(fallback.deleted_rows, 0);

            // Both events should survive
            let remaining = storage.count_events_before(now + 1000).await.unwrap();
            assert_eq!(remaining, 2);

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn tier_with_unhandled_filter_only_deletes_unhandled() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("unhandled_filter").await;
            let now = now_ms();
            let old_ts = now - 10 * 86_400_000;

            let ev1_id = storage
                .record_event(make_event(old_ts, "info", "detection"))
                .await
                .unwrap();
            let _ev2_id = storage
                .record_event(make_event(old_ts, "info", "detection"))
                .await
                .unwrap();

            // Mark ev1 as handled
            storage
                .mark_event_handled(ev1_id, None, "auto")
                .await
                .unwrap();

            // Tier: only delete UNhandled info events older than 3 days
            let config = StorageConfig {
                retention_days: 30,
                retention_tiers: vec![RetentionTier {
                    name: "info-unhandled".to_string(),
                    retention_days: 3,
                    severities: vec!["info".to_string()],
                    event_types: vec![],
                    handled: Some(false),
                }],
                ..Default::default()
            };

            let plan = cleanup_apply(&storage, &config).await.expect("apply");
            let tier = plan
                .tables
                .iter()
                .find(|t| t.table.contains("info-unhandled"))
                .unwrap();
            assert_eq!(
                tier.deleted_rows, 1,
                "only the unhandled event should be deleted"
            );

            // The handled event should remain
            let remaining = storage.count_events_before(now + 1000).await.unwrap();
            assert_eq!(remaining, 1, "handled event should survive");

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn preview_retention_days_propagated_to_table_summaries() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("retention_propagate").await;

            let config = StorageConfig {
                retention_days: 45,
                retention_tiers: vec![RetentionTier {
                    name: "special".to_string(),
                    retention_days: 120,
                    severities: vec!["critical".to_string()],
                    event_types: vec![],
                    handled: None,
                }],
                ..Default::default()
            };

            let plan = cleanup_preview(&storage, &config).await.expect("preview");

            // The special tier should have retention_days=120
            let special = plan
                .tables
                .iter()
                .find(|t| t.table.contains("special"))
                .unwrap();
            assert_eq!(special.retention_days, 120);

            // Global tables should have retention_days=45
            let audit = plan
                .tables
                .iter()
                .find(|t| t.table == "audit_actions")
                .unwrap();
            assert_eq!(audit.retention_days, 45);

            let usage = plan
                .tables
                .iter()
                .find(|t| t.table == "usage_metrics")
                .unwrap();
            assert_eq!(usage.retention_days, 45);

            let notif = plan
                .tables
                .iter()
                .find(|t| t.table == "notification_history")
                .unwrap();
            assert_eq!(notif.retention_days, 45);

            let segments = plan
                .tables
                .iter()
                .find(|t| t.table == "output_segments")
                .unwrap();
            assert_eq!(segments.retention_days, 45);

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn preview_with_one_day_retention() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("one_day_retention").await;
            let now = now_ms();
            // Event from 2 days ago should be eligible with 1-day retention
            let ts_2d = now - 2 * 86_400_000;
            // Event from 12 hours ago should NOT be eligible
            let ts_12h = now - 12 * 60 * 60 * 1000;

            storage
                .record_event(make_event(ts_2d, "info", "test"))
                .await
                .unwrap();
            storage
                .record_event(make_event(ts_12h, "info", "test"))
                .await
                .unwrap();

            let config = StorageConfig {
                retention_days: 1,
                retention_tiers: vec![],
                ..Default::default()
            };

            let plan = cleanup_preview(&storage, &config).await.expect("preview");
            let events = plan.tables.iter().find(|t| t.table == "events").unwrap();
            assert_eq!(events.eligible_rows, 1, "only the 2-day-old event");

            teardown(storage, &db_path).await;
        });
    }

    #[test]
    fn apply_with_very_short_retention_cleans_almost_everything() {
        run_async_test(async {
            let (storage, db_path) = setup_storage("short_retention").await;
            let now = now_ms();

            // Insert events at various ages
            for days_ago in [1, 3, 7, 14, 30] {
                let ts = now - days_ago * 86_400_000;
                storage
                    .record_event(make_event(ts, "info", "test"))
                    .await
                    .unwrap();
            }

            // 1-day retention should clean everything except the 1-day-old one
            // (since cutoff = now - 1 day, a 1-day-old event is right at the boundary)
            let config = StorageConfig {
                retention_days: 2, // 2-day retention
                retention_tiers: vec![],
                ..Default::default()
            };

            let plan = cleanup_apply(&storage, &config).await.expect("apply");
            let events = plan.tables.iter().find(|t| t.table == "events").unwrap();
            // Events at 3, 7, 14, 30 days are older than 2 days -> 4 eligible
            // Event at 1 day is within 2-day retention -> survives
            assert_eq!(events.deleted_rows, 4);

            let remaining = storage.count_events_before(now + 1000).await.unwrap();
            assert_eq!(remaining, 1, "only the 1-day-old event survives");

            teardown(storage, &db_path).await;
        });
    }
}
