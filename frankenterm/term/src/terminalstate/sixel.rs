use crate::TerminalState;
use crate::terminalstate::image::*;
use crate::terminalstate::{ImageAttachParams, default_color_map};
use ::image::RgbaImage;
use frankenterm_cell::color::RgbColor;
use frankenterm_cell::image::ImageDataType;
use frankenterm_escape_parser::{Sixel, SixelData};

fn sixel_pixel_row(y: u32, bitno: u32, height: u32) -> Option<u32> {
    y.checked_add(bitno).filter(|row| *row < height)
}

impl TerminalState {
    pub(crate) fn sixel(&mut self, sixel: Box<Sixel>) {
        let (width, height) = sixel.dimensions();

        if let Err(err) = check_image_dimensions(width, height) {
            log::error!("{}", err);
            return;
        }

        let mut private_color_map;
        let color_map = if self.use_private_color_registers_for_each_graphic {
            private_color_map = default_color_map();
            &mut private_color_map
        } else {
            &mut self.color_map
        };

        let mut image = if sixel.background_is_transparent {
            RgbaImage::new(width, height)
        } else {
            let background_color = color_map
                .get(&0)
                .cloned()
                .unwrap_or(RgbColor::new_8bpc(0, 0, 0));
            let (red, green, blue) = background_color.to_tuple_rgb8();
            RgbaImage::from_pixel(width, height, [red, green, blue, 0xffu8].into())
        };

        let mut x = 0;
        let mut y = 0;
        let mut foreground_color = RgbColor::new_8bpc(0, 0xff, 0);

        let mut emit_sixel = |d: &u8, foreground_color: &RgbColor, x: u32, y: u32| {
            if x >= width {
                return;
            }
            let (red, green, blue) = foreground_color.to_tuple_rgb8();
            for bitno in 0..6 {
                let Some(row) = sixel_pixel_row(y, bitno, height) else {
                    break;
                };
                let on = (d & (1 << bitno)) != 0;
                if on {
                    image.get_pixel_mut(x, row).0 = [red, green, blue, 0xffu8];
                }
            }
        };

        for d in &sixel.data {
            match d {
                SixelData::Data(d) => {
                    emit_sixel(d, &foreground_color, x, y);
                    x = x.saturating_add(1);
                }

                SixelData::Repeat { repeat_count, data } => {
                    for _ in 0..*repeat_count {
                        emit_sixel(data, &foreground_color, x, y);
                        x = x.saturating_add(1);
                    }
                }

                SixelData::CarriageReturn => x = 0,
                SixelData::NewLine => {
                    x = 0;
                    y = y.saturating_add(6);
                }

                SixelData::DefineColorMapRGB { color_number, rgb } => {
                    if color_map.len() >= self.config.max_color_map_entries()
                        && !color_map.contains_key(color_number)
                    {
                        // Evict an arbitrary entry to stay within bounds
                        if let Some(&key) = color_map.keys().next() {
                            color_map.remove(&key);
                        }
                    }
                    color_map.insert(*color_number, *rgb);
                }

                SixelData::DefineColorMapHSL {
                    color_number,
                    hue_angle,
                    saturation,
                    lightness,
                } => {
                    // Sixel's hue angles are: blue=0, red=120, green=240,
                    // whereas Hsl has red=0, green=120, blue=240.
                    // Looking at red, we need to rotate left by 120 to
                    // go from sixel red to standard hsl red.
                    // Negative values wrap around the circle.
                    // https://github.com/wezterm/wezterm/issues/775
                    let angle = (*hue_angle as f64) - 120.0;
                    let angle = if angle < 0. { 360.0 + angle } else { angle };
                    let c = csscolorparser::Color::from_hsla(
                        angle,
                        *saturation as f64 / 100.,
                        *lightness as f64 / 100.,
                        1.,
                    );
                    let [r, g, b, _] = c.to_rgba8();
                    if color_map.len() >= self.config.max_color_map_entries()
                        && !color_map.contains_key(color_number)
                    {
                        if let Some(&key) = color_map.keys().next() {
                            color_map.remove(&key);
                        }
                    }
                    color_map.insert(*color_number, RgbColor::new_8bpc(r, g, b));
                }

                SixelData::SelectColorMapEntry(n) => {
                    foreground_color = color_map.get(n).cloned().unwrap_or_else(|| {
                        log::error!("sixel selected noexistent colormap entry {}", n);
                        RgbColor::new_8bpc(255, 255, 255)
                    });
                }
            }
        }

        let data = image.into_vec();
        let image_data = ImageDataType::new_single_frame(width, height, data);

        let image_data = match self.raw_image_to_image_data(image_data) {
            Ok(d) => d,
            Err(err) => {
                log::error!("error while processing sixel image: {err:#}");
                return;
            }
        };
        let old_cursor = self.cursor;
        if self.sixel_display_mode {
            // Sixel Display Mode (DECSDM) requires placing the image
            // at the top-left corner, but not moving the text cursor
            // position.
            self.cursor.x = 0;
            self.cursor.y = 0;
        }
        if let Err(err) = self.assign_image_to_cells(ImageAttachParams {
            image_width: width,
            image_height: height,
            source_width: None,
            source_height: None,
            rows: None,
            columns: None,
            source_origin_x: 0,
            source_origin_y: 0,
            cell_padding_left: 0,
            cell_padding_top: 0,
            data: image_data,
            style: ImageAttachStyle::Sixel,
            z_index: 0,
            image_id: None,
            placement_id: None,
            do_not_move_cursor: self.sixel_display_mode,
        }) {
            log::error!("set sixel image: {:#}", err);
        }
        if self.sixel_display_mode {
            self.cursor = old_cursor;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixel_pixel_row_stops_at_image_height() {
        assert_eq!(sixel_pixel_row(8, 0, 10), Some(8));
        assert_eq!(sixel_pixel_row(8, 1, 10), Some(9));
        assert_eq!(sixel_pixel_row(8, 2, 10), None);
    }

    #[test]
    fn sixel_pixel_row_rejects_overflowing_rows() {
        assert_eq!(sixel_pixel_row(u32::MAX, 0, u32::MAX), None);
        assert_eq!(sixel_pixel_row(u32::MAX, 1, u32::MAX), None);
    }
}
