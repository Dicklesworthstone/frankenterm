use crate::color::{LinearRgba, SrgbaPixel};
use crate::{Point, Rect, Size};
use anyhow::{anyhow, ensure};
use downcast_rs::{Downcast, impl_downcast};
use glium::texture::SrgbTexture2d;
use std::cell::RefCell;

pub mod atlas;

pub struct TextureUnit;
pub type TextureCoord = euclid::Point2D<f32, TextureUnit>;
pub type TextureRect = euclid::Rect<f32, TextureUnit>;
pub type TextureSize = euclid::Size2D<f32, TextureUnit>;

fn checked_pixel_count(width: usize, height: usize) -> usize {
    width
        .checked_mul(height)
        .expect("image dimensions overflow pixel count")
}

fn checked_bgra32_len(width: usize, height: usize) -> usize {
    checked_pixel_count(width, height)
        .checked_mul(4)
        .expect("image dimensions overflow bgra32 byte length")
}

fn checked_bgra32_offset(width: usize, x: usize, y: usize) -> usize {
    y.checked_mul(width)
        .and_then(|row| row.checked_add(x))
        .and_then(|pixel| pixel.checked_mul(4))
        .expect("image pixel offset overflow")
}

fn assert_pixel_in_bounds(width: usize, height: usize, x: usize, y: usize) {
    assert!(
        x < width && y < height,
        "x={} width={} y={} height={}",
        x,
        width,
        y,
        height
    );
}

fn assert_horizontal_range_in_bounds(width: usize, height: usize, x1: usize, x2: usize, y: usize) {
    assert!(
        x1 <= x2 && x2 <= width && y < height,
        "x1={} x2={} width={} y={} height={}",
        x1,
        x2,
        width,
        y,
        height
    );
}

/// Represents a big endian bgra32 bitmap that may not be present
/// in local RAM, but may be addressable in eg: video RAM
pub trait Texture2d: Downcast {
    /// Copy the bits from the source bitmap to the texture at the location
    /// specified by the rectangle.
    /// The dimensions of the rectangle must match the source image
    fn write(&self, rect: Rect, im: &dyn BitmapImage);

    /// Returns whether this texture implementation exposes synchronous CPU
    /// readback through [`Texture2d::read`].
    fn supports_readback(&self) -> bool {
        true
    }

    /// Copy the bits from the texture at the location specified by the rectangle
    /// into the bitmap image.
    ///
    /// Readback is tightly packed into the destination image with no row padding.
    /// Implementations return an error when readback is unsupported or when the
    /// requested rectangle/destination image cannot be satisfied.
    fn read(&self, rect: Rect, im: &mut dyn BitmapImage) -> anyhow::Result<()>;

    /// Returns the width of the texture in pixels
    fn width(&self) -> usize;

    /// Returns the height of the texture in pixels
    fn height(&self) -> usize;

    /// Converts a rect in pixel coordinates to texture coordinates
    fn to_texture_coords(&self, coords: Rect) -> TextureRect {
        let coords = coords.to_f32();
        let width = self.width() as f32;
        let height = self.height() as f32;
        TextureRect::new(
            TextureCoord::new(coords.min_x() / width, coords.min_y() / height),
            TextureSize::new(coords.size.width / width, coords.size.height / height),
        )
    }
}
impl_downcast!(Texture2d);

fn unsupported_texture_readback_error(texture_kind: &str) -> anyhow::Error {
    anyhow!("{texture_kind} does not expose synchronous Texture2d::read CPU readback")
}

/// A validated, tightly packed CPU-readback request expressed in source-texture
/// coordinates. The destination bitmap is expected to have the same dimensions
/// as `width` x `height`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureReadbackRequest {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}

