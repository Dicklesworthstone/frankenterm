//! Property-based tests for the `session_correlation` module.
//!
//! Covers `correlate_from_sessions` invariants (confidence bounds, window logic,
//! tie-breaking, override bypass), serde roundtrips for correlation types,
//! and `SessionCorrelation::to_external_meta` structural properties.

use frankenterm_core::cass::CassSession;
use frankenterm_core::session_correlation::{
    CASS_CORRELATION_VERSION, CassCorrelationOptions, CorrelationStatus, SessionCorrelation,
    correlate_from_sessions,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_correlation_options() -> impl Strategy<Value = CassCorrelationOptions> {
    (
        0_i64..3_600_000,
        0_i64..3_600_000,
        proptest::option::of("[a-z0-9-]{5,20}"),
    )
        .prop_map(|(before, after, override_id)| CassCorrelationOptions {
            window_before_ms: before,
            window_after_ms: after,
            override_session_id: override_id,
        })
}

fn arb_correlation_status() -> impl Strategy<Value = CorrelationStatus> {
    prop_oneof![
        Just(CorrelationStatus::Linked),
        Just(CorrelationStatus::Unlinked),
        Just(CorrelationStatus::Error),
    ]
}

/// Build a CassSession with a timestamp at `base_ms + offset_ms`.
///
/// Uses raw integer timestamps (ms since epoch), which `parse_cass_timestamp_ms`
/// accepts directly when the value is > 10_000_000_000.
fn make_session_at_offset(id: &str, base_ms: i64, offset_ms: i64) -> CassSession {
    let ts_ms = base_ms.saturating_add(offset_ms);
    CassSession {
        session_id: Some(id.to_string()),
        started_at: Some(ts_ms.to_string()),
        ..Default::default()
    }
}

// =========================================================================
// correlate_from_sessions — confidence bounds
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Confidence is always in [0.0, 0.95] for non-override correlations.
    #[test]
    fn prop_confidence_bounded(
        n_sessions in 0_usize..8,
        offsets in proptest::collection::vec(-600_000_i64..600_000, 0..8),
    ) {
        let base_ms = 1_700_000_000_000_i64;
        let sessions: Vec<CassSession> = offsets.iter().take(n_sessions).enumerate()
            .map(|(i, &off)| make_session_at_offset(&format!("s-{i}"), base_ms, off))
            .collect();

        let opts = CassCorrelationOptions::default();
        let result = correlate_from_sessions(&sessions, base_ms, &opts);
        prop_assert!(result.confidence >= 0.0, "confidence {} < 0", result.confidence);
        prop_assert!(result.confidence <= 0.95, "confidence {} > 0.95", result.confidence);
    }

    /// Manual override always yields confidence == 1.0 and Linked status.
    #[test]
    fn prop_override_always_linked(
        override_id in "[a-z0-9-]{5,20}",
        base_ms in 1_000_000_i64..100_000_000,
    ) {
        let opts = CassCorrelationOptions {
            override_session_id: Some(override_id.clone()),
            ..Default::default()
        };
        let result = correlate_from_sessions(&[], base_ms, &opts);
        prop_assert_eq!(result.status, CorrelationStatus::Linked);
        prop_assert_eq!(result.external_id.as_deref(), Some(override_id.as_str()));
        prop_assert!((result.confidence - 1.0).abs() < f64::EPSILON);
    }
}

