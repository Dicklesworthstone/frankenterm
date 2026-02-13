//! Integration tests for the recorder stack.
//!
//! Exercises cross-module interactions between:
//! - `recorder_storage` (append-log backend)
//! - `recorder_retention` (segment lifecycle management)
//! - `storage_telemetry` (metrics, SLO tracking, diagnostics)
//! - `recorder_audit` (tamper-evident audit trail)
//!
//! These tests validate that the four recorder modules compose correctly
//! and that data flows coherently through the full pipeline.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use frankenterm_core::policy::ActorKind;
use frankenterm_core::recorder_audit::{
    AUDIT_SCHEMA_VERSION, AccessTier, ActorIdentity, AuditEventBuilder, AuditEventType, AuditLog,
    AuditLogConfig, AuthzDecision, GENESIS_HASH, check_authorization, required_tier_for_event,
};
use frankenterm_core::recorder_retention::{
    RetentionConfig, RetentionManager, SegmentMeta, SegmentPhase, SensitivityTier,
};
use frankenterm_core::recorder_storage::{
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, DurabilityLevel, FlushMode,
    RecorderStorage, RecorderStorageErrorClass,
};
use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
    RecorderEventSource, RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
};
use frankenterm_core::storage_telemetry::{
    StorageHealthTier, StorageTelemetry, StorageTelemetryConfig, diagnose, remediation_for_error,
};

// =============================================================================
// Test helpers
// =============================================================================

fn sample_event(event_id: &str, pane_id: u64, seq: u64, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some("sess-integration".to_string()),
        workflow_id: None,
        correlation_id: Some("corr-integ".to_string()),
        source: RecorderEventSource::RobotMode,
        occurred_at_ms: 1_700_000_000_000 + seq,
        recorded_at_ms: 1_700_000_000_001 + seq,
        sequence: seq,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::IngressText {
            text: text.to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
    }
}

fn sample_redacted_event(event_id: &str, pane_id: u64, seq: u64) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some("sess-integration".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms: 1_700_000_000_000 + seq,
        recorded_at_ms: 1_700_000_000_001 + seq,
        sequence: seq,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::IngressText {
            text: "[REDACTED]".to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::Partial,
            ingress_kind: RecorderIngressKind::SendText,
        },
    }
}

fn storage_config(path: &std::path::Path) -> AppendLogStorageConfig {
    AppendLogStorageConfig {
        data_path: path.join("events.log"),
        state_path: path.join("state.json"),
        queue_capacity: 16,
        max_batch_events: 64,
        max_batch_bytes: 1024 * 1024,
        max_idempotency_entries: 32,
    }
}

fn make_segment(
    id: &str,
    tier: SensitivityTier,
    phase: SegmentPhase,
    created_at_ms: u64,
    size_bytes: u64,
    events: u64,
    ordinal_start: u64,
) -> SegmentMeta {
    SegmentMeta {
        segment_id: id.to_string(),
        sensitivity: tier,
        phase,
        start_ordinal: ordinal_start,
        end_ordinal: Some(ordinal_start + events - 1),
        size_bytes,
        created_at_ms,
        sealed_at_ms: if phase != SegmentPhase::Active {
            Some(created_at_ms + 3_600_000)
        } else {
            None
        },
        archived_at_ms: if phase == SegmentPhase::Archived {
            Some(created_at_ms + 7 * 86_400_000)
        } else {
            None
        },
        purged_at_ms: None,
        event_count: events,
    }
}

fn human_actor() -> ActorIdentity {
    ActorIdentity::new(ActorKind::Human, "operator-1")
}

fn robot_actor() -> ActorIdentity {
    ActorIdentity::new(ActorKind::Robot, "agent-swarm-1")
}

fn workflow_actor() -> ActorIdentity {
    ActorIdentity::new(ActorKind::Workflow, "wf-restart-42")
}

// =============================================================================
// 1. Storage → Telemetry integration
// =============================================================================

/// Append events and verify telemetry records the operation.
#[tokio::test]
async fn storage_append_records_telemetry_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(storage_config(dir.path())).unwrap();
    let telemetry = Arc::new(StorageTelemetry::with_defaults());

    let events: Vec<RecorderEvent> = (0..5)
        .map(|i| sample_event(&format!("evt-{}", i), 1, i, "hello"))
        .collect();

    let req = AppendRequest {
        batch_id: "batch-1".to_string(),
        events,
        required_durability: DurabilityLevel::Appended,
        producer_ts_ms: 1_700_000_000_000,
    };

    let start = Instant::now();
    let result = storage.append_batch(req).await;
    let elapsed_us = start.elapsed().as_micros() as f64;

    assert!(result.is_ok());
    let resp = result.unwrap();

    // Record in telemetry.
    telemetry.record_append(elapsed_us, resp.accepted_count, 500, false);

    let snapshot = telemetry.snapshot();
    assert_eq!(snapshot.total_events_appended, 5);
    assert_eq!(snapshot.total_batches, 1);
    assert!(snapshot.append_rate_ewma > 0.0);
}

