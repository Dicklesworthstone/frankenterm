use crate::bitmaps::{BitmapImage, Texture2d, TextureRect};
use crate::{Point, Rect, Size};
use anyhow::{ensure, Result as Fallible};
use guillotiere::{SimpleAtlasAllocator, Size as AtlasSize};
use std::convert::TryInto;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::*;

const PADDING: i32 = 1;

#[derive(Debug, Error)]
#[error("Texture Size exceeded, need {:?}", size)]
pub struct OutOfTextureSpace {
    pub size: Option<usize>,
    pub current_size: usize,
}

/// Atlases are bitmaps of srgba data that are sized as a power of 2.
/// We allocate sprites out of the available space, using AtlasAllocator
/// to manage the available rectangles.
///
/// ## Versioning
///
/// `Atlas` carries a monotonically increasing `version` counter. The
/// counter is bumped every time the atlas is mutated:
///
/// - successful `allocate` / `allocate_with_padding` (sprite upload)
/// - `clear` (full reset)
///
/// Each [`Sprite`] returned from `allocate*` is stamped with the
/// version at which it was uploaded. Per-frame renderer state can
/// snapshot `atlas.version()` and only re-sync sprites whose
/// stamp is newer than the last-synced cursor — this is the
/// foundation for the ghostty-style "never rebuild on resize" pattern
/// (ft-mpc9b.1.1).
///
/// `AtomicU64` is used for forward compatibility with multi-threaded
/// readers; the current callers all hold the atlas via `Rc` and
/// access it from a single thread.
pub struct Atlas {
    texture: Rc<dyn Texture2d>,

    allocator: SimpleAtlasAllocator,

    /// Dimensions of the texture
    side: usize,

    /// Monotonically increasing modification counter.
    ///
    /// Starts at 0 (initial empty state), bumps on every successful
    /// allocate and on every clear. Readers compare this against
    /// their cached `last_synced_version` to detect drift.
    version: AtomicU64,
}

impl Atlas {
    pub fn new(texture: &Rc<dyn Texture2d>) -> Fallible<Self> {
        ensure!(
            texture.width() == texture.height(),
            "texture must be square!"
        );
        let side = texture.width();
        let iside = side as isize;

        let image = crate::Image::new(side, side);
        let rect = Rect::new(Point::new(0, 0), Size::new(iside, iside));
        texture.write(rect, &image);

        let allocator =
            SimpleAtlasAllocator::new(AtlasSize::new(side.try_into()?, side.try_into()?));

        // Record the atlas footprint so dashboards can surface
        // memory pressure once the grow path lands. SRGBA = 4 bytes
        // per pixel.
        let bytes_estimate = (side as u64).saturating_mul(side as u64).saturating_mul(4);
        metrics::gauge!("window.atlas.size_bytes").set(bytes_estimate as f64);

        Ok(Self {
            texture: Rc::clone(texture),
            side,
            allocator,
            version: AtomicU64::new(0),
        })
    }

    #[inline]
    pub fn texture(&self) -> Rc<dyn Texture2d> {
        Rc::clone(&self.texture)
    }

    /// Current modification version of this atlas.
    ///
    /// Bumps on every successful sprite upload and on every `clear`.
    /// Per-frame state should snapshot this and re-sync only
    /// sprites whose `Sprite::version()` exceeds the snapshot.
    #[inline]
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Reserve space for a sprite of the given size
    pub fn allocate(&mut self, im: &dyn BitmapImage) -> Result<Sprite, OutOfTextureSpace> {
        self.allocate_with_padding(im, None, None)
    }

