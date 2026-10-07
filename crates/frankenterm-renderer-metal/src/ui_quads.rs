//! Window chrome quads (ft-yccm0.4.7.1), the platform-independent half.
//!
//! The GUI lays out and paints window chrome (the fancy tab bar, modal
//! overlays and other box-model elements) with the WebGpu renderer's own
//! code into quads, then hands them to the Metal renderer as a [`UiLayer`].
//! Each [`UiQuad`] is drawn as WebGpu's quad shader draws it
//! (`shader.wgsl` `fs_main`, mirrored in `shaders/ui.metal`), sampling the
//! [`UiAtlas`]: the RGBA8 sRGB bytes of the glyph cache the chrome was
//! painted with. The layer is drawn after every pane and fill, in quad
//! order. [`shade_ui_quad`] and [`blend_ui`] are the CPU reference.

use crate::cell_bg::{hsv_to_rgb, rgb_to_hsv};

/// The Metal source of the chrome quad pass.
pub const UI_SHADER: &str = include_str!("shaders/ui.metal");

/// `shader.wgsl`'s quad kinds.
pub mod kind {
    /// A monochrome glyph: the texture's alpha is the coverage of `fg`.
    pub const GLYPH: f32 = 0.0;
    /// A color glyph: the texture is the color.
    pub const COLOR_EMOJI: f32 = 1.0;
    /// A background image, linearly filtered, its alpha scaled by `fg`'s.
    pub const BG_IMAGE: f32 = 2.0;
    /// A solid `fg` rectangle.
    pub const SOLID_COLOR: f32 = 3.0;
    /// A poly's coverage scaling `fg`'s alpha.
    pub const GRAY_SCALE: f32 = 4.0;
}

/// Bytes of one [`UiQuad`] in the quad buffer.
pub const UI_QUAD_BYTES: usize = 96;

/// Bytes of the UI uniforms block at the start of the quad buffer.
pub const UI_UNIFORMS_BYTES: usize = 256;

/// One chrome quad.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct UiQuad {
    /// Left, top, right and bottom, in drawable pixels.
    pub rect: [f32; 4],
    /// Left, top, right and bottom texture coordinates in the atlas,
    /// normalized.
    pub uv: [f32; 4],
    /// Linear light, straight alpha.
    pub fg: [f32; 4],
    /// Mixed into `fg` by `mix` (an animated color).
    pub alt: [f32; 4],
    pub mix: f32,
    /// Multipliers of hue, saturation and brightness; ones are the identity.
    pub hsv: [f32; 3],
    /// One of [`kind`].
    pub kind: f32,
}

impl UiQuad {
    /// The quad as `shaders/ui.metal` reads it.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; UI_QUAD_BYTES] {
        let floats: [f32; 24] = [
            self.rect[0],
            self.rect[1],
            self.rect[2],
            self.rect[3],
            self.uv[0],
            self.uv[1],
            self.uv[2],
            self.uv[3],
            self.fg[0],
            self.fg[1],
            self.fg[2],
            self.fg[3],
            self.alt[0],
            self.alt[1],
            self.alt[2],
            self.alt[3],
            self.hsv[0],
            self.hsv[1],
            self.hsv[2],
            self.kind,
            self.mix,
            0.0,
            0.0,
            0.0,
        ];
        let mut bytes = [0; UI_QUAD_BYTES];
        for (chunk, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(floats) {
            *chunk = value.to_le_bytes();
        }
        bytes
    }
}

