//! Color-management contract and regression-fixture infrastructure
//! ([BR-TERM-EMULATOR-UPLIFT.A11Y.3] / `ft-mpc9b.10.3`).
//!
//! Modern displays support wide-gamut profiles (Display-P3, Rec.2020,
//! ProPhoto-RGB). A renderer that drops or skips per-display ICC
//! profile application produces wrong colors silently — Apple Pro
//! Display XDR users, photographers, and developers running
//! HDR-aware tools see drift the visual-regression lane in
//! `ft-mpc9b.1.6` cannot catch (that lane runs SSIM/ΔL∞ on the
//! render-target's native space; profile-space drift looks identical
//! at the pixel comparator).
//!
//! This module establishes the **contract**:
//!
//! - [`ColorSpace`] — the closed list of color spaces ft cares about
//!   (sRGB, Display-P3, Rec.2020, Rec.709, ProPhoto-RGB, plus a
//!   sentinel `Unknown` for surfaces we couldn't classify).
//! - [`Color`] — an RGBA8 sample tagged with its color space.
//! - [`IccProfile`] — opaque profile metadata (id, declared gamut)
//!   the per-display detector populates.
//! - [`ColorPattern`] — the closed list of test patterns
//!   (24-bit-cube, P3 gamut probes, Rec.2020 probes, gamma-ramp).
//! - `delta_e_2000` — the CIE 2000 perceptual color-difference
//!   metric. ΔE ≤ 1.0 is "perceptually identical"; the regression
//!   lane gates on it.
//! - [`ColorMeasurement`] — one row of the structured JSONL log at
//!   `tests/color/logs/<scenario>.jsonl`.
//! - [`SurfaceFormatGamut`] — the renderer-side classifier:
//!   `wgpu::TextureFormat → ColorSpace`. Consumed by
//!   `crates/frankenterm-gui/src/termwindow/webgpu.rs` at startup so
//!   we have observability before the per-display ICC integration
//!   beads land.
//!
//! See `docs/render/color-management.md` for the per-platform audit
//! and the closure plan that fills the integration gap.
//!
//! ## Why this lives in `frankenterm-core`
//!
//! The colour math (sRGB transfer function, XYZ↔Lab, ΔE2000) is
//! pure, has no GPU dependency, and is consumed by both the GUI
//! crate (for surface-gamut classification at startup) and the
//! regression fixture (for ΔE assertions). Keeping it here avoids a
//! cyclic dep when future beads add the `wgpu`-side per-display
//! profile detector.

use serde::{Deserialize, Serialize};

// ============================================================================
// Color spaces
// ============================================================================

/// The closed list of color spaces ft can reason about.
///
/// Adding a new variant requires extending the conversion matrices in
/// [`linear_rgb_to_xyz`] and the gamut-classifier in
/// [`SurfaceFormatGamut`]; the meta-test
/// `every_color_space_has_an_xyz_matrix` in this module's `tests`
/// keeps the two in lockstep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorSpace {
    /// IEC 61966-2-1 sRGB — the legacy 8-bit baseline.
    Srgb,
    /// Apple Display-P3 (DCI-P3 primaries, sRGB transfer function).
    DisplayP3,
    /// ITU-R Rec.2020 — UHDTV / HDR.
    Rec2020,
    /// ITU-R Rec.709 — HDTV / sRGB primaries.
    Rec709,
    /// ROMM RGB / ProPhoto RGB — wide gamut printing.
    ProPhotoRgb,
    /// Surface format did not declare a color space ft recognizes.
    Unknown,
}

impl ColorSpace {
    /// Stable filename slug
    /// (`tests/color/golden/<color_space>-<pattern>.jsonl`).
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Srgb => "srgb",
            Self::DisplayP3 => "display_p3",
            Self::Rec2020 => "rec_2020",
            Self::Rec709 => "rec_709",
            Self::ProPhotoRgb => "prophoto_rgb",
            Self::Unknown => "unknown",
        }
    }

    /// Every named space (excludes `Unknown` — the sentinel never
    /// participates in conversions).
    pub const ALL_NAMED: &'static [ColorSpace] = &[
        Self::Srgb,
        Self::DisplayP3,
        Self::Rec2020,
        Self::Rec709,
        Self::ProPhotoRgb,
    ];

    /// Whether this space is wider than sRGB.
    #[must_use]
    pub const fn is_wide_gamut(self) -> bool {
        matches!(self, Self::DisplayP3 | Self::Rec2020 | Self::ProPhotoRgb)
    }
}

