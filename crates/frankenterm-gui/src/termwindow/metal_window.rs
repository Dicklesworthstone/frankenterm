//! Multi-pane Metal frames (ft-yccm0.4.6), the window's side.
//!
//! A Metal window frame draws every visible pane in one render pass
//! (`MetalRenderer::render_window`). The main thread captures one
//! [`MetalPaneRequest`] per visible pane:
//! - its frame inputs;
//! - the rectangle it draws in: its cells, plus its share of the padding and
//!   of the split gaps, where the WebGpu renderer fills its background;
//! - its default background;
//! - the configured `inactive_pane_hsb` dimming when it is inactive.
//!
//! The window chrome the WebGpu renderer draws as quads becomes
//! [`SolidRect`] fills over the panes' text, from the same geometry: the
//! window border, agent and floating-pane borders, the scrollbar thumb and
//! the splits. The visual bell's background flash recolors the pane's
//! background. The hit-test items for splits and the scrollbar are
//! registered as `paint_pass` registers them (ft-yccm0.4.7.1).
//!
//! Wherever the window is drawn, [`MetalPanes`] keeps one `MetalFrame`
//! (render mirror, scene and glyphs) per visible pane. A pane whose rows did
//! not change rebuilds no row, and the renderer uploads none of its rows.

use super::metal_cells::{
    MetalBlinkClocks, MetalBlinkPhase, MetalCompose, MetalFrame, MetalFrameInputs,
};
use super::metal_glyphs::FallbackReady;
use super::render::pane::pane_border_rects;
use super::render::split::split_render_geometry;
use super::{PendingFallbackInvalidation, TermWindow, TermWindowNotif, UIItem, UIItemType};
use crate::utilsprites::RenderMetrics;
use ::window::RectF;
use ::window::color::LinearRgba;
use ::window::{DeadKeyStatus, WindowOps};
use config::VisualBellTarget;
use frankenterm_font::FontConfiguration;
use frankenterm_renderer_metal::{
    ClearColor, MetalRenderer, PaneScene, PixelRect, SolidRect, UiLayer, WindowFrame,
};
use mux::pane::PaneId;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;
use wezterm_term::color::SrgbaTuple;

/// One visible pane of a Metal window frame, captured on the main thread.
#[derive(Clone)]
pub(crate) struct MetalPaneRequest {
    pub(crate) inputs: MetalFrameInputs,
    /// Where the pane draws, in drawable pixels.
    pub(crate) rect: PixelRect,
    /// Its default background, scaled by `window_background_opacity`.
    pub(crate) clear: ClearColor,
    /// `inactive_pane_hsb` for an inactive pane, unless it is the identity.
    pub(crate) hsb: Option<[f32; 3]>,
}

/// The pixels a GPU rasterizes for a rectangle: those whose centers lie
/// inside it.
// Window pixel coordinates are far inside u32, and clamped non-negative.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn pixel_rect(x: f32, y: f32, width: f32, height: f32) -> PixelRect {
    let first = |start: f32| (start - 0.5).ceil().max(0.0);
    let (left, top) = (first(x), first(y));
    let (right, bottom) = (first(x + width).max(left), first(y + height).max(top));
    PixelRect::new(
        left as u32,
        top as u32,
        (right - left) as u32,
        (bottom - top) as u32,
    )
}

/// Where a window's panes are, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PaneLayout {
    cell_width: f32,
    cell_height: f32,
    /// Where the first column starts: left padding and border.
    left: f32,
    /// Where the first row starts: a top tab bar, top padding and border.
    top: f32,
    padding_top: f32,
    /// The tab's size in cells.
    cols: usize,
    rows: usize,
    pixel_width: f32,
    pixel_height: f32,
}

// Cell counts and pixel sizes are small.
#[allow(clippy::cast_precision_loss)]
impl PaneLayout {
    /// Where the grid of the pane whose top-left cell is `(left, top)`
    /// starts.
    fn grid_origin(&self, left: usize, top: usize) -> [f32; 2] {
        [
            self.left + left as f32 * self.cell_width,
            self.top + top as f32 * self.cell_height,
        ]
    }