pub fn validate_texture_readback_request(
    texture_width: usize,
    texture_height: usize,
    rect: Rect,
    im: &dyn BitmapImage,
) -> anyhow::Result<TextureReadbackRequest> {
    ensure!(
        rect.origin.x >= 0 && rect.origin.y >= 0,
        "texture readback rect origin must be non-negative: {:?}",
        rect
    );
    ensure!(
        rect.size.width >= 0 && rect.size.height >= 0,
        "texture readback rect size must be non-negative: {:?}",
        rect
    );

    let width = rect.size.width as usize;
    let height = rect.size.height as usize;
    let (dest_width, dest_height) = im.image_dimensions();

    ensure!(
        (dest_width, dest_height) == (width, height),
        "texture readback destination dimensions {}x{} do not match requested rect {}x{}",
        dest_width,
        dest_height,
        width,
        height
    );

    let left = rect.origin.x as usize;
    let top = rect.origin.y as usize;
    let right = left
        .checked_add(width)
        .ok_or_else(|| anyhow!("texture readback rect overflows width bounds: {:?}", rect))?;
    let bottom = top
        .checked_add(height)
        .ok_or_else(|| anyhow!("texture readback rect overflows height bounds: {:?}", rect))?;

    ensure!(
        right <= texture_width && bottom <= texture_height,
        "texture readback rect {:?} exceeds texture bounds {}x{}",
        rect,
        texture_width,
        texture_height
    );

    Ok(TextureReadbackRequest {
        left: left as u32,
        top: top as u32,
        width: width as u32,
        height: height as u32,
    })
}

fn copy_bitmap_readback(
    source: &dyn BitmapImage,
    request: TextureReadbackRequest,
    dest: &mut dyn BitmapImage,
) {
    for row in 0..request.height as usize {
        let src_y = request.top as usize + row;
        let src = source.horizontal_pixel_range(
            request.left as usize,
            request.left as usize + request.width as usize,
            src_y,
        );
        let dst = dest.horizontal_pixel_range_mut(0, request.width as usize, row);
        dst.copy_from_slice(src);
    }
}

impl Texture2d for SrgbTexture2d {
    fn write(&self, rect: Rect, im: &dyn BitmapImage) {
        let (im_width, im_height) = im.image_dimensions();

        let source = glium::texture::RawImage2d {
            data: std::borrow::Cow::Borrowed(im.pixels()),
            width: im_width as u32,
            height: im_height as u32,
            format: glium::texture::ClientFormat::U8U8U8U8,
        };

        SrgbTexture2d::write(
            self,
            glium::Rect {
                left: rect.min_x() as u32,
                bottom: rect.min_y() as u32,
                width: rect.size.width as u32,
                height: rect.size.height as u32,
            },
            source,
        )
    }

    fn supports_readback(&self) -> bool {
        false
    }

    fn read(&self, _rect: Rect, _im: &mut dyn BitmapImage) -> anyhow::Result<()> {
        Err(unsupported_texture_readback_error("SrgbTexture2d"))
    }

    fn width(&self) -> usize {
        SrgbTexture2d::width(self) as usize
    }

    fn height(&self) -> usize {
        SrgbTexture2d::height(self) as usize
    }
}

/// A bitmap in big endian rbga32 color format with abstract
/// storage filled in by the trait implementation.
pub trait BitmapImage {
    /// Obtain a read only pointer to the pixel data
    /// # Safety
    /// The caller is responsible for ensuring that pixel
    /// access is bounded by the image_dimensions
    unsafe fn pixel_data(&self) -> *const u8;

    /// Obtain a mutable pointer to the pixel data
    /// # Safety
    /// The caller is responsible for ensuring that pixel
    /// access is bounded by the image_dimensions.
    ///
    /// **Pre-condition**: callers MUST check
    /// [`BitmapImage::is_mutable`] returns `true` before calling
    /// this method. Implementors of read-only views (e.g.,
    /// `DecodedImageHandle` in glyphcache.rs) panic when this
    /// pre-condition is violated. (br-ft-82pp1)
    unsafe fn pixel_data_mut(&mut self) -> *mut u8;

