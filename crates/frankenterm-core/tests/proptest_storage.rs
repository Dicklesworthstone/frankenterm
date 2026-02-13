//! Property-based tests for the `storage` module data structures.
//!
//! Covers serde roundtrips and behaviour for `CorrelationType`, `MetricType`,
//! `NotificationStatus`, `DatabasePageStats` (with `free_ratio()`),
//! `CheckpointResult`, `Gap`, `EventAnnotations`, `EventMuteRecord`,
//! `FtsIndexState`, `FtsPaneProgress`, and `FtsSyncResult`.

use frankenterm_core::storage::{
    CheckpointResult, CorrelationType, DatabasePageStats, EventAnnotations, EventMuteRecord,
    FtsIndexState, FtsPaneProgress, FtsSyncResult, Gap, MetricType, NotificationStatus,
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
