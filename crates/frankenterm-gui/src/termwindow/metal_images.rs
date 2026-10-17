//! Image cells on the Metal front end (ft-yccm0.4.7.2).
//!
//! The WebGpu renderer draws an image cell (kitty, iTerm2 or sixel) as a
//! quad over the cell less the image's padding, sampling the cell's slice of
//! the image, nearest, from its glyph cache's atlas (`populate_image_quad`).
//! A Metal frame draws the same pixels as one color glyph per cell:
//! - [`ImageSlices`] keeps a glyph cache over a CPU texture, so decoding,
//!   animation frames and load states come from the image cache the WebGpu
//!   renderer uses, with its out-of-space retries;
//! - [`sample_slice`] samples a slice into a bitmap the size of the quad, as
//!   WebGpu's quad samples it;
//! - the pane's glyph source places the bitmaps in the renderer's color
//!   atlas, and [`ImageSlices`] keeps them while frames draw them.

use super::render::paint::AllowImage;
use super::render::{
    canonical_image_texture_region, image_cache_padding_for_cell, image_padding_fits_cell,
};
use crate::glyphcache::{GlyphCache, LoadState};
use ::window::bitmaps::atlas::{AtlasAllocationFailure, OutOfTextureSpace, Sprite};
use ::window::bitmaps::{BitmapImage, Image, ImageTexture, Texture2d};
use frankenterm_font::FontConfiguration;
use frankenterm_gui::metal_scene::PlacedGlyph;
use frankenterm_renderer_metal::AtlasSlot;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;
use termwiz::image::ImageCell;

/// The CPU texture's first side; it grows as the WebGpu atlas grows.
const FIRST_SIDE: usize = 1024;
/// The largest side it grows to. Past it, images are scaled down, as the
/// WebGpu renderer scales them when its atlas cannot grow.
const MAX_SIDE: usize = 8192;
/// Out-of-space retries for one image: recreate, grow, then scale down.
const MAX_ATTEMPTS: usize = 8;

/// Which bitmap draws a slice over a cell: the image frame's sprite in the
/// CPU texture (left, top, width, height), the slice's region of it (as
/// bits) and the image's padding in the cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SliceKey {
    sprite: [isize; 4],
    region: [u32; 4],
    padding: (u16, u16, u16, u16),
}

/// What draws a slice over a cell.
pub(crate) enum Slice {
    /// A bitmap already in the color atlas.
    Placed(PlacedGlyph),
    /// A bitmap to place in the color atlas, `offset` from the cell's
    /// top-left (the image's padding), then to hand back with
    /// [`ImageSlices::placed`].
    New {
        key: SliceKey,
        bitmap: Image,
        offset: [i16; 2],
    },
}

/// A pane's image cells' slices.
pub(crate) struct ImageSlices {
    fonts: Rc<FontConfiguration>,
    cache: GlyphCache,
    surface: Rc<ImageTexture>,
    side: usize,
    /// WebGpu's `allow_images`, scaled down only when the texture cannot
    /// grow to fit an image.
    allow: AllowImage,
    /// The bitmaps placed in the color atlas, by slice.
    placed: HashMap<SliceKey, PlacedGlyph>,
    /// The slices drawn since the frame began; the others are forgotten at
    /// the next, and their atlas slots age out.
    used: HashSet<SliceKey>,
    /// When a frame is next due for these images: an animation's next frame,
    /// or a poll of an image still decoding.
    due: Option<Instant>,
    /// An image drawn this frame is still decoding: its cells draw nothing.
    loading: bool,
}

impl ImageSlices {
    pub(crate) fn new(fonts: &Rc<FontConfiguration>) -> anyhow::Result<Self> {
        Self::with_side(fonts, FIRST_SIDE, AllowImage::Yes)
    }

    fn with_side(
        fonts: &Rc<FontConfiguration>,
        side: usize,
        allow: AllowImage,
    ) -> anyhow::Result<Self> {
        let surface = Rc::new(ImageTexture::new(side, side));
        let cache =
            GlyphCache::with_atlas_surface(fonts, Rc::clone(&surface) as Rc<dyn Texture2d>)?;
        Ok(Self {
            fonts: Rc::clone(fonts),
            cache,
            surface,
            side,
            allow,
            placed: HashMap::new(),
            used: HashSet::new(),
            due: None,
            loading: false,
        })
    }

    /// Starts a frame: forgets the slices the last frame did not draw, and
    /// its due time and loading state.
    pub(crate) fn begin_frame(&mut self) {
        let used = std::mem::take(&mut self.used);
        self.placed.retain(|key, _| used.contains(key));
        self.due = None;
        self.loading = false;
    }

    /// The atlas slots of the bitmaps kept.
    pub(crate) fn slots(&self) -> impl Iterator<Item = &AtlasSlot> {
        self.placed.values().map(|placed| &placed.slot)
    }