    /// br-ft-82pp1: returns `true` if the underlying pixel data
    /// can be mutated through `pixel_data_mut`. Read-only
    /// implementations override to return `false` so callers can
    /// branch BEFORE invoking the unsafe `*mut u8` accessor.
    ///
    /// Default: `true` (mutable, matches the original
    /// `Image`-only contract).
    fn is_mutable(&self) -> bool {
        true
    }

    /// Return the pair (width, height) of the image, measured in pixels
    fn image_dimensions(&self) -> (usize, usize);

    fn pixel_data_slice(&self) -> &[u8] {
        let (width, height) = self.image_dimensions();
        let len = checked_bgra32_len(width, height);
        unsafe {
            let first = self.pixel_data();
            std::slice::from_raw_parts(first, len)
        }
    }

    fn pixel_data_slice_mut(&mut self) -> &mut [u8] {
        assert!(
            self.is_mutable(),
            "BitmapImage::pixel_data_slice_mut called on read-only impl; check is_mutable() first (br-ft-82pp1)"
        );
        let (width, height) = self.image_dimensions();
        let len = checked_bgra32_len(width, height);
        unsafe {
            let first = self.pixel_data_mut();
            std::slice::from_raw_parts_mut(first, len)
        }
    }

    #[inline]
    fn pixels(&self) -> &[u32] {
        let (width, height) = self.image_dimensions();
        let len = checked_pixel_count(width, height);
        unsafe {
            #[allow(clippy::cast_ptr_alignment)]
            let first = self.pixel_data() as *const u32;
            std::slice::from_raw_parts(first, len)
        }
    }

    #[inline]
    fn pixels_mut(&mut self) -> &mut [u32] {
        assert!(
            self.is_mutable(),
            "BitmapImage::pixels_mut called on read-only impl; check is_mutable() first (br-ft-82pp1)"
        );
        let (width, height) = self.image_dimensions();
        let len = checked_pixel_count(width, height);
        unsafe {
            #[allow(clippy::cast_ptr_alignment)]
            let first = self.pixel_data_mut() as *mut u32;
            std::slice::from_raw_parts_mut(first, len)
        }
    }

    #[inline]
    /// Obtain a mutable reference to the raw bgra pixel at the specified coordinates
    fn pixel_mut(&mut self, x: usize, y: usize) -> &mut u32 {
        assert!(
            self.is_mutable(),
            "BitmapImage::pixel_mut called on read-only impl; check is_mutable() first (br-ft-82pp1)"
        );
        let (width, height) = self.image_dimensions();
        assert_pixel_in_bounds(width, height, x, y);
        unsafe {
            let offset = checked_bgra32_offset(width, x, y);
            #[allow(clippy::cast_ptr_alignment)]
            &mut *(self.pixel_data_mut().add(offset) as *mut u32)
        }
    }

    #[inline]
    /// Read the raw bgra pixel at the specified coordinates
    fn pixel(&self, x: usize, y: usize) -> &u32 {
        let (width, height) = self.image_dimensions();
        assert_pixel_in_bounds(width, height, x, y);
        unsafe {
            let offset = checked_bgra32_offset(width, x, y);
            #[allow(clippy::cast_ptr_alignment)]
            &*(self.pixel_data().add(offset) as *const u32)
        }
    }

    #[inline]
    fn horizontal_pixel_range(&self, x1: usize, x2: usize, y: usize) -> &[u32] {
        let (width, height) = self.image_dimensions();
        assert_horizontal_range_in_bounds(width, height, x1, x2, y);
        let offset = checked_bgra32_offset(width, x1, y);
        unsafe {
            #[allow(clippy::cast_ptr_alignment)]
            let first = self.pixel_data().add(offset) as *const u32;
            std::slice::from_raw_parts(first, x2 - x1)
        }
    }

