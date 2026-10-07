//! The Metal renderer's glyphs, from the window's fonts (ft-yccm0.4.4).
//!
//! [`FontGlyphs`] is the [`GlyphSource`] of the live Metal frame. It draws
//! with the code the WebGpu renderer draws with (ft-yccm0.4.7.3):
//! - A cluster (the render mirror makes them with `CellCluster::make_cluster`,
//!   as WebGpu clusters its lines) takes the font style
//!   `FontConfiguration::match_style` gives its attributes, and the whole
//!   cluster is shaped with one `LoadedFont::shape` call with the inputs
//!   WebGpu passes: its presentation, direction and presentation width, and
//!   the filter that leaves custom block glyphs to the glyph cache. So
//!   ligatures, combining marks and emoji sequences come out as they do there.
//! - Each glyph is laid out by [`GlyphCache::lay_out_glyph`], the glyph
//!   cache's own fitting and positioning, and placed where WebGpu places it:
//!   at the cell its shaper cell count reaches, shifted by its offset and
//!   bearing, its top at the cell height plus the descender (and the super-
//!   or subscript shift) less its offset and bearing.
//! - With `custom_block_glyphs`, box drawing, block elements and powerline
//!   glyphs are the glyph cache's own bitmaps
//!   ([`GlyphCache::block_sprite_image`]) at the cell's top-left.
//! - Decorations and cursors are its line and cursor sprites
//!   ([`GlyphCache::line_sprite_image`], [`GlyphCache::cursor_sprite_image`]).
//!
//! Bitmaps go to the renderer's atlases: coverage in the R8 atlas,
//! premultiplied color in the BGRA atlas.
//!
//! Shaped clusters are cached by font style, vertical alignment and text, as
//! WebGpu caches by style and text. Every frame touches the cached slots, so
//! the atlases keep them.
//! - A fallback font resolving asynchronously (a completion from the shaper)
//!   or a slot the atlas evicted anyway clears the cache and asks for a full
//!   scene rebuild.
//! - So does a full atlas, after which the cache refills with what the frame
//!   still draws.
//!
//! Not yet drawn the WebGpu way: double-width and double-height lines, bidi
//! (the render mirror clusters left to right), and
//! `experimental_pixel_positioning`.

use crate::customglyph::BlockKey;
use crate::glyphcache::GlyphCache;
use crate::utilsprites::RenderMetrics;
use config::TextStyle;
use frankenterm_font::shaper::PresentationWidth;
use frankenterm_font::{ClearShapeCache, FontConfiguration};
use frankenterm_gui::metal_scene::{
    CursorSprite, GlyphSource, LineSprite, PlacedGlyph, ShapedGlyph,
};
use frankenterm_renderer_metal::{AtlasKind, MetalRenderer};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use termwiz::cell::VerticalAlign;
use termwiz::cellcluster::CellCluster;
use termwiz::surface::CursorShape;
use window::bitmaps::{BitmapImage, Image};

/// What a shaped cluster depends on besides its cells' widths, which its
/// text determines: the font style, the vertical alignment (as its `u8`
/// code) and the text.
type ShapeKey = (TextStyle, u8, String);

pub(crate) struct FontGlyphs {
    fonts: Rc<FontConfiguration>,
    config: config::ConfigHandle,
    renderer: Rc<MetalRenderer>,
    cell_height: f64,
    descender: f64,
    /// The cell geometry block, line and cursor sprites are drawn for.
    metrics: RenderMetrics,
    /// The configuration generation the glyphs were rasterized under.
    config_generation: usize,
    shaped: HashMap<ShapeKey, Vec<ShapedGlyph>>,
    /// Line sprites placed in the grayscale atlas (ft-yccm0.4.7.3).
    lines: HashMap<LineSprite, PlacedGlyph>,
    /// Cursor sprites by shape and width in cells (ft-yccm0.4.7.3).
    cursors: HashMap<(CursorSprite, u8), PlacedGlyph>,
    /// Set by the shaper when a fallback font finishes resolving.
    fallback_resolved: Arc<AtomicBool>,
    /// The font's fallback chain grew while shaping this frame: the clusters
    /// shaped before then are stale ([`Self::take_chain_changed`]).
    chain_changed: bool,
    /// An atlas refused a glyph: clear the cache before the next frame.
    reset: bool,
}

/// The most times a scene is shaped again in one frame because the fallback
/// chain grew: the WebGpu renderer's bound on paint passes.
pub(crate) const MAX_SHAPE_PASSES: usize = 16;

