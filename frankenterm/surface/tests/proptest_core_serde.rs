#![cfg(feature = "use_serde")]

use frankenterm_surface::{CursorShape, CursorVisibility, DirtyRect, Position};
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

fn arb_cursor_visibility() -> impl Strategy<Value = CursorVisibility> {
    prop_oneof![
        Just(CursorVisibility::Hidden),
        Just(CursorVisibility::Visible),
    ]
}

fn arb_cursor_shape() -> impl Strategy<Value = CursorShape> {
    prop_oneof![
        Just(CursorShape::Default),
        Just(CursorShape::BlinkingBlock),
        Just(CursorShape::SteadyBlock),
        Just(CursorShape::BlinkingUnderline),
        Just(CursorShape::SteadyUnderline),
        Just(CursorShape::BlinkingBar),
        Just(CursorShape::SteadyBar),
    ]
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

    #[test]
    fn cursor_visibility_json_roundtrip(value in arb_cursor_visibility()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: CursorVisibility = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn cursor_shape_json_roundtrip(value in arb_cursor_shape()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: CursorShape = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }
}