    pub fn allocate_with_padding(
        &mut self,
        im: &dyn BitmapImage,
        padding: Option<usize>,
        scale_down: Option<usize>,
    ) -> Result<Sprite, OutOfTextureSpace> {
        let (width, height) = im.image_dimensions();

        if let Some(scale_down) = scale_down {
            let mut copied = crate::Image::new(width, height);
            copied.draw_image(Point::new(0, 0), None, im);

            let scaled = copied.resize(width / scale_down, height / scale_down);

            return self.allocate_with_padding(&scaled, padding, None);
        }

        // If we can't convert the sizes to i32, then we'll never
        // be able to store this image
        let reserve_width: i32 = width.try_into().map_err(|_| OutOfTextureSpace {
            size: None,
            current_size: self.side,
        })?;
        let reserve_height: i32 = height.try_into().map_err(|_| OutOfTextureSpace {
            size: None,
            current_size: self.side,
        })?;

        // We pad each sprite reservation with blank space to avoid
        // surprising and unexpected artifacts when the texture is
        // interpolated on to the render surface.
        let reserve_width = reserve_width + padding.unwrap_or(0) as i32 + PADDING * 2;
        let reserve_height = reserve_height + padding.unwrap_or(0) as i32 + PADDING * 2;

        let start = std::time::Instant::now();
        let res = if let Some(allocation) = self
            .allocator
            .allocate(AtlasSize::new(reserve_width, reserve_height))
        {
            let left = allocation.min.x;
            let top = allocation.min.y;
            let rect = Rect::new(
                Point::new((left + PADDING) as isize, (top + PADDING) as isize),
                Size::new(width as isize, height as isize),
            );

            self.texture.write(rect, im);

            // Bump after the texture write so readers that observe the
            // post-bump version always see the sprite's bytes.
            let version = self.version.fetch_add(1, Ordering::AcqRel) + 1;

            metrics::histogram!("window.atlas.allocate.success.rate").record(1.);
            metrics::counter!("window.atlas.uploads.total").increment(1);
            Ok(Sprite {
                texture: Rc::clone(&self.texture),
                coords: rect,
                version,
            })
        } else {
            // It's not possible to satisfy that request
            let size = (reserve_width.max(reserve_height) as usize).next_power_of_two();
            metrics::histogram!("window.atlas.allocate.failure.rate").record(1.);
            Err(OutOfTextureSpace {
                size: Some((self.side * 2).max(size)),
                current_size: self.side,
            })
        };
        metrics::histogram!("window.atlas.allocate.latency").record(start.elapsed());

        res
    }

    pub fn size(&self) -> usize {
        self.side
    }

    /// Zero out the texture, and forget all allocated regions.
    ///
    /// This is the explicit "rebuild" path: every sprite previously
    /// returned by `allocate*` is now stale (its stamped version
    /// predates the post-clear version).
    pub fn clear(&mut self) {
        let iside = self.side as isize;
        let image = crate::Image::new(self.side, self.side);
        let rect = Rect::new(Point::new(0, 0), Size::new(iside, iside));
        self.texture.write(rect, &image);
        self.allocator.clear();
        self.version.fetch_add(1, Ordering::AcqRel);
        metrics::counter!("window.atlas.rebuilds.total").increment(1);
    }
}

pub struct Sprite {
    pub texture: Rc<dyn Texture2d>,
    pub coords: Rect,
    /// The atlas version at which this sprite was uploaded.
    ///
    /// Per-frame state can compare this against its `last_synced_version`
    /// snapshot to decide whether the sprite needs re-syncing — which
    /// for an immutable sprite is essentially "never, until the atlas
    /// is cleared." Atlas growth (out-of-space rebuild) bumps every
    /// re-uploaded sprite to the post-grow version.
    pub version: u64,
}

impl std::fmt::Debug for Sprite {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        fmt.debug_struct("Sprite")
            .field("coords", &self.coords)
            .field("version", &self.version)
            .field("texture_width", &self.texture.width())
            .field("texture_height", &self.texture.height())
            .finish()
    }
}

impl Clone for Sprite {
    fn clone(&self) -> Self {
        Self {
            texture: Rc::clone(&self.texture),
            coords: self.coords,
            version: self.version,
        }
    }
}

impl Sprite {
    /// Returns the texture coordinates of the sprite
    pub fn texture_coords(&self) -> TextureRect {
        self.texture.to_texture_coords(self.coords)
    }

