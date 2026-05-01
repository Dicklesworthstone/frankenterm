//! Subpixel glyph-positioning substrate (ft-2okh0.10).
//!
//! Pure-math substrate for the bead's "fractional X kerning on high-
//! DPI mixed-scaling setups" requirement. The atlas-storage,
//! shader-side X-offset application, and LRU eviction wiring live in
//! the integration crate; this module ships the bin classifier,
//! scale-factor detector, atlas-key extension policy, and the
//! "should we even enable subpixel today?" decision.
//!
//! ## What this module ships
//!
//! - `SubpixelBin` — `Quarter0 / Quarter1 / Quarter2 / Quarter3`
//!   matching the bead's "4 bins per pixel" budget. Total memory
//!   overhead = 4× when enabled, but only on the displays + glyphs
//!   that need it.
//! - `classify_x` — pure-math: given a fractional X position
//!   (`f64`), return the bin. The shader then applies the residual
//!   sub-bin offset for sub-quarter precision (rare; the integration
//!   layer's choice).
//! - `ScaleFactor` — `(numerator, denominator)` rational
//!   representation so 1.25x / 1.5x / 1.75x etc. are exact, not
//!   floating-point approximations.
//! - `is_fractional_scale` — predicate: should the renderer enable
//!   subpixel positioning at all? Returns `false` for 1x, 2x, 3x
//!   integer scales (no benefit, full cost) and `true` otherwise.
//! - `SubpixelGlyphKey` — atlas-key extension. The integration's
//!   stable-atlas (ft-mpc9b.1.1) keys glyphs by `(glyph_id, font_id,
//!   size, ...)`; subpixel adds `bin` so the 4 variants don't
//!   collide.
//! - `SubpixelPolicyConfig` — operator override
//!   `subpixel_positioning = bool` from frankenterm.toml.
//! - `should_enable_subpixel` — pure-logic gate composing user-
//!   override + scale-factor detection + bead's a11y "some users
//!   prefer integer positioning" rule.
//! - `AtlasOverheadEstimate` — `bytes_per_glyph_subpixel = 4 *
//!   bytes_per_glyph_integer`. Pure data so the integration's
//!   memory budget can size the atlas correctly.
//!
//! ## What is deferred to the integration bead (ft-2okh0.10.cont)
//!
//! - 4-variant atlas storage in `frankenterm/window/src/bitmaps/atlas.rs`.
//! - LRU eviction on subpixel-stamped atlas entries
//!   (cross-link ft-2okh0.11 GPU atlas tiered swap).
//! - Shader X-offset application — vertex shader applies
//!   `fract(position.x)` before sampling.
//! - Per-display scale-factor probe (NSScreen.backingScaleFactor on
//!   macOS, GDK / wlr scale on Linux, GetDpiForWindow on Windows).
//! - Visual regression test on the GPU harness (cross-link
//!   ft-ombfl).
//! - Cross-link to ft-mpc9b.10.5 a11y prefs for the operator
//!   override surfaced via [adaptive_fps] / [accessibility].

#![allow(dead_code)]

// ============================================================================
// SubpixelBin
// ============================================================================

/// Bead's "4 bins per pixel: 0/4, 1/4, 2/4, 3/4". Each bin
/// represents the integer-pixel position the glyph rasterises into;
/// the shader applies the residual sub-bin offset (always < 1/4
/// pixel, so visually negligible).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum SubpixelBin {
    /// X position in `[0/4, 1/4)` of the cell — round to 0/4.
    #[default]
    Quarter0,
    /// X position in `[1/4, 2/4)` — round to 1/4.
    Quarter1,
    /// X position in `[2/4, 3/4)` — round to 2/4.
    Quarter2,
    /// X position in `[3/4, 1.0)` — round to 3/4.
    Quarter3,
}

