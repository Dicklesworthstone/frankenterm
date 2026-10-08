//! Window chrome on the Metal front end (ft-yccm0.4.7.1).
//!
//! The WebGpu front end lays out and draws window chrome (the fancy tab bar
//! and modal overlays) into quads, with its glyph cache. A Metal window has
//! no WebGpu render state, so it keeps a [`MetalChrome`] as its
//! [`ChromeTarget`]: a glyph cache over a CPU texture, and a quad layer per
//! zindex. The same layout and paint code (`paint_fancy_tab_bar`,
//! `paint_modal`) runs against it and registers the same hit-test items. The
//! recorded quads become [`UiQuad`]s that the Metal renderer draws as WebGpu's
//! quad shader draws them, sampling a copy of the glyph cache's texture.

use super::TermWindow;
use super::box_model::ChromeTarget;
use super::render::paint::AllowImage;
use crate::glyphcache::GlyphCache;
use crate::quad::{HeapQuadAllocator, TripleLayerQuadAllocator, V_BOT_RIGHT, V_TOP_LEFT, Vertex};
use crate::utilsprites::{RenderMetrics, UtilSprites};
use ::window::bitmaps::atlas::OutOfTextureSpace;
use ::window::bitmaps::{BitmapImage, ImageTexture, Texture2d};
use frankenterm_font::FontConfiguration;
use frankenterm_renderer_metal::{UiAtlas, UiLayer, UiQuad};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The chrome glyph cache's first texture side; it doubles when full.
const FIRST_SIDE: usize = 512;
/// The largest side it grows to.
const MAX_SIDE: usize = 4096;

/// Versions of the chrome atlas copies handed to renderers. One counter for
/// the process, so a rebuilt glyph cache (whose own version restarts) never
/// repeats a version a renderer already holds.
static NEXT_ATLAS_VERSION: AtomicU64 = AtomicU64::new(1);

/// A copy of the chrome glyph cache's texture.
pub(crate) struct ChromeAtlas {
    width: u32,
    height: u32,
    version: u64,
    rgba: Vec<u8>,
}

/// One frame's window chrome, as the render thread draws it.
#[derive(Clone)]
pub(crate) struct ChromeFrame {
    quads: Arc<[UiQuad]>,
    /// How many of `quads`, from the first, are window background layers,
    /// drawn under the panes.
    under: usize,
    atlas: Arc<ChromeAtlas>,
    foreground_text_hsb: [f32; 3],
}

impl ChromeFrame {
    pub(crate) fn layer(&self) -> UiLayer<'_> {
        UiLayer {
            atlas: UiAtlas {
                width: self.atlas.width,
                height: self.atlas.height,
                version: self.atlas.version,
                rgba: &self.atlas.rgba,
            },
            quads: &self.quads,
            under: self.under,
            // Pane images under the text come with ft-yccm0.4.7.2's GUI half.
            under_text: 0,
            foreground_text_hsb: self.foreground_text_hsb,
        }
    }

    /// Window background layers were drawn: as the WebGpu renderer does
    /// then, nothing else fills the window or the panes' backgrounds.
    pub(crate) fn has_background_layers(&self) -> bool {
        self.under > 0
    }
}

/// A Metal window's chrome target.
pub(crate) struct MetalChrome {
    glyph_cache: RefCell<GlyphCache>,
    util_sprites: UtilSprites,
    surface: Rc<ImageTexture>,
    layers: RefCell<BTreeMap<i8, HeapQuadAllocator>>,
    /// The fonts and cell size the glyph cache rasterizes for.
    fonts: Rc<FontConfiguration>,
    cell_size: (isize, isize),
    side: usize,
    /// The last atlas copy, with the glyph cache atlas version it holds.
    snapshot: RefCell<Option<(u64, Arc<ChromeAtlas>)>>,
}

impl ChromeTarget for MetalChrome {
    fn glyph_cache(&self) -> &RefCell<GlyphCache> {
        &self.glyph_cache
    }

    fn util_sprites(&self) -> &UtilSprites {
        &self.util_sprites
    }