    #[inline]
    fn horizontal_pixel_range_mut(&mut self, x1: usize, x2: usize, y: usize) -> &mut [u32] {
        assert!(
            self.is_mutable(),
            "BitmapImage::horizontal_pixel_range_mut called on read-only impl; check is_mutable() first (br-ft-82pp1)"
        );
        let (width, height) = self.image_dimensions();
        assert_horizontal_range_in_bounds(width, height, x1, x2, y);
        let offset = checked_bgra32_offset(width, x1, y);
        unsafe {
            #[allow(clippy::cast_ptr_alignment)]
            let first = self.pixel_data_mut().add(offset) as *mut u32;
            std::slice::from_raw_parts_mut(first, x2 - x1)
        }
    }

    /// Clear the entire image to the specific color
    fn clear(&mut self, color: SrgbaPixel) {
        for c in self.pixels_mut() {
            *c = color.as_srgba32();
        }
    }

    fn clear_rect(&mut self, rect: Rect, color: SrgbaPixel) {
        let (dim_width, dim_height) = self.image_dimensions();
        let max_x = rect.max_x().min(dim_width as isize) as usize;
        let max_y = rect.max_y().min(dim_height as isize) as usize;

        let dest_x = rect.origin.x.max(0) as usize;
        if dest_x >= dim_width {
            return;
        }
        let dest_y = rect.origin.y.max(0) as usize;

        for y in dest_y..max_y {
            let range = self.horizontal_pixel_range_mut(dest_x, max_x, y);
            for c in range {
                *c = color.as_srgba32();
            }
        }
    }

    /// Draw a line starting at `start` and ending at `end`.
    /// The line will be anti-aliased and applied to the surface.
    fn draw_line(&mut self, start: Point, end: Point, color: SrgbaPixel) {
        let (dim_width, dim_height) = self.image_dimensions();
        let linear = color.to_linear();
        let (red, green, blue, alpha) = linear.tuple();

        for ((x, y), value) in line_drawing::XiaolinWu::<f32, isize>::new(
            (start.x as f32, start.y as f32),
            (end.x as f32, end.y as f32),
        ) {
            if y < 0 || x < 0 {
                continue;
            }
            if y >= dim_height as isize || x >= dim_width as isize {
                continue;
            }
            let pix = self.pixel_mut(x as usize, y as usize);

            let color = LinearRgba::with_components(red, green, blue, alpha * value);
            *pix = color.srgba_pixel().as_srgba32();
        }
    }

    /// Draw a 1-pixel wide rectangle
    fn draw_rect(&mut self, rect: Rect, color: SrgbaPixel) {
        let bottom_right = rect.origin.add_size(&rect.size);

        // Draw the vertical lines down either side
        self.draw_line(
            rect.origin,
            Point::new(rect.origin.x, bottom_right.y),
            color,
        );
        self.draw_line(
            Point::new(bottom_right.x, rect.origin.y),
            bottom_right,
            color,
        );
        // And the horizontals for the top and bottom
        self.draw_line(
            rect.origin,
            Point::new(bottom_right.x, rect.origin.y),
            color,
        );
        self.draw_line(
            Point::new(rect.origin.x, bottom_right.y),
            bottom_right,
            color,
        );
    }

    fn draw_image(&mut self, dest_top_left: Point, src_rect: Option<Rect>, im: &dyn BitmapImage) {
        let (im_width, im_height) = im.image_dimensions();
        let src_rect = src_rect
            .unwrap_or_else(|| Rect::from_size(Size::new(im_width as isize, im_height as isize)));

        let (dim_width, dim_height) = self.image_dimensions();
        debug_assert!(
            src_rect.size.width <= im_width as isize && src_rect.size.height <= im_height as isize
        );

        let desired_width = src_rect.max_x().saturating_sub(src_rect.min_x()).max(0);
        let src_width = desired_width.min(im_width as isize).max(0);
        let dest_rightmost = dest_top_left
            .x
            .saturating_add(src_width)
            .min(dim_width as isize);
        let dest_width = dest_rightmost.saturating_sub(dest_top_left.x).max(0);
        let copy_width = dest_width.min(src_width).max(0);

        let desired_height = src_rect.max_y().saturating_sub(src_rect.min_y()).max(0);
        let src_height = desired_height.min(im_height as isize).max(0);
        let dest_bottommost = dest_top_left
            .y
            .saturating_add(src_height)
            .min(dim_height as isize);
        let dest_height = dest_bottommost.saturating_sub(dest_top_left.y).max(0);
        let copy_height = dest_height.min(src_height).max(0);

        if copy_width == 0 || copy_height == 0 {
            return;
        }

        for y in src_rect.origin.y..src_rect.origin.y + copy_height {
            let dest_y = y + dest_top_left.y - src_rect.origin.y;
            if dest_y < 0 {
                continue;
            }

            let src_pixels = im.horizontal_pixel_range(
                src_rect.min_x() as usize,
                (src_rect.min_x() + copy_width) as usize,
                y as usize,
            );
            let dest_pixels = self.horizontal_pixel_range_mut(
                dest_top_left.x.max(0) as usize,
                (dest_top_left.x + copy_width).max(0) as usize,
                dest_y as usize,
            );
            for (src_pix, dest_pix) in src_pixels.iter().zip(dest_pixels.iter_mut()) {
                *dest_pix = *src_pix;
            }
        }
    }
}

