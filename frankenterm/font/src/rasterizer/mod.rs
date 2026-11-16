use crate::parser::ParsedFont;
use crate::units::*;
use config::FontRasterizerSelection;
#[cfg(not(windows))]
use image::{ImageBuffer, Rgba};

pub(crate) mod cache;

/// The amount, as a number in [0,1], to horizontally skew a glyph when rendering synthetic
/// italics
#[cfg(not(windows))]
pub(crate) const FAKE_ITALIC_SKEW: f64 = 0.2;

#[cfg(not(windows))]
pub mod colr;
#[cfg(target_os = "macos")]
pub mod coretext;
#[cfg(not(windows))]
pub mod freetype;
#[cfg(not(windows))]
pub mod harfbuzz;

/// A bitmap representation of a glyph.
/// The data is stored as pre-multiplied RGBA 32bpp.
#[derive(Clone, Debug)]
pub struct RasterizedGlyph {
    pub data: Vec<u8>,
    pub height: usize,
    pub width: usize,
    pub bearing_x: PixelLength,
    pub bearing_y: PixelLength,
    pub has_color: bool,
    /// if true, glyphcache shouldn't need to scale the
    /// glyph to match metrics
    pub is_scaled: bool,
}

/// Rasterizes the specified glyph index in the associated font
/// and returns the generated bitmap
pub trait FontRasterizer {
    fn rasterize_glyph(
        &self,
        glyph_pos: u32,
        size: f64,
        dpi: u32,
    ) -> anyhow::Result<RasterizedGlyph>;
}

#[cfg(not(windows))]
pub fn new_rasterizer(
    rasterizer: FontRasterizerSelection,
    handle: &ParsedFont,
    pixel_geometry: config::DisplayPixelGeometry,
) -> anyhow::Result<Box<dyn FontRasterizer>> {
    match rasterizer {
        FontRasterizerSelection::FreeType => Ok(Box::new(
            freetype::FreeTypeRasterizer::from_locator(handle, pixel_geometry)?,
        )),
        FontRasterizerSelection::Harfbuzz => Ok(Box::new(
            harfbuzz::HarfbuzzRasterizer::from_locator(handle)?,
        )),
        FontRasterizerSelection::CoreText => coretext_or_freetype(handle, pixel_geometry),
    }
}