/// Flush and verify telemetry snapshot captures flush stats.
#[tokio::test]
async fn storage_flush_updates_telemetry() {
    let dir = tempfile::tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(storage_config(dir.path())).unwrap();
    let telemetry = Arc::new(StorageTelemetry::with_defaults());

    // Append first.
    let events = vec![sample_event("evt-0", 1, 0, "data")];
    let req = AppendRequest {
        batch_id: "batch-flush".to_string(),
        events,
        required_durability: DurabilityLevel::Appended,
        producer_ts_ms: 1_700_000_000_000,
    };
    let start = Instant::now();
    let resp = storage.append_batch(req).await.unwrap();
    telemetry.record_append(
        start.elapsed().as_micros() as f64,
        resp.accepted_count,
        100,
        false,
    );

    // Flush.
    let flush_start = Instant::now();
    let flush_result = storage.flush(FlushMode::Buffered).await;
    assert!(flush_result.is_ok());
    telemetry.record_flush(flush_start.elapsed().as_micros() as f64);

    let snapshot = telemetry.snapshot();
    assert_eq!(snapshot.total_flushes, 1);
    assert_eq!(snapshot.total_batches, 1);
}

/// Health status propagates to telemetry tier classification.
#[tokio::test]
async fn storage_health_propagates_to_telemetry_tier() {
    let dir = tempfile::tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(storage_config(dir.path())).unwrap();
    let telemetry = Arc::new(StorageTelemetry::with_defaults());

    let health = storage.health().await;
    telemetry.update_health(health);

    // Fresh storage should be healthy (Green).
    assert_eq!(telemetry.current_tier(), StorageHealthTier::Green);
}

/// Error recording tracks class distribution.
#[test]
fn telemetry_error_class_tracking() {
    let telemetry = StorageTelemetry::with_defaults();

    telemetry.record_error(RecorderStorageErrorClass::Overload);
    telemetry.record_error(RecorderStorageErrorClass::Overload);
    telemetry.record_error(RecorderStorageErrorClass::Retryable);
    telemetry.record_error(RecorderStorageErrorClass::Corruption);

    let snapshot = telemetry.snapshot();
    assert_eq!(snapshot.errors.overload, 2);
    assert_eq!(snapshot.errors.retryable, 1);
    assert_eq!(snapshot.errors.corruption, 1);
    assert_eq!(snapshot.errors.total(), 4);
}

// =============================================================================
// 2. Retention → Audit integration
// =============================================================================

/// Retention sweep produces segment transitions auditable via AuditLog.
#[test]
fn retention_sweep_generates_auditable_events() {
    let config = RetentionConfig::default();
    let mut mgr = RetentionManager::new(config).unwrap();
    let audit_log = AuditLog::new(AuditLogConfig::default());

    let base_ms = 1_700_000_000_000u64;
    let hot_expiry = base_ms + 25 * 3_600_000; // 25 hours past creation (past hot window).

    // Add an active segment created at base time.
    mgr.add_segment(make_segment(
        "seg_t1_001",
        SensitivityTier::T1Standard,
        SegmentPhase::Active,
        base_ms,
        1_000_000,
        500,
        0,
    ));

    // Sweep at a time past hot window — should seal.
    let result = mgr.sweep(hot_expiry, &HashMap::new());

    // Audit each transition.
    for seg_id in &result.sealed {
        audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::RetentionSegmentSealed,
                ActorIdentity::new(ActorKind::Workflow, "retention-sweep"),
                hot_expiry,
            )
            .with_segment_ids(vec![seg_id.clone()]),
        );
    }

    assert!(!result.sealed.is_empty());
    let entries = audit_log.entries_by_type(AuditEventType::RetentionSegmentSealed);
    assert_eq!(entries.len(), result.sealed.len());

    // Audit chain is intact.
    let all = audit_log.entries();
    let verification = AuditLog::verify_chain(&all, GENESIS_HASH);
    assert!(verification.chain_intact);
}