    fn draw_in_layer(
        &self,
        zindex: i8,
        draw: &mut dyn FnMut(&mut TripleLayerQuadAllocator) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut layers = self.layers.borrow_mut();
        let layer = layers.entry(zindex).or_default();
        draw(&mut TripleLayerQuadAllocator::Heap(layer))
    }
}

impl MetalChrome {
    fn new(
        fonts: &Rc<FontConfiguration>,
        metrics: &RenderMetrics,
        side: usize,
    ) -> anyhow::Result<Self> {
        let surface = Rc::new(ImageTexture::new(side, side));
        let mut glyph_cache =
            GlyphCache::with_atlas_surface(fonts, Rc::clone(&surface) as Rc<dyn Texture2d>)?;
        let util_sprites = UtilSprites::new(&mut glyph_cache, metrics)?;
        Ok(Self {
            glyph_cache: RefCell::new(glyph_cache),
            util_sprites,
            surface,
            layers: RefCell::new(BTreeMap::new()),
            fonts: Rc::clone(fonts),
            cell_size: (metrics.cell_size.width, metrics.cell_size.height),
            side,
            snapshot: RefCell::new(None),
        })
    }

    fn serves(&self, fonts: &Rc<FontConfiguration>, metrics: &RenderMetrics) -> bool {
        Rc::ptr_eq(&self.fonts, fonts)
            && self.cell_size == (metrics.cell_size.width, metrics.cell_size.height)
    }

    /// The quads drawn since the layers were cleared, in the order the
    /// WebGpu front end draws them: by zindex, then by layer.
    /// Also how many come from negative zindexes, the window background
    /// layers the WebGpu renderer draws under the panes (zindex 0).
    fn quads(&self, half_width: f32, half_height: f32) -> (Arc<[UiQuad]>, usize) {
        let layers = self.layers.borrow();
        let under = layers
            .range(..0)
            .map(|(_, layer)| layer.vertices().count())
            .sum();
        let quads = layers
            .values()
            .flat_map(HeapQuadAllocator::vertices)
            .map(|vertices| ui_quad(&vertices, half_width, half_height))
            .collect();
        (quads, under)
    }

    /// A copy of the glyph cache's texture, made again only when it changed.
    fn atlas(&self) -> Arc<ChromeAtlas> {
        let version = self.glyph_cache.borrow().atlas.version();
        let mut snapshot = self.snapshot.borrow_mut();
        if let Some((held, atlas)) = snapshot.as_ref()
            && *held == version
        {
            return Arc::clone(atlas);
        }
        let image = self.surface.image.borrow();
        let (width, height) = image.image_dimensions();
        let atlas = Arc::new(ChromeAtlas {
            width: u32::try_from(width).unwrap_or(u32::MAX),
            height: u32::try_from(height).unwrap_or(u32::MAX),
            version: NEXT_ATLAS_VERSION.fetch_add(1, Ordering::Relaxed),
            rgba: bytemuck::cast_slice::<u32, u8>(image.pixels()).to_vec(),
        });
        *snapshot = Some((version, Arc::clone(&atlas)));
        atlas
    }
}

/// A recorded quad as the Metal renderer draws it: positions move from the
/// WebGpu projection's window-centered pixels to drawable pixels.
fn ui_quad(
    vertices: &[Vertex; crate::quad::VERTICES_PER_CELL],
    half_width: f32,
    half_height: f32,
) -> UiQuad {
    let (top_left, bottom_right) = (&vertices[V_TOP_LEFT], &vertices[V_BOT_RIGHT]);
    UiQuad {
        rect: [
            top_left.position[0] + half_width,
            top_left.position[1] + half_height,
            bottom_right.position[0] + half_width,
            bottom_right.position[1] + half_height,
        ],
        uv: [
            top_left.tex[0],
            top_left.tex[1],
            bottom_right.tex[0],
            bottom_right.tex[1],
        ],
        fg: top_left.fg_color,
        alt: top_left.alt_color,
        mix: top_left.mix_value,
        hsv: top_left.hsv,
        kind: top_left.has_color,
    }
}

