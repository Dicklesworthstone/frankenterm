//! Property-based tests for the `storage` module data structures.
//!
//! Covers serde roundtrips and behaviour for `CorrelationType`, `MetricType`,
//! `NotificationStatus`, `DatabasePageStats` (with `free_ratio()`),
//! `CheckpointResult`, `Gap`, `EventAnnotations`, `EventMuteRecord`,
//! `FtsIndexState`, `FtsPaneProgress`, `FtsSyncResult`,
//! `MigrationStage`, `MigrationRollbackClass`, `MigrationRollbackTrigger`,
//! `MigrationInvariantSummary`, `MigrationRollbackClassifierConfig`,
//! `MigrationRollbackDecision`, `Segment`, `SemanticSearchHit`,
//! `PaneIndexingStats`, `EmbeddingStats`, `IndexingHealthReport`,
//! `classify_migration_rollback_trigger`, `DbCheckStatus`, `DbCheckItem`,
//! `DbCheckReport`, `DbRepairItem`, `DbRepairReport`, `TableStats`,
//! `PaneStats`, `EventTypeStats`, `DbStatsReport`, `SearchLintSeverity`,
//! `SearchLint`, `SearchSuggestion`, `SemanticBudgetConfig`,
//! `SemanticBudgetMetrics`, `SemanticBudgetSnapshot`, `Correlation`,
//! `CorrelationRef`, `PaneInfo`, `HandledInfo`, `TimelineEvent`, `Timeline`,
//! `DailyMetricSummary`, `AgentMetricBreakdown`,
//! `MigrationForensicBackendState`, `MigrationForensicMigrationCheckpoint`,
//! `MigrationForensicCorruptionDetail`, `MigrationForensicCaptureContext`,
//! `MigrationForensicBundle`, `MigrationRollbackExecutionState`,
//! `MigrationRollbackExecutionReport`, `MigrationRollbackExecutionError`,
//! and `SearchResult`.

use frankenterm_core::recorder_storage::{RecorderBackendKind, RecorderOffset};
use frankenterm_core::storage::{
    AgentMetricBreakdown, CheckpointResult, Correlation, CorrelationRef, CorrelationType,
    DailyMetricSummary, DatabasePageStats, DbCheckItem, DbCheckReport, DbCheckStatus, DbRepairItem,
    DbRepairReport, DbStatsReport, EmbeddingStats, EventAnnotations, EventMuteRecord,
    EventTypeStats, FtsIndexState, FtsPaneProgress, FtsSyncResult, Gap, HandledInfo,
    IndexingHealthReport, MetricType, MigrationForensicBackendState, MigrationForensicBundle,
    MigrationForensicCaptureContext, MigrationForensicCorruptionDetail,
    MigrationForensicMigrationCheckpoint, MigrationInvariantSummary, MigrationRollbackClass,
    MigrationRollbackClassifierConfig, MigrationRollbackClassifierInput, MigrationRollbackDecision,
    MigrationRollbackExecutionError, MigrationRollbackExecutionReport,
    MigrationRollbackExecutionState, MigrationRollbackTrigger, MigrationStage, NotificationStatus,
    PaneIndexingStats, PaneInfo, PaneStats, SearchLint, SearchLintSeverity, SearchResult,
    SearchSuggestion, Segment, SemanticBudgetConfig, SemanticBudgetMetrics, SemanticBudgetSnapshot,
    SemanticSearchHit, TableStats, Timeline, TimelineEvent, classify_migration_rollback_trigger,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_correlation_type() -> impl Strategy<Value = CorrelationType> {
    prop_oneof![
        Just(CorrelationType::Failover),
        Just(CorrelationType::Cascade),
        Just(CorrelationType::Temporal),
        Just(CorrelationType::WorkflowGroup),
        Just(CorrelationType::DedupeGroup),
    ]
}

fn arb_metric_type() -> impl Strategy<Value = MetricType> {
    prop_oneof![
        Just(MetricType::TokenUsage),
        Just(MetricType::ApiCost),
        Just(MetricType::ApiCall),
        Just(MetricType::RateLimitHit),
        Just(MetricType::WorkflowCost),
        Just(MetricType::SessionDuration),
    ]
}

fn arb_notification_status() -> impl Strategy<Value = NotificationStatus> {
    prop_oneof![
        Just(NotificationStatus::Pending),
        Just(NotificationStatus::Sent),
        Just(NotificationStatus::Failed),
        Just(NotificationStatus::Throttled),
    ]
}

// =========================================================================
// CorrelationType — serde + Display
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_correlation_type_serde(ct in arb_correlation_type()) {
        let json = serde_json::to_string(&ct).unwrap();
        let back: CorrelationType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, ct);
    }

    #[test]
    fn prop_correlation_type_display_not_empty(ct in arb_correlation_type()) {
        let display = ct.to_string();
        prop_assert!(!display.is_empty());
    }

    #[test]
    fn prop_correlation_type_display_values(ct in arb_correlation_type()) {
        let display = ct.to_string();
        let expected = match ct {
            CorrelationType::Failover => "failover",
            CorrelationType::Cascade => "cascade",
            CorrelationType::Temporal => "temporal",
            CorrelationType::WorkflowGroup => "workflow_group",
            CorrelationType::DedupeGroup => "dedupe_group",
        };
        prop_assert_eq!(&display, expected);
    }
}

// =========================================================================
// MetricType — serde + Display + FromStr + as_str
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_metric_type_serde(mt in arb_metric_type()) {
        let json = serde_json::to_string(&mt).unwrap();
        let back: MetricType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, mt);
    }

    #[test]
    fn prop_metric_type_display_roundtrip(mt in arb_metric_type()) {
        let s = mt.to_string();
        let back: MetricType = s.parse().unwrap();
        prop_assert_eq!(back, mt);
    }

    #[test]
    fn prop_metric_type_as_str_matches_display(mt in arb_metric_type()) {
        prop_assert_eq!(mt.as_str(), &mt.to_string());
    }

    #[test]
    fn prop_metric_type_snake_case(mt in arb_metric_type()) {
        let json = serde_json::to_string(&mt).unwrap();
        let expected = match mt {
            MetricType::TokenUsage => "\"token_usage\"",
            MetricType::ApiCost => "\"api_cost\"",
            MetricType::ApiCall => "\"api_call\"",
            MetricType::RateLimitHit => "\"rate_limit_hit\"",
            MetricType::WorkflowCost => "\"workflow_cost\"",
            MetricType::SessionDuration => "\"session_duration\"",
        };
        prop_assert_eq!(&json, expected);
    }
}

// =========================================================================
// NotificationStatus — serde + Display + FromStr + as_str
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_notification_status_serde(ns in arb_notification_status()) {
        let json = serde_json::to_string(&ns).unwrap();
        let back: NotificationStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, ns);
    }

    #[test]
    fn prop_notification_status_display_roundtrip(ns in arb_notification_status()) {
        let s = ns.to_string();
        let back: NotificationStatus = s.parse().unwrap();
        prop_assert_eq!(back, ns);
    }

    #[test]
    fn prop_notification_status_as_str_matches_display(ns in arb_notification_status()) {
        prop_assert_eq!(ns.as_str(), &ns.to_string());
    }
}

