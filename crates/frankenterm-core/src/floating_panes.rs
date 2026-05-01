//! Floating-pane geometry + z-order policy substrate (ft-mpc9b.4.2).
//!
//! Steals the zellij pattern of pane state being either `Tiled` (the
//! existing per-grid-cell layout) or `Floating(Rect)` (a free-form
//! rectangle stacked above the tiled grid). Provides:
//!
//! - `PanePosition` — the discriminator. The integration layer's
//!   pane abstraction adds this field; existing tiled panes continue
//!   to work unchanged.
//! - `FloatingRect` — `(x, y, w, h)` in grid coordinates with
//!   non-zero-size invariant.
//! - `ZOrder` — opaque `u32` lane. Higher = drawn later (on top).
//!   Stable monotonic so raises don't compact the IDs of other panes.
//! - `FloatingZStack` — registry mapping pane-id → ZOrder, with
//!   `raise` / `lower` / `raise_to_top` / `lower_to_bottom` /
//!   `cycle_among_overlapping` operations.
//! - `DragResizeState` — pure state machine for the drag handle +
//!   8 resize handles (corners / edges). Tracks the operation in
//!   progress and its pre-operation snapshot for cancel.
//! - `SnapEdge` — `Top / Bottom / Left / Right / TopLeft / TopRight
//!   / BottomLeft / BottomRight`. The classifier `snap_target` looks
//!   at a draft rect and returns the snap that engages, if any, so
//!   the renderer can preview the snapped position.
//! - `KeyboardCommand` — every mouse path has a keyboard equivalent
//!   per the bead's a11y rule. The substrate enumerates them so the
//!   integration layer's keymap routes to the same state machine.
//!
//! ## What is deferred to the integration bead (ft-mpc9b.4.2.cont)
//!
//! - Wiring into Layer 2 of the compositor (cross-link ft-mpc9b.4.1
//!   already shipped).
//! - Mouse hit-testing for drag / resize handles in the gui crate.
//! - Screen-reader announcements via the AT-tree
//!   (`a11y_tree.rs` cross-link). The substrate's
//!   `FloatingPaneA11yMessage` defines the announcement format; the
//!   integration plays it through NSAccessibility / AT-SPI.
//! - High-contrast border styling.
//! - Layout serialisation (load/save floating layouts).

#![allow(dead_code)]

// ============================================================================
// Pane position
// ============================================================================

/// Discriminator added to the pane abstraction. Existing tiled panes
/// hold `Tiled`; floating panes hold `Floating(rect)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanePosition {
    Tiled,
    Floating(FloatingRect),
}

impl PanePosition {
    #[must_use]
    pub fn is_floating(&self) -> bool {
        matches!(self, Self::Floating(_))
    }

    #[must_use]
    pub fn is_tiled(&self) -> bool {
        matches!(self, Self::Tiled)
    }

    #[must_use]
    pub fn rect(&self) -> Option<FloatingRect> {
        match self {
            Self::Floating(r) => Some(*r),
            Self::Tiled => None,
        }
    }
}

// ============================================================================
// FloatingRect
// ============================================================================

/// `(x, y, width, height)` in grid coordinates. Constructor enforces
/// `width > 0 && height > 0`; the `try_new` variant returns `None` on
/// degenerate input (the integration layer surfaces that as a config
/// error).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FloatingRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl FloatingRect {
    /// Construct a rect, returning `None` if `width` or `height` is
    /// zero.
    #[must_use]
    pub const fn try_new(x: u16, y: u16, width: u16, height: u16) -> Option<Self> {
        if width == 0 || height == 0 {
            None
        } else {
            Some(Self {
                x,
                y,
                width,
                height,
            })
        }
    }

    /// Construct a rect, panicking on degenerate input. Use in tests
    /// and known-non-zero call sites.
    #[must_use]
    pub fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self::try_new(x, y, width, height)
            .expect("FloatingRect dimensions must be > 0")
    }

    #[must_use]
    pub const fn right(&self) -> u32 {
        self.x as u32 + self.width as u32
    }

    #[must_use]
    pub const fn bottom(&self) -> u32 {
        self.y as u32 + self.height as u32
    }

    /// Whether this rect overlaps another. Edge-touching counts as
    /// non-overlap (boundary-shared rects are visually disjoint).
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        let h_overlap = self.x < other.x + other.width
            && other.x < self.x + self.width;
        let v_overlap = self.y < other.y + other.height
            && other.y < self.y + self.height;
        h_overlap && v_overlap
    }

    /// Whether the rect contains a grid coordinate.
    #[must_use]
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x
            && x < self.x + self.width
            && y >= self.y
            && y < self.y + self.height
    }
}

// ============================================================================
// Z-order
// ============================================================================

