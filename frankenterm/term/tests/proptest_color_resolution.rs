use frankenterm_term::color::{ColorAttribute, ColorPalette, SrgbaTuple};
use proptest::prelude::*;

fn arb_srgb_tuple() -> impl Strategy<Value = SrgbaTuple> {
    (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(r, g, b)| {
        SrgbaTuple(
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
            1.0,
        )
    })
}

fn arb_out_of_gamut_tuple() -> impl Strategy<Value = SrgbaTuple> {
    (-128i16..=383, -128i16..=383, -128i16..=383).prop_map(|(r, g, b)| {
        SrgbaTuple(
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
            1.0,
        )
    })
}

fn arb_hostile_tuple() -> impl Strategy<Value = SrgbaTuple> {
    (any::<f32>(), any::<f32>(), any::<f32>(), any::<f32>())
        .prop_map(|(r, g, b, a)| SrgbaTuple(r, g, b, a))
}

fn normalized_channel(channel: f32) -> f32 {
    if channel.is_finite() {
        channel.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn normalized_tuple(color: SrgbaTuple) -> SrgbaTuple {
    SrgbaTuple(
        normalized_channel(color.0),
        normalized_channel(color.1),
        normalized_channel(color.2),
        normalized_channel(color.3),
    )
}

fn squared_distance(a: SrgbaTuple, b: SrgbaTuple) -> f32 {
    let dr = a.0.clamp(0.0, 1.0) - b.0.clamp(0.0, 1.0);
    let dg = a.1.clamp(0.0, 1.0) - b.1.clamp(0.0, 1.0);
    let db = a.2.clamp(0.0, 1.0) - b.2.clamp(0.0, 1.0);
    dr.mul_add(dr, dg.mul_add(dg, db * db))
}

fn reduced_color(palette: &ColorPalette, source: SrgbaTuple, max_colors: usize) -> SrgbaTuple {
    let idx = palette
        .reduce_truecolor_to_palette_index(source, max_colors)
        .expect("non-empty palette reduction domain");
    palette.resolve_fg(ColorAttribute::PaletteIndex(idx))
}

fn is_in_gamut(color: SrgbaTuple) -> bool {
    color.0.is_finite()
        && (0.0..=1.0).contains(&color.0)
        && color.1.is_finite()
        && (0.0..=1.0).contains(&color.1)
        && color.2.is_finite()
        && (0.0..=1.0).contains(&color.2)
        && color.3.is_finite()
        && (0.0..=1.0).contains(&color.3)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn truecolor_reduction_returns_valid_16_and_256_palette_indices(color in arb_srgb_tuple()) {
        let palette = ColorPalette::default();

        let idx16 = palette.reduce_truecolor_to_palette_index(color, 16).unwrap();
        let idx256 = palette.reduce_truecolor_to_palette_index(color, 256).unwrap();

        prop_assert!(idx16 < 16);
        prop_assert!(usize::from(idx256) < 256);
        prop_assert!(is_in_gamut(palette.resolve_fg(ColorAttribute::PaletteIndex(idx16))));
        prop_assert!(is_in_gamut(palette.resolve_fg(ColorAttribute::PaletteIndex(idx256))));
    }

    #[test]
    fn xterm_256_reduction_is_never_farther_than_ansi_16_reduction(color in arb_srgb_tuple()) {
        let palette = ColorPalette::default();

        let reduced16 = reduced_color(&palette, color, 16);
        let reduced256 = reduced_color(&palette, color, 256);

        prop_assert!(
            squared_distance(color, reduced256) <= squared_distance(color, reduced16) + f32::EPSILON,
            "256-color reduction should be at least as close as 16-color reduction: source={color:?} reduced16={reduced16:?} reduced256={reduced256:?}"
        );
    }

    #[test]
    fn palette_reduction_error_is_monotonic_as_available_gamut_grows(
        color in arb_srgb_tuple(),
        smaller in 1usize..=255,
        extra in 0usize..=255,
    ) {
        let palette = ColorPalette::default();
        let larger = smaller.saturating_add(extra).clamp(smaller, 256);

        let reduced_smaller = reduced_color(&palette, color, smaller);
        let reduced_larger = reduced_color(&palette, color, larger);

        prop_assert!(
            squared_distance(color, reduced_larger)
                <= squared_distance(color, reduced_smaller) + f32::EPSILON,
            "expanding palette domain from {smaller} to {larger} should not increase error: source={color:?} smaller={reduced_smaller:?} larger={reduced_larger:?}"
        );
    }

    #[test]
    fn palette_entries_reduce_back_to_an_equivalent_palette_color(idx in any::<u8>()) {
        let palette = ColorPalette::default();
        let source = palette.resolve_fg(ColorAttribute::PaletteIndex(idx));
        let max_colors = usize::from(idx).saturating_add(1);
        let reduced = reduced_color(&palette, source, max_colors);

        prop_assert_eq!(
            squared_distance(source, reduced),
            0.0,
            "palette entry {} should reduce to an identical color within its reduction domain",
            idx
        );
    }

    #[test]
    fn out_of_gamut_truecolor_inputs_reduce_to_in_gamut_palette_colors(color in arb_out_of_gamut_tuple()) {
        let palette = ColorPalette::default();

        prop_assert!(is_in_gamut(reduced_color(&palette, color, 16)));
        prop_assert!(is_in_gamut(reduced_color(&palette, color, 256)));
    }

    #[test]
    fn hostile_truecolor_inputs_reduce_like_sanitized_gamut_inputs(
        color in arb_hostile_tuple(),
        max_colors in 1usize..=256,
    ) {
        let palette = ColorPalette::default();
        let sanitized = normalized_tuple(color);

        prop_assert_eq!(
            palette.reduce_truecolor_to_palette_index(color, max_colors),
            palette.reduce_truecolor_to_palette_index(sanitized, max_colors),
            "non-finite and out-of-gamut components should be normalized before palette search: source={:?} sanitized={:?} max_colors={}",
            color,
            sanitized,
            max_colors
        );
    }

    #[test]
    fn reduced_palette_contrast_ratios_are_finite_symmetric_and_bounded(
        fg in arb_srgb_tuple(),
        bg in arb_srgb_tuple(),
        max_colors in prop_oneof![Just(16usize), Just(256usize)],
    ) {
        let palette = ColorPalette::default();
        let reduced_fg = reduced_color(&palette, fg, max_colors);
        let reduced_bg = reduced_color(&palette, bg, max_colors);

        let forward = reduced_fg.contrast_ratio(&reduced_bg);
        let backward = reduced_bg.contrast_ratio(&reduced_fg);

        prop_assert!(forward.is_finite());
        prop_assert!((1.0..=21.0).contains(&forward));
        prop_assert!((forward - backward).abs() <= 0.0001);
    }
}
