//! Glyph blending for `text_gamma` and `text_contrast`, which follow kitty's
//! `text_composition_strategy`.
//!
//! The work happens per fragment in the glyph shaders: `glyph-frag.glsl`
//! (OpenGL) and `shader.wgsl` (WebGpu). This module is the CPU version of
//! their math, so it can be tested, and it builds the uniform values both
//! shaders read. They take the same four-float parameter vector
//! ([`TextCompositionUniforms::params`]), so they cannot disagree on its
//! layout.
//!
//! With both options unset nothing changes: glyph coverage is blended in
//! gamma space, as it always was. With either one set, each coverage value
//! first goes through kitty's curve ([`CoverageCurve::apply`]) and is then
//! blended in linear light. A WebGpu sRGB surface already blends in linear
//! light. The OpenGL framebuffer blends sRGB-encoded values, so its shader
//! also rescales the coverage to make that blend land on the linear-light
//! result ([`coverage_for_gamma_space_blend`]).

/// Name of the OpenGL uniform that holds [`TextCompositionUniforms::params`].
pub const GLSL_PARAMS_UNIFORM: &str = "text_composition";
/// Name of the OpenGL uniform that holds
/// [`TextCompositionUniforms::background`].
pub const GLSL_BACKGROUND_UNIFORM: &str = "text_composition_background";

/// Below this difference between the sRGB-encoded text and background
/// values, [`coverage_for_gamma_space_blend`] returns the coverage as is.
/// The blend then barely changes the pixel, and the rescaling would divide
/// by (almost) zero. `glyph-frag.glsl` uses the same threshold.
pub const MIN_ENCODED_SPAN: f32 = 1.0e-4;

/// The luminance of a linear RGB color, with the Rec. 709 coefficients that
/// the shaders also use.
pub fn luminance(rgb: [f32; 3]) -> f32 {
    let [r, g, b] = rgb;
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// kitty's coverage curve for one `text_gamma` / `text_contrast` setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoverageCurve {
    /// `1 / text_gamma`. Coverage is raised to this power.
    pub exponent: f32,
    /// `1 + text_contrast / 100`. The curved coverage is multiplied by this.
    pub gain: f32,
}

impl CoverageCurve {
    /// The curve for these option values, or `None` when both are unset,
    /// which keeps the original blending. An unset option is neutral: gamma
    /// 1, contrast 0.
    ///
    /// Config validation rejects out-of-range values, but a `Config` built
    /// in code never goes through it. So a gamma below
    /// [`config::MIN_TEXT_GAMMA`] is raised to it, a contrast is clamped to
    /// 0..=100 and a NaN or infinite value is replaced by the neutral one
    /// (the options still count as set): the shaders never get a non-finite
    /// exponent or gain.
    pub fn from_options(text_gamma: Option<f32>, text_contrast: Option<f32>) -> Option<Self> {
        if text_gamma.is_none() && text_contrast.is_none() {
            return None;
        }
        let gamma = text_gamma
            .filter(|gamma| gamma.is_finite())
            .map_or(1.0, |gamma| gamma.max(config::MIN_TEXT_GAMMA));
        let contrast = text_contrast
            .filter(|contrast| contrast.is_finite())
            .map_or(0.0, |contrast| contrast.clamp(0.0, 100.0));
        Some(Self {
            exponent: gamma.recip(),
            gain: 1.0 + contrast / 100.0,
        })
    }

    pub fn from_config(config: &config::Config) -> Option<Self> {
        Self::from_options(config.text_gamma, config.text_contrast)
    }

    /// The reshaped coverage of one glyph pixel. The luminances are those of
    /// the linear text and background colors. How much of the power curve
    /// applies is `(1 - text_luminance + background_luminance) / 2`: all of
    /// it for black text on white, none of it for white text on black. The
    /// gain then applies to every pixel.
    pub fn apply(&self, coverage: f32, text_luminance: f32, background_luminance: f32) -> f32 {
        let weight = 0.5 * (1.0 - text_luminance + background_luminance);
        let curved = lerp(coverage, coverage.powf(self.exponent), weight);
        (curved * self.gain).clamp(0.0, 1.0)
    }
}

/// The values both glyph shaders read for `text_gamma` / `text_contrast`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextCompositionUniforms {
    /// `text_composition` in both shaders. x: 1 when the options are on, 0
    /// to keep the original blending; y: [`CoverageCurve::exponent`];
    /// z: [`CoverageCurve::gain`]; w: luminance of [`Self::background`].
    pub params: [f32; 4],
    /// Linear RGB of the color text is assumed to be drawn over (a fragment
    /// shader cannot read the pixel it blends onto). The OpenGL shader
    /// rescales subpixel coverage per channel against it.
    pub background: [f32; 3],
}