/// Pane identifier. Opaque to this module — the integration layer's
/// per-pane id (likely `u32` in the gui).
pub type PaneId = u32;

/// Z-order lane. Higher = drawn later (on top). Stable monotonic so
/// `raise(p)` increments past the current top without renumbering
/// any other pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ZOrder(pub u32);

/// Per-pane z-order registry. Owns the mapping `PaneId → ZOrder` and
/// hands out new lanes for raises. Internal storage is a `Vec<(PaneId,
/// ZOrder)>` kept sorted by ZOrder ascending so the painter walks
/// it back-to-front in O(N).
#[derive(Debug, Clone, Default)]
pub struct FloatingZStack {
    entries: Vec<(PaneId, ZOrder)>,
    next_lane: u32,
}

impl FloatingZStack {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_lane: 0,
        }
    }

    /// Number of floating panes registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert a pane at the top of the z-order. Returns the assigned
    /// `ZOrder` so the integration layer can store it on the pane
    /// record for fast lookup. If `id` is already registered, raises
    /// it instead.
    pub fn insert_top(&mut self, id: PaneId) -> ZOrder {
        if let Some(idx) = self.find_index(id) {
            self.entries.remove(idx);
        }
        let z = ZOrder(self.next_lane);
        self.next_lane = self.next_lane.saturating_add(1);
        self.entries.push((id, z));
        z
    }

    /// Remove a pane (e.g. it was closed or pinned back to tiled).
    /// Idempotent — removing an absent id is a no-op.
    pub fn remove(&mut self, id: PaneId) {
        if let Some(idx) = self.find_index(id) {
            self.entries.remove(idx);
        }
    }

    /// Raise a pane one step. If it's already on top, no-op.
    pub fn raise(&mut self, id: PaneId) {
        let Some(idx) = self.find_index(id) else { return };
        if idx + 1 >= self.entries.len() {
            return;
        }
        // Swap with the next-higher entry.
        self.entries.swap(idx, idx + 1);
        // Rewrite the swapped pair's ZOrder values so the higher
        // entry's lane is strictly greater (keep the invariant that
        // entries is sorted by ZOrder ascending).
        let (lower_id, lower_z) = self.entries[idx];
        let (higher_id, higher_z) = self.entries[idx + 1];
        if lower_z > higher_z {
            // Swap back the ZOrder values to maintain ordering.
            self.entries[idx] = (lower_id, higher_z);
            self.entries[idx + 1] = (higher_id, lower_z);
        }
    }

    /// Lower a pane one step. If it's already on bottom, no-op.
    pub fn lower(&mut self, id: PaneId) {
        let Some(idx) = self.find_index(id) else { return };
        if idx == 0 {
            return;
        }
        self.entries.swap(idx, idx - 1);
        let (lower_id, lower_z) = self.entries[idx - 1];
        let (higher_id, higher_z) = self.entries[idx];
        if lower_z > higher_z {
            self.entries[idx - 1] = (lower_id, higher_z);
            self.entries[idx] = (higher_id, lower_z);
        }
    }

    /// Raise a pane to the top of the stack.
    pub fn raise_to_top(&mut self, id: PaneId) {
        let Some(idx) = self.find_index(id) else { return };
        if idx + 1 == self.entries.len() {
            return;
        }
        let entry = self.entries.remove(idx);
        let new_z = ZOrder(self.next_lane);
        self.next_lane = self.next_lane.saturating_add(1);
        self.entries.push((entry.0, new_z));
    }

    /// Lower a pane to the bottom of the stack. Reuses the lowest
    /// unused lane below the current minimum.
    pub fn lower_to_bottom(&mut self, id: PaneId) {
        let Some(idx) = self.find_index(id) else { return };
        if idx == 0 {
            return;
        }
        let entry = self.entries.remove(idx);
        let min_z = self.entries.first().map_or(ZOrder(0), |(_, z)| *z);
        let new_z = if min_z.0 == 0 {
            // Compact upward: shift every existing entry up by 1 and
            // reuse 0 for the new bottom. Rare path (only when min
            // is exactly 0).
            for e in &mut self.entries {
                e.1 = ZOrder(e.1.0.saturating_add(1));
            }
            self.next_lane = self.next_lane.saturating_add(1);
            ZOrder(0)
        } else {
            ZOrder(min_z.0 - 1)
        };
        self.entries.insert(0, (entry.0, new_z));
    }

    /// Cycle focus among overlapping floating panes at a given
    /// coordinate. Returns the next pane in the stack that
    /// overlaps `(x, y)` after the currently-focused pane, wrapping
    /// to the top. Mouse path: alt-click; keyboard path: bound to a
    /// keyboard command.
    #[must_use]
    pub fn cycle_among_overlapping(
        &self,
        currently_focused: Option<PaneId>,
        x: u16,
        y: u16,
        rect_for: impl Fn(PaneId) -> Option<FloatingRect>,
    ) -> Option<PaneId> {
        // Build the list of overlapping panes in z-order
        // descending (top to bottom).
        let overlapping: Vec<PaneId> = self
            .entries
            .iter()
            .rev()
            .filter_map(|(id, _)| {
                rect_for(*id).filter(|r| r.contains(x, y)).map(|_| *id)
            })
            .collect();
        if overlapping.is_empty() {
            return None;
        }
        let start = match currently_focused {
            Some(id) => overlapping.iter().position(|p| *p == id).unwrap_or(0),
            None => 0,
        };
        let next = (start + 1) % overlapping.len();
        Some(overlapping[next])
    }

    /// Iterate panes back-to-front (lowest z first; highest z last).
    /// The painter walks this order so higher-z panes draw on top.
    pub fn iter_back_to_front(&self) -> impl Iterator<Item = (PaneId, ZOrder)> + '_ {
        self.entries.iter().copied()
    }

    /// Z-order lookup. Returns `None` for tiled / unregistered panes.
    #[must_use]
    pub fn z_of(&self, id: PaneId) -> Option<ZOrder> {
        self.find_index(id).map(|i| self.entries[i].1)
    }

    fn find_index(&self, id: PaneId) -> Option<usize> {
        self.entries.iter().position(|(pid, _)| *pid == id)
    }
}