/// The UI uniforms block as `shaders/ui.metal` reads it.
#[must_use]
pub fn ui_uniforms_bytes(
    viewport: [u32; 2],
    foreground_text_hsb: [f32; 3],
) -> [u8; UI_UNIFORMS_BYTES] {
    // Drawable sizes are far inside f32's exact integers.
    #[allow(clippy::cast_precision_loss)]
    let floats: [f32; 8] = [
        viewport[0] as f32,
        viewport[1] as f32,
        0.0,
        0.0,
        foreground_text_hsb[0],
        foreground_text_hsb[1],
        foreground_text_hsb[2],
        0.0,
    ];
    let mut bytes = [0; UI_UNIFORMS_BYTES];
    for (chunk, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(floats) {
        *chunk = value.to_le_bytes();
    }
    bytes
}

/// The texture chrome quads sample: RGBA8 bytes, sRGB-encoded as WebGpu's
/// `Rgba8UnormSrgb` glyph atlas holds them, row by row.
#[derive(Debug, Clone, Copy)]
pub struct UiAtlas<'a> {
    pub width: u32,
    pub height: u32,
    /// Changes whenever `rgba` changes; the renderer uploads only then.
    pub version: u64,
    pub rgba: &'a [u8],
}

/// Window chrome drawn over a window frame.
#[derive(Debug, Clone, Copy)]
pub struct UiLayer<'a> {
    pub atlas: UiAtlas<'a>,
    /// In paint order: later quads cover earlier ones.
    pub quads: &'a [UiQuad],
    /// How many of `quads`, from the first, are drawn under the panes:
    /// window background layers, which the WebGpu renderer draws at
    /// negative zindex. The panes' backgrounds then blend over them. The
    /// rest are drawn over everything.
    pub under: usize,
    /// `foreground_text_hsb`, applied to monochrome glyphs.
    pub foreground_text_hsb: [f32; 3],
}

/// What `fs_main` returns for `quad` (linear light, straight alpha), given
/// the atlas texel under the fragment sampled nearest and linearly, both
/// decoded to linear light.
#[must_use]
// Kinds are small whole numbers, compared exactly as fs_main compares them.
#[allow(clippy::float_cmp)]
pub fn shade_ui_quad(
    quad: &UiQuad,
    nearest: [f32; 4],
    linear: [f32; 4],
    foreground_text_hsb: [f32; 3],
) -> [f32; 4] {
    let fg: [f32; 4] = std::array::from_fn(|i| quad.fg[i] + (quad.alt[i] - quad.fg[i]) * quad.mix);
    let mut hsv = quad.hsv;
    let color = if quad.kind == kind::SOLID_COLOR {
        fg
    } else if quad.kind == kind::BG_IMAGE {
        [linear[0], linear[1], linear[2], linear[3] * fg[3]]
    } else if quad.kind == kind::COLOR_EMOJI {
        nearest
    } else if quad.kind == kind::GRAY_SCALE {
        [fg[0], fg[1], fg[2], fg[3] * nearest[3]]
    } else {
        hsv = std::array::from_fn(|i| hsv[i] * foreground_text_hsb[i]);
        [fg[0], fg[1], fg[2], nearest[3]]
    };
    let shifted = rgb_to_hsv([color[0], color[1], color[2]]);
    let rgb = hsv_to_rgb(std::array::from_fn(|i| shifted[i] * hsv[i]));
    [rgb[0], rgb[1], rgb[2], color[3]]
}

