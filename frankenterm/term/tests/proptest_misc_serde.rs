#![cfg(feature = "use_serde")]

use frankenterm_term::{
    Progress, SemanticType, SemanticZone, TerminalSize, TieredScrollbackStatus,
};
use proptest::prelude::*;

fn arb_semantic_type() -> impl Strategy<Value = SemanticType> {
    prop_oneof![
        Just(SemanticType::Output),
        Just(SemanticType::Input),
        Just(SemanticType::Prompt),
    ]
}

fn arb_progress() -> impl Strategy<Value = Progress> {
    prop_oneof![
        Just(Progress::None),
        any::<u8>().prop_map(Progress::Percentage),
        any::<u8>().prop_map(Progress::Error),
        Just(Progress::Indeterminate),
    ]
}

fn arb_terminal_size() -> impl Strategy<Value = TerminalSize> {
    (
        1usize..=1024,
        1usize..=1024,
        0usize..=16384,
        0usize..=16384,
        0u32..=960,
    )
        .prop_map(
            |(rows, cols, pixel_width, pixel_height, dpi)| TerminalSize {
                rows,
                cols,
                pixel_width,
                pixel_height,
                dpi,
            },
        )
}

fn arb_semantic_zone() -> impl Strategy<Value = SemanticZone> {
    (
        -100_000isize..=100_000isize,
        0usize..=4096,
        -100_000isize..=100_000isize,
        0usize..=4096,
        arb_semantic_type(),
    )
        .prop_map(
            |(start_y, start_x, end_y, end_x, semantic_type)| SemanticZone {
                start_y,
                start_x,
                end_y,
                end_x,
                semantic_type,
            },
        )
}

fn arb_tiered_scrollback_status() -> impl Strategy<Value = TieredScrollbackStatus> {
    (
        (
            any::<bool>(),
            0usize..=100_000,
            0usize..=100_000,
            0usize..=10_000_000,
            0usize..=100_000,
            0usize..=100_000,
        ),
        (
            0usize..=100_000,
            0usize..=10_000_000,
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
        ),
        (
            0usize..=100_000,
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
        ),
    )
        .prop_map(
            |(
                (
                    tiering_enabled,
                    configured_scrollback_rows,
                    configured_hot_lines,
                    configured_warm_max_bytes,
                    visible_rows,
                    in_memory_scrollback_rows,
                ),
                (
                    warm_resident_lines,
                    warm_resident_bytes,
                    warm_spill_lines_total,
                    warm_spill_bytes_total,
                    cold_spill_lines_total,
                    cold_spill_bytes_total,
                ),
                (
                    cold_worker_peak_backlog_depth,
                    cold_worker_completion_throughput_lines_per_sec,
                    cold_worker_completed_lines_total,
                    cold_worker_completed_batches_total,
                    cold_worker_cancellation_count,
                ),
            )| TieredScrollbackStatus {
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
    fn progress_json_roundtrip(value in arb_progress()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: Progress = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn terminal_size_json_roundtrip(value in arb_terminal_size()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: TerminalSize = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn semantic_zone_json_roundtrip(value in arb_semantic_zone()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: SemanticZone = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn semantic_type_json_roundtrip(value in arb_semantic_type()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: SemanticType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn tiered_scrollback_status_json_roundtrip(value in arb_tiered_scrollback_status()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: TieredScrollbackStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }
}

#[test]
fn progress_default_is_none() {
    assert_eq!(Progress::default(), Progress::None);
}

#[test]
fn terminal_size_default_matches_classic_terminal_geometry() {
    assert_eq!(
        TerminalSize::default(),
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        }
    );
}
