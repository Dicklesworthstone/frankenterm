//! Rendering of Changes using terminfo
use crate::caps::{Capabilities, ColorLevel};
use crate::cell::{AttributeChange, Blink, CellAttributes, Intensity, Underline};
use crate::color::{ColorAttribute, ColorSpec};
use crate::escape::csi::{Cursor, Edit, EraseInDisplay, EraseInLine, Sgr, CSI};
use crate::escape::esc::EscCode;
use crate::escape::osc::OperatingSystemCommand;
#[cfg(feature = "use_image")]
use crate::escape::osc::{ITermDimension, ITermFileData, ITermProprietary};
use crate::escape::{Esc, OneBased};
#[cfg(feature = "use_image")]
use crate::image::{ImageData, ImageDataType, TextureCoordinate};
use crate::render::RenderTty;
use crate::surface::{Change, CursorShape, CursorVisibility, LineAttribute, Position};
use crate::Result;
use finl_unicode::grapheme_clusters::Graphemes;
#[cfg(feature = "use_image")]
use image::codecs::gif::GifEncoder;
#[cfg(feature = "use_image")]
use image::codecs::png::PngEncoder;
#[cfg(feature = "use_image")]
use image::{
    Delay, ExtendedColorType, Frame, GenericImage, GenericImageView, ImageBuffer, ImageEncoder,
    Rgba, RgbaImage,
};
use std::io::Write;
use terminfo::{capability as cap, Capability as TermInfoCapability};

#[cfg(feature = "use_image")]
fn is_full_image_region(top_left: TextureCoordinate, bottom_right: TextureCoordinate) -> bool {
    top_left == TextureCoordinate::new_f32(0.0, 0.0)
        && bottom_right == TextureCoordinate::new_f32(1.0, 1.0)
}

#[cfg(feature = "use_image")]
fn texture_axis_to_pixel_bounds(start: f32, end: f32, dimension: u32) -> Result<(u32, u32)> {
    crate::ensure!(
        (0.0..=1.0).contains(&start) && (0.0..=1.0).contains(&end),
        "image texture coordinates must be within 0..=1, got {start}..{end}"
    );
    crate::ensure!(
        matches!(start.partial_cmp(&end), Some(std::cmp::Ordering::Less)),
        "image texture coordinates must define a non-empty region, got {start}..{end}"
    );
    crate::ensure!(
        dimension > 0,
        "image dimension must be positive when computing crop bounds"
    );

    let start_px =
        ((start * dimension as f32).floor() as i64).clamp(0, i64::from(dimension)) as u32;
    let raw_end_px = ((end * dimension as f32).ceil() as i64).clamp(0, i64::from(dimension)) as u32;
    let end_px = raw_end_px.max(start_px.saturating_add(1)).min(dimension);

    crate::ensure!(
        end_px > start_px,
        "image crop bounds resolved to an empty slice ({start_px}..{end_px})"
    );

    Ok((start_px, end_px))
}

#[cfg(feature = "use_image")]
fn crop_rgba_region(
    width: u32,
    height: u32,
    data: &[u8],
    top_left: TextureCoordinate,
    bottom_right: TextureCoordinate,
) -> Result<RgbaImage> {
    let source = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, data.to_vec())
        .ok_or_else(|| crate::error::StringWrap("ill formed RGBA image".to_string()))?;

    let (left, right) =
        texture_axis_to_pixel_bounds(top_left.x.into_inner(), bottom_right.x.into_inner(), width)?;
    let (top, bottom) =
        texture_axis_to_pixel_bounds(top_left.y.into_inner(), bottom_right.y.into_inner(), height)?;

    let view = source.view(left, top, right - left, bottom - top);
    let mut cropped = RgbaImage::new(right - left, bottom - top);
    cropped
        .copy_from(&*view, 0, 0)
        .map_err(anyhow::Error::from)?;
    Ok(cropped)
}

#[cfg(feature = "use_image")]
fn encode_png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut encoded = Vec::new();
    let encoder = PngEncoder::new(&mut encoded);
    encoder
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(anyhow::Error::from)?;
    Ok(encoded)
}

#[cfg(feature = "use_image")]
fn encode_gif(frames: Vec<Frame>) -> Result<Vec<u8>> {
    let mut encoded = Vec::new();
    {
        let mut encoder = GifEncoder::new(&mut encoded);
        encoder.encode_frames(frames).map_err(anyhow::Error::from)?;
    }
    Ok(encoded)
}

#[cfg(feature = "use_image")]
fn encode_iterm_inline_image(
    image: &crate::surface::Image,
    top_left: TextureCoordinate,
    bottom_right: TextureCoordinate,
) -> Result<Vec<u8>> {
    let full_region = is_full_image_region(top_left, bottom_right);
    let image_data = image.image.data().clone();

    match image_data {
        ImageDataType::EncodedFile(data) if full_region => Ok(data),
        ImageDataType::EncodedLease(lease) if full_region => Ok(lease.get_data()?),
        ImageDataType::EncodedFile(data) => match ImageDataType::EncodedFile(data).decode() {
            ImageDataType::EncodedFile(_) | ImageDataType::EncodedLease(_) => {
                crate::bail!("cannot crop encoded image data unless the format is decodable")
            }
            decoded => encode_iterm_inline_image(
                &crate::surface::Image {
                    image: std::sync::Arc::new(ImageData::with_data(decoded)),
                    ..image.clone()
                },
                top_left,
                bottom_right,
            ),
        },
        ImageDataType::EncodedLease(lease) => {
            match ImageDataType::EncodedFile(lease.get_data()?).decode() {
                ImageDataType::EncodedFile(_) | ImageDataType::EncodedLease(_) => {
                    crate::bail!("cannot crop encoded image data unless the format is decodable")
                }
                decoded => encode_iterm_inline_image(
                    &crate::surface::Image {
                        image: std::sync::Arc::new(ImageData::with_data(decoded)),
                        ..image.clone()
                    },
                    top_left,
                    bottom_right,
                ),
            }
        }
        ImageDataType::Rgba8 {
            width,
            height,
            data,
            ..
        } => encode_png(&crop_rgba_region(
            width,
            height,
            &data,
            top_left,
            bottom_right,
        )?),
        ImageDataType::AnimRgba8 {
            width,
            height,
            durations,
            frames,
            ..
        } => {
            let mut encoded_frames = Vec::with_capacity(frames.len());
            for (frame_data, duration) in frames.into_iter().zip(durations) {
                let frame = crop_rgba_region(width, height, &frame_data, top_left, bottom_right)?;
                encoded_frames.push(Frame::from_parts(
                    frame,
                    0,
                    0,
                    Delay::from_saturating_duration(duration),
                ));
            }
            encode_gif(encoded_frames)
        }
    }
}

pub struct TerminfoRenderer {
    caps: Capabilities,
    current_attr: CellAttributes,
    pending_attr: Option<CellAttributes>,
    cursor_position: Option<(usize, usize)>,
    /* TODO: we should record cursor position, shape and color here
     * so that we can optimize updating them on screen. */
}

impl TerminfoRenderer {
    pub fn new(caps: Capabilities) -> Self {
        Self {
            caps,
            current_attr: CellAttributes::default(),
            pending_attr: None,
            cursor_position: Some((0, 0)),
        }
    }

    fn get_capability<'a, T: TermInfoCapability<'a>>(&'a self) -> Option<T> {
        self.caps.terminfo_db().and_then(|db| db.get::<T>())
    }

    fn attr_apply<F: FnOnce(&mut CellAttributes)>(&mut self, func: F) {
        self.pending_attr = Some(match self.pending_attr.take() {
            Some(mut attr) => {
                func(&mut attr);
                attr
            }
            None => {
                let mut attr = self.current_attr.clone();
                func(&mut attr);
                attr
            }
        });
    }