/// A bitmap in big endian bgra32 color format, with storage
/// in a Vec<u8>.
#[derive(Clone)]
pub struct Image {
    data: Vec<u8>,
    width: usize,
    height: usize,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.debug_struct("Image")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

impl From<Image> for Vec<u8> {
    fn from(val: Image) -> Self {
        val.data
    }
}

impl Image {
    /// Create a new bgra32 image buffer with the specified dimensions.
    /// The buffer is initialized to all zeroes.
    pub fn new(width: usize, height: usize) -> Image {
        let size = checked_bgra32_len(width, height);
        let data = vec![0; size];
        Image {
            data,
            width,
            height,
        }
    }

    pub fn from_raw(width: usize, height: usize, data: Vec<u8>) -> Self {
        let expected_len = checked_bgra32_len(width, height);
        assert_eq!(
            data.len(),
            expected_len,
            "raw bgra32 image buffer length does not match dimensions"
        );
        Self {
            data,
            width,
            height,
        }
    }

    /// Create a new bgra32 image buffer with the specified dimensions.
    /// The buffer is populated with the source data in rgba32 format.
    pub fn with_rgba32(width: usize, height: usize, stride: usize, data: &[u8]) -> Image {
        let row_bytes = width
            .checked_mul(4)
            .expect("rgba32 row byte length overflows usize");
        assert!(
            stride >= row_bytes,
            "rgba32 stride is smaller than row byte length"
        );
        let required_len = if height == 0 || row_bytes == 0 {
            0
        } else {
            (height - 1)
                .checked_mul(stride)
                .and_then(|last_row| last_row.checked_add(row_bytes))
                .expect("rgba32 source dimensions overflow byte length")
        };
        assert!(
            data.len() >= required_len,
            "rgba32 source buffer is smaller than dimensions require"
        );

        let mut image = Image::new(width, height);
        for y in 0..height {
            let src_offset = y
                .checked_mul(stride)
                .expect("rgba32 source stride offset overflow");
            let dest_offset = checked_bgra32_offset(width, 0, y);
            #[allow(clippy::identity_op)]
            for x in 0..width {
                let red = data[src_offset + (x * 4) + 0];
                let green = data[src_offset + (x * 4) + 1];
                let blue = data[src_offset + (x * 4) + 2];
                let alpha = data[src_offset + (x * 4) + 3];
                image.data[dest_offset + (x * 4) + 0] = red;
                image.data[dest_offset + (x * 4) + 1] = green;
                image.data[dest_offset + (x * 4) + 2] = blue;
                image.data[dest_offset + (x * 4) + 3] = alpha;
            }
        }
        image
    }

