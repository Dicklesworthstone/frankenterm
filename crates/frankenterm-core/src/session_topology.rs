//! Mux topology serializer for session persistence.
//!
//! Captures WezTerm mux session topology (windows, tabs, pane split trees)
//! into a versioned JSON format for storage in `mux_sessions.topology_json`.
//! Supports best-effort reconstruction after a crash via legacy heuristics over
//! pane dimensions. `PaneInfo` can carry coordinates and the direct mux
//! response carries an exact ordered tree, but the legacy builder below does
//! not consume that authority after the response is flattened. Schema v1
//! therefore cannot promise appearance fidelity.
//!
//! Schema v1 is a deterministic, pane-list-derived layout snapshot. It sorts
//! numeric window/tab/pane IDs and therefore does **not** preserve authoritative
//! user tab or pane order. The legacy builder leaves `active_tab_index` unknown because
//! pane activity identifies the active pane within each tab, not the selected
//! tab for the window. Durable order/active-tab capture must use the
//! richer identity and authority contract in
//! `docs/proposals/ft-7xqz4-8-10-1-tab-order-authority-contract.md`; the schema
//! migration is owned by `ft-interactive-swarm-product-convergence-7xqz4.8.10.7`.
//!
//! # Data flow
//!
//! ```text
//! wezterm cli list (JSON) → Vec<PaneInfo> → TopologySnapshot → topology_json
//!                                                ↑
//!                           legacy dimension-only split-tree heuristics
//! ```

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use thiserror::Error;

use crate::wezterm::PaneInfo;

// =============================================================================
// Core types
// =============================================================================

/// Current schema version for topology snapshots.
pub const TOPOLOGY_SCHEMA_VERSION: u32 = 1;

/// ft-nrqf7: hard byte cap on inputs accepted by `from_json`.
/// A crafted `.ft/snapshots/<session>.json` larger than this is
/// rejected before serde_json touches it, so an attacker cannot
/// exhaust memory by handing the recorder a 4 GB pane tree.
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

/// ft-nrqf7: hard cap on `PaneNode` HSplit/VSplit nesting depth.
///
/// `serde_json`'s default 128-level recursion guard prevents the
/// parser from blowing the stack on pathological inputs. Each
/// `PaneNode` level in a snapshot consumes roughly 4 JSON nesting
/// levels (the tagged-enum object + the `children` array + the
/// `(f64, PaneNode)` tuple-as-array + the inner object), so a hard
/// cap of 32 leaves comfortable headroom under the parser's 128
/// ceiling. Real-world fleets are typically <10 deep, so 32
/// preserves all legitimate captures while bricking quine-shaped
/// attack inputs.
pub const MAX_PANE_TREE_DEPTH: usize = 32;

/// Maximum windows accepted by the legacy pane-list checkpoint/restore schema.
///
/// These q4096 limits are an intentionally lower local admission envelope than
/// the maximum future ordered-protocol ceilings. They prevent legacy restore
/// from claiming a scale class that has no retained memory/latency proof.
pub const MAX_TOPOLOGY_WINDOWS: usize = 4_096;
/// Maximum tabs accepted across one legacy topology snapshot.
pub const MAX_TOPOLOGY_TABS: usize = 4_096;
/// Maximum pane leaves accepted across one legacy topology snapshot.
pub const MAX_TOPOLOGY_PANES: usize = 4_096;
/// Maximum split and leaf nodes accepted across one legacy topology snapshot.
pub const MAX_TOPOLOGY_NODES: usize = 8_192;
/// Maximum direct children accepted by one legacy split node.
pub const MAX_SPLIT_FANOUT: usize = 4_096;

/// Errors returned by `TopologySnapshot::from_json`. Distinguishes
/// the DoS guards (TooLarge / TooDeep) from genuine parse errors so
/// callers can log and alert without panicking.
#[derive(Debug, Error)]
pub enum TopologySnapshotError {
    /// Input bytes exceed `MAX_SNAPSHOT_BYTES`. ft-nrqf7 guard.
    #[error("topology snapshot too large: {bytes} bytes (limit {limit})")]
    TooLarge { bytes: usize, limit: usize },
    /// `PaneNode` HSplit/VSplit nesting exceeds `MAX_PANE_TREE_DEPTH`.
    /// ft-nrqf7 guard.
    #[error("topology pane-tree too deeply nested: depth {depth} (limit {limit})")]
    TooDeep { depth: usize, limit: usize },
    /// A bounded topology resource exceeds its admission limit.
    #[error("topology {resource} count exceeds limit: {count} (limit {limit})")]
    ResourceLimit {
        resource: &'static str,
        count: usize,
        limit: usize,
    },
    /// A topology is within numeric limits but cannot be restored faithfully
    /// because its IDs, active markers, dimensions, or split shape conflict.
    #[error("invalid topology structure: {reason}")]
    InvalidStructure { reason: &'static str },
    /// Underlying serde_json parse error.
    #[error("invalid topology snapshot JSON: {0}")]
    Json(#[from] serde_json::Error),
}

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

/// Counts the exact serialized size while retaining bytes only until the
/// restore input cap is crossed. This avoids allocating an arbitrarily large
/// intermediate string merely to discover that persistence must reject it.
struct BoundedTopologyJsonWriter {
    bytes: Vec<u8>,
    serialized_bytes: usize,
    limit: usize,
    overflowed: bool,
}

impl BoundedTopologyJsonWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            serialized_bytes: 0,
            limit,
            overflowed: false,
        }
    }
}

