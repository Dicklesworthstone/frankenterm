use mux::pane::{CollapsePriority, PaneConstraints, Pattern, PatternType, SearchResult};
use mux::renderable::{PaneTieredScrollbackStatus, RenderableDimensions};
use proptest::prelude::*;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..32).prop_map(|chars| chars.into_iter().collect())
}

fn arb_optional_usize() -> impl Strategy<Value = Option<usize>> {
    prop_oneof![Just(None), (0usize..=4096).prop_map(Some),]
}

fn arb_pane_constraints() -> impl Strategy<Value = PaneConstraints> {
    (
        0usize..=4096,
        0usize..=4096,
        arb_optional_usize(),
        arb_optional_usize(),
        arb_optional_usize(),
        arb_optional_usize(),
        any::<bool>(),
    )
        .prop_map(
            |(
                min_width,
                min_height,
                max_width,
                max_height,
                preferred_width,
                preferred_height,
                fixed,
            )| PaneConstraints {
                min_width,
                min_height,
                max_width,
                max_height,
                preferred_width,
                preferred_height,
                fixed,
            },
        )
}

fn arb_search_result() -> impl Strategy<Value = SearchResult> {
    (
        -100_000isize..=100_000isize,
        0usize..=4096,
        -100_000isize..=100_000isize,
        0usize..=4096,
        0usize..=4096,
    )
        .prop_map(|(start_y, start_x, end_y, end_x, match_id)| SearchResult {
            start_y,
            start_x,
            end_y,
            end_x,
            match_id,
        })
}

fn arb_pattern() -> impl Strategy<Value = Pattern> {
    prop_oneof![
        arb_small_string().prop_map(Pattern::CaseSensitiveString),
        arb_small_string().prop_map(Pattern::CaseInSensitiveString),
        arb_small_string().prop_map(Pattern::Regex),
    ]
}

fn arb_pattern_type() -> impl Strategy<Value = PatternType> {
    prop_oneof![
        Just(PatternType::CaseSensitiveString),
        Just(PatternType::CaseInSensitiveString),
        Just(PatternType::Regex),
    ]
}

fn arb_collapse_priority() -> impl Strategy<Value = CollapsePriority> {
    prop_oneof![
        Just(CollapsePriority::Never),
        Just(CollapsePriority::Low),
        Just(CollapsePriority::Normal),
        Just(CollapsePriority::High),
    ]
}

fn arb_renderable_dimensions() -> impl Strategy<Value = RenderableDimensions> {
    (
        0usize..=512,
        0usize..=512,
        0usize..=100_000,
        -100_000isize..=100_000isize,
        -100_000isize..=100_000isize,
        0u32..=960,
        0usize..=8192,
        0usize..=8192,
        any::<bool>(),
    )
        .prop_map(
            |(
                cols,
                viewport_rows,
                scrollback_rows,
                physical_top,
                scrollback_top,
                dpi,
                pixel_width,
                pixel_height,
                reverse_video,
            )| RenderableDimensions {
                cols,
                viewport_rows,
                scrollback_rows,
                physical_top,
                scrollback_top,
                dpi,
                pixel_width,
                pixel_height,
                reverse_video,
            },
        )
}

fn arb_pane_tiered_scrollback_status() -> impl Strategy<Value = PaneTieredScrollbackStatus> {
    (
        any::<bool>(),
        0usize..=100_000,
        0usize..=100_000,
        0usize..=10_000_000,
        0usize..=100_000,
        0usize..=100_000,
        0usize..=100_000,
        0usize..=10_000_000,
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        0usize..=100_000,
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(
                tiering_enabled,
                configured_scrollback_rows,
                configured_hot_lines,
                configured_warm_max_bytes,
                visible_rows,
                in_memory_scrollback_rows,
                warm_resident_lines,
                warm_resident_bytes,
                warm_spill_lines_total,
                warm_spill_bytes_total,
                cold_spill_lines_total,
                cold_spill_bytes_total,
                cold_worker_peak_backlog_depth,
                cold_worker_completion_throughput_lines_per_sec,
                cold_worker_completed_lines_total,
                cold_worker_completed_batches_total,
                cold_worker_cancellation_count,
            )| PaneTieredScrollbackStatus {
                tiering_enabled,
                configured_scrollback_rows,
                configured_hot_lines,
                configured_warm_max_bytes,
                visible_rows,
                in_memory_scrollback_rows,
                warm_resident_lines,
                warm_resident_bytes,
                warm_spill_lines_total,
                warm_spill_bytes_total,
                cold_spill_lines_total,
                cold_spill_bytes_total,
                cold_worker_peak_backlog_depth,
                cold_worker_completion_throughput_lines_per_sec,
                cold_worker_completed_lines_total,
                cold_worker_completed_batches_total,
                cold_worker_cancellation_count,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn pane_constraints_json_roundtrip(value in arb_pane_constraints()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: PaneConstraints = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn search_result_json_roundtrip(value in arb_search_result()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: SearchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn pattern_json_roundtrip(value in arb_pattern()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: Pattern = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn pattern_type_json_roundtrip(value in arb_pattern_type()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: PatternType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn collapse_priority_json_roundtrip(value in arb_collapse_priority()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: CollapsePriority = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn renderable_dimensions_json_roundtrip(value in arb_renderable_dimensions()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: RenderableDimensions = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn pane_tiered_scrollback_status_json_roundtrip(value in arb_pane_tiered_scrollback_status()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: PaneTieredScrollbackStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }
}
