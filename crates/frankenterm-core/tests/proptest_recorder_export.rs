//! Property-based tests for the `recorder_export` module.
//!
//! Covers `ExportFormat` serde roundtrips and Display, `ExportRequest` serde
//! roundtrips, builder methods, and `required_tier` access-control logic.

use frankenterm_core::recorder_audit::AccessTier;
use frankenterm_core::recorder_export::{ExportFormat, ExportRequest};
use frankenterm_core::recorder_retention::SensitivityTier;
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_export_format() -> impl Strategy<Value = ExportFormat> {
    prop_oneof![
        Just(ExportFormat::JsonLines),
        Just(ExportFormat::Csv),
        Just(ExportFormat::Transcript),
    ]
}

fn arb_export_request() -> impl Strategy<Value = ExportRequest> {
    (
        arb_export_format(),
        proptest::collection::vec(0_u64..10_000, 0..5),
        0_usize..1000,
        any::<bool>(),
        proptest::option::of("[a-z ]{3,20}"),
    )
        .prop_map(
            |(format, pane_ids, max_events, include_text, label)| ExportRequest {
                format,
                time_range: None,
                pane_ids,
                kind_filter: vec![],
                max_events,
                include_text,
                max_sensitivity: None,
                label,
            },
        )
}

fn arb_sensitivity_tier() -> impl Strategy<Value = SensitivityTier> {
    prop_oneof![
        Just(SensitivityTier::T1Standard),
        Just(SensitivityTier::T2Sensitive),
        Just(SensitivityTier::T3Restricted),
    ]
}

// =========================================================================
// ExportFormat — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// 1. ExportFormat serde roundtrip.
    #[test]
    fn prop_format_serde(fmt in arb_export_format()) {
        let json = serde_json::to_string(&fmt).unwrap();
        let back: ExportFormat = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, fmt);
    }

    /// 2. ExportFormat serializes to snake_case.
    #[test]
    fn prop_format_snake_case(fmt in arb_export_format()) {
        let json = serde_json::to_string(&fmt).unwrap();
        let expected = match fmt {
            ExportFormat::JsonLines => "\"json_lines\"",
            ExportFormat::Csv => "\"csv\"",
            ExportFormat::Transcript => "\"transcript\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }

    /// 3. ExportFormat Display produces non-empty string.
    #[test]
    fn prop_format_display_nonempty(fmt in arb_export_format()) {
        let display = fmt.to_string();
        prop_assert!(!display.is_empty());
    }

    /// 4. ExportFormat serde is deterministic.
    #[test]
    fn prop_format_serde_deterministic(fmt in arb_export_format()) {
        let j1 = serde_json::to_string(&fmt).unwrap();
        let j2 = serde_json::to_string(&fmt).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// ExportRequest — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// 5. ExportRequest serde roundtrip preserves key fields.
    #[test]
    fn prop_request_serde(req in arb_export_request()) {
        let json = serde_json::to_string(&req).unwrap();
        let back: ExportRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.format, req.format);
        prop_assert_eq!(&back.pane_ids, &req.pane_ids);
        prop_assert_eq!(back.max_events, req.max_events);
        prop_assert_eq!(back.include_text, req.include_text);
        prop_assert_eq!(&back.label, &req.label);
    }

    /// 6. Default ExportRequest has expected values.
    #[test]
    fn prop_default_request(_dummy in 0..1_u8) {
        let req = ExportRequest::default();
        prop_assert_eq!(req.format, ExportFormat::JsonLines);
        prop_assert!(req.pane_ids.is_empty());
        prop_assert!(req.kind_filter.is_empty());
        prop_assert_eq!(req.max_events, 0);
        prop_assert!(req.include_text);
        prop_assert!(req.max_sensitivity.is_none());
        prop_assert!(req.label.is_none());
        prop_assert!(req.time_range.is_none());
    }
}

// =========================================================================
// ExportRequest — builder methods
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// 7. jsonl() builder sets format and time_range correctly.
    #[test]
    fn prop_jsonl_builder(start in 0_u64..1_000_000, end in 0_u64..1_000_000) {
        let req = ExportRequest::jsonl(start, end);
        prop_assert_eq!(req.format, ExportFormat::JsonLines);
        let tr = req.time_range.unwrap();
        prop_assert_eq!(tr.start_ms, start);
        prop_assert_eq!(tr.end_ms, end);
        prop_assert!(req.include_text);
    }

    /// 8. csv_for_panes() builder sets format and pane_ids correctly.
    #[test]
    fn prop_csv_builder(pane_ids in proptest::collection::vec(0_u64..10_000, 0..10)) {
        let req = ExportRequest::csv_for_panes(pane_ids.clone());
        prop_assert_eq!(req.format, ExportFormat::Csv);
        prop_assert_eq!(&req.pane_ids, &pane_ids);
        prop_assert!(req.time_range.is_none());
    }

    /// 9. transcript() builder sets format and time_range correctly.
    #[test]
    fn prop_transcript_builder(start in 0_u64..1_000_000, end in 0_u64..1_000_000) {
        let req = ExportRequest::transcript(start, end);
        prop_assert_eq!(req.format, ExportFormat::Transcript);
        let tr = req.time_range.unwrap();
        prop_assert_eq!(tr.start_ms, start);
        prop_assert_eq!(tr.end_ms, end);
    }

    /// 10. with_max_events() builder sets max_events.
    #[test]
    fn prop_with_max_events(max in 0_usize..10_000) {
        let req = ExportRequest::default().with_max_events(max);
        prop_assert_eq!(req.max_events, max);
    }

    /// 11. with_label() builder sets label.
    #[test]
    fn prop_with_label(label in "[a-z ]{3,20}") {
        let req = ExportRequest::default().with_label(label.clone());
        prop_assert_eq!(req.label.as_deref(), Some(label.as_str()));
    }

    /// 12. Builder chaining preserves previous fields.
    #[test]
    fn prop_builder_chaining(start in 0_u64..1_000_000, end in 0_u64..1_000_000, max in 0_usize..1000) {
        let req = ExportRequest::jsonl(start, end)
            .with_max_events(max)
            .with_label("test export");
        prop_assert_eq!(req.format, ExportFormat::JsonLines);
        prop_assert_eq!(req.max_events, max);
        prop_assert_eq!(req.label.as_deref(), Some("test export"));
        let tr = req.time_range.unwrap();
        prop_assert_eq!(tr.start_ms, start);
    }
}

