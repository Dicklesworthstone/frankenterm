//! The live Metal frame (ft-yccm0.4.4).
//!
//! Each Metal paint captures the active pane's changed rows into its render
//! mirror under one short hold of the terminal lock
//! (`LocalPane::capture_render_rows`; the generic pane path for mux clients
//! and cold scrollback). It then rebuilds the frame's cell backgrounds and
//! glyph instances outside the lock, only for the rows that need it
//! ([`frankenterm_gui::metal_scene`]). Colors and the cursor are resolved the
//! way the WebGpu renderer resolves them. The palette comes from the pane's
//! published render facts, so it is read without the lock. The image-parity
//! snapshot (ft-yccm0.1.10) draws the same scene offscreen.

use crate::termwindow::TermWindow;
use crate::termwindow::metal_glyphs::FontGlyphs;
use frankenterm_gui::metal_scene::{MetalScene, SceneStyle};
use frankenterm_renderer_metal::{
    BackgroundUniforms, CursorShape as MetalCursorShape, CursorUniform, MetalRenderer, TextUniforms,
};
use mux::localpane::LocalPane;
use mux::pane::PaneId;
use mux::render_mirror::{CaptureRequest, RenderMirror, capture_pane_rows};
use std::rc::Rc;
use termwiz::surface::{CursorShape, CursorVisibility};
use wezterm_term::color::{ColorPalette, SrgbaTuple};

/// Straight-alpha sRGB to the premultiplied RGBA the shader composites.
fn premultiplied(color: SrgbaTuple) -> [f32; 4] {
    let SrgbaTuple(red, green, blue, alpha) = color;
    [red * alpha, green * alpha, blue * alpha, alpha]
}

/// The metal cursor shape for a terminal cursor shape: an unfocused window
/// (or inactive pane) draws a hollow block, as the WebGpu renderer does.
fn cursor_shape(shape: CursorShape, focused_and_active: bool) -> MetalCursorShape {
    if !focused_and_active {
        return MetalCursorShape::HollowBlock;
    }
    match shape {
        CursorShape::BlinkingUnderline | CursorShape::SteadyUnderline => {
            MetalCursorShape::Underline
        }
        CursorShape::BlinkingBar | CursorShape::SteadyBar => MetalCursorShape::Bar,
        CursorShape::Default | CursorShape::BlinkingBlock | CursorShape::SteadyBlock => {
            MetalCursorShape::Block
        }
    }
}

/// A color the palette leaves fully transparent keeps the cell's own.
fn visible(color: SrgbaTuple) -> Option<SrgbaTuple> {
    (color.3 > 0.0).then_some(color)
}

/// One pane's live Metal frame state.
pub(crate) struct MetalFrame {
    pane_id: PaneId,
    mirror: RenderMirror,
    scene: MetalScene,
    glyphs: FontGlyphs,
}

impl MetalFrame {
    pub(crate) fn scene(&self) -> &MetalScene {
        &self.scene
    }
}

/// The uniforms of one Metal frame.
pub(crate) struct MetalFrameUniforms {
    pub(crate) background: BackgroundUniforms,
    pub(crate) text: TextUniforms,
}