impl Write for BoundedTopologyJsonWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.serialized_bytes = self.serialized_bytes.saturating_add(buffer.len());
        if !self.overflowed && self.serialized_bytes <= self.limit {
            self.bytes.extend_from_slice(buffer);
        } else if !self.overflowed {
            self.overflowed = true;
            self.bytes.clear();
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// =============================================================================
// Capture / construction
// =============================================================================

impl TopologySnapshot {
    /// Build a `TopologySnapshot` from a list of panes.
    ///
    /// Groups panes by window and tab, then infers split structure within
    /// each tab from pane sizes. This legacy builder sorts window, tab, and
    /// pane IDs for
    /// reproducibility; it is not an authoritative user-tab-order capture.
    #[must_use]
    pub fn from_panes(panes: &[PaneInfo], captured_at: u64) -> (Self, CaptureReport) {
        let first_workspace = panes.first().and_then(|pane| pane.workspace.as_deref());
        let workspace_id = if panes
            .iter()
            .all(|pane| pane.workspace.as_deref() == first_workspace)
        {
            first_workspace.map(str::to_owned)
        } else {
            None
        };

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
            let mut tabs_map = windows_map
                .remove(&window_id)
                .expect("window id came from the same grouping map");
            let mut tabs = Vec::new();

            let mut tab_ids: Vec<u64> = tabs_map.keys().copied().collect();
            tab_ids.sort_unstable();

            for tab_id in tab_ids {
                // Pane enumeration order is not a visual-order authority. Use
                // a stable source-ID order so the same pane set cannot produce
                // a different topology hash and redundant checkpoint merely
                // because a backend returned its list in another order.
                let mut tab_panes = tabs_map
                    .remove(&tab_id)
                    .expect("tab id came from the same grouping map");
                tab_panes.sort_unstable_by_key(|pane| pane.pane_id);
                let (pane_tree, quality) = infer_split_tree(&tab_panes);
                inference_quality.insert(tab_id, quality);

                let active_pane_id = tab_panes.iter().find(|p| p.is_active).map(|p| p.pane_id);

                tabs.push(TabSnapshot {
                    tab_id,
                    // `PaneInfo::title` is pane/application title, not tab-title
                    // authority. The flattened legacy surface cannot persist an
                    // authoritative tab title.
                    title: None,
                    pane_tree,
                    active_pane_id,
                });
                total_tabs += 1;
            }

            windows.push(WindowSnapshot {
                window_id,
                title: None,
                position: None,
                size: None,
                tabs,
                // `PaneInfo::is_active` is per-tab active-pane authority. It
                // cannot identify the selected tab for this window, so the
                // legacy pane-list builder must not manufacture that state.
                active_tab_index: None,
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

    /// Validate and serialize a topology for durable snapshot persistence.
    ///
    /// Unlike [`Self::to_json`], this writer fails closed on restore-invalid
    /// structure and never retains more than [`MAX_SNAPSHOT_BYTES`] of output.
    /// Derived topology structs contain no map-valued fields, so serde's stable
    /// declaration/sequence order is already the canonical persisted order.
    pub(crate) fn to_persistence_json(&self) -> Result<String, TopologySnapshotError> {
        self.validate_persistence_semantics()?;
        let mut writer = BoundedTopologyJsonWriter::new(MAX_SNAPSHOT_BYTES);
        serde_json::to_writer(&mut writer, self)?;
        if writer.overflowed {
            return Err(TopologySnapshotError::TooLarge {
                bytes: writer.serialized_bytes,
                limit: MAX_SNAPSHOT_BYTES,
            });
        }
        String::from_utf8(writer.bytes).map_err(|_error| TopologySnapshotError::InvalidStructure {
            reason: "topology serializer emitted non-UTF-8 JSON",
        })
    }

    /// Deserialize a topology snapshot from JSON.
    ///
    /// ft-nrqf7: enforces bounded-input and bounded-topology guards before
    /// returning a snapshot:
    ///
    /// * Total input bytes ≤ `MAX_SNAPSHOT_BYTES` (64 MB). A crafted
    ///   `.ft/snapshots/<session>.json` larger than this is rejected
    ///   without invoking serde_json, so the recorder cannot be
    ///   memory-exhausted by simply handing it a multi-gigabyte file.
    /// * `PaneNode` recursion depth ≤ [`MAX_PANE_TREE_DEPTH`].
    ///   `serde_json`'s built-in 128-level recursion limit prevents
    ///   stack overflow during parsing; the post-parse walk catches
    ///   inputs that satisfy the parser's stack guard but still
    ///   represent pathological non-real input.
    /// * Window, tab, pane, total-node, and split-fanout counts stay within
    ///   the public admission constants. These post-parse limits prevent a
    ///   shallow but extremely wide snapshot from amplifying into restore
    ///   maps, failure ledgers, and mux mutations.
    ///
    /// # Errors
    /// Returns [`TopologySnapshotError::TooLarge`] if the input exceeds
    /// the byte cap, [`TopologySnapshotError::TooDeep`] if the parsed
    /// `pane_tree` exceeds the depth cap,
    /// [`TopologySnapshotError::ResourceLimit`] if a topology count exceeds
    /// its admission limit, or
    /// [`TopologySnapshotError::Json`] for ordinary parse errors.
    pub fn from_json(json: &str) -> Result<Self, TopologySnapshotError> {
        if json.len() > MAX_SNAPSHOT_BYTES {
            return Err(TopologySnapshotError::TooLarge {
                bytes: json.len(),
                limit: MAX_SNAPSHOT_BYTES,
            });
        }

        let snapshot: Self = serde_json::from_str(json)?;

        snapshot.validate_resource_limits()?;

        Ok(snapshot)
    }

    /// Validate topology resource bounds without recursion.
    ///
    /// Layout restoration invokes this for programmatically constructed
    /// snapshots as well as snapshots parsed through [`Self::from_json`], so
    /// bypassing JSON cannot bypass admission.
    ///
    /// # Errors
    ///
    /// Returns [`TopologySnapshotError::TooDeep`] when a pane tree exceeds the
    /// depth limit, or [`TopologySnapshotError::ResourceLimit`] when a window,
    /// tab, pane, node, or split-fanout count exceeds its legacy admission cap.
    pub fn validate_resource_limits(&self) -> Result<(), TopologySnapshotError> {
        if self.windows.len() > MAX_TOPOLOGY_WINDOWS {
            return Err(TopologySnapshotError::ResourceLimit {
                resource: "windows",
                count: self.windows.len(),
                limit: MAX_TOPOLOGY_WINDOWS,
            });
        }

        let mut tab_count = 0_usize;
        let mut pane_count = 0_usize;
        let mut node_count = 0_usize;
        let mut stack = Vec::new();

        for window in &self.windows {
            tab_count = tab_count.saturating_add(window.tabs.len());
            if tab_count > MAX_TOPOLOGY_TABS {
                return Err(TopologySnapshotError::ResourceLimit {
                    resource: "tabs",
                    count: tab_count,
                    limit: MAX_TOPOLOGY_TABS,
                });
            }

            for tab in &window.tabs {
                stack.push((&tab.pane_tree, 1_usize));
                while let Some((node, depth)) = stack.pop() {
                    if depth > MAX_PANE_TREE_DEPTH {
                        return Err(TopologySnapshotError::TooDeep {
                            depth,
                            limit: MAX_PANE_TREE_DEPTH,
                        });
                    }

                    node_count = node_count.saturating_add(1);
                    if node_count > MAX_TOPOLOGY_NODES {
                        return Err(TopologySnapshotError::ResourceLimit {
                            resource: "nodes",
                            count: node_count,
                            limit: MAX_TOPOLOGY_NODES,
                        });
                    }

                    match node {
                        PaneNode::Leaf { .. } => {
                            pane_count = pane_count.saturating_add(1);
                            if pane_count > MAX_TOPOLOGY_PANES {
                                return Err(TopologySnapshotError::ResourceLimit {
                                    resource: "panes",
                                    count: pane_count,
                                    limit: MAX_TOPOLOGY_PANES,
                                });
                            }
                        }
                        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
                            if children.len() > MAX_SPLIT_FANOUT {
                                return Err(TopologySnapshotError::ResourceLimit {
                                    resource: "split_fanout",
                                    count: children.len(),
                                    limit: MAX_SPLIT_FANOUT,
                                });
                            }
                            for (_, child) in children.iter().rev() {
                                stack.push((child, depth.saturating_add(1)));
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Validate semantics required for a newly persisted topology to remain
    /// admissible to the layout restorer.
    ///
    /// Empty whole-session snapshots remain valid shutdown evidence. Any
    /// represented window/tab/tree, however, must have unique identities,
    /// valid focus markers, positive leaf dimensions, and real split nodes.
    ///
    /// # Errors
    ///
    /// Returns the resource error from [`Self::validate_resource_limits`] or a
    /// finite [`TopologySnapshotError::InvalidStructure`] classification.
    pub fn validate_persistence_semantics(&self) -> Result<(), TopologySnapshotError> {
        self.validate_resource_limits()?;
        if self.schema_version != TOPOLOGY_SCHEMA_VERSION {
            return Err(TopologySnapshotError::InvalidStructure {
                reason: "unsupported topology schema version",
            });
        }

        let mut seen_window_ids = HashSet::with_capacity(self.windows.len());
        let mut seen_tab_ids = HashSet::new();
        let mut seen_pane_ids = HashSet::new();
        let mut stack = Vec::new();

        for window in &self.windows {
            if !seen_window_ids.insert(window.window_id) {
                return Err(TopologySnapshotError::InvalidStructure {
                    reason: "duplicate window id",
                });
            }
            if window.tabs.is_empty() {
                return Err(TopologySnapshotError::InvalidStructure {
                    reason: "window contains no tabs",
                });
            }
            if window
                .active_tab_index
                .is_some_and(|index| index >= window.tabs.len())
            {
                return Err(TopologySnapshotError::InvalidStructure {
                    reason: "active tab index is outside its window",
                });
            }

            for tab in &window.tabs {
                if !seen_tab_ids.insert(tab.tab_id) {
                    return Err(TopologySnapshotError::InvalidStructure {
                        reason: "duplicate tab id",
                    });
                }

                let mut active_leaf_id = None;
                let mut explicit_active_found = tab.active_pane_id.is_none();
                stack.push(&tab.pane_tree);
                while let Some(node) = stack.pop() {
                    match node {
                        PaneNode::Leaf {
                            pane_id,
                            rows,
                            cols,
                            is_active,
                            ..
                        } => {
                            if *rows == 0 || *cols == 0 {
                                return Err(TopologySnapshotError::InvalidStructure {
                                    reason: "pane has zero terminal dimensions",
                                });
                            }
                            if !seen_pane_ids.insert(*pane_id) {
                                return Err(TopologySnapshotError::InvalidStructure {
                                    reason: "duplicate pane id",
                                });
                            }
                            if tab.active_pane_id == Some(*pane_id) {
                                explicit_active_found = true;
                            }
                            if *is_active && active_leaf_id.replace(*pane_id).is_some() {
                                return Err(TopologySnapshotError::InvalidStructure {
                                    reason: "tab contains multiple active pane markers",
                                });
                            }
                        }
                        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
                            if children.len() < 2 {
                                return Err(TopologySnapshotError::InvalidStructure {
                                    reason: "split node has fewer than two children",
                                });
                            }
                            for (ratio, child) in children.iter().rev() {
                                if !ratio.is_finite() || *ratio <= 0.0 {
                                    return Err(TopologySnapshotError::InvalidStructure {
                                        reason: "split ratio is non-positive or non-finite",
                                    });
                                }
                                stack.push(child);
                            }
                        }
                    }
                }

                if !explicit_active_found {
                    return Err(TopologySnapshotError::InvalidStructure {
                        reason: "active pane id is outside its tab",
                    });
                }
                if matches!(
                    (tab.active_pane_id, active_leaf_id),
                    (Some(explicit), Some(marked)) if explicit != marked
                ) {
                    return Err(TopologySnapshotError::InvalidStructure {
                        reason: "active pane id contradicts the active pane marker",
                    });
                }
            }
        }

        Ok(())
    }

    /// Count total panes across all windows and tabs.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        let mut count = 0_usize;
        let mut stack = Vec::new();
        for window in self.windows.iter().rev() {
            for tab in window.tabs.iter().rev() {
                stack.push(&tab.pane_tree);
            }
        }
        while let Some(node) = stack.pop() {
            match node {
                PaneNode::Leaf { .. } => count = count.saturating_add(1),
                PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
                    for (_, child) in children.iter().rev() {
                        stack.push(child);
                    }
                }
            }
        }
        count
    }

    /// Collect all pane IDs in the snapshot.
    #[must_use]
    pub fn pane_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        let mut stack = Vec::new();
        for window in self.windows.iter().rev() {
            for tab in window.tabs.iter().rev() {
                stack.push(&tab.pane_tree);
            }
        }
        while let Some(node) = stack.pop() {
            match node {
                PaneNode::Leaf { pane_id, .. } => ids.push(*pane_id),
                PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
                    for (_, child) in children.iter().rev() {
                        stack.push(child);
                    }
                }
            }
        }
        ids
    }
}

impl PaneNode {
    /// Count panes in this subtree.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        let mut count = 0_usize;
        let mut stack = vec![self];
        while let Some(node) = stack.pop() {
            match node {
                Self::Leaf { .. } => count = count.saturating_add(1),
                Self::HSplit { children } | Self::VSplit { children } => {
                    stack.extend(children.iter().map(|(_, child)| child));
                }
            }
        }
        count
    }

    /// Collect all pane IDs in this subtree.
    pub fn collect_pane_ids(&self, out: &mut Vec<u64>) {
        let mut stack = vec![self];
        while let Some(node) = stack.pop() {
            match node {
                Self::Leaf { pane_id, .. } => out.push(*pane_id),
                Self::HSplit { children } | Self::VSplit { children } => {
                    for (_, child) in children.iter().rev() {
                        stack.push(child);
                    }
                }
            }
        }
    }

    /// Greatest nesting depth of this subtree (Leaf = 1).
    ///
    /// ft-nrqf7: walked iteratively with an explicit stack so a
    /// pathologically deep tree cannot blow the program stack while
    /// being measured. The byte cap in `from_json` keeps the queue
    /// bounded.
    #[must_use]
    pub fn depth(&self) -> usize {
        let mut stack: Vec<(&PaneNode, usize)> = vec![(self, 1)];
        let mut max_depth = 0usize;
        while let Some((node, depth)) = stack.pop() {
            if depth > max_depth {
                max_depth = depth;
            }
            match node {
                Self::Leaf { .. } => {}
                Self::HSplit { children } | Self::VSplit { children } => {
                    for (_, child) in children {
                        stack.push((child, depth + 1));
                    }
                }
            }
        }
        max_depth
    }
}

// =============================================================================
// Split-tree inference
// =============================================================================

/// Internal representation of pane geometry for split inference.
#[derive(Debug, Clone, Copy)]
struct PaneGeometry<'a> {
    pane: &'a PaneInfo,
    rows: u16,
    cols: u16,
}

impl<'a> PaneGeometry<'a> {
    fn from_pane_info(pane: &'a PaneInfo) -> Self {
        Self {
            pane,
            rows: schema_v1_dimension(pane.effective_rows()),
            cols: schema_v1_dimension(pane.effective_cols()),
        }
    }

    fn to_leaf(self) -> PaneNode {
        PaneNode::Leaf {
            pane_id: self.pane.pane_id,
            rows: self.rows,
            cols: self.cols,
            cwd: self.pane.cwd.clone(),
            title: self.pane.title.clone(),
            is_active: self.pane.is_active,
        }
    }
}

fn schema_v1_dimension(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// Infer split tree structure from a set of panes within a single tab.
///
/// Strategy:
/// 1. Single pane → Leaf node.
/// 2. Multiple panes with one common dimension → one flat split.
/// 3. Equal or mixed dimensions without coordinates → deterministic flat
///    fallback, explicitly marked non-authoritative.
fn infer_split_tree(panes: &[&PaneInfo]) -> (PaneNode, InferenceQuality) {
    let geometries: Vec<PaneGeometry<'_>> = panes
        .iter()
        .copied()
        .map(PaneGeometry::from_pane_info)
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
        _ => infer_flat_split(&geometries),
    }
}

/// Uses common dimensions as a two-pane orientation hint. With more than two
/// panes the same dimensions cannot prove grouping or order, so the produced
/// flat tree is always marked as fallback even when an orientation is chosen.
fn infer_flat_split(geometries: &[PaneGeometry<'_>]) -> (PaneNode, InferenceQuality) {
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
        // Dimensions hint at orientation and ratios, but not which pane is
        // topmost. Canonical pane-ID order is deterministic, not visual-order
        // authority, so every multi-pane legacy tree remains a fallback.
        (
            PaneNode::HSplit { children },
            InferenceQuality::FlatFallback,
        )
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
        // Dimensions hint at orientation and ratios, but not which pane is
        // leftmost. Canonical pane-ID order is deterministic, not visual-order
        // authority, so every multi-pane legacy tree remains a fallback.
        (
            PaneNode::VSplit { children },
            InferenceQuality::FlatFallback,
        )
    } else if all_same_cols && all_same_rows {
        // Equal dimensions carry no split-direction authority. Preserve the
        // canonical pane-ID order, but mark the arbitrary
        // flat orientation as fallback rather than inferred truth.
        let n = geometries.len() as f64;
        let children: Vec<(f64, PaneNode)> =
            geometries.iter().map(|g| (1.0 / n, g.to_leaf())).collect();
        (
            PaneNode::VSplit { children },
            InferenceQuality::FlatFallback,
        )
    } else {
        // This legacy dimension-only heuristic does not consume PaneInfo
        // coordinates. Mixed dimensions therefore cannot establish grouping,
        // orientation, or visual order here. Do not sort panes by width and
        // present that manufactured tree as inferred authority.
        let n = geometries.len() as f64;
        let children: Vec<(f64, PaneNode)> =
            geometries.iter().map(|g| (1.0 / n, g.to_leaf())).collect();
        (
            PaneNode::HSplit { children },
            InferenceQuality::FlatFallback,
        )
    }
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
                let size_match = old_leaf.rows == schema_v1_dimension(new_pane.effective_rows())
                    && old_leaf.cols == schema_v1_dimension(new_pane.effective_cols());
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
// Native mux lifecycle model (ft-3681t.2.1 slice)
// =============================================================================

/// Canonical lifecycle entity kinds for native mux orchestration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEntityKind {
    Session,
    Window,
    Pane,
    Agent,
}

impl LifecycleEntityKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Window => "window",
            Self::Pane => "pane",
            Self::Agent => "agent",
        }
    }
}

/// Stable globally-addressable identity for mux lifecycle entities.
///
/// The identity key intentionally includes workspace + domain + local_id +
/// generation to support:
/// - cross-domain orchestration
/// - deterministic addressing for robot/workflow APIs
/// - recovery after reconnect/restart where local IDs may be reused
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LifecycleIdentity {
    pub kind: LifecycleEntityKind,
    pub workspace_id: String,
    pub domain: String,
    pub local_id: u64,
    pub generation: u64,
}

impl LifecycleIdentity {
    #[must_use]
    pub fn new(
        kind: LifecycleEntityKind,
        workspace_id: impl Into<String>,
        domain: impl Into<String>,
        local_id: u64,
        generation: u64,
    ) -> Self {
        Self {
            kind,
            workspace_id: workspace_id.into(),
            domain: domain.into(),
            local_id,
            generation,
        }
    }

    /// Build a stable pane identity from `PaneInfo` + observed generation.
    #[must_use]
    pub fn from_pane_info(pane: &PaneInfo, generation: u64) -> Self {
        let workspace_id = pane
            .workspace
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let domain = pane.inferred_domain();
        Self::new(
            LifecycleEntityKind::Pane,
            workspace_id,
            domain,
            pane.pane_id,
            generation,
        )
    }

    /// Build a stable window identity from `PaneInfo` + observed generation.
    #[must_use]
    pub fn window_from_pane_info(pane: &PaneInfo, generation: u64) -> Self {
        let workspace_id = pane
            .workspace
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let domain = pane.inferred_domain();
        Self::new(
            LifecycleEntityKind::Window,
            workspace_id,
            domain,
            pane.window_id,
            generation,
        )
    }

    /// Build a stable session identity from `PaneInfo` + observed generation.
    #[must_use]
    pub fn session_from_pane_info(pane: &PaneInfo, session_id: u64, generation: u64) -> Self {
        let workspace_id = pane
            .workspace
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let domain = pane.inferred_domain();
        Self::new(
            LifecycleEntityKind::Session,
            workspace_id,
            domain,
            session_id,
            generation,
        )
    }

    /// Build a stable agent identity.
    #[must_use]
    pub fn agent(
        workspace_id: impl Into<String>,
        domain: impl Into<String>,
        agent_id: u64,
        generation: u64,
    ) -> Self {
        Self::new(
            LifecycleEntityKind::Agent,
            workspace_id,
            domain,
            agent_id,
            generation,
        )
    }