    /// The atlas version at which this sprite was uploaded.
    #[inline]
    pub fn version(&self) -> u64 {
        self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitmaps::ImageTexture;
    use crate::Image;

    fn cell(width: usize, height: usize, byte: u8) -> Image {
        let mut image = Image::new(width, height);
        // Fill via the safe pixel_mut accessor. The exact value
        // doesn't matter — the tests assert on version, not content.
        let pixel =
            (u32::from(byte) << 24) | (u32::from(byte) << 16) | (u32::from(byte) << 8) | 0xff;
        for y in 0..height {
            for x in 0..width {
                *image.pixel_mut(x, y) = pixel;
            }
        }
        image
    }

    fn fresh_atlas(side: usize) -> Atlas {
        let texture: Rc<dyn Texture2d> = Rc::new(ImageTexture::new(side, side));
        Atlas::new(&texture).expect("atlas construction")
    }

    #[test]
    fn version_starts_at_zero() {
        let atlas = fresh_atlas(64);
        assert_eq!(atlas.version(), 0);
    }

    #[test]
    fn version_bumps_on_each_successful_upload() {
        let mut atlas = fresh_atlas(64);
        let baseline = atlas.version();

        let sprite_a = atlas.allocate(&cell(8, 8, 0x10)).expect("allocate a");
        assert_eq!(atlas.version(), baseline + 1);
        assert_eq!(sprite_a.version(), baseline + 1);

        let sprite_b = atlas.allocate(&cell(8, 8, 0x20)).expect("allocate b");
        assert_eq!(atlas.version(), baseline + 2);
        assert_eq!(sprite_b.version(), baseline + 2);

        // Each sprite carries the version it was stamped with.
        assert!(sprite_b.version() > sprite_a.version());
    }

    #[test]
    fn version_bumps_on_clear() {
        let mut atlas = fresh_atlas(64);
        let _ = atlas.allocate(&cell(8, 8, 0x33)).expect("allocate");
        let pre_clear = atlas.version();

        atlas.clear();
        let post_clear = atlas.version();
        assert!(
            post_clear > pre_clear,
            "clear must bump the atlas version: pre={}, post={}",
            pre_clear,
            post_clear,
        );
    }

    #[test]
    fn allocation_failure_does_not_bump_version() {
        // 16x16 atlas, 32x32 sprite — guaranteed to fail allocation.
        let mut atlas = fresh_atlas(16);
        let baseline = atlas.version();

        let result = atlas.allocate(&cell(32, 32, 0x55));
        assert!(result.is_err(), "oversized allocate must fail");
        assert_eq!(
            atlas.version(),
            baseline,
            "failed allocation must not bump version",
        );
    }

    #[test]
    fn version_monotonic_under_many_allocates() {
        let mut atlas = fresh_atlas(256);
        let mut last = atlas.version();

        for byte in 0..32u8 {
            let sprite = atlas.allocate(&cell(4, 4, byte)).expect("allocate");
            let now = atlas.version();
            assert!(
                now > last,
                "version must strictly increase across uploads: last={}, now={}",
                last,
                now,
            );
            assert_eq!(sprite.version(), now);
            last = now;
        }
    }

    #[test]
    fn cloned_sprite_preserves_version_stamp() {
        let mut atlas = fresh_atlas(64);
        let sprite = atlas.allocate(&cell(8, 8, 0x77)).expect("allocate");
        let clone = sprite.clone();
        assert_eq!(clone.version(), sprite.version());
    }

    #[test]
    fn last_synced_cursor_drift_detection() {
        // Models the per-frame "last_synced_version" pattern callers
        // will use to decide when to re-sync glyph regions on resize.
        let mut atlas = fresh_atlas(64);
        let mut last_synced_version = atlas.version();

        // Pure window resize (no glyph additions): version is
        // unchanged, so nothing needs re-syncing.
        assert_eq!(
            atlas.version(),
            last_synced_version,
            "no upload must not bump version (this is the resize fast path)",
        );

        // Glyph addition bumps the version above the cursor.
        let _ = atlas.allocate(&cell(8, 8, 0x42)).expect("allocate");
        assert!(atlas.version() > last_synced_version);

        // Caller catches up.
        last_synced_version = atlas.version();
        assert_eq!(atlas.version(), last_synced_version);
    }
}