impl SubpixelBin {
    /// Numerator of the bin's fractional position (denominator is 4).
    /// `Quarter0 = 0, Quarter1 = 1, Quarter2 = 2, Quarter3 = 3`.
    #[must_use]
    pub const fn numerator(self) -> u8 {
        match self {
            Self::Quarter0 => 0,
            Self::Quarter1 => 1,
            Self::Quarter2 => 2,
            Self::Quarter3 => 3,
        }
    }

    /// Convert to the offset within a pixel `[0.0, 1.0)`.
    #[must_use]
    pub fn fractional_offset(self) -> f64 {
        f64::from(self.numerator()) / 4.0
    }

    /// Stable iteration order for atlas-pre-rasterisation passes.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Quarter0,
            Self::Quarter1,
            Self::Quarter2,
            Self::Quarter3,
        ]
    }
}

/// Number of subpixel bins per pixel (the bead's budget choice).
pub const BINS_PER_PIXEL: u8 = 4;

/// Classify a fractional X position into a `SubpixelBin`. Pure-math.
///
/// Algorithm:
/// 1. Take `x.fract()` to get the position within the cell `[0.0,
///    1.0)`. Negative inputs are reflected (defensive — should
///    never happen but a flaky probe could produce `-0.001`).
/// 2. Multiply by `BINS_PER_PIXEL = 4` and floor to get the bin
///    index in `[0, 3]`.
/// 3. Map to the enum.
///
/// Non-finite inputs return `Quarter0` defensively.
#[must_use]
pub fn classify_x(x: f64) -> SubpixelBin {
    if !x.is_finite() {
        return SubpixelBin::Quarter0;
    }
    // .fract() returns the signed fractional part; we want the
    // positive remainder. .rem_euclid(1.0) handles negatives.
    let frac = x.rem_euclid(1.0);
    let idx = (frac * f64::from(BINS_PER_PIXEL)).floor() as i32;
    match idx.clamp(0, 3) {
        0 => SubpixelBin::Quarter0,
        1 => SubpixelBin::Quarter1,
        2 => SubpixelBin::Quarter2,
        _ => SubpixelBin::Quarter3,
    }
}

// ============================================================================
// ScaleFactor (rational)
// ============================================================================

/// Display scale factor as a rational so common fractional scales
/// (1.25 = 5/4, 1.5 = 3/2, 1.75 = 7/4) round-trip exactly without
/// floating-point error. Constructor reduces by GCD so `(2, 4)`
/// becomes `(1, 2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScaleFactor {
    /// **Private** per ft-1mktd: previously public, allowing
    /// callers to construct `ScaleFactor { numerator: 1,
    /// denominator: 0 }` (division-by-zero / inf) or
    /// non-canonical forms like `(4, 8)` that break
    /// `PartialEq` + `Hash` against the canonical `(1, 2)`.
    /// Use [`Self::new`] (validates + canonicalises) or the
    /// `ONE_X` / `ONE_25_X` / etc. constants (already
    /// canonical).
    numerator: u32,
    denominator: u32,
}

impl ScaleFactor {
    /// Construct, reducing by GCD. Returns `None` for zero
    /// denominator.
    #[must_use]
    pub fn new(numerator: u32, denominator: u32) -> Option<Self> {
        if denominator == 0 {
            return None;
        }
        let g = gcd_u32(numerator, denominator);
        Some(Self {
            numerator: numerator / g.max(1),
            denominator: denominator / g.max(1),
        })
    }

    /// Read the numerator (always GCD-reduced).
    #[must_use]
    pub fn numerator(&self) -> u32 {
        self.numerator
    }

    /// Read the denominator (always non-zero, GCD-reduced).
    #[must_use]
    pub fn denominator(&self) -> u32 {
        self.denominator
    }

    /// Convenience: 1x = `(1, 1)`.
    pub const ONE_X: Self = Self {
        numerator: 1,
        denominator: 1,
    };
    /// 1.25x = `(5, 4)`.
    pub const ONE_25_X: Self = Self {
        numerator: 5,
        denominator: 4,
    };
    /// 1.5x = `(3, 2)`.
    pub const ONE_5_X: Self = Self {
        numerator: 3,
        denominator: 2,
    };
    /// 1.75x = `(7, 4)`.
    pub const ONE_75_X: Self = Self {
        numerator: 7,
        denominator: 4,
    };
    /// 2x = `(2, 1)`.
    pub const TWO_X: Self = Self {
        numerator: 2,
        denominator: 1,
    };
    /// 3x = `(3, 1)`.
    pub const THREE_X: Self = Self {
        numerator: 3,
        denominator: 1,
    };