// =========================================================================
// DatabasePageStats — serde + free_ratio()
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_page_stats_serde(
        page_count in 0_i64..1_000_000,
        free_pages in 0_i64..1_000_000,
    ) {
        let stats = DatabasePageStats { page_count, free_pages };
        let json = serde_json::to_string(&stats).unwrap();
        let back: DatabasePageStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.page_count, stats.page_count);
        prop_assert_eq!(back.free_pages, stats.free_pages);
    }

    /// free_ratio is in [0.0, 1.0].
    #[test]
    fn prop_free_ratio_bounded(
        page_count in 0_i64..1_000_000,
        free_pages in 0_i64..1_000_000,
    ) {
        let stats = DatabasePageStats { page_count, free_pages };
        let ratio = stats.free_ratio();
        prop_assert!(ratio >= 0.0);
        prop_assert!(ratio <= 1.0);
    }

    /// free_ratio is 0.0 when page_count is 0.
    #[test]
    fn prop_free_ratio_zero_pages(free_pages in 0_i64..1000) {
        let stats = DatabasePageStats { page_count: 0, free_pages };
        prop_assert!(stats.free_ratio().abs() < f64::EPSILON);
    }

    /// free_ratio is 0.0 when free_pages is 0.
    #[test]
    fn prop_free_ratio_zero_free(page_count in 1_i64..1_000_000) {
        let stats = DatabasePageStats { page_count, free_pages: 0 };
        prop_assert!(stats.free_ratio().abs() < f64::EPSILON);
    }

    /// free_ratio equals 1.0 when all pages are free.
    #[test]
    fn prop_free_ratio_all_free(page_count in 1_i64..1_000_000) {
        let stats = DatabasePageStats { page_count, free_pages: page_count };
        let ratio = stats.free_ratio();
        prop_assert!((ratio - 1.0).abs() < 1e-10);
    }
}

// =========================================================================
// CheckpointResult — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_checkpoint_result_serde(
        wal_pages in 0_i64..100_000,
        optimized in any::<bool>(),
    ) {
        let result = CheckpointResult { wal_pages, optimized };
        let json = serde_json::to_string(&result).unwrap();
        let back: CheckpointResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.wal_pages, result.wal_pages);
        prop_assert_eq!(back.optimized, result.optimized);
    }
}

// =========================================================================
// Gap — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_gap_serde(
        id in 0_i64..100_000,
        pane_id in 0_u64..100,
        seq_before in 0_u64..100_000,
        seq_after in 0_u64..100_000,
        reason in "[a-z_]{5,20}",
        detected_at in 1_000_000_000_000_i64..2_000_000_000_000,
    ) {
        let gap = Gap {
            id, pane_id, seq_before, seq_after,
            reason: reason.clone(),
            detected_at,
        };
        let json = serde_json::to_string(&gap).unwrap();
        let back: Gap = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, gap.id);
        prop_assert_eq!(back.pane_id, gap.pane_id);
        prop_assert_eq!(back.seq_before, gap.seq_before);
        prop_assert_eq!(back.seq_after, gap.seq_after);
        prop_assert_eq!(&back.reason, &gap.reason);
        prop_assert_eq!(back.detected_at, gap.detected_at);
    }
}

// =========================================================================
// EventAnnotations — serde + Default
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_event_annotations_default(_dummy in 0..1_u8) {
        let annot = EventAnnotations::default();
        prop_assert!(annot.triage_state.is_none());
        prop_assert!(annot.note.is_none());
        prop_assert!(annot.labels.is_empty());
    }

    #[test]
    fn prop_event_annotations_serde(
        has_triage in any::<bool>(),
        has_note in any::<bool>(),
        label_count in 0_usize..5,
    ) {
        let annot = EventAnnotations {
            triage_state: if has_triage { Some("acknowledged".into()) } else { None },
            triage_updated_at: if has_triage { Some(1_700_000_000_000) } else { None },
            triage_updated_by: None,
            note: if has_note { Some("test note".into()) } else { None },
            note_updated_at: if has_note { Some(1_700_000_000_001) } else { None },
            note_updated_by: None,
            labels: (0..label_count).map(|i| format!("label_{i}")).collect(),
        };
        let json = serde_json::to_string(&annot).unwrap();
        let back: EventAnnotations = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.triage_state, &annot.triage_state);
        prop_assert_eq!(&back.note, &annot.note);
        prop_assert_eq!(back.labels.len(), annot.labels.len());
    }
}

// =========================================================================
// EventMuteRecord — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_event_mute_record_serde(
        identity_key in "[a-f0-9]{16}",
        scope in prop_oneof![Just("workspace".to_string()), Just("global".to_string())],
        created_at in 1_000_000_000_000_i64..2_000_000_000_000,
        has_expiry in any::<bool>(),
    ) {
        let record = EventMuteRecord {
            identity_key: identity_key.clone(),
            scope: scope.clone(),
            created_at,
            expires_at: if has_expiry { Some(created_at + 3_600_000) } else { None },
            created_by: Some("operator".into()),
            reason: Some("noise".into()),
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: EventMuteRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.identity_key, &record.identity_key);
        prop_assert_eq!(&back.scope, &record.scope);
        prop_assert_eq!(back.created_at, record.created_at);
        prop_assert_eq!(back.expires_at, record.expires_at);
    }
}

// =========================================================================
// FtsIndexState — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_fts_index_state_serde(
        index_version in 0_u32..100,
        has_rebuild in any::<bool>(),
        created_at in 1_000_000_000_000_i64..2_000_000_000_000,
        updated_at in 1_000_000_000_000_i64..2_000_000_000_000,
    ) {
        let state = FtsIndexState {
            index_version,
            last_full_rebuild_at: if has_rebuild { Some(created_at - 1000) } else { None },
            created_at,
            updated_at,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: FtsIndexState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.index_version, state.index_version);
        prop_assert_eq!(back.last_full_rebuild_at, state.last_full_rebuild_at);
        prop_assert_eq!(back.created_at, state.created_at);
        prop_assert_eq!(back.updated_at, state.updated_at);
    }
}

// =========================================================================
// FtsPaneProgress — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_fts_pane_progress_serde(
        pane_id in 0_u64..100,
        last_indexed_seq in 0_u64..100_000,
        indexed_count in 0_u64..100_000,
        last_indexed_at in 1_000_000_000_000_i64..2_000_000_000_000,
    ) {
        let progress = FtsPaneProgress {
            pane_id, last_indexed_seq, indexed_count, last_indexed_at,
        };
        let json = serde_json::to_string(&progress).unwrap();
        let back: FtsPaneProgress = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, progress.pane_id);
        prop_assert_eq!(back.last_indexed_seq, progress.last_indexed_seq);
        prop_assert_eq!(back.indexed_count, progress.indexed_count);
        prop_assert_eq!(back.last_indexed_at, progress.last_indexed_at);
    }
}

