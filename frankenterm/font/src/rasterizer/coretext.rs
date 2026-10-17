//! The CoreText glyph rasterizer on macOS (ft-yccm0.4.3.1).
//!
//! macOS users notice when text is not rasterized the way the rest of the
//! system draws it, and Apple Color Emoji (sbix bitmaps) renders best through
//! CoreText. Ghostty rasterizes with CoreText too.
//!
//! Each glyph is drawn with `CTFontDrawGlyphs` into a `CGBitmapContext` sized
//! to the glyph's ink bounds:
//!
//! - outline glyphs into an 8-bit DeviceGray context, white on black, so the
//!   gray value is the coverage. It is stored like the FreeType rasterizer's
//!   grayscale glyphs (sRGB-encoded gray, linear coverage as alpha), so the
//!   shaders treat both alike;
//! - glyphs of color fonts (sbix, COLR version 0) into a premultiplied RGBA
//!   sRGB context, which is already the `RasterizedGlyph` layout.
//!
//! Antialiasing is on and font smoothing (stem darkening) is allowed but off,
//! matching Ghostty's default (its `font-thicken` is opt-in). Synthetic italic
//! is a text-matrix skew of [`FAKE_ITALIC_SKEW`]; synthetic bold strokes the
//! outline, about as wide as FreeType's emboldening (ppem / 24).
//!
//! Advances and positions keep coming from the HarfBuzz shaper; this module
//! only produces bitmaps and their bearings, so the metrics plumbing is the
//! same as with FreeType. Faces CoreText cannot load the same way FreeType
//! does fall back to FreeType (see [`CoreTextRasterizer::from_locator`]):
//! collection faces held in memory, variable-font named instances, and color
//! fonts whose color data CoreText draws blank: COLR version 1 paint graphs
//! (the bundled Noto Color Emoji) and CBDT bitmaps.
//! `font_rasterizer = "FreeType"` switches CoreText off entirely.

use crate::locator::FontDataSource;
use crate::parser::ParsedFont;
use crate::rasterizer::{FontRasterizer, FAKE_ITALIC_SKEW};
use crate::units::PixelLength;
use crate::RasterizedGlyph;
use anyhow::{anyhow, bail, Context as _};
use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::url::CFURL;
use core_graphics::base::{kCGBitmapByteOrder32Big, kCGImageAlphaPremultipliedLast, CGFloat};
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::{CGContext, CGTextDrawingMode};
use core_graphics::geometry::{CGAffineTransform, CGPoint, CGRect};
use core_text::font::CTFont;
use core_text::font_descriptor::{
    kCTFontColorGlyphsTrait, kCTFontOrientationDefault, CTFontDescriptor,
};
use std::cell::RefCell;
use wezterm_color_types::linear_u8_to_srgb8;

/// `kCGImageAlphaNone`: the 8-bit gray context has no alpha channel.
const IMAGE_ALPHA_NONE: u32 = 0;

/// Largest bitmap side drawn; anything bigger is an absurd size request.
const MAX_GLYPH_EXTENT: usize = 4096;

/// Transparent margin around the ink bounds so antialiased edges are never
/// clipped; cropped away after drawing.
const PADDING: f64 = 1.0;

/// An OpenType table tag as CoreText takes it.
const fn tag(name: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*name)
}

/// Why a face goes to FreeType instead, from the tables it carries
/// (`colr_version` is the COLR table's version, if it has one). CoreText draws
/// sbix and COLR version 0 color glyphs, but neither COLR version 1 paint
/// graphs nor CBDT/CBLC bitmaps: those faces come out blank (every emoji of
/// the bundled COLRv1 Noto Color Emoji did, measured in the image-parity
/// corpus). FreeType draws them, COLR through its HarfBuzz COLR painter.
pub(crate) fn unsupported_color_format(
    has_cbdt: bool,
    has_sbix: bool,
    colr_version: Option<u16>,
) -> bool {
    !has_sbix
        && match colr_version {
            Some(0) => false,
            Some(_) => true,
            None => has_cbdt,
        }
}

