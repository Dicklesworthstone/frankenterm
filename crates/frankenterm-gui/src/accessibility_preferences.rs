//! GUI-side accessibility preference integration.
//!
//! The core crate owns the pure preference model. This module is the
//! GUI bridge: it probes platform state, applies operator overrides,
//! turns changes into live-update decisions, and exposes the
//! render-quality animation matrix used by paint paths.

use frankenterm_core::accessibility_preferences::{
    AccessibilityPreferences, ColorSchemePreference, ContrastPreference, LiveUpdateDecision,
    MotionPreference, PreferenceChange, RenderQualityHint, ThemeClass, select_theme_class,
    should_skip_animation,
};
use std::ffi::OsStr;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReduceMotionOverride {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContrastOverride {
    #[default]
    Auto,
    More,
    Less,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorSchemeOverride {
    #[default]
    Auto,
    Light,
    Dark,
}

/// Mirrors the intended `[accessibility]` section in
/// `frankenterm.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccessibilityPreferenceOverrides {
    pub reduce_motion: ReduceMotionOverride,
    pub contrast: ContrastOverride,
    pub color_scheme: ColorSchemeOverride,
}

impl AccessibilityPreferenceOverrides {
    #[must_use]
    pub fn resolve(self, os: AccessibilityPreferences) -> AccessibilityPreferences {
        AccessibilityPreferences {
            motion: match self.reduce_motion {
                ReduceMotionOverride::Auto => os.motion,
                ReduceMotionOverride::Always => MotionPreference::Reduce,
                ReduceMotionOverride::Never => MotionPreference::NoPreference,
            },
            contrast: match self.contrast {
                ContrastOverride::Auto => os.contrast,
                ContrastOverride::More => ContrastPreference::More,
                ContrastOverride::Less => ContrastPreference::Less,
                ContrastOverride::Custom => ContrastPreference::Custom,
            },
            color_scheme: match self.color_scheme {
                ColorSchemeOverride::Auto => os.color_scheme,
                ColorSchemeOverride::Light => ColorSchemePreference::Light,
                ColorSchemeOverride::Dark => ColorSchemePreference::Dark,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemePalette {
    pub class: ThemeClass,
    pub name: &'static str,
    pub foreground: &'static str,
    pub background: &'static str,
    pub accent: &'static str,
    pub high_contrast: bool,
}

pub const LIGHT_PALETTE: ThemePalette = ThemePalette {
    class: ThemeClass::Light,
    name: "Light",
    foreground: "#202124",
    background: "#f8f9fa",
    accent: "#0b57d0",
    high_contrast: false,
};

pub const DARK_PALETTE: ThemePalette = ThemePalette {
    class: ThemeClass::Dark,
    name: "Dark",
    foreground: "#f1f3f4",
    background: "#1f1f1f",
    accent: "#8ab4f8",
    high_contrast: false,
};

pub const HIGH_CONTRAST_LIGHT_PALETTE: ThemePalette = ThemePalette {
    class: ThemeClass::HighContrastLight,
    name: "HighContrastLight",
    foreground: "#000000",
    background: "#ffffff",
    accent: "#0042a5",
    high_contrast: true,
};

pub const HIGH_CONTRAST_DARK_PALETTE: ThemePalette = ThemePalette {
    class: ThemeClass::HighContrastDark,
    name: "HighContrastDark",
    foreground: "#ffffff",
    background: "#000000",
    accent: "#9ecbff",
    high_contrast: true,
};

pub const ACCESSIBILITY_PALETTES: [ThemePalette; 4] = [
    LIGHT_PALETTE,
    DARK_PALETTE,
    HIGH_CONTRAST_LIGHT_PALETTE,
    HIGH_CONTRAST_DARK_PALETTE,
];

#[must_use]
pub fn palette_for_theme_class(class: ThemeClass) -> ThemePalette {
    match class {
        ThemeClass::Light => LIGHT_PALETTE,
        ThemeClass::Dark => DARK_PALETTE,
        ThemeClass::HighContrastLight => HIGH_CONTRAST_LIGHT_PALETTE,
        ThemeClass::HighContrastDark | ThemeClass::Standard => HIGH_CONTRAST_DARK_PALETTE,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderQualityAnimationPolicy {
    pub quality: RenderQualityHint,
    pub skip_animation: bool,
}

#[must_use]
pub fn animation_policy_matrix(
    prefs: AccessibilityPreferences,
) -> [RenderQualityAnimationPolicy; 3] {
    [
        RenderQualityAnimationPolicy {
            quality: RenderQualityHint::Draft,
            skip_animation: should_skip_animation(prefs.motion, RenderQualityHint::Draft),
        },
        RenderQualityAnimationPolicy {
            quality: RenderQualityHint::Standard,
            skip_animation: should_skip_animation(prefs.motion, RenderQualityHint::Standard),
        },
        RenderQualityAnimationPolicy {
            quality: RenderQualityHint::Fancy,
            skip_animation: should_skip_animation(prefs.motion, RenderQualityHint::Fancy),
        },
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferenceUpdate {
    pub resolved: AccessibilityPreferences,
    pub changes: Vec<PreferenceChange>,
    pub decisions: Vec<LiveUpdateDecision>,
    pub theme_class: ThemeClass,
    pub palette: ThemePalette,
    pub animation_policy: [RenderQualityAnimationPolicy; 3],
}

pub trait AccessibilityPreferenceProbe {
    fn probe(&self) -> AccessibilityPreferences;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PlatformPreferenceProbe;

impl AccessibilityPreferenceProbe for PlatformPreferenceProbe {
    fn probe(&self) -> AccessibilityPreferences {
        probe_platform_preferences()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AccessibilityPreferenceController<P> {
    probe: P,
    overrides: AccessibilityPreferenceOverrides,
    current: AccessibilityPreferences,
}

impl<P: AccessibilityPreferenceProbe> AccessibilityPreferenceController<P> {
    #[must_use]
    pub fn new(probe: P, overrides: AccessibilityPreferenceOverrides) -> Self {
        let current = overrides.resolve(probe.probe());
        Self {
            probe,
            overrides,
            current,
        }
    }

    #[must_use]
    pub fn current(&self) -> AccessibilityPreferences {
        self.current
    }

    #[must_use]
    pub fn refresh(&mut self) -> PreferenceUpdate {
        self.refresh_from_os(self.probe.probe())
    }

    #[must_use]
    pub fn refresh_from_os(&mut self, os: AccessibilityPreferences) -> PreferenceUpdate {
        let next = self.overrides.resolve(os);
        let changes = self.current.diff(&next);
        self.current = next;
        build_update(next, changes)
    }

    pub fn set_overrides(&mut self, overrides: AccessibilityPreferenceOverrides) {
        self.overrides = overrides;
    }
}

#[must_use]
pub fn build_update(
    resolved: AccessibilityPreferences,
    changes: Vec<PreferenceChange>,
) -> PreferenceUpdate {
    let decisions = changes
        .iter()
        .map(PreferenceChange::live_update_decision)
        .collect();
    let theme_class = select_theme_class(resolved);
    PreferenceUpdate {
        resolved,
        changes,
        decisions,
        theme_class,
        palette: palette_for_theme_class(theme_class),
        animation_policy: animation_policy_matrix(resolved),
    }
}

#[must_use]
pub fn probe_platform_preferences() -> AccessibilityPreferences {
    #[cfg(target_os = "macos")]
    {
        return probe_macos_preferences();
    }

    #[cfg(target_os = "linux")]
    {
        return probe_linux_preferences();
    }

    #[cfg(windows)]
    {
        return AccessibilityPreferences::default();
    }

    #[allow(unreachable_code)]
    AccessibilityPreferences::default()
}

#[cfg(target_os = "macos")]
fn probe_macos_preferences() -> AccessibilityPreferences {
    let motion = defaults_bool("AppleReduceMotion")
        .map(|reduce| {
            if reduce {
                MotionPreference::Reduce
            } else {
                MotionPreference::NoPreference
            }
        })
        .unwrap_or_default();
    let contrast = defaults_bool("AppleIncreaseContrast")
        .map(|more| {
            if more {
                ContrastPreference::More
            } else {
                ContrastPreference::NoPreference
            }
        })
        .unwrap_or_default();
    let color_scheme = command_stdout("defaults", ["read", "-g", "AppleInterfaceStyle"])
        .map(|value| {
            if value.trim().eq_ignore_ascii_case("dark") {
                ColorSchemePreference::Dark
            } else {
                ColorSchemePreference::Light
            }
        })
        .unwrap_or_default();

    AccessibilityPreferences::new(motion, contrast, color_scheme)
}

#[cfg(target_os = "macos")]
fn defaults_bool(key: &str) -> Option<bool> {
    command_stdout("defaults", ["read", "-g", key]).and_then(|value| parse_bool(&value))
}

#[cfg(target_os = "linux")]
fn probe_linux_preferences() -> AccessibilityPreferences {
    let motion = command_stdout(
        "gsettings",
        ["get", "org.gnome.desktop.interface", "enable-animations"],
    )
    .and_then(|enabled| parse_bool(&enabled))
    .map(|enabled| {
        if enabled {
            MotionPreference::NoPreference
        } else {
            MotionPreference::Reduce
        }
    })
    .unwrap_or_default();

    let color_scheme = command_stdout(
        "gsettings",
        ["get", "org.gnome.desktop.interface", "color-scheme"],
    )
    .map(|value| {
        if value.to_ascii_lowercase().contains("dark") {
            ColorSchemePreference::Dark
        } else if value.to_ascii_lowercase().contains("light") {
            ColorSchemePreference::Light
        } else {
            ColorSchemePreference::NoPreference
        }
    })
    .unwrap_or_default();

    let contrast = command_stdout(
        "gsettings",
        ["get", "org.gnome.desktop.interface", "gtk-theme"],
    )
    .map(|value| {
        if value.to_ascii_lowercase().contains("highcontrast")
            || value.to_ascii_lowercase().contains("high-contrast")
        {
            ContrastPreference::Custom
        } else {
            ContrastPreference::NoPreference
        }
    })
    .unwrap_or_default();

    AccessibilityPreferences::new(motion, contrast, color_scheme)
}

fn command_stdout<I, S>(program: &str, args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().trim_matches('\'').trim_matches('"') {
        "1" | "true" | "TRUE" | "True" | "yes" | "YES" | "Yes" => Some(true),
        "0" | "false" | "FALSE" | "False" | "no" | "NO" | "No" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy)]
    struct StaticProbe(AccessibilityPreferences);

    impl AccessibilityPreferenceProbe for StaticProbe {
        fn probe(&self) -> AccessibilityPreferences {
            self.0
        }
    }

    fn os_reduce_dark_custom() -> AccessibilityPreferences {
        AccessibilityPreferences::new(
            MotionPreference::Reduce,
            ContrastPreference::Custom,
            ColorSchemePreference::Dark,
        )
    }

    #[test]
    fn overrides_auto_follow_os_preferences() {
        let resolved = AccessibilityPreferenceOverrides::default().resolve(os_reduce_dark_custom());

        assert_eq!(resolved, os_reduce_dark_custom());
    }

    #[test]
    fn overrides_pin_motion_contrast_and_scheme() {
        let overrides = AccessibilityPreferenceOverrides {
            reduce_motion: ReduceMotionOverride::Never,
            contrast: ContrastOverride::More,
            color_scheme: ColorSchemeOverride::Light,
        };

        let resolved = overrides.resolve(os_reduce_dark_custom());

        assert_eq!(resolved.motion, MotionPreference::NoPreference);
        assert_eq!(resolved.contrast, ContrastPreference::More);
        assert_eq!(resolved.color_scheme, ColorSchemePreference::Light);
    }

    #[test]
    fn palette_table_covers_four_concrete_theme_classes() {
        let classes: Vec<ThemeClass> = ACCESSIBILITY_PALETTES
            .iter()
            .map(|palette| palette.class)
            .collect();

        assert_eq!(
            classes,
            vec![
                ThemeClass::Light,
                ThemeClass::Dark,
                ThemeClass::HighContrastLight,
                ThemeClass::HighContrastDark,
            ]
        );
    }

    #[test]
    fn reduce_motion_skips_animation_in_every_quality_mode() {
        let prefs = AccessibilityPreferences::new(
            MotionPreference::Reduce,
            ContrastPreference::NoPreference,
            ColorSchemePreference::NoPreference,
        );

        let matrix = animation_policy_matrix(prefs);

        assert!(matrix.iter().all(|policy| policy.skip_animation));
    }

    #[test]
    fn standard_motion_only_skips_draft_quality() {
        let prefs = AccessibilityPreferences::default();

        let matrix = animation_policy_matrix(prefs);

        assert_eq!(
            matrix,
            [
                RenderQualityAnimationPolicy {
                    quality: RenderQualityHint::Draft,
                    skip_animation: true,
                },
                RenderQualityAnimationPolicy {
                    quality: RenderQualityHint::Standard,
                    skip_animation: false,
                },
                RenderQualityAnimationPolicy {
                    quality: RenderQualityHint::Fancy,
                    skip_animation: false,
                },
            ]
        );
    }

    #[test]
    fn refresh_emits_live_update_decisions_and_palette() {
        let mut controller = AccessibilityPreferenceController::new(
            StaticProbe(AccessibilityPreferences::default()),
            AccessibilityPreferenceOverrides::default(),
        );

        let update = controller.refresh_from_os(os_reduce_dark_custom());

        assert_eq!(update.resolved, os_reduce_dark_custom());
        assert_eq!(update.changes.len(), 3);
        assert!(
            update
                .decisions
                .iter()
                .all(|decision| *decision == LiveUpdateDecision::ApplyImmediately)
        );
        assert_eq!(update.theme_class, ThemeClass::HighContrastDark);
        assert_eq!(update.palette, HIGH_CONTRAST_DARK_PALETTE);
    }

    #[test]
    fn pinned_override_ignores_later_os_change_until_override_changes() {
        let mut controller = AccessibilityPreferenceController::new(
            StaticProbe(AccessibilityPreferences::default()),
            AccessibilityPreferenceOverrides {
                reduce_motion: ReduceMotionOverride::Always,
                contrast: ContrastOverride::Auto,
                color_scheme: ColorSchemeOverride::Auto,
            },
        );

        let update = controller.refresh_from_os(AccessibilityPreferences::default());

        assert_eq!(update.resolved.motion, MotionPreference::Reduce);
        assert_eq!(controller.current().motion, MotionPreference::Reduce);
    }

    #[test]
    fn bool_parser_accepts_platform_spellings() {
        assert_eq!(parse_bool("1"), Some(true));
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("'true'"), Some(true));
        assert_eq!(parse_bool("\"NO\""), Some(false));
        assert_eq!(parse_bool("maybe"), None);
    }
}
