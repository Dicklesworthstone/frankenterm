//! Property-based tests for `storage` module health, stats, and search types.
//!
//! Covers `DbCheckStatus` serde+Display, `DbCheckItem`, `DbCheckReport`
//! (with `has_errors`/`has_warnings`/`problem_count`), `DbRepairItem`,
//! `DbRepairReport` (with `all_succeeded`), `TableStats`, `PaneStats`,
//! `EventTypeStats`, `DbStatsReport`, `SearchLintSeverity`, `SearchLint`,
//! `SearchSuggestion`, `ApprovalTokenRecord` (with `is_active`),
//! and `PaneReservation` (with `is_active`).

use frankenterm_core::storage::{
    ApprovalTokenRecord, DbCheckItem, DbCheckReport, DbCheckStatus, DbRepairItem, DbRepairReport,
    DbStatsReport, EventTypeStats, PaneReservation, PaneStats, SearchLint, SearchLintSeverity,
    SearchSuggestion, TableStats,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
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
        "[a-z_]{3,15}",
        arb_db_check_status(),
        proptest::option::of("[a-z ]{5,30}"),
    )
        .prop_map(|(name, status, detail)| DbCheckItem {
            name,
            status,
            detail,
        })
}

fn arb_search_lint_severity() -> impl Strategy<Value = SearchLintSeverity> {
    prop_oneof![
        Just(SearchLintSeverity::Error),
        Just(SearchLintSeverity::Warning),
    ]
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
    fn prop_db_check_status_snake_case(status in arb_db_check_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let expected = match status {
            DbCheckStatus::Ok => "\"ok\"",
            DbCheckStatus::Warning => "\"warning\"",
            DbCheckStatus::Error => "\"error\"",
        };
        prop_assert_eq!(&json, expected);
    }

    #[test]
    fn prop_db_check_status_display(status in arb_db_check_status()) {
        let display = status.to_string();
        let expected = match status {
            DbCheckStatus::Ok => "OK",
            DbCheckStatus::Warning => "WARNING",
            DbCheckStatus::Error => "ERROR",
        };
        prop_assert_eq!(&display, expected);
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
        db_exists in any::<bool>(),
        has_size in any::<bool>(),
        has_version in any::<bool>(),
        checks in proptest::collection::vec(arb_db_check_item(), 0..10),
    ) {
        let report = DbCheckReport {
            db_path: "/tmp/test.db".to_string(),
            db_exists,
            db_size_bytes: if has_size { Some(1024) } else { None },
            schema_version: if has_version { Some(23) } else { None },
            checks,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: DbCheckReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.db_path, &report.db_path);
        prop_assert_eq!(back.db_exists, report.db_exists);
        prop_assert_eq!(back.db_size_bytes, report.db_size_bytes);
        prop_assert_eq!(back.schema_version, report.schema_version);
        prop_assert_eq!(back.checks.len(), report.checks.len());
    }

    /// has_errors() true iff at least one check has Error status.
    #[test]
    fn prop_db_check_report_has_errors(
        checks in proptest::collection::vec(arb_db_check_item(), 0..10),
    ) {
        let report = DbCheckReport {
            db_path: String::new(),
            db_exists: true,
            db_size_bytes: None,
            schema_version: None,
            checks: checks.clone(),
        };
        let expected = checks.iter().any(|c| c.status == DbCheckStatus::Error);
        prop_assert_eq!(report.has_errors(), expected);
    }

    /// has_warnings() true iff at least one check has Warning status.
    #[test]
    fn prop_db_check_report_has_warnings(
        checks in proptest::collection::vec(arb_db_check_item(), 0..10),
    ) {
        let report = DbCheckReport {
            db_path: String::new(),
            db_exists: true,
            db_size_bytes: None,
            schema_version: None,
            checks: checks.clone(),
        };
        let expected = checks.iter().any(|c| c.status == DbCheckStatus::Warning);
        prop_assert_eq!(report.has_warnings(), expected);
    }

    /// problem_count == errors + warnings.
    #[test]
    fn prop_db_check_report_problem_count(
        checks in proptest::collection::vec(arb_db_check_item(), 0..10),
    ) {
        let report = DbCheckReport {
            db_path: String::new(),
            db_exists: true,
            db_size_bytes: None,
            schema_version: None,
            checks: checks.clone(),
        };
        let expected = checks.iter().filter(|c| c.status != DbCheckStatus::Ok).count();
        prop_assert_eq!(report.problem_count(), expected);
    }

    /// problem_count + ok_count == checks.len().
    #[test]
    fn prop_db_check_report_counts_partition(
        checks in proptest::collection::vec(arb_db_check_item(), 0..10),
    ) {
        let report = DbCheckReport {
            db_path: String::new(),
            db_exists: true,
            db_size_bytes: None,
            schema_version: None,
            checks: checks.clone(),
        };
        let ok_count = checks.iter().filter(|c| c.status == DbCheckStatus::Ok).count();
        prop_assert_eq!(report.problem_count() + ok_count, checks.len());
    }
}