/// The version of a COLR table, from its first two bytes.
pub(crate) fn colr_table_version(table: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes([*table.first()?, *table.get(1)?]))
}

/// The pixel box that holds `rect` (in pixels, y up) after an x-shear of
/// `skew`, grown by `outset` on every side: `(x0, y0, x1, y1)`, integral.
pub(crate) fn pixel_box(rect: CGRect, skew: f64, outset: f64) -> (f64, f64, f64, f64) {
    let (min_x, min_y) = (rect.origin.x, rect.origin.y);
    let (max_x, max_y) = (min_x + rect.size.width, min_y + rect.size.height);
    // x' = x + skew * y, at the four corners.
    let xs = [
        min_x + skew * min_y,
        min_x + skew * max_y,
        max_x + skew * min_y,
        max_x + skew * max_y,
    ];
    let left = xs.iter().copied().fold(f64::INFINITY, f64::min);
    let right = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (
        (left - outset).floor(),
        (min_y - outset).floor(),
        (right + outset).ceil(),
        (max_y + outset).ceil(),
    )
}

/// The bounds of the non-transparent pixels of a `width x height` image whose
/// alpha is `alpha(x, y)`: `(left, top, right, bottom)`, exclusive end, or
/// `None` when every pixel is transparent.
pub(crate) fn ink_bounds(
    width: usize,
    height: usize,
    alpha: impl Fn(usize, usize) -> u8,
) -> Option<(usize, usize, usize, usize)> {
    let (mut left, mut top, mut right, mut bottom) = (width, height, 0, 0);
    for y in 0..height {
        for x in 0..width {
            if alpha(x, y) != 0 {
                left = left.min(x);
                top = top.min(y);
                right = right.max(x + 1);
                bottom = bottom.max(y + 1);
            }
        }
    }
    (right > left).then_some((left, top, right, bottom))
}

/// The face, at its loaded size, plus a per-size clone for drawing.
pub struct CoreTextRasterizer {
    font: CTFont,
    sized: RefCell<Option<(u64, CTFont)>>,
    has_color: bool,
    synthesize_bold: bool,
    synthesize_italic: bool,
    scale: f64,
}

impl CoreTextRasterizer {
    /// Loads the face behind `parsed`. Errors mean "use FreeType for this
    /// face": the caller falls back.
    pub fn from_locator(parsed: &ParsedFont) -> anyhow::Result<Self> {
        let handle = &parsed.handle;
        if handle.variation != 0 {
            bail!(
                "variable-font named instance {} is drawn by FreeType",
                handle.variation
            );
        }
        let font = match &handle.source {
            FontDataSource::OnDisk(path) => {
                let descriptor = collection_descriptor(path, handle.index)?;
                core_text::font::new_from_descriptor(&descriptor, 12.0)
            }
            FontDataSource::BuiltIn { data, .. } => font_from_buffer(data, handle.index)?,
            FontDataSource::Memory { data, .. } => font_from_buffer(data, handle.index)?,
        };
        let has_table = |name: &[u8; 4]| font.get_font_table(tag(name)).is_some();
        let colr_version = font
            .get_font_table(tag(b"COLR"))
            .and_then(|table| colr_table_version(table.bytes()));
        if unsupported_color_format(has_table(b"CBDT"), has_table(b"sbix"), colr_version) {
            bail!("its color glyphs are CBDT bitmaps or COLR v1 paint graphs, which CoreText does not draw");
        }
        let has_color = font.symbolic_traits() & kCTFontColorGlyphsTrait != 0;
        Ok(Self {
            font,
            sized: RefCell::new(None),
            has_color,
            synthesize_bold: parsed.synthesize_bold,
            synthesize_italic: parsed.synthesize_italic,
            scale: parsed.scale.unwrap_or(1.0),
        })
    }

    /// Whether this face draws color glyphs (Apple Color Emoji, COLR).
    pub fn has_color(&self) -> bool {
        self.has_color
    }