    /// The pixels a pane fills: [`Self::background_rect`].
    fn pane_rect(&self, left: usize, top: usize, width: usize, height: usize) -> PixelRect {
        let rect = self.background_rect(left, top, width, height);
        pixel_rect(rect.min_x(), rect.min_y(), rect.width(), rect.height())
    }

    /// The rectangle a pane fills, as the WebGpu renderer fills its
    /// background (`paint_pane`): its cells and half a cell into each split
    /// gap; a pane on an edge of the tab also fills the window to that edge.
    /// Pane borders are drawn inside it.
    fn background_rect(&self, left: usize, top: usize, width: usize, height: usize) -> RectF {
        let (x, width_delta) = if left == 0 {
            (0.0, self.left + self.cell_width / 2.0)
        } else {
            (
                self.left - self.cell_width / 2.0 + left as f32 * self.cell_width,
                self.cell_width,
            )
        };
        let (y, height_delta) = if top == 0 {
            (
                self.top - self.padding_top,
                self.padding_top + self.cell_height / 2.0,
            )
        } else {
            (
                self.top + top as f32 * self.cell_height - self.cell_height / 2.0,
                self.cell_height,
            )
        };
        let width = if left + width >= self.cols {
            self.pixel_width - x
        } else {
            width as f32 * self.cell_width + width_delta
        };
        let height = if top + height >= self.rows {
            self.pixel_height - y
        } else {
            height as f32 * self.cell_height + height_delta
        };
        euclid::rect(x, y, width, height)
    }
}

/// A WebGpu chrome quad as a Metal fill: the pixels it covers, in its color.
fn fill(rect: RectF, color: ClearColor) -> SolidRect {
    SolidRect {
        rect: pixel_rect(rect.min_x(), rect.min_y(), rect.width(), rect.height()),
        color,
    }
}

/// A color the WebGpu renderer composites in linear light, for Metal, which
/// takes sRGB-encoded colors.
fn linear_color(color: LinearRgba) -> ClearColor {
    let SrgbaTuple(red, green, blue, alpha) = color.to_srgb();
    ClearColor::from_srgba(red, green, blue, alpha)
}

fn srgb_color(color: SrgbaTuple) -> ClearColor {
    let SrgbaTuple(red, green, blue, alpha) = color;
    ClearColor::from_srgba(red, green, blue, alpha)
}

/// The pane background the WebGpu renderer draws while the visual bell
/// flashes it (`paint_pane`): `background` (linear, scaled by the window
/// opacity) blended toward `flash` by `intensity`, or `flash` at alpha
/// `intensity` over it in a transparent window.
fn bell_background(
    background: LinearRgba,
    flash: LinearRgba,
    intensity: f32,
    window_is_transparent: bool,
) -> LinearRgba {
    let LinearRgba(red, green, blue, _) = flash;
    let (r1, g1, b1, a1) = background.tuple();
    if window_is_transparent {
        let alpha = intensity + a1 * (1.0 - intensity);
        if alpha <= 0.0 {
            return LinearRgba::with_components(0.0, 0.0, 0.0, 0.0);
        }
        let over =
            |top: f32, bottom: f32| (top * intensity + bottom * a1 * (1.0 - intensity)) / alpha;
        LinearRgba::with_components(over(red, r1), over(green, g1), over(blue, b1), alpha)
    } else {
        LinearRgba::with_components(
            r1 + (red - r1) * intensity,
            g1 + (green - g1) * intensity,
            b1 + (blue - b1) * intensity,
            a1,
        )
    }
}