impl TextCompositionUniforms {
    /// Both options unset.
    pub const OFF: Self = Self {
        params: [0.0, 1.0, 1.0, 0.0],
        background: [0.0; 3],
    };

    /// Uniforms for `curve`, with text assumed to be drawn over
    /// `background` (linear RGB; clamped to 0..=1, a non-finite channel
    /// becomes 0).
    pub fn new(curve: CoverageCurve, background: [f32; 3]) -> Self {
        let background = background.map(|channel| {
            if channel.is_finite() {
                channel.clamp(0.0, 1.0)
            } else {
                0.0
            }
        });
        Self {
            params: [1.0, curve.exponent, curve.gain, luminance(background)],
            background,
        }
    }
}

/// The sRGB encoding of one linear channel, as `to_srgb` in
/// `glyph-frag.glsl` computes it.
pub fn srgb_encode(linear: f32) -> f32 {
    if linear < 0.0031308 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// Coverage for the OpenGL glyph pass, which blends sRGB-encoded values:
/// `srgb(text) * c + srgb(background) * (1 - c)`. Returns the `c` that makes
/// that equal the linear-light blend at `coverage`,
/// `srgb(text * coverage + background * (1 - coverage))`. `text` and
/// `background` are linear values of one channel, or luminances.
pub fn coverage_for_gamma_space_blend(coverage: f32, text: f32, background: f32) -> f32 {
    let text_encoded = srgb_encode(text);
    let background_encoded = srgb_encode(background);
    let span = text_encoded - background_encoded;
    if span.abs() < MIN_ENCODED_SPAN {
        return coverage;
    }
    let target = srgb_encode(lerp(background, text, coverage));
    ((target - background_encoded) / span).clamp(0.0, 1.0)
}

/// GLSL/WGSL `mix`: `from * (1 - t) + to * t`.
fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from * (1.0 - t) + to * t
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINEAR_LEVELS: [f32; 8] = [0.0, 0.001, 0.003, 0.04, 0.18, 0.45, 0.9, 1.0];
    const COVERAGE_STEPS: usize = 32;

    fn coverages() -> impl Iterator<Item = f32> {
        (0..=COVERAGE_STEPS).map(|step| step as f32 / COVERAGE_STEPS as f32)
    }

    fn curve(gamma: Option<f32>, contrast: Option<f32>) -> CoverageCurve {
        CoverageCurve::from_options(gamma, contrast).expect("an option is set")
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn no_curve_without_either_option() {
        assert_eq!(CoverageCurve::from_options(None, None), None);
        assert_eq!(CoverageCurve::from_config(&config::Config::default()), None);
    }

    #[test]
    fn one_option_alone_leaves_the_other_neutral() {
        assert_eq!(
            curve(Some(4.0), None),
            CoverageCurve {
                exponent: 0.25,
                gain: 1.0
            }
        );
        let contrast_only = curve(None, Some(20.0));
        assert_eq!(contrast_only.exponent, 1.0);
        assert!(close(contrast_only.gain, 1.2));
        // Neutral values still turn the options on (and with them the
        // linear-light blend).
        assert_eq!(
            curve(Some(1.0), Some(0.0)),
            CoverageCurve {
                exponent: 1.0,
                gain: 1.0
            }
        );
    }

    #[test]
    fn values_that_skipped_validation_are_made_safe() {
        let low_gamma = curve(Some(0.001), Some(-3.0));
        assert!(close(low_gamma.exponent, 1.0 / config::MIN_TEXT_GAMMA));
        assert_eq!(low_gamma.gain, 1.0);
        assert_eq!(curve(None, Some(400.0)).gain, 2.0);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                curve(Some(bad), Some(bad)),
                CoverageCurve {
                    exponent: 1.0,
                    gain: 1.0
                }
            );
        }
    }

    #[test]
    fn the_neutral_curve_changes_nothing() {
        let neutral = curve(Some(1.0), Some(0.0));
        for coverage in coverages() {
            for (text, background) in [(0.0, 1.0), (1.0, 0.0), (0.3, 0.3), (0.8, 0.1)] {
                assert!(close(neutral.apply(coverage, text, background), coverage));
            }
        }
    }

    #[test]
    fn gamma_weight_follows_the_luminance_difference() {
        let heavy = curve(Some(2.5), None);
        let exponent = 1.0 / 2.5;
        // Black text on white: the whole power curve.
        assert!(close(heavy.apply(0.3, 0.0, 1.0), 0.3f32.powf(exponent)));
        // White text on black: none of it.
        assert!(close(heavy.apply(0.3, 1.0, 0.0), 0.3));
        // Equal luminances: half of it.
        let half = 0.5 * (0.3 + 0.3f32.powf(exponent));
        assert!(close(heavy.apply(0.3, 0.6, 0.6), half));
    }

    #[test]
    fn gamma_above_one_never_thins_text() {
        let heavy = curve(Some(1.8), None);
        for coverage in coverages() {
            for (text, background) in [(0.0, 1.0), (0.2, 0.7), (0.9, 0.05), (1.0, 0.0)] {
                assert!(heavy.apply(coverage, text, background) >= coverage - 1e-6);
            }
        }
    }

    #[test]
    fn contrast_multiplies_and_saturates() {
        let boosted = curve(None, Some(25.0));
        assert!(close(boosted.apply(0.6, 0.9, 0.1), 0.75));
        assert_eq!(boosted.apply(0.85, 0.9, 0.1), 1.0);
        assert_eq!(boosted.apply(0.0, 0.9, 0.1), 0.0);
    }

    #[test]
    fn uniforms_pack_the_curve_and_the_background() {
        // Off, the shaders skip everything; the neutral curve values keep
        // even an accidental use harmless.
        assert_eq!(TextCompositionUniforms::OFF.params, [0.0, 1.0, 1.0, 0.0]);
        let curve = curve(Some(2.0), Some(10.0));
        let uniforms = TextCompositionUniforms::new(curve, [0.5, 0.25, 1.0]);
        assert_eq!(uniforms.params[0], 1.0);
        assert_eq!(uniforms.params[1], 0.5);
        assert!(close(uniforms.params[2], 1.1));
        assert!(close(
            uniforms.params[3],
            0.2126 * 0.5 + 0.7152 * 0.25 + 0.0722
        ));
        assert_eq!(uniforms.background, [0.5, 0.25, 1.0]);

        let odd = TextCompositionUniforms::new(curve, [f32::NAN, -0.5, 7.0]);
        assert_eq!(odd.background, [0.0, 0.0, 1.0]);
        assert!(close(odd.params[3], 0.0722));
    }

    #[test]
    fn rescaled_coverage_reproduces_the_linear_blend() {
        for text in LINEAR_LEVELS {
            for background in LINEAR_LEVELS {
                for coverage in coverages() {
                    let rescaled = coverage_for_gamma_space_blend(coverage, text, background);
                    assert!((0.0..=1.0).contains(&rescaled));
                    let gamma_space = lerp(srgb_encode(background), srgb_encode(text), rescaled);
                    let linear = srgb_encode(lerp(background, text, coverage));
                    assert!(
                        (gamma_space - linear).abs() < 1e-5,
                        "text {text}, background {background}, coverage {coverage}: \
                         gamma-space blend {gamma_space}, linear blend {linear}"
                    );
                }
            }
        }
    }

    #[test]
    fn rescaling_thickens_light_on_dark_and_thins_dark_on_light() {
        // A quarter-covered white pixel on black: the gamma-space blend
        // writes 0.25, the linear-light one srgb(0.25), about 0.537.
        let light_on_dark = coverage_for_gamma_space_blend(0.25, 1.0, 0.0);
        assert!(close(light_on_dark, srgb_encode(0.25)));
        assert!(light_on_dark > 0.5);
        let dark_on_light = coverage_for_gamma_space_blend(0.25, 0.0, 1.0);
        assert!(dark_on_light < 0.25);
    }

    #[test]
    fn indistinguishable_text_and_background_keep_the_coverage() {
        assert_eq!(coverage_for_gamma_space_blend(0.7, 0.42, 0.42), 0.7);
        assert_eq!(coverage_for_gamma_space_blend(0.7, 0.0, 0.0), 0.7);
    }

    #[test]
    fn srgb_encode_matches_reference_points() {
        assert_eq!(srgb_encode(0.0), 0.0);
        assert!(close(srgb_encode(1.0), 1.0));
        assert!(close(srgb_encode(0.002), 0.002 * 12.92));
        // 18% grey encodes to about 46%.
        assert!((srgb_encode(0.18) - 0.4614).abs() < 1e-3);
    }

    #[test]
    fn luminance_of_white_is_one() {
        assert!(close(luminance([1.0, 1.0, 1.0]), 1.0));
        assert_eq!(luminance([0.0; 3]), 0.0);
        assert!(close(luminance([0.0, 1.0, 0.0]), 0.7152));
    }

    /// glium ignores a uniform the program does not declare, so a renamed
    /// shader uniform would silently turn these options off.
    #[test]
    fn glyph_shader_declares_the_uniforms_draw_sets() {
        let shader = include_str!("glyph-frag.glsl");
        for declaration in [
            format!("uniform vec4 {GLSL_PARAMS_UNIFORM};"),
            format!("uniform vec3 {GLSL_BACKGROUND_UNIFORM};"),
        ] {
            assert!(
                shader.contains(&declaration),
                "glyph-frag.glsl lacks `{declaration}`"
            );
        }
    }
}
