//! OS accessibility-preference plumbing substrate (ft-mpc9b.10.5).
//!
//! OS-level preferences (`prefers-reduced-motion`,
//! `prefers-contrast`, `prefers-color-scheme`) must be honoured by
//! the renderer regardless of `RenderQuality`. The bead's rule:
//! Draft mode already skips animations, but a renderer change that
//! re-enables animation work must still respect reduced-motion.
//! Same for high-contrast theming and dark/light scheme.
//!
//! This module ships the pure-logic types + decision policy:
//!
//! - `AccessibilityPreferences` — three-axis state record
//!   (motion / contrast / colour-scheme) the integration layer
//!   refreshes on each OS-level change.
//! - `MotionPreference` / `ContrastPreference` / `ColorSchemePreference`
//!   — the canonical enum surface mirroring CSS Media Queries Level 5
//!   semantics so the test matrix matches what web users already know.
//! - `PreferenceChange` — describes a single-axis flip; the
//!   integration's preference-probe emits one per change.
//! - `should_skip_animation` — pure predicate. Composes
//!   `MotionPreference` with `RenderQuality` (cross-link
//!   `crates/frankenterm-core/src/render_quality.rs` if/when that
//!   ships; substrate accepts the value as `RenderQualityHint`).
//! - `select_theme_class` — picks the operator-visible theme class
//!   from contrast + colour-scheme preferences. The integration's
//!   theme system reads this and chooses the corresponding palette.
//! - `LiveUpdateDecision` — what the integration must do per change:
//!   `ApplyImmediately` (theme + reduce-motion: 1-frame budget),
//!   `IgnoreUntilNextFrame` (cosmetic-only changes that can wait one
//!   frame).
//!
//! ## What is deferred to the integration bead (ft-mpc9b.10.5.cont)
//!
//! - Per-OS preference probes:
//!   - macOS: `NSWorkspace` accessibility-display-options + `defaults
//!     read -g AppleInterfaceStyle` (light/dark) + `Reduce motion`
//!     toggle in System Preferences.
//!   - Linux GNOME: `org.gnome.desktop.interface` gsettings keys
//!     (`enable-animations`, `gtk-theme`, `color-scheme`).
//!   - Linux KDE: `kdeglobals` + `kwinrc`.
//!   - Windows: `SystemParametersInfo(SPI_GETCLIENTAREAANIMATION)` +
//!     UISettings `AdvancedEffectsEnabled` + dark-mode registry key.
//! - Live notification subscription per platform (so changes flip
//!   within 1 frame of the OS-level change, per the bead).
//! - Theme palette tables for high-contrast / dark / light variants.
//! - Frankenterm.toml override (operator can pin a preference
//!   regardless of OS).
//! - Per-RenderQuality animation-skip wiring through paint.rs.
//! - Cross-RenderQuality regression test (toggle motion / contrast /
//!   scheme across Draft / Standard / Fancy; assert preferences
//!   honoured in all 3).

#![allow(dead_code)]

// ============================================================================
// Preference axes
// ============================================================================

/// Mirrors CSS `prefers-reduced-motion`. The integration layer's
/// preference probe reads the OS state and feeds one of these
/// values; renderers compose with `should_skip_animation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MotionPreference {
    /// User wants animations and motion effects (the default OS
    /// state on every platform).
    #[default]
    NoPreference,
    /// User has explicitly opted out of animation. The renderer
    /// skips cursor-blink, dialog-fade, smooth-scroll, etc.
    Reduce,
}

/// Mirrors CSS `prefers-contrast`. Three states matching what
/// macOS / GNOME / Windows expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ContrastPreference {
    /// Default OS state.
    #[default]
    NoPreference,
    /// Higher-contrast palette requested. Bumps borders +
    /// foreground-vs-background separation.
    More,
    /// Lower-contrast palette. Rare but exposed by macOS for
    /// glare-sensitive users.
    Less,
    /// Bigger jump than `More` — system-level high-contrast theme.
    /// macOS doesn't expose this granularity; on Linux GNOME's
    /// "high-contrast" theme + Windows "High Contrast Mode" both
    /// surface here.
    Custom,
}

/// Mirrors CSS `prefers-color-scheme`. The integration layer reads
/// the OS dark/light setting and maps to one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ColorSchemePreference {
    /// User has not expressed a preference; the operator's
    /// frankenterm.toml `[theme] default = "..."` decides.
    #[default]
    NoPreference,
    Light,
    Dark,
}

// ============================================================================
// Composite preference state
// ============================================================================