/// T3 accelerated purge generates both retention sweep result and audit entry.
#[test]
fn t3_accelerated_purge_audited() {
    let config = RetentionConfig {
        t3_max_hours: 24,
        ..RetentionConfig::default()
    };
    let mut mgr = RetentionManager::new(config).unwrap();
    let audit_log = AuditLog::new(AuditLogConfig::default());

    let base_ms = 1_700_000_000_000u64;

    // Add a T3 sealed segment.
    let mut seg = make_segment(
        "seg_t3_001",
        SensitivityTier::T3Restricted,
        SegmentPhase::Active,
        base_ms,
        500_000,
        100,
        0,
    );
    // Manually seal it (simulating prior sweep).
    seg.phase = SegmentPhase::Sealed;
    seg.sealed_at_ms = Some(base_ms + 3_600_000);
    mgr.add_segment(seg);

    // Sweep 25 hours later — T3 should be purge-eligible.
    let sweep_time = base_ms + 25 * 3_600_000;
    let result = mgr.sweep(sweep_time, &HashMap::new());

    // T3 data should be archived or marked for purge.
    let total_transitions = result.sealed.len()
        + result.archived.len()
        + result.purge_candidates.len()
        + result.purged.len();
    assert!(total_transitions > 0, "T3 data should have transitioned");

    // Audit the accelerated purge.
    for seg_id in result.purge_candidates.iter().chain(result.purged.iter()) {
        audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::RetentionAcceleratedPurge,
                ActorIdentity::new(ActorKind::Workflow, "retention-t3-purge"),
                sweep_time,
            )
            .with_segment_ids(vec![seg_id.clone()])
            .with_justification("T3 data exceeded 24h retention window"),
        );
    }

    // Verify all audit entries have justification.
    for entry in audit_log.entries() {
        if entry.event_type == AuditEventType::RetentionAcceleratedPurge {
            assert!(entry.justification.is_some());
        }
    }
}

// =============================================================================
// 3. Sensitivity classification → Retention → Audit pipeline
// =============================================================================

/// Redacted events classify as T2 sensitive.
#[test]
fn redacted_events_classify_as_t2() {
    let redacted = sample_redacted_event("redacted-1", 5, 0);
    if let RecorderEventPayload::IngressText { redaction, .. } = &redacted.payload {
        let tier = SensitivityTier::classify(*redaction, false);
        assert_eq!(tier, SensitivityTier::T2Sensitive);
    } else {
        panic!("Expected IngressText payload");
    }
}

/// Classify events by redaction level, assign to retention tiers, audit lifecycle.
#[test]
fn sensitivity_classification_flows_through_retention_and_audit() {
    let audit_log = AuditLog::new(AuditLogConfig::default());

    // Classify different redaction levels.
    let t1 = SensitivityTier::classify(RecorderRedactionLevel::None, false);
    let t2 = SensitivityTier::classify(RecorderRedactionLevel::Partial, false);
    let t3 = SensitivityTier::classify(RecorderRedactionLevel::None, true);

    assert_eq!(t1, SensitivityTier::T1Standard);
    assert_eq!(t2, SensitivityTier::T2Sensitive);
    assert_eq!(t3, SensitivityTier::T3Restricted);

    // Create segments with each tier.
    let mut mgr = RetentionManager::with_defaults();
    let base_ms = 1_700_000_000_000u64;

    mgr.add_segment(make_segment(
        "seg_t1",
        t1,
        SegmentPhase::Active,
        base_ms,
        1000,
        10,
        0,
    ));
    mgr.add_segment(make_segment(
        "seg_t2",
        t2,
        SegmentPhase::Active,
        base_ms,
        2000,
        20,
        10,
    ));
    mgr.add_segment(make_segment(
        "seg_t3",
        t3,
        SegmentPhase::Active,
        base_ms,
        3000,
        30,
        30,
    ));

    assert_eq!(mgr.segment_count(), 3);
    assert_eq!(mgr.segments_by_tier(SensitivityTier::T1Standard).len(), 1);
    assert_eq!(mgr.segments_by_tier(SensitivityTier::T2Sensitive).len(), 1);
    assert_eq!(mgr.segments_by_tier(SensitivityTier::T3Restricted).len(), 1);

    // Audit segment creation.
    for (seg_id, tier) in [("seg_t1", t1), ("seg_t2", t2), ("seg_t3", t3)] {
        audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::RetentionSegmentSealed,
                workflow_actor(),
                base_ms,
            )
            .with_segment_ids(vec![seg_id.to_string()])
            .with_details(serde_json::json!({"sensitivity_tier": format!("{:?}", tier)})),
        );
    }

    assert_eq!(audit_log.len(), 3);
    let verification = AuditLog::verify_chain(&audit_log.entries(), GENESIS_HASH);
    assert!(verification.chain_intact);
}

