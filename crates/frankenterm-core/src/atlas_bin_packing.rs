//! Atlas bin-packing algorithms (ft-mpc9b.1.4).
//!
//! Per Jukka Jylänki's *A Thousand Ways to Pack the Bin*, several
//! algorithms trade speed for waste:
//!
//! - **Shelf-packing** — fastest, ~10% wasted; row-based, ideal for
//!   uniform-height glyph runs (small static atlases).
//! - **Skyline** — simple, ~5% wasted, online; the default the bead
//!   names for atlases below 2048².
//! - **Maximal rectangles** — tighter (~2% wasted), slower; the bead's
//!   default for atlases above 4096². Substrate carries the
//!   `MaximalRectangles` placeholder type + the dispatch decision so
//!   the integration layer can drop the algorithm in without
//!   rewriting the selector.
//!
//! ## What this module ships
//!
//! - `Atlas2DSize { width, height }` and `GlyphSize` with non-zero
//!   invariant.
//! - `PackedRect { x, y, width, height }` — the result of an
//!   allocation. The integration layer copies this into the atlas's
//!   per-glyph metadata (cross-link `atlas_stability.rs` ft-mpc9b.1.1).
//! - `AllocationOutcome` — `Placed(rect)` / `Rejected(reason)`.
//! - `PackerKind` — `Shelf | Skyline | MaximalRectangles`. The
//!   adaptive selector picks one from atlas size.
//! - `select_packer(size) -> PackerKind` — pure-logic policy.
//!   Configurable thresholds via `PackerSelectionThresholds`.
//! - `ShelfPacker` and `SkylinePacker` — both implement the
//!   `BinPacker` trait surface (just `try_alloc`); `MaximalRectanglesPacker`
//!   is a placeholder until the integration bead ships the full
//!   algorithm.
//! - `non_overlapping` — invariant checker for tests + integration's
//!   debug-mode assertions. Returns the first overlap pair if any.
//! - `PackingStats` — running counters (`alloc_total`,
//!   `reject_total`, `wasted_bytes`, `used_bytes`) + `efficiency_pct`
//!   for `ft doctor`.
//!
//! ## What is deferred to the integration bead (ft-mpc9b.1.4.cont)
//!
//! - `BinPacker` trait wired into
//!   `frankenterm/window/src/bitmaps/atlas.rs`.
//! - The full `MaximalRectanglesPacker` algorithm — the bead hints at
//!   the BSSF (Best Short Side Fit) heuristic; the substrate ships
//!   the dispatch hook + a `Rejected(MaximalRectanglesNotImplemented)`
//!   placeholder.
//! - Bench harness comparing packing efficiency on a representative
//!   glyph corpus (Latin + CJK + Nerd Font + emoji).
//! - JSON-line structured logging at
//!   `tests/atlas_packing/logs/<scenario>.jsonl`.
//! - `ft doctor` surface for `packing_efficiency_pct` /
//!   `fragmentation_pct` / `packer_in_use`.
//! - Per-release attestation entry (BR-RC-FOUNDATION.G3.1 cross-link).

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

