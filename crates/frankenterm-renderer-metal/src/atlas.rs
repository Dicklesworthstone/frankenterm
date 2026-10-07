//! Glyph atlases, the platform-independent half (ft-yccm0.4.3.2).
//!
//! The renderer keeps two atlases: [`AtlasKind::Grayscale`] (an `R8Unorm`
//! texture, one byte per pixel, for ordinary glyph coverage) and
//! [`AtlasKind::Color`] (a `BGRA8Unorm_sRGB` texture for color emoji and
//! images, decoded to linear light when sampled),
//! so grayscale glyphs no longer pay four bytes per pixel. Each starts at
//! [`DEFAULT_ATLAS_EXTENT`] squared, large enough that the T0 emoji set fits
//! without churn, instead of starting tiny and rebuilding.
//!
//! An atlas is split into horizontal *pages* of equal height. Each page packs
//! rectangles with a [`Skyline`] packer, whose cost per allocation is bounded
//! by the length of the page's skyline, not by a list of free rectangles that
//! every allocation rescans (the maximal-rectangles cost of ft-8r43i.10).
//!
//! Pages are also the unit of reuse. Every frame that draws a glyph
//! [`GlyphAtlas::touch`]es its page with the frame number. When the atlas is
//! full it first grows (taller, up to [`AtlasConfig::max_bytes`]), and then
//! evicts the least recently used page that no unfinished frame references:
//! its last use is older than [`FrameFence::retired_before`]. So new glyph
//! pixels are only ever written where no in-flight frame reads, and the GPU
//! half can upload them in place, with no staging copy and no full re-upload.
//! An eviction bumps the page's epoch; every [`AtlasSlot`] carries the epoch
//! it was allocated under, so a glyph cache recognizes the slots it lost and
//! rasterizes those glyphs again on demand.
//!
//! [`GlyphAtlas::generation`] counts growths and evictions: once the working
//! set fits, it stays flat. Growth replaces the texture; the old one must
//! outlive the frames that sampled it, which [`RetireQueue`] tracks.

use std::collections::VecDeque;

/// Default atlas width and initial height, in pixels.
pub const DEFAULT_ATLAS_EXTENT: u32 = 2048;

/// Default page height, in pixels: eight pages in a default atlas.
pub const DEFAULT_PAGE_HEIGHT: u32 = 256;

/// Default gap left to the right of and below every glyph, in pixels, so
/// filtered sampling never bleeds a neighbor in.
pub const DEFAULT_PADDING: u32 = 1;

/// Largest atlas side; the Apple-family 2D texture limit.
pub const MAX_ATLAS_EXTENT: u32 = crate::MAX_TEXTURE_EXTENT;

/// What an atlas stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AtlasKind {
    /// Glyph coverage, one byte per pixel (`R8Unorm`).
    Grayscale,
    /// Color glyphs and images, premultiplied sRGB `[B, G, R, A]` per pixel
    /// (`BGRA8Unorm_sRGB` in the renderer).
    Color,
}

impl AtlasKind {
    pub const ALL: [Self; 2] = [Self::Grayscale, Self::Color];

    #[must_use]
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Grayscale => 1,
            Self::Color => 4,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Grayscale => "grayscale",
            Self::Color => "color",
        }
    }
}

/// Sizing of one atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtlasConfig {
    /// Texture width; fixed for the atlas's lifetime.
    pub width: u32,
    /// Initial texture height; a whole number of pages.
    pub initial_height: u32,
    /// Page height; the largest glyph height (plus padding) an atlas takes.
    pub page_height: u32,
    /// Gap to the right of and below every glyph.
    pub padding: u32,
    /// Growth stops before the texture would exceed this many bytes.
    pub max_bytes: u64,
}

impl AtlasConfig {
    /// The defaults for `kind`: 2048x2048, 256-pixel pages, one pixel of
    /// padding, growable to 2048x8192 (16 MiB grayscale, 64 MiB color).
    #[must_use]
    pub const fn for_kind(kind: AtlasKind) -> Self {
        let width = DEFAULT_ATLAS_EXTENT;
        Self {
            width,
            initial_height: DEFAULT_ATLAS_EXTENT,
            page_height: DEFAULT_PAGE_HEIGHT,
            padding: DEFAULT_PADDING,
            max_bytes: width as u64
                * (4 * DEFAULT_ATLAS_EXTENT as u64)
                * kind.bytes_per_pixel() as u64,
        }
    }