// =========================================================================
// ExportRequest::required_tier — access control logic
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// 13. include_text=false always gives A0PublicMetadata.
    #[test]
    fn prop_required_tier_no_text(pane_ids in proptest::collection::vec(0_u64..100, 0..5)) {
        let req = ExportRequest {
            include_text: false,
            pane_ids,
            ..Default::default()
        };
        prop_assert_eq!(req.required_tier(), AccessTier::A0PublicMetadata);
    }

    /// 14. T3Restricted sensitivity always gives A3PrivilegedRaw.
    #[test]
    fn prop_required_tier_t3_restricted(_dummy in 0..1_u8) {
        let req = ExportRequest {
            include_text: true,
            max_sensitivity: Some(SensitivityTier::T3Restricted),
            ..Default::default()
        };
        prop_assert_eq!(req.required_tier(), AccessTier::A3PrivilegedRaw);
    }

    /// 15. Multiple pane_ids with text gives A2FullQuery.
    #[test]
    fn prop_required_tier_multi_pane(pane_ids in proptest::collection::vec(0_u64..100, 2..10)) {
        let req = ExportRequest {
            include_text: true,
            pane_ids,
            max_sensitivity: None,
            ..Default::default()
        };
        prop_assert_eq!(req.required_tier(), AccessTier::A2FullQuery);
    }

    /// 16. Single pane with text gives A1RedactedQuery.
    #[test]
    fn prop_required_tier_single_pane(pane_id in 0_u64..100) {
        let req = ExportRequest {
            include_text: true,
            pane_ids: vec![pane_id],
            max_sensitivity: None,
            ..Default::default()
        };
        prop_assert_eq!(req.required_tier(), AccessTier::A1RedactedQuery);
    }

    /// 17. Empty pane_ids with text gives A1RedactedQuery (len <= 1).
    #[test]
    fn prop_required_tier_empty_panes(_dummy in 0..1_u8) {
        let req = ExportRequest {
            include_text: true,
            pane_ids: vec![],
            max_sensitivity: None,
            ..Default::default()
        };
        prop_assert_eq!(req.required_tier(), AccessTier::A1RedactedQuery);
    }

    /// 18. required_tier is deterministic.
    #[test]
    fn prop_required_tier_deterministic(req in arb_export_request()) {
        let t1 = req.required_tier();
        let t2 = req.required_tier();
        prop_assert_eq!(t1, t2);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn export_format_variants_distinct() {
    assert_ne!(ExportFormat::JsonLines, ExportFormat::Csv);
    assert_ne!(ExportFormat::Csv, ExportFormat::Transcript);
    assert_ne!(ExportFormat::JsonLines, ExportFormat::Transcript);
}

#[test]
fn export_format_display() {
    assert_eq!(ExportFormat::JsonLines.to_string(), "jsonl");
    assert_eq!(ExportFormat::Csv.to_string(), "csv");
    assert_eq!(ExportFormat::Transcript.to_string(), "transcript");
}

#[test]
fn default_required_tier() {
    let req = ExportRequest::default();
    // Default: include_text=true, no max_sensitivity, empty pane_ids
    assert_eq!(req.required_tier(), AccessTier::A1RedactedQuery);
}

// =========================================================================
// NEW #19: ExportFormat — Clone produces equal value
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Clone produces an equal value for ExportFormat.
    #[test]
    fn prop_format_clone_eq(fmt in arb_export_format()) {
        #[allow(clippy::clone_on_copy)]
        let cloned = fmt.clone();
        prop_assert_eq!(fmt, cloned);
    }
}