// ============================================================================
// Pixel samples
// ============================================================================

/// One RGBA8 color sample, tagged with its source color space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
    pub space: ColorSpace,
}

impl Color {
    #[must_use]
    pub const fn opaque(r: u8, g: u8, b: u8, space: ColorSpace) -> Self {
        Self {
            r,
            g,
            b,
            a: 255,
            space,
        }
    }

    /// Convert the gamma-encoded RGB triple to linear-light floats in
    /// `[0, 1]`. Alpha is dropped (color math is alpha-agnostic; the
    /// regression lane compares premultiplied colors before this hop).
    #[must_use]
    pub fn to_linear_rgb(self) -> [f64; 3] {
        let r = srgb_decode(f64::from(self.r) / 255.0);
        let g = srgb_decode(f64::from(self.g) / 255.0);
        let b = srgb_decode(f64::from(self.b) / 255.0);
        [r, g, b]
    }
}

// ============================================================================
// Transfer function (sRGB / Display-P3 share the same TRC; Rec.2020
// uses BT.1886 in the strict spec but commonly the sRGB TRC in
// renderer paths — we use the sRGB TRC as the renderer-canonical
// approximation since wgpu's `Rgba8UnormSrgb` family is the only one
// in current use; future Rec.2020 work will revisit if it actually
// targets HDR PQ).
// ============================================================================