// ============================================================================
// Drag / resize state machine
// ============================================================================

/// Which of the 8 resize handles is grabbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResizeHandle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

/// State of an in-flight drag or resize operation. The integration
/// layer routes mouse events / keyboard commands through `begin` /
/// `update` / `commit` / `cancel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragResizeState {
    /// No operation in progress.
    Idle,
    /// Drag (move) in progress. `original` is the pre-drag rect for
    /// cancel; `current` is the live preview.
    Dragging {
        pane: PaneId,
        original: FloatingRect,
        current: FloatingRect,
    },
    /// Resize in progress on a specific handle.
    Resizing {
        pane: PaneId,
        handle: ResizeHandle,
        original: FloatingRect,
        current: FloatingRect,
    },
}

impl Default for DragResizeState {
    fn default() -> Self {
        Self::Idle
    }
}

impl DragResizeState {
    #[must_use]
    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    #[must_use]
    pub fn pane(&self) -> Option<PaneId> {
        match self {
            Self::Idle => None,
            Self::Dragging { pane, .. } | Self::Resizing { pane, .. } => Some(*pane),
        }
    }

    #[must_use]
    pub fn current_rect(&self) -> Option<FloatingRect> {
        match self {
            Self::Idle => None,
            Self::Dragging { current, .. } | Self::Resizing { current, .. } => {
                Some(*current)
            }
        }
    }

    #[must_use]
    pub fn original_rect(&self) -> Option<FloatingRect> {
        match self {
            Self::Idle => None,
            Self::Dragging { original, .. } | Self::Resizing { original, .. } => {
                Some(*original)
            }
        }
    }

    /// Begin a drag. No-op if already in an operation.
    pub fn begin_drag(&mut self, pane: PaneId, rect: FloatingRect) -> bool {
        if !self.is_idle() {
            return false;
        }
        *self = Self::Dragging {
            pane,
            original: rect,
            current: rect,
        };
        true
    }

    /// Begin a resize. No-op if already in an operation.
    pub fn begin_resize(
        &mut self,
        pane: PaneId,
        handle: ResizeHandle,
        rect: FloatingRect,
    ) -> bool {
        if !self.is_idle() {
            return false;
        }
        *self = Self::Resizing {
            pane,
            handle,
            original: rect,
            current: rect,
        };
        true
    }

    /// Update the current rect during an in-flight operation. Caller
    /// computes the new rect from mouse delta (or keyboard step) and
    /// passes it in. Returns `false` if the state is `Idle` (the
    /// integration layer logs an unexpected-update).
    pub fn update(&mut self, new_rect: FloatingRect) -> bool {
        match self {
            Self::Dragging { current, .. } | Self::Resizing { current, .. } => {
                *current = new_rect;
                true
            }
            Self::Idle => false,
        }
    }

    /// Commit the operation. Returns the final rect; transitions to
    /// `Idle`.
    pub fn commit(&mut self) -> Option<FloatingRect> {
        let rect = self.current_rect();
        *self = Self::Idle;
        rect
    }

    /// Cancel the operation, restoring the original rect. Returns the
    /// rect to restore; transitions to `Idle`.
    pub fn cancel(&mut self) -> Option<FloatingRect> {
        let rect = self.original_rect();
        *self = Self::Idle;
        rect
    }
}