/// Three-axis state record. The integration layer holds one
/// `AccessibilityPreferences` per ft session and re-reads it on each
/// OS-level change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct AccessibilityPreferences {
    pub motion: MotionPreference,
    pub contrast: ContrastPreference,
    pub color_scheme: ColorSchemePreference,
}

impl AccessibilityPreferences {
    #[must_use]
    pub fn new(
        motion: MotionPreference,
        contrast: ContrastPreference,
        color_scheme: ColorSchemePreference,
    ) -> Self {
        Self {
            motion,
            contrast,
            color_scheme,
        }
    }

    /// Returns the per-axis change-set between two preference
    /// snapshots. Used by the integration's probe to emit a
    /// `PreferenceChange` event per axis that flipped.
    #[must_use]
    pub fn diff(&self, other: &Self) -> Vec<PreferenceChange> {
        let mut changes = Vec::new();
        if self.motion != other.motion {
            changes.push(PreferenceChange::Motion {
                old: self.motion,
                new: other.motion,
            });
        }
        if self.contrast != other.contrast {
            changes.push(PreferenceChange::Contrast {
                old: self.contrast,
                new: other.contrast,
            });
        }
        if self.color_scheme != other.color_scheme {
            changes.push(PreferenceChange::ColorScheme {
                old: self.color_scheme,
                new: other.color_scheme,
            });
        }
        changes
    }