impl TermWindow {
    /// The window's repaint when a fallback font a Metal frame's shaping
    /// asked for resolves (ft-yccm0.4.7.3): what `fallback_font_completion`
    /// does for the WebGpu renderer's shaping, callable from every
    /// completion, on whichever thread the font resolves.
    pub(crate) fn metal_fallback_ready(&self) -> FallbackReady {
        let window = Mutex::new(self.window.clone());
        let pending = Arc::clone(&self.fallback_invalidation_pending);
        Arc::new(move || {
            let window = window.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(window) = window.as_ref() {
                if let Some(ticket) = PendingFallbackInvalidation::acquire(&pending) {
                    window.notify(TermWindowNotif::FallbackFontsReady(ticket));
                }
            }
        })
    }

    /// Releases the fallback invalidation a resolved font queued, before a
    /// Metal frame reads fonts, as `paint_impl` releases it for the other
    /// front ends (ft-yccm0.4.7.3). A later resolver completion can then
    /// queue another repaint, and the snapshot's readiness check stops
    /// seeing a font pending. Without it the flag stays set after the first
    /// resolution: snapshots defer forever and later resolutions never
    /// repaint.
    pub(crate) fn begin_metal_font_read(&mut self) {
        if let Some(ticket) = self.fallback_invalidation_for_paint.take() {
            ticket.begin_font_read();
        }
    }

    /// Schedules a Metal window's next paint for what its frames animate
    /// (ft-yccm0.4.7.3), as `paint_impl` schedules it for the other front
    /// ends: the visual bell's fade, which `get_intensity_if_bell_target_ringing`
    /// records in `has_animation` while the frame request is built, and
    /// `also` (blinking text and cursors drawn on the main thread). Taking
    /// `has_animation` leaves the next request to record its own.
    pub(crate) fn schedule_metal_animation(&mut self, also: Option<Instant>) {
        let animation = self.has_animation.borrow_mut().take();
        if let Some(due) = animation.into_iter().chain(also).min() {
            self.schedule_animation_wake(due);
        }
    }