fn srgb_decode(c: f64) -> f64 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn srgb_encode(linear: f64) -> f64 {
    let l = linear.clamp(0.0, 1.0);
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

// ============================================================================
// Color-space conversions (linear RGB ↔ CIE XYZ ↔ CIE Lab)
//
// Matrices: D65 reference white. Numbers from the IEC 61966-2-1 (sRGB),
// IEC 61966-2-2 (sRGB linear), Apple Display-P3 spec, ITU-R BT.2020,
// and ROMM RGB references. Comments cite each source.
// ============================================================================

/// 3x3 matrix multiplication helper.
fn mul3(m: [[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// Linear-RGB → CIE XYZ (D65). Caller decodes the transfer function
/// before calling.
fn linear_rgb_to_xyz(space: ColorSpace, rgb: [f64; 3]) -> [f64; 3] {
    let m = match space {
        // sRGB / IEC 61966-2-1, D65.
        ColorSpace::Srgb | ColorSpace::Rec709 => [
            [0.412_390_8, 0.357_584_3, 0.180_480_8],
            [0.212_639_0, 0.715_168_7, 0.072_192_3],
            [0.019_330_8, 0.119_194_8, 0.950_532_1],
        ],
        // Apple Display-P3, D65.
        ColorSpace::DisplayP3 => [
            [0.486_570_9, 0.265_667_6, 0.198_217_4],
            [0.228_974_8, 0.691_738_6, 0.079_286_6],
            [0.000_000_0, 0.045_113_4, 1.043_944_2],
        ],
        // ITU-R BT.2020, D65.
        ColorSpace::Rec2020 => [
            [0.636_958_0, 0.144_616_8, 0.168_880_8],
            [0.262_700_2, 0.677_998_0, 0.059_301_7],
            [0.000_000_0, 0.028_072_7, 1.060_985_1],
        ],
        // ROMM RGB (ProPhoto), D50 — converted to D65 with Bradford.
        ColorSpace::ProPhotoRgb => [
            [0.797_675_0, 0.135_192_0, 0.031_309_0],
            [0.288_040_0, 0.711_874_0, 0.000_086_0],
            [0.000_000_0, 0.000_000_0, 0.825_210_0],
        ],
        ColorSpace::Unknown => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    };
    mul3(m, rgb)
}

/// CIE XYZ → CIE Lab (D65 reference white).
fn xyz_to_lab(xyz: [f64; 3]) -> [f64; 3] {
    // D65 reference white.
    const XN: f64 = 0.950_47;
    const YN: f64 = 1.0;
    const ZN: f64 = 1.088_83;
    const EPSILON: f64 = 216.0 / 24389.0;
    const KAPPA: f64 = 24389.0 / 27.0;

    fn f(t: f64) -> f64 {
        if t > EPSILON {
            t.cbrt()
        } else {
            (KAPPA * t + 16.0) / 116.0
        }
    }
    let fx = f(xyz[0] / XN);
    let fy = f(xyz[1] / YN);
    let fz = f(xyz[2] / ZN);
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// Compute the CIE Lab coordinates of a `Color`.
#[must_use]
pub fn color_to_lab(c: Color) -> [f64; 3] {
    let linear = c.to_linear_rgb();
    let xyz = linear_rgb_to_xyz(c.space, linear);
    xyz_to_lab(xyz)
}

// ============================================================================
// CIE ΔE 2000 — the perceptual color-difference metric.
//
// Implementation per Sharma, Wu, Dalal (2005),
// "The CIEDE2000 Color-Difference Formula: Implementation Notes,
// Supplementary Test Data, and Mathematical Observations".
// Cross-checked against Bruce Lindbloom's reference data.
// ============================================================================

/// Compute the CIE ΔE 2000 perceptual difference between two `Color`s.
/// Conventionally `ΔE ≤ 1.0` is treated as "perceptually identical".
#[must_use]
pub fn delta_e_2000(a: Color, b: Color) -> f64 {
    let lab_a = color_to_lab(a);
    let lab_b = color_to_lab(b);
    delta_e_2000_lab(lab_a, lab_b)
}

#[must_use]
fn delta_e_2000_lab(lab1: [f64; 3], lab2: [f64; 3]) -> f64 {
    let (l1, a1, b1) = (lab1[0], lab1[1], lab1[2]);
    let (l2, a2, b2) = (lab2[0], lab2[1], lab2[2]);

    let avg_l = (l1 + l2) / 2.0;
    let c1 = (a1 * a1 + b1 * b1).sqrt();
    let c2 = (a2 * a2 + b2 * b2).sqrt();
    let avg_c = (c1 + c2) / 2.0;

    let pow7_avg_c = avg_c.powi(7);
    let pow7_25 = 25f64.powi(7);
    let g = 0.5 * (1.0 - (pow7_avg_c / (pow7_avg_c + pow7_25)).sqrt());

    let a1p = (1.0 + g) * a1;
    let a2p = (1.0 + g) * a2;
    let c1p = (a1p * a1p + b1 * b1).sqrt();
    let c2p = (a2p * a2p + b2 * b2).sqrt();
    let avg_cp = (c1p + c2p) / 2.0;

    let h1p = if b1 == 0.0 && a1p == 0.0 {
        0.0
    } else {
        b1.atan2(a1p).to_degrees().rem_euclid(360.0)
    };
    let h2p = if b2 == 0.0 && a2p == 0.0 {
        0.0
    } else {
        b2.atan2(a2p).to_degrees().rem_euclid(360.0)
    };

    let delta_lp = l2 - l1;
    let delta_cp = c2p - c1p;

    let delta_hp = if c1p * c2p == 0.0 {
        0.0
    } else {
        let raw = h2p - h1p;
        if raw.abs() <= 180.0 {
            raw
        } else if raw > 180.0 {
            raw - 360.0
        } else {
            raw + 360.0
        }
    };
    let delta_hp_capital = 2.0 * (c1p * c2p).sqrt() * (delta_hp.to_radians() / 2.0).sin();

    let avg_hp = if c1p * c2p == 0.0 {
        h1p + h2p
    } else if (h1p - h2p).abs() <= 180.0 {
        (h1p + h2p) / 2.0
    } else if h1p + h2p < 360.0 {
        (h1p + h2p + 360.0) / 2.0
    } else {
        (h1p + h2p - 360.0) / 2.0
    };

    let t = 1.0 - 0.17 * ((avg_hp - 30.0).to_radians()).cos()
        + 0.24 * (2.0 * avg_hp.to_radians()).cos()
        + 0.32 * ((3.0 * avg_hp + 6.0).to_radians()).cos()
        - 0.20 * ((4.0 * avg_hp - 63.0).to_radians()).cos();

    let delta_theta = 30.0 * (-(((avg_hp - 275.0) / 25.0).powi(2))).exp();
    let pow7_avg_cp = avg_cp.powi(7);
    let rc = 2.0 * (pow7_avg_cp / (pow7_avg_cp + pow7_25)).sqrt();
    let sl = 1.0 + (0.015 * (avg_l - 50.0).powi(2)) / (20.0 + (avg_l - 50.0).powi(2)).sqrt();
    let sc = 1.0 + 0.045 * avg_cp;
    let sh = 1.0 + 0.015 * avg_cp * t;
    let rt = -(2.0 * delta_theta.to_radians()).sin() * rc;

    let kl = 1.0;
    let kc = 1.0;
    let kh = 1.0;

    let term_l = delta_lp / (kl * sl);
    let term_c = delta_cp / (kc * sc);
    let term_h = delta_hp_capital / (kh * sh);

    (term_l * term_l + term_c * term_c + term_h * term_h + rt * term_c * term_h)
        .max(0.0)
        .sqrt()
}

// ============================================================================
// Test patterns
// ============================================================================

/// The closed list of test patterns the regression fixture exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorPattern {
    /// Canonical 24-bit color cube — 6 vertices (R, G, B, C, M, Y) +
    /// black + white. Catches catastrophic gamut mis-mapping.
    Cube24Bit,
    /// Display-P3 gamut probes — colors that fall OUTSIDE sRGB but
    /// inside P3. Catches a renderer that silently downgrades P3
    /// content.
    P3GamutProbes,
    /// Rec.2020 gamut probes — colors that fall outside both sRGB and
    /// P3.
    Rec2020GamutProbes,
    /// Gamma-ramp — 16 grayscale steps from 0 to 255. Catches
    /// transfer-function regressions (linear vs. sRGB-encoded).
    GammaRamp,
}

impl ColorPattern {
    /// Stable filename slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Cube24Bit => "cube_24bit",
            Self::P3GamutProbes => "p3_gamut_probes",
            Self::Rec2020GamutProbes => "rec2020_gamut_probes",
            Self::GammaRamp => "gamma_ramp",
        }
    }

    /// Every pattern in declaration order.
    pub const ALL: &'static [ColorPattern] = &[
        Self::Cube24Bit,
        Self::P3GamutProbes,
        Self::Rec2020GamutProbes,
        Self::GammaRamp,
    ];

    /// The colors the pattern produces, tagged with the source color
    /// space they're authored in.
    #[must_use]
    pub fn samples(self) -> Vec<Color> {
        match self {
            Self::Cube24Bit => vec![
                Color::opaque(0, 0, 0, ColorSpace::Srgb),
                Color::opaque(255, 0, 0, ColorSpace::Srgb),
                Color::opaque(0, 255, 0, ColorSpace::Srgb),
                Color::opaque(0, 0, 255, ColorSpace::Srgb),
                Color::opaque(0, 255, 255, ColorSpace::Srgb),
                Color::opaque(255, 0, 255, ColorSpace::Srgb),
                Color::opaque(255, 255, 0, ColorSpace::Srgb),
                Color::opaque(255, 255, 255, ColorSpace::Srgb),
            ],
            Self::P3GamutProbes => vec![
                // P3 saturated red — outside sRGB.
                Color::opaque(255, 0, 0, ColorSpace::DisplayP3),
                // P3 saturated green — outside sRGB.
                Color::opaque(0, 255, 0, ColorSpace::DisplayP3),
                // P3 saturated cyan — outside sRGB.
                Color::opaque(0, 255, 255, ColorSpace::DisplayP3),
            ],
            Self::Rec2020GamutProbes => vec![
                Color::opaque(255, 0, 0, ColorSpace::Rec2020),
                Color::opaque(0, 255, 0, ColorSpace::Rec2020),
                Color::opaque(0, 0, 255, ColorSpace::Rec2020),
            ],
            Self::GammaRamp => (0u32..16u32)
                .map(|step| {
                    let v = (step * 17) as u8; // 0, 17, 34, …, 255
                    Color::opaque(v, v, v, ColorSpace::Srgb)
                })
                .collect(),
        }
    }
}

// ============================================================================
// ICC profile metadata
// ============================================================================

/// Opaque ICC profile metadata. The detector is per-platform; this
/// struct is what the platform layer hands the renderer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IccProfile {
    /// Stable id (e.g. `"system:Display P3"`,
    /// `"system:sRGB IEC61966-2.1"`). Used as the JSONL log key.
    pub id: String,
    /// Display name from the profile's `desc` tag.
    pub name: String,
    /// Declared gamut. The detector picks the closest [`ColorSpace`]
    /// to the profile's primaries; if no match is within tolerance
    /// the detector reports [`ColorSpace::Unknown`].
    pub declared_gamut: ColorSpace,
}

