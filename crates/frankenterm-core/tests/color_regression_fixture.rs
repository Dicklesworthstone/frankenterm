//! Color-management regression fixture (`ft-mpc9b.10.3`).
//!
//! Foundation slice for the per-display ICC integration lane. Until
//! the per-platform detector beads land (one each for macOS
//! ColorSync, Linux mutter/portal, Windows MPCC), this fixture
//! operates in **contract mode**: it pins the canonical
//! `(expected_color, actual_color)` pairs that a correctly-color-
//! managed renderer would produce, asserts the ΔE2000 invariants
//! against them, and writes the structured-log corpus the
//! integration beads' real recorders will be compared against.
//!
//! ## Goldens
//!
//! `crates/frankenterm-core/tests/color/golden/<color_space>-<pattern>.jsonl`
//! is the committed baseline. Run with `FT_COLOR_BLESS=1` to
//! regenerate after a deliberate contract change (the test panics
//! with a "blessed; re-run without `FT_COLOR_BLESS`" message so the
//! bless flow is two-step).

use std::path::PathBuf;

use frankenterm_core::color_management::{
    Color, ColorMeasurement, ColorPattern, ColorSpace, IccProfile, SurfaceFormatGamut,
    delta_e_2000, render_measurements_jsonl,
};
use proptest::prelude::*;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("color")
        .join("golden")
}

fn golden_path(space: ColorSpace, pattern: ColorPattern) -> PathBuf {
    golden_dir().join(format!("{}-{}.jsonl", space.slug(), pattern.slug()))
}

fn bless_enabled() -> bool {
    std::env::var("FT_COLOR_BLESS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn ensure_golden_dir_exists() {
    std::fs::create_dir_all(golden_dir()).expect("create golden dir");
}

/// Build the canonical "no drift" measurement set for a pattern: each
/// expected sample compared to itself (ΔE = 0). Future per-platform
/// recorders will replace `actual` with the captured value; the
/// goldens stay stable as the renderer-correct baseline.
fn no_drift_measurements(pattern: ColorPattern) -> Vec<ColorMeasurement> {
    pattern
        .samples()
        .into_iter()
        .enumerate()
        .map(|(i, sample)| ColorMeasurement::new(i as u64 * 10, pattern.slug(), sample, sample))
        .collect()
}

// ============================================================================
// Test 1 — every pattern's no-drift measurement set is perceptually
// identical (ΔE = 0). Guards against an accidental edit to
// `ColorPattern::samples` that breaks reflexivity.
// ============================================================================

#[test]
fn every_pattern_no_drift_is_perceptually_identical() {
    for pattern in ColorPattern::ALL {
        let measurements = no_drift_measurements(*pattern);
        for m in &measurements {
            assert!(
                m.perceptually_identical,
                "{:?} no-drift sample {:?} reported ΔE={} (expected ≤ 1.0)",
                pattern, m.expected, m.delta_e
            );
            assert!(
                m.delta_e < 1e-9,
                "{:?} identity ΔE should be ≈0, got {}",
                pattern,
                m.delta_e
            );
        }
    }
}

// ============================================================================
// Test 2 — golden snapshot per (color_space, pattern) where the
// pattern's authored space matches.
//
// Each pattern's samples carry their authored `ColorSpace`; the
// golden filename uses that space so future Display-P3 / Rec.2020
// patterns get distinct goldens automatically.
// ============================================================================

#[test]
fn golden_srgb_cube_24bit() {
    snapshot_golden(ColorPattern::Cube24Bit);
}

#[test]
fn golden_p3_gamut_probes() {
    snapshot_golden(ColorPattern::P3GamutProbes);
}

#[test]
fn golden_rec2020_gamut_probes() {
    snapshot_golden(ColorPattern::Rec2020GamutProbes);
}

#[test]
fn golden_srgb_gamma_ramp() {
    snapshot_golden(ColorPattern::GammaRamp);
}

fn snapshot_golden(pattern: ColorPattern) {
    let measurements = no_drift_measurements(pattern);
    // Every pattern uses a single internally-consistent color space;
    // the lib unit test `pattern_samples_are_non_empty_and_tagged`
    // pins this.
    let space = pattern.samples()[0].space;
    let rendered = render_measurements_jsonl(&measurements);
    let path = golden_path(space, pattern);

    if bless_enabled() {
        ensure_golden_dir_exists();
        std::fs::write(&path, &rendered).expect("write blessed golden");
        panic!(
            "{}: golden blessed at {}; re-run without FT_COLOR_BLESS to validate",
            pattern.slug(),
            path.display()
        );
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing golden for {pattern:?} at {}: {err} \
             (re-run with FT_COLOR_BLESS=1 to generate)",
            path.display()
        )
    });

    assert_eq!(
        rendered,
        expected,
        "{pattern:?} drifted from golden at {}",
        path.display()
    );
}

