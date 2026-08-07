use super::usize_to_i64_saturating;
use crate::{Position, StableRowIndex, TerminalState};
use anyhow::Context;
use frankenterm_cell::image::{ImageCell, ImageDataType};
use frankenterm_cell::Cell;
use frankenterm_surface::change::ImageData;
use frankenterm_surface::TextureCoordinate;
use humansize::{SizeFormatter, DECIMAL};
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
            .min(usize_to_u16_saturating(cell_pixel_width.saturating_sub(1)));
        let cell_padding_top = params
            .cell_padding_top
            .min(usize_to_u16_saturating(cell_pixel_height.saturating_sub(1)));
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

        let (fullcells_width, remainder_width_cell) = match params.columns {
            Some(cols) => {
                checked_explicit_cell_span_pixels(
                    IMAGE_CELL_SPAN_COLUMNS,
                    cols,
                    physical_cols,
                    cell_pixel_width,
                )?;
                (cols, 0)
            }
            None => (
                draw_width as usize / cell_pixel_width,
                draw_width as usize % cell_pixel_width,
            ),
        };
        let (fullcells_height, remainder_height_cell) = match params.rows {
            Some(rows) => {
                checked_explicit_cell_span_pixels(
                    IMAGE_CELL_SPAN_ROWS,
                    rows,
                    physical_rows,
                    cell_pixel_height,
                )?;
                (rows, 0)
            }
            None => (
                draw_height as usize / cell_pixel_height,
                draw_height as usize % cell_pixel_height,
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

        let cursor_x = self.cursor.x;

        let occupied_pixel_width = target_pixel_width
            .checked_add(cell_padding_left as usize)
            .context("image placement columns padded target span overflows")?;
        let occupied_pixel_height = target_pixel_height
            .checked_add(cell_padding_top as usize)
            .context("image placement rows padded target span overflows")?;
        let requested_width_in_cells = checked_ceil_cell_count(
            IMAGE_CELL_SPAN_COLUMNS,
            occupied_pixel_width,
            cell_pixel_width,
        )?;
        let requested_height_in_cells = checked_ceil_cell_count(
            IMAGE_CELL_SPAN_ROWS,
            occupied_pixel_height,
            cell_pixel_height,
        )?;

        // Image dimensions are peer/config controlled. Never let an implicit
        // skinny image expand a line or generate scrollback in work
        // proportional to millions of source pixels. Horizontal placement is
        // clipped to the visible row. A scrolling vertical placement retains
        // the final viewport of output cells, while a fixed-cursor placement
        // retains the leading rows that fit below the cursor.
        let width_in_cells = requested_width_in_cells
            .min(physical_cols.saturating_sub(cursor_x.min(physical_cols)));
        let available_fixed_rows = remaining_rows_from_cursor(physical_rows, self.cursor.y);
        let height_in_cells = if params.do_not_move_cursor {
            requested_height_in_cells.min(available_fixed_rows)
        } else {
            requested_height_in_cells.min(physical_rows)
        };
        let first_source_cell_row = if params.do_not_move_cursor {
            0
        } else {
            requested_height_in_cells.saturating_sub(height_in_cells)
        };

        if width_in_cells == 0 || height_in_cells == 0 {
            // A cursor outside the drawable viewport or a fixed placement
            // below its final row has no representable cells. In particular,
            // do not advance/scroll vertically when horizontal clipping left
            // no cells to attach.
            return Ok(PlacementInfo {
                first_row,
                rows: 0,
                cols: 0,
            });
        }

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

        for y in 0..height_in_cells {
            let source_cell_y = first_source_cell_row.saturating_add(y);
            let Some(y_slice) = image_cell_slice(
                IMAGE_CELL_SPAN_ROWS,
                source_cell_y,
                cell_pixel_height,
                cell_padding_top as usize,
                target_pixel_height,
            )? else {
                continue;
            };
            let texture_top = normalized_texture_position(
                IMAGE_CELL_SPAN_ROWS,
                params.source_origin_y,
                draw_height,
                y_slice.content_start,
                target_pixel_height,
                params.image_height,
            )?;
            let texture_bottom = normalized_texture_position(
                IMAGE_CELL_SPAN_ROWS,
                params.source_origin_y,
                draw_height,
                y_slice.content_end,
                target_pixel_height,
                params.image_height,
            )?;

            let cursor_y = if params.do_not_move_cursor {
                self.cursor.y.saturating_add(usize_to_i64_saturating(y))
            } else {
                self.cursor.y
            };
            log::debug!(
                "setting cells for y={} x=[{}..{}]",
                cursor_y,
                cursor_x,
                cursor_x.saturating_add(width_in_cells)
            );
            for x in 0..width_in_cells {
                let Some(x_slice) = image_cell_slice(
                    IMAGE_CELL_SPAN_COLUMNS,
                    x,
                    cell_pixel_width,
                    cell_padding_left as usize,
                    target_pixel_width,
                )? else {
                    continue;
                };
                let texture_left = normalized_texture_position(
                    IMAGE_CELL_SPAN_COLUMNS,
                    params.source_origin_x,
                    draw_width,
                    x_slice.content_start,
                    target_pixel_width,
                    params.image_width,
                )?;
                let texture_right = normalized_texture_position(
                    IMAGE_CELL_SPAN_COLUMNS,
                    params.source_origin_x,
                    draw_width,
                    x_slice.content_end,
                    target_pixel_width,
                    params.image_width,
                )?;
                log::debug!(
                    "texture x=[{}, {}], y=[{}, {}], padding_right={}, padding_bottom={}",
                    texture_left,
                    texture_right,
                    texture_top,
                    texture_bottom,
                    x_slice.padding_after,
                    y_slice.padding_after
                );
                let cell_x = cursor_x.saturating_add(x);
                let mut cell = self
                    .screen_mut()
                    .get_cell(cell_x, cursor_y)
                    .cloned()
                    .unwrap_or_else(Cell::blank);
                let img = Box::new(ImageCell::with_z_index(
                    TextureCoordinate::new_f32(texture_left, texture_top),
                    TextureCoordinate::new_f32(texture_right, texture_bottom),
                    params.data.clone(),
                    params.z_index,
                    checked_padding(IMAGE_CELL_SPAN_COLUMNS, x_slice.padding_before)?,
                    checked_padding(IMAGE_CELL_SPAN_ROWS, y_slice.padding_before)?,
                    checked_padding(IMAGE_CELL_SPAN_COLUMNS, x_slice.padding_after)?,
                    checked_padding(IMAGE_CELL_SPAN_ROWS, y_slice.padding_after)?,
                    params.image_id,
                    params.placement_id,
                ));
                match params.style {
                    ImageAttachStyle::Kitty => cell.attrs_mut().attach_image(img),
                    ImageAttachStyle::Sixel | ImageAttachStyle::Iterm => {
                        cell.attrs_mut().set_image(img)
                    }
                };

                self.screen_mut().set_cell(cell_x, cursor_y, &cell, seqno);
            }
            if !params.do_not_move_cursor && y < height_in_cells - 1 {
                self.new_line(false);
            }
        }

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
                    &Position::Relative(usize_to_i64_saturating(width_in_cells)),
                    &Position::Relative(0),
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

fn normalized_texture_position(
    axis: &str,
    source_origin: u32,
    draw_pixels: u32,
    consumed_target_pixels: usize,
    total_target_pixels: usize,
    image_pixels: u32,
) -> anyhow::Result<f32> {
    if total_target_pixels == 0 || image_pixels == 0 {
        anyhow::bail!("image placement {axis} texture normalization has a zero divisor");
    }
    if consumed_target_pixels > total_target_pixels {
        anyhow::bail!(
            "image placement {axis} consumed pixels {consumed_target_pixels} exceed target {total_target_pixels}"
        );
    }
    let source_end = source_origin.checked_add(draw_pixels).ok_or_else(|| {
        anyhow::anyhow!("image placement {axis} source endpoint overflowed")
    })?;
    if source_end > image_pixels {
        anyhow::bail!(
            "image placement {axis} source endpoint {source_end} exceeds image size {image_pixels}"
        );
    }

    // Derive each endpoint from integer progress rather than repeatedly adding
    // an f32 delta.  Cumulative addition can drift beyond 1.0 on the final
    // cell, which later peers correctly reject as malformed texture geometry.
    // Computing in f64 and handling the final endpoint explicitly also makes
    // adjacent cells share the exact same representable boundary.
    let source_position = if consumed_target_pixels == total_target_pixels {
        f64::from(source_end)
    } else {
        f64::from(source_origin)
            + f64::from(draw_pixels)
                * (consumed_target_pixels as f64 / total_target_pixels as f64)
    };
    let normalized = (source_position / f64::from(image_pixels)) as f32;
    Ok(normalized.clamp(0.0, 1.0))
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

fn checked_ceil_cell_count(
    axis: &str,
    pixel_extent: usize,
    cell_pixels: usize,
) -> anyhow::Result<usize> {
    if pixel_extent == 0 || cell_pixels == 0 {
        anyhow::bail!("image placement {axis} cell count has a zero extent");
    }
    let full_cells = pixel_extent / cell_pixels;
    full_cells
        .checked_add(usize::from(!pixel_extent.is_multiple_of(cell_pixels)))
        .ok_or_else(|| anyhow::anyhow!("image placement {axis} cell count overflows"))
}

fn remaining_rows_from_cursor(physical_rows: usize, cursor_y: i64) -> usize {
    let cursor_y = usize::try_from(cursor_y.max(0)).unwrap_or(usize::MAX);
    physical_rows.saturating_sub(cursor_y)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageCellSlice {
    content_start: usize,
    content_end: usize,
    padding_before: usize,
    padding_after: usize,
}

fn image_cell_slice(
    axis: &str,
    cell_index: usize,
    cell_pixels: usize,
    content_offset: usize,
    content_pixels: usize,
) -> anyhow::Result<Option<ImageCellSlice>> {
    if cell_pixels == 0 || content_pixels == 0 {
        anyhow::bail!("image placement {axis} cell slice has a zero extent");
    }
    let cell_start = cell_index.checked_mul(cell_pixels).ok_or_else(|| {
        anyhow::anyhow!("image placement {axis} cell offset overflows")
    })?;
    let cell_end = cell_start.checked_add(cell_pixels).ok_or_else(|| {
        anyhow::anyhow!("image placement {axis} cell endpoint overflows")
    })?;
    let content_end = content_offset.checked_add(content_pixels).ok_or_else(|| {
        anyhow::anyhow!("image placement {axis} content endpoint overflows")
    })?;
    let intersection_start = cell_start.max(content_offset);
    let intersection_end = cell_end.min(content_end);
    if intersection_start >= intersection_end {
        return Ok(None);
    }

    Ok(Some(ImageCellSlice {
        content_start: intersection_start.saturating_sub(content_offset),
        content_end: intersection_end.saturating_sub(content_offset),
        padding_before: intersection_start.saturating_sub(cell_start),
        padding_after: cell_end.saturating_sub(intersection_end),
    }))
}

fn checked_padding(axis: &str, pixels: usize) -> anyhow::Result<u16> {
    u16::try_from(pixels).with_context(|| {
        format!("image placement {axis} padding {pixels} pixels exceeds wire representation")
    })
}

fn usize_to_u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
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

    #[test]
    fn image_cell_padding_limit_saturates_for_wide_cells() {
        assert_eq!(usize_to_u16_saturating(0), 0);
        assert_eq!(usize_to_u16_saturating(u16::MAX as usize), u16::MAX);
        assert_eq!(usize_to_u16_saturating((u16::MAX as usize) + 1), u16::MAX);
    }

    #[test]
    fn image_cell_slices_apply_leading_and_trailing_padding_only_at_edges() {
        assert_eq!(
            image_cell_slice(IMAGE_CELL_SPAN_COLUMNS, 0, 8, 3, 8).unwrap(),
            Some(ImageCellSlice {
                content_start: 0,
                content_end: 5,
                padding_before: 3,
                padding_after: 0,
            })
        );
        assert_eq!(
            image_cell_slice(IMAGE_CELL_SPAN_COLUMNS, 1, 8, 3, 8).unwrap(),
            Some(ImageCellSlice {
                content_start: 5,
                content_end: 8,
                padding_before: 0,
                padding_after: 5,
            })
        );
        assert_eq!(
            image_cell_slice(IMAGE_CELL_SPAN_COLUMNS, 2, 8, 3, 8).unwrap(),
            None
        );
    }

    #[test]
    fn image_cell_count_and_remaining_rows_are_bounded() {
        assert_eq!(
            checked_ceil_cell_count(IMAGE_CELL_SPAN_COLUMNS, 17, 4).unwrap(),
            5
        );
        assert_eq!(
            checked_ceil_cell_count(IMAGE_CELL_SPAN_COLUMNS, usize::MAX, 1).unwrap(),
            usize::MAX
        );
        assert_eq!(remaining_rows_from_cursor(24, -1), 24);
        assert_eq!(remaining_rows_from_cursor(24, 10), 14);
        assert_eq!(remaining_rows_from_cursor(24, i64::MAX), 0);
    }

    fn sole_image_cell(
        terminal: &mut TerminalState,
        x: usize,
        y: i64,
    ) -> frankenterm_cell::image::ImageCell {
        terminal
            .screen_mut()
            .get_cell(x, y)
            .and_then(|cell| cell.attrs().images())
            .and_then(|mut images| images.pop())
            .expect("expected exactly one image on the requested cell")
    }

    #[test]
    fn kitty_offsets_expand_output_extent_and_only_pad_edge_cells() {
        let mut terminal = test_terminal_state();
        let mut params = test_image_attach_params();
        params.cell_padding_left = 3;
        params.cell_padding_top = 5;

        let info = terminal
            .assign_image_to_cells(params)
            .expect("offset Kitty placement should attach");
        assert_eq!((info.cols, info.rows), (2, 2));
        assert_eq!((terminal.cursor.x, terminal.cursor.y), (2, 1));

        let top_left = sole_image_cell(&mut terminal, 0, 0);
        let top_right = sole_image_cell(&mut terminal, 1, 0);
        let bottom_left = sole_image_cell(&mut terminal, 0, 1);
        let bottom_right = sole_image_cell(&mut terminal, 1, 1);
        assert_eq!(top_left.padding(), (3, 5, 0, 0));
        assert_eq!(top_right.padding(), (0, 5, 5, 0));
        assert_eq!(bottom_left.padding(), (3, 0, 0, 11));
        assert_eq!(bottom_right.padding(), (0, 0, 5, 11));
        assert_eq!(top_left.top_left(), TextureCoordinate::new_f32(0.0, 0.0));
        assert_eq!(
            top_left.bottom_right(),
            TextureCoordinate::new_f32(0.625, 0.6875)
        );
        assert_eq!(
            bottom_right.top_left(),
            TextureCoordinate::new_f32(0.625, 0.6875)
        );
        assert_eq!(
            bottom_right.bottom_right(),
            TextureCoordinate::new_f32(1.0, 1.0)
        );
    }

    #[test]
    fn explicit_downscale_without_padding_does_not_invent_cursor_spill() {
        let mut terminal = test_terminal_state();
        let mut params = test_image_attach_params();
        params.image_width = 100;
        params.image_height = 100;
        params.source_width = Some(100);
        params.source_height = Some(100);

        let info = terminal
            .assign_image_to_cells(params)
            .expect("explicitly downscaled placement should attach");
        assert_eq!((info.cols, info.rows), (1, 1));
        assert_eq!((terminal.cursor.x, terminal.cursor.y), (1, 0));
        let image = sole_image_cell(&mut terminal, 0, 0);
        assert_eq!(image.padding(), (0, 0, 0, 0));
        assert_eq!(image.top_left(), TextureCoordinate::new_f32(0.0, 0.0));
        assert_eq!(image.bottom_right(), TextureCoordinate::new_f32(1.0, 1.0));
    }

    #[test]
    fn implicit_skinny_images_are_clipped_to_bounded_terminal_work() {
        let mut wide_terminal = test_terminal_state();
        let mut wide = test_image_attach_params();
        wide.image_width = 25_000_000;
        wide.image_height = 1;
        wide.source_width = Some(25_000_000);
        wide.source_height = Some(1);
        wide.columns = None;
        wide.rows = None;
        let wide_info = wide_terminal
            .assign_image_to_cells(wide)
            .expect("wide placement should be safely clipped");
        assert_eq!(wide_info.cols, 80);
        assert_eq!(wide_info.rows, 1);
        assert_eq!(wide_terminal.cursor.x, 79);

        let mut tall_terminal = test_terminal_state();
        let mut tall = test_image_attach_params();
        tall.image_width = 1;
        tall.image_height = 25_000_000;
        tall.source_width = Some(1);
        tall.source_height = Some(25_000_000);
        tall.columns = None;
        tall.rows = None;
        let tall_info = tall_terminal
            .assign_image_to_cells(tall)
            .expect("tall placement should retain only bounded final rows");
        assert_eq!(tall_info.cols, 1);
        assert_eq!(tall_info.rows, 24);
        assert_eq!(tall_terminal.cursor.y, 23);
    }

    #[test]
    fn horizontally_clipped_image_cannot_scroll_without_attaching_cells() {
        let mut terminal = test_terminal_state();
        terminal.cursor.x = terminal.screen().physical_cols;
        terminal.cursor.y = 7;
        let before_cursor = (terminal.cursor.x, terminal.cursor.y);

        let info = terminal
            .assign_image_to_cells(test_image_attach_params())
            .expect("fully clipped placement should be a benign no-op");

        assert_eq!((info.cols, info.rows), (0, 0));
        assert_eq!((terminal.cursor.x, terminal.cursor.y), before_cursor);
    }

    #[test]
    fn iterm_explicit_cell_dimensions_reject_extreme_spans_without_overflow() {
        let mut terminal = test_terminal_state();

        for (axis, columns, rows) in [
            (IMAGE_CELL_SPAN_COLUMNS, Some(usize::MAX), Some(1)),
            (IMAGE_CELL_SPAN_ROWS, Some(1), Some(usize::MAX)),
        ] {
            let mut params = test_image_attach_params();
            params.style = ImageAttachStyle::Iterm;
            params.columns = columns;
            params.rows = rows;

            let err = terminal.assign_image_to_cells(params).unwrap_err();
            assert!(
                err.to_string().contains(axis),
                "expected {axis} in overflow-safe rejection, got {err}",
                axis = axis,
                err = err,
            );
        }
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

}