    fn validate(&self, kind: AtlasKind) -> Result<(), AtlasConfigError> {
        let extent_ok = |side: u32| (1..=MAX_ATLAS_EXTENT).contains(&side);
        if !extent_ok(self.width) || !extent_ok(self.initial_height) {
            return Err(AtlasConfigError::Extent {
                width: self.width,
                height: self.initial_height,
            });
        }
        if self.page_height == 0 || !self.initial_height.is_multiple_of(self.page_height) {
            return Err(AtlasConfigError::Pages {
                height: self.initial_height,
                page_height: self.page_height,
            });
        }
        let initial_bytes = texture_bytes(kind, self.width, self.initial_height);
        if initial_bytes > self.max_bytes {
            return Err(AtlasConfigError::Budget {
                initial_bytes,
                max_bytes: self.max_bytes,
            });
        }
        Ok(())
    }
}

/// An [`AtlasConfig`] the atlas cannot use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtlasConfigError {
    /// A side is zero or larger than [`MAX_ATLAS_EXTENT`].
    Extent { width: u32, height: u32 },
    /// The page height is zero or does not divide the initial height.
    Pages { height: u32, page_height: u32 },
    /// The initial texture already exceeds the growth budget.
    Budget { initial_bytes: u64, max_bytes: u64 },
}

impl std::fmt::Display for AtlasConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Extent { width, height } => write!(
                f,
                "atlas extent {width}x{height} is outside 1..={MAX_ATLAS_EXTENT} per side"
            ),
            Self::Pages {
                height,
                page_height,
            } => write!(
                f,
                "page height {page_height} does not divide the atlas height {height}"
            ),
            Self::Budget {
                initial_bytes,
                max_bytes,
            } => write!(
                f,
                "the initial atlas needs {initial_bytes} bytes, over its {max_bytes}-byte budget"
            ),
        }
    }
}

impl std::error::Error for AtlasConfigError {}

/// Why a glyph did not reach an atlas.
#[derive(Debug)]
pub enum AtlasError {
    Config(AtlasConfigError),
    Full(AtlasFull),
    /// `pixels` was not exactly `width * height * bytes_per_pixel` bytes.
    PixelBytes {
        expected: usize,
        actual: usize,
    },
    Gpu(crate::FrameError),
}

impl std::fmt::Display for AtlasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(error) => write!(f, "atlas configuration: {error}"),
            Self::Full(full) => write!(f, "atlas full: {full:?}"),
            Self::PixelBytes { expected, actual } => {
                write!(f, "glyph pixels are {actual} bytes, expected {expected}")
            }
            Self::Gpu(error) => write!(f, "atlas GPU operation failed: {error}"),
        }
    }
}

impl std::error::Error for AtlasError {}

/// Bytes of a `width x height` texture of `kind`.
#[must_use]
pub fn texture_bytes(kind: AtlasKind, width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height) * u64::from(kind.bytes_per_pixel())
}

/// One run of equal height along a [`Skyline`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segment {
    x: u32,
    y: u32,
    width: u32,
}

/// Bottom-left skyline packer for one `width x height` rectangle.
///
/// The skyline is the upper contour of everything placed so far, kept as
/// segments sorted by `x` that exactly tile `0..width`. An allocation tries
/// each segment as a left edge, takes the lowest resulting top (leftmost on a
/// tie), and splices the new rectangle's top into the skyline. Space below the
/// skyline is never revisited, which wastes a little area on mixed heights but
/// keeps every allocation proportional to the skyline's length.
#[derive(Debug, Clone)]
pub struct Skyline {
    width: u32,
    height: u32,
    segments: Vec<Segment>,
}