// =========================================================================
// NEW #20: ExportFormat — Debug output is non-empty
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Debug output is non-empty for all ExportFormat variants.
    #[test]
    fn prop_format_debug_nonempty_all(fmt in arb_export_format()) {
        let debug = format!("{:?}", fmt);
        prop_assert!(!debug.is_empty());
    }
}

// =========================================================================
// NEW #21: ExportFormat — all 3 Display strings are distinct
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// All 3 Display strings are distinct from each other.
    #[test]
    fn prop_format_display_distinct(_dummy in 0..1_u8) {
        let jsonl = ExportFormat::JsonLines.to_string();
        let csv = ExportFormat::Csv.to_string();
        let transcript = ExportFormat::Transcript.to_string();
        prop_assert_ne!(&jsonl, &csv);
        prop_assert_ne!(&csv, &transcript);
        prop_assert_ne!(&jsonl, &transcript);
    }
}

// =========================================================================
// NEW #22: ExportFormat — same variant produces same hash
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Same ExportFormat variant produces the same hash value.
    #[test]
    fn prop_format_hash_consistent(fmt in arb_export_format()) {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut h1 = DefaultHasher::new();
        fmt.hash(&mut h1);
        let hash1 = h1.finish();

        let mut h2 = DefaultHasher::new();
        fmt.hash(&mut h2);
        let hash2 = h2.finish();

        prop_assert_eq!(hash1, hash2);
    }
}

// =========================================================================
// NEW #23: ExportRequest — full serde roundtrip including format check
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Full ExportRequest serde roundtrip verifies all preserved fields
    /// including format, time_range presence, and max_sensitivity.
    #[test]
    fn prop_request_serde_roundtrip_full(req in arb_export_request()) {
        let json = serde_json::to_string(&req).unwrap();
        let back: ExportRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.format, req.format);
        prop_assert_eq!(&back.pane_ids, &req.pane_ids);
        prop_assert_eq!(back.max_events, req.max_events);
        prop_assert_eq!(back.include_text, req.include_text);
        prop_assert_eq!(&back.label, &req.label);
        prop_assert_eq!(back.time_range.is_some(), req.time_range.is_some());
        prop_assert_eq!(back.max_sensitivity, req.max_sensitivity);
    }
}

// =========================================================================
// NEW #24: ExportRequest — Clone preserves format field
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Clone preserves the format field of ExportRequest.
    #[test]
    fn prop_request_clone_preserves_format(req in arb_export_request()) {
        let cloned = req.clone();
        prop_assert_eq!(cloned.format, req.format);
    }
}

// =========================================================================
// NEW #25: ExportRequest — Clone preserves pane_ids field
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Clone preserves the pane_ids field of ExportRequest.
    #[test]
    fn prop_request_clone_preserves_pane_ids(req in arb_export_request()) {
        let cloned = req.clone();
        prop_assert_eq!(&cloned.pane_ids, &req.pane_ids);
    }
}

// =========================================================================
// NEW #26: ExportRequest — serialized JSON always contains "format"
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Serialized JSON always contains the "format" key.
    #[test]
    fn prop_request_json_has_format_field(req in arb_export_request()) {
        let json = serde_json::to_string(&req).unwrap();
        prop_assert!(json.contains("\"format\""));
    }
}

// =========================================================================
// NEW #27: ExportRequest — serialized JSON always contains "pane_ids"
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Serialized JSON always contains the "pane_ids" key.
    #[test]
    fn prop_request_json_has_pane_ids(req in arb_export_request()) {
        let json = serde_json::to_string(&req).unwrap();
        prop_assert!(json.contains("\"pane_ids\""));
    }
}

// =========================================================================
// NEW #28: required_tier — include_text=false gives A0 regardless of
//          any sensitivity tier
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// include_text=false always gives A0PublicMetadata regardless of any
    /// sensitivity tier setting (None or any Some variant).
    #[test]
    fn prop_required_tier_no_text_any_sensitivity(
        sensitivity in proptest::option::of(arb_sensitivity_tier()),
        pane_ids in proptest::collection::vec(0_u64..100, 0..5),
    ) {
        let req = ExportRequest {
            include_text: false,
            max_sensitivity: sensitivity,
            pane_ids,
            ..Default::default()
        };
        prop_assert_eq!(
            req.required_tier(),
            AccessTier::A0PublicMetadata,
            "no text should always be A0, got {:?} for sensitivity {:?}",
            req.required_tier(),
            sensitivity,
        );
    }
}

