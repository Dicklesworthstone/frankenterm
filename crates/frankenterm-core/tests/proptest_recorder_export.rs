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

// =========================================================================
// NEW: ExportFormat — Clone/Copy/Debug
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ExportFormat Copy semantics preserve equality.
    #[test]
    fn prop_format_copy(fmt in arb_export_format()) {
        let copied = fmt;
        prop_assert_eq!(fmt, copied);
    }

    /// ExportFormat Debug is non-empty.
    #[test]
    fn prop_format_debug_nonempty(fmt in arb_export_format()) {
        let dbg = format!("{:?}", fmt);
        prop_assert!(!dbg.is_empty());
    }

    /// ExportFormat Debug contains variant name.
    #[test]
    fn prop_format_debug_contains_variant(fmt in arb_export_format()) {
        let dbg = format!("{:?}", fmt);
        let has_name = dbg.contains("JsonLines")
            || dbg.contains("Csv")
            || dbg.contains("Transcript");
        prop_assert!(has_name, "Debug '{}' should contain variant name", dbg);
    }

    /// ExportFormat Display is ASCII.
    #[test]
    fn prop_format_display_ascii(fmt in arb_export_format()) {
        let s = fmt.to_string();
        prop_assert!(s.is_ascii(), "Display '{}' should be ASCII", s);
    }
}

// =========================================================================
// NEW: ExportFormat — pretty JSON roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Pretty JSON roundtrip.
    #[test]
    fn prop_format_pretty_serde(fmt in arb_export_format()) {
        let json = serde_json::to_string_pretty(&fmt).unwrap();
        let back: ExportFormat = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(fmt, back);
    }
}

// =========================================================================
// NEW: ExportRequest — serde with sensitivity tiers
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ExportRequest with T1Standard sensitivity roundtrips.
    #[test]
    fn prop_request_serde_with_t1_sensitivity(fmt in arb_export_format()) {
        let req = ExportRequest {
            format: fmt,
            max_sensitivity: Some(SensitivityTier::T1Standard),
            ..Default::default()
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: ExportRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.max_sensitivity, Some(SensitivityTier::T1Standard));
        prop_assert_eq!(back.format, fmt);
    }

    /// ExportRequest with T3Restricted sensitivity roundtrips.
    #[test]
    fn prop_request_serde_with_t3_sensitivity(fmt in arb_export_format()) {
        let req = ExportRequest {
            format: fmt,
            max_sensitivity: Some(SensitivityTier::T3Restricted),
            ..Default::default()
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: ExportRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.max_sensitivity, Some(SensitivityTier::T3Restricted));
    }

    /// ExportRequest serde is deterministic.
    #[test]
    fn prop_request_serde_deterministic(req in arb_export_request()) {
        let j1 = serde_json::to_string(&req).unwrap();
        let j2 = serde_json::to_string(&req).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// NEW: ExportRequest — Clone / Debug
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ExportRequest Clone preserves all fields.
    #[test]
    fn prop_request_clone(req in arb_export_request()) {
        let cloned = req.clone();
        prop_assert_eq!(cloned.format, req.format);
        prop_assert_eq!(&cloned.pane_ids, &req.pane_ids);
        prop_assert_eq!(cloned.max_events, req.max_events);
        prop_assert_eq!(cloned.include_text, req.include_text);
        prop_assert_eq!(&cloned.label, &req.label);
    }

    /// ExportRequest Debug is non-empty.
    #[test]
    fn prop_request_debug_nonempty(req in arb_export_request()) {
        let dbg = format!("{:?}", req);
        prop_assert!(!dbg.is_empty());
    }
}

// =========================================================================
// NEW: required_tier — T1/T2 sensitivity boundaries
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// T1Standard sensitivity with text gives at least A1.
    #[test]
    fn prop_required_tier_t1_standard(pane_ids in proptest::collection::vec(0_u64..100, 0..5)) {
        let req = ExportRequest {
            include_text: true,
            max_sensitivity: Some(SensitivityTier::T1Standard),
            pane_ids,
            ..Default::default()
        };
        let tier = req.required_tier();
        prop_assert!(
            tier == AccessTier::A1RedactedQuery
                || tier == AccessTier::A2FullQuery
                || tier == AccessTier::A3PrivilegedRaw,
            "T1Standard tier should be >= A1, got {:?}", tier
        );
    }

    /// required_tier with no text is always A0 regardless of sensitivity.
    #[test]
    fn prop_required_tier_no_text_any_sensitivity(
        pane_ids in proptest::collection::vec(0_u64..100, 0..5),
    ) {
        for sensitivity in [
            Some(SensitivityTier::T1Standard),
            Some(SensitivityTier::T2Sensitive),
            Some(SensitivityTier::T3Restricted),
            None,
        ] {
            let req = ExportRequest {
                include_text: false,
                max_sensitivity: sensitivity,
                pane_ids: pane_ids.clone(),
                ..Default::default()
            };
            prop_assert_eq!(req.required_tier(), AccessTier::A0PublicMetadata,
                "no text should always be A0, got {:?} for sensitivity {:?}",
                req.required_tier(), sensitivity);
        }
    }
}

// =========================================================================
// NEW: Builder method isolation
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// with_max_events doesn't change format or pane_ids.
    #[test]
    fn prop_with_max_events_isolation(
        start in 0_u64..1_000_000,
        end in 0_u64..1_000_000,
        max in 0_usize..10_000,
    ) {
        let base = ExportRequest::jsonl(start, end);
        let modified = base.clone().with_max_events(max);
        prop_assert_eq!(modified.format, ExportFormat::JsonLines);
        prop_assert_eq!(modified.time_range.unwrap().start_ms, start);
        prop_assert_eq!(modified.max_events, max);
    }

    /// with_label doesn't change format or include_text.
    #[test]
    fn prop_with_label_isolation(label in "[a-z ]{3,20}") {
        let base = ExportRequest::default();
        let modified = base.clone().with_label(label.clone());
        prop_assert_eq!(modified.format, base.format);
        prop_assert_eq!(modified.include_text, base.include_text);
        prop_assert_eq!(modified.label.as_deref(), Some(label.as_str()));
    }
}