    /// The face at `pixel_size` (one CoreText point per pixel), cached for
    /// the last size asked for.
    fn at_size(&self, pixel_size: f64) -> CTFont {
        let key = pixel_size.to_bits();
        let mut sized = self.sized.borrow_mut();
        match sized.as_ref() {
            Some((cached, font)) if *cached == key => font.clone(),
            _ => {
                let font = self.font.clone_with_font_size(pixel_size);
                *sized = Some((key, font.clone()));
                font
            }
        }
    }

    /// The horizontal advance of `glyph` at `pixel_size`, in pixels.
    pub fn advance(&self, glyph: u16, pixel_size: f64) -> f64 {
        let font = self.at_size(pixel_size);
        let glyphs = [glyph];
        let mut advances = [core_graphics::geometry::CGSize::new(0.0, 0.0)];
        // SAFETY: both pointers address live one-element arrays and the count
        // is 1, matching CTFontGetAdvancesForGlyphs's contract.
        unsafe {
            font.get_advances_for_glyphs(
                kCTFontOrientationDefault,
                glyphs.as_ptr(),
                advances.as_mut_ptr(),
                1,
            );
        }
        advances[0].width
    }
}

/// The `index`th face of the font file at `path` (collections hold several).
fn collection_descriptor(path: &std::path::Path, index: u32) -> anyhow::Result<CTFontDescriptor> {
    let url = CFURL::from_path(path, false)
        .ok_or_else(|| anyhow!("{} is not a file URL", path.display()))?;
    // SAFETY: CTFontManagerCreateFontDescriptorsFromURL takes a valid CFURL
    // (borrowed for the call) and returns a +1 CFArray of CTFontDescriptor or
    // NULL; it is checked for NULL before being wrapped.
    let raw = unsafe {
        core_text::font_manager::CTFontManagerCreateFontDescriptorsFromURL(
            url.as_concrete_TypeRef(),
        )
    };
    if raw.is_null() {
        bail!("CoreText found no fonts in {}", path.display());
    }
    // SAFETY: `raw` is a non-NULL array returned under the create rule, so
    // wrapping it transfers that single reference to the CFArray.
    let descriptors: CFArray<CTFontDescriptor> = unsafe { CFArray::wrap_under_create_rule(raw) };
    let position = isize::try_from(index).context("collection index")?;
    let descriptor = descriptors.get(position).ok_or_else(|| {
        anyhow!(
            "{} holds {} faces; face {index} is missing",
            path.display(),
            descriptors.len()
        )
    })?;
    // A retained copy, so the descriptor outlives the array.
    Ok((*descriptor).clone())
}

fn font_from_buffer(data: &[u8], index: u32) -> anyhow::Result<CTFont> {
    if index != 0 {
        bail!("face {index} of an in-memory collection is drawn by FreeType");
    }
    core_text::font::new_from_buffer(data)
        .map_err(|()| anyhow!("CoreText cannot parse the font data"))
}