// =========================================================================
// DbRepairItem + DbRepairReport — serde + all_succeeded
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_repair_item_serde(
        name in "[a-z_]{3,15}",
        success in any::<bool>(),
        detail in "[a-z ]{5,30}",
    ) {
        let item = DbRepairItem { name: name.clone(), success, detail: detail.clone() };
        let json = serde_json::to_string(&item).unwrap();
        let back: DbRepairItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &item.name);
        prop_assert_eq!(back.success, item.success);
        prop_assert_eq!(&back.detail, &item.detail);
    }

    #[test]
    fn prop_repair_report_all_succeeded(
        repairs in proptest::collection::vec(
            ("[a-z_]{3,10}", any::<bool>(), "[a-z]{5,10}").prop_map(|(name, success, detail)| {
                DbRepairItem { name, success, detail }
            }),
            0..10,
        ),
    ) {
        let report = DbRepairReport {
            backup_path: Some("/tmp/backup.db".into()),
            repairs: repairs.clone(),
        };
        let expected = repairs.iter().all(|r| r.success);
        prop_assert_eq!(report.all_succeeded(), expected);
    }

    /// Empty repair list means all_succeeded() is true.
    #[test]
    fn prop_empty_repair_all_succeeded(_dummy in 0..1_u8) {
        let report = DbRepairReport { backup_path: None, repairs: vec![] };
        prop_assert!(report.all_succeeded());
    }
}

// =========================================================================
// TableStats, PaneStats, EventTypeStats, DbStatsReport — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_table_stats_serde(
        name in "[a-z_]{3,15}",
        row_count in 0_u64..1_000_000,
    ) {
        let stats = TableStats { name: name.clone(), row_count };
        let json = serde_json::to_string(&stats).unwrap();
        let back: TableStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &stats.name);
        prop_assert_eq!(back.row_count, stats.row_count);
    }

    #[test]
    fn prop_pane_stats_serde(
        pane_id in 0_u64..100,
        has_title in any::<bool>(),
        segment_count in 0_u64..100_000,
        segment_bytes in 0_u64..100_000_000,
        event_count in 0_u64..10_000,
    ) {
        let stats = PaneStats {
            pane_id,
            title: if has_title { Some("my pane".into()) } else { None },
            segment_count,
            segment_bytes,
            event_count,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: PaneStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, stats.pane_id);
        prop_assert_eq!(&back.title, &stats.title);
        prop_assert_eq!(back.segment_count, stats.segment_count);
    }

    #[test]
    fn prop_event_type_stats_serde(
        event_type in "[a-z_]{3,15}",
        count in 0_u64..100_000,
    ) {
        let stats = EventTypeStats { event_type: event_type.clone(), count };
        let json = serde_json::to_string(&stats).unwrap();
        let back: EventTypeStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.event_type, &stats.event_type);
        prop_assert_eq!(back.count, stats.count);
    }

    #[test]
    fn prop_db_stats_report_serde(
        table_count in 0_usize..5,
        pane_count in 0_usize..3,
        event_type_count in 0_usize..5,
        suggestion_count in 0_usize..3,
    ) {
        let report = DbStatsReport {
            db_path: "/tmp/test.db".to_string(),
            db_size_bytes: Some(1024 * 1024),
            tables: (0..table_count)
                .map(|i| TableStats { name: format!("table_{i}"), row_count: i as u64 * 100 })
                .collect(),
            top_panes: (0..pane_count)
                .map(|i| PaneStats {
                    pane_id: i as u64,
                    title: None,
                    segment_count: 100,
                    segment_bytes: 10_000,
                    event_count: 50,
                })
                .collect(),
            event_types: (0..event_type_count)
                .map(|i| EventTypeStats { event_type: format!("type_{i}"), count: i as u64 })
                .collect(),
            suggestions: (0..suggestion_count)
                .map(|i| format!("suggestion_{i}"))
                .collect(),
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: DbStatsReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.db_path, &report.db_path);
        prop_assert_eq!(back.tables.len(), report.tables.len());
        prop_assert_eq!(back.top_panes.len(), report.top_panes.len());
        prop_assert_eq!(back.event_types.len(), report.event_types.len());
        prop_assert_eq!(back.suggestions.len(), report.suggestions.len());
    }
}