impl TermWindow {
    /// Captures the active pane's changed rows and brings its Metal scene up
    /// to date; `self.metal_frame` then holds the scene. `None` without a
    /// pane.
    // Pixel sizes and cell coordinates are small and non-negative.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub(crate) fn update_metal_frame(
        &mut self,
        metal: &Rc<MetalRenderer>,
    ) -> Option<MetalFrameUniforms> {
        let pane = self.get_active_pane_or_overlay()?;
        let pane_id = pane.pane_id();
        let mut frame = match self.metal_frame.take() {
            Some(frame)
                if frame.pane_id == pane_id
                    && frame.glyphs.serves(
                        &self.fonts,
                        metal,
                        &self.render_metrics,
                        self.config.generation(),
                    ) =>
            {
                frame
            }
            _ => MetalFrame {
                pane_id,
                mirror: RenderMirror::new(),
                scene: MetalScene::new(),
                glyphs: FontGlyphs::new(
                    Rc::clone(&self.fonts),
                    self.config.clone(),
                    Rc::clone(metal),
                    &self.render_metrics,
                ),
            },
        };

        let request = CaptureRequest {
            viewport_top: self.get_viewport(pane_id),
            rules: &self.config.hyperlink_rules,
            rules_generation: self.config.generation(),
        };
        let captured = pane
            .downcast_ref::<LocalPane>()
            .and_then(|local| local.capture_render_rows(&mut frame.mirror, &request))
            .unwrap_or_else(|| capture_pane_rows(&*pane, &mut frame.mirror, &request));
        metrics::histogram!("gui.metal.capture.rows_copied").record(captured.rows_captured as f64);

        if frame.glyphs.begin_frame() {
            frame.scene.invalidate();
        }
        let facts = pane.render_facts();
        let palette: &ColorPalette = &facts.palette;
        let focused_and_active = self.focused.is_some();
        let cursor = frame.mirror.cursor();
        let shape = self
            .config
            .default_cursor_style
            .effective_shape(cursor.shape);
        let block_cursor = focused_and_active
            && matches!(
                shape,
                CursorShape::Default | CursorShape::BlinkingBlock | CursorShape::SteadyBlock
            );
        let selection = self.selection(pane_id).and_then(|selection| {
            selection
                .range
                .map(|range| (range.normalize(), selection.rectangular))
        });
        let selected = |stable| {
            selection.as_ref().map_or(0..0, |(range, rectangular)| {
                range.cols_for_row(stable, *rectangular)
            })
        };
        let style = SceneStyle {
            palette,
            generation: ((self.config.generation() as u64) << 32)
                | (facts.palette_generation & 0xffff_ffff),
            bold_brightens: self.config.bold_brightens_ansi_colors != config::BoldBrightening::No,
            selection_fg: visible(palette.selection_fg),
            cursor_fg: if block_cursor {
                visible(palette.cursor_fg)
            } else {
                None
            },
            hover: self.current_highlight.as_ref(),
        };
        let update = frame
            .scene
            .update(&frame.mirror, &style, &selected, &mut frame.glyphs);
        metrics::histogram!("gui.metal.scene.rows_rebuilt").record(update.rows_rebuilt as f64);

        let metrics = &self.render_metrics;
        let rows = frame.mirror.rows();
        let cursor_row = cursor.y - frame.mirror.first();
        let cursor = match usize::try_from(cursor_row) {
            Ok(row) if cursor.visibility == CursorVisibility::Visible && row < rows.len() => {
                let width_cells = rows[row]
                    .cells()
                    .iter()
                    .find(|cell| (cell.col()..cell.col() + cell.width()).contains(&cursor.x))
                    .map_or(1, |cell| cell.width().clamp(1, 2) as u32);
                CursorUniform {
                    shape: cursor_shape(shape, focused_and_active),
                    col: cursor.x as u32,
                    row: row as u32,
                    width_cells,
                    thickness: metrics.underline_height.max(1) as f32,
                    color: premultiplied(if focused_and_active {
                        palette.cursor_bg
                    } else {
                        palette.cursor_border
                    }),
                }
            }
            _ => CursorUniform::default(),
        };
        let (padding_left, padding_top) = self.padding_left_top();
        let tab_bar_height = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().unwrap_or(0.0)
        } else {
            0.0
        };
        let border = self.get_os_border();
        let uniforms = MetalFrameUniforms {
            background: BackgroundUniforms {
                cell_size: [
                    metrics.cell_size.width as f32,
                    metrics.cell_size.height as f32,
                ],
                grid_origin: [
                    padding_left + border.left.get() as f32,
                    tab_bar_height + padding_top + border.top.get() as f32,
                ],
                row_offset: frame.scene.cells().row_offset(),
                cursor,
                selection_tint: premultiplied(palette.selection_bg),
                search_tint: [0.0; 4],
                current_match_tint: [0.0; 4],
            },
            text: TextUniforms {
                underline_position: metrics.descender_row as f32,
                line_thickness: metrics.underline_height.max(1) as f32,
                strikethrough_position: metrics.strike_row as f32,
            },
        };
        self.metal_frame = Some(frame);
        Some(uniforms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfocused_windows_draw_a_hollow_cursor_whatever_the_shape() {
        for shape in [
            CursorShape::Default,
            CursorShape::SteadyBar,
            CursorShape::BlinkingUnderline,
        ] {
            assert_eq!(cursor_shape(shape, false), MetalCursorShape::HollowBlock);
        }
    }

    #[test]
    fn focused_cursor_shapes_map_one_to_one() {
        assert_eq!(
            cursor_shape(CursorShape::Default, true),
            MetalCursorShape::Block
        );
        assert_eq!(
            cursor_shape(CursorShape::SteadyBlock, true),
            MetalCursorShape::Block
        );
        assert_eq!(
            cursor_shape(CursorShape::BlinkingUnderline, true),
            MetalCursorShape::Underline
        );
        assert_eq!(
            cursor_shape(CursorShape::SteadyBar, true),
            MetalCursorShape::Bar
        );
    }

    #[test]
    fn transparent_palette_colors_keep_the_cell_color_and_tints_premultiply() {
        assert_eq!(visible(SrgbaTuple(0.0, 0.0, 0.0, 0.0)), None);
        assert_eq!(
            visible(SrgbaTuple(1.0, 0.0, 0.0, 1.0)),
            Some(SrgbaTuple(1.0, 0.0, 0.0, 1.0))
        );
        assert_eq!(
            premultiplied(SrgbaTuple(1.0, 0.5, 0.25, 0.5)),
            [0.5, 0.25, 0.125, 0.5]
        );
    }
}
