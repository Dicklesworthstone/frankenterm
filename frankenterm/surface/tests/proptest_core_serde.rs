#![cfg(feature = "use_serde")]

use frankenterm_surface::hyperlink::Rule;
use frankenterm_surface::{
    Change, CursorShape, CursorVisibility, DirtyRect, LineAttribute, Position,
};
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

fn arb_line_attribute() -> impl Strategy<Value = LineAttribute> {
    prop_oneof![
        Just(LineAttribute::DoubleHeightTopHalfLine),
        Just(LineAttribute::DoubleHeightBottomHalfLine),
        Just(LineAttribute::DoubleWidthLine),
        Just(LineAttribute::SingleWidthLine),
    ]
}

fn arb_change() -> impl Strategy<Value = Change> {
    prop_oneof![
        proptest::collection::vec(any::<char>(), 0..32)
            .prop_map(|chars| Change::Text(chars.into_iter().collect())),
        (arb_position(), arb_position()).prop_map(|(x, y)| Change::CursorPosition { x, y }),
        arb_cursor_shape().prop_map(Change::CursorShape),
        arb_cursor_visibility().prop_map(Change::CursorVisibility),
        arb_line_attribute().prop_map(Change::LineAttribute),
    ]
}

fn arb_rule() -> impl Strategy<Value = Rule> {
    prop_oneof![
        Just(Rule::new(r"foo", "$0").unwrap()),
        Just(Rule::new(r"\\d+", "num:$0").unwrap()),
        Just(Rule::with_highlight(r"(\\w+)://(\\S+)", "link:$2", 1).unwrap()),
        proptest::collection::vec(any::<char>(), 0..32)
            .prop_map(|chars| chars.into_iter().collect::<String>())
            .prop_map(|format| Rule::new(r"foo", &format).unwrap()),
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

    #[test]
    fn line_attribute_json_roundtrip(value in arb_line_attribute()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: LineAttribute = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn change_json_roundtrip(value in arb_change()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: Change = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn hyperlink_rule_json_roundtrip(value in arb_rule()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: Rule = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(back.regex.to_string(), value.regex.to_string());
        prop_assert_eq!(back.format, value.format);
        prop_assert_eq!(back.highlight, value.highlight);
    }
}