impl Skyline {
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            segments: vec![Segment { x: 0, y: 0, width }],
        }
    }

    /// Places a `width x height` rectangle; returns its top-left corner.
    pub fn allocate(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        if width == 0 || height == 0 || width > self.width || height > self.height {
            return None;
        }
        // (top, x, y, first segment index) of the best placement.
        let mut best: Option<(u32, u32, u32, usize)> = None;
        for start in 0..self.segments.len() {
            let x = self.segments[start].x;
            if x + width > self.width {
                break;
            }
            let y = self.resting_height(start, width);
            let top = y + height;
            if top > self.height {
                continue;
            }
            if best.is_none_or(|(best_top, best_x, _, _)| (top, x) < (best_top, best_x)) {
                best = Some((top, x, y, start));
            }
        }
        let (top, x, y, start) = best?;
        self.splice(start, x, width, top);
        Some((x, y))
    }

    /// The height a rectangle `width` wide rests at with its left edge on
    /// segment `start`: the highest segment it spans.
    fn resting_height(&self, start: usize, width: u32) -> u32 {
        let mut remaining = width;
        let mut y = 0;
        for segment in &self.segments[start..] {
            y = y.max(segment.y);
            if segment.width >= remaining {
                break;
            }
            remaining -= segment.width;
        }
        y
    }

    /// Raises `x..x + width` to `top`, starting at segment `start`.
    fn splice(&mut self, start: usize, x: u32, width: u32, top: u32) {
        let end = x + width;
        self.segments.insert(start, Segment { x, y: top, width });
        let next = start + 1;
        while next < self.segments.len() && self.segments[next].x < end {
            let segment = self.segments[next];
            let segment_end = segment.x + segment.width;
            if segment_end <= end {
                self.segments.remove(next);
            } else {
                self.segments[next] = Segment {
                    x: end,
                    y: segment.y,
                    width: segment_end - end,
                };
                break;
            }
        }
        // Merge equal neighbors so the skyline stays as short as possible.
        let mut index = start.saturating_sub(1);
        while index + 1 < self.segments.len() && index <= start + 1 {
            if self.segments[index].y == self.segments[index + 1].y {
                self.segments[index].width += self.segments[index + 1].width;
                self.segments.remove(index + 1);
            } else {
                index += 1;
            }
        }
    }

    /// Forgets every placement.
    pub fn reset(&mut self) {
        self.segments.clear();
        self.segments.push(Segment {
            x: 0,
            y: 0,
            width: self.width,
        });
    }

    /// Segments in the skyline; what one allocation scans at most.
    #[must_use]
    pub fn segments(&self) -> usize {
        self.segments.len()
    }
}

/// Which frames may still read the atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameFence {
    /// The frame now being encoded; glyphs it uses are touched with it.
    pub current: u64,
    /// Every frame numbered below this has finished on the GPU (or was never
    /// submitted), so nothing it drew is still being read.
    pub retired_before: u64,
}

/// A glyph's place in an atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AtlasSlot {
    pub kind: AtlasKind,
    pub page: u32,
    /// The page epoch the slot was allocated under; see [`GlyphAtlas::is_live`].
    pub epoch: u32,
    /// Texture coordinates of the glyph's pixels (padding excluded).
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// What an allocation did to the atlas besides placing the glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtlasChange {
    /// The texture grew from `old_height` to `new_height` rows. The GPU half
    /// copies the old texture's rows into a new one and retires the old one.
    Grew { old_height: u32, new_height: u32 },
    /// `page` was emptied. Its glyphs' slots are stale; nothing on the GPU
    /// changes, since no unfinished frame reads that page.
    Evicted { page: u32 },
}

/// A successful [`GlyphAtlas::allocate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    pub slot: AtlasSlot,
    pub change: Option<AtlasChange>,
}

/// Why a glyph could not be placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtlasFull {
    /// The glyph plus padding is wider than the atlas or taller than a page.
    TooLarge { width: u32, height: u32 },
    /// The atlas is at its budget and every page is used by an unfinished
    /// frame; retry once older frames finish.
    AllPagesInFlight,
}

/// Counters for logs and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AtlasStats {
    pub allocations: u64,
    pub growths: u64,
    pub evictions: u64,
    pub refusals: u64,
}

#[derive(Debug, Clone)]
struct Page {
    skyline: Skyline,
    /// The latest frame that drew from this page.
    last_used: Option<u64>,
    epoch: u32,
}

/// The allocator state of one atlas; the texture lives in the GPU half.
#[derive(Debug, Clone)]
pub struct GlyphAtlas {
    kind: AtlasKind,
    config: AtlasConfig,
    height: u32,
    pages: Vec<Page>,
    /// The page that placed the previous glyph; tried first.
    cursor: usize,
    generation: u64,
    stats: AtlasStats,
}

impl GlyphAtlas {
    pub fn new(kind: AtlasKind, config: AtlasConfig) -> Result<Self, AtlasConfigError> {
        config.validate(kind)?;
        let mut atlas = Self {
            kind,
            config,
            height: 0,
            pages: Vec::new(),
            cursor: 0,
            generation: 0,
            stats: AtlasStats::default(),
        };
        atlas.add_pages(config.initial_height);
        Ok(atlas)
    }

