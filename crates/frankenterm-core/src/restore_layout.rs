//! Layout restoration engine — recreate window/tab/pane split topology from snapshot.
//!
//! Given a [`TopologySnapshot`] captured by the session persistence system,
//! this module recreates the exact window/tab/pane arrangement using
//! `wezterm cli spawn` and `wezterm cli split-pane` commands via the
//! `WeztermInterface` trait.
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
use crate::wezterm::{SpawnTarget, SplitDirection, WeztermHandle};

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
                Ok(restored_any_tabs) => {
                    if restored_any_tabs {
                        result.windows_created += 1;
                    }
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
    ) -> crate::Result<bool> {
        debug!(
            window_id = window.window_id,
            tabs = window.tabs.len(),
            "restoring window"
        );

        let mut restored_window_id = None;
        let mut active_window_pane_id = None;
        let mut restored_any_tabs = false;

        for (tab_idx, tab) in window.tabs.iter().enumerate() {
            let target = SpawnTarget {
                window_id: restored_window_id,
                new_window: restored_window_id.is_none(),
            };
            match self
                .restore_tab(tab, win_idx, tab_idx, target, result)
                .await
            {
                Ok((window_id, active_pane_id)) => {
                    restored_window_id.get_or_insert(window_id);
                    let should_activate_tab = window.active_tab_index == Some(tab_idx)
                        || (window.active_tab_index.is_none() && window.tabs.len() == 1);
                    if should_activate_tab {
                        active_window_pane_id = active_pane_id;
                    }
                    result.tabs_created += 1;
                    restored_any_tabs = true;
                }
                Err(e) => {
                    record_failed_tree(result, &tab.pane_tree, &e.to_string());
                    warn!(tab_id = tab.tab_id, error = %e, "failed to restore tab");
                    if !self.config.continue_on_error {
                        return Err(e);
                    }
                }
            }
        }

        if let Some(active_pane_id) = active_window_pane_id {
            if let Err(e) = self.wezterm.activate_pane(active_pane_id).await {
                debug!(pane_id = active_pane_id, error = %e, "failed to activate window pane");
            }
        }

        Ok(restored_any_tabs)
    }

    /// Restore a single tab with its pane tree.
    async fn restore_tab(
        &self,
        tab: &TabSnapshot,
        win_idx: usize,
        tab_idx: usize,
        spawn_target: SpawnTarget,
        result: &mut RestoreResult,
    ) -> crate::Result<(u64, Option<u64>)> {
        debug!(
            tab_id = tab.tab_id,
            win_idx,
            tab_idx,
            ?spawn_target,
            "restoring tab"
        );

        // Get initial CWD from the first leaf in the pane tree.
        let initial_cwd = if self.config.restore_working_dirs {
            first_leaf_cwd(&tab.pane_tree)
        } else {
            None
        };

        // Spawn the initial pane for this tab.
        let root_pane_id = self
            .wezterm
            .spawn_targeted(initial_cwd.as_deref(), None, spawn_target)
            .await?;
        let root_pane = self.wezterm.get_pane(root_pane_id).await?;

        debug!(root_pane_id, tab_idx, "spawned root pane for tab");

        // Recursively restore the pane tree within this tab.
        self.restore_pane_tree(&tab.pane_tree, root_pane_id, result)
            .await?;

        let active_pane_id = tab
            .active_pane_id
            .and_then(|active_id| result.pane_id_map.get(&active_id).copied());

        Ok((root_pane.window_id, active_pane_id))
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
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => children
            .first()
            .and_then(|(_, child)| first_leaf_cwd(child)),
    }
}

/// Collect all leaf pane IDs from a pane tree.
fn collect_leaf_ids(node: &PaneNode) -> Vec<u64> {
    let mut ids = Vec::new();
    collect_leaf_ids_inner(node, &mut ids);
    ids
}

