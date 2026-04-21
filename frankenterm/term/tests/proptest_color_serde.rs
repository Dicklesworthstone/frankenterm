#![cfg(feature = "use_serde")]

// frankenterm-term uses edition 2018; TryInto is prelude-level in 2021+
// but has to be imported explicitly here. The `colors.try_into()` call
// at `arb_palette256` needs this.
use std::convert::TryInto;

use frankenterm_term::color::{ColorPalette, Palette256, SrgbaTuple};
use proptest::prelude::*;

fn arb_srgba_tuple() -> impl Strategy<Value = SrgbaTuple> {
    (any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(r, g, b, a)| {
        SrgbaTuple(
            (r as f32) / 255.0,
            (g as f32) / 255.0,
            (b as f32) / 255.0,
            (a as f32) / 255.0,
        )
    })
}

fn arb_palette256() -> impl Strategy<Value = Palette256> {
    proptest::collection::vec(arb_srgba_tuple(), 256).prop_map(|colors| {
        let array: [SrgbaTuple; 256] = colors.try_into().unwrap();
        Palette256(array)
    })
}

fn arb_color_palette() -> impl Strategy<Value = ColorPalette> {
    (
        arb_palette256(),
        arb_srgba_tuple(),
        arb_srgba_tuple(),
        arb_srgba_tuple(),
        arb_srgba_tuple(),
        arb_srgba_tuple(),
        arb_srgba_tuple(),
        arb_srgba_tuple(),
        arb_srgba_tuple(),
        arb_srgba_tuple(),
    )
        .prop_map(
            |(
                colors,
                foreground,
                background,
                cursor_fg,
                cursor_bg,
                cursor_border,
                selection_fg,
                selection_bg,
                scrollbar_thumb,
                split,
            )| ColorPalette {
                colors,
                foreground,
                background,
                cursor_fg,
                cursor_bg,
                cursor_border,
                selection_fg,
                selection_bg,
                scrollbar_thumb,
                split,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn palette256_json_roundtrip(value in arb_palette256()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: Palette256 = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn color_palette_json_roundtrip(value in arb_color_palette()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: ColorPalette = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }
}
