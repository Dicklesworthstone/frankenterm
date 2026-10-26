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
#[cfg(any(target_os = "macos", test))]
use crate::{FrameError, MAX_TEXTURE_EXTENT};
use std::sync::Arc;

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

/// Rows shared by one immutable chrome-atlas stripe. A changed glyph copies
/// only the stripes it touches when a new frame snapshot is published.
pub const UI_ATLAS_STRIPE_ROWS: u32 = 32;

/// One full-width stripe of an immutable chrome-atlas snapshot.
#[derive(Debug, Clone)]
pub struct UiAtlasStripe {
    /// Globally unique snapshot revision that last changed this stripe,
    /// retained when a newer snapshot shares it at the same row position.
    /// A recreated atlas must use new versions.
    pub version: u64,
    /// Tightly packed RGBA8 sRGB rows. Only the final stripe may be short.
    pub rgba: Arc<[u8]>,
}

/// Complete atlas pixels, either contiguous or shared in immutable stripes.
/// Every snapshot contains the entire atlas, so renderers can skip any
/// number of intermediate versions without needing a damage history.
#[derive(Debug, Clone, Copy)]
pub enum UiAtlasPixels<'a> {
    Rgba(&'a [u8]),
    Stripes(&'a [UiAtlasStripe]),
}

/// The texture chrome quads sample: RGBA8 bytes, sRGB-encoded as WebGpu's
/// `Rgba8UnormSrgb` glyph atlas holds them, row by row.
#[derive(Debug, Clone, Copy)]
pub struct UiAtlas<'a> {
    pub width: u32,
    pub height: u32,
    /// Changes whenever the pixels change; the renderer uploads only then.
    pub version: u64,
    pub pixels: UiAtlasPixels<'a>,
}

#[cfg(any(target_os = "macos", test))]
impl UiAtlas<'_> {
    /// Checks all source ranges before the first GPU write, including every
    /// stripe even if a renderer already holds some of their versions.
    // A valid stripe index is below MAX_TEXTURE_EXTENT / stripe rows.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn validate(self) -> Result<(), FrameError> {
        let invalid = || FrameError::InvalidExtent {
            width: self.width,
            height: self.height,
        };
        if !(1..=MAX_TEXTURE_EXTENT).contains(&self.width)
            || !(1..=MAX_TEXTURE_EXTENT).contains(&self.height)
        {
            return Err(invalid());
        }
        let pitch = self.width as usize * 4;
        let valid = match self.pixels {
            UiAtlasPixels::Rgba(rgba) => rgba.len() == pitch * self.height as usize,
            UiAtlasPixels::Stripes(stripes) => {
                stripes.len() == self.height.div_ceil(UI_ATLAS_STRIPE_ROWS) as usize
                    && stripes.iter().enumerate().all(|(index, stripe)| {
                        let row = index as u32 * UI_ATLAS_STRIPE_ROWS;
                        let rows = (self.height - row).min(UI_ATLAS_STRIPE_ROWS);
                        stripe.rgba.len() == pitch * rows as usize
                    })
            }
        };
        if valid { Ok(()) } else { Err(invalid()) }
    }
}

/// What one independently reusable GPU texture currently holds. The native
/// renderer keeps one alongside each leased frame slot's chrome texture.
#[derive(Debug, Default)]
#[cfg(any(target_os = "macos", test))]
pub(crate) struct UiAtlasUploads {
    width: u32,
    height: u32,
    version: Option<u64>,
    stripes: Vec<u64>,
}

#[cfg(any(target_os = "macos", test))]
impl UiAtlasUploads {
    pub(crate) fn is_current(&self, atlas: UiAtlas<'_>) -> bool {
        self.version == Some(atlas.version)
            && self.width == atlas.width
            && self.height == atlas.height
    }

