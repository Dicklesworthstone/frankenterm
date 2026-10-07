//! The Metal renderer's glyphs, from the window's fonts (ft-yccm0.4.4).
//!
//! [`FontGlyphs`] is the [`GlyphSource`] of the live Metal frame. It draws
//! with the code the WebGpu renderer draws with (ft-yccm0.4.7.3):
//! - A row's clusters (the render mirror makes them with
//!   `CellCluster::make_cluster`, as WebGpu clusters its lines) are shaped
//!   by the line-shaping code both renderers share
//!   ([`frankenterm_gui::line_shaping`]): clusters split only by paint
//!   attributes shape as one run, and a line of several clusters is shaped
//!   with its whole text as paragraph context.
//! - Each shaper call is one `LoadedFont::shape` with the inputs WebGpu
//!   passes: the font style `FontConfiguration::match_style` gives the
//!   cluster's attributes, its presentation, direction and presentation
//!   width, and the filter that leaves custom block glyphs to the glyph
//!   cache. So ligatures, combining marks and emoji sequences come out as
//!   they do there.
//! - Each glyph is laid out by [`GlyphCache::lay_out_glyph`], the glyph
//!   cache's own fitting and positioning, and placed where WebGpu places it:
//!   at the cell the shaper cell counts of the row's glyphs before it reach,
//!   shifted by its offset and bearing, its top at the cell height plus the
//!   descender (and the super- or subscript shift) less its offset and
//!   bearing, all in WebGpu's `f32` arithmetic.
//! - With `custom_block_glyphs`, box drawing, block elements and powerline
//!   glyphs are the glyph cache's own bitmaps
//!   ([`GlyphCache::block_sprite_image`]) at the cell's top-left.
//! - Decorations and cursors are its line and cursor sprites
//!   ([`GlyphCache::line_sprite_image`], [`GlyphCache::cursor_sprite_image`]).
//!
//! Bitmaps go to the renderer's atlases: coverage in the R8 atlas,
//! premultiplied color in the BGRA atlas.
//!
//! The caches are WebGpu's:
//! - Shaper output for a cluster shaped without paragraph context is kept
//!   by font style and text, in an LFU cache as large as WebGpu's
//!   (`shape_cache_size`). Shapes with paragraph context depend on their
//!   line and are not kept.
//! - A laid-out glyph is kept by WebGpu's glyph key: the font, the fallback
//!   face, the glyph, its cell count, the font style and whether a space
//!   follows it. As in WebGpu, the first layout of a key (with its shaper
//!   offsets) serves every later use of it.
//!
//! Every frame touches the cached atlas slots, so the atlases keep them.
//! - A fallback font resolving asynchronously (a completion from the shaper)
//!   drops the kept shaper output and asks for a full scene rebuild, as
//!   WebGpu drops its shape cache.
//! - A slot the atlas evicted anyway, or a full atlas, drops the placed
//!   glyphs and sprites and asks for a full scene rebuild; the caches refill
//!   with what the frame still draws.
//!
//! Not yet drawn the WebGpu way: double-width and double-height lines, bidi
//! (the render mirror clusters left to right), and
//! `experimental_pixel_positioning`.

use crate::customglyph::BlockKey;
use crate::glyphcache::GlyphCache;
use crate::utilsprites::RenderMetrics;
use config::TextStyle;
use frankenterm_font::shaper::PresentationWidth;
use frankenterm_font::{ClearShapeCache, FontConfiguration, GlyphInfo, LoadedFont, LoadedFontId};
use frankenterm_gui::line_shaping::{cluster_shaping_context, line_paragraph_context, shape_runs};
use frankenterm_gui::metal_scene::{
    CursorSprite, GlyphSource, LineSprite, PlacedGlyph, ShapedGlyph,
};
use frankenterm_renderer_metal::{AtlasKind, AtlasSlot, MetalRenderer};
use lfucache::LfuCache;
use std::collections::HashMap;
use std::convert::Infallible;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use termwiz::cell::VerticalAlign;
use termwiz::cellcluster::CellCluster;
use termwiz::surface::CursorShape;
use window::bitmaps::{BitmapImage, Image};