    #[allow(clippy::cognitive_complexity)]
    fn flush_pending_attr<W: RenderTty + Write>(&mut self, out: &mut W) -> Result<()> {
        macro_rules! attr_on {
            ($cap:ident, $sgr:expr) => {{
                let cap = if self.caps.force_terminfo_render_to_use_ansi_sgr() {
                    None
                } else {
                    self.get_capability::<cap::$cap>()
                };
                if let Some(attr) = cap {
                    attr.expand().to(out.by_ref())?;
                } else {
                    write!(out, "{}", CSI::Sgr($sgr))?;
                }
            }};
            ($sgr:expr) => {
                write!(out, "{}", CSI::Sgr($sgr))?;
            };
        }

        if let Some(attr) = self.pending_attr.take() {
            let mut current_foreground = self.current_attr.foreground();
            let mut current_background = self.current_attr.background();

            if !attr.attribute_bits_equal(&self.current_attr) {
                // Updating the attribute bits also resets the colors.
                current_foreground = ColorAttribute::Default;
                current_background = ColorAttribute::Default;

                let sgr = if self.caps.force_terminfo_render_to_use_ansi_sgr() {
                    None
                } else {
                    self.get_capability::<cap::SetAttributes>()
                };
                // The SetAttributes capability can only handle single underline and slow blink.
                if let Some(sgr) = sgr {
                    sgr.expand()
                        .bold(attr.intensity() == Intensity::Bold)
                        .dim(attr.intensity() == Intensity::Half)
                        .underline(attr.underline() == Underline::Single)
                        .blink(attr.blink() == Blink::Slow)
                        .reverse(attr.reverse())
                        .invisible(attr.invisible())
                        .to(out.by_ref())?;
                } else {
                    attr_on!(ExitAttributeMode, Sgr::Reset);

                    match attr.intensity() {
                        Intensity::Bold => attr_on!(EnterBoldMode, Sgr::Intensity(Intensity::Bold)),
                        Intensity::Half => attr_on!(EnterDimMode, Sgr::Intensity(Intensity::Half)),
                        _ => {}
                    }

                    if attr.underline() == Underline::Single {
                        attr_on!(Sgr::Underline(Underline::Single));
                    }

                    if attr.blink() == Blink::Slow {
                        attr_on!(Sgr::Blink(Blink::Slow));
                    }

                    if attr.reverse() {
                        attr_on!(EnterReverseMode, Sgr::Inverse(true));
                    }

                    if attr.invisible() {
                        attr_on!(Sgr::Invisible(true));
                    }
                }

                if attr.underline() == Underline::Double {
                    attr_on!(Sgr::Underline(Underline::Double));
                }

                if attr.blink() == Blink::Rapid {
                    attr_on!(Sgr::Blink(Blink::Rapid));
                }

                if attr.italic() {
                    attr_on!(EnterItalicsMode, Sgr::Italic(true));
                }

                if attr.strikethrough() {
                    attr_on!(Sgr::StrikeThrough(true));
                }
            }

            let has_true_color = self.caps.color_level() == ColorLevel::TrueColor;
            // Whether to use terminfo to render 256 colors. If this is too large (ex. 16777216 from xterm-direct),
            // then setaf expects the index to be true color, in which case we cannot use it to render 256 (or even 16) colors.
            let terminfo_256_color: i32 = match self.get_capability::<cap::MaxColors>() {
                Some(cap::MaxColors(n)) => {
                    if n > 256 {
                        0
                    } else {
                        n
                    }
                }
                None => 0,
            };

            if attr.foreground() != current_foreground
                && self.caps.color_level() != ColorLevel::MonoChrome
            {
                match (has_true_color, attr.foreground()) {
                    (true, ColorAttribute::TrueColorWithPaletteFallback(tc, _))
                    | (true, ColorAttribute::TrueColorWithDefaultFallback(tc)) => {
                        write!(
                            out,
                            "{}",
                            CSI::Sgr(Sgr::Foreground(ColorSpec::TrueColor(tc)))
                        )?;
                    }
                    (false, ColorAttribute::TrueColorWithDefaultFallback(_))
                    | (_, ColorAttribute::Default) => {
                        // Terminfo doesn't define a reset color to default, so
                        // we use the ANSI code.
                        write!(out, "{}", CSI::Sgr(Sgr::Foreground(ColorSpec::Default)))?;
                    }
                    (false, ColorAttribute::TrueColorWithPaletteFallback(_, idx))
                    | (_, ColorAttribute::PaletteIndex(idx)) => {
                        match self.get_capability::<cap::SetAForeground>() {
                            Some(set) if (idx as i32) < terminfo_256_color => {
                                set.expand().color(idx).to(out.by_ref())?;
                            }
                            _ => {
                                write!(
                                    out,
                                    "{}",
                                    CSI::Sgr(Sgr::Foreground(ColorSpec::PaletteIndex(idx)))
                                )?;
                            }
                        }
                    }
                }
            }

            if attr.background() != current_background
                && self.caps.color_level() != ColorLevel::MonoChrome
            {
                match (has_true_color, attr.background()) {
                    (true, ColorAttribute::TrueColorWithPaletteFallback(tc, _))
                    | (true, ColorAttribute::TrueColorWithDefaultFallback(tc)) => {
                        write!(
                            out,
                            "{}",
                            CSI::Sgr(Sgr::Background(ColorSpec::TrueColor(tc)))
                        )?;
                    }
                    (false, ColorAttribute::TrueColorWithDefaultFallback(_))
                    | (_, ColorAttribute::Default) => {
                        // Terminfo doesn't define a reset color to default, so
                        // we use the ANSI code.
                        write!(out, "{}", CSI::Sgr(Sgr::Background(ColorSpec::Default)))?;
                    }
                    (false, ColorAttribute::TrueColorWithPaletteFallback(_, idx))
                    | (_, ColorAttribute::PaletteIndex(idx)) => {
                        match self.get_capability::<cap::SetABackground>() {
                            Some(set) if (idx as i32) < terminfo_256_color => {
                                set.expand().color(idx).to(out.by_ref())?;
                            }
                            _ => {
                                write!(
                                    out,
                                    "{}",
                                    CSI::Sgr(Sgr::Background(ColorSpec::PaletteIndex(idx)))
                                )?;
                            }
                        }
                    }
                }
            }

            if self.caps.hyperlinks() {
                if let Some(link) = attr.hyperlink() {
                    let osc = OperatingSystemCommand::SetHyperlink(Some((**link).clone()));
                    write!(out, "{}", osc)?;
                } else if self.current_attr.hyperlink().is_some() {
                    // Close out the old hyperlink
                    let osc = OperatingSystemCommand::SetHyperlink(None);
                    write!(out, "{}", osc)?;
                }
            }

            self.current_attr = attr;
        }

        Ok(())
    }

    fn cursor_up<W: RenderTty + Write>(&mut self, n: u32, out: &mut W) -> Result<()> {
        if n > 0 {
            if let Some(attr) = self.get_capability::<cap::ParmUpCursor>() {
                attr.expand().count(n).to(out.by_ref())?;
            } else {
                write!(out, "{}", CSI::Cursor(Cursor::Up(n)))?;
            }
        }
        Ok(())
    }

    fn cursor_down<W: RenderTty + Write>(&mut self, n: u32, out: &mut W) -> Result<()> {
        if n > 0 {
            if let Some(attr) = self.get_capability::<cap::ParmDownCursor>() {
                attr.expand().count(n).to(out.by_ref())?;
            } else {
                write!(out, "{}", CSI::Cursor(Cursor::Down(n)))?;
            }
        }
        Ok(())
    }

