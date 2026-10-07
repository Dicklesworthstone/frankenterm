//! Glyph coverage adjustment for the `text_gamma` and `text_contrast`
//! options, modelled on kitty's `text_composition_strategy`.
//!
//! The math runs in the glyph shaders (`glyph-frag.glsl` and `shader.wgsl`).
//! The functions here mirror it so the formulas can be unit tested.
//!
//! When neither option is set the shaders skip all of this, so the default
//! output is unchanged. When either is set:
//!
//! 1. Coverage gets kitty's curve: `mix(a, pow(a, 1/gamma), w) * contrast`,
//!    where `w = (1 - fg_luminance + bg_luminance) / 2`.
//! 2. On the OpenGL path, which blends in sRGB space, the coverage is then
//!    converted so the gamma-space blend lands where a linear-light blend
//!    would. The WebGPU path renders to an sRGB surface and already blends in
//!    linear light, so it skips this step.

/// Rec. 709 luminance weights for linear RGB.
pub const LUMINANCE: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// Below this sRGB difference between text and background, a channel's
/// coverage is left alone: the blend barely changes the pixel, and the
/// conversion would divide by almost zero.
pub const MIN_SRGB_DELTA: f32 = 1.0e-4;

/// Shader uniforms derived from the config.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextComposition {
    /// False keeps the legacy gamma-space blending and skips all adjustment.
    pub enabled: bool,
    /// Exponent applied to coverage, `1 / text_gamma`.
    pub gamma_adjustment: f32,
    /// Coverage multiplier, `1 + text_contrast / 100`.
    pub contrast: f32,
}

impl TextComposition {
    pub fn from_config(config: &config::Config) -> Self {
        let enabled = config.text_gamma.is_some() || config.text_contrast.is_some();
        let gamma = config.text_gamma.unwrap_or(1.0).max(0.01);
        let contrast = config.text_contrast.unwrap_or(0.0).clamp(0.0, 100.0);
        Self {
            enabled,
            gamma_adjustment: 1.0 / gamma,
            contrast: 1.0 + contrast * 0.01,
        }
    }

    /// kitty's coverage curve. `fg_luminance` and `bg_luminance` are the
    /// luminance of the linear text and background colors.
    pub fn adjust_coverage(&self, coverage: f32, fg_luminance: f32, bg_luminance: f32) -> f32 {
        let weight = (1.0 - fg_luminance + bg_luminance) * 0.5;
        let curved = mix(coverage, coverage.powf(self.gamma_adjustment), weight);
        (curved * self.contrast).clamp(0.0, 1.0)
    }
}

pub fn luminance(rgb: [f32; 3]) -> f32 {
    rgb[0] * LUMINANCE[0] + rgb[1] * LUMINANCE[1] + rgb[2] * LUMINANCE[2]
}