    /// Stable string key used by orchestration, storage, and robot references.
    #[must_use]
    pub fn stable_key(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.workspace_id,
            self.domain,
            self.kind.as_str(),
            self.local_id,
            self.generation
        )
    }
}

/// Shared lifecycle transition events across session/window/pane/agent models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEvent {
    Provisioned,
    StartWork,
    WorkFinished,
    Attach,
    Detach,
    DrainRequested,
    DrainCompleted,
    PeerDisconnected,
    Recover,
    ForceClose,
}

/// Result of a transition attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionOutcome<S> {
    pub next_state: S,
    /// True when the transition was accepted as a safe no-op.
    pub idempotent: bool,
}

impl<S> TransitionOutcome<S> {
    fn changed(next_state: S) -> Self {
        Self {
            next_state,
            idempotent: false,
        }
    }

    fn noop(state: S) -> Self {
        Self {
            next_state: state,
            idempotent: true,
        }
    }
}

/// Lifecycle transition error for illegal state/event combinations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleTransitionError {
    pub entity: LifecycleEntityKind,
    pub state: &'static str,
    pub event: LifecycleEvent,
}

impl std::fmt::Display for LifecycleTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid {} lifecycle transition: state={} event={:?}",
            self.entity.as_str(),
            self.state,
            self.event
        )
    }
}

impl std::error::Error for LifecycleTransitionError {}

/// Session lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycleState {
    Provisioning,
    Active,
    Draining,
    Recovering,
    Closed,
}

/// Window lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowLifecycleState {
    Provisioning,
    Active,
    Draining,
    Recovering,
    Closed,
}

/// Pane lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MuxPaneLifecycleState {
    Provisioning,
    Ready,
    Running,
    Draining,
    Orphaned,
    Closed,
}

/// Agent lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLifecycleState {
    Registered,
    Attached,
    Detached,
    Retired,
}

fn invalid_transition(
    entity: LifecycleEntityKind,
    state: &'static str,
    event: LifecycleEvent,
) -> LifecycleTransitionError {
    LifecycleTransitionError {
        entity,
        state,
        event,
    }
}

/// Apply a session lifecycle transition.
pub fn transition_session_state(
    current: SessionLifecycleState,
    event: LifecycleEvent,
) -> Result<TransitionOutcome<SessionLifecycleState>, LifecycleTransitionError> {
    use LifecycleEvent::{
        DrainCompleted, DrainRequested, ForceClose, PeerDisconnected, Provisioned, Recover,
    };
    use SessionLifecycleState::{Active, Closed, Draining, Provisioning, Recovering};

    match (current, event) {
        (Provisioning, Provisioned) => Ok(TransitionOutcome::changed(Active)),
        (Active, DrainRequested) => Ok(TransitionOutcome::changed(Draining)),
        (Draining, DrainRequested) => Ok(TransitionOutcome::noop(Draining)),
        (Draining, DrainCompleted) => Ok(TransitionOutcome::changed(Closed)),
        (Active | Draining, PeerDisconnected) => Ok(TransitionOutcome::changed(Recovering)),
        (Recovering, Recover) => Ok(TransitionOutcome::changed(Active)),
        (Closed, Recover) => Ok(TransitionOutcome::changed(Recovering)),
        (Closed, ForceClose) => Ok(TransitionOutcome::noop(Closed)),
        (Provisioning | Active | Draining | Recovering, ForceClose) => {
            Ok(TransitionOutcome::changed(Closed))
        }
        (state, e) => Err(invalid_transition(
            LifecycleEntityKind::Session,
            state_label_session(state),
            e,
        )),
    }
}

/// Apply a window lifecycle transition.
pub fn transition_window_state(
    current: WindowLifecycleState,
    event: LifecycleEvent,
) -> Result<TransitionOutcome<WindowLifecycleState>, LifecycleTransitionError> {
    use LifecycleEvent::{
        DrainCompleted, DrainRequested, ForceClose, PeerDisconnected, Provisioned, Recover,
    };
    use WindowLifecycleState::{Active, Closed, Draining, Provisioning, Recovering};

    match (current, event) {
        (Provisioning, Provisioned) => Ok(TransitionOutcome::changed(Active)),
        (Active, DrainRequested) => Ok(TransitionOutcome::changed(Draining)),
        (Draining, DrainRequested) => Ok(TransitionOutcome::noop(Draining)),
        (Draining, DrainCompleted) => Ok(TransitionOutcome::changed(Closed)),
        (Active | Draining, PeerDisconnected) => Ok(TransitionOutcome::changed(Recovering)),
        (Recovering, Recover) => Ok(TransitionOutcome::changed(Active)),
        (Closed, Recover) => Ok(TransitionOutcome::changed(Recovering)),
        (Closed, ForceClose) => Ok(TransitionOutcome::noop(Closed)),
        (Provisioning | Active | Draining | Recovering, ForceClose) => {
            Ok(TransitionOutcome::changed(Closed))
        }
        (state, e) => Err(invalid_transition(
            LifecycleEntityKind::Window,
            state_label_window(state),
            e,
        )),
    }
}

/// Apply a pane lifecycle transition.
pub fn transition_pane_state(
    current: MuxPaneLifecycleState,
    event: LifecycleEvent,
) -> Result<TransitionOutcome<MuxPaneLifecycleState>, LifecycleTransitionError> {
    use LifecycleEvent::{
        DrainCompleted, DrainRequested, ForceClose, PeerDisconnected, Provisioned, Recover,
        StartWork, WorkFinished,
    };
    use MuxPaneLifecycleState::{Closed, Draining, Orphaned, Provisioning, Ready, Running};

    match (current, event) {
        (Provisioning, Provisioned) => Ok(TransitionOutcome::changed(Ready)),
        (Ready, StartWork) => Ok(TransitionOutcome::changed(Running)),
        (Running, WorkFinished) => Ok(TransitionOutcome::changed(Ready)),
        (Ready | Running, DrainRequested) => Ok(TransitionOutcome::changed(Draining)),
        (Draining, DrainRequested) => Ok(TransitionOutcome::noop(Draining)),
        (Draining, DrainCompleted) => Ok(TransitionOutcome::changed(Closed)),
        (Ready | Running | Draining, PeerDisconnected) => Ok(TransitionOutcome::changed(Orphaned)),
        (Orphaned, Recover) => Ok(TransitionOutcome::changed(Ready)),
        (Closed, ForceClose) => Ok(TransitionOutcome::noop(Closed)),
        (Provisioning | Ready | Running | Draining | Orphaned, ForceClose) => {
            Ok(TransitionOutcome::changed(Closed))
        }
        (state, e) => Err(invalid_transition(
            LifecycleEntityKind::Pane,
            state_label_pane(state),
            e,
        )),
    }
}

/// Apply an agent lifecycle transition.
pub fn transition_agent_state(
    current: AgentLifecycleState,
    event: LifecycleEvent,
) -> Result<TransitionOutcome<AgentLifecycleState>, LifecycleTransitionError> {
    use AgentLifecycleState::{Attached, Detached, Registered, Retired};
    use LifecycleEvent::{Attach, Detach, ForceClose};

    match (current, event) {
        (Registered | Detached, Attach) => Ok(TransitionOutcome::changed(Attached)),
        (Attached, Attach) => Ok(TransitionOutcome::noop(Attached)),
        (Attached, Detach) => Ok(TransitionOutcome::changed(Detached)),
        (Detached, Detach) => Ok(TransitionOutcome::noop(Detached)),
        (Retired, ForceClose) => Ok(TransitionOutcome::noop(Retired)),
        (Registered | Attached | Detached, ForceClose) => Ok(TransitionOutcome::changed(Retired)),
        (state, e) => Err(invalid_transition(
            LifecycleEntityKind::Agent,
            state_label_agent(state),
            e,
        )),
    }
}

fn state_label_session(state: SessionLifecycleState) -> &'static str {
    match state {
        SessionLifecycleState::Provisioning => "provisioning",
        SessionLifecycleState::Active => "active",
        SessionLifecycleState::Draining => "draining",
        SessionLifecycleState::Recovering => "recovering",
        SessionLifecycleState::Closed => "closed",
    }
}

fn state_label_window(state: WindowLifecycleState) -> &'static str {
    match state {
        WindowLifecycleState::Provisioning => "provisioning",
        WindowLifecycleState::Active => "active",
        WindowLifecycleState::Draining => "draining",
        WindowLifecycleState::Recovering => "recovering",
        WindowLifecycleState::Closed => "closed",
    }
}

fn state_label_pane(state: MuxPaneLifecycleState) -> &'static str {
    match state {
        MuxPaneLifecycleState::Provisioning => "provisioning",
        MuxPaneLifecycleState::Ready => "ready",
        MuxPaneLifecycleState::Running => "running",
        MuxPaneLifecycleState::Draining => "draining",
        MuxPaneLifecycleState::Orphaned => "orphaned",
        MuxPaneLifecycleState::Closed => "closed",
    }
}

fn state_label_agent(state: AgentLifecycleState) -> &'static str {
    match state {
        AgentLifecycleState::Registered => "registered",
        AgentLifecycleState::Attached => "attached",
        AgentLifecycleState::Detached => "detached",
        AgentLifecycleState::Retired => "retired",
    }
}

/// Runtime lifecycle state for any canonical lifecycle entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "state", rename_all = "snake_case")]
pub enum LifecycleState {
    Session(SessionLifecycleState),
    Window(WindowLifecycleState),
    Pane(MuxPaneLifecycleState),
    Agent(AgentLifecycleState),
}

impl LifecycleState {
    #[must_use]
    pub const fn kind(self) -> LifecycleEntityKind {
        match self {
            Self::Session(_) => LifecycleEntityKind::Session,
            Self::Window(_) => LifecycleEntityKind::Window,
            Self::Pane(_) => LifecycleEntityKind::Pane,
            Self::Agent(_) => LifecycleEntityKind::Agent,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Session(state) => state_label_session(state),
            Self::Window(state) => state_label_window(state),
            Self::Pane(state) => state_label_pane(state),
            Self::Agent(state) => state_label_agent(state),
        }
    }
}

/// Apply a lifecycle transition against the canonical runtime state union.
pub fn apply_lifecycle_state_transition(
    current: LifecycleState,
    event: LifecycleEvent,
) -> Result<TransitionOutcome<LifecycleState>, LifecycleTransitionError> {
    match current {
        LifecycleState::Session(state) => {
            transition_session_state(state, event).map(|outcome| TransitionOutcome {
                next_state: LifecycleState::Session(outcome.next_state),
                idempotent: outcome.idempotent,
            })
        }
        LifecycleState::Window(state) => {
            transition_window_state(state, event).map(|outcome| TransitionOutcome {
                next_state: LifecycleState::Window(outcome.next_state),
                idempotent: outcome.idempotent,
            })
        }
        LifecycleState::Pane(state) => {
            transition_pane_state(state, event).map(|outcome| TransitionOutcome {
                next_state: LifecycleState::Pane(outcome.next_state),
                idempotent: outcome.idempotent,
            })
        }
        LifecycleState::Agent(state) => {
            transition_agent_state(state, event).map(|outcome| TransitionOutcome {
                next_state: LifecycleState::Agent(outcome.next_state),
                idempotent: outcome.idempotent,
            })
        }
    }
}

/// Stored lifecycle record for one globally addressable entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleEntityRecord {
    pub identity: LifecycleIdentity,
    pub state: LifecycleState,
    pub version: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<LifecycleEvent>,
}

impl LifecycleEntityRecord {
    #[must_use]
    pub fn stable_key(&self) -> String {
        self.identity.stable_key()
    }
}

/// Transition request metadata used by policy/orchestration/robot callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleTransitionContext {
    pub timestamp_ms: u64,
    pub component: String,
    pub correlation_id: String,
    pub scenario_id: String,
    pub reason_code: String,
}

impl LifecycleTransitionContext {
    #[must_use]
    pub fn new(
        timestamp_ms: u64,
        component: impl Into<String>,
        correlation_id: impl Into<String>,
        scenario_id: impl Into<String>,
        reason_code: impl Into<String>,
    ) -> Self {
        Self {
            timestamp_ms,
            component: component.into(),
            correlation_id: correlation_id.into(),
            scenario_id: scenario_id.into(),
            reason_code: reason_code.into(),
        }
    }
}

/// One transition request against a registered lifecycle entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleTransitionRequest {
    pub identity: LifecycleIdentity,
    pub event: LifecycleEvent,
    #[serde(default)]
    pub expected_version: Option<u64>,
    pub context: LifecycleTransitionContext,
}

/// Decision class for lifecycle transition handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleDecision {
    Applied,
    Noop,
    Rejected,
}

/// Structured transition log entry for auditability and replay triage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleTransitionLogEntry {
    pub timestamp_ms: u64,
    pub component: String,
    pub correlation_id: String,
    pub scenario_id: String,
    pub identity_key: String,
    pub entity: LifecycleEntityKind,
    pub event: LifecycleEvent,
    pub input_state: String,
    pub output_state: String,
    pub decision: LifecycleDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<u64>,
    pub actual_version: u64,
    pub reason_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// Result of applying one lifecycle transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleApplyResult {
    pub record: LifecycleEntityRecord,
    pub version_changed: bool,
    pub idempotent: bool,
}