// =========================================================================
// SearchLintSeverity + SearchLint + SearchSuggestion — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_search_lint_severity_serde(severity in arb_search_lint_severity()) {
        let json = serde_json::to_string(&severity).unwrap();
        let back: SearchLintSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, severity);
    }

    #[test]
    fn prop_search_lint_severity_snake_case(severity in arb_search_lint_severity()) {
        let json = serde_json::to_string(&severity).unwrap();
        let expected = match severity {
            SearchLintSeverity::Error => "\"error\"",
            SearchLintSeverity::Warning => "\"warning\"",
        };
        prop_assert_eq!(&json, expected);
    }

    #[test]
    fn prop_search_lint_serde(
        code in "[A-Z]{2}[0-9]{3}",
        severity in arb_search_lint_severity(),
        message in "[a-z ]{5,30}",
        has_suggestion in any::<bool>(),
    ) {
        let lint = SearchLint {
            code: code.clone(),
            severity,
            message: message.clone(),
            suggestion: if has_suggestion { Some("try this".into()) } else { None },
        };
        let json = serde_json::to_string(&lint).unwrap();
        let back: SearchLint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.code, &lint.code);
        prop_assert_eq!(back.severity, lint.severity);
        prop_assert_eq!(&back.message, &lint.message);
        prop_assert_eq!(&back.suggestion, &lint.suggestion);
    }

    /// SearchLint with None suggestion omits the field (skip_serializing_if).
    #[test]
    fn prop_search_lint_skip_none_suggestion(_dummy in 0..1_u8) {
        let lint = SearchLint {
            code: "QL001".into(),
            severity: SearchLintSeverity::Warning,
            message: "test".into(),
            suggestion: None,
        };
        let json = serde_json::to_string(&lint).unwrap();
        let has_suggestion = json.contains("\"suggestion\"");
        prop_assert!(!has_suggestion, "None suggestion should be omitted from JSON");
    }

    #[test]
    fn prop_search_suggestion_serde(
        text in "[a-z ]{3,20}",
        has_desc in any::<bool>(),
    ) {
        let suggestion = SearchSuggestion {
            text: text.clone(),
            description: if has_desc { Some("describes it".into()) } else { None },
        };
        let json = serde_json::to_string(&suggestion).unwrap();
        let back: SearchSuggestion = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.text, &suggestion.text);
        prop_assert_eq!(&back.description, &suggestion.description);
    }

    /// SearchSuggestion with None description omits the field.
    #[test]
    fn prop_search_suggestion_skip_none_description(_dummy in 0..1_u8) {
        let suggestion = SearchSuggestion {
            text: "test".into(),
            description: None,
        };
        let json = serde_json::to_string(&suggestion).unwrap();
        let has_desc = json.contains("\"description\"");
        prop_assert!(!has_desc, "None description should be omitted from JSON");
    }
}