    /// Convert to floating-point. `1.25 → 1.25` exactly (the
    /// rational stays exact; the f64 conversion is the only lossy
    /// step). Always finite (denominator guaranteed non-zero
    /// per ft-1mktd).
    #[must_use]
    pub fn as_f64(&self) -> f64 {
        f64::from(self.numerator) / f64::from(self.denominator)
    }

    /// Whether this scale factor is exactly an integer (i.e. the
    /// renderer can skip subpixel positioning entirely without
    /// quality loss).
    #[must_use]
    pub fn is_integer(&self) -> bool {
        self.denominator == 1
    }
}

/// GCD via Stein's algorithm (binary GCD). Pure-math, deterministic.
const fn gcd_u32(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

/// Whether the renderer should enable subpixel positioning given
/// this scale factor. Integer scales (1x / 2x / 3x / etc.) get
/// nothing from subpixel positioning so the substrate disables it
/// — saves 4× atlas memory.
#[must_use]
pub fn is_fractional_scale(scale: ScaleFactor) -> bool {
    !scale.is_integer()
}

// ============================================================================
// Atlas key extension
// ============================================================================

/// Atlas-key extension for subpixel-positioned glyphs. The
/// integration's stable atlas (ft-mpc9b.1.1) keys glyphs on
/// `(glyph_id, font_id, size, ...)`; this struct carries the
/// `(base_key_hash, bin)` pair that disambiguates the 4 variants.
///
/// `base_key_hash` is the atlas's existing key reduced to a `u64`
/// (FNV-1a or whatever the atlas uses); the substrate doesn't need
/// to know the structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubpixelGlyphKey {
    pub base_key_hash: u64,
    pub bin: SubpixelBin,
}

impl SubpixelGlyphKey {
    #[must_use]
    pub const fn new(base_key_hash: u64, bin: SubpixelBin) -> Self {
        Self { base_key_hash, bin }
    }

    /// Whether this key represents the canonical (Quarter0)
    /// variant — useful for the integration's "always store
    /// Quarter0; only store other quarters when fractional scale
    /// detected" pre-pass.
    #[must_use]
    pub fn is_canonical(&self) -> bool {
        matches!(self.bin, SubpixelBin::Quarter0)
    }
}

// ============================================================================
// Operator override
// ============================================================================

/// Operator's frankenterm.toml override per the bead's
/// "user override config flag `subpixel_positioning = false`" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SubpixelPolicyConfig {
    /// Auto-enable when the active display has fractional scale.
    /// Default.
    #[default]
    Auto,
    /// Force on regardless of scale (mostly for testing — wastes
    /// 4× atlas on integer scales).
    ForceOn,
    /// Force off — operator prefers integer-snapping (the bead's
    /// a11y note: "some users prefer integer positioning").
    ForceOff,
}

/// Whether the renderer should enable subpixel positioning given
/// the operator's config + the active display's scale factor. Pure
/// predicate.
#[must_use]
pub fn should_enable_subpixel(config: SubpixelPolicyConfig, scale: ScaleFactor) -> bool {
    match config {
        SubpixelPolicyConfig::ForceOn => true,
        SubpixelPolicyConfig::ForceOff => false,
        SubpixelPolicyConfig::Auto => is_fractional_scale(scale),
    }
}

// ============================================================================
// Atlas overhead estimate
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtlasOverheadEstimate {
    pub bytes_per_glyph_integer: u64,
    pub bytes_per_glyph_subpixel: u64,
}

