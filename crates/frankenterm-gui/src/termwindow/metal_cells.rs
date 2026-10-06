//! The active pane's cell backgrounds for the Metal background pass
//! (ft-yccm0.4.2.2), resolved the way the WebGpu renderer resolves them:
//! explicit and reverse-video colors, wide characters with their spacer
//! cells, the selection, and the cursor shape, focused or not.
//!
//! The Metal render snapshot (ft-yccm0.1.10) draws these through the real
//! background pass. The full renderer adapter is ft-yccm0.4.4.

use crate::termwindow::TermWindow;
use frankenterm_renderer_metal::{
    BackgroundUniforms, CellBg, CellBgGrid, CursorShape as MetalCursorShape, CursorUniform,
    GridExtent,
};
use termwiz::surface::{CursorShape, CursorVisibility};
use wezterm_term::StableRowIndex;
use wezterm_term::color::{ColorAttribute, SrgbaTuple};

/// An sRGB color component as the 8-bit value the GPU stores.
// Clamped to 0.0..=255.0 before the cast.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn unorm8(component: f32) -> u8 {
    (component.clamp(0.0, 1.0) * 255.0).round() as u8
}

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

impl TermWindow {
    /// The active pane's visible cell backgrounds and the background-pass
    /// uniforms (cell size, grid origin, cursor, selection tint).
    // Pixel sizes and cell coordinates are small and non-negative.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub(crate) fn metal_cell_backgrounds(&mut self) -> Option<(CellBgGrid, BackgroundUniforms)> {
        let pane = self.get_active_pane_or_overlay()?;
        let dims = pane.get_dimensions();
        let rows = dims.viewport_rows;
        let cols = dims.cols;
        let top = dims.physical_top;
        let palette = pane.palette();
        let mut cells = CellBgGrid::new(GridExtent::new(rows, cols));

        let visible = top..top.saturating_add(StableRowIndex::try_from(rows).ok()?);
        let (first, lines) = pane.get_lines(visible);
        for (index, line) in lines.iter().enumerate() {
            let Ok(row) = u32::try_from((first - top) as usize + index) else {
                continue;
            };
            for cell in line.visible_cells() {
                let attrs = cell.attrs();
                let color = if attrs.reverse() {
                    Some(palette.resolve_fg(attrs.foreground()))
                } else {
                    match attrs.background() {
                        ColorAttribute::Default => None,
                        explicit => Some(palette.resolve_bg(explicit)),
                    }
                };
                let Some(SrgbaTuple(red, green, blue, _)) = color else {
                    continue;
                };
                let background = CellBg::rgb(unorm8(red), unorm8(green), unorm8(blue));
                let col = u32::try_from(cell.cell_index()).unwrap_or(u32::MAX);
                if cell.width() > 1 {
                    cells.set_wide(row, col, background);
                } else {
                    cells.set(row, col, background);
                }
            }
        }

        if let Some(selection) = self.selection(pane.pane_id()) {
            if let Some(range) = selection.range {
                let range = range.normalize();
                for row in 0..rows {
                    let stable = top.saturating_add(row as StableRowIndex);
                    let span = range.cols_for_row(stable, selection.rectangular);
                    for col in span.start..span.end.min(cols) {
                        let (row, col) = (row as u32, col as u32);
                        if let Some(background) = cells.get(row, col) {
                            cells.set(row, col, background.selected());
                        }
                    }
                }
            }
        }

        let cursor = pane.get_cursor_position();
        let cursor_row = cursor.y - top;
        let focused_and_active = self.focused.is_some();
        let metrics = &self.render_metrics;
        let cursor = if cursor.visibility == CursorVisibility::Visible
            && (0..rows as StableRowIndex).contains(&cursor_row)
        {
            let shape = self
                .config
                .default_cursor_style
                .effective_shape(cursor.shape);
            let row = cursor_row as u32;
            let col = cursor.x as u32;
            let width_cells = lines
                .get((top + cursor_row - first) as usize)
                .and_then(|line| line.get_cell(cursor.x))
                .map_or(1, |cell| cell.width().clamp(1, 2) as u32);
            CursorUniform {
                shape: cursor_shape(shape, focused_and_active),
                col,
                row,
                width_cells,
                thickness: metrics.underline_height.max(1) as f32,
                color: premultiplied(if focused_and_active {
                    palette.cursor_bg
                } else {
                    palette.cursor_border
                }),
            }
        } else {
            CursorUniform::default()
        };

        let (padding_left, padding_top) = self.padding_left_top();
        let tab_bar_height = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().unwrap_or(0.0)
        } else {
            0.0
        };
        let border = self.get_os_border();
        let background = BackgroundUniforms {
            cell_size: [
                metrics.cell_size.width as f32,
                metrics.cell_size.height as f32,
            ],
            grid_origin: [
                padding_left + border.left.get() as f32,
                tab_bar_height + padding_top + border.top.get() as f32,
            ],
            row_offset: cells.row_offset(),
            cursor,
            selection_tint: premultiplied(palette.selection_bg),
            search_tint: [0.0; 4],
            current_match_tint: [0.0; 4],
        };
        Some((cells, background))
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
    fn colors_become_unorm_bytes_and_premultiplied_tints() {
        assert_eq!(unorm8(0.2), 51);
        assert_eq!(unorm8(1.5), 255);
        assert_eq!(unorm8(-1.0), 0);
        assert_eq!(
            premultiplied(SrgbaTuple(1.0, 0.5, 0.25, 0.5)),
            [0.5, 0.25, 0.125, 0.5]
        );
    }
}