    fn add_pages(&mut self, new_height: u32) {
        while self.height < new_height {
            self.pages.push(Page {
                skyline: Skyline::new(self.config.width, self.config.page_height),
                last_used: None,
                epoch: 0,
            });
            self.height += self.config.page_height;
        }
    }

    /// Places a `width x height` glyph for the frame `fence.current`: in an
    /// existing page if one has room, else by growing within the budget,
    /// else by evicting the least recently used retired page.
    pub fn allocate(
        &mut self,
        width: u32,
        height: u32,
        fence: FrameFence,
    ) -> Result<Allocation, AtlasFull> {
        let padded = (
            width.saturating_add(self.config.padding),
            height.saturating_add(self.config.padding),
        );
        if width == 0
            || height == 0
            || padded.0 > self.config.width
            || padded.1 > self.config.page_height
        {
            self.stats.refusals += 1;
            return Err(AtlasFull::TooLarge { width, height });
        }
        let pages = self.pages.len();
        for offset in 0..pages {
            let page = (self.cursor + offset) % pages;
            if let Some(slot) = self.place(page, width, height, padded, fence) {
                return Ok(Allocation { slot, change: None });
            }
        }
        if let Some(new_height) = self.grown_height() {
            let old_height = self.height;
            self.add_pages(new_height);
            self.generation += 1;
            self.stats.growths += 1;
            let slot = self
                .place(pages, width, height, padded, fence)
                .expect("a fresh page fits any glyph no taller than a page");
            return Ok(Allocation {
                slot,
                change: Some(AtlasChange::Grew {
                    old_height,
                    new_height,
                }),
            });
        }
        let victim = self
            .pages
            .iter()
            .enumerate()
            .filter(|(_, page)| {
                page.last_used
                    .is_none_or(|frame| frame < fence.retired_before)
            })
            .min_by_key(|(_, page)| page.last_used)
            .map(|(index, _)| index);
        let Some(victim) = victim else {
            self.stats.refusals += 1;
            return Err(AtlasFull::AllPagesInFlight);
        };
        let page = &mut self.pages[victim];
        page.skyline.reset();
        page.epoch = page.epoch.wrapping_add(1);
        page.last_used = None;
        self.generation += 1;
        self.stats.evictions += 1;
        let slot = self
            .place(victim, width, height, padded, fence)
            .expect("an emptied page fits any glyph no taller than a page");
        Ok(Allocation {
            slot,
            change: Some(AtlasChange::Evicted {
                page: u32::try_from(victim).expect("page count fits u32"),
            }),
        })
    }

    fn place(
        &mut self,
        page_index: usize,
        width: u32,
        height: u32,
        padded: (u32, u32),
        fence: FrameFence,
    ) -> Option<AtlasSlot> {
        let page_height = self.config.page_height;
        let page = &mut self.pages[page_index];
        let (x, y) = page.skyline.allocate(padded.0, padded.1)?;
        page.last_used = Some(
            page.last_used
                .map_or(fence.current, |last| last.max(fence.current)),
        );
        self.cursor = page_index;
        self.stats.allocations += 1;
        let page_number = u32::try_from(page_index).expect("page count fits u32");
        Some(AtlasSlot {
            kind: self.kind,
            page: page_number,
            epoch: page.epoch,
            x,
            y: page_number * page_height + y,
            width,
            height,
        })
    }

    /// The next height within the budget and the texture limit, if any:
    /// doubled, or as many pages as still fit.
    fn grown_height(&self) -> Option<u32> {
        let page_height = self.config.page_height;
        let row_bytes = texture_bytes(self.kind, self.config.width, 1);
        let budget_rows = self.config.max_bytes / row_bytes.max(1);
        let limit = u64::from(MAX_ATLAS_EXTENT).min(budget_rows);
        let limit = u32::try_from(limit).unwrap_or(MAX_ATLAS_EXTENT);
        let limit = limit - limit % page_height;
        let doubled = self.height.saturating_mul(2).min(limit);
        (doubled > self.height).then_some(doubled)
    }

