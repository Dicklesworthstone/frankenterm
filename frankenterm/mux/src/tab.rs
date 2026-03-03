use crate::domain::DomainId;
use crate::layout::{redistribute_panes, LayoutCycle, PaneStack, SwapLayout};
use crate::pane::*;
use crate::renderable::StableCursorPosition;
use crate::{Mux, MuxNotification, WindowId};
use bintree::PathBranch;
use config::configuration;
use config::keyassignment::PaneDirection;
use frankenterm_term::{StableRowIndex, TerminalSize};
use parking_lot::Mutex;
use rangeset::intersects_range;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::sync::Arc;
use url::Url;

pub type Tree = bintree::Tree<Arc<dyn Pane>, SplitDirectionAndSize>;
pub type Cursor = bintree::Cursor<Arc<dyn Pane>, SplitDirectionAndSize>;

static TAB_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub type TabId = usize;

#[derive(Default)]
struct Recency {
    count: usize,
    by_idx: HashMap<usize, usize>,
}

impl Recency {
    fn tag(&mut self, idx: usize) {
        self.by_idx.insert(idx, self.count);
        self.count += 1;
    }

    fn score(&self, idx: usize) -> usize {
        self.by_idx.get(&idx).copied().unwrap_or(0)
    }
}

struct TabInner {
    id: TabId,
    pane: Option<Tree>,
    floating_panes: Vec<FloatingPane>,
    floating_focus: Option<PaneId>,
    size: TerminalSize,
    size_before_zoom: TerminalSize,
    active: usize,
    zoomed: Option<Arc<dyn Pane>>,
    title: String,
    recency: Recency,
    /// Set of pane IDs that have been collapsed because the terminal
    /// shrank below the aggregate minimum constraints.  Collapsed panes
    /// retain their tree position but are allocated zero space.
    collapsed_panes: HashSet<PaneId>,
    /// Optional layout cycle for swap-layout support.
    /// When set, the user can cycle through pre-defined arrangements.
    layout_cycle: Option<LayoutCycle>,
    /// Pane stacks: slot_index → PaneStack.  When a layout has fewer
    /// slots than panes, overflow panes are stacked in the last slot.
    /// Only the active pane in each stack is visible in the tree.
    pane_stacks: HashMap<usize, PaneStack>,
}

/// A Tab is a container of Panes
pub struct Tab {
    inner: Mutex<TabInner>,
    tab_id: TabId,
}

#[derive(Clone)]
pub struct PositionedPane {
    /// The topological pane index that can be used to reference this pane
    pub index: usize,
    /// true if this is the active pane at the time the position was computed
    pub is_active: bool,
    /// true if this pane is zoomed
    pub is_zoomed: bool,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub top: usize,
    /// The width of this pane in cells
    pub width: usize,
    pub pixel_width: usize,
    /// The height of this pane in cells
    pub height: usize,
    pub pixel_height: usize,
    /// The pane instance
    pub pane: Arc<dyn Pane>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct FloatingPaneRect {
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
}

struct FloatingPane {
    pane: Arc<dyn Pane>,
    rect: FloatingPaneRect,
    z_order: u32,
    visible: bool,
    pinned: bool,
    opacity: f32,
}

#[derive(Clone)]
pub struct PositionedFloatingPane {
    pub pane_id: PaneId,
    pub is_focused: bool,
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
    pub z_order: u32,
    pub visible: bool,
    pub pinned: bool,
    pub opacity: f32,
    pub pane: Arc<dyn Pane>,
}

impl std::fmt::Debug for PositionedFloatingPane {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        fmt.debug_struct("PositionedFloatingPane")
            .field("pane_id", &self.pane_id)
            .field("is_focused", &self.is_focused)
            .field("left", &self.left)
            .field("top", &self.top)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("z_order", &self.z_order)
            .field("visible", &self.visible)
            .field("pinned", &self.pinned)
            .field("opacity", &self.opacity)
            .finish()
    }
}

