//! Layout restoration engine — recreate window/tab/pane split topology from snapshot.
//!
//! Given a [`TopologySnapshot`] captured by the session persistence system,
//! this module recreates the supported window/tab/pane split topology using
//! mux spawn and split operations exposed by the `WeztermInterface` trait.
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

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::RuntimeOperationSource;
use crate::session_topology::{
    MAX_PANE_TREE_DEPTH, PaneNode, TOPOLOGY_SCHEMA_VERSION, TabSnapshot, TopologySnapshot,
    WindowSnapshot,
};
use crate::wezterm::{SpawnTarget, SplitDirection, WeztermHandle};

fn restore_cancelled_error(operation: &'static str, _err: impl std::fmt::Display) -> crate::Error {
    crate::Error::RuntimeOperation {
        operation,
        source: RuntimeOperationSource::Cancelled("caller capability stopped".to_string()),
    }
}

fn restore_layout_error(operation: &'static str, detail: &'static str) -> crate::Error {
    crate::Error::RuntimeOperation {
        operation,
        source: RuntimeOperationSource::Backend(detail.to_string()),
    }
}

const FAILURE_TAB_RESTORE: &str = "tab restoration failed";
const FAILURE_SPLIT: &str = "pane split creation failed";
const FAILURE_ACTIVATION: &str = "pane activation failed";
const MAX_DETAILED_FAILURE_LOGS: usize = 20;

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
    #[cfg(test)]
    fn new() -> Self {
        Self::with_capacity(0)
    }

    fn with_capacity(pane_capacity: usize) -> Self {
        Self {
            pane_id_map: HashMap::with_capacity(pane_capacity),
            failed_panes: Vec::with_capacity(pane_capacity),
            windows_created: 0,
            tabs_created: 0,
            panes_created: 0,
        }
    }
}

struct RestoreAccumulator {
    result: RestoreResult,
    failed_pane_ids: HashSet<u64>,
    failure_logs_emitted: usize,
    failure_logs_suppressed: usize,
}

impl RestoreAccumulator {
    fn with_capacity(pane_capacity: usize) -> Self {
        Self {
            result: RestoreResult::with_capacity(pane_capacity),
            failed_pane_ids: HashSet::with_capacity(pane_capacity),
            failure_logs_emitted: 0,
            failure_logs_suppressed: 0,
        }
    }

    fn record_failure(&mut self, pane_id: u64, reason: &'static str) -> bool {
        if !self.failed_pane_ids.insert(pane_id) {
            return false;
        }
        self.result.failed_panes.push((pane_id, reason.to_string()));
        true
    }

    fn record_unmapped_failure(&mut self, pane_id: u64, reason: &'static str) -> bool {
        if self.result.pane_id_map.contains_key(&pane_id) {
            return false;
        }
        self.record_failure(pane_id, reason)
    }

    fn claim_failure_log_slot(&mut self) -> bool {
        if self.failure_logs_emitted < MAX_DETAILED_FAILURE_LOGS {
            self.failure_logs_emitted += 1;
            true
        } else {
            self.failure_logs_suppressed = self.failure_logs_suppressed.saturating_add(1);
            false
        }
    }
}

struct RestorePreflight {
    pane_count: usize,
    normalized_cwds: HashMap<u64, String>,
}