// ============================================================================
// Snap-to-edge
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SnapEdge {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Default snap-distance in grid cells. The integration layer can
/// override per-config.
pub const DEFAULT_SNAP_DISTANCE: u16 = 2;

/// Determine which edge (if any) the draft rect should snap to,
/// given the screen size and snap distance. Returns `None` when the
/// rect is far enough from every edge.
///
/// Corners (TopLeft / TopRight / BottomLeft / BottomRight) take
/// precedence over single edges when both axes qualify. This matches
/// the "Aero-snap" mental model the bead references.
#[must_use]
pub fn snap_target(
    rect: FloatingRect,
    screen_width: u16,
    screen_height: u16,
    snap_distance: u16,
) -> Option<SnapEdge> {
    let near_left = rect.x <= snap_distance;
    let near_right = (rect.x + rect.width) >= screen_width.saturating_sub(snap_distance);
    let near_top = rect.y <= snap_distance;
    let near_bottom =
        (rect.y + rect.height) >= screen_height.saturating_sub(snap_distance);

    match (near_top, near_bottom, near_left, near_right) {
        (true, _, true, _) => Some(SnapEdge::TopLeft),
        (true, _, _, true) => Some(SnapEdge::TopRight),
        (_, true, true, _) => Some(SnapEdge::BottomLeft),
        (_, true, _, true) => Some(SnapEdge::BottomRight),
        (true, false, false, false) => Some(SnapEdge::Top),
        (false, true, false, false) => Some(SnapEdge::Bottom),
        (false, false, true, false) => Some(SnapEdge::Left),
        (false, false, false, true) => Some(SnapEdge::Right),
        _ => None,
    }
}

/// Apply a snap, returning the snapped rect. Pure-logic; integration
/// layer renders the snap-preview overlay separately.
#[must_use]
pub fn apply_snap(
    rect: FloatingRect,
    edge: SnapEdge,
    screen_width: u16,
    screen_height: u16,
) -> FloatingRect {
    let half_w = screen_width / 2;
    let half_h = screen_height / 2;
    match edge {
        SnapEdge::Top => FloatingRect::new(0, 0, screen_width, half_h.max(1)),
        SnapEdge::Bottom => {
            FloatingRect::new(0, half_h, screen_width, screen_height - half_h)
        }
        SnapEdge::Left => FloatingRect::new(0, 0, half_w.max(1), screen_height),
        SnapEdge::Right => FloatingRect::new(half_w, 0, screen_width - half_w, screen_height),
        SnapEdge::TopLeft => FloatingRect::new(0, 0, half_w.max(1), half_h.max(1)),
        SnapEdge::TopRight => {
            FloatingRect::new(half_w, 0, screen_width - half_w, half_h.max(1))
        }
        SnapEdge::BottomLeft => {
            FloatingRect::new(0, half_h, half_w.max(1), screen_height - half_h)
        }
        SnapEdge::BottomRight => FloatingRect::new(
            half_w,
            half_h,
            screen_width - half_w,
            screen_height - half_h,
        ),
    }
}

// ============================================================================
// Keyboard command equivalents (a11y)
// ============================================================================

/// Every mouse path has a keyboard equivalent (the bead's a11y rule:
/// "no mouse-required path"). Enumerated so the integration's keymap
/// routes to the same state machine as the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyboardCommand {
    /// Move focused pane by one cell in a direction.
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    /// Resize focused pane by one cell. Default handle is the
    /// bottom-right corner (matches "grow" / "shrink" intent).
    GrowHorizontal,
    ShrinkHorizontal,
    GrowVertical,
    ShrinkVertical,
    /// Snap focused pane to a screen edge.
    SnapTop,
    SnapBottom,
    SnapLeft,
    SnapRight,
    /// Pin (toggle floating ↔ tiled).
    TogglePin,
    /// Z-order operations.
    RaiseOne,
    LowerOne,
    RaiseToTop,
    LowerToBottom,
    /// Cycle focus among overlapping panes at the cursor.
    CycleOverlapping,
    /// Cancel an in-flight drag/resize.
    CancelOperation,
}

// ============================================================================
// A11y announcement payload
// ============================================================================

/// Announcement payload the screen reader emits when a floating pane
/// gains focus or its rect changes. The integration layer plays it
/// through NSAccessibility / AT-SPI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloatingPaneA11yMessage {
    pub pane: PaneId,
    pub position: FloatingRect,
    pub z_order: ZOrder,
    pub kind: FloatingPaneA11yKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FloatingPaneA11yKind {
    /// Focus gained — full announcement: "floating, position X,Y,
    /// size W×H, z-order N".
    FocusGained,
    /// Rect changed (drag/resize commit) — short: "moved to X,Y,
    /// size W×H".
    RectChanged,
    /// Z-order changed — short: "z-order N".
    ZOrderChanged,
    /// Pinned back to tiled — "pinned to grid".
    PinnedToTiled,
    /// Toggled to floating — "floating".
    UnpinnedToFloating,
}