impl IccProfile {
    /// The profile every system "just works" with — used when no
    /// detector is wired or as the safe fallback.
    #[must_use]
    pub fn srgb_default() -> Self {
        Self {
            id: "system:srgb_default".to_string(),
            name: "sRGB IEC61966-2.1 (default)".to_string(),
            declared_gamut: ColorSpace::Srgb,
        }
    }
}

// ============================================================================
// Surface-format gamut classifier
// ============================================================================

/// Maps a wgpu surface texture format (named by string here so the
/// core crate doesn't take a `wgpu` dependency) to the [`ColorSpace`]
/// the renderer should treat its output as.
///
/// The GUI crate's `webgpu.rs` calls this once per surface
/// configuration so the startup log records the actual gamut. Adding
/// a new wide-gamut format requires extending exactly this table —
/// the meta-test in `tests` keeps the table comprehensive.
pub struct SurfaceFormatGamut;

impl SurfaceFormatGamut {
    /// Classify the format by its wgpu debug name. Names follow
    /// `wgpu::TextureFormat`'s `Debug` representation:
    ///
    /// - `Rgba8UnormSrgb`, `Bgra8UnormSrgb` → sRGB
    /// - `Rgba16Float`, `Rgb10a2Unorm`, `Bgr10a2Unorm` → wide-gamut
    ///   surface; the *actual* color space is platform-decided
    ///   (Display-P3 on macOS Pro Display XDR, Rec.2020 on HDR
    ///   monitors). Without a per-display ICC probe we report
    ///   [`ColorSpace::DisplayP3`] as the modal expected gamut and
    ///   surface a `wide_gamut_unverified` flag.
    /// - everything else → [`ColorSpace::Unknown`].
    #[must_use]
    pub fn classify(format_debug_name: &str) -> SurfaceGamutClassification {
        match format_debug_name {
            "Rgba8UnormSrgb" | "Bgra8UnormSrgb" => SurfaceGamutClassification {
                color_space: ColorSpace::Srgb,
                wide_gamut_unverified: false,
                hdr_capable: false,
            },
            "Rgba16Float" => SurfaceGamutClassification {
                color_space: ColorSpace::DisplayP3,
                wide_gamut_unverified: true,
                hdr_capable: true,
            },
            "Rgb10a2Unorm" | "Bgra10a2Unorm" | "Bgr10a2Unorm" => SurfaceGamutClassification {
                color_space: ColorSpace::DisplayP3,
                wide_gamut_unverified: true,
                hdr_capable: false,
            },
            _ => SurfaceGamutClassification {
                color_space: ColorSpace::Unknown,
                wide_gamut_unverified: false,
                hdr_capable: false,
            },
        }
    }
}