// =========================================================================
// correlate_from_sessions — window filtering
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// Sessions outside the window always produce Unlinked.
    #[test]
    fn prop_outside_window_unlinked(
        window_ms in 1_000_i64..60_000,
        outside_offset in 61_000_i64..600_000,
    ) {
        let base_ms = 1_700_000_000_000_i64;
        let sessions = vec![
            make_session_at_offset("far-before", base_ms, -outside_offset),
            make_session_at_offset("far-after", base_ms, outside_offset),
        ];
        let opts = CassCorrelationOptions {
            window_before_ms: window_ms,
            window_after_ms: window_ms,
            override_session_id: None,
        };
        let result = correlate_from_sessions(&sessions, base_ms, &opts);
        prop_assert_eq!(result.status, CorrelationStatus::Unlinked);
    }

    /// Empty session list always produces Unlinked with 0 candidates.
    #[test]
    fn prop_empty_sessions_unlinked(base_ms in 0_i64..100_000_000) {
        let result = correlate_from_sessions(&[], base_ms, &CassCorrelationOptions::default());
        prop_assert_eq!(result.status, CorrelationStatus::Unlinked);
        prop_assert_eq!(result.candidates_considered, 0);
        prop_assert!((result.confidence - 0.0).abs() < f64::EPSILON,
            "confidence should be 0.0, got {}", result.confidence);
    }

    /// Sessions missing session_id are skipped (never selected).
    #[test]
    fn prop_missing_id_skipped(base_ms in 1_000_000_i64..100_000_000) {
        let sessions = vec![CassSession {
            session_id: None,
            started_at: Some("2026-01-29T17:00:00Z".to_string()),
            ..Default::default()
        }];
        let result = correlate_from_sessions(&sessions, base_ms, &CassCorrelationOptions::default());
        prop_assert_eq!(result.status, CorrelationStatus::Unlinked);
    }
}

// =========================================================================
// correlate_from_sessions — selection invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// The selected session (if Linked) is always closest to the base start time.
    #[test]
    fn prop_closest_candidate_selected(
        off1 in 1_000_i64..300_000,
        off2 in 300_001_i64..600_000,
    ) {
        let base_ms = 1_700_000_000_000_i64;
        let sessions = vec![
            make_session_at_offset("close", base_ms, off1),
            make_session_at_offset("far", base_ms, off2),
        ];
        let opts = CassCorrelationOptions {
            window_before_ms: 700_000,
            window_after_ms: 700_000,
            override_session_id: None,
        };
        let result = correlate_from_sessions(&sessions, base_ms, &opts);
        prop_assert_eq!(result.status, CorrelationStatus::Linked);
        prop_assert_eq!(result.external_id.as_deref(), Some("close"));
    }

    /// Algorithm version is always CASS_CORRELATION_VERSION.
    #[test]
    fn prop_algorithm_version_constant(
        base_ms in 1_000_000_i64..100_000_000,
        n_sessions in 0_usize..5,
    ) {
        let sessions: Vec<CassSession> = (0..n_sessions)
            .map(|i| make_session_at_offset(&format!("s-{i}"), base_ms, (i as i64) * 1000))
            .collect();
        let opts = CassCorrelationOptions::default();
        let result = correlate_from_sessions(&sessions, base_ms, &opts);
        prop_assert_eq!(result.algorithm_version.as_str(), CASS_CORRELATION_VERSION);
    }

    /// Window bounds are always reported in the result.
    #[test]
    fn prop_window_bounds_reported(
        base_ms in 1_000_000_i64..100_000_000,
        before_ms in 0_i64..600_000,
        after_ms in 0_i64..600_000,
    ) {
        let opts = CassCorrelationOptions {
            window_before_ms: before_ms,
            window_after_ms: after_ms,
            override_session_id: None,
        };
        let result = correlate_from_sessions(&[], base_ms, &opts);
        prop_assert_eq!(result.window_start_ms, base_ms - before_ms);
        prop_assert_eq!(result.window_end_ms, base_ms + after_ms);
    }
}