// =========================================================================
// FtsSyncResult — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_fts_sync_result_serde(
        segments_indexed in 0_u64..100_000,
        panes_processed in 0_u64..100,
        full_rebuild in any::<bool>(),
        duration_ms in 0_u64..100_000,
        warning_count in 0_usize..5,
    ) {
        let result = FtsSyncResult {
            segments_indexed,
            panes_processed,
            full_rebuild,
            duration_ms,
            warnings: (0..warning_count).map(|i| format!("warning_{i}")).collect(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: FtsSyncResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.segments_indexed, result.segments_indexed);
        prop_assert_eq!(back.panes_processed, result.panes_processed);
        prop_assert_eq!(back.full_rebuild, result.full_rebuild);
        prop_assert_eq!(back.duration_ms, result.duration_ms);
        prop_assert_eq!(back.warnings.len(), result.warnings.len());
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn all_correlation_types_distinct_json() {
    let types = [
        CorrelationType::Failover,
        CorrelationType::Cascade,
        CorrelationType::Temporal,
        CorrelationType::WorkflowGroup,
        CorrelationType::DedupeGroup,
    ];
    let jsons: Vec<_> = types
        .iter()
        .map(|t| serde_json::to_string(t).unwrap())
        .collect();
    for (i, json_i) in jsons.iter().enumerate() {
        for json_j in &jsons[i + 1..] {
            assert_ne!(json_i, json_j);
        }
    }
}

#[test]
fn all_metric_types_distinct_json() {
    let types = [
        MetricType::TokenUsage,
        MetricType::ApiCost,
        MetricType::ApiCall,
        MetricType::RateLimitHit,
        MetricType::WorkflowCost,
        MetricType::SessionDuration,
    ];
    let jsons: Vec<_> = types
        .iter()
        .map(|t| serde_json::to_string(t).unwrap())
        .collect();
    for (i, json_i) in jsons.iter().enumerate() {
        for json_j in &jsons[i + 1..] {
            assert_ne!(json_i, json_j);
        }
    }
}

#[test]
fn all_notification_statuses_distinct_json() {
    let statuses = [
        NotificationStatus::Pending,
        NotificationStatus::Sent,
        NotificationStatus::Failed,
        NotificationStatus::Throttled,
    ];
    let jsons: Vec<_> = statuses
        .iter()
        .map(|s| serde_json::to_string(s).unwrap())
        .collect();
    for (i, json_i) in jsons.iter().enumerate() {
        for json_j in &jsons[i + 1..] {
            assert_ne!(json_i, json_j);
        }
    }
}

#[test]
fn metric_type_from_str_rejects_unknown() {
    assert!("unknown".parse::<MetricType>().is_err());
}

#[test]
fn notification_status_from_str_rejects_unknown() {
    assert!("unknown".parse::<NotificationStatus>().is_err());
}

#[test]
fn free_ratio_negative_values() {
    let stats = DatabasePageStats {
        page_count: -10,
        free_pages: 5,
    };
    assert!(stats.free_ratio().abs() < f64::EPSILON);
    let stats2 = DatabasePageStats {
        page_count: 10,
        free_pages: -5,
    };
    assert!(stats2.free_ratio().abs() < f64::EPSILON);
}

#[test]
fn correlation_type_debug_nonempty() {
    let debug = format!("{:?}", CorrelationType::Failover);
    assert!(!debug.is_empty());
}

// ── Additional behavioral invariants ──────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// DatabasePageStats free_ratio is in [0, 1] for valid inputs.
    #[test]
    fn prop_free_ratio_bounded_clamped(page_count in 1i64..10_000, free_pages in 0i64..10_000) {
        let stats = DatabasePageStats {
            page_count,
            free_pages: free_pages.min(page_count),
        };
        let ratio = stats.free_ratio();
        prop_assert!((0.0..=1.0).contains(&ratio),
            "free_ratio {} should be in [0, 1]", ratio);
    }

    /// DatabasePageStats free_ratio with zero page_count returns 0.
    #[test]
    fn prop_free_ratio_zero_pages_edge(_dummy in 0..1u8) {
        let stats = DatabasePageStats { page_count: 0, free_pages: 0 };
        prop_assert!((stats.free_ratio() - 0.0).abs() < f64::EPSILON);
    }

    /// CheckpointResult serde roundtrip preserves wal_pages.
    #[test]
    fn prop_checkpoint_result_serde_ext(pages in 0i64..10_000) {
        let result = CheckpointResult {
            wal_pages: pages,
            optimized: true,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: CheckpointResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.wal_pages, pages);
    }

    /// Gap serde roundtrip preserves seq_before and seq_after.
    #[test]
    fn prop_gap_serde_ext(seq_before in 0u64..1_000_000, gap_size in 1u64..100_000) {
        let gap = Gap {
            id: 1,
            pane_id: 1,
            seq_before,
            seq_after: seq_before + gap_size,
            reason: "test".to_string(),
            detected_at: 12345,
        };
        let json = serde_json::to_string(&gap).unwrap();
        let back: Gap = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.seq_before, seq_before);
        prop_assert_eq!(back.seq_after, seq_before + gap_size);
    }

    /// MetricType as_str roundtrip through from_str.
    #[test]
    fn prop_metric_type_as_str_roundtrip(idx in 0usize..6) {
        let variants = ["token_usage", "api_cost", "api_call", "rate_limit_hit", "workflow_cost", "session_duration"];
        if idx < variants.len() {
            let parsed: MetricType = variants[idx].parse().unwrap();
            let s = parsed.as_str();
            prop_assert_eq!(s, variants[idx]);
        }
    }

    /// NotificationStatus as_str roundtrip through from_str.
    #[test]
    fn prop_notification_status_roundtrip(idx in 0usize..4) {
        let variants = ["pending", "sent", "failed", "throttled"];
        if idx < variants.len() {
            let parsed: NotificationStatus = variants[idx].parse().unwrap();
            let s = parsed.as_str();
            prop_assert_eq!(s, variants[idx]);
        }
    }

    /// FtsPaneProgress serde roundtrip preserves pane_id and indexed_count.
    #[test]
    fn prop_fts_pane_progress_serde_ext(pane_id in 1u64..1000, indexed in 0u64..10_000) {
        let progress = FtsPaneProgress {
            pane_id,
            last_indexed_seq: 42,
            indexed_count: indexed,
            last_indexed_at: 12345,
        };
        let json = serde_json::to_string(&progress).unwrap();
        let back: FtsPaneProgress = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, pane_id);
        prop_assert_eq!(back.indexed_count, indexed);
    }
}

// =========================================================================
// Migration + search type strategies
// =========================================================================

fn arb_migration_stage() -> impl Strategy<Value = MigrationStage> {
    prop_oneof![
        Just(MigrationStage::Preflight),
        Just(MigrationStage::Export),
        Just(MigrationStage::Import),
        Just(MigrationStage::CheckpointSync),
        Just(MigrationStage::ProjectionRebuild),
        Just(MigrationStage::Activate),
        Just(MigrationStage::Soak),
    ]
}

fn arb_rollback_class() -> impl Strategy<Value = MigrationRollbackClass> {
    prop_oneof![
        Just(MigrationRollbackClass::Immediate),
        Just(MigrationRollbackClass::PostCutover),
        Just(MigrationRollbackClass::DataIntegrityEmergency),
    ]
}

