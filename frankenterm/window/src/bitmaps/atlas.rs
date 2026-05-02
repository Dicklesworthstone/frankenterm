use crate::bitmaps::{BitmapImage, Texture2d, TextureRect};
use crate::{Point, Rect, Size};
use anyhow::{ensure, Result as Fallible};
use frankenterm_core::atlas_bin_packing::{
    make_packer, select_packer, AllocationOutcome, Atlas2DSize, BinPacker, GlyphSize, PackerKind,
    PackerSelectionThresholds, PackingStats,
};
use std::convert::{TryFrom, TryInto};
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

    allocator: Box<dyn BinPacker>,
    packing_stats: PackingStats,

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
        ensure!(side > 0, "texture must be non-empty");
        let iside = side as isize;

        let image = crate::Image::new(side, side);
        let rect = Rect::new(Point::new(0, 0), Size::new(iside, iside));
        texture.write(rect, &image);

        let atlas_size = atlas_size_from_side(side)?;
        let allocator = make_packer(
            select_packer(atlas_size, PackerSelectionThresholds::default()),
            atlas_size,
        );
        let mut packing_stats = PackingStats::default();
        packing_stats.record_atlas_size(atlas_size);

        // Record the atlas footprint so dashboards can surface
        // memory pressure once the grow path lands. SRGBA = 4 bytes
        // per pixel.
        let bytes_estimate = (side as u64).saturating_mul(side as u64).saturating_mul(4);
        metrics::gauge!("window.atlas.size_bytes").set(bytes_estimate as f64);

        Ok(Self {
            texture: Rc::clone(texture),
            side,
            allocator,
            packing_stats,
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
        let glyph = GlyphSize::try_new(
            reserve_width.try_into().map_err(|_| OutOfTextureSpace {
                size: None,
                current_size: self.side,
            })?,
            reserve_height.try_into().map_err(|_| OutOfTextureSpace {
                size: None,
                current_size: self.side,
            })?,
        )
        .ok_or(OutOfTextureSpace {
            size: None,
            current_size: self.side,
        })?;

        let res = if let AllocationOutcome::Placed(allocation) = self.allocator.try_alloc(glyph) {
            let left = isize::try_from(allocation.x).map_err(|_| OutOfTextureSpace {
                size: None,
                current_size: self.side,
            })?;
            let top = isize::try_from(allocation.y).map_err(|_| OutOfTextureSpace {
                size: None,
                current_size: self.side,
            })?;
            let rect = Rect::new(
                Point::new(left + PADDING as isize, top + PADDING as isize),
                Size::new(width as isize, height as isize),
            );

            self.texture.write(rect, im);
            self.packing_stats.record_placed(allocation);
            self.record_packing_metrics();

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
            self.packing_stats.record_reject();
            self.record_packing_metrics();
            metrics::histogram!("window.atlas.allocate.failure.rate").record(1.);
            metrics::counter!(
                "window.atlas.rejects.total",
                "packer" => self.packer_name()
            )
            .increment(1);
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

    pub fn packer_kind(&self) -> PackerKind {
        self.allocator.kind()
    }

    pub fn packing_efficiency_pct(&self) -> u32 {
        self.packing_stats.efficiency_pct()
    }

    pub fn fragmentation_pct(&self) -> u32 {
        self.packing_stats.wasted_pct()
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
        self.packing_stats = PackingStats::default();
        self.packing_stats.record_atlas_size(self.allocator.size());
        self.record_packing_metrics();
        self.version.fetch_add(1, Ordering::AcqRel);
        metrics::counter!("window.atlas.rebuilds.total").increment(1);
    }

    /// Grow the atlas onto a larger texture (ft-c9arc).
    ///
    /// `new_texture` MUST be a square texture whose side is at least
    /// the current `side`. The atlas takes ownership of the new
    /// texture, the allocator is reset, the version is bumped, and
    /// the size_bytes gauge is refreshed.
    ///
    /// # Sprite preservation
    ///
    /// In this foundation slice, growing resets the allocator and
    /// invalidates existing sprites' coords (their versions
    /// become stale, mirroring the [`clear`](Self::clear) contract).
    /// The full ghostty-pattern blit-and-retain — which requires
    /// `Texture2d::supports_readback` on every render-path backend —
    /// is the follow-on integration captured in the bead's closure
    /// plan; until then, the version-cursor in glyphcache observes
    /// the bump and lazy-rerasterizes on demand.
    ///
    /// Bumps `window.atlas.grow.count` and refreshes
    /// `window.atlas.size_bytes`.
    pub fn grow(&mut self, new_texture: &Rc<dyn Texture2d>) -> Fallible<()> {
        ensure!(
            new_texture.width() == new_texture.height(),
            "grow texture must be square!"
        );
        ensure!(
            new_texture.width() >= self.side,
            "grow texture side {} must be >= current side {}",
            new_texture.width(),
            self.side
        );
        let new_side = new_texture.width();
        let iside = new_side as isize;
        let image = crate::Image::new(new_side, new_side);
        let rect = Rect::new(Point::new(0, 0), Size::new(iside, iside));
        new_texture.write(rect, &image);

        self.texture = Rc::clone(new_texture);
        self.side = new_side;
        let atlas_size = atlas_size_from_side(new_side)?;
        self.allocator = make_packer(
            select_packer(atlas_size, PackerSelectionThresholds::default()),
            atlas_size,
        );
        self.packing_stats = PackingStats::default();
        self.packing_stats.record_atlas_size(atlas_size);
        self.record_packing_metrics();
        self.version.fetch_add(1, Ordering::AcqRel);

        let bytes_estimate = (new_side as u64)
            .saturating_mul(new_side as u64)
            .saturating_mul(4);
        metrics::gauge!("window.atlas.size_bytes").set(bytes_estimate as f64);
        metrics::counter!("window.atlas.grow.count").increment(1);

        Ok(())
    }

    fn packer_name(&self) -> &'static str {
        match self.allocator.kind() {
            PackerKind::Shelf => "shelf",
            PackerKind::Skyline => "skyline",
            PackerKind::MaximalRectangles => "maximal_rectangles",
        }
    }

    fn record_packing_metrics(&self) {
        metrics::gauge!(
            "window.atlas.packing_efficiency_pct",
            "packer" => self.packer_name()
        )
        .set(self.packing_stats.efficiency_pct() as f64);
        metrics::gauge!(
            "window.atlas.fragmentation_pct",
            "packer" => self.packer_name()
        )
        .set(self.packing_stats.wasted_pct() as f64);
    }
}

fn atlas_size_from_side(side: usize) -> Fallible<Atlas2DSize> {
    let side: u32 = side.try_into()?;
    Atlas2DSize::try_new(side, side).ok_or_else(|| anyhow::anyhow!("atlas side must be non-zero"))
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
    use guillotiere::{SimpleAtlasAllocator, Size as LegacyAtlasSize};

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

    fn legacy_simple_atlas_coords(side: i32, images: &[(usize, usize)]) -> Vec<Rect> {
        let mut allocator = SimpleAtlasAllocator::new(LegacyAtlasSize::new(side, side));
        images
            .iter()
            .map(|&(width, height)| {
                let reserve_width = i32::try_from(width).expect("fixture width fits") + PADDING * 2;
                let reserve_height =
                    i32::try_from(height).expect("fixture height fits") + PADDING * 2;
                let allocation = allocator
                    .allocate(LegacyAtlasSize::new(reserve_width, reserve_height))
                    .expect("fixture allocation fits legacy atlas");
                Rect::new(
                    Point::new(
                        isize::try_from(allocation.min.x + PADDING).expect("x fits"),
                        isize::try_from(allocation.min.y + PADDING).expect("y fits"),
                    ),
                    Size::new(
                        isize::try_from(width).expect("width fits"),
                        isize::try_from(height).expect("height fits"),
                    ),
                )
            })
            .collect()
    }

    #[test]
    fn version_starts_at_zero() {
        let atlas = fresh_atlas(64);
        assert_eq!(atlas.version(), 0);
    }

    #[test]
    fn version_bumps_on_each_successful_upload() {
        let mut atlas = fresh_atlas(64);
        assert_eq!(atlas.packer_kind(), PackerKind::Shelf);
        let baseline = atlas.version();

        let sprite_a = atlas.allocate(&cell(8, 8, 0x10)).expect("allocate a");
        assert_eq!(atlas.version(), baseline + 1);
        assert_eq!(sprite_a.version(), baseline + 1);

        let sprite_b = atlas.allocate(&cell(8, 8, 0x20)).expect("allocate b");
        assert_eq!(atlas.version(), baseline + 2);
        assert_eq!(sprite_b.version(), baseline + 2);

        // Each sprite carries the version it was stamped with.
        assert!(sprite_b.version() > sprite_a.version());
        assert!(atlas.packing_efficiency_pct() > 0);
        assert!(atlas.fragmentation_pct() < 100);
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
        assert_eq!(atlas.packing_efficiency_pct(), 0);
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

    #[test]
    fn grow_bumps_version_and_resizes() {
        let mut atlas = fresh_atlas(64);
        let _ = atlas.allocate(&cell(8, 8, 0x11)).expect("allocate");
        let pre_grow_version = atlas.version();
        assert_eq!(atlas.size(), 64);

        let new_texture: Rc<dyn Texture2d> = Rc::new(ImageTexture::new(128, 128));
        atlas.grow(&new_texture).expect("grow");

        assert_eq!(atlas.size(), 128);
        assert_eq!(atlas.packer_kind(), PackerKind::Shelf);
        assert_eq!(atlas.packing_efficiency_pct(), 0);
        assert!(
            atlas.version() > pre_grow_version,
            "grow must bump version: pre={}, post={}",
            pre_grow_version,
            atlas.version(),
        );
    }

    #[test]
    fn grow_rejects_smaller_texture() {
        let mut atlas = fresh_atlas(128);
        let smaller: Rc<dyn Texture2d> = Rc::new(ImageTexture::new(64, 64));
        assert!(
            atlas.grow(&smaller).is_err(),
            "grow into a smaller texture must fail"
        );
    }

    #[test]
    fn grow_rejects_non_square_texture() {
        let mut atlas = fresh_atlas(64);
        let non_square: Rc<dyn Texture2d> = Rc::new(ImageTexture::new(128, 64));
        assert!(
            atlas.grow(&non_square).is_err(),
            "grow into a non-square texture must fail"
        );
    }

    #[test]
    fn post_grow_atlas_can_satisfy_allocations_that_didnt_fit_before() {
        // Pre-grow: 32x32 atlas, 32x32 sprite would fail (PADDING > 0).
        let mut atlas = fresh_atlas(32);
        let too_big = cell(32, 32, 0xAA);
        assert!(
            atlas.allocate(&too_big).is_err(),
            "32x32 sprite into a 32x32 atlas should fail (padding overflows)"
        );

        // Grow to 128 — the same sprite now fits.
        let bigger: Rc<dyn Texture2d> = Rc::new(ImageTexture::new(128, 128));
        atlas.grow(&bigger).expect("grow");
        let sprite = atlas
            .allocate(&too_big)
            .expect("post-grow allocate must succeed");
        assert_eq!(sprite.version(), atlas.version());
    }

    #[test]
    fn atlas_packer_selection_changes_with_texture_size() {
        assert_eq!(fresh_atlas(512).packer_kind(), PackerKind::Shelf);
        assert_eq!(fresh_atlas(1024).packer_kind(), PackerKind::Skyline);
    }

    #[test]
    fn bin_packer_backed_atlas_matches_legacy_simple_atlas_fixture() {
        let fixtures = [(8, 8), (10, 6), (4, 12), (12, 12), (6, 6)];
        let expected = legacy_simple_atlas_coords(128, &fixtures);
        let mut atlas = fresh_atlas(128);

        let actual: Vec<Rect> = fixtures
            .iter()
            .enumerate()
            .map(|(index, &(width, height))| {
                atlas
                    .allocate(&cell(
                        width,
                        height,
                        u8::try_from(index).expect("index fits"),
                    ))
                    .expect("fixture allocation fits bin-packer atlas")
                    .coords
            })
            .collect();

        assert_eq!(actual, expected);
        assert_eq!(atlas.version(), fixtures.len() as u64);
    }
}