// =========================================================================
// correlate_from_sessions — determinism
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Same inputs → same output (determinism).
    #[test]
    fn prop_correlate_deterministic(
        base_ms in 1_000_000_i64..100_000_000,
        offsets in proptest::collection::vec(-300_000_i64..300_000, 0..6),
    ) {
        let sessions: Vec<CassSession> = offsets.iter().enumerate()
            .map(|(i, &off)| make_session_at_offset(&format!("s-{i}"), base_ms, off))
            .collect();
        let opts = CassCorrelationOptions::default();

        let r1 = correlate_from_sessions(&sessions, base_ms, &opts);
        let r2 = correlate_from_sessions(&sessions, base_ms, &opts);

        prop_assert_eq!(r1.status, r2.status);
        prop_assert_eq!(r1.external_id, r2.external_id);
        prop_assert!((r1.confidence - r2.confidence).abs() < f64::EPSILON);
        prop_assert_eq!(r1.candidates_considered, r2.candidates_considered);
    }
}

// =========================================================================
// Serde roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// CassCorrelationOptions serde roundtrip.
    #[test]
    fn prop_correlation_options_serde(opts in arb_correlation_options()) {
        let json = serde_json::to_string(&opts).unwrap();
        let back: CassCorrelationOptions = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.window_before_ms, opts.window_before_ms);
        prop_assert_eq!(back.window_after_ms, opts.window_after_ms);
        prop_assert_eq!(back.override_session_id, opts.override_session_id);
    }

    /// CorrelationStatus serde roundtrip.
    #[test]
    fn prop_correlation_status_serde(status in arb_correlation_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let back: CorrelationStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, status);
    }

    /// SessionCorrelation serde roundtrip preserves key fields.
    #[test]
    fn prop_session_correlation_serde_roundtrip(
        status in arb_correlation_status(),
        ext_id in proptest::option::of("[a-z0-9-]{5,15}"),
        confidence in 0.0_f64..1.0,
        candidates in 0_usize..20,
        window_start in 0_i64..100_000_000,
        window_end in 0_i64..100_000_000,
    ) {
        let corr = SessionCorrelation {
            status,
            external_id: ext_id.clone(),
            confidence,
            reasons: vec!["test_reason".to_string()],
            candidates_considered: candidates,
            window_start_ms: window_start,
            window_end_ms: window_end,
            selected_started_at_ms: None,
            algorithm_version: CASS_CORRELATION_VERSION.to_string(),
            error: None,
        };
        let json = serde_json::to_string(&corr).unwrap();
        let back: SessionCorrelation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.status, status);
        prop_assert_eq!(back.external_id, ext_id);
        prop_assert!((back.confidence - confidence).abs() < f64::EPSILON);
        prop_assert_eq!(back.candidates_considered, candidates);
        prop_assert_eq!(back.window_start_ms, window_start);
        prop_assert_eq!(back.window_end_ms, window_end);
    }

    /// CorrelationStatus serializes to snake_case strings.
    #[test]
    fn prop_status_snake_case(status in arb_correlation_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let expected = match status {
            CorrelationStatus::Linked => "\"linked\"",
            CorrelationStatus::Unlinked => "\"unlinked\"",
            CorrelationStatus::Error => "\"error\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }
}

// =========================================================================
// to_external_meta
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// to_external_meta always produces a valid JSON object with status field.
    #[test]
    fn prop_to_external_meta_is_object(
        ext_id in proptest::option::of("[a-z0-9]{5,10}"),
        confidence in 0.0_f64..1.0,
    ) {
        let corr = SessionCorrelation {
            status: CorrelationStatus::Linked,
            external_id: ext_id,
            confidence,
            reasons: vec![],
            candidates_considered: 0,
            window_start_ms: 0,
            window_end_ms: 0,
            selected_started_at_ms: None,
            algorithm_version: CASS_CORRELATION_VERSION.to_string(),
            error: None,
        };
        let meta = corr.to_external_meta();
        prop_assert!(meta.is_object(), "meta should be a JSON object");
        prop_assert!(meta.get("status").is_some(), "meta should have status field");
        prop_assert!(meta.get("algorithm_version").is_some(), "meta should have algorithm_version");
    }
}

// =========================================================================
// Unit tests for edge cases
// =========================================================================