fn arb_rollback_trigger() -> impl Strategy<Value = MigrationRollbackTrigger> {
    prop_oneof![
        Just(MigrationRollbackTrigger::ImportDigestMismatch),
        Just(MigrationRollbackTrigger::EventCardinalityMismatch),
        Just(MigrationRollbackTrigger::CheckpointRegression),
        Just(MigrationRollbackTrigger::CorruptImport),
        Just(MigrationRollbackTrigger::InvariantErrors),
        Just(MigrationRollbackTrigger::InvariantCritical),
        Just(MigrationRollbackTrigger::SustainedSloBreach),
        Just(MigrationRollbackTrigger::SloAppendP95Breached),
        Just(MigrationRollbackTrigger::SloFlushP95Breached),
        Just(MigrationRollbackTrigger::HealthTierBlack),
        Just(MigrationRollbackTrigger::ProjectionLagBreach),
        Just(MigrationRollbackTrigger::RepeatedWriteFailures),
        Just(MigrationRollbackTrigger::RepeatedIndexFailures),
        Just(MigrationRollbackTrigger::PolicyAuditRegression),
        Just(MigrationRollbackTrigger::CanonicalDataLossConfirmed),
        Just(MigrationRollbackTrigger::CanonicalCorruptionSuspected),
    ]
}

// =========================================================================
// MigrationStage — serde + as_str + Default
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_migration_stage_serde_and_as_str(stage in arb_migration_stage()) {
        // Serde roundtrip
        let json = serde_json::to_string(&stage).unwrap();
        let back: MigrationStage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, stage);

        // as_str matches snake_case serde encoding
        let expected_json = format!("\"{}\"", stage.as_str());
        prop_assert_eq!(json, expected_json);
    }

    #[test]
    fn prop_migration_stage_default(_dummy in 0..1_u32) {
        let default = MigrationStage::default();
        prop_assert_eq!(default, MigrationStage::Preflight);
    }
}

// =========================================================================
// MigrationRollbackClass — serde + as_str
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_rollback_class_serde_and_as_str(class in arb_rollback_class()) {
        let json = serde_json::to_string(&class).unwrap();
        let back: MigrationRollbackClass = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, class);

        let expected_json = format!("\"{}\"", class.as_str());
        prop_assert_eq!(json, expected_json);
    }
}

// =========================================================================
// MigrationRollbackTrigger — serde + as_str (16 variants)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_rollback_trigger_serde_and_as_str(trigger in arb_rollback_trigger()) {
        let json = serde_json::to_string(&trigger).unwrap();
        let back: MigrationRollbackTrigger = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, trigger);

        let expected_json = format!("\"{}\"", trigger.as_str());
        prop_assert_eq!(json, expected_json);
    }
}

// =========================================================================
// MigrationInvariantSummary — serde + Default + has_breakage
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_invariant_summary_serde(
        warning_count in 0_usize..100,
        error_count in 0_usize..100,
        critical_count in 0_usize..100,
    ) {
        let summary = MigrationInvariantSummary {
            warning_count,
            error_count,
            critical_count,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: MigrationInvariantSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, summary);
    }

    #[test]
    fn prop_invariant_summary_default(_dummy in 0..1_u32) {
        let d = MigrationInvariantSummary::default();
        prop_assert_eq!(d.warning_count, 0);
        prop_assert_eq!(d.error_count, 0);
        prop_assert_eq!(d.critical_count, 0);
        prop_assert!(!d.has_breakage());
    }

    #[test]
    fn prop_invariant_summary_has_breakage(
        errors in 0_usize..10,
        criticals in 0_usize..10,
    ) {
        let summary = MigrationInvariantSummary {
            warning_count: 5,
            error_count: errors,
            critical_count: criticals,
        };
        let expected = errors > 0 || criticals > 0;
        prop_assert_eq!(summary.has_breakage(), expected);
    }
}

// =========================================================================
// MigrationRollbackClassifierConfig — serde + Default
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_classifier_config_serde(
        slo_windows in 1_u32..10,
        write_threshold in 1_u32..10,
        index_threshold in 1_u32..10,
    ) {
        let config = MigrationRollbackClassifierConfig {
            sustained_slo_windows: slo_windows,
            repeated_write_failure_threshold: write_threshold,
            repeated_index_failure_threshold: index_threshold,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: MigrationRollbackClassifierConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, config);
    }

    #[test]
    fn prop_classifier_config_default(_dummy in 0..1_u32) {
        let d = MigrationRollbackClassifierConfig::default();
        prop_assert_eq!(d.sustained_slo_windows, 3);
        prop_assert_eq!(d.repeated_write_failure_threshold, 3);
        prop_assert_eq!(d.repeated_index_failure_threshold, 3);
    }
}

// =========================================================================
// MigrationRollbackDecision — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_rollback_decision_serde(
        should_rollback in proptest::bool::ANY,
        has_class in proptest::bool::ANY,
        class in arb_rollback_class(),
        stage in arb_migration_stage(),
        trigger_count in 0_usize..4,
    ) {
        let triggers: Vec<MigrationRollbackTrigger> = (0..trigger_count)
            .map(|i| match i % 3 {
                0 => MigrationRollbackTrigger::CorruptImport,
                1 => MigrationRollbackTrigger::InvariantErrors,
                _ => MigrationRollbackTrigger::HealthTierBlack,
            })
            .collect();
        let decision = MigrationRollbackDecision {
            should_rollback,
            rollback_class: if has_class { Some(class) } else { None },
            triggers,
            stage,
            rationale: "test rationale".to_string(),
        };
        let json = serde_json::to_string(&decision).unwrap();
        let back: MigrationRollbackDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.should_rollback, decision.should_rollback);
        prop_assert_eq!(back.rollback_class, decision.rollback_class);
        prop_assert_eq!(back.triggers.len(), decision.triggers.len());
        prop_assert_eq!(back.stage, decision.stage);
        prop_assert_eq!(back.rationale, decision.rationale);
    }
}

// =========================================================================
// Segment — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_segment_serde(
        id in 0_i64..100_000,
        pane_id in 0_u64..100,
        seq in 0_u64..100_000,
        content in "[a-zA-Z0-9 ]{1,50}",
        has_hash in proptest::bool::ANY,
        captured_at in 1_000_000_000_000_i64..2_000_000_000_000,
    ) {
        let content_len = content.len();
        let segment = Segment {
            id,
            pane_id,
            seq,
            content: content.clone(),
            content_len,
            content_hash: if has_hash { Some("abc123".to_string()) } else { None },
            captured_at,
        };
        let json = serde_json::to_string(&segment).unwrap();
        let back: Segment = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, segment.id);
        prop_assert_eq!(back.pane_id, segment.pane_id);
        prop_assert_eq!(back.seq, segment.seq);
        prop_assert_eq!(&back.content, &segment.content);
        prop_assert_eq!(back.content_len, segment.content_len);
        prop_assert_eq!(back.content_hash, segment.content_hash);
        prop_assert_eq!(back.captured_at, segment.captured_at);
    }
}

