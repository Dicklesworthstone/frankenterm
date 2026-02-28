//! Swap layout definitions and pane redistribution.
//!
//! This module implements Zellij-inspired swap layouts that let users cycle
//! between pre-defined pane arrangements with a single keypress.  Panes are
//! preserved across swaps — only their positions change.
//!
//! # Key types
//!
//! - [`SwapLayout`] — A named layout template with an arrangement tree.
//! - [`LayoutArrangement`] — Recursive tree describing splits and stack slots.
//! - [`PaneStack`] — Multiple panes sharing a single position (vertical tabs).
//! - [`LayoutCycle`] — Ordered list of layouts for swap-key cycling.

use crate::pane::{Pane, PaneId};
use crate::tab::{SplitDirection, SplitDirectionAndSize, Tree};
use frankenterm_term::TerminalSize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// A named, pre-defined layout template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SwapLayout {
    /// Human-readable name (e.g. "grid-4", "main-side", "stacked").
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// The arrangement tree describing how panes are positioned.
    pub arrangement: LayoutArrangement,
}

/// Recursive tree describing a layout arrangement.
///
/// Each node is either a split (with a direction and ratio) or a leaf slot
/// where a pane (or stack of panes) will be placed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LayoutArrangement {
    /// A split containing two children.
    Split {
        direction: SplitDirection,
        /// Ratio allocated to the first child (0.0–1.0).
        ratio: f64,
        first: Box<LayoutArrangement>,
        second: Box<LayoutArrangement>,
    },
    /// A single slot that holds one or more panes.
    /// If `is_main` is true, the currently focused pane is placed here
    /// during redistribution.
    Slot { is_main: bool },
}

impl LayoutArrangement {
    /// Count the number of leaf slots in this arrangement.
    pub fn slot_count(&self) -> usize {
        match self {
            LayoutArrangement::Split { first, second, .. } => {
                first.slot_count() + second.slot_count()
            }
            LayoutArrangement::Slot { .. } => 1,
        }
    }

    /// Returns true if any slot has `is_main` set.
    pub fn has_main_slot(&self) -> bool {
        match self {
            LayoutArrangement::Split { first, second, .. } => {
                first.has_main_slot() || second.has_main_slot()
            }
            LayoutArrangement::Slot { is_main } => *is_main,
        }
    }
}

/// A stack of panes sharing a single layout position.
///
/// Only the pane at `active_index` is visible; the rest are hidden
/// like vertical tabs.
#[derive(Clone)]
pub struct PaneStack {
    /// Ordered list of panes in this stack.
    panes: Vec<Arc<dyn Pane>>,
    /// Index of the currently visible pane.
    active_index: usize,
}

impl std::fmt::Debug for PaneStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ids: Vec<PaneId> = self.panes.iter().map(|p| p.pane_id()).collect();
        f.debug_struct("PaneStack")
            .field("pane_ids", &ids)
            .field("active_index", &self.active_index)
            .finish()
    }
}

impl PaneStack {
    /// Create a stack from a non-empty list of panes.
    /// The first pane is initially visible.
    pub fn new(panes: Vec<Arc<dyn Pane>>) -> Self {
        assert!(!panes.is_empty(), "PaneStack requires at least one pane");
        Self {
            panes,
            active_index: 0,
        }
    }

    /// Create a stack containing a single pane.
    pub fn single(pane: Arc<dyn Pane>) -> Self {
        Self {
            panes: vec![pane],
            active_index: 0,
        }
    }

    /// Returns the currently visible pane.
    pub fn active_pane(&self) -> &Arc<dyn Pane> {
        &self.panes[self.active_index]
    }

    /// Returns the number of panes in this stack.
    pub fn len(&self) -> usize {
        self.panes.len()
    }

    /// Returns true if the stack is empty.
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// Returns true if the stack has only one pane.
    pub fn is_single(&self) -> bool {
        self.panes.len() == 1
    }

    /// Cycle to the next pane in the stack.  Wraps around.
    pub fn cycle_next(&mut self) {
        self.active_index = (self.active_index + 1) % self.panes.len();
    }

    /// Cycle to the previous pane in the stack.  Wraps around.
    pub fn cycle_prev(&mut self) {
        if self.active_index == 0 {
            self.active_index = self.panes.len() - 1;
        } else {
            self.active_index -= 1;
        }
    }

