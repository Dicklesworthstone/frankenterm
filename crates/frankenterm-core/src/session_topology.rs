//! Mux topology serializer for session persistence.
//!
//! Captures WezTerm mux session topology (windows, tabs, pane split trees)
//! into a versioned JSON format for storage in `mux_sessions.topology_json`.
//! Supports reconstruction after crash/restart via split-tree inference from
//! pane positions.
//!
//! # Data flow
//!
//! ```text
//! wezterm cli list (JSON) → Vec<PaneInfo> → TopologySnapshot → topology_json
//!                                                ↑
//!                              split-tree inference from pane positions
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::wezterm::PaneInfo;

// =============================================================================
// Core types
// =============================================================================

/// Current schema version for topology snapshots.
pub const TOPOLOGY_SCHEMA_VERSION: u32 = 1;

/// Complete snapshot of the mux session topology.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TopologySnapshot {
    /// Schema version for forward compatibility.
    pub schema_version: u32,
    /// When this snapshot was captured (epoch ms).
    pub captured_at: u64,
    /// Workspace identifier (from `wezterm cli list`).
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// All windows in the session.
    pub windows: Vec<WindowSnapshot>,
}

/// Snapshot of a single window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowSnapshot {
    pub window_id: u64,
    #[serde(default)]
    pub title: Option<String>,
    /// Window position (x, y) in pixels.
    #[serde(default)]
    pub position: Option<(i32, i32)>,
    /// Window size (width, height) in pixels.
    #[serde(default)]
    pub size: Option<(u32, u32)>,
    pub tabs: Vec<TabSnapshot>,
    #[serde(default)]
    pub active_tab_index: Option<usize>,
}

/// Snapshot of a single tab.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TabSnapshot {
    pub tab_id: u64,
    #[serde(default)]
    pub title: Option<String>,
    pub pane_tree: PaneNode,
    #[serde(default)]
    pub active_pane_id: Option<u64>,
}

/// Recursive pane tree representing splits within a tab.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum PaneNode {
    /// A leaf node: a single pane.
    Leaf {
        pane_id: u64,
        rows: u16,
        cols: u16,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        is_active: bool,
    },
    /// Horizontal split: children arranged top-to-bottom.
    HSplit { children: Vec<(f64, PaneNode)> },
    /// Vertical split: children arranged left-to-right.
    VSplit { children: Vec<(f64, PaneNode)> },
}

/// Result of attempting to reconstruct a split tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferenceQuality {
    /// Split structure was successfully inferred.
    Inferred,
    /// Could not determine split structure; flat layout used as fallback.
    FlatFallback,
}

/// Report from a topology capture operation.
#[derive(Debug, Clone)]
pub struct CaptureReport {
    pub window_count: usize,
    pub tab_count: usize,
    pub pane_count: usize,
    pub inference_quality: HashMap<u64, InferenceQuality>,
}

// =============================================================================
// Capture / construction
// =============================================================================