    /// Creates a new image with the contents of the current image, but
    /// resized to the specified dimensions.
    pub fn resize(&self, width: usize, height: usize) -> Image {
        let target_pixels = checked_pixel_count(width, height);
        let source_pixels = checked_pixel_count(self.width, self.height);
        let mut dest = Image::new(width, height);
        let algo = if target_pixels < source_pixels {
            resize::Type::Lanczos3
        } else {
            resize::Type::Mitchell
        };
        resize::new(
            self.width,
            self.height,
            width,
            height,
            resize::Pixel::RGBA,
            algo,
        )
        .resize(&self.data, &mut dest.data);
        dest
    }

    pub fn scale_by(&self, scale: f64) -> Image {
        let width = (self.width as f64 * scale) as usize;
        let height = (self.height as f64 * scale) as usize;
        self.resize(width, height)
    }

    #[allow(dead_code)]
    pub fn log_bits(&self) {
        log::info!("Image pixels:");
        for y in 0..self.height {
            let row = self.horizontal_pixel_range(0, self.width, y);
            let mut line = String::new();
            for p in row {
                line.push_str(&format!("{:08x} ", *p));
            }
            log::info!("{}", line);
        }
    }
}

impl BitmapImage for Image {
    unsafe fn pixel_data(&self) -> *const u8 {
        self.data.as_ptr()
    }

    unsafe fn pixel_data_mut(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    fn image_dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }
}

#[derive(Debug)]
pub struct ImageTexture {
    pub image: RefCell<Image>,
}

impl ImageTexture {
    pub fn new(width: usize, height: usize) -> Self {
        let im = Image::new(width, height);
        Self {
            image: RefCell::new(im),
        }
    }
}

impl Texture2d for ImageTexture {
    fn write(&self, rect: Rect, im: &dyn BitmapImage) {
        let mut image = self.image.borrow_mut();
        image.draw_image(rect.origin, None, im);
    }

    fn read(&self, rect: Rect, im: &mut dyn BitmapImage) -> anyhow::Result<()> {
        let image = self.image.borrow();
        let request = validate_texture_readback_request(self.width(), self.height(), rect, im)?;
        copy_bitmap_readback(&*image, request, im);
        Ok(())
    }

    /// Returns the width of the texture in pixels
    fn width(&self) -> usize {
        let (width, _height) = self.image.borrow().image_dimensions();
        width
    }

    /// Returns the height of the texture in pixels
    fn height(&self) -> usize {
        let (_width, height) = self.image.borrow().image_dimensions();
        height
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BitmapImage, Image, ImageTexture, Texture2d, unsupported_texture_readback_error,
        validate_texture_readback_request,
    };
    use crate::{Point, Rect, Size};

    struct ReadOnlyBitmap {
        pixel: [u8; 4],
    }

    impl BitmapImage for ReadOnlyBitmap {
        unsafe fn pixel_data(&self) -> *const u8 {
            self.pixel.as_ptr()
        }

        unsafe fn pixel_data_mut(&mut self) -> *mut u8 {
            panic!("read-only bitmap should not expose mutable pixels");
        }

        fn is_mutable(&self) -> bool {
            false
        }

        fn image_dimensions(&self) -> (usize, usize) {
            (1, 1)
        }
    }