impl std::fmt::Debug for PositionedPane {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        fmt.debug_struct("PositionedPane")
            .field("index", &self.index)
            .field("is_active", &self.is_active)
            .field("left", &self.left)
            .field("top", &self.top)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pane_id", &self.pane.pane_id())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// The size is of the (first, second) child of the split
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct SplitDirectionAndSize {
    pub direction: SplitDirection,
    pub first: TerminalSize,
    pub second: TerminalSize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitSize {
    Cells(usize),
    Percent(u8),
}

impl Default for SplitSize {
    fn default() -> Self {
        Self::Percent(50)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct SplitRequest {
    pub direction: SplitDirection,
    /// Whether the newly created item will be in the second part
    /// of the split (right/bottom)
    pub target_is_second: bool,
    /// Split across the top of the tab rather than the active pane
    pub top_level: bool,
    /// The size of the new item
    pub size: SplitSize,
}

impl Default for SplitRequest {
    fn default() -> Self {
        Self {
            direction: SplitDirection::Horizontal,
            target_is_second: true,
            top_level: false,
            size: SplitSize::default(),
        }
    }
}

impl SplitDirectionAndSize {
    fn top_of_second(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => 0,
            SplitDirection::Vertical => self.first.rows as usize + 1,
        }
    }

    fn left_of_second(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => self.first.cols as usize + 1,
            SplitDirection::Vertical => 0,
        }
    }

    pub fn width(&self) -> usize {
        if self.direction == SplitDirection::Horizontal {
            self.first.cols + self.second.cols + 1
        } else {
            self.first.cols
        }
    }

    pub fn height(&self) -> usize {
        if self.direction == SplitDirection::Vertical {
            self.first.rows + self.second.rows + 1
        } else {
            self.first.rows
        }
    }

    pub fn size(&self) -> TerminalSize {
        let cell_width = self.first.pixel_width / self.first.cols;
        let cell_height = self.first.pixel_height / self.first.rows;

        let rows = self.height();
        let cols = self.width();

        TerminalSize {
            rows,
            cols,
            pixel_height: cell_height * rows,
            pixel_width: cell_width * cols,
            dpi: self.first.dpi,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PositionedSplit {
    /// The topological node index that can be used to reference this split
    pub index: usize,
    pub direction: SplitDirection,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in cells.
    pub left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in cells.
    pub top: usize,
    /// For Horizontal splits, how tall the split should be, for Vertical
    /// splits how wide it should be
    pub size: usize,
}

fn is_pane(pane: &Arc<dyn Pane>, other: &Option<&Arc<dyn Pane>>) -> bool {
    if let Some(other) = other {
        other.pane_id() == pane.pane_id()
    } else {
        false
    }
}

fn pane_tree(
    tree: &Tree,
    tab_id: TabId,
    window_id: WindowId,
    active: Option<&Arc<dyn Pane>>,
    zoomed: Option<&Arc<dyn Pane>>,
    workspace: &str,
    left_col: usize,
    top_row: usize,
) -> PaneNode {
    match tree {
        Tree::Empty => PaneNode::Empty,
        Tree::Node { left, right, data } => {
            let data = data.unwrap();
            PaneNode::Split {
                left: Box::new(pane_tree(
                    &*left, tab_id, window_id, active, zoomed, workspace, left_col, top_row,
                )),
                right: Box::new(pane_tree(
                    &*right,
                    tab_id,
                    window_id,
                    active,
                    zoomed,
                    workspace,
                    if data.direction == SplitDirection::Vertical {
                        left_col
                    } else {
                        left_col + data.left_of_second()
                    },
                    if data.direction == SplitDirection::Horizontal {
                        top_row
                    } else {
                        top_row + data.top_of_second()
                    },
                )),
                node: data,
            }
        }
        Tree::Leaf(pane) => {
            let dims = pane.get_dimensions();
            let working_dir = pane.get_current_working_dir(CachePolicy::AllowStale);
            let cursor_pos = pane.get_cursor_position();

            PaneNode::Leaf(PaneEntry {
                window_id,
                tab_id,
                pane_id: pane.pane_id(),
                title: pane.get_title(),
                is_active_pane: is_pane(pane, &active),
                is_zoomed_pane: is_pane(pane, &zoomed),
                size: TerminalSize {
                    cols: dims.cols,
                    rows: dims.viewport_rows,
                    pixel_height: dims.pixel_height,
                    pixel_width: dims.pixel_width,
                    dpi: dims.dpi,
                },
                working_dir: working_dir.map(Into::into),
                workspace: workspace.to_string(),
                cursor_pos,
                physical_top: dims.physical_top,
                left_col,
                top_row,
                tty_name: pane.tty_name(),
            })
        }
    }
}

fn build_from_pane_tree<F>(
    tree: bintree::Tree<PaneEntry, SplitDirectionAndSize>,
    active: &mut Option<Arc<dyn Pane>>,
    zoomed: &mut Option<Arc<dyn Pane>>,
    make_pane: &mut F,
) -> Tree
where
    F: FnMut(PaneEntry) -> Arc<dyn Pane>,
{
    match tree {
        bintree::Tree::Empty => Tree::Empty,
        bintree::Tree::Node { left, right, data } => Tree::Node {
            left: Box::new(build_from_pane_tree(*left, active, zoomed, make_pane)),
            right: Box::new(build_from_pane_tree(*right, active, zoomed, make_pane)),
            data,
        },
        bintree::Tree::Leaf(entry) => {
            let is_zoomed_pane = entry.is_zoomed_pane;
            let is_active_pane = entry.is_active_pane;
            let pane = make_pane(entry);
            if is_zoomed_pane {
                zoomed.replace(Arc::clone(&pane));
            }
            if is_active_pane {
                active.replace(Arc::clone(&pane));
            }
            Tree::Leaf(pane)
        }
    }
}

/// Computes the minimum (x, y) size based on the panes in this portion
/// of the tree.
fn compute_min_size(tree: &Tree) -> (usize, usize) {
    match tree {
        Tree::Node { data: None, .. } | Tree::Empty => (1, 1),
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let (left_x, left_y) = compute_min_size(&*left);
            let (right_x, right_y) = compute_min_size(&*right);
            match data.direction {
                SplitDirection::Vertical => (left_x.max(right_x), left_y + right_y + 1),
                SplitDirection::Horizontal => (left_x + right_x + 1, left_y.max(right_y)),
            }
        }
        Tree::Leaf(pane) => {
            let constraints = pane.pane_constraints();
            let min_width = constraints.min_width.max(1);
            let min_height = constraints.min_height.max(1);
            if constraints.fixed {
                let dims = pane.get_dimensions();
                (min_width.max(dims.cols), min_height.max(dims.viewport_rows))
            } else {
                (min_width, min_height)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Axis {
    Width,
    Height,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AxisConstraints {
    min: usize,
    max: Option<usize>,
    preferred: Option<usize>,
}

impl AxisConstraints {
    fn normalized(self) -> Self {
        let min = self.min.max(1);
        let max = self.max.map(|value| value.max(min));
        let preferred = self.preferred.map(|value| {
            let clamped = value.max(min);
            max.map_or(clamped, |max_value| clamped.min(max_value))
        });

        Self {
            min,
            max,
            preferred,
        }
    }
}

fn axis_constraints_from_pane_constraints(
    constraints: PaneConstraints,
    axis: Axis,
    fixed_size: Option<usize>,
) -> AxisConstraints {
    let (mut min, mut max, mut preferred) = match axis {
        Axis::Width => (
            constraints.min_width.max(1),
            constraints.max_width,
            constraints.preferred_width,
        ),
        Axis::Height => (
            constraints.min_height.max(1),
            constraints.max_height,
            constraints.preferred_height,
        ),
    };

    if constraints.fixed {
        if let Some(size) = fixed_size {
            min = min.max(size);
            max = Some(size);
            preferred = Some(size);
        }
    }

    AxisConstraints {
        min,
        max,
        preferred,
    }
    .normalized()
}

fn pane_axis_constraints(pane: &Arc<dyn Pane>, axis: Axis) -> AxisConstraints {
    let constraints = pane.pane_constraints();
    let dims = pane.get_dimensions();
    let fixed_size = match axis {
        Axis::Width => Some(dims.cols),
        Axis::Height => Some(dims.viewport_rows),
    };
    axis_constraints_from_pane_constraints(constraints, axis, fixed_size)
}

fn shared_axis_constraints(left: AxisConstraints, right: AxisConstraints) -> AxisConstraints {
    let min = left.min.max(right.min);
    let max = match (left.max, right.max) {
        (Some(left_max), Some(right_max)) => Some(left_max.min(right_max)),
        (Some(left_max), None) => Some(left_max),
        (None, Some(right_max)) => Some(right_max),
        (None, None) => None,
    };
    let preferred = match (left.preferred, right.preferred) {
        (Some(left_pref), Some(right_pref)) => Some(left_pref.max(right_pref)),
        (Some(left_pref), None) => Some(left_pref),
        (None, Some(right_pref)) => Some(right_pref),
        (None, None) => None,
    };

    AxisConstraints {
        min,
        max,
        preferred,
    }
    .normalized()
}

fn additive_axis_constraints(left: AxisConstraints, right: AxisConstraints) -> AxisConstraints {
    let min = left.min.saturating_add(right.min).saturating_add(1);
    let max = match (left.max, right.max) {
        (Some(left_max), Some(right_max)) => {
            Some(left_max.saturating_add(right_max).saturating_add(1))
        }
        _ => None,
    };
    let preferred = match (left.preferred, right.preferred) {
        (Some(left_pref), Some(right_pref)) => {
            Some(left_pref.saturating_add(right_pref).saturating_add(1))
        }
        _ => None,
    };

    AxisConstraints {
        min,
        max,
        preferred,
    }
    .normalized()
}

fn compute_axis_constraints(tree: &Tree, axis: Axis) -> AxisConstraints {
    match tree {
        Tree::Empty | Tree::Node { data: None, .. } => AxisConstraints {
            min: 1,
            max: None,
            preferred: None,
        },
        Tree::Leaf(pane) => pane_axis_constraints(pane, axis),
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let left_constraints = compute_axis_constraints(&*left, axis);
            let right_constraints = compute_axis_constraints(&*right, axis);
            match (data.direction, axis) {
                (SplitDirection::Horizontal, Axis::Width)
                | (SplitDirection::Vertical, Axis::Height) => {
                    additive_axis_constraints(left_constraints, right_constraints)
                }
                _ => shared_axis_constraints(left_constraints, right_constraints),
            }
        }
    }
}

fn split_allocation(
    total: usize,
    first: AxisConstraints,
    second: AxisConstraints,
    preferred_first: Option<usize>,
) -> Option<(usize, usize)> {
    let available = total.checked_sub(1)?;
    if first.min.saturating_add(second.min) > available {
        return None;
    }

    let first_min = second.max.map_or(first.min, |second_max| {
        first.min.max(available.saturating_sub(second_max))
    });
    let first_max = first
        .max
        .unwrap_or(available)
        .min(available.saturating_sub(second.min));
    if first_min > first_max {
        return None;
    }

    let preferred =
        preferred_first
            .or(first.preferred)
            .or_else(|| {
                second
                    .preferred
                    .map(|value| available.saturating_sub(value))
            })
            .unwrap_or(first.min.saturating_add(
                available.saturating_sub(first.min.saturating_add(second.min)) / 2,
            ));
    let first_size = preferred.clamp(first_min, first_max);
    let second_size = available.saturating_sub(first_size);
    Some((first_size, second_size))
}

fn split_dimension_for_request(
    dim: usize,
    request: SplitRequest,
    first: AxisConstraints,
    second: AxisConstraints,
) -> Option<(usize, usize)> {
    let requested = match request.size {
        SplitSize::Cells(n) => n,
        SplitSize::Percent(n) => (dim * (n as usize)) / 100,
    }
    .max(1);

    if request.target_is_second {
        let preferred_first = dim.saturating_sub(1).saturating_sub(requested);
        split_allocation(dim, first, second, Some(preferred_first))
    } else {
        split_allocation(dim, first, second, Some(requested))
    }
}

fn pane_size_satisfies_constraints(
    size: &TerminalSize,
    width: AxisConstraints,
    height: AxisConstraints,
) -> bool {
    if size.cols < width.min || size.rows < height.min {
        return false;
    }
    if let Some(max_width) = width.max {
        if size.cols > max_width {
            return false;
        }
    }
    if let Some(max_height) = height.max {
        if size.rows > max_height {
            return false;
        }
    }
    true
}

/// Collect all leaf panes from a tree, returning (pane_id, collapse_priority).
fn collect_leaf_panes(tree: &Tree) -> Vec<(PaneId, CollapsePriority)> {
    let mut result = Vec::new();
    collect_leaf_panes_recursive(tree, &mut result);
    result
}

fn collect_leaf_panes_recursive(tree: &Tree, out: &mut Vec<(PaneId, CollapsePriority)>) {
    match tree {
        Tree::Empty | Tree::Node { data: None, .. } => {}
        Tree::Leaf(pane) => {
            out.push((pane.pane_id(), pane.collapse_priority()));
        }
        Tree::Node {
            left,
            right,
            data: Some(_),
            ..
        } => {
            collect_leaf_panes_recursive(left, out);
            collect_leaf_panes_recursive(right, out);
        }
    }
}

/// Return a numeric collapse order: lower number = collapse first.
/// `Low` collapses before `Normal` before `High`; `Never` is not collapsible.
fn collapse_order(priority: CollapsePriority) -> Option<u8> {
    match priority {
        CollapsePriority::Low => Some(0),
        CollapsePriority::Normal => Some(1),
        CollapsePriority::High => Some(2),
        CollapsePriority::Never => None,
    }
}

/// Compute the minimum size of a tree when a given set of panes are
/// treated as collapsed (contributing zero space).  This is used to
/// determine whether collapsing certain panes makes the tree fit.
fn compute_min_size_with_collapsed(tree: &Tree, collapsed: &HashSet<PaneId>) -> (usize, usize) {
    match tree {
        Tree::Empty | Tree::Node { data: None, .. } => (0, 0),
        Tree::Leaf(pane) => {
            if collapsed.contains(&pane.pane_id()) {
                (0, 0)
            } else {
                let c = pane.pane_constraints();
                (c.min_width.max(1), c.min_height.max(1))
            }
        }
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let (lw, lh) = compute_min_size_with_collapsed(left, collapsed);
            let (rw, rh) = compute_min_size_with_collapsed(right, collapsed);
            match data.direction {
                SplitDirection::Horizontal => {
                    let w = if lw == 0 && rw == 0 {
                        0
                    } else if lw == 0 {
                        rw
                    } else if rw == 0 {
                        lw
                    } else {
                        lw + 1 + rw
                    };
                    (w, lh.max(rh))
                }
                SplitDirection::Vertical => {
                    let h = if lh == 0 && rh == 0 {
                        0
                    } else if lh == 0 {
                        rh
                    } else if rh == 0 {
                        lh
                    } else {
                        lh + 1 + rh
                    };
                    (lw.max(rw), h)
                }
            }
        }
    }
}

/// Compute the resize budget for a given split: how far in each direction
/// the split divider can be moved while respecting all constraints.
/// Returns `(max_shrink_first, max_grow_first)` — negative deltas shrink
/// the first child, positive deltas grow it.
fn compute_split_resize_budget(
    left: &Tree,
    right: &Tree,
    direction: SplitDirection,
    first_size: &TerminalSize,
    second_size: &TerminalSize,
) -> (isize, isize) {
    let (left_wc, left_hc) = (
        compute_axis_constraints(left, Axis::Width),
        compute_axis_constraints(left, Axis::Height),
    );
    let (right_wc, right_hc) = (
        compute_axis_constraints(right, Axis::Width),
        compute_axis_constraints(right, Axis::Height),
    );

    match direction {
        SplitDirection::Horizontal => {
            let left_can_shrink = first_size.cols.saturating_sub(left_wc.min);
            let right_can_shrink = second_size.cols.saturating_sub(right_wc.min);
            let left_can_grow = left_wc.max.map_or(right_can_shrink, |max| {
                max.saturating_sub(first_size.cols).min(right_can_shrink)
            });
            (-(left_can_shrink as isize), left_can_grow as isize)
        }
        SplitDirection::Vertical => {
            let left_can_shrink = first_size.rows.saturating_sub(left_hc.min);
            let right_can_shrink = second_size.rows.saturating_sub(right_hc.min);
            let left_can_grow = left_hc.max.map_or(right_can_shrink, |max| {
                max.saturating_sub(first_size.rows).min(right_can_shrink)
            });
            (-(left_can_shrink as isize), left_can_grow as isize)
        }
    }
}

/// Replace a pane in the tree by matching on PaneId.
fn replace_pane_recursive(tree: &mut Tree, old_id: PaneId, new_pane: Arc<dyn Pane>) {
    match tree {
        Tree::Empty | Tree::Node { data: None, .. } => {}
        Tree::Leaf(pane) => {
            if pane.pane_id() == old_id {
                *pane = new_pane;
            }
        }
        Tree::Node { left, right, .. } => {
            replace_pane_recursive(left, old_id, new_pane.clone());
            replace_pane_recursive(right, old_id, new_pane);
        }
    }
}

/// Returns `true` if every leaf pane in `tree` belongs to `collapsed`.
fn is_subtree_fully_collapsed(tree: &Tree, collapsed: &HashSet<PaneId>) -> bool {
    match tree {
        Tree::Empty | Tree::Node { data: None, .. } => true,
        Tree::Leaf(pane) => collapsed.contains(&pane.pane_id()),
        Tree::Node {
            left,
            right,
            data: Some(_),
        } => {
            is_subtree_fully_collapsed(left, collapsed)
                && is_subtree_fully_collapsed(right, collapsed)
        }
    }
}

/// Post-pass that redistributes space away from fully-collapsed subtrees.
/// At each split node, if one child is fully collapsed its allocated space
/// (plus the separator cell) is given to the sibling.  Collapsed leaf panes
/// receive a 1×1 allocation so that `pane.resize()` does not reject a 0-size.
fn redistribute_for_collapsed(
    tree: &mut Tree,
    collapsed: &HashSet<PaneId>,
    cell_dims: &TerminalSize,
) {
    if collapsed.is_empty() {
        return;
    }
    match tree {
        Tree::Empty | Tree::Leaf(_) | Tree::Node { data: None, .. } => {}
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let left_collapsed = is_subtree_fully_collapsed(left, collapsed);
            let right_collapsed = is_subtree_fully_collapsed(right, collapsed);

            if left_collapsed && !right_collapsed {
                match data.direction {
                    SplitDirection::Horizontal => {
                        // Left is collapsed: give its cols + 1 separator to right
                        let freed = data.first.cols.saturating_add(1);
                        data.second.cols = data.second.cols.saturating_add(freed);
                        data.second.pixel_width =
                            data.second.cols.saturating_mul(cell_dims.pixel_width);
                        data.first.cols = 1;
                        data.first.pixel_width = cell_dims.pixel_width;
                    }
                    SplitDirection::Vertical => {
                        let freed = data.first.rows.saturating_add(1);
                        data.second.rows = data.second.rows.saturating_add(freed);
                        data.second.pixel_height =
                            data.second.rows.saturating_mul(cell_dims.pixel_height);
                        data.first.rows = 1;
                        data.first.pixel_height = cell_dims.pixel_height;
                    }
                }
            } else if right_collapsed && !left_collapsed {
                match data.direction {
                    SplitDirection::Horizontal => {
                        let freed = data.second.cols.saturating_add(1);
                        data.first.cols = data.first.cols.saturating_add(freed);
                        data.first.pixel_width =
                            data.first.cols.saturating_mul(cell_dims.pixel_width);
                        data.second.cols = 1;
                        data.second.pixel_width = cell_dims.pixel_width;
                    }
                    SplitDirection::Vertical => {
                        let freed = data.second.rows.saturating_add(1);
                        data.first.rows = data.first.rows.saturating_add(freed);
                        data.first.pixel_height =
                            data.first.rows.saturating_mul(cell_dims.pixel_height);
                        data.second.rows = 1;
                        data.second.pixel_height = cell_dims.pixel_height;
                    }
                }
            }
            // Both collapsed or neither: leave sizes as-is.

            // Recurse into non-fully-collapsed children.
            if !left_collapsed {
                redistribute_for_collapsed(left, collapsed, cell_dims);
            }
            if !right_collapsed {
                redistribute_for_collapsed(right, collapsed, cell_dims);
            }
        }
    }
}

/// Recursively walk the tree in pre-order to find the split at `target_index`
/// and compute its resize budget.
fn find_split_budget(
    tree: &Tree,
    target_index: usize,
    counter: &mut usize,
) -> Option<(isize, isize)> {
    match tree {
        Tree::Empty | Tree::Leaf(_) | Tree::Node { data: None, .. } => None,
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            if *counter == target_index {
                return Some(compute_split_resize_budget(
                    left,
                    right,
                    data.direction,
                    &data.first,
                    &data.second,
                ));
            }
            *counter += 1;
            if let Some(result) = find_split_budget(left, target_index, counter) {
                return Some(result);
            }
            find_split_budget(right, target_index, counter)
        }
    }
}

fn adjust_x_size(tree: &mut Tree, mut x_adjust: isize, cell_dimensions: &TerminalSize) {
    let x_constraints = compute_axis_constraints(tree, Axis::Width);
    let min_x = x_constraints.min;
    let max_x = x_constraints.max;
    while x_adjust != 0 {
        match tree {
            Tree::Empty | Tree::Leaf(_) => return,
            Tree::Node { data: None, .. } => return,
            Tree::Node {
                left,
                right,
                data: Some(data),
            } => {
                data.first.dpi = cell_dimensions.dpi;
                data.second.dpi = cell_dimensions.dpi;
                match data.direction {
                    SplitDirection::Vertical => {
                        let mut new_cols = (data.first.cols as isize)
                            .saturating_add(x_adjust)
                            .max(min_x as isize);
                        if let Some(max_cols) = max_x {
                            new_cols = new_cols.min(max_cols as isize);
                        }
                        x_adjust = new_cols.saturating_sub(data.first.cols as isize);

                        if x_adjust != 0 {
                            adjust_x_size(&mut *left, x_adjust, cell_dimensions);
                            data.first.cols = new_cols.try_into().unwrap();
                            data.first.pixel_width =
                                data.first.cols.saturating_mul(cell_dimensions.pixel_width);

                            adjust_x_size(&mut *right, x_adjust, cell_dimensions);
                            data.second.cols = data.first.cols;
                            data.second.pixel_width = data.first.pixel_width;
                        }
                        return;
                    }
                    SplitDirection::Horizontal if x_adjust > 0 => {
                        let left_max_x = compute_axis_constraints(&*left, Axis::Width).max;
                        if left_max_x.map_or(true, |max_cols| data.first.cols < max_cols) {
                            adjust_x_size(&mut *left, 1, cell_dimensions);
                            data.first.cols += 1;
                            data.first.pixel_width =
                                data.first.cols.saturating_mul(cell_dimensions.pixel_width);
                            x_adjust -= 1;
                        }

                        if x_adjust > 0 {
                            let right_max_x = compute_axis_constraints(&*right, Axis::Width).max;
                            if right_max_x.map_or(true, |max_cols| data.second.cols < max_cols) {
                                adjust_x_size(&mut *right, 1, cell_dimensions);
                                data.second.cols += 1;
                                data.second.pixel_width =
                                    data.second.cols.saturating_mul(cell_dimensions.pixel_width);
                                x_adjust -= 1;
                            } else {
                                return;
                            }
                        }
                    }
                    SplitDirection::Horizontal => {
                        // x_adjust is negative
                        let (left_min_x, _) = compute_min_size(&*left);
                        let (right_min_x, _) = compute_min_size(&*right);
                        if data.first.cols > left_min_x {
                            adjust_x_size(&mut *left, -1, cell_dimensions);
                            data.first.cols -= 1;
                            data.first.pixel_width =
                                data.first.cols.saturating_mul(cell_dimensions.pixel_width);
                            x_adjust += 1;
                        }
                        if x_adjust < 0 && data.second.cols > right_min_x {
                            adjust_x_size(&mut *right, -1, cell_dimensions);
                            data.second.cols -= 1;
                            data.second.pixel_width =
                                data.second.cols.saturating_mul(cell_dimensions.pixel_width);
                            x_adjust += 1;
                        }
                    }
                }
            }
        }
    }
}

fn adjust_y_size(tree: &mut Tree, mut y_adjust: isize, cell_dimensions: &TerminalSize) {
    let y_constraints = compute_axis_constraints(tree, Axis::Height);
    let min_y = y_constraints.min;
    let max_y = y_constraints.max;
    while y_adjust != 0 {
        match tree {
            Tree::Empty | Tree::Leaf(_) => return,
            Tree::Node { data: None, .. } => return,
            Tree::Node {
                left,
                right,
                data: Some(data),
            } => {
                data.first.dpi = cell_dimensions.dpi;
                data.second.dpi = cell_dimensions.dpi;
                match data.direction {
                    SplitDirection::Horizontal => {
                        let mut new_rows = (data.first.rows as isize)
                            .saturating_add(y_adjust)
                            .max(min_y as isize);
                        if let Some(max_rows) = max_y {
                            new_rows = new_rows.min(max_rows as isize);
                        }
                        y_adjust = new_rows.saturating_sub(data.first.rows as isize);

                        if y_adjust != 0 {
                            adjust_y_size(&mut *left, y_adjust, cell_dimensions);
                            data.first.rows = new_rows.try_into().unwrap();
                            data.first.pixel_height =
                                data.first.rows.saturating_mul(cell_dimensions.pixel_height);

                            adjust_y_size(&mut *right, y_adjust, cell_dimensions);
                            data.second.rows = data.first.rows;
                            data.second.pixel_height = data.first.pixel_height;
                        }
                        return;
                    }
                    SplitDirection::Vertical if y_adjust > 0 => {
                        let left_max_y = compute_axis_constraints(&*left, Axis::Height).max;
                        if left_max_y.map_or(true, |max_rows| data.first.rows < max_rows) {
                            adjust_y_size(&mut *left, 1, cell_dimensions);
                            data.first.rows += 1;
                            data.first.pixel_height =
                                data.first.rows.saturating_mul(cell_dimensions.pixel_height);
                            y_adjust -= 1;
                        }
                        if y_adjust > 0 {
                            let right_max_y = compute_axis_constraints(&*right, Axis::Height).max;
                            if right_max_y.map_or(true, |max_rows| data.second.rows < max_rows) {
                                adjust_y_size(&mut *right, 1, cell_dimensions);
                                data.second.rows += 1;
                                data.second.pixel_height = data
                                    .second
                                    .rows
                                    .saturating_mul(cell_dimensions.pixel_height);
                                y_adjust -= 1;
                            } else {
                                return;
                            }
                        }
                    }
                    SplitDirection::Vertical => {
                        // y_adjust is negative
                        let (_, left_min_y) = compute_min_size(&*left);
                        let (_, right_min_y) = compute_min_size(&*right);
                        if data.first.rows > left_min_y {
                            adjust_y_size(&mut *left, -1, cell_dimensions);
                            data.first.rows -= 1;
                            data.first.pixel_height =
                                data.first.rows.saturating_mul(cell_dimensions.pixel_height);
                            y_adjust += 1;
                        }
                        if y_adjust < 0 && data.second.rows > right_min_y {
                            adjust_y_size(&mut *right, -1, cell_dimensions);
                            data.second.rows -= 1;
                            data.second.pixel_height = data
                                .second
                                .rows
                                .saturating_mul(cell_dimensions.pixel_height);
                            y_adjust += 1;
                        }
                    }
                }
            }
        }
    }
}

fn collect_pane_resize_work(
    tree: &Tree,
    size: &TerminalSize,
    work: &mut Vec<(Arc<dyn Pane>, TerminalSize)>,
) {
    match tree {
        Tree::Empty => {}
        Tree::Node { data: None, .. } => {}
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            collect_pane_resize_work(&*left, &data.first, work);
            collect_pane_resize_work(&*right, &data.second, work);
        }
        Tree::Leaf(pane) => {
            work.push((Arc::clone(pane), *size));
        }
    }
}

const RESIZE_FANOUT_PARALLEL_THRESHOLD: usize = 8;
const RESIZE_FANOUT_MIN_BATCH_SIZE: usize = 4;
const RESIZE_FANOUT_MAX_WORKERS: usize = 8;

fn compute_resize_fanout_workers(work_len: usize, available_parallelism: usize) -> usize {
    if work_len < RESIZE_FANOUT_PARALLEL_THRESHOLD {
        return 1;
    }

    let mut workers = work_len
        .min(available_parallelism.max(1))
        .min(RESIZE_FANOUT_MAX_WORKERS);
    while workers > 1 && work_len.div_ceil(workers) < RESIZE_FANOUT_MIN_BATCH_SIZE {
        workers -= 1;
    }
    workers.max(1)
}

fn resize_fanout_workers_for_host(work_len: usize) -> usize {
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    compute_resize_fanout_workers(work_len, available)
}

fn apply_sizes_from_splits(tree: &Tree, size: &TerminalSize) {
    let mut work = Vec::new();
    collect_pane_resize_work(tree, size, &mut work);

    if work.len() <= 1 {
        for (pane, pane_size) in work {
            pane.resize(pane_size).ok();
        }
        return;
    }

    let work_len = work.len();
    let worker_count = resize_fanout_workers_for_host(work_len);

    if worker_count <= 1 {
        for (pane, pane_size) in work {
            pane.resize(pane_size).ok();
        }
        return;
    }

    let bucket_len = work_len.div_ceil(worker_count);
    let mut buckets = Vec::with_capacity(worker_count);
    let mut iter = work.into_iter();
    while buckets.len() < worker_count {
        let mut bucket = Vec::with_capacity(bucket_len);
        for _ in 0..bucket_len {
            if let Some(item) = iter.next() {
                bucket.push(item);
            } else {
                break;
            }
        }
        if bucket.is_empty() {
            break;
        }
        buckets.push(bucket);
    }

    log::trace!(
        "apply_sizes_from_splits fanout panes={} workers={} bucket_len={}",
        work_len,
        buckets.len(),
        bucket_len
    );

    let _ = crossbeam::thread::scope(|scope| {
        for bucket in buckets {
            scope.spawn(move |_| {
                for (pane, pane_size) in bucket {
                    pane.resize(pane_size).ok();
                }
            });
        }
    });
}

fn cell_dimensions(size: &TerminalSize) -> TerminalSize {
    TerminalSize {
        rows: 1,
        cols: 1,
        pixel_width: size.pixel_width / size.cols,
        pixel_height: size.pixel_height / size.rows,
        dpi: size.dpi,
    }
}

const MIN_FLOATING_PANE_WIDTH: usize = 5;
const MIN_FLOATING_PANE_HEIGHT: usize = 3;

impl Tab {
    pub fn new(size: &TerminalSize) -> Self {
        let inner = TabInner::new(size);
        let tab_id = inner.id;
        Self {
            inner: Mutex::new(inner),
            tab_id,
        }
    }

    pub fn get_title(&self) -> String {
        self.inner.lock().title.clone()
    }

    pub fn set_title(&self, title: &str) {
        let mut inner = self.inner.lock();
        if inner.title != title {
            inner.title = title.to_string();
            Mux::try_get().map(|mux| {
                mux.notify(MuxNotification::TabTitleChanged {
                    tab_id: inner.id,
                    title: title.to_string(),
                })
            });
        }
    }

    /// Called by the multiplexer client when building a local tab to
    /// mirror a remote tab.  The supplied `root` is the information
    /// about our counterpart in the the remote server.
    /// This method builds a local tree based on the remote tree which
    /// then replaces the local tree structure.
    ///
    /// The `make_pane` function is provided by the caller, and its purpose
    /// is to lookup an existing Pane that corresponds to the provided
    /// PaneEntry, or to create a new Pane from that entry.
    /// make_pane is expected to add the pane to the mux if it creates
    /// a new pane, otherwise the pane won't poll/update in the GUI.
    pub fn sync_with_pane_tree<F>(&self, size: TerminalSize, root: PaneNode, make_pane: F)
    where
        F: FnMut(PaneEntry) -> Arc<dyn Pane>,
    {
        self.inner.lock().sync_with_pane_tree(size, root, make_pane)
    }

    pub fn codec_pane_tree(&self) -> PaneNode {
        self.inner.lock().codec_pane_tree()
    }

    /// Returns a count of how many panes are in this tab
    pub fn count_panes(&self) -> Option<usize> {
        self.inner.try_lock().map(|mut inner| inner.count_panes())
    }

    /// Sets the zoom state, returns the prior state
    pub fn set_zoomed(&self, zoomed: bool) -> bool {
        self.inner.lock().set_zoomed(zoomed)
    }

    pub fn toggle_zoom(&self) {
        self.inner.lock().toggle_zoom()
    }

    pub fn contains_pane(&self, pane: PaneId) -> bool {
        self.inner.lock().contains_pane(pane)
    }

    pub fn iter_panes(&self) -> Vec<PositionedPane> {
        self.inner.lock().iter_panes()
    }

    pub fn iter_panes_ignoring_zoom(&self) -> Vec<PositionedPane> {
        self.inner.lock().iter_panes_ignoring_zoom()
    }

    pub fn add_floating_pane(
        &self,
        pane: Arc<dyn Pane>,
        rect: FloatingPaneRect,
    ) -> PositionedFloatingPane {
        self.inner.lock().add_floating_pane(pane, rect)
    }

    pub fn set_floating_pane_rect(
        &self,
        pane_id: PaneId,
        rect: FloatingPaneRect,
    ) -> Option<PositionedFloatingPane> {
        self.inner.lock().set_floating_pane_rect(pane_id, rect)
    }

    pub fn set_floating_pane_visible(&self, pane_id: PaneId, visible: bool) -> bool {
        self.inner
            .lock()
            .set_floating_pane_visible(pane_id, visible)
    }

    pub fn set_floating_pane_focus(&self, pane_id: PaneId) -> bool {
        self.inner.lock().set_floating_pane_focus(pane_id)
    }

    pub fn bring_floating_pane_to_front(&self, pane_id: PaneId) -> bool {
        self.inner.lock().bring_floating_pane_to_front(pane_id)
    }

    pub fn remove_floating_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        self.inner.lock().remove_floating_pane(pane_id)
    }

    pub fn iter_floating_panes(&self) -> Vec<PositionedFloatingPane> {
        self.inner.lock().iter_floating_panes()
    }

    pub fn rotate_counter_clockwise(&self) {
        self.inner.lock().rotate_counter_clockwise()
    }

    pub fn rotate_clockwise(&self) {
        self.inner.lock().rotate_clockwise()
    }

    pub fn iter_splits(&self) -> Vec<PositionedSplit> {
        self.inner.lock().iter_splits()
    }

    pub fn tab_id(&self) -> TabId {
        self.tab_id
    }

    pub fn get_size(&self) -> TerminalSize {
        self.inner.lock().get_size()
    }

    /// Apply the new size of the tab to the panes contained within.
    /// The delta between the current and the new size is computed,
    /// and is distributed between the splits.  For small resizes
    /// this algorithm biases towards adjusting the left/top nodes
    /// first.  For large resizes this tends to proportionally adjust
    /// the relative sizes of the elements in a split.
    pub fn resize(&self, size: TerminalSize) {
        self.inner.lock().resize(size)
    }

    /// Called when running in the mux server after an individual pane
    /// has been resized.
    /// Because the split manipulation happened on the GUI we "lost"
    /// the information that would have allowed us to call resize_split_by()
    /// and instead need to back-infer the split size information.
    /// We rely on the client to have resized (or be in the process
    /// of resizing) affected panes consistently with its own Tab
    /// tree model.
    /// This method does a simple tree walk to the leaves to back-propagate
    /// the size of the panes up to their containing node split data.
    /// Without this step, disconnecting and reconnecting would cause
    /// the GUI to use stale size information for the window it spawns
    /// to attach this tab.
    pub fn rebuild_splits_sizes_from_contained_panes(&self) {
        self.inner
            .lock()
            .rebuild_splits_sizes_from_contained_panes()
    }

    /// Given split_index, the topological index of a split returned by
    /// iter_splits() as PositionedSplit::index, revised the split position
    /// by the provided delta; positive values move the split to the right/bottom,
    /// and negative values to the left/top.
    /// The adjusted size is propogated downwards to contained children and
    /// their panes are resized accordingly.
    pub fn resize_split_by(&self, split_index: usize, delta: isize) {
        self.inner.lock().resize_split_by(split_index, delta)
    }

    /// Returns `true` if the given pane is currently collapsed (hidden
    /// because the terminal shrank below minimum constraints).
    pub fn is_pane_collapsed(&self, pane_id: PaneId) -> bool {
        self.inner.lock().is_pane_collapsed(pane_id)
    }

    /// Returns the set of currently collapsed pane IDs.
    pub fn collapsed_pane_ids(&self) -> HashSet<PaneId> {
        self.inner.lock().collapsed_pane_ids().clone()
    }

    /// Set the layout cycle for swap-layout support.
    pub fn set_layout_cycle(&self, cycle: LayoutCycle) {
        self.inner.lock().set_layout_cycle(cycle)
    }

    /// Swap to the next layout in the cycle.
    /// Returns the name of the new layout, or None if no cycle is configured.
    pub fn swap_to_next_layout(&self) -> Option<String> {
        self.inner.lock().swap_to_next_layout()
    }

    /// Swap to the previous layout in the cycle.
    pub fn swap_to_prev_layout(&self) -> Option<String> {
        self.inner.lock().swap_to_prev_layout()
    }

    /// Swap to a specific layout by index in the cycle.
    pub fn swap_to_layout_index(&self, index: usize) -> Option<String> {
        self.inner.lock().swap_to_layout_index(index)
    }

    /// Cycle to the next pane in a stack at the given slot index.
    pub fn cycle_stack(&self, slot_index: usize) -> Option<PaneId> {
        self.inner.lock().cycle_stack(slot_index)
    }

    /// Cycle to the previous pane in a stack at the given slot index.
    pub fn cycle_stack_backward(&self, slot_index: usize) -> Option<PaneId> {
        self.inner.lock().cycle_stack_backward(slot_index)
    }

    /// Returns the current layout name, if a cycle is active.
    pub fn current_layout_name(&self) -> Option<String> {
        self.inner.lock().current_layout_name()
    }

    /// Returns the number of pane stacks.
    pub fn stack_count(&self) -> usize {
        self.inner.lock().stack_count()
    }

    /// Returns the first stack slot index that has more than one pane.
    pub fn first_nontrivial_stack_slot_index(&self) -> Option<usize> {
        self.inner.lock().first_nontrivial_stack_slot_index()
    }

    /// Returns all stacked pane IDs across all slots.
    pub fn all_stacked_pane_ids(&self) -> Vec<PaneId> {
        self.inner.lock().all_stacked_pane_ids()
    }

    /// Compute the resize budget for a split identified by its topological
    /// index.  Returns `None` if the index is out of range, otherwise
    /// `(max_shrink, max_grow)` where max_shrink is negative (how far
    /// the first child can shrink) and max_grow is positive (how far
    /// the first child can grow).
    pub fn compute_split_budget(&self, split_index: usize) -> Option<(isize, isize)> {
        self.inner.lock().compute_split_budget(split_index)
    }

    /// Adjusts the size of the active pane in the specified direction
    /// by the specified amount.
    pub fn adjust_pane_size(&self, direction: PaneDirection, amount: usize) {
        self.inner.lock().adjust_pane_size(direction, amount)
    }

    /// Activate an adjacent pane in the specified direction.
    /// In cases where there are multiple adjacent panes in the
    /// intended direction, we take the pane that has the largest
    /// edge intersection.
    pub fn activate_pane_direction(&self, direction: PaneDirection) {
        self.inner.lock().activate_pane_direction(direction)
    }

    /// Returns an adjacent pane in the specified direction.
    /// In cases where there are multiple adjacent panes in the
    /// intended direction, we take the pane that has the largest
    /// edge intersection.
    pub fn get_pane_direction(&self, direction: PaneDirection, ignore_zoom: bool) -> Option<usize> {
        self.inner.lock().get_pane_direction(direction, ignore_zoom)
    }

    pub fn prune_dead_panes(&self) -> bool {
        self.inner.lock().prune_dead_panes()
    }

    pub fn kill_pane(&self, pane_id: PaneId) -> bool {
        self.inner.lock().kill_pane(pane_id)
    }

    pub fn kill_panes_in_domain(&self, domain: DomainId) -> bool {
        self.inner.lock().kill_panes_in_domain(domain)
    }

    /// Remove pane from tab.
    /// The pane is still live in the mux; the intent is for the pane to
    /// be added to a different tab.
    pub fn remove_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        self.inner.lock().remove_pane(pane_id)
    }

    pub fn can_close_without_prompting(&self, reason: CloseReason) -> bool {
        self.inner.lock().can_close_without_prompting(reason)
    }

    pub fn is_dead(&self) -> bool {
        self.inner.lock().is_dead()
    }

    pub fn get_active_pane(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().get_active_pane()
    }

    #[allow(unused)]
    pub fn get_active_idx(&self) -> usize {
        self.inner.lock().get_active_idx()
    }

    pub fn set_active_pane(&self, pane: &Arc<dyn Pane>) {
        self.inner.lock().set_active_pane(pane)
    }

    pub fn set_active_idx(&self, pane_index: usize) {
        self.inner.lock().set_active_idx(pane_index)
    }

    /// Assigns the root pane.
    /// This is suitable when creating a new tab and then assigning
    /// the initial pane
    pub fn assign_pane(&self, pane: &Arc<dyn Pane>) {
        self.inner.lock().assign_pane(pane)
    }

    /// Swap the active pane with the specified pane_index
    pub fn swap_active_with_index(&self, pane_index: usize, keep_focus: bool) -> Option<()> {
        self.inner
            .lock()
            .swap_active_with_index(pane_index, keep_focus)
    }

    /// Computes the size of the pane that would result if the specified
    /// pane was split in a particular direction.
    /// The intent is to call this prior to spawning the new pane so that
    /// you can create it with the correct size.
    /// May return None if the specified pane_index is invalid.
    pub fn compute_split_size(
        &self,
        pane_index: usize,
        request: SplitRequest,
    ) -> Option<SplitDirectionAndSize> {
        self.inner.lock().compute_split_size(pane_index, request)
    }

    /// Split the pane that has pane_index in the given direction and assign
    /// the right/bottom pane of the newly created split to the provided Pane
    /// instance.  Returns the resultant index of the newly inserted pane.
    /// Both the split and the inserted pane will be resized.
    pub fn split_and_insert(
        &self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        self.inner
            .lock()
            .split_and_insert(pane_index, request, pane)
    }

    pub fn get_zoomed_pane(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().get_zoomed_pane()
    }
}

impl TabInner {
    fn new(size: &TerminalSize) -> Self {
        Self {
            id: TAB_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed),
            pane: Some(Tree::new()),
            floating_panes: vec![],
            floating_focus: None,
            size: *size,
            size_before_zoom: *size,
            active: 0,
            zoomed: None,
            title: String::new(),
            recency: Recency::default(),
            collapsed_panes: HashSet::new(),
            layout_cycle: Some(crate::layout::default_cycle()),
            pane_stacks: HashMap::new(),
        }
    }

    fn sync_with_pane_tree<F>(&mut self, size: TerminalSize, root: PaneNode, mut make_pane: F)
    where
        F: FnMut(PaneEntry) -> Arc<dyn Pane>,
    {
        let mut active = None;
        let mut zoomed = None;

        log::debug!("sync_with_pane_tree with size {:?}", size);

        let t = build_from_pane_tree(root.into_tree(), &mut active, &mut zoomed, &mut make_pane);
        let mut cursor = t.cursor();

        self.active = 0;
        if let Some(active) = active {
            // Resolve the active pane to its index
            let mut index = 0;
            loop {
                if let Some(pane) = cursor.leaf_mut() {
                    if active.pane_id() == pane.pane_id() {
                        // Found it
                        self.active = index;
                        self.recency.tag(index);
                        break;
                    }
                    index += 1;
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(c) => {
                        // Didn't find it
                        cursor = c;
                        break;
                    }
                }
            }
        }
        self.pane.replace(cursor.tree());
        self.floating_panes.clear();
        self.floating_focus = None;
        self.zoomed = zoomed;
        self.size = size;

        self.resize(size);

        log::debug!(
            "sync tab: {:#?} zoomed: {} {:#?}",
            size,
            self.zoomed.is_some(),
            self.iter_panes()
        );
        assert!(self.pane.is_some());
    }

    fn codec_pane_tree(&mut self) -> PaneNode {
        let mux = Mux::get();
        let tab_id = self.id;
        let window_id = match mux.window_containing_tab(tab_id) {
            Some(w) => w,
            None => {
                log::error!("no window contains tab {}", tab_id);
                return PaneNode::Empty;
            }
        };

        let workspace = match mux
            .get_window(window_id)
            .map(|w| w.get_workspace().to_string())
        {
            Some(ws) => ws,
            None => {
                log::error!("window id {} doesn't have a window!?", window_id);
                return PaneNode::Empty;
            }
        };

        let active = self.get_active_pane();
        let zoomed = self.zoomed.as_ref();
        if let Some(root) = self.pane.as_ref() {
            pane_tree(
                root,
                tab_id,
                window_id,
                active.as_ref(),
                zoomed,
                &workspace,
                0,
                0,
            )
        } else {
            PaneNode::Empty
        }
    }

    /// Returns a count of how many panes are in this tab
    fn count_panes(&mut self) -> usize {
        let floating_count = self.count_floating_panes();
        let mut count = 0;
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                count += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    return count + floating_count;
                }
            }
        }
    }

    /// Sets the zoom state, returns the prior state
    fn set_zoomed(&mut self, zoomed: bool) -> bool {
        if self.zoomed.is_some() == zoomed {
            // Current zoom state matches intended zoom state,
            // so we have nothing to do.
            return zoomed;
        }
        self.toggle_zoom();
        !zoomed
    }

    fn toggle_zoom(&mut self) {
        let size = self.size;
        if self.zoomed.take().is_some() {
            // We were zoomed, but now we are not.
            // Re-apply the size to the panes
            if let Some(pane) = self.get_active_pane() {
                pane.set_zoomed(false);
            }
            self.size = self.size_before_zoom;
            self.resize(size);
        } else {
            // We weren't zoomed, but now we want to zoom.
            // Locate the active pane
            self.size_before_zoom = size;
            if let Some(pane) = self.get_active_pane() {
                pane.set_zoomed(true);
                pane.resize(size).ok();
                self.zoomed.replace(pane);
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn contains_pane(&self, pane: PaneId) -> bool {
        fn contains(tree: &Tree, pane: PaneId) -> bool {
            match tree {
                Tree::Empty => false,
                Tree::Node { left, right, .. } => contains(left, pane) || contains(right, pane),
                Tree::Leaf(p) => p.pane_id() == pane,
            }
        }
        let in_tree = match &self.pane {
            Some(root) => contains(root, pane),
            None => false,
        };
        in_tree
            || self
                .floating_panes
                .iter()
                .any(|floating| floating.pane.pane_id() == pane)
    }

    fn clamp_floating_rect(&self, rect: FloatingPaneRect) -> FloatingPaneRect {
        let max_width = self.size.cols.max(1);
        let max_height = self.size.rows.max(1);
        let min_width = MIN_FLOATING_PANE_WIDTH.min(max_width);
        let min_height = MIN_FLOATING_PANE_HEIGHT.min(max_height);

        let width = rect.width.max(min_width).min(max_width);
        let height = rect.height.max(min_height).min(max_height);
        let left = rect.left.min(max_width.saturating_sub(width));
        let top = rect.top.min(max_height.saturating_sub(height));

        FloatingPaneRect {
            left,
            top,
            width,
            height,
        }
    }

    fn floating_pane_size(&self, rect: FloatingPaneRect) -> TerminalSize {
        let dims = self.cell_dimensions();
        TerminalSize {
            rows: rect.height,
            cols: rect.width,
            pixel_width: dims.pixel_width.saturating_mul(rect.width),
            pixel_height: dims.pixel_height.saturating_mul(rect.height),
            dpi: dims.dpi,
        }
    }

    fn floating_index_by_id(&self, pane_id: PaneId) -> Option<usize> {
        self.floating_panes
            .iter()
            .position(|floating| floating.pane.pane_id() == pane_id)
    }

    fn next_floating_z_order(&self) -> u32 {
        self.floating_panes
            .iter()
            .map(|floating| floating.z_order)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    fn positioned_floating_pane(&self, floating: &FloatingPane) -> PositionedFloatingPane {
        PositionedFloatingPane {
            pane_id: floating.pane.pane_id(),
            is_focused: self.floating_focus == Some(floating.pane.pane_id()),
            left: floating.rect.left,
            top: floating.rect.top,
            width: floating.rect.width,
            height: floating.rect.height,
            z_order: floating.z_order,
            visible: floating.visible,
            pinned: floating.pinned,
            opacity: floating.opacity,
            pane: Arc::clone(&floating.pane),
        }
    }

    fn add_floating_pane(
        &mut self,
        pane: Arc<dyn Pane>,
        rect: FloatingPaneRect,
    ) -> PositionedFloatingPane {
        let prior = self.get_active_pane();
        let rect = self.clamp_floating_rect(rect);
        let pane_id = pane.pane_id();
        pane.resize(self.floating_pane_size(rect)).ok();

        let floating = FloatingPane {
            pane: Arc::clone(&pane),
            rect,
            z_order: self.next_floating_z_order(),
            visible: true,
            pinned: false,
            opacity: 1.0,
        };
        self.floating_panes.push(floating);
        self.floating_focus = Some(pane_id);

        self.advise_focus_change(prior);
        self.positioned_floating_pane(self.floating_panes.last().expect("floating pane added"))
    }

    fn set_floating_pane_rect(
        &mut self,
        pane_id: PaneId,
        rect: FloatingPaneRect,
    ) -> Option<PositionedFloatingPane> {
        let idx = self.floating_index_by_id(pane_id)?;
        let rect = self.clamp_floating_rect(rect);
        let size = self.floating_pane_size(rect);
        {
            let floating = self.floating_panes.get_mut(idx)?;
            floating.rect = rect;
            floating.pane.resize(size).ok();
        }
        self.floating_panes
            .get(idx)
            .map(|floating| self.positioned_floating_pane(floating))
    }

    fn set_floating_pane_visible(&mut self, pane_id: PaneId, visible: bool) -> bool {
        let idx = match self.floating_index_by_id(pane_id) {
            Some(idx) => idx,
            None => return false,
        };
        let prior = self.get_active_pane();
        let floating = &mut self.floating_panes[idx];
        floating.visible = visible;
        if !visible && self.floating_focus == Some(pane_id) {
            self.floating_focus = None;
        }
        self.advise_focus_change(prior);
        true
    }

    fn bring_floating_pane_to_front(&mut self, pane_id: PaneId) -> bool {
        let idx = match self.floating_index_by_id(pane_id) {
            Some(idx) => idx,
            None => return false,
        };
        let next_z = self.next_floating_z_order();
        self.floating_panes[idx].z_order = next_z;
        true
    }

    fn set_floating_pane_focus(&mut self, pane_id: PaneId) -> bool {
        let idx = match self.floating_index_by_id(pane_id) {
            Some(idx) => idx,
            None => return false,
        };
        if !self.floating_panes[idx].visible {
            return false;
        }
        let prior = self.get_active_pane();
        let next_z = self.next_floating_z_order();
        self.floating_focus = Some(pane_id);
        self.floating_panes[idx].z_order = next_z;
        self.advise_focus_change(prior);
        true
    }

    fn remove_floating_pane(&mut self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        let idx = self.floating_index_by_id(pane_id)?;
        let prior = self.get_active_pane();
        let removed = self.floating_panes.remove(idx);
        if self.floating_focus == Some(pane_id) {
            self.floating_focus = None;
        }
        self.advise_focus_change(prior);
        Some(removed.pane)
    }

    fn iter_floating_panes(&self) -> Vec<PositionedFloatingPane> {
        let mut panes: Vec<PositionedFloatingPane> = self
            .floating_panes
            .iter()
            .map(|floating| self.positioned_floating_pane(floating))
            .collect();
        panes.sort_by(|left, right| {
            let left_key = (left.z_order, u8::from(left.is_focused));
            let right_key = (right.z_order, u8::from(right.is_focused));
            left_key.cmp(&right_key)
        });
        panes
    }

    fn count_floating_panes(&self) -> usize {
        self.floating_panes.len()
    }

    fn focused_floating_pane(&self) -> Option<Arc<dyn Pane>> {
        let pane_id = self.floating_focus?;
        self.floating_panes
            .iter()
            .find(|floating| floating.visible && floating.pane.pane_id() == pane_id)
            .map(|floating| Arc::clone(&floating.pane))
    }

    fn clear_floating_focus(&mut self) {
        self.floating_focus = None;
    }

    fn has_floating_pane(&self, pane_id: PaneId) -> bool {
        self.floating_index_by_id(pane_id).is_some()
    }

    fn remove_floating_panes_in_domain(&mut self, domain: DomainId) -> Vec<Arc<dyn Pane>> {
        let mut removed = vec![];
        self.floating_panes.retain(|floating| {
            if floating.pane.domain_id() == domain {
                removed.push(Arc::clone(&floating.pane));
                false
            } else {
                true
            }
        });
        if let Some(pane_id) = self.floating_focus {
            if !self
                .floating_panes
                .iter()
                .any(|floating| floating.pane.pane_id() == pane_id)
            {
                self.floating_focus = None;
            }
        }
        removed
    }

    fn resize_floating_panes_to_fit(&mut self) {
        for idx in 0..self.floating_panes.len() {
            let rect = self.clamp_floating_rect(self.floating_panes[idx].rect);
            self.floating_panes[idx].rect = rect;
            self.floating_panes[idx]
                .pane
                .resize(self.floating_pane_size(rect))
                .ok();
        }
        if let Some(pane_id) = self.floating_focus {
            let has_visible_focus = self
                .floating_panes
                .iter()
                .any(|floating| floating.visible && floating.pane.pane_id() == pane_id);
            if !has_visible_focus {
                self.floating_focus = None;
            }
        }
    }

    /// Determine which panes should be collapsed so that the tree fits
    /// within the given `(cols, rows)` budget.  Returns the set of pane
    /// IDs to collapse.  Panes with `CollapsePriority::Never` are exempt.
    fn select_panes_to_collapse(&self, cols: usize, rows: usize) -> HashSet<PaneId> {
        let tree = match self.pane.as_ref() {
            Some(t) => t,
            None => return HashSet::new(),
        };
        let mut collapsed = self.collapsed_panes.clone();

        // Collect candidates sorted by collapse order (Low first).
        let mut candidates: Vec<(PaneId, u8)> = collect_leaf_panes(tree)
            .into_iter()
            .filter_map(|(id, priority)| collapse_order(priority).map(|order| (id, order)))
            .filter(|(id, _)| !collapsed.contains(id))
            .collect();
        candidates.sort_by_key(|&(_, order)| order);

        for (pane_id, _) in candidates {
            let (min_w, min_h) = compute_min_size_with_collapsed(tree, &collapsed);
            if min_w <= cols && min_h <= rows {
                break;
            }
            collapsed.insert(pane_id);
        }
        collapsed
    }

    /// Attempt to restore previously collapsed panes if the terminal has
    /// grown large enough to accommodate them.  Returns the updated
    /// collapsed set.
    fn select_panes_to_uncollapse(&self, cols: usize, rows: usize) -> HashSet<PaneId> {
        let tree = match self.pane.as_ref() {
            Some(t) => t,
            None => return HashSet::new(),
        };
        if self.collapsed_panes.is_empty() {
            return HashSet::new();
        }

        // Build restoration order: High priority panes restore first.
        let pane_priorities: HashMap<PaneId, CollapsePriority> =
            collect_leaf_panes(tree).into_iter().collect();
        let mut restore_candidates: Vec<PaneId> = self.collapsed_panes.iter().copied().collect();
        restore_candidates.sort_by(|a, b| {
            let a_order = pane_priorities
                .get(a)
                .and_then(|p| collapse_order(*p))
                .unwrap_or(3);
            let b_order = pane_priorities
                .get(b)
                .and_then(|p| collapse_order(*p))
                .unwrap_or(3);
            b_order.cmp(&a_order) // High priority (order 2) restores before Low (order 0)
        });

        let mut collapsed = self.collapsed_panes.clone();
        for pane_id in restore_candidates {
            let mut trial = collapsed.clone();
            trial.remove(&pane_id);
            let (min_w, min_h) = compute_min_size_with_collapsed(tree, &trial);
            if min_w <= cols && min_h <= rows {
                collapsed = trial;
            }
        }
        collapsed
    }

    /// Returns `true` if the given pane is currently collapsed.
    fn is_pane_collapsed(&self, pane_id: PaneId) -> bool {
        self.collapsed_panes.contains(&pane_id)
    }

    /// Returns the set of currently collapsed pane IDs.
    fn collapsed_pane_ids(&self) -> &HashSet<PaneId> {
        &self.collapsed_panes
    }

    // --- Swap layout support ---

    /// Set the layout cycle for this tab.
    fn set_layout_cycle(&mut self, cycle: LayoutCycle) {
        self.layout_cycle = Some(cycle);
    }

    /// Swap to the next layout in the cycle.  Returns the name of the
    /// new layout, or None if no cycle is configured or the tab has no panes.
    fn swap_to_next_layout(&mut self) -> Option<String> {
        let cycle = self.layout_cycle.as_mut()?;
        let layout = cycle.advance().clone();
        self.apply_layout(&layout)
    }

    /// Swap to the previous layout in the cycle.
    fn swap_to_prev_layout(&mut self) -> Option<String> {
        let cycle = self.layout_cycle.as_mut()?;
        let layout = cycle.prev().clone();
        self.apply_layout(&layout)
    }

    /// Swap to a specific layout by index in the cycle.
    fn swap_to_layout_index(&mut self, index: usize) -> Option<String> {
        let cycle = self.layout_cycle.as_mut()?;
        if !cycle.select(index) {
            return None;
        }
        let layout = cycle.current().clone();
        self.apply_layout(&layout)
    }

    /// Apply a layout, redistributing panes from the current tree.
    fn apply_layout(&mut self, layout: &SwapLayout) -> Option<String> {
        // Collect all panes from the current tree AND from any existing stacks.
        let all_panes = self.collect_all_panes();
        if all_panes.is_empty() {
            return None;
        }

        let active_pane_id = self
            .get_active_pane()
            .map(|p| p.pane_id())
            .unwrap_or_else(|| all_panes[0].pane_id());

        let result = redistribute_panes(&layout.arrangement, all_panes, active_pane_id, self.size)?;

        self.pane = Some(result.tree);
        self.pane_stacks = result.stacks;
        self.active = result.active_index;
        self.collapsed_panes.clear();

        // Apply sizes to the new tree.
        let size = self.size;
        if let Some(tree) = self.pane.as_mut() {
            apply_sizes_from_splits(tree, &size);
        }

        // Notify about the focus change.
        if let Some(pane) = self.get_active_pane() {
            self.recency.tag(self.active);
            Mux::try_get().map(|mux| {
                mux.notify(MuxNotification::PaneFocused(pane.pane_id()));
            });
        }

        Some(layout.name.clone())
    }

    /// Collect all panes: from the tree leaves AND from stacked (hidden) panes.
    fn collect_all_panes(&mut self) -> Vec<Arc<dyn Pane>> {
        let mut panes: Vec<Arc<dyn Pane>> = Vec::new();

        // Collect from tree leaves.
        let positioned = self.iter_panes_ignoring_zoom();
        for pp in &positioned {
            panes.push(pp.pane.clone());
        }

        // Collect from stacks (non-visible panes that aren't already in the tree).
        let tree_ids: HashSet<PaneId> = panes.iter().map(|p| p.pane_id()).collect();
        for (_slot, stack) in self.pane_stacks.drain() {
            for p in stack.into_panes() {
                if !tree_ids.contains(&p.pane_id()) {
                    panes.push(p);
                }
            }
        }

        panes
    }

    /// Cycle to the next pane in a stack at the given slot index.
    /// Returns the newly visible pane ID, or None if no stack at that slot.
    fn cycle_stack(&mut self, slot_index: usize) -> Option<PaneId> {
        let stack = self.pane_stacks.get_mut(&slot_index)?;
        if stack.is_single() {
            return None; // nothing to cycle
        }

        let old_pane_id = stack.active_pane().pane_id();
        stack.cycle_next();
        let new_pane = stack.active_pane().clone();
        let new_pane_id = new_pane.pane_id();

        // Swap the visible pane in the tree leaf.
        self.replace_pane_in_tree(old_pane_id, new_pane);

        Some(new_pane_id)
    }

    /// Cycle to the previous pane in a stack at the given slot index.
    /// Returns the newly visible pane ID, or None if no stack at that slot.
    fn cycle_stack_backward(&mut self, slot_index: usize) -> Option<PaneId> {
        let stack = self.pane_stacks.get_mut(&slot_index)?;
        if stack.is_single() {
            return None; // nothing to cycle
        }

        let old_pane_id = stack.active_pane().pane_id();
        stack.cycle_prev();
        let new_pane = stack.active_pane().clone();
        let new_pane_id = new_pane.pane_id();

        // Swap the visible pane in the tree leaf.
        self.replace_pane_in_tree(old_pane_id, new_pane);

        Some(new_pane_id)
    }

    /// Replace a pane in the tree by its ID with a new pane.
    fn replace_pane_in_tree(&mut self, old_id: PaneId, new_pane: Arc<dyn Pane>) {
        if let Some(tree) = self.pane.as_mut() {
            replace_pane_recursive(tree, old_id, new_pane);
            let size = self.size;
            apply_sizes_from_splits(tree, &size);
        }
    }

    /// Returns the current layout name, if a cycle is active.
    fn current_layout_name(&self) -> Option<String> {
        self.layout_cycle.as_ref().map(|c| c.current().name.clone())
    }

    /// Returns the number of pane stacks.
    fn stack_count(&self) -> usize {
        self.pane_stacks.len()
    }

    /// Returns the first stack slot index that has more than one pane.
    fn first_nontrivial_stack_slot_index(&self) -> Option<usize> {
        self.pane_stacks
            .iter()
            .filter_map(|(slot_index, stack)| (!stack.is_single()).then_some(*slot_index))
            .min()
    }

    /// Returns all stacked pane IDs across all slots.
    fn all_stacked_pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        for stack in self.pane_stacks.values() {
            ids.extend(stack.pane_ids());
        }
        ids
    }

    /// Compute the resize budget for a split identified by its topological
    /// index.  Returns `None` if the index is out of range, otherwise
    /// `(max_shrink, max_grow)` for the first child.
    fn compute_split_budget(&self, split_index: usize) -> Option<(isize, isize)> {
        let tree = self.pane.as_ref()?;
        let mut counter = 0usize;
        find_split_budget(tree, split_index, &mut counter)
    }

    /// Walks the pane tree to produce the topologically ordered flattened
    /// list of PositionedPane instances along with their positioning information.
    fn iter_panes(&mut self) -> Vec<PositionedPane> {
        self.iter_panes_impl(true)
    }

    /// Like iter_panes, except that it will include all panes, regardless of
    /// whether one of them is currently zoomed.
    fn iter_panes_ignoring_zoom(&mut self) -> Vec<PositionedPane> {
        self.iter_panes_impl(false)
    }

    fn rotate_counter_clockwise(&mut self) {
        let panes = self.iter_panes_ignoring_zoom();
        if panes.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return;
        }
        let mut pane_to_swap = panes
            .first()
            .map(|p| p.pane.clone())
            .expect("at least one pane");

        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                std::mem::swap(&mut pane_to_swap, cursor.leaf_mut().unwrap());
            }

            match cursor.postorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    let size = self.size;
                    apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);
                    break;
                }
            }
        }
    }

    fn rotate_clockwise(&mut self) {
        let panes = self.iter_panes_ignoring_zoom();
        if panes.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return;
        }
        let mut pane_to_swap = panes
            .last()
            .map(|p| p.pane.clone())
            .expect("at least one pane");

        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                std::mem::swap(&mut pane_to_swap, cursor.leaf_mut().unwrap());
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    let size = self.size;
                    apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);
                    break;
                }
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn iter_panes_impl(&mut self, respect_zoom_state: bool) -> Vec<PositionedPane> {
        let mut panes = vec![];

        if respect_zoom_state {
            if let Some(zoomed) = self.zoomed.as_ref() {
                let size = self.size;
                panes.push(PositionedPane {
                    index: 0,
                    is_active: true,
                    is_zoomed: true,
                    left: 0,
                    top: 0,
                    width: size.cols.into(),
                    pixel_width: size.pixel_width.into(),
                    height: size.rows.into(),
                    pixel_height: size.pixel_height.into(),
                    pane: Arc::clone(zoomed),
                });
                return panes;
            }
        }

        let active_idx = self.active;
        let zoomed_id = self.zoomed.as_ref().map(|p| p.pane_id());
        let root_size = self.size;
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                let index = panes.len();
                let mut left = 0usize;
                let mut top = 0usize;
                let mut parent_size = None;
                for (branch, node) in cursor.path_to_root() {
                    if let Some(node) = node {
                        if parent_size.is_none() {
                            parent_size.replace(if branch == PathBranch::IsRight {
                                node.second
                            } else {
                                node.first
                            });
                        }
                        if branch == PathBranch::IsRight {
                            top += node.top_of_second();
                            left += node.left_of_second();
                        }
                    }
                }

                let pane = Arc::clone(cursor.leaf_mut().unwrap());
                let dims = parent_size.unwrap_or_else(|| root_size);

                panes.push(PositionedPane {
                    index,
                    is_active: index == active_idx,
                    is_zoomed: zoomed_id == Some(pane.pane_id()),
                    left,
                    top,
                    width: dims.cols as _,
                    height: dims.rows as _,
                    pixel_width: dims.pixel_width as _,
                    pixel_height: dims.pixel_height as _,
                    pane,
                });
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        panes
    }

    fn iter_splits(&mut self) -> Vec<PositionedSplit> {
        let mut dividers = vec![];
        if self.zoomed.is_some() {
            return dividers;
        }

        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        loop {
            if !cursor.is_leaf() {
                let mut left = 0usize;
                let mut top = 0usize;
                for (branch, p) in cursor.path_to_root() {
                    if let Some(p) = p {
                        if branch == PathBranch::IsRight {
                            left += p.left_of_second();
                            top += p.top_of_second();
                        }
                    }
                }
                if let Ok(Some(node)) = cursor.node_mut() {
                    match node.direction {
                        SplitDirection::Horizontal => left += node.first.cols as usize,
                        SplitDirection::Vertical => top += node.first.rows as usize,
                    }

                    dividers.push(PositionedSplit {
                        index,
                        direction: node.direction,
                        left,
                        top,
                        size: if node.direction == SplitDirection::Horizontal {
                            node.height() as usize
                        } else {
                            node.width() as usize
                        },
                    })
                }
                index += 1;
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        dividers
    }

    fn get_size(&self) -> TerminalSize {
        self.size
    }

    fn resize(&mut self, size: TerminalSize) {
        if size.rows == 0 || size.cols == 0 {
            // Ignore "impossible" resize requests
            return;
        }

        if let Some(zoomed) = &self.zoomed {
            self.size = size;
            zoomed.resize(size).ok();
        } else {
            let dims = cell_dimensions(&size);
            let width_constraints =
                compute_axis_constraints(self.pane.as_ref().unwrap(), Axis::Width);
            let height_constraints =
                compute_axis_constraints(self.pane.as_ref().unwrap(), Axis::Height);
            let current_size = self.size;

            // If the tree minimum exceeds available space, collapse panes
            // in priority order to make it fit.
            if width_constraints.min > size.cols || height_constraints.min > size.rows {
                self.collapsed_panes = self.select_panes_to_collapse(size.cols, size.rows);
            } else if !self.collapsed_panes.is_empty() {
                // Terminal grew — try to restore previously collapsed panes
                self.collapsed_panes = self.select_panes_to_uncollapse(size.cols, size.rows);
            }

            // Constrain the new size to the minimum possible dimensions
            let cols = width_constraints
                .max
                .map_or(size.cols.max(width_constraints.min), |max_cols| {
                    size.cols.max(width_constraints.min).min(max_cols)
                });
            let rows = height_constraints
                .max
                .map_or(size.rows.max(height_constraints.min), |max_rows| {
                    size.rows.max(height_constraints.min).min(max_rows)
                });
            let size = TerminalSize {
                rows,
                cols,
                pixel_width: cols * dims.pixel_width,
                pixel_height: rows * dims.pixel_height,
                dpi: dims.dpi,
            };

            // Update the split nodes with adjusted sizes
            adjust_x_size(
                self.pane.as_mut().unwrap(),
                cols as isize - current_size.cols as isize,
                &dims,
            );
            adjust_y_size(
                self.pane.as_mut().unwrap(),
                rows as isize - current_size.rows as isize,
                &dims,
            );

            // Redistribute space away from collapsed subtrees so that
            // their siblings receive the freed columns/rows.
            if !self.collapsed_panes.is_empty() {
                redistribute_for_collapsed(
                    self.pane.as_mut().unwrap(),
                    &self.collapsed_panes,
                    &dims,
                );
            }

            self.size = size;

            // And then resize the individual panes to match
            apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);
        }

        self.resize_floating_panes_to_fit();
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn apply_pane_size(&mut self, pane_size: TerminalSize, cursor: &mut Cursor) {
        let cell_width = pane_size
            .pixel_width
            .checked_div(pane_size.cols)
            .unwrap_or(1);
        let cell_height = pane_size
            .pixel_height
            .checked_div(pane_size.rows)
            .unwrap_or(1);
        let (
            left_width_constraints,
            left_height_constraints,
            right_width_constraints,
            right_height_constraints,
        ) = match cursor.subtree() {
            Tree::Node {
                left,
                right,
                data: Some(_),
            } => {
                let left_width_constraints = compute_axis_constraints(&**left, Axis::Width);
                let left_height_constraints = compute_axis_constraints(&**left, Axis::Height);
                let right_width_constraints = compute_axis_constraints(&**right, Axis::Width);
                let right_height_constraints = compute_axis_constraints(&**right, Axis::Height);
                (
                    left_width_constraints,
                    left_height_constraints,
                    right_width_constraints,
                    right_height_constraints,
                )
            }
            _ => return,
        };
        if let Ok(Some(node)) = cursor.node_mut() {
            // Adjust the size of the node; we preserve the size of the first
            // child and adjust the second, so if we are split down the middle
            // and the window is made wider, the right column will grow in
            // size, leaving the left at its current width.
            if node.direction == SplitDirection::Horizontal {
                node.first.rows = pane_size.rows;
                node.second.rows = pane_size.rows;

                if let Some((first_cols, second_cols)) = split_allocation(
                    pane_size.cols,
                    left_width_constraints,
                    right_width_constraints,
                    Some(node.first.cols),
                ) {
                    node.first.cols = first_cols;
                    node.second.cols = second_cols;
                } else {
                    return;
                }
            } else {
                node.first.cols = pane_size.cols;
                node.second.cols = pane_size.cols;

                if let Some((first_rows, second_rows)) = split_allocation(
                    pane_size.rows,
                    left_height_constraints,
                    right_height_constraints,
                    Some(node.first.rows),
                ) {
                    node.first.rows = first_rows;
                    node.second.rows = second_rows;
                } else {
                    return;
                }
            }
            node.first.pixel_width = node.first.cols * cell_width;
            node.first.pixel_height = node.first.rows * cell_height;

            node.second.pixel_width = node.second.cols * cell_width;
            node.second.pixel_height = node.second.rows * cell_height;
        }
    }

    fn rebuild_splits_sizes_from_contained_panes(&mut self) {
        if self.zoomed.is_some() {
            return;
        }

        fn compute_size(node: &mut Tree) -> Option<TerminalSize> {
            match node {
                Tree::Empty => None,
                Tree::Leaf(pane) => {
                    let dims = pane.get_dimensions();
                    let size = TerminalSize {
                        cols: dims.cols,
                        rows: dims.viewport_rows,
                        pixel_height: dims.pixel_height,
                        pixel_width: dims.pixel_width,
                        dpi: dims.dpi,
                    };
                    Some(size)
                }
                Tree::Node { left, right, data } => {
                    if let Some(data) = data {
                        if let Some(first) = compute_size(left) {
                            data.first = first;
                        }
                        if let Some(second) = compute_size(right) {
                            data.second = second;
                        }
                        Some(data.size())
                    } else {
                        None
                    }
                }
            }
        }

        if let Some(root) = self.pane.as_mut() {
            if let Some(size) = compute_size(root) {
                self.size = size;
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn resize_split_by(&mut self, split_index: usize, delta: isize) {
        if self.zoomed.is_some() {
            return;
        }

        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        // Position cursor on the specified split
        loop {
            if !cursor.is_leaf() {
                if index == split_index {
                    // Found it
                    break;
                }
                index += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    // Didn't find it
                    self.pane.replace(c.tree());
                    return;
                }
            }
        }

        // Now cursor is looking at the split
        self.adjust_node_at_cursor(&mut cursor, delta);
        self.cascade_size_from_cursor(cursor);
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn adjust_node_at_cursor(&mut self, cursor: &mut Cursor, delta: isize) {
        let cell_dimensions = self.cell_dimensions();
        let (
            left_width_constraints,
            left_height_constraints,
            right_width_constraints,
            right_height_constraints,
        ) = match cursor.subtree() {
            Tree::Node {
                left,
                right,
                data: Some(_),
            } => {
                let left_width_constraints = compute_axis_constraints(&**left, Axis::Width);
                let left_height_constraints = compute_axis_constraints(&**left, Axis::Height);
                let right_width_constraints = compute_axis_constraints(&**right, Axis::Width);
                let right_height_constraints = compute_axis_constraints(&**right, Axis::Height);
                (
                    left_width_constraints,
                    left_height_constraints,
                    right_width_constraints,
                    right_height_constraints,
                )
            }
            _ => return,
        };
        if let Ok(Some(node)) = cursor.node_mut() {
            match node.direction {
                SplitDirection::Horizontal => {
                    let width = node.width();
                    let preferred_cols = if delta >= 0 {
                        node.first.cols.saturating_add(delta as usize)
                    } else {
                        node.first.cols.saturating_sub((-delta) as usize)
                    };
                    if let Some((first_cols, second_cols)) = split_allocation(
                        width,
                        left_width_constraints,
                        right_width_constraints,
                        Some(preferred_cols),
                    ) {
                        node.first.cols = first_cols;
                        node.second.cols = second_cols;
                        node.first.pixel_width =
                            node.first.cols.saturating_mul(cell_dimensions.pixel_width);
                        node.second.pixel_width =
                            node.second.cols.saturating_mul(cell_dimensions.pixel_width);
                    }
                }
                SplitDirection::Vertical => {
                    let height = node.height();
                    let preferred_rows = if delta >= 0 {
                        node.first.rows.saturating_add(delta as usize)
                    } else {
                        node.first.rows.saturating_sub((-delta) as usize)
                    };
                    if let Some((first_rows, second_rows)) = split_allocation(
                        height,
                        left_height_constraints,
                        right_height_constraints,
                        Some(preferred_rows),
                    ) {
                        node.first.rows = first_rows;
                        node.second.rows = second_rows;
                        node.first.pixel_height =
                            node.first.rows.saturating_mul(cell_dimensions.pixel_height);
                        node.second.pixel_height = node
                            .second
                            .rows
                            .saturating_mul(cell_dimensions.pixel_height);
                    }
                }
            }
        }
    }

    fn cascade_size_from_cursor(&mut self, mut cursor: Cursor) {
        // Now we need to cascade this down to children
        match cursor.preorder_next() {
            Ok(c) => cursor = c,
            Err(c) => {
                self.pane.replace(c.tree());
                return;
            }
        }
        let root_size = self.size;

        loop {
            // Figure out the available size by looking at our immediate parent node.
            // If we are the root, look at the provided new size
            let pane_size = if let Some((branch, Some(parent))) = cursor.path_to_root().next() {
                if branch == PathBranch::IsRight {
                    parent.second
                } else {
                    parent.first
                }
            } else {
                root_size
            };

            if cursor.is_leaf() {
                // Apply our size to the tty
                cursor.leaf_mut().map(|pane| pane.resize(pane_size));
            } else {
                self.apply_pane_size(pane_size, &mut cursor);
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn adjust_pane_size(&mut self, direction: PaneDirection, amount: usize) {
        if self.zoomed.is_some() {
            return;
        }
        let active_index = self.active;
        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        // Position cursor on the active leaf
        loop {
            if cursor.is_leaf() {
                if index == active_index {
                    // Found it
                    break;
                }
                index += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    // Didn't find it
                    self.pane.replace(c.tree());
                    return;
                }
            }
        }

        // We are on the active leaf.
        // Now we go up until we find the parent node that is
        // aligned with the desired direction.
        let split_direction = match direction {
            PaneDirection::Left | PaneDirection::Right => SplitDirection::Horizontal,
            PaneDirection::Up | PaneDirection::Down => SplitDirection::Vertical,
            PaneDirection::Next | PaneDirection::Prev => unreachable!(),
        };
        let delta = match direction {
            PaneDirection::Down | PaneDirection::Right => amount as isize,
            PaneDirection::Up | PaneDirection::Left => -(amount as isize),
            PaneDirection::Next | PaneDirection::Prev => unreachable!(),
        };
        loop {
            match cursor.go_up() {
                Ok(mut c) => {
                    if let Ok(Some(node)) = c.node_mut() {
                        if node.direction == split_direction {
                            self.adjust_node_at_cursor(&mut c, delta);
                            self.cascade_size_from_cursor(c);
                            return;
                        }
                    }

                    cursor = c;
                }

                Err(c) => {
                    self.pane.replace(c.tree());
                    return;
                }
            }
        }
    }

    fn activate_pane_direction(&mut self, direction: PaneDirection) {
        if self.zoomed.is_some() {
            if !configuration().unzoom_on_switch_pane {
                return;
            }
            self.toggle_zoom();
        }
        if let Some(panel_idx) = self.get_pane_direction(direction, false) {
            self.set_active_idx(panel_idx);
        }
        let mux = Mux::get();
        if let Some(window_id) = mux.window_containing_tab(self.id) {
            mux.notify(MuxNotification::WindowInvalidated(window_id));
        }
    }

    fn get_pane_direction(&mut self, direction: PaneDirection, ignore_zoom: bool) -> Option<usize> {
        let panes = if ignore_zoom {
            self.iter_panes_ignoring_zoom()
        } else {
            self.iter_panes()
        };

        let active = match panes.iter().find(|pane| pane.is_active) {
            Some(p) => p,
            None => {
                // No active pane somehow...
                return Some(0);
            }
        };

        if matches!(direction, PaneDirection::Next | PaneDirection::Prev) {
            let max_pane_id = panes.iter().map(|p| p.index).max().unwrap_or(active.index);

            return Some(if direction == PaneDirection::Next {
                if active.index == max_pane_id {
                    0
                } else {
                    active.index + 1
                }
            } else {
                if active.index == 0 {
                    max_pane_id
                } else {
                    active.index - 1
                }
            });
        }

        let mut best = None;

        let recency = &self.recency;

        fn edge_intersects(
            active_start: usize,
            active_size: usize,
            current_start: usize,
            current_size: usize,
        ) -> bool {
            intersects_range(
                &(active_start..active_start + active_size),
                &(current_start..current_start + current_size),
            )
        }

        for pane in &panes {
            let score = match direction {
                PaneDirection::Right => {
                    if pane.left == active.left + active.width + 1
                        && edge_intersects(active.top, active.height, pane.top, pane.height)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Left => {
                    if pane.left + pane.width + 1 == active.left
                        && edge_intersects(active.top, active.height, pane.top, pane.height)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Up => {
                    if pane.top + pane.height + 1 == active.top
                        && edge_intersects(active.left, active.width, pane.left, pane.width)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Down => {
                    if active.top + active.height + 1 == pane.top
                        && edge_intersects(active.left, active.width, pane.left, pane.width)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Next | PaneDirection::Prev => unreachable!(),
            };

            if score > 0 {
                let target = match best.take() {
                    Some((best_score, best_pane)) if best_score > score => (best_score, best_pane),
                    _ => (score, pane),
                };
                best.replace(target);
            }
        }

        if let Some((_, target)) = best.take() {
            return Some(target.index);
        }
        None
    }

    fn prune_dead_panes(&mut self) -> bool {
        let mux = Mux::get();
        let dead_floating: Vec<PaneId> = self
            .floating_panes
            .iter()
            .filter(|floating| {
                let in_mux = mux.get_pane(floating.pane.pane_id()).is_some();
                let dead = floating.pane.is_dead();
                dead || !in_mux
            })
            .map(|floating| floating.pane.pane_id())
            .collect();

        for pane_id in &dead_floating {
            let _ = self.remove_floating_pane(*pane_id);
        }

        let removed_tree = !self
            .remove_pane_if(
                |_, pane| {
                    // If the pane is no longer known to the mux, then its liveness
                    // state isn't guaranteed to be monitored or updated, so let's
                    // consider the pane effectively dead if it isn't in the mux.
                    // <https://github.com/wezterm/wezterm/issues/4030>
                    let in_mux = mux.get_pane(pane.pane_id()).is_some();
                    let dead = pane.is_dead();
                    log::trace!(
                        "prune_dead_panes: pane_id={} dead={} in_mux={}",
                        pane.pane_id(),
                        dead,
                        in_mux
                    );
                    dead || !in_mux
                },
                true,
            )
            .is_empty();
        !dead_floating.is_empty() || removed_tree
    }

    fn kill_pane(&mut self, pane_id: PaneId) -> bool {
        if self.has_floating_pane(pane_id) {
            if self.remove_floating_pane(pane_id).is_some() {
                promise::spawn::spawn_into_main_thread(async move {
                    Mux::get().remove_pane(pane_id);
                })
                .detach();
                return true;
            }
            return false;
        }
        !self
            .remove_pane_if(|_, pane| pane.pane_id() == pane_id, true)
            .is_empty()
    }

    fn kill_panes_in_domain(&mut self, domain: DomainId) -> bool {
        let removed_floating = self.remove_floating_panes_in_domain(domain);
        if !removed_floating.is_empty() {
            let ids: Vec<PaneId> = removed_floating.iter().map(|pane| pane.pane_id()).collect();
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                for pane_id in ids {
                    mux.remove_pane(pane_id);
                }
            })
            .detach();
        }
        let removed_tree = self.remove_pane_if(|_, pane| pane.domain_id() == domain, true);
        !removed_floating.is_empty() || !removed_tree.is_empty()
    }

    fn remove_pane(&mut self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        if let Some(pane) = self.remove_floating_pane(pane_id) {
            return Some(pane);
        }
        let panes = self.remove_pane_if(|_, pane| pane.pane_id() == pane_id, false);
        panes.into_iter().next()
    }

    fn remove_pane_if<F>(&mut self, f: F, kill: bool) -> Vec<Arc<dyn Pane>>
    where
        F: Fn(usize, &Arc<dyn Pane>) -> bool,
    {
        let mut dead_panes = vec![];
        let zoomed_pane = self.zoomed.as_ref().map(|p| p.pane_id());

        {
            let root_size = self.size;
            let mut cursor = self.pane.take().unwrap().cursor();
            let mut pane_index = 0;
            let mut removed_indices = vec![];
            let cell_dims = self.cell_dimensions();

            loop {
                // Figure out the available size by looking at our immediate parent node.
                // If we are the root, look at the tab size
                let pane_size = if let Some((branch, Some(parent))) = cursor.path_to_root().next() {
                    if branch == PathBranch::IsRight {
                        parent.second
                    } else {
                        parent.first
                    }
                } else {
                    root_size
                };

                if cursor.is_leaf() {
                    let pane = Arc::clone(cursor.leaf_mut().unwrap());
                    if f(pane_index, &pane) {
                        removed_indices.push(pane_index);
                        if Some(pane.pane_id()) == zoomed_pane {
                            // If we removed the zoomed pane, un-zoom our state!
                            self.zoomed.take();
                        }
                        let parent;
                        match cursor.unsplit_leaf() {
                            Ok((c, dead, p)) => {
                                dead_panes.push(dead);
                                parent = p.unwrap();
                                cursor = c;
                            }
                            Err(c) => {
                                // We might be the root, for example
                                if c.is_top() && c.is_leaf() {
                                    self.pane.replace(Tree::Empty);
                                    dead_panes.push(pane);
                                } else {
                                    self.pane.replace(c.tree());
                                }
                                break;
                            }
                        };

                        // Now we need to increase the size of the current node
                        // and propagate the revised size to its children.
                        let size = TerminalSize {
                            rows: parent.height(),
                            cols: parent.width(),
                            pixel_width: cell_dims.pixel_width * parent.width(),
                            pixel_height: cell_dims.pixel_height * parent.height(),
                            dpi: cell_dims.dpi,
                        };

                        if let Some(unsplit) = cursor.leaf_mut() {
                            unsplit.resize(size).ok();
                        } else {
                            self.apply_pane_size(size, &mut cursor);
                        }
                    } else if !dead_panes.is_empty() {
                        // Apply our revised size to the tty
                        pane.resize(pane_size).ok();
                    }

                    pane_index += 1;
                } else if !dead_panes.is_empty() {
                    self.apply_pane_size(pane_size, &mut cursor);
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(c) => {
                        self.pane.replace(c.tree());
                        break;
                    }
                }
            }

            // Figure out which pane should now be active.
            // If panes earlier than the active pane were closed, then we
            // need to shift the active pane down
            let active_idx = self.active;
            removed_indices.retain(|&idx| idx <= active_idx);
            self.active = active_idx.saturating_sub(removed_indices.len());
        }

        if !dead_panes.is_empty() && kill {
            let to_kill: Vec<_> = dead_panes.iter().map(|p| p.pane_id()).collect();
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                for pane_id in to_kill.into_iter() {
                    mux.remove_pane(pane_id);
                }
            })
            .detach();
        }
        dead_panes
    }

    fn can_close_without_prompting(&mut self, reason: CloseReason) -> bool {
        let panes = self.iter_panes_ignoring_zoom();
        for pos in &panes {
            if !pos.pane.can_close_without_prompting(reason) {
                return false;
            }
        }
        true
    }

    fn is_dead(&mut self) -> bool {
        // Make sure we account for all panes, so that we don't
        // kill the whole tab if the zoomed pane is dead!
        let panes = self.iter_panes_ignoring_zoom();
        let mut dead_count = 0;
        for pos in &panes {
            if pos.pane.is_dead() {
                dead_count += 1;
            }
        }
        dead_count == panes.len()
    }

    fn get_active_pane(&mut self) -> Option<Arc<dyn Pane>> {
        if let Some(zoomed) = self.zoomed.as_ref() {
            return Some(Arc::clone(zoomed));
        }
        if let Some(focused) = self.focused_floating_pane() {
            return Some(focused);
        }

        self.iter_panes_ignoring_zoom()
            .iter()
            .nth(self.active)
            .map(|p| Arc::clone(&p.pane))
    }

    fn get_active_idx(&self) -> usize {
        self.active
    }

    fn set_active_pane(&mut self, pane: &Arc<dyn Pane>) {
        let prior = self.get_active_pane();

        if is_pane(pane, &prior.as_ref()) {
            return;
        }

        if self.zoomed.is_some() {
            if !configuration().unzoom_on_switch_pane {
                return;
            }
            self.toggle_zoom();
        }

        if self.has_floating_pane(pane.pane_id()) {
            self.floating_focus = Some(pane.pane_id());
            self.bring_floating_pane_to_front(pane.pane_id());
            self.advise_focus_change(prior);
            return;
        }

        if let Some(item) = self
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == pane.pane_id())
        {
            self.active = item.index;
            self.recency.tag(item.index);
            self.clear_floating_focus();
            self.advise_focus_change(prior);
        }
    }

    fn advise_focus_change(&mut self, prior: Option<Arc<dyn Pane>>) {
        let mux = Mux::get();
        let current = self.get_active_pane();
        match (prior, current) {
            (Some(prior), Some(current)) if prior.pane_id() != current.pane_id() => {
                prior.focus_changed(false);
                current.focus_changed(true);
                mux.notify(MuxNotification::PaneFocused(current.pane_id()));
            }
            (None, Some(current)) => {
                current.focus_changed(true);
                mux.notify(MuxNotification::PaneFocused(current.pane_id()));
            }
            (Some(prior), None) => {
                prior.focus_changed(false);
            }
            (Some(_), Some(_)) | (None, None) => {
                // no change
            }
        }
    }

    fn set_active_idx(&mut self, pane_index: usize) {
        let prior = self.get_active_pane();
        self.active = pane_index;
        self.recency.tag(pane_index);
        self.clear_floating_focus();
        self.advise_focus_change(prior);
    }

    fn assign_pane(&mut self, pane: &Arc<dyn Pane>) {
        match Tree::new().cursor().assign_top(Arc::clone(pane)) {
            Ok(c) => self.pane = Some(c.tree()),
            Err(_) => panic!("tried to assign root pane to non-empty tree"),
        }
    }

    fn cell_dimensions(&self) -> TerminalSize {
        cell_dimensions(&self.size)
    }

    fn swap_active_with_index(&mut self, pane_index: usize, keep_focus: bool) -> Option<()> {
        let active_idx = self.get_active_idx();
        let mut pane = self.get_active_pane()?;
        log::trace!(
            "swap_active_with_index: pane_index {} active {}",
            pane_index,
            active_idx
        );

        {
            let mut cursor = self.pane.take().unwrap().cursor();

            // locate the requested index
            match cursor.go_to_nth_leaf(pane_index) {
                Ok(c) => cursor = c,
                Err(c) => {
                    log::trace!("didn't find pane {pane_index}");
                    self.pane.replace(c.tree());
                    return None;
                }
            };

            std::mem::swap(&mut pane, cursor.leaf_mut().unwrap());

            // re-position to the root
            cursor = cursor.tree().cursor();

            // and now go and update the active idx
            match cursor.go_to_nth_leaf(active_idx) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    log::trace!("didn't find active {active_idx}");
                    return None;
                }
            };

            std::mem::swap(&mut pane, cursor.leaf_mut().unwrap());
            self.pane.replace(cursor.tree());

            // Advise the panes of their new sizes
            let size = self.size;
            apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);
        }

        // And update focus
        if keep_focus {
            self.set_active_idx(pane_index);
        } else {
            self.advise_focus_change(Some(pane));
        }
        None
    }

    fn compute_split_size(
        &mut self,
        pane_index: usize,
        request: SplitRequest,
    ) -> Option<SplitDirectionAndSize> {
        let cell_dims = self.cell_dimensions();
        let default_new_constraints = PaneConstraints::default();
        let default_width_constraints =
            axis_constraints_from_pane_constraints(default_new_constraints, Axis::Width, None);
        let default_height_constraints =
            axis_constraints_from_pane_constraints(default_new_constraints, Axis::Height, None);

        if request.top_level {
            let size = self.size;
            let tree_width_constraints =
                compute_axis_constraints(self.pane.as_ref().unwrap_or(&Tree::Empty), Axis::Width);
            let tree_height_constraints =
                compute_axis_constraints(self.pane.as_ref().unwrap_or(&Tree::Empty), Axis::Height);

            let ((width1, width2), (height1, height2)) = match request.direction {
                SplitDirection::Horizontal => {
                    let first_constraints = if request.target_is_second {
                        tree_width_constraints
                    } else {
                        default_width_constraints
                    };
                    let second_constraints = if request.target_is_second {
                        default_width_constraints
                    } else {
                        tree_width_constraints
                    };
                    let widths = split_dimension_for_request(
                        size.cols as usize,
                        request,
                        first_constraints,
                        second_constraints,
                    )?;
                    (widths, (size.rows as usize, size.rows as usize))
                }
                SplitDirection::Vertical => {
                    let first_constraints = if request.target_is_second {
                        tree_height_constraints
                    } else {
                        default_height_constraints
                    };
                    let second_constraints = if request.target_is_second {
                        default_height_constraints
                    } else {
                        tree_height_constraints
                    };
                    let heights = split_dimension_for_request(
                        size.rows as usize,
                        request,
                        first_constraints,
                        second_constraints,
                    )?;
                    ((size.cols as usize, size.cols as usize), heights)
                }
            };

            return Some(SplitDirectionAndSize {
                direction: request.direction,
                first: TerminalSize {
                    rows: height1 as _,
                    cols: width1 as _,
                    pixel_height: cell_dims.pixel_height * height1,
                    pixel_width: cell_dims.pixel_width * width1,
                    dpi: cell_dims.dpi,
                },
                second: TerminalSize {
                    rows: height2 as _,
                    cols: width2 as _,
                    pixel_height: cell_dims.pixel_height * height2,
                    pixel_width: cell_dims.pixel_width * width2,
                    dpi: cell_dims.dpi,
                },
            });
        }

        // Ensure that we're not zoomed, otherwise we'll end up in
        // a bogus split state (https://github.com/wezterm/wezterm/issues/723)
        self.set_zoomed(false);

        self.iter_panes().iter().nth(pane_index).and_then(|pos| {
            let existing_constraints = pos.pane.pane_constraints();
            let existing_width_constraints = axis_constraints_from_pane_constraints(
                existing_constraints,
                Axis::Width,
                Some(pos.width),
            );
            let existing_height_constraints = axis_constraints_from_pane_constraints(
                existing_constraints,
                Axis::Height,
                Some(pos.height),
            );
            let ((width1, width2), (height1, height2)) = match request.direction {
                SplitDirection::Horizontal => {
                    let first_constraints = if request.target_is_second {
                        existing_width_constraints
                    } else {
                        default_width_constraints
                    };
                    let second_constraints = if request.target_is_second {
                        default_width_constraints
                    } else {
                        existing_width_constraints
                    };
                    let widths = split_dimension_for_request(
                        pos.width,
                        request,
                        first_constraints,
                        second_constraints,
                    )?;
                    (widths, (pos.height, pos.height))
                }
                SplitDirection::Vertical => {
                    let first_constraints = if request.target_is_second {
                        existing_height_constraints
                    } else {
                        default_height_constraints
                    };
                    let second_constraints = if request.target_is_second {
                        default_height_constraints
                    } else {
                        existing_height_constraints
                    };
                    let heights = split_dimension_for_request(
                        pos.height,
                        request,
                        first_constraints,
                        second_constraints,
                    )?;
                    ((pos.width, pos.width), heights)
                }
            };

            Some(SplitDirectionAndSize {
                direction: request.direction,
                first: TerminalSize {
                    rows: height1 as _,
                    cols: width1 as _,
                    pixel_height: cell_dims.pixel_height * height1,
                    pixel_width: cell_dims.pixel_width * width1,
                    dpi: cell_dims.dpi,
                },
                second: TerminalSize {
                    rows: height2 as _,
                    cols: width2 as _,
                    pixel_height: cell_dims.pixel_height * height2,
                    pixel_width: cell_dims.pixel_width * width2,
                    dpi: cell_dims.dpi,
                },
            })
        })
    }

    fn split_and_insert(
        &mut self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        if self.zoomed.is_some() {
            anyhow::bail!("cannot split while zoomed");
        }

        {
            let split_info = self
                .compute_split_size(pane_index, request)
                .ok_or_else(|| {
                    anyhow::anyhow!("invalid pane_index {}; cannot split!", pane_index)
                })?;

            let tab_size = self.size;
            if split_info.first.rows == 0
                || split_info.first.cols == 0
                || split_info.second.rows == 0
                || split_info.second.cols == 0
                || split_info.top_of_second() + split_info.second.rows > tab_size.rows
                || split_info.left_of_second() + split_info.second.cols > tab_size.cols
            {
                log::error!(
                    "No space for split!!! {:#?} height={} width={} top_of_second={} left_of_second={} tab_size={:?}",
                    split_info,
                    split_info.height(),
                    split_info.width(),
                    split_info.top_of_second(),
                    split_info.left_of_second(),
                    tab_size
                );
                anyhow::bail!("No space for split!");
            }

            if request.top_level && self.pane.as_ref().unwrap().num_leaves() > 0 {
                let existing_width_constraints =
                    compute_axis_constraints(self.pane.as_ref().unwrap(), Axis::Width);
                let existing_height_constraints =
                    compute_axis_constraints(self.pane.as_ref().unwrap(), Axis::Height);
                let new_width_constraints = pane_axis_constraints(&pane, Axis::Width);
                let new_height_constraints = pane_axis_constraints(&pane, Axis::Height);

                let (existing_size, new_size) = if request.target_is_second {
                    (split_info.first, split_info.second)
                } else {
                    (split_info.second, split_info.first)
                };

                if !pane_size_satisfies_constraints(
                    &existing_size,
                    existing_width_constraints,
                    existing_height_constraints,
                ) || !pane_size_satisfies_constraints(
                    &new_size,
                    new_width_constraints,
                    new_height_constraints,
                ) {
                    anyhow::bail!(
                        "No space for top-level split constraints: existing={:?} new={:?}",
                        existing_size,
                        new_size
                    );
                }
            }

            let needs_resize = if request.top_level {
                self.pane.as_ref().unwrap().num_leaves() > 1
            } else {
                false
            };

            if needs_resize {
                // Pre-emptively resize the tab contents down to
                // match the target size; it's easier to reuse
                // existing resize logic that way
                if request.target_is_second {
                    self.resize(split_info.first.clone());
                } else {
                    self.resize(split_info.second.clone());
                }
            }

            let mut cursor = self.pane.take().unwrap().cursor();

            if request.top_level && !cursor.is_leaf() {
                let result = if request.target_is_second {
                    cursor.split_node_and_insert_right(Arc::clone(&pane))
                } else {
                    cursor.split_node_and_insert_left(Arc::clone(&pane))
                };
                cursor = match result {
                    Ok(c) => {
                        cursor = match c.assign_node(Some(split_info)) {
                            Err(c) | Ok(c) => c,
                        };

                        self.pane.replace(cursor.tree());

                        let pane_index = if request.target_is_second {
                            self.pane.as_ref().unwrap().num_leaves().saturating_sub(1)
                        } else {
                            0
                        };

                        self.active = pane_index;
                        self.recency.tag(pane_index);
                        return Ok(pane_index);
                    }
                    Err(cursor) => cursor,
                };
            }

            match cursor.go_to_nth_leaf(pane_index) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    anyhow::bail!("invalid pane_index {}; cannot split!", pane_index);
                }
            };

            let existing_pane = Arc::clone(cursor.leaf_mut().unwrap());

            let (pane1, pane2) = if request.target_is_second {
                (existing_pane, pane)
            } else {
                (pane, existing_pane)
            };

            let pane1_width_constraints = pane_axis_constraints(&pane1, Axis::Width);
            let pane1_height_constraints = pane_axis_constraints(&pane1, Axis::Height);
            let pane2_width_constraints = pane_axis_constraints(&pane2, Axis::Width);
            let pane2_height_constraints = pane_axis_constraints(&pane2, Axis::Height);
            if !pane_size_satisfies_constraints(
                &split_info.first,
                pane1_width_constraints,
                pane1_height_constraints,
            ) || !pane_size_satisfies_constraints(
                &split_info.second,
                pane2_width_constraints,
                pane2_height_constraints,
            ) {
                anyhow::bail!(
                    "No space for split constraints: first={:?} second={:?}",
                    split_info.first,
                    split_info.second
                );
            }

            pane1.resize(split_info.first)?;
            pane2.resize(split_info.second.clone())?;

            *cursor.leaf_mut().unwrap() = pane1;

            match cursor.split_leaf_and_insert_right(pane2) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    anyhow::bail!("invalid pane_index {}; cannot split!", pane_index);
                }
            };

            // cursor now points to the newly created split node;
            // we need to populate its split information
            match cursor.assign_node(Some(split_info)) {
                Err(c) | Ok(c) => self.pane.replace(c.tree()),
            };

            if request.target_is_second {
                self.active = pane_index + 1;
                self.recency.tag(pane_index + 1);
            }
        }

        log::debug!("split info after split: {:#?}", self.iter_splits());
        log::debug!("pane info after split: {:#?}", self.iter_panes());

        Ok(if request.target_is_second {
            pane_index + 1
        } else {
            pane_index
        })
    }

    fn get_zoomed_pane(&self) -> Option<Arc<dyn Pane>> {
        self.zoomed.clone()
    }
}