#[test]
fn correlation_status_is_exhaustive() {
    let statuses = [
        CorrelationStatus::Linked,
        CorrelationStatus::Unlinked,
        CorrelationStatus::Error,
    ];
    for s in statuses {
        let json = serde_json::to_string(&s).unwrap();
        let back: CorrelationStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}

#[test]
fn default_options_have_symmetric_windows() {
    let opts = CassCorrelationOptions::default();
    assert_eq!(opts.window_before_ms, opts.window_after_ms);
    assert!(opts.override_session_id.is_none());
}

#[test]
fn linked_result_has_external_id() {
    let base_ms = 1_700_000_000_000_i64;
    let sessions = vec![make_session_at_offset("test-id", base_ms, 1000)];
    let result = correlate_from_sessions(&sessions, base_ms, &CassCorrelationOptions::default());
    assert_eq!(result.status, CorrelationStatus::Linked);
    assert!(result.external_id.is_some());
}

#[test]
fn unlinked_result_has_no_external_id() {
    let result = correlate_from_sessions(&[], 1_000_000, &CassCorrelationOptions::default());
    assert_eq!(result.status, CorrelationStatus::Unlinked);
    assert!(result.external_id.is_none());
    assert!(
        (result.confidence - 0.0).abs() < f64::EPSILON,
        "confidence should be 0.0, got {}",
        result.confidence
    );
}

// =========================================================================
// NEW: CassCorrelationOptions Clone preserves
// =========================================================================

proptest! {
    #[test]
    fn correlation_options_clone_preserves(opts in arb_correlation_options()) {
        let cloned = opts.clone();
        prop_assert_eq!(cloned.window_before_ms, opts.window_before_ms);
        prop_assert_eq!(cloned.window_after_ms, opts.window_after_ms);
        prop_assert_eq!(cloned.override_session_id, opts.override_session_id);
    }
}

// =========================================================================
// NEW: CassCorrelationOptions Debug non-empty
// =========================================================================

proptest! {
    #[test]
    fn correlation_options_debug_nonempty(opts in arb_correlation_options()) {
        let dbg = format!("{:?}", opts);
        prop_assert!(!dbg.is_empty());
        prop_assert!(dbg.contains("CassCorrelationOptions"));
    }
}

// =========================================================================
// NEW: CorrelationStatus Copy/PartialEq
// =========================================================================

proptest! {
    #[test]
    fn correlation_status_copy_eq(status in arb_correlation_status()) {
        let copied = status;
        prop_assert_eq!(status, copied);
    }
}

// =========================================================================
// NEW: CorrelationStatus Debug non-empty
// =========================================================================

proptest! {
    #[test]
    fn correlation_status_debug_nonempty(status in arb_correlation_status()) {
        let dbg = format!("{:?}", status);
        prop_assert!(!dbg.is_empty());
    }
}

// =========================================================================
// NEW: SessionCorrelation Clone preserves key fields
// =========================================================================

proptest! {
    #[test]
    fn session_correlation_clone_preserves(
        status in arb_correlation_status(),
        ext_id in proptest::option::of("[a-z0-9-]{5,15}"),
        confidence in 0.0_f64..1.0,
        candidates in 0_usize..20,
    ) {
        let corr = SessionCorrelation {
            status,
            external_id: ext_id.clone(),
            confidence,
            reasons: vec!["reason".to_string()],
            candidates_considered: candidates,
            window_start_ms: 0,
            window_end_ms: 100_000,
            selected_started_at_ms: None,
            algorithm_version: CASS_CORRELATION_VERSION.to_string(),
            error: None,
        };
        let cloned = corr.clone();
        prop_assert_eq!(cloned.status, status);
        prop_assert_eq!(cloned.external_id, ext_id);
        prop_assert!((cloned.confidence - confidence).abs() < f64::EPSILON);
        prop_assert_eq!(cloned.candidates_considered, candidates);
    }
}

// =========================================================================
// NEW: SessionCorrelation Debug non-empty
// =========================================================================

proptest! {
    #[test]
    fn session_correlation_debug_nonempty(status in arb_correlation_status()) {
        let corr = SessionCorrelation {
            status,
            external_id: None,
            confidence: 0.0,
            reasons: vec![],
            candidates_considered: 0,
            window_start_ms: 0,
            window_end_ms: 0,
            selected_started_at_ms: None,
            algorithm_version: CASS_CORRELATION_VERSION.to_string(),
            error: None,
        };
        let dbg = format!("{:?}", corr);
        prop_assert!(!dbg.is_empty());
        prop_assert!(dbg.contains("SessionCorrelation"));
    }
}

// =========================================================================
// NEW: CassCorrelationOptions serde deterministic
// =========================================================================

proptest! {
    #[test]
    fn correlation_options_serde_deterministic(opts in arb_correlation_options()) {
        let j1 = serde_json::to_string(&opts).unwrap();
        let j2 = serde_json::to_string(&opts).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// NEW: SessionCorrelation serde deterministic
// =========================================================================

proptest! {
    #[test]
    fn session_correlation_serde_deterministic(
        status in arb_correlation_status(),
        confidence in 0.0_f64..1.0,
    ) {
        let corr = SessionCorrelation {
            status,
            external_id: None,
            confidence,
            reasons: vec![],
            candidates_considered: 0,
            window_start_ms: 0,
            window_end_ms: 100_000,
            selected_started_at_ms: None,
            algorithm_version: CASS_CORRELATION_VERSION.to_string(),
            error: None,
        };
        let j1 = serde_json::to_string(&corr).unwrap();
        let j2 = serde_json::to_string(&corr).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// NEW: Candidates considered <= session count
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn candidates_considered_le_sessions(
        n_sessions in 0_usize..8,
        offsets in proptest::collection::vec(-300_000_i64..300_000, 0..8),
    ) {
        let base_ms = 1_700_000_000_000_i64;
        let sessions: Vec<CassSession> = offsets.iter().take(n_sessions).enumerate()
            .map(|(i, &off)| make_session_at_offset(&format!("s-{i}"), base_ms, off))
            .collect();
        let opts = CassCorrelationOptions::default();
        let result = correlate_from_sessions(&sessions, base_ms, &opts);
        prop_assert!(result.candidates_considered <= sessions.len(),
            "candidates {} > sessions {}", result.candidates_considered, sessions.len());
    }
}

// =========================================================================
// NEW: to_external_meta always has confidence field
// =========================================================================

proptest! {
    #[test]
    fn to_external_meta_has_confidence(confidence in 0.0_f64..1.0) {
        let corr = SessionCorrelation {
            status: CorrelationStatus::Linked,
            external_id: Some("test".to_string()),
            confidence,
            reasons: vec![],
            candidates_considered: 1,
            window_start_ms: 0,
            window_end_ms: 100_000,
            selected_started_at_ms: None,
            algorithm_version: CASS_CORRELATION_VERSION.to_string(),
            error: None,
        };
        let meta = corr.to_external_meta();
        prop_assert!(meta.get("confidence").is_some());
    }
}

// =========================================================================
// NEW: Default options override_session_id is None
// =========================================================================

proptest! {
    #[test]
    fn default_options_no_override(_dummy in 0..1u8) {
        let opts = CassCorrelationOptions::default();
        prop_assert!(opts.override_session_id.is_none());
        prop_assert!(opts.window_before_ms > 0);
        prop_assert!(opts.window_after_ms > 0);
    }

    /// CorrelationStatus Debug is non-empty.
    #[test]
    fn prop_correlation_status_debug(_dummy in 0..1u8) {
        let statuses = [
            CorrelationStatus::Linked,
            CorrelationStatus::Unlinked,
            CorrelationStatus::Error,
        ];
        for s in &statuses {
            let debug = format!("{:?}", s);
            prop_assert!(!debug.is_empty());
        }
    }
}

// =========================================================================
// Batch 14: additional property tests (DarkMill)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// CASS_CORRELATION_VERSION is a non-empty constant.
    #[test]
    fn version_constant_nonempty(_dummy in 0..1u8) {
        prop_assert!(!CASS_CORRELATION_VERSION.is_empty());
    }

    /// SessionCorrelation with Linked status always has external_id.
    #[test]
    fn linked_always_has_external_id(confidence in 0.0_f64..1.0) {
        let corr = SessionCorrelation {
            status: CorrelationStatus::Linked,
            external_id: Some("session-123".to_string()),
            confidence,
            reasons: vec!["closest match".to_string()],
            candidates_considered: 5,
            window_start_ms: 0,
            window_end_ms: 60_000,
            selected_started_at_ms: Some(30_000),
            algorithm_version: CASS_CORRELATION_VERSION.to_string(),
            error: None,
        };
        prop_assert!(corr.external_id.is_some());
        prop_assert_eq!(corr.status, CorrelationStatus::Linked);
    }

    /// to_external_meta always has algorithm_version field.
    #[test]
    fn meta_has_algorithm_version(confidence in 0.0_f64..1.0) {
        let corr = SessionCorrelation {
            status: CorrelationStatus::Linked,
            external_id: Some("s1".to_string()),
            confidence,
            reasons: vec![],
            candidates_considered: 1,
            window_start_ms: 0,
            window_end_ms: 100_000,
            selected_started_at_ms: None,
            algorithm_version: CASS_CORRELATION_VERSION.to_string(),
            error: None,
        };
        let meta = corr.to_external_meta();
        prop_assert!(meta.get("algorithm_version").is_some());
    }

    /// CassCorrelationOptions serde roundtrip.
    #[test]
    fn options_serde_roundtrip(opts in arb_correlation_options()) {
        let json = serde_json::to_string(&opts).unwrap();
        let parsed: CassCorrelationOptions = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.window_before_ms, opts.window_before_ms);
        prop_assert_eq!(parsed.window_after_ms, opts.window_after_ms);
        prop_assert_eq!(parsed.override_session_id, opts.override_session_id);
    }

    /// CorrelationStatus serde roundtrip for all variants.
    #[test]
    fn status_serde_roundtrip_ext(_dummy in 0..1u8) {
        for status in &[CorrelationStatus::Linked, CorrelationStatus::Unlinked, CorrelationStatus::Error] {
            let json = serde_json::to_string(status).unwrap();
            let parsed: CorrelationStatus = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(&parsed, status);
        }
    }

    /// Error status correlation has None external_id and Some error.
    #[test]
    fn error_status_has_error_field(_dummy in 0..1u8) {
        let corr = SessionCorrelation {
            status: CorrelationStatus::Error,
            external_id: None,
            confidence: 0.0,
            reasons: vec![],
            candidates_considered: 0,
            window_start_ms: 0,
            window_end_ms: 0,
            selected_started_at_ms: None,
            algorithm_version: CASS_CORRELATION_VERSION.to_string(),
            error: Some("something failed".to_string()),
        };
        prop_assert!(corr.error.is_some());
        prop_assert!(corr.external_id.is_none());
    }

    /// Reasons vector can be empty for any status.
    #[test]
    fn empty_reasons_valid(confidence in 0.0_f64..1.0) {
        let corr = SessionCorrelation {
            status: CorrelationStatus::Unlinked,
            external_id: None,
            confidence,
            reasons: vec![],
            candidates_considered: 0,
            window_start_ms: 0,
            window_end_ms: 100_000,
            selected_started_at_ms: None,
            algorithm_version: CASS_CORRELATION_VERSION.to_string(),
            error: None,
        };
        let json = serde_json::to_string(&corr).unwrap();
        prop_assert!(json.contains("\"reasons\":[]"));
    }
}