// ============================================================================
// Test 3 — sentinel for the "no integration wired yet" state.
//
// The per-display ICC detector and the wide-gamut surface-format
// upgrade both come in follow-on beads. Until then, every
// non-sRGB surface format the classifier sees MUST report
// `wide_gamut_unverified = true`. When an integration lands, this
// test is updated alongside the new contract.
// ============================================================================

#[test]
fn wide_gamut_surface_formats_are_marked_unverified() {
    for format_name in [
        "Rgba16Float",
        "Rgb10a2Unorm",
        "Bgra10a2Unorm",
        "Bgr10a2Unorm",
    ] {
        let cls = SurfaceFormatGamut::classify(format_name);
        assert!(
            cls.wide_gamut_unverified,
            "{format_name}: expected wide_gamut_unverified=true until the \
             per-display ICC integration beads land"
        );
        assert!(cls.color_space.is_wide_gamut());
    }
}

#[test]
fn srgb_surface_formats_are_marked_verified() {
    for format_name in ["Rgba8UnormSrgb", "Bgra8UnormSrgb"] {
        let cls = SurfaceFormatGamut::classify(format_name);
        assert!(
            !cls.wide_gamut_unverified,
            "{format_name}: sRGB surfaces never need ICC verification"
        );
        assert_eq!(cls.color_space, ColorSpace::Srgb);
    }
}

// ============================================================================
// Test 4 — IccProfile default sanity.
// ============================================================================

#[test]
fn icc_profile_default_round_trips() {
    let p = IccProfile::srgb_default();
    let json = serde_json::to_string(&p).unwrap();
    let parsed: IccProfile = serde_json::from_str(&json).unwrap();
    assert_eq!(p, parsed);
}

// ============================================================================
// Test 5 — proptest properties on ΔE2000.
// ============================================================================

prop_compose! {
    fn arb_color()(
        r in 0u8..=255,
        g in 0u8..=255,
        b in 0u8..=255,
        space_idx in 0u8..3,
    ) -> Color {
        let space = match space_idx {
            0 => ColorSpace::Srgb,
            1 => ColorSpace::DisplayP3,
            _ => ColorSpace::Rec2020,
        };
        Color::opaque(r, g, b, space)
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    /// ΔE2000 is non-negative for every pair (excluding NaN, which the
    /// `.max(0.0).sqrt()` clamp in the implementation rules out).
    #[test]
    fn delta_e_is_non_negative(a in arb_color(), b in arb_color()) {
        let de = delta_e_2000(a, b);
        prop_assert!(de.is_finite(), "ΔE produced non-finite for {a:?} vs {b:?}: {de}");
        prop_assert!(de >= 0.0, "ΔE produced negative for {a:?} vs {b:?}: {de}");
    }

    /// ΔE2000 is symmetric for every pair within fp tolerance.
    #[test]
    fn delta_e_is_symmetric_property(a in arb_color(), b in arb_color()) {
        let ab = delta_e_2000(a, b);
        let ba = delta_e_2000(b, a);
        prop_assert!(
            (ab - ba).abs() < 1e-6,
            "ΔE asymmetric for {a:?} vs {b:?}: {ab} vs {ba}"
        );
    }

    /// Identity: a color compared to itself has ΔE ≈ 0.
    #[test]
    fn delta_e_identity_property(a in arb_color()) {
        let de = delta_e_2000(a, a);
        prop_assert!(
            de < 1e-6,
            "identity ΔE for {a:?} should be ≈0, got {de}"
        );
    }

    /// Measurement constructor is total: every (expected, actual) pair
    /// produces a finite, non-negative ΔE.
    #[test]
    fn measurement_ctor_is_total(a in arb_color(), b in arb_color()) {
        let m = ColorMeasurement::new(0, "prop", a, b);
        prop_assert!(m.delta_e.is_finite() && m.delta_e >= 0.0);
        prop_assert_eq!(m.perceptually_identical, m.delta_e <= 1.0);
    }
}