    /// Forgets every placed bitmap: the atlas dropped them.
    pub(crate) fn forget_placed(&mut self) {
        self.placed.clear();
        self.used.clear();
    }

    /// When a frame is next due for the images drawn since the frame began,
    /// and whether one of them is still decoding.
    pub(crate) fn poll(&self) -> (Option<Instant>, bool) {
        (self.due, self.loading)
    }

    /// Keeps the bitmap of a [`Slice::New`], placed.
    pub(crate) fn placed(&mut self, key: SliceKey, placed: PlacedGlyph) {
        self.placed.insert(key, placed);
    }

    /// What draws `image` over one column of its cell, a cell `cell` pixels
    /// in size, as `populate_image_quad` decides and samples it. `None`
    /// while it draws nothing: a slice or padding WebGpu refuses, an image
    /// still decoding or failed, or images scaled down to none.
    // Cell sizes and paddings are small; the padding fits the cell.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub(crate) fn slice(&mut self, image: &ImageCell, cell: (isize, isize)) -> Option<Slice> {
        let Some(region) = canonical_image_texture_region(image.top_left(), image.bottom_right())
        else {
            rejected("invalid_texture_region");
            return None;
        };
        let padding = image.padding();
        if !image_padding_fits_cell(padding, cell.0, cell.1) {
            rejected("invalid_padding");
            return None;
        }
        let Some(cache_padding) = image_cache_padding_for_cell(cell.0, cell.1) else {
            rejected("invalid_cell_geometry");
            return None;
        };
        let (sprite, next_due, load_state) = self.cached(image, cache_padding)?;
        if let Some(due) = next_due {
            self.due = Some(self.due.map_or(due, |known| known.min(due)));
        }
        if load_state == LoadState::Loading {
            self.loading = true;
        }
        if load_state != LoadState::Loaded {
            return None;
        }
        let (left, top, right, bottom) = padding;
        let key = SliceKey {
            sprite: [
                sprite.coords.origin.x,
                sprite.coords.origin.y,
                sprite.coords.size.width,
                sprite.coords.size.height,
            ],
            region: [
                region.0.to_bits(),
                region.1.to_bits(),
                region.2.to_bits(),
                region.3.to_bits(),
            ],
            padding,
        };
        self.used.insert(key);
        if let Some(placed) = self.placed.get(&key) {
            return Some(Slice::Placed(*placed));
        }
        let quad = [
            cell.0 as usize - usize::from(left) - usize::from(right),
            cell.1 as usize - usize::from(top) - usize::from(bottom),
        ];
        let rgba = {
            let atlas = self.surface.image.borrow();
            let (width, height) = atlas.image_dimensions();
            sample_slice(
                atlas.pixel_data_slice(),
                [width, height],
                [
                    sprite.coords.origin.x.max(0) as usize,
                    sprite.coords.origin.y.max(0) as usize,
                ],
                [
                    sprite.coords.size.width.max(0) as usize,
                    sprite.coords.size.height.max(0) as usize,
                ],
                [region.0, region.1, region.2, region.3],
                quad,
            )
        };
        Some(Slice::New {
            key,
            bitmap: Image::from_raw(quad[0], quad[1], rgba),
            offset: [
                i16::try_from(left).unwrap_or(i16::MAX),
                i16::try_from(top).unwrap_or(i16::MAX),
            ],
        })
    }

    /// The image cache's sprite, next due time and load state for `image`,
    /// as `populate_image_quad` asks for them. When the texture is out of
    /// space, it does what the WebGpu paint loop does with its atlas:
    /// recreate it at its size, then grow it to the size asked for, then
    /// scale images down; each time the image cache starts afresh.
    fn cached(
        &mut self,
        image: &ImageCell,
        cache_padding: usize,
    ) -> Option<(Sprite, Option<Instant>, LoadState)> {
        for attempt in 0..MAX_ATTEMPTS {
            if self.allow == AllowImage::No {
                return None;
            }
            let cached =
                self.cache
                    .cached_image(image.image_data(), Some(cache_padding), self.allow);
            let err = match cached {
                Ok(cached) => return Some(cached),
                Err(err) => err,
            };
            let Some(&OutOfTextureSpace {
                size: Some(size),
                current_size,
                failure: AtlasAllocationFailure::Capacity,
                ..
            }) = err.root_cause().downcast_ref::<OutOfTextureSpace>()
            else {
                log::debug!("Metal image cell not drawn: {err:#}");
                return None;
            };
            let (side, allow) = if attempt == 0 {
                (current_size, self.allow)
            } else if size <= MAX_SIDE {
                (size, self.allow)
            } else {
                (
                    self.side,
                    match self.allow {
                        AllowImage::Yes | AllowImage::Scale(0..=1) => AllowImage::Scale(2),
                        AllowImage::Scale(2..=3) => AllowImage::Scale(4),
                        AllowImage::Scale(4..=7) => AllowImage::Scale(8),
                        AllowImage::Scale(_) | AllowImage::No => AllowImage::No,
                    },
                )
            };
            log::info!(
                "Metal image cells: out of texture space ({err:#}); \
                 retrying at {side} with {allow:?}"
            );
            match Self::with_side(&self.fonts, side, allow) {
                Ok(fresh) => *self = fresh,
                Err(err) => {
                    log::warn!("Metal image cells: {err:#}");
                    return None;
                }
            }
        }
        None
    }
}