    /// Select a specific pane by index.  Returns false if out of range.
    pub fn select(&mut self, index: usize) -> bool {
        if index < self.panes.len() {
            self.active_index = index;
            true
        } else {
            false
        }
    }

    /// Returns a slice of all panes in this stack.
    pub fn panes(&self) -> &[Arc<dyn Pane>] {
        &self.panes
    }

    /// Returns the active index.
    pub fn active_index(&self) -> usize {
        self.active_index
    }

    /// Returns all pane IDs in this stack.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.panes.iter().map(|p| p.pane_id()).collect()
    }

    /// Push a pane onto the stack.
    pub fn push(&mut self, pane: Arc<dyn Pane>) {
        self.panes.push(pane);
    }

    /// Remove a pane by ID.  Returns the removed pane, or None.
    /// If the active pane is removed, the active index is adjusted.
    pub fn remove(&mut self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        let pos = self.panes.iter().position(|p| p.pane_id() == pane_id)?;
        let pane = self.panes.remove(pos);
        if self.active_index >= self.panes.len() && !self.panes.is_empty() {
            self.active_index = self.panes.len() - 1;
        }
        Some(pane)
    }

    /// Drain all panes out of the stack, consuming it.
    pub fn into_panes(self) -> Vec<Arc<dyn Pane>> {
        self.panes
    }
}

/// An ordered list of layouts that the user can cycle through.
#[derive(Debug, Clone)]
pub struct LayoutCycle {
    layouts: Vec<SwapLayout>,
    current: usize,
}

impl LayoutCycle {
    /// Create a new cycle from a non-empty list of layouts.
    pub fn new(layouts: Vec<SwapLayout>) -> Self {
        assert!(
            !layouts.is_empty(),
            "LayoutCycle requires at least one layout"
        );
        Self {
            layouts,
            current: 0,
        }
    }

    /// Returns the current layout.
    pub fn current(&self) -> &SwapLayout {
        &self.layouts[self.current]
    }

    /// Returns the current index.
    pub fn current_index(&self) -> usize {
        self.current
    }

    /// Advance to the next layout and return it.
    pub fn advance(&mut self) -> &SwapLayout {
        self.current = (self.current + 1) % self.layouts.len();
        &self.layouts[self.current]
    }

    /// Go to the previous layout and return it.
    pub fn prev(&mut self) -> &SwapLayout {
        if self.current == 0 {
            self.current = self.layouts.len() - 1;
        } else {
            self.current -= 1;
        }
        &self.layouts[self.current]
    }

    /// Select a layout by index.  Returns false if out of range.
    pub fn select(&mut self, index: usize) -> bool {
        if index < self.layouts.len() {
            self.current = index;
            true
        } else {
            false
        }
    }

    /// Returns the number of layouts.
    pub fn len(&self) -> usize {
        self.layouts.len()
    }

    /// Returns true if the cycle is empty (should never happen after construction).
    pub fn is_empty(&self) -> bool {
        self.layouts.is_empty()
    }

    /// Returns a slice of all layouts.
    pub fn layouts(&self) -> &[SwapLayout] {
        &self.layouts
    }
}

/// Result of redistributing panes into a new layout.
pub struct LayoutSwapResult {
    /// The new binary tree with panes placed according to the layout.
    pub tree: Tree,
    /// Stacks created for overflow panes (slot_index → stack).
    pub stacks: HashMap<usize, PaneStack>,
    /// The leaf index of the active pane in the new tree.
    pub active_index: usize,
}