#[derive(Debug, Clone, Copy)]
struct RestoredTab {
    window_id: u64,
    active_old_pane_id: u64,
    active_new_pane_id: u64,
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
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.restore_with_cx(&cx, snapshot).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`restore`].
    ///
    /// Pre-flight checkpoint gates the restoration before any
    /// mux spawn/split calls fire; additionally, a per-window
    /// checkpoint between iterations makes cancellation
    /// responsive for multi-window restores where each window
    /// involves multiple expensive spawn operations.
    pub async fn restore_with_cx(
        &self,
        cx: &crate::cx::Cx,
        snapshot: &TopologySnapshot,
    ) -> crate::Result<RestoreResult> {
        cx.checkpoint()
            .map_err(|err| restore_cancelled_error("restore_layout.restore.preflight", err))?;

        // Validate the complete immutable snapshot before the first mux mutation.
        // A malformed later window/tab must never be discovered only after earlier
        // windows have already been created.
        let preflight = validate_restore_snapshot(snapshot, self.config.restore_working_dirs)?;
        let mut state = RestoreAccumulator::with_capacity(preflight.pane_count);

        info!(
            windows = snapshot.windows.len(),
            "starting layout restoration from snapshot (cx-first)"
        );

        for (win_idx, window) in snapshot.windows.iter().enumerate() {
            cx.checkpoint().map_err(|err| {
                restore_cancelled_error("restore_layout.restore.between_windows", err)
            })?;
            match self
                .restore_window(cx, window, win_idx, &preflight, &mut state)
                .await
            {
                Ok(restored_any_tabs) => {
                    if restored_any_tabs {
                        state.result.windows_created += 1;
                    }
                }
                Err(e) => {
                    cx.checkpoint().map_err(|err| {
                        restore_cancelled_error("restore_layout.restore.window_failed", err)
                    })?;
                    if state.claim_failure_log_slot() {
                        warn!(window_id = window.window_id, "failed to restore window");
                    }
                    if !self.config.continue_on_error {
                        return Err(e);
                    }
                }
            }
        }

        info!(
            windows = state.result.windows_created,
            tabs = state.result.tabs_created,
            panes = state.result.panes_created,
            failed = state.result.failed_panes.len(),
            suppressed_failure_logs = state.failure_logs_suppressed,
            "layout restoration complete (cx-first)"
        );

        Ok(state.result)
    }

    /// Restore a single window and all its tabs.
    async fn restore_window(
        &self,
        cx: &crate::cx::Cx,
        window: &WindowSnapshot,
        win_idx: usize,
        preflight: &RestorePreflight,
        state: &mut RestoreAccumulator,
    ) -> crate::Result<bool> {
        debug!(
            window_id = window.window_id,
            tabs = window.tabs.len(),
            "restoring window"
        );

        let mut restored_window_id = None;
        let selected_tab_index = window.active_tab_index.unwrap_or(0);
        let mut selected_tab = None;
        let mut restored_any_tabs = false;

        for (tab_idx, tab) in window.tabs.iter().enumerate() {
            cx.checkpoint().map_err(|err| {
                restore_cancelled_error("restore_layout.restore_window.between_tabs", err)
            })?;
            let target = SpawnTarget {
                window_id: restored_window_id,
                new_window: restored_window_id.is_none(),
            };
            match self
                .restore_tab(cx, tab, win_idx, tab_idx, target, preflight, state)
                .await
            {
                Ok(restored_tab) => {
                    restored_window_id.get_or_insert(restored_tab.window_id);
                    if tab_idx == selected_tab_index {
                        selected_tab = Some(restored_tab);
                    }
                    state.result.tabs_created += 1;
                    restored_any_tabs = true;
                }
                Err(e) => {
                    cx.checkpoint().map_err(|err| {
                        restore_cancelled_error("restore_layout.restore_window.tab_failed", err)
                    })?;
                    let affected =
                        record_failed_tree(state, &tab.pane_tree, FAILURE_TAB_RESTORE);
                    if state.claim_failure_log_slot() {
                        warn!(
                            tab_id = tab.tab_id,
                            affected_panes = affected,
                            "failed to restore tab"
                        );
                    }
                    if !self.config.continue_on_error {
                        return Err(e);
                    }
                }
            }
        }

        // Each successfully restored tab selected its own active pane during
        // `restore_tab`. Re-select the window's recorded tab last because
        // spawning later tabs changes the selected tab as a side effect.
        if let Some(selected_tab) = selected_tab {
            cx.checkpoint().map_err(|err| {
                restore_cancelled_error("restore_layout.restore_window.before_activate", err)
            })?;
            if self
                .wezterm
                .activate_pane_with_cx(cx, selected_tab.active_new_pane_id)
                .await
                .is_err()
            {
                cx.checkpoint().map_err(|err| {
                    restore_cancelled_error("restore_layout.restore_window.activate_failed", err)
                })?;
                state.record_failure(selected_tab.active_old_pane_id, FAILURE_ACTIVATION);
                if state.claim_failure_log_slot() {
                    warn!(
                        pane_id = selected_tab.active_new_pane_id,
                        "failed to activate selected window tab"
                    );
                }
                if !self.config.continue_on_error {
                    return Err(restore_layout_error(
                        "restore_layout.restore_window.activate",
                        FAILURE_ACTIVATION,
                    ));
                }
            }
        }

        Ok(restored_any_tabs)
    }

    /// Restore a single tab with its pane tree.
    async fn restore_tab(
        &self,
        cx: &crate::cx::Cx,
        tab: &TabSnapshot,
        win_idx: usize,
        tab_idx: usize,
        spawn_target: SpawnTarget,
        preflight: &RestorePreflight,
        state: &mut RestoreAccumulator,
    ) -> crate::Result<RestoredTab> {
        debug!(
            tab_id = tab.tab_id,
            win_idx,
            tab_idx,
            ?spawn_target,
            "restoring tab"
        );
        cx.checkpoint().map_err(|err| {
            restore_cancelled_error("restore_layout.restore_tab.preflight", err)
        })?;

        // Get initial CWD from the first leaf in the pane tree.
        let initial_cwd = first_leaf_cwd(&tab.pane_tree, &preflight.normalized_cwds);

        // Spawn the initial pane for this tab.
        let root_pane_id = match self
            .wezterm
            .spawn_targeted_with_cx(cx, initial_cwd, None, spawn_target)
            .await
        {
            Ok(pane_id) => pane_id,
            Err(_) => {
                cx.checkpoint().map_err(|err| {
                    restore_cancelled_error("restore_layout.restore_tab.spawn_failed", err)
                })?;
                return Err(restore_layout_error(
                    "restore_layout.restore_tab.spawn",
                    FAILURE_TAB_RESTORE,
                ));
            }
        };
        let window_id = if let Some(window_id) = spawn_target.window_id {
            window_id
        } else {
            match self.wezterm.get_pane_with_cx(cx, root_pane_id).await {
                Ok(root_pane) => root_pane.window_id,
                Err(_) => {
                    cx.checkpoint().map_err(|err| {
                        restore_cancelled_error("restore_layout.restore_tab.lookup_failed", err)
                    })?;
                    return Err(restore_layout_error(
                        "restore_layout.restore_tab.lookup",
                        FAILURE_TAB_RESTORE,
                    ));
                }
            }
        };

        debug!(root_pane_id, tab_idx, "spawned root pane for tab");

        // Recursively restore the pane tree within this tab.
        self.restore_pane_tree(cx, &tab.pane_tree, root_pane_id, preflight, state)
            .await?;

        let preferred_old_pane_id = tab
            .active_pane_id
            .or_else(|| first_active_leaf_id(&tab.pane_tree))
            .or_else(|| first_leaf_id(&tab.pane_tree))
            .ok_or_else(|| {
                restore_layout_error(
                    "restore_layout.restore_tab.active",
                    "restored tab has no leaf pane",
                )
            })?;
        let (active_old_pane_id, active_new_pane_id) = state
            .result
            .pane_id_map
            .get(&preferred_old_pane_id)
            .copied()
            .map(|new_pane_id| (preferred_old_pane_id, new_pane_id))
            .or_else(|| first_mapped_leaf(&tab.pane_tree, &state.result.pane_id_map))
            .ok_or_else(|| {
                restore_layout_error(
                    "restore_layout.restore_tab.active",
                    "restored tab has no mapped pane",
                )
            })?;

        if self
            .wezterm
            .activate_pane_with_cx(cx, active_new_pane_id)
            .await
            .is_err()
        {
            cx.checkpoint().map_err(|err| {
                restore_cancelled_error("restore_layout.restore_tab.activate_failed", err)
            })?;
            state.record_failure(active_old_pane_id, FAILURE_ACTIVATION);
            if state.claim_failure_log_slot() {
                warn!(pane_id = active_new_pane_id, "failed to activate restored tab pane");
            }
            if !self.config.continue_on_error {
                return Err(restore_layout_error(
                    "restore_layout.restore_tab.activate",
                    FAILURE_ACTIVATION,
                ));
            }
        }

        Ok(RestoredTab {
            window_id,
            active_old_pane_id,
            active_new_pane_id,
        })
    }

    /// Recursively restore a pane tree.
    ///
    /// Uses explicit `Pin<Box<..>>` return type because async recursion
    /// requires boxing the future.
    fn restore_pane_tree<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        node: &'a PaneNode,
        current_pane_id: u64,
        preflight: &'a RestorePreflight,
        state: &'a mut RestoreAccumulator,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            cx.checkpoint().map_err(|err| {
                restore_cancelled_error("restore_layout.restore_pane_tree.node", err)
            })?;
            match node {
                PaneNode::Leaf { pane_id, .. } => {
                    state.result.pane_id_map.insert(*pane_id, current_pane_id);
                    state.result.panes_created += 1;

                    Ok(())
                }

                PaneNode::HSplit { children } => {
                    self.restore_split_children(
                        cx,
                        children,
                        current_pane_id,
                        SplitDirection::Bottom,
                        preflight,
                        state,
                    )
                    .await
                }

                PaneNode::VSplit { children } => {
                    self.restore_split_children(
                        cx,
                        children,
                        current_pane_id,
                        SplitDirection::Right,
                        preflight,
                        state,
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
        cx: &'a crate::cx::Cx,
        children: &'a [(f64, PaneNode)],
        current_pane_id: u64,
        direction: SplitDirection,
        preflight: &'a RestorePreflight,
        state: &'a mut RestoreAccumulator,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            // Peel right/bottom children from the outside in. `SplitSize` is
            // the size of the newly-created second child, so forward splitting
            // of the first pane reverses siblings. Reverse-prefix construction
            // preserves both the requested ratios and the recorded order:
            // for ratios [a,b,c], split c/(a+b+c), then b/(a+b), leaving a.
            let mut prefix_ratio: f64 = children.iter().map(|(ratio, _)| *ratio).sum();
            for (ratio, child) in children[1..].iter().rev() {
                cx.checkpoint().map_err(|err| {
                    restore_cancelled_error(
                        "restore_layout.restore_split_children.before_split",
                        err,
                    )
                })?;
                let percent = if self.config.restore_split_ratios {
                    Some(split_percent(*ratio, prefix_ratio))
                } else {
                    None
                };

                let cwd = first_leaf_cwd(child, &preflight.normalized_cwds);

                match self
                    .wezterm
                    .split_pane_with_cx(cx, current_pane_id, direction, cwd, percent)
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
                        self.restore_pane_tree(cx, child, new_pane_id, preflight, state)
                            .await?;
                    }
                    Err(_) => {
                        cx.checkpoint().map_err(|err| {
                            restore_cancelled_error(
                                "restore_layout.restore_split_children.split_failed",
                                err,
                            )
                        })?;
                        let affected = record_failed_tree(state, child, FAILURE_SPLIT);
                        if state.claim_failure_log_slot() {
                            warn!(
                                parent = current_pane_id,
                                affected_panes = affected,
                                "failed to create split pane"
                            );
                        }
                        if !self.config.continue_on_error {
                            return Err(restore_layout_error(
                                "restore_layout.restore_split_children.split",
                                FAILURE_SPLIT,
                            ));
                        }
                    }
                }
                prefix_ratio -= *ratio;
            }

            let (_, first_child) = &children[0];
            self.restore_pane_tree(cx, first_child, current_pane_id, preflight, state)
                .await?;

            Ok(())
        })
    }

}