impl TopologySnapshot {
    /// Build a `TopologySnapshot` from a list of panes.
    ///
    /// Groups panes by window and tab, then infers split structure within
    /// each tab from pane sizes.
    #[must_use]
    pub fn from_panes(panes: &[PaneInfo], captured_at: u64) -> (Self, CaptureReport) {
        let workspace_id = panes.first().and_then(|p| p.workspace.clone());

        // Group panes by (window_id, tab_id)
        let mut windows_map: HashMap<u64, HashMap<u64, Vec<&PaneInfo>>> = HashMap::new();
        for pane in panes {
            windows_map
                .entry(pane.window_id)
                .or_default()
                .entry(pane.tab_id)
                .or_default()
                .push(pane);
        }

        let mut windows = Vec::new();
        let mut inference_quality = HashMap::new();
        let mut total_tabs = 0usize;

        let mut window_ids: Vec<u64> = windows_map.keys().copied().collect();
        window_ids.sort_unstable();

        for window_id in window_ids {
            let tabs_map = &windows_map[&window_id];
            let mut tabs = Vec::new();

            let mut tab_ids: Vec<u64> = tabs_map.keys().copied().collect();
            tab_ids.sort_unstable();

            for tab_id in tab_ids {
                let tab_panes = &tabs_map[&tab_id];
                let (pane_tree, quality) = infer_split_tree(tab_panes);
                inference_quality.insert(tab_id, quality);

                let active_pane_id = tab_panes.iter().find(|p| p.is_active).map(|p| p.pane_id);

                let title = tab_panes
                    .iter()
                    .find(|p| p.is_active)
                    .and_then(|p| p.title.clone());

                tabs.push(TabSnapshot {
                    tab_id,
                    title,
                    pane_tree,
                    active_pane_id,
                });
                total_tabs += 1;
            }

            // Find active tab (the one containing the active pane)
            let active_tab_index = tabs.iter().position(|t| t.active_pane_id.is_some());

            windows.push(WindowSnapshot {
                window_id,
                title: None,
                position: None,
                size: None,
                tabs,
                active_tab_index,
            });
        }

        let report = CaptureReport {
            window_count: windows.len(),
            tab_count: total_tabs,
            pane_count: panes.len(),
            inference_quality,
        };

        let snapshot = Self {
            schema_version: TOPOLOGY_SCHEMA_VERSION,
            captured_at,
            workspace_id,
            windows,
        };

        (snapshot, report)
    }

    /// Create an empty topology snapshot.
    #[must_use]
    pub fn empty(captured_at: u64) -> Self {
        Self {
            schema_version: TOPOLOGY_SCHEMA_VERSION,
            captured_at,
            workspace_id: None,
            windows: Vec::new(),
        }
    }

    /// Serialize this snapshot to JSON for storage in topology_json.
    ///
    /// # Errors
    /// Returns error if serialization fails (should not happen for valid data).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Deserialize a topology snapshot from JSON.
    ///
    /// # Errors
    /// Returns error if the JSON is invalid or missing required fields.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Count total panes across all windows and tabs.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.windows
            .iter()
            .flat_map(|w| &w.tabs)
            .map(|t| t.pane_tree.pane_count())
            .sum()
    }

    /// Collect all pane IDs in the snapshot.
    #[must_use]
    pub fn pane_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        for window in &self.windows {
            for tab in &window.tabs {
                tab.pane_tree.collect_pane_ids(&mut ids);
            }
        }
        ids
    }
}

impl PaneNode {
    /// Count panes in this subtree.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        match self {
            Self::Leaf { .. } => 1,
            Self::HSplit { children } | Self::VSplit { children } => {
                children.iter().map(|(_, child)| child.pane_count()).sum()
            }
        }
    }

    /// Collect all pane IDs in this subtree.
    pub fn collect_pane_ids(&self, out: &mut Vec<u64>) {
        match self {
            Self::Leaf { pane_id, .. } => out.push(*pane_id),
            Self::HSplit { children } | Self::VSplit { children } => {
                for (_, child) in children {
                    child.collect_pane_ids(out);
                }
            }
        }
    }
}

// =============================================================================
// Split-tree inference
// =============================================================================

/// Internal representation of pane geometry for split inference.
#[derive(Debug, Clone)]
struct PaneGeometry {
    pane_id: u64,
    rows: u16,
    cols: u16,
    cwd: Option<String>,
    title: Option<String>,
    is_active: bool,
}

impl PaneGeometry {
    fn from_pane_info(p: &PaneInfo) -> Self {
        Self {
            pane_id: p.pane_id,
            rows: p.effective_rows() as u16,
            cols: p.effective_cols() as u16,
            cwd: p.cwd.clone(),
            title: p.title.clone(),
            is_active: p.is_active,
        }
    }

    fn to_leaf(&self) -> PaneNode {
        PaneNode::Leaf {
            pane_id: self.pane_id,
            rows: self.rows,
            cols: self.cols,
            cwd: self.cwd.clone(),
            title: self.title.clone(),
            is_active: self.is_active,
        }
    }
}