// =========================================================================
// SemanticSearchHit — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_semantic_search_hit_serde(
        segment_id in 0_i64..100_000,
        score in -1.0_f64..1.0,
    ) {
        let hit = SemanticSearchHit { segment_id, score };
        let json = serde_json::to_string(&hit).unwrap();
        let back: SemanticSearchHit = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.segment_id, hit.segment_id);
        prop_assert!((back.score - hit.score).abs() < 1e-10);
    }
}

// =========================================================================
// PaneIndexingStats — serde + fts_consistent invariant
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_pane_indexing_stats_serde(
        pane_id in 0_u64..100,
        segment_count in 0_u64..10_000,
        total_bytes in 0_u64..1_000_000,
        has_max_seq in proptest::bool::ANY,
        max_seq_val in 0_u64..100_000,
        fts_row_count in 0_u64..10_000,
    ) {
        let fts_consistent = segment_count == fts_row_count;
        let stats = PaneIndexingStats {
            pane_id,
            segment_count,
            total_bytes,
            max_seq: if has_max_seq { Some(max_seq_val) } else { None },
            last_segment_at: Some(1_700_000_000_000),
            fts_row_count,
            fts_consistent,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: PaneIndexingStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, stats.pane_id);
        prop_assert_eq!(back.segment_count, stats.segment_count);
        prop_assert_eq!(back.fts_row_count, stats.fts_row_count);
        prop_assert_eq!(back.fts_consistent, stats.fts_consistent);
    }
}

// =========================================================================
// EmbeddingStats — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_embedding_stats_serde(
        embedder_id in "[a-z_]{3,15}",
        dimension in 64_i32..2048,
        count in 0_i64..100_000,
        earliest_at in 1_000_000_000_i64..1_700_000_000,
        latest_at in 1_700_000_000_i64..2_000_000_000,
    ) {
        let stats = EmbeddingStats {
            embedder_id: embedder_id.clone(),
            dimension,
            count,
            earliest_at,
            latest_at,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: EmbeddingStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.embedder_id, &stats.embedder_id);
        prop_assert_eq!(back.dimension, stats.dimension);
        prop_assert_eq!(back.count, stats.count);
        prop_assert_eq!(back.earliest_at, stats.earliest_at);
        prop_assert_eq!(back.latest_at, stats.latest_at);
    }
}

// =========================================================================
// IndexingHealthReport — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_indexing_health_report_serde(
        pane_count in 0_usize..5,
        total_segments in 0_u64..10_000,
        total_bytes in 0_u64..1_000_000,
        healthy in proptest::bool::ANY,
    ) {
        let panes: Vec<PaneIndexingStats> = (0..pane_count).map(|i| PaneIndexingStats {
            pane_id: i as u64,
            segment_count: 10,
            total_bytes: 1000,
            max_seq: Some(10),
            last_segment_at: Some(1_700_000_000_000),
            fts_row_count: 10,
            fts_consistent: true,
        }).collect();
        let report = IndexingHealthReport {
            panes,
            total_segments,
            total_bytes,
            total_fts_rows: total_segments,
            inconsistent_panes: 0,
            healthy,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: IndexingHealthReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.panes.len(), report.panes.len());
        prop_assert_eq!(back.total_segments, report.total_segments);
        prop_assert_eq!(back.healthy, report.healthy);
    }
}

// =========================================================================
// classify_migration_rollback_trigger — no rollback on clean input
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_classifier_clean_input_no_rollback(stage in arb_migration_stage()) {
        let mut input = MigrationRollbackClassifierInput::default();
        input.stage = stage;
        // All signals are clean/false
        let decision = classify_migration_rollback_trigger(&input);
        prop_assert!(!decision.should_rollback);
        prop_assert!(decision.rollback_class.is_none());
        prop_assert!(decision.triggers.is_empty());
        prop_assert_eq!(decision.stage, stage);
    }

    #[test]
    fn prop_classifier_data_loss_triggers_emergency(_dummy in 0..1_u32) {
        let mut input = MigrationRollbackClassifierInput::default();
        input.confirmed_canonical_data_loss = true;
        let decision = classify_migration_rollback_trigger(&input);
        prop_assert!(decision.should_rollback);
        let is_emergency = decision.rollback_class == Some(MigrationRollbackClass::DataIntegrityEmergency);
        prop_assert!(is_emergency);
        let has_trigger = decision.triggers.contains(&MigrationRollbackTrigger::CanonicalDataLossConfirmed);
        prop_assert!(has_trigger);
    }

    #[test]
    fn prop_classifier_corrupt_import_triggers_immediate(_dummy in 0..1_u32) {
        let mut input = MigrationRollbackClassifierInput::default();
        input.corrupt_import = true;
        let decision = classify_migration_rollback_trigger(&input);
        prop_assert!(decision.should_rollback);
        let has_trigger = decision.triggers.contains(&MigrationRollbackTrigger::CorruptImport);
        prop_assert!(has_trigger);
    }

    #[test]
    fn prop_classifier_invariant_errors_trigger(
        errors in 1_usize..10,
    ) {
        let mut input = MigrationRollbackClassifierInput::default();
        input.invariants = Some(MigrationInvariantSummary {
            warning_count: 0,
            error_count: errors,
            critical_count: 0,
        });
        let decision = classify_migration_rollback_trigger(&input);
        prop_assert!(decision.should_rollback);
        let has_trigger = decision.triggers.contains(&MigrationRollbackTrigger::InvariantErrors);
        prop_assert!(has_trigger);
    }
}

// =========================================================================
// Strategies — DB health types
// =========================================================================

fn arb_db_check_status() -> impl Strategy<Value = DbCheckStatus> {
    prop_oneof![
        Just(DbCheckStatus::Ok),
        Just(DbCheckStatus::Warning),
        Just(DbCheckStatus::Error),
    ]
}

fn arb_db_check_item() -> impl Strategy<Value = DbCheckItem> {
    (
        "[a-z_]{3,12}",
        arb_db_check_status(),
        proptest::option::of("[a-z ]{5,20}"),
    )
        .prop_map(|(name, status, detail)| DbCheckItem {
            name,
            status,
            detail,
        })
}

fn arb_recorder_offset() -> impl Strategy<Value = RecorderOffset> {
    (0_u64..100, 0_u64..100_000, 0_u64..100_000).prop_map(|(segment_id, byte_offset, ordinal)| {
        RecorderOffset {
            segment_id,
            byte_offset,
            ordinal,
        }
    })
}

fn arb_recorder_backend_kind() -> impl Strategy<Value = RecorderBackendKind> {
    prop_oneof![
        Just(RecorderBackendKind::AppendLog),
        Just(RecorderBackendKind::FrankenSqlite),
    ]
}

fn arb_search_lint_severity() -> impl Strategy<Value = SearchLintSeverity> {
    prop_oneof![
        Just(SearchLintSeverity::Error),
        Just(SearchLintSeverity::Warning),
    ]
}