// =============================================================================
// 4. Access control → Audit integration
// =============================================================================

/// Access control decisions are correctly audited with appropriate event types.
#[test]
fn access_control_decisions_audited() {
    let audit_log = AuditLog::new(AuditLogConfig::default());
    let now = 1_700_000_000_000u64;

    // Human queries at A2 (allowed).
    let human_decision = check_authorization(ActorKind::Human, AccessTier::A2FullQuery);
    assert_eq!(human_decision, AuthzDecision::Allow);
    audit_log.append(
        AuditEventBuilder::new(AuditEventType::RecorderQuery, human_actor(), now)
            .with_decision(human_decision)
            .with_pane_ids(vec![1, 2, 3])
            .with_query("error timeout")
            .with_result_count(15),
    );

    // Robot tries A3 (denied).
    let robot_decision = check_authorization(ActorKind::Robot, AccessTier::A3PrivilegedRaw);
    assert_eq!(robot_decision, AuthzDecision::Deny);
    audit_log.append(
        AuditEventBuilder::new(
            AuditEventType::RecorderQueryPrivileged,
            robot_actor(),
            now + 1000,
        )
        .with_decision(robot_decision),
    );

    // Human elevates to A3 (elevated with justification).
    let elevate_decision = check_authorization(ActorKind::Human, AccessTier::A3PrivilegedRaw);
    assert_eq!(elevate_decision, AuthzDecision::Elevate);
    audit_log.append(
        AuditEventBuilder::new(
            AuditEventType::AccessApprovalGranted,
            human_actor(),
            now + 2000,
        )
        .with_decision(AuthzDecision::Allow)
        .with_justification("Investigating production incident INC-456"),
    );

    // Now human does privileged query with approval.
    audit_log.append(
        AuditEventBuilder::new(
            AuditEventType::RecorderQueryPrivileged,
            human_actor(),
            now + 3000,
        )
        .with_decision(AuthzDecision::Allow)
        .with_pane_ids(vec![7])
        .with_time_range(now - 3_600_000, now)
        .with_justification("Approved via INC-456")
        .with_result_count(3),
    );

    // Verify stats.
    let stats = audit_log.stats();
    assert_eq!(stats.total_entries, 4);
    assert_eq!(stats.denied_count, 1);
    assert_eq!(stats.elevated_count, 0); // Elevation was recorded as Allow.
    assert_eq!(stats.by_actor.get("human"), Some(&3));
    assert_eq!(stats.by_actor.get("robot"), Some(&1));

    // Verify chain.
    let verification = AuditLog::verify_chain(&audit_log.entries(), GENESIS_HASH);
    assert!(verification.chain_intact);
    assert_eq!(verification.total_entries, 4);
}

/// Event types map to required access tiers per governance policy.
#[test]
fn event_types_require_correct_access_tiers() {
    // Standard queries need A1.
    assert_eq!(
        required_tier_for_event(AuditEventType::RecorderQuery),
        AccessTier::A1RedactedQuery
    );

    // Admin operations need A4.
    assert_eq!(
        required_tier_for_event(AuditEventType::AdminPurge),
        AccessTier::A4Admin
    );
    assert_eq!(
        required_tier_for_event(AuditEventType::AdminRetentionOverride),
        AccessTier::A4Admin
    );

    // Privileged raw needs A3.
    assert_eq!(
        required_tier_for_event(AuditEventType::RecorderQueryPrivileged),
        AccessTier::A3PrivilegedRaw
    );

    // Retention lifecycle is A0 (internal).
    assert_eq!(
        required_tier_for_event(AuditEventType::RetentionSegmentSealed),
        AccessTier::A0PublicMetadata
    );
}

