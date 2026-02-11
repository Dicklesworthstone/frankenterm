//! Layout restoration engine — recreate window/tab/pane split topology from snapshot.
//!
//! Given a [`TopologySnapshot`] captured by the session persistence system,
//! this module recreates the exact window/tab/pane arrangement using
//! `wezterm cli spawn` and `wezterm cli split-pane` commands via the
//! [`WeztermInterface`] trait.
//!
//! # Data flow
//!
//! ```text
//! TopologySnapshot → LayoutRestorer → WeztermInterface (spawn/split) → PaneIdMap
//! ```
//!
//! The returned [`RestoreResult`] contains a mapping from old pane IDs (in the
//! snapshot) to new pane IDs (in the live mux session), which downstream engines
//! (scrollback injection, process re-launch) use to target the correct panes.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::session_topology::{PaneNode, TabSnapshot, TopologySnapshot, WindowSnapshot};
use crate::wezterm::{SplitDirection, WeztermHandle};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for layout restoration behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RestoreConfig {
    /// Restore working directories for each pane.
    pub restore_working_dirs: bool,
    /// Attempt to restore approximate split ratios.
    pub restore_split_ratios: bool,
    /// Continue restoring remaining panes if one split fails.
    pub continue_on_error: bool,
}

impl Default for RestoreConfig {
    fn default() -> Self {
        Self {
            restore_working_dirs: true,
            restore_split_ratios: true,
            continue_on_error: true,
        }
    }
}

// =============================================================================
// Result types
// =============================================================================

/// Result of a layout restoration operation.
#[derive(Debug, Clone)]
pub struct RestoreResult {
    /// Mapping from old pane IDs (snapshot) to new pane IDs (live session).
    pub pane_id_map: HashMap<u64, u64>,
    /// Panes that failed to restore (old pane ID → error description).
    pub failed_panes: Vec<(u64, String)>,
    /// Number of windows created.
    pub windows_created: usize,
    /// Number of tabs created.
    pub tabs_created: usize,
    /// Total number of panes created.
    pub panes_created: usize,
}

impl RestoreResult {
    fn new() -> Self {
        Self {
            pane_id_map: HashMap::new(),
            failed_panes: Vec::new(),
            windows_created: 0,
            tabs_created: 0,
            panes_created: 0,
        }
    }
}

// =============================================================================
// Layout restorer
// =============================================================================

/// Engine that recreates mux session layout from a topology snapshot.
pub struct LayoutRestorer {
    wezterm: WeztermHandle,
    config: RestoreConfig,
}

impl LayoutRestorer {
    /// Create a new layout restorer.
    pub fn new(wezterm: WeztermHandle, config: RestoreConfig) -> Self {
        Self { wezterm, config }
    }

    /// Restore the full topology from a snapshot.
    ///
    /// Creates windows, tabs, and pane splits to match the captured layout.
    /// Returns a mapping from old pane IDs to new pane IDs.
    pub async fn restore(&self, snapshot: &TopologySnapshot) -> crate::Result<RestoreResult> {
        let mut result = RestoreResult::new();

        info!(
            windows = snapshot.windows.len(),
            "starting layout restoration from snapshot"
        );

        for (win_idx, window) in snapshot.windows.iter().enumerate() {
            match self.restore_window(window, win_idx, &mut result).await {
                Ok(()) => {
                    result.windows_created += 1;
                }
                Err(e) => {
                    warn!(window_id = window.window_id, error = %e, "failed to restore window");
                    if !self.config.continue_on_error {
                        return Err(e);
                    }
                }
            }
        }

        info!(
            windows = result.windows_created,
            tabs = result.tabs_created,
            panes = result.panes_created,
            failed = result.failed_panes.len(),
            "layout restoration complete"
        );

        Ok(result)
    }

    /// Restore a single window and all its tabs.
    async fn restore_window(
        &self,
        window: &WindowSnapshot,
        win_idx: usize,
        result: &mut RestoreResult,
    ) -> crate::Result<()> {
        debug!(
            window_id = window.window_id,
            tabs = window.tabs.len(),
            "restoring window"
        );

        for (tab_idx, tab) in window.tabs.iter().enumerate() {
            match self.restore_tab(tab, win_idx, tab_idx, result).await {
                Ok(()) => {
                    result.tabs_created += 1;
                }
                Err(e) => {
                    warn!(tab_id = tab.tab_id, error = %e, "failed to restore tab");
                    if !self.config.continue_on_error {
                        return Err(e);
                    }
                }
            }
        }

        Ok(())
    }