impl TermWindow {
    /// Lays out and paints this Metal window's chrome, the tab bar and any
    /// modal overlay, appending their hit-test items to `ui_items`
    /// as `paint_pass` does, and returns its quads. `None` without chrome.
    // Window pixel sizes are far inside f32's exact integers.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn metal_chrome_frame(&mut self) -> Option<ChromeFrame> {
        let tab_bar = self.show_tab_bar;
        // As paint_pass: background layers when images are allowed.
        let backgrounds = !self.window_background.is_empty()
            && matches!(self.allow_images, AllowImage::Yes | AllowImage::Scale(_));
        if !tab_bar && !backgrounds && self.get_modal().is_none() {
            return None;
        }
        let mut side = self
            .metal_chrome
            .as_ref()
            .map_or(FIRST_SIDE, |chrome| chrome.side);
        loop {
            let fresh = !self.metal_chrome.as_ref().is_some_and(|chrome| {
                chrome.side == side && chrome.serves(&self.fonts, &self.render_metrics)
            });
            if fresh {
                match MetalChrome::new(&self.fonts, &self.render_metrics, side) {
                    Ok(chrome) => self.metal_chrome = Some(chrome),
                    Err(err) => {
                        log::error!("Metal chrome: {err:#}");
                        return None;
                    }
                }
                // Its cached glyphs point into the old glyph cache's texture.
                self.fancy_tab_bar = None;
            }
            if let Some(chrome) = &self.metal_chrome {
                chrome.layers.borrow_mut().clear();
            }
            let items_before = self.ui_items.len();
            match self.paint_metal_chrome(backgrounds, tab_bar) {
                Ok(()) => break,
                Err(err)
                    if err
                        .root_cause()
                        .downcast_ref::<OutOfTextureSpace>()
                        .is_some()
                        && side < MAX_SIDE =>
                {
                    // Grow the glyph cache and lay the chrome out again.
                    self.ui_items.truncate(items_before);
                    side *= 2;
                }
                Err(err) => {
                    self.ui_items.truncate(items_before);
                    log::warn!("Metal chrome not drawn: {err:#}");
                    return None;
                }
            }
        }
        let chrome = self.metal_chrome.as_ref()?;
        let (quads, under) = chrome.quads(
            self.dimensions.pixel_width as f32 / 2.0,
            self.dimensions.pixel_height as f32 / 2.0,
        );
        Some(ChromeFrame {
            quads,
            under,
            atlas: chrome.atlas(),
            foreground_text_hsb: {
                let hsb = self.config.foreground_text_hsb;
                [hsb.hue, hsb.saturation, hsb.brightness]
            },
        })
    }

    /// The window background layers (when `backgrounds`), the fancy or
    /// retro tab bar (when `tab_bar`), then any modal, as `paint_pass`
    /// paints them for the other front ends.
    fn paint_metal_chrome(&mut self, backgrounds: bool, tab_bar: bool) -> anyhow::Result<()> {
        if backgrounds {
            let bg_color = self.palette().background.to_linear();
            let top = self
                .get_panes_to_render()
                .iter()
                .find(|pos| pos.is_active)
                .map(|pos| match self.get_viewport(pos.pane.pane_id()) {
                    Some(top) => top,
                    None => pos.pane.render_facts().dimensions.physical_top,
                })
                .unwrap_or(0);
            // Layers still loading draw nothing yet, as on WebGpu, which then
            // fills the terminal background instead: so does the Metal frame
            // while no layer quad was drawn.
            self.render_backgrounds(bg_color, top)?;
        }
        if tab_bar && self.config.use_fancy_tab_bar {
            if self.fancy_tab_bar.is_none() {
                let palette = self.palette().clone();
                let bar = self.build_fancy_tab_bar(&palette)?;
                self.fancy_tab_bar.replace(bar);
            }
            let mut items = self.paint_fancy_tab_bar()?;
            self.ui_items.append(&mut items);
        } else if tab_bar {
            // The retro tab bar is a screen line in the WebGpu renderer's
            // zindex 0 layer, painted after the panes.
            let palette = self.palette().clone();
            let mut items = Vec::new();
            let chrome = self.chrome()?;
            chrome.draw_in_layer(0, &mut |layers| {
                items = self.paint_retro_tab_bar(layers, &palette)?;
                Ok(())
            })?;
            self.ui_items.append(&mut items);
        }
        self.paint_modal()
    }
}