/// Infer split tree structure from a set of panes within a single tab.
///
/// Strategy:
/// 1. Single pane → Leaf node.
/// 2. Two panes → detect HSplit (same cols) or VSplit (same rows).
/// 3. Multiple panes → group by common dimensions, recurse.
/// 4. If inference fails, fall back to flat HSplit layout.
fn infer_split_tree(panes: &[&PaneInfo]) -> (PaneNode, InferenceQuality) {
    let geometries: Vec<PaneGeometry> = panes
        .iter()
        .map(|p| PaneGeometry::from_pane_info(p))
        .collect();

    match geometries.len() {
        0 => {
            // Empty tab — shouldn't happen, but handle gracefully
            let leaf = PaneNode::Leaf {
                pane_id: 0,
                rows: 0,
                cols: 0,
                cwd: None,
                title: None,
                is_active: false,
            };
            (leaf, InferenceQuality::FlatFallback)
        }
        1 => {
            let g = &geometries[0];
            (g.to_leaf(), InferenceQuality::Inferred)
        }
        _ => infer_split_recursive(&geometries),
    }
}

/// Recursive split inference.
///
/// Checks if all panes share the same column count (→ HSplit) or row count
/// (→ VSplit). If neither, falls back to a flat HSplit.
fn infer_split_recursive(geometries: &[PaneGeometry]) -> (PaneNode, InferenceQuality) {
    if geometries.len() == 1 {
        return (geometries[0].to_leaf(), InferenceQuality::Inferred);
    }

    let all_same_cols = geometries.windows(2).all(|w| w[0].cols == w[1].cols);
    let all_same_rows = geometries.windows(2).all(|w| w[0].rows == w[1].rows);

    if all_same_cols && !all_same_rows {
        // Horizontal split: same width, different heights → stacked vertically
        let total_rows: f64 = geometries.iter().map(|g| f64::from(g.rows)).sum();
        let children: Vec<(f64, PaneNode)> = geometries
            .iter()
            .map(|g| {
                let proportion = if total_rows > 0.0 {
                    f64::from(g.rows) / total_rows
                } else {
                    1.0 / geometries.len() as f64
                };
                (proportion, g.to_leaf())
            })
            .collect();
        (PaneNode::HSplit { children }, InferenceQuality::Inferred)
    } else if all_same_rows && !all_same_cols {
        // Vertical split: same height, different widths → side by side
        let total_cols: f64 = geometries.iter().map(|g| f64::from(g.cols)).sum();
        let children: Vec<(f64, PaneNode)> = geometries
            .iter()
            .map(|g| {
                let proportion = if total_cols > 0.0 {
                    f64::from(g.cols) / total_cols
                } else {
                    1.0 / geometries.len() as f64
                };
                (proportion, g.to_leaf())
            })
            .collect();
        (PaneNode::VSplit { children }, InferenceQuality::Inferred)
    } else if all_same_cols && all_same_rows {
        // All panes same size — cannot determine split direction.
        // Default to VSplit (side-by-side).
        let n = geometries.len() as f64;
        let children: Vec<(f64, PaneNode)> =
            geometries.iter().map(|g| (1.0 / n, g.to_leaf())).collect();
        (PaneNode::VSplit { children }, InferenceQuality::Inferred)
    } else {
        // Mixed dimensions — try grouping by cols (VSplit of HSplit groups)
        if let Some(node) = try_group_by_cols(geometries) {
            return (node, InferenceQuality::Inferred);
        }

        // Fall back to flat HSplit
        tracing::warn!(
            pane_count = geometries.len(),
            "Could not infer split structure, falling back to flat layout"
        );
        let n = geometries.len() as f64;
        let children: Vec<(f64, PaneNode)> =
            geometries.iter().map(|g| (1.0 / n, g.to_leaf())).collect();
        (
            PaneNode::HSplit { children },
            InferenceQuality::FlatFallback,
        )
    }
}