// =========================================================================
// NEW #29: required_tier — T1Standard sensitivity + text + single pane
//          gives A1RedactedQuery
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// T1Standard sensitivity with text and a single pane gives
    /// A1RedactedQuery (the T3-specific branch is not taken).
    #[test]
    fn prop_required_tier_t1_with_text_single_pane(pane_id in 0_u64..100) {
        let req = ExportRequest {
            include_text: true,
            max_sensitivity: Some(SensitivityTier::T1Standard),
            pane_ids: vec![pane_id],
            ..Default::default()
        };
        prop_assert_eq!(req.required_tier(), AccessTier::A1RedactedQuery);
    }
}

// =========================================================================
// NEW #30: required_tier — more panes never decreases tier (monotonic)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Adding more panes never decreases the required tier (weak monotonicity).
    #[test]
    fn prop_required_tier_monotonic_with_pane_count(
        base_panes in proptest::collection::vec(0_u64..100, 0..3),
        extra_panes in proptest::collection::vec(0_u64..100, 1..5),
    ) {
        let req_small = ExportRequest {
            include_text: true,
            pane_ids: base_panes.clone(),
            max_sensitivity: None,
            ..Default::default()
        };
        let mut larger_panes = base_panes;
        larger_panes.extend_from_slice(&extra_panes);
        let req_large = ExportRequest {
            include_text: true,
            pane_ids: larger_panes,
            max_sensitivity: None,
            ..Default::default()
        };
        prop_assert!(
            req_large.required_tier() >= req_small.required_tier(),
            "larger pane set should not decrease tier: {:?} vs {:?}",
            req_large.required_tier(),
            req_small.required_tier(),
        );
    }
}

// =========================================================================
// NEW #31: jsonl builder preserves include_text=true and empty kind_filter
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// jsonl() builder preserves include_text=true, empty kind_filter,
    /// no max_sensitivity, and no label.
    #[test]
    fn prop_builder_jsonl_preserves_defaults(start in 0_u64..1_000_000, end in 0_u64..1_000_000) {
        let req = ExportRequest::jsonl(start, end);
        prop_assert!(req.include_text);
        prop_assert!(req.kind_filter.is_empty());
        prop_assert!(req.max_sensitivity.is_none());
        prop_assert!(req.label.is_none());
        prop_assert_eq!(req.max_events, 0);
    }
}

// =========================================================================
// NEW #32: csv_for_panes builder preserves include_text=true
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// csv_for_panes() builder preserves include_text=true and other defaults.
    #[test]
    fn prop_builder_csv_preserves_defaults(pane_ids in proptest::collection::vec(0_u64..10_000, 0..10)) {
        let req = ExportRequest::csv_for_panes(pane_ids);
        prop_assert!(req.include_text);
        prop_assert!(req.kind_filter.is_empty());
        prop_assert!(req.max_sensitivity.is_none());
        prop_assert!(req.label.is_none());
        prop_assert_eq!(req.max_events, 0);
    }
}

// =========================================================================
// NEW #33: transcript builder preserves include_text=true
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// transcript() builder preserves include_text=true and other defaults.
    #[test]
    fn prop_builder_transcript_preserves_defaults(start in 0_u64..1_000_000, end in 0_u64..1_000_000) {
        let req = ExportRequest::transcript(start, end);
        prop_assert!(req.include_text);
        prop_assert!(req.kind_filter.is_empty());
        prop_assert!(req.max_sensitivity.is_none());
        prop_assert!(req.label.is_none());
        prop_assert_eq!(req.max_events, 0);
    }
}

// =========================================================================
// NEW #34: with_max_events is idempotent (last call wins)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Setting max_events twice keeps only the last value.
    #[test]
    fn prop_with_max_events_idempotent(first in 0_usize..5000, second in 0_usize..5000) {
        let req = ExportRequest::default()
            .with_max_events(first)
            .with_max_events(second);
        prop_assert_eq!(req.max_events, second);
    }
}

// =========================================================================
// NEW #35: with_label is idempotent (last call wins)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Setting label twice keeps only the last value.
    #[test]
    fn prop_with_label_idempotent(
        first in "[a-z]{3,10}",
        second in "[a-z]{3,10}",
    ) {
        let req = ExportRequest::default()
            .with_label(first)
            .with_label(second.clone());
        prop_assert_eq!(req.label.as_deref(), Some(second.as_str()));
    }
}
