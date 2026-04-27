use crate::termwindow::render::TripleLayerQuadAllocator;
use crate::termwindow::{UIItem, UIItemType};
use ::window::RectF;
use mux::pane::Pane;
use mux::tab::{PositionedSplit, SplitDirection};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq)]
struct SplitRenderGeometry {
    rect: RectF,
    ui_x: usize,
    ui_y: usize,
    ui_width: usize,
    ui_height: usize,
}

fn split_render_geometry(
    split: &PositionedSplit,
    cell_width: f32,
    cell_height: f32,
    underline_height: f32,
    first_row_offset: f32,
    padding_left: f32,
    padding_top: f32,
    border_left: usize,
) -> SplitRenderGeometry {
    let pos_y = split.top as f32 * cell_height + first_row_offset + padding_top;
    let pos_x = split.left as f32 * cell_width + padding_left + border_left as f32;

    let ui_x = border_left + padding_left as usize + (split.left * cell_width as usize);
    let ui_y = padding_top as usize + first_row_offset as usize + split.top * cell_height as usize;

    if split.direction == SplitDirection::Horizontal {
        SplitRenderGeometry {
            rect: euclid::rect(
                pos_x + (cell_width / 2.0),
                pos_y - (cell_height / 2.0),
                underline_height,
                (1. + split.size as f32) * cell_height,
            ),
            ui_x,
            ui_y,
            ui_width: cell_width as usize,
            ui_height: split.size * cell_height as usize,
        }
    } else {
        SplitRenderGeometry {
            rect: euclid::rect(
                pos_x - (cell_width / 2.0),
                pos_y + (cell_height / 2.0),
                (1.0 + split.size as f32) * cell_width,
                underline_height,
            ),
            ui_x,
            ui_y,
            ui_width: split.size * cell_width as usize,
            ui_height: cell_height as usize,
        }
    }
}

impl crate::TermWindow {
    pub fn paint_split(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        split: &PositionedSplit,
        pane: &Arc<dyn Pane>,
    ) -> anyhow::Result<()> {
        let palette = pane.palette();
        let foreground = palette.split.to_linear();
        let cell_width = self.render_metrics.cell_size.width as f32;
        let cell_height = self.render_metrics.cell_size.height as f32;

        let border = self.get_os_border();
        let first_row_offset = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height()?
        } else {
            0.
        } + border.top.get() as f32;

        let (padding_left, padding_top) = self.padding_left_top();

        let geometry = split_render_geometry(
            split,
            cell_width,
            cell_height,
            self.render_metrics.underline_height as f32,
            first_row_offset,
            padding_left,
            padding_top,
            border.left.get(),
        );

        if split.direction == SplitDirection::Horizontal {
            self.filled_rectangle(layers, 2, geometry.rect, foreground)?;
            self.ui_items.push(UIItem {
                x: geometry.ui_x,
                width: geometry.ui_width,
                y: geometry.ui_y,
                height: geometry.ui_height,
                item_type: UIItemType::Split(split.clone()),
            });
        } else {
            self.filled_rectangle(layers, 2, geometry.rect, foreground)?;
            self.ui_items.push(UIItem {
                x: geometry.ui_x,
                width: geometry.ui_width,
                y: geometry.ui_y,
                height: geometry.ui_height,
                item_type: UIItemType::Split(split.clone()),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(direction: SplitDirection) -> PositionedSplit {
        PositionedSplit {
            index: 7,
            direction,
            left: 4,
            top: 3,
            size: 5,
        }
    }

    #[test]
    fn horizontal_split_geometry_centers_thin_gpu_quad_in_split_cell() {
        let geometry = split_render_geometry(
            &split(SplitDirection::Horizontal),
            10.0,
            20.0,
            2.0,
            30.0,
            4.0,
            6.0,
            8,
        );

        assert_eq!(geometry.rect.min_x(), 57.0);
        assert_eq!(geometry.rect.min_y(), 86.0);
        assert_eq!(geometry.rect.width(), 2.0);
        assert_eq!(geometry.rect.height(), 120.0);

        assert_eq!(geometry.ui_x, 52);
        assert_eq!(geometry.ui_y, 96);
        assert_eq!(geometry.ui_width, 10);
        assert_eq!(geometry.ui_height, 100);
    }

    #[test]
    fn vertical_split_geometry_centers_thin_gpu_quad_in_split_cell() {
        let geometry = split_render_geometry(
            &split(SplitDirection::Vertical),
            10.0,
            20.0,
            2.0,
            30.0,
            4.0,
            6.0,
            8,
        );

        assert_eq!(geometry.rect.min_x(), 47.0);
        assert_eq!(geometry.rect.min_y(), 106.0);
        assert_eq!(geometry.rect.width(), 60.0);
        assert_eq!(geometry.rect.height(), 2.0);

        assert_eq!(geometry.ui_x, 52);
        assert_eq!(geometry.ui_y, 96);
        assert_eq!(geometry.ui_width, 50);
        assert_eq!(geometry.ui_height, 20);
    }
}
