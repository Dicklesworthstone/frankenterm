#![cfg(feature = "use_serde")]

use frankenterm_term::{Progress, SemanticType, SemanticZone, TerminalSize};
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
}
