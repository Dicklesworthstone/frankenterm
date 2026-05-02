use crate::{Position, StableRowIndex, TerminalState};
use anyhow::Context;
use frankenterm_cell::image::{ImageCell, ImageDataType};
use frankenterm_cell::Cell;
use frankenterm_surface::change::ImageData;
use frankenterm_surface::TextureCoordinate;
use humansize::{SizeFormatter, DECIMAL};
use num_traits::{One, Zero};
use ordered_float::NotNan;
use std::convert::TryFrom;
use std::sync::Arc;

const IMAGE_CELL_SPAN_COLUMNS: &str = "columns";
const IMAGE_CELL_SPAN_ROWS: &str = "rows";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementInfo {
    pub first_row: StableRowIndex,
    pub rows: usize,
    pub cols: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ImageAttachParams {
    /// Dimensions of the underlying ImageData, in pixels
    pub image_width: u32,
    pub image_height: u32,

    /// Dimensions of the area of the image to be displayed, in pixels
    pub source_width: Option<u32>,
    pub source_height: Option<u32>,

    /// Origin of the source data region, top left corner in pixels
    pub source_origin_x: u32,
    pub source_origin_y: u32,

    /// When rendering in the cell, use this offset from the top left
    /// of the cell. This is only used in the Kitty image protocol.
    /// This should be smaller than the size of the cell. Larger values will
    /// be truncated.
    pub cell_padding_left: u16,
    pub cell_padding_top: u16,

    /// Plane on which to display the image
    pub z_index: i32,

    /// Desired number of cells to span.
    /// If None, then compute based on source_width and source_height
    pub columns: Option<usize>,
    pub rows: Option<usize>,

    pub image_id: Option<u32>,
    pub placement_id: Option<u32>,

    pub style: ImageAttachStyle,
    pub do_not_move_cursor: bool,

    pub data: Arc<ImageData>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageAttachStyle {
    Sixel,
    Iterm,
    Kitty,
}

impl TerminalState {
    pub(crate) fn assign_image_to_cells(
        &mut self,
        params: ImageAttachParams,
    ) -> anyhow::Result<PlacementInfo> {
        let seqno = self.seqno;
        let physical_cols = self.screen().physical_cols.max(1);
        let physical_rows = self.screen().physical_rows.max(1);
        let cell_pixel_width = self.pixel_width / physical_cols;
        let cell_pixel_height = self.pixel_height / physical_rows;
        if cell_pixel_width == 0 || cell_pixel_height == 0 {
            anyhow::bail!(
                "terminal cell pixel dimensions must be non-zero ({}x{})",
                cell_pixel_width,
                cell_pixel_height
            );
        }
        let cell_padding_left = params
            .cell_padding_left
            .min(cell_pixel_width.saturating_sub(1) as u16);
        let cell_padding_top = params
            .cell_padding_top
            .min(cell_pixel_height.saturating_sub(1) as u16);
        if params.image_width == 0 || params.image_height == 0 {
            anyhow::bail!("image has zero dimensions");
        }

        let image_max_width = params.image_width.saturating_sub(params.source_origin_x);
        let image_max_height = params.image_height.saturating_sub(params.source_origin_y);
        let draw_width = params
            .source_width
            .unwrap_or(image_max_width)
            .min(image_max_width);
        let draw_height = params
            .source_height
            .unwrap_or(image_max_height)
            .min(image_max_height);

        if draw_width == 0 || draw_height == 0 {
            anyhow::bail!("image draw region has zero dimensions");
        }

        let (fullcells_width, remainder_width_cell, x_delta_divisor) = match params.columns {
            Some(cols) => {
                let target_pixels = checked_explicit_cell_span_pixels(
                    IMAGE_CELL_SPAN_COLUMNS,
                    cols,
                    physical_cols,
                    cell_pixel_width,
                )?;
                let x_delta_divisor = checked_texture_delta_divisor(
                    IMAGE_CELL_SPAN_COLUMNS,
                    target_pixels,
                    params.image_width,
                    draw_width,
                )?;
                (cols, 0, x_delta_divisor)
            }
            None => (
                draw_width as usize / cell_pixel_width,
                draw_width as usize % cell_pixel_width,
                params.image_width,
            ),
        };
        let (fullcells_height, remainder_height_cell, y_delta_divisor) = match params.rows {
            Some(rows) => {
                let target_pixels = checked_explicit_cell_span_pixels(
                    IMAGE_CELL_SPAN_ROWS,
                    rows,
                    physical_rows,
                    cell_pixel_height,
                )?;
                let y_delta_divisor = checked_texture_delta_divisor(
                    IMAGE_CELL_SPAN_ROWS,
                    target_pixels,
                    params.image_height,
                    draw_height,
                )?;
                (rows, 0, y_delta_divisor)
            }
            None => (
                draw_height as usize / cell_pixel_height,
                draw_height as usize % cell_pixel_height,
                params.image_height,
            ),
        };

        let target_pixel_width = checked_target_pixels(
            IMAGE_CELL_SPAN_COLUMNS,
            fullcells_width,
            cell_pixel_width,
            remainder_width_cell,
        )?;
        let target_pixel_height = checked_target_pixels(
            IMAGE_CELL_SPAN_ROWS,
            fullcells_height,
            cell_pixel_height,
            remainder_height_cell,
        )?;
        let first_row = self.screen().visible_row_to_stable_row(self.cursor.y);

        let mut ypos = NotNan::new(params.source_origin_y as f32 / params.image_height as f32)
            .with_context(|| format!("computing ypos {params:#?}"))?;
        let start_xpos = NotNan::new(params.source_origin_x as f32 / params.image_width as f32)
            .context("computing xpos")?;

        let cursor_x = self.cursor.x;

        let width_in_cells = fullcells_width + one_or_zero::<usize>(remainder_width_cell > 0);
        let height_in_cells = fullcells_height + one_or_zero::<usize>(remainder_height_cell > 0);
        let height_in_cells = if params.do_not_move_cursor {
            height_in_cells.min(self.screen().physical_rows - self.cursor.y as usize)
        } else {
            height_in_cells
        };

        log::debug!(
            "image is {}x{} cells (cell is {}x{}), target pixel dims {}x{}, {:?}, (term is {}x{}@{}x{})",
            width_in_cells,
            height_in_cells,
            cell_pixel_width,
            cell_pixel_height,
            target_pixel_width,
            target_pixel_height,
            params,
            physical_cols,
            physical_rows,
            self.pixel_width,
            self.pixel_height
        );

        let mut remain_y = target_pixel_height;
        for y in 0..height_in_cells {
            let padding_bottom = cell_pixel_height.saturating_sub(remain_y) as u16;
            let y_delta = (remain_y.min(cell_pixel_height) as f32) / y_delta_divisor as f32;
            remain_y = remain_y.saturating_sub(cell_pixel_height);

            let mut xpos = start_xpos;
            let cursor_y = if params.do_not_move_cursor {
                self.cursor.y + y as i64
            } else {
                self.cursor.y
            };
            log::debug!(
                "setting cells for y={} x=[{}..{}]",
                cursor_y,
                cursor_x,
                cursor_x + fullcells_width
            );
            let mut remain_x = target_pixel_width;
            for x in 0..width_in_cells {
                let padding_right = cell_pixel_width.saturating_sub(remain_x) as u16;
                let x_delta = (remain_x.min(cell_pixel_width) as f32) / x_delta_divisor as f32;
                remain_x = remain_x.saturating_sub(cell_pixel_width);
                log::debug!(
                    "x_delta {} ({} px), y_delta {} ({} px), padding_right={}, padding_bottom={}",
                    x_delta,
                    x_delta * x_delta_divisor as f32,
                    y_delta,
                    y_delta * y_delta_divisor as f32,
                    padding_right,
                    padding_bottom
                );
                let mut cell = self
                    .screen_mut()
                    .get_cell(cursor_x + x, cursor_y)
                    .cloned()
                    .unwrap_or_else(Cell::blank);
                let img = Box::new(ImageCell::with_z_index(
                    TextureCoordinate::new(xpos, ypos),
                    TextureCoordinate::new(xpos + x_delta, ypos + y_delta),
                    params.data.clone(),
                    params.z_index,
                    cell_padding_left,
                    cell_padding_top,
                    padding_right,
                    padding_bottom,
                    params.image_id,
                    params.placement_id,
                ));
                match params.style {
                    ImageAttachStyle::Kitty => cell.attrs_mut().attach_image(img),
                    ImageAttachStyle::Sixel | ImageAttachStyle::Iterm => {
                        cell.attrs_mut().set_image(img)
                    }
                };

                self.screen_mut()
                    .set_cell(cursor_x + x, cursor_y, &cell, seqno);
                xpos += x_delta;
            }
            ypos += y_delta;
            if !params.do_not_move_cursor && y < height_in_cells - 1 {
                self.new_line(false);
            }
        }

        // adjust cursor position if the drawn cells move beyond current cell
        let x_padding_shift: i64 = one_or_zero(
            draw_width as usize + cell_padding_left as usize > cell_pixel_width * width_in_cells,
        );
        let y_padding_shift: i64 = one_or_zero(
            draw_height as usize + cell_padding_top as usize > cell_pixel_height * height_in_cells,
        );
        if !params.do_not_move_cursor {
            // Sixel places the cursor under the left corner of the image,
            // unless sixel_scrolls_right is enabled.
            // iTerm places it after the bottom right corner.
            let bottom_right = match params.style {
                ImageAttachStyle::Kitty | ImageAttachStyle::Iterm => true,
                ImageAttachStyle::Sixel => self.sixel_scrolls_right,
            };

            if bottom_right {
                self.set_cursor_pos(
                    &Position::Relative(width_in_cells as i64 + x_padding_shift),
                    &Position::Relative(y_padding_shift),
                );
            }
        }

        Ok(PlacementInfo {
            first_row,
            rows: height_in_cells,
            cols: width_in_cells,
        })
    }

    /// cache recent images and avoid assigning a new id for repeated data!
    pub(crate) fn raw_image_to_image_data(
        &mut self,
        data: ImageDataType,
    ) -> Result<Arc<ImageData>, termwiz::error::InternalError> {
        let key = data.compute_hash();
        if let Some(item) = self.image_cache.get(&key) {
            Ok(Arc::clone(item))
        } else {
            let data = data.swap_out()?;
            let image_data = Arc::new(ImageData::with_data(data));
            self.image_cache.put(key, Arc::clone(&image_data));
            Ok(image_data)
        }
    }
}

pub(crate) fn check_image_dimensions(width: u32, height: u32) -> anyhow::Result<()> {
    const MAX_IMAGE_SIZE: u32 = 100_000_000;
    let size = width.saturating_mul(height).saturating_mul(4);
    if size > MAX_IMAGE_SIZE {
        anyhow::bail!(
            "Ignoring image data for image with dimensions {}x{} \
             because required RAM {} > max allowed {}",
            width,
            height,
            SizeFormatter::new(size, DECIMAL),
            SizeFormatter::new(MAX_IMAGE_SIZE, DECIMAL),
        );
    }
    if size == 0 {
        anyhow::bail!("Ignoring image with 0x0 dimensions");
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub format: image::ImageFormat,
}

pub(crate) fn dimensions(data: &[u8]) -> anyhow::Result<ImageInfo> {
    let reader = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?;
    let format = reader
        .format()
        .ok_or_else(|| anyhow::anyhow!("unknown format!?"))?;
    let (width, height) = reader.into_dimensions()?;
    Ok(ImageInfo {
        width,
        height,
        format,
    })
}

/// Returns `1` if `b` is true, else `0`,
fn one_or_zero<T: Zero + One>(b: bool) -> T {
    if b {
        T::one()
    } else {
        T::zero()
    }
}

fn checked_explicit_cell_span_pixels(
    axis: &str,
    span: usize,
    terminal_span: usize,
    cell_pixels: usize,
) -> anyhow::Result<usize> {
    if span == 0 {
        anyhow::bail!("image placement {axis} must be at least 1");
    }
    if span > terminal_span {
        anyhow::bail!(
            "image placement {axis} {} exceeds terminal {axis} {}",
            span,
            terminal_span
        );
    }
    span.checked_mul(cell_pixels).ok_or_else(|| {
        anyhow::anyhow!("image placement {axis} pixel span overflows: {span} * {cell_pixels}")
    })
}

fn checked_texture_delta_divisor(
    axis: &str,
    target_pixels: usize,
    image_pixels: u32,
    draw_pixels: u32,
) -> anyhow::Result<u32> {
    let target_pixels = u32::try_from(target_pixels).with_context(|| {
        format!("image placement {axis} target pixel span exceeds u32: {target_pixels}")
    })?;
    target_pixels
        .checked_mul(image_pixels)
        .map(|value| value / draw_pixels)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            anyhow::anyhow!("image placement {axis} texture delta divisor overflows or is zero")
        })
}

fn checked_target_pixels(
    axis: &str,
    full_cells: usize,
    cell_pixels: usize,
    remainder_pixels: usize,
) -> anyhow::Result<usize> {
    full_cells
        .checked_mul(cell_pixels)
        .and_then(|value| value.checked_add(remainder_pixels))
        .ok_or_else(|| anyhow::anyhow!("image placement {axis} target pixel span overflows"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorPalette;
    use crate::{TerminalConfiguration, TerminalSize};

    #[derive(Debug)]
    struct ImageAttachTestConfig;

    impl TerminalConfiguration for ImageAttachTestConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    fn test_terminal_state() -> TerminalState {
        TerminalState::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            Arc::new(ImageAttachTestConfig),
            "test-program",
            "1.0",
            Box::new(std::io::sink()),
        )
    }

    fn test_image_attach_params() -> ImageAttachParams {
        ImageAttachParams {
            image_width: 8,
            image_height: 8,
            source_width: None,
            source_height: None,
            source_origin_x: 0,
            source_origin_y: 0,
            cell_padding_left: 0,
            cell_padding_top: 0,
            z_index: 0,
            columns: Some(1),
            rows: Some(1),
            image_id: Some(1),
            placement_id: None,
            style: ImageAttachStyle::Kitty,
            do_not_move_cursor: false,
            data: Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
                8,
                8,
                vec![0; 8 * 8 * 4],
            ))),
        }
    }

    // ── PlacementInfo ──────────────────────────────────────

    #[test]
    fn placement_info_construction() {
        let info = PlacementInfo {
            first_row: 0,
            rows: 3,
            cols: 4,
        };
        assert_eq!(info.first_row, 0);
        assert_eq!(info.rows, 3);
        assert_eq!(info.cols, 4);
    }

    #[test]
    fn placement_info_clone_copy() {
        let info = PlacementInfo {
            first_row: 5,
            rows: 10,
            cols: 20,
        };
        let copied = info;
        assert_eq!(info, copied);
    }

    #[test]
    fn placement_info_debug() {
        let info = PlacementInfo {
            first_row: 0,
            rows: 1,
            cols: 1,
        };
        let dbg = format!("{:?}", info);
        assert!(dbg.contains("PlacementInfo"));
    }

    #[test]
    fn placement_info_eq_ne() {
        let a = PlacementInfo {
            first_row: 0,
            rows: 1,
            cols: 1,
        };
        let b = PlacementInfo {
            first_row: 0,
            rows: 1,
            cols: 2,
        };
        assert_eq!(a, a);
        assert_ne!(a, b);
    }

    // ── ImageAttachStyle ───────────────────────────────────

    #[test]
    fn image_attach_style_eq() {
        assert_eq!(ImageAttachStyle::Sixel, ImageAttachStyle::Sixel);
        assert_eq!(ImageAttachStyle::Iterm, ImageAttachStyle::Iterm);
        assert_eq!(ImageAttachStyle::Kitty, ImageAttachStyle::Kitty);
    }

    #[test]
    fn image_attach_style_ne() {
        assert_ne!(ImageAttachStyle::Sixel, ImageAttachStyle::Iterm);
        assert_ne!(ImageAttachStyle::Iterm, ImageAttachStyle::Kitty);
        assert_ne!(ImageAttachStyle::Kitty, ImageAttachStyle::Sixel);
    }

    #[test]
    fn image_attach_style_clone_copy() {
        let style = ImageAttachStyle::Kitty;
        let copied = style;
        assert_eq!(style, copied);
    }

    #[test]
    fn image_attach_style_debug() {
        assert!(format!("{:?}", ImageAttachStyle::Sixel).contains("Sixel"));
        assert!(format!("{:?}", ImageAttachStyle::Iterm).contains("Iterm"));
        assert!(format!("{:?}", ImageAttachStyle::Kitty).contains("Kitty"));
    }

    #[test]
    fn assign_image_rejects_zero_explicit_columns() {
        let mut terminal = test_terminal_state();
        let mut params = test_image_attach_params();
        params.columns = Some(0);

        let err = terminal.assign_image_to_cells(params).unwrap_err();
        assert!(err.to_string().contains("columns must be at least 1"));
    }

    #[test]
    fn assign_image_rejects_explicit_columns_past_terminal_width() {
        let mut terminal = test_terminal_state();
        let mut params = test_image_attach_params();
        params.columns = Some(u32::MAX as usize);

        let err = terminal.assign_image_to_cells(params).unwrap_err();
        assert!(
            err.to_string().contains("columns")
                && err.to_string().contains("exceeds terminal columns")
        );
    }

    #[test]
    fn assign_image_rejects_explicit_rows_past_terminal_height() {
        let mut terminal = test_terminal_state();
        let mut params = test_image_attach_params();
        params.rows = Some(u32::MAX as usize);

        let err = terminal.assign_image_to_cells(params).unwrap_err();
        assert!(
            err.to_string().contains("rows") && err.to_string().contains("exceeds terminal rows")
        );
    }

    #[test]
    fn assign_image_accepts_bounded_explicit_columns_and_rows() {
        let mut terminal = test_terminal_state();
        let mut params = test_image_attach_params();
        params.columns = Some(2);
        params.rows = Some(2);

        let info = terminal
            .assign_image_to_cells(params)
            .expect("bounded placement should attach");
        assert_eq!(info.cols, 2);
        assert_eq!(info.rows, 2);
    }

    // ── check_image_dimensions ─────────────────────────────

    #[test]
    fn check_image_dimensions_valid() {
        assert!(check_image_dimensions(100, 100).is_ok());
    }

    #[test]
    fn check_image_dimensions_zero_width() {
        let err = check_image_dimensions(0, 100).unwrap_err();
        assert!(err.to_string().contains("0x0"));
    }

    #[test]
    fn check_image_dimensions_zero_height() {
        let err = check_image_dimensions(100, 0).unwrap_err();
        assert!(err.to_string().contains("0x0"));
    }

    #[test]
    fn check_image_dimensions_too_large() {
        // 10000 * 10000 * 4 = 400_000_000 > 100_000_000
        let err = check_image_dimensions(10000, 10000).unwrap_err();
        assert!(err.to_string().contains("Ignoring image data"));
    }

    #[test]
    fn check_image_dimensions_at_limit() {
        // 5000 * 5000 * 4 = 100_000_000 == MAX_IMAGE_SIZE
        // The check uses `>` (strictly greater), so exactly-at-limit is allowed.
        let result = check_image_dimensions(5000, 5000);
        assert!(result.is_ok());
    }

    #[test]
    fn check_image_dimensions_just_under_limit() {
        // 4999 * 5000 * 4 = 99_980_000 < 100_000_000
        assert!(check_image_dimensions(4999, 5000).is_ok());
    }

    #[test]
    fn check_image_dimensions_one_by_one() {
        assert!(check_image_dimensions(1, 1).is_ok());
    }

    #[test]
    fn check_image_dimensions_overflow_saturates() {
        // u32::MAX * u32::MAX would overflow, but saturating_mul caps it
        let err = check_image_dimensions(u32::MAX, u32::MAX).unwrap_err();
        assert!(err.to_string().contains("Ignoring image data"));
    }

    // ── one_or_zero ────────────────────────────────────────

    #[test]
    fn one_or_zero_true() {
        assert_eq!(one_or_zero::<usize>(true), 1);
        assert_eq!(one_or_zero::<i32>(true), 1);
        assert_eq!(one_or_zero::<i64>(true), 1);
    }

    #[test]
    fn one_or_zero_false() {
        assert_eq!(one_or_zero::<usize>(false), 0);
        assert_eq!(one_or_zero::<i32>(false), 0);
        assert_eq!(one_or_zero::<i64>(false), 0);
    }
}
