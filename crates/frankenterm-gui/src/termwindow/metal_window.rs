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
//! Split borders are [`SolidRect`]s in the active pane's split color, where
//! the WebGpu renderer draws them.
//!
//! Wherever the window is drawn, [`MetalPanes`] keeps one `MetalFrame`
//! (render mirror, scene and glyphs) per visible pane. A pane whose rows did
//! not change rebuilds no row, and the renderer uploads none of its rows.

use super::TermWindow;
use super::metal_cells::{MetalFrame, MetalFrameInputs};
use super::render::split::split_render_geometry;
use crate::utilsprites::RenderMetrics;
use frankenterm_font::FontConfiguration;
use frankenterm_renderer_metal::{
    ClearColor, MetalRenderer, PaneScene, PixelRect, SolidRect, WindowFrame,
};
use mux::pane::PaneId;
use std::collections::HashMap;
use std::rc::Rc;

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

    /// The rectangle a pane fills, as the WebGpu renderer fills its
    /// background (`paint_pane`): its cells and half a cell into each split
    /// gap; a pane on an edge of the tab also fills the window to that edge.
    fn pane_rect(&self, left: usize, top: usize, width: usize, height: usize) -> PixelRect {
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
        pixel_rect(x, y, width, height)
    }
}

impl TermWindow {
    /// The visible panes and split borders of this window's Metal frame.
    // Pixel sizes and border widths are small.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn metal_window_panes(&mut self) -> (Vec<MetalPaneRequest>, Vec<SolidRect>) {
        let (padding_left, padding_top) = self.padding_left_top();
        let tab_bar_height = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().unwrap_or(0.0)
        } else {
            0.0
        };
        let border = self.get_os_border();
        let layout = PaneLayout {
            cell_width: self.render_metrics.cell_size.width as f32,
            cell_height: self.render_metrics.cell_size.height as f32,
            left: padding_left + border.left.get() as f32,
            top: tab_bar_height + padding_top + border.top.get() as f32,
            padding_top,
            cols: self.terminal_size.cols,
            rows: self.terminal_size.rows,
            pixel_width: self.dimensions.pixel_width as f32,
            pixel_height: self.dimensions.pixel_height as f32,
        };
        let focused = self.focused.is_some();
        let opacity = self.config.window_background_opacity;
        let dim = self.config.inactive_pane_hsb;
        let dim = [dim.hue, dim.saturation, dim.brightness];
        let dim = dim
            .iter()
            .any(|factor| (factor - 1.0).abs() > f32::EPSILON)
            .then_some(dim);
        let panes = self
            .get_panes_to_render()
            .into_iter()
            .map(|pos| {
                let pane_id = pos.pane.pane_id();
                let background = pos.pane.render_facts().palette.background;
                MetalPaneRequest {
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
                        pane: Some(pos.pane),
                    },
                    rect: layout.pane_rect(pos.left, pos.top, pos.width, pos.height),
                    clear: ClearColor::from_srgba(
                        background.0,
                        background.1,
                        background.2,
                        background.3 * opacity,
                    ),
                    hsb: if pos.is_active { None } else { dim },
                }
            })
            .collect();
        let splits = self.get_splits();
        let split_color = self
            .get_active_pane_or_overlay()
            .filter(|_| !splits.is_empty())
            .map(|pane| pane.render_facts().palette.split);
        let fills = split_color.map_or_else(Vec::new, |color| {
            let color = ClearColor::from_srgba(color.0, color.1, color.2, color.3);
            splits
                .iter()
                .map(|split| {
                    // As paint_split draws it, over the split's cells.
                    let rect = split_render_geometry(
                        split,
                        layout.cell_width,
                        layout.cell_height,
                        self.render_metrics.underline_height as f32,
                        tab_bar_height + border.top.get() as f32,
                        padding_left,
                        padding_top,
                        border.left.get(),
                    )
                    .rect;
                    SolidRect {
                        rect: pixel_rect(rect.min_x(), rect.min_y(), rect.width(), rect.height()),
                        color,
                    }
                })
                .collect()
        });
        (panes, fills)
    }
}

/// The Metal frames of a window's visible panes, one per pane.
#[derive(Default)]
pub(crate) struct MetalPanes {
    frames: HashMap<PaneId, MetalFrame>,
    /// How many rows each pane's scene rebuilt in the last draw.
    rebuilt: Vec<(PaneId, usize)>,
}

impl MetalPanes {
    /// How many rows each pane drawn last rebuilt.
    pub(crate) fn rows_rebuilt(&self) -> &[(PaneId, usize)] {
        &self.rebuilt
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
        clear: ClearColor,
        fonts: &Rc<FontConfiguration>,
        metrics: &RenderMetrics,
        renderer: &Rc<MetalRenderer>,
        draw: impl FnOnce(Option<&WindowFrame<'_>>) -> R,
    ) -> R {
        let mut previous = std::mem::take(&mut self.frames);
        let mut updated = Vec::with_capacity(panes.len());
        self.rebuilt.clear();
        for pane in panes {
            let Some(pane_id) = pane.inputs.pane.as_ref().map(|pane| pane.pane_id()) else {
                continue;
            };
            let mut frame = previous.remove(&pane_id);
            let uniforms = MetalFrame::update(&mut frame, &pane.inputs, fonts, metrics, renderer);
            if let Some((frame, uniforms)) = frame.zip(uniforms) {
                self.rebuilt.push((pane_id, uniforms.rows_rebuilt));
                updated.push((pane_id, pane, frame, uniforms));
            }
        }
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