fn arb_pane_info() -> impl Strategy<Value = PaneInfo> {
    (
        0_u64..10000,
        proptest::option::of("[a-f0-9]{8}"),
        proptest::option::of("codex|claude_code|gemini"),
        "local|ssh|wsl",
        proptest::option::of("/tmp/[a-z]{3,8}"),
        proptest::option::of("[a-z ]{3,15}"),
    )
        .prop_map(
            |(pane_id, pane_uuid, agent_type, domain, cwd, title)| PaneInfo {
                pane_id,
                pane_uuid,
                agent_type,
                domain,
                cwd,
                title,
            },
        )
}

// =========================================================================
// DbCheckStatus — serde + Display
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_db_check_status_serde(status in arb_db_check_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let back: DbCheckStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, status);
    }

    #[test]
    fn prop_db_check_status_display_not_empty(status in arb_db_check_status()) {
        let display = status.to_string();
        prop_assert!(!display.is_empty());
    }
}

// =========================================================================
// DbCheckItem — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_db_check_item_serde(item in arb_db_check_item()) {
        let json = serde_json::to_string(&item).unwrap();
        let back: DbCheckItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &item.name);
        prop_assert_eq!(back.status, item.status);
        prop_assert_eq!(&back.detail, &item.detail);
    }
}

// =========================================================================
// DbCheckReport — serde + has_errors/has_warnings/problem_count
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_db_check_report_serde(
        path in "[a-z/]{5,20}",
        exists in any::<bool>(),
        size in proptest::option::of(0_u64..1_000_000),
        version in proptest::option::of(0_i32..100),
        items in proptest::collection::vec(arb_db_check_item(), 0..5),
    ) {
        let report = DbCheckReport {
            db_path: path,
            db_exists: exists,
            db_size_bytes: size,
            schema_version: version,
            checks: items,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: DbCheckReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.db_path, &report.db_path);
        prop_assert_eq!(back.checks.len(), report.checks.len());
    }

    #[test]
    fn prop_db_check_report_has_errors_consistent(
        items in proptest::collection::vec(arb_db_check_item(), 0..5),
    ) {
        let report = DbCheckReport {
            db_path: "/test".to_string(),
            db_exists: true,
            db_size_bytes: None,
            schema_version: None,
            checks: items,
        };
        let expected = report.checks.iter().any(|c| c.status == DbCheckStatus::Error);
        prop_assert_eq!(report.has_errors(), expected);
    }

    #[test]
    fn prop_db_check_report_problem_count(
        items in proptest::collection::vec(arb_db_check_item(), 0..5),
    ) {
        let report = DbCheckReport {
            db_path: "/test".to_string(),
            db_exists: true,
            db_size_bytes: None,
            schema_version: None,
            checks: items,
        };
        let expected = report
            .checks
            .iter()
            .filter(|c| c.status != DbCheckStatus::Ok)
            .count();
        prop_assert_eq!(report.problem_count(), expected);
    }
}

// =========================================================================
// DbRepairItem + DbRepairReport — serde + all_succeeded
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_db_repair_item_serde(
        name in "[a-z_]{3,12}",
        success in any::<bool>(),
        detail in "[a-z ]{5,20}",
    ) {
        let item = DbRepairItem { name, success, detail };
        let json = serde_json::to_string(&item).unwrap();
        let back: DbRepairItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &item.name);
        prop_assert_eq!(back.success, item.success);
    }

    #[test]
    fn prop_db_repair_report_all_succeeded(
        repairs in proptest::collection::vec(
            ("[a-z_]{3,8}", any::<bool>(), "[a-z]{3,8}").prop_map(|(n, s, d)| DbRepairItem { name: n, success: s, detail: d }),
            0..5,
        ),
        backup in proptest::option::of("[a-z/]{5,15}"),
    ) {
        let report = DbRepairReport { backup_path: backup, repairs };
        let expected = report.repairs.iter().all(|r| r.success);
        prop_assert_eq!(report.all_succeeded(), expected);
    }
}

// =========================================================================
// TableStats, PaneStats, EventTypeStats, DbStatsReport — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_table_stats_serde(name in "[a-z_]{3,12}", row_count in 0_u64..100_000) {
        let ts = TableStats { name, row_count };
        let json = serde_json::to_string(&ts).unwrap();
        let back: TableStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &ts.name);
        prop_assert_eq!(back.row_count, ts.row_count);
    }

    #[test]
    fn prop_pane_stats_serde(
        pane_id in 0_u64..10000,
        title in proptest::option::of("[a-z ]{3,10}"),
        seg_count in 0_u64..10000,
        seg_bytes in 0_u64..1_000_000,
        evt_count in 0_u64..10000,
    ) {
        let ps = PaneStats { pane_id, title, segment_count: seg_count, segment_bytes: seg_bytes, event_count: evt_count };
        let json = serde_json::to_string(&ps).unwrap();
        let back: PaneStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, ps.pane_id);
        prop_assert_eq!(back.segment_count, ps.segment_count);
    }

    #[test]
    fn prop_event_type_stats_serde(event_type in "[a-z_]{3,12}", count in 0_u64..10000) {
        let ets = EventTypeStats { event_type, count };
        let json = serde_json::to_string(&ets).unwrap();
        let back: EventTypeStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.event_type, &ets.event_type);
        prop_assert_eq!(back.count, ets.count);
    }

    #[test]
    fn prop_db_stats_report_serde(
        path in "[a-z/]{5,15}",
        size in proptest::option::of(0_u64..1_000_000),
    ) {
        let report = DbStatsReport {
            db_path: path.clone(),
            db_size_bytes: size,
            tables: vec![TableStats { name: "events".to_string(), row_count: 42 }],
            top_panes: vec![],
            event_types: vec![],
            suggestions: vec!["run vacuum".to_string()],
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: DbStatsReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.db_path, &path);
        prop_assert_eq!(back.tables.len(), 1);
    }
}

// =========================================================================
// SearchLintSeverity + SearchLint + SearchSuggestion — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_search_lint_severity_serde(sev in arb_search_lint_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let back: SearchLintSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, sev);
    }

    #[test]
    fn prop_search_lint_serde(
        code in "[A-Z]{2}[0-9]{3}",
        sev in arb_search_lint_severity(),
        message in "[a-z ]{5,20}",
        suggestion in proptest::option::of("[a-z ]{5,15}"),
    ) {
        let lint = SearchLint { code, severity: sev, message, suggestion };
        let json = serde_json::to_string(&lint).unwrap();
        let back: SearchLint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.code, &lint.code);
        prop_assert_eq!(back.severity, lint.severity);
    }

    #[test]
    fn prop_search_lint_skip_none_suggestion(
        code in "[A-Z]{2}[0-9]{3}",
        sev in arb_search_lint_severity(),
        message in "[a-z ]{5,20}",
    ) {
        let lint = SearchLint { code, severity: sev, message, suggestion: None };
        let json = serde_json::to_string(&lint).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        let has_suggestion = val.as_object().unwrap().contains_key("suggestion");
        prop_assert!(!has_suggestion, "None suggestion should be skipped");
    }

    #[test]
    fn prop_search_suggestion_serde(
        text in "[a-z]{3,12}",
        description in proptest::option::of("[a-z ]{5,15}"),
    ) {
        let sug = SearchSuggestion { text, description };
        let json = serde_json::to_string(&sug).unwrap();
        let back: SearchSuggestion = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.text, &sug.text);
    }
}