// =========================================================================
// ApprovalTokenRecord — serde + is_active()
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_approval_token_serde(
        id in 0_i64..100_000,
        code_hash in "[a-f0-9]{64}",
        created_at in 1_000_000_000_000_i64..1_500_000_000_000,
        expires_at in 1_500_000_000_000_i64..2_000_000_000_000,
        has_used in any::<bool>(),
    ) {
        let record = ApprovalTokenRecord {
            id,
            code_hash: code_hash.clone(),
            created_at,
            expires_at,
            used_at: if has_used { Some(created_at + 1000) } else { None },
            workspace_id: "default".into(),
            action_kind: "send_text".into(),
            pane_id: Some(42),
            action_fingerprint: "fp123".into(),
            plan_hash: None,
            plan_version: None,
            risk_summary: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: ApprovalTokenRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, record.id);
        prop_assert_eq!(&back.code_hash, &record.code_hash);
        prop_assert_eq!(back.expires_at, record.expires_at);
        prop_assert_eq!(back.used_at, record.used_at);
    }

    /// Unused + unexpired token is active.
    #[test]
    fn prop_approval_active_when_unused_unexpired(
        expires_at in 2_000_000_000_000_i64..3_000_000_000_000,
        now_ms in 1_000_000_000_000_i64..2_000_000_000_000,
    ) {
        let record = ApprovalTokenRecord {
            id: 1,
            code_hash: "abc".into(),
            created_at: 1_000_000_000_000,
            expires_at,
            used_at: None,
            workspace_id: "ws".into(),
            action_kind: "act".into(),
            pane_id: None,
            action_fingerprint: "fp".into(),
            plan_hash: None,
            plan_version: None,
            risk_summary: None,
        };
        prop_assert!(record.is_active(now_ms));
    }

    /// Used token is not active regardless of expiry.
    #[test]
    fn prop_approval_inactive_when_used(
        expires_at in 2_000_000_000_000_i64..3_000_000_000_000,
        now_ms in 1_000_000_000_000_i64..2_000_000_000_000,
    ) {
        let record = ApprovalTokenRecord {
            id: 1,
            code_hash: "abc".into(),
            created_at: 1_000_000_000_000,
            expires_at,
            used_at: Some(1_500_000_000_000),
            workspace_id: "ws".into(),
            action_kind: "act".into(),
            pane_id: None,
            action_fingerprint: "fp".into(),
            plan_hash: None,
            plan_version: None,
            risk_summary: None,
        };
        prop_assert!(!record.is_active(now_ms));
    }

    /// Expired token is not active even if unused.
    #[test]
    fn prop_approval_inactive_when_expired(
        expires_at in 1_000_000_000_000_i64..1_500_000_000_000,
        now_ms in 1_500_000_000_001_i64..2_000_000_000_000,
    ) {
        let record = ApprovalTokenRecord {
            id: 1,
            code_hash: "abc".into(),
            created_at: 900_000_000_000,
            expires_at,
            used_at: None,
            workspace_id: "ws".into(),
            action_kind: "act".into(),
            pane_id: None,
            action_fingerprint: "fp".into(),
            plan_hash: None,
            plan_version: None,
            risk_summary: None,
        };
        prop_assert!(!record.is_active(now_ms));
    }
}

