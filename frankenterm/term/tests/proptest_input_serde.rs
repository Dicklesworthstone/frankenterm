#![cfg(feature = "use_serde")]

use frankenterm_term::{
    CursorPosition, CursorShape, CursorVisibility, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use proptest::prelude::*;

fn arb_mouse_button() -> impl Strategy<Value = MouseButton> {
    prop_oneof![
        Just(MouseButton::Left),
        Just(MouseButton::Middle),
        Just(MouseButton::Right),
        (0usize..=64).prop_map(MouseButton::WheelUp),
        (0usize..=64).prop_map(MouseButton::WheelDown),
        (0usize..=64).prop_map(MouseButton::WheelLeft),
        (0usize..=64).prop_map(MouseButton::WheelRight),
        Just(MouseButton::None),
    ]
}

fn arb_mouse_event_kind() -> impl Strategy<Value = MouseEventKind> {
    prop_oneof![
        Just(MouseEventKind::Press),
        Just(MouseEventKind::Release),
        Just(MouseEventKind::Move),
    ]
}

fn arb_key_modifiers() -> impl Strategy<Value = KeyModifiers> {
    (any::<bool>(), any::<bool>(), any::<bool>()).prop_map(|(shift, alt, ctrl)| {
        let mut mods = KeyModifiers::NONE;
        if shift {
            mods |= KeyModifiers::SHIFT;
        }
        if alt {
            mods |= KeyModifiers::ALT;
        }
        if ctrl {
            mods |= KeyModifiers::CTRL;
        }
        mods
    })
}

fn arb_mouse_event() -> impl Strategy<Value = MouseEvent> {
    (
        arb_mouse_event_kind(),
        0usize..=4096,
        -10_000i64..=10_000i64,
        -1024isize..=1024isize,
        -1024isize..=1024isize,
        arb_mouse_button(),
        arb_key_modifiers(),
    )
        .prop_map(
            |(kind, x, y, x_pixel_offset, y_pixel_offset, button, modifiers)| MouseEvent {
                kind,
                x,
                y,
                x_pixel_offset,
                y_pixel_offset,
                button,
                modifiers,
            },
        )
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

fn arb_cursor_visibility() -> impl Strategy<Value = CursorVisibility> {
    prop_oneof![
        Just(CursorVisibility::Hidden),
        Just(CursorVisibility::Visible),
    ]
}

fn arb_cursor_position() -> impl Strategy<Value = CursorPosition> {
    (
        0usize..=4096,
        -10_000i64..=10_000i64,
        arb_cursor_shape(),
        arb_cursor_visibility(),
        0usize..=8192,
    )
        .prop_map(|(x, y, shape, visibility, seqno)| CursorPosition {
            x,
            y,
            shape,
            visibility,
            seqno,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mouse_button_json_roundtrip(value in arb_mouse_button()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: MouseButton = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn mouse_event_kind_json_roundtrip(value in arb_mouse_event_kind()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: MouseEventKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn key_modifiers_json_roundtrip(value in arb_key_modifiers()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: KeyModifiers = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn mouse_event_json_roundtrip(value in arb_mouse_event()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: MouseEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn cursor_position_json_roundtrip(value in arb_cursor_position()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: CursorPosition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }
}