/// WebGpu's `BlendState::ALPHA_BLENDING` of a straight-alpha `source` over
/// `destination` (premultiplied, as the target stores it), in linear light.
#[must_use]
pub fn blend_ui(source: [f32; 4], destination: [f32; 4]) -> [f32; 4] {
    let alpha = source[3];
    [
        source[0] * alpha + destination[0] * (1.0 - alpha),
        source[1] * alpha + destination[1] * (1.0 - alpha),
        source[2] * alpha + destination[2] * (1.0 - alpha),
        alpha + destination[3] * (1.0 - alpha),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quad_is_laid_out_as_the_shader_reads_it() {
        let quad = UiQuad {
            rect: [1.0, 2.0, 3.0, 4.0],
            uv: [0.5, 0.25, 0.75, 1.0],
            fg: [0.1, 0.2, 0.3, 0.4],
            alt: [0.5, 0.6, 0.7, 0.8],
            mix: 0.25,
            hsv: [1.0, 0.5, 0.75],
            kind: kind::GRAY_SCALE,
        };
        let bytes = quad.to_bytes();
        let at = |offset: usize| f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        assert_eq!([at(0), at(12)], [1.0, 4.0], "rect at 0");
        assert_eq!([at(16), at(28)], [0.5, 1.0], "uv at 16");
        assert_eq!([at(32), at(44)], [0.1, 0.4], "fg at 32");
        assert_eq!([at(48), at(60)], [0.5, 0.8], "alt at 48");
        assert_eq!([at(64), at(72), at(76)], [1.0, 0.75, kind::GRAY_SCALE]);
        assert_eq!(at(80), 0.25, "mix at 80");
        // The shader's struct comments carry the same offsets.
        for (field, offset) in [
            ("rect", 0),
            ("uv", 16),
            ("fg", 32),
            ("alt", 48),
            ("hsv_kind", 64),
            ("mix_pad", 80),
        ] {
            let line = UI_SHADER
                .lines()
                .find(|line| line.contains(&format!(" {field};")) && line.contains("//"))
                .unwrap_or_else(|| panic!("the shader declares {field}"));
            assert!(line.contains(&format!("// {offset}")), "{field}: {line}");
        }
        let uniforms = ui_uniforms_bytes([640, 480], [1.0, 0.9, 0.8]);
        let at =
            |offset: usize| f32::from_le_bytes(uniforms[offset..offset + 4].try_into().unwrap());
        assert_eq!([at(0), at(4), at(16), at(24)], [640.0, 480.0, 1.0, 0.8]);
    }

    /// The reference follows fs_main per kind; ones leave colors as they are.
    #[test]
    fn each_kind_shades_as_fs_main_does() {
        let identity = [1.0; 3];
        let texel = [0.2, 0.4, 0.6, 0.5];
        let close = |a: [f32; 4], b: [f32; 4]| a.iter().zip(&b).all(|(a, b)| (a - b).abs() < 1e-5);
        let quad = |kind| UiQuad {
            fg: [0.8, 0.6, 0.4, 0.9],
            alt: [0.0, 0.0, 0.0, 0.0],
            hsv: identity,
            kind,
            ..UiQuad::default()
        };
        let shade = |kind| shade_ui_quad(&quad(kind), texel, [0.1, 0.1, 0.1, 0.8], identity);
        assert!(close(shade(kind::SOLID_COLOR), [0.8, 0.6, 0.4, 0.9]));
        assert!(
            close(shade(kind::GLYPH), [0.8, 0.6, 0.4, 0.5]),
            "coverage replaces alpha"
        );
        assert!(
            close(shade(kind::GRAY_SCALE), [0.8, 0.6, 0.4, 0.45]),
            "coverage scales alpha"
        );
        assert!(close(shade(kind::COLOR_EMOJI), texel));
        assert!(
            close(shade(kind::BG_IMAGE), [0.1, 0.1, 0.1, 0.72]),
            "linear sample, fg alpha"
        );
        // An animated color mixes toward alt.
        let mixed = UiQuad {
            mix: 0.5,
            ..quad(kind::SOLID_COLOR)
        };
        assert!(close(
            shade_ui_quad(&mixed, texel, texel, identity),
            [0.4, 0.3, 0.2, 0.45]
        ));
        // foreground_text_hsb dims glyphs only.
        let dim = [1.0, 1.0, 0.5];
        assert!(close(
            shade_ui_quad(&quad(kind::GLYPH), texel, texel, dim),
            [0.4, 0.3, 0.2, 0.5]
        ));
        assert!(close(
            shade_ui_quad(&quad(kind::SOLID_COLOR), texel, texel, dim),
            [0.8, 0.6, 0.4, 0.9]
        ));
        // Straight alpha over a premultiplied destination.
        assert!(close(
            blend_ui([1.0, 0.0, 0.0, 0.25], [0.0, 0.0, 0.4, 1.0]),
            [0.25, 0.0, 0.3, 1.0]
        ));
    }
}