// =============================================================================
// Helpers
// =============================================================================

fn validate_restore_snapshot(
    snapshot: &TopologySnapshot,
    restore_working_dirs: bool,
) -> crate::Result<RestorePreflight> {
    const OPERATION: &str = "restore_layout.preflight";

    if snapshot.schema_version != TOPOLOGY_SCHEMA_VERSION {
        return Err(restore_layout_error(
            OPERATION,
            "unsupported topology schema version",
        ));
    }
    if snapshot.windows.is_empty() {
        return Err(restore_layout_error(
            OPERATION,
            "topology contains no windows",
        ));
    }

    let mut seen_window_ids = HashSet::with_capacity(snapshot.windows.len());
    let mut seen_tab_ids = HashSet::new();
    let mut seen_pane_ids = HashSet::new();
    let mut normalized_cwds = HashMap::new();
    let mut pane_count = 0usize;

    for window in &snapshot.windows {
        if !seen_window_ids.insert(window.window_id) {
            return Err(restore_layout_error(
                OPERATION,
                "topology contains a duplicate window id",
            ));
        }
        if window.tabs.is_empty() {
            return Err(restore_layout_error(
                OPERATION,
                "topology contains an empty window",
            ));
        }
        if window
            .active_tab_index
            .is_some_and(|index| index >= window.tabs.len())
        {
            return Err(restore_layout_error(
                OPERATION,
                "topology contains an invalid active tab index",
            ));
        }

        for tab in &window.tabs {
            if !seen_tab_ids.insert(tab.tab_id) {
                return Err(restore_layout_error(
                    OPERATION,
                    "topology contains a duplicate tab id",
                ));
            }
            let (tab_panes, active_leaf_count) = validate_pane_tree(
                &tab.pane_tree,
                1,
                restore_working_dirs,
                &mut seen_pane_ids,
                &mut normalized_cwds,
            )?;
            if tab_panes == 0 {
                return Err(restore_layout_error(
                    OPERATION,
                    "topology contains a zero-leaf pane tree",
                ));
            }
            if active_leaf_count > 1 {
                return Err(restore_layout_error(
                    OPERATION,
                    "tab contains multiple active leaf markers",
                ));
            }
            if tab
                .active_pane_id
                .is_some_and(|pane_id| !pane_tree_contains(&tab.pane_tree, pane_id))
            {
                return Err(restore_layout_error(
                    OPERATION,
                    "tab active pane is outside its pane tree",
                ));
            }
            pane_count = pane_count.checked_add(tab_panes).ok_or_else(|| {
                restore_layout_error(OPERATION, "topology pane count overflowed")
            })?;
        }
    }

    if pane_count == 0 {
        return Err(restore_layout_error(
            OPERATION,
            "topology contains no pane leaves",
        ));
    }

    Ok(RestorePreflight {
        pane_count,
        normalized_cwds,
    })
}