/// Errors from lifecycle registry operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEngineError {
    EntityNotFound {
        identity_key: String,
    },
    KindMismatch {
        identity_kind: LifecycleEntityKind,
        state_kind: LifecycleEntityKind,
    },
    ConcurrencyConflict {
        identity_key: String,
        expected_version: u64,
        actual_version: u64,
    },
    InvalidContext {
        field: &'static str,
    },
    Transition(LifecycleTransitionError),
}

impl std::fmt::Display for LifecycleEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EntityNotFound { identity_key } => {
                write!(f, "lifecycle entity not found: {identity_key}")
            }
            Self::KindMismatch {
                identity_kind,
                state_kind,
            } => write!(
                f,
                "lifecycle kind mismatch: identity={} state={}",
                identity_kind.as_str(),
                state_kind.as_str()
            ),
            Self::ConcurrencyConflict {
                identity_key,
                expected_version,
                actual_version,
            } => write!(
                f,
                "lifecycle concurrency conflict: key={identity_key} expected={expected_version} actual={actual_version}"
            ),
            Self::InvalidContext { field } => {
                write!(f, "lifecycle transition context missing field: {field}")
            }
            Self::Transition(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for LifecycleEngineError {}

impl From<LifecycleTransitionError> for LifecycleEngineError {
    fn from(value: LifecycleTransitionError) -> Self {
        Self::Transition(value)
    }
}

const LIFECYCLE_ERROR_ENTITY_NOT_FOUND: &str = "native_mux.lifecycle.entity_not_found";
const LIFECYCLE_ERROR_VERSION_CONFLICT: &str = "native_mux.lifecycle.version_conflict";
const LIFECYCLE_ERROR_INVALID_CONTEXT: &str = "native_mux.lifecycle.invalid_context";
const LIFECYCLE_ERROR_INVALID_TRANSITION: &str = "native_mux.lifecycle.invalid_transition";

/// Canonical lifecycle registry for native mux entities.
///
/// Concurrency semantics:
/// - caller may provide `expected_version` for optimistic write protection
/// - accepted idempotent transitions do not bump `version`
/// - non-idempotent transitions bump `version` by exactly 1
///
/// Recovery semantics:
/// - transitions flow through state-machine guards (`Recover`, disconnect)
/// - rejection outcomes are logged with stable reason/error codes
#[derive(Debug, Clone, Default)]
pub struct LifecycleRegistry {
    entities: HashMap<String, LifecycleEntityRecord>,
    transition_log: Vec<LifecycleTransitionLogEntry>,
}

impl LifecycleRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a lifecycle registry baseline from observed panes.
    ///
    /// Sessions/windows are marked `active`; panes are seeded as:
    /// - active pane -> `running`
    /// - non-active pane -> `ready`
    ///
    /// A synthetic `session_id=0` is used per `(workspace, domain, generation)`.
    pub fn bootstrap_from_panes(
        panes: &[PaneInfo],
        generation: u64,
        timestamp_ms: u64,
    ) -> Result<Self, LifecycleEngineError> {
        let mut registry = Self::new();
        for pane in panes {
            let session = LifecycleIdentity::session_from_pane_info(pane, 0, generation);
            if registry.get(&session).is_none() {
                let _ = registry.register_entity(
                    session,
                    LifecycleState::Session(SessionLifecycleState::Active),
                    timestamp_ms,
                )?;
            }

            let window = LifecycleIdentity::window_from_pane_info(pane, generation);
            if registry.get(&window).is_none() {
                let _ = registry.register_entity(
                    window,
                    LifecycleState::Window(WindowLifecycleState::Active),
                    timestamp_ms,
                )?;
            }

            let pane_identity = LifecycleIdentity::from_pane_info(pane, generation);
            let pane_state = if pane.is_active {
                LifecycleState::Pane(MuxPaneLifecycleState::Running)
            } else {
                LifecycleState::Pane(MuxPaneLifecycleState::Ready)
            };
            let pane_key = pane_identity.stable_key();
            if let Some(existing) = registry.entities.get_mut(&pane_key) {
                // Duplicate pane rows can appear during connector/list races.
                // Preserve the strongest observed bootstrap state.
                if pane.is_active
                    && matches!(
                        existing.state,
                        LifecycleState::Pane(MuxPaneLifecycleState::Ready)
                    )
                {
                    existing.state = LifecycleState::Pane(MuxPaneLifecycleState::Running);
                    existing.updated_at_ms = timestamp_ms;
                }
                continue;
            }
            let _ = registry.register_entity(pane_identity, pane_state, timestamp_ms)?;
        }
        Ok(registry)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    #[must_use]
    pub fn transition_log(&self) -> &[LifecycleTransitionLogEntry] {
        &self.transition_log
    }

    /// Serialize transition log entries for diagnostics/evidence capture.
    ///
    /// # Errors
    /// Returns an error if log serialization fails.
    pub fn transition_log_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.transition_log)
    }

    /// Snapshot records sorted by stable key for deterministic downstream use.
    #[must_use]
    pub fn snapshot(&self) -> Vec<LifecycleEntityRecord> {
        let mut items: Vec<LifecycleEntityRecord> = self.entities.values().cloned().collect();
        items.sort_unstable_by_key(LifecycleEntityRecord::stable_key);
        items
    }

    /// Serialize a deterministic lifecycle snapshot for diagnostics/evidence.
    ///
    /// # Errors
    /// Returns an error if snapshot serialization fails.
    pub fn snapshot_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.snapshot())
    }

    #[must_use]
    pub fn get(&self, identity: &LifecycleIdentity) -> Option<&LifecycleEntityRecord> {
        self.entities.get(&identity.stable_key())
    }

    #[must_use]
    pub fn entity_count_by_kind(&self, kind: LifecycleEntityKind) -> usize {
        self.entities
            .values()
            .filter(|record| record.identity.kind == kind)
            .count()
    }

    /// Register or replace one lifecycle entity record.
    pub fn register_entity(
        &mut self,
        identity: LifecycleIdentity,
        state: LifecycleState,
        timestamp_ms: u64,
    ) -> Result<LifecycleEntityRecord, LifecycleEngineError> {
        let state_kind = state.kind();
        if identity.kind != state_kind {
            return Err(LifecycleEngineError::KindMismatch {
                identity_kind: identity.kind,
                state_kind,
            });
        }

        let record = LifecycleEntityRecord {
            identity: identity.clone(),
            state,
            version: 0,
            updated_at_ms: timestamp_ms,
            last_event: None,
        };
        self.entities.insert(identity.stable_key(), record.clone());
        Ok(record)
    }

    /// Apply one lifecycle transition request with optimistic concurrency checks.
    pub fn apply_transition(
        &mut self,
        request: LifecycleTransitionRequest,
    ) -> Result<LifecycleApplyResult, LifecycleEngineError> {
        let identity_key = request.identity.stable_key();
        if let Err(err) = validate_transition_context(&request.context) {
            let (entity, input_state, actual_version) =
                if let Some(record) = self.entities.get(&identity_key) {
                    (record.identity.kind, record.state.label(), record.version)
                } else {
                    (request.identity.kind, "unknown", 0)
                };
            self.log_rejection(
                &request,
                identity_key.clone(),
                entity,
                input_state,
                input_state,
                actual_version,
                Some(LIFECYCLE_ERROR_INVALID_CONTEXT),
            );
            return Err(err);
        }

        let Some(existing_record) = self.entities.get(&identity_key).cloned() else {
            self.log_rejection(
                &request,
                identity_key.clone(),
                request.identity.kind,
                "unknown",
                "unknown",
                0,
                Some(LIFECYCLE_ERROR_ENTITY_NOT_FOUND),
            );
            return Err(LifecycleEngineError::EntityNotFound { identity_key });
        };

        if let Some(expected_version) = request.expected_version {
            if expected_version != existing_record.version {
                let actual_version = existing_record.version;
                let input_state = existing_record.state.label().to_string();
                self.log_rejection(
                    &request,
                    identity_key.clone(),
                    existing_record.identity.kind,
                    &input_state,
                    &input_state,
                    actual_version,
                    Some(LIFECYCLE_ERROR_VERSION_CONFLICT),
                );
                return Err(LifecycleEngineError::ConcurrencyConflict {
                    identity_key,
                    expected_version,
                    actual_version,
                });
            }
        }

        let current_state = existing_record.state;
        let outcome =
            apply_lifecycle_state_transition(current_state, request.event).map_err(|err| {
                let input_state = current_state.label().to_string();
                self.log_rejection(
                    &request,
                    identity_key.clone(),
                    current_state.kind(),
                    &input_state,
                    &input_state,
                    existing_record.version,
                    Some(LIFECYCLE_ERROR_INVALID_TRANSITION),
                );
                LifecycleEngineError::Transition(err)
            })?;

        let version_changed = !outcome.idempotent;
        let record = self
            .entities
            .get_mut(&identity_key)
            .expect("record exists after pre-check");
        if version_changed {
            record.state = outcome.next_state;
            record.version = record.version.saturating_add(1);
        }
        record.updated_at_ms = request.context.timestamp_ms;
        record.last_event = Some(request.event);

        let decision = if outcome.idempotent {
            LifecycleDecision::Noop
        } else {
            LifecycleDecision::Applied
        };

        let input_state = current_state.label().to_string();
        let output_state = record.state.label().to_string();
        self.transition_log.push(LifecycleTransitionLogEntry {
            timestamp_ms: request.context.timestamp_ms,
            component: request.context.component.clone(),
            correlation_id: request.context.correlation_id.clone(),
            scenario_id: request.context.scenario_id.clone(),
            identity_key: identity_key.clone(),
            entity: record.identity.kind,
            event: request.event,
            input_state: input_state.clone(),
            output_state: output_state.clone(),
            decision,
            expected_version: request.expected_version,
            actual_version: record.version,
            reason_code: request.context.reason_code.clone(),
            error_code: None,
        });

        tracing::info!(
            timestamp_ms = request.context.timestamp_ms,
            subsystem = "native_mux.lifecycle",
            component = %request.context.component,
            correlation_id = %request.context.correlation_id,
            scenario_id = %request.context.scenario_id,
            identity_key = %identity_key,
            entity = record.identity.kind.as_str(),
            event = ?request.event,
            input_state = %input_state,
            output_state = %output_state,
            decision = ?decision,
            expected_version = request.expected_version,
            actual_version = record.version,
            reason_code = %request.context.reason_code,
            "lifecycle transition processed"
        );

        Ok(LifecycleApplyResult {
            record: record.clone(),
            version_changed,
            idempotent: outcome.idempotent,
        })
    }

    // Rejection logs preserve the transition request and every operator-facing reason dimension.
    #[allow(clippy::too_many_arguments)]
    fn log_rejection(
        &mut self,
        request: &LifecycleTransitionRequest,
        identity_key: String,
        entity: LifecycleEntityKind,
        input_state: &str,
        output_state: &str,
        actual_version: u64,
        error_code: Option<&str>,
    ) {
        let entry = LifecycleTransitionLogEntry {
            timestamp_ms: request.context.timestamp_ms,
            component: request.context.component.clone(),
            correlation_id: request.context.correlation_id.clone(),
            scenario_id: request.context.scenario_id.clone(),
            identity_key: identity_key.clone(),
            entity,
            event: request.event,
            input_state: input_state.to_string(),
            output_state: output_state.to_string(),
            decision: LifecycleDecision::Rejected,
            expected_version: request.expected_version,
            actual_version,
            reason_code: request.context.reason_code.clone(),
            error_code: error_code.map(str::to_string),
        };
        self.transition_log.push(entry);

        tracing::warn!(
            timestamp_ms = request.context.timestamp_ms,
            subsystem = "native_mux.lifecycle",
            component = %request.context.component,
            correlation_id = %request.context.correlation_id,
            scenario_id = %request.context.scenario_id,
            identity_key = %identity_key,
            entity = entity.as_str(),
            event = ?request.event,
            input_state = %input_state,
            output_state = %output_state,
            decision = "rejected",
            expected_version = request.expected_version,
            actual_version,
            reason_code = %request.context.reason_code,
            error_code = error_code.unwrap_or(LIFECYCLE_ERROR_INVALID_CONTEXT),
            "lifecycle transition rejected"
        );
    }
}

