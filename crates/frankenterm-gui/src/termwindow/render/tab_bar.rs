use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::render::RenderScreenLineParams;
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use config::{ConfigHandle, TabBarPosition};
use mux::renderable::RenderableDimensions;
use wezterm_term::Line;
use wezterm_term::color::{ColorAttribute, ColorPalette};
use window::color::LinearRgba;

/// Pixels that the tab bar takes from each edge of the window's
/// terminal area.  At most one of the fields is non-zero.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TabBarInsets {
    pub top: f32,
    pub bottom: f32,
    pub left: f32,
    pub right: f32,
}

impl TabBarInsets {
    /// `horizontal_height` is the height of a top/bottom tab bar and
    /// `vertical_width` the width of a left/right tab bar, in pixels.
    pub fn new(
        position: TabBarPosition,
        show_tab_bar: bool,
        horizontal_height: f32,
        vertical_width: f32,
    ) -> Self {
        let mut insets = Self::default();
        if !show_tab_bar {
            return insets;
        }
        match position {
            TabBarPosition::Top => insets.top = horizontal_height,
            TabBarPosition::Bottom => insets.bottom = horizontal_height,
            TabBarPosition::Left => insets.left = vertical_width,
            TabBarPosition::Right => insets.right = vertical_width,
        }
        insets
    }

    /// Total width taken from the terminal area.
    pub fn width(&self) -> f32 {
        self.left + self.right
    }

    /// Total height taken from the terminal area.
    pub fn height(&self) -> f32 {
        self.top + self.bottom
    }
}

impl crate::TermWindow {
    pub fn paint_tab_bar(&mut self, layers: &mut TripleLayerQuadAllocator) -> anyhow::Result<()> {
        if self.config.is_vertical_tab_bar() {
            // The vertical tab bar is always drawn in the retro style,
            // including when use_fancy_tab_bar is set.
            return self.paint_vertical_tab_bar(layers);
        }

        if self.config.use_fancy_tab_bar {
            if self.fancy_tab_bar.is_none() {
                let palette = self.palette().clone();
                let tab_bar = self.build_fancy_tab_bar(&palette)?;
                self.fancy_tab_bar.replace(tab_bar);
            }

            self.ui_items.append(&mut self.paint_fancy_tab_bar()?);
            return Ok(());
        }

        let border = self.get_os_border();

        let palette = self.palette().clone();
        let tab_bar_height = self.tab_bar_pixel_height()?;
        let tab_bar_y = if self.config.is_tab_bar_at_bottom() {
            ((self.dimensions.pixel_height as f32) - (tab_bar_height + border.bottom.get() as f32))
                .max(0.)
        } else {
            border.top.get() as f32
        };

        // Register the tab bar location
        self.ui_items.append(&mut self.tab_bar.compute_ui_items(
            0,
            tab_bar_y as usize,
            self.render_metrics.cell_size.width as usize,
            self.render_metrics.cell_size.height as usize,
        ));

        let cols =
            self.dimensions.pixel_width / (self.render_metrics.cell_size.width as usize).max(1);
        self.render_tab_bar_line(
            layers,
            &palette,
            self.tab_bar.line(),
            0.,
            tab_bar_y,
            self.dimensions.pixel_width as f32,
            cols,
        )
    }

    fn paint_vertical_tab_bar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let border = self.get_os_border();
        let palette = self.palette().clone();
        let tab_bar_width = self.vertical_tab_bar_pixel_width();
        let cell_height = self.render_metrics.cell_size.height as f32;

        let left_pixel_x = self.vertical_tab_bar_left_pixel_x();
        let top_pixel_y = border.top.get() as f32;

        self.ui_items.append(&mut self.tab_bar.compute_ui_items(
            left_pixel_x as usize,
            top_pixel_y as usize,
            self.render_metrics.cell_size.width as usize,
            self.render_metrics.cell_size.height as usize,
        ));

