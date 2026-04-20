#![cfg(feature = "use_serde")]

use frankenterm_escape_parser::csi::{Blink, Underline, VerticalAlign};
use frankenterm_escape_parser::hyperlink::Hyperlink;
use proptest::prelude::*;
use std::collections::HashMap;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..24).prop_map(|chars| chars.into_iter().collect())
}

fn arb_hyperlink() -> impl Strategy<Value = Hyperlink> {
    (
        "[a-z0-9]{1,12}",
        "[a-z0-9/_-]{0,24}",
        proptest::collection::vec((arb_small_string(), arb_small_string()), 0..4),
        any::<bool>(),
    )
        .prop_map(|(host, path, params, implicit)| {
            let path = path.trim_matches('/');
            let uri = if path.is_empty() {
                format!("https://{host}.example/")
            } else {
                format!("https://{host}.example/{path}")
            };

            if implicit {
                Hyperlink::new_implicit(uri)
            } else if params.is_empty() {
                Hyperlink::new(uri)
            } else {
                let map: HashMap<String, String> = params.into_iter().collect();
                Hyperlink::new_with_params(uri, map)
            }
        })
}

fn arb_underline() -> impl Strategy<Value = Underline> {
    prop_oneof![
        Just(Underline::None),
        Just(Underline::Single),
        Just(Underline::Double),
        Just(Underline::Curly),
        Just(Underline::Dotted),
        Just(Underline::Dashed),
    ]
}

fn arb_blink() -> impl Strategy<Value = Blink> {
    prop_oneof![Just(Blink::None), Just(Blink::Slow), Just(Blink::Rapid),]
}

fn arb_vertical_align() -> impl Strategy<Value = VerticalAlign> {
    prop_oneof![
        Just(VerticalAlign::BaseLine),
        Just(VerticalAlign::SuperScript),
        Just(VerticalAlign::SubScript),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn hyperlink_json_roundtrip(value in arb_hyperlink()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: Hyperlink = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn underline_json_roundtrip(value in arb_underline()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: Underline = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn blink_json_roundtrip(value in arb_blink()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: Blink = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn vertical_align_json_roundtrip(value in arb_vertical_align()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: VerticalAlign = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }
}
