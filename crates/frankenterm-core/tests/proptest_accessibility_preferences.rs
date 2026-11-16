use proptest::prelude::*;

use frankenterm_core::accessibility_preferences::{
    AccessibilityPreferences, ColorSchemePreference, ContrastPreference, LiveUpdateDecision,
    MotionPreference, PreferenceChange, RenderQualityHint, ThemeClass, select_theme_class,
    should_skip_animation,
};

fn arb_motion() -> impl Strategy<Value = MotionPreference> {
    prop_oneof![
        Just(MotionPreference::NoPreference),
        Just(MotionPreference::Reduce),
    ]
}

fn arb_contrast() -> impl Strategy<Value = ContrastPreference> {
    prop_oneof![
        Just(ContrastPreference::NoPreference),
        Just(ContrastPreference::More),
        Just(ContrastPreference::Less),
        Just(ContrastPreference::Custom),
    ]
}

fn arb_color_scheme() -> impl Strategy<Value = ColorSchemePreference> {
    prop_oneof![
        Just(ColorSchemePreference::NoPreference),
        Just(ColorSchemePreference::Light),
        Just(ColorSchemePreference::Dark),
    ]
}

fn arb_quality() -> impl Strategy<Value = RenderQualityHint> {
    prop_oneof![
        Just(RenderQualityHint::Standard),
        Just(RenderQualityHint::Fancy),
        Just(RenderQualityHint::Draft),
    ]
}

fn arb_preferences() -> impl Strategy<Value = AccessibilityPreferences> {
    (arb_motion(), arb_contrast(), arb_color_scheme()).prop_map(
        |(motion, contrast, color_scheme)| {
            AccessibilityPreferences::new(motion, contrast, color_scheme)
        },
    )
}

fn expected_theme_class(prefs: AccessibilityPreferences) -> ThemeClass {
    let high_contrast = matches!(
        prefs.contrast,
        ContrastPreference::More | ContrastPreference::Custom
    );
    match (prefs.color_scheme, high_contrast) {
        (ColorSchemePreference::Light, true) => ThemeClass::HighContrastLight,
        (ColorSchemePreference::Dark, true) => ThemeClass::HighContrastDark,
        (ColorSchemePreference::Light, false) => ThemeClass::Light,
        (ColorSchemePreference::Dark, false) => ThemeClass::Dark,
        (ColorSchemePreference::NoPreference, true) => ThemeClass::HighContrastDark,
        (ColorSchemePreference::NoPreference, false) => ThemeClass::Standard,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn proptest_accessibility_diff_has_one_entry_per_changed_axis(
        before in arb_preferences(),
        after in arb_preferences(),
    ) {
        let changes = before.diff(&after);
        let expected_len = usize::from(before.motion != after.motion)
            + usize::from(before.contrast != after.contrast)
            + usize::from(before.color_scheme != after.color_scheme);

        prop_assert_eq!(changes.len(), expected_len);
        prop_assert_eq!(before.has_changed(&after), expected_len > 0);
    }

    #[test]
    fn proptest_accessibility_diff_preserves_axis_order_and_values(
        before in arb_preferences(),
        after in arb_preferences(),
    ) {
        let changes = before.diff(&after);
        let mut expected = Vec::new();
        if before.motion != after.motion {
            expected.push(PreferenceChange::Motion {
                old: before.motion,
                new: after.motion,
            });
        }
        if before.contrast != after.contrast {
            expected.push(PreferenceChange::Contrast {
                old: before.contrast,
                new: after.contrast,
            });
        }
        if before.color_scheme != after.color_scheme {
            expected.push(PreferenceChange::ColorScheme {
                old: before.color_scheme,
                new: after.color_scheme,
            });
        }

        prop_assert_eq!(changes, expected);
    }

    #[test]
    fn proptest_accessibility_animation_skip_is_strictest_signal(
        motion in arb_motion(),
        quality in arb_quality(),
    ) {
        prop_assert_eq!(
            should_skip_animation(motion, quality),
            motion == MotionPreference::Reduce || quality == RenderQualityHint::Draft,
        );
    }

    #[test]
    fn proptest_accessibility_theme_selection_ignores_motion(
        motion in arb_motion(),
        contrast in arb_contrast(),
        color_scheme in arb_color_scheme(),
    ) {
        let prefs = AccessibilityPreferences::new(motion, contrast, color_scheme);
        let no_motion_pref =
            AccessibilityPreferences::new(MotionPreference::NoPreference, contrast, color_scheme);

        prop_assert_eq!(select_theme_class(prefs), expected_theme_class(prefs));
        prop_assert_eq!(select_theme_class(prefs), select_theme_class(no_motion_pref));
    }

    #[test]
    fn proptest_accessibility_all_reported_changes_apply_immediately(
        before in arb_preferences(),
        after in arb_preferences(),
    ) {
        for change in before.diff(&after) {
            prop_assert_eq!(
                change.live_update_decision(),
                LiveUpdateDecision::ApplyImmediately,
            );
        }
    }
}