fn record_failed_tree(result: &mut RestoreResult, node: &PaneNode, error: &str) {
    for pane_id in collect_leaf_ids(node) {
        if result.pane_id_map.contains_key(&pane_id) {
            continue;
        }
        if result
            .failed_panes
            .iter()
            .any(|(existing_id, _)| *existing_id == pane_id)
        {
            continue;
        }
        result.failed_panes.push((pane_id, error.to_string()));
    }
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
    use crate::wezterm::{
        MockWezterm, SpawnTarget, WeztermFuture, WeztermHandle, WeztermInterface,
    };

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_compat::CompatRuntime;
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build restore_layout test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn make_restorer(mock: Arc<MockWezterm>) -> LayoutRestorer {
        LayoutRestorer::new(mock, RestoreConfig::default())
    }

    struct AlwaysFailSpawnWezterm;

    struct AlwaysFailSplitWezterm {
        inner: Arc<MockWezterm>,
    }

    impl WeztermInterface for AlwaysFailSpawnWezterm {
        fn list_panes(&self) -> WeztermFuture<'_, Vec<crate::wezterm::PaneInfo>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, crate::wezterm::PaneInfo> {
            Box::pin(async move {
                Err(crate::Error::Runtime(format!(
                    "unexpected get_pane({pane_id}) on failing spawn mock"
                )))
            })
        }

        fn get_text(&self, pane_id: u64, _: bool) -> WeztermFuture<'_, String> {
            Box::pin(async move {
                Err(crate::Error::Runtime(format!(
                    "unexpected get_text({pane_id}) on failing spawn mock"
                )))
            })
        }

        fn send_text(&self, pane_id: u64, _: &str) -> WeztermFuture<'_, ()> {
            Box::pin(async move {
                Err(crate::Error::Runtime(format!(
                    "unexpected send_text({pane_id}) on failing spawn mock"
                )))
            })
        }

        fn send_text_no_paste(&self, pane_id: u64, _: &str) -> WeztermFuture<'_, ()> {
            self.send_text(pane_id, "")
        }

        fn send_text_with_options(
            &self,
            pane_id: u64,
            _: &str,
            _: bool,
            _: bool,
        ) -> WeztermFuture<'_, ()> {
            self.send_text(pane_id, "")
        }

        fn send_control(&self, pane_id: u64, _: &str) -> WeztermFuture<'_, ()> {
            self.send_text(pane_id, "")
        }

        fn send_ctrl_c(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.send_text(pane_id, "")
        }

        fn send_ctrl_d(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.send_text(pane_id, "")
        }

        fn spawn(&self, _: Option<&str>, _: Option<&str>) -> WeztermFuture<'_, u64> {
            Box::pin(async { Err(crate::Error::Runtime("simulated spawn failure".to_string())) })
        }

        fn spawn_targeted(
            &self,
            _: Option<&str>,
            _: Option<&str>,
            _: SpawnTarget,
        ) -> WeztermFuture<'_, u64> {
            Box::pin(async { Err(crate::Error::Runtime("simulated spawn failure".to_string())) })
        }

        fn split_pane(
            &self,
            pane_id: u64,
            _: crate::wezterm::SplitDirection,
            _: Option<&str>,
            _: Option<u8>,
        ) -> WeztermFuture<'_, u64> {
            Box::pin(async move {
                Err(crate::Error::Runtime(format!(
                    "unexpected split_pane({pane_id}) on failing spawn mock"
                )))
            })
        }

        fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            Box::pin(async move {
                Err(crate::Error::Runtime(format!(
                    "unexpected activate_pane({pane_id}) on failing spawn mock"
                )))
            })
        }

        fn get_pane_direction(
            &self,
            pane_id: u64,
            _: crate::wezterm::MoveDirection,
        ) -> WeztermFuture<'_, Option<u64>> {
            Box::pin(async move {
                Err(crate::Error::Runtime(format!(
                    "unexpected get_pane_direction({pane_id}) on failing spawn mock"
                )))
            })
        }

        fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            Box::pin(async move {
                Err(crate::Error::Runtime(format!(
                    "unexpected kill_pane({pane_id}) on failing spawn mock"
                )))
            })
        }

        fn zoom_pane(&self, pane_id: u64, _: bool) -> WeztermFuture<'_, ()> {
            Box::pin(async move {
                Err(crate::Error::Runtime(format!(
                    "unexpected zoom_pane({pane_id}) on failing spawn mock"
                )))
            })
        }

        fn circuit_status(&self) -> crate::circuit_breaker::CircuitBreakerStatus {
            crate::circuit_breaker::CircuitBreakerStatus::default()
        }
    }

    impl WeztermInterface for AlwaysFailSplitWezterm {
        fn list_panes(&self) -> WeztermFuture<'_, Vec<crate::wezterm::PaneInfo>> {
            self.inner.list_panes()
        }

        fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, crate::wezterm::PaneInfo> {
            self.inner.get_pane(pane_id)
        }

        fn get_text(&self, pane_id: u64, escapes: bool) -> WeztermFuture<'_, String> {
            self.inner.get_text(pane_id, escapes)
        }

        fn send_text(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
            self.inner.send_text(pane_id, text)
        }

        fn send_text_no_paste(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
            self.inner.send_text_no_paste(pane_id, text)
        }

        fn send_text_with_options(
            &self,
            pane_id: u64,
            text: &str,
            bracketed_paste: bool,
            normalize_newlines: bool,
        ) -> WeztermFuture<'_, ()> {
            self.inner
                .send_text_with_options(pane_id, text, bracketed_paste, normalize_newlines)
        }

        fn send_control(&self, pane_id: u64, control_char: &str) -> WeztermFuture<'_, ()> {
            self.inner.send_control(pane_id, control_char)
        }

        fn send_ctrl_c(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.send_ctrl_c(pane_id)
        }

        fn send_ctrl_d(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.send_ctrl_d(pane_id)
        }

        fn spawn(&self, cwd: Option<&str>, domain_name: Option<&str>) -> WeztermFuture<'_, u64> {
            self.inner.spawn(cwd, domain_name)
        }

        fn spawn_targeted(
            &self,
            cwd: Option<&str>,
            domain_name: Option<&str>,
            target: SpawnTarget,
        ) -> WeztermFuture<'_, u64> {
            self.inner.spawn_targeted(cwd, domain_name, target)
        }

        fn split_pane(
            &self,
            _: u64,
            _: crate::wezterm::SplitDirection,
            _: Option<&str>,
            _: Option<u8>,
        ) -> WeztermFuture<'_, u64> {
            Box::pin(async { Err(crate::Error::Runtime("simulated split failure".to_string())) })
        }

        fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.activate_pane(pane_id)
        }

        fn get_pane_direction(
            &self,
            pane_id: u64,
            direction: crate::wezterm::MoveDirection,
        ) -> WeztermFuture<'_, Option<u64>> {
            self.inner.get_pane_direction(pane_id, direction)
        }

        fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.kill_pane(pane_id)
        }

        fn zoom_pane(&self, pane_id: u64, zoomed: bool) -> WeztermFuture<'_, ()> {
            self.inner.zoom_pane(pane_id, zoomed)
        }

        fn circuit_status(&self) -> crate::circuit_breaker::CircuitBreakerStatus {
            self.inner.circuit_status()
        }
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

    #[test]
    fn restore_single_pane() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let snapshot = single_tab_snapshot(leaf(42, Some("/home/user")));

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 1);
            assert_eq!(result.windows_created, 1);
            assert_eq!(result.tabs_created, 1);
            assert!(result.failed_panes.is_empty());
            assert!(result.pane_id_map.contains_key(&42));
        });
    }

    #[test]
    fn restore_horizontal_split() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let tree = hsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]);
            let snapshot = single_tab_snapshot(tree);

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 2);
            assert!(result.pane_id_map.contains_key(&1));
            assert!(result.pane_id_map.contains_key(&2));
            assert_ne!(result.pane_id_map[&1], result.pane_id_map[&2]);
        });
    }

    #[test]
    fn restore_vertical_split() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let tree = vsplit(vec![(0.5, leaf(10, None)), (0.5, leaf(11, None))]);
            let snapshot = single_tab_snapshot(tree);

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 2);
            assert!(result.pane_id_map.contains_key(&10));
            assert!(result.pane_id_map.contains_key(&11));
        });
    }

    #[test]
    fn restore_three_pane_l_shape() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn restore_four_pane_grid() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn restore_deeply_nested_splits() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn restore_multiple_tabs() {
        run_async_test(async {
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
                            pane_tree: vsplit(vec![(0.5, leaf(2, None)), (0.5, leaf(3, None))]),
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

            let pane_one = mock.pane_state(result.pane_id_map[&1]).await.unwrap();
            let pane_two = mock.pane_state(result.pane_id_map[&2]).await.unwrap();
            let pane_three = mock.pane_state(result.pane_id_map[&3]).await.unwrap();
            assert_eq!(pane_one.window_id, pane_two.window_id);
            assert_eq!(pane_two.window_id, pane_three.window_id);
            assert_ne!(pane_one.tab_id, pane_two.tab_id);
            assert_eq!(pane_two.tab_id, pane_three.tab_id);
        });
    }

    #[test]
    fn restore_multiple_windows() {
        run_async_test(async {
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

            let pane_one = mock.pane_state(result.pane_id_map[&1]).await.unwrap();
            let pane_two = mock.pane_state(result.pane_id_map[&2]).await.unwrap();
            assert_ne!(pane_one.window_id, pane_two.window_id);
        });
    }

    #[test]
    fn restore_multiple_tabs_respects_active_tab_index() {
        run_async_test(async {
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
                            active_pane_id: Some(1),
                        },
                        TabSnapshot {
                            tab_id: 1,
                            title: None,
                            pane_tree: vsplit(vec![(0.5, leaf(2, None)), (0.5, leaf(3, None))]),
                            active_pane_id: Some(3),
                        },
                    ],
                    active_tab_index: Some(0),
                }],
            };

            let result = restorer.restore(&snapshot).await.unwrap();

            let active_first = mock.pane_state(result.pane_id_map[&1]).await.unwrap();
            let inactive_second = mock.pane_state(result.pane_id_map[&3]).await.unwrap();
            assert!(active_first.is_active);
            assert!(!inactive_second.is_active);
        });
    }

    #[test]
    fn restore_activates_correct_pane() {
        run_async_test(async {
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
                        pane_tree: vsplit(vec![(0.5, leaf(1, None)), (0.5, active_leaf(2))]),
                        active_pane_id: Some(2),
                    }],
                    active_tab_index: None,
                }],
            };

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 2);
            assert!(result.pane_id_map.contains_key(&2));
            let active = mock.pane_state(result.pane_id_map[&2]).await.unwrap();
            assert!(active.is_active);
        });
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

    #[test]
    fn restore_empty_snapshot() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn restore_failed_window_does_not_increment_window_count() {
        run_async_test(async {
            let wezterm: WeztermHandle = Arc::new(AlwaysFailSpawnWezterm);
            let restorer = LayoutRestorer::new(wezterm, RestoreConfig::default());
            let snapshot = single_tab_snapshot(leaf(1, None));

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.windows_created, 0);
            assert_eq!(result.tabs_created, 0);
            assert_eq!(result.panes_created, 0);
            assert_eq!(
                result.failed_panes,
                vec![(1, "simulated spawn failure".to_string())]
            );
        });
    }

    #[test]
    fn restore_window_partial_failure_does_not_mark_restored_leaf_failed() {
        run_async_test(async {
            let inner = Arc::new(MockWezterm::new());
            let wezterm: WeztermHandle = Arc::new(AlwaysFailSplitWezterm {
                inner: inner.clone(),
            });
            let restorer = LayoutRestorer::new(
                wezterm,
                RestoreConfig {
                    continue_on_error: false,
                    ..RestoreConfig::default()
                },
            );
            let window = WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs: vec![TabSnapshot {
                    tab_id: 0,
                    title: None,
                    pane_tree: vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]),
                    active_pane_id: None,
                }],
                active_tab_index: None,
            };
            let mut result = RestoreResult::new();

            assert!(
                restorer
                    .restore_window(&window, 0, &mut result)
                    .await
                    .is_err()
            );
            assert!(result.pane_id_map.contains_key(&1));
            assert_eq!(
                result.failed_panes,
                vec![(2, "simulated split failure".to_string())]
            );
        });
    }

    #[test]
    fn restore_partial_first_tab_reuses_created_window_for_later_tabs() {
        run_async_test(async {
            let inner = Arc::new(MockWezterm::new());
            let wezterm: WeztermHandle = Arc::new(AlwaysFailSplitWezterm {
                inner: inner.clone(),
            });
            let restorer = LayoutRestorer::new(wezterm, RestoreConfig::default());
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
                            pane_tree: vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]),
                            active_pane_id: None,
                        },
                        TabSnapshot {
                            tab_id: 1,
                            title: None,
                            pane_tree: leaf(3, None),
                            active_pane_id: None,
                        },
                    ],
                    active_tab_index: None,
                }],
            };

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.windows_created, 1);
            assert_eq!(result.tabs_created, 2);
            assert!(result.pane_id_map.contains_key(&1));
            assert!(result.pane_id_map.contains_key(&3));
            assert_eq!(
                result.failed_panes,
                vec![(2, "simulated split failure".to_string())]
            );

            let first_tab_pane = inner.pane_state(result.pane_id_map[&1]).await.unwrap();
            let second_tab_pane = inner.pane_state(result.pane_id_map[&3]).await.unwrap();
            assert_eq!(first_tab_pane.window_id, second_tab_pane.window_id);
            assert_ne!(first_tab_pane.tab_id, second_tab_pane.tab_id);
        });
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

    #[test]
    fn restore_three_way_split() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn pane_id_map_completeness() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn restore_sets_cwd_from_file_uri() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn config_skip_cwd_restore() {
        run_async_test(async {
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
        });
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

    // =================================================================
    // RestoreConfig additional tests
    // =================================================================

    #[test]
    fn restore_config_serde_roundtrip() {
        let config = RestoreConfig {
            restore_working_dirs: false,
            restore_split_ratios: true,
            continue_on_error: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: RestoreConfig = serde_json::from_str(&json).unwrap();
        assert!(!back.restore_working_dirs);
        assert!(back.restore_split_ratios);
        assert!(!back.continue_on_error);
    }

    #[test]
    fn restore_config_serde_default_fills_missing() {
        // Empty JSON object should deserialize with defaults (#[serde(default)])
        let back: RestoreConfig = serde_json::from_str("{}").unwrap();
        assert!(back.restore_working_dirs);
        assert!(back.restore_split_ratios);
        assert!(back.continue_on_error);
    }

    #[test]
    fn restore_config_debug() {
        let c = RestoreConfig::default();
        let dbg = format!("{c:?}");
        assert!(dbg.contains("RestoreConfig"));
        assert!(dbg.contains("restore_working_dirs"));
    }

    // =================================================================
    // normalize_cwd edge cases
    // =================================================================

    #[test]
    fn normalize_cwd_empty_string() {
        assert_eq!(normalize_cwd(""), "");
    }

    #[test]
    fn normalize_cwd_file_uri_with_hostname() {
        // file://hostname/path should extract /path
        assert_eq!(normalize_cwd("file://myhost/home/user"), "/home/user");
    }

    #[test]
    fn normalize_cwd_file_uri_empty_authority() {
        // file:// with no authority and no path
        assert_eq!(normalize_cwd("file://"), "");
    }

    #[test]
    fn normalize_cwd_file_uri_root_only() {
        assert_eq!(normalize_cwd("file:///"), "/");
    }

    #[test]
    fn normalize_cwd_file_uri_with_spaces() {
        assert_eq!(
            normalize_cwd("file:///home/my user/project"),
            "/home/my user/project"
        );
    }

    #[test]
    fn normalize_cwd_tilde_path() {
        // Non-file:// paths pass through unchanged
        assert_eq!(normalize_cwd("~/projects"), "~/projects");
    }

    #[test]
    fn normalize_cwd_windows_style_path() {
        assert_eq!(normalize_cwd("C:\\Users\\test"), "C:\\Users\\test");
    }

    // =================================================================
    // shell_escape edge cases
    // =================================================================

    #[test]
    fn shell_escape_empty_string() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn shell_escape_with_dollar_sign() {
        assert_eq!(shell_escape("/home/$USER"), "'/home/$USER'");
    }

    #[test]
    fn shell_escape_with_special_chars() {
        // Shell special chars (;, &, |, >) should be safely quoted
        assert_eq!(shell_escape("cmd; rm -rf /"), "'cmd; rm -rf /'");
    }

    #[test]
    fn shell_escape_with_newline() {
        assert_eq!(shell_escape("line1\nline2"), "'line1\nline2'");
    }

    #[test]
    fn shell_escape_multiple_single_quotes() {
        assert_eq!(shell_escape("it's a 'test'"), "'it'\\''s a '\\''test'\\'''",);
    }

    #[test]
    fn shell_escape_only_single_quote() {
        assert_eq!(shell_escape("'"), "''\\'''");
    }

    // =================================================================
    // collect_leaf_ids edge cases
    // =================================================================

    #[test]
    fn collect_leaf_ids_single_leaf() {
        let node = leaf(42, None);
        assert_eq!(collect_leaf_ids(&node), vec![42]);
    }

    #[test]
    fn collect_leaf_ids_empty_hsplit() {
        let node = hsplit(vec![]);
        assert_eq!(collect_leaf_ids(&node), Vec::<u64>::new());
    }

    #[test]
    fn collect_leaf_ids_empty_vsplit() {
        let node = vsplit(vec![]);
        assert_eq!(collect_leaf_ids(&node), Vec::<u64>::new());
    }

    #[test]
    fn collect_leaf_ids_deeply_nested() {
        let tree = hsplit(vec![(
            1.0,
            vsplit(vec![(
                1.0,
                hsplit(vec![(1.0, vsplit(vec![(1.0, leaf(99, None))]))]),
            )]),
        )]);
        assert_eq!(collect_leaf_ids(&tree), vec![99]);
    }

    #[test]
    fn collect_leaf_ids_preserves_order() {
        // Left-to-right, depth-first traversal order
        let tree = vsplit(vec![
            (0.33, leaf(5, None)),
            (
                0.33,
                hsplit(vec![(0.5, leaf(3, None)), (0.5, leaf(7, None))]),
            ),
            (0.34, leaf(1, None)),
        ]);
        assert_eq!(collect_leaf_ids(&tree), vec![5, 3, 7, 1]);
    }

    // =================================================================
    // first_leaf_cwd edge cases
    // =================================================================

    #[test]
    fn first_leaf_cwd_nested_three_levels() {
        let tree = hsplit(vec![(
            1.0,
            vsplit(vec![(
                1.0,
                hsplit(vec![(1.0, leaf(1, Some("/deep/path")))]),
            )]),
        )]);
        assert_eq!(first_leaf_cwd(&tree), Some("/deep/path".to_string()));
    }

    #[test]
    fn first_leaf_cwd_file_uri_normalized() {
        let node = leaf(1, Some("file:///home/agent"));
        assert_eq!(first_leaf_cwd(&node), Some("/home/agent".to_string()));
    }

    #[test]
    fn first_leaf_cwd_empty_children() {
        let node = hsplit(vec![]);
        assert_eq!(first_leaf_cwd(&node), None);
    }

    #[test]
    fn first_leaf_cwd_first_child_no_cwd_second_has() {
        // first_leaf_cwd returns the FIRST leaf's cwd, even if None
        let tree = vsplit(vec![
            (0.5, leaf(1, None)),
            (0.5, leaf(2, Some("/home/user"))),
        ]);
        assert_eq!(first_leaf_cwd(&tree), None);
    }

    #[test]
    fn first_leaf_cwd_hsplit_vs_vsplit() {
        let tree_h = hsplit(vec![(1.0, leaf(1, Some("/h")))]);
        let tree_v = vsplit(vec![(1.0, leaf(1, Some("/v")))]);
        assert_eq!(first_leaf_cwd(&tree_h), Some("/h".to_string()));
        assert_eq!(first_leaf_cwd(&tree_v), Some("/v".to_string()));
    }

    // =================================================================
    // count_leaves / count_panes edge cases
    // =================================================================

    #[test]
    fn count_leaves_single() {
        assert_eq!(count_leaves(&leaf(1, None)), 1);
    }

    #[test]
    fn count_leaves_empty_split() {
        assert_eq!(count_leaves(&hsplit(vec![])), 0);
        assert_eq!(count_leaves(&vsplit(vec![])), 0);
    }

    #[test]
    fn count_leaves_deeply_nested() {
        let tree = vsplit(vec![(
            1.0,
            hsplit(vec![
                (0.5, leaf(1, None)),
                (
                    0.5,
                    vsplit(vec![(0.5, leaf(2, None)), (0.5, leaf(3, None))]),
                ),
            ]),
        )]);
        assert_eq!(count_leaves(&tree), 3);
    }

    #[test]
    fn count_panes_empty_snapshot() {
        let snapshot = TopologySnapshot {
            schema_version: 1,
            captured_at: 0,
            workspace_id: None,
            windows: vec![],
        };
        assert_eq!(count_panes(&snapshot), 0);
    }

    #[test]
    fn count_panes_multi_window_multi_tab() {
        let snapshot = TopologySnapshot {
            schema_version: 1,
            captured_at: 0,
            workspace_id: None,
            windows: vec![
                WindowSnapshot {
                    window_id: 0,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![
                        TabSnapshot {
                            tab_id: 0,
                            title: None,
                            pane_tree: vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]),
                            active_pane_id: None,
                        },
                        TabSnapshot {
                            tab_id: 1,
                            title: None,
                            pane_tree: leaf(3, None),
                            active_pane_id: None,
                        },
                    ],
                    active_tab_index: None,
                },
                WindowSnapshot {
                    window_id: 1,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![TabSnapshot {
                        tab_id: 2,
                        title: None,
                        pane_tree: hsplit(vec![(0.5, leaf(4, None)), (0.5, leaf(5, None))]),
                        active_pane_id: None,
                    }],
                    active_tab_index: None,
                },
            ],
        };
        assert_eq!(count_panes(&snapshot), 5);
    }

    // =================================================================
    // RestoreResult tests
    // =================================================================

    #[test]
    fn restore_result_debug() {
        let r = RestoreResult::new();
        let dbg = format!("{r:?}");
        assert!(dbg.contains("RestoreResult"));
        assert!(dbg.contains("pane_id_map"));
    }
}