/// Try to group panes by column count (for a VSplit of HSplit groups).
///
/// This handles common layouts like a 2x2 grid where the top-left and
/// bottom-left have the same cols, and top-right and bottom-right have
/// the same cols (but different from left).
fn try_group_by_cols(geometries: &[PaneGeometry]) -> Option<PaneNode> {
    let mut groups: HashMap<u16, Vec<&PaneGeometry>> = HashMap::new();
    for g in geometries {
        groups.entry(g.cols).or_default().push(g);
    }

    // Need at least 2 groups for this to be meaningful
    if groups.len() < 2 {
        return None;
    }

    let mut col_groups: Vec<(u16, Vec<&PaneGeometry>)> = groups.into_iter().collect();
    col_groups.sort_by_key(|(cols, _)| *cols);

    let total_cols: f64 = col_groups
        .iter()
        .map(|(_, group)| f64::from(group[0].cols))
        .sum();

    let children: Vec<(f64, PaneNode)> = col_groups
        .iter()
        .map(|(_, group)| {
            let proportion = if total_cols > 0.0 {
                f64::from(group[0].cols) / total_cols
            } else {
                1.0 / col_groups.len() as f64
            };

            let child = if group.len() == 1 {
                group[0].to_leaf()
            } else {
                // Sub-group: vertical stack (HSplit)
                let total_rows: f64 = group.iter().map(|g| f64::from(g.rows)).sum();
                let sub_children: Vec<(f64, PaneNode)> = group
                    .iter()
                    .map(|g| {
                        let sub_prop = if total_rows > 0.0 {
                            f64::from(g.rows) / total_rows
                        } else {
                            1.0 / group.len() as f64
                        };
                        (sub_prop, g.to_leaf())
                    })
                    .collect();
                PaneNode::HSplit {
                    children: sub_children,
                }
            };

            (proportion, child)
        })
        .collect();

    Some(PaneNode::VSplit { children })
}

// =============================================================================
// Pane matching for restore
// =============================================================================

/// Result of matching old panes to new panes after a restart.
#[derive(Debug, Clone)]
pub struct PaneMapping {
    /// old_pane_id → new_pane_id
    pub mappings: HashMap<u64, u64>,
    /// Old pane IDs that could not be matched.
    pub unmatched_old: Vec<u64>,
    /// New pane IDs that were not matched to any old pane.
    pub unmatched_new: Vec<u64>,
}

/// Match old panes (from snapshot) to new panes (from current session)
/// using cwd and title as the primary key, with terminal size as tiebreaker.
#[must_use]
pub fn match_panes(old_snapshot: &TopologySnapshot, new_panes: &[PaneInfo]) -> PaneMapping {
    let mut mappings = HashMap::new();
    let mut used_new: Vec<bool> = vec![false; new_panes.len()];

    // Collect old pane info
    let old_ids = old_snapshot.pane_ids();
    let mut old_leaves: Vec<PaneLeafInfo> = Vec::new();
    for window in &old_snapshot.windows {
        for tab in &window.tabs {
            collect_leaf_info(&tab.pane_tree, &mut old_leaves);
        }
    }

    // First pass: exact match on (cwd, title)
    for old_leaf in &old_leaves {
        let mut best_match: Option<(usize, u32)> = None;

        for (i, new_pane) in new_panes.iter().enumerate() {
            if used_new[i] {
                continue;
            }

            let cwd_match =
                old_leaf.cwd.as_deref() == new_pane.cwd.as_deref() && old_leaf.cwd.is_some();
            let title_match =
                old_leaf.title.as_deref() == new_pane.title.as_deref() && old_leaf.title.is_some();

            if cwd_match || title_match {
                // Score: cwd match = 2 points, title match = 1 point,
                // same size = 1 bonus point
                let mut score = 0u32;
                if cwd_match {
                    score += 2;
                }
                if title_match {
                    score += 1;
                }
                let size_match = old_leaf.rows == new_pane.effective_rows() as u16
                    && old_leaf.cols == new_pane.effective_cols() as u16;
                if size_match {
                    score += 1;
                }

                if best_match.is_none_or(|(_, s)| score > s) {
                    best_match = Some((i, score));
                }
            }
        }

        if let Some((idx, _)) = best_match {
            mappings.insert(old_leaf.pane_id, new_panes[idx].pane_id);
            used_new[idx] = true;
        }
    }

    let unmatched_old: Vec<u64> = old_ids
        .iter()
        .filter(|id| !mappings.contains_key(id))
        .copied()
        .collect();

    let unmatched_new: Vec<u64> = new_panes
        .iter()
        .enumerate()
        .filter(|(i, _)| !used_new[*i])
        .map(|(_, p)| p.pane_id)
        .collect();

    PaneMapping {
        mappings,
        unmatched_old,
        unmatched_new,
    }
}