// =========================================================================
// SemanticBudgetConfig — serde + Default
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_semantic_budget_config_default_roundtrip(_dummy in 0..1_u8) {
        let config = SemanticBudgetConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: SemanticBudgetConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.max_semantic_latency_ms, 75);
        prop_assert_eq!(back.cache_capacity, 256);
    }

    #[test]
    fn prop_semantic_budget_config_serde(
        latency in 1_u64..1000,
        cooldown in 1_i64..60_000,
        queries in 1_u32..1000,
    ) {
        let config = SemanticBudgetConfig {
            max_semantic_latency_ms: latency,
            semantic_backoff_cooldown_ms: cooldown,
            max_semantic_queries_per_window: queries,
            ..SemanticBudgetConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SemanticBudgetConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.max_semantic_latency_ms, config.max_semantic_latency_ms);
        prop_assert_eq!(back.max_semantic_queries_per_window, config.max_semantic_queries_per_window);
    }
}

// =========================================================================
// SemanticBudgetMetrics — serde + Default
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_semantic_budget_metrics_default_roundtrip(_dummy in 0..1_u8) {
        let metrics = SemanticBudgetMetrics::default();
        let json = serde_json::to_string(&metrics).unwrap();
        let back: SemanticBudgetMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_semantic_requests, 0);
        prop_assert_eq!(back.semantic_cache_hits, 0);
    }

    #[test]
    fn prop_semantic_budget_metrics_serde(
        requests in 0_u64..10_000,
        hits in 0_u64..10_000,
        misses in 0_u64..10_000,
    ) {
        let mut metrics = SemanticBudgetMetrics::default();
        metrics.total_semantic_requests = requests;
        metrics.semantic_cache_hits = hits;
        metrics.semantic_cache_misses = misses;
        let json = serde_json::to_string(&metrics).unwrap();
        let back: SemanticBudgetMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_semantic_requests, requests);
        prop_assert_eq!(back.semantic_cache_hits, hits);
    }
}

// =========================================================================
// SemanticBudgetSnapshot — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_semantic_budget_snapshot_serde(
        cache_entries in 0_usize..1000,
        backoff in proptest::option::of(0_i64..100_000),
    ) {
        let snapshot = SemanticBudgetSnapshot {
            config: SemanticBudgetConfig::default(),
            metrics: SemanticBudgetMetrics::default(),
            ewma_semantic_latency_ms: 12.5,
            backoff_until_ms: backoff,
            cache_entries,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: SemanticBudgetSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.cache_entries, cache_entries);
        prop_assert_eq!(back.backoff_until_ms, backoff);
    }
}

// =========================================================================
// Timeline types — Correlation, CorrelationRef, PaneInfo, HandledInfo,
// TimelineEvent, Timeline
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_correlation_ref_serde(
        id in "[a-f0-9]{8}",
        ct in arb_correlation_type(),
    ) {
        let cr = CorrelationRef { id, correlation_type: ct };
        let json = serde_json::to_string(&cr).unwrap();
        let back: CorrelationRef = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.id, &cr.id);
        prop_assert_eq!(back.correlation_type, cr.correlation_type);
    }

    #[test]
    fn prop_correlation_serde(
        id in "[a-f0-9]{8}",
        event_ids in proptest::collection::vec(0_i64..10000, 1..5),
        ct in arb_correlation_type(),
        desc in "[a-z ]{5,20}",
    ) {
        let corr = Correlation {
            id,
            event_ids,
            correlation_type: ct,
            confidence: 0.85,
            description: desc,
        };
        let json = serde_json::to_string(&corr).unwrap();
        let back: Correlation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.id, &corr.id);
        prop_assert_eq!(back.event_ids.len(), corr.event_ids.len());
    }

    #[test]
    fn prop_pane_info_serde(pi in arb_pane_info()) {
        let json = serde_json::to_string(&pi).unwrap();
        let back: PaneInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, pi.pane_id);
        prop_assert_eq!(&back.domain, &pi.domain);
    }

    #[test]
    fn prop_handled_info_serde(
        handled_at in 0_i64..2_000_000_000_000,
        workflow_id in proptest::option::of("[a-f0-9]{8}"),
        status in "success|failure|skipped",
    ) {
        let hi = HandledInfo { handled_at, workflow_id, status };
        let json = serde_json::to_string(&hi).unwrap();
        let back: HandledInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.handled_at, hi.handled_at);
        prop_assert_eq!(&back.status, &hi.status);
    }

    #[test]
    fn prop_timeline_event_serde(pi in arb_pane_info()) {
        let te = TimelineEvent {
            id: 42,
            timestamp: 1700000000000,
            pane_info: pi,
            rule_id: "test_rule".to_string(),
            event_type: "error".to_string(),
            severity: "high".to_string(),
            confidence: 0.95,
            handled: None,
            correlations: vec![],
            summary: Some("test summary".to_string()),
        };
        let json = serde_json::to_string(&te).unwrap();
        let back: TimelineEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, 42);
        prop_assert_eq!(&back.rule_id, "test_rule");
    }

    #[test]
    fn prop_timeline_serde(
        total in 0_u64..1000,
        has_more in any::<bool>(),
    ) {
        let timeline = Timeline {
            start: 1700000000000,
            end: 1700001000000,
            events: vec![],
            correlations: vec![],
            total_count: total,
            has_more,
        };
        let json = serde_json::to_string(&timeline).unwrap();
        let back: Timeline = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_count, total);
        prop_assert_eq!(back.has_more, has_more);
    }
}

// =========================================================================
// SearchResult — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_search_result_serde(
        pane_id in 0_u64..10000,
        seq in 0_u64..10000,
        content in "[a-z ]{5,30}",
        score in 0.0_f64..100.0,
    ) {
        let sr = SearchResult {
            segment: Segment {
                id: 1,
                pane_id,
                seq,
                content: content.clone(),
                content_len: content.len(),
                content_hash: None,
                captured_at: 1700000000,
            },
            snippet: Some("...match...".to_string()),
            highlight: None,
            score,
        };
        let json = serde_json::to_string(&sr).unwrap();
        let back: SearchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.segment.pane_id, pane_id);
    }
}

// =========================================================================
// DailyMetricSummary + AgentMetricBreakdown — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_daily_metric_summary_serde(
        day_ts in 0_i64..2_000_000_000_000,
        agent_type in proptest::option::of("codex|claude_code"),
        tokens in 0_i64..1_000_000,
        events in 0_i64..10_000,
    ) {
        let dms = DailyMetricSummary {
            day_ts,
            agent_type,
            total_tokens: tokens,
            total_cost: 1.23,
            event_count: events,
        };
        let json = serde_json::to_string(&dms).unwrap();
        let back: DailyMetricSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.day_ts, dms.day_ts);
        prop_assert_eq!(back.total_tokens, dms.total_tokens);
    }

    #[test]
    fn prop_agent_metric_breakdown_serde(
        agent_type in "codex|claude_code|gemini",
        tokens in 0_i64..1_000_000,
    ) {
        let amb = AgentMetricBreakdown {
            agent_type,
            total_tokens: tokens,
            total_cost: 5.67,
            avg_tokens_per_event: 123.4,
        };
        let json = serde_json::to_string(&amb).unwrap();
        let back: AgentMetricBreakdown = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.agent_type, &amb.agent_type);
        prop_assert_eq!(back.total_tokens, amb.total_tokens);
    }
}

