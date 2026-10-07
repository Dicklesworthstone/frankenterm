//! The Metal renderer's glyphs, from the window's fonts (ft-yccm0.4.4).
//!
//! [`FontGlyphs`] is the [`GlyphSource`] of the live Metal frame:
//! - it resolves the font a glyph style selects (the configured `font_rules`
//!   applied to the style's intensity and italic);
//! - it shapes the grapheme with HarfBuzz and rasterizes each glyph with the
//!   configured rasterizer (CoreText by default on macOS);
//! - it places the result in the renderer's atlases: coverage in the R8
//!   atlas, premultiplied color in the BGRA atlas.
//!
//! Glyphs are positioned as the WebGpu renderer positions them: the pen
//! advances through the cluster's glyphs, each sits at its shaped offset plus
//! its bearing, and the baseline is the cell's descender above the bottom.
//!
//! Decorations are the WebGpu renderer's line sprites (ft-yccm0.4.7.3): the
//! same cell-sized bitmaps ([`GlyphCache::line_sprite_image`]) placed in the
//! grayscale atlas, so underline patterns match pixel for pixel.
//!
//! Placed glyphs are cached by grapheme, style and width. Every frame touches
//! the cached slots, so the atlases keep them.
//! - A fallback font resolving asynchronously (a completion from the shaper)
//!   or a slot the atlas evicted anyway clears the cache and asks for a full
//!   scene rebuild.
//! - So does a full atlas, after which the cache refills with what the frame
//!   still draws.
//!
//! Not yet drawn the WebGpu way:
//! - custom block glyphs (box drawing comes from the font);
//! - emoji scaled to fit the cell;
//! - double-width and double-height lines;
//! - ligatures across cells (every cell is shaped alone).

use crate::glyphcache::GlyphCache;
use crate::utilsprites::RenderMetrics;
use frankenterm_font::FontConfiguration;
use frankenterm_font::rasterizer::RasterizedGlyph;
use frankenterm_gui::metal_scene::{GlyphSource, GlyphStyle, LineSprite, PlacedGlyph};
use frankenterm_renderer_metal::{AtlasKind, MetalRenderer};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use termwiz::cell::{CellAttributes, Intensity};
use wezterm_bidi::Direction;
use window::bitmaps::BitmapImage;

type ByText = HashMap<String, Vec<PlacedGlyph>>;

pub(crate) struct FontGlyphs {
    fonts: Rc<FontConfiguration>,
    config: config::ConfigHandle,
    renderer: Rc<MetalRenderer>,
    cell_height: f64,
    descender: f64,
    /// The cell geometry line sprites are drawn for.
    metrics: RenderMetrics,
    /// The configuration generation the glyphs were rasterized under.
    config_generation: usize,
    placed: HashMap<(GlyphStyle, usize), ByText>,
    /// Line sprites placed in the grayscale atlas (ft-yccm0.4.7.3).
    lines: HashMap<LineSprite, PlacedGlyph>,
    /// Set by the shaper when a fallback font finishes resolving.
    fallback_resolved: Arc<AtomicBool>,
    /// An atlas refused a glyph: clear the cache before the next frame.
    reset: bool,
}

/// The atlas a rasterized glyph goes to, and its pixels in that atlas's
/// layout: one coverage byte per pixel, or premultiplied `[B, G, R, A]`.
fn atlas_pixels(raster: &RasterizedGlyph) -> (AtlasKind, Vec<u8>) {
    if raster.has_color {
        let pixels = raster
            .data
            .chunks_exact(4)
            .flat_map(|rgba| [rgba[2], rgba[1], rgba[0], rgba[3]])
            .collect();
        (AtlasKind::Color, pixels)
    } else {
        let pixels = raster.data.chunks_exact(4).map(|rgba| rgba[3]).collect();
        (AtlasKind::Grayscale, pixels)
    }
}

