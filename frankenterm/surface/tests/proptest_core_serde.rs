#![cfg(feature = "use_serde")]

use frankenterm_surface::{DirtyRect, Position};
use proptest::prelude::*;

fn arb_position() -> impl Strategy<Value = Position> {
    prop_oneof![
        (-100_000isize..=100_000isize).prop_map(Position::Relative),
        (0usize..=100_000).prop_map(Position::Absolute),
        (0usize..=100_000).prop_map(Position::EndRelative),
    ]
}

fn arb_dirty_rect() -> impl Strategy<Value = DirtyRect> {
    (0usize..=4096, 0usize..=4096, 0usize..=4096, 0usize..=4096).prop_map(
        |(x, y, width, height)| DirtyRect {
            x,
            y,
            width,
            height,
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn position_json_roundtrip(value in arb_position()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: Position = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn dirty_rect_json_roundtrip(value in arb_dirty_rect()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: DirtyRect = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }
}