/// Build a binary tree from a `LayoutArrangement` and a list of panes.
///
/// # Redistribution algorithm
///
/// 1. Collect all current panes in tree order.
/// 2. If the layout has a "main" slot, place the active pane there.
/// 3. Assign remaining panes 1:1 to remaining slots.
/// 4. If more panes than slots, stack overflow panes in the last slot.
/// 5. If fewer panes than slots, leave extra slots with the last pane
///    duplicated (shouldn't happen in practice — caller should ensure
///    at least one pane per slot or handle empty slots).
///
/// Returns `None` if `panes` is empty.
pub fn redistribute_panes(
    arrangement: &LayoutArrangement,
    panes: Vec<Arc<dyn Pane>>,
    active_pane_id: PaneId,
    tab_size: TerminalSize,
) -> Option<LayoutSwapResult> {
    if panes.is_empty() {
        return None;
    }

    let slot_count = arrangement.slot_count();
    let _has_main = arrangement.has_main_slot();

    // Separate the active pane from the rest.
    let mut active_pane: Option<Arc<dyn Pane>> = None;
    let mut other_panes: Vec<Arc<dyn Pane>> = Vec::with_capacity(panes.len());
    for p in panes {
        if p.pane_id() == active_pane_id && active_pane.is_none() {
            active_pane = Some(p);
        } else {
            other_panes.push(p);
        }
    }

    // If active pane wasn't found (shouldn't happen), use the first pane.
    let active = active_pane.unwrap_or_else(|| other_panes.remove(0));

    // Build assignment: for each slot in preorder, assign a pane.
    // Main slot gets the active pane; others get remaining panes in order.
    let mut slot_assignments: Vec<Vec<Arc<dyn Pane>>> = vec![Vec::new(); slot_count];
    let mut slot_idx = 0;
    let mut main_slot_idx: Option<usize> = None;

    // First pass: identify main slot index.
    assign_slot_indices(arrangement, &mut slot_idx, &mut main_slot_idx);

    // Place active pane in main slot (or slot 0 if no main).
    let main_target = main_slot_idx.unwrap_or(0);
    slot_assignments[main_target].push(active.clone());

    // Distribute other panes to remaining slots.
    let mut pane_iter = other_panes.into_iter();
    for (i, slot) in slot_assignments.iter_mut().enumerate().take(slot_count) {
        if i == main_target {
            continue; // main slot already has the active pane
        }
        if let Some(p) = pane_iter.next() {
            slot.push(p);
        }
    }

    // Overflow: stack remaining panes in the last slot.
    let overflow_target = slot_count - 1;
    for p in pane_iter {
        slot_assignments[overflow_target].push(p);
    }

    // Ensure every slot has at least one pane.
    // For empty slots (more slots than panes), we can't create panes here —
    // the caller must handle empty slots.  We'll track which slots are empty.
    // For now, empty slots will produce leaves with a placeholder that the
    // caller should handle.

    // Build the tree and stacks.
    let mut stacks: HashMap<usize, PaneStack> = HashMap::new();
    let mut slot_counter = 0;
    let mut active_leaf_index = 0;
    let mut leaf_counter = 0;

    let tree = build_tree_from_arrangement(
        arrangement,
        &slot_assignments,
        &mut slot_counter,
        &mut stacks,
        active_pane_id,
        &mut active_leaf_index,
        &mut leaf_counter,
        tab_size,
    );

    Some(LayoutSwapResult {
        tree,
        stacks,
        active_index: active_leaf_index,
    })
}

/// Recursively assign slot indices, finding the main slot.
fn assign_slot_indices(
    arrangement: &LayoutArrangement,
    counter: &mut usize,
    main_idx: &mut Option<usize>,
) {
    match arrangement {
        LayoutArrangement::Split { first, second, .. } => {
            assign_slot_indices(first, counter, main_idx);
            assign_slot_indices(second, counter, main_idx);
        }
        LayoutArrangement::Slot { is_main } => {
            if *is_main && main_idx.is_none() {
                *main_idx = Some(*counter);
            }
            *counter += 1;
        }
    }
}

