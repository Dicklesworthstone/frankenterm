//! The live Metal frame (ft-yccm0.4.4).
//!
//! Each Metal paint captures every visible pane's changed rows into its
//! render mirror under one short hold of its terminal lock
//! (`LocalPane::capture_render_rows`; the generic pane path for mux clients
//! and cold scrollback). It then rebuilds the frame's cell backgrounds and
//! glyph instances outside the lock, only for the rows that need it
//! ([`frankenterm_gui::metal_scene`]). Colors and the cursor are resolved the
//! way the WebGpu renderer resolves them. The palette comes from the pane's
//! published render facts, so it is read without the lock. The image-parity
//! snapshot (ft-yccm0.1.10) draws the same scene offscreen.
//!
//! What a frame needs from its window is captured on the main thread as
//! [`MetalFrameInputs`], one per visible pane (`metal_window`, ft-yccm0.4.6),
//! so the frame itself can be built on the window's render thread
//! (ft-yccm0.4.1.2) with that thread's own fonts.

use crate::selection::SelectionRange;
use crate::termwindow::metal_glyphs::{FontGlyphs, MAX_SHAPE_PASSES};
use crate::utilsprites::RenderMetrics;
use config::ConfigHandle;
use frankenterm_font::FontConfiguration;
use frankenterm_gui::metal_scene::{
    BlinkLevels, CursorSprite, MetalScene, SceneStyle, cursor_attr_colors,
    reverse_video_cursor_applies,
};
use frankenterm_renderer_metal::{
    BackgroundUniforms, CursorShape as MetalCursorShape, CursorUniform, MetalRenderer, TextUniforms,
};
use mux::localpane::LocalPane;
use mux::pane::{Pane, PaneId};
use mux::render_mirror::{CaptureRequest, RenderMirror, capture_pane_rows};
use std::rc::Rc;
use std::sync::Arc;
use termwiz::hyperlink::Hyperlink;
use termwiz::surface::{CursorShape, CursorVisibility};
use wezterm_term::color::{ColorPalette, SrgbaTuple};
use wezterm_term::{StableRowIndex, TerminalConfiguration};

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

