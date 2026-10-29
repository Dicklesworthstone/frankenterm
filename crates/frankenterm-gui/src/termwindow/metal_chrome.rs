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
use frankenterm_renderer_metal::ui_quads::{UI_ATLAS_STRIPE_ROWS, UiAtlasPixels, UiAtlasStripe};
use frankenterm_renderer_metal::{UiAtlas, UiLayer, UiQuad};
use std::cell::{Cell, RefCell};
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

/// An immutable snapshot whose unchanged stripes share storage with earlier
/// frames, including frames still being drawn on the render thread.
pub(crate) struct ChromeAtlas {
    width: u32,
    height: u32,
    version: u64,
    stripes: Vec<UiAtlasStripe>,
}

/// Tracks writes at the texture boundary, including atlas clears. Damage
/// does not depend on how the glyph cache allocates or reuses its sprites.
struct ChromeTexture {
    texture: ImageTexture,
    dirty: RefCell<Vec<bool>>,
    changed: Cell<bool>,
    snapshot: RefCell<Option<Arc<ChromeAtlas>>>,
}

impl ChromeTexture {
    fn new(width: usize, height: usize) -> Self {
        Self {
            texture: ImageTexture::new(width, height),
            dirty: RefCell::new(vec![true; height.div_ceil(UI_ATLAS_STRIPE_ROWS as usize)]),
            changed: Cell::new(true),
            snapshot: RefCell::new(None),
        }
    }

    /// Shares unchanged pixels, copying only the stripes touched since the
    /// previous snapshot. Every snapshot remains complete, so skipped frame
    /// publications and independently lagging GPU slots need no damage log.
    fn snapshot(&self) -> Arc<ChromeAtlas> {
        let mut snapshot = self.snapshot.borrow_mut();
        if let Some(atlas) = snapshot.as_ref()
            && !self.changed.get()
        {
            return Arc::clone(atlas);
        }
        let image = self.texture.image.borrow();
        let (width, height) = image.image_dimensions();
        let version = NEXT_ATLAS_VERSION
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .expect("chrome atlas content version space exhausted");
        let rgba = bytemuck::cast_slice::<u32, u8>(image.pixels());
        let mut dirty = self.dirty.borrow_mut();
        let stripes = dirty
            .iter()
            .enumerate()
            .map(|(index, changed)| {
                if !changed && let Some(previous) = snapshot.as_ref() {
                    return previous.stripes[index].clone();
                }
                let first = index * UI_ATLAS_STRIPE_ROWS as usize;
                let end = (first + UI_ATLAS_STRIPE_ROWS as usize).min(height);
                UiAtlasStripe {
                    version,
                    rgba: Arc::from(&rgba[first * width * 4..end * width * 4]),
                }
            })
            .collect();
        let atlas = Arc::new(ChromeAtlas {
            width: u32::try_from(width).unwrap_or(u32::MAX),
            height: u32::try_from(height).unwrap_or(u32::MAX),
            version,
            stripes,
        });
        *snapshot = Some(Arc::clone(&atlas));
        dirty.fill(false);
        self.changed.set(false);
        atlas
    }
}

impl Texture2d for ChromeTexture {
    // The clipped image coordinates are nonnegative and fit usize.
    #[allow(clippy::cast_sign_loss)]
    fn write(&self, rect: ::window::Rect, image: &dyn BitmapImage) {
        self.texture.write(rect, image);
        // ImageTexture draws the source image at rect.origin. Include its
        // actual height and clip the damage as the bitmap does.
        let height = isize::try_from(self.height()).unwrap_or(isize::MAX);
        let source_height = isize::try_from(image.image_dimensions().1).unwrap_or(isize::MAX);
        let first = rect.origin.y.clamp(0, height) as usize;
        let end = rect.origin.y.saturating_add(source_height).clamp(0, height) as usize;
        if first < end {
            let rows = UI_ATLAS_STRIPE_ROWS as usize;
            self.dirty.borrow_mut()[first / rows..end.div_ceil(rows)].fill(true);
            self.changed.set(true);
        }
    }

    fn read(&self, rect: ::window::Rect, image: &mut dyn BitmapImage) -> anyhow::Result<()> {
        self.texture.read(rect, image)
    }

    fn width(&self) -> usize {
        self.texture.width()
    }