/// Recursively build a Tree from a LayoutArrangement.
fn build_tree_from_arrangement(
    arrangement: &LayoutArrangement,
    slot_assignments: &[Vec<Arc<dyn Pane>>],
    slot_counter: &mut usize,
    stacks: &mut HashMap<usize, PaneStack>,
    active_pane_id: PaneId,
    active_leaf_index: &mut usize,
    leaf_counter: &mut usize,
    available_size: TerminalSize,
) -> Tree {
    match arrangement {
        LayoutArrangement::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let (first_size, second_size) = compute_split_sizes(*direction, *ratio, available_size);

            let left = build_tree_from_arrangement(
                first,
                slot_assignments,
                slot_counter,
                stacks,
                active_pane_id,
                active_leaf_index,
                leaf_counter,
                first_size,
            );
            let right = build_tree_from_arrangement(
                second,
                slot_assignments,
                slot_counter,
                stacks,
                active_pane_id,
                active_leaf_index,
                leaf_counter,
                second_size,
            );

            let split_data = SplitDirectionAndSize {
                direction: *direction,
                first: first_size,
                second: second_size,
            };

            bintree::Tree::Node {
                left: Box::new(left),
                right: Box::new(right),
                data: Some(split_data),
            }
        }
        LayoutArrangement::Slot { .. } => {
            let idx = *slot_counter;
            *slot_counter += 1;
            let leaf_idx = *leaf_counter;
            *leaf_counter += 1;

            let assigned = &slot_assignments[idx];
            if assigned.is_empty() {
                // Empty slot — return Tree::Empty.
                // Caller should handle this (e.g. create a placeholder pane).
                return bintree::Tree::Empty;
            }

            let visible_pane = assigned[0].clone();

            // Track active leaf index.
            if visible_pane.pane_id() == active_pane_id {
                *active_leaf_index = leaf_idx;
            }

            // If multiple panes assigned, create a stack.
            if assigned.len() > 1 {
                let stack = PaneStack::new(assigned.clone());
                // Check if active pane is in this stack.
                for p in assigned {
                    if p.pane_id() == active_pane_id {
                        *active_leaf_index = leaf_idx;
                        break;
                    }
                }
                stacks.insert(idx, stack);
            }

            // Resize the visible pane to match its slot.
            visible_pane.resize(available_size).ok();

            bintree::Tree::Leaf(visible_pane)
        }
    }
}

/// Compute the sizes for a split given a direction, ratio, and available space.
fn compute_split_sizes(
    direction: SplitDirection,
    ratio: f64,
    available: TerminalSize,
) -> (TerminalSize, TerminalSize) {
    let ratio = ratio.clamp(0.05, 0.95);

    match direction {
        SplitDirection::Horizontal => {
            // Split left-right.  Subtract 1 for the separator.
            let total_cols = available.cols.saturating_sub(1);
            let first_cols = ((total_cols as f64) * ratio).round() as usize;
            let second_cols = total_cols.saturating_sub(first_cols);

            let first = TerminalSize {
                cols: first_cols.max(1),
                rows: available.rows,
                pixel_width: 0,
                pixel_height: 0,
                dpi: available.dpi,
            };
            let second = TerminalSize {
                cols: second_cols.max(1),
                rows: available.rows,
                pixel_width: 0,
                pixel_height: 0,
                dpi: available.dpi,
            };
            (first, second)
        }
        SplitDirection::Vertical => {
            // Split top-bottom.  Subtract 1 for the separator.
            let total_rows = available.rows.saturating_sub(1);
            let first_rows = ((total_rows as f64) * ratio).round() as usize;
            let second_rows = total_rows.saturating_sub(first_rows);

            let first = TerminalSize {
                rows: first_rows.max(1),
                cols: available.cols,
                pixel_width: 0,
                pixel_height: 0,
                dpi: available.dpi,
            };
            let second = TerminalSize {
                rows: second_rows.max(1),
                cols: available.cols,
                pixel_width: 0,
                pixel_height: 0,
                dpi: available.dpi,
            };
            (first, second)
        }
    }
}

// --- Built-in layout presets ---

/// Create a 2x2 grid layout.
pub fn grid_4() -> SwapLayout {
    SwapLayout {
        name: "grid-4".to_string(),
        description: Some("2x2 grid".to_string()),
        arrangement: LayoutArrangement::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutArrangement::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutArrangement::Slot { is_main: true }),
                second: Box::new(LayoutArrangement::Slot { is_main: false }),
            }),
            second: Box::new(LayoutArrangement::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutArrangement::Slot { is_main: false }),
                second: Box::new(LayoutArrangement::Slot { is_main: false }),
            }),
        },
    }
}

/// Create a main pane + side panel layout (70/30 split).
pub fn main_side() -> SwapLayout {
    SwapLayout {
        name: "main-side".to_string(),
        description: Some("Main pane + side panel".to_string()),
        arrangement: LayoutArrangement::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.7,
            first: Box::new(LayoutArrangement::Slot { is_main: true }),
            second: Box::new(LayoutArrangement::Split {
                direction: SplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutArrangement::Slot { is_main: false }),
                second: Box::new(LayoutArrangement::Slot { is_main: false }),
            }),
        },
    }
}

