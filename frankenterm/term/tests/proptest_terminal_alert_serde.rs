#![cfg(feature = "use_serde")]

use frankenterm_term::{Alert, ClipboardSelection, Progress};
use proptest::prelude::*;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..32).prop_map(|chars| chars.into_iter().collect())
}

fn arb_optional_string() -> impl Strategy<Value = Option<String>> {
    prop_oneof![Just(None), arb_small_string().prop_map(Some),]
}

fn arb_sanitized_alt_text() -> impl Strategy<Value = String> {
    let word = proptest::collection::vec(
        any::<char>().prop_filter("printable non-whitespace character", |ch| {
            !ch.is_control() && !ch.is_whitespace()
        }),
        1..=8,
    )
    .prop_map(|chars| chars.into_iter().collect::<String>());

    proptest::collection::vec(word, 1..=8).prop_map(|words| words.join(" "))
}

fn arb_progress() -> impl Strategy<Value = Progress> {
    prop_oneof![
        Just(Progress::None),
        any::<u8>().prop_map(Progress::Percentage),
        any::<u8>().prop_map(Progress::Error),
        Just(Progress::Indeterminate),
    ]
}

fn arb_clipboard_selection() -> impl Strategy<Value = ClipboardSelection> {
    prop_oneof![
        Just(ClipboardSelection::Clipboard),
        Just(ClipboardSelection::PrimarySelection),
    ]
}

fn arb_alert() -> impl Strategy<Value = Alert> {
    prop_oneof![
        Just(Alert::Bell),
        (arb_optional_string(), arb_small_string(), any::<bool>())
            .prop_map(|(title, body, focus)| { Alert::ToastNotification { title, body, focus } }),
        Just(Alert::CurrentWorkingDirectoryChanged),
        arb_optional_string().prop_map(Alert::IconTitleChanged),
        arb_small_string().prop_map(Alert::WindowTitleChanged),
        arb_optional_string().prop_map(Alert::TabTitleChanged),
        Just(Alert::PaletteChanged),
        (arb_small_string(), arb_small_string())
            .prop_map(|(name, value)| Alert::SetUserVar { name, value }),
        Just(Alert::OutputSinceFocusLost),
        arb_progress().prop_map(Alert::Progress),
        arb_small_string().prop_map(|name| Alert::SetProfileRequested { name }),
        arb_small_string().prop_map(|shape| Alert::MouseShapeRequested { shape }),
        (any::<u32>(), arb_sanitized_alt_text())
            .prop_map(|(image_id, text)| Alert::ImageAltText { image_id, text }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn clipboard_selection_json_roundtrip(value in arb_clipboard_selection()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: ClipboardSelection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn alert_json_roundtrip(value in arb_alert()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: Alert = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }
}
