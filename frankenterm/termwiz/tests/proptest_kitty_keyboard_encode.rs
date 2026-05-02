use frankenterm_escape_parser::csi::KittyKeyboardFlags;
use proptest::prelude::*;
use termwiz::input::{KeyCode, KeyCodeEncodeModes, KeyboardEncoding, Modifiers};

fn kitty_flags() -> impl Strategy<Value = KittyKeyboardFlags> {
    any::<u16>().prop_map(KittyKeyboardFlags::from_bits_truncate)
}

fn modifiers() -> impl Strategy<Value = Modifiers> {
    any::<u16>().prop_map(Modifiers::from_bits_truncate)
}

fn kitty_mode(flags: KittyKeyboardFlags) -> KeyCodeEncodeModes {
    KeyCodeEncodeModes {
        encoding: KeyboardEncoding::Kitty(flags),
        newline_mode: false,
        application_cursor_keys: false,
        application_keypad: false,
        modify_other_keys: None,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn function_keys_never_panic_in_kitty_mode(
        n in any::<u8>(),
        flags in kitty_flags(),
        mods in modifiers(),
        is_down in any::<bool>(),
    ) {
        let encoded = KeyCode::Function(n)
            .encode(mods, kitty_mode(flags), is_down)
            .expect("kitty function key encoding should not error");

        if (1..=24).contains(&n)
            && (is_down || flags.contains(KittyKeyboardFlags::REPORT_EVENT_TYPES))
        {
            prop_assert!(
                encoded.starts_with("\x1b["),
                "valid Kitty function key should encode as CSI, got {encoded:?}"
            );
        }
    }

    #[test]
    fn invalid_function_keys_encode_empty_in_kitty_mode(
        n in prop_oneof![Just(0u8), 25u8..=u8::MAX],
        flags in kitty_flags(),
        mods in modifiers(),
        is_down in any::<bool>(),
    ) {
        let encoded = KeyCode::Function(n)
            .encode(mods, kitty_mode(flags), is_down)
            .expect("invalid kitty function key encoding should not error");
        prop_assert_eq!(encoded, "");
    }

    #[test]
    fn kitty_function_key_release_obeys_event_type_flag(
        n in 1u8..=24,
        flags in kitty_flags(),
        mods in modifiers(),
    ) {
        let encoded = KeyCode::Function(n)
            .encode(mods, kitty_mode(flags), false)
            .expect("kitty function key release encoding should not error");

        if flags.contains(KittyKeyboardFlags::REPORT_EVENT_TYPES) {
            prop_assert!(
                encoded.contains(":3"),
                "release event should include Kitty event type marker, got {encoded:?}"
            );
        } else {
            prop_assert_eq!(encoded, "");
        }
    }

    #[test]
    fn kitty_function_key_prefixes_match_function_ranges(
        n in 1u8..=24,
        flags in kitty_flags(),
        mods in modifiers(),
    ) {
        let encoded = KeyCode::Function(n)
            .encode(mods, kitty_mode(flags), true)
            .expect("kitty function key press encoding should not error");

        if n <= 12 {
            prop_assert!(
                encoded.ends_with('~'),
                "F1-F12 should use tilde terminator, got {encoded:?}"
            );
        } else {
            prop_assert!(
                encoded.ends_with('u'),
                "F13-F24 should use Kitty private codepoint terminator, got {encoded:?}"
            );
        }
    }
}
