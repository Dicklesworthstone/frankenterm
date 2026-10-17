use crate::quad::TripleLayerQuadAllocator;
use crate::utilsprites::RenderMetrics;
use ::window::ULength;
use config::{ConfigHandle, DimensionContext};

impl crate::TermWindow {
    pub fn paint_window_borders(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        for (rect, color) in self.window_border_rects() {
            self.filled_rectangle(layers, 1, rect, color)?;
        }
        Ok(())
    }

    /// The window border's edges with their colors, in the order both front
    /// ends draw them: top, left, bottom, right (the Metal frame since
    /// ft-yccm0.4.7.1). Empty without a border.
    pub(crate) fn window_border_rects(
        &self,
    ) -> Vec<(::window::RectF, ::window::color::LinearRgba)> {
        let border_dimensions = self.get_os_border();
        let height = self.dimensions.pixel_height as f32;
        let width = self.dimensions.pixel_width as f32;
        let frame = &self.config.window_frame;
        let color = |configured: Option<config::RgbaColor>| {
            configured
                .map(|c| c.to_linear())
                .unwrap_or(border_dimensions.color)
        };
        let mut edges = Vec::new();

        let border_top = border_dimensions.top.get() as f32;
        if border_top > 0.0 {
            edges.push((
                euclid::rect(0.0, 0.0, width, border_top),
                color(frame.border_top_color),
            ));
        }

        let border_left = border_dimensions.left.get() as f32;
        if border_left > 0.0 {
            edges.push((
                euclid::rect(0.0, 0.0, border_left, height),
                color(frame.border_left_color),
            ));
        }

        let border_bottom = border_dimensions.bottom.get() as f32;
        if border_bottom > 0.0 {
            edges.push((
                euclid::rect(0.0, height - border_bottom, width, height),
                color(frame.border_bottom_color),
            ));
        }

        let border_right = border_dimensions.right.get() as f32;
        if border_right > 0.0 {
            edges.push((
                euclid::rect(width - border_right, 0.0, border_right, height),
                color(frame.border_right_color),
            ));
        }
        edges
    }

    pub fn get_os_border_impl(
        os_parameters: &Option<window::parameters::Parameters>,
        config: &ConfigHandle,
        dimensions: &crate::Dimensions,
        render_metrics: &RenderMetrics,
    ) -> window::parameters::Border {
        let mut border = os_parameters
            .as_ref()
            .and_then(|p| p.border_dimensions.clone())
            .unwrap_or_default();

        border.left += ULength::new(
            config
                .window_frame
                .border_left_width
                .evaluate_as_pixels(DimensionContext {
                    dpi: dimensions.dpi as f32,
                    pixel_max: dimensions.pixel_width as f32,
                    pixel_cell: render_metrics.cell_size.width as f32,
                })
                .ceil() as usize,
        );
        border.right += ULength::new(
            config
                .window_frame
                .border_right_width
                .evaluate_as_pixels(DimensionContext {
                    dpi: dimensions.dpi as f32,
                    pixel_max: dimensions.pixel_width as f32,
                    pixel_cell: render_metrics.cell_size.width as f32,
                })
                .ceil() as usize,
        );
        border.top += ULength::new(
            config
                .window_frame
                .border_top_height
                .evaluate_as_pixels(DimensionContext {
                    dpi: dimensions.dpi as f32,
                    pixel_max: dimensions.pixel_height as f32,
                    pixel_cell: render_metrics.cell_size.height as f32,
                })
                .ceil() as usize,
        );
        border.bottom += ULength::new(
            config
                .window_frame
                .border_bottom_height
                .evaluate_as_pixels(DimensionContext {
                    dpi: dimensions.dpi as f32,
                    pixel_max: dimensions.pixel_height as f32,
                    pixel_cell: render_metrics.cell_size.height as f32,
                })
                .ceil() as usize,
        );

        border
    }

    pub fn get_os_border(&self) -> window::parameters::Border {
        Self::get_os_border_impl(
            &self.os_parameters,
            &self.config,
            &self.dimensions,
            &self.render_metrics,
        )
    }
}