/// The first pixel a quad starting at `position` covers, as the WebGpu
/// renderer's quads are rasterized (pixel centers at half pixels, a center
/// on the leading edge included) and sampled (nearest texel): the bitmap's
/// first column or row lands there.
// Positions are a few cells from the cell's corner.
#[allow(clippy::cast_possible_truncation)]
fn quad_origin(position: f64) -> i16 {
    (position - 0.5)
        .ceil()
        .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

impl FontGlyphs {
    pub(crate) fn new(
        fonts: Rc<FontConfiguration>,
        config: config::ConfigHandle,
        renderer: Rc<MetalRenderer>,
        metrics: &RenderMetrics,
    ) -> Self {
        let config_generation = config.generation();
        Self {
            fonts,
            config,
            renderer,
            cell_height: metrics.cell_size.height as f64,
            descender: metrics.descender.get(),
            metrics: *metrics,
            config_generation,
            shaped: HashMap::new(),
            lines: HashMap::new(),
            cursors: HashMap::new(),
            fallback_resolved: Arc::new(AtomicBool::new(false)),
            chain_changed: false,
            reset: false,
        }
    }

    /// Whether the fallback chain grew while shaping since the last call.
    /// The shaped clusters are then dropped, and the caller rebuilds its
    /// scene so every cluster is shaped against the grown chain.
    pub(crate) fn take_chain_changed(&mut self) -> bool {
        let changed = std::mem::take(&mut self.chain_changed);
        if changed {
            self.shaped.clear();
        }
        changed
    }

    /// Whether this source still serves `fonts`, `renderer`, these cell
    /// metrics and this configuration: a font size or configuration change
    /// needs new rasterizations and positions.
    pub(crate) fn serves(
        &self,
        fonts: &Rc<FontConfiguration>,
        renderer: &Rc<MetalRenderer>,
        metrics: &RenderMetrics,
        config_generation: usize,
    ) -> bool {
        Rc::ptr_eq(&self.fonts, fonts)
            && Rc::ptr_eq(&self.renderer, renderer)
            && self.cell_height == metrics.cell_size.height as f64
            && self.descender == metrics.descender.get()
            && self.config_generation == config_generation
    }

    /// Readies the cache for a frame. True when the scene must be rebuilt
    /// from scratch, because cached glyphs went stale.
    pub(crate) fn begin_frame(&mut self) -> bool {
        let mut rebuild = std::mem::take(&mut self.reset);
        if self.fallback_resolved.swap(false, Ordering::AcqRel) {
            rebuild = true;
        }
        if !rebuild {
            let renderer = &self.renderer;
            let alive = self
                .shaped
                .values()
                .flatten()
                .map(|shaped| &shaped.glyph)
                .chain(self.lines.values())
                .chain(self.cursors.values())
                .all(|glyph| renderer.touch_glyph(&glyph.slot));
            rebuild = !alive;
        }
        if rebuild {
            self.shaped.clear();
            self.lines.clear();
            self.cursors.clear();
        }
        rebuild
    }

    /// Places `image` in an atlas, `offset` pixels from a cell's top-left:
    /// its premultiplied RGBA in the color atlas, or its alpha (the coverage)
    /// in the grayscale atlas.
    fn place_image(
        &mut self,
        image: &Image,
        color: bool,
        offset: [i16; 2],
        what: &str,
    ) -> Option<PlacedGlyph> {
        let (width, height) = image.image_dimensions();
        let rgba = image.pixel_data_slice().chunks_exact(4);
        let (kind, pixels): (AtlasKind, Vec<u8>) = if color {
            let bgra = rgba.flat_map(|rgba| [rgba[2], rgba[1], rgba[0], rgba[3]]);
            (AtlasKind::Color, bgra.collect())
        } else {
            (AtlasKind::Grayscale, rgba.map(|rgba| rgba[3]).collect())
        };
        let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
            return None;
        };
        match self.renderer.insert_glyph(kind, width, height, &pixels) {
            Ok(slot) => Some(PlacedGlyph { slot, offset }),
            Err(err) => {
                log::debug!("Metal glyphs: atlas refused {what}: {err}");
                self.reset = true;
                None
            }
        }
    }

    /// Places the WebGpu renderer's line sprite for `lines` in the grayscale
    /// atlas: the same bitmap ([`GlyphCache::line_sprite_image`]), its alpha
    /// as coverage, at the cell's top-left.
    fn place_line(&mut self, lines: LineSprite) -> Option<PlacedGlyph> {
        let image = GlyphCache::line_sprite_image(
            lines.strikethrough,
            lines.underline,
            lines.overline,
            &self.metrics,
        );
        self.place_image(&image, false, [0, 0], "a line sprite")
    }

    /// Places the WebGpu renderer's cursor sprite for `shape`
    /// ([`GlyphCache::cursor_sprite_image`], with the configured
    /// `cursor_thickness`): a hollow block is its unfocused block outline.
    fn place_cursor(&mut self, shape: CursorSprite, width_cells: u8) -> Option<PlacedGlyph> {
        let shape = match shape {
            CursorSprite::HollowBlock => CursorShape::SteadyBlock,
            CursorSprite::Bar => CursorShape::SteadyBar,
            CursorSprite::Underline => CursorShape::SteadyUnderline,
        };
        let image =
            GlyphCache::cursor_sprite_image(&self.fonts, Some(shape), &self.metrics, width_cells);
        self.place_image(&image, false, [0, 0], "a cursor sprite")
    }

    /// Shapes and places `cluster` the WebGpu renderer's way; see the module
    /// docs.
    fn shape(&mut self, cluster: &CellCluster) -> Vec<ShapedGlyph> {
        let style = self.fonts.match_style(&self.config, &cluster.attrs).clone();
        let font = match self.fonts.resolve_font(&style) {
            Ok(font) => font,
            Err(err) => {
                log::warn!("Metal glyphs: no font for {:?}: {err:#}", cluster.text);
                return Vec::new();
            }
        };
        let presentation_width = PresentationWidth::with_cluster(cluster);
        let mut attempts = 0;
        let infos = loop {
            let resolved = Arc::clone(&self.fallback_resolved);
            match font.shape(
                &cluster.text,
                move || resolved.store(true, Ordering::Release),
                BlockKey::filter_out_synthetic,
                Some(cluster.presentation),
                cluster.direction,
                None,
                Some(&presentation_width),
            ) {
                Ok(infos) => break infos,
                // The font installed fallback faces while shaping: shape
                // again against the grown chain, and have the caller rebuild
                // what it shaped before, as the WebGpu renderer re-runs its
                // paint pass (bounded as it is).
                Err(err)
                    if err.root_cause().downcast_ref::<ClearShapeCache>().is_some()
                        && attempts < MAX_SHAPE_PASSES =>
                {
                    attempts += 1;
                    self.chain_changed = true;
                }
                Err(err) => {
                    log::warn!("Metal glyphs: shaping {:?} failed: {err:#}", cluster.text);
                    return Vec::new();
                }
            }
        };
        // The WebGpu renderer's super- and subscript shift.
        let valign = match cluster.attrs.vertical_align() {
            VerticalAlign::BaseLine => 0.0,
            VerticalAlign::SuperScript => self.cell_height * -0.25,
            VerticalAlign::SubScript => self.cell_height * 0.25,
        };
        let custom_blocks = self.config.custom_block_glyphs;
        let mut shaped = Vec::with_capacity(infos.len());
        let mut cell = 0usize;
        for (index, info) in infos.iter().enumerate() {
            let block = if custom_blocks {
                info.only_char.and_then(BlockKey::from_char)
            } else {
                None
            };
            if let Some(block) = block {
                let image = GlyphCache::block_sprite_image(&self.metrics, block);
                if let Some(glyph) = self.place_image(&image, false, [0, 0], "a block glyph") {
                    shaped.push(ShapedGlyph {
                        cell,
                        glyph,
                        brightness: 1.0,
                    });
                }
            } else {
                let followed_by_space = infos.get(index + 1).is_some_and(|next| next.is_space);
                match GlyphCache::lay_out_glyph(
                    &self.config,
                    info,
                    &font,
                    followed_by_space,
                    info.num_cells,
                ) {
                    Ok(layout) => {
                        if let Some(image) = &layout.image {
                            let left = (layout.x_offset + layout.bearing_x).get();
                            let top = self.cell_height + self.descender + valign
                                - (layout.y_offset + layout.bearing_y).get();
                            let offset = [quad_origin(left), quad_origin(top)];
                            if let Some(glyph) =
                                self.place_image(image, layout.has_color, offset, "a glyph")
                            {
                                shaped.push(ShapedGlyph {
                                    cell,
                                    glyph,
                                    brightness: layout.brightness_adjust,
                                });
                            }
                        }
                    }
                    Err(err) => {
                        log::warn!(
                            "Metal glyphs: laying out glyph {} failed: {err:#}",
                            info.glyph_pos
                        );
                    }
                }
            }
            cell += usize::from(info.num_cells);
        }
        shaped
    }
}

impl GlyphSource for FontGlyphs {
    fn shape_cluster(&mut self, cluster: &CellCluster) -> &[ShapedGlyph] {
        let style = self.fonts.match_style(&self.config, &cluster.attrs).clone();
        let key = (
            style,
            cluster.attrs.vertical_align() as u8,
            cluster.text.clone(),
        );
        if !self.shaped.contains_key(&key) {
            let shaped = self.shape(cluster);
            self.shaped.insert(key.clone(), shaped);
        }
        &self.shaped[&key]
    }

    fn line_sprite(&mut self, lines: LineSprite) -> Option<PlacedGlyph> {
        if let Some(placed) = self.lines.get(&lines) {
            return Some(*placed);
        }
        let placed = self.place_line(lines)?;
        self.lines.insert(lines, placed);
        Some(placed)
    }

    fn cursor_sprite(&mut self, shape: CursorSprite, width_cells: u8) -> Option<PlacedGlyph> {
        if let Some(placed) = self.cursors.get(&(shape, width_cells)) {
            return Some(*placed);
        }
        let placed = self.place_cursor(shape, width_cells)?;
        self.cursors.insert((shape, width_cells), placed);
        Some(placed)
    }
}