/// A glyph laid out the WebGpu renderer's way, in an atlas.
#[derive(Debug, Clone, Copy)]
enum LaidGlyph {
    /// A custom block glyph's bitmap, drawn from the cell's top-left.
    Block(PlacedGlyph),
    /// A font glyph: its bitmap, and the offset plus bearing WebGpu places
    /// it by (`x_offset + bearing_x`, `y_offset + bearing_y`).
    Font {
        slot: AtlasSlot,
        x_adjust: f32,
        y_adjust: f32,
        brightness: f32,
    },
}

/// One glyph of a shaped cluster or run: its shaper cell count, and what
/// draws it (`None` for an inkless glyph, which only advances the cells).
#[derive(Debug, Clone, Copy)]
struct RunGlyph {
    num_cells: u8,
    laid: Option<LaidGlyph>,
}

/// WebGpu's glyph key (`GlyphKey`) within one font configuration and cell
/// metrics: the font, the fallback face, the glyph, its cell count, the font
/// style (as its [`FontGlyphs::style_id`]) and whether a space follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    font: LoadedFontId,
    font_idx: usize,
    glyph_pos: u32,
    num_cells: u8,
    style: u32,
    followed_by_space: bool,
}

pub(crate) struct FontGlyphs {
    fonts: Rc<FontConfiguration>,
    config: config::ConfigHandle,
    renderer: Rc<MetalRenderer>,
    cell_height: f32,
    descender: f32,
    /// The cell geometry block, line and cursor sprites are drawn for.
    metrics: RenderMetrics,
    /// The configuration generation the glyphs were rasterized under.
    config_generation: usize,
    /// Font style ids by the address `match_style` returned (stable while
    /// `config` lives), and by value: equal styles share an id, as they
    /// share WebGpu's cache entries.
    style_addresses: HashMap<usize, u32>,
    style_ids: HashMap<TextStyle, u32>,
    /// Shaper output for clusters shaped without paragraph context.
    shapes: LfuCache<(u32, String), Rc<Vec<GlyphInfo>>>,
    /// Laid-out font glyphs, `None` for inkless ones.
    glyphs: HashMap<GlyphKey, Option<LaidGlyph>>,
    /// Custom block glyphs placed in the grayscale atlas.
    blocks: HashMap<BlockKey, PlacedGlyph>,
    /// Line sprites placed in the grayscale atlas (ft-yccm0.4.7.3).
    lines: HashMap<LineSprite, PlacedGlyph>,
    /// Cursor sprites by shape and width in cells (ft-yccm0.4.7.3).
    cursors: HashMap<(CursorSprite, u8), PlacedGlyph>,
    /// Set by the shaper when a fallback font finishes resolving.
    fallback_resolved: Arc<AtomicBool>,
    /// The font's fallback chain grew while shaping this frame: the rows
    /// shaped before then are stale ([`Self::take_chain_changed`]).
    chain_changed: bool,
    /// An atlas refused a glyph: clear the placed glyphs before the next
    /// frame.
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
fn quad_origin(position: f32) -> i16 {
    (position - 0.5)
        .ceil()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

impl FontGlyphs {
    pub(crate) fn new(
        fonts: Rc<FontConfiguration>,
        config: config::ConfigHandle,
        renderer: Rc<MetalRenderer>,
        metrics: &RenderMetrics,
    ) -> Self {
        let config_generation = config.generation();
        let shapes = LfuCache::new(
            "metal_shape_cache.hit.rate",
            "metal_shape_cache.miss.rate",
            |config| config.shape_cache_size,
            &config,
        );
        Self {
            fonts,
            config,
            renderer,
            cell_height: metrics.cell_size.height as f32,
            descender: metrics.descender.get() as f32,
            metrics: *metrics,
            config_generation,
            style_addresses: HashMap::new(),
            style_ids: HashMap::new(),
            shapes,
            glyphs: HashMap::new(),
            blocks: HashMap::new(),
            lines: HashMap::new(),
            cursors: HashMap::new(),
            fallback_resolved: Arc::new(AtomicBool::new(false)),
            chain_changed: false,
            reset: false,
        }
    }

    /// Whether the fallback chain grew while shaping since the last call.
    /// The kept shaper output is then dropped, and the caller rebuilds its
    /// scene so every row is shaped against the grown chain.
    pub(crate) fn take_chain_changed(&mut self) -> bool {
        let changed = std::mem::take(&mut self.chain_changed);
        if changed {
            self.shapes.clear();
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
            && self.metrics.cell_size == metrics.cell_size
            && self.metrics.descender == metrics.descender
            && self.config_generation == config_generation
    }

    /// Readies the caches for a frame. True when the scene must be rebuilt
    /// from scratch, because cached glyphs went stale.
    pub(crate) fn begin_frame(&mut self) -> bool {
        let fallback_resolved = self.fallback_resolved.swap(false, Ordering::AcqRel);
        if fallback_resolved {
            self.shapes.clear();
        }
        let mut rebuild = std::mem::take(&mut self.reset);
        if !rebuild {
            let renderer = &self.renderer;
            let alive = self
                .glyphs
                .values()
                .filter_map(|laid| match laid {
                    Some(LaidGlyph::Font { slot, .. }) => Some(slot),
                    _ => None,
                })
                .chain(self.blocks.values().map(|placed| &placed.slot))
                .chain(self.lines.values().map(|placed| &placed.slot))
                .chain(self.cursors.values().map(|placed| &placed.slot))
                .all(|slot| renderer.touch_glyph(slot));
            rebuild = !alive;
        }
        if rebuild {
            self.glyphs.clear();
            self.blocks.clear();
            self.lines.clear();
            self.cursors.clear();
        }
        rebuild || fallback_resolved
    }

    /// The id of font style `style`, which `match_style` returned for this
    /// source's configuration.
    fn style_id(&mut self, style: &TextStyle) -> u32 {
        let address = std::ptr::from_ref(style).addr();
        if let Some(&id) = self.style_addresses.get(&address) {
            return id;
        }
        let next = u32::try_from(self.style_ids.len()).unwrap_or(u32::MAX);
        let id = *self.style_ids.entry(style.clone()).or_insert(next);
        self.style_addresses.insert(address, id);
        id
    }

    /// Places `image` in an atlas: its premultiplied RGBA in the color
    /// atlas, or its alpha (the coverage) in the grayscale atlas.
    fn place_image(&mut self, image: &Image, color: bool, what: &str) -> Option<AtlasSlot> {
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
            Ok(slot) => Some(slot),
            Err(err) => {
                log::debug!("Metal glyphs: atlas refused {what}: {err}");
                self.reset = true;
                None
            }
        }
    }

    /// Places a grayscale sprite drawn from the cell's top-left.
    fn place_sprite(&mut self, image: &Image, what: &str) -> Option<PlacedGlyph> {
        let slot = self.place_image(image, false, what)?;
        Some(PlacedGlyph {
            slot,
            offset: [0, 0],
        })
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
        self.place_sprite(&image, "a line sprite")
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
        self.place_sprite(&image, "a cursor sprite")
    }

    /// The glyph cache's bitmap for custom block glyph `block`, placed at
    /// the cell's top-left.
    fn block(&mut self, block: BlockKey) -> Option<PlacedGlyph> {
        if let Some(placed) = self.blocks.get(&block) {
            return Some(*placed);
        }
        let image = GlyphCache::block_sprite_image(&self.metrics, block);
        let placed = self.place_sprite(&image, "a block glyph")?;
        self.blocks.insert(block, placed);
        Some(placed)
    }

    /// Runs the shaper as the WebGpu renderer's `cached_cluster_shape` does:
    /// over the paragraph text and the cluster's range in it when `context`
    /// is given, or else over the cluster's text. When the font installs
    /// fallback faces while shaping, it shapes again against the grown chain
    /// and flags the change, so the caller rebuilds the rows it shaped
    /// before, as the WebGpu renderer re-runs its paint pass (bounded as it
    /// is).
    ///
    /// The glyphs' byte clusters are left as the shaper gives them (absolute
    /// in the paragraph): nothing here reads them.
    fn run_shaper(
        &mut self,
        font: &LoadedFont,
        cluster: &CellCluster,
        context: Option<(&str, Range<usize>)>,
    ) -> Option<Vec<GlyphInfo>> {
        let (text, range) = match context {
            Some((text, range)) => (text, Some(range)),
            None => (cluster.text.as_str(), None),
        };
        let presentation_width = match &range {
            Some(range) => PresentationWidth::with_cluster_and_byte_offset(cluster, range.start),
            None => PresentationWidth::with_cluster(cluster),
        };
        let mut attempts = 0;
        loop {
            let resolved = Arc::clone(&self.fallback_resolved);
            match font.shape(
                text,
                move || resolved.store(true, Ordering::Release),
                BlockKey::filter_out_synthetic,
                Some(cluster.presentation),
                cluster.direction,
                range.clone(),
                Some(&presentation_width),
            ) {
                Ok(infos) => return Some(infos),
                Err(err)
                    if err.root_cause().downcast_ref::<ClearShapeCache>().is_some()
                        && attempts < MAX_SHAPE_PASSES =>
                {
                    attempts += 1;
                    self.chain_changed = true;
                }
                Err(err) => {
                    log::warn!("Metal glyphs: shaping {:?} failed: {err:#}", cluster.text);
                    return None;
                }
            }
        }
    }

    /// Lays out one shaped glyph as the WebGpu renderer's
    /// `glyph_infos_to_glyphs` does, through the glyph cache. `None` for an
    /// inkless glyph, or one that failed to lay out or place (not kept).
    fn lay_out(
        &mut self,
        style_id: u32,
        font: &Rc<LoadedFont>,
        info: &GlyphInfo,
        followed_by_space: bool,
    ) -> Option<LaidGlyph> {
        if self.config.custom_block_glyphs {
            if let Some(block) = info.only_char.and_then(BlockKey::from_char) {
                return self.block(block).map(LaidGlyph::Block);
            }
        }
        let key = GlyphKey {
            font: font.id(),
            font_idx: info.font_idx,
            glyph_pos: info.glyph_pos,
            num_cells: info.num_cells,
            style: style_id,
            followed_by_space,
        };
        if let Some(laid) = self.glyphs.get(&key) {
            return *laid;
        }
        let layout = match GlyphCache::lay_out_glyph(
            &self.config,
            info,
            font,
            followed_by_space,
            info.num_cells,
        ) {
            Ok(layout) => layout,
            Err(err) => {
                log::warn!(
                    "Metal glyphs: laying out glyph {} failed: {err:#}",
                    info.glyph_pos
                );
                return None;
            }
        };
        let laid = match &layout.image {
            Some(image) => {
                let slot = self.place_image(image, layout.has_color, "a glyph")?;
                Some(LaidGlyph::Font {
                    slot,
                    x_adjust: (layout.x_offset + layout.bearing_x).get() as f32,
                    y_adjust: (layout.y_offset + layout.bearing_y).get() as f32,
                    brightness: layout.brightness_adjust,
                })
            }
            None => None,
        };
        self.glyphs.insert(key, laid);
        laid
    }

    /// Shapes `cluster` (a cluster, or a run of them joined) with `style`,
    /// as the WebGpu renderer's `cached_cluster_shape` does, and lays out
    /// its glyphs. Empty when shaping failed.
    fn shape(
        &mut self,
        style: &TextStyle,
        cluster: &CellCluster,
        context: Option<(&str, Range<usize>)>,
    ) -> Rc<Vec<RunGlyph>> {
        let font = match self.fonts.resolve_font(style) {
            Ok(font) => font,
            Err(err) => {
                log::warn!("Metal glyphs: no font for {:?}: {err:#}", cluster.text);
                return Rc::new(Vec::new());
            }
        };
        let style_id = self.style_id(style);
        let infos = if context.is_some() {
            match self.run_shaper(&font, cluster, context) {
                Some(infos) => Rc::new(infos),
                None => return Rc::new(Vec::new()),
            }
        } else {
            let key = (style_id, cluster.text.clone());
            match self.shapes.get(&key) {
                Some(infos) => Rc::clone(infos),
                None => match self.run_shaper(&font, cluster, None) {
                    Some(infos) => {
                        let infos = Rc::new(infos);
                        self.shapes.put(key, Rc::clone(&infos));
                        infos
                    }
                    None => return Rc::new(Vec::new()),
                },
            }
        };
        let mut glyphs = Vec::with_capacity(infos.len());
        for (index, info) in infos.iter().enumerate() {
            let followed_by_space = infos.get(index + 1).is_some_and(|next| next.is_space);
            glyphs.push(RunGlyph {
                num_cells: info.num_cells,
                laid: self.lay_out(style_id, &font, info, followed_by_space),
            });
        }
        Rc::new(glyphs)
    }

    /// Where `laid` is drawn from the top-left of its cell in a cluster with
    /// vertical alignment `valign`, as the WebGpu renderer positions it.
    fn place(&self, laid: LaidGlyph, valign: VerticalAlign) -> (PlacedGlyph, f32) {
        match laid {
            LaidGlyph::Block(placed) => (placed, 1.0),
            LaidGlyph::Font {
                slot,
                x_adjust,
                y_adjust,
                brightness,
            } => {
                // The WebGpu renderer's super- and subscript shift.
                let valign = match valign {
                    VerticalAlign::BaseLine => 0.0,
                    VerticalAlign::SuperScript => self.cell_height * -0.25,
                    VerticalAlign::SubScript => self.cell_height * 0.25,
                };
                let top = self.cell_height + (self.descender + valign - y_adjust);
                let offset = [quad_origin(x_adjust), quad_origin(top)];
                (PlacedGlyph { slot, offset }, brightness)
            }
        }
    }
}

impl GlyphSource for FontGlyphs {
    fn shape_row(&mut self, clusters: &[CellCluster]) -> Vec<ShapedGlyph> {
        let fonts = Rc::clone(&self.fonts);
        let config = self.config.clone();
        let styles: Vec<&TextStyle> = clusters
            .iter()
            .map(|cluster| fonts.match_style(&config, &cluster.attrs))
            .collect();
        let paragraph_context = line_paragraph_context(clusters);
        let Ok(runs) = shape_runs(
            clusters,
            &styles,
            paragraph_context.as_ref(),
            |glyph: &RunGlyph| glyph.num_cells,
            |style, cluster, context| Ok::<_, Infallible>(self.shape(style, cluster, context)),
        );
        let mut run_glyphs = runs.glyphs;
        let mut shaped = Vec::new();
        // The WebGpu renderer advances along the line by every glyph's cell
        // count, from the first cluster on.
        let mut cell = 0usize;
        for (idx, cluster) in clusters.iter().enumerate() {
            let glyphs = match run_glyphs[idx].take() {
                Some(glyphs) => glyphs,
                None => {
                    let context = cluster_shaping_context(paragraph_context.as_ref(), cluster, idx);
                    self.shape(styles[idx], cluster, context)
                }
            };
            let valign = cluster.attrs.vertical_align();
            for glyph in glyphs.iter() {
                if let Some(laid) = glyph.laid {
                    let (glyph, brightness) = self.place(laid, valign);
                    shaped.push(ShapedGlyph {
                        cell,
                        glyph,
                        brightness,
                    });
                }
                cell += usize::from(glyph.num_cells);
            }
        }
        shaped
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
