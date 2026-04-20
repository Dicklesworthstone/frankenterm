use frankenterm_input_types::{KeyCode, Modifiers, PhysKeyCode, WindowDecorations};
use proptest::prelude::*;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..24).prop_map(|chars| chars.into_iter().collect())
}

fn arb_phys_key_code() -> impl Strategy<Value = PhysKeyCode> {
    prop_oneof![
        Just(PhysKeyCode::A),
        Just(PhysKeyCode::B),
        Just(PhysKeyCode::Escape),
        Just(PhysKeyCode::Enter),
        Just(PhysKeyCode::LeftShift),
        Just(PhysKeyCode::RightShift),
        Just(PhysKeyCode::LeftControl),
        Just(PhysKeyCode::RightControl),
        Just(PhysKeyCode::LeftAlt),
        Just(PhysKeyCode::RightAlt),
        Just(PhysKeyCode::UpArrow),
        Just(PhysKeyCode::DownArrow),
        Just(PhysKeyCode::LeftArrow),
        Just(PhysKeyCode::RightArrow),
        Just(PhysKeyCode::Home),
        Just(PhysKeyCode::End),
        Just(PhysKeyCode::PageUp),
        Just(PhysKeyCode::PageDown),
        Just(PhysKeyCode::Insert),
        Just(PhysKeyCode::Delete),
        Just(PhysKeyCode::F1),
        Just(PhysKeyCode::F12),
        Just(PhysKeyCode::K0),
        Just(PhysKeyCode::K9),
    ]
}

fn arb_key_code() -> impl Strategy<Value = KeyCode> {
    prop_oneof![
        any::<char>().prop_map(KeyCode::Char),
        arb_small_string().prop_map(KeyCode::Composed),
        any::<u32>().prop_map(KeyCode::RawCode),
        arb_phys_key_code().prop_map(KeyCode::Physical),
        Just(KeyCode::Hyper),
        Just(KeyCode::Super),
        Just(KeyCode::Meta),
        Just(KeyCode::Shift),
        Just(KeyCode::Control),
        Just(KeyCode::Alt),
        Just(KeyCode::PageUp),
        Just(KeyCode::PageDown),
        Just(KeyCode::Home),
        Just(KeyCode::End),
        Just(KeyCode::LeftArrow),
        Just(KeyCode::RightArrow),
        Just(KeyCode::UpArrow),
        Just(KeyCode::DownArrow),
        (0u8..=9).prop_map(KeyCode::Numpad),
        (1u8..=24).prop_map(KeyCode::Function),
        Just(KeyCode::Copy),
        Just(KeyCode::Paste),
    ]
}

fn arb_modifiers() -> impl Strategy<Value = Modifiers> {
    (
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(shift, alt, ctrl, super_, leader, hyper, meta)| {
            let mut mods = Modifiers::NONE;
            if shift {
                mods |= Modifiers::SHIFT;
            }
            if alt {
                mods |= Modifiers::ALT;
            }
            if ctrl {
                mods |= Modifiers::CTRL;
            }
            if super_ {
                mods |= Modifiers::SUPER;
            }
            if leader {
                mods |= Modifiers::LEADER;
            }
            if hyper {
                mods |= Modifiers::HYPER;
            }
            if meta {
                mods |= Modifiers::META;
            }
            mods
        })
}

fn arb_window_decorations() -> impl Strategy<Value = WindowDecorations> {
    (
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(title, resize, integrated, bg_titlebar, force_enable_shadow, shadow_or_corners)| {
                let mut flags = WindowDecorations::NONE;
                if title {
                    flags |= WindowDecorations::TITLE;
                }
                if resize {
                    flags |= WindowDecorations::RESIZE;
                }
                if integrated {
                    flags |= WindowDecorations::INTEGRATED_BUTTONS;
                }
                if bg_titlebar {
                    flags |= WindowDecorations::MACOS_USE_BACKGROUND_COLOR_AS_TITLEBAR_COLOR;
                }
                if force_enable_shadow {
                    flags |= WindowDecorations::MACOS_FORCE_ENABLE_SHADOW;
                } else if shadow_or_corners {
                    flags |= WindowDecorations::MACOS_FORCE_DISABLE_SHADOW;
                } else {
                    flags |= WindowDecorations::MACOS_FORCE_SQUARE_CORNERS;
                }
                flags
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn key_code_json_roundtrip(value in arb_key_code()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: KeyCode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn modifiers_json_roundtrip(value in arb_modifiers()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: Modifiers = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn phys_key_code_json_roundtrip(value in arb_phys_key_code()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: PhysKeyCode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn window_decorations_json_roundtrip(value in arb_window_decorations()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: WindowDecorations = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }
}