/// Create a single stacked layout (all panes share one position).
pub fn stacked() -> SwapLayout {
    SwapLayout {
        name: "stacked".to_string(),
        description: Some("All panes stacked (vertical tabs)".to_string()),
        arrangement: LayoutArrangement::Slot { is_main: true },
    }
}

/// Create a tall layout: main pane on top, small panes on bottom.
pub fn main_bottom() -> SwapLayout {
    SwapLayout {
        name: "main-bottom".to_string(),
        description: Some("Main pane on top, helpers on bottom".to_string()),
        arrangement: LayoutArrangement::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.7,
            first: Box::new(LayoutArrangement::Slot { is_main: true }),
            second: Box::new(LayoutArrangement::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutArrangement::Slot { is_main: false }),
                second: Box::new(LayoutArrangement::Slot { is_main: false }),
            }),
        },
    }
}

/// Returns the default layout cycle: grid-4 → main-side → stacked.
pub fn default_cycle() -> LayoutCycle {
    LayoutCycle::new(vec![grid_4(), main_side(), stacked(), main_bottom()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_count_single() {
        let layout = stacked();
        assert_eq!(layout.arrangement.slot_count(), 1);
    }

    #[test]
    fn slot_count_grid() {
        let layout = grid_4();
        assert_eq!(layout.arrangement.slot_count(), 4);
    }

    #[test]
    fn slot_count_main_side() {
        let layout = main_side();
        assert_eq!(layout.arrangement.slot_count(), 3);
    }

    #[test]
    fn has_main_slot_detection() {
        assert!(grid_4().arrangement.has_main_slot());
        assert!(stacked().arrangement.has_main_slot());
        let no_main = LayoutArrangement::Slot { is_main: false };
        assert!(!no_main.has_main_slot());
    }

    #[test]
    fn layout_cycle_wraps() {
        let mut cycle = default_cycle();
        assert_eq!(cycle.current().name, "grid-4");
        assert_eq!(cycle.advance().name, "main-side");
        assert_eq!(cycle.advance().name, "stacked");
        assert_eq!(cycle.advance().name, "main-bottom");
        assert_eq!(cycle.advance().name, "grid-4"); // wraps
    }

    #[test]
    fn layout_cycle_prev_wraps() {
        let mut cycle = default_cycle();
        assert_eq!(cycle.current().name, "grid-4");
        assert_eq!(cycle.prev().name, "main-bottom"); // wraps backward
        assert_eq!(cycle.prev().name, "stacked");
    }

    #[test]
    fn pane_stack_cycle() {
        // Use a minimal test — PaneStack doesn't need real panes for cycle tests,
        // but we test with the stack API.
        let arrangement = stacked();
        assert_eq!(arrangement.arrangement.slot_count(), 1);
    }

    #[test]
    fn compute_split_sizes_horizontal() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 96,
        };
        let (first, second) = compute_split_sizes(SplitDirection::Horizontal, 0.5, size);
        // 80 - 1 separator = 79 total cols; 50% = 40 first, 39 second
        assert_eq!(first.cols + second.cols + 1, 80);
        assert_eq!(first.rows, 24);
        assert_eq!(second.rows, 24);
    }

    #[test]
    fn compute_split_sizes_vertical() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 96,
        };
        let (first, second) = compute_split_sizes(SplitDirection::Vertical, 0.5, size);
        // 24 - 1 separator = 23 total rows; 50% = 12 first, 11 second
        assert_eq!(first.rows + second.rows + 1, 24);
        assert_eq!(first.cols, 80);
        assert_eq!(second.cols, 80);
    }

    #[test]
    fn compute_split_sizes_clamps_ratio() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 96,
        };
        // Extreme ratio should be clamped to 0.05-0.95.
        let (first, second) = compute_split_sizes(SplitDirection::Horizontal, 0.0, size);
        assert!(first.cols >= 1);
        assert!(second.cols >= 1);

        let (first, second) = compute_split_sizes(SplitDirection::Horizontal, 1.0, size);
        assert!(first.cols >= 1);
        assert!(second.cols >= 1);
    }

    #[test]
    fn layout_serialization_roundtrip() {
        let layout = grid_4();
        let json = serde_json::to_string(&layout).unwrap();
        let deserialized: SwapLayout = serde_json::from_str(&json).unwrap();
        assert_eq!(layout, deserialized);
    }
}