impl AtlasOverheadEstimate {
    /// Per the bead, subpixel = 4× the integer-positioning case.
    #[must_use]
    pub fn from_integer_size(bytes_per_glyph_integer: u64) -> Self {
        Self {
            bytes_per_glyph_integer,
            bytes_per_glyph_subpixel: bytes_per_glyph_integer
                .saturating_mul(u64::from(BINS_PER_PIXEL)),
        }
    }

    /// Total atlas size given a glyph count under each policy.
    #[must_use]
    pub fn total_bytes(&self, glyph_count: u64, subpixel: bool) -> u64 {
        let per = if subpixel {
            self.bytes_per_glyph_subpixel
        } else {
            self.bytes_per_glyph_integer
        };
        per.saturating_mul(glyph_count)
    }

    /// Overhead ratio when subpixel is on. Always 4x per the bead.
    #[must_use]
    pub const fn overhead_factor(&self) -> u8 {
        BINS_PER_PIXEL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    // ----------------------------------------------------------------
    // SubpixelBin
    // ----------------------------------------------------------------

    #[test]
    fn bin_default_is_quarter_0() {
        assert_eq!(SubpixelBin::default(), SubpixelBin::Quarter0);
    }

    #[test]
    fn bin_numerator_matches_quarter() {
        assert_eq!(SubpixelBin::Quarter0.numerator(), 0);
        assert_eq!(SubpixelBin::Quarter1.numerator(), 1);
        assert_eq!(SubpixelBin::Quarter2.numerator(), 2);
        assert_eq!(SubpixelBin::Quarter3.numerator(), 3);
    }

    #[test]
    fn bin_fractional_offset() {
        assert!(approx(SubpixelBin::Quarter0.fractional_offset(), 0.0));
        assert!(approx(SubpixelBin::Quarter1.fractional_offset(), 0.25));
        assert!(approx(SubpixelBin::Quarter2.fractional_offset(), 0.5));
        assert!(approx(SubpixelBin::Quarter3.fractional_offset(), 0.75));
    }

    #[test]
    fn bin_all_iter_has_4_in_order() {
        let all = SubpixelBin::all();
        assert_eq!(all.len(), BINS_PER_PIXEL as usize);
        assert_eq!(all[0], SubpixelBin::Quarter0);
        assert_eq!(all[3], SubpixelBin::Quarter3);
    }

    // ----------------------------------------------------------------
    // classify_x
    // ----------------------------------------------------------------

    #[test]
    fn classify_x_at_zero_yields_quarter0() {
        assert_eq!(classify_x(0.0), SubpixelBin::Quarter0);
    }

    #[test]
    fn classify_x_at_quarter_boundaries() {
        assert_eq!(classify_x(0.0), SubpixelBin::Quarter0);
        assert_eq!(classify_x(0.25), SubpixelBin::Quarter1);
        assert_eq!(classify_x(0.5), SubpixelBin::Quarter2);
        assert_eq!(classify_x(0.75), SubpixelBin::Quarter3);
    }

    #[test]
    fn classify_x_within_each_bin() {
        assert_eq!(classify_x(0.1), SubpixelBin::Quarter0);
        assert_eq!(classify_x(0.24), SubpixelBin::Quarter0);
        assert_eq!(classify_x(0.26), SubpixelBin::Quarter1);
        assert_eq!(classify_x(0.49), SubpixelBin::Quarter1);
        assert_eq!(classify_x(0.51), SubpixelBin::Quarter2);
        assert_eq!(classify_x(0.74), SubpixelBin::Quarter2);
        assert_eq!(classify_x(0.76), SubpixelBin::Quarter3);
        assert_eq!(classify_x(0.99), SubpixelBin::Quarter3);
    }

    #[test]
    fn classify_x_integer_x_yields_quarter0() {
        // Integer pixel positions all go to Quarter0.
        for n in 1..=100 {
            assert_eq!(classify_x(n as f64), SubpixelBin::Quarter0);
        }
    }

    #[test]
    fn classify_x_negative_handled_via_rem_euclid() {
        // -0.25 has fractional part 0.75 (rem_euclid).
        assert_eq!(classify_x(-0.25), SubpixelBin::Quarter3);
        // -0.5 has fractional part 0.5.
        assert_eq!(classify_x(-0.5), SubpixelBin::Quarter2);
    }

    #[test]
    fn classify_x_non_finite_defaults_to_quarter0() {
        assert_eq!(classify_x(f64::NAN), SubpixelBin::Quarter0);
        assert_eq!(classify_x(f64::INFINITY), SubpixelBin::Quarter0);
        assert_eq!(classify_x(f64::NEG_INFINITY), SubpixelBin::Quarter0);
    }

    // ----------------------------------------------------------------
    // ScaleFactor
    // ----------------------------------------------------------------

    #[test]
    fn scale_factor_new_reduces_by_gcd() {
        let s = ScaleFactor::new(2, 4).unwrap();
        assert_eq!(s, ScaleFactor::new(1, 2).unwrap());
        let s = ScaleFactor::new(10, 8).unwrap();
        assert_eq!(s, ScaleFactor::new(5, 4).unwrap());
    }

    #[test]
    fn scale_factor_zero_denominator_rejected() {
        assert!(ScaleFactor::new(1, 0).is_none());
    }

    #[test]
    fn scale_factor_constants_match_common_scales() {
        assert!(approx(ScaleFactor::ONE_X.as_f64(), 1.0));
        assert!(approx(ScaleFactor::ONE_25_X.as_f64(), 1.25));
        assert!(approx(ScaleFactor::ONE_5_X.as_f64(), 1.5));
        assert!(approx(ScaleFactor::ONE_75_X.as_f64(), 1.75));
        assert!(approx(ScaleFactor::TWO_X.as_f64(), 2.0));
        assert!(approx(ScaleFactor::THREE_X.as_f64(), 3.0));
    }

    #[test]
    fn scale_factor_is_integer() {
        assert!(ScaleFactor::ONE_X.is_integer());
        assert!(ScaleFactor::TWO_X.is_integer());
        assert!(ScaleFactor::THREE_X.is_integer());
        assert!(!ScaleFactor::ONE_25_X.is_integer());
        assert!(!ScaleFactor::ONE_5_X.is_integer());
        assert!(!ScaleFactor::ONE_75_X.is_integer());
    }

    // ----------------------------------------------------------------
    // is_fractional_scale
    // ----------------------------------------------------------------

    #[test]
    fn fractional_scale_detected_for_common_high_dpi() {
        assert!(is_fractional_scale(ScaleFactor::ONE_25_X));
        assert!(is_fractional_scale(ScaleFactor::ONE_5_X));
        assert!(is_fractional_scale(ScaleFactor::ONE_75_X));
    }

    #[test]
    fn fractional_scale_rejected_for_integer_scales() {
        assert!(!is_fractional_scale(ScaleFactor::ONE_X));
        assert!(!is_fractional_scale(ScaleFactor::TWO_X));
        assert!(!is_fractional_scale(ScaleFactor::THREE_X));
    }

    // ----------------------------------------------------------------
    // SubpixelGlyphKey
    // ----------------------------------------------------------------

    #[test]
    fn glyph_key_round_trips() {
        let k = SubpixelGlyphKey::new(0xDEAD_BEEF, SubpixelBin::Quarter2);
        assert_eq!(k.base_key_hash, 0xDEAD_BEEF);
        assert_eq!(k.bin, SubpixelBin::Quarter2);
    }

    #[test]
    fn glyph_key_canonical_only_for_quarter0() {
        assert!(SubpixelGlyphKey::new(0, SubpixelBin::Quarter0).is_canonical());
        assert!(!SubpixelGlyphKey::new(0, SubpixelBin::Quarter1).is_canonical());
        assert!(!SubpixelGlyphKey::new(0, SubpixelBin::Quarter2).is_canonical());
        assert!(!SubpixelGlyphKey::new(0, SubpixelBin::Quarter3).is_canonical());
    }

    #[test]
    fn glyph_key_4_variants_distinct_for_same_base() {
        let base = 0x1234_5678;
        let mut seen = Vec::new();
        for &bin in SubpixelBin::all() {
            let k = SubpixelGlyphKey::new(base, bin);
            assert!(!seen.contains(&k), "duplicate key for {bin:?}");
            seen.push(k);
        }
        assert_eq!(seen.len(), 4);
    }

    // ----------------------------------------------------------------
    // SubpixelPolicyConfig + should_enable_subpixel
    // ----------------------------------------------------------------

    #[test]
    fn policy_default_is_auto() {
        assert_eq!(SubpixelPolicyConfig::default(), SubpixelPolicyConfig::Auto);
    }

    #[test]
    fn auto_enables_only_for_fractional_scales() {
        for scale in [
            ScaleFactor::ONE_25_X,
            ScaleFactor::ONE_5_X,
            ScaleFactor::ONE_75_X,
        ] {
            assert!(should_enable_subpixel(SubpixelPolicyConfig::Auto, scale));
        }
        for scale in [ScaleFactor::ONE_X, ScaleFactor::TWO_X, ScaleFactor::THREE_X] {
            assert!(!should_enable_subpixel(SubpixelPolicyConfig::Auto, scale));
        }
    }

    #[test]
    fn force_on_enables_for_integer_scales_too() {
        for scale in [ScaleFactor::ONE_X, ScaleFactor::TWO_X, ScaleFactor::THREE_X] {
            assert!(should_enable_subpixel(SubpixelPolicyConfig::ForceOn, scale));
        }
    }

    #[test]
    fn force_off_disables_even_for_fractional() {
        for scale in [
            ScaleFactor::ONE_25_X,
            ScaleFactor::ONE_5_X,
            ScaleFactor::ONE_75_X,
        ] {
            assert!(!should_enable_subpixel(
                SubpixelPolicyConfig::ForceOff,
                scale
            ));
        }
    }

    // ----------------------------------------------------------------
    // AtlasOverheadEstimate
    // ----------------------------------------------------------------

    #[test]
    fn overhead_4x_per_bead() {
        let e = AtlasOverheadEstimate::from_integer_size(1024);
        assert_eq!(e.bytes_per_glyph_integer, 1024);
        assert_eq!(e.bytes_per_glyph_subpixel, 4096);
        assert_eq!(e.overhead_factor(), 4);
    }

    #[test]
    fn overhead_total_bytes_with_glyph_count() {
        let e = AtlasOverheadEstimate::from_integer_size(1024);
        // 1000 glyphs × 1024 bytes integer = 1 MiB.
        assert_eq!(e.total_bytes(1000, false), 1_024_000);
        // Same 1000 glyphs subpixel = 4 MiB.
        assert_eq!(e.total_bytes(1000, true), 4_096_000);
    }

    #[test]
    fn overhead_saturating_on_huge_inputs() {
        // u64-saturating multiplication shouldn't panic.
        let e = AtlasOverheadEstimate::from_integer_size(u64::MAX / 2);
        let _ = e.total_bytes(1_000_000, true);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_macos_retina_2x_no_subpixel_needed() {
        // Retina = exactly 2x; no need for subpixel positioning.
        let scale = ScaleFactor::TWO_X;
        assert!(!should_enable_subpixel(SubpixelPolicyConfig::Auto, scale));
        // No 4× atlas overhead.
    }

    #[test]
    fn scenario_linux_125_pct_scaling_enables_subpixel() {
        // GNOME-on-Wayland 125% — the bead's headline target.
        let scale = ScaleFactor::ONE_25_X;
        assert!(should_enable_subpixel(SubpixelPolicyConfig::Auto, scale));
        let e = AtlasOverheadEstimate::from_integer_size(2048);
        // 5000 glyphs × 4 bins × 2048 bytes = 40 MiB.
        assert_eq!(e.total_bytes(5000, true), 40_960_000);
    }

    #[test]
    fn scenario_a11y_user_force_off_keeps_integer_positioning() {
        // Bead's a11y note: "some users prefer integer positioning".
        // ForceOff respects that even on fractional scales.
        let scale = ScaleFactor::ONE_5_X;
        assert!(!should_enable_subpixel(
            SubpixelPolicyConfig::ForceOff,
            scale
        ));
    }

    #[test]
    fn scenario_glyph_position_walk_classifies_4_bins() {
        // Walk a glyph along x = 0.0, 0.125, 0.25, ... up to 1.0
        // and confirm each lands in the expected bin.
        let positions = [
            (0.0, SubpixelBin::Quarter0),
            (0.125, SubpixelBin::Quarter0),
            (0.25, SubpixelBin::Quarter1),
            (0.375, SubpixelBin::Quarter1),
            (0.5, SubpixelBin::Quarter2),
            (0.625, SubpixelBin::Quarter2),
            (0.75, SubpixelBin::Quarter3),
            (0.875, SubpixelBin::Quarter3),
            (1.0, SubpixelBin::Quarter0), // wraps back to Quarter0
        ];
        for (x, expected) in positions {
            assert_eq!(classify_x(x), expected, "x={x}");
        }
    }

    #[test]
    fn scenario_scale_factor_arithmetic_round_trips() {
        // Common high-DPI fractional scales reduce correctly.
        // 100/80 = 5/4 = 1.25
        let s = ScaleFactor::new(100, 80).unwrap();
        assert_eq!(s, ScaleFactor::ONE_25_X);
        // 6/4 = 3/2 = 1.5
        let s = ScaleFactor::new(6, 4).unwrap();
        assert_eq!(s, ScaleFactor::ONE_5_X);
        // 200/100 = 2/1 = 2x
        let s = ScaleFactor::new(200, 100).unwrap();
        assert_eq!(s, ScaleFactor::TWO_X);
    }

    /// ft-1mktd regression guard: ScaleFactor fields are private
    /// so callers cannot construct denominator=0 or non-canonical
    /// forms. new() is the only entry point that takes arbitrary
    /// (n, d) pairs.
    #[test]
    fn scale_factor_new_rejects_zero_denominator() {
        assert!(ScaleFactor::new(1, 0).is_none());
        assert!(ScaleFactor::new(0, 0).is_none());
        assert!(ScaleFactor::new(1000, 0).is_none());
    }

    #[test]
    fn scale_factor_new_canonicalises_via_gcd() {
        // (4, 8) and (1, 2) are semantically equal; both reduce to
        // (1, 2). PartialEq + Hash agree only after canonicalisation.
        let a = ScaleFactor::new(4, 8).unwrap();
        let b = ScaleFactor::new(1, 2).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.numerator(), 1);
        assert_eq!(a.denominator(), 2);
    }

    #[test]
    fn scale_factor_getters_return_canonical_form() {
        let s = ScaleFactor::new(100, 80).unwrap();
        assert_eq!(s.numerator(), 5);
        assert_eq!(s.denominator(), 4);
        assert!(!s.is_integer());

        let two = ScaleFactor::TWO_X;
        assert_eq!(two.numerator(), 2);
        assert_eq!(two.denominator(), 1);
        assert!(two.is_integer());
    }

    #[test]
    fn scale_factor_as_f64_always_finite() {
        // Pre-fix risk: ScaleFactor { numerator: 1, denominator: 0 }
        // → as_f64() = inf. Now structurally impossible — every
        // construction path (new + ONE_X + TWO_X + …) guarantees
        // denominator >= 1.
        for s in [
            ScaleFactor::ONE_X,
            ScaleFactor::ONE_25_X,
            ScaleFactor::ONE_5_X,
            ScaleFactor::ONE_75_X,
            ScaleFactor::TWO_X,
            ScaleFactor::THREE_X,
            ScaleFactor::new(7, 13).unwrap(),
        ] {
            let v = s.as_f64();
            assert!(v.is_finite(), "as_f64 must be finite, got {v}");
            assert!(v > 0.0, "as_f64 must be positive, got {v}");
        }
    }
}
