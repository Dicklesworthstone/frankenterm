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
        .prop_map(|(format, pane_ids, max_events, include_text, label)| {
            ExportRequest {
                format,
                time_range: None,
                pane_ids,
                kind_filter: vec![],
                max_events,
                include_text,
                max_sensitivity: None,
                label,
            }
        })
}

// =========================================================================
// ExportFormat — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ExportFormat serde roundtrip.
    #[test]
    fn prop_format_serde(fmt in arb_export_format()) {
        let json = serde_json::to_string(&fmt).unwrap();
        let back: ExportFormat = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, fmt);
    }

    /// ExportFormat serializes to snake_case.
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

    /// ExportFormat Display produces non-empty string.
    #[test]
    fn prop_format_display_nonempty(fmt in arb_export_format()) {
        let display = fmt.to_string();
        prop_assert!(!display.is_empty());
    }

    /// ExportFormat serde is deterministic.
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

    /// ExportRequest serde roundtrip preserves key fields.
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

    /// Default ExportRequest has expected values.
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

    /// jsonl() builder sets format and time_range correctly.
    #[test]
    fn prop_jsonl_builder(start in 0_u64..1_000_000, end in 0_u64..1_000_000) {
        let req = ExportRequest::jsonl(start, end);
        prop_assert_eq!(req.format, ExportFormat::JsonLines);
        let tr = req.time_range.unwrap();
        prop_assert_eq!(tr.start_ms, start);
        prop_assert_eq!(tr.end_ms, end);
        prop_assert!(req.include_text);
    }

    /// csv_for_panes() builder sets format and pane_ids correctly.
    #[test]
    fn prop_csv_builder(pane_ids in proptest::collection::vec(0_u64..10_000, 0..10)) {
        let req = ExportRequest::csv_for_panes(pane_ids.clone());
        prop_assert_eq!(req.format, ExportFormat::Csv);
        prop_assert_eq!(&req.pane_ids, &pane_ids);
        prop_assert!(req.time_range.is_none());
    }

    /// transcript() builder sets format and time_range correctly.
    #[test]
    fn prop_transcript_builder(start in 0_u64..1_000_000, end in 0_u64..1_000_000) {
        let req = ExportRequest::transcript(start, end);
        prop_assert_eq!(req.format, ExportFormat::Transcript);
        let tr = req.time_range.unwrap();
        prop_assert_eq!(tr.start_ms, start);
        prop_assert_eq!(tr.end_ms, end);
    }

    /// with_max_events() builder sets max_events.
    #[test]
    fn prop_with_max_events(max in 0_usize..10_000) {
        let req = ExportRequest::default().with_max_events(max);
        prop_assert_eq!(req.max_events, max);
    }

    /// with_label() builder sets label.
    #[test]
    fn prop_with_label(label in "[a-z ]{3,20}") {
        let req = ExportRequest::default().with_label(label.clone());
        prop_assert_eq!(req.label.as_deref(), Some(label.as_str()));
    }

    /// Builder chaining preserves previous fields.
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

    /// include_text=false always gives A0PublicMetadata.
    #[test]
    fn prop_required_tier_no_text(pane_ids in proptest::collection::vec(0_u64..100, 0..5)) {
        let req = ExportRequest {
            include_text: false,
            pane_ids,
            ..Default::default()
        };
        prop_assert_eq!(req.required_tier(), AccessTier::A0PublicMetadata);
    }

    /// T3Restricted sensitivity always gives A3PrivilegedRaw.
    #[test]
    fn prop_required_tier_t3_restricted(_dummy in 0..1_u8) {
        let req = ExportRequest {
            include_text: true,
            max_sensitivity: Some(SensitivityTier::T3Restricted),
            ..Default::default()
        };
        prop_assert_eq!(req.required_tier(), AccessTier::A3PrivilegedRaw);
    }

    /// Multiple pane_ids with text gives A2FullQuery.
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

    /// Single pane with text gives A1RedactedQuery.
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

    /// Empty pane_ids with text gives A1RedactedQuery (len <= 1).
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

    /// required_tier is deterministic.
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