fn validate_pane_tree(
    node: &PaneNode,
    depth: usize,
    restore_working_dirs: bool,
    seen_pane_ids: &mut HashSet<u64>,
    normalized_cwds: &mut HashMap<u64, String>,
) -> crate::Result<(usize, usize)> {
    const OPERATION: &str = "restore_layout.preflight";

    if depth > MAX_PANE_TREE_DEPTH {
        return Err(restore_layout_error(
            OPERATION,
            "topology pane tree exceeds the supported depth",
        ));
    }

    match node {
        PaneNode::Leaf {
            pane_id,
            rows,
            cols,
            cwd,
            is_active,
            ..
        } => {
            if *rows == 0 || *cols == 0 {
                return Err(restore_layout_error(
                    OPERATION,
                    "pane has zero terminal dimensions",
                ));
            }
            if !seen_pane_ids.insert(*pane_id) {
                return Err(restore_layout_error(
                    OPERATION,
                    "topology contains a duplicate pane id",
                ));
            }
            if restore_working_dirs && let Some(cwd) = cwd {
                let normalized = normalize_restored_cwd(cwd)
                    .map_err(|detail| restore_layout_error(OPERATION, detail))?;
                normalized_cwds.insert(*pane_id, normalized);
            }
            Ok((1, usize::from(*is_active)))
        }
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            if children.len() < 2 {
                return Err(restore_layout_error(
                    OPERATION,
                    "split node has fewer than two children",
                ));
            }
            let mut pane_count = 0usize;
            let mut active_leaf_count = 0usize;
            let mut ratio_sum = 0.0f64;
            for (ratio, child) in children {
                if !ratio.is_finite() || *ratio <= 0.0 {
                    return Err(restore_layout_error(
                        OPERATION,
                        "split node has a non-positive or non-finite ratio",
                    ));
                }
                ratio_sum += *ratio;
                if !ratio_sum.is_finite() {
                    return Err(restore_layout_error(
                        OPERATION,
                        "split ratio sum overflowed",
                    ));
                }
                let (child_panes, child_active) = validate_pane_tree(
                    child,
                    depth + 1,
                    restore_working_dirs,
                    seen_pane_ids,
                    normalized_cwds,
                )?;
                pane_count = pane_count.checked_add(child_panes).ok_or_else(|| {
                    restore_layout_error(OPERATION, "topology pane count overflowed")
                })?;
                active_leaf_count = active_leaf_count.checked_add(child_active).ok_or_else(|| {
                    restore_layout_error(OPERATION, "topology active pane count overflowed")
                })?;
            }
            Ok((pane_count, active_leaf_count))
        }
    }
}

fn normalize_restored_cwd(cwd: &str) -> Result<String, &'static str> {
    let (path, is_file_uri) = if let Some(rest) = cwd.strip_prefix("file://") {
        if rest.starts_with('/') {
            (rest, true)
        } else {
            let slash = rest
                .find('/')
                .ok_or("file URI working directory has no absolute path")?;
            let authority = &rest[..slash];
            if !authority.eq_ignore_ascii_case("localhost") {
                return Err("non-local file URI working directory requires a mux domain");
            }
            (&rest[slash..], true)
        }
    } else {
        (cwd, false)
    };

    let decoded = if is_file_uri {
        strict_percent_decode(path)?
    } else {
        path.to_string()
    };
    if decoded.is_empty() || !Path::new(&decoded).is_absolute() {
        return Err("working directory is not an absolute local path");
    }
    if decoded
        .chars()
        .any(|ch| ch.is_control() || matches!(ch as u32, 0x7f..=0x9f))
    {
        return Err("working directory contains a terminal control character");
    }
    Ok(decoded)
}