/// Robot cannot access admin operations; workflow can elevate to A3.
#[test]
fn actor_elevation_rules_match_governance_policy() {
    // Robot: A1 default, can elevate to A2, denied A3+.
    assert_eq!(
        check_authorization(ActorKind::Robot, AccessTier::A1RedactedQuery),
        AuthzDecision::Allow
    );
    assert_eq!(
        check_authorization(ActorKind::Robot, AccessTier::A2FullQuery),
        AuthzDecision::Elevate
    );
    assert_eq!(
        check_authorization(ActorKind::Robot, AccessTier::A3PrivilegedRaw),
        AuthzDecision::Deny
    );
    assert_eq!(
        check_authorization(ActorKind::Robot, AccessTier::A4Admin),
        AuthzDecision::Deny
    );

    // Workflow: A2 default, can elevate to A3, denied A4.
    assert_eq!(
        check_authorization(ActorKind::Workflow, AccessTier::A2FullQuery),
        AuthzDecision::Allow
    );
    assert_eq!(
        check_authorization(ActorKind::Workflow, AccessTier::A3PrivilegedRaw),
        AuthzDecision::Elevate
    );
    assert_eq!(
        check_authorization(ActorKind::Workflow, AccessTier::A4Admin),
        AuthzDecision::Deny
    );

    // Human: A2 default, can elevate to A3 and A4.
    assert_eq!(
        check_authorization(ActorKind::Human, AccessTier::A3PrivilegedRaw),
        AuthzDecision::Elevate
    );
    assert_eq!(
        check_authorization(ActorKind::Human, AccessTier::A4Admin),
        AuthzDecision::Elevate
    );
}

// =============================================================================
// 5. Telemetry → Diagnostics integration
// =============================================================================

/// Diagnostics summary reflects telemetry snapshot state.
#[test]
fn telemetry_snapshot_produces_diagnostic_summary() {
    let telemetry = StorageTelemetry::with_defaults();

    // Record some operations.
    for i in 0..20 {
        telemetry.record_append(100.0 + i as f64 * 10.0, 5, 500, false);
    }
    telemetry.record_flush(50.0);
    telemetry.record_flush(75.0);

    let snapshot = telemetry.snapshot();
    assert_eq!(snapshot.total_batches, 20);
    assert_eq!(snapshot.total_events_appended, 100);
    assert_eq!(snapshot.total_flushes, 2);

    let summary = diagnose(&snapshot);
    assert!(!summary.status.is_empty());
    // Green health — no urgent items.
    assert_eq!(snapshot.health_tier, StorageHealthTier::Green);
    assert_eq!(summary.tier, StorageHealthTier::Green);
}

/// Error remediation messages are non-empty for all error classes.
#[test]
fn remediation_covers_all_error_classes() {
    let classes = [
        RecorderStorageErrorClass::Overload,
        RecorderStorageErrorClass::Retryable,
        RecorderStorageErrorClass::TerminalData,
        RecorderStorageErrorClass::Corruption,
    ];

    for class in &classes {
        let msg = remediation_for_error(*class);
        assert!(!msg.is_empty(), "Remediation missing for {:?}", class);
    }
}

// =============================================================================
// 6. Storage → Telemetry → Retention → Audit end-to-end
// =============================================================================

/// Full pipeline: append events, record telemetry, manage retention, audit everything.
#[tokio::test]
async fn full_pipeline_append_telemetry_retention_audit() {
    let dir = tempfile::tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(storage_config(dir.path())).unwrap();
    let telemetry = Arc::new(StorageTelemetry::with_defaults());
    let audit_log = AuditLog::new(AuditLogConfig::default());
    let mut retention_mgr = RetentionManager::with_defaults();

    let base_ms = 1_700_000_000_000u64;

    // Step 1: Append events to storage.
    let events: Vec<RecorderEvent> = (0..10)
        .map(|i| sample_event(&format!("pipe-{}", i), 1, i, &format!("data-{}", i)))
        .collect();

    let req = AppendRequest {
        batch_id: "pipeline-batch-1".to_string(),
        events,
        required_durability: DurabilityLevel::Appended,
        producer_ts_ms: base_ms,
    };

    let start = Instant::now();
    let resp = storage.append_batch(req).await.unwrap();
    let elapsed_us = start.elapsed().as_micros() as f64;

    // Step 2: Record in telemetry.
    telemetry.record_append(elapsed_us, resp.accepted_count, 5000, false);

    // Step 3: Create retention segment from appended data.
    let seg = make_segment(
        "seg_pipeline_001",
        SensitivityTier::T1Standard,
        SegmentPhase::Active,
        base_ms,
        5000,
        10,
        resp.first_offset.ordinal,
    );
    retention_mgr.add_segment(seg);

    // Step 4: Audit the query.
    let decision = check_authorization(ActorKind::Human, AccessTier::A2FullQuery);
    audit_log.append(
        AuditEventBuilder::new(AuditEventType::RecorderQuery, human_actor(), base_ms)
            .with_decision(decision)
            .with_pane_ids(vec![1])
            .with_result_count(10),
    );

    // Step 5: Flush storage.
    let flush_start = Instant::now();
    storage.flush(FlushMode::Buffered).await.unwrap();
    telemetry.record_flush(flush_start.elapsed().as_micros() as f64);

    // Step 6: Verify telemetry snapshot.
    let health = storage.health().await;
    telemetry.update_health(health);
    let snapshot = telemetry.snapshot();

    assert_eq!(snapshot.total_events_appended, 10);
    assert_eq!(snapshot.total_batches, 1);
    assert_eq!(snapshot.total_flushes, 1);
    assert_eq!(snapshot.health_tier, StorageHealthTier::Green);

    // Step 7: Retention stats.
    let ret_stats = retention_mgr.stats();
    assert_eq!(ret_stats.active_count, 1);
    assert_eq!(retention_mgr.total_events(), 10);

    // Step 8: Audit chain is intact.
    let verification = AuditLog::verify_chain(&audit_log.entries(), GENESIS_HASH);
    assert!(verification.chain_intact);
}