    /// The visible panes and chrome fills of this window's Metal frame. It
    /// also rebuilds the window's hit-test items (`ui_items`) for what the
    /// frame draws, as `paint_pass` does for the other front ends: without
    /// them, Metal windows could not drag splits or the scrollbar
    /// (ft-yccm0.4.7.1).
    ///
    /// The fills follow the WebGpu renderer's layers: the window border
    /// (layer 1), then each pane's agent and floating-pane borders and the
    /// active pane's scrollbar thumb, then the splits (layer 2). The renderer
    /// draws them all over the panes' text.
    // Pixel sizes and border widths are small.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn metal_window_panes(&mut self) -> (Vec<MetalPaneRequest>, Vec<SolidRect>) {
        let (padding_left, padding_top) = self.padding_left_top();
        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height().unwrap_or(0.0)
        } else {
            0.0
        };
        let (top_bar_height, bottom_bar_height) = if self.config.tab_bar_at_bottom {
            (0.0, tab_bar_height)
        } else {
            (tab_bar_height, 0.0)
        };
        let border = self.get_os_border();
        let layout = PaneLayout {
            cell_width: self.render_metrics.cell_size.width as f32,
            cell_height: self.render_metrics.cell_size.height as f32,
            left: padding_left + border.left.get() as f32,
            top: top_bar_height + padding_top + border.top.get() as f32,
            padding_top,
            cols: self.terminal_size.cols,
            rows: self.terminal_size.rows,
            pixel_width: self.dimensions.pixel_width as f32,
            pixel_height: self.dimensions.pixel_height as f32,
        };
        let focused = self.focused.is_some();
        let opacity = self.config.window_background_opacity;
        let window_is_transparent = !self.window_background.is_empty() || opacity != 1.0;
        let dim = self.config.inactive_pane_hsb;
        let dim = [dim.hue, dim.saturation, dim.brightness];
        let dim = dim
            .iter()
            .any(|factor| (factor - 1.0).abs() > f32::EPSILON)
            .then_some(dim);

        let mut ui_items = Vec::new();
        let mut fills: Vec<SolidRect> = self
            .window_border_rects()
            .into_iter()
            .map(|(rect, color)| fill(rect, linear_color(color)))
            .collect();
        let mut panes = Vec::new();
        for pos in self.get_panes_to_render() {
            let pane_id = pos.pane.pane_id();
            let facts = pos.pane.render_facts();
            let palette = &facts.palette;
            let background_rect = layout.background_rect(pos.left, pos.top, pos.width, pos.height);
            let flash = self
                .get_intensity_if_bell_target_ringing(
                    &pos.pane,
                    &self.config,
                    VisualBellTarget::BackgroundColor,
                )
                .map(|intensity| {
                    let target = self
                        .config
                        .resolved_palette
                        .visual_bell
                        .as_deref()
                        .unwrap_or(&palette.foreground)
                        .to_linear();
                    let background = palette.background.to_linear().mul_alpha(opacity);
                    linear_color(bell_background(
                        background,
                        target,
                        intensity,
                        window_is_transparent,
                    ))
                });
            let background = palette.background;
            if self.config.agent_detection_enabled {
                let agent = self
                    .agent_pane_states
                    .get(&pane_id)
                    .and_then(|state| state.border_color_rgba());
                if let Some((red, green, blue, alpha)) = agent {
                    let color = LinearRgba::with_components(
                        f32::from(red) / 255.0,
                        f32::from(green) / 255.0,
                        f32::from(blue) / 255.0,
                        f32::from(alpha) / 255.0,
                    );
                    let width = self.config.agent_border_width.max(1) as f32;
                    for edge in pane_border_rects(background_rect, width)
                        .into_iter()
                        .flatten()
                    {
                        fills.push(fill(edge, linear_color(color)));
                    }
                }
            }
            if let Some(width) = self.focused_floating_pane_border_width(pane_id) {
                for edge in pane_border_rects(background_rect, width)
                    .into_iter()
                    .flatten()
                {
                    fills.push(fill(edge, srgb_color(palette.cursor_border)));
                }
            }
            if pos.is_active && self.show_scroll_bar {
                let thumb = self.scroll_thumb_geometry(
                    &facts.dimensions,
                    self.get_viewport(pane_id),
                    top_bar_height,
                    bottom_bar_height,
                );
                ui_items.extend(thumb.ui_items());
                fills.push(fill(thumb.rect(), srgb_color(palette.scrollbar_thumb)));
            }
            panes.push(MetalPaneRequest {
                inputs: MetalFrameInputs {
                    viewport_top: self.get_viewport(pane_id),
                    selection: self.selection(pane_id).and_then(|selection| {
                        selection
                            .range
                            .map(|range| (range.normalize(), selection.rectangular))
                    }),
                    config: self.config.clone(),
                    focused: focused && pos.is_active,
                    hover: self.current_highlight.clone(),
                    grid_origin: layout.grid_origin(pos.left, pos.top),
                    compose: (pos.is_active
                        && (self.dead_key_status != DeadKeyStatus::None
                            || self.leader_is_active()))
                    .then(|| MetalCompose {
                        text: match &self.dead_key_status {
                            DeadKeyStatus::Composing(text) => Some(text.clone()),
                            _ => None,
                        },
                    }),
                    bell_cursor: self.get_intensity_if_bell_target_ringing(
                        &pos.pane,
                        &self.config,
                        VisualBellTarget::CursorColor,
                    ),
                    fallback_ready: Some(self.metal_fallback_ready()),
                    pane: Some(pos.pane),
                },
                rect: layout.pane_rect(pos.left, pos.top, pos.width, pos.height),
                clear: flash.unwrap_or_else(|| {
                    ClearColor::from_srgba(
                        background.0,
                        background.1,
                        background.2,
                        background.3 * opacity,
                    )
                }),
                hsb: if pos.is_active { None } else { dim },
            });
        }

        let splits = self.get_splits();
        let split_color = self
            .get_active_pane_or_overlay()
            .filter(|_| !splits.is_empty())
            .map(|pane| srgb_color(pane.render_facts().palette.split));
        if let Some(color) = split_color {
            for split in &splits {
                // As paint_split draws it, over the split's cells.
                let geometry = split_render_geometry(
                    split,
                    layout.cell_width,
                    layout.cell_height,
                    self.render_metrics.underline_height as f32,
                    top_bar_height + border.top.get() as f32,
                    padding_left,
                    padding_top,
                    border.left.get(),
                );
                fills.push(fill(geometry.rect, color));
                ui_items.push(UIItem {
                    x: geometry.ui_x,
                    width: geometry.ui_width,
                    y: geometry.ui_y,
                    height: geometry.ui_height,
                    item_type: UIItemType::Split(split.clone()),
                });
            }
        }
        self.ui_items = ui_items;
        (panes, fills)
    }
}

