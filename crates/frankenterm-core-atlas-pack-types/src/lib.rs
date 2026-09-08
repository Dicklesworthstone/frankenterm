//! Atlas bin-packing algorithms (ft-mpc9b.1.4).
//!
//! Per Jukka Jylänki's *A Thousand Ways to Pack the Bin*, several
//! algorithms trade speed for waste:
//!
//! - **Shelf-packing** — constant-time placement; row-based, ideal for
//!   uniform-height glyph runs (small static atlases).
//! - **Skyline** — online placement; selected by default when the
//!   largest axis is above 512 and below 4096.
//! - **Maximal rectangles** — more expensive placement with overlapping
//!   free rectangles; selected by default at a largest axis of 4096 or above. The full
//!   [`MaximalRectanglesPacker`] implementation and selector dispatch
//!   live in this crate. Packing efficiency depends on the glyph workload.
//!
//! ## What this module ships
//!
//! - `Atlas2DSize { width, height }` and `GlyphSize` with constructors
//!   that reject zero dimensions; their public fields can bypass those checks.
//! - `PackedRect { x, y, width, height }` — the result of an
//!   allocation. The integration layer copies this into the atlas's
//!   per-glyph metadata (cross-link `atlas_stability.rs` ft-mpc9b.1.1).
//! - `AllocationOutcome` — `Placed(rect)` / `Rejected(reason)`.
//! - `PackerKind` — `Shelf | Skyline | MaximalRectangles`. The
//!   adaptive selector picks one from atlas size.
//! - `select_packer(size, thresholds) -> PackerKind` — pure-logic policy.
//!   Configurable thresholds via `PackerSelectionThresholds`.
//! - `ShelfPacker`, `SkylinePacker`, and `MaximalRectanglesPacker` all
//!   implement the [`BinPacker`] trait. The factory
//!   [`make_packer`] returns a `Box<dyn BinPacker>` so the integration
//!   layer can swap algorithms by atlas size without compile-time
//!   monomorphization.
//! - `non_overlapping` — invariant checker for tests + integration's
//!   debug-mode assertions. Returns the first overlap pair if any.
//! - `PackingStats` — running counters (`alloc_total`,
//!   `reject_total`, `atlas_bytes`, `used_bytes`) + `efficiency_pct`
//!   for `ft doctor`.
//!
//! ## What ships in this module (ft-i1y15)
//!
//! - [`BinPacker`] trait + impls for all three packers.
//! - Full [`MaximalRectanglesPacker`] BSSF (Best Short Side Fit)
//!   algorithm per Jylänki — maintains a list of maximal free
//!   rectangles, picks the one with the smallest residual
//!   short-side after placing the glyph, splits intersecting free
//!   rects into up to four maximal pieces, and prunes contained
//!   rectangles on every alloc.
//!
//! ## Companion surfaces now wired outside this module
//!
//! - `frankenterm/window/src/bitmaps/atlas.rs` consumes
//!   `Box<dyn BinPacker>` and records packing metrics on allocation,
//!   clear, and grow paths.
//! - `crates/frankenterm-core/benches/atlas_packing.rs` compares
//!   packing efficiency across representative glyph corpora.
//! - `crates/frankenterm-core/tests/atlas_packing_jsonl_emission.rs`
//!   covers structured JSON-line scenario logging.
//! - `crates/frankenterm-core/src/atlas_doctor.rs` surfaces
//!   `packing_efficiency_pct`, `fragmentation_pct`, and
//!   `packer_in_use`-style rows.
//! - `crates/frankenterm-core/examples/atlas_packing_attestation.rs`
//!   refreshes `docs/perf/atlas-packing.json` for release
//!   attestation bundles.

#![allow(dead_code)]

// ============================================================================
// Atlas + glyph size types
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Atlas2DSize {
    pub width: u32,
    pub height: u32,
}

impl Atlas2DSize {
    /// Construct, returning `None` for zero dimensions.
    #[must_use]
    pub const fn try_new(width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            None
        } else {
            Some(Self { width, height })
        }
    }

    #[must_use]
    pub const fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

/// Glyph dimensions to allocate. Constructors require non-zero dimensions;
/// callers using the public fields directly can also represent zero sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphSize {
    pub width: u32,
    pub height: u32,
}

impl GlyphSize {
    #[must_use]
    pub const fn try_new(width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            None
        } else {
            Some(Self { width, height })
        }
    }

    /// Construct, panicking on zero dimensions. For tests +
    /// known-non-zero call sites only.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self::try_new(width, height).expect("GlyphSize dimensions must be > 0")
    }

    #[must_use]
    pub const fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