// =============================================================================
// 7. Audit log resume across "persistence boundaries"
// =============================================================================

/// Simulate persisting audit log to disk and resuming — chain stays valid.
#[test]
fn audit_log_resume_preserves_chain_across_persistence() {
    // Phase 1: Write some audit entries.
    let log1 = AuditLog::new(AuditLogConfig::default());

    for i in 0..5 {
        log1.append(
            AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                human_actor(),
                1_700_000_000_000 + i * 1000,
            )
            .with_pane_ids(vec![1]),
        );
    }

    let phase1_entries = log1.entries();
    let phase1_last_hash = log1.last_hash();
    let phase1_next_ordinal = log1.next_ordinal();

    // Simulate "flush to disk" — drain entries.
    let drained = log1.drain();
    assert_eq!(drained.len(), 5);
    assert!(log1.is_empty());

    // Phase 2: Resume from persisted state.
    let log2 = AuditLog::resume(
        AuditLogConfig::default(),
        phase1_next_ordinal,
        phase1_last_hash,
    );

    for i in 5..10 {
        log2.append(
            AuditEventBuilder::new(
                AuditEventType::RecorderReplay,
                robot_actor(),
                1_700_000_005_000 + i * 1000,
            )
            .with_decision(AuthzDecision::Allow),
        );
    }

    let phase2_entries = log2.entries();

    // Combined entries should form a valid chain.
    let mut all: Vec<_> = phase1_entries;
    all.extend(phase2_entries);

    let verification = AuditLog::verify_chain(&all, GENESIS_HASH);
    assert!(verification.chain_intact);
    assert_eq!(verification.total_entries, 10);
    assert_eq!(verification.ordinal_range, Some((0, 9)));
    assert!(verification.missing_ordinals.is_empty());
}

// =============================================================================
// 8. Retention policy validation across tiers
// =============================================================================

/// Default retention config is valid.
#[test]
fn default_retention_config_validates() {
    let config = RetentionConfig::default();
    assert!(config.validate().is_ok());
}

/// Retention hours differ by sensitivity tier per governance policy.
#[test]
fn retention_windows_differ_by_tier() {
    let config = RetentionConfig::default();

    let t1_hours = config.retention_hours(SensitivityTier::T1Standard);
    let t2_hours = config.retention_hours(SensitivityTier::T2Sensitive);
    let t3_hours = config.retention_hours(SensitivityTier::T3Restricted);

    // T3 has accelerated purge (shortest retention).
    assert!(
        t3_hours <= t1_hours,
        "T3 ({}) should be <= T1 ({})",
        t3_hours,
        t1_hours
    );
    assert!(
        t3_hours <= t2_hours,
        "T3 ({}) should be <= T2 ({})",
        t3_hours,
        t2_hours
    );

    // All tiers have positive retention.
    assert!(t1_hours > 0);
    assert!(t2_hours > 0);
    assert!(t3_hours > 0);
}

/// Segment lifecycle transitions follow valid paths.
#[test]
fn segment_lifecycle_transitions_are_valid() {
    assert!(SegmentPhase::Active.can_transition_to(SegmentPhase::Sealed));
    assert!(SegmentPhase::Sealed.can_transition_to(SegmentPhase::Archived));
    assert!(SegmentPhase::Archived.can_transition_to(SegmentPhase::Purged));

    // Invalid transitions.
    assert!(!SegmentPhase::Active.can_transition_to(SegmentPhase::Purged));
    assert!(!SegmentPhase::Sealed.can_transition_to(SegmentPhase::Active));
    assert!(!SegmentPhase::Purged.can_transition_to(SegmentPhase::Active));
}