    /// Restore a single tab with its pane tree.
    async fn restore_tab(
        &self,
        tab: &TabSnapshot,
        win_idx: usize,
        tab_idx: usize,
        result: &mut RestoreResult,
    ) -> crate::Result<()> {
        debug!(tab_id = tab.tab_id, win_idx, tab_idx, "restoring tab");

        // Get initial CWD from the first leaf in the pane tree.
        let initial_cwd = if self.config.restore_working_dirs {
            first_leaf_cwd(&tab.pane_tree)
        } else {
            None
        };

        // Spawn the initial pane for this tab.
        let root_pane_id = self.wezterm.spawn(initial_cwd.as_deref(), None).await?;

        debug!(root_pane_id, tab_idx, "spawned root pane for tab");

        // Recursively restore the pane tree within this tab.
        self.restore_pane_tree(&tab.pane_tree, root_pane_id, result)
            .await?;

        // Activate the originally-active pane if known.
        if let Some(active_id) = tab.active_pane_id {
            if let Some(&new_id) = result.pane_id_map.get(&active_id) {
                if let Err(e) = self.wezterm.activate_pane(new_id).await {
                    debug!(old_pane = active_id, new_pane = new_id, error = %e, "failed to activate pane");
                }
            }
        }

        Ok(())
    }

    /// Recursively restore a pane tree.
    ///
    /// Uses explicit `Pin<Box<..>>` return type because async recursion
    /// requires boxing the future.
    fn restore_pane_tree<'a>(
        &'a self,
        node: &'a PaneNode,
        current_pane_id: u64,
        result: &'a mut RestoreResult,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match node {
                PaneNode::Leaf { pane_id, cwd, .. } => {
                    result.pane_id_map.insert(*pane_id, current_pane_id);
                    result.panes_created += 1;

                    if self.config.restore_working_dirs {
                        if let Some(dir) = cwd {
                            let path = normalize_cwd(dir);
                            if !path.is_empty() {
                                self.set_cwd(current_pane_id, &path).await;
                            }
                        }
                    }

                    Ok(())
                }

                PaneNode::HSplit { children } => {
                    self.restore_split_children(
                        children,
                        current_pane_id,
                        SplitDirection::Bottom,
                        result,
                    )
                    .await
                }

                PaneNode::VSplit { children } => {
                    self.restore_split_children(
                        children,
                        current_pane_id,
                        SplitDirection::Right,
                        result,
                    )
                    .await
                }
            }
        })
    }

    /// Restore children of a split node.
    ///
    /// The first child inherits `current_pane_id`. Each subsequent child
    /// is created by splitting from `current_pane_id` in the given direction.
    fn restore_split_children<'a>(
        &'a self,
        children: &'a [(f64, PaneNode)],
        current_pane_id: u64,
        direction: SplitDirection,
        result: &'a mut RestoreResult,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if children.is_empty() {
                return Ok(());
            }

            // First child uses the existing pane.
            let (_, first_child) = &children[0];
            self.restore_pane_tree(first_child, current_pane_id, result)
                .await?;

            // Remaining children: split from current_pane_id.
            for (i, (ratio, child)) in children.iter().enumerate().skip(1) {
                let percent = if self.config.restore_split_ratios {
                    let remaining_ratio: f64 = children[i..].iter().map(|(r, _)| r).sum();
                    if remaining_ratio > 0.0 {
                        let pct = (ratio / remaining_ratio * 100.0).round() as u8;
                        Some(pct.clamp(10, 90))
                    } else {
                        None
                    }
                } else {
                    None
                };

                let cwd = if self.config.restore_working_dirs {
                    first_leaf_cwd(child)
                } else {
                    None
                };

                match self
                    .wezterm
                    .split_pane(current_pane_id, direction, cwd.as_deref(), percent)
                    .await
                {
                    Ok(new_pane_id) => {
                        debug!(
                            parent = current_pane_id,
                            new_pane = new_pane_id,
                            ?direction,
                            percent,
                            "split pane created"
                        );
                        self.restore_pane_tree(child, new_pane_id, result).await?;
                    }
                    Err(e) => {
                        let leaf_ids = collect_leaf_ids(child);
                        warn!(
                            parent = current_pane_id,
                            error = %e,
                            affected_panes = ?leaf_ids,
                            "failed to create split pane"
                        );
                        for id in leaf_ids {
                            result.failed_panes.push((id, e.to_string()));
                        }
                        if !self.config.continue_on_error {
                            return Err(e);
                        }
                    }
                }
            }

            Ok(())
        })
    }

    /// Best-effort set working directory by sending a `cd` command.
    async fn set_cwd(&self, pane_id: u64, path: &str) {
        let cmd = format!("cd {}\n", shell_escape(path));
        if let Err(e) = self.wezterm.send_text(pane_id, &cmd).await {
            debug!(pane_id, path, error = %e, "failed to set cwd");
        }
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Extract the CWD from the first leaf node in a pane tree.
fn first_leaf_cwd(node: &PaneNode) -> Option<String> {
    match node {
        PaneNode::Leaf { cwd, .. } => cwd.as_ref().map(|c| normalize_cwd(c)),
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            children.first().and_then(|(_, child)| first_leaf_cwd(child))
        }
    }
}