    fn height(&self) -> usize {
        self.texture.height()
    }
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
                pixels: UiAtlasPixels::Stripes(&self.atlas.stripes),
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
    surface: Rc<ChromeTexture>,
    layers: RefCell<BTreeMap<i8, HeapQuadAllocator>>,
    /// The fonts and cell size the glyph cache rasterizes for.
    fonts: Rc<FontConfiguration>,
    cell_size: (isize, isize),
    side: usize,
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
        let surface = Rc::new(ChromeTexture::new(side, side));
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
            atlas: chrome.surface.snapshot(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use ::window::bitmaps::Image;
    use ::window::{Point, Rect, Size};

    fn snapshot_pixels(atlas: &ChromeAtlas) -> Vec<u8> {
        atlas
            .stripes
            .iter()
            .flat_map(|stripe| stripe.rgba.iter().copied())
            .collect()
    }

    #[test]
    fn chrome_snapshots_share_unchanged_stripes_and_preserve_in_flight_pixels() {
        let texture = ChromeTexture::new(2, 70);
        let initial = texture.snapshot();
        assert_eq!((initial.width, initial.height), (2, 70));
        assert_eq!(
            initial
                .stripes
                .iter()
                .map(|stripe| stripe.rgba.len())
                .collect::<Vec<_>>(),
            [256, 256, 48]
        );
        assert_eq!(snapshot_pixels(&initial), vec![0; 560]);
        assert!(Arc::ptr_eq(&initial, &texture.snapshot()));

        // ImageTexture writes the source image's extent, not rect.size.
        // Two actual source rows straddle the stripe boundary at row 32.
        texture.write(
            Rect::new(Point::new(0, 31), Size::new(1, 1)),
            &Image::from_raw(1, 2, vec![17; 8]),
        );
        let boundary = texture.snapshot();
        assert_ne!(boundary.version, initial.version);
        for index in 0..2 {
            assert_ne!(
                boundary.stripes[index].version,
                initial.stripes[index].version
            );
            assert!(!Arc::ptr_eq(
                &boundary.stripes[index].rgba,
                &initial.stripes[index].rgba
            ));
        }
        assert_eq!(boundary.stripes[2].version, initial.stripes[2].version);
        assert!(Arc::ptr_eq(
            &boundary.stripes[2].rgba,
            &initial.stripes[2].rgba
        ));
        let mut expected = vec![0; 560];
        expected[31 * 8..31 * 8 + 4].fill(17);
        expected[32 * 8..32 * 8 + 4].fill(17);
        assert_eq!(snapshot_pixels(&boundary), expected);
        assert_eq!(snapshot_pixels(&initial), vec![0; 560]);
        assert!(Arc::ptr_eq(&boundary, &texture.snapshot()));

        // Source rows beyond the atlas are clipped, and the final six-row
        // stripe is retained at its exact length rather than padded to 32.
        texture.write(
            Rect::new(Point::new(1, 69), Size::new(1, 1)),
            &Image::from_raw(1, 8, vec![29; 32]),
        );
        let last = texture.snapshot();
        for index in 0..2 {
            assert!(Arc::ptr_eq(
                &last.stripes[index].rgba,
                &boundary.stripes[index].rgba
            ));
            assert_eq!(last.stripes[index].version, boundary.stripes[index].version);
        }
        assert_ne!(last.stripes[2].version, boundary.stripes[2].version);
        assert_eq!(last.stripes[2].rgba.len(), 48);
        assert_eq!(snapshot_pixels(&boundary), expected);
        expected[69 * 8 + 4..70 * 8].fill(29);
        assert_eq!(snapshot_pixels(&last), expected);
        let mut readback = Image::new(2, 70);
        texture
            .read(Rect::new(Point::new(0, 0), Size::new(2, 70)), &mut readback)
            .unwrap();
        assert_eq!(readback.pixel_data_slice(), expected);

        // Atlas::clear uses this same whole-image write path. Every stripe
        // must get a new identity while older snapshots keep their pixels.
        texture.write(
            Rect::new(Point::new(0, 0), Size::new(2, 70)),
            &Image::new(2, 70),
        );
        let cleared = texture.snapshot();
        for (current, previous) in cleared.stripes.iter().zip(&last.stripes) {
            assert_ne!(current.version, previous.version);
            assert!(!Arc::ptr_eq(&current.rgba, &previous.rgba));
        }
        assert_eq!(snapshot_pixels(&cleared), vec![0; 560]);
        assert_eq!(snapshot_pixels(&last), expected);
        assert_eq!(snapshot_pixels(&initial), vec![0; 560]);
    }

    #[test]
    fn chrome_damage_clips_negative_rows_and_recreated_atlases_use_new_versions() {
        let texture = ChromeTexture::new(2, 65);
        let initial = texture.snapshot();
        texture.write(
            Rect::new(Point::new(0, -4), Size::new(1, 1)),
            &Image::from_raw(1, 2, vec![7; 8]),
        );
        assert!(
            Arc::ptr_eq(&initial, &texture.snapshot()),
            "fully clipped writes publish no damage"
        );
        texture.write(
            Rect::new(Point::new(0, -1), Size::new(1, 1)),
            &Image::from_raw(1, 2, vec![19; 8]),
        );
        let clipped = texture.snapshot();
        assert_ne!(clipped.stripes[0].version, initial.stripes[0].version);
        for index in 1..3 {
            assert!(Arc::ptr_eq(
                &clipped.stripes[index].rgba,
                &initial.stripes[index].rgba
            ));
        }
        let mut expected = vec![0; 520];
        expected[..4].fill(19);
        assert_eq!(snapshot_pixels(&clipped), expected);
        let recreated = ChromeTexture::new(2, 65).snapshot();
        assert_ne!(recreated.version, initial.version);
        assert_ne!(recreated.version, clipped.version);
        for stripe in &recreated.stripes {
            assert!(
                initial
                    .stripes
                    .iter()
                    .all(|old| old.version != stripe.version)
            );
            assert!(
                clipped
                    .stripes
                    .iter()
                    .all(|old| old.version != stripe.version)
            );
        }
        assert_eq!(snapshot_pixels(&recreated), vec![0; 520]);
    }
}