/// Counts an image cell refused before drawing, as the WebGpu renderer
/// counts it.
fn rejected(reason: &'static str) {
    metrics::counter!("gui.render.image_cell_rejected.total", "reason" => reason).increment(1);
}

/// The pixels WebGpu's image quad draws (`populate_image_quad`): a quad
/// `quad` pixels in size over `region` (left, top, right and bottom,
/// normalized) of the sprite at `origin`, `size` texels in size, in an RGBA
/// `atlas` `atlas_size` texels across and down. Each pixel takes the texel
/// nearest the texture coordinate interpolated at its center, computed as
/// WebGpu computes the quad's coordinates (normalized in the atlas, in
/// single precision) and clamped to the atlas's edge as its sampler does.
/// Returns the quad's RGBA bytes, row by row.
// Atlas and quad sizes are far inside f32's exact integers; texel indexes
// are floored and clamped before the cast.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub(crate) fn sample_slice(
    atlas: &[u8],
    atlas_size: [usize; 2],
    origin: [usize; 2],
    size: [usize; 2],
    region: [f32; 4],
    quad: [usize; 2],
) -> Vec<u8> {
    let [left, top, right, bottom] = region;
    let axis = |origin: usize, size: usize, start: f32, end: f32, extent: usize, pixels: usize| {
        let texture = extent as f32;
        let from = (origin as f32 + start * size as f32) / texture;
        let span = (end - start) * size as f32 / texture;
        (0..pixels)
            .map(|pixel| {
                let at = from + span * ((pixel as f32 + 0.5) / pixels as f32);
                ((at * texture).floor().max(0.0) as usize).min(extent.saturating_sub(1))
            })
            .collect::<Vec<usize>>()
    };
    let columns = axis(origin[0], size[0], left, right, atlas_size[0], quad[0]);
    let rows = axis(origin[1], size[1], top, bottom, atlas_size[1], quad[1]);
    let mut rgba = Vec::with_capacity(quad[0] * quad[1] * 4);
    for row in rows {
        for &column in &columns {
            let at = (row * atlas_size[0] + column) * 4;
            rgba.extend_from_slice(atlas.get(at..at + 4).unwrap_or(&[0; 4]));
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4x2-texel atlas whose texel (x, y) is [x, y, 7, 255].
    fn atlas() -> Vec<u8> {
        (0..2u8)
            .flat_map(|y| (0..4u8).flat_map(move |x| [x, y, 7, 255]))
            .collect()
    }

    fn texels(rgba: &[u8]) -> Vec<(u8, u8)> {
        rgba.chunks_exact(4)
            .map(|texel| (texel[0], texel[1]))
            .collect()
    }

    /// ft-yccm0.4.7.2: a slice drawn at its own size takes its texels one to
    /// one; a slice drawn larger repeats each texel nearest each pixel's
    /// center; a slice of part of a sprite takes only that part.
    #[test]
    fn a_slice_is_sampled_nearest_at_each_pixels_center() {
        let atlas = atlas();
        // The whole atlas as one sprite, at its own size.
        let whole = sample_slice(&atlas, [4, 2], [0, 0], [4, 2], [0.0, 0.0, 1.0, 1.0], [4, 2]);
        assert_eq!(whole, atlas);
        // The sprite in columns 1 and 2, drawn twice as large.
        let doubled = sample_slice(&atlas, [4, 2], [1, 0], [2, 2], [0.0, 0.0, 1.0, 1.0], [4, 4]);
        assert_eq!(
            texels(&doubled),
            [
                [(1, 0), (1, 0), (2, 0), (2, 0)],
                [(1, 0), (1, 0), (2, 0), (2, 0)],
                [(1, 1), (1, 1), (2, 1), (2, 1)],
                [(1, 1), (1, 1), (2, 1), (2, 1)],
            ]
            .concat()
        );
        // The right half of the whole sprite's bottom row, as one cell's
        // slice of an image spread over two cells.
        let half = sample_slice(&atlas, [4, 2], [0, 0], [4, 2], [0.5, 0.5, 1.0, 1.0], [2, 1]);
        assert_eq!(texels(&half), [(2, 1), (3, 1)]);
        // Drawn smaller, each pixel takes the texel under its center.
        let halved = sample_slice(&atlas, [4, 2], [0, 0], [4, 2], [0.0, 0.0, 1.0, 1.0], [2, 1]);
        assert_eq!(texels(&halved), [(1, 1), (3, 1)]);
    }
}