/// Collect all leaf pane IDs from a pane tree.
fn collect_leaf_ids(node: &PaneNode) -> Vec<u64> {
    let mut ids = Vec::new();
    collect_leaf_ids_inner(node, &mut ids);
    ids
}

fn collect_leaf_ids_inner(node: &PaneNode, ids: &mut Vec<u64>) {
    match node {
        PaneNode::Leaf { pane_id, .. } => ids.push(*pane_id),
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            for (_, child) in children {
                collect_leaf_ids_inner(child, ids);
            }
        }
    }
}

/// Normalize a CWD string, stripping `file://` URI prefix if present.
fn normalize_cwd(cwd: &str) -> String {
    if let Some(rest) = cwd.strip_prefix("file://") {
        if let Some(path) = rest.strip_prefix('/') {
            format!("/{path}")
        } else if let Some(slash_pos) = rest.find('/') {
            rest[slash_pos..].to_string()
        } else {
            rest.to_string()
        }
    } else {
        cwd.to_string()
    }
}

/// Minimal shell escaping for paths (wraps in single quotes).
fn shell_escape(s: &str) -> String {
    if s.contains('\'') {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        format!("'{s}'")
    }
}

/// Count total leaf panes in a snapshot.
pub fn count_panes(snapshot: &TopologySnapshot) -> usize {
    snapshot
        .windows
        .iter()
        .flat_map(|w| &w.tabs)
        .map(|t| count_leaves(&t.pane_tree))
        .sum()
}