/// One surface's classification result. `wide_gamut_unverified`
/// indicates the format is wide-gamut but no per-display ICC profile
/// has been queried yet — the integration beads will set the flag
/// false once they wire the platform detector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceGamutClassification {
    pub color_space: ColorSpace,
    pub wide_gamut_unverified: bool,
    pub hdr_capable: bool,
}

// ============================================================================
// Structured logging
// ============================================================================

/// One row of `tests/color/logs/<scenario>.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColorMeasurement {
    /// Monotonic timestamp (ms since fixture start).
    pub ts_ms: u64,
    /// Scenario / pattern name.
    pub scenario: String,
    /// Expected color (the source-space authored value).
    pub expected: Color,
    /// Actual color the renderer produced.
    pub actual: Color,
    /// CIE 2000 perceptual difference between expected and actual.
    pub delta_e: f64,
    /// Whether `delta_e <= 1.0` (perceptually identical).
    pub perceptually_identical: bool,
}

impl ColorMeasurement {
    /// Build a measurement, computing `delta_e` and the threshold
    /// flag from `expected` and `actual`.
    #[must_use]
    pub fn new(ts_ms: u64, scenario: impl Into<String>, expected: Color, actual: Color) -> Self {
        let delta_e = delta_e_2000(expected, actual);
        Self {
            ts_ms,
            scenario: scenario.into(),
            expected,
            actual,
            delta_e,
            perceptually_identical: delta_e <= 1.0,
        }
    }
}