        let cols = self.config.vertical_tab_width.max(1);
        for (row, line) in self.tab_bar.vertical_lines().iter().enumerate() {
            self.render_tab_bar_line(
                layers,
                &palette,
                line,
                left_pixel_x,
                top_pixel_y + (row as f32 * cell_height),
                tab_bar_width,
                cols,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_tab_bar_line(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        palette: &ColorPalette,
        line: &Line,
        left_pixel_x: f32,
        top_pixel_y: f32,
        pixel_width: f32,
        cols: usize,
    ) -> anyhow::Result<()> {
        let window_is_transparent =
            !self.window_background.is_empty() || self.config.window_background_opacity != 1.0;
        let gl_state = self
            .render_state
            .as_ref()
            .context("render state is not initialized")?;
        let white_space = gl_state.util_sprites.white_space.texture_coords();
        let filled_box = gl_state.util_sprites.filled_box.texture_coords();
        let default_bg = palette
            .resolve_bg(ColorAttribute::Default)
            .to_linear()
            .mul_alpha(if window_is_transparent {
                0.
            } else {
                self.config.text_background_opacity
            });

        self.render_screen_line(
            RenderScreenLineParams {
                top_pixel_y,
                left_pixel_x,
                pixel_width,
                stable_line_idx: None,
                line,
                selection: 0..0,
                cursor: &Default::default(),
                palette,
                dims: &RenderableDimensions {
                    cols,
                    physical_top: 0,
                    scrollback_rows: 0,
                    scrollback_top: 0,
                    viewport_rows: 1,
                    dpi: self.terminal_size.dpi,
                    pixel_height: self.render_metrics.cell_size.height as usize,
                    pixel_width: pixel_width as usize,
                    reverse_video: false,
                },
                config: &self.config,
                cursor_border_color: LinearRgba::default(),
                foreground: palette.foreground.to_linear(),
                pane: None,
                is_active: true,
                selection_fg: LinearRgba::default(),
                selection_bg: LinearRgba::default(),
                cursor_fg: LinearRgba::default(),
                cursor_bg: LinearRgba::default(),
                cursor_is_default_color: true,
                white_space,
                filled_box,
                window_is_transparent,
                default_bg,
                style: None,
                font: None,
                use_pixel_positioning: self.config.experimental_pixel_positioning,
                render_metrics: self.render_metrics,
                shape_key: None,
                password_input: false,
            },
            layers,
        )?;

        Ok(())
    }

    pub fn tab_bar_pixel_height_impl(
        config: &ConfigHandle,
        fontconfig: &frankenterm_font::FontConfiguration,
        render_metrics: &RenderMetrics,
    ) -> anyhow::Result<f32> {
        if config.use_fancy_tab_bar {
            let font = fontconfig.title_font()?;
            Ok((font.metrics().cell_height.get() as f32 * 1.75).ceil())
        } else {
            Ok(render_metrics.cell_size.height as f32)
        }
    }

    /// Height of a horizontal (top or bottom) tab bar, in pixels.
    pub fn tab_bar_pixel_height(&self) -> anyhow::Result<f32> {
        Self::tab_bar_pixel_height_impl(&self.config, &self.fonts, &self.render_metrics)
    }

    /// Width of a vertical (left or right) tab bar, in pixels.
    pub fn vertical_tab_bar_pixel_width_impl(
        config: &ConfigHandle,
        render_metrics: &RenderMetrics,
    ) -> f32 {
        (config.vertical_tab_width * render_metrics.cell_size.width as usize) as f32
    }

    pub fn vertical_tab_bar_pixel_width(&self) -> f32 {
        Self::vertical_tab_bar_pixel_width_impl(&self.config, &self.render_metrics)
    }

    /// X coordinate of the left edge of the vertical tab bar.
    pub fn vertical_tab_bar_left_pixel_x(&self) -> f32 {
        let border = self.get_os_border();
        match self.config.effective_tab_bar_position() {
            TabBarPosition::Right => (self.dimensions.pixel_width as f32
                - self.vertical_tab_bar_pixel_width()
                - border.right.get() as f32)
                .max(0.),
            _ => border.left.get() as f32,
        }
    }

    pub fn tab_bar_insets_impl(
        config: &ConfigHandle,
        show_tab_bar: bool,
        fontconfig: &frankenterm_font::FontConfiguration,
        render_metrics: &RenderMetrics,
    ) -> anyhow::Result<TabBarInsets> {
        let position = config.effective_tab_bar_position();
        if !show_tab_bar {
            return Ok(TabBarInsets::default());
        }
        let (height, width) = if config.is_vertical_tab_bar() {
            (
                0.,
                Self::vertical_tab_bar_pixel_width_impl(config, render_metrics),
            )
        } else {
            (
                Self::tab_bar_pixel_height_impl(config, fontconfig, render_metrics)?,
                0.,
            )
        };
        Ok(TabBarInsets::new(position, show_tab_bar, height, width))
    }

    /// The space the tab bar currently takes from each edge of the
    /// terminal area.  All zero when the tab bar is hidden.
    pub fn tab_bar_insets(&self) -> anyhow::Result<TabBarInsets> {
        Self::tab_bar_insets_impl(
            &self.config,
            self.show_tab_bar,
            &self.fonts,
            &self.render_metrics,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insets_follow_position() {
        let top = TabBarInsets::new(TabBarPosition::Top, true, 20., 200.);
        assert_eq!(
            top,
            TabBarInsets {
                top: 20.,
                ..Default::default()
            }
        );
        let bottom = TabBarInsets::new(TabBarPosition::Bottom, true, 20., 200.);
        assert_eq!(bottom.bottom, 20.);
        assert_eq!(bottom.height(), 20.);
        assert_eq!(bottom.width(), 0.);

        let left = TabBarInsets::new(TabBarPosition::Left, true, 20., 200.);
        assert_eq!(
            left,
            TabBarInsets {
                left: 200.,
                ..Default::default()
            }
        );
        assert_eq!(left.width(), 200.);
        assert_eq!(left.height(), 0.);

        let right = TabBarInsets::new(TabBarPosition::Right, true, 20., 200.);
        assert_eq!(right.right, 200.);
        assert_eq!(right.width(), 200.);
    }

    #[test]
    fn hidden_tab_bar_takes_no_space() {
        for position in [
            TabBarPosition::Top,
            TabBarPosition::Bottom,
            TabBarPosition::Left,
            TabBarPosition::Right,
        ] {
            assert_eq!(
                TabBarInsets::new(position, false, 20., 200.),
                TabBarInsets::default()
            );
        }
    }
}