fn strict_percent_decode(value: &str) -> Result<String, &'static str> {
    fn hex_value(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err("working directory contains truncated percent encoding");
            }
            let high = hex_value(bytes[index + 1])
                .ok_or("working directory contains invalid percent encoding")?;
            let low = hex_value(bytes[index + 2])
                .ok_or("working directory contains invalid percent encoding")?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| "working directory is not valid UTF-8")
}

fn first_leaf_id(node: &PaneNode) -> Option<u64> {
    match node {
        PaneNode::Leaf { pane_id, .. } => Some(*pane_id),
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => children
            .first()
            .and_then(|(_, child)| first_leaf_id(child)),
    }
}

fn first_active_leaf_id(node: &PaneNode) -> Option<u64> {
    match node {
        PaneNode::Leaf {
            pane_id,
            is_active,
            ..
        } => is_active.then_some(*pane_id),
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => children
            .iter()
            .find_map(|(_, child)| first_active_leaf_id(child)),
    }
}

fn first_leaf_cwd<'a>(
    node: &PaneNode,
    normalized_cwds: &'a HashMap<u64, String>,
) -> Option<&'a str> {
    first_leaf_id(node).and_then(|pane_id| normalized_cwds.get(&pane_id).map(String::as_str))
}

fn first_mapped_leaf(
    node: &PaneNode,
    pane_id_map: &HashMap<u64, u64>,
) -> Option<(u64, u64)> {
    match node {
        PaneNode::Leaf { pane_id, .. } => pane_id_map
            .get(pane_id)
            .copied()
            .map(|new_pane_id| (*pane_id, new_pane_id)),
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => children
            .iter()
            .find_map(|(_, child)| first_mapped_leaf(child, pane_id_map)),
    }
}

fn pane_tree_contains(node: &PaneNode, pane_id: u64) -> bool {
    match node {
        PaneNode::Leaf {
            pane_id: candidate,
            ..
        } => *candidate == pane_id,
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => children
            .iter()
            .any(|(_, child)| pane_tree_contains(child, pane_id)),
    }
}

fn split_percent(child_ratio: f64, prefix_ratio: f64) -> u8 {
    if !child_ratio.is_finite()
        || !prefix_ratio.is_finite()
        || child_ratio <= 0.0
        || prefix_ratio <= 0.0
    {
        return 50;
    }
    ((child_ratio / prefix_ratio * 100.0).round() as u8).clamp(1, 99)
}

fn record_failed_tree(
    state: &mut RestoreAccumulator,
    node: &PaneNode,
    reason: &'static str,
) -> usize {
    match node {
        PaneNode::Leaf { pane_id, .. } => {
            usize::from(state.record_unmapped_failure(*pane_id, reason))
        }
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => children
            .iter()
            .map(|(_, child)| record_failed_tree(state, child, reason))
            .sum(),
    }
}

/// Collect all leaf pane IDs from a pane tree.
#[cfg(test)]
fn collect_leaf_ids(node: &PaneNode) -> Vec<u64> {
    let mut ids = Vec::new();
    collect_leaf_ids_inner(node, &mut ids);
    ids
}

#[cfg(test)]
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