/// The sRGB transfer function, as `to_srgb` in `glyph-frag.glsl`.
pub fn srgb_encode(linear: f32) -> f32 {
    if linear < 0.0031308 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// The coverage that, blended in sRGB space, gives the same result as a
/// linear-light blend at `coverage`. `fg` and `bg` are linear values for one
/// channel (or luminance). Solves
/// `srgb(fg) * c + srgb(bg) * (1 - c) = srgb(fg * coverage + bg * (1 - coverage))`
/// for `c`.
pub fn gamma_space_coverage(coverage: f32, fg: f32, bg: f32) -> f32 {
    let fg_srgb = srgb_encode(fg);
    let bg_srgb = srgb_encode(bg);
    let delta = fg_srgb - bg_srgb;
    if delta.abs() < MIN_SRGB_DELTA {
        return coverage;
    }
    ((srgb_encode(mix(bg, fg, coverage)) - bg_srgb) / delta).clamp(0.0, 1.0)
}

fn mix(a: f32, b: f32, t: f32) -> f32 {
    a * (1.0 - t) + b * t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composition(gamma: Option<f32>, contrast: Option<f32>) -> TextComposition {
        let mut config = config::Config::default();
        config.text_gamma = gamma;
        config.text_contrast = contrast;
        TextComposition::from_config(&config)
    }

    const COVERAGES: [f32; 7] = [0.0, 0.05, 0.25, 0.5, 0.75, 0.95, 1.0];

    #[test]
    fn default_config_disables_composition() {
        let tc = composition(None, None);
        assert!(!tc.enabled);
        assert_eq!(tc.gamma_adjustment, 1.0);
        assert_eq!(tc.contrast, 1.0);
    }

    #[test]
    fn either_option_enables_composition() {
        assert!(composition(Some(1.0), None).enabled);
        assert!(composition(None, Some(0.0)).enabled);
        let tc = composition(Some(2.0), Some(30.0));
        assert_eq!(tc.gamma_adjustment, 0.5);
        assert!((tc.contrast - 1.3).abs() < 1e-6);
    }

    #[test]
    fn neutral_curve_is_identity() {
        let tc = composition(Some(1.0), Some(0.0));
        for a in COVERAGES {
            for (fg, bg) in [(0.0, 1.0), (1.0, 0.0), (0.7, 0.03)] {
                assert!((tc.adjust_coverage(a, fg, bg) - a).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn higher_gamma_thickens_dark_text_more_than_light_text() {
        let tc = composition(Some(1.7), None);
        let dark_on_light = tc.adjust_coverage(0.5, 0.0, 1.0);
        let light_on_dark = tc.adjust_coverage(0.5, 1.0, 0.0);
        // Dark on light gets the full curve: 0.5^(1/1.7).
        assert!((dark_on_light - 0.5f32.powf(1.0 / 1.7)).abs() < 1e-6);
        // Light on dark gets none of it.
        assert!((light_on_dark - 0.5).abs() < 1e-6);
    }

    #[test]
    fn contrast_scales_and_clamps() {
        let tc = composition(None, Some(50.0));
        assert!((tc.adjust_coverage(0.4, 1.0, 0.0) - 0.6).abs() < 1e-6);
        assert_eq!(tc.adjust_coverage(0.9, 1.0, 0.0), 1.0);
        assert_eq!(tc.adjust_coverage(0.0, 1.0, 0.0), 0.0);
    }

    #[test]
    fn gamma_space_coverage_reproduces_linear_blend() {
        let colors = [0.0, 0.02, 0.1, 0.35, 0.69, 1.0];
        for fg in colors {
            for bg in colors {
                for a in COVERAGES {
                    let c = gamma_space_coverage(a, fg, bg);
                    let gamma_blend = srgb_encode(fg) * c + srgb_encode(bg) * (1.0 - c);
                    let linear_blend = srgb_encode(fg * a + bg * (1.0 - a));
                    assert!(
                        (gamma_blend - linear_blend).abs() < 1e-5,
                        "fg={fg} bg={bg} a={a}: {gamma_blend} != {linear_blend}"
                    );
                }
            }
        }
    }

    #[test]
    fn linear_blending_thickens_light_on_dark_and_thins_dark_on_light() {
        let light_on_dark = gamma_space_coverage(0.5, 1.0, 0.0);
        let dark_on_light = gamma_space_coverage(0.5, 0.0, 1.0);
        assert!(light_on_dark > 0.7, "{light_on_dark}");
        assert!(dark_on_light < 0.3, "{dark_on_light}");
    }

    #[test]
    fn shaders_declare_the_same_uniforms() {
        let glsl = include_str!("glyph-frag.glsl");
        for name in [
            "text_composition",
            "text_gamma_adjustment",
            "text_contrast",
            "text_background",
        ] {
            assert!(glsl.contains(name), "glyph-frag.glsl is missing {name}");
        }
        let wgsl = include_str!("shader.wgsl");
        for name in ["text_composition", "text_background"] {
            assert!(wgsl.contains(name), "shader.wgsl is missing {name}");
        }
    }
}