    /// Records that the frame `frame` draws the glyph in `slot`. Returns
    /// false when the slot is stale and the glyph must be placed again.
    pub fn touch(&mut self, slot: &AtlasSlot, frame: u64) -> bool {
        let Some(page) = self.live_page_mut(slot) else {
            return false;
        };
        page.last_used = Some(page.last_used.map_or(frame, |last| last.max(frame)));
        true
    }

    /// Whether `slot` still holds the glyph it was allocated for.
    #[must_use]
    pub fn is_live(&self, slot: &AtlasSlot) -> bool {
        slot.kind == self.kind
            && usize::try_from(slot.page)
                .ok()
                .and_then(|page| self.pages.get(page))
                .is_some_and(|page| page.epoch == slot.epoch)
    }

    fn live_page_mut(&mut self, slot: &AtlasSlot) -> Option<&mut Page> {
        if slot.kind != self.kind {
            return None;
        }
        let page = self.pages.get_mut(usize::try_from(slot.page).ok()?)?;
        (page.epoch == slot.epoch).then_some(page)
    }

    #[must_use]
    pub fn kind(&self) -> AtlasKind {
        self.kind
    }

    /// The texture extent the GPU half must hold, `(width, height)`.
    #[must_use]
    pub fn extent(&self) -> (u32, u32) {
        (self.config.width, self.height)
    }

    /// Bytes of the current texture.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        texture_bytes(self.kind, self.config.width, self.height)
    }

    /// Growths plus evictions; flat once the working set fits.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn stats(&self) -> AtlasStats {
        self.stats
    }

    #[must_use]
    pub fn pages(&self) -> usize {
        self.pages.len()
    }

    #[must_use]
    pub fn config(&self) -> AtlasConfig {
        self.config
    }
}

/// GPU objects kept alive until the frames that might read them finish.
#[derive(Debug)]
pub struct RetireQueue<T> {
    pending: VecDeque<(u64, T)>,
}

impl<T> Default for RetireQueue<T> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
        }
    }
}

impl<T> RetireQueue<T> {
    /// Keeps `item` until every frame up to and including `last_frame` has
    /// finished.
    pub fn retire(&mut self, item: T, last_frame: u64) {
        self.pending.push_back((last_frame, item));
    }