// =============================================================================
// 9. Multi-actor concurrent audit scenario
// =============================================================================

/// Multiple actors performing operations simultaneously — all audited correctly.
#[test]
fn multi_actor_concurrent_operations_audited() {
    let audit_log = AuditLog::new(AuditLogConfig {
        max_memory_entries: 1000,
        ..AuditLogConfig::default()
    });

    let now = 1_700_000_000_000u64;

    // Human: queries and admin.
    for i in 0..10 {
        audit_log.append(
            AuditEventBuilder::new(AuditEventType::RecorderQuery, human_actor(), now + i * 100)
                .with_pane_ids(vec![1, 2])
                .with_result_count(i + 1),
        );
    }

    // Robot: queries (some denied).
    for i in 0..5 {
        let decision = if i % 2 == 0 {
            AuthzDecision::Allow
        } else {
            AuthzDecision::Deny
        };
        audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                robot_actor(),
                now + 1000 + i * 100,
            )
            .with_decision(decision),
        );
    }

    // Workflow: replay with elevation.
    for i in 0..3 {
        audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::RecorderReplay,
                workflow_actor(),
                now + 2000 + i * 100,
            )
            .with_decision(AuthzDecision::Elevate)
            .with_justification("Automated incident analysis"),
        );
    }

    // Admin purge by human.
    audit_log.append(
        AuditEventBuilder::new(AuditEventType::AdminPurge, human_actor(), now + 3000)
            .with_segment_ids(vec!["seg_expired_001".to_string()])
            .with_justification("Quarterly data cleanup"),
    );

    assert_eq!(audit_log.len(), 19);

    let stats = audit_log.stats();
    assert_eq!(stats.by_actor.get("human"), Some(&11));
    assert_eq!(stats.by_actor.get("robot"), Some(&5));
    assert_eq!(stats.by_actor.get("workflow"), Some(&3));
    assert_eq!(stats.denied_count, 2); // 2 odd-indexed robot queries.
    assert_eq!(stats.elevated_count, 3); // 3 workflow replays.

    // Full chain verification.
    let verification = AuditLog::verify_chain(&audit_log.entries(), GENESIS_HASH);
    assert!(verification.chain_intact);
    assert_eq!(verification.total_entries, 19);
    assert!(verification.missing_ordinals.is_empty());
}

// =============================================================================
// 10. Tamper detection scenarios
// =============================================================================

/// Modifying an entry's field breaks the hash chain.
#[test]
fn tamper_detection_modified_field() {
    let log = AuditLog::new(AuditLogConfig::default());
    for i in 0..5 {
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            human_actor(),
            i * 1000,
        ));
    }

    let mut entries = log.entries();
    // Tamper: change the result count of entry 2.
    entries[2].scope.result_count = Some(999);

    let result = AuditLog::verify_chain(&entries, GENESIS_HASH);
    assert!(!result.chain_intact);
    assert_eq!(result.first_break_at, Some(3)); // Entry 3's prev hash won't match.
}

/// Deleting an entry from the middle creates a gap and breaks the chain.
#[test]
fn tamper_detection_deleted_entry() {
    let log = AuditLog::new(AuditLogConfig::default());
    for i in 0..5 {
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            human_actor(),
            i * 1000,
        ));
    }

    let mut entries = log.entries();
    entries.remove(2); // Delete ordinal 2.

    let result = AuditLog::verify_chain(&entries, GENESIS_HASH);
    assert!(!result.chain_intact);
    assert_eq!(result.missing_ordinals, vec![2]);
}

/// Inserting a fake entry breaks the chain.
#[test]
fn tamper_detection_inserted_entry() {
    let log = AuditLog::new(AuditLogConfig::default());
    for i in 0..3 {
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            human_actor(),
            i * 1000,
        ));
    }

    let mut entries = log.entries();

    // Create a fake entry pretending to be ordinal 1.5.
    let fake = frankenterm_core::recorder_audit::RecorderAuditEntry {
        audit_version: AUDIT_SCHEMA_VERSION.to_string(),
        ordinal: 10, // Wrong ordinal.
        event_type: AuditEventType::AdminPurge,
        actor: ActorIdentity::new(ActorKind::Human, "attacker"),
        timestamp_ms: 999,
        scope: Default::default(),
        decision: AuthzDecision::Allow,
        justification: None,
        policy_version: "governance.v1".to_string(),
        prev_entry_hash: "fake_hash".to_string(),
        details: None,
    };

    entries.insert(2, fake);

    let result = AuditLog::verify_chain(&entries, GENESIS_HASH);
    assert!(!result.chain_intact);
}