/// The Metal frames of a window's visible panes, one per pane.
#[derive(Default)]
pub(crate) struct MetalPanes {
    frames: HashMap<PaneId, MetalFrame>,
    /// How many rows each pane's scene rebuilt in the last draw.
    rebuilt: Vec<(PaneId, usize)>,
    /// The window's blink clocks (ft-yccm0.4.7.3), made on the first draw.
    blink: Option<MetalBlinkClocks>,
    /// When the last draw's blinking text or cursor next needs a frame.
    redraw_at: Option<Instant>,
}

impl MetalPanes {
    /// How many rows each pane drawn last rebuilt.
    pub(crate) fn rows_rebuilt(&self) -> &[(PaneId, usize)] {
        &self.rebuilt
    }

    /// When the last draw's blinking text or blinking cursor next needs a
    /// frame, as the WebGpu renderer schedules its next frame for them.
    pub(crate) fn redraw_at(&self) -> Option<Instant> {
        self.redraw_at
    }

    /// Captures each of `panes`' changed rows and brings its scene up to
    /// date, forgets the frames of panes no longer drawn, then calls `draw`
    /// with the window frame: `None` when no pane has a scene.
    // One parameter per frame input; a struct would only rename them.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw<R>(
        &mut self,
        panes: &[MetalPaneRequest],
        fills: &[SolidRect],
        ui: Option<UiLayer<'_>>,
        clear: ClearColor,
        fonts: &Rc<FontConfiguration>,
        metrics: &RenderMetrics,
        renderer: &Rc<MetalRenderer>,
        draw: impl FnOnce(Option<&WindowFrame<'_>>) -> R,
    ) -> R {
        let mut previous = std::mem::take(&mut self.frames);
        let mut updated = Vec::with_capacity(panes.len());
        self.rebuilt.clear();
        self.redraw_at = None;
        // One blink phase for the whole window, as WebGpu's window-wide
        // blink states give all its panes.
        let phase = panes.first().map_or_else(MetalBlinkPhase::default, |pane| {
            self.blink
                .get_or_insert_with(|| MetalBlinkClocks::new(&pane.inputs.config))
                .text_phase(&pane.inputs.config)
        });
        let mut blinking = (false, false);
        let mut cursor_due = None;
        for pane in panes {
            let Some(pane_id) = pane.inputs.pane.as_ref().map(|pane| pane.pane_id()) else {
                continue;
            };
            let mut frame = previous.remove(&pane_id);
            let clocks = self
                .blink
                .get_or_insert_with(|| MetalBlinkClocks::new(&pane.inputs.config));
            let uniforms = MetalFrame::update(
                &mut frame,
                &pane.inputs,
                fonts,
                metrics,
                renderer,
                clocks,
                &phase,
            );
            if let Some((frame, uniforms)) = frame.zip(uniforms) {
                self.rebuilt.push((pane_id, uniforms.rows_rebuilt));
                blinking = (
                    blinking.0 || uniforms.blinking.0,
                    blinking.1 || uniforms.blinking.1,
                );
                cursor_due = cursor_due.into_iter().chain(uniforms.cursor_due).min();
                updated.push((pane_id, pane, frame, uniforms));
            }
        }
        self.redraw_at = phase.due(blinking).into_iter().chain(cursor_due).min();
        let scenes: Vec<PaneScene<'_>> = updated
            .iter()
            .map(|(pane_id, pane, frame, uniforms)| PaneScene {
                key: *pane_id as u64,
                rect: pane.rect,
                cells: frame.scene().cells(),
                background: uniforms.background,
                text: Some((frame.scene().text(), uniforms.text)),
                clear: pane.clear,
                hsb: pane.hsb,
            })
            .collect();
        let drawn = if scenes.is_empty() {
            draw(None)
        } else {
            draw(Some(&WindowFrame {
                clear,
                panes: &scenes,
                fills,
                ui,
            }))
        };
        drop(scenes);
        self.frames = updated
            .into_iter()
            .map(|(pane_id, _, frame, _)| (pane_id, frame))
            .collect();
        drawn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8x16 cells, 4 px of left padding, 6 px of top padding, no tab bar or
    /// border, in a window `cols` x `rows` cells big plus padding.
    fn layout(cols: usize, rows: usize) -> PaneLayout {
        PaneLayout {
            cell_width: 8.0,
            cell_height: 16.0,
            left: 4.0,
            top: 6.0,
            padding_top: 6.0,
            cols,
            rows,
            pixel_width: 4.0 + cols as f32 * 8.0 + 4.0,
            pixel_height: 6.0 + rows as f32 * 16.0 + 6.0,
        }
    }

    #[test]
    fn a_sole_pane_fills_the_window_and_its_grid_starts_past_the_padding() {
        let layout = layout(80, 24);
        assert_eq!(
            layout.pane_rect(0, 0, 80, 24),
            PixelRect::new(0, 0, 648, 396)
        );
        assert_eq!(layout.grid_origin(0, 0), [4.0, 6.0]);
    }

    /// Side by side, with the split in column 40: the panes meet in the
    /// middle of the split's cell, with no gap and no overlap.
    #[test]
    fn panes_beside_a_split_meet_in_the_middle_of_its_cell() {
        let wide = layout(81, 24);
        let left = wide.pane_rect(0, 0, 40, 24);
        let right = wide.pane_rect(41, 0, 40, 24);
        // The split's cell spans 324..332; its middle is 328.
        assert_eq!(left, PixelRect::new(0, 0, 328, 396));
        assert_eq!(right, PixelRect::new(328, 0, 656 - 328, 396));
        assert_eq!(wide.grid_origin(41, 0), [332.0, 6.0]);
        // Planted negative: a pane one column off overlaps its neighbor.
        let shifted = wide.pane_rect(40, 0, 41, 24);
        assert!(shifted.x < left.x + left.width);

        // Stacked, with the split in row 12: its cell spans 198..214.
        let tall = layout(80, 25);
        let top = tall.pane_rect(0, 0, 80, 12);
        let bottom = tall.pane_rect(0, 13, 80, 12);
        assert_eq!(top, PixelRect::new(0, 0, 648, 206));
        assert_eq!(bottom, PixelRect::new(0, 206, 648, 412 - 206));
        assert_eq!(tall.grid_origin(0, 13), [4.0, 214.0]);
    }

    #[test]
    fn rectangles_cover_the_pixels_whose_centers_they_contain() {
        assert_eq!(pixel_rect(0.0, 0.0, 10.0, 4.0), PixelRect::new(0, 0, 10, 4));
        // [2.5, 4.5) holds the centers 2.5, 3.5: pixels 2 and 3.
        assert_eq!(pixel_rect(2.5, 0.0, 2.0, 1.0), PixelRect::new(2, 0, 2, 1));
        // [2.6, 4.6) holds 3.5 and 4.5: pixels 3 and 4.
        assert_eq!(pixel_rect(2.6, 0.0, 2.0, 1.0), PixelRect::new(3, 0, 2, 1));
        // Off the window's top-left edge, clamped.
        assert_eq!(pixel_rect(-3.0, -3.0, 5.0, 5.0), PixelRect::new(0, 0, 2, 2));
        assert_eq!(pixel_rect(5.0, 5.0, 0.0, 0.0).width, 0);
    }
}