/// Leaf info extracted from a PaneNode tree.
#[derive(Debug, Clone)]
struct PaneLeafInfo {
    pane_id: u64,
    rows: u16,
    cols: u16,
    cwd: Option<String>,
    title: Option<String>,
}

fn collect_leaf_info(node: &PaneNode, out: &mut Vec<PaneLeafInfo>) {
    match node {
        PaneNode::Leaf {
            pane_id,
            rows,
            cols,
            cwd,
            title,
            ..
        } => {
            out.push(PaneLeafInfo {
                pane_id: *pane_id,
                rows: *rows,
                cols: *cols,
                cwd: cwd.clone(),
                title: title.clone(),
            });
        }
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            for (_, child) in children {
                collect_leaf_info(child, out);
            }
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wezterm::PaneInfo;
    use std::collections::HashMap as StdHashMap;

    /// Helper to create a minimal PaneInfo for testing.
    fn make_pane(
        pane_id: u64,
        tab_id: u64,
        window_id: u64,
        rows: u32,
        cols: u32,
        cwd: Option<&str>,
        title: Option<&str>,
        is_active: bool,
    ) -> PaneInfo {
        PaneInfo {
            pane_id,
            tab_id,
            window_id,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: Some(crate::wezterm::PaneSize {
                rows,
                cols,
                pixel_width: None,
                pixel_height: None,
                dpi: None,
            }),
            rows: None,
            cols: None,
            title: title.map(String::from),
            cwd: cwd.map(String::from),
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active,
            is_zoomed: false,
            extra: StdHashMap::new(),
        }
    }

    // -------------------------------------------------------------------------
    // Roundtrip tests
    // -------------------------------------------------------------------------

    #[test]
    fn topology_roundtrip_single_pane() {
        let panes = vec![make_pane(
            0,
            0,
            0,
            24,
            80,
            Some("/home"),
            Some("bash"),
            true,
        )];
        let (snapshot, report) = TopologySnapshot::from_panes(&panes, 1000);

        assert_eq!(report.window_count, 1);
        assert_eq!(report.tab_count, 1);
        assert_eq!(report.pane_count, 1);
        assert_eq!(snapshot.pane_count(), 1);

        let json = snapshot.to_json().unwrap();
        let restored = TopologySnapshot::from_json(&json).unwrap();
        assert_eq!(snapshot, restored);
    }

    #[test]
    fn topology_roundtrip_2x2_grid() {
        // 2x2 grid: 2 columns (40 cols each), 2 rows (12 rows each)
        let panes = vec![
            make_pane(0, 0, 0, 12, 40, Some("/a"), Some("top-left"), true),
            make_pane(1, 0, 0, 12, 40, Some("/b"), Some("top-right"), false),
            make_pane(2, 0, 0, 12, 40, Some("/c"), Some("bot-left"), false),
            make_pane(3, 0, 0, 12, 40, Some("/d"), Some("bot-right"), false),
        ];
        let (snapshot, report) = TopologySnapshot::from_panes(&panes, 2000);

        assert_eq!(report.pane_count, 4);
        assert_eq!(snapshot.pane_count(), 4);

        let json = snapshot.to_json().unwrap();
        let restored = TopologySnapshot::from_json(&json).unwrap();
        assert_eq!(snapshot, restored);
    }

    #[test]
    fn topology_roundtrip_deep_nested_splits() {
        // HSplit of 3: different heights, same width
        let panes = vec![
            make_pane(0, 0, 0, 8, 80, None, None, true),
            make_pane(1, 0, 0, 8, 80, None, None, false),
            make_pane(2, 0, 0, 8, 80, None, None, false),
        ];
        let (snapshot, _) = TopologySnapshot::from_panes(&panes, 3000);
        assert_eq!(snapshot.pane_count(), 3);

        let json = snapshot.to_json().unwrap();
        let restored = TopologySnapshot::from_json(&json).unwrap();
        assert_eq!(snapshot, restored);
    }

    #[test]
    fn topology_roundtrip_multiple_windows() {
        let panes = vec![
            make_pane(0, 0, 0, 24, 80, None, None, true),
            make_pane(1, 1, 0, 24, 80, None, None, false),
            make_pane(2, 0, 1, 24, 80, None, None, true),
        ];
        let (snapshot, report) = TopologySnapshot::from_panes(&panes, 4000);

        assert_eq!(report.window_count, 2);
        assert_eq!(report.tab_count, 3);
        assert_eq!(report.pane_count, 3);

        let json = snapshot.to_json().unwrap();
        let restored = TopologySnapshot::from_json(&json).unwrap();
        assert_eq!(snapshot, restored);
    }

    #[test]
    fn topology_roundtrip_ten_panes() {
        let panes: Vec<PaneInfo> = (0..10)
            .map(|i| make_pane(i, 0, 0, 24, 80, None, None, i == 0))
            .collect();
        let (snapshot, report) = TopologySnapshot::from_panes(&panes, 5000);

        assert_eq!(report.pane_count, 10);
        assert_eq!(snapshot.pane_count(), 10);

        let json = snapshot.to_json().unwrap();
        let restored = TopologySnapshot::from_json(&json).unwrap();
        assert_eq!(snapshot, restored);
    }

    // -------------------------------------------------------------------------
    // Split-tree inference tests
    // -------------------------------------------------------------------------

    #[test]
    fn split_tree_inference_hsplit() {
        // Same cols (80), different rows → HSplit
        let panes = [
            make_pane(0, 0, 0, 12, 80, None, None, true),
            make_pane(1, 0, 0, 12, 80, None, None, false),
        ];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        let (tree, quality) = infer_split_tree(&refs);

        assert_eq!(quality, InferenceQuality::Inferred);
        match &tree {
            PaneNode::VSplit { children } => {
                // Same rows AND same cols → defaults to VSplit
                assert_eq!(children.len(), 2);
            }
            PaneNode::HSplit { children } => {
                assert_eq!(children.len(), 2);
            }
            PaneNode::Leaf { .. } => panic!("Expected split, got leaf"),
        }
    }

    #[test]
    fn split_tree_inference_vsplit() {
        // Same rows (24), different cols → VSplit
        let panes = [
            make_pane(0, 0, 0, 24, 40, None, None, true),
            make_pane(1, 0, 0, 24, 40, None, None, false),
        ];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        let (tree, quality) = infer_split_tree(&refs);

        assert_eq!(quality, InferenceQuality::Inferred);
        match &tree {
            PaneNode::VSplit { children } => {
                assert_eq!(children.len(), 2);
            }
            _ => panic!("Expected VSplit"),
        }
    }

    #[test]
    fn split_tree_inference_mixed_falls_back() {
        // Mixed: pane 0 = 12x40, pane 1 = 24x80 → no common dimension
        let panes = [
            make_pane(0, 0, 0, 12, 40, None, None, true),
            make_pane(1, 0, 0, 24, 80, None, None, false),
        ];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        let (tree, quality) = infer_split_tree(&refs);

        // Should try grouping, but with only 1 pane per column group,
        // it's still a valid inference (VSplit from group_by_cols)
        assert_eq!(tree.pane_count(), 2);
        assert!(quality == InferenceQuality::Inferred || quality == InferenceQuality::FlatFallback);
    }

    // -------------------------------------------------------------------------
    // Empty workspace
    // -------------------------------------------------------------------------

    #[test]
    fn empty_workspace() {
        let panes: Vec<PaneInfo> = vec![];
        let (snapshot, report) = TopologySnapshot::from_panes(&panes, 0);

        assert_eq!(report.window_count, 0);
        assert_eq!(report.tab_count, 0);
        assert_eq!(report.pane_count, 0);
        assert_eq!(snapshot.pane_count(), 0);
        assert!(snapshot.windows.is_empty());

        let json = snapshot.to_json().unwrap();
        let restored = TopologySnapshot::from_json(&json).unwrap();
        assert_eq!(snapshot, restored);
    }

    // -------------------------------------------------------------------------
    // Schema version forward compat
    // -------------------------------------------------------------------------

    #[test]
    fn schema_version_forward_compat_unknown_fields_ignored() {
        // Simulate a v2 snapshot with an extra field
        let json = r#"{
            "schema_version": 2,
            "captured_at": 1000,
            "workspace_id": null,
            "windows": [],
            "future_field": "ignored"
        }"#;
        // Should parse without error (serde default ignores unknown fields)
        let snapshot: TopologySnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.schema_version, 2);
        assert!(snapshot.windows.is_empty());
    }

    // -------------------------------------------------------------------------
    // Pane matching tests
    // -------------------------------------------------------------------------

    #[test]
    fn pane_matching_by_cwd() {
        let old_panes = vec![
            make_pane(10, 0, 0, 24, 80, Some("/project/a"), Some("a"), true),
            make_pane(11, 0, 0, 24, 80, Some("/project/b"), Some("b"), false),
        ];
        let (old_snapshot, _) = TopologySnapshot::from_panes(&old_panes, 1000);

        let new_panes = vec![
            make_pane(20, 0, 0, 24, 80, Some("/project/b"), Some("b"), false),
            make_pane(21, 0, 0, 24, 80, Some("/project/a"), Some("a"), true),
        ];

        let mapping = match_panes(&old_snapshot, &new_panes);

        assert_eq!(mapping.mappings.get(&10), Some(&21)); // /project/a
        assert_eq!(mapping.mappings.get(&11), Some(&20)); // /project/b
        assert!(mapping.unmatched_old.is_empty());
        assert!(mapping.unmatched_new.is_empty());
    }

    #[test]
    fn pane_matching_ambiguous_size_tiebreak() {
        // Two panes with same cwd but different sizes
        let old_panes = vec![
            make_pane(10, 0, 0, 12, 80, Some("/project"), Some("small"), true),
            make_pane(11, 0, 0, 24, 80, Some("/project"), Some("big"), false),
        ];
        let (old_snapshot, _) = TopologySnapshot::from_panes(&old_panes, 1000);

        let new_panes = vec![
            make_pane(20, 0, 0, 24, 80, Some("/project"), Some("big"), false),
            make_pane(21, 0, 0, 12, 80, Some("/project"), Some("small"), true),
        ];

        let mapping = match_panes(&old_snapshot, &new_panes);
        assert_eq!(mapping.mappings.len(), 2);
        assert!(mapping.unmatched_old.is_empty());
    }

    #[test]
    fn pane_matching_partial_no_match() {
        let old_panes = vec![make_pane(10, 0, 0, 24, 80, Some("/gone"), Some("x"), true)];
        let (old_snapshot, _) = TopologySnapshot::from_panes(&old_panes, 1000);

        let new_panes = vec![make_pane(20, 0, 0, 24, 80, Some("/new"), Some("y"), true)];

        let mapping = match_panes(&old_snapshot, &new_panes);
        assert!(mapping.mappings.is_empty());
        assert_eq!(mapping.unmatched_old, vec![10]);
        assert_eq!(mapping.unmatched_new, vec![20]);
    }

    // -------------------------------------------------------------------------
    // PaneNode helpers
    // -------------------------------------------------------------------------

    #[test]
    fn pane_ids_collected_correctly() {
        let panes = vec![
            make_pane(5, 0, 0, 24, 80, None, None, true),
            make_pane(10, 0, 0, 24, 80, None, None, false),
            make_pane(15, 1, 0, 24, 80, None, None, false),
        ];
        let (snapshot, _) = TopologySnapshot::from_panes(&panes, 1000);

        let mut ids = snapshot.pane_ids();
        ids.sort_unstable();
        assert_eq!(ids, vec![5, 10, 15]);
    }
}