/// Minimal shell escaping for paths (wraps in single quotes).
#[cfg(test)]
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
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::session_topology::{PaneNode, TabSnapshot, TopologySnapshot, WindowSnapshot};
    use crate::wezterm::{
        MockWezterm, SpawnTarget, WeztermFuture, WeztermHandle, WeztermInterface,
    };

    fn test_runtime_error(operation: &'static str, detail: impl Into<String>) -> crate::Error {
        crate::Error::RuntimeOperation {
            operation,
            source: crate::error::RuntimeOperationSource::Backend(detail.into()),
        }
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
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
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn make_restorer(mock: Arc<MockWezterm>) -> LayoutRestorer {
        LayoutRestorer::new(mock, RestoreConfig::default())
    }

    struct AlwaysFailSpawnWezterm;

    struct InstrumentedWezterm {
        inner: Arc<MockWezterm>,
        fail_splits: bool,
        fail_activations: bool,
        split_attempts: Mutex<Vec<(u64, SplitDirection, Option<u8>)>>,
        activation_attempts: Mutex<Vec<u64>>,
        get_pane_attempts: Mutex<Vec<u64>>,
    }

    impl InstrumentedWezterm {
        fn new(inner: Arc<MockWezterm>, fail_splits: bool, fail_activations: bool) -> Self {
            Self {
                inner,
                fail_splits,
                fail_activations,
                split_attempts: Mutex::new(Vec::new()),
                activation_attempts: Mutex::new(Vec::new()),
                get_pane_attempts: Mutex::new(Vec::new()),
            }
        }
    }

    impl WeztermInterface for AlwaysFailSpawnWezterm {
        fn list_panes(&self) -> WeztermFuture<'_, Vec<crate::wezterm::PaneInfo>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, crate::wezterm::PaneInfo> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_get_pane",
                    format!("unexpected get_pane({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn get_text(&self, pane_id: u64, _: bool) -> WeztermFuture<'_, String> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_get_text",
                    format!("unexpected get_text({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn send_text(&self, pane_id: u64, _: &str) -> WeztermFuture<'_, ()> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_send_text",
                    format!("unexpected send_text({pane_id}) on failing spawn mock"),
                ))
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
            Box::pin(async {
                Err(test_runtime_error(
                    "restore_layout.test.spawn",
                    "simulated spawn failure",
                ))
            })
        }

        fn spawn_targeted(
            &self,
            _: Option<&str>,
            _: Option<&str>,
            _: SpawnTarget,
        ) -> WeztermFuture<'_, u64> {
            Box::pin(async {
                Err(test_runtime_error(
                    "restore_layout.test.spawn_targeted",
                    "simulated spawn failure",
                ))
            })
        }

        fn split_pane(
            &self,
            pane_id: u64,
            _: crate::wezterm::SplitDirection,
            _: Option<&str>,
            _: Option<u8>,
        ) -> WeztermFuture<'_, u64> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_split_pane",
                    format!("unexpected split_pane({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_activate_pane",
                    format!("unexpected activate_pane({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn get_pane_direction(
            &self,
            pane_id: u64,
            _: crate::wezterm::MoveDirection,
        ) -> WeztermFuture<'_, Option<u64>> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_get_pane_direction",
                    format!("unexpected get_pane_direction({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_kill_pane",
                    format!("unexpected kill_pane({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn zoom_pane(&self, pane_id: u64, _: bool) -> WeztermFuture<'_, ()> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_zoom_pane",
                    format!("unexpected zoom_pane({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn circuit_status(&self) -> crate::circuit_breaker::CircuitBreakerStatus {
            crate::circuit_breaker::CircuitBreakerStatus::default()
        }
    }

    impl WeztermInterface for InstrumentedWezterm {
        fn list_panes(&self) -> WeztermFuture<'_, Vec<crate::wezterm::PaneInfo>> {
            self.inner.list_panes()
        }

        fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, crate::wezterm::PaneInfo> {
            self.get_pane_attempts
                .lock()
                .expect("get-pane attempt lock poisoned")
                .push(pane_id);
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
            pane_id: u64,
            direction: crate::wezterm::SplitDirection,
            cwd: Option<&str>,
            percent: Option<u8>,
        ) -> WeztermFuture<'_, u64> {
            self.split_attempts
                .lock()
                .expect("split-attempt lock poisoned")
                .push((pane_id, direction, percent));
            if self.fail_splits {
                Box::pin(async {
                    Err(test_runtime_error(
                        "restore_layout.test.split_pane",
                        "simulated split failure",
                    ))
                })
            } else {
                self.inner.split_pane(pane_id, direction, cwd, percent)
            }
        }

        fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.activation_attempts
                .lock()
                .expect("activation-attempt lock poisoned")
                .push(pane_id);
            if self.fail_activations {
                Box::pin(async {
                    Err(test_runtime_error(
                        "restore_layout.test.activate_pane",
                        "simulated activation failure",
                    ))
                })
            } else {
                self.inner.activate_pane(pane_id)
            }
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
    fn restore_cancelled_error_uses_structured_runtime_operation() {
        let err = restore_cancelled_error("restore_layout.test_checkpoint", "caller cancelled");

        match err {
            crate::Error::RuntimeOperation { operation, source } => {
                assert_eq!(operation, "restore_layout.test_checkpoint");
                assert_eq!(
                    source,
                    crate::error::RuntimeOperationSource::Cancelled(
                        "caller capability stopped".to_string()
                    )
                );
            }
            other => panic!("expected structured runtime operation, got {other:?}"),
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

    /// ft-xbnl0.2.3 Cx-first: `restore_with_cx` must produce
    /// the same RestoreResult shape (pane/window/tab counts,
    /// pane_id_map coverage, no failures) as `restore` for an
    /// uncancelled cx.
    #[test]
    fn restore_with_cx_matches_legacy() {
        run_async_test(async {
            let tree = hsplit(vec![(0.5, leaf(101, None)), (0.5, leaf(102, None))]);
            let snapshot = single_tab_snapshot(tree);

            let legacy_mock = Arc::new(MockWezterm::new());
            let legacy_restorer = make_restorer(legacy_mock.clone());
            let legacy = legacy_restorer.restore(&snapshot).await.unwrap();

            let cx_mock = Arc::new(MockWezterm::new());
            let cx_restorer = make_restorer(cx_mock.clone());
            let cx = crate::cx::for_request();
            let cx_first = cx_restorer.restore_with_cx(&cx, &snapshot).await.unwrap();

            assert_eq!(legacy.panes_created, cx_first.panes_created);
            assert_eq!(legacy.windows_created, cx_first.windows_created);
            assert_eq!(legacy.tabs_created, cx_first.tabs_created);
            assert_eq!(legacy.failed_panes.len(), cx_first.failed_panes.len());
            assert_eq!(legacy.pane_id_map.len(), cx_first.pane_id_map.len());
            assert!(cx_first.pane_id_map.contains_key(&101));
            assert!(cx_first.pane_id_map.contains_key(&102));
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
            let instrumented = Arc::new(InstrumentedWezterm::new(
                mock.clone(),
                false,
                false,
            ));
            let restorer = LayoutRestorer::new(
                instrumented.clone(),
                RestoreConfig::default(),
            );
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
            assert_eq!(
                *instrumented
                    .activation_attempts
                    .lock()
                    .expect("activation-attempt lock poisoned"),
                vec![
                    result.pane_id_map[&1],
                    result.pane_id_map[&3],
                    result.pane_id_map[&1],
                ],
                "each tab must select its own active pane before the window reselects its recorded tab"
            );
            assert_eq!(
                instrumented
                    .get_pane_attempts
                    .lock()
                    .expect("get-pane attempt lock poisoned")
                    .len(),
                1,
                "only the first tab needs a pane lookup to discover its new window id"
            );
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
    fn restore_activation_failure_is_reported_once_for_mapped_pane() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let instrumented = Arc::new(InstrumentedWezterm::new(mock, false, true));
            let restorer = LayoutRestorer::new(
                instrumented.clone(),
                RestoreConfig::default(),
            );
            let snapshot = single_tab_snapshot(leaf(1, None));

            let result = restorer.restore(&snapshot).await.unwrap();

            assert!(result.pane_id_map.contains_key(&1));
            assert_eq!(
                result.failed_panes,
                vec![(1, FAILURE_ACTIVATION.to_string())]
            );
            assert_eq!(
                *instrumented
                    .activation_attempts
                    .lock()
                    .expect("activation-attempt lock poisoned"),
                vec![result.pane_id_map[&1], result.pane_id_map[&1]],
                "the tab activation and final window-tab activation both fail, but the pane failure is deduplicated"
            );
        });
    }

    #[test]
    fn normalize_cwd_file_uri() {
        assert_eq!(
            normalize_restored_cwd("file:///home/user").unwrap(),
            "/home/user"
        );
        assert_eq!(
            normalize_restored_cwd("file://localhost/home/user").unwrap(),
            "/home/user"
        );
        assert_eq!(normalize_restored_cwd("/home/user").unwrap(), "/home/user");
        assert_eq!(normalize_restored_cwd("file:///").unwrap(), "/");
    }

    #[test]
    fn normalize_cwd_plain_path() {
        assert_eq!(normalize_restored_cwd("/tmp/work").unwrap(), "/tmp/work");
        assert_eq!(
            normalize_restored_cwd("/tmp/100%literal").unwrap(),
            "/tmp/100%literal"
        );
        assert!(normalize_restored_cwd("relative/path").is_err());
    }

    #[test]
    fn split_percent_preserves_full_valid_range() {
        assert_eq!(split_percent(1.0, 2.0), 50);
        assert_eq!(split_percent(7.0, 10.0), 70);
        assert_eq!(split_percent(2.0, 3.0), 67);
        assert_eq!(split_percent(f64::MIN_POSITIVE, 1.0), 1);
        assert_eq!(split_percent(1.0, 1.0), 99);
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
    fn restore_empty_snapshot_fails_before_mux_effects() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let snapshot = TopologySnapshot {
                schema_version: 1,
                captured_at: 1000,
                workspace_id: None,
                windows: vec![],
            };

            restorer
                .restore(&snapshot)
                .await
                .expect_err("empty topology must fail closed");
            assert!(mock.list_panes().await.unwrap().is_empty());
        });
    }

    #[test]
    fn restore_empty_window_fails_before_mux_effects() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let snapshot = TopologySnapshot {
                schema_version: TOPOLOGY_SCHEMA_VERSION,
                captured_at: 1000,
                workspace_id: None,
                windows: vec![WindowSnapshot {
                    window_id: 0,
                    title: None,
                    position: None,
                    size: None,
                    tabs: Vec::new(),
                    active_tab_index: None,
                }],
            };

            restorer
                .restore(&snapshot)
                .await
                .expect_err("empty windows must fail closed");
            assert!(mock.list_panes().await.unwrap().is_empty());
        });
    }

    #[test]
    fn restore_invalid_cwd_fails_before_mux_effects() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let snapshot = single_tab_snapshot(leaf(1, Some("relative/path")));

            restorer
                .restore(&snapshot)
                .await
                .expect_err("relative working directories must fail closed");
            assert!(mock.list_panes().await.unwrap().is_empty());
        });
    }

    #[test]
    fn restore_zero_leaf_tree_fails_before_mux_effects() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let snapshot = single_tab_snapshot(hsplit(Vec::new()));

            restorer
                .restore(&snapshot)
                .await
                .expect_err("zero-leaf pane trees must fail closed");
            assert!(mock.list_panes().await.unwrap().is_empty());
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
                vec![(1, FAILURE_TAB_RESTORE.to_string())]
            );
        });
    }

    #[test]
    fn restore_window_partial_failure_does_not_mark_restored_leaf_failed() {
        run_async_test(async {
            let inner = Arc::new(MockWezterm::new());
            let wezterm: WeztermHandle = Arc::new(InstrumentedWezterm::new(
                inner.clone(),
                true,
                false,
            ));
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
            let snapshot = TopologySnapshot {
                schema_version: TOPOLOGY_SCHEMA_VERSION,
                captured_at: 1000,
                workspace_id: None,
                windows: vec![window.clone()],
            };
            let preflight = validate_restore_snapshot(&snapshot, true).unwrap();
            let mut state = RestoreAccumulator::with_capacity(preflight.pane_count);
            let cx = crate::cx::for_request();

            assert!(
                restorer
                    .restore_window(&cx, &window, 0, &preflight, &mut state)
                    .await
                    .is_err()
            );
            // Reverse-prefix construction must create the outer sibling before
            // assigning the existing root pane to the first child.
            assert!(state.result.pane_id_map.is_empty());
            assert_eq!(
                state.result.failed_panes,
                vec![(2, FAILURE_SPLIT.to_string())]
            );
        });
    }

    #[test]
    fn restore_partial_first_tab_reuses_created_window_for_later_tabs() {
        run_async_test(async {
            let inner = Arc::new(MockWezterm::new());
            let wezterm: WeztermHandle = Arc::new(InstrumentedWezterm::new(
                inner.clone(),
                true,
                false,
            ));
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
                vec![(2, FAILURE_SPLIT.to_string())]
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
        let cwd_map = HashMap::from([(1, "/home/user".to_string())]);
        assert_eq!(first_leaf_cwd(&node, &cwd_map), Some("/home/user"));
    }

    #[test]
    fn first_leaf_cwd_from_split() {
        let node = vsplit(vec![
            (0.5, leaf(1, Some("/tmp"))),
            (0.5, leaf(2, Some("/home"))),
        ]);
        let cwd_map = HashMap::from([(1, "/tmp".to_string()), (2, "/home".to_string())]);
        assert_eq!(first_leaf_cwd(&node, &cwd_map), Some("/tmp"));
    }

    #[test]
    fn first_leaf_cwd_none() {
        let node = leaf(1, None);
        assert_eq!(first_leaf_cwd(&node, &HashMap::new()), None);
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
            let inner = Arc::new(MockWezterm::new());
            let instrumented = Arc::new(InstrumentedWezterm::new(
                inner,
                false,
                false,
            ));
            let restorer = LayoutRestorer::new(
                instrumented.clone(),
                RestoreConfig::default(),
            );
            let tree = vsplit(vec![
                (1.0, leaf(1, None)),
                (2.0, leaf(2, None)),
                (7.0, leaf(3, None)),
            ]);
            let snapshot = single_tab_snapshot(tree);

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 3);
            let new_ids: std::collections::HashSet<_> = result.pane_id_map.values().collect();
            assert_eq!(new_ids.len(), 3);
            let split_attempts = instrumented
                .split_attempts
                .lock()
                .expect("split-attempt lock poisoned")
                .clone();
            assert_eq!(
                split_attempts,
                vec![
                    (result.pane_id_map[&1], SplitDirection::Right, Some(70)),
                    (result.pane_id_map[&1], SplitDirection::Right, Some(67)),
                ]
            );
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
            let pane = mock.pane_state(new_id).await.unwrap();
            assert_eq!(pane.cwd, "/home/agent/project");
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
            let tree = leaf(1, Some("/captured/working-directory"));
            let snapshot = single_tab_snapshot(tree);

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 1);
            let new_id = result.pane_id_map[&1];
            let pane = mock.pane_state(new_id).await.unwrap();
            assert_eq!(pane.cwd, "/home/user", "backend default cwd should remain");
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
        assert!(normalize_restored_cwd("").is_err());
    }

    #[test]
    fn normalize_cwd_rejects_non_local_file_uri_authority() {
        assert!(normalize_restored_cwd("file://myhost/home/user").is_err());
    }

    #[test]
    fn normalize_cwd_file_uri_empty_authority() {
        assert!(normalize_restored_cwd("file://").is_err());
    }

    #[test]
    fn normalize_cwd_file_uri_root_only() {
        assert_eq!(normalize_restored_cwd("file:///").unwrap(), "/");
    }

    #[test]
    fn normalize_cwd_file_uri_with_spaces() {
        assert_eq!(
            normalize_restored_cwd("file:///home/my user/project").unwrap(),
            "/home/my user/project"
        );
    }

    #[test]
    fn normalize_cwd_rejects_tilde_path() {
        assert!(normalize_restored_cwd("~/projects").is_err());
    }

    #[test]
    fn normalize_cwd_rejects_foreign_windows_path_on_unix() {
        if cfg!(unix) {
            assert!(normalize_restored_cwd("C:\\Users\\test").is_err());
        }
    }

    #[test]
    fn normalize_cwd_strictly_decodes_percent_encoding() {
        assert_eq!(
            normalize_restored_cwd("file:///home/my%20project").unwrap(),
            "/home/my project"
        );
        assert!(normalize_restored_cwd("file:///home/%zz").is_err());
        assert!(normalize_restored_cwd("file:///home/%").is_err());
        assert!(normalize_restored_cwd("file:///home/%00bad").is_err());
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
        let cwd_map = HashMap::from([(1, "/deep/path".to_string())]);
        assert_eq!(first_leaf_cwd(&tree, &cwd_map), Some("/deep/path"));
    }

    #[test]
    fn first_leaf_cwd_file_uri_normalized() {
        let node = leaf(1, Some("file:///home/agent"));
        let cwd_map = HashMap::from([(
            1,
            normalize_restored_cwd("file:///home/agent").unwrap(),
        )]);
        assert_eq!(first_leaf_cwd(&node, &cwd_map), Some("/home/agent"));
    }

    #[test]
    fn first_leaf_cwd_empty_children() {
        let node = hsplit(vec![]);
        assert_eq!(first_leaf_cwd(&node, &HashMap::new()), None);
    }

    #[test]
    fn first_leaf_cwd_first_child_no_cwd_second_has() {
        // first_leaf_cwd returns the FIRST leaf's cwd, even if None
        let tree = vsplit(vec![
            (0.5, leaf(1, None)),
            (0.5, leaf(2, Some("/home/user"))),
        ]);
        let cwd_map = HashMap::from([(2, "/home/user".to_string())]);
        assert_eq!(first_leaf_cwd(&tree, &cwd_map), None);
    }

    #[test]
    fn first_leaf_cwd_hsplit_vs_vsplit() {
        let tree_h = hsplit(vec![(1.0, leaf(1, Some("/h")))]);
        let tree_v = vsplit(vec![(1.0, leaf(1, Some("/v")))]);
        assert_eq!(
            first_leaf_cwd(&tree_h, &HashMap::from([(1, "/h".to_string())])),
            Some("/h")
        );
        assert_eq!(
            first_leaf_cwd(&tree_v, &HashMap::from([(1, "/v".to_string())])),
            Some("/v")
        );
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