impl FontRasterizer for CoreTextRasterizer {
    fn rasterize_glyph(
        &self,
        glyph_pos: u32,
        size: f64,
        dpi: u32,
    ) -> anyhow::Result<RasterizedGlyph> {
        let glyph = u16::try_from(glyph_pos).context("glyph index beyond 65535")?;
        let pixel_size = size * self.scale * f64::from(dpi) / 72.0;
        if !pixel_size.is_finite() || pixel_size <= 0.0 {
            bail!("invalid glyph size {size} pt at {dpi} dpi");
        }
        let font = self.at_size(pixel_size);
        let ink = font.get_bounding_rects_for_glyphs(kCTFontOrientationDefault, &[glyph]);
        if ink.size.width <= 0.0 || ink.size.height <= 0.0 {
            return Ok(empty_glyph(self.has_color));
        }
        let skew = if self.synthesize_italic {
            FAKE_ITALIC_SKEW
        } else {
            0.0
        };
        let stroke = if self.synthesize_bold {
            (pixel_size / 24.0).max(0.5)
        } else {
            0.0
        };
        let (x0, y0, x1, y1) = pixel_box(ink, skew, PADDING + stroke / 2.0);
        // Integral and positive by construction; bounded below.
        let (width, height) = ((x1 - x0) as usize, (y1 - y0) as usize);
        if width == 0 || height == 0 || width > MAX_GLYPH_EXTENT || height > MAX_GLYPH_EXTENT {
            bail!("glyph {glyph} at {pixel_size:.1} px needs a {width}x{height} bitmap");
        }
        let (bytes_per_pixel, mut context) = if self.has_color {
            let space = srgb_color_space();
            let context = CGContext::create_bitmap_context(
                None,
                width,
                height,
                8,
                width * 4,
                &space,
                kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big,
            );
            (4, context)
        } else {
            let space = CGColorSpace::create_device_gray();
            let context = CGContext::create_bitmap_context(
                None,
                width,
                height,
                8,
                width,
                &space,
                IMAGE_ALPHA_NONE,
            );
            (1, context)
        };
        context.set_allows_antialiasing(true);
        context.set_should_antialias(true);
        context.set_allows_font_smoothing(true);
        context.set_should_smooth_fonts(false);
        context.set_allows_font_subpixel_positioning(true);
        context.set_should_subpixel_position_fonts(true);
        context.set_allows_font_subpixel_quantization(false);
        context.set_should_subpixel_quantize_fonts(false);
        context.set_rgb_fill_color(1.0, 1.0, 1.0, 1.0);
        context.set_rgb_stroke_color(1.0, 1.0, 1.0, 1.0);
        context.set_text_matrix(&CGAffineTransform::new(
            1.0,
            0.0,
            skew as CGFloat,
            1.0,
            0.0,
            0.0,
        ));
        if stroke > 0.0 {
            context.set_text_drawing_mode(CGTextDrawingMode::CGTextFillStroke);
            context.set_line_width(stroke);
        }
        font.draw_glyphs(&[glyph], &[CGPoint::new(-x0, -y0)], context.clone());

        // Rows are stored top first; the bitmap's top edge is y1 above the
        // baseline and its left edge x0 right of the pen.
        let pixels = context.data();
        let alpha = |x: usize, y: usize| match bytes_per_pixel {
            4 => pixels[(y * width + x) * 4 + 3],
            _ => pixels[y * width + x],
        };
        let Some((left, top, right, bottom)) = ink_bounds(width, height, alpha) else {
            return Ok(empty_glyph(self.has_color));
        };
        let (out_width, out_height) = (right - left, bottom - top);
        let mut data = Vec::with_capacity(out_width * out_height * 4);
        for y in top..bottom {
            for x in left..right {
                if bytes_per_pixel == 4 {
                    let at = (y * width + x) * 4;
                    data.extend_from_slice(&pixels[at..at + 4]);
                } else {
                    let coverage = pixels[y * width + x];
                    let gray = linear_u8_to_srgb8(coverage);
                    data.extend_from_slice(&[gray, gray, gray, coverage]);
                }
            }
        }
        Ok(RasterizedGlyph {
            data,
            width: out_width,
            height: out_height,
            bearing_x: PixelLength::new(x0 + left as f64),
            bearing_y: PixelLength::new(y1 - top as f64),
            has_color: self.has_color,
            is_scaled: true,
        })
    }
}

fn empty_glyph(has_color: bool) -> RasterizedGlyph {
    RasterizedGlyph {
        data: Vec::new(),
        width: 0,
        height: 0,
        bearing_x: PixelLength::new(0.0),
        bearing_y: PixelLength::new(0.0),
        has_color,
        is_scaled: true,
    }
}