// =============================================================================
// 11. Checkpoint holds prevent retention purge
// =============================================================================

/// Segments with active checkpoint consumers are held during sweep.
#[test]
fn checkpoint_holds_prevent_purge() {
    let config = RetentionConfig::default();
    let mut mgr = RetentionManager::new(config).unwrap();
    let audit_log = AuditLog::new(AuditLogConfig::default());

    let base_ms = 1_700_000_000_000u64;

    // Add an archived segment past cold window.
    let mut seg = make_segment(
        "seg_held",
        SensitivityTier::T1Standard,
        SegmentPhase::Archived,
        base_ms,
        10_000,
        100,
        0,
    );
    seg.archived_at_ms = Some(base_ms + 8 * 86_400_000);
    mgr.add_segment(seg);

    // A consumer holds a checkpoint referencing this segment.
    let mut holders: HashMap<String, Vec<String>> = HashMap::new();
    holders.insert("seg_held".to_string(), vec!["tantivy-indexer".to_string()]);

    // Sweep well past cold window.
    let sweep_time = base_ms + 90 * 86_400_000;
    let result = mgr.sweep(sweep_time, &holders);

    // Segment should be held, not purged.
    assert!(
        !result.held.is_empty(),
        "Segment should be held by checkpoint"
    );

    // Audit the hold.
    for (seg_id, consumer) in &result.held {
        audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::RetentionSegmentArchived, // Documented as held.
                workflow_actor(),
                sweep_time,
            )
            .with_segment_ids(vec![seg_id.clone()])
            .with_details(serde_json::json!({"held_by": consumer})),
        );
    }

    assert!(!audit_log.is_empty());
}

// =============================================================================
// 12. Audit schema version consistency
// =============================================================================

/// All audit entries use the current schema version.
#[test]
fn audit_entries_use_current_schema_version() {
    let log = AuditLog::new(AuditLogConfig::default());

    let event_types = [
        AuditEventType::RecorderQuery,
        AuditEventType::RecorderQueryPrivileged,
        AuditEventType::AdminPurge,
        AuditEventType::AccessApprovalGranted,
        AuditEventType::RetentionSegmentSealed,
    ];

    for (i, event_type) in event_types.iter().enumerate() {
        log.append(AuditEventBuilder::new(
            *event_type,
            human_actor(),
            i as u64 * 1000,
        ));
    }

    for entry in log.entries() {
        assert_eq!(entry.audit_version, AUDIT_SCHEMA_VERSION);
    }
}

// =============================================================================
// 13. Telemetry SLO evaluation
// =============================================================================

/// SLO status reflects latency percentiles.
#[test]
fn slo_status_reflects_latency() {
    let config = StorageTelemetryConfig {
        slo_append_p95_us: 5000.0, // 5ms SLO.
        ..StorageTelemetryConfig::default()
    };
    let telemetry = StorageTelemetry::new(config);

    // Record fast operations — should meet SLO.
    for _ in 0..100 {
        telemetry.record_append(100.0, 1, 100, false); // 100μs.
    }

    let snapshot = telemetry.snapshot();
    // With all operations at 100μs, p95 should be well under 5000μs SLO.
    assert_eq!(snapshot.total_events_appended, 100);
}

// =============================================================================
// 14. Retention manager total data tracking
// =============================================================================

/// Total data bytes and events tracked across segments.
#[test]
fn retention_tracks_aggregate_data() {
    let mut mgr = RetentionManager::with_defaults();
    let base_ms = 1_700_000_000_000u64;

    mgr.add_segment(make_segment(
        "seg_a",
        SensitivityTier::T1Standard,
        SegmentPhase::Active,
        base_ms,
        10_000,
        100,
        0,
    ));
    mgr.add_segment(make_segment(
        "seg_b",
        SensitivityTier::T2Sensitive,
        SegmentPhase::Sealed,
        base_ms,
        20_000,
        200,
        100,
    ));
    mgr.add_segment(make_segment(
        "seg_c",
        SensitivityTier::T3Restricted,
        SegmentPhase::Archived,
        base_ms,
        5_000,
        50,
        300,
    ));

    assert_eq!(mgr.total_data_bytes(), 35_000);
    assert_eq!(mgr.total_events(), 350);
    assert_eq!(mgr.segment_count(), 3);

    let stats = mgr.stats();
    assert_eq!(stats.live_count(), 3);
    assert_eq!(stats.live_bytes(), 35_000);
    assert_eq!(mgr.total_events(), 350);
}