    /// Removes and returns every item whose last frame is below
    /// `retired_before`, for the caller to release.
    pub fn take_ready(&mut self, retired_before: u64) -> Vec<T> {
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].0 < retired_before {
                if let Some((_, item)) = self.pending.remove(index) {
                    ready.push(item);
                }
            } else {
                index += 1;
            }
        }
        ready
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fence(current: u64) -> FrameFence {
        FrameFence {
            current,
            retired_before: current.saturating_sub(2),
        }
    }

    fn overlaps(a: (u32, u32, u32, u32), b: (u32, u32, u32, u32)) -> bool {
        a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
    }

    /// Deterministic xorshift for test sizes.
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn below(state: &mut u64, bound: u64) -> u32 {
        u32::try_from(next(state) % bound).expect("bounded")
    }

    #[test]
    fn skyline_places_mixed_rectangles_in_bounds_without_overlap() {
        let mut skyline = Skyline::new(512, 256);
        let mut placed: Vec<(u32, u32, u32, u32)> = Vec::new();
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..2_000 {
            let width = 4 + below(&mut state, 60);
            let height = 4 + below(&mut state, 40);
            if let Some((x, y)) = skyline.allocate(width, height) {
                assert!(
                    x + width <= 512 && y + height <= 256,
                    "{x},{y} {width}x{height}"
                );
                let rect = (x, y, width, height);
                assert!(
                    placed.iter().all(|other| !overlaps(*other, rect)),
                    "{rect:?} overlaps an earlier placement"
                );
                placed.push(rect);
            }
            // What one allocation scans: never more segments than pixel
            // columns, however many rectangles the page holds.
            assert!(skyline.segments() <= 512, "{}", skyline.segments());
        }
        let area: u32 = placed.iter().map(|rect| rect.2 * rect.3).sum();
        assert!(
            f64::from(area) > 0.6 * 512.0 * 256.0,
            "mixed sizes still fill most of the page: {area}"
        );
    }

    #[test]
    fn skyline_packs_uniform_cells_into_full_rows() {
        let mut skyline = Skyline::new(2048, 256);
        let mut count = 0;
        while skyline.allocate(49, 59).is_some() {
            count += 1;
        }
        // 41 per row (2009 of 2048 pixels) and 4 rows (236 of 256).
        assert_eq!(count, 41 * 4);
        assert_eq!(skyline.allocate(1, 1), Some((2009, 0)));
        skyline.reset();
        assert_eq!(skyline.segments(), 1);
        assert_eq!(skyline.allocate(2048, 256), Some((0, 0)));
        assert_eq!(skyline.allocate(1, 1), None);
        assert_eq!(skyline.allocate(0, 4), None);
        assert_eq!(skyline.allocate(2049, 1), None);
    }

    #[test]
    fn config_defaults_and_validation() {
        for kind in AtlasKind::ALL {
            let config = AtlasConfig::for_kind(kind);
            assert_eq!((config.width, config.initial_height), (2048, 2048));
            assert_eq!(config.max_bytes, texture_bytes(kind, 2048, 8192));
            let atlas = GlyphAtlas::new(kind, config).unwrap();
            assert_eq!(atlas.extent(), (2048, 2048));
            assert_eq!(atlas.pages(), 8);
        }
        assert_eq!(
            texture_bytes(AtlasKind::Grayscale, 2048, 2048) * 4,
            texture_bytes(AtlasKind::Color, 2048, 2048),
            "grayscale glyphs cost a quarter of the color atlas per pixel"
        );
        let mut bad = AtlasConfig::for_kind(AtlasKind::Color);
        bad.page_height = 300;
        assert!(matches!(
            GlyphAtlas::new(AtlasKind::Color, bad),
            Err(AtlasConfigError::Pages { .. })
        ));
        bad = AtlasConfig::for_kind(AtlasKind::Color);
        bad.width = MAX_ATLAS_EXTENT + 1;
        assert!(matches!(
            GlyphAtlas::new(AtlasKind::Color, bad),
            Err(AtlasConfigError::Extent { .. })
        ));
        bad = AtlasConfig::for_kind(AtlasKind::Color);
        bad.max_bytes = 1;
        assert!(matches!(
            GlyphAtlas::new(AtlasKind::Color, bad),
            Err(AtlasConfigError::Budget { .. })
        ));
    }

    /// A 256-wide atlas of 64-row pages, starting at two pages and growable
    /// to `max_pages`.
    fn small_config(kind: AtlasKind, max_pages: u32) -> AtlasConfig {
        AtlasConfig {
            width: 256,
            initial_height: 128,
            page_height: 64,
            padding: 0,
            max_bytes: texture_bytes(kind, 256, 64 * max_pages),
        }
    }

    #[test]
    fn a_full_atlas_grows_within_its_budget_then_evicts_the_oldest_retired_page() {
        let mut atlas =
            GlyphAtlas::new(AtlasKind::Grayscale, small_config(AtlasKind::Grayscale, 4)).unwrap();
        // Two pages of 64x64 cells: four per page.
        let mut slots = Vec::new();
        for frame in 0..8 {
            let allocation = atlas.allocate(64, 64, fence(frame)).unwrap();
            assert_eq!(allocation.change, None);
            slots.push(allocation.slot);
        }
        assert_eq!(atlas.generation(), 0);
        // The ninth grows the texture to the four-page budget.
        let grown = atlas.allocate(64, 64, fence(8)).unwrap();
        assert_eq!(
            grown.change,
            Some(AtlasChange::Grew {
                old_height: 128,
                new_height: 256
            })
        );
        assert_eq!((atlas.extent(), atlas.bytes()), ((256, 256), 256 * 256));
        for frame in 9..16 {
            assert_eq!(atlas.allocate(64, 64, fence(frame)).unwrap().change, None);
        }
        // Full at the budget. Page 0 (frames 0..3) is the least recently used,
        // but frame 2 keeps it alive until frames below 3 have retired.
        assert_eq!(
            atlas.allocate(
                64,
                64,
                FrameFence {
                    current: 16,
                    retired_before: 3
                }
            ),
            Err(AtlasFull::AllPagesInFlight)
        );
        let evicted = atlas
            .allocate(
                64,
                64,
                FrameFence {
                    current: 16,
                    retired_before: 4,
                },
            )
            .unwrap();
        assert_eq!(evicted.change, Some(AtlasChange::Evicted { page: 0 }));
        assert_eq!(evicted.slot.page, 0);
        assert_eq!(atlas.generation(), 2, "one growth and one eviction");
        assert!(!atlas.is_live(&slots[0]), "page 0's old glyphs are stale");
        assert!(atlas.is_live(&slots[4]));
        assert!(!atlas.touch(&slots[0], 17));
        // A touched page is no longer the eviction victim.
        assert!(atlas.touch(&slots[4], 17));
        let next = atlas
            .allocate(
                64,
                64,
                FrameFence {
                    current: 17,
                    retired_before: 17,
                },
            )
            .unwrap();
        assert_eq!(next.change, None, "page 0 still had room");
        assert_eq!(atlas.stats().growths, 1);
        assert_eq!(atlas.stats().evictions, 1);
        assert_eq!(atlas.stats().refusals, 1);
    }

    #[test]
    fn oversized_glyphs_are_refused_not_placed() {
        let mut atlas =
            GlyphAtlas::new(AtlasKind::Color, small_config(AtlasKind::Color, 2)).unwrap();
        assert_eq!(
            atlas.allocate(10, 65, fence(0)),
            Err(AtlasFull::TooLarge {
                width: 10,
                height: 65
            })
        );
        assert_eq!(
            atlas.allocate(257, 1, fence(0)),
            Err(AtlasFull::TooLarge {
                width: 257,
                height: 1
            })
        );
        assert!(atlas.allocate(0, 1, fence(0)).is_err());
        assert_eq!(atlas.generation(), 0);
    }

    /// ft-yccm0.4.3.2 AC: 1,000 forced evictions keep the texture at its
    /// budget; nothing grows and nothing leaks.
    #[test]
    fn a_thousand_forced_evictions_keep_the_atlas_at_its_budget() {
        let mut atlas =
            GlyphAtlas::new(AtlasKind::Color, small_config(AtlasKind::Color, 2)).unwrap();
        let budget = atlas.bytes();
        // Every glyph fills a whole page, so from the third on each one evicts.
        let mut evictions = 0;
        for frame in 0..1_002_u64 {
            let allocation = atlas
                .allocate(
                    256,
                    64,
                    FrameFence {
                        current: frame,
                        retired_before: frame,
                    },
                )
                .unwrap();
            if let Some(AtlasChange::Evicted { .. }) = allocation.change {
                evictions += 1;
            }
            assert_ne!(
                allocation
                    .change
                    .map(|change| matches!(change, AtlasChange::Grew { .. })),
                Some(true),
                "the budget allows no growth"
            );
            assert_eq!(atlas.bytes(), budget);
        }
        assert_eq!(evictions, 1_000);
        assert_eq!(atlas.generation(), 1_000);
        assert_eq!(atlas.pages(), 2);
    }

    /// ft-yccm0.4.3.2 AC capacity, as an upper bound: all 1,447 T0 glyphs
    /// (the 1,376 emoji and the 71 ASCII characters, which in practice go to
    /// the grayscale atlas) placed in the default color atlas. At the
    /// operator's font (16 pt at 2x: emoji about 40x40 px) they fit with no
    /// growth and no eviction. At the bead's larger 48x58 estimate the atlas
    /// grows once during warm-up and then stays flat.
    #[test]
    fn the_t0_emoji_pool_fits_without_churn_after_warmup() {
        for ((width, height), expected_generation) in [((40, 40), 0), ((48, 58), 1)] {
            let mut atlas =
                GlyphAtlas::new(AtlasKind::Color, AtlasConfig::for_kind(AtlasKind::Color)).unwrap();
            let mut slots = Vec::new();
            for glyph in 0..1_447_u64 {
                let frame = glyph / 64;
                slots.push(atlas.allocate(width, height, fence(frame)).unwrap().slot);
            }
            let warm = atlas.generation();
            assert_eq!(warm, expected_generation, "{width}x{height}");
            assert_eq!(atlas.stats().evictions, 0);
            // Later frames draw every glyph again: all slots stay live and the
            // generation stays flat.
            for frame in 100..110 {
                for slot in &slots {
                    assert!(atlas.touch(slot, frame));
                }
            }
            assert_eq!(atlas.generation(), warm);
        }
    }

    #[test]
    fn retire_queue_holds_items_until_their_frames_finish() {
        let mut queue = RetireQueue::default();
        queue.retire("atlas v1", 5);
        queue.retire("atlas v2", 9);
        assert!(queue.take_ready(5).is_empty(), "frame 5 may still read v1");
        assert_eq!(queue.take_ready(6), vec!["atlas v1"]);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.take_ready(100), vec!["atlas v2"]);
        assert!(queue.is_empty());
    }
}