fn validate_transition_context(
    context: &LifecycleTransitionContext,
) -> Result<(), LifecycleEngineError> {
    if context.component.trim().is_empty() {
        return Err(LifecycleEngineError::InvalidContext { field: "component" });
    }
    if context.correlation_id.trim().is_empty() {
        return Err(LifecycleEngineError::InvalidContext {
            field: "correlation_id",
        });
    }
    if context.scenario_id.trim().is_empty() {
        return Err(LifecycleEngineError::InvalidContext {
            field: "scenario_id",
        });
    }
    if context.reason_code.trim().is_empty() {
        return Err(LifecycleEngineError::InvalidContext {
            field: "reason_code",
        });
    }
    Ok(())
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
    // Fixture mirrors PaneInfo identity and geometry fields for readable scenarios.
    #[allow(clippy::too_many_arguments)]
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
            make_pane(1, 0, 0, 20, 80, None, None, false),
        ];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        let (tree, quality) = infer_split_tree(&refs);

        assert_eq!(quality, InferenceQuality::FlatFallback);
        match &tree {
            PaneNode::HSplit { children } => assert_eq!(children.len(), 2),
            _ => panic!("Expected HSplit"),
        }
    }

    #[test]
    fn split_tree_inference_vsplit() {
        // Same rows (24), different cols → VSplit
        let panes = [
            make_pane(0, 0, 0, 24, 40, None, None, true),
            make_pane(1, 0, 0, 24, 60, None, None, false),
        ];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        let (tree, quality) = infer_split_tree(&refs);

        assert_eq!(quality, InferenceQuality::FlatFallback);
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

        assert_eq!(tree.pane_count(), 2);
        assert_eq!(quality, InferenceQuality::FlatFallback);
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

    // ── Constants ──────────────────────────────────────────────────────

    #[test]
    fn topology_schema_version_is_one_batch2() {
        assert_eq!(TOPOLOGY_SCHEMA_VERSION, 1);
    }

    // ── TopologySnapshot::empty ────────────────────────────────────────

    #[test]
    fn empty_snapshot_fields() {
        let snap = TopologySnapshot::empty(42);
        assert_eq!(snap.schema_version, TOPOLOGY_SCHEMA_VERSION);
        assert_eq!(snap.captured_at, 42);
        assert!(snap.workspace_id.is_none());
        assert!(snap.windows.is_empty());
        assert_eq!(snap.pane_count(), 0);
        assert!(snap.pane_ids().is_empty());
    }

    #[test]
    fn empty_snapshot_roundtrips_json() {
        let snap = TopologySnapshot::empty(99);
        let json = snap.to_json().unwrap();
        let restored = TopologySnapshot::from_json(&json).unwrap();
        assert_eq!(snap, restored);
    }

    // ── PaneNode helpers ───────────────────────────────────────────────

    #[test]
    fn pane_node_leaf_count_is_one() {
        let leaf = PaneNode::Leaf {
            pane_id: 1,
            rows: 24,
            cols: 80,
            cwd: None,
            title: None,
            is_active: false,
        };
        assert_eq!(leaf.pane_count(), 1);
    }

    #[test]
    fn pane_node_hsplit_count() {
        let node = PaneNode::HSplit {
            children: vec![
                (
                    0.5,
                    PaneNode::Leaf {
                        pane_id: 1,
                        rows: 12,
                        cols: 80,
                        cwd: None,
                        title: None,
                        is_active: true,
                    },
                ),
                (
                    0.5,
                    PaneNode::Leaf {
                        pane_id: 2,
                        rows: 12,
                        cols: 80,
                        cwd: None,
                        title: None,
                        is_active: false,
                    },
                ),
            ],
        };
        assert_eq!(node.pane_count(), 2);
    }

    #[test]
    fn pane_node_vsplit_nested_count() {
        let node = PaneNode::VSplit {
            children: vec![
                (
                    0.5,
                    PaneNode::HSplit {
                        children: vec![
                            (
                                0.5,
                                PaneNode::Leaf {
                                    pane_id: 1,
                                    rows: 12,
                                    cols: 40,
                                    cwd: None,
                                    title: None,
                                    is_active: false,
                                },
                            ),
                            (
                                0.5,
                                PaneNode::Leaf {
                                    pane_id: 2,
                                    rows: 12,
                                    cols: 40,
                                    cwd: None,
                                    title: None,
                                    is_active: false,
                                },
                            ),
                        ],
                    },
                ),
                (
                    0.5,
                    PaneNode::Leaf {
                        pane_id: 3,
                        rows: 24,
                        cols: 40,
                        cwd: None,
                        title: None,
                        is_active: true,
                    },
                ),
            ],
        };
        assert_eq!(node.pane_count(), 3);
    }

    #[test]
    fn pane_node_collect_ids_nested() {
        let node = PaneNode::VSplit {
            children: vec![
                (
                    0.5,
                    PaneNode::Leaf {
                        pane_id: 10,
                        rows: 24,
                        cols: 40,
                        cwd: None,
                        title: None,
                        is_active: false,
                    },
                ),
                (
                    0.5,
                    PaneNode::HSplit {
                        children: vec![
                            (
                                0.5,
                                PaneNode::Leaf {
                                    pane_id: 20,
                                    rows: 12,
                                    cols: 40,
                                    cwd: None,
                                    title: None,
                                    is_active: false,
                                },
                            ),
                            (
                                0.5,
                                PaneNode::Leaf {
                                    pane_id: 30,
                                    rows: 12,
                                    cols: 40,
                                    cwd: None,
                                    title: None,
                                    is_active: true,
                                },
                            ),
                        ],
                    },
                ),
            ],
        };
        let mut ids = Vec::new();
        node.collect_pane_ids(&mut ids);
        assert_eq!(ids, vec![10, 20, 30]);
    }

    // ── InferenceQuality ───────────────────────────────────────────────

    #[test]
    fn inference_quality_eq() {
        assert_eq!(InferenceQuality::Inferred, InferenceQuality::Inferred);
        assert_eq!(
            InferenceQuality::FlatFallback,
            InferenceQuality::FlatFallback
        );
        assert_ne!(InferenceQuality::Inferred, InferenceQuality::FlatFallback);
    }

    // ── PaneNode serde ─────────────────────────────────────────────────

    #[test]
    fn pane_node_leaf_serde_roundtrip() {
        let leaf = PaneNode::Leaf {
            pane_id: 42,
            rows: 24,
            cols: 80,
            cwd: Some("/home".to_string()),
            title: Some("bash".to_string()),
            is_active: true,
        };
        let json = serde_json::to_string(&leaf).unwrap();
        assert!(json.contains("\"type\":\"Leaf\""));
        let restored: PaneNode = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, leaf);
    }

    #[test]
    fn pane_node_hsplit_serde_roundtrip_batch2() {
        let node = PaneNode::HSplit {
            children: vec![(
                1.0,
                PaneNode::Leaf {
                    pane_id: 1,
                    rows: 24,
                    cols: 80,
                    cwd: None,
                    title: None,
                    is_active: false,
                },
            )],
        };
        let json = serde_json::to_string(&node).unwrap();
        assert!(json.contains("\"type\":\"HSplit\""));
        let restored: PaneNode = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, node);
    }

    #[test]
    fn pane_node_vsplit_serde_roundtrip() {
        let node = PaneNode::VSplit {
            children: vec![(
                1.0,
                PaneNode::Leaf {
                    pane_id: 1,
                    rows: 24,
                    cols: 80,
                    cwd: None,
                    title: None,
                    is_active: false,
                },
            )],
        };
        let json = serde_json::to_string(&node).unwrap();
        assert!(json.contains("\"type\":\"VSplit\""));
        let restored: PaneNode = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, node);
    }

    // ── from_panes extras ──────────────────────────────────────────────

    #[test]
    fn from_panes_captures_workspace_id() {
        let mut pane = make_pane(0, 0, 0, 24, 80, None, None, true);
        pane.workspace = Some("my-workspace".to_string());
        let (snapshot, _) = TopologySnapshot::from_panes(&[pane], 1000);
        assert_eq!(snapshot.workspace_id.as_deref(), Some("my-workspace"));
    }

    #[test]
    fn from_panes_mixed_workspaces_are_unknown_and_order_independent() {
        let mut first = make_pane(9, 0, 0, 24, 80, None, None, true);
        first.workspace = Some("workspace-b".to_string());
        let mut second = make_pane(2, 1, 0, 24, 80, None, None, true);
        second.workspace = Some("workspace-a".to_string());

        let (forward, _) = TopologySnapshot::from_panes(&[first.clone(), second.clone()], 1_000);
        let (reversed, _) = TopologySnapshot::from_panes(&[second, first], 1_000);

        assert!(forward.workspace_id.is_none());
        assert_eq!(forward, reversed);
    }

    #[test]
    fn from_panes_does_not_invent_selected_tab_from_per_tab_activity() {
        let panes = vec![
            make_pane(0, 0, 0, 24, 80, None, None, true),
            make_pane(1, 1, 0, 24, 80, None, None, true),
        ];
        let (snapshot, _) = TopologySnapshot::from_panes(&panes, 1000);
        assert_eq!(snapshot.windows.len(), 1);
        assert!(snapshot.windows[0].active_tab_index.is_none());
        assert_eq!(snapshot.windows[0].tabs[0].active_pane_id, Some(0));
        assert_eq!(snapshot.windows[0].tabs[1].active_pane_id, Some(1));
    }

    #[test]
    fn from_panes_same_tab_permutations_produce_identical_topology() {
        let panes = vec![
            make_pane(9, 4, 2, 24, 80, Some("/nine"), Some("nine"), false),
            make_pane(2, 4, 2, 24, 80, Some("/two"), Some("two"), false),
            make_pane(7, 4, 2, 24, 80, Some("/seven"), Some("seven"), true),
        ];
        let permuted = vec![panes[2].clone(), panes[0].clone(), panes[1].clone()];

        let (first, first_report) = TopologySnapshot::from_panes(&panes, 1_000);
        let (second, second_report) = TopologySnapshot::from_panes(&permuted, 1_000);

        assert_eq!(first, second);
        assert_eq!(
            first.to_json().expect("serialize first topology"),
            second.to_json().expect("serialize permuted topology")
        );
        assert_eq!(
            first_report.inference_quality,
            second_report.inference_quality
        );
        assert_eq!(
            first_report.inference_quality.get(&4),
            Some(&InferenceQuality::FlatFallback)
        );
    }

    #[test]
    fn schema_v1_dimensions_saturate_instead_of_wrapping() {
        let pane = make_pane(0, 0, 0, u32::from(u16::MAX) + 1, u32::MAX, None, None, true);
        let (snapshot, _) = TopologySnapshot::from_panes(&[pane], 1000);
        let PaneNode::Leaf { rows, cols, .. } = snapshot.windows[0].tabs[0].pane_tree else {
            panic!("one pane must produce one leaf");
        };
        assert_eq!(rows, u16::MAX);
        assert_eq!(cols, u16::MAX);
    }

    #[test]
    fn from_panes_no_active_pane_no_active_tab() {
        let mut pane = make_pane(0, 0, 0, 24, 80, None, None, false);
        pane.is_active = false;
        let (snapshot, _) = TopologySnapshot::from_panes(&[pane], 1000);
        assert!(snapshot.windows[0].active_tab_index.is_none());
    }

    #[test]
    fn from_panes_does_not_mislabel_active_pane_title_as_tab_title() {
        let panes = vec![
            make_pane(0, 0, 0, 24, 80, None, Some("inactive"), false),
            make_pane(1, 0, 0, 24, 80, None, Some("active-title"), true),
        ];
        let (snapshot, _) = TopologySnapshot::from_panes(&panes, 1000);
        assert!(snapshot.windows[0].tabs[0].title.is_none());
    }

    // ── split inference extras ─────────────────────────────────────────

    #[test]
    fn infer_single_pane_is_leaf() {
        let panes = [make_pane(1, 0, 0, 24, 80, None, None, true)];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        let (tree, quality) = infer_split_tree(&refs);
        assert_eq!(quality, InferenceQuality::Inferred);
        assert!(matches!(tree, PaneNode::Leaf { pane_id: 1, .. }));
    }

    #[test]
    fn infer_hsplit_different_heights_same_width() {
        let panes = [
            make_pane(0, 0, 0, 10, 80, None, None, true),
            make_pane(1, 0, 0, 14, 80, None, None, false),
        ];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        let (tree, quality) = infer_split_tree(&refs);
        assert_eq!(quality, InferenceQuality::FlatFallback);
        match tree {
            PaneNode::HSplit { children } => {
                assert_eq!(children.len(), 2);
                // Proportions based on rows: 10/24 and 14/24
                let (p0, _) = &children[0];
                let (p1, _) = &children[1];
                assert!((p0 - 10.0 / 24.0).abs() < 0.01);
                assert!((p1 - 14.0 / 24.0).abs() < 0.01);
            }
            _ => panic!("Expected HSplit, got {:?}", tree),
        }
    }

    #[test]
    fn infer_vsplit_different_widths_same_height() {
        let panes = [
            make_pane(0, 0, 0, 24, 30, None, None, true),
            make_pane(1, 0, 0, 24, 50, None, None, false),
        ];
        let refs: Vec<&PaneInfo> = panes.iter().collect();
        let (tree, quality) = infer_split_tree(&refs);
        assert_eq!(quality, InferenceQuality::FlatFallback);
        match tree {
            PaneNode::VSplit { children } => {
                assert_eq!(children.len(), 2);
                let (p0, _) = &children[0];
                let (p1, _) = &children[1];
                assert!((p0 - 30.0 / 80.0).abs() < 0.01);
                assert!((p1 - 50.0 / 80.0).abs() < 0.01);
            }
            _ => panic!("Expected VSplit, got {:?}", tree),
        }
    }

    // ── match_panes extras ─────────────────────────────────────────────

    #[test]
    fn match_panes_empty_old_snapshot() {
        let (old_snapshot, _) = TopologySnapshot::from_panes(&[], 1000);
        let new_panes = vec![make_pane(1, 0, 0, 24, 80, None, None, true)];
        let mapping = match_panes(&old_snapshot, &new_panes);
        assert!(mapping.mappings.is_empty());
        assert!(mapping.unmatched_old.is_empty());
        assert_eq!(mapping.unmatched_new, vec![1]);
    }

    #[test]
    fn match_panes_empty_new_panes() {
        let old_panes = vec![make_pane(1, 0, 0, 24, 80, Some("/a"), None, true)];
        let (old_snapshot, _) = TopologySnapshot::from_panes(&old_panes, 1000);
        let mapping = match_panes(&old_snapshot, &[]);
        assert!(mapping.mappings.is_empty());
        assert_eq!(mapping.unmatched_old, vec![1]);
        assert!(mapping.unmatched_new.is_empty());
    }

    #[test]
    fn match_panes_by_title_only() {
        let old_panes = vec![make_pane(10, 0, 0, 24, 80, None, Some("vim"), true)];
        let (old_snapshot, _) = TopologySnapshot::from_panes(&old_panes, 1000);

        let new_panes = vec![make_pane(20, 0, 0, 24, 80, None, Some("vim"), true)];
        let mapping = match_panes(&old_snapshot, &new_panes);
        assert_eq!(mapping.mappings.get(&10), Some(&20));
        assert!(mapping.unmatched_old.is_empty());
        assert!(mapping.unmatched_new.is_empty());
    }

    #[test]
    fn match_panes_no_match_when_all_none() {
        let old_panes = vec![make_pane(10, 0, 0, 24, 80, None, None, true)];
        let (old_snapshot, _) = TopologySnapshot::from_panes(&old_panes, 1000);

        let new_panes = vec![make_pane(20, 0, 0, 24, 80, None, None, true)];
        let mapping = match_panes(&old_snapshot, &new_panes);
        // No cwd and no title → no match possible
        assert!(mapping.mappings.is_empty());
        assert_eq!(mapping.unmatched_old, vec![10]);
        assert_eq!(mapping.unmatched_new, vec![20]);
    }

    // ── WindowSnapshot serde ───────────────────────────────────────────

    #[test]
    fn window_snapshot_serde_with_optional_fields() {
        let window = WindowSnapshot {
            window_id: 1,
            title: Some("Main".to_string()),
            position: Some((100, 200)),
            size: Some((800, 600)),
            tabs: vec![],
            active_tab_index: Some(0),
        };
        let json = serde_json::to_string(&window).unwrap();
        let restored: WindowSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, window);
    }

    #[test]
    fn window_snapshot_serde_defaults_none_fields() {
        let json = r#"{"window_id":1,"tabs":[]}"#;
        let window: WindowSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(window.window_id, 1);
        assert!(window.title.is_none());
        assert!(window.position.is_none());
        assert!(window.size.is_none());
        assert!(window.active_tab_index.is_none());
    }

    // ── CaptureReport ──────────────────────────────────────────────────

    #[test]
    fn capture_report_single_pane_fields() {
        let panes = vec![make_pane(1, 0, 0, 24, 80, None, None, true)];
        let (_, report) = TopologySnapshot::from_panes(&panes, 1000);
        assert_eq!(report.window_count, 1);
        assert_eq!(report.tab_count, 1);
        assert_eq!(report.pane_count, 1);
        assert_eq!(
            report.inference_quality.get(&0),
            Some(&InferenceQuality::Inferred)
        );
    }

    #[test]
    fn capture_report_multi_window_tab_counts() {
        let panes = vec![
            make_pane(0, 0, 0, 24, 80, None, None, true),
            make_pane(1, 1, 0, 24, 80, None, None, false),
            make_pane(2, 0, 1, 24, 80, None, None, false),
            make_pane(3, 1, 1, 24, 80, None, None, false),
        ];
        let (_, report) = TopologySnapshot::from_panes(&panes, 1000);
        assert_eq!(report.window_count, 2);
        assert_eq!(report.tab_count, 4);
        assert_eq!(report.pane_count, 4);
    }

    // =========================================================================
    // Batch: DarkBadger wa-1u90p.7.1 — trait impls and edge cases
    // =========================================================================

    // -- TopologySnapshot --

    #[test]
    fn topology_snapshot_debug() {
        let snap = TopologySnapshot::empty(1000);
        let dbg = format!("{:?}", snap);
        assert!(dbg.contains("TopologySnapshot"));
    }

    #[test]
    fn topology_snapshot_clone_independence() {
        let snap = TopologySnapshot::empty(1000);
        let mut cloned = snap.clone();
        cloned.captured_at = 9999;
        assert_eq!(snap.captured_at, 1000);
    }

    #[test]
    fn topology_snapshot_partial_eq() {
        let a = TopologySnapshot::empty(1000);
        let b = TopologySnapshot::empty(1000);
        let c = TopologySnapshot::empty(2000);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn topology_snapshot_pane_ids_empty() {
        let snap = TopologySnapshot::empty(1000);
        assert!(snap.pane_ids().is_empty());
    }

    #[test]
    fn topology_snapshot_pane_count_zero() {
        let snap = TopologySnapshot::empty(1000);
        assert_eq!(snap.pane_count(), 0);
    }

    // -- WindowSnapshot --

    #[test]
    fn window_snapshot_debug_clone() {
        let win = WindowSnapshot {
            window_id: 1,
            title: Some("Test".to_string()),
            position: Some((100, 200)),
            size: Some((1920, 1080)),
            tabs: vec![],
            active_tab_index: Some(0),
        };
        let dbg = format!("{:?}", win);
        assert!(dbg.contains("WindowSnapshot"));
        let cloned = win.clone();
        assert_eq!(cloned.window_id, 1);
        assert_eq!(cloned.title.as_deref(), Some("Test"));
    }

    #[test]
    fn window_snapshot_serde_with_all_fields() {
        let win = WindowSnapshot {
            window_id: 42,
            title: Some("My Window".to_string()),
            position: Some((10, 20)),
            size: Some((800, 600)),
            tabs: vec![],
            active_tab_index: Some(1),
        };
        let json = serde_json::to_string(&win).unwrap();
        let parsed: WindowSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, win);
    }

    #[test]
    fn window_snapshot_partial_eq() {
        let a = WindowSnapshot {
            window_id: 1,
            title: None,
            position: None,
            size: None,
            tabs: vec![],
            active_tab_index: None,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    // -- TabSnapshot --

    #[test]
    fn tab_snapshot_debug_clone() {
        let tab = TabSnapshot {
            tab_id: 5,
            title: Some("Tab Title".to_string()),
            pane_tree: PaneNode::Leaf {
                pane_id: 10,
                rows: 24,
                cols: 80,
                cwd: None,
                title: None,
                is_active: true,
            },
            active_pane_id: Some(10),
        };
        let dbg = format!("{:?}", tab);
        assert!(dbg.contains("TabSnapshot"));
        let cloned = tab.clone();
        assert_eq!(cloned.tab_id, 5);
    }

    #[test]
    fn tab_snapshot_serde_roundtrip() {
        let tab = TabSnapshot {
            tab_id: 3,
            title: None,
            pane_tree: PaneNode::Leaf {
                pane_id: 1,
                rows: 30,
                cols: 120,
                cwd: Some("/home".to_string()),
                title: Some("bash".to_string()),
                is_active: false,
            },
            active_pane_id: None,
        };
        let json = serde_json::to_string(&tab).unwrap();
        let parsed: TabSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, tab);
    }

    // -- PaneNode --

    #[test]
    fn pane_node_leaf_debug() {
        let leaf = PaneNode::Leaf {
            pane_id: 1,
            rows: 24,
            cols: 80,
            cwd: None,
            title: None,
            is_active: false,
        };
        let dbg = format!("{:?}", leaf);
        assert!(dbg.contains("Leaf"));
    }

    #[test]
    fn pane_node_hsplit_serde_roundtrip_v2() {
        let node = PaneNode::HSplit {
            children: vec![
                (
                    0.5,
                    PaneNode::Leaf {
                        pane_id: 1,
                        rows: 12,
                        cols: 80,
                        cwd: None,
                        title: None,
                        is_active: true,
                    },
                ),
                (
                    0.5,
                    PaneNode::Leaf {
                        pane_id: 2,
                        rows: 12,
                        cols: 80,
                        cwd: None,
                        title: None,
                        is_active: false,
                    },
                ),
            ],
        };
        let json = serde_json::to_string(&node).unwrap();
        let parsed: PaneNode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_count(), 2);
    }

    #[test]
    fn pane_node_vsplit_collect_ids() {
        let node = PaneNode::VSplit {
            children: vec![
                (
                    0.3,
                    PaneNode::Leaf {
                        pane_id: 10,
                        rows: 24,
                        cols: 40,
                        cwd: None,
                        title: None,
                        is_active: false,
                    },
                ),
                (
                    0.7,
                    PaneNode::Leaf {
                        pane_id: 20,
                        rows: 24,
                        cols: 40,
                        cwd: None,
                        title: None,
                        is_active: false,
                    },
                ),
            ],
        };
        let mut ids = Vec::new();
        node.collect_pane_ids(&mut ids);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&10));
        assert!(ids.contains(&20));
    }

    #[test]
    fn pane_node_deeply_nested_pane_count() {
        // 3 levels: VSplit > HSplit > Leaf
        let node = PaneNode::VSplit {
            children: vec![
                (
                    0.5,
                    PaneNode::HSplit {
                        children: vec![
                            (
                                0.5,
                                PaneNode::Leaf {
                                    pane_id: 1,
                                    rows: 12,
                                    cols: 40,
                                    cwd: None,
                                    title: None,
                                    is_active: false,
                                },
                            ),
                            (
                                0.5,
                                PaneNode::Leaf {
                                    pane_id: 2,
                                    rows: 12,
                                    cols: 40,
                                    cwd: None,
                                    title: None,
                                    is_active: false,
                                },
                            ),
                        ],
                    },
                ),
                (
                    0.5,
                    PaneNode::Leaf {
                        pane_id: 3,
                        rows: 24,
                        cols: 40,
                        cwd: None,
                        title: None,
                        is_active: false,
                    },
                ),
            ],
        };
        assert_eq!(node.pane_count(), 3);
    }

    // -- InferenceQuality --

    #[test]
    fn inference_quality_debug_clone() {
        let q = InferenceQuality::Inferred;
        let dbg = format!("{:?}", q);
        assert!(dbg.contains("Inferred"));
        let cloned = q.clone();
        assert_eq!(cloned, InferenceQuality::Inferred);
    }

    #[test]
    fn inference_quality_eq_all_variants() {
        assert_eq!(InferenceQuality::Inferred, InferenceQuality::Inferred);
        assert_eq!(
            InferenceQuality::FlatFallback,
            InferenceQuality::FlatFallback
        );
        assert_ne!(InferenceQuality::Inferred, InferenceQuality::FlatFallback);
    }

    // -- CaptureReport --

    #[test]
    fn capture_report_debug() {
        let report = CaptureReport {
            window_count: 2,
            tab_count: 3,
            pane_count: 5,
            inference_quality: HashMap::new(),
        };
        let dbg = format!("{:?}", report);
        assert!(dbg.contains("CaptureReport"));
        assert!(dbg.contains("window_count"));
    }

    #[test]
    fn capture_report_clone_independence() {
        let mut quality = HashMap::new();
        quality.insert(0u64, InferenceQuality::Inferred);
        let report = CaptureReport {
            window_count: 1,
            tab_count: 2,
            pane_count: 3,
            inference_quality: quality,
        };
        let cloned = report.clone();
        assert_eq!(cloned.window_count, 1);
        assert_eq!(
            cloned.inference_quality.get(&0),
            Some(&InferenceQuality::Inferred)
        );
    }

    // -- PaneMapping --

    #[test]
    fn pane_mapping_debug() {
        let mapping = PaneMapping {
            mappings: HashMap::new(),
            unmatched_old: vec![],
            unmatched_new: vec![],
        };
        let dbg = format!("{:?}", mapping);
        assert!(dbg.contains("PaneMapping"));
    }

    #[test]
    fn pane_mapping_clone_independence() {
        let mut m = HashMap::new();
        m.insert(1u64, 10u64);
        let mapping = PaneMapping {
            mappings: m,
            unmatched_old: vec![2],
            unmatched_new: vec![20],
        };
        let cloned = mapping.clone();
        assert_eq!(cloned.mappings.get(&1), Some(&10));
        assert_eq!(cloned.unmatched_old, vec![2]);
        assert_eq!(cloned.unmatched_new, vec![20]);
    }

    // -- TOPOLOGY_SCHEMA_VERSION --

    #[test]
    fn topology_schema_version_is_one_v2() {
        assert_eq!(TOPOLOGY_SCHEMA_VERSION, 1);
    }

    // -- TopologySnapshot serde edge cases --

    #[test]
    fn topology_snapshot_serde_preserves_workspace_id() {
        let snap = TopologySnapshot {
            schema_version: TOPOLOGY_SCHEMA_VERSION,
            captured_at: 5000,
            workspace_id: Some("my-workspace".to_string()),
            windows: vec![],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: TopologySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.workspace_id.as_deref(), Some("my-workspace"));
    }

    #[test]
    fn topology_snapshot_serde_missing_workspace_defaults_none() {
        let json = r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#;
        let parsed: TopologySnapshot = serde_json::from_str(json).unwrap();
        assert!(parsed.workspace_id.is_none());
    }

    // -- PaneNode split ratio precision --

    #[test]
    fn pane_node_split_ratio_precision_roundtrip() {
        let node = PaneNode::HSplit {
            children: vec![
                (
                    0.333_333_333,
                    PaneNode::Leaf {
                        pane_id: 1,
                        rows: 8,
                        cols: 80,
                        cwd: None,
                        title: None,
                        is_active: false,
                    },
                ),
                (
                    0.666_666_667,
                    PaneNode::Leaf {
                        pane_id: 2,
                        rows: 16,
                        cols: 80,
                        cwd: None,
                        title: None,
                        is_active: false,
                    },
                ),
            ],
        };
        let json = serde_json::to_string(&node).unwrap();
        let parsed: PaneNode = serde_json::from_str(&json).unwrap();
        if let PaneNode::HSplit { children } = parsed {
            assert!((children[0].0 - 0.333_333_333).abs() < 0.01);
            assert!((children[1].0 - 0.666_666_667).abs() < 0.01);
        } else {
            panic!("expected HSplit");
        }
    }

    // -- Native lifecycle model --

    #[test]
    fn lifecycle_identity_stable_key_contains_all_components() {
        let id = LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws-a", "local", 42, 7);
        assert_eq!(id.stable_key(), "ws-a:local:pane:42:7");
    }

    #[test]
    fn lifecycle_identity_from_pane_info_defaults_workspace() {
        let pane = make_pane(9, 1, 2, 24, 80, Some("/tmp"), Some("shell"), true);
        let id = LifecycleIdentity::from_pane_info(&pane, 3);
        assert_eq!(id.kind, LifecycleEntityKind::Pane);
        assert_eq!(id.workspace_id, "default");
        assert_eq!(id.domain, "local");
        assert_eq!(id.local_id, 9);
        assert_eq!(id.generation, 3);
    }

    #[test]
    fn transition_pane_state_happy_path() {
        let s1 = transition_pane_state(
            MuxPaneLifecycleState::Provisioning,
            LifecycleEvent::Provisioned,
        )
        .unwrap();
        assert_eq!(s1.next_state, MuxPaneLifecycleState::Ready);
        assert!(!s1.idempotent);

        let s2 = transition_pane_state(s1.next_state, LifecycleEvent::StartWork).unwrap();
        assert_eq!(s2.next_state, MuxPaneLifecycleState::Running);

        let s3 = transition_pane_state(s2.next_state, LifecycleEvent::WorkFinished).unwrap();
        assert_eq!(s3.next_state, MuxPaneLifecycleState::Ready);

        let s4 = transition_pane_state(s3.next_state, LifecycleEvent::DrainRequested).unwrap();
        assert_eq!(s4.next_state, MuxPaneLifecycleState::Draining);

        let s5 = transition_pane_state(s4.next_state, LifecycleEvent::DrainCompleted).unwrap();
        assert_eq!(s5.next_state, MuxPaneLifecycleState::Closed);
    }

    #[test]
    fn transition_pane_state_idempotent_and_recovery_paths() {
        let draining = transition_pane_state(
            MuxPaneLifecycleState::Draining,
            LifecycleEvent::DrainRequested,
        )
        .unwrap();
        assert!(draining.idempotent);
        assert_eq!(draining.next_state, MuxPaneLifecycleState::Draining);

        let orphaned = transition_pane_state(
            MuxPaneLifecycleState::Ready,
            LifecycleEvent::PeerDisconnected,
        )
        .unwrap();
        assert_eq!(orphaned.next_state, MuxPaneLifecycleState::Orphaned);

        let recovered =
            transition_pane_state(orphaned.next_state, LifecycleEvent::Recover).unwrap();
        assert_eq!(recovered.next_state, MuxPaneLifecycleState::Ready);

        let closed =
            transition_pane_state(MuxPaneLifecycleState::Closed, LifecycleEvent::ForceClose)
                .unwrap();
        assert!(closed.idempotent);
        assert_eq!(closed.next_state, MuxPaneLifecycleState::Closed);
    }

    #[test]
    fn transition_pane_state_rejects_invalid_transition() {
        let err = transition_pane_state(MuxPaneLifecycleState::Closed, LifecycleEvent::StartWork)
            .unwrap_err();
        assert_eq!(err.entity, LifecycleEntityKind::Pane);
        assert_eq!(err.state, "closed");
        assert_eq!(err.event, LifecycleEvent::StartWork);
    }

    #[test]
    fn transition_session_window_agent_paths() {
        let session = transition_session_state(
            SessionLifecycleState::Provisioning,
            LifecycleEvent::Provisioned,
        )
        .unwrap();
        assert_eq!(session.next_state, SessionLifecycleState::Active);

        let window = transition_window_state(
            WindowLifecycleState::Active,
            LifecycleEvent::PeerDisconnected,
        )
        .unwrap();
        assert_eq!(window.next_state, WindowLifecycleState::Recovering);

        let agent = transition_agent_state(AgentLifecycleState::Registered, LifecycleEvent::Attach)
            .unwrap();
        assert_eq!(agent.next_state, AgentLifecycleState::Attached);
    }

    fn make_context(
        timestamp_ms: u64,
        scenario_id: &str,
        reason_code: &str,
    ) -> LifecycleTransitionContext {
        LifecycleTransitionContext::new(
            timestamp_ms,
            "native_mux.lifecycle.tests",
            format!("corr-{timestamp_ms}"),
            scenario_id,
            reason_code,
        )
    }

    #[test]
    fn lifecycle_state_union_routes_transitions() {
        let current = LifecycleState::Pane(MuxPaneLifecycleState::Ready);
        let next = apply_lifecycle_state_transition(current, LifecycleEvent::StartWork).unwrap();
        assert_eq!(
            next.next_state,
            LifecycleState::Pane(MuxPaneLifecycleState::Running)
        );
        assert!(!next.idempotent);
    }

    #[test]
    fn lifecycle_registry_rejects_kind_mismatch_registration() {
        let mut registry = LifecycleRegistry::new();
        let identity = LifecycleIdentity::new(LifecycleEntityKind::Window, "ws", "local", 1, 0);
        let err = registry
            .register_entity(
                identity,
                LifecycleState::Pane(MuxPaneLifecycleState::Ready),
                10,
            )
            .unwrap_err();
        assert_eq!(
            err,
            LifecycleEngineError::KindMismatch {
                identity_kind: LifecycleEntityKind::Window,
                state_kind: LifecycleEntityKind::Pane,
            }
        );
    }

    #[test]
    fn lifecycle_registry_transition_updates_version_and_log() {
        let mut registry = LifecycleRegistry::new();
        let identity = LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws", "local", 7, 0);
        registry
            .register_entity(
                identity.clone(),
                LifecycleState::Pane(MuxPaneLifecycleState::Ready),
                100,
            )
            .unwrap();

        let result = registry
            .apply_transition(LifecycleTransitionRequest {
                identity: identity.clone(),
                event: LifecycleEvent::StartWork,
                expected_version: Some(0),
                context: make_context(101, "unit-transition", "native_mux.lifecycle.start_work"),
            })
            .unwrap();

        assert!(result.version_changed);
        assert!(!result.idempotent);
        assert_eq!(result.record.version, 1);
        assert_eq!(
            result.record.state,
            LifecycleState::Pane(MuxPaneLifecycleState::Running)
        );
        assert_eq!(result.record.last_event, Some(LifecycleEvent::StartWork));

        let log = registry.transition_log().last().unwrap();
        assert_eq!(log.decision, LifecycleDecision::Applied);
        assert_eq!(log.expected_version, Some(0));
        assert_eq!(log.actual_version, 1);
        assert_eq!(log.reason_code, "native_mux.lifecycle.start_work");
        assert!(log.error_code.is_none());
    }

    #[test]
    fn lifecycle_registry_idempotent_transition_does_not_bump_version() {
        let mut registry = LifecycleRegistry::new();
        let identity = LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws", "local", 8, 0);
        registry
            .register_entity(
                identity.clone(),
                LifecycleState::Pane(MuxPaneLifecycleState::Draining),
                200,
            )
            .unwrap();

        let result = registry
            .apply_transition(LifecycleTransitionRequest {
                identity: identity.clone(),
                event: LifecycleEvent::DrainRequested,
                expected_version: Some(0),
                context: make_context(201, "unit-idempotent", "native_mux.lifecycle.drain_retry"),
            })
            .unwrap();

        assert!(!result.version_changed);
        assert!(result.idempotent);
        assert_eq!(result.record.version, 0);
        assert_eq!(
            result.record.state,
            LifecycleState::Pane(MuxPaneLifecycleState::Draining)
        );
        let log = registry.transition_log().last().unwrap();
        assert_eq!(log.decision, LifecycleDecision::Noop);
        assert_eq!(log.actual_version, 0);
    }

    #[test]
    fn lifecycle_registry_concurrency_conflict_is_rejected_and_logged() {
        let mut registry = LifecycleRegistry::new();
        let identity = LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws", "local", 9, 0);
        registry
            .register_entity(
                identity.clone(),
                LifecycleState::Pane(MuxPaneLifecycleState::Ready),
                300,
            )
            .unwrap();

        let _ = registry
            .apply_transition(LifecycleTransitionRequest {
                identity: identity.clone(),
                event: LifecycleEvent::StartWork,
                expected_version: Some(0),
                context: make_context(301, "unit-concurrency-1", "native_mux.lifecycle.start"),
            })
            .unwrap();

        let err = registry
            .apply_transition(LifecycleTransitionRequest {
                identity,
                event: LifecycleEvent::WorkFinished,
                expected_version: Some(0),
                context: make_context(302, "unit-concurrency-2", "native_mux.lifecycle.finish"),
            })
            .unwrap_err();

        assert_eq!(
            err,
            LifecycleEngineError::ConcurrencyConflict {
                identity_key: "ws:local:pane:9:0".to_string(),
                expected_version: 0,
                actual_version: 1,
            }
        );

        let log = registry.transition_log().last().unwrap();
        assert_eq!(log.decision, LifecycleDecision::Rejected);
        assert_eq!(
            log.error_code.as_deref(),
            Some("native_mux.lifecycle.version_conflict")
        );
    }

    #[test]
    fn lifecycle_registry_recovery_path_updates_state_machine() {
        let mut registry = LifecycleRegistry::new();
        let identity = LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws", "local", 10, 0);
        registry
            .register_entity(
                identity.clone(),
                LifecycleState::Pane(MuxPaneLifecycleState::Running),
                400,
            )
            .unwrap();

        let orphaned = registry
            .apply_transition(LifecycleTransitionRequest {
                identity: identity.clone(),
                event: LifecycleEvent::PeerDisconnected,
                expected_version: Some(0),
                context: make_context(
                    401,
                    "unit-recovery-disconnect",
                    "native_mux.lifecycle.disconnect",
                ),
            })
            .unwrap();
        assert_eq!(
            orphaned.record.state,
            LifecycleState::Pane(MuxPaneLifecycleState::Orphaned)
        );
        assert_eq!(orphaned.record.version, 1);

        let recovered = registry
            .apply_transition(LifecycleTransitionRequest {
                identity,
                event: LifecycleEvent::Recover,
                expected_version: Some(1),
                context: make_context(402, "unit-recovery-recover", "native_mux.lifecycle.recover"),
            })
            .unwrap();
        assert_eq!(
            recovered.record.state,
            LifecycleState::Pane(MuxPaneLifecycleState::Ready)
        );
        assert_eq!(recovered.record.version, 2);
    }

    #[test]
    fn lifecycle_registry_bootstrap_from_panes_seeds_session_window_and_pane_records() {
        let panes = vec![
            make_pane(1, 10, 100, 24, 80, Some("/a"), Some("a"), true),
            make_pane(2, 10, 100, 24, 80, Some("/b"), Some("b"), false),
            make_pane(3, 11, 101, 24, 80, Some("/c"), Some("c"), false),
        ];
        let registry = LifecycleRegistry::bootstrap_from_panes(&panes, 4, 500).unwrap();

        assert_eq!(
            registry.entity_count_by_kind(LifecycleEntityKind::Session),
            1
        );
        assert_eq!(
            registry.entity_count_by_kind(LifecycleEntityKind::Window),
            2
        );
        assert_eq!(registry.entity_count_by_kind(LifecycleEntityKind::Pane), 3);
        assert_eq!(registry.entity_count_by_kind(LifecycleEntityKind::Agent), 0);
        assert_eq!(registry.len(), 6);

        let active_pane_id = LifecycleIdentity::from_pane_info(&panes[0], 4);
        let active_record = registry.get(&active_pane_id).unwrap();
        assert_eq!(
            active_record.state,
            LifecycleState::Pane(MuxPaneLifecycleState::Running)
        );
    }

    #[test]
    fn lifecycle_registry_bootstrap_duplicate_pane_rows_keep_running_state() {
        let panes = vec![
            make_pane(20, 10, 100, 24, 80, Some("/a"), Some("a"), false),
            make_pane(20, 10, 100, 24, 80, Some("/a"), Some("a"), true),
        ];
        let registry = LifecycleRegistry::bootstrap_from_panes(&panes, 4, 550).unwrap();

        assert_eq!(registry.entity_count_by_kind(LifecycleEntityKind::Pane), 1);
        let pane_id = LifecycleIdentity::from_pane_info(&panes[0], 4);
        let record = registry.get(&pane_id).unwrap();
        assert_eq!(
            record.state,
            LifecycleState::Pane(MuxPaneLifecycleState::Running)
        );
        assert_eq!(record.version, 0);
    }

    #[test]
    fn lifecycle_registry_rejects_invalid_context() {
        let mut registry = LifecycleRegistry::new();
        let identity = LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws", "local", 11, 0);
        registry
            .register_entity(
                identity.clone(),
                LifecycleState::Pane(MuxPaneLifecycleState::Ready),
                600,
            )
            .unwrap();

        let err = registry
            .apply_transition(LifecycleTransitionRequest {
                identity,
                event: LifecycleEvent::StartWork,
                expected_version: Some(0),
                context: LifecycleTransitionContext::new(601, "", "corr", "scenario", "reason"),
            })
            .unwrap_err();
        assert_eq!(
            err,
            LifecycleEngineError::InvalidContext { field: "component" }
        );
        let rejection = registry.transition_log().last().unwrap();
        assert_eq!(rejection.decision, LifecycleDecision::Rejected);
        assert_eq!(
            rejection.error_code.as_deref(),
            Some("native_mux.lifecycle.invalid_context")
        );
    }

    // ── ft-nrqf7: from_json DoS guards ────────────────────────────────────
    //
    // Snapshots are read from `.ft/snapshots/<session>.json` on the
    // recorder's startup path. A crafted input must reject with a
    // structured error, NOT panic / OOM / stack-overflow.

    /// Build a JSON string of `n` bytes that would parse as a valid
    /// topology if not for the size cap (so the byte-cap reject path
    /// fires *before* serde_json sees the input). The bytes are a
    /// long valid `workspace_id` string so even if the cap were
    /// removed the input would round-trip — we are pinning that the
    /// cap fires first, not that the input is otherwise invalid.
    fn oversized_snapshot_json(n: usize) -> String {
        let mut s = String::with_capacity(n + 256);
        s.push_str(r#"{"schema_version":1,"captured_at":0,"workspace_id":""#);
        let prefix_len = s.len();
        let suffix = r#"","windows":[]}"#;
        let payload_target = n.saturating_sub(prefix_len + suffix.len());
        s.extend(std::iter::repeat_n('a', payload_target));
        s.push_str(suffix);
        s
    }

    /// Generate a JSON string nesting `splits` HSplit layers deep,
    /// terminating in a single Leaf. Resulting `pane_tree.depth()` is
    /// `splits + 1` (the trailing Leaf counts as one level). Output is
    /// well-formed; the only reason `from_json` should reject is the
    /// depth cap.
    fn deeply_nested_snapshot_json(splits: usize) -> String {
        let depth = splits;
        let mut json = String::from(
            r#"{"schema_version":1,"captured_at":0,"windows":[{"window_id":1,"tabs":[{"tab_id":1,"pane_tree":"#,
        );

        // Build the nested HSplit prefix.
        for _ in 0..depth {
            json.push_str(r#"{"type":"HSplit","children":[[1.0,"#);
        }
        json.push_str(r#"{"type":"Leaf","pane_id":1,"rows":24,"cols":80}"#);
        for _ in 0..depth {
            json.push_str(r"]]}");
        }
        json.push_str(r"}]}]}");
        json
    }

    #[test]
    fn from_json_rejects_oversize_input_with_too_large_error() {
        // Just over the cap. Build a string longer than MAX_SNAPSHOT_BYTES
        // so the byte check rejects before parsing. Value is well-formed.
        let oversized = oversized_snapshot_json(MAX_SNAPSHOT_BYTES + 1024);
        assert!(oversized.len() > MAX_SNAPSHOT_BYTES);
        let err = TopologySnapshot::from_json(&oversized)
            .expect_err("ft-nrqf7: oversized input must reject");
        match err {
            TopologySnapshotError::TooLarge { bytes, limit } => {
                assert_eq!(limit, MAX_SNAPSHOT_BYTES);
                assert!(bytes > MAX_SNAPSHOT_BYTES);
            }
            other => panic!("ft-nrqf7: expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn from_json_rejects_deeply_nested_input_with_too_deep_error() {
        // splits = MAX_PANE_TREE_DEPTH → resulting depth = cap + 1
        // (the trailing Leaf counts as one level). Must reject with
        // TooDeep, not a serde_json error and not a panic.
        let json = deeply_nested_snapshot_json(MAX_PANE_TREE_DEPTH);
        let err = TopologySnapshot::from_json(&json)
            .expect_err("ft-nrqf7: deeply nested input must reject");
        match err {
            TopologySnapshotError::TooDeep { depth, limit } => {
                assert_eq!(limit, MAX_PANE_TREE_DEPTH);
                assert!(depth > MAX_PANE_TREE_DEPTH);
            }
            other => panic!("ft-nrqf7: expected TooDeep, got {other:?}"),
        }
    }

    #[test]
    fn from_json_accepts_input_at_depth_cap() {
        // splits = cap - 1 → depth = cap. Must parse cleanly — guards
        // reject "over", not "at-or-near".
        let json = deeply_nested_snapshot_json(MAX_PANE_TREE_DEPTH - 1);
        let snapshot = TopologySnapshot::from_json(&json)
            .expect("ft-nrqf7: depth-at-cap must parse; reject only when strictly greater");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].tabs.len(), 1);
        assert_eq!(
            snapshot.windows[0].tabs[0].pane_tree.depth(),
            MAX_PANE_TREE_DEPTH
        );
    }

    #[test]
    fn from_json_rejects_shallow_split_above_fanout_cap() {
        let children = (0..=MAX_SPLIT_FANOUT)
            .map(|pane_id| {
                (
                    1.0,
                    PaneNode::Leaf {
                        pane_id: u64::try_from(pane_id).expect("test pane id fits u64"),
                        rows: 24,
                        cols: 80,
                        cwd: None,
                        title: None,
                        is_active: pane_id == 0,
                    },
                )
            })
            .collect();
        let snapshot = TopologySnapshot {
            schema_version: TOPOLOGY_SCHEMA_VERSION,
            captured_at: 1_000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 1,
                title: None,
                position: None,
                size: None,
                tabs: vec![TabSnapshot {
                    tab_id: 1,
                    title: None,
                    pane_tree: PaneNode::VSplit { children },
                    active_pane_id: Some(0),
                }],
                active_tab_index: None,
            }],
        };
        let json = snapshot.to_json().expect("serialize wide topology");

        let error = TopologySnapshot::from_json(&json)
            .expect_err("wide split must fail bounded topology admission");
        assert!(matches!(
            error,
            TopologySnapshotError::ResourceLimit {
                resource: "split_fanout",
                count,
                limit: MAX_SPLIT_FANOUT,
            } if count == MAX_SPLIT_FANOUT + 1
        ));
    }

    #[test]
    fn programmatic_topology_rejects_total_panes_above_cap() {
        let leaf = |pane_id: usize| PaneNode::Leaf {
            pane_id: u64::try_from(pane_id).expect("test pane id fits u64"),
            rows: 24,
            cols: 80,
            cwd: None,
            title: None,
            is_active: pane_id == 0,
        };
        let first_children = (0..MAX_TOPOLOGY_PANES)
            .map(|pane_id| (1.0, leaf(pane_id)))
            .collect();
        let snapshot = TopologySnapshot {
            schema_version: TOPOLOGY_SCHEMA_VERSION,
            captured_at: 1_000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 1,
                title: None,
                position: None,
                size: None,
                tabs: vec![
                    TabSnapshot {
                        tab_id: 1,
                        title: None,
                        pane_tree: PaneNode::VSplit {
                            children: first_children,
                        },
                        active_pane_id: Some(0),
                    },
                    TabSnapshot {
                        tab_id: 2,
                        title: None,
                        pane_tree: leaf(MAX_TOPOLOGY_PANES),
                        active_pane_id: Some(
                            u64::try_from(MAX_TOPOLOGY_PANES).expect("test pane id fits u64"),
                        ),
                    },
                ],
                active_tab_index: Some(0),
            }],
        };

        assert!(matches!(
            snapshot.validate_resource_limits(),
            Err(TopologySnapshotError::ResourceLimit {
                resource: "panes",
                count,
                limit: MAX_TOPOLOGY_PANES,
            }) if count == MAX_TOPOLOGY_PANES + 1
        ));
    }

    #[test]
    fn programmatic_topology_rejects_total_nodes_above_cap() {
        let children = (0..MAX_SPLIT_FANOUT)
            .map(|_| {
                (
                    1.0,
                    PaneNode::HSplit {
                        children: vec![(
                            1.0,
                            PaneNode::VSplit {
                                children: Vec::new(),
                            },
                        )],
                    },
                )
            })
            .collect();
        let snapshot = TopologySnapshot {
            schema_version: TOPOLOGY_SCHEMA_VERSION,
            captured_at: 1_000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 1,
                title: None,
                position: None,
                size: None,
                tabs: vec![TabSnapshot {
                    tab_id: 1,
                    title: None,
                    pane_tree: PaneNode::VSplit { children },
                    active_pane_id: None,
                }],
                active_tab_index: None,
            }],
        };

        assert!(matches!(
            snapshot.validate_resource_limits(),
            Err(TopologySnapshotError::ResourceLimit {
                resource: "nodes",
                count,
                limit: MAX_TOPOLOGY_NODES,
            }) if count == MAX_TOPOLOGY_NODES + 1
        ));
    }

    #[test]
    fn from_json_round_trips_typical_capture() {
        // Pin that the new return type doesn't break ordinary capture →
        // serialize → deserialize on a small realistic topology.
        let snapshot = TopologySnapshot::empty(1_777_200_000_000);
        let json = snapshot.to_json().expect("serialize");
        let restored = TopologySnapshot::from_json(&json).expect("deserialize");
        assert_eq!(restored, snapshot);
    }

    #[test]
    fn persistence_semantics_reject_duplicate_pane_ids() {
        let panes = vec![
            make_pane(7, 1, 1, 24, 80, None, None, true),
            make_pane(7, 1, 1, 24, 80, None, None, false),
        ];
        let (snapshot, _) = TopologySnapshot::from_panes(&panes, 1_000);
        assert!(matches!(
            snapshot.validate_persistence_semantics(),
            Err(TopologySnapshotError::InvalidStructure {
                reason: "duplicate pane id",
            })
        ));
    }

    #[test]
    fn persistence_semantics_reject_multiple_active_pane_markers() {
        let panes = vec![
            make_pane(7, 1, 1, 24, 80, None, None, true),
            make_pane(8, 1, 1, 24, 80, None, None, true),
        ];
        let (snapshot, _) = TopologySnapshot::from_panes(&panes, 1_000);
        assert!(matches!(
            snapshot.validate_persistence_semantics(),
            Err(TopologySnapshotError::InvalidStructure {
                reason: "tab contains multiple active pane markers",
            })
        ));
    }

    #[test]
    fn from_json_returns_json_variant_for_malformed_input() {
        // Malformed JSON must reach the parser and surface as the
        // Json variant — not silently coerced into a guard error.
        let err = TopologySnapshot::from_json("not json at all")
            .expect_err("ft-nrqf7: invalid JSON must error");
        match err {
            TopologySnapshotError::Json(_) => {}
            other => panic!("ft-nrqf7: expected Json variant, got {other:?}"),
        }
    }

    #[test]
    fn from_json_does_not_panic_on_extreme_depth() {
        // Construct an input that would blow a recursive-walk stack
        // if depth() were not iterative. serde_json's parser caps at
        // ~128 levels, so this also exercises that the parser's own
        // guard fires before our depth check would.
        let json = deeply_nested_snapshot_json(512);
        // Either the parser rejects (recursion_limit) or our depth
        // check rejects — both are structured errors, never panics.
        let err =
            TopologySnapshot::from_json(&json).expect_err("ft-nrqf7: extreme depth must reject");
        match err {
            TopologySnapshotError::TooDeep { .. } | TopologySnapshotError::Json(_) => {}
            other @ (TopologySnapshotError::TooLarge { .. }
            | TopologySnapshotError::ResourceLimit { .. }
            | TopologySnapshotError::InvalidStructure { .. }) => {
                panic!("ft-nrqf7: expected TooDeep or Json, got {other:?}")
            }
        }
    }

    #[test]
    fn pane_node_depth_iterative_matches_recursive_intuition() {
        // Sanity-check the iterative depth() walk. A tree built as
        // HSplit(Leaf, HSplit(Leaf, Leaf)) should report depth 3.
        let leaf = || PaneNode::Leaf {
            pane_id: 1,
            rows: 24,
            cols: 80,
            cwd: None,
            title: None,
            is_active: false,
        };
        let tree = PaneNode::HSplit {
            children: vec![
                (
                    0.5,
                    PaneNode::HSplit {
                        children: vec![(1.0, leaf())],
                    },
                ),
                (0.5, leaf()),
            ],
        };
        assert_eq!(tree.depth(), 3);
        assert_eq!(leaf().depth(), 1);
    }
}