    fn cursor_y_relative<W: RenderTty + Write>(&mut self, y: isize, out: &mut W) -> Result<()> {
        if y > 0 {
            self.cursor_down(y as u32, out)
        } else {
            self.cursor_up(-y as u32, out)
        }
    }

    fn cursor_left<W: RenderTty + Write>(&mut self, n: u32, out: &mut W) -> Result<()> {
        if n > 0 {
            if let Some(attr) = self.get_capability::<cap::ParmLeftCursor>() {
                attr.expand().count(n).to(out.by_ref())?;
            } else {
                write!(out, "{}", CSI::Cursor(Cursor::Left(n)))?;
            }
        }
        Ok(())
    }

    fn cursor_right<W: RenderTty + Write>(&mut self, n: u32, out: &mut W) -> Result<()> {
        if n > 0 {
            if let Some(attr) = self.get_capability::<cap::ParmRightCursor>() {
                attr.expand().count(n).to(out.by_ref())?;
            } else {
                write!(out, "{}", CSI::Cursor(Cursor::Right(n)))?;
            }
        }
        Ok(())
    }

    fn cursor_x_relative<W: RenderTty + Write>(&mut self, x: isize, out: &mut W) -> Result<()> {
        if x > 0 {
            self.cursor_right(x as u32, out)
        } else {
            self.cursor_left(-x as u32, out)
        }
    }

    fn move_cursor_absolute<W: RenderTty + Write>(
        &mut self,
        x: u32,
        y: u32,
        out: &mut W,
    ) -> Result<()> {
        if x == 0 && y == 0 {
            if let Some(attr) = self.get_capability::<cap::CursorHome>() {
                attr.expand().to(out.by_ref())?;
                self.cursor_position = Some((0, 0));
                return Ok(());
            }
        }

        if let Some(attr) = self.get_capability::<cap::CursorAddress>() {
            // terminfo expansion automatically converts coordinates to 1-based,
            // so we can pass in the 0-based coordinates as-is
            attr.expand().x(x).y(y).to(out.by_ref())?;
        } else {
            // We need to manually convert to 1-based as the CSI representation
            // requires it and there's no automatic conversion.
            write!(
                out,
                "{}",
                CSI::Cursor(Cursor::Position {
                    line: OneBased::from_zero_based(x),
                    col: OneBased::from_zero_based(y),
                })
            )?;
        }
        self.cursor_position = Some((x as usize, y as usize));
        Ok(())
    }