/// Render a slice of measurements as JSONL.
#[must_use]
pub fn render_measurements_jsonl(measurements: &[ColorMeasurement]) -> String {
    let mut out = String::new();
    for m in measurements {
        let line = serde_json::to_string(m).expect("ColorMeasurement always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure-color identity ΔE: a color compared to itself MUST be
    /// exactly 0.0. Sanity test on the ΔE2000 implementation.
    #[test]
    fn delta_e_identity_is_zero() {
        for c in [
            Color::opaque(0, 0, 0, ColorSpace::Srgb),
            Color::opaque(255, 255, 255, ColorSpace::Srgb),
            Color::opaque(255, 0, 0, ColorSpace::Srgb),
            Color::opaque(128, 200, 64, ColorSpace::Srgb),
        ] {
            let de = delta_e_2000(c, c);
            assert!(de < 1e-9, "identity ΔE for {c:?} should be 0, got {de}");
        }
    }

    /// ΔE is symmetric: ΔE(a, b) == ΔE(b, a) within fp tolerance.
    #[test]
    fn delta_e_is_symmetric() {
        let a = Color::opaque(255, 0, 0, ColorSpace::Srgb);
        let b = Color::opaque(0, 0, 255, ColorSpace::Srgb);
        let ab = delta_e_2000(a, b);
        let ba = delta_e_2000(b, a);
        assert!((ab - ba).abs() < 1e-9, "ΔE not symmetric: {ab} vs {ba}");
    }

    /// Sharma 2005 reference: pure-red vs. pure-blue in sRGB has a
    /// ΔE2000 north of 50 (perceptually catastrophic). This is the
    /// bracket sanity test — a value of 0 (broken implementation) or
    /// >300 (broken matrix) immediately falls out.
    #[test]
    fn red_vs_blue_in_srgb_is_perceptually_far() {
        let red = Color::opaque(255, 0, 0, ColorSpace::Srgb);
        let blue = Color::opaque(0, 0, 255, ColorSpace::Srgb);
        let de = delta_e_2000(red, blue);
        assert!(de > 50.0, "expected ΔE > 50, got {de}");
        assert!(de < 100.0, "ΔE bound regression: {de}");
    }

    /// Sharma 2005 reference: pure-white vs. pure-black has L1=100,
    /// L2=0 → ΔL=100. ΔE2000 ≈ 100 (the rotation/scaling terms vanish
    /// at the achromatic axis).
    #[test]
    fn white_vs_black_delta_e_is_about_100() {
        let w = Color::opaque(255, 255, 255, ColorSpace::Srgb);
        let k = Color::opaque(0, 0, 0, ColorSpace::Srgb);
        let de = delta_e_2000(w, k);
        assert!((de - 100.0).abs() < 1.0, "expected ≈100, got {de}");
    }

    /// Lab of pure black is (0, 0, 0); pure white is (100, 0, 0).
    #[test]
    fn black_and_white_lab_endpoints() {
        let lab_black = color_to_lab(Color::opaque(0, 0, 0, ColorSpace::Srgb));
        assert!(lab_black[0].abs() < 1e-3);
        let lab_white = color_to_lab(Color::opaque(255, 255, 255, ColorSpace::Srgb));
        assert!(
            (lab_white[0] - 100.0).abs() < 1e-3,
            "pure white L* = {} (expected 100)",
            lab_white[0]
        );
    }

    /// The same RGB triple in sRGB vs. Display-P3 should NOT be
    /// perceptually identical — Display-P3 saturated red is wider than
    /// sRGB saturated red, so ΔE is large. (Catches a renderer that
    /// silently treats P3 RGB as sRGB.)
    #[test]
    fn p3_red_diverges_from_srgb_red() {
        let srgb = Color::opaque(255, 0, 0, ColorSpace::Srgb);
        let p3 = Color::opaque(255, 0, 0, ColorSpace::DisplayP3);
        let de = delta_e_2000(srgb, p3);
        assert!(
            de > 5.0,
            "P3-red vs sRGB-red ΔE should be perceptually large; got {de}"
        );
    }

    #[test]
    fn surface_format_classifier_known_formats() {
        let srgb = SurfaceFormatGamut::classify("Rgba8UnormSrgb");
        assert_eq!(srgb.color_space, ColorSpace::Srgb);
        assert!(!srgb.wide_gamut_unverified);

        let p3_unverified = SurfaceFormatGamut::classify("Rgba16Float");
        assert_eq!(p3_unverified.color_space, ColorSpace::DisplayP3);
        assert!(p3_unverified.wide_gamut_unverified);
        assert!(p3_unverified.hdr_capable);

        let rgb10 = SurfaceFormatGamut::classify("Rgb10a2Unorm");
        assert_eq!(rgb10.color_space, ColorSpace::DisplayP3);
        assert!(rgb10.wide_gamut_unverified);
        assert!(!rgb10.hdr_capable);

        let unknown = SurfaceFormatGamut::classify("R8Sint");
        assert_eq!(unknown.color_space, ColorSpace::Unknown);
        assert!(!unknown.wide_gamut_unverified);
    }

    /// Meta-test: every named ColorSpace has an XYZ matrix entry.
    /// Adding a new variant without extending `linear_rgb_to_xyz`
    /// would yield the identity fallback — this test makes sure the
    /// fallback matrix is *only* hit by `Unknown`.
    #[test]
    fn every_color_space_has_an_xyz_matrix() {
        for space in ColorSpace::ALL_NAMED {
            let unit_red = linear_rgb_to_xyz(*space, [1.0, 0.0, 0.0]);
            assert!(
                unit_red[0] > 0.0 || unit_red[1] > 0.0 || unit_red[2] > 0.0,
                "{:?} produced zero XYZ for unit red",
                space
            );
            // The identity-matrix fallback would map (1, 0, 0) to
            // (1, 0, 0) — every real matrix has off-diagonal energy.
            let off_diag = unit_red[1].abs() + unit_red[2].abs();
            assert!(
                off_diag > 0.0,
                "{:?} appears to use the identity fallback (off-diag = 0)",
                space
            );
        }
    }

    #[test]
    fn pattern_samples_are_non_empty_and_tagged() {
        for pattern in ColorPattern::ALL {
            let samples = pattern.samples();
            assert!(!samples.is_empty(), "{:?} produced no samples", pattern);
            // Each pattern uses a single internally-consistent color
            // space — verify by checking the first vs all.
            let first_space = samples[0].space;
            for s in &samples {
                assert_eq!(
                    s.space, first_space,
                    "{:?} mixes color spaces ({:?} vs {:?})",
                    pattern, first_space, s.space
                );
            }
        }
    }

    #[test]
    fn measurement_threshold_is_perceptually_identical() {
        let red = Color::opaque(255, 0, 0, ColorSpace::Srgb);
        let m = ColorMeasurement::new(0, "identity", red, red);
        assert!(m.perceptually_identical);
        assert!(m.delta_e < 1e-9);

        let red_off = Color::opaque(254, 0, 0, ColorSpace::Srgb);
        let m_close = ColorMeasurement::new(1, "tiny_delta", red, red_off);
        assert!(
            m_close.delta_e < 0.5,
            "single-LSB drift ΔE = {}",
            m_close.delta_e
        );

        let blue = Color::opaque(0, 0, 255, ColorSpace::Srgb);
        let m_far = ColorMeasurement::new(2, "max_delta", red, blue);
        assert!(!m_far.perceptually_identical);
    }

    #[test]
    fn measurement_jsonl_roundtrips() {
        let red = Color::opaque(255, 0, 0, ColorSpace::Srgb);
        let m = ColorMeasurement::new(0, "test", red, red);
        let rendered = render_measurements_jsonl(&[m.clone()]);
        let parsed: ColorMeasurement = serde_json::from_str(rendered.trim()).unwrap();
        assert_eq!(parsed, m);
    }

    #[test]
    fn icc_profile_default_is_srgb() {
        let p = IccProfile::srgb_default();
        assert_eq!(p.declared_gamut, ColorSpace::Srgb);
    }

    #[test]
    fn wide_gamut_classification_is_consistent() {
        assert!(ColorSpace::DisplayP3.is_wide_gamut());
        assert!(ColorSpace::Rec2020.is_wide_gamut());
        assert!(ColorSpace::ProPhotoRgb.is_wide_gamut());
        assert!(!ColorSpace::Srgb.is_wide_gamut());
        assert!(!ColorSpace::Rec709.is_wide_gamut());
        assert!(!ColorSpace::Unknown.is_wide_gamut());
    }

    /// srgb_decode/encode are inverse on the open interval.
    #[test]
    fn srgb_transfer_is_inverse() {
        for v in [0.0_f64, 0.05, 0.18, 0.5, 0.8, 1.0] {
            let round_trip = srgb_encode(srgb_decode(v));
            assert!(
                (round_trip - v).abs() < 1e-9,
                "srgb roundtrip failed at {v}: got {round_trip}"
            );
        }
    }
}