    /// Whether any axis differs from the other snapshot.
    #[must_use]
    pub fn has_changed(&self, other: &Self) -> bool {
        !self.diff(other).is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreferenceChange {
    Motion {
        old: MotionPreference,
        new: MotionPreference,
    },
    Contrast {
        old: ContrastPreference,
        new: ContrastPreference,
    },
    ColorScheme {
        old: ColorSchemePreference,
        new: ColorSchemePreference,
    },
}

impl PreferenceChange {
    /// Whether this change requires a 1-frame-budget redraw (theme
    /// changes + reduce-motion need to apply immediately) vs a
    /// can-wait-a-frame redraw.
    #[must_use]
    pub fn live_update_decision(&self) -> LiveUpdateDecision {
        match self {
            // Reducing motion mid-animation is critical: keeping the
            // animation running after the user opted out is
            // user-hostile. Apply immediately.
            Self::Motion { .. } => LiveUpdateDecision::ApplyImmediately,
            // Contrast + colour scheme are theming changes; the
            // bead's rule is "within 1 frame", which means apply
            // immediately on the next paint.
            Self::Contrast { .. } | Self::ColorScheme { .. } => {
                LiveUpdateDecision::ApplyImmediately
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LiveUpdateDecision {
    /// Theme or motion change — schedule a paint within 1 frame
    /// budget per the bead's rule.
    ApplyImmediately,
    /// Cosmetic-only change that can defer a frame (e.g. a
    /// preference axis added in the future that doesn't affect the
    /// current paint pass).
    IgnoreUntilNextFrame,
}

// ============================================================================
// RenderQuality interaction
// ============================================================================

/// Mirrors the renderer's quality enum (cross-link
/// `RenderQuality` from ft-mpc9b.2.2 / commit 46bcee773 if it lands
/// the canonical home in core; substrate accepts the value as a
/// hint so it works with whatever the integration calls it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RenderQualityHint {
    /// Full-quality, all animations + effects.
    #[default]
    Standard,
    /// Higher-quality (e.g. higher-resolution glyph atlas, more
    /// post-processing).
    Fancy,
    /// Reduced-quality during live-resize / heavy bursts. Already
    /// skips animations as a side effect.
    Draft,
}

impl RenderQualityHint {
    #[must_use]
    pub fn skips_animations_by_default(self) -> bool {
        matches!(self, Self::Draft)
    }
}

// ============================================================================
// Animation-skip predicate
// ============================================================================

/// Pure predicate composing `MotionPreference` with
/// `RenderQualityHint`. The renderer hot path calls this before
/// dispatching cursor-blink / dialog-fade / smooth-scroll work.
///
/// Returns `true` when the renderer should skip the animation.
/// `Draft` already skips animations even with `NoPreference`, so the
/// predicate respects whichever signal is stricter.
#[must_use]
pub fn should_skip_animation(
    motion: MotionPreference,
    quality: RenderQualityHint,
) -> bool {
    motion == MotionPreference::Reduce || quality.skips_animations_by_default()
}

// ============================================================================
// Theme class selection
// ============================================================================

/// The visual theme class the renderer applies. Five canonical
/// classes the integration's theme system implements palettes for.
/// Default = `Standard` (operator's frankenterm.toml `[theme]`
/// chooses the actual palette inside the class).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ThemeClass {
    /// Light palette + standard contrast.
    Light,
    /// Dark palette + standard contrast.
    Dark,
    /// Light palette + high contrast.
    HighContrastLight,
    /// Dark palette + high contrast.
    HighContrastDark,
    /// Operator hasn't expressed a preference and OS hasn't either
    /// — fall back to whatever frankenterm.toml `[theme] default`
    /// names. The integration's theme resolver maps `Standard` to
    /// the configured default.
    #[default]
    Standard,
}

/// Pure-logic theme-class selector. The integration layer feeds in
/// the live preferences; this returns the class to use.
///
/// Composition rules:
/// - `Custom` contrast on either scheme → high-contrast variant.
/// - `More` contrast → high-contrast variant (matches user intent;
///   `More` is the cross-platform "I want more separation" signal).
/// - `Less` contrast → standard variant (lower contrast doesn't
///   warrant a separate class; the operator's regular palette is
///   the closest match).
/// - `NoPreference` contrast → standard variant.
/// - `NoPreference` colour-scheme + standard contrast → `Standard`
///   (let frankenterm.toml decide).
/// - Light / Dark scheme → corresponding standard or high-contrast
///   variant.
#[must_use]
pub fn select_theme_class(prefs: AccessibilityPreferences) -> ThemeClass {
    let high_contrast = matches!(
        prefs.contrast,
        ContrastPreference::More | ContrastPreference::Custom
    );
    match (prefs.color_scheme, high_contrast) {
        (ColorSchemePreference::Light, true) => ThemeClass::HighContrastLight,
        (ColorSchemePreference::Dark, true) => ThemeClass::HighContrastDark,
        (ColorSchemePreference::Light, false) => ThemeClass::Light,
        (ColorSchemePreference::Dark, false) => ThemeClass::Dark,
        (ColorSchemePreference::NoPreference, true) => {
            // Operator hasn't pinned a scheme, but contrast is
            // elevated. Default to high-contrast dark since dark
            // palettes are typically the higher-contrast canvas.
            ThemeClass::HighContrastDark
        }
        (ColorSchemePreference::NoPreference, false) => ThemeClass::Standard,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // Defaults
    // ----------------------------------------------------------------

    #[test]
    fn defaults_are_no_preference() {
        let p = AccessibilityPreferences::default();
        assert_eq!(p.motion, MotionPreference::NoPreference);
        assert_eq!(p.contrast, ContrastPreference::NoPreference);
        assert_eq!(p.color_scheme, ColorSchemePreference::NoPreference);
    }

    #[test]
    fn render_quality_default_is_standard() {
        assert_eq!(RenderQualityHint::default(), RenderQualityHint::Standard);
    }

    // ----------------------------------------------------------------
    // diff / has_changed
    // ----------------------------------------------------------------

    #[test]
    fn diff_empty_when_identical() {
        let a = AccessibilityPreferences::default();
        let b = AccessibilityPreferences::default();
        assert!(a.diff(&b).is_empty());
        assert!(!a.has_changed(&b));
    }

    #[test]
    fn diff_emits_motion_change() {
        let a = AccessibilityPreferences::default();
        let b = AccessibilityPreferences {
            motion: MotionPreference::Reduce,
            ..a
        };
        let changes = a.diff(&b);
        assert_eq!(changes.len(), 1);
        match changes[0] {
            PreferenceChange::Motion { old, new } => {
                assert_eq!(old, MotionPreference::NoPreference);
                assert_eq!(new, MotionPreference::Reduce);
            }
            other => panic!("expected Motion change, got {other:?}"),
        }
    }

    #[test]
    fn diff_emits_contrast_change() {
        let a = AccessibilityPreferences::default();
        let b = AccessibilityPreferences {
            contrast: ContrastPreference::More,
            ..a
        };
        let changes = a.diff(&b);
        assert_eq!(changes.len(), 1);
        assert!(matches!(
            changes[0],
            PreferenceChange::Contrast {
                new: ContrastPreference::More,
                ..
            }
        ));
    }

    #[test]
    fn diff_emits_color_scheme_change() {
        let a = AccessibilityPreferences::default();
        let b = AccessibilityPreferences {
            color_scheme: ColorSchemePreference::Dark,
            ..a
        };
        let changes = a.diff(&b);
        assert_eq!(changes.len(), 1);
        assert!(matches!(
            changes[0],
            PreferenceChange::ColorScheme {
                new: ColorSchemePreference::Dark,
                ..
            }
        ));
    }

    #[test]
    fn diff_emits_all_three_when_all_changed() {
        let a = AccessibilityPreferences::default();
        let b = AccessibilityPreferences {
            motion: MotionPreference::Reduce,
            contrast: ContrastPreference::More,
            color_scheme: ColorSchemePreference::Dark,
        };
        let changes = a.diff(&b);
        assert_eq!(changes.len(), 3);
    }

    #[test]
    fn has_changed_short_circuits_on_motion() {
        let a = AccessibilityPreferences::default();
        let b = AccessibilityPreferences {
            motion: MotionPreference::Reduce,
            ..a
        };
        assert!(a.has_changed(&b));
    }

    // ----------------------------------------------------------------
    // PreferenceChange::live_update_decision
    // ----------------------------------------------------------------

    #[test]
    fn motion_change_applies_immediately() {
        let c = PreferenceChange::Motion {
            old: MotionPreference::NoPreference,
            new: MotionPreference::Reduce,
        };
        assert_eq!(c.live_update_decision(), LiveUpdateDecision::ApplyImmediately);
    }

    #[test]
    fn contrast_change_applies_immediately() {
        let c = PreferenceChange::Contrast {
            old: ContrastPreference::NoPreference,
            new: ContrastPreference::More,
        };
        assert_eq!(c.live_update_decision(), LiveUpdateDecision::ApplyImmediately);
    }

    #[test]
    fn color_scheme_change_applies_immediately() {
        let c = PreferenceChange::ColorScheme {
            old: ColorSchemePreference::Light,
            new: ColorSchemePreference::Dark,
        };
        assert_eq!(c.live_update_decision(), LiveUpdateDecision::ApplyImmediately);
    }

    // ----------------------------------------------------------------
    // RenderQualityHint
    // ----------------------------------------------------------------

    #[test]
    fn render_quality_draft_skips_animations_by_default() {
        assert!(RenderQualityHint::Draft.skips_animations_by_default());
        assert!(!RenderQualityHint::Standard.skips_animations_by_default());
        assert!(!RenderQualityHint::Fancy.skips_animations_by_default());
    }

    // ----------------------------------------------------------------
    // should_skip_animation
    // ----------------------------------------------------------------

    #[test]
    fn skip_animation_when_motion_reduce_in_any_quality() {
        for q in [RenderQualityHint::Standard, RenderQualityHint::Fancy, RenderQualityHint::Draft] {
            assert!(
                should_skip_animation(MotionPreference::Reduce, q),
                "Reduce + {q:?} must skip animations"
            );
        }
    }

    #[test]
    fn skip_animation_in_draft_with_no_preference() {
        assert!(should_skip_animation(
            MotionPreference::NoPreference,
            RenderQualityHint::Draft
        ));
    }

    #[test]
    fn run_animation_in_standard_or_fancy_with_no_preference() {
        assert!(!should_skip_animation(
            MotionPreference::NoPreference,
            RenderQualityHint::Standard
        ));
        assert!(!should_skip_animation(
            MotionPreference::NoPreference,
            RenderQualityHint::Fancy
        ));
    }

    // ----------------------------------------------------------------
    // select_theme_class
    // ----------------------------------------------------------------

    #[test]
    fn theme_default_is_standard_for_no_prefs() {
        let prefs = AccessibilityPreferences::default();
        assert_eq!(select_theme_class(prefs), ThemeClass::Standard);
    }

    #[test]
    fn theme_light_when_scheme_light_no_high_contrast() {
        let prefs = AccessibilityPreferences {
            color_scheme: ColorSchemePreference::Light,
            ..AccessibilityPreferences::default()
        };
        assert_eq!(select_theme_class(prefs), ThemeClass::Light);
    }

    #[test]
    fn theme_dark_when_scheme_dark_no_high_contrast() {
        let prefs = AccessibilityPreferences {
            color_scheme: ColorSchemePreference::Dark,
            ..AccessibilityPreferences::default()
        };
        assert_eq!(select_theme_class(prefs), ThemeClass::Dark);
    }

    #[test]
    fn theme_high_contrast_light_when_scheme_light_more_contrast() {
        let prefs = AccessibilityPreferences {
            color_scheme: ColorSchemePreference::Light,
            contrast: ContrastPreference::More,
            ..AccessibilityPreferences::default()
        };
        assert_eq!(select_theme_class(prefs), ThemeClass::HighContrastLight);
    }

    #[test]
    fn theme_high_contrast_dark_when_scheme_dark_more_contrast() {
        let prefs = AccessibilityPreferences {
            color_scheme: ColorSchemePreference::Dark,
            contrast: ContrastPreference::More,
            ..AccessibilityPreferences::default()
        };
        assert_eq!(select_theme_class(prefs), ThemeClass::HighContrastDark);
    }

    #[test]
    fn theme_high_contrast_dark_when_no_scheme_pref_but_custom_contrast() {
        // Operator hasn't pinned a scheme; system high-contrast theme
        // is on (Custom). Substrate defaults to high-contrast dark
        // (the higher-contrast canvas).
        let prefs = AccessibilityPreferences {
            contrast: ContrastPreference::Custom,
            ..AccessibilityPreferences::default()
        };
        assert_eq!(select_theme_class(prefs), ThemeClass::HighContrastDark);
    }

    #[test]
    fn theme_less_contrast_falls_through_to_standard_variants() {
        // Less contrast doesn't trigger high-contrast; falls through
        // to the regular Light/Dark/Standard.
        let prefs = AccessibilityPreferences {
            color_scheme: ColorSchemePreference::Light,
            contrast: ContrastPreference::Less,
            ..AccessibilityPreferences::default()
        };
        assert_eq!(select_theme_class(prefs), ThemeClass::Light);
    }

    #[test]
    fn theme_motion_does_not_affect_theme_class() {
        // Motion preference is orthogonal to theme.
        let a = select_theme_class(AccessibilityPreferences {
            motion: MotionPreference::NoPreference,
            ..AccessibilityPreferences::default()
        });
        let b = select_theme_class(AccessibilityPreferences {
            motion: MotionPreference::Reduce,
            ..AccessibilityPreferences::default()
        });
        assert_eq!(a, b);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_user_enables_dark_mode_mid_session() {
        // Session start: no prefs.
        let mut current = AccessibilityPreferences::default();
        assert_eq!(select_theme_class(current), ThemeClass::Standard);

        // OS flips to Dark.
        let next = AccessibilityPreferences {
            color_scheme: ColorSchemePreference::Dark,
            ..current
        };
        let changes = current.diff(&next);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].live_update_decision(), LiveUpdateDecision::ApplyImmediately);
        current = next;
        assert_eq!(select_theme_class(current), ThemeClass::Dark);
    }

    #[test]
    fn scenario_user_enables_reduce_motion_in_draft_mode_no_op() {
        // Live-resize is in flight (Draft mode); animations already
        // skipped. User toggles reduce-motion. Predicate value
        // doesn't change but the integration still applies the new
        // preference for the next non-Draft frame.
        let prefs_before = AccessibilityPreferences::default();
        let prefs_after = AccessibilityPreferences {
            motion: MotionPreference::Reduce,
            ..prefs_before
        };
        // Both before and after: Draft skips animations.
        assert!(should_skip_animation(prefs_before.motion, RenderQualityHint::Draft));
        assert!(should_skip_animation(prefs_after.motion, RenderQualityHint::Draft));
        // After live-resize ends and quality returns to Standard:
        // before would NOT skip; after WILL skip.
        assert!(!should_skip_animation(prefs_before.motion, RenderQualityHint::Standard));
        assert!(should_skip_animation(prefs_after.motion, RenderQualityHint::Standard));
    }

    #[test]
    fn scenario_high_contrast_dark_session_full_pipeline() {
        // User has dark theme + high contrast + reduce motion.
        let prefs = AccessibilityPreferences {
            motion: MotionPreference::Reduce,
            contrast: ContrastPreference::More,
            color_scheme: ColorSchemePreference::Dark,
        };
        // Theme = HighContrastDark.
        assert_eq!(select_theme_class(prefs), ThemeClass::HighContrastDark);
        // Animations skipped in any quality.
        for q in [RenderQualityHint::Standard, RenderQualityHint::Fancy, RenderQualityHint::Draft] {
            assert!(should_skip_animation(prefs.motion, q));
        }
    }

    #[test]
    fn scenario_three_axis_change_emits_three_events() {
        // Power user toggles all 3 prefs at once (e.g. via a system
        // accessibility shortcut).
        let before = AccessibilityPreferences::default();
        let after = AccessibilityPreferences {
            motion: MotionPreference::Reduce,
            contrast: ContrastPreference::Custom,
            color_scheme: ColorSchemePreference::Dark,
        };
        let changes = before.diff(&after);
        assert_eq!(changes.len(), 3);
        // All three apply immediately.
        for change in &changes {
            assert_eq!(change.live_update_decision(), LiveUpdateDecision::ApplyImmediately);
        }
    }
}