    fn write_spaces<W: Write>(&self, count: usize, out: &mut W) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        let mut buf = Vec::with_capacity(count);
        buf.resize(count, b' ');
        out.write_all(buf.as_slice())?;
        Ok(())
    }

    fn update_cursor_position(&mut self, x: &Position, y: &Position, cols: usize, rows: usize) {
        let Some((cursor_x, cursor_y)) = self.cursor_position else {
            return;
        };

        let screen_cols = cols.max(1);
        let screen_rows = rows.max(1);

        let next_x = match x {
            Position::Relative(dx) => (cursor_x as isize + dx).rem_euclid(screen_cols as isize),
            Position::Absolute(value) => (value % screen_cols) as isize,
            Position::EndRelative(value) => ((screen_cols - value) % screen_cols) as isize,
        };

        let next_y = match y {
            Position::Relative(dy) => (cursor_y as isize + dy).rem_euclid(screen_rows as isize),
            Position::Absolute(value) => (value % screen_rows) as isize,
            Position::EndRelative(value) => ((screen_rows - value) % screen_rows) as isize,
        };

        self.cursor_position = Some((next_x as usize, next_y as usize));
    }

    fn update_cursor_position_for_text(&mut self, text: &str, cols: usize) {
        let Some((mut cursor_x, mut cursor_y)) = self.cursor_position else {
            return;
        };

        let screen_cols = cols.max(1);

        for grapheme in Graphemes::new(text) {
            if cursor_x == screen_cols {
                cursor_y = cursor_y.saturating_add(1);
                cursor_x = 0;
            }

            match grapheme {
                "\n" => {
                    cursor_y = cursor_y.saturating_add(1);
                }
                "\r" => {
                    cursor_x = 0;
                }
                "\r\n" => {
                    cursor_y = cursor_y.saturating_add(1);
                    cursor_x = 0;
                }
                _ => {
                    cursor_x =
                        cursor_x.saturating_add(crate::cell::unicode_column_width(grapheme, None));
                }
            }
        }

        self.cursor_position = Some((cursor_x, cursor_y));
    }

    #[allow(clippy::cognitive_complexity)]
    pub fn render_to<W: RenderTty + Write>(
        &mut self,
        changes: &[Change],
        out: &mut W,
    ) -> Result<()> {
        macro_rules! record {
            ($accesor:ident, $value:expr) => {
                self.attr_apply(|attr| {
                    attr.$accesor(*$value);
                });
            };
        }

        for change in changes {
            match change {
                Change::ClearScreen(color) => {
                    // ClearScreen implicitly resets all to default
                    let defaults = CellAttributes::default().set_background(*color).clone();
                    if self.current_attr != defaults {
                        self.pending_attr = Some(defaults);
                        self.flush_pending_attr(out)?;
                    }
                    self.pending_attr = None;

                    if self.current_attr.background() == ColorAttribute::Default || self.caps.bce()
                    {
                        // The erase operation respects "background color erase",
                        // or we're clearing to the default background color, so we can
                        // simply emit a clear screen op.
                        if let Some(clr) = self.get_capability::<cap::ClearScreen>() {
                            clr.expand().to(out.by_ref())?;
                        } else {
                            self.move_cursor_absolute(0, 0, out)?;
                            write!(
                                out,
                                "{}",
                                CSI::Edit(Edit::EraseInDisplay(EraseInDisplay::EraseDisplay))
                            )?;
                        }
                    } else {
                        // We're setting the background to a specific color, so we get to
                        // paint the whole thing.
                        self.move_cursor_absolute(0, 0, out)?;

                        let (cols, rows) = out.get_size_in_cells()?;
                        let num_spaces = cols * rows;
                        self.write_spaces(num_spaces, out)?;
                        self.move_cursor_absolute(0, 0, out)?;
                    }
                    self.cursor_position = Some((0, 0));
                }
                Change::ClearToEndOfLine(color) => {
                    // ClearScreen implicitly resets all to default
                    let defaults = CellAttributes::default().set_background(*color).clone();
                    if self.current_attr != defaults {
                        self.pending_attr = Some(defaults);
                        self.flush_pending_attr(out)?;
                    }
                    self.pending_attr = None;

                    if color != &ColorAttribute::Default && !self.caps.bce() {
                        if let Some((cursor_x, _cursor_y)) = self.cursor_position {
                            let (cols, _rows) = out.get_size_in_cells()?;
                            self.write_spaces(cols.saturating_sub(cursor_x), out)?;
                            out.by_ref().write_all(b"\r")?;
                            self.cursor_right(cursor_x as u32, out)?;
                        } else if let Some(clr) = self.get_capability::<cap::ClrEol>() {
                            clr.expand().to(out.by_ref())?;
                        } else {
                            write!(
                                out,
                                "{}",
                                CSI::Edit(Edit::EraseInLine(EraseInLine::EraseToEndOfLine))
                            )?;
                        }
                    } else if let Some(clr) = self.get_capability::<cap::ClrEol>() {
                        clr.expand().to(out.by_ref())?;
                    } else {
                        write!(
                            out,
                            "{}",
                            CSI::Edit(Edit::EraseInLine(EraseInLine::EraseToEndOfLine))
                        )?;
                    }
                }
                Change::ClearToEndOfScreen(color) => {
                    // ClearScreen implicitly resets all to default
                    let defaults = CellAttributes::default().set_background(*color).clone();
                    if self.current_attr != defaults {
                        self.pending_attr = Some(defaults);
                        self.flush_pending_attr(out)?;
                    }
                    self.pending_attr = None;

                    if color != &ColorAttribute::Default && !self.caps.bce() {
                        if let Some((cursor_x, cursor_y)) = self.cursor_position {
                            let (cols, rows) = out.get_size_in_cells()?;
                            let rows_below = rows.saturating_sub(cursor_y.saturating_add(1));
                            self.write_spaces(cols.saturating_sub(cursor_x), out)?;
                            for _ in 0..rows_below {
                                out.by_ref().write_all(b"\r\n")?;
                                self.write_spaces(cols, out)?;
                            }
                            self.cursor_up(rows_below as u32, out)?;
                            out.by_ref().write_all(b"\r")?;
                            self.cursor_right(cursor_x as u32, out)?;
                        } else if let Some(clr) = self.get_capability::<cap::ClrEos>() {
                            clr.expand().to(out.by_ref())?;
                        } else {
                            write!(
                                out,
                                "{}",
                                CSI::Edit(Edit::EraseInDisplay(
                                    EraseInDisplay::EraseToEndOfDisplay
                                ))
                            )?;
                        }
                    } else if let Some(clr) = self.get_capability::<cap::ClrEos>() {
                        clr.expand().to(out.by_ref())?;
                    } else {
                        write!(
                            out,
                            "{}",
                            CSI::Edit(Edit::EraseInDisplay(EraseInDisplay::EraseToEndOfDisplay))
                        )?;
                    }
                }
                Change::Attribute(AttributeChange::Intensity(value)) => {
                    record!(set_intensity, value);
                }
                Change::Attribute(AttributeChange::Italic(value)) => {
                    record!(set_italic, value);
                }
                Change::Attribute(AttributeChange::Reverse(value)) => {
                    record!(set_reverse, value);
                }
                Change::Attribute(AttributeChange::StrikeThrough(value)) => {
                    record!(set_strikethrough, value);
                }
                Change::Attribute(AttributeChange::Blink(value)) => {
                    record!(set_blink, value);
                }
                Change::Attribute(AttributeChange::Invisible(value)) => {
                    record!(set_invisible, value);
                }
                Change::Attribute(AttributeChange::Underline(value)) => {
                    record!(set_underline, value);
                }
                Change::Attribute(AttributeChange::Foreground(col)) => {
                    self.attr_apply(|attr| {
                        attr.set_foreground(*col);
                    });
                }
                Change::Attribute(AttributeChange::Background(col)) => {
                    self.attr_apply(|attr| {
                        attr.set_background(*col);
                    });
                }
                Change::Attribute(AttributeChange::Hyperlink(link)) => {
                    self.attr_apply(|attr| {
                        attr.set_hyperlink(link.clone());
                    });
                }
                Change::AllAttributes(all) => {
                    self.pending_attr = Some(all.clone());
                }
                Change::Text(text) => {
                    self.flush_pending_attr(out)?;
                    out.by_ref().write_all(text.as_bytes())?;
                    let (cols, _rows) = out.get_size_in_cells()?;
                    self.update_cursor_position_for_text(text, cols);
                }

                Change::CursorPosition { x, y } => {
                    // Note: we use `cursor_up(screen_height)` to move the cursor all the way to
                    // the top of the screen when we need to absolutely position only y.

                    match (x, y) {
                        (Position::Relative(0), Position::Relative(0)) => {}
                        (Position::Absolute(0), Position::Relative(1)) => {
                            out.by_ref().write_all(b"\r\n")?;
                        }
                        (Position::Absolute(x), Position::Relative(y)) => {
                            self.cursor_y_relative(*y, out)?;
                            out.by_ref().write_all(b"\r")?;
                            self.cursor_right(*x as u32, out)?;
                        }

                        (Position::Relative(x), Position::EndRelative(y)) => {
                            let (_cols, rows) = out.get_size_in_cells()?;
                            self.cursor_up(rows as u32, out)?;
                            self.cursor_down(rows.saturating_sub(y + 1) as u32, out)?;
                            self.cursor_x_relative(*x, out)?;
                        }
                        (Position::Relative(x), Position::Relative(y)) => {
                            self.cursor_y_relative(*y, out)?;
                            self.cursor_x_relative(*x, out)?;
                        }
                        (Position::EndRelative(x), Position::Relative(y)) => {
                            self.cursor_y_relative(*y, out)?;
                            let (cols, _rows) = out.get_size_in_cells()?;
                            out.by_ref().write_all(b"\r")?;
                            self.cursor_right(cols.saturating_sub(x + 1) as u32, out)?;
                        }

                        (Position::Absolute(x), Position::Absolute(y)) => {
                            self.move_cursor_absolute(*x as u32, *y as u32, out)?;
                        }
                        (Position::Absolute(x), Position::EndRelative(y)) => {
                            let (_cols, rows) = out.get_size_in_cells()?;
                            self.move_cursor_absolute(
                                *x as u32,
                                rows.saturating_sub(y + 1) as u32,
                                out,
                            )?;
                        }
                        (Position::EndRelative(x), Position::EndRelative(y)) => {
                            let (cols, rows) = out.get_size_in_cells()?;
                            self.move_cursor_absolute(
                                cols.saturating_sub(x + 1) as u32,
                                rows.saturating_sub(y + 1) as u32,
                                out,
                            )?;
                        }

                        (Position::Relative(x), Position::Absolute(y)) => {
                            let (_cols, rows) = out.get_size_in_cells()?;
                            self.cursor_up(rows as u32, out)?;
                            self.cursor_down(*y as u32, out)?;
                            self.cursor_x_relative(*x, out)?;
                        }

                        (Position::EndRelative(x), Position::Absolute(y)) => {
                            let (cols, _rows) = out.get_size_in_cells()?;
                            self.move_cursor_absolute(
                                cols.saturating_sub(x + 1) as u32,
                                *y as u32,
                                out,
                            )?;
                        }
                    }

                    let (cols, rows) = out.get_size_in_cells()?;
                    self.update_cursor_position(x, y, cols, rows);
                }

                Change::CursorColor(_color) => {
                    // TODO: this isn't spec'd by terminfo, but some terminals
                    // support it.  Add this to capabilities?
                }
                Change::CursorShape(shape) => match shape {
                    CursorShape::Default => {
                        if let Some(normal) = self.get_capability::<cap::CursorNormal>() {
                            normal.expand().to(out.by_ref())?;
                        }
                        if let Some(reset) = self.get_capability::<cap::ResetCursorStyle>() {
                            reset.expand().to(out.by_ref())?;
                        }
                    }
                    _ => {
                        let param = match shape {
                            CursorShape::Default => unreachable!(),
                            CursorShape::BlinkingBlock => 1,
                            CursorShape::SteadyBlock => 2,
                            CursorShape::BlinkingUnderline => 3,
                            CursorShape::SteadyUnderline => 4,
                            CursorShape::BlinkingBar => 5,
                            CursorShape::SteadyBar => 6,
                        };
                        if let Some(set) = self.get_capability::<cap::SetCursorStyle>() {
                            set.expand().kind(param).to(out.by_ref())?;
                        }
                    }
                },
                Change::CursorVisibility(visibility) => match visibility {
                    CursorVisibility::Visible => {
                        if let Some(show) = self.get_capability::<cap::CursorNormal>() {
                            show.expand().to(out.by_ref())?;
                        }
                    }
                    CursorVisibility::Hidden => {
                        if let Some(hide) = self.get_capability::<cap::CursorInvisible>() {
                            hide.expand().to(out.by_ref())?;
                        }
                    }
                },
                #[cfg(feature = "use_image")]
                Change::Image(image) => {
                    if self.caps.iterm2_image() {
                        let data =
                            encode_iterm_inline_image(image, image.top_left, image.bottom_right)?;

                        let file = ITermFileData {
                            name: None,
                            size: Some(data.len()),
                            width: ITermDimension::Cells(image.width as i64),
                            height: ITermDimension::Cells(image.height as i64),
                            preserve_aspect_ratio: true,
                            inline: true,
                            do_not_move_cursor: false,
                            data,
                        };

                        let osc = OperatingSystemCommand::ITermProprietary(ITermProprietary::File(
                            Box::new(file),
                        ));

                        write!(out, "{}", osc)?;

                    // TODO: } else if self.caps.sixel() {
                    } else {
                        // Blank out the cells and move the cursor to the right spot
                        for y in 0..image.height {
                            for _ in 0..image.width {
                                write!(out, " ")?;
                            }

                            if y != image.height - 1 {
                                writeln!(out)?;
                                self.cursor_left(image.width as u32, out)?;
                            }
                        }
                        self.cursor_up(image.height as u32, out)?;
                    }
                    self.cursor_position = None;
                }
                Change::ScrollRegionUp {
                    first_row,
                    region_size,
                    scroll_count,
                } => {
                    if *region_size > 0 {
                        if let Some(csr) = self.get_capability::<cap::ChangeScrollRegion>() {
                            let top = *first_row as u32;
                            let bottom = (*first_row + *region_size - 1) as u32;
                            let scroll_count = *scroll_count as u32;
                            csr.expand().top(top).bottom(bottom).to(out.by_ref())?;
                            if scroll_count > 0 {
                                if let Some(scroll) = self.get_capability::<cap::ParmIndex>() {
                                    scroll.expand().count(scroll_count).to(out.by_ref())?
                                } else {
                                    let scroll = self.get_capability::<cap::ScrollForward>();
                                    let set_position = self.get_capability::<cap::CursorAddress>();
                                    if let (Some(scroll), Some(set_position)) =
                                        (scroll, set_position)
                                    {
                                        set_position.expand().x(0).y(bottom).to(out.by_ref())?;
                                        for _ in 0..scroll_count {
                                            scroll.expand().to(out.by_ref())?
                                        }
                                    }
                                }
                            }
                        }
                    }
                    self.cursor_position = None;
                }
                Change::ScrollRegionDown {
                    first_row,
                    region_size,
                    scroll_count,
                } => {
                    if *region_size > 0 {
                        if let Some(csr) = self.get_capability::<cap::ChangeScrollRegion>() {
                            let top = *first_row as u32;
                            let bottom = (*first_row + *region_size - 1) as u32;
                            let scroll_count = *scroll_count as u32;
                            csr.expand().top(top).bottom(bottom).to(out.by_ref())?;
                            if scroll_count > 0 {
                                if let Some(scroll) = self.get_capability::<cap::ParmRindex>() {
                                    scroll.expand().count(scroll_count).to(out.by_ref())?
                                } else {
                                    let scroll = self.get_capability::<cap::ScrollReverse>();
                                    let set_position = self.get_capability::<cap::CursorAddress>();
                                    if let (Some(scroll), Some(set_position)) =
                                        (scroll, set_position)
                                    {
                                        set_position.expand().x(0).y(top).to(out.by_ref())?;
                                        for _ in 0..scroll_count {
                                            scroll.expand().to(out.by_ref())?
                                        }
                                    }
                                }
                            }
                        }
                    }
                    self.cursor_position = None;
                }

                Change::Title(text) => {
                    let osc = OperatingSystemCommand::SetWindowTitle(text.to_string());
                    write!(out, "{}", osc)?;
                }

                Change::LineAttribute(attr) => {
                    let esc = Esc::Code(match attr {
                        LineAttribute::DoubleHeightTopHalfLine => {
                            EscCode::DecDoubleHeightTopHalfLine
                        }
                        LineAttribute::DoubleHeightBottomHalfLine => {
                            EscCode::DecDoubleHeightBottomHalfLine
                        }
                        LineAttribute::DoubleWidthLine => EscCode::DecDoubleWidthLine,
                        LineAttribute::SingleWidthLine => EscCode::DecSingleWidthLine,
                    });
                    write!(out, "{esc}")?;
                }
            }
        }

        self.flush_pending_attr(out)?;
        out.flush()?;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod test {
    use super::*;
    use crate::bail;
    use crate::caps::ProbeHints;
    use crate::color::{AnsiColor, ColorAttribute};
    #[cfg(feature = "use_image")]
    use crate::escape::osc::{ITermDimension, ITermProprietary, OperatingSystemCommand};
    use crate::escape::parser::Parser;
    use crate::escape::{Action, ControlCode, Esc, EscCode};
    use crate::input::InputEvent;
    use crate::terminal::unix::{Purge, SetAttributeWhen, UnixTty};
    use crate::terminal::{cast, ScreenSize, Terminal, TerminalWaker};
    #[cfg(feature = "use_image")]
    use crate::{image::ImageData, surface::Image};
    #[cfg(feature = "use_image")]
    use image::AnimationDecoder;
    use libc::winsize;
    use std::io::{Error as IoError, ErrorKind, Read, Result as IoResult, Write};
    use std::mem;
    #[cfg(feature = "use_image")]
    use std::sync::Arc;
    use std::time::Duration;
    use terminfo;
    use termios::Termios;

    /// Return Capabilities loaded from the included xterm terminfo data
    fn xterm_terminfo() -> Capabilities {
        xterm_terminfo_with_hints(ProbeHints::default())
    }

    fn xterm_terminfo_with_hints(hints: ProbeHints) -> Capabilities {
        // Load our own compiled data so that the tests have an
        // environment that doesn't vary machine by machine.
        let data = include_bytes!("../../data/xterm-256color");
        Capabilities::new_with_hints(hints.terminfo_db(Some(
            terminfo::Database::from_buffer(data.as_ref()).unwrap(),
        )))
        .unwrap()
    }

    fn no_terminfo_all_enabled() -> Capabilities {
        Capabilities::new_with_hints(ProbeHints::default().color_level(Some(ColorLevel::TrueColor)))
            .unwrap()
    }

    struct FakeTty {
        buf: Vec<u8>,
        size: winsize,
        termios: Termios,
    }

    impl FakeTty {
        fn new_with_size(width: usize, height: usize) -> Self {
            let size = winsize {
                ws_col: cast(width).unwrap(),
                ws_row: cast(height).unwrap(),
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let buf = Vec::new();
            Self {
                size,
                buf,
                termios: unsafe { mem::zeroed() },
            }
        }
    }
    impl RenderTty for FakeTty {
        fn get_size_in_cells(&mut self) -> Result<(usize, usize)> {
            Ok((self.size.ws_col as usize, self.size.ws_row as usize))
        }
    }

    impl UnixTty for FakeTty {
        fn get_size(&mut self) -> Result<winsize> {
            Ok(self.size)
        }
        fn set_size(&mut self, size: winsize) -> Result<()> {
            self.size = size;
            Ok(())
        }
        fn get_termios(&mut self) -> Result<Termios> {
            Ok(self.termios)
        }
        fn set_termios(&mut self, termios: &Termios, _when: SetAttributeWhen) -> Result<()> {
            self.termios = *termios;
            Ok(())
        }
        /// Waits until all written data has been transmitted.
        fn drain(&mut self) -> Result<()> {
            Ok(())
        }
        fn purge(&mut self, _purge: Purge) -> Result<()> {
            Ok(())
        }
    }

    impl Read for FakeTty {
        fn read(&mut self, _buf: &mut [u8]) -> std::result::Result<usize, IoError> {
            Err(IoError::new(ErrorKind::Other, "not implemented"))
        }
    }
    impl Write for FakeTty {
        fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
            self.buf.write(buf)
        }

        fn flush(&mut self) -> IoResult<()> {
            self.buf.flush()
        }
    }

    struct FakeTerm {
        write: FakeTty,
        renderer: TerminfoRenderer,
    }

    impl FakeTerm {
        fn new(caps: Capabilities) -> Self {
            Self::new_with_size(caps, 80, 24)
        }

        fn new_with_size(caps: Capabilities, width: usize, height: usize) -> Self {
            let write = FakeTty::new_with_size(width, height);
            let renderer = TerminfoRenderer::new(caps);
            Self { write, renderer }
        }

        fn parse(&self) -> Vec<Action> {
            let mut p = Parser::new();
            p.parse_as_vec(&self.write.buf)
        }
    }

    impl Terminal for FakeTerm {
        fn set_raw_mode(&mut self) -> Result<()> {
            bail!("not implemented");
        }

        fn set_cooked_mode(&mut self) -> Result<()> {
            bail!("not implemented");
        }

        fn enter_alternate_screen(&mut self) -> Result<()> {
            bail!("not implemented");
        }

        fn exit_alternate_screen(&mut self) -> Result<()> {
            bail!("not implemented");
        }

        fn render(&mut self, changes: &[Change]) -> Result<()> {
            self.renderer.render_to(changes, &mut self.write)
        }

        fn get_screen_size(&mut self) -> Result<ScreenSize> {
            let size = self.write.get_size()?;
            Ok(ScreenSize {
                rows: cast(size.ws_row)?,
                cols: cast(size.ws_col)?,
                xpixel: cast(size.ws_xpixel)?,
                ypixel: cast(size.ws_ypixel)?,
            })
        }

        fn set_screen_size(&mut self, size: ScreenSize) -> Result<()> {
            let size = winsize {
                ws_row: cast(size.rows)?,
                ws_col: cast(size.cols)?,
                ws_xpixel: cast(size.xpixel)?,
                ws_ypixel: cast(size.ypixel)?,
            };

            self.write.set_size(size)
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }

        fn poll_input(&mut self, _wait: Option<Duration>) -> Result<Option<InputEvent>> {
            bail!("not implemented");
        }

        fn waker(&self) -> TerminalWaker {
            unimplemented!();
        }
    }

    #[cfg(feature = "use_image")]
    fn iterm2_caps() -> Capabilities {
        xterm_terminfo_with_hints(
            ProbeHints::default()
                .term_program(Some("iTerm.app".into()))
                .term_program_version(Some("3.4.0".into())),
        )
    }

    #[cfg(feature = "use_image")]
    fn parse_iterm_file(out: &FakeTerm) -> ITermFileData {
        let parsed = out.parse();
        match parsed.as_slice() {
            [Action::OperatingSystemCommand(osc)]
            | [Action::OperatingSystemCommand(osc), Action::Esc(Esc::Code(EscCode::StringTerminator))] => {
                match osc.as_ref() {
                    OperatingSystemCommand::ITermProprietary(ITermProprietary::File(file)) => {
                        (**file).clone()
                    }
                    other => panic!("expected iTerm file OSC, got {:?}", other),
                }
            }
            other => panic!("expected a single OSC action, got {:?}", other),
        }
    }

    #[cfg(feature = "use_image")]
    fn render_iterm_image(change: Change) -> FakeTerm {
        let mut out = FakeTerm::new(iterm2_caps());
        out.render(&[change]).unwrap();
        out
    }

    #[test]
    fn empty_render() {
        let mut out = FakeTerm::new(xterm_terminfo());
        out.render(&[]).unwrap();
        assert_eq!("", String::from_utf8(out.write.buf).unwrap());
        assert_eq!(out.renderer.current_attr, CellAttributes::default());
    }

    #[test]
    fn basic_text() {
        let mut out = FakeTerm::new(xterm_terminfo());
        out.render(&[Change::Text("foo".into())]).unwrap();
        assert_eq!("foo", String::from_utf8(out.write.buf).unwrap());
        assert_eq!(out.renderer.current_attr, CellAttributes::default());
    }

    #[test]
    fn bold_text() {
        let mut out = FakeTerm::new(xterm_terminfo());
        out.render(&[
            Change::Text("not ".into()),
            Change::Attribute(AttributeChange::Intensity(Intensity::Bold)),
            Change::Text("foo".into()),
        ])
        .unwrap();

        let result = out.parse();
        assert_eq!(
            result,
            vec![
                Action::Print('n'),
                Action::Print('o'),
                Action::Print('t'),
                Action::Print(' '),
                Action::Esc(Esc::Code(EscCode::AsciiCharacterSetG0)),
                Action::CSI(CSI::Sgr(Sgr::Reset)),
                Action::CSI(CSI::Sgr(Sgr::Intensity(Intensity::Bold))),
                Action::Print('f'),
                Action::Print('o'),
                Action::Print('o'),
            ]
        );

        assert_eq!(
            out.renderer.current_attr,
            CellAttributes::default()
                .set_intensity(Intensity::Bold)
                .clone()
        );
    }

    #[cfg(feature = "use_image")]
    #[test]
    fn iterm2_full_rgba_image_is_encoded_as_png() {
        let change = Change::Image(Image {
            width: 1,
            height: 1,
            top_left: TextureCoordinate::new_f32(0.0, 0.0),
            bottom_right: TextureCoordinate::new_f32(1.0, 1.0),
            image: Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
                2,
                1,
                vec![255, 0, 0, 255, 0, 255, 0, 255],
            ))),
        });

        let out = render_iterm_image(change);
        let file = parse_iterm_file(&out);

        assert_eq!(file.width, ITermDimension::Cells(1));
        assert_eq!(file.height, ITermDimension::Cells(1));
        assert_eq!(
            image::guess_format(&file.data).unwrap(),
            image::ImageFormat::Png
        );
        assert_eq!(
            image::load_from_memory(&file.data)
                .unwrap()
                .into_rgba8()
                .into_vec(),
            vec![255, 0, 0, 255, 0, 255, 0, 255]
        );
    }

    #[cfg(feature = "use_image")]
    #[test]
    fn iterm2_cropped_rgba_image_uses_only_requested_region() {
        let change = Change::Image(Image {
            width: 1,
            height: 1,
            top_left: TextureCoordinate::new_f32(0.5, 0.0),
            bottom_right: TextureCoordinate::new_f32(1.0, 1.0),
            image: Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
                2,
                2,
                vec![
                    255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
                ],
            ))),
        });

        let out = render_iterm_image(change);
        let file = parse_iterm_file(&out);
        let image = image::load_from_memory(&file.data).unwrap().into_rgba8();

        assert_eq!(image.dimensions(), (1, 2));
        assert_eq!(image.into_vec(), vec![0, 255, 0, 255, 255, 255, 0, 255]);
    }

    #[cfg(feature = "use_image")]
    #[test]
    fn iterm2_animated_rgba_image_is_encoded_as_gif() {
        let change = Change::Image(Image {
            width: 1,
            height: 1,
            top_left: TextureCoordinate::new_f32(0.0, 0.0),
            bottom_right: TextureCoordinate::new_f32(1.0, 1.0),
            image: Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
                width: 1,
                height: 1,
                durations: vec![Duration::from_millis(10), Duration::from_millis(20)],
                frames: vec![vec![255, 0, 0, 255], vec![0, 0, 255, 255]],
                hashes: vec![
                    ImageDataType::hash_bytes(&[255, 0, 0, 255]),
                    ImageDataType::hash_bytes(&[0, 0, 255, 255]),
                ],
            })),
        });

        let out = render_iterm_image(change);
        let file = parse_iterm_file(&out);

        assert_eq!(
            image::guess_format(&file.data).unwrap(),
            image::ImageFormat::Gif
        );

        let decoder =
            image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&file.data)).unwrap();
        let frames = decoder.into_frames().collect_frames().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0].clone().into_buffer().into_vec(),
            vec![255, 0, 0, 255]
        );
        assert_eq!(
            frames[1].clone().into_buffer().into_vec(),
            vec![0, 0, 255, 255]
        );
    }

    #[cfg(feature = "use_image")]
    #[test]
    fn iterm2_cropped_undecodable_image_returns_an_explicit_error() {
        let mut out = FakeTerm::new(iterm2_caps());
        let image = Change::Image(Image {
            width: 1,
            height: 1,
            top_left: TextureCoordinate::new_f32(0.25, 0.0),
            bottom_right: TextureCoordinate::new_f32(0.75, 1.0),
            image: Arc::new(ImageData::with_data(ImageDataType::EncodedFile(vec![
                0xde, 0xad, 0xbe, 0xef,
            ]))),
        });

        let err = out.render(&[image]).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("cannot crop encoded image data unless the format is decodable"),
            "unexpected error: {}",
            message
        );
    }

    #[test]
    // Sanity that force_terminfo_render_to_use_ansi_sgr does something.
    fn bold_text_force_ansi_sgr() {
        let mut out = FakeTerm::new(xterm_terminfo_with_hints(
            ProbeHints::default().force_terminfo_render_to_use_ansi_sgr(Some(true)),
        ));
        out.render(&[
            Change::Text("not ".into()),
            AttributeChange::Intensity(Intensity::Bold).into(),
            Change::Text("foo".into()),
        ])
        .unwrap();

        let result = out.parse();
        assert_eq!(
            result,
            vec![
                // Same as bold_text() above, but without the "(B" from srg/sgr0.
                Action::Print('n'),
                Action::Print('o'),
                Action::Print('t'),
                Action::Print(' '),
                Action::CSI(CSI::Sgr(Sgr::Reset)),
                Action::CSI(CSI::Sgr(Sgr::Intensity(Intensity::Bold))),
                Action::Print('f'),
                Action::Print('o'),
                Action::Print('o'),
            ],
        );
    }

    #[test]
    fn clear_screen() {
        let mut out = FakeTerm::new_with_size(xterm_terminfo(), 4, 3);
        out.render(&[Change::ClearScreen(ColorAttribute::default())])
            .unwrap();

        let result = out.parse();
        assert_eq!(
            result,
            vec![
                Action::CSI(CSI::Cursor(Cursor::Position {
                    line: OneBased::new(1),
                    col: OneBased::new(1)
                })),
                Action::CSI(CSI::Edit(Edit::EraseInDisplay(
                    EraseInDisplay::EraseDisplay,
                ))),
            ]
        );

        assert_eq!(out.renderer.current_attr, CellAttributes::default());
    }

    #[test]
    fn clear_screen_bce() {
        let mut out = FakeTerm::new_with_size(xterm_terminfo(), 4, 3);
        out.render(&[Change::ClearScreen(AnsiColor::Maroon.into())])
            .unwrap();

        let result = out.parse();
        assert_eq!(
            result,
            vec![
                Action::CSI(CSI::Sgr(Sgr::Background(AnsiColor::Maroon.into()))),
                Action::CSI(CSI::Cursor(Cursor::Position {
                    line: OneBased::new(1),
                    col: OneBased::new(1)
                })),
                Action::CSI(CSI::Edit(Edit::EraseInDisplay(
                    EraseInDisplay::EraseDisplay,
                ))),
            ]
        );

        assert_eq!(
            out.renderer.current_attr,
            CellAttributes::default()
                .set_background(AnsiColor::Maroon)
                .clone()
        );
    }

    #[test]
    fn clear_screen_no_terminfo() {
        let mut out = FakeTerm::new_with_size(no_terminfo_all_enabled(), 4, 3);
        out.render(&[Change::ClearScreen(ColorAttribute::default())])
            .unwrap();

        let result = out.parse();
        assert_eq!(
            result,
            vec![
                Action::CSI(CSI::Cursor(Cursor::Position {
                    line: OneBased::new(1),
                    col: OneBased::new(1)
                })),
                Action::CSI(CSI::Edit(Edit::EraseInDisplay(
                    EraseInDisplay::EraseDisplay,
                ))),
            ]
        );

        assert_eq!(out.renderer.current_attr, CellAttributes::default());
    }

    #[test]
    fn clear_screen_bce_no_terminfo() {
        let mut out = FakeTerm::new_with_size(no_terminfo_all_enabled(), 4, 3);
        out.render(&[Change::ClearScreen(AnsiColor::Maroon.into())])
            .unwrap();

        let result = out.parse();
        assert_eq!(
            result,
            vec![
                Action::CSI(CSI::Sgr(Sgr::Background(AnsiColor::Maroon.into()))),
                Action::CSI(CSI::Cursor(Cursor::Position {
                    line: OneBased::new(1),
                    col: OneBased::new(1)
                })),
                // bce is not known to be available, so we emit a bunch of spaces.
                // TODO: could we use ECMA-48 REP for this?
                Action::Print(' '),
                Action::Print(' '),
                Action::Print(' '),
                Action::Print(' '),
                Action::Print(' '),
                Action::Print(' '),
                Action::Print(' '),
                Action::Print(' '),
                Action::Print(' '),
                Action::Print(' '),
                Action::Print(' '),
                Action::Print(' '),
                Action::CSI(CSI::Cursor(Cursor::Position {
                    line: OneBased::new(1),
                    col: OneBased::new(1)
                })),
            ]
        );

        assert_eq!(
            out.renderer.current_attr,
            CellAttributes::default()
                .set_background(AnsiColor::Maroon)
                .clone()
        );
    }

    #[test]
    fn clear_to_end_of_line_no_bce() {
        let mut out = FakeTerm::new_with_size(no_terminfo_all_enabled(), 4, 3);
        out.render(&[
            Change::CursorPosition {
                x: Position::Absolute(1),
                y: Position::Absolute(1),
            },
            Change::ClearToEndOfLine(AnsiColor::Maroon.into()),
        ])
        .unwrap();

        let result = out.parse();
        assert_eq!(
            result,
            vec![
                Action::CSI(CSI::Cursor(Cursor::Position {
                    line: OneBased::new(2),
                    col: OneBased::new(2)
                })),
                Action::CSI(CSI::Sgr(Sgr::Background(AnsiColor::Maroon.into()))),
                Action::Print(' '),
                Action::Print(' '),
                Action::Print(' '),
                Action::Control(ControlCode::CarriageReturn),
                Action::CSI(CSI::Cursor(Cursor::Right(1))),
            ]
        );

        assert_eq!(
            out.renderer.current_attr,
            CellAttributes::default()
                .set_background(AnsiColor::Maroon)
                .clone()
        );
    }

    #[test]
    fn clear_to_end_of_screen_no_bce() {
        let mut out = FakeTerm::new_with_size(no_terminfo_all_enabled(), 4, 3);
        out.render(&[
            Change::CursorPosition {
                x: Position::Absolute(1),
                y: Position::Absolute(1),
            },
            Change::ClearToEndOfScreen(AnsiColor::Maroon.into()),
        ])
        .unwrap();

        let result = out.parse();
        assert_eq!(
            result.first(),
            Some(&Action::CSI(CSI::Cursor(Cursor::Position {
                line: OneBased::new(2),
                col: OneBased::new(2)
            })))
        );
        assert_eq!(
            result.get(1),
            Some(&Action::CSI(CSI::Sgr(Sgr::Background(
                AnsiColor::Maroon.into()
            ))))
        );
        assert_eq!(
            result.last(),
            Some(&Action::CSI(CSI::Cursor(Cursor::Right(1))))
        );
        assert_eq!(
            result
                .iter()
                .filter(|action| matches!(action, Action::Print(' ')))
                .count(),
            7
        );
        assert_eq!(
            result
                .iter()
                .filter(|action| matches!(action, Action::Control(ControlCode::CarriageReturn)))
                .count(),
            2
        );
        assert_eq!(
            result
                .iter()
                .filter(|action| matches!(action, Action::Control(ControlCode::LineFeed)))
                .count(),
            1
        );
        assert_eq!(
            result
                .iter()
                .filter(|action| matches!(action, Action::CSI(CSI::Cursor(Cursor::Up(1)))))
                .count(),
            1
        );

        assert_eq!(
            out.renderer.current_attr,
            CellAttributes::default()
                .set_background(AnsiColor::Maroon)
                .clone()
        );
    }

    #[test]
    fn bold_text_no_terminfo() {
        let mut out = FakeTerm::new(no_terminfo_all_enabled());
        out.render(&[
            Change::Text("not ".into()),
            Change::Attribute(AttributeChange::Intensity(Intensity::Bold)),
            Change::Text("foo".into()),
        ])
        .unwrap();

        let result = out.parse();
        assert_eq!(
            result,
            vec![
                Action::Print('n'),
                Action::Print('o'),
                Action::Print('t'),
                Action::Print(' '),
                Action::CSI(CSI::Sgr(Sgr::Reset)),
                Action::CSI(CSI::Sgr(Sgr::Intensity(Intensity::Bold))),
                Action::Print('f'),
                Action::Print('o'),
                Action::Print('o'),
            ]
        );

        assert_eq!(
            out.renderer.current_attr,
            CellAttributes::default()
                .set_intensity(Intensity::Bold)
                .clone()
        );
    }

    #[test]
    fn red_bold_text() {
        let mut out = FakeTerm::new(xterm_terminfo());
        out.render(&[
            Change::Attribute(AttributeChange::Foreground(AnsiColor::Maroon.into())),
            Change::Attribute(AttributeChange::Intensity(Intensity::Bold)),
            Change::Text("red".into()),
            Change::Attribute(AttributeChange::Foreground(AnsiColor::Red.into())),
        ])
        .unwrap();

        let result = out.parse();
        assert_eq!(
            result,
            vec![
                Action::Esc(Esc::Code(EscCode::AsciiCharacterSetG0)),
                Action::CSI(CSI::Sgr(Sgr::Reset)),
                // Note that the render code rearranges (red,bold) to (bold,red)
                Action::CSI(CSI::Sgr(Sgr::Intensity(Intensity::Bold))),
                Action::CSI(CSI::Sgr(Sgr::Foreground(AnsiColor::Maroon.into()))),
                Action::Print('r'),
                Action::Print('e'),
                Action::Print('d'),
                Action::CSI(CSI::Sgr(Sgr::Foreground(AnsiColor::Red.into()))),
            ]
        );

        assert_eq!(
            out.renderer.current_attr,
            CellAttributes::default()
                .set_intensity(Intensity::Bold)
                .set_foreground(AnsiColor::Red)
                .clone()
        );
    }

    #[test]
    fn red_bold_text_no_terminfo() {
        let mut out = FakeTerm::new(no_terminfo_all_enabled());
        out.render(&[
            Change::Attribute(AttributeChange::Foreground(AnsiColor::Maroon.into())),
            Change::Attribute(AttributeChange::Intensity(Intensity::Bold)),
            Change::Text("red".into()),
            Change::Attribute(AttributeChange::Foreground(AnsiColor::Red.into())),
        ])
        .unwrap();

        let result = out.parse();
        assert_eq!(
            result,
            vec![
                Action::CSI(CSI::Sgr(Sgr::Reset)),
                // Note that the render code rearranges (red,bold) to (bold,red)
                Action::CSI(CSI::Sgr(Sgr::Intensity(Intensity::Bold))),
                Action::CSI(CSI::Sgr(Sgr::Foreground(AnsiColor::Maroon.into()))),
                Action::Print('r'),
                Action::Print('e'),
                Action::Print('d'),
                Action::CSI(CSI::Sgr(Sgr::Foreground(AnsiColor::Red.into()))),
            ]
        );

        assert_eq!(
            out.renderer.current_attr,
            CellAttributes::default()
                .set_intensity(Intensity::Bold)
                .set_foreground(AnsiColor::Red)
                .clone()
        );
    }

    #[test]
    fn color_after_attribute_change() {
        let mut out = FakeTerm::new(xterm_terminfo());
        out.render(&[
            Change::Attribute(AttributeChange::Foreground(AnsiColor::Maroon.into())),
            Change::Attribute(AttributeChange::Intensity(Intensity::Bold)),
            Change::Text("red".into()),
            Change::Attribute(AttributeChange::Intensity(Intensity::Normal)),
            Change::Text("2".into()),
        ])
        .unwrap();

        let result = out.parse();
        assert_eq!(
            result,
            vec![
                Action::Esc(Esc::Code(EscCode::AsciiCharacterSetG0)),
                Action::CSI(CSI::Sgr(Sgr::Reset)),
                // Note that the render code rearranges (red,bold) to (bold,red)
                Action::CSI(CSI::Sgr(Sgr::Intensity(Intensity::Bold))),
                Action::CSI(CSI::Sgr(Sgr::Foreground(AnsiColor::Maroon.into()))),
                Action::Print('r'),
                Action::Print('e'),
                Action::Print('d'),
                // Turning off bold is translated into reset and set red again
                Action::Esc(Esc::Code(EscCode::AsciiCharacterSetG0)),
                Action::CSI(CSI::Sgr(Sgr::Reset)),
                Action::CSI(CSI::Sgr(Sgr::Foreground(AnsiColor::Maroon.into()))),
                Action::Print('2'),
            ]
        );

        assert_eq!(
            out.renderer.current_attr,
            CellAttributes::default()
                .set_foreground(AnsiColor::Maroon)
                .clone()
        );
    }

    #[test]
    fn truecolor() {
        let mut out = FakeTerm::new(xterm_terminfo());
        out.render(&[
            Change::Attribute(AttributeChange::Foreground(
                ColorSpec::TrueColor((255, 128, 64).into()).into(),
            )),
            Change::Text("A".into()),
        ])
        .unwrap();

        let result = out.parse();
        assert_eq!(
            result,
            vec![
                Action::CSI(CSI::Sgr(Sgr::Foreground(ColorSpec::TrueColor(
                    (255, 128, 64).into()
                ),))),
                Action::Print('A'),
            ]
        );
    }

    #[test]
    fn truecolor_no_terminfo() {
        let mut out = FakeTerm::new(no_terminfo_all_enabled());
        out.render(&[
            Change::Attribute(AttributeChange::Foreground(
                ColorSpec::TrueColor((255, 128, 64).into()).into(),
            )),
            Change::Text("A".into()),
        ])
        .unwrap();

        let result = out.parse();
        assert_eq!(
            result,
            vec![
                Action::CSI(CSI::Sgr(Sgr::Foreground(ColorSpec::TrueColor(
                    (255, 128, 64).into()
                ),))),
                Action::Print('A'),
            ]
        );
    }
}