/// The sRGB color space color glyphs are drawn in, to match the sRGB
/// textures the renderer samples.
fn srgb_color_space() -> CGColorSpace {
    // SAFETY: kCGColorSpaceSRGB is an immutable CFString constant exported
    // by CoreGraphics; reading it is a plain load.
    let name = unsafe { core_graphics::color_space::kCGColorSpaceSRGB };
    CGColorSpace::create_with_name(name).unwrap_or_else(CGColorSpace::create_device_rgb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::FontDatabase;
    use crate::locator::{FontDataHandle, FontOrigin};
    use crate::rasterizer::freetype::FreeTypeRasterizer;
    use crate::shaper::harfbuzz::HarfbuzzShaper;
    use crate::shaper::FontShaper;
    use config::FontAttributes;
    use core_graphics::geometry::CGSize;
    use wezterm_bidi::Direction;

    const SIZE_PT: f64 = 13.0;
    const DPI: u32 = 144;

    fn jetbrains_mono() -> ParsedFont {
        let db = FontDatabase::with_built_in().expect("built-in fonts");
        db.resolve(
            &FontAttributes {
                family: "JetBrains Mono".into(),
                ..FontAttributes::default()
            },
            26,
        )
        .expect("JetBrains Mono is built in")
        .clone()
    }

    fn system_font(path: &str, index: u32) -> ParsedFont {
        ParsedFont::from_locator(&FontDataHandle {
            source: FontDataSource::OnDisk(path.into()),
            index,
            variation: 0,
            origin: FontOrigin::CoreText,
            coverage: None,
        })
        .unwrap_or_else(|err| panic!("{path} face {index}: {err:#}"))
    }

    /// Glyph ids and advances HarfBuzz shapes `text` into, at 13 pt / 144 dpi.
    fn shaped(font: &ParsedFont, text: &str) -> Vec<(u32, f64)> {
        let config = config::configuration();
        let shaper = HarfbuzzShaper::new(&config, std::slice::from_ref(font)).expect("shaper");
        shaper
            .shape(
                text,
                SIZE_PT,
                DPI,
                &mut Vec::new(),
                None,
                Direction::LeftToRight,
                None,
                None,
            )
            .expect("shape")
            .iter()
            .map(|info| (info.glyph_pos, info.x_advance.get()))
            .collect()
    }

    fn coverage(glyph: &RasterizedGlyph) -> u64 {
        glyph.data.chunks(4).map(|px| u64::from(px[3])).sum()
    }

    /// The glyph `character` maps to in `font`, via CoreText's cmap lookup.
    fn glyph_for(font: &CTFont, character: char) -> Option<u16> {
        let mut units = [0u16; 2];
        let len = character.encode_utf16(&mut units).len();
        let mut glyphs = [0u16; 2];
        // SAFETY: `units` holds `len` (1 or 2) UTF-16 code units and
        // `glyphs` has room for as many glyphs, the count passed.
        let found = unsafe {
            font.get_glyphs_for_characters(units.as_ptr(), glyphs.as_mut_ptr(), len as isize)
        };
        (found && glyphs[0] != 0).then_some(glyphs[0])
    }

    #[test]
    fn cbdt_and_colr_v1_color_fonts_fall_back() {
        assert!(unsupported_color_format(true, false, None), "CBDT only");
        assert!(unsupported_color_format(false, false, Some(1)), "COLR v1");
        assert!(
            unsupported_color_format(true, false, Some(1)),
            "CBDT and COLR v1"
        );
        assert!(!unsupported_color_format(true, true, None), "sbix is drawn");
        assert!(
            !unsupported_color_format(false, true, Some(1)),
            "sbix is drawn"
        );
        assert!(
            !unsupported_color_format(true, false, Some(0)),
            "COLR v0 is drawn"
        );
        assert!(
            !unsupported_color_format(false, false, None),
            "outline fonts"
        );
        assert_eq!(colr_table_version(&[0, 1, 0, 0]), Some(1));
        assert_eq!(colr_table_version(&[0]), None);
    }

    /// The bundled Noto Color Emoji is COLR v1: CoreText would draw it blank,
    /// so the CoreText selection rasterizes it through FreeType, in color.
    #[test]
    fn colr_v1_emoji_rasterize_in_color_through_the_freetype_fallback() {
        let db = FontDatabase::with_built_in().expect("built-in fonts");
        let font = db
            .resolve(
                &FontAttributes {
                    family: "Noto Color Emoji".into(),
                    ..FontAttributes::default()
                },
                26,
            )
            .expect("Noto Color Emoji is built in")
            .clone();
        let reason = CoreTextRasterizer::from_locator(&font)
            .err()
            .expect("CoreText refuses a COLR v1 face");
        assert!(format!("{reason:#}").contains("COLR v1"), "{reason:#}");
        let rasterizer = crate::rasterizer::new_rasterizer(
            config::FontRasterizerSelection::CoreText,
            &font,
            Default::default(),
        )
        .expect("FreeType fallback");
        let (glyph, _) = shaped(&font, "\u{1F600}")[0];
        let raster = rasterizer.rasterize_glyph(glyph, SIZE_PT, DPI).unwrap();
        assert!(
            raster.has_color && coverage(&raster) > 0,
            "U+1F600 drew nothing"
        );
        assert!(
            raster
                .data
                .chunks(4)
                .any(|px| px[0] != px[1] || px[1] != px[2]),
            "U+1F600 came out gray"
        );
    }

    #[test]
    fn pixel_boxes_cover_skew_and_outset() {
        let rect = CGRect::new(&CGPoint::new(1.2, -3.5), &CGSize::new(5.0, 10.0));
        assert_eq!(pixel_box(rect, 0.0, 0.0), (1.0, -4.0, 7.0, 7.0));
        assert_eq!(pixel_box(rect, 0.0, 1.0), (0.0, -5.0, 8.0, 8.0));
        // x + 0.2 * y over the corners: 1.2 - 0.7 = 0.5 .. 6.2 + 1.3 = 7.5.
        assert_eq!(pixel_box(rect, 0.2, 0.0), (0.0, -4.0, 8.0, 7.0));
    }

    #[test]
    fn ink_bounds_find_the_opaque_rectangle() {
        let image = [[0, 0, 0, 0], [0, 9, 0, 0], [0, 0, 7, 0]];
        assert_eq!(ink_bounds(4, 3, |x, y| image[y][x]), Some((1, 1, 3, 3)));
        assert_eq!(ink_bounds(4, 3, |_, _| 0), None);
    }

    #[test]
    fn ascii_glyphs_are_gray_with_freetype_like_geometry() {
        let font = jetbrains_mono();
        let coretext =
            CoreTextRasterizer::from_locator(&font).expect("CoreText loads JetBrains Mono");
        assert!(!coretext.has_color());
        let freetype =
            FreeTypeRasterizer::from_locator(&font, Default::default()).expect("FreeType");
        for (glyph, _) in shaped(&font, "Hgy@") {
            let ct = coretext.rasterize_glyph(glyph, SIZE_PT, DPI).unwrap();
            let ft = freetype.rasterize_glyph(glyph, SIZE_PT, DPI).unwrap();
            assert!(!ct.has_color && ct.is_scaled);
            assert_eq!(ct.data.len(), ct.width * ct.height * 4);
            assert!(coverage(&ct) > 0, "glyph {glyph} drew nothing");
            assert!(
                ct.data.chunks(4).all(|px| px[0] == px[1] && px[1] == px[2]),
                "gray"
            );
            let close = |a: usize, b: usize| a.abs_diff(b) <= 2;
            assert!(
                close(ct.width, ft.width),
                "glyph {glyph} width {} vs FreeType {}",
                ct.width,
                ft.width
            );
            assert!(
                close(ct.height, ft.height),
                "glyph {glyph} height {} vs FreeType {}",
                ct.height,
                ft.height
            );
            assert!(
                (ct.bearing_x.get() - ft.bearing_x.get()).abs() <= 1.5,
                "glyph {glyph} bearing_x {} vs FreeType {}",
                ct.bearing_x.get(),
                ft.bearing_x.get()
            );
            assert!(
                (ct.bearing_y.get() - ft.bearing_y.get()).abs() <= 1.5,
                "glyph {glyph} bearing_y {} vs FreeType {}",
                ct.bearing_y.get(),
                ft.bearing_y.get()
            );
        }
    }

    #[test]
    fn coretext_advances_agree_with_harfbuzz_shaping() {
        let font = jetbrains_mono();
        let coretext = CoreTextRasterizer::from_locator(&font).unwrap();
        let pixel_size = SIZE_PT * f64::from(DPI) / 72.0;
        let (mut shaped_pen, mut coretext_pen) = (0.0, 0.0);
        for (glyph, advance) in shaped(&font, "Hello, World! 0123 {}[]") {
            let ct = coretext.advance(u16::try_from(glyph).unwrap(), pixel_size);
            assert!(
                (ct - advance).abs() <= 0.5,
                "glyph {glyph}: CoreText {ct} vs HarfBuzz {advance}"
            );
            shaped_pen += advance;
            coretext_pen += ct;
        }
        assert!(
            (shaped_pen - coretext_pen).abs() <= 1.0,
            "pen positions drift: HarfBuzz {shaped_pen} vs CoreText {coretext_pen}"
        );
    }

    #[test]
    fn synthetic_italic_slants_and_synthetic_bold_thickens() {
        let regular_font = jetbrains_mono();
        let glyph = shaped(&regular_font, "l")[0].0;
        let regular = CoreTextRasterizer::from_locator(&regular_font)
            .unwrap()
            .rasterize_glyph(glyph, SIZE_PT, DPI)
            .unwrap();
        let mut italic_font = regular_font.clone();
        italic_font.synthesize_italic = true;
        let italic = CoreTextRasterizer::from_locator(&italic_font)
            .unwrap()
            .rasterize_glyph(glyph, SIZE_PT, DPI)
            .unwrap();
        // A 0.2 skew over the glyph's height widens an upright stem.
        assert!(
            italic.width > regular.width + 2,
            "{} vs {}",
            italic.width,
            regular.width
        );
        let mut bold_font = regular_font.clone();
        bold_font.synthesize_bold = true;
        let bold = CoreTextRasterizer::from_locator(&bold_font)
            .unwrap()
            .rasterize_glyph(glyph, SIZE_PT, DPI)
            .unwrap();
        assert!(
            coverage(&bold) > coverage(&regular) * 11 / 10,
            "bold covers {} vs regular {}",
            coverage(&bold),
            coverage(&regular)
        );
    }

    #[test]
    fn size_and_dpi_scale_the_bitmap() {
        let font = jetbrains_mono();
        let coretext = CoreTextRasterizer::from_locator(&font).unwrap();
        let glyph = shaped(&font, "H")[0].0;
        let small = coretext.rasterize_glyph(glyph, SIZE_PT, 72).unwrap();
        let large = coretext.rasterize_glyph(glyph, SIZE_PT, 144).unwrap();
        let bigger = coretext.rasterize_glyph(glyph, SIZE_PT * 2.0, 144).unwrap();
        let ratio = large.height as f64 / small.height as f64;
        assert!(
            (1.8..=2.2).contains(&ratio),
            "dpi doubling scaled height by {ratio}"
        );
        let ratio = bigger.height as f64 / large.height as f64;
        assert!(
            (1.8..=2.2).contains(&ratio),
            "size doubling scaled height by {ratio}"
        );
        // Returning to a size re-renders identically (the per-size cache).
        assert_eq!(
            coretext.rasterize_glyph(glyph, SIZE_PT, 72).unwrap().data,
            small.data
        );
    }

    #[test]
    fn spaces_rasterize_empty() {
        let font = jetbrains_mono();
        let coretext = CoreTextRasterizer::from_locator(&font).unwrap();
        let space = shaped(&font, " ")[0].0;
        let glyph = coretext.rasterize_glyph(space, SIZE_PT, DPI).unwrap();
        assert_eq!((glyph.width, glyph.height, glyph.data.len()), (0, 0, 0));
    }

    /// The operator's T0 pool: every codepoint of these ranges.
    const T0_EMOJI_RANGES: [(u32, u32); 5] = [
        (0x1F600, 0x1F64F),
        (0x1F300, 0x1F5FF),
        (0x1F680, 0x1F6FF),
        (0x1F900, 0x1F9FF),
        (0x1FA70, 0x1FAFF),
    ];

    #[test]
    fn apple_color_emoji_rasterizes_the_t0_set_in_color() {
        let font = system_font("/System/Library/Fonts/Apple Color Emoji.ttc", 0);
        let coretext =
            CoreTextRasterizer::from_locator(&font).expect("CoreText loads Apple Color Emoji");
        assert!(coretext.has_color());
        let pixel_size = SIZE_PT * f64::from(DPI) / 72.0;
        let sized = coretext.at_size(pixel_size);
        let (mut drawn, mut missing) = (0, Vec::new());
        for (first, last) in T0_EMOJI_RANGES {
            for codepoint in first..=last {
                let character = char::from_u32(codepoint).unwrap();
                let Some(glyph) = glyph_for(&sized, character) else {
                    missing.push(codepoint);
                    continue;
                };
                let raster = coretext
                    .rasterize_glyph(u32::from(glyph), SIZE_PT, DPI)
                    .unwrap();
                assert!(raster.has_color, "U+{codepoint:X}");
                assert!(coverage(&raster) > 0, "U+{codepoint:X} drew nothing");
                assert!(
                    raster
                        .data
                        .chunks(4)
                        .any(|px| px[0] != px[1] || px[1] != px[2]),
                    "U+{codepoint:X} came out gray"
                );
                // Premultiplied: no channel exceeds its alpha.
                assert!(raster
                    .data
                    .chunks(4)
                    .all(|px| px[0] <= px[3] && px[1] <= px[3] && px[2] <= px[3]));
                drawn += 1;
            }
        }
        eprintln!(
            "Apple Color Emoji: {drawn} of 1376 T0 codepoints drawn in color; missing (fallback per ft-yccm0.4.3.5): {}",
            missing.iter().map(|cp| format!("U+{cp:X}")).collect::<Vec<_>>().join(" ")
        );
        assert_eq!(drawn + missing.len(), 1376);
        assert!(drawn >= 1300, "only {drawn} of the T0 emoji have glyphs");
    }

    #[test]
    fn collection_faces_load_by_index() {
        let path = "/System/Library/Fonts/Hiragino Sans GB.ttc";
        let light = CoreTextRasterizer::from_locator(&system_font(path, 0)).unwrap();
        let heavy = CoreTextRasterizer::from_locator(&system_font(path, 1)).unwrap();
        let pixel_size = SIZE_PT * f64::from(DPI) / 72.0;
        let glyph_light = glyph_for(&light.at_size(pixel_size), '中').expect("中 in face 0");
        let glyph_heavy = glyph_for(&heavy.at_size(pixel_size), '中').expect("中 in face 1");
        let light = light
            .rasterize_glyph(u32::from(glyph_light), SIZE_PT, DPI)
            .unwrap();
        let heavy = heavy
            .rasterize_glyph(u32::from(glyph_heavy), SIZE_PT, DPI)
            .unwrap();
        assert!(
            coverage(&heavy) > coverage(&light),
            "face 1 (W6) is heavier than face 0 (W3): {} vs {}",
            coverage(&heavy),
            coverage(&light)
        );
        let missing = CoreTextRasterizer::from_locator(&system_font(path, 0)).map(|_| ());
        assert!(missing.is_ok());
        assert!(collection_descriptor(std::path::Path::new(path), 99).is_err());
    }

    #[test]
    fn in_memory_collection_faces_and_named_instances_fall_back() {
        assert!(font_from_buffer(b"not a font", 1).is_err());
        let mut font = jetbrains_mono();
        font.handle.variation = 3;
        assert!(CoreTextRasterizer::from_locator(&font).is_err());
    }
}