// =========================================================================
// Migration forensic types — serde roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_forensic_backend_state_serde(
        health in any::<bool>(),
        head in proptest::option::of(arb_recorder_offset()),
        cp in proptest::option::of(arb_recorder_offset()),
    ) {
        let fbs = MigrationForensicBackendState {
            health,
            head_offset: head,
            last_checkpoint: cp,
        };
        let json = serde_json::to_string(&fbs).unwrap();
        let back: MigrationForensicBackendState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.health, fbs.health);
        prop_assert_eq!(back, fbs);
    }

    #[test]
    fn prop_forensic_migration_checkpoint_serde(
        stage in arb_migration_stage(),
        manifest in "[a-z_]{5,15}",
    ) {
        let fmc = MigrationForensicMigrationCheckpoint {
            last_completed_stage: stage,
            manifest,
        };
        let json = serde_json::to_string(&fmc).unwrap();
        let back: MigrationForensicMigrationCheckpoint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, fmc);
    }

    #[test]
    fn prop_forensic_corruption_detail_serde(
        location in "[a-z_]{5,15}",
        ordinals in proptest::collection::vec(0_u64..10_000, 0..5),
        detail in "[a-z ]{5,20}",
    ) {
        let fcd = MigrationForensicCorruptionDetail {
            location,
            affected_ordinals: ordinals,
            detail,
        };
        let json = serde_json::to_string(&fcd).unwrap();
        let back: MigrationForensicCorruptionDetail = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, fcd);
    }

    #[test]
    fn prop_forensic_capture_context_serde(
        src_health in any::<bool>(),
        tgt_health in any::<bool>(),
        stage in arb_migration_stage(),
    ) {
        let ctx = MigrationForensicCaptureContext {
            source_state: MigrationForensicBackendState {
                health: src_health,
                head_offset: None,
                last_checkpoint: None,
            },
            target_state: MigrationForensicBackendState {
                health: tgt_health,
                head_offset: None,
                last_checkpoint: None,
            },
            migration_checkpoint: MigrationForensicMigrationCheckpoint {
                last_completed_stage: stage,
                manifest: "v1".to_string(),
            },
            corruption_detail: MigrationForensicCorruptionDetail {
                location: "index".to_string(),
                affected_ordinals: vec![],
                detail: "none".to_string(),
            },
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let back: MigrationForensicCaptureContext = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, ctx);
    }

    #[test]
    fn prop_forensic_bundle_serde(
        captured_at in 0_i64..2_000_000_000_000,
        rc in arb_rollback_class(),
        stage in arb_migration_stage(),
    ) {
        let bundle = MigrationForensicBundle {
            captured_at_ms: captured_at,
            rollback_class: rc,
            from_stage: stage,
            source_state: MigrationForensicBackendState {
                health: true,
                head_offset: None,
                last_checkpoint: None,
            },
            target_state: MigrationForensicBackendState {
                health: false,
                head_offset: None,
                last_checkpoint: None,
            },
            migration_checkpoint: MigrationForensicMigrationCheckpoint {
                last_completed_stage: MigrationStage::Preflight,
                manifest: "m1".to_string(),
            },
            corruption_detail: MigrationForensicCorruptionDetail {
                location: "wal".to_string(),
                affected_ordinals: vec![1, 2, 3],
                detail: "crc mismatch".to_string(),
            },
        };
        let json = serde_json::to_string(&bundle).unwrap();
        let back: MigrationForensicBundle = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, bundle);
    }
}

// =========================================================================
// MigrationRollbackExecutionState — serde + Default + methods
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_execution_state_default_roundtrip(_dummy in 0..1_u8) {
        let state = MigrationRollbackExecutionState::default();
        let json = serde_json::to_string(&state).unwrap();
        let back: MigrationRollbackExecutionState = serde_json::from_str(&json).unwrap();
        prop_assert!(!back.migration_active);
        prop_assert!(!back.emergency_freeze);
        prop_assert_eq!(back, state);
    }

    #[test]
    fn prop_execution_state_recorder_writes_blocked(freeze in any::<bool>()) {
        let mut state = MigrationRollbackExecutionState::default();
        state.emergency_freeze = freeze;
        prop_assert_eq!(state.recorder_writes_blocked(), freeze);
    }

    #[test]
    fn prop_execution_state_manual_reenable(_dummy in 0..1_u8) {
        let mut state = MigrationRollbackExecutionState::default();
        state.emergency_freeze = true;
        prop_assert!(state.recorder_writes_blocked());
        state.manual_reenable_recorder_writes();
        prop_assert!(!state.recorder_writes_blocked());
    }
}

// =========================================================================
// MigrationRollbackExecutionReport — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_execution_report_serde(
        rc in arb_rollback_class(),
        stage in arb_migration_stage(),
        active in any::<bool>(),
        backend in arb_recorder_backend_kind(),
    ) {
        let report = MigrationRollbackExecutionReport {
            tier: rc,
            from_stage: stage,
            migration_active: active,
            backend_selector: backend,
            target_cleared: false,
            projection_rebuild_triggered: true,
            checkpoints_reset: false,
            source_health_verified: true,
            index_health_verified: true,
            emergency_freeze_active: false,
            forensic_bundle_path: None,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: MigrationRollbackExecutionReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, report);
    }
}

// =========================================================================
// MigrationRollbackExecutionError — tagged serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_execution_error_unsupported_tier_serde(rc in arb_rollback_class()) {
        let err = MigrationRollbackExecutionError::UnsupportedTier { rollback_class: rc };
        let json = serde_json::to_string(&err).unwrap();
        let back: MigrationRollbackExecutionError = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, err);
        prop_assert!(json.contains("unsupported_tier"));
    }

    #[test]
    fn prop_execution_error_source_health_failed_serde(rc in arb_rollback_class()) {
        let err = MigrationRollbackExecutionError::SourceHealthFailed { tier: rc };
        let json = serde_json::to_string(&err).unwrap();
        let back: MigrationRollbackExecutionError = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, err);
        prop_assert!(json.contains("source_health_failed"));
    }

    #[test]
    fn prop_execution_error_all_variants_distinct(_dummy in 0..1_u8) {
        let variants = [
            serde_json::to_string(&MigrationRollbackExecutionError::IndexHealthFailedPostCutover).unwrap(),
            serde_json::to_string(&MigrationRollbackExecutionError::ForensicCaptureMissing).unwrap(),
            serde_json::to_string(&MigrationRollbackExecutionError::ForensicPersistFailed {
                path: std::path::PathBuf::from("/tmp/forensic.json"),
                error: "disk full".to_string(),
            }).unwrap(),
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i != j {
                    prop_assert_ne!(a, b);
                }
            }
        }
    }
}