fn count_leaves(node: &PaneNode) -> usize {
    match node {
        PaneNode::Leaf { .. } => 1,
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            children.iter().map(|(_, c)| count_leaves(c)).sum()
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::session_topology::{PaneNode, TabSnapshot, TopologySnapshot, WindowSnapshot};
    use crate::wezterm::{MockWezterm, WeztermInterface};

    fn make_restorer(mock: Arc<MockWezterm>) -> LayoutRestorer {
        LayoutRestorer::new(mock, RestoreConfig::default())
    }

    fn leaf(pane_id: u64, cwd: Option<&str>) -> PaneNode {
        PaneNode::Leaf {
            pane_id,
            rows: 24,
            cols: 80,
            cwd: cwd.map(String::from),
            title: None,
            is_active: false,
        }
    }

    fn active_leaf(pane_id: u64) -> PaneNode {
        PaneNode::Leaf {
            pane_id,
            rows: 24,
            cols: 80,
            cwd: None,
            title: None,
            is_active: true,
        }
    }

    fn hsplit(children: Vec<(f64, PaneNode)>) -> PaneNode {
        PaneNode::HSplit { children }
    }

    fn vsplit(children: Vec<(f64, PaneNode)>) -> PaneNode {
        PaneNode::VSplit { children }
    }

    fn single_tab_snapshot(pane_tree: PaneNode) -> TopologySnapshot {
        TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs: vec![TabSnapshot {
                    tab_id: 0,
                    title: None,
                    pane_tree,
                    active_pane_id: None,
                }],
                active_tab_index: None,
            }],
        }
    }

    #[tokio::test]
    async fn restore_single_pane() {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let snapshot = single_tab_snapshot(leaf(42, Some("/home/user")));

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 1);
        assert_eq!(result.windows_created, 1);
        assert_eq!(result.tabs_created, 1);
        assert!(result.failed_panes.is_empty());
        assert!(result.pane_id_map.contains_key(&42));
    }

    #[tokio::test]
    async fn restore_horizontal_split() {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = hsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 2);
        assert!(result.pane_id_map.contains_key(&1));
        assert!(result.pane_id_map.contains_key(&2));
        assert_ne!(result.pane_id_map[&1], result.pane_id_map[&2]);
    }

    #[tokio::test]
    async fn restore_vertical_split() {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = vsplit(vec![(0.5, leaf(10, None)), (0.5, leaf(11, None))]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 2);
        assert!(result.pane_id_map.contains_key(&10));
        assert!(result.pane_id_map.contains_key(&11));
    }

    #[tokio::test]
    async fn restore_three_pane_l_shape() {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = vsplit(vec![
            (0.5, leaf(1, None)),
            (
                0.5,
                hsplit(vec![(0.5, leaf(2, None)), (0.5, leaf(3, None))]),
            ),
        ]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 3);
        for id in [1, 2, 3] {
            assert!(result.pane_id_map.contains_key(&id));
        }
    }

    #[tokio::test]
    async fn restore_four_pane_grid() {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = hsplit(vec![
            (
                0.5,
                vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]),
            ),
            (
                0.5,
                vsplit(vec![(0.5, leaf(3, None)), (0.5, leaf(4, None))]),
            ),
        ]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 4);
        for id in 1..=4 {
            assert!(result.pane_id_map.contains_key(&id));
        }
        let new_ids: std::collections::HashSet<_> = result.pane_id_map.values().collect();
        assert_eq!(new_ids.len(), 4);
    }

    #[tokio::test]
    async fn restore_deeply_nested_splits() {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = hsplit(vec![
            (0.5, leaf(1, None)),
            (
                0.5,
                vsplit(vec![
                    (0.5, leaf(2, None)),
                    (
                        0.5,
                        hsplit(vec![
                            (0.5, leaf(3, None)),
                            (
                                0.5,
                                vsplit(vec![
                                    (0.5, leaf(4, None)),
                                    (
                                        0.5,
                                        hsplit(vec![
                                            (0.5, leaf(5, None)),
                                            (0.5, leaf(6, None)),
                                        ]),
                                    ),
                                ]),
                            ),
                        ]),
                    ),
                ]),
            ),
        ]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 6);
        for id in 1..=6 {
            assert!(result.pane_id_map.contains_key(&id));
        }
    }

    #[tokio::test]
    async fn restore_multiple_tabs() {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let snapshot = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs: vec![
                    TabSnapshot {
                        tab_id: 0,
                        title: None,
                        pane_tree: leaf(1, None),
                        active_pane_id: None,
                    },
                    TabSnapshot {
                        tab_id: 1,
                        title: None,
                        pane_tree: vsplit(vec![
                            (0.5, leaf(2, None)),
                            (0.5, leaf(3, None)),
                        ]),
                        active_pane_id: Some(3),
                    },
                ],
                active_tab_index: Some(1),
            }],
        };

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.tabs_created, 2);
        assert_eq!(result.panes_created, 3);
        for id in 1..=3 {
            assert!(result.pane_id_map.contains_key(&id));
        }
    }

    #[tokio::test]
    async fn restore_multiple_windows() {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let snapshot = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![
                WindowSnapshot {
                    window_id: 0,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![TabSnapshot {
                        tab_id: 0,
                        title: None,
                        pane_tree: leaf(1, None),
                        active_pane_id: None,
                    }],
                    active_tab_index: None,
                },
                WindowSnapshot {
                    window_id: 1,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![TabSnapshot {
                        tab_id: 1,
                        title: None,
                        pane_tree: leaf(2, None),
                        active_pane_id: None,
                    }],
                    active_tab_index: None,
                },
            ],
        };

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.windows_created, 2);
        assert_eq!(result.panes_created, 2);
    }

    #[tokio::test]
    async fn restore_activates_correct_pane() {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let snapshot = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs: vec![TabSnapshot {
                    tab_id: 0,
                    title: None,
                    pane_tree: vsplit(vec![
                        (0.5, leaf(1, None)),
                        (0.5, active_leaf(2)),
                    ]),
                    active_pane_id: Some(2),
                }],
                active_tab_index: None,
            }],
        };

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 2);
        assert!(result.pane_id_map.contains_key(&2));
    }

    #[test]
    fn normalize_cwd_file_uri() {
        assert_eq!(normalize_cwd("file:///home/user"), "/home/user");
        assert_eq!(normalize_cwd("file://localhost/home/user"), "/home/user");
        assert_eq!(normalize_cwd("/home/user"), "/home/user");
        assert_eq!(normalize_cwd("file:///"), "/");
    }

    #[test]
    fn normalize_cwd_plain_path() {
        assert_eq!(normalize_cwd("/tmp/work"), "/tmp/work");
        assert_eq!(normalize_cwd("relative/path"), "relative/path");
    }

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("/home/user"), "'/home/user'");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("/home/user's dir"), "'/home/user'\\''s dir'");
    }

    #[test]
    fn shell_escape_with_spaces() {
        assert_eq!(shell_escape("/home/my dir"), "'/home/my dir'");
    }

    #[test]
    fn count_panes_single() {
        let snapshot = single_tab_snapshot(leaf(1, None));
        assert_eq!(count_panes(&snapshot), 1);
    }

    #[test]
    fn count_panes_complex() {
        let tree = hsplit(vec![
            (
                0.5,
                vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]),
            ),
            (0.5, leaf(3, None)),
        ]);
        let snapshot = single_tab_snapshot(tree);
        assert_eq!(count_panes(&snapshot), 3);
    }

    #[tokio::test]
    async fn restore_empty_snapshot() {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let snapshot = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![],
        };

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.windows_created, 0);
        assert_eq!(result.tabs_created, 0);
        assert_eq!(result.panes_created, 0);
    }

    #[test]
    fn first_leaf_cwd_from_leaf() {
        let node = leaf(1, Some("/home/user"));
        assert_eq!(first_leaf_cwd(&node), Some("/home/user".to_string()));
    }

    #[test]
    fn first_leaf_cwd_from_split() {
        let node = vsplit(vec![
            (0.5, leaf(1, Some("/tmp"))),
            (0.5, leaf(2, Some("/home"))),
        ]);
        assert_eq!(first_leaf_cwd(&node), Some("/tmp".to_string()));
    }

    #[test]
    fn first_leaf_cwd_none() {
        let node = leaf(1, None);
        assert_eq!(first_leaf_cwd(&node), None);
    }

    #[test]
    fn collect_leaf_ids_from_tree() {
        let tree = hsplit(vec![
            (
                0.5,
                vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]),
            ),
            (0.5, leaf(3, None)),
        ]);
        let ids = collect_leaf_ids(&tree);
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn restore_three_way_split() {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = vsplit(vec![
            (0.33, leaf(1, None)),
            (0.33, leaf(2, None)),
            (0.34, leaf(3, None)),
        ]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 3);
        let new_ids: std::collections::HashSet<_> = result.pane_id_map.values().collect();
        assert_eq!(new_ids.len(), 3);
    }

    #[tokio::test]
    async fn pane_id_map_completeness() {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = hsplit(vec![
            (0.25, leaf(100, None)),
            (0.25, leaf(200, None)),
            (0.25, leaf(300, None)),
            (0.25, leaf(400, None)),
        ]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        let all_leaves = collect_leaf_ids(&snapshot.windows[0].tabs[0].pane_tree);
        for id in &all_leaves {
            assert!(
                result.pane_id_map.contains_key(id),
                "pane {id} missing from pane_id_map"
            );
        }
        assert_eq!(result.pane_id_map.len(), all_leaves.len());
    }

    #[tokio::test]
    async fn restore_sets_cwd_from_file_uri() {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = leaf(1, Some("file:///home/agent/project"));
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 1);
        let new_id = result.pane_id_map[&1];
        let text: String = WeztermInterface::get_text(&*mock, new_id, false)
            .await
            .unwrap();
        assert!(text.contains("/home/agent/project"), "cwd should be set");
    }

    #[tokio::test]
    async fn config_skip_cwd_restore() {
        let mock = Arc::new(MockWezterm::new());
        let config = RestoreConfig {
            restore_working_dirs: false,
            ..Default::default()
        };
        let restorer = LayoutRestorer::new(mock.clone(), config);
        let tree = leaf(1, Some("/home/user"));
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 1);
        let new_id = result.pane_id_map[&1];
        let text: String = WeztermInterface::get_text(&*mock, new_id, false)
            .await
            .unwrap();
        assert!(!text.contains("cd "), "cwd should not be set");
    }

    #[test]
    fn restore_result_new_is_empty() {
        let r = RestoreResult::new();
        assert!(r.pane_id_map.is_empty());
        assert!(r.failed_panes.is_empty());
        assert_eq!(r.windows_created, 0);
        assert_eq!(r.tabs_created, 0);
        assert_eq!(r.panes_created, 0);
    }

    #[test]
    fn restore_config_defaults() {
        let c = RestoreConfig::default();
        assert!(c.restore_working_dirs);
        assert!(c.restore_split_ratios);
        assert!(c.continue_on_error);
    }
}