    /// Uploads changed full-width regions and advances the texture's
    /// history only after all writes. A new texture uses a fresh history.
    /// The callback receives (first row, row count, tightly packed pixels).
    // Validation bounds the stripe index by MAX_TEXTURE_EXTENT.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn write(
        &mut self,
        atlas: UiAtlas<'_>,
        mut upload: impl FnMut(u32, u32, &[u8]),
    ) -> Result<u64, FrameError> {
        if self.is_current(atlas) {
            return Ok(0);
        }
        atlas.validate()?;
        let same_extent =
            self.version.is_some() && self.width == atlas.width && self.height == atlas.height;
        let bytes = match atlas.pixels {
            UiAtlasPixels::Rgba(rgba) => {
                upload(0, atlas.height, rgba);
                self.stripes.clear();
                rgba.len() as u64
            }
            UiAtlasPixels::Stripes(stripes) => {
                let can_reuse = same_extent && self.stripes.len() == stripes.len();
                let mut bytes = 0;
                for (index, stripe) in stripes.iter().enumerate() {
                    if can_reuse && self.stripes[index] == stripe.version {
                        continue;
                    }
                    let row = index as u32 * UI_ATLAS_STRIPE_ROWS;
                    let rows = (atlas.height - row).min(UI_ATLAS_STRIPE_ROWS);
                    upload(row, rows, &stripe.rgba);
                    bytes += stripe.rgba.len() as u64;
                }
                self.stripes.clear();
                self.stripes
                    .extend(stripes.iter().map(|stripe| stripe.version));
                bytes
            }
        };
        self.width = atlas.width;
        self.height = atlas.height;
        self.version = Some(atlas.version);
        Ok(bytes)
    }
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
    /// rest, after the `under_text` ones, are drawn over everything.
    pub under: usize,
    /// How many of `quads` after the `under` ones are drawn over the panes'
    /// backgrounds and under their text: images at a negative z-index,
    /// which the WebGpu renderer draws in a line's background sub-layer
    /// (ft-yccm0.4.7.2).
    pub under_text: usize,
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

    fn atlas_stripes(width: u32, height: u32, versions: &[u64]) -> Vec<UiAtlasStripe> {
        assert_eq!(
            versions.len(),
            height.div_ceil(UI_ATLAS_STRIPE_ROWS) as usize
        );
        versions
            .iter()
            .enumerate()
            .map(|(index, &version)| {
                let row = u32::try_from(index).unwrap() * UI_ATLAS_STRIPE_ROWS;
                let rows = (height - row).min(UI_ATLAS_STRIPE_ROWS);
                UiAtlasStripe {
                    version,
                    rgba: vec![version.to_le_bytes()[0]; (width * rows * 4) as usize].into(),
                }
            })
            .collect()
    }

    fn stripe_pixels(stripes: &[UiAtlasStripe]) -> Vec<u8> {
        stripes
            .iter()
            .flat_map(|stripe| stripe.rgba.iter().copied())
            .collect()
    }

    fn write_atlas(
        history: &mut UiAtlasUploads,
        atlas: UiAtlas<'_>,
        destination: &mut [u8],
    ) -> (u64, Vec<(u32, u32)>) {
        let pitch = atlas.width as usize * 4;
        let mut regions = Vec::new();
        let bytes = history
            .write(atlas, |row, rows, rgba| {
                assert_eq!(rgba.len(), rows as usize * pitch);
                let first = row as usize * pitch;
                destination[first..first + rgba.len()].copy_from_slice(rgba);
                regions.push((row, rows));
            })
            .unwrap();
        (bytes, regions)
    }

    #[test]
    fn atlas_uploads_keep_three_lagging_slots_complete_after_skipped_snapshots() {
        let (width, height) = (3, 70);
        let snapshots = [
            atlas_stripes(width, height, &[11, 12, 13]),
            atlas_stripes(width, height, &[21, 12, 13]),
            atlas_stripes(width, height, &[21, 22, 13]),
            atlas_stripes(width, height, &[21, 22, 23]),
        ];
        let atlas = |index: usize| UiAtlas {
            width,
            height,
            version: 100 + index as u64,
            pixels: UiAtlasPixels::Stripes(&snapshots[index]),
        };
        let mut slots: [UiAtlasUploads; 3] = std::array::from_fn(|_| UiAtlasUploads::default());
        let mut textures: [Vec<u8>; 3] = std::array::from_fn(|_| vec![0xcd; 840]);
        for slot in 0..3 {
            let (bytes, regions) = write_atlas(&mut slots[slot], atlas(slot), &mut textures[slot]);
            assert_eq!(bytes, 840, "a newly allocated slot initializes every pixel");
            assert_eq!(regions, [(0, 32), (32, 32), (64, 6)]);
            assert_eq!(textures[slot], stripe_pixels(&snapshots[slot]));
        }
        let expected_bytes = [840, 456, 72];
        let expected_regions = [
            vec![(0, 32), (32, 32), (64, 6)],
            vec![(32, 32), (64, 6)],
            vec![(64, 6)],
        ];
        for slot in 0..3 {
            let (bytes, regions) = write_atlas(&mut slots[slot], atlas(3), &mut textures[slot]);
            assert_eq!(bytes, expected_bytes[slot]);
            assert_eq!(regions, expected_regions[slot]);
            assert_eq!(textures[slot], stripe_pixels(&snapshots[3]));
            assert_eq!(
                write_atlas(&mut slots[slot], atlas(3), &mut textures[slot]),
                (0, vec![])
            );
            assert!(slots[slot].is_current(atlas(3)));
        }
        // A later complete snapshot may share all its stripes. This advances
        // snapshot identity without any upload and needs no intermediate log.
        let unchanged = UiAtlas {
            version: 200,
            ..atlas(3)
        };
        assert_eq!(
            write_atlas(&mut slots[0], unchanged, &mut textures[0]),
            (0, vec![])
        );
        assert!(slots[0].is_current(unchanged));
    }

    #[test]
    fn atlas_uploads_reinitialize_new_textures_and_representation_or_extent_changes() {
        let stripes = atlas_stripes(3, 70, &[31, 32, 33]);
        let pixels = stripe_pixels(&stripes);
        let atlas = UiAtlas {
            width: 3,
            height: 70,
            version: 300,
            pixels: UiAtlasPixels::Stripes(&stripes),
        };
        let mut history = UiAtlasUploads::default();
        let mut texture = vec![0xcd; pixels.len()];
        assert_eq!(write_atlas(&mut history, atlas, &mut texture).0, 840);
        let contiguous = UiAtlas {
            version: 301,
            pixels: UiAtlasPixels::Rgba(&pixels),
            ..atlas
        };
        assert_eq!(
            write_atlas(&mut history, contiguous, &mut texture),
            (840, vec![(0, 70)])
        );
        let striped_again = UiAtlas {
            version: 302,
            ..atlas
        };
        assert_eq!(
            write_atlas(&mut history, striped_again, &mut texture).0,
            840
        );
        assert_eq!(texture, pixels);

        for (width, height, version) in [(4, 70, 303), (4, 65, 304)] {
            // Even the same stripe count and stripe ids cannot authorize
            // reuse when their row pitch or final stripe extent changes.
            let stripes = atlas_stripes(width, height, &[31, 32, 33]);
            let atlas = UiAtlas {
                width,
                height,
                version,
                pixels: UiAtlasPixels::Stripes(&stripes),
            };
            texture = vec![0xcd; (width * height * 4) as usize];
            let (bytes, regions) = write_atlas(&mut history, atlas, &mut texture);
            assert_eq!(bytes, u64::from(width * height * 4));
            assert_eq!(regions, [(0, 32), (32, 32), (64, height - 64)]);
            assert_eq!(texture, stripe_pixels(&stripes));
            let mut fresh_history = UiAtlasUploads::default();
            texture.fill(0xcd);
            assert_eq!(
                write_atlas(&mut fresh_history, atlas, &mut texture).0,
                bytes
            );
            assert_eq!(texture, stripe_pixels(&stripes));
        }
    }

    #[test]
    fn malformed_later_stripe_rejects_before_any_write_or_history_change() {
        let initial = atlas_stripes(3, 70, &[41, 42, 43]);
        let old = UiAtlas {
            width: 3,
            height: 70,
            version: 400,
            pixels: UiAtlasPixels::Stripes(&initial),
        };
        let mut history = UiAtlasUploads::default();
        let mut texture = vec![0; 840];
        write_atlas(&mut history, old, &mut texture);
        let original_pixels = texture.clone();
        let mut malformed = atlas_stripes(3, 70, &[51, 42, 43]);
        // The later stripe even advertises an already uploaded version.
        // Validation must still inspect it before uploading the changed first.
        malformed[2].rgba = vec![43; 71].into();
        let invalid = UiAtlas {
            version: 401,
            pixels: UiAtlasPixels::Stripes(&malformed),
            ..old
        };
        let mut callbacks = 0;
        assert!(history.write(invalid, |_, _, _| callbacks += 1).is_err());
        assert_eq!(callbacks, 0);
        assert!(history.is_current(old));
        assert!(!history.is_current(invalid));
        assert_eq!(texture, original_pixels);

        let repaired = atlas_stripes(3, 70, &[51, 42, 53]);
        let retry = UiAtlas {
            pixels: UiAtlasPixels::Stripes(&repaired),
            ..invalid
        };
        assert_eq!(
            write_atlas(&mut history, retry, &mut texture),
            (456, vec![(0, 32), (64, 6)])
        );
        assert_eq!(texture, stripe_pixels(&repaired));
    }

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