/// The sprite the scene draws for a cursor (ft-yccm0.4.7.3): the WebGpu
/// renderer's own cursor sprites, anti-aliased, for every shape but a
/// focused block, which the background pass fills.
fn cursor_sprite(shape: MetalCursorShape) -> Option<CursorSprite> {
    match shape {
        MetalCursorShape::HollowBlock => Some(CursorSprite::HollowBlock),
        MetalCursorShape::Bar => Some(CursorSprite::Bar),
        MetalCursorShape::Underline => Some(CursorSprite::Underline),
        MetalCursorShape::Block | MetalCursorShape::Hidden => None,
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
    /// How many of the scene's rows this update rebuilt (ft-yccm0.4.6).
    pub(crate) rows_rebuilt: usize,
}

/// What a Metal frame needs from its window, captured on the main thread
/// (ft-yccm0.4.1.2). It is all `Send`, so a render thread can build the frame
/// from it.
#[derive(Clone)]
pub(crate) struct MetalFrameInputs {
    /// The pane, or its overlay; `None` draws only the clear color.
    pub(crate) pane: Option<Arc<dyn Pane>>,
    pub(crate) viewport_top: Option<StableRowIndex>,
    pub(crate) config: ConfigHandle,
    /// The window is focused and the pane is its active pane.
    pub(crate) focused: bool,
    /// The pane's selection, normalized, and whether it is rectangular.
    pub(crate) selection: Option<(SelectionRange, bool)>,
    /// The hyperlink under the mouse.
    pub(crate) hover: Option<Arc<Hyperlink>>,
    /// Where the grid's first cell starts, in pixels: padding, tab bar and
    /// window border.
    pub(crate) grid_origin: [f32; 2],
}

impl MetalFrame {
    /// Captures `inputs.pane`'s changed rows into the render mirror of the
    /// frame in `slot` and brings its scene up to date; `slot` then holds the
    /// frame. `None` without a pane. Runs on whichever thread draws the
    /// window: the main thread, or the window's render thread with that
    /// thread's own `fonts` and `metrics` (ft-yccm0.4.1.2).
    // Pixel sizes and cell coordinates are small and non-negative.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub(crate) fn update(
        slot: &mut Option<MetalFrame>,
        inputs: &MetalFrameInputs,
        fonts: &Rc<FontConfiguration>,
        metrics: &RenderMetrics,
        metal: &Rc<MetalRenderer>,
    ) -> Option<MetalFrameUniforms> {
        let pane = inputs.pane.as_ref()?;
        let pane_id = pane.pane_id();
        let config = &inputs.config;
        let mut frame = match slot.take() {
            Some(frame)
                if frame.pane_id == pane_id
                    && frame
                        .glyphs
                        .serves(fonts, metal, metrics, config.generation()) =>
            {
                frame
            }
            _ => MetalFrame {
                pane_id,
                mirror: RenderMirror::new(),
                scene: MetalScene::new(),
                glyphs: FontGlyphs::new(
                    Rc::clone(fonts),
                    config.clone(),
                    Rc::clone(metal),
                    metrics,
                ),
            },
        };

        let request = CaptureRequest {
            viewport_top: inputs.viewport_top,
            rules: &config.hyperlink_rules,
            rules_generation: config.generation(),
        };
        let captured = pane
            .downcast_ref::<LocalPane>()
            .and_then(|local| local.capture_render_rows(&mut frame.mirror, &request))
            .unwrap_or_else(|| capture_pane_rows(&**pane, &mut frame.mirror, &request));
        metrics::histogram!("gui.metal.capture.rows_copied").record(captured.rows_captured as f64);

        if frame.glyphs.begin_frame() {
            frame.scene.invalidate();
        }
        let facts = pane.render_facts();
        let palette: &ColorPalette = &facts.palette;
        let focused_and_active = inputs.focused;
        let cursor = frame.mirror.cursor();
        let shape = config.default_cursor_style.effective_shape(cursor.shape);
        let block_cursor = focused_and_active
            && matches!(
                shape,
                CursorShape::Default | CursorShape::BlinkingBlock | CursorShape::SteadyBlock
            );
        let metal_shape = cursor_shape(shape, focused_and_active);
        let bold_brightens = config.bold_brightens_ansi_colors != config::BoldBrightening::No;
        // WebGpu's reverse-video cursor (force_reverse_video_cursor), for a
        // pane that keeps the window's cursor colors.
        let reverse_video_cursor = config
            .force_reverse_video_cursor
            .then(|| {
                let window = config::TermConfig::new().color_palette();
                palette.cursor_fg == window.cursor_fg && palette.cursor_bg == window.cursor_bg
            })
            .unwrap_or(false)
            .then_some(config.reverse_video_cursor_min_contrast);
        // A focused cursor is in the cursor color, or in its cell's
        // foreground where the reverse-video cursor applies to the cell.
        let cursor_color = if focused_and_active {
            let row = usize::try_from(cursor.y - frame.mirror.first())
                .ok()
                .and_then(|row| frame.mirror.rows().get(row));
            reverse_video_cursor
                .and_then(|min_contrast| {
                    let (fg, bg) = cursor_attr_colors(row, cursor.x, palette, bold_brightens);
                    reverse_video_cursor_applies(fg, bg, min_contrast).then_some(fg)
                })
                .unwrap_or(palette.cursor_bg)
        } else {
            palette.cursor_border
        };
        let selection = &inputs.selection;
        let selected = |stable| {
            selection.as_ref().map_or(0..0, |(range, rectangular)| {
                range.cols_for_row(stable, *rectangular)
            })
        };
        let style = SceneStyle {
            palette,
            generation: ((config.generation() as u64) << 32)
                | (facts.palette_generation & 0xffff_ffff),
            bold_brightens,
            selection_fg: visible(palette.selection_fg),
            selection_bg: palette.selection_bg,
            cursor_fg: if block_cursor {
                visible(palette.cursor_fg)
            } else {
                None
            },
            cursor_bg: block_cursor.then_some(palette.cursor_bg),
            cursor_sprite: cursor_sprite(metal_shape).map(|sprite| (sprite, cursor_color)),
            hover: inputs.hover.as_ref(),
            // The window's blink levels and compose state are not passed to
            // the frame yet, so blinking text draws as plain text and no
            // compose cursor is drawn (ft-yccm0.4.7.3).
            blink: BlinkLevels::default(),
            compose: None,
            min_contrast: config.text_min_contrast_ratio,
            reverse_video_cursor,
        };
        let mut update = frame
            .scene
            .update(&frame.mirror, &style, &selected, &mut frame.glyphs);
        // ft-yccm0.4.7.3: a fallback font installed while shaping leaves the
        // clusters shaped before it stale. Rebuild the scene against the grown
        // chain, as the WebGpu renderer re-runs its paint pass.
        let mut passes = 1;
        while frame.glyphs.take_chain_changed() && passes < MAX_SHAPE_PASSES {
            frame.scene.invalidate();
            update = frame
                .scene
                .update(&frame.mirror, &style, &selected, &mut frame.glyphs);
            passes += 1;
        }
        metrics::histogram!("gui.metal.scene.rows_rebuilt").record(update.rows_rebuilt as f64);

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
                    // A cursor the scene draws as a sprite is not drawn here.
                    shape: if cursor_sprite(metal_shape).is_some() {
                        MetalCursorShape::Hidden
                    } else {
                        metal_shape
                    },
                    col: cursor.x as u32,
                    row: row as u32,
                    width_cells,
                    thickness: metrics.underline_height.max(1) as f32,
                    color: premultiplied(cursor_color),
                }
            }
            _ => CursorUniform::default(),
        };
        let uniforms = MetalFrameUniforms {
            background: BackgroundUniforms {
                cell_size: [
                    metrics.cell_size.width as f32,
                    metrics.cell_size.height as f32,
                ],
                grid_origin: inputs.grid_origin,
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
            rows_rebuilt: update.rows_rebuilt,
        };
        *slot = Some(frame);
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

    /// ft-yccm0.4.7.3: only a focused block is left to the background pass.
    #[test]
    fn every_cursor_but_a_focused_block_is_a_sprite() {
        assert_eq!(cursor_sprite(MetalCursorShape::Block), None);
        assert_eq!(cursor_sprite(MetalCursorShape::Hidden), None);
        for (shape, focused, sprite) in [
            (CursorShape::SteadyBlock, false, CursorSprite::HollowBlock),
            (CursorShape::SteadyBar, false, CursorSprite::HollowBlock),
            (CursorShape::SteadyBar, true, CursorSprite::Bar),
            (
                CursorShape::BlinkingUnderline,
                true,
                CursorSprite::Underline,
            ),
        ] {
            assert_eq!(
                cursor_sprite(cursor_shape(shape, focused)),
                Some(sprite),
                "{shape:?} focused {focused}"
            );
        }
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