/// This type is used directly by the codec, take care to bump
/// the codec version if you change this
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub enum PaneNode {
    Empty,
    Split {
        left: Box<PaneNode>,
        right: Box<PaneNode>,
        node: SplitDirectionAndSize,
    },
    Leaf(PaneEntry),
}

impl PaneNode {
    pub fn into_tree(self) -> bintree::Tree<PaneEntry, SplitDirectionAndSize> {
        match self {
            PaneNode::Empty => bintree::Tree::Empty,
            PaneNode::Split { left, right, node } => bintree::Tree::Node {
                left: Box::new((*left).into_tree()),
                right: Box::new((*right).into_tree()),
                data: Some(node),
            },
            PaneNode::Leaf(e) => bintree::Tree::Leaf(e),
        }
    }

    pub fn root_size(&self) -> Option<TerminalSize> {
        match self {
            PaneNode::Empty => None,
            PaneNode::Split { node, .. } => Some(node.size()),
            PaneNode::Leaf(entry) => Some(entry.size),
        }
    }

    pub fn window_and_tab_ids(&self) -> Option<(WindowId, TabId)> {
        match self {
            PaneNode::Empty => None,
            PaneNode::Split { left, right, .. } => match left.window_and_tab_ids() {
                Some(res) => Some(res),
                None => right.window_and_tab_ids(),
            },
            PaneNode::Leaf(entry) => Some((entry.window_id, entry.tab_id)),
        }
    }
}