/// CoreText for faces it can load; FreeType, the kill-switch fallback, for
/// the rest (ft-yccm0.4.3.1).
#[cfg(target_os = "macos")]
fn coretext_or_freetype(
    handle: &ParsedFont,
    pixel_geometry: config::DisplayPixelGeometry,
) -> anyhow::Result<Box<dyn FontRasterizer>> {
    match coretext::CoreTextRasterizer::from_locator(handle) {
        Ok(rasterizer) => Ok(Box::new(rasterizer)),
        Err(reason) => {
            log::info!(
                "CoreText rasterizer: {} falls back to FreeType: {reason:#}",
                handle.handle.diagnostic_string()
            );
            Ok(Box::new(freetype::FreeTypeRasterizer::from_locator(
                handle,
                pixel_geometry,
            )?))
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn coretext_or_freetype(
    handle: &ParsedFont,
    pixel_geometry: config::DisplayPixelGeometry,
) -> anyhow::Result<Box<dyn FontRasterizer>> {
    log::warn!("font_rasterizer = \"CoreText\" is macOS-only; using FreeType");
    Ok(Box::new(freetype::FreeTypeRasterizer::from_locator(
        handle,
        pixel_geometry,
    )?))
}

#[cfg(windows)]
pub fn new_rasterizer(
    rasterizer: FontRasterizerSelection,
    handle: &ParsedFont,
    pixel_geometry: config::DisplayPixelGeometry,
) -> anyhow::Result<Box<dyn FontRasterizer>> {
    let _ = (rasterizer, handle, pixel_geometry);
    anyhow::bail!("Windows font rasterization is not wired without cairo");
}

#[cfg(not(windows))]
pub(crate) fn swap_red_and_blue<Container: std::ops::Deref<Target = [u8]> + std::ops::DerefMut>(
    image: &mut ImageBuffer<Rgba<u8>, Container>,
) {
    for pixel in image.pixels_mut() {
        let red = pixel[0];
        pixel[0] = pixel[2];
        pixel[2] = red;
    }
}

#[cfg(not(windows))]
pub(crate) fn crop_to_non_transparent<Container>(
    image: &mut image::ImageBuffer<Rgba<u8>, Container>,
) -> image::SubImage<&mut ImageBuffer<Rgba<u8>, Container>>
where
    Container: std::ops::Deref<Target = [u8]>,
{
    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel[3] != 0 {
            bounds = Some(match bounds {
                Some((left, top, right, bottom)) => {
                    (left.min(x), top.min(y), right.max(x), bottom.max(y))
                }
                None => (x, y, x, y),
            });
        }
    }

    let (left, top, width, height) = match bounds {
        // The extrema are occupied pixels, so both ends are inclusive.
        // In particular, a one-pixel glyph must remain one pixel wide and tall.
        Some((left, top, right, bottom)) => (left, top, right - left + 1, bottom - top + 1),
        // Preserve the existing empty-image dimensions and origin.
        None => (0, 0, image.width(), image.height()),
    };

    image::imageops::crop(image, left, top, width, height)
}

/// Crop a FreeType color bitmap without changing its position relative to the
/// glyph origin. Cropping translates the bitmap; it does not scale its metrics.
/// Bitmap rows run downwards, while the top bearing measures up from the baseline.
#[cfg(not(windows))]
fn rasterize_bgra_image<Container>(
    image: &mut ImageBuffer<Rgba<u8>, Container>,
    bearing_x: f64,
    bearing_y: f64,
    has_color: bool,
    is_scaled: bool,
) -> RasterizedGlyph
where
    Container: std::ops::Deref<Target = [u8]> + 'static,
{
    let cropped = crop_to_non_transparent(image);
    let (left, top) = cropped.offsets();
    let mut pixels = cropped.to_image();
    swap_red_and_blue(&mut pixels);

    RasterizedGlyph {
        height: pixels.height() as usize,
        width: pixels.width() as usize,
        data: pixels.into_vec(),
        bearing_x: PixelLength::new(bearing_x + f64::from(left)),
        bearing_y: PixelLength::new(bearing_y - f64::from(top)),
        has_color,
        is_scaled,
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn bitmap_crop_preserves_single_pixels_and_occupied_edges() {
        for (width, height) in [(1, 1), (1, 7), (9, 1), (5, 4)] {
            let mut image = ImageBuffer::from_pixel(width, height, Rgba([7, 13, 29, 127]));
            let glyph = rasterize_bgra_image(&mut image, -3.0, 9.0, true, false);
            assert_eq!(
                (glyph.width, glyph.height),
                (width as usize, height as usize)
            );
            assert_eq!(glyph.bearing_x.get(), -3.0);
            assert_eq!(glyph.bearing_y.get(), 9.0);
            assert!(glyph
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [29, 13, 7, 127]));
            assert!(glyph.has_color);
            assert!(!glyph.is_scaled);
        }
    }

    #[test]
    fn padded_bitmap_crops_to_exact_ink_bounds_without_moving_the_glyph() {
        let mut image = ImageBuffer::from_pixel(8, 7, Rgba([0, 0, 0, 0]));
        image.put_pixel(2, 3, Rgba([5, 10, 20, 40]));
        image.put_pixel(6, 5, Rgba([7, 14, 28, 56]));
        let glyph = rasterize_bgra_image(&mut image, -4.0, 9.0, true, false);
        assert_eq!((glyph.width, glyph.height), (5, 3));
        assert_eq!(glyph.bearing_x.get(), -2.0);
        assert_eq!(glyph.bearing_y.get(), 6.0);
        assert_eq!(&glyph.data[..4], &[20, 10, 5, 40]);
        assert_eq!(&glyph.data[glyph.data.len() - 4..], &[28, 14, 7, 56]);
    }

    #[test]
    fn bitmap_crop_preserves_every_ink_pixel_in_baseline_coordinates() {
        // Exhaust every ink placement in a small bitmap. This includes isolated
        // pixels, disjoint extrema, one-row/column crops, and transparent images.
        for mask in 0u32..64 {
            let mut image = ImageBuffer::from_fn(3, 2, |x, y| {
                if mask & (1 << (y * 3 + x)) == 0 {
                    Rgba([0, 0, 0, 0])
                } else {
                    Rgba([11 + x as u8, 23 + y as u8, 47, 128])
                }
            });
            let source = image.clone();
            let glyph = rasterize_bgra_image(&mut image, -5.0, 7.0, true, true);
            assert_eq!(image, source, "source pixels must remain immutable");
            assert!(glyph.is_scaled);
            assert_eq!(glyph.data.len(), glyph.width * glyph.height * 4);

            let left = (glyph.bearing_x.get() + 5.0) as u32;
            let top = (7.0 - glyph.bearing_y.get()) as u32;
            for (x, y, pixel) in source.enumerate_pixels() {
                if pixel[3] == 0 {
                    continue;
                }
                assert!(x >= left && y >= top);
                let local_x = (x - left) as usize;
                let local_y = (y - top) as usize;
                assert!(local_x < glyph.width && local_y < glyph.height);
                let offset = (local_y * glyph.width + local_x) * 4;
                assert_eq!(
                    &glyph.data[offset..offset + 4],
                    &[pixel[2], pixel[1], pixel[0], pixel[3]]
                );
                assert_eq!(glyph.bearing_x.get() + local_x as f64, -5.0 + f64::from(x));
                assert_eq!(glyph.bearing_y.get() - local_y as f64, 7.0 - f64::from(y));
            }

            for y in 0..glyph.height {
                for x in 0..glyph.width {
                    let pixel = source.get_pixel(left + x as u32, top + y as u32);
                    let offset = (y * glyph.width + x) * 4;
                    assert_eq!(
                        &glyph.data[offset..offset + 4],
                        &[pixel[2], pixel[1], pixel[0], pixel[3]]
                    );
                }
            }
        }
    }

    #[test]
    fn transparent_bitmap_keeps_its_dimensions_and_bearings() {
        for (width, height) in [(0, 0), (0, 3), (4, 0), (4, 3)] {
            let mut image = ImageBuffer::from_pixel(width, height, Rgba([0, 0, 0, 0]));
            let glyph = rasterize_bgra_image(&mut image, 2.0, -4.0, false, true);
            assert_eq!(
                (glyph.width, glyph.height),
                (width as usize, height as usize)
            );
            assert_eq!(glyph.bearing_x.get(), 2.0);
            assert_eq!(glyph.bearing_y.get(), -4.0);
            assert_eq!(glyph.data, vec![0; width as usize * height as usize * 4]);
            assert!(!glyph.has_color);
        }
    }
}