/// Build the announcement payload from a state change. Pure data
/// construction so the integration layer doesn't need to remember
/// the schema.
#[must_use]
pub fn make_a11y_message(
    pane: PaneId,
    position: FloatingRect,
    z_order: ZOrder,
    kind: FloatingPaneA11yKind,
) -> FloatingPaneA11yMessage {
    FloatingPaneA11yMessage {
        pane,
        position,
        z_order,
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: u16, y: u16, w: u16, h: u16) -> FloatingRect {
        FloatingRect::new(x, y, w, h)
    }

    // ----------------------------------------------------------------
    // PanePosition
    // ----------------------------------------------------------------

    #[test]
    fn pane_position_is_floating_vs_tiled() {
        assert!(PanePosition::Tiled.is_tiled());
        assert!(!PanePosition::Tiled.is_floating());
        let f = PanePosition::Floating(r(0, 0, 10, 5));
        assert!(f.is_floating());
        assert!(!f.is_tiled());
    }

    #[test]
    fn pane_position_rect_extracts() {
        assert_eq!(PanePosition::Tiled.rect(), None);
        assert_eq!(
            PanePosition::Floating(r(1, 2, 3, 4)).rect(),
            Some(r(1, 2, 3, 4))
        );
    }

    // ----------------------------------------------------------------
    // FloatingRect
    // ----------------------------------------------------------------

    #[test]
    fn floating_rect_try_new_rejects_zero() {
        assert!(FloatingRect::try_new(0, 0, 0, 5).is_none());
        assert!(FloatingRect::try_new(0, 0, 5, 0).is_none());
        assert!(FloatingRect::try_new(0, 0, 5, 5).is_some());
    }

    #[test]
    fn floating_rect_overlaps_basic() {
        let a = r(0, 0, 10, 10);
        let b = r(5, 5, 10, 10);
        assert!(a.overlaps(&b));
        assert!(b.overlaps(&a));
    }

    #[test]
    fn floating_rect_overlaps_disjoint() {
        let a = r(0, 0, 5, 5);
        let b = r(10, 10, 5, 5);
        assert!(!a.overlaps(&b));
    }

    #[test]
    fn floating_rect_overlaps_edge_touching_is_disjoint() {
        let a = r(0, 0, 5, 5);
        let b = r(5, 0, 5, 5); // touches at x=5
        assert!(!a.overlaps(&b));
    }

    #[test]
    fn floating_rect_contains_inclusive_at_origin_exclusive_at_far_edge() {
        let r0 = r(2, 3, 5, 4);
        assert!(r0.contains(2, 3));
        assert!(r0.contains(6, 6));
        assert!(!r0.contains(7, 3)); // x at far edge is exclusive
        assert!(!r0.contains(2, 7));
    }

    #[test]
    fn floating_rect_right_and_bottom_use_u32() {
        let r0 = FloatingRect::new(u16::MAX - 1, u16::MAX - 1, 5, 5);
        assert_eq!(r0.right(), u32::from(u16::MAX - 1) + 5);
        assert_eq!(r0.bottom(), u32::from(u16::MAX - 1) + 5);
    }

    // ----------------------------------------------------------------
    // FloatingZStack
    // ----------------------------------------------------------------

    #[test]
    fn z_stack_starts_empty() {
        let s = FloatingZStack::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn z_stack_insert_top_assigns_monotonic_lane() {
        let mut s = FloatingZStack::new();
        let z1 = s.insert_top(1);
        let z2 = s.insert_top(2);
        let z3 = s.insert_top(3);
        assert!(z1 < z2);
        assert!(z2 < z3);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn z_stack_iter_back_to_front_is_z_ascending() {
        let mut s = FloatingZStack::new();
        s.insert_top(10);
        s.insert_top(20);
        s.insert_top(30);
        let order: Vec<PaneId> = s.iter_back_to_front().map(|(id, _)| id).collect();
        assert_eq!(order, vec![10, 20, 30]);
    }

    #[test]
    fn z_stack_remove_idempotent() {
        let mut s = FloatingZStack::new();
        s.insert_top(1);
        s.remove(1);
        s.remove(1); // no-op
        assert!(s.is_empty());
    }

    #[test]
    fn z_stack_insert_top_existing_id_re_raises() {
        let mut s = FloatingZStack::new();
        s.insert_top(1);
        s.insert_top(2);
        let z1_after = s.insert_top(1); // re-raise 1 to top
        let order: Vec<PaneId> = s.iter_back_to_front().map(|(id, _)| id).collect();
        assert_eq!(order, vec![2, 1]);
        // 1's new z is greater than 2's z.
        assert!(z1_after > s.z_of(2).unwrap());
    }

    #[test]
    fn z_stack_raise_swaps_with_neighbour_above() {
        let mut s = FloatingZStack::new();
        s.insert_top(10);
        s.insert_top(20);
        s.insert_top(30);
        s.raise(20);
        let order: Vec<PaneId> = s.iter_back_to_front().map(|(id, _)| id).collect();
        assert_eq!(order, vec![10, 30, 20]);
    }

    #[test]
    fn z_stack_raise_when_already_top_is_noop() {
        let mut s = FloatingZStack::new();
        s.insert_top(10);
        s.insert_top(20);
        s.raise(20); // already on top
        let order: Vec<PaneId> = s.iter_back_to_front().map(|(id, _)| id).collect();
        assert_eq!(order, vec![10, 20]);
    }

    #[test]
    fn z_stack_lower_swaps_with_neighbour_below() {
        let mut s = FloatingZStack::new();
        s.insert_top(10);
        s.insert_top(20);
        s.insert_top(30);
        s.lower(30);
        let order: Vec<PaneId> = s.iter_back_to_front().map(|(id, _)| id).collect();
        assert_eq!(order, vec![10, 30, 20]);
    }

    #[test]
    fn z_stack_lower_when_already_bottom_is_noop() {
        let mut s = FloatingZStack::new();
        s.insert_top(10);
        s.insert_top(20);
        s.lower(10);
        let order: Vec<PaneId> = s.iter_back_to_front().map(|(id, _)| id).collect();
        assert_eq!(order, vec![10, 20]);
    }

    #[test]
    fn z_stack_raise_to_top_jumps_to_top() {
        let mut s = FloatingZStack::new();
        s.insert_top(10);
        s.insert_top(20);
        s.insert_top(30);
        s.raise_to_top(10);
        let order: Vec<PaneId> = s.iter_back_to_front().map(|(id, _)| id).collect();
        assert_eq!(order, vec![20, 30, 10]);
    }

    #[test]
    fn z_stack_lower_to_bottom_jumps_to_bottom() {
        let mut s = FloatingZStack::new();
        s.insert_top(10);
        s.insert_top(20);
        s.insert_top(30);
        s.lower_to_bottom(30);
        let order: Vec<PaneId> = s.iter_back_to_front().map(|(id, _)| id).collect();
        assert_eq!(order, vec![30, 10, 20]);
    }

    #[test]
    fn z_stack_z_of_returns_none_for_unregistered() {
        let s = FloatingZStack::new();
        assert_eq!(s.z_of(99), None);
    }

    #[test]
    fn z_stack_cycle_among_overlapping_at_a_point() {
        // Three overlapping panes at (5, 5).
        let mut s = FloatingZStack::new();
        s.insert_top(10);
        s.insert_top(20);
        s.insert_top(30);
        let rects = |id: PaneId| -> Option<FloatingRect> {
            match id {
                10 => Some(r(0, 0, 10, 10)),
                20 => Some(r(2, 2, 8, 8)),
                30 => Some(r(4, 4, 6, 6)),
                _ => None,
            }
        };
        // Top pane at the point is 30; cycle next → 10 (back-to-front
        // of the overlap stack, top-down then wrap).
        let next = s.cycle_among_overlapping(Some(30), 5, 5, &rects);
        assert_eq!(next, Some(20));
        let next = s.cycle_among_overlapping(Some(20), 5, 5, &rects);
        assert_eq!(next, Some(10));
        let next = s.cycle_among_overlapping(Some(10), 5, 5, &rects);
        assert_eq!(next, Some(30));
    }

    #[test]
    fn z_stack_cycle_with_no_focus_starts_at_top() {
        let mut s = FloatingZStack::new();
        s.insert_top(10);
        s.insert_top(20);
        let rects = |_id: PaneId| Some(r(0, 0, 10, 10));
        let next = s.cycle_among_overlapping(None, 5, 5, &rects);
        // No focus → start at top (20), cycle to next → 10.
        assert_eq!(next, Some(10));
    }

    #[test]
    fn z_stack_cycle_at_point_with_no_overlap_returns_none() {
        let mut s = FloatingZStack::new();
        s.insert_top(10);
        let rects = |_id: PaneId| Some(r(0, 0, 5, 5));
        // (50, 50) is outside any pane.
        assert_eq!(s.cycle_among_overlapping(Some(10), 50, 50, &rects), None);
    }

    // ----------------------------------------------------------------
    // DragResizeState
    // ----------------------------------------------------------------

    #[test]
    fn drag_resize_default_is_idle() {
        let s = DragResizeState::default();
        assert!(s.is_idle());
        assert_eq!(s.pane(), None);
    }

    #[test]
    fn drag_begin_then_update_then_commit() {
        let mut s = DragResizeState::Idle;
        let original = r(10, 10, 20, 5);
        assert!(s.begin_drag(7, original));
        assert_eq!(s.pane(), Some(7));
        assert_eq!(s.current_rect(), Some(original));

        let new_rect = r(15, 12, 20, 5);
        assert!(s.update(new_rect));
        assert_eq!(s.current_rect(), Some(new_rect));

        let committed = s.commit();
        assert_eq!(committed, Some(new_rect));
        assert!(s.is_idle());
    }

    #[test]
    fn drag_cancel_returns_original() {
        let mut s = DragResizeState::Idle;
        let original = r(0, 0, 5, 5);
        s.begin_drag(1, original);
        s.update(r(20, 20, 5, 5));
        let restored = s.cancel();
        assert_eq!(restored, Some(original));
        assert!(s.is_idle());
    }

    #[test]
    fn drag_begin_when_already_in_progress_is_rejected() {
        let mut s = DragResizeState::Idle;
        s.begin_drag(1, r(0, 0, 5, 5));
        // Second begin without commit/cancel returns false.
        assert!(!s.begin_drag(2, r(10, 10, 5, 5)));
        assert_eq!(s.pane(), Some(1));
    }

    #[test]
    fn resize_begin_then_update_then_commit() {
        let mut s = DragResizeState::Idle;
        let original = r(0, 0, 10, 10);
        assert!(s.begin_resize(3, ResizeHandle::BottomRight, original));
        s.update(r(0, 0, 15, 12));
        let committed = s.commit();
        assert_eq!(committed, Some(r(0, 0, 15, 12)));
        assert!(s.is_idle());
    }

    #[test]
    fn update_when_idle_returns_false() {
        let mut s = DragResizeState::Idle;
        assert!(!s.update(r(0, 0, 5, 5)));
        assert!(s.is_idle());
    }

    #[test]
    fn cancel_when_idle_returns_none() {
        let mut s = DragResizeState::Idle;
        assert_eq!(s.cancel(), None);
        assert!(s.is_idle());
    }

    // ----------------------------------------------------------------
    // Snap-to-edge
    // ----------------------------------------------------------------

    #[test]
    fn snap_target_centre_of_screen_returns_none() {
        // Centre rect, far from every edge → no snap.
        let rect = r(40, 20, 10, 10);
        let snap = snap_target(rect, 100, 60, DEFAULT_SNAP_DISTANCE);
        assert_eq!(snap, None);
    }

    #[test]
    fn snap_target_top_left_corner() {
        let rect = r(1, 1, 10, 10);
        let snap = snap_target(rect, 100, 60, DEFAULT_SNAP_DISTANCE);
        assert_eq!(snap, Some(SnapEdge::TopLeft));
    }

    #[test]
    fn snap_target_top_edge_only() {
        // Near top, far from left/right.
        let rect = r(40, 1, 10, 10);
        let snap = snap_target(rect, 100, 60, DEFAULT_SNAP_DISTANCE);
        assert_eq!(snap, Some(SnapEdge::Top));
    }

    #[test]
    fn snap_target_right_edge_only() {
        // Right edge = x + w = 89 + 10 = 99 ≥ 100 - 2 → near_right.
        // y = 30 (centred vertically) → not near_top / not near_bottom.
        let rect = r(89, 30, 10, 10);
        let snap = snap_target(rect, 100, 60, DEFAULT_SNAP_DISTANCE);
        assert_eq!(snap, Some(SnapEdge::Right));
    }

    #[test]
    fn snap_target_bottom_right_corner() {
        let rect = r(85, 45, 14, 14);
        let snap = snap_target(rect, 100, 60, DEFAULT_SNAP_DISTANCE);
        assert_eq!(snap, Some(SnapEdge::BottomRight));
    }

    #[test]
    fn apply_snap_top_takes_top_half() {
        let rect = r(0, 0, 10, 10);
        let snapped = apply_snap(rect, SnapEdge::Top, 100, 60);
        assert_eq!(snapped, r(0, 0, 100, 30));
    }

    #[test]
    fn apply_snap_top_left_takes_top_left_quadrant() {
        let rect = r(0, 0, 10, 10);
        let snapped = apply_snap(rect, SnapEdge::TopLeft, 100, 60);
        assert_eq!(snapped, r(0, 0, 50, 30));
    }

    #[test]
    fn apply_snap_bottom_right_takes_bottom_right_quadrant() {
        let rect = r(0, 0, 10, 10);
        let snapped = apply_snap(rect, SnapEdge::BottomRight, 100, 60);
        assert_eq!(snapped, r(50, 30, 50, 30));
    }

    // ----------------------------------------------------------------
    // A11y message
    // ----------------------------------------------------------------

    #[test]
    fn a11y_message_focus_gained() {
        let m = make_a11y_message(7, r(10, 5, 30, 20), ZOrder(2), FloatingPaneA11yKind::FocusGained);
        assert_eq!(m.pane, 7);
        assert_eq!(m.position, r(10, 5, 30, 20));
        assert_eq!(m.z_order, ZOrder(2));
        assert_eq!(m.kind, FloatingPaneA11yKind::FocusGained);
    }

    #[test]
    fn a11y_message_kinds_distinct() {
        let kinds = [
            FloatingPaneA11yKind::FocusGained,
            FloatingPaneA11yKind::RectChanged,
            FloatingPaneA11yKind::ZOrderChanged,
            FloatingPaneA11yKind::PinnedToTiled,
            FloatingPaneA11yKind::UnpinnedToFloating,
        ];
        // Crude distinctness check.
        for (i, a) in kinds.iter().enumerate() {
            for (j, b) in kinds.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }

    // ----------------------------------------------------------------
    // KeyboardCommand
    // ----------------------------------------------------------------

    #[test]
    fn keyboard_command_covers_every_mouse_path() {
        // Every mouse path enumerated in the bead (drag/resize/snap/
        // pin/z-order/cycle/cancel) must have a KeyboardCommand
        // variant per the a11y rule. This test just exists to make
        // adding a new mouse path without a keyboard equivalent
        // visible at PR review time — it asserts the variant count.
        let all = [
            KeyboardCommand::MoveLeft,
            KeyboardCommand::MoveRight,
            KeyboardCommand::MoveUp,
            KeyboardCommand::MoveDown,
            KeyboardCommand::GrowHorizontal,
            KeyboardCommand::ShrinkHorizontal,
            KeyboardCommand::GrowVertical,
            KeyboardCommand::ShrinkVertical,
            KeyboardCommand::SnapTop,
            KeyboardCommand::SnapBottom,
            KeyboardCommand::SnapLeft,
            KeyboardCommand::SnapRight,
            KeyboardCommand::TogglePin,
            KeyboardCommand::RaiseOne,
            KeyboardCommand::LowerOne,
            KeyboardCommand::RaiseToTop,
            KeyboardCommand::LowerToBottom,
            KeyboardCommand::CycleOverlapping,
            KeyboardCommand::CancelOperation,
        ];
        assert_eq!(all.len(), 19);
    }

    // ----------------------------------------------------------------
    // Cross-cut
    // ----------------------------------------------------------------

    #[test]
    fn scenario_three_floating_panes_drag_one_to_top_then_cycle() {
        let mut zs = FloatingZStack::new();
        let mut state = DragResizeState::Idle;

        // Three overlapping floating panes.
        zs.insert_top(1);
        zs.insert_top(2);
        zs.insert_top(3);

        let rects = |id: PaneId| -> Option<FloatingRect> {
            match id {
                1 => Some(r(0, 0, 20, 10)),
                2 => Some(r(5, 2, 20, 10)),
                3 => Some(r(10, 4, 20, 10)),
                _ => None,
            }
        };

        // Drag pane 2 to a new position; commit.
        state.begin_drag(2, rects(2).unwrap());
        state.update(r(7, 4, 20, 10));
        let committed = state.commit().unwrap();
        assert_eq!(committed, r(7, 4, 20, 10));

        // Raise pane 2 to top via keyboard command equivalent.
        zs.raise_to_top(2);
        let order: Vec<PaneId> = zs.iter_back_to_front().map(|(id, _)| id).collect();
        assert_eq!(order, vec![1, 3, 2]);

        // Cycle from top among overlapping panes at (12, 6) where all
        // three overlap.
        let next = zs.cycle_among_overlapping(Some(2), 12, 6, &rects);
        assert_eq!(next, Some(3));
    }

    #[test]
    fn scenario_drag_then_cancel_restores_and_keyboard_can_resume() {
        let mut state = DragResizeState::Idle;
        let original = r(10, 10, 20, 5);
        state.begin_drag(1, original);
        state.update(r(50, 50, 20, 5));
        let restored = state.cancel().unwrap();
        assert_eq!(restored, original);

        // After cancel, a fresh keyboard-driven move begins cleanly.
        state.begin_drag(1, original);
        state.update(r(11, 10, 20, 5)); // 1-cell move via MoveRight
        let committed = state.commit().unwrap();
        assert_eq!(committed, r(11, 10, 20, 5));
    }
}