/// This type is used directly by the codec, take care to bump
/// the codec version if you change this
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct PaneEntry {
    pub window_id: WindowId,
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub title: String,
    pub size: TerminalSize,
    pub working_dir: Option<SerdeUrl>,
    pub is_active_pane: bool,
    pub is_zoomed_pane: bool,
    pub workspace: String,
    pub cursor_pos: StableCursorPosition,
    pub physical_top: StableRowIndex,
    pub top_row: usize,
    pub left_col: usize,
    pub tty_name: Option<String>,
}

#[derive(Deserialize, Clone, Serialize, PartialEq, Debug)]
#[serde(try_from = "String", into = "String")]
pub struct SerdeUrl {
    pub url: Url,
}

impl std::convert::TryFrom<String> for SerdeUrl {
    type Error = url::ParseError;
    fn try_from(s: String) -> Result<SerdeUrl, url::ParseError> {
        let url = Url::parse(&s)?;
        Ok(SerdeUrl { url })
    }
}

impl From<Url> for SerdeUrl {
    fn from(url: Url) -> SerdeUrl {
        SerdeUrl { url }
    }
}

impl Into<Url> for SerdeUrl {
    fn into(self) -> Url {
        self.url
    }
}

impl Into<String> for SerdeUrl {
    fn into(self) -> String {
        self.url.as_str().into()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::renderable::*;
    use frankenterm_term::color::ColorPalette;
    use frankenterm_term::{KeyCode, KeyModifiers, Line, MouseEvent, StableRowIndex};
    use parking_lot::{MappedMutexGuard, Mutex};
    use proptest::prelude::*;
    use rangeset::RangeSet;
    use std::convert::TryFrom;
    use std::ops::Range;
    use termwiz::surface::SequenceNo;
    use url::Url;

    /// Ensure the global Mux singleton is initialized for tests that trigger
    /// focus-change notifications (e.g. floating pane and top-level split tests).
    fn ensure_mux_initialized() {
        if Mux::try_get().is_none() {
            let mux = Arc::new(Mux::new(None));
            Mux::set_mux(&mux);
        }
    }

    struct FakePane {
        id: PaneId,
        size: Mutex<TerminalSize>,
        constraints: PaneConstraints,
        priority: CollapsePriority,
    }

    impl FakePane {
        fn new(id: PaneId, size: TerminalSize) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                constraints: PaneConstraints::default(),
                priority: CollapsePriority::default(),
            })
        }

        fn new_with_constraints(
            id: PaneId,
            size: TerminalSize,
            constraints: PaneConstraints,
        ) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                constraints,
                priority: CollapsePriority::default(),
            })
        }

        fn new_with_priority(
            id: PaneId,
            size: TerminalSize,
            constraints: PaneConstraints,
            priority: CollapsePriority,
        ) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                constraints,
                priority,
            })
        }
    }

    impl Pane for FakePane {
        fn pane_id(&self) -> PaneId {
            self.id
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            unimplemented!();
        }

        fn get_current_seqno(&self) -> SequenceNo {
            unimplemented!();
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            unimplemented!();
        }

        fn with_lines_mut(
            &self,
            _stable_range: Range<StableRowIndex>,
            _with_lines: &mut dyn WithPaneLines,
        ) {
            unimplemented!();
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
            unimplemented!();
        }

        fn get_lines(&self, _lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            unimplemented!();
        }

        fn get_logical_lines(&self, _lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            unimplemented!();
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            let size = *self.size.lock();
            RenderableDimensions {
                cols: size.cols,
                viewport_rows: size.rows,
                scrollback_rows: size.rows,
                physical_top: 0,
                scrollback_top: 0,
                dpi: size.dpi,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
                reverse_video: false,
            }
        }

        fn pane_constraints(&self) -> PaneConstraints {
            self.constraints
        }

        fn collapse_priority(&self) -> CollapsePriority {
            self.priority
        }

        fn get_title(&self) -> String {
            unimplemented!()
        }
        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }
        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            unimplemented!()
        }
        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            *self.size.lock() = size;
            Ok(())
        }

        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn key_up(&self, _: KeyCode, _: KeyModifiers) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn is_dead(&self) -> bool {
            false
        }
        fn palette(&self) -> ColorPalette {
            unimplemented!()
        }
        fn domain_id(&self) -> DomainId {
            1
        }
        fn is_mouse_grabbed(&self) -> bool {
            false
        }
        fn is_alt_screen_active(&self) -> bool {
            false
        }
        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            None
        }
    }

    #[test]
    fn tab_splitting() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        let panes = tab.iter_panes();
        assert_eq!(1, panes.len());
        assert_eq!(0, panes[0].index);
        assert_eq!(true, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(80, panes[0].width);
        assert_eq!(24, panes[0].height);

        assert!(tab
            .compute_split_size(
                1,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                }
            )
            .is_none());

        let horz_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            horz_size,
            SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                second: TerminalSize {
                    rows: 24,
                    cols: 40,
                    pixel_width: 400,
                    pixel_height: 600,
                    dpi: 96,
                },
                first: TerminalSize {
                    rows: 24,
                    cols: 39,
                    pixel_width: 390,
                    pixel_height: 600,
                    dpi: 96,
                },
            }
        );

        let vert_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            vert_size,
            SplitDirectionAndSize {
                direction: SplitDirection::Vertical,
                second: TerminalSize {
                    rows: 12,
                    cols: 80,
                    pixel_width: 800,
                    pixel_height: 300,
                    dpi: 96,
                },
                first: TerminalSize {
                    rows: 11,
                    cols: 80,
                    pixel_width: 800,
                    pixel_height: 275,
                    dpi: 96,
                }
            }
        );

        let new_index = tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new(2, horz_size.second),
            )
            .unwrap();
        assert_eq!(new_index, 1);

        let panes = tab.iter_panes();
        assert_eq!(2, panes.len());

        assert_eq!(0, panes[0].index);
        assert_eq!(false, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(39, panes[0].width);
        assert_eq!(24, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(600, panes[0].pixel_height);
        assert_eq!(1, panes[0].pane.pane_id());

        assert_eq!(1, panes[1].index);
        assert_eq!(true, panes[1].is_active);
        assert_eq!(40, panes[1].left);
        assert_eq!(0, panes[1].top);
        assert_eq!(40, panes[1].width);
        assert_eq!(24, panes[1].height);
        assert_eq!(400, panes[1].pixel_width);
        assert_eq!(600, panes[1].pixel_height);
        assert_eq!(2, panes[1].pane.pane_id());

        let vert_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        let new_index = tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    top_level: false,
                    target_is_second: true,
                    size: Default::default(),
                },
                FakePane::new(3, vert_size.second),
            )
            .unwrap();
        assert_eq!(new_index, 1);

        let panes = tab.iter_panes();
        assert_eq!(3, panes.len());

        assert_eq!(0, panes[0].index);
        assert_eq!(false, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(39, panes[0].width);
        assert_eq!(11, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(275, panes[0].pixel_height);
        assert_eq!(1, panes[0].pane.pane_id());

        assert_eq!(1, panes[1].index);
        assert_eq!(true, panes[1].is_active);
        assert_eq!(0, panes[1].left);
        assert_eq!(12, panes[1].top);
        assert_eq!(39, panes[1].width);
        assert_eq!(12, panes[1].height);
        assert_eq!(390, panes[1].pixel_width);
        assert_eq!(300, panes[1].pixel_height);
        assert_eq!(3, panes[1].pane.pane_id());

        assert_eq!(2, panes[2].index);
        assert_eq!(false, panes[2].is_active);
        assert_eq!(40, panes[2].left);
        assert_eq!(0, panes[2].top);
        assert_eq!(40, panes[2].width);
        assert_eq!(24, panes[2].height);
        assert_eq!(400, panes[2].pixel_width);
        assert_eq!(600, panes[2].pixel_height);
        assert_eq!(2, panes[2].pane.pane_id());

        tab.resize_split_by(1, 1);
        let panes = tab.iter_panes();
        assert_eq!(39, panes[0].width);
        assert_eq!(12, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(300, panes[0].pixel_height);

        assert_eq!(39, panes[1].width);
        assert_eq!(11, panes[1].height);
        assert_eq!(390, panes[1].pixel_width);
        assert_eq!(275, panes[1].pixel_height);

        assert_eq!(40, panes[2].width);
        assert_eq!(24, panes[2].height);
        assert_eq!(400, panes[2].pixel_width);
        assert_eq!(600, panes[2].pixel_height);
    }

    #[test]
    fn floating_pane_add_clamps_rect_and_takes_focus() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        let floating = tab.add_floating_pane(
            FakePane::new(99, size),
            FloatingPaneRect {
                left: 78,
                top: 23,
                width: 1,
                height: 1,
            },
        );

        assert_eq!(99, floating.pane_id);
        assert!(floating.is_focused);
        assert_eq!(75, floating.left);
        assert_eq!(21, floating.top);
        assert_eq!(MIN_FLOATING_PANE_WIDTH, floating.width);
        assert_eq!(MIN_FLOATING_PANE_HEIGHT, floating.height);
        assert_eq!(Some(2), tab.count_panes());
        assert_eq!(99, tab.get_active_pane().expect("floating focus").pane_id());
    }

    #[test]
    fn floating_pane_focus_and_visibility_fallback() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        tab.add_floating_pane(
            FakePane::new(2, size),
            FloatingPaneRect {
                left: 2,
                top: 2,
                width: 20,
                height: 10,
            },
        );
        tab.add_floating_pane(
            FakePane::new(3, size),
            FloatingPaneRect {
                left: 8,
                top: 6,
                width: 25,
                height: 12,
            },
        );

        let panes = tab.iter_floating_panes();
        assert_eq!(2, panes.len());
        assert_eq!(2, panes[0].pane_id);
        assert_eq!(3, panes[1].pane_id);
        assert!(panes[1].is_focused);

        assert!(tab.set_floating_pane_focus(2));
        let panes = tab.iter_floating_panes();
        assert_eq!(2, panes.last().expect("focused pane").pane_id);
        assert!(panes.last().expect("focused pane").is_focused);
        assert_eq!(
            2,
            tab.get_active_pane().expect("focused floating").pane_id()
        );

        assert!(tab.set_floating_pane_visible(2, false));
        assert_eq!(
            1,
            tab.get_active_pane()
                .expect("fallback split pane")
                .pane_id()
        );

        let pane_two = tab
            .iter_floating_panes()
            .into_iter()
            .find(|pane| pane.pane_id == 2)
            .expect("pane 2 exists");
        assert!(!pane_two.visible);
    }

    #[test]
    fn remove_floating_pane_updates_membership_and_count() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        tab.add_floating_pane(
            FakePane::new(42, size),
            FloatingPaneRect {
                left: 4,
                top: 4,
                width: 30,
                height: 8,
            },
        );

        assert!(tab.contains_pane(42));
        let removed = tab
            .remove_floating_pane(42)
            .expect("floating pane should be removed");
        assert_eq!(42, removed.pane_id());
        assert!(!tab.contains_pane(42));
        assert_eq!(Some(1), tab.count_panes());
        assert_eq!(
            1,
            tab.get_active_pane().expect("split pane focus").pane_id()
        );
    }

    #[test]
    fn floating_pane_z_order_deterministic_after_operations() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 30,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 750,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        // Add three floating panes
        for id in 10..13 {
            tab.add_floating_pane(
                FakePane::new(id, size),
                FloatingPaneRect {
                    left: id * 2,
                    top: id,
                    width: 20,
                    height: 10,
                },
            );
        }

        // Bring pane 10 to front (z_order only, not focus)
        assert!(tab.bring_floating_pane_to_front(10));

        let panes = tab.iter_floating_panes();
        assert_eq!(3, panes.len());

        // Pane 10 now has the highest z_order and sorts last,
        // but pane 12 retains focus since set_floating_pane_focus
        // was not called.
        assert_eq!(10, panes.last().unwrap().pane_id);
        // Focus remains on pane 12 (last added)
        let focused = panes.iter().find(|p| p.is_focused).unwrap();
        assert_eq!(12, focused.pane_id);

        // Verify z-orders are unique
        let z_orders: Vec<u32> = panes.iter().map(|p| p.z_order).collect();
        let mut deduped = z_orders.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(z_orders.len(), deduped.len(), "z-orders must be unique");
    }

    #[test]
    fn floating_pane_reposition_updates_geometry() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 30,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 750,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        tab.add_floating_pane(
            FakePane::new(50, size),
            FloatingPaneRect {
                left: 5,
                top: 5,
                width: 30,
                height: 15,
            },
        );

        let new_rect = FloatingPaneRect {
            left: 10,
            top: 10,
            width: 40,
            height: 12,
        };
        let updated = tab.set_floating_pane_rect(50, new_rect).unwrap();
        assert_eq!(10, updated.left);
        assert_eq!(10, updated.top);
        assert_eq!(40, updated.width);
        assert_eq!(12, updated.height);

        // Non-existent pane returns None
        assert!(tab.set_floating_pane_rect(999, new_rect).is_none());
    }

    #[test]
    fn floating_pane_resize_clamps_to_tab() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        // Resize tab to a very small size and check floating pane gets clamped
        tab.add_floating_pane(
            FakePane::new(60, size),
            FloatingPaneRect {
                left: 50,
                top: 10,
                width: 30,
                height: 15,
            },
        );

        let small = TerminalSize {
            rows: 10,
            cols: 20,
            pixel_width: 200,
            pixel_height: 250,
            dpi: 96,
        };
        tab.resize(small);

        let panes = tab.iter_floating_panes();
        assert_eq!(1, panes.len());
        let fp = &panes[0];
        // After resize to 20 cols, floating pane should be clamped
        assert!(
            fp.left + fp.width <= 20,
            "floating pane should fit within new cols: left={} width={}",
            fp.left,
            fp.width
        );
        assert!(
            fp.top + fp.height <= 10,
            "floating pane should fit within new rows: top={} height={}",
            fp.top,
            fp.height
        );
    }

    #[test]
    fn floating_pane_remove_nonexistent_returns_none() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        assert!(tab.remove_floating_pane(999).is_none());
    }

    #[test]
    fn resize_split_by_clamps_to_horizontal_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_width: 5,
                ..PaneConstraints::default()
            },
        ));

        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .expect("split to compute");
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_constraints(
                2,
                split.second,
                PaneConstraints {
                    min_width: 30,
                    ..PaneConstraints::default()
                },
            ),
        )
        .expect("split insertion to succeed");

        tab.resize_split_by(0, -200);
        let panes = tab.iter_panes();
        assert_eq!(5, panes[0].width);
        assert_eq!(74, panes[1].width);

        tab.resize_split_by(0, 200);
        let panes = tab.iter_panes();
        assert_eq!(49, panes[0].width);
        assert_eq!(30, panes[1].width);
    }

    #[test]
    fn resize_split_by_clamps_to_vertical_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_height: 10,
                ..PaneConstraints::default()
            },
        ));

        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .expect("split to compute");
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Vertical,
                ..Default::default()
            },
            FakePane::new_with_constraints(
                2,
                split.second,
                PaneConstraints {
                    min_height: 7,
                    ..PaneConstraints::default()
                },
            ),
        )
        .expect("split insertion to succeed");

        tab.resize_split_by(0, -200);
        let panes = tab.iter_panes();
        assert_eq!(10, panes[0].height);
        assert_eq!(13, panes[1].height);

        tab.resize_split_by(0, 200);
        let panes = tab.iter_panes();
        assert_eq!(16, panes[0].height);
        assert_eq!(7, panes[1].height);
    }

    #[test]
    fn resize_clamps_to_tree_constraint_minimum() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_width: 30,
                ..PaneConstraints::default()
            },
        ));

        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .expect("split to compute");
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_constraints(
                2,
                split.second,
                PaneConstraints {
                    min_width: 20,
                    ..PaneConstraints::default()
                },
            ),
        )
        .expect("split insertion to succeed");

        tab.resize(TerminalSize {
            rows: 24,
            cols: 10,
            pixel_width: 100,
            pixel_height: 600,
            dpi: 96,
        });

        let resized = tab.get_size();
        assert_eq!(51, resized.cols);
        assert_eq!(24, resized.rows);

        let panes = tab.iter_panes();
        assert_eq!(30, panes[0].width);
        assert_eq!(20, panes[1].width);
    }

    #[test]
    fn compute_split_size_clamps_to_existing_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_width: 30,
                ..PaneConstraints::default()
            },
        ));

        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    target_is_second: true,
                    size: SplitSize::Cells(70),
                    ..Default::default()
                },
            )
            .expect("split to compute");

        assert_eq!(30, split.first.cols);
        assert_eq!(49, split.second.cols);
    }

    #[test]
    fn split_and_insert_rejects_unfittable_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_width: 30,
                ..PaneConstraints::default()
            },
        ));

        let result = tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                target_is_second: true,
                size: SplitSize::Cells(5),
                ..Default::default()
            },
            FakePane::new_with_constraints(
                2,
                size,
                PaneConstraints {
                    min_width: 60,
                    ..PaneConstraints::default()
                },
            ),
        );

        assert!(result.is_err());
    }

    #[test]
    fn compute_split_size_respects_existing_max_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                max_width: Some(35),
                ..PaneConstraints::default()
            },
        ));

        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    target_is_second: false,
                    size: SplitSize::Cells(10),
                    ..Default::default()
                },
            )
            .expect("split to compute");

        assert_eq!(35, split.second.cols);
        assert_eq!(44, split.first.cols);
    }

    #[test]
    fn resize_clamps_to_fixed_pane_size() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                fixed: true,
                ..PaneConstraints::default()
            },
        ));

        tab.resize(TerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 1000,
            dpi: 96,
        });

        let resized = tab.get_size();
        assert_eq!(80, resized.cols);
        assert_eq!(24, resized.rows);
    }

    #[test]
    fn resize_split_by_respects_max_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                max_width: Some(35),
                ..PaneConstraints::default()
            },
        ));

        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .expect("split to compute");
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new(2, split.second),
        )
        .expect("split insertion to succeed");

        tab.resize_split_by(0, 200);
        let panes = tab.iter_panes();
        assert_eq!(35, panes[0].width);
        assert_eq!(44, panes[1].width);
    }

    #[test]
    fn top_level_split_rejects_incompatible_new_pane_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        let first_split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .expect("initial split to compute");
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new(2, first_split.second),
        )
        .expect("initial split insertion to succeed");

        let result = tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                top_level: true,
                target_is_second: true,
                size: SplitSize::Cells(10),
            },
            FakePane::new_with_constraints(
                3,
                size,
                PaneConstraints {
                    min_width: 60,
                    ..PaneConstraints::default()
                },
            ),
        );

        assert!(result.is_err());
    }

    #[test]
    #[ignore] // wa-2dd4s.4: constraint validation not yet implemented
    fn top_level_split_rejects_incompatible_existing_tree_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_width: 40,
                ..PaneConstraints::default()
            },
        ));

        let first_split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    target_is_second: true,
                    size: SplitSize::Cells(30),
                    ..Default::default()
                },
            )
            .expect("initial split to compute");
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                target_is_second: true,
                size: SplitSize::Cells(30),
                ..Default::default()
            },
            FakePane::new_with_constraints(
                2,
                first_split.second,
                PaneConstraints {
                    min_width: 30,
                    ..PaneConstraints::default()
                },
            ),
        )
        .expect("initial split insertion to succeed");

        let result = tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                top_level: true,
                target_is_second: true,
                size: SplitSize::Cells(20),
            },
            FakePane::new(3, size),
        );

        assert!(result.is_err());
    }

    #[test]
    fn resize_fanout_worker_plan_stays_sequential_for_small_work() {
        assert_eq!(1, compute_resize_fanout_workers(1, 16));
        assert_eq!(1, compute_resize_fanout_workers(7, 16));
    }

    #[test]
    fn resize_fanout_worker_plan_caps_worker_count() {
        assert_eq!(
            RESIZE_FANOUT_MAX_WORKERS,
            compute_resize_fanout_workers(256, 64)
        );
    }

    #[test]
    fn resize_fanout_worker_plan_enforces_min_batch_size() {
        assert_eq!(2, compute_resize_fanout_workers(9, 16));
        assert_eq!(3, compute_resize_fanout_workers(12, 16));
        assert_eq!(2, compute_resize_fanout_workers(8, 16));
    }

    proptest! {
        #[test]
        fn resize_split_by_preserves_width_budget_and_mins(
            left_min in 1usize..40,
            right_min in 1usize..40,
            delta in -400isize..400isize,
        ) {
            let size = TerminalSize {
                rows: 30,
                cols: 160,
                pixel_width: 1600,
                pixel_height: 900,
                dpi: 96,
            };
            let tab = Tab::new(&size);
            tab.assign_pane(&FakePane::new_with_constraints(
                1,
                size,
                PaneConstraints {
                    min_width: left_min,
                    ..PaneConstraints::default()
                },
            ));

            let split = tab
                .compute_split_size(
                    0,
                    SplitRequest {
                        direction: SplitDirection::Horizontal,
                        ..Default::default()
                    },
                )
                .expect("split to compute");
            tab.split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new_with_constraints(
                    2,
                    split.second,
                    PaneConstraints {
                        min_width: right_min,
                        ..PaneConstraints::default()
                    },
                ),
            )
            .expect("split insertion to succeed");

            tab.resize_split_by(0, delta);
            let panes = tab.iter_panes();
            prop_assert_eq!(2, panes.len());
            prop_assert_eq!(panes[0].width + panes[1].width + 1, tab.get_size().cols);
            prop_assert!(panes[0].width >= left_min);
            prop_assert!(panes[1].width >= right_min);
        }

        #[test]
        fn fixed_pane_ignores_resize_requests(
            target_cols in 20usize..240,
            target_rows in 8usize..120,
        ) {
            let size = TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 800,
                pixel_height: 600,
                dpi: 96,
            };
            let tab = Tab::new(&size);
            tab.assign_pane(&FakePane::new_with_constraints(
                1,
                size,
                PaneConstraints {
                    fixed: true,
                    ..PaneConstraints::default()
                },
            ));

            tab.resize(TerminalSize {
                rows: target_rows,
                cols: target_cols,
                pixel_width: target_cols.saturating_mul(10),
                pixel_height: target_rows.saturating_mul(20),
                dpi: 96,
            });

            let resized = tab.get_size();
            prop_assert_eq!(size.cols, resized.cols);
            prop_assert_eq!(size.rows, resized.rows);
        }

        /// Verify that collapsing and uncollapsing are consistent: after a
        /// shrink + grow cycle, no pane remains spuriously collapsed.
        #[test]
        fn collapse_uncollapse_cycle_is_consistent(
            target_cols in 10usize..60,
        ) {
            let initial_cols = 120usize;
            let size = TerminalSize {
                rows: 24,
                cols: initial_cols,
                pixel_width: initial_cols * 10,
                pixel_height: 600,
                dpi: 96,
            };

            let tab = Tab::new(&size);
            tab.assign_pane(&FakePane::new_with_priority(
                1,
                size,
                PaneConstraints {
                    min_width: 20,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Low,
            ));
            let split = tab
                .compute_split_size(
                    0,
                    SplitRequest {
                        direction: SplitDirection::Horizontal,
                        ..Default::default()
                    },
                )
                .expect("split");
            tab.split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new_with_priority(
                    2,
                    split.second,
                    PaneConstraints {
                        min_width: 20,
                        min_height: 3,
                        ..PaneConstraints::default()
                    },
                    CollapsePriority::Normal,
                ),
            )
            .expect("insert");

            // Shrink to target_cols
            let small = TerminalSize {
                rows: 24,
                cols: target_cols,
                pixel_width: target_cols * 10,
                pixel_height: 600,
                dpi: 96,
            };
            tab.resize(small);

            // Grow back to original
            tab.resize(size);

            // After growing back, no pane should remain collapsed
            prop_assert!(
                tab.collapsed_pane_ids().is_empty(),
                "all panes should be uncollapsed after growing back to original"
            );
        }

        /// Verify that Never-priority panes are never found in the collapsed set
        /// regardless of target size.
        #[test]
        fn never_priority_never_in_collapsed_set(
            target_cols in 5usize..40,
        ) {
            let size = TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 800,
                pixel_height: 600,
                dpi: 96,
            };

            let tab = Tab::new(&size);
            tab.assign_pane(&FakePane::new_with_priority(
                1,
                size,
                PaneConstraints {
                    min_width: 15,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Low,
            ));
            let split = tab
                .compute_split_size(
                    0,
                    SplitRequest {
                        direction: SplitDirection::Horizontal,
                        ..Default::default()
                    },
                )
                .expect("split");
            tab.split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new_with_priority(
                    2,
                    split.second,
                    PaneConstraints {
                        min_width: 15,
                        min_height: 3,
                        ..PaneConstraints::default()
                    },
                    CollapsePriority::Never,
                ),
            )
            .expect("insert");

            let small = TerminalSize {
                rows: 24,
                cols: target_cols,
                pixel_width: target_cols * 10,
                pixel_height: 600,
                dpi: 96,
            };
            tab.resize(small);

            prop_assert!(
                !tab.is_pane_collapsed(2),
                "Never-priority pane must never be collapsed"
            );
        }

        /// Verify that after adding N floating panes and bringing random
        /// ones to front, z-orders remain unique and the focused pane
        /// renders last in iteration order.
        #[test]
        fn floating_z_order_always_unique_after_focus_ops(
            bring_to_front_id in 10usize..15,
        ) {
            ensure_mux_initialized();
            let size = TerminalSize {
                rows: 40,
                cols: 120,
                pixel_width: 1200,
                pixel_height: 1000,
                dpi: 96,
            };
            let tab = Tab::new(&size);
            tab.assign_pane(&FakePane::new(1, size));

            // Add 5 floating panes (ids 10-14)
            for id in 10..15 {
                tab.add_floating_pane(
                    FakePane::new(id, size),
                    FloatingPaneRect {
                        left: id * 3,
                        top: id * 2,
                        width: 20,
                        height: 10,
                    },
                );
            }

            // Bring a random one to front
            tab.set_floating_pane_focus(bring_to_front_id);

            let panes = tab.iter_floating_panes();
            prop_assert_eq!(5, panes.len());

            // Check z-orders are unique
            let mut z_orders: Vec<u32> = panes.iter().map(|p| p.z_order).collect();
            z_orders.sort();
            let before = z_orders.len();
            z_orders.dedup();
            prop_assert_eq!(before, z_orders.len(), "z-orders must be unique");

            // Focused pane must be last in iteration
            let last = panes.last().unwrap();
            prop_assert!(last.is_focused, "last in iteration must be focused");
            prop_assert_eq!(bring_to_front_id, last.pane_id);
        }
    }

    fn is_send_and_sync<T: Send + Sync>() -> bool {
        true
    }

    #[test]
    fn tab_is_send_and_sync() {
        assert!(is_send_and_sync::<Tab>());
    }

    // ── SplitDirection ───────────────────────────────────────

    #[test]
    fn split_direction_equality() {
        assert_eq!(SplitDirection::Horizontal, SplitDirection::Horizontal);
        assert_eq!(SplitDirection::Vertical, SplitDirection::Vertical);
        assert_ne!(SplitDirection::Horizontal, SplitDirection::Vertical);
    }

    #[test]
    fn split_direction_clone_copy() {
        let d = SplitDirection::Horizontal;
        let d2 = d; // Copy
        let d3 = d.clone(); // Clone
        assert_eq!(d, d2);
        assert_eq!(d, d3);
    }

    #[test]
    fn split_direction_debug() {
        assert!(format!("{:?}", SplitDirection::Horizontal).contains("Horizontal"));
        assert!(format!("{:?}", SplitDirection::Vertical).contains("Vertical"));
    }

    // ── SplitSize ────────────────────────────────────────────

    #[test]
    fn split_size_default_is_50_percent() {
        assert_eq!(SplitSize::default(), SplitSize::Percent(50));
    }

    #[test]
    fn split_size_equality() {
        assert_eq!(SplitSize::Cells(10), SplitSize::Cells(10));
        assert_eq!(SplitSize::Percent(50), SplitSize::Percent(50));
        assert_ne!(SplitSize::Cells(10), SplitSize::Cells(20));
        assert_ne!(SplitSize::Cells(50), SplitSize::Percent(50));
    }

    #[test]
    fn split_size_clone_copy() {
        let s = SplitSize::Cells(42);
        let s2 = s; // Copy
        let s3 = s.clone(); // Clone
        assert_eq!(s, s2);
        assert_eq!(s, s3);
    }

    // ── SplitRequest ─────────────────────────────────────────

    #[test]
    fn split_request_default() {
        let r = SplitRequest::default();
        assert_eq!(r.direction, SplitDirection::Horizontal);
        assert!(r.target_is_second);
        assert!(!r.top_level);
        assert_eq!(r.size, SplitSize::Percent(50));
    }

    #[test]
    fn split_request_equality() {
        let a = SplitRequest::default();
        let b = SplitRequest::default();
        assert_eq!(a, b);
        let c = SplitRequest {
            direction: SplitDirection::Vertical,
            ..Default::default()
        };
        assert_ne!(a, c);
    }

    // ── PositionedSplit ──────────────────────────────────────

    #[test]
    fn positioned_split_equality() {
        let a = PositionedSplit {
            index: 0,
            direction: SplitDirection::Horizontal,
            left: 40,
            top: 0,
            size: 24,
        };
        let b = PositionedSplit {
            index: 0,
            direction: SplitDirection::Horizontal,
            left: 40,
            top: 0,
            size: 24,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn positioned_split_inequality() {
        let a = PositionedSplit {
            index: 0,
            direction: SplitDirection::Horizontal,
            left: 40,
            top: 0,
            size: 24,
        };
        let b = PositionedSplit {
            index: 1,
            direction: SplitDirection::Vertical,
            left: 0,
            top: 12,
            size: 80,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn positioned_split_clone_copy() {
        let a = PositionedSplit {
            index: 5,
            direction: SplitDirection::Vertical,
            left: 10,
            top: 20,
            size: 30,
        };
        let b = a; // Copy
        let c = a.clone(); // Clone
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn positioned_split_debug() {
        let s = PositionedSplit {
            index: 0,
            direction: SplitDirection::Horizontal,
            left: 40,
            top: 0,
            size: 24,
        };
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("PositionedSplit"));
        assert!(dbg.contains("Horizontal"));
    }

    // ── SplitDirectionAndSize ────────────────────────────────

    #[test]
    fn split_direction_and_size_width_horizontal() {
        let s = SplitDirectionAndSize {
            direction: SplitDirection::Horizontal,
            first: TerminalSize {
                cols: 40,
                rows: 24,
                pixel_width: 400,
                pixel_height: 600,
                dpi: 96,
            },
            second: TerminalSize {
                cols: 39,
                rows: 24,
                pixel_width: 390,
                pixel_height: 600,
                dpi: 96,
            },
        };
        // Horizontal: first.cols + second.cols + 1 (for separator)
        assert_eq!(s.width(), 80);
        assert_eq!(s.height(), 24);
    }

    #[test]
    fn split_direction_and_size_height_vertical() {
        let s = SplitDirectionAndSize {
            direction: SplitDirection::Vertical,
            first: TerminalSize {
                cols: 80,
                rows: 12,
                pixel_width: 800,
                pixel_height: 300,
                dpi: 96,
            },
            second: TerminalSize {
                cols: 80,
                rows: 11,
                pixel_width: 800,
                pixel_height: 275,
                dpi: 96,
            },
        };
        // Vertical: first.rows + second.rows + 1 (for separator)
        assert_eq!(s.height(), 24);
        assert_eq!(s.width(), 80);
    }

    // ── PaneNode ─────────────────────────────────────────────

    #[test]
    fn pane_node_empty_root_size_is_none() {
        let node = PaneNode::Empty;
        assert!(node.root_size().is_none());
    }

    #[test]
    fn pane_node_empty_window_and_tab_ids_is_none() {
        let node = PaneNode::Empty;
        assert!(node.window_and_tab_ids().is_none());
    }

    #[test]
    fn pane_node_leaf_root_size() {
        let entry = PaneEntry {
            window_id: 0,
            tab_id: 0,
            pane_id: 1,
            title: "test".to_string(),
            size: TerminalSize::default(),
            working_dir: None,
            is_active_pane: true,
            is_zoomed_pane: false,
            workspace: "default".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        };
        let node = PaneNode::Leaf(entry);
        let size = node.root_size();
        assert!(size.is_some());
        assert_eq!(size.unwrap().rows, 24);
        assert_eq!(size.unwrap().cols, 80);
    }

    #[test]
    fn pane_node_leaf_window_and_tab_ids() {
        let entry = PaneEntry {
            window_id: 5,
            tab_id: 10,
            pane_id: 1,
            title: "test".to_string(),
            size: TerminalSize::default(),
            working_dir: None,
            is_active_pane: false,
            is_zoomed_pane: false,
            workspace: "ws".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: Some("/dev/pts/0".to_string()),
        };
        let node = PaneNode::Leaf(entry);
        assert_eq!(node.window_and_tab_ids(), Some((5, 10)));
    }

    #[test]
    fn pane_node_debug() {
        let node = PaneNode::Empty;
        let dbg = format!("{:?}", node);
        assert!(dbg.contains("Empty"));
    }

    // ── SerdeUrl ─────────────────────────────────────────────

    #[test]
    fn serde_url_from_url() {
        let url = Url::parse("https://example.com").unwrap();
        let serde_url = SerdeUrl::from(url.clone());
        assert_eq!(serde_url.url, url);
    }

    #[test]
    fn serde_url_try_from_string() {
        let serde_url = SerdeUrl::try_from("https://example.com".to_string());
        assert!(serde_url.is_ok());
        assert_eq!(serde_url.unwrap().url.as_str(), "https://example.com/");
    }

    #[test]
    fn serde_url_try_from_invalid_string() {
        let result = SerdeUrl::try_from("not a url".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn serde_url_into_string() {
        let url = Url::parse("https://example.com/path").unwrap();
        let serde_url = SerdeUrl::from(url);
        let s: String = serde_url.into();
        assert_eq!(s, "https://example.com/path");
    }

    #[test]
    fn serde_url_into_url() {
        let url = Url::parse("file:///home/user").unwrap();
        let serde_url = SerdeUrl::from(url.clone());
        let back: Url = serde_url.into();
        assert_eq!(back, url);
    }

    #[test]
    fn serde_url_clone_eq() {
        let url = Url::parse("https://example.com").unwrap();
        let a = SerdeUrl::from(url);
        let b = a.clone();
        assert_eq!(a, b);
    }

    // ── PaneEntry ────────────────────────────────────────────

    #[test]
    fn pane_entry_clone_eq() {
        let entry = PaneEntry {
            window_id: 0,
            tab_id: 0,
            pane_id: 1,
            title: "shell".to_string(),
            size: TerminalSize::default(),
            working_dir: None,
            is_active_pane: true,
            is_zoomed_pane: false,
            workspace: "default".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        };
        let cloned = entry.clone();
        assert_eq!(entry, cloned);
    }

    #[test]
    fn pane_entry_debug() {
        let entry = PaneEntry {
            window_id: 1,
            tab_id: 2,
            pane_id: 3,
            title: "vim".to_string(),
            size: TerminalSize::default(),
            working_dir: None,
            is_active_pane: false,
            is_zoomed_pane: true,
            workspace: "coding".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 100,
            top_row: 0,
            left_col: 5,
            tty_name: Some("/dev/pts/1".to_string()),
        };
        let dbg = format!("{:?}", entry);
        assert!(dbg.contains("PaneEntry"));
        assert!(dbg.contains("vim"));
    }

    // ── Collapse priority tests ─────────────────────────────

    #[test]
    fn collapse_low_priority_pane_on_shrink() {
        // Two horizontal panes: left has Low priority (min_width=20),
        // right has Never priority (min_width=20).  Total min = 20+1+20 = 41.
        // When we shrink to 30 cols, left should be collapsed.
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 20,
                min_height: 3,
                ..PaneConstraints::default()
            },
            CollapsePriority::Low,
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split.second,
                PaneConstraints {
                    min_width: 20,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Never,
            ),
        )
        .unwrap();

        // Sanity: nothing collapsed yet
        assert!(!tab.is_pane_collapsed(1));
        assert!(!tab.is_pane_collapsed(2));

        // Shrink to 30 cols — below min of 41 — should collapse pane 1 (Low)
        let small = TerminalSize {
            rows: 24,
            cols: 30,
            pixel_width: 300,
            pixel_height: 600,
            dpi: 96,
        };
        tab.resize(small);

        assert!(
            tab.is_pane_collapsed(1),
            "Low-priority pane should be collapsed"
        );
        assert!(
            !tab.is_pane_collapsed(2),
            "Never-priority pane should NOT be collapsed"
        );

        // The non-collapsed pane should have gotten the extra space
        let panes = tab.iter_panes();
        let pane2 = panes.iter().find(|p| p.pane.pane_id() == 2).unwrap();
        // Pane 2 should use most of the 30 cols (minus 1 separator, 1 for collapsed)
        assert!(
            pane2.width >= 20,
            "Non-collapsed pane should get the freed space, got width={}",
            pane2.width
        );
    }

    #[test]
    fn collapse_priority_ordering() {
        // Three horizontal panes: Low, Normal, High priority.
        // Use a wide terminal so all three splits succeed.
        let size = TerminalSize {
            rows: 24,
            cols: 200,
            pixel_width: 2000,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 30,
                min_height: 3,
                ..PaneConstraints::default()
            },
            CollapsePriority::Low,
        ));

        // Split horizontally to add pane 2 (Normal)
        let split1 = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split1.second,
                PaneConstraints {
                    min_width: 30,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Normal,
            ),
        )
        .unwrap();

        // Split the right pane (index 1) to add pane 3 (High)
        let split2 = tab
            .compute_split_size(
                1,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            1,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                3,
                split2.second,
                PaneConstraints {
                    min_width: 30,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::High,
            ),
        )
        .unwrap();

        // Three panes at 30 min each: min total = 30+1+30+1+30 = 92.
        // Shrink to 35 cols: needs two collapsed to fit (30+1+1+1+1 = 34 ≤ 35).
        let small = TerminalSize {
            rows: 24,
            cols: 35,
            pixel_width: 350,
            pixel_height: 600,
            dpi: 96,
        };
        tab.resize(small);

        // Low should collapse first, then Normal
        assert!(
            tab.is_pane_collapsed(1),
            "Low-priority pane should be collapsed first"
        );
        assert!(
            tab.is_pane_collapsed(2),
            "Normal-priority pane should be collapsed second"
        );
        assert!(
            !tab.is_pane_collapsed(3),
            "High-priority pane should remain"
        );
    }

    #[test]
    fn never_priority_pane_exempt_from_collapse() {
        let size = TerminalSize {
            rows: 24,
            cols: 60,
            pixel_width: 600,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 25,
                min_height: 3,
                ..PaneConstraints::default()
            },
            CollapsePriority::Never,
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split.second,
                PaneConstraints {
                    min_width: 25,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Never,
            ),
        )
        .unwrap();

        // Shrink below both panes' minimum — neither should collapse
        let small = TerminalSize {
            rows: 24,
            cols: 20,
            pixel_width: 200,
            pixel_height: 600,
            dpi: 96,
        };
        tab.resize(small);

        assert!(
            !tab.is_pane_collapsed(1),
            "Never-priority should never collapse"
        );
        assert!(
            !tab.is_pane_collapsed(2),
            "Never-priority should never collapse"
        );
    }

    #[test]
    fn uncollapse_panes_on_grow() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 20,
                min_height: 3,
                ..PaneConstraints::default()
            },
            CollapsePriority::Low,
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split.second,
                PaneConstraints {
                    min_width: 20,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Normal,
            ),
        )
        .unwrap();

        // Shrink to cause collapse
        let small = TerminalSize {
            rows: 24,
            cols: 25,
            pixel_width: 250,
            pixel_height: 600,
            dpi: 96,
        };
        tab.resize(small);
        assert!(tab.is_pane_collapsed(1), "pane 1 should be collapsed");

        // Grow back to original size — pane should uncollapse
        tab.resize(size);
        assert!(
            !tab.is_pane_collapsed(1),
            "pane 1 should be uncollapsed after growing"
        );
        assert!(
            !tab.is_pane_collapsed(2),
            "pane 2 should be uncollapsed after growing"
        );
    }

    #[test]
    fn collapsed_pane_ids_api() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 20,
                min_height: 3,
                ..PaneConstraints::default()
            },
            CollapsePriority::Low,
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split.second,
                PaneConstraints {
                    min_width: 20,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Never,
            ),
        )
        .unwrap();

        // Initially empty
        assert!(tab.collapsed_pane_ids().is_empty());

        // Shrink to trigger collapse
        let small = TerminalSize {
            rows: 24,
            cols: 25,
            pixel_width: 250,
            pixel_height: 600,
            dpi: 96,
        };
        tab.resize(small);

        let collapsed = tab.collapsed_pane_ids();
        assert!(collapsed.contains(&1));
        assert!(!collapsed.contains(&2));
    }

    #[test]
    fn compute_split_budget_basic() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_width: 10,
                min_height: 3,
                ..PaneConstraints::default()
            },
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_constraints(
                2,
                split.second,
                PaneConstraints {
                    min_width: 10,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
            ),
        )
        .unwrap();

        let budget = tab.compute_split_budget(0);
        assert!(budget.is_some(), "split 0 should exist");
        let (shrink, grow) = budget.unwrap();
        // Shrink is negative (how far first child can shrink)
        assert!(shrink < 0, "should be able to shrink first child");
        // Grow is positive (how far first child can grow)
        assert!(grow > 0, "should be able to grow first child");

        // Non-existent split returns None
        assert!(tab.compute_split_budget(99).is_none());
    }

    #[test]
    fn vertical_collapse_on_shrink() {
        // Vertical split: top pane Low priority, bottom pane Never.
        let size = TerminalSize {
            rows: 40,
            cols: 80,
            pixel_width: 800,
            pixel_height: 1000,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 5,
                min_height: 15,
                ..PaneConstraints::default()
            },
            CollapsePriority::Low,
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Vertical,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split.second,
                PaneConstraints {
                    min_width: 5,
                    min_height: 15,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Never,
            ),
        )
        .unwrap();

        // Shrink rows below minimum (15+1+15 = 31)
        let small = TerminalSize {
            rows: 20,
            cols: 80,
            pixel_width: 800,
            pixel_height: 500,
            dpi: 96,
        };
        tab.resize(small);

        assert!(
            tab.is_pane_collapsed(1),
            "Top pane (Low) should be collapsed on vertical shrink"
        );
        assert!(
            !tab.is_pane_collapsed(2),
            "Bottom pane (Never) should remain"
        );
    }

    // ---- Swap layout tests ----

    /// Helper: create a tab with N panes in a horizontal split chain.
    fn make_tab_with_n_panes(n: usize) -> (Tab, TerminalSize) {
        let size = TerminalSize {
            rows: 24,
            cols: 400, // Wide enough for up to 8 panes with separators
            pixel_width: 4000,
            pixel_height: 600,
            dpi: 96,
        };
        ensure_mux_initialized();
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        for i in 2..=n {
            // Split the last pane (right-most) to avoid shrinking pane 0 too much.
            let last_idx = i - 2; // index of the last leaf
            let split = tab
                .compute_split_size(
                    last_idx,
                    SplitRequest {
                        direction: SplitDirection::Horizontal,
                        ..Default::default()
                    },
                )
                .unwrap();
            tab.split_and_insert(
                last_idx,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new(i as PaneId, split.second),
            )
            .unwrap();
        }
        (tab, size)
    }

    #[test]
    fn swap_layout_preserves_all_panes() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(4);
        let pane_ids_before: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        assert_eq!(pane_ids_before.len(), 4);

        tab.set_layout_cycle(default_cycle());

        // Swap to main-side (3 slots, 4 panes → 1 stacked)
        let name = tab.swap_to_next_layout().unwrap();
        assert_eq!(name, "main-side");

        // All pane IDs should still be present (tree + stacks).
        let tree_ids: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        let stacked_ids: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        let all_ids: HashSet<PaneId> = tree_ids.union(&stacked_ids).copied().collect();
        assert_eq!(
            pane_ids_before, all_ids,
            "No panes should be lost during layout swap"
        );
    }

    #[test]
    fn swap_layout_cycle_wraps() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(2);
        tab.set_layout_cycle(default_cycle());

        // Cycle through all layouts and back to start.
        let n1 = tab.swap_to_next_layout().unwrap(); // main-side
        let n2 = tab.swap_to_next_layout().unwrap(); // stacked
        let n3 = tab.swap_to_next_layout().unwrap(); // main-bottom
        let n4 = tab.swap_to_next_layout().unwrap(); // grid-4 (wraps)
        assert_eq!(n1, "main-side");
        assert_eq!(n2, "stacked");
        assert_eq!(n3, "main-bottom");
        assert_eq!(n4, "grid-4");
    }

    #[test]
    fn swap_to_stacked_puts_all_panes_in_stack() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);
        tab.set_layout_cycle(default_cycle());

        // Advance to "stacked" layout (index 2).
        tab.swap_to_layout_index(2);
        let name = tab.current_layout_name().unwrap();
        assert_eq!(name, "stacked");

        // Stacked layout has 1 slot → 2 overflow panes stacked.
        let tree_panes = tab.iter_panes_ignoring_zoom();
        assert_eq!(tree_panes.len(), 1, "Stacked layout should show 1 leaf");
        assert!(
            tab.stack_count() > 0,
            "Should have at least one stack for overflow panes"
        );
    }

    #[test]
    fn swap_layout_focus_preserved() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);

        // Find pane 2 and set it as active.
        let pane_2 = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == 2)
            .unwrap()
            .pane
            .clone();
        tab.set_active_pane(&pane_2);
        let active_before = tab.get_active_pane().unwrap().pane_id();
        assert_eq!(active_before, 2);

        tab.set_layout_cycle(default_cycle());
        tab.swap_to_next_layout(); // main-side: has main slot

        // Active pane should still be pane 2 (placed in main slot).
        let active_after = tab.get_active_pane().unwrap().pane_id();
        assert_eq!(
            active_after, active_before,
            "Focus should be preserved across layout swap"
        );
    }

    #[test]
    fn swap_layout_roundtrip_restores_pane_set() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(4);
        let ids_before: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();

        tab.set_layout_cycle(default_cycle());

        // Swap forward through entire cycle and back.
        for _ in 0..4 {
            tab.swap_to_next_layout();
        }

        // Verify all panes present.
        let tree_ids: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        let stacked_ids: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        let all_ids: HashSet<PaneId> = tree_ids.union(&stacked_ids).copied().collect();
        assert_eq!(
            ids_before, all_ids,
            "Full cycle swap should preserve all panes"
        );
    }

    #[test]
    fn cycle_stack_switches_visible_pane() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);
        tab.set_layout_cycle(default_cycle());

        // Switch to stacked layout (all 3 panes in 1 slot).
        tab.swap_to_layout_index(2);

        let visible_before = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();

        // Cycle the stack.
        let new_visible = tab.cycle_stack(0);
        if let Some(new_id) = new_visible {
            assert_ne!(
                new_id, visible_before,
                "Cycling stack should change visible pane"
            );
            // Verify new pane is now in the tree.
            let current = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
            assert_eq!(current, new_id);
        }
    }

    #[test]
    fn cycle_stack_backward_returns_to_previous_visible_pane() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);
        tab.set_layout_cycle(default_cycle());
        tab.swap_to_layout_index(2); // stacked layout

        let visible_before = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        let visible_after_forward = tab.cycle_stack(0).expect("forward cycle should succeed");
        assert_ne!(
            visible_after_forward, visible_before,
            "Forward cycle should change visible pane"
        );

        let visible_after_backward = tab
            .cycle_stack_backward(0)
            .expect("backward cycle should succeed");
        assert_eq!(
            visible_after_backward, visible_before,
            "Backward cycle should return to the previously visible pane"
        );
        let current = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        assert_eq!(current, visible_before);
    }

    #[test]
    fn cycle_stack_backward_single_pane_stack_returns_none() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(1);
        tab.set_layout_cycle(default_cycle());
        tab.swap_to_layout_index(2); // stacked layout with one pane

        let visible_before = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        assert!(
            tab.cycle_stack_backward(0).is_none(),
            "Single-pane stack should not cycle backward"
        );
        let current = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        assert_eq!(current, visible_before);
    }

    #[test]
    fn cycle_stack_backward_invalid_slot_returns_none() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);
        tab.set_layout_cycle(default_cycle());
        tab.swap_to_layout_index(2); // stacked layout in slot 0

        let visible_before = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        assert!(
            tab.cycle_stack_backward(999).is_none(),
            "Unknown stack slot should return None"
        );
        let current = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        assert_eq!(current, visible_before);
    }

    #[test]
    fn first_nontrivial_stack_slot_index_identifies_cycleable_stack() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(5);
        tab.set_layout_cycle(default_cycle());

        let mut layout_index = 0usize;
        let slot_index = loop {
            if tab.swap_to_layout_index(layout_index).is_none() {
                panic!("expected at least one layout with a non-trivial pane stack");
            }
            if let Some(slot) = tab.first_nontrivial_stack_slot_index() {
                break slot;
            }
            layout_index += 1;
        };

        let visible_before: Vec<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();

        tab.cycle_stack(slot_index)
            .expect("forward cycle should succeed");
        let visible_after_forward: Vec<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        assert_ne!(
            visible_after_forward, visible_before,
            "forward cycle should change visible tree panes"
        );

        tab.cycle_stack_backward(slot_index)
            .expect("backward cycle should succeed");
        let visible_after_backward: Vec<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        assert_eq!(
            visible_after_backward, visible_before,
            "backward cycle should restore original visible tree panes"
        );
    }

    #[test]
    fn swap_without_cycle_returns_none() {
        let (tab, _size) = make_tab_with_n_panes(2);
        assert!(
            tab.swap_to_next_layout().is_none(),
            "Should return None when no cycle is set"
        );
        assert!(tab.current_layout_name().is_none());
    }

    #[test]
    fn layout_swap_with_single_pane() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(1);
        tab.set_layout_cycle(default_cycle());

        // Swap to grid-4 (4 slots, but only 1 pane).
        tab.swap_to_next_layout();
        let panes = tab.iter_panes_ignoring_zoom();
        // Should have the pane somewhere in the tree.
        let has_pane_1 = panes.iter().any(|p| p.pane.pane_id() == 1);
        assert!(has_pane_1, "Single pane should be placed in the layout");
    }

    // ---- Proptest: swap layout invariants ----

    proptest! {
        /// Swapping through any number of layouts never loses panes.
        #[test]
        fn swap_layout_never_loses_panes(
            num_panes in 1usize..8,
            num_swaps in 1usize..12,
        ) {
            use crate::layout::default_cycle;

            let (tab, _size) = make_tab_with_n_panes(num_panes);
            let ids_before: HashSet<PaneId> = tab
                .iter_panes_ignoring_zoom()
                .iter()
                .map(|p| p.pane.pane_id())
                .collect();

            tab.set_layout_cycle(default_cycle());

            for _ in 0..num_swaps {
                tab.swap_to_next_layout();
            }

            let tree_ids: HashSet<PaneId> = tab
                .iter_panes_ignoring_zoom()
                .iter()
                .map(|p| p.pane.pane_id())
                .collect();
            let stacked_ids: HashSet<PaneId> =
                tab.all_stacked_pane_ids().into_iter().collect();
            let all_ids: HashSet<PaneId> =
                tree_ids.union(&stacked_ids).copied().collect();

            prop_assert_eq!(
                ids_before.len(),
                all_ids.len(),
                "pane count mismatch: before={}, after={}",
                ids_before.len(),
                all_ids.len()
            );
            for id in &ids_before {
                prop_assert!(
                    all_ids.contains(id),
                    "pane {} lost during swap",
                    id
                );
            }
        }

        /// Focus is always on a valid pane after any sequence of swaps.
        #[test]
        fn swap_layout_focus_always_valid(
            num_panes in 1usize..6,
            num_swaps in 1usize..8,
        ) {
            use crate::layout::default_cycle;

            let (tab, _size) = make_tab_with_n_panes(num_panes);
            tab.set_layout_cycle(default_cycle());

            for _ in 0..num_swaps {
                tab.swap_to_next_layout();
            }

            let active = tab.get_active_pane();
            prop_assert!(
                active.is_some(),
                "Active pane should never be None after swap"
            );
        }
    }

    // ---- FrankenMux integration tests (ft-2dd4s.5) ----

    /// Integration test: floating panes + swap layouts + constraints
    /// all work together without interfering.
    #[test]
    fn frankenmux_integration_floating_and_swap() {
        use crate::layout::default_cycle;

        let size = TerminalSize {
            rows: 40,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 1000,
            dpi: 96,
        };
        ensure_mux_initialized();

        let tab = Tab::new(&size);

        // Create 3 tiled panes.
        tab.assign_pane(&FakePane::new(1, size));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new(2, split.second),
        )
        .unwrap();
        let split2 = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Vertical,
                ..Default::default()
            },
            FakePane::new(3, split2.second),
        )
        .unwrap();

        // Add a floating pane.
        let float_pane = FakePane::new(
            10,
            TerminalSize {
                rows: 10,
                cols: 40,
                pixel_width: 400,
                pixel_height: 250,
                dpi: 96,
            },
        );
        tab.add_floating_pane(
            float_pane.clone(),
            FloatingPaneRect {
                left: 20,
                top: 5,
                width: 40,
                height: 10,
            },
        );

        // Verify initial state: 3 tiled + 1 floating.
        let tiled = tab.iter_panes_ignoring_zoom();
        assert_eq!(tiled.len(), 3, "Should have 3 tiled panes");
        let floating = tab.iter_floating_panes();
        assert_eq!(floating.len(), 1, "Should have 1 floating pane");

        // Now swap layouts — this should only affect tiled panes, not floating.
        tab.set_layout_cycle(default_cycle());
        tab.swap_to_next_layout(); // main-side

        // Floating pane should still be there.
        let floating_after = tab.iter_floating_panes();
        assert_eq!(
            floating_after.len(),
            1,
            "Floating pane should survive layout swap"
        );
        assert_eq!(floating_after[0].pane_id, 10);

        // All 3 tiled panes should still exist (in tree + stacks).
        let tree_ids: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        let stacked_ids: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        let all_tiled: HashSet<PaneId> = tree_ids.union(&stacked_ids).copied().collect();
        assert!(all_tiled.contains(&1));
        assert!(all_tiled.contains(&2));
        assert!(all_tiled.contains(&3));

        // Swap to stacked layout.
        tab.swap_to_layout_index(2);
        assert_eq!(tab.current_layout_name().unwrap(), "stacked");

        // Still 1 floating pane.
        assert_eq!(tab.iter_floating_panes().len(), 1);

        // Swap back to grid-4.
        tab.swap_to_layout_index(0);

        // All tiled panes still present.
        let tree_ids: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        let stacked_ids: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        let all_final: HashSet<PaneId> = tree_ids.union(&stacked_ids).copied().collect();
        assert_eq!(all_final.len(), 3);
    }

    /// Integration test: constraint-based resize works after layout swap.
    #[test]
    fn frankenmux_integration_constraints_after_swap() {
        use crate::layout::default_cycle;

        let size = TerminalSize {
            rows: 40,
            cols: 200,
            pixel_width: 2000,
            pixel_height: 1000,
            dpi: 96,
        };
        ensure_mux_initialized();

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 20,
                min_height: 10,
                ..PaneConstraints::default()
            },
            CollapsePriority::Low,
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split.second,
                PaneConstraints {
                    min_width: 20,
                    min_height: 10,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Never,
            ),
        )
        .unwrap();

        // Set layout cycle and swap.
        tab.set_layout_cycle(default_cycle());
        tab.swap_to_next_layout(); // main-side

        // Now resize the tab smaller — constraints should still work.
        let small = TerminalSize {
            rows: 40,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 1000,
            dpi: 96,
        };
        tab.resize(small);

        // Tab should not crash and panes should still exist.
        let panes = tab.iter_panes_ignoring_zoom();
        assert!(
            !panes.is_empty(),
            "Tab should have panes after resize with constraints"
        );
    }

    /// Integration test: zoom interacts correctly with layout swap.
    #[test]
    fn frankenmux_integration_zoom_and_swap() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);

        // Zoom a pane.
        tab.set_zoomed(true);

        // Set layout cycle.
        tab.set_layout_cycle(default_cycle());

        // Swap layout while zoomed — should still work.
        let name = tab.swap_to_next_layout();
        assert!(name.is_some(), "Swap should work even when zoomed");

        // All panes should be accounted for.
        let tree_ids: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        let stacked_ids: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        let all: HashSet<PaneId> = tree_ids.union(&stacked_ids).copied().collect();
        assert_eq!(all.len(), 3, "All 3 panes should survive zoom + swap");
    }

    #[test]
    fn swap_layout_pane_count_mismatch_overflow_stacks() {
        use crate::layout::{default_cycle, grid_4};

        // Create 6 panes, then swap to grid-4 (4 slots) — 2 extras must be stacked.
        let (tab, _size) = make_tab_with_n_panes(6);
        tab.set_layout_cycle(default_cycle());

        // grid-4 is the default (index 0).
        tab.swap_to_layout_index(0);
        let name = tab.current_layout_name().unwrap();
        assert_eq!(name, "grid-4");

        let tree_panes: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        let stacked_panes: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        let total = tree_panes.len() + stacked_panes.len();

        assert_eq!(total, 6, "All 6 panes must survive (tree + stacked)");
        assert_eq!(
            tree_panes.len(),
            grid_4().arrangement.slot_count(),
            "Tree should have exactly as many leaves as grid-4 slots"
        );
        assert_eq!(
            stacked_panes.len(),
            2,
            "2 overflow panes should be stacked in the last slot"
        );
    }

    #[test]
    fn collapse_priority_default_is_normal() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let pane = FakePane::new(1, size);
        assert_eq!(
            pane.collapse_priority(),
            CollapsePriority::Normal,
            "Default collapse priority should be Normal"
        );
    }

    #[test]
    fn floating_pane_focus_cycle_through_multiple() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 30,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 750,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        // Add 3 floating panes.
        for id in [10, 20, 30] {
            tab.add_floating_pane(
                FakePane::new(id, size),
                FloatingPaneRect {
                    left: id * 2,
                    top: id,
                    width: 20,
                    height: 10,
                },
            );
        }

        // Last added (30) should be focused.
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 30);

        // Cycle focus: 30 → 10 → 20 → 30
        assert!(tab.set_floating_pane_focus(10));
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 10);

        assert!(tab.set_floating_pane_focus(20));
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 20);

        assert!(tab.set_floating_pane_focus(30));
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 30);

        // Non-existent pane returns false.
        assert!(!tab.set_floating_pane_focus(999));
    }
}