    fn seed_image(width: usize, height: usize) -> Image {
        let mut image = Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                *image.pixel_mut(x, y) = ((y * width + x) as u32) + 1;
            }
        }
        image
    }

    #[test]
    #[should_panic(expected = "image dimensions overflow bgra32 byte length")]
    fn image_new_rejects_overflowing_dimensions() {
        let _ = Image::new(usize::MAX, 2);
    }

    #[test]
    #[should_panic(expected = "raw bgra32 image buffer length does not match dimensions")]
    fn image_from_raw_rejects_wrong_buffer_length() {
        let _ = Image::from_raw(2, 2, vec![0; 12]);
    }

    #[test]
    #[should_panic(expected = "rgba32 stride is smaller than row byte length")]
    fn image_with_rgba32_rejects_short_stride() {
        let _ = Image::with_rgba32(2, 1, 4, &[0; 4]);
    }

    #[test]
    #[should_panic(expected = "rgba32 source buffer is smaller than dimensions require")]
    fn image_with_rgba32_rejects_short_source_buffer() {
        let _ = Image::with_rgba32(2, 2, 8, &[0; 8]);
    }

    #[test]
    fn image_with_rgba32_allows_zero_width_without_source_rows() {
        let image = Image::with_rgba32(0, 2, 16, &[]);
        assert_eq!(image.image_dimensions(), (0, 2));
        assert!(image.pixel_data_slice().is_empty());
    }

    #[test]
    #[should_panic(expected = "x=1 width=1 y=0 height=1")]
    fn image_pixel_mut_rejects_out_of_bounds_coordinates() {
        let mut image = Image::new(1, 1);
        let _ = image.pixel_mut(1, 0);
    }

    #[test]
    #[should_panic(expected = "x=0 width=1 y=1 height=1")]
    fn image_pixel_rejects_out_of_bounds_coordinates() {
        let image = Image::new(1, 1);
        let _ = image.pixel(0, 1);
    }

    #[test]
    #[should_panic(expected = "x1=0 x2=2 width=1 y=0 height=1")]
    fn image_horizontal_pixel_range_rejects_out_of_bounds_end() {
        let image = Image::new(1, 1);
        let _ = image.horizontal_pixel_range(0, 2, 0);
    }

    #[test]
    fn readonly_bitmap_mut_helpers_panic_before_mut_ptr_access() {
        let mut bytes = ReadOnlyBitmap { pixel: [0; 4] };
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = bytes.pixel_data_slice_mut();
            }))
            .is_err()
        );

        let mut pixels = ReadOnlyBitmap { pixel: [0; 4] };
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = pixels.pixels_mut();
            }))
            .is_err()
        );

        let mut one_pixel = ReadOnlyBitmap { pixel: [0; 4] };
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = one_pixel.pixel_mut(0, 0);
            }))
            .is_err()
        );
    }

    #[test]
    fn image_texture_read_copies_requested_region() {
        let texture = ImageTexture::new(4, 4);
        *texture.image.borrow_mut() = seed_image(4, 4);

        let mut dest = Image::new(2, 2);
        texture
            .read(Rect::new(Point::new(1, 1), Size::new(2, 2)), &mut dest)
            .expect("readback should succeed");

        assert_eq!(*dest.pixel(0, 0), 6);
        assert_eq!(*dest.pixel(1, 0), 7);
        assert_eq!(*dest.pixel(0, 1), 10);
        assert_eq!(*dest.pixel(1, 1), 11);
    }

    #[test]
    fn image_texture_read_allows_empty_capture() {
        let texture = ImageTexture::new(4, 4);
        let mut dest = Image::new(0, 0);

        texture
            .read(Rect::new(Point::new(2, 2), Size::new(0, 0)), &mut dest)
            .expect("empty readback should succeed");
    }

    #[test]
    fn texture_readback_rejects_destination_size_mismatch() {
        let texture = ImageTexture::new(4, 4);
        let mut dest = Image::new(1, 2);
        let err = texture
            .read(Rect::new(Point::new(0, 0), Size::new(2, 2)), &mut dest)
            .expect_err("mismatched destination dimensions should fail");

        assert!(
            err.to_string().contains("destination dimensions"),
            "unexpected error: {err:#}",
            err = err,
        );
    }

    #[test]
    fn validate_texture_readback_request_rejects_out_of_bounds_rects() {
        let dest = Image::new(2, 2);
        let err = validate_texture_readback_request(
            4,
            4,
            Rect::new(Point::new(3, 3), Size::new(2, 2)),
            &dest,
        )
        .expect_err("out-of-bounds readback should fail");

        assert!(
            err.to_string().contains("exceeds texture bounds"),
            "unexpected error: {err:#}",
            err = err,
        );
    }

    #[test]
    fn unsupported_texture_readback_error_is_explicit() {
        let err = unsupported_texture_readback_error("SrgbTexture2d");
        assert!(
            err.to_string()
                .contains("does not expose synchronous Texture2d::read"),
            "unexpected error: {err:#}",
            err = err,
        );
    }
}