// =========================================================================
// PaneReservation — serde + is_active()
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_reservation_serde(
        id in 0_i64..100_000,
        pane_id in 0_u64..100,
        owner_kind in prop_oneof![Just("workflow".to_string()), Just("agent".to_string())],
        owner_id in "[a-z_]{3,15}",
        created_at in 1_000_000_000_000_i64..1_500_000_000_000,
        expires_at in 1_500_000_000_000_i64..2_000_000_000_000,
    ) {
        let reservation = PaneReservation {
            id, pane_id,
            owner_kind: owner_kind.clone(),
            owner_id: owner_id.clone(),
            reason: Some("testing".into()),
            created_at, expires_at,
            released_at: None,
            status: "active".into(),
        };
        let json = serde_json::to_string(&reservation).unwrap();
        let back: PaneReservation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, reservation.id);
        prop_assert_eq!(back.pane_id, reservation.pane_id);
        prop_assert_eq!(&back.owner_kind, &reservation.owner_kind);
        prop_assert_eq!(&back.owner_id, &reservation.owner_id);
        prop_assert_eq!(back.expires_at, reservation.expires_at);
    }

    /// Active reservation with status="active", no release, unexpired.
    #[test]
    fn prop_reservation_active(
        expires_at in 2_000_000_000_000_i64..3_000_000_000_000,
        now_ms in 1_000_000_000_000_i64..2_000_000_000_000,
    ) {
        let r = PaneReservation {
            id: 1, pane_id: 42,
            owner_kind: "workflow".into(),
            owner_id: "wf1".into(),
            reason: None,
            created_at: 1_000_000_000_000,
            expires_at,
            released_at: None,
            status: "active".into(),
        };
        prop_assert!(r.is_active(now_ms));
    }

    /// Released reservation is not active.
    #[test]
    fn prop_reservation_released_inactive(
        expires_at in 2_000_000_000_000_i64..3_000_000_000_000,
        now_ms in 1_000_000_000_000_i64..2_000_000_000_000,
    ) {
        let r = PaneReservation {
            id: 1, pane_id: 42,
            owner_kind: "workflow".into(),
            owner_id: "wf1".into(),
            reason: None,
            created_at: 1_000_000_000_000,
            expires_at,
            released_at: Some(1_200_000_000_000),
            status: "active".into(),
        };
        prop_assert!(!r.is_active(now_ms));
    }

    /// Expired reservation is not active.
    #[test]
    fn prop_reservation_expired_inactive(
        expires_at in 1_000_000_000_000_i64..1_500_000_000_000,
        now_ms in 1_500_000_000_000_i64..2_000_000_000_000,
    ) {
        let r = PaneReservation {
            id: 1, pane_id: 42,
            owner_kind: "workflow".into(),
            owner_id: "wf1".into(),
            reason: None,
            created_at: 900_000_000_000,
            expires_at,
            released_at: None,
            status: "active".into(),
        };
        prop_assert!(!r.is_active(now_ms));
    }

    /// Non-"active" status is not active.
    #[test]
    fn prop_reservation_status_not_active(
        expires_at in 2_000_000_000_000_i64..3_000_000_000_000,
        now_ms in 1_000_000_000_000_i64..2_000_000_000_000,
    ) {
        let r = PaneReservation {
            id: 1, pane_id: 42,
            owner_kind: "workflow".into(),
            owner_id: "wf1".into(),
            reason: None,
            created_at: 1_000_000_000_000,
            expires_at,
            released_at: None,
            status: "released".into(),
        };
        prop_assert!(!r.is_active(now_ms));
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn all_db_check_statuses_distinct_json() {
    let statuses = [DbCheckStatus::Ok, DbCheckStatus::Warning, DbCheckStatus::Error];
    let jsons: Vec<_> = statuses.iter().map(|s| serde_json::to_string(s).unwrap()).collect();
    for i in 0..jsons.len() {
        for j in (i + 1)..jsons.len() {
            assert_ne!(jsons[i], jsons[j]);
        }
    }
}

#[test]
fn search_lint_severities_distinct_json() {
    let a = serde_json::to_string(&SearchLintSeverity::Error).unwrap();
    let b = serde_json::to_string(&SearchLintSeverity::Warning).unwrap();
    assert_ne!(a, b);
}

#[test]
fn approval_token_boundary_exact_expiry() {
    let record = ApprovalTokenRecord {
        id: 1,
        code_hash: "abc".into(),
        created_at: 1_000_000_000_000,
        expires_at: 1_500_000_000_000,
        used_at: None,
        workspace_id: "ws".into(),
        action_kind: "act".into(),
        pane_id: None,
        action_fingerprint: "fp".into(),
        plan_hash: None,
        plan_version: None,
        risk_summary: None,
    };
    // At exact expiry time: is_active checks expires_at >= now_ms
    assert!(record.is_active(1_500_000_000_000));
    // One ms after: not active
    assert!(!record.is_active(1_500_000_000_001));
}

#[test]
fn pane_reservation_boundary_exact_expiry() {
    let r = PaneReservation {
        id: 1, pane_id: 42,
        owner_kind: "workflow".into(),
        owner_id: "wf1".into(),
        reason: None,
        created_at: 1_000_000_000_000,
        expires_at: 1_500_000_000_000,
        released_at: None,
        status: "active".into(),
    };
    // At exact expiry: is_active checks expires_at > now_ms (strict)
    assert!(!r.is_active(1_500_000_000_000));
    // One ms before: active
    assert!(r.is_active(1_499_999_999_999));
}