/// Glyph dimensions to allocate. Non-zero on construction so the
/// packer's invariants hold without per-call zero-checks.
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
    /// `MaximalRectanglesPacker` placeholder; the integration bead
    /// fills in the algorithm.
    MaximalRectanglesNotImplemented,
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
    /// Atlases at or below this size on either axis use `Shelf`.
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
/// axis, `Skyline` for everything in between (the default for the
/// bead's "<2048²" range).
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
/// glyph doesn't fit on the current one. ~10% wasted space; O(1)
/// per allocation.
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
        // Try the current shelf first.
        if self.cursor_x + glyph.width <= self.size.width
            && self.shelf_y + glyph.height <= self.size.height
            && (glyph.height <= self.shelf_height
                || self.shelf_y + glyph.height <= self.size.height)
        {
            // Place on current shelf if cursor + glyph width fits;
            // expand shelf height to max of (current, glyph).
            let new_shelf_height = self.shelf_height.max(glyph.height);
            // Check that growing the shelf doesn't bust the atlas.
            if self.shelf_y + new_shelf_height <= self.size.height {
                let rect = PackedRect {
                    x: self.cursor_x,
                    y: self.shelf_y,
                    width: glyph.width,
                    height: glyph.height,
                };
                self.cursor_x += glyph.width;
                self.shelf_height = new_shelf_height;
                self.placements.push(rect);
                return AllocationOutcome::Placed(rect);
            }
        }
        // Open a new shelf below the current one.
        let new_shelf_y = self.shelf_y + self.shelf_height;
        if new_shelf_y + glyph.height > self.size.height || glyph.width > self.size.width {
            return AllocationOutcome::Rejected(RejectReason::AtlasFull);
        }
        self.shelf_y = new_shelf_y;
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
            if node.x + glyph.width > self.size.width {
                continue;
            }
            let span_top = self.span_top(i, glyph.width);
            if span_top + glyph.height > self.size.height {
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
        let span_start = self.skyline[start_idx].x;
        let span_end = span_start + width;
        let mut top = self.skyline[start_idx].y;
        let mut x = span_start;
        let mut idx = start_idx;
        while x < span_end && idx < self.skyline.len() {
            let node = self.skyline[idx];
            if node.y > top {
                top = node.y;
            }
            x = node.x + node.width;
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
        let new_top_y = place_y + height;
        let new_node = SkylineNode {
            x: place_x,
            y: new_top_y,
            width,
        };
        let span_end = place_x + width;

        // Remove fully-shadowed nodes (entire range fits inside the
        // glyph's horizontal span); trim a partially-shadowed node
        // at the trailing edge.
        let mut idx = start_idx;
        while idx < self.skyline.len() {
            let node = self.skyline[idx];
            if node.x >= span_end {
                break;
            }
            let node_end = node.x + node.width;
            if node_end <= span_end {
                // Fully shadowed.
                self.skyline.remove(idx);
                continue;
            }
            // Partially shadowed: trim the leading part inside the
            // span; keep the trailing part.
            let trim = span_end - node.x;
            self.skyline[idx].x += trim;
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
                self.skyline[i].width += self.skyline[i + 1].width;
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
// Maximal-Rectangles placeholder
// ============================================================================

/// Placeholder for the BSSF (Best Short Side Fit) maximal-rectangles
/// packer. The integration bead implements the full algorithm; this
/// substrate ships the type + dispatch hook so the selector logic
/// works end-to-end.
#[derive(Debug, Clone)]
pub struct MaximalRectanglesPacker {
    size: Atlas2DSize,
}

impl MaximalRectanglesPacker {
    #[must_use]
    pub fn new(size: Atlas2DSize) -> Self {
        Self { size }
    }

    pub fn try_alloc(&mut self, _glyph: GlyphSize) -> AllocationOutcome {
        AllocationOutcome::Rejected(RejectReason::MaximalRectanglesNotImplemented)
    }

    #[must_use]
    pub fn size(&self) -> Atlas2DSize {
        self.size
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
    pub used_bytes: u64,
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
        ((self.used_bytes * 100) / self.atlas_bytes).min(100) as u32
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
    // MaximalRectanglesPacker (placeholder)
    // ----------------------------------------------------------------

    #[test]
    fn maximal_rectangles_placeholder_rejects_with_specific_reason() {
        let mut p = MaximalRectanglesPacker::new(atlas(4096, 4096));
        let outcome = p.try_alloc(glyph(10, 10));
        assert_eq!(
            outcome.reject_reason(),
            Some(RejectReason::MaximalRectanglesNotImplemented)
        );
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
        s.record_atlas_size(atlas(100, 100)); // 10_000 bytes
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
}