/// A pixel offset as the instance stores it.
// Offsets are a few cells at most.
#[allow(clippy::cast_possible_truncation)]
fn offset_px(value: f64) -> i16 {
    value
        .round()
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
            placed: HashMap::new(),
            lines: HashMap::new(),
            fallback_resolved: Arc::new(AtomicBool::new(false)),
            reset: false,
        }
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
                .placed
                .values()
                .flat_map(HashMap::values)
                .flatten()
                .chain(self.lines.values())
                .all(|glyph| renderer.touch_glyph(&glyph.slot));
            rebuild = !alive;
        }
        if rebuild {
            self.placed.clear();
            self.lines.clear();
        }
        rebuild
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
        let (width, height) = image.image_dimensions();
        let coverage: Vec<u8> = image
            .pixel_data_slice()
            .chunks_exact(4)
            .map(|rgba| rgba[3])
            .collect();
        let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
            return None;
        };
        match self
            .renderer
            .insert_glyph(AtlasKind::Grayscale, width, height, &coverage)
        {
            Ok(slot) => Some(PlacedGlyph {
                slot,
                offset: [0, 0],
            }),
            Err(err) => {
                log::debug!("Metal glyphs: atlas refused line sprite {lines:?}: {err}");
                self.reset = true;
                None
            }
        }
    }

    fn place(&mut self, text: &str, style: GlyphStyle) -> Vec<PlacedGlyph> {
        let mut attrs = CellAttributes::default();
        attrs.set_intensity(if style.bold {
            Intensity::Bold
        } else if style.half {
            Intensity::Half
        } else {
            Intensity::Normal
        });
        attrs.set_italic(style.italic);
        let text_style = self.fonts.match_style(&self.config, &attrs).clone();
        let font = match self.fonts.resolve_font(&text_style) {
            Ok(font) => font,
            Err(err) => {
                log::warn!("Metal glyphs: no font for {text:?}: {err:#}");
                return Vec::new();
            }
        };
        let resolved = Arc::clone(&self.fallback_resolved);
        let presentation = termwiz::cell::Presentation::for_grapheme(text).0;
        let infos = match font.shape(
            text,
            move || resolved.store(true, Ordering::Release),
            |_| {},
            Some(presentation),
            Direction::LeftToRight,
            None,
            None,
        ) {
            Ok(infos) => infos,
            Err(err) => {
                log::warn!("Metal glyphs: shaping {text:?} failed: {err:#}");
                return Vec::new();
            }
        };
        let mut placed = Vec::with_capacity(infos.len());
        let mut pen = 0.0;
        for info in &infos {
            let raster = match font.rasterize_glyph(info.glyph_pos, info.font_idx) {
                Ok(raster) => raster,
                Err(err) => {
                    log::warn!(
                        "Metal glyphs: rasterizing glyph {} failed: {err:#}",
                        info.glyph_pos
                    );
                    pen += info.x_advance.get();
                    continue;
                }
            };
            if raster.width > 0 && raster.height > 0 {
                let (kind, pixels) = atlas_pixels(&raster);
                let size = (u32::try_from(raster.width), u32::try_from(raster.height));
                let (Ok(width), Ok(height)) = size else {
                    continue;
                };
                match self.renderer.insert_glyph(kind, width, height, &pixels) {
                    Ok(slot) => placed.push(PlacedGlyph {
                        slot,
                        offset: [
                            offset_px(pen + info.x_offset.get() + raster.bearing_x.get()),
                            offset_px(
                                self.cell_height + self.descender
                                    - (info.y_offset.get() + raster.bearing_y.get()),
                            ),
                        ],
                    }),
                    Err(err) => {
                        log::debug!(
                            "Metal glyphs: atlas refused glyph {}: {err}",
                            info.glyph_pos
                        );
                        self.reset = true;
                    }
                }
            }
            pen += info.x_advance.get();
        }
        placed
    }
}

impl GlyphSource for FontGlyphs {
    fn glyphs(&mut self, text: &str, style: GlyphStyle, width: usize) -> &[PlacedGlyph] {
        let known = self
            .placed
            .get(&(style, width))
            .is_some_and(|by_text| by_text.contains_key(text));
        if !known {
            let placed = self.place(text, style);
            self.placed
                .entry((style, width))
                .or_default()
                .insert(text.to_string(), placed);
        }
        self.placed
            .get(&(style, width))
            .and_then(|by_text| by_text.get(text))
            .map_or(&[], Vec::as_slice)
    }

    fn line_sprite(&mut self, lines: LineSprite) -> Option<PlacedGlyph> {
        if let Some(placed) = self.lines.get(&lines) {
            return Some(*placed);
        }
        let placed = self.place_line(lines)?;
        self.lines.insert(lines, placed);
        Some(placed)
    }
}