// ============================================================================
// PackedRect + AllocationOutcome
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackedRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PackedRect {
    #[must_use]
    pub const fn right(&self) -> u64 {
        self.x as u64 + self.width as u64
    }

    #[must_use]
    pub const fn bottom(&self) -> u64 {
        self.y as u64 + self.height as u64
    }

    /// Whether this rect overlaps another (edge-touching is
    /// non-overlap).
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        let h = (self.x as u64) < other.right() && (other.x as u64) < self.right();
        let v = (self.y as u64) < other.bottom() && (other.y as u64) < self.bottom();
        h && v
    }

    #[must_use]
    pub const fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectReason {
    /// Glyph wider than the atlas — cannot ever fit.
    GlyphWiderThanAtlas,
    /// Glyph taller than the atlas — cannot ever fit.
    GlyphTallerThanAtlas,
    /// No free area left.
    AtlasFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationOutcome {
    Placed(PackedRect),
    Rejected(RejectReason),
}

impl AllocationOutcome {
    #[must_use]
    pub fn placed(&self) -> Option<PackedRect> {
        match self {
            Self::Placed(r) => Some(*r),
            Self::Rejected(_) => None,
        }
    }

    #[must_use]
    pub fn reject_reason(&self) -> Option<RejectReason> {
        match self {
            Self::Placed(_) => None,
            Self::Rejected(r) => Some(*r),
        }
    }
}

// ============================================================================
// Packer kind + selection policy
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PackerKind {
    Shelf,
    Skyline,
    MaximalRectangles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackerSelectionThresholds {
    /// Atlases at or below this size on both axes use `Shelf`.
    /// Default `512` per the bead's hint that shelf is the legacy
    /// fallback for small static atlases.
    pub shelf_max_axis: u32,
    /// Atlases at or above this size on either axis use
    /// `MaximalRectangles`. Default `4096` per the bead.
    pub maximal_rectangles_min_axis: u32,
}

impl Default for PackerSelectionThresholds {
    fn default() -> Self {
        Self {
            shelf_max_axis: 512,
            maximal_rectangles_min_axis: 4096,
        }
    }
}

/// Adaptive selector. `Shelf` for ≤ shelf_max_axis on both axes,
/// `MaximalRectangles` for ≥ maximal_rectangles_min_axis on either
/// axis, `Skyline` for everything in between.
#[must_use]
pub fn select_packer(size: Atlas2DSize, thresholds: PackerSelectionThresholds) -> PackerKind {
    let max_axis = size.width.max(size.height);
    if size.width <= thresholds.shelf_max_axis && size.height <= thresholds.shelf_max_axis {
        return PackerKind::Shelf;
    }
    if max_axis >= thresholds.maximal_rectangles_min_axis {
        return PackerKind::MaximalRectangles;
    }
    PackerKind::Skyline
}

// ============================================================================
// Shelf packer
// ============================================================================

/// Row-based shelf-packer. Tracks the current shelf's `y` and the
/// cursor `x` along the active shelf; opens a new shelf when the
/// glyph doesn't fit on the current one. O(1) per allocation;
/// wasted space depends on the glyph sequence.
#[derive(Debug, Clone)]
pub struct ShelfPacker {
    size: Atlas2DSize,
    /// Top-y of the active shelf.
    shelf_y: u32,
    /// Tallest glyph placed on the active shelf (so far). Determines
    /// when to open the next shelf.
    shelf_height: u32,
    /// Next free x along the active shelf.
    cursor_x: u32,
    /// Allocations placed so far (for invariant checking + stats).
    placements: Vec<PackedRect>,
}

impl ShelfPacker {
    #[must_use]
    pub fn new(size: Atlas2DSize) -> Self {
        Self {
            size,
            shelf_y: 0,
            shelf_height: 0,
            cursor_x: 0,
            placements: Vec::new(),
        }
    }

    /// Try to allocate a glyph. O(1).
    pub fn try_alloc(&mut self, glyph: GlyphSize) -> AllocationOutcome {
        if glyph.width > self.size.width {
            return AllocationOutcome::Rejected(RejectReason::GlyphWiderThanAtlas);
        }
        if glyph.height > self.size.height {
            return AllocationOutcome::Rejected(RejectReason::GlyphTallerThanAtlas);
        }
        let cursor_right = u64::from(self.cursor_x) + u64::from(glyph.width);
        let shelf_glyph_bottom = u64::from(self.shelf_y) + u64::from(glyph.height);
        let atlas_width = u64::from(self.size.width);
        let atlas_height = u64::from(self.size.height);
        // Try the current shelf first. The active shelf may grow taller
        // until the tallest glyph on it would exceed the atlas bottom.
        let new_shelf_height = self.shelf_height.max(glyph.height);
        if cursor_right <= atlas_width
            && shelf_glyph_bottom <= atlas_height
            && u64::from(self.shelf_y) + u64::from(new_shelf_height) <= atlas_height
        {
            let rect = PackedRect {
                x: self.cursor_x,
                y: self.shelf_y,
                width: glyph.width,
                height: glyph.height,
            };
            self.cursor_x = u32::try_from(cursor_right)
                .expect("shelf cursor must remain inside u32 atlas bounds");
            self.shelf_height = new_shelf_height;
            self.placements.push(rect);
            return AllocationOutcome::Placed(rect);
        }
        // Open a new shelf below the current one.
        let new_shelf_y = u64::from(self.shelf_y) + u64::from(self.shelf_height);
        if new_shelf_y + u64::from(glyph.height) > atlas_height || glyph.width > self.size.width {
            return AllocationOutcome::Rejected(RejectReason::AtlasFull);
        }
        self.shelf_y =
            u32::try_from(new_shelf_y).expect("shelf y must remain inside u32 atlas bounds");
        self.shelf_height = glyph.height;
        self.cursor_x = glyph.width;
        let rect = PackedRect {
            x: 0,
            y: self.shelf_y,
            width: glyph.width,
            height: glyph.height,
        };
        self.placements.push(rect);
        AllocationOutcome::Placed(rect)
    }

    #[must_use]
    pub fn placements(&self) -> &[PackedRect] {
        &self.placements
    }

    #[must_use]
    pub fn size(&self) -> Atlas2DSize {
        self.size
    }
}

// ============================================================================
// Skyline packer
// ============================================================================

/// Bottom-Left skyline packer. Tracks the upper boundary of placed
/// glyphs as a list of horizontal segments (`SkylineNode`). For
/// each allocation, finds the segment where the glyph fits with the
/// lowest top-y, places it, and merges segments. ~5% wasted; O(N)
/// per allocation in the segment count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SkylineNode {
    x: u32,
    y: u32,
    width: u32,
}

#[derive(Debug, Clone)]
pub struct SkylinePacker {
    size: Atlas2DSize,
    skyline: Vec<SkylineNode>,
    placements: Vec<PackedRect>,
}

impl SkylinePacker {
    #[must_use]
    pub fn new(size: Atlas2DSize) -> Self {
        Self {
            size,
            skyline: vec![SkylineNode {
                x: 0,
                y: 0,
                width: size.width,
            }],
            placements: Vec::new(),
        }
    }

    pub fn try_alloc(&mut self, glyph: GlyphSize) -> AllocationOutcome {
        if glyph.width > self.size.width {
            return AllocationOutcome::Rejected(RejectReason::GlyphWiderThanAtlas);
        }
        if glyph.height > self.size.height {
            return AllocationOutcome::Rejected(RejectReason::GlyphTallerThanAtlas);
        }
        // Find the best fit: scan each segment, compute the highest
        // skyline y across the span [x, x+width), and pick the one
        // with lowest resulting top.
        let mut best: Option<(usize, u32, u32)> = None; // (segment_idx, place_x, place_y)
        for (i, node) in self.skyline.iter().enumerate() {
            if u64::from(node.x) + u64::from(glyph.width) > u64::from(self.size.width) {
                continue;
            }
            let span_top = self.span_top(i, glyph.width);
            if u64::from(span_top) + u64::from(glyph.height) > u64::from(self.size.height) {
                continue;
            }
            // Bottom-left rule: pick lowest y, ties to leftmost x.
            let candidate = (i, node.x, span_top);
            best = match best {
                None => Some(candidate),
                Some((_, _, by)) if span_top < by => Some(candidate),
                Some((_, bx, by)) if span_top == by && node.x < bx => Some(candidate),
                Some(b) => Some(b),
            };
        }

        let Some((seg_idx, place_x, place_y)) = best else {
            return AllocationOutcome::Rejected(RejectReason::AtlasFull);
        };

        // Place + update skyline.
        self.update_skyline(seg_idx, place_x, place_y, glyph.width, glyph.height);
        let rect = PackedRect {
            x: place_x,
            y: place_y,
            width: glyph.width,
            height: glyph.height,
        };
        self.placements.push(rect);
        AllocationOutcome::Placed(rect)
    }

    /// Compute the highest skyline y across the horizontal span
    /// `[skyline[i].x, skyline[i].x + width)`. Used during fit-search.
    fn span_top(&self, start_idx: usize, width: u32) -> u32 {
        let span_start = u64::from(self.skyline[start_idx].x);
        let span_end = span_start + u64::from(width);
        let mut top = self.skyline[start_idx].y;
        let mut x = span_start;
        let mut idx = start_idx;
        while x < span_end && idx < self.skyline.len() {
            let node = self.skyline[idx];
            if node.y > top {
                top = node.y;
            }
            x = u64::from(node.x) + u64::from(node.width);
            idx += 1;
        }
        top
    }

    /// After placing a glyph at `(place_x, place_y)` with size
    /// `(width, height)`, update the skyline: insert a new node at
    /// the glyph's top, remove fully-shadowed nodes, trim the
    /// trailing partially-shadowed node, and merge adjacent nodes
    /// with the same y.
    fn update_skyline(
        &mut self,
        start_idx: usize,
        place_x: u32,
        place_y: u32,
        width: u32,
        height: u32,
    ) {
        let new_top_y = u64::from(place_y) + u64::from(height);
        let new_node = SkylineNode {
            x: place_x,
            y: u32::try_from(new_top_y).expect("skyline top must remain inside u32 atlas bounds"),
            width,
        };
        let span_end = u64::from(place_x) + u64::from(width);

        // Remove fully-shadowed nodes (entire range fits inside the
        // glyph's horizontal span); trim a partially-shadowed node
        // at the trailing edge.
        let idx = start_idx;
        while idx < self.skyline.len() {
            let node = self.skyline[idx];
            if u64::from(node.x) >= span_end {
                break;
            }
            let node_end = u64::from(node.x) + u64::from(node.width);
            if node_end <= span_end {
                // Fully shadowed.
                self.skyline.remove(idx);
                continue;
            }
            // Partially shadowed: trim the leading part inside the
            // span; keep the trailing part.
            let trim = u32::try_from(span_end - u64::from(node.x))
                .expect("skyline trim must remain inside u32 atlas bounds");
            self.skyline[idx].x = u32::try_from(span_end)
                .expect("skyline node x must remain inside u32 atlas bounds");
            self.skyline[idx].width -= trim;
            break;
        }

        // Insert the new node at the right position to keep the
        // skyline sorted by x.
        let insert_at = self
            .skyline
            .iter()
            .position(|n| n.x >= place_x)
            .unwrap_or(self.skyline.len());
        self.skyline.insert(insert_at, new_node);

        // Merge adjacent nodes with the same y.
        let mut i = 0;
        while i + 1 < self.skyline.len() {
            if self.skyline[i].y == self.skyline[i + 1].y {
                let merged_width =
                    u64::from(self.skyline[i].width) + u64::from(self.skyline[i + 1].width);
                self.skyline[i].width = u32::try_from(merged_width)
                    .expect("merged skyline width must remain inside u32 atlas bounds");
                self.skyline.remove(i + 1);
            } else {
                i += 1;
            }
        }
    }

    #[must_use]
    pub fn placements(&self) -> &[PackedRect] {
        &self.placements
    }

    #[must_use]
    pub fn size(&self) -> Atlas2DSize {
        self.size
    }
}

// ============================================================================
// Maximal-Rectangles packer (BSSF — Best Short Side Fit)
// ============================================================================

/// One maximal free rectangle in the [`MaximalRectanglesPacker`]'s
/// free list. Two maximal rectangles may overlap — that is the
/// defining property of the algorithm: the list represents every
/// maximal-area free zone, not a partition of the unused atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FreeRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[cfg(test)]
thread_local! {
    static CONTAINMENT_CHECKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl FreeRect {
    #[inline]
    const fn right(&self) -> u64 {
        self.x as u64 + self.width as u64
    }

    #[inline]
    const fn bottom(&self) -> u64 {
        self.y as u64 + self.height as u64
    }

    #[inline]
    const fn fits(&self, glyph: GlyphSize) -> bool {
        self.width >= glyph.width && self.height >= glyph.height
    }

    /// Strict overlap with the rectangle `(x, y, w, h)`. Edge-touching
    /// is treated as disjoint, matching [`PackedRect::overlaps`].
    #[inline]
    fn intersects(&self, x: u32, y: u32, w: u32, h: u32) -> bool {
        let glyph_right = u64::from(x) + u64::from(w);
        let glyph_bottom = u64::from(y) + u64::from(h);
        u64::from(x) < self.right()
            && u64::from(self.x) < glyph_right
            && u64::from(y) < self.bottom()
            && u64::from(self.y) < glyph_bottom
    }

    /// Whether `self` fully contains `other` (with edge-coincidence
    /// counting as containment). Used to prune redundant maximal
    /// rectangles after a split.
    #[inline]
    fn contains(&self, other: &Self) -> bool {
        #[cfg(test)]
        CONTAINMENT_CHECKS.with(|count| count.set(count.get().saturating_add(1)));
        self.x <= other.x
            && self.y <= other.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }
}

/// Maximal-rectangles bin packer using the Best Short Side Fit
/// heuristic from Jukka Jylänki, *A Thousand Ways to Pack the Bin*.
///
/// Per allocation the packer:
///
/// 1. Scans every free rectangle large enough to hold the glyph and
///    picks the one with the smallest residual short side
///    (`min(free.w - g.w, free.h - g.h)`). Ties go to the smaller
///    long side, then to the upper-left corner — the same Bottom-Left
///    rule [`SkylinePacker`] uses, so the placement is deterministic
///    run-to-run for identical input.
/// 2. Places the glyph at the chosen free rect's upper-left corner.
/// 3. Splits every free rect that intersects the placed glyph into
///    up to four new maximal rectangles (above, below, left, right).
/// 4. Prunes any free rect fully contained in another.
///
/// Pruning costs `O(N * K)` for `N` candidate free rectangles and `K`
/// newly split strips. Unchanged rectangles are already mutually maximal
/// and need no repeated containment checks. Worst-case cost remains
/// `O(N²)` when an allocation splits a large fraction of the free list.
#[derive(Debug, Clone)]
pub struct MaximalRectanglesPacker {
    size: Atlas2DSize,
    free_rects: Vec<FreeRect>,
    placements: Vec<PackedRect>,
}

impl MaximalRectanglesPacker {
    #[must_use]
    pub fn new(size: Atlas2DSize) -> Self {
        Self {
            size,
            free_rects: vec![FreeRect {
                x: 0,
                y: 0,
                width: size.width,
                height: size.height,
            }],
            placements: Vec::new(),
        }
    }

    pub fn try_alloc(&mut self, glyph: GlyphSize) -> AllocationOutcome {
        if glyph.width > self.size.width {
            return AllocationOutcome::Rejected(RejectReason::GlyphWiderThanAtlas);
        }
        if glyph.height > self.size.height {
            return AllocationOutcome::Rejected(RejectReason::GlyphTallerThanAtlas);
        }

        // BSSF scan with deterministic tie-break.
        let mut best: Option<(usize, u32, u32, u32, u32)> = None;
        // (idx, short_side, long_side, x, y)
        for (i, free) in self.free_rects.iter().enumerate() {
            if !free.fits(glyph) {
                continue;
            }
            let leftover_w = free.width - glyph.width;
            let leftover_h = free.height - glyph.height;
            let short = leftover_w.min(leftover_h);
            let long = leftover_w.max(leftover_h);
            let candidate = (i, short, long, free.x, free.y);
            best = match best {
                None => Some(candidate),
                Some((_, bs, _, _, _)) if short < bs => Some(candidate),
                Some((_, bs, bl, _, _)) if short == bs && long < bl => Some(candidate),
                Some((_, bs, bl, bx, _)) if short == bs && long == bl && free.x < bx => {
                    Some(candidate)
                }
                Some((_, bs, bl, bx, by))
                    if short == bs && long == bl && free.x == bx && free.y < by =>
                {
                    Some(candidate)
                }
                Some(b) => Some(b),
            };
        }

        let Some((_, _, _, place_x, place_y)) = best else {
            return AllocationOutcome::Rejected(RejectReason::AtlasFull);
        };

        // Split every free rect intersecting the placed glyph.
        let glyph_right = u64::from(place_x) + u64::from(glyph.width);
        let glyph_bottom = u64::from(place_y) + u64::from(glyph.height);
        let mut new_free: Vec<FreeRect> = Vec::with_capacity(self.free_rects.len() + 4);
        let mut split_indices = Vec::new();
        for free in &self.free_rects {
            if !free.intersects(place_x, place_y, glyph.width, glyph.height) {
                new_free.push(*free);
                continue;
            }
            // Above strip — exists iff the glyph's top edge is below
            // the free rect's top.
            if place_y > free.y {
                split_indices.push(new_free.len());
                new_free.push(FreeRect {
                    x: free.x,
                    y: free.y,
                    width: free.width,
                    height: place_y - free.y,
                });
            }
            // Below strip.
            if glyph_bottom < free.bottom() {
                split_indices.push(new_free.len());
                new_free.push(FreeRect {
                    x: free.x,
                    y: u32::try_from(glyph_bottom)
                        .expect("free-rect y must remain inside u32 atlas bounds"),
                    width: free.width,
                    height: u32::try_from(free.bottom() - glyph_bottom)
                        .expect("free-rect height must remain inside u32 atlas bounds"),
                });
            }
            // Left strip.
            if place_x > free.x {
                split_indices.push(new_free.len());
                new_free.push(FreeRect {
                    x: free.x,
                    y: free.y,
                    width: place_x - free.x,
                    height: free.height,
                });
            }
            // Right strip.
            if glyph_right < free.right() {
                split_indices.push(new_free.len());
                new_free.push(FreeRect {
                    x: u32::try_from(glyph_right)
                        .expect("free-rect x must remain inside u32 atlas bounds"),
                    y: free.y,
                    width: u32::try_from(free.right() - glyph_right)
                        .expect("free-rect width must remain inside u32 atlas bounds"),
                    height: free.height,
                });
            }
        }

        // The old free list is an antichain under containment. Every new
        // strip is contained in its old parent, so it cannot contain an
        // unchanged old rectangle: that would contradict the old antichain.
        // Only new strips can therefore become redundant. Check each against
        // the full candidate list, retaining the last equal rectangle just
        // as the original all-pairs pruning did. A containing candidate may
        // itself be removed; transitivity still makes this removal valid.
        split_indices.retain(|&i| {
            let rect = &new_free[i];
            new_free
                .iter()
                .enumerate()
                .any(|(j, other)| j != i && other.contains(rect) && (other != rect || j > i))
        });
        // Indices remain sorted. One stable compaction preserves the exact
        // original survivor order, including allocation tie breaking.
        let mut removed = split_indices.into_iter().peekable();
        let mut index = 0;
        new_free.retain(|_| {
            let discard = removed.peek() == Some(&index);
            if discard {
                removed.next();
            }
            index += 1;
            !discard
        });

        self.free_rects = new_free;
        let rect = PackedRect {
            x: place_x,
            y: place_y,
            width: glyph.width,
            height: glyph.height,
        };
        self.placements.push(rect);
        AllocationOutcome::Placed(rect)
    }

    #[must_use]
    pub fn placements(&self) -> &[PackedRect] {
        &self.placements
    }

    #[must_use]
    pub fn size(&self) -> Atlas2DSize {
        self.size
    }

    /// Current count of maximal free rectangles. Exposed for the
    /// fragmentation telemetry the `ft doctor` follow-on reports.
    #[must_use]
    pub fn free_rect_count(&self) -> usize {
        self.free_rects.len()
    }
}

// ============================================================================
// BinPacker trait + factory
// ============================================================================

/// Trait surface shared by every packer in this module. The GUI atlas
/// consumes a `Box<dyn BinPacker>` so the algorithm can be swapped at
/// construction time without recompiling the renderer.
pub trait BinPacker {
    /// Allocate space for `glyph`, returning the placed rectangle or a
    /// rejection reason. Implementations must preserve the
    /// `non_overlapping(placements())` invariant after every call.
    fn try_alloc(&mut self, glyph: GlyphSize) -> AllocationOutcome;

    /// All glyphs the packer has placed so far, in allocation order.
    fn placements(&self) -> &[PackedRect];

    /// Atlas dimensions the packer was constructed with.
    fn size(&self) -> Atlas2DSize;

    /// Identifies which algorithm is in use. Used by `ft doctor` to
    /// surface `packer_in_use` per atlas.
    fn kind(&self) -> PackerKind;

    /// Reset all internal state so the packer behaves as if freshly
    /// constructed at its current `size()`. Existing `placements()`
    /// are dropped; subsequent `try_alloc` calls start from an empty
    /// atlas.
    ///
    /// br-ft-gtcm9 substrate-pass: this trait extension is the
    /// missing surface the GUI atlas integration (item 1) needs in
    /// order to swap `guillotiere::SimpleAtlasAllocator` (which
    /// exposes a `clear` method on its `Allocator` surface) for
    /// `Box<dyn BinPacker>`. The wired-pass cont-bead consumes
    /// this method.
    fn clear(&mut self);
}

impl BinPacker for ShelfPacker {
    fn try_alloc(&mut self, glyph: GlyphSize) -> AllocationOutcome {
        ShelfPacker::try_alloc(self, glyph)
    }

    fn placements(&self) -> &[PackedRect] {
        ShelfPacker::placements(self)
    }

    fn size(&self) -> Atlas2DSize {
        ShelfPacker::size(self)
    }

    fn kind(&self) -> PackerKind {
        PackerKind::Shelf
    }

    fn clear(&mut self) {
        self.shelf_y = 0;
        self.shelf_height = 0;
        self.cursor_x = 0;
        self.placements.clear();
    }
}

impl BinPacker for SkylinePacker {
    fn try_alloc(&mut self, glyph: GlyphSize) -> AllocationOutcome {
        SkylinePacker::try_alloc(self, glyph)
    }

    fn placements(&self) -> &[PackedRect] {
        SkylinePacker::placements(self)
    }

    fn size(&self) -> Atlas2DSize {
        SkylinePacker::size(self)
    }

    fn kind(&self) -> PackerKind {
        PackerKind::Skyline
    }

    fn clear(&mut self) {
        self.skyline.clear();
        self.skyline.push(SkylineNode {
            x: 0,
            y: 0,
            width: self.size.width,
        });
        self.placements.clear();
    }
}

impl BinPacker for MaximalRectanglesPacker {
    fn try_alloc(&mut self, glyph: GlyphSize) -> AllocationOutcome {
        MaximalRectanglesPacker::try_alloc(self, glyph)
    }

    fn placements(&self) -> &[PackedRect] {
        MaximalRectanglesPacker::placements(self)
    }

    fn size(&self) -> Atlas2DSize {
        MaximalRectanglesPacker::size(self)
    }

    fn kind(&self) -> PackerKind {
        PackerKind::MaximalRectangles
    }

    fn clear(&mut self) {
        self.free_rects.clear();
        self.free_rects.push(FreeRect {
            x: 0,
            y: 0,
            width: self.size.width,
            height: self.size.height,
        });
        self.placements.clear();
    }
}

/// Construct a packer for `kind` at the given atlas size. Pair with
/// [`select_packer`] to honour the bead's adaptive policy:
///
/// ```ignore
/// let kind = select_packer(size, PackerSelectionThresholds::default());
/// let packer = make_packer(kind, size);
/// ```
#[must_use]
pub fn make_packer(kind: PackerKind, size: Atlas2DSize) -> Box<dyn BinPacker> {
    match kind {
        PackerKind::Shelf => Box::new(ShelfPacker::new(size)),
        PackerKind::Skyline => Box::new(SkylinePacker::new(size)),
        PackerKind::MaximalRectangles => Box::new(MaximalRectanglesPacker::new(size)),
    }
}

// ============================================================================
// Non-overlap invariant checker
// ============================================================================

/// Returns the first overlap pair (i, j) in `placements` if any. Used
/// in tests + integration's debug-mode assertions to enforce the
/// bead's "packing is non-overlapping" invariant.
#[must_use]
pub fn first_overlap(placements: &[PackedRect]) -> Option<(usize, usize)> {
    for i in 0..placements.len() {
        for j in (i + 1)..placements.len() {
            if placements[i].overlaps(&placements[j]) {
                return Some((i, j));
            }
        }
    }
    None
}

#[must_use]
pub fn non_overlapping(placements: &[PackedRect]) -> bool {
    first_overlap(placements).is_none()
}

// ============================================================================
// Stats
// ============================================================================

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackingStats {
    pub alloc_total: u64,
    pub reject_total: u64,
    /// Sum of placed rectangle areas in pixels, despite the historical field name.
    pub used_bytes: u64,
    /// Atlas area in pixels; multiply by the pixel format's byte width for storage.
    pub atlas_bytes: u64,
}

impl PackingStats {
    pub fn record_placed(&mut self, rect: PackedRect) {
        self.alloc_total = self.alloc_total.saturating_add(1);
        self.used_bytes = self.used_bytes.saturating_add(rect.area());
    }

    pub fn record_reject(&mut self) {
        self.reject_total = self.reject_total.saturating_add(1);
    }

    pub fn record_atlas_size(&mut self, size: Atlas2DSize) {
        self.atlas_bytes = size.area();
    }

    /// Packing efficiency as integer percent `[0..=100]`. `0` when
    /// the atlas is empty / unset.
    #[must_use]
    pub fn efficiency_pct(&self) -> u32 {
        if self.atlas_bytes == 0 {
            return 0;
        }
        ((u128::from(self.used_bytes) * 100) / u128::from(self.atlas_bytes)).min(100) as u32
    }

    /// Wasted space as integer percent. `100 - efficiency_pct` when
    /// the atlas is set, `0` when unset.
    #[must_use]
    pub fn wasted_pct(&self) -> u32 {
        if self.atlas_bytes == 0 {
            return 0;
        }
        100 - self.efficiency_pct()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(w: u32, h: u32) -> GlyphSize {
        GlyphSize::new(w, h)
    }

    fn atlas(w: u32, h: u32) -> Atlas2DSize {
        Atlas2DSize::try_new(w, h).unwrap()
    }

    // ----------------------------------------------------------------
    // Atlas2DSize / GlyphSize
    // ----------------------------------------------------------------

    #[test]
    fn atlas_size_rejects_zero() {
        assert!(Atlas2DSize::try_new(0, 100).is_none());
        assert!(Atlas2DSize::try_new(100, 0).is_none());
        assert!(Atlas2DSize::try_new(100, 100).is_some());
    }

    #[test]
    fn atlas_size_area() {
        assert_eq!(atlas(100, 200).area(), 20_000);
    }

    #[test]
    fn glyph_size_rejects_zero() {
        assert!(GlyphSize::try_new(0, 5).is_none());
        assert!(GlyphSize::try_new(5, 0).is_none());
    }

    // ----------------------------------------------------------------
    // PackedRect
    // ----------------------------------------------------------------

    #[test]
    fn rect_overlaps_and_disjoint() {
        let a = PackedRect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let b = PackedRect {
            x: 5,
            y: 5,
            width: 10,
            height: 10,
        };
        let c = PackedRect {
            x: 20,
            y: 20,
            width: 5,
            height: 5,
        };
        assert!(a.overlaps(&b));
        assert!(!a.overlaps(&c));
    }

    #[test]
    fn rect_edge_touching_is_disjoint() {
        let a = PackedRect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let b = PackedRect {
            x: 10,
            y: 0,
            width: 5,
            height: 5,
        };
        assert!(!a.overlaps(&b));
    }

    // ----------------------------------------------------------------
    // Packer selection
    // ----------------------------------------------------------------

    #[test]
    fn selector_picks_shelf_for_small_atlas() {
        let t = PackerSelectionThresholds::default();
        assert_eq!(select_packer(atlas(256, 256), t), PackerKind::Shelf);
        assert_eq!(select_packer(atlas(512, 512), t), PackerKind::Shelf);
    }

    #[test]
    fn selector_picks_skyline_for_medium_atlas() {
        let t = PackerSelectionThresholds::default();
        assert_eq!(select_packer(atlas(1024, 1024), t), PackerKind::Skyline);
        assert_eq!(select_packer(atlas(2048, 2048), t), PackerKind::Skyline);
    }

    #[test]
    fn selector_picks_maximal_rectangles_for_large_atlas() {
        let t = PackerSelectionThresholds::default();
        assert_eq!(
            select_packer(atlas(4096, 4096), t),
            PackerKind::MaximalRectangles
        );
        assert_eq!(
            select_packer(atlas(8192, 8192), t),
            PackerKind::MaximalRectangles
        );
    }

    #[test]
    fn selector_picks_maximal_rectangles_when_either_axis_is_large() {
        let t = PackerSelectionThresholds::default();
        // Non-square atlas: one axis above maximal_rectangles_min.
        assert_eq!(
            select_packer(atlas(4096, 1024), t),
            PackerKind::MaximalRectangles
        );
    }

    #[test]
    fn selector_threshold_overrides_work() {
        let t = PackerSelectionThresholds {
            shelf_max_axis: 1024,
            maximal_rectangles_min_axis: 2048,
        };
        assert_eq!(select_packer(atlas(1024, 1024), t), PackerKind::Shelf);
        assert_eq!(select_packer(atlas(1500, 1500), t), PackerKind::Skyline);
        assert_eq!(
            select_packer(atlas(2048, 2048), t),
            PackerKind::MaximalRectangles
        );
    }

    // ----------------------------------------------------------------
    // ShelfPacker
    // ----------------------------------------------------------------

    #[test]
    fn shelf_first_alloc_lands_at_origin() {
        let mut p = ShelfPacker::new(atlas(100, 100));
        let r = p.try_alloc(glyph(10, 8)).placed().unwrap();
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 0);
        assert_eq!(r.width, 10);
        assert_eq!(r.height, 8);
    }

    #[test]
    fn shelf_packs_along_first_row() {
        let mut p = ShelfPacker::new(atlas(100, 100));
        let r1 = p.try_alloc(glyph(10, 8)).placed().unwrap();
        let r2 = p.try_alloc(glyph(20, 8)).placed().unwrap();
        let r3 = p.try_alloc(glyph(15, 8)).placed().unwrap();
        assert_eq!(r1.x, 0);
        assert_eq!(r2.x, 10);
        assert_eq!(r3.x, 30);
        assert!(non_overlapping(p.placements()));
    }

    #[test]
    fn shelf_allows_current_row_to_grow_for_taller_glyph() {
        let mut p = ShelfPacker::new(atlas(100, 20));
        let r1 = p.try_alloc(glyph(10, 8)).placed().unwrap();
        let r2 = p.try_alloc(glyph(10, 12)).placed().unwrap();
        let r3 = p.try_alloc(glyph(90, 8)).placed().unwrap();

        assert_eq!(r1.y, 0);
        assert_eq!(r2.y, 0);
        assert_eq!(r2.x, 10);
        assert_eq!(r3.y, 12);
        assert_eq!(r3.x, 0);
        assert!(non_overlapping(p.placements()));
    }

    #[test]
    fn shelf_opens_new_row_when_first_full() {
        let mut p = ShelfPacker::new(atlas(20, 100));
        let r1 = p.try_alloc(glyph(15, 8)).placed().unwrap();
        let r2 = p.try_alloc(glyph(15, 8)).placed().unwrap();
        assert_eq!(r1.y, 0);
        assert_eq!(r2.y, 8);
        assert_eq!(r2.x, 0);
    }

    #[test]
    fn shelf_rejects_glyph_wider_than_atlas() {
        let mut p = ShelfPacker::new(atlas(100, 100));
        let outcome = p.try_alloc(glyph(101, 5));
        assert_eq!(
            outcome.reject_reason(),
            Some(RejectReason::GlyphWiderThanAtlas)
        );
    }

    #[test]
    fn shelf_rejects_glyph_taller_than_atlas() {
        let mut p = ShelfPacker::new(atlas(100, 100));
        let outcome = p.try_alloc(glyph(5, 101));
        assert_eq!(
            outcome.reject_reason(),
            Some(RejectReason::GlyphTallerThanAtlas)
        );
    }

    #[test]
    fn shelf_atlas_full_when_no_more_rows_fit() {
        // 20x10 atlas; place a 20x10 glyph; next alloc should reject.
        let mut p = ShelfPacker::new(atlas(20, 10));
        p.try_alloc(glyph(20, 10)).placed().unwrap();
        let outcome = p.try_alloc(glyph(5, 5));
        assert_eq!(outcome.reject_reason(), Some(RejectReason::AtlasFull));
    }

    #[test]
    fn shelf_near_u32_max_width_opens_new_row_without_overflow() {
        let mut p = ShelfPacker::new(atlas(u32::MAX, 2));
        let first = p.try_alloc(glyph(u32::MAX, 1)).placed().unwrap();
        assert_eq!(first.x, 0);
        assert_eq!(first.y, 0);

        let second = p.try_alloc(glyph(1, 1)).placed().unwrap();
        assert_eq!(second.x, 0);
        assert_eq!(second.y, 1);
        assert!(non_overlapping(p.placements()));
    }

    #[test]
    fn shelf_non_overlap_invariant_after_many_allocs() {
        let mut p = ShelfPacker::new(atlas(100, 100));
        // Pack 50 small glyphs.
        for i in 0..50 {
            let h = (i % 5) as u32 + 4; // 4..9
            let w = (i % 7) as u32 + 3; // 3..10
            let _ = p.try_alloc(glyph(w, h));
        }
        assert!(
            non_overlapping(p.placements()),
            "shelf must produce non-overlapping rects"
        );
    }

    // ----------------------------------------------------------------
    // SkylinePacker
    // ----------------------------------------------------------------

    #[test]
    fn skyline_first_alloc_lands_at_origin() {
        let mut p = SkylinePacker::new(atlas(100, 100));
        let r = p.try_alloc(glyph(10, 8)).placed().unwrap();
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 0);
    }

    #[test]
    fn skyline_packs_three_glyphs_non_overlapping() {
        let mut p = SkylinePacker::new(atlas(100, 100));
        p.try_alloc(glyph(10, 8)).placed().unwrap();
        p.try_alloc(glyph(20, 12)).placed().unwrap();
        p.try_alloc(glyph(15, 6)).placed().unwrap();
        assert!(non_overlapping(p.placements()));
    }

    #[test]
    fn skyline_uses_horizontal_space_efficiently() {
        // Place 5 small glyphs that should pack into the bottom row
        // first.
        let mut p = SkylinePacker::new(atlas(50, 50));
        let r1 = p.try_alloc(glyph(10, 5)).placed().unwrap();
        let r2 = p.try_alloc(glyph(10, 5)).placed().unwrap();
        let r3 = p.try_alloc(glyph(10, 5)).placed().unwrap();
        let r4 = p.try_alloc(glyph(10, 5)).placed().unwrap();
        let r5 = p.try_alloc(glyph(10, 5)).placed().unwrap();
        // All 5 should land at y=0 since they fit horizontally.
        for r in [r1, r2, r3, r4, r5] {
            assert_eq!(r.y, 0);
        }
        assert!(non_overlapping(p.placements()));
    }

    #[test]
    fn skyline_climbs_when_row_full() {
        let mut p = SkylinePacker::new(atlas(20, 50));
        // Two 20x10 glyphs side-by-side won't fit; second goes above
        // the first.
        let r1 = p.try_alloc(glyph(20, 10)).placed().unwrap();
        let r2 = p.try_alloc(glyph(20, 10)).placed().unwrap();
        assert_eq!(r1.y, 0);
        assert_eq!(r2.y, 10);
    }

    #[test]
    fn skyline_uses_left_alignment_for_tied_y() {
        // After placing one glyph, the next should pick the left-most
        // free segment (bottom-left rule).
        let mut p = SkylinePacker::new(atlas(100, 100));
        let r1 = p.try_alloc(glyph(30, 10)).placed().unwrap();
        let r2 = p.try_alloc(glyph(20, 5)).placed().unwrap();
        // Both at y=0; r1 covers x=0..30; r2 should pick x=30.
        assert_eq!(r1.x, 0);
        assert_eq!(r1.y, 0);
        assert_eq!(r2.y, 0);
        assert_eq!(r2.x, 30);
    }

    #[test]
    fn skyline_atlas_full_rejects() {
        // Fill a tiny atlas exactly; further allocs reject.
        let mut p = SkylinePacker::new(atlas(10, 10));
        p.try_alloc(glyph(10, 10)).placed().unwrap();
        let outcome = p.try_alloc(glyph(1, 1));
        assert_eq!(outcome.reject_reason(), Some(RejectReason::AtlasFull));
    }

    #[test]
    fn skyline_near_u32_max_tail_node_does_not_wrap_fit_check() {
        let mut p = SkylinePacker::new(atlas(u32::MAX, 2));
        let first = p.try_alloc(glyph(u32::MAX - 1, 1)).placed().unwrap();
        assert_eq!(first.x, 0);
        assert_eq!(first.y, 0);

        let second = p.try_alloc(glyph(2, 1)).placed().unwrap();
        assert_eq!(second.x, 0);
        assert_eq!(second.y, 1);
        assert!(u32::try_from(second.right()).is_ok());
        assert!(non_overlapping(p.placements()));
    }

    #[test]
    fn skyline_rejects_glyph_wider_than_atlas() {
        let mut p = SkylinePacker::new(atlas(50, 50));
        assert_eq!(
            p.try_alloc(glyph(60, 5)).reject_reason(),
            Some(RejectReason::GlyphWiderThanAtlas)
        );
    }

    #[test]
    fn skyline_non_overlap_invariant_with_mixed_sizes() {
        let mut p = SkylinePacker::new(atlas(100, 100));
        let sizes = [
            (15, 8),
            (10, 12),
            (20, 6),
            (8, 15),
            (12, 10),
            (25, 8),
            (6, 6),
            (18, 14),
            (10, 10),
            (14, 7),
        ];
        for (w, h) in sizes {
            let _ = p.try_alloc(glyph(w, h));
        }
        assert!(
            non_overlapping(p.placements()),
            "skyline must produce non-overlapping rects"
        );
    }

    // ----------------------------------------------------------------
    // MaximalRectanglesPacker (BSSF)
    // ----------------------------------------------------------------

    /// Frozen pre-optimization allocation oracle. Keep its all-pairs pruning
    /// independent of the production pruning path so survivor order and equal
    /// rectangle tie breaking are compared after every allocation.
    fn legacy_maximal_alloc(
        packer: &mut MaximalRectanglesPacker,
        size: GlyphSize,
    ) -> AllocationOutcome {
        if size.width > packer.size.width {
            return AllocationOutcome::Rejected(RejectReason::GlyphWiderThanAtlas);
        }
        if size.height > packer.size.height {
            return AllocationOutcome::Rejected(RejectReason::GlyphTallerThanAtlas);
        }
        let best = packer
            .free_rects
            .iter()
            .filter(|free| free.fits(size))
            .min_by_key(|free| {
                let w = free.width - size.width;
                let h = free.height - size.height;
                (w.min(h), w.max(h), free.x, free.y)
            });
        let Some(best) = best else {
            return AllocationOutcome::Rejected(RejectReason::AtlasFull);
        };
        let placed = PackedRect {
            x: best.x,
            y: best.y,
            width: size.width,
            height: size.height,
        };
        let right = u64::from(placed.x) + u64::from(size.width);
        let bottom = u64::from(placed.y) + u64::from(size.height);
        let mut free_rects = Vec::new();
        for free in &packer.free_rects {
            if !free.intersects(placed.x, placed.y, size.width, size.height) {
                free_rects.push(*free);
                continue;
            }
            if placed.y > free.y {
                free_rects.push(FreeRect {
                    height: placed.y - free.y,
                    ..*free
                });
            }
            if bottom < free.bottom() {
                free_rects.push(FreeRect {
                    y: u32::try_from(bottom).unwrap(),
                    height: u32::try_from(free.bottom() - bottom).unwrap(),
                    ..*free
                });
            }
            if placed.x > free.x {
                free_rects.push(FreeRect {
                    width: placed.x - free.x,
                    ..*free
                });
            }
            if right < free.right() {
                free_rects.push(FreeRect {
                    x: u32::try_from(right).unwrap(),
                    width: u32::try_from(free.right() - right).unwrap(),
                    ..*free
                });
            }
        }
        let mut i = 0;
        while i < free_rects.len() {
            let mut removed_i = false;
            let mut j = i + 1;
            while j < free_rects.len() {
                if free_rects[j].contains(&free_rects[i]) {
                    free_rects.remove(i);
                    removed_i = true;
                    break;
                }
                if free_rects[i].contains(&free_rects[j]) {
                    free_rects.remove(j);
                } else {
                    j += 1;
                }
            }
            if !removed_i {
                i += 1;
            }
        }
        packer.free_rects = free_rects;
        packer.placements.push(placed);
        AllocationOutcome::Placed(placed)
    }

    #[test]
    fn maximal_rectangles_matches_legacy_allocation_streams() {
        for seed in 0_u64..24 {
            let mut random = seed + 1;
            let mut current = MaximalRectanglesPacker::new(atlas(128, 128));
            let mut legacy = current.clone();
            for step in 0..256 {
                random = random
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let width = ((random >> 32) % 40) as u32;
                random = random
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let height = ((random >> 32) % 40) as u32;
                // Public fields also permit degenerate dimensions; compare
                // that existing behavior without calling the nonzero helper.
                let request = GlyphSize { width, height };
                assert_eq!(
                    current.try_alloc(request),
                    legacy_maximal_alloc(&mut legacy, request),
                    "seed={seed} step={step}"
                );
                assert_eq!(
                    current.free_rects, legacy.free_rects,
                    "seed={seed} step={step}"
                );
                assert_eq!(current.placements, legacy.placements);
                if step % 97 == 96 {
                    current.clear();
                    legacy.clear();
                }
            }
        }
        for edge in [0, 1, 16, u32::MAX] {
            let mut current = MaximalRectanglesPacker::new(Atlas2DSize {
                width: edge,
                height: edge,
            });
            let mut legacy = current.clone();
            for (width, height) in [
                (0, 0),
                (1, 1),
                (edge, 0),
                (0, edge),
                (edge, edge),
                (u32::MAX, u32::MAX),
            ] {
                let request = GlyphSize { width, height };
                assert_eq!(
                    current.try_alloc(request),
                    legacy_maximal_alloc(&mut legacy, request)
                );
                assert_eq!(current.free_rects, legacy.free_rects);
            }
        }
    }

    #[test]
    fn maximal_rectangles_zoom_workload() {
        for round in 0..3 {
            let mut packer = MaximalRectanglesPacker::new(atlas(4096, 4096));
            let mut fingerprint = 0xcbf2_9ce4_8422_2325_u64;
            CONTAINMENT_CHECKS.with(|count| count.set(0));
            let start = std::time::Instant::now();
            for index in 0..4096_u32 {
                let request = glyph(8 + (index * 17 % 31), 12 + (index * 29 % 37));
                let placed = packer
                    .try_alloc(std::hint::black_box(request))
                    .placed()
                    .unwrap();
                for value in [placed.x, placed.y, placed.width, placed.height] {
                    fingerprint = (fingerprint ^ u64::from(value)).wrapping_mul(0x100_0000_01b3);
                }
                for free in &packer.free_rects {
                    for value in [free.x, free.y, free.width, free.height] {
                        fingerprint =
                            (fingerprint ^ u64::from(value)).wrapping_mul(0x100_0000_01b3);
                    }
                }
            }
            let elapsed = start.elapsed();
            let comparisons = CONTAINMENT_CHECKS.with(std::cell::Cell::get);
            // Captured from the pre-optimization allocator, including every
            // placement and the complete ordered free list after each glyph.
            assert_eq!(fingerprint, 0x8d5c_7a7e_d8e5_0bfb);
            // The old all-pairs pass made 12,977,242,433 checks. Bound actual
            // work rather than wall time so reintroducing that scan fails
            // even on a fast or otherwise idle test worker.
            assert!(
                comparisons < 100_000_000,
                "atlas pruning repeated excessive containment work: {comparisons}"
            );
            assert!(non_overlapping(packer.placements()));
            println!(
                "ATLAS_ZOOM_WORKLOAD round={round} allocations={} free_rects={} contains={comparisons} fingerprint={fingerprint:016x} elapsed_us={}",
                packer.placements.len(),
                packer.free_rects.len(),
                elapsed.as_micros()
            );
        }
    }

    #[test]
    fn maximal_rectangles_first_alloc_lands_at_origin() {
        let mut p = MaximalRectanglesPacker::new(atlas(100, 100));
        let r = p.try_alloc(glyph(20, 30)).placed().unwrap();
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 0);
        assert_eq!(r.width, 20);
        assert_eq!(r.height, 30);
    }

    #[test]
    fn maximal_rectangles_three_glyphs_are_non_overlapping() {
        let mut p = MaximalRectanglesPacker::new(atlas(100, 100));
        p.try_alloc(glyph(40, 30)).placed().unwrap();
        p.try_alloc(glyph(30, 40)).placed().unwrap();
        p.try_alloc(glyph(20, 20)).placed().unwrap();
        assert!(non_overlapping(p.placements()));
        assert_eq!(p.placements().len(), 3);
    }

    #[test]
    fn maximal_rectangles_rejects_glyph_wider_than_atlas() {
        let mut p = MaximalRectanglesPacker::new(atlas(50, 50));
        assert_eq!(
            p.try_alloc(glyph(60, 10)).reject_reason(),
            Some(RejectReason::GlyphWiderThanAtlas)
        );
    }

    #[test]
    fn maximal_rectangles_rejects_glyph_taller_than_atlas() {
        let mut p = MaximalRectanglesPacker::new(atlas(50, 50));
        assert_eq!(
            p.try_alloc(glyph(10, 60)).reject_reason(),
            Some(RejectReason::GlyphTallerThanAtlas)
        );
    }

    #[test]
    fn maximal_rectangles_rejects_when_atlas_full() {
        let mut p = MaximalRectanglesPacker::new(atlas(20, 20));
        // Fill exactly with two 20x10 glyphs, then any further glyph
        // must be rejected as full.
        p.try_alloc(glyph(20, 10)).placed().unwrap();
        p.try_alloc(glyph(20, 10)).placed().unwrap();
        let outcome = p.try_alloc(glyph(5, 5));
        assert_eq!(outcome.reject_reason(), Some(RejectReason::AtlasFull));
    }

    #[test]
    fn maximal_rectangles_packs_uniform_glyphs_at_100_pct() {
        // 100x100 atlas; pack 10x10 glyphs until full. Should fit 100
        // exactly with zero waste.
        let mut p = MaximalRectanglesPacker::new(atlas(100, 100));
        let mut stats = PackingStats::default();
        stats.record_atlas_size(atlas(100, 100));
        for _ in 0..100 {
            match p.try_alloc(glyph(10, 10)) {
                AllocationOutcome::Placed(r) => stats.record_placed(r),
                AllocationOutcome::Rejected(_) => stats.record_reject(),
            }
        }
        assert!(non_overlapping(p.placements()));
        assert_eq!(stats.alloc_total, 100);
        assert_eq!(stats.efficiency_pct(), 100);
        assert_eq!(
            p.try_alloc(glyph(1, 1)).reject_reason(),
            Some(RejectReason::AtlasFull)
        );
    }

    #[test]
    fn maximal_rectangles_50_alloc_non_overlap_invariant() {
        let mut p = MaximalRectanglesPacker::new(atlas(256, 256));
        for i in 0..50 {
            let g = glyph(4 + (i % 13), 4 + (i % 11));
            let _ = p.try_alloc(g);
            assert!(
                non_overlapping(p.placements()),
                "non-overlap invariant must hold after every alloc"
            );
        }
    }

    #[test]
    fn maximal_rectangles_packs_at_least_as_many_as_shelf_on_mixed_corpus() {
        // The expected ordering from Jylänki's bench:
        //   maximal_rectangles >= skyline >= shelf  on packing efficiency.
        // Verify the lower bound (>= shelf) on a mixed corpus.
        let mut shelf = ShelfPacker::new(atlas(128, 128));
        let mut maximal = MaximalRectanglesPacker::new(atlas(128, 128));
        let glyphs: Vec<_> = (0..200)
            .map(|i| glyph(6 + (i % 17) as u32, 6 + (i % 13) as u32))
            .collect();
        let shelf_placed = glyphs
            .iter()
            .filter(|g| shelf.try_alloc(**g).placed().is_some())
            .count();
        let maximal_placed = glyphs
            .iter()
            .filter(|g| maximal.try_alloc(**g).placed().is_some())
            .count();
        assert!(non_overlapping(shelf.placements()));
        assert!(non_overlapping(maximal.placements()));
        assert!(
            maximal_placed >= shelf_placed,
            "maximal-rectangles packed {} but shelf packed {}",
            maximal_placed,
            shelf_placed,
        );
    }

    #[test]
    fn maximal_rectangles_free_rect_count_starts_at_one() {
        let p = MaximalRectanglesPacker::new(atlas(100, 100));
        assert_eq!(p.free_rect_count(), 1);
    }

    #[test]
    fn maximal_rectangles_free_rect_count_drops_to_zero_when_atlas_perfectly_filled() {
        let mut p = MaximalRectanglesPacker::new(atlas(20, 20));
        p.try_alloc(glyph(20, 20)).placed().unwrap();
        assert_eq!(p.free_rect_count(), 0);
    }

    // ----------------------------------------------------------------
    // BinPacker trait + factory
    // ----------------------------------------------------------------

    #[test]
    fn make_packer_dispatches_to_correct_kind() {
        let shelf = make_packer(PackerKind::Shelf, atlas(256, 256));
        let skyline = make_packer(PackerKind::Skyline, atlas(1024, 1024));
        let maximal = make_packer(PackerKind::MaximalRectangles, atlas(4096, 4096));
        assert_eq!(shelf.kind(), PackerKind::Shelf);
        assert_eq!(skyline.kind(), PackerKind::Skyline);
        assert_eq!(maximal.kind(), PackerKind::MaximalRectangles);
        assert_eq!(shelf.size(), atlas(256, 256));
        assert_eq!(skyline.size(), atlas(1024, 1024));
        assert_eq!(maximal.size(), atlas(4096, 4096));
    }

    #[test]
    fn make_packer_round_trip_with_select_packer() {
        let t = PackerSelectionThresholds::default();
        for size in [atlas(256, 256), atlas(1024, 1024), atlas(4096, 4096)] {
            let kind = select_packer(size, t);
            let packer = make_packer(kind, size);
            assert_eq!(packer.kind(), kind);
            assert_eq!(packer.size(), size);
        }
    }

    #[test]
    fn bin_packer_dyn_preserves_non_overlap_invariant() {
        let mut packers: Vec<Box<dyn BinPacker>> = vec![
            make_packer(PackerKind::Shelf, atlas(96, 96)),
            make_packer(PackerKind::Skyline, atlas(96, 96)),
            make_packer(PackerKind::MaximalRectangles, atlas(96, 96)),
        ];
        for p in &mut packers {
            for i in 0..40 {
                let g = glyph(5 + (i % 7), 5 + (i % 5));
                let _ = p.try_alloc(g);
            }
            assert!(
                non_overlapping(p.placements()),
                "{:?} packer broke non-overlap invariant under dyn dispatch",
                p.kind()
            );
        }
    }

    #[test]
    fn bin_packer_all_three_reject_oversized_glyph_consistently() {
        for kind in [
            PackerKind::Shelf,
            PackerKind::Skyline,
            PackerKind::MaximalRectangles,
        ] {
            let mut p = make_packer(kind, atlas(50, 50));
            assert_eq!(
                p.try_alloc(glyph(60, 10)).reject_reason(),
                Some(RejectReason::GlyphWiderThanAtlas),
                "{:?} packer must report wider-than-atlas",
                kind
            );
            assert_eq!(
                p.try_alloc(glyph(10, 60)).reject_reason(),
                Some(RejectReason::GlyphTallerThanAtlas),
                "{:?} packer must report taller-than-atlas",
                kind
            );
        }
    }

    // ----------------------------------------------------------------
    // PackingStats
    // ----------------------------------------------------------------

    #[test]
    fn stats_default_efficiency_zero() {
        let s = PackingStats::default();
        assert_eq!(s.efficiency_pct(), 0);
        assert_eq!(s.wasted_pct(), 0);
    }

    #[test]
    fn stats_efficiency_after_placements() {
        let mut s = PackingStats::default();
        s.record_atlas_size(atlas(100, 100)); // 10_000 pixels
        s.record_placed(PackedRect {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        }); // 2500
        s.record_placed(PackedRect {
            x: 50,
            y: 0,
            width: 50,
            height: 50,
        }); // 2500
        // 5000 / 10000 = 50%
        assert_eq!(s.efficiency_pct(), 50);
        assert_eq!(s.wasted_pct(), 50);
        assert_eq!(s.alloc_total, 2);
    }

    #[test]
    fn stats_record_reject() {
        let mut s = PackingStats::default();
        s.record_reject();
        s.record_reject();
        assert_eq!(s.reject_total, 2);
        assert_eq!(s.alloc_total, 0);
    }

    #[test]
    fn stats_efficiency_caps_at_100() {
        let mut s = PackingStats::default();
        s.record_atlas_size(atlas(10, 10));
        // Synthetic over-provisioning; defensive cap.
        s.used_bytes = 200;
        assert_eq!(s.efficiency_pct(), 100);
    }

    #[test]
    fn stats_efficiency_handles_full_width_counters() {
        for (used_bytes, atlas_bytes, expected) in [
            (u64::MAX, u64::MAX, 100),
            (u64::MAX / 2, u64::MAX, 49),
            (u64::MAX - 1, u64::MAX, 99),
            (u64::MAX, 1, 100),
            (u64::MAX, 0, 0),
            (0, u64::MAX, 0),
        ] {
            let stats = PackingStats {
                used_bytes,
                atlas_bytes,
                ..PackingStats::default()
            };
            assert_eq!(stats.efficiency_pct(), expected);
            assert_eq!(
                stats.wasted_pct(),
                if atlas_bytes == 0 { 0 } else { 100 - expected }
            );
        }
    }

    #[test]
    fn stats_saturated_placements_keep_full_efficiency() {
        let mut stats = PackingStats::default();
        stats.record_atlas_size(atlas(u32::MAX, u32::MAX));
        let rect = PackedRect {
            x: 0,
            y: 0,
            width: u32::MAX,
            height: u32::MAX,
        };
        stats.record_placed(rect);
        assert_eq!(stats.efficiency_pct(), 100);
        stats.record_placed(rect);
        assert_eq!(stats.used_bytes, u64::MAX);
        assert_eq!(stats.efficiency_pct(), 100);
        assert_eq!(stats.wasted_pct(), 0);
    }

    // ----------------------------------------------------------------
    // Cross-cut: realistic glyph-pack scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_fill_skyline_with_uniform_glyphs() {
        // 100x100 atlas; pack 10x10 glyphs until full. Should fit 100
        // exactly with zero waste.
        let mut p = SkylinePacker::new(atlas(100, 100));
        let mut stats = PackingStats::default();
        stats.record_atlas_size(atlas(100, 100));
        for _ in 0..100 {
            let outcome = p.try_alloc(glyph(10, 10));
            match outcome {
                AllocationOutcome::Placed(r) => stats.record_placed(r),
                AllocationOutcome::Rejected(_) => stats.record_reject(),
            }
        }
        // Next alloc should reject.
        let outcome = p.try_alloc(glyph(10, 10));
        assert_eq!(outcome.reject_reason(), Some(RejectReason::AtlasFull));
        assert!(non_overlapping(p.placements()));
        assert_eq!(stats.alloc_total, 100);
        assert_eq!(stats.efficiency_pct(), 100);
    }

    #[test]
    fn scenario_shelf_then_skyline_efficiency_comparison() {
        // Pack the same uniform corpus into both packers; on uniform
        // input shelf is competitive with skyline.
        let mut shelf = ShelfPacker::new(atlas(64, 64));
        let mut skyline = SkylinePacker::new(atlas(64, 64));
        let glyphs = (0..50).map(|i| glyph(8 + (i % 4), 8 + (i % 3)));
        let mut shelf_stats = PackingStats::default();
        let mut skyline_stats = PackingStats::default();
        shelf_stats.record_atlas_size(atlas(64, 64));
        skyline_stats.record_atlas_size(atlas(64, 64));
        for g in glyphs {
            if let AllocationOutcome::Placed(r) = shelf.try_alloc(g) {
                shelf_stats.record_placed(r);
            } else {
                shelf_stats.record_reject();
            }
            if let AllocationOutcome::Placed(r) = skyline.try_alloc(g) {
                skyline_stats.record_placed(r);
            } else {
                skyline_stats.record_reject();
            }
        }
        // Both should produce non-overlapping placements.
        assert!(non_overlapping(shelf.placements()));
        assert!(non_overlapping(skyline.placements()));
        // Both should pack at least 30 glyphs successfully (sanity).
        assert!(shelf_stats.alloc_total >= 30);
        assert!(skyline_stats.alloc_total >= 30);
    }

    #[test]
    fn scenario_selector_dispatches_through_realistic_atlas_sizes() {
        let t = PackerSelectionThresholds::default();
        // Small static atlas: shelf.
        assert_eq!(select_packer(atlas(256, 256), t), PackerKind::Shelf);
        // Default ft atlas (1024x1024 mid-range): skyline.
        assert_eq!(select_packer(atlas(1024, 1024), t), PackerKind::Skyline);
        // CJK / emoji-rich session (4K atlas): maximal rectangles.
        assert_eq!(
            select_packer(atlas(4096, 4096), t),
            PackerKind::MaximalRectangles
        );
    }

    // br-ft-gtcm9 substrate-pass: BinPacker::clear() round-trip.
    // After clear(), each packer must accept the same allocation
    // sequence that filled it (proving internal state was actually
    // reset, not just truncated).

    fn clear_round_trip<P: BinPacker>(mut packer: P) {
        // Pre-fill with a few placements.
        for _ in 0..8 {
            let outcome = packer.try_alloc(GlyphSize {
                width: 32,
                height: 32,
            });
            assert!(matches!(outcome, AllocationOutcome::Placed(_)));
        }
        assert_eq!(packer.placements().len(), 8);

        // Reset.
        packer.clear();
        assert_eq!(
            packer.placements().len(),
            0,
            "clear() must drop all placements"
        );

        // Same sequence must succeed identically post-clear.
        for i in 0..8 {
            let outcome = packer.try_alloc(GlyphSize {
                width: 32,
                height: 32,
            });
            assert!(
                matches!(outcome, AllocationOutcome::Placed(_)),
                "post-clear allocation #{i} must succeed identically to pre-clear"
            );
        }
        assert_eq!(packer.placements().len(), 8);
    }

    #[test]
    fn shelf_packer_clear_resets_to_empty_atlas() {
        clear_round_trip(ShelfPacker::new(atlas(256, 256)));
    }

    #[test]
    fn skyline_packer_clear_resets_to_empty_atlas() {
        clear_round_trip(SkylinePacker::new(atlas(256, 256)));
    }

    #[test]
    fn maximal_rectangles_packer_clear_resets_to_empty_atlas() {
        clear_round_trip(MaximalRectanglesPacker::new(atlas(256, 256)));
    }

    #[test]
    fn make_packer_factory_returns_clearable_box_dyn() {
        // The GUI atlas integration (item 1 cont-bead) consumes
        // make_packer's Box<dyn BinPacker>; verify the dyn-dispatched
        // clear() call works through the trait object.
        let mut packer: Box<dyn BinPacker> = make_packer(PackerKind::Shelf, atlas(256, 256));
        assert!(matches!(
            packer.try_alloc(GlyphSize {
                width: 16,
                height: 16,
            }),
            AllocationOutcome::Placed(_)
        ));
        assert_eq!(packer.placements().len(), 1);
        packer.clear();
        assert_eq!(packer.placements().len(), 0);
    }
}
