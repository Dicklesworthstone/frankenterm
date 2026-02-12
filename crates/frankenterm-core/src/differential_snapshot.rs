//! Differential (incremental) snapshots for continuous background saving.
//!
//! Instead of capturing the entire mux state every time, this module tracks
//! which panes have changed ("dirty set") and records only the diffs.
//! Snapshots form a chain: `base → diff₁ → diff₂ → …` that can be
//! replayed to reconstruct state at any diff point.
//!
//! # Architecture
//!
//! ```text
//! DirtyTracker  ←──  pane events (output, resize, title, close, create)
//!       │
//!       ▼
//! DiffSnapshotEngine::capture_diff()
//!       │
//!       ├── only captures dirty panes
//!       ├── emits Vec<SnapshotDiff>
//!       └── clears dirty set
//!       │
//!       ▼
//! DiffChain  (base + ordered diffs)
//!       │
//!       ├── restore()  → full state at any diff point
//!       └── compact()  → merge chain into new base
//! ```
//!
//! See bead `wa-3kxe.3` for the full design.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::session_pane_state::PaneStateSnapshot;
use crate::session_topology::TopologySnapshot;

// =============================================================================
// Dirty tracking
// =============================================================================

/// What aspect of a pane changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirtyField {
    /// New scrollback output received.
    Scrollback,
    /// Terminal metadata changed (title, cursor, cwd, size).
    Metadata,
    /// Pane was newly created.
    Created,
    /// Pane was closed / destroyed.
    Closed,
}

/// Tracks which panes have been modified since the last differential snapshot.
///
/// Thread-safe via interior mutability is not needed here — the tracker is
/// owned by the `DiffSnapshotEngine` which serializes access.
#[derive(Debug, Clone)]
pub struct DirtyTracker {
    /// Pane ID → set of dirty fields.
    dirty: HashMap<u64, HashSet<DirtyField>>,
    /// Pane IDs that were created since last snapshot.
    created: HashSet<u64>,
    /// Pane IDs that were closed since last snapshot.
    closed: HashSet<u64>,
    /// Whether the layout topology changed.
    layout_dirty: bool,
}

impl DirtyTracker {
    /// Create a new empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dirty: HashMap::new(),
            created: HashSet::new(),
            closed: HashSet::new(),
            layout_dirty: false,
        }
    }

    /// Mark a specific field of a pane as dirty.
    pub fn mark_dirty(&mut self, pane_id: u64, field: DirtyField) {
        self.dirty.entry(pane_id).or_default().insert(field);

        match field {
            DirtyField::Created => {
                self.created.insert(pane_id);
                self.layout_dirty = true;
            }
            DirtyField::Closed => {
                self.closed.insert(pane_id);
                self.layout_dirty = true;
            }
            _ => {}
        }
    }

    /// Mark a pane as having new scrollback output.
    pub fn mark_output(&mut self, pane_id: u64) {
        self.mark_dirty(pane_id, DirtyField::Scrollback);
    }

    /// Mark a pane's metadata as changed.
    pub fn mark_metadata(&mut self, pane_id: u64) {
        self.mark_dirty(pane_id, DirtyField::Metadata);
    }

    /// Mark a pane as newly created.
    pub fn mark_created(&mut self, pane_id: u64) {
        self.mark_dirty(pane_id, DirtyField::Created);
    }

    /// Mark a pane as closed.
    pub fn mark_closed(&mut self, pane_id: u64) {
        self.mark_dirty(pane_id, DirtyField::Closed);
    }

    /// Mark the layout topology as changed.
    pub fn mark_layout_dirty(&mut self) {
        self.layout_dirty = true;
    }

    /// Returns the set of dirty pane IDs.
    #[must_use]
    pub fn dirty_pane_ids(&self) -> HashSet<u64> {
        self.dirty.keys().copied().collect()
    }

    /// Returns the dirty fields for a specific pane.
    #[must_use]
    pub fn dirty_fields(&self, pane_id: u64) -> Option<&HashSet<DirtyField>> {
        self.dirty.get(&pane_id)
    }

    /// Check if the layout is dirty.
    #[must_use]
    pub fn is_layout_dirty(&self) -> bool {
        self.layout_dirty
    }

    /// Returns true if there are no dirty panes or layout changes.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.dirty.is_empty() && !self.layout_dirty
    }

    /// Returns the total number of dirty panes.
    #[must_use]
    pub fn dirty_count(&self) -> usize {
        self.dirty.len()
    }

    /// Clear all dirty state (called after a successful diff snapshot).
    pub fn clear(&mut self) {
        self.dirty.clear();
        self.created.clear();
        self.closed.clear();
        self.layout_dirty = false;
    }
}

impl Default for DirtyTracker {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Diff records
// =============================================================================

/// A single diff record describing what changed in one snapshot delta.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum SnapshotDiff {
    /// Pane scrollback content changed (new lines appended).
    PaneScrollbackChanged {
        pane_id: u64,
        /// Updated scrollback reference (latest seq, line count).
        new_scrollback_ref: Option<crate::session_pane_state::ScrollbackRef>,
    },
    /// Pane metadata changed (title, cursor, cwd, size, agent state).
    PaneMetadataChanged {
        pane_id: u64,
        /// The full new pane state snapshot (replaces prior state).
        new_state: PaneStateSnapshot,
    },
    /// A new pane was created.
    PaneCreated {
        pane_id: u64,
        /// Full initial state of the new pane.
        snapshot: PaneStateSnapshot,
    },
    /// A pane was closed / destroyed.
    PaneClosed { pane_id: u64 },
    /// The layout topology changed.
    LayoutChanged {
        /// Full new topology (we don't diff topology — it's small).
        new_topology: TopologySnapshot,
    },
}

impl SnapshotDiff {
    /// Returns the pane ID affected by this diff, if applicable.
    #[must_use]
    pub fn pane_id(&self) -> Option<u64> {
        match self {
            Self::PaneScrollbackChanged { pane_id, .. }
            | Self::PaneMetadataChanged { pane_id, .. }
            | Self::PaneCreated { pane_id, .. }
            | Self::PaneClosed { pane_id } => Some(*pane_id),
            Self::LayoutChanged { .. } => None,
        }
    }
}

// =============================================================================
// Diff snapshot (a single delta)
// =============================================================================

/// A single differential snapshot — the set of changes from the previous state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiffSnapshot {
    /// Monotonically increasing sequence number within the chain.
    pub seq: u64,
    /// When this diff was captured (epoch ms).
    pub captured_at: u64,
    /// The individual diff records.
    pub diffs: Vec<SnapshotDiff>,
}

// =============================================================================
// Base snapshot (full state at a point in time)
// =============================================================================

/// A full base snapshot from which diffs are applied.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BaseSnapshot {
    /// When this base was captured (epoch ms).
    pub captured_at: u64,
    /// Layout topology at capture time.
    pub topology: TopologySnapshot,
    /// Per-pane states keyed by pane ID.
    pub pane_states: HashMap<u64, PaneStateSnapshot>,
}

impl BaseSnapshot {
    /// Create a base snapshot from a topology and pane state list.
    #[must_use]
    pub fn new(
        captured_at: u64,
        topology: TopologySnapshot,
        pane_states: Vec<PaneStateSnapshot>,
    ) -> Self {
        let pane_map = pane_states.into_iter().map(|ps| (ps.pane_id, ps)).collect();
        Self {
            captured_at,
            topology,
            pane_states: pane_map,
        }
    }

    /// Apply a single diff snapshot to produce a new state.
    ///
    /// This mutates `self` in place for efficiency.
    pub fn apply_diff(&mut self, diff: &DiffSnapshot) {
        self.captured_at = diff.captured_at;

        for record in &diff.diffs {
            match record {
                SnapshotDiff::PaneScrollbackChanged {
                    pane_id,
                    new_scrollback_ref,
                } => {
                    if let Some(state) = self.pane_states.get_mut(pane_id) {
                        state.scrollback_ref.clone_from(new_scrollback_ref);
                    }
                }
                SnapshotDiff::PaneMetadataChanged { pane_id, new_state } => {
                    self.pane_states.insert(*pane_id, new_state.clone());
                }
                SnapshotDiff::PaneCreated { pane_id, snapshot } => {
                    self.pane_states.insert(*pane_id, snapshot.clone());
                }
                SnapshotDiff::PaneClosed { pane_id } => {
                    self.pane_states.remove(pane_id);
                }
                SnapshotDiff::LayoutChanged { new_topology } => {
                    self.topology = new_topology.clone();
                }
            }
        }
    }
}

// =============================================================================
// Diff chain
// =============================================================================

/// An ordered chain of diffs from a base snapshot.
///
/// Supports restoring state at any point in the chain, and compacting
/// the chain into a new base snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffChain {
    /// The base snapshot (full state).
    pub base: BaseSnapshot,
    /// Ordered diff snapshots (oldest first).
    pub diffs: Vec<DiffSnapshot>,
    /// Next sequence number for the next diff.
    next_seq: u64,
}

impl DiffChain {
    /// Create a new chain from a base snapshot.
    #[must_use]
    pub fn new(base: BaseSnapshot) -> Self {
        Self {
            base,
            diffs: Vec::new(),
            next_seq: 1,
        }
    }

    /// Append a new diff to the chain.
    pub fn push_diff(&mut self, mut diff: DiffSnapshot) {
        diff.seq = self.next_seq;
        self.next_seq += 1;
        self.diffs.push(diff);
    }

    /// Restore the full state at the latest diff (or base if no diffs).
    #[must_use]
    pub fn restore_latest(&self) -> BaseSnapshot {
        let mut state = self.base.clone();
        for diff in &self.diffs {
            state.apply_diff(diff);
        }
        state
    }

    /// Restore the full state at a specific sequence number.
    ///
    /// Returns `None` if the sequence number is not in the chain.
    #[must_use]
    pub fn restore_at(&self, seq: u64) -> Option<BaseSnapshot> {
        if seq == 0 {
            return Some(self.base.clone());
        }

        let mut state = self.base.clone();
        for diff in &self.diffs {
            if diff.seq > seq {
                break;
            }
            state.apply_diff(diff);
        }

        // Check if we actually found the requested seq
        if self.diffs.iter().any(|d| d.seq == seq) {
            Some(state)
        } else {
            None
        }
    }

    /// Compact the chain: merge all diffs into a new base snapshot.
    ///
    /// After compaction, the chain has no diffs and the base reflects
    /// the latest state. Returns the number of diffs that were merged.
    pub fn compact(&mut self) -> usize {
        if self.diffs.is_empty() {
            return 0;
        }
        let count = self.diffs.len();
        self.base = self.restore_latest();
        self.diffs.clear();
        // Don't reset next_seq — sequence numbers are monotonic
        count
    }

    /// Number of diffs in the chain.
    #[must_use]
    pub fn chain_len(&self) -> usize {
        self.diffs.len()
    }
}

// =============================================================================
// Diff snapshot engine
// =============================================================================

/// Engine that captures differential snapshots using a dirty tracker.
///
/// Usage:
/// 1. As pane events occur, call `tracker_mut().mark_*()` methods.
/// 2. Periodically call `capture_diff()` to produce a diff snapshot.
/// 3. The engine maintains the diff chain internally.
/// 4. Call `compact()` when the chain gets too long.
#[derive(Debug)]
pub struct DiffSnapshotEngine {
    /// Dirty tracker for change detection.
    tracker: DirtyTracker,
    /// The diff chain (base + diffs).
    chain: Option<DiffChain>,
    /// Maximum chain length before auto-compaction.
    max_chain_len: usize,
}

impl DiffSnapshotEngine {
    /// Create a new diff snapshot engine.
    ///
    /// `max_chain_len` controls when auto-compaction triggers (0 = never).
    #[must_use]
    pub fn new(max_chain_len: usize) -> Self {
        Self {
            tracker: DirtyTracker::new(),
            chain: None,
            max_chain_len,
        }
    }

    /// Access the dirty tracker for marking changes.
    pub fn tracker_mut(&mut self) -> &mut DirtyTracker {
        &mut self.tracker
    }

    /// Access the dirty tracker (read-only).
    #[must_use]
    pub fn tracker(&self) -> &DirtyTracker {
        &self.tracker
    }

    /// Initialize with a full base snapshot.
    ///
    /// This must be called before `capture_diff()`. Typically called once
    /// with the initial full snapshot from `SnapshotEngine`.
    pub fn initialize(&mut self, base: BaseSnapshot) {
        self.chain = Some(DiffChain::new(base));
        self.tracker.clear();
    }

    /// Returns true if the engine has been initialized with a base snapshot.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.chain.is_some()
    }

    /// Capture a differential snapshot of only the dirty panes.
    ///
    /// `current_panes` provides the current state of panes (only dirty ones
    /// are read). `current_topology` is used if the layout changed.
    ///
    /// Returns `None` if nothing changed.
    pub fn capture_diff(
        &mut self,
        current_panes: &HashMap<u64, PaneStateSnapshot>,
        current_topology: Option<&TopologySnapshot>,
        now_ms: u64,
    ) -> Option<DiffSnapshot> {
        let chain = self.chain.as_mut()?;

        if self.tracker.is_clean() {
            return None;
        }

        let mut diffs = Vec::new();

        // Process closed panes first (before they disappear from current_panes)
        for &pane_id in &self.tracker.closed {
            diffs.push(SnapshotDiff::PaneClosed { pane_id });
        }

        // Process created panes
        for &pane_id in &self.tracker.created {
            if let Some(snapshot) = current_panes.get(&pane_id) {
                diffs.push(SnapshotDiff::PaneCreated {
                    pane_id,
                    snapshot: snapshot.clone(),
                });
            }
        }

        // Process dirty panes (excluding created/closed which are already handled)
        for (&pane_id, fields) in &self.tracker.dirty {
            if self.tracker.created.contains(&pane_id) || self.tracker.closed.contains(&pane_id) {
                continue;
            }

            if let Some(current) = current_panes.get(&pane_id) {
                if fields.contains(&DirtyField::Metadata) {
                    diffs.push(SnapshotDiff::PaneMetadataChanged {
                        pane_id,
                        new_state: current.clone(),
                    });
                } else if fields.contains(&DirtyField::Scrollback) {
                    diffs.push(SnapshotDiff::PaneScrollbackChanged {
                        pane_id,
                        new_scrollback_ref: current.scrollback_ref.clone(),
                    });
                }
            }
        }

        // Process layout changes
        if self.tracker.is_layout_dirty() {
            if let Some(topo) = current_topology {
                diffs.push(SnapshotDiff::LayoutChanged {
                    new_topology: topo.clone(),
                });
            }
        }

        if diffs.is_empty() {
            self.tracker.clear();
            return None;
        }

        let diff = DiffSnapshot {
            seq: 0, // will be set by push_diff
            captured_at: now_ms,
            diffs,
        };

        chain.push_diff(diff.clone());
        self.tracker.clear();

        // Auto-compact if chain is too long
        if self.max_chain_len > 0 && chain.chain_len() > self.max_chain_len {
            chain.compact();
        }

        Some(diff)
    }

    /// Restore the latest state from the diff chain.
    #[must_use]
    pub fn restore_latest(&self) -> Option<BaseSnapshot> {
        self.chain.as_ref().map(DiffChain::restore_latest)
    }

    /// Manually trigger compaction of the diff chain.
    ///
    /// Returns the number of diffs merged, or `None` if not initialized.
    pub fn compact(&mut self) -> Option<usize> {
        self.chain.as_mut().map(DiffChain::compact)
    }

    /// Returns the current chain length (number of diffs since last base).
    #[must_use]
    pub fn chain_len(&self) -> usize {
        self.chain.as_ref().map_or(0, DiffChain::chain_len)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_pane_state::{ScrollbackRef, TerminalState};
    use crate::session_topology::{
        PaneNode, TOPOLOGY_SCHEMA_VERSION, TabSnapshot, TopologySnapshot, WindowSnapshot,
    };

    // ---- Helpers ----

    fn make_terminal(rows: u16, cols: u16) -> TerminalState {
        TerminalState {
            rows,
            cols,
            cursor_row: 0,
            cursor_col: 0,
            is_alt_screen: false,
            title: "test".to_string(),
        }
    }

    fn make_pane_state(pane_id: u64, rows: u16, cols: u16) -> PaneStateSnapshot {
        PaneStateSnapshot::new(pane_id, 1000, make_terminal(rows, cols))
            .with_cwd(format!("/home/user/pane-{pane_id}"))
    }

    fn make_topology(pane_ids: &[u64]) -> TopologySnapshot {
        let tabs: Vec<TabSnapshot> = pane_ids
            .iter()
            .map(|&id| TabSnapshot {
                tab_id: id,
                title: Some(format!("tab-{id}")),
                pane_tree: PaneNode::Leaf {
                    pane_id: id,
                    rows: 24,
                    cols: 80,
                    cwd: None,
                    title: None,
                    is_active: false,
                },
                active_pane_id: Some(id),
            })
            .collect();

        TopologySnapshot {
            schema_version: TOPOLOGY_SCHEMA_VERSION,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: Some("test-window".to_string()),
                position: None,
                size: None,
                tabs,
                active_tab_index: Some(0),
            }],
        }
    }

    fn make_base(pane_ids: &[u64]) -> BaseSnapshot {
        let pane_states: Vec<PaneStateSnapshot> = pane_ids
            .iter()
            .map(|&id| make_pane_state(id, 24, 80))
            .collect();
        BaseSnapshot::new(1000, make_topology(pane_ids), pane_states)
    }

    // ---- DirtyTracker tests ----

    #[test]
    fn tracker_starts_clean() {
        let tracker = DirtyTracker::new();
        assert!(tracker.is_clean());
        assert_eq!(tracker.dirty_count(), 0);
    }

    #[test]
    fn tracker_marks_output_dirty() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_output(1);
        assert!(!tracker.is_clean());
        assert_eq!(tracker.dirty_count(), 1);
        assert!(tracker.dirty_pane_ids().contains(&1));
        assert!(
            tracker
                .dirty_fields(1)
                .unwrap()
                .contains(&DirtyField::Scrollback)
        );
    }

    #[test]
    fn tracker_marks_metadata_dirty() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_metadata(2);
        assert!(
            tracker
                .dirty_fields(2)
                .unwrap()
                .contains(&DirtyField::Metadata)
        );
    }

    #[test]
    fn tracker_marks_created() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_created(3);
        assert!(
            tracker
                .dirty_fields(3)
                .unwrap()
                .contains(&DirtyField::Created)
        );
        assert!(tracker.is_layout_dirty());
    }

    #[test]
    fn tracker_marks_closed() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_closed(4);
        assert!(
            tracker
                .dirty_fields(4)
                .unwrap()
                .contains(&DirtyField::Closed)
        );
        assert!(tracker.is_layout_dirty());
    }

    #[test]
    fn tracker_multiple_fields_per_pane() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_output(1);
        tracker.mark_metadata(1);
        assert_eq!(tracker.dirty_count(), 1);
        let fields = tracker.dirty_fields(1).unwrap();
        assert!(fields.contains(&DirtyField::Scrollback));
        assert!(fields.contains(&DirtyField::Metadata));
    }

    #[test]
    fn tracker_clear_resets_all() {
        let mut tracker = DirtyTracker::new();
        tracker.mark_output(1);
        tracker.mark_created(2);
        tracker.mark_layout_dirty();
        assert!(!tracker.is_clean());

        tracker.clear();
        assert!(tracker.is_clean());
        assert_eq!(tracker.dirty_count(), 0);
        assert!(!tracker.is_layout_dirty());
    }

    // ---- BaseSnapshot tests ----

    #[test]
    fn base_snapshot_from_pane_list() {
        let base = make_base(&[1, 2, 3]);
        assert_eq!(base.pane_states.len(), 3);
        assert!(base.pane_states.contains_key(&1));
        assert!(base.pane_states.contains_key(&2));
        assert!(base.pane_states.contains_key(&3));
    }

    #[test]
    fn apply_diff_pane_created() {
        let mut base = make_base(&[1, 2]);
        let new_pane = make_pane_state(3, 30, 120);

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneCreated {
                pane_id: 3,
                snapshot: new_pane.clone(),
            }],
        };

        base.apply_diff(&diff);
        assert_eq!(base.pane_states.len(), 3);
        assert_eq!(base.pane_states[&3].pane_id, 3);
        assert_eq!(base.captured_at, 2000);
    }

    #[test]
    fn apply_diff_pane_closed() {
        let mut base = make_base(&[1, 2, 3]);
        assert_eq!(base.pane_states.len(), 3);

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneClosed { pane_id: 2 }],
        };

        base.apply_diff(&diff);
        assert_eq!(base.pane_states.len(), 2);
        assert!(!base.pane_states.contains_key(&2));
    }

    #[test]
    fn apply_diff_metadata_changed() {
        let mut base = make_base(&[1, 2]);
        let mut updated = make_pane_state(1, 30, 120);
        updated.cwd = Some("/new/path".to_string());

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneMetadataChanged {
                pane_id: 1,
                new_state: updated,
            }],
        };

        base.apply_diff(&diff);
        assert_eq!(base.pane_states[&1].cwd, Some("/new/path".to_string()));
        assert_eq!(base.pane_states[&1].terminal.rows, 30);
    }

    #[test]
    fn apply_diff_scrollback_changed() {
        let mut base = make_base(&[1]);

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneScrollbackChanged {
                pane_id: 1,
                new_scrollback_ref: Some(ScrollbackRef {
                    output_segments_seq: 42,
                    total_lines_captured: 500,
                    last_capture_at: 1999,
                }),
            }],
        };

        base.apply_diff(&diff);
        let sb = base.pane_states[&1].scrollback_ref.as_ref().unwrap();
        assert_eq!(sb.output_segments_seq, 42);
        assert_eq!(sb.total_lines_captured, 500);
    }

    #[test]
    fn apply_diff_layout_changed() {
        let mut base = make_base(&[1, 2]);
        let new_topo = make_topology(&[1, 2, 3]);

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::LayoutChanged {
                new_topology: new_topo.clone(),
            }],
        };

        base.apply_diff(&diff);
        assert_eq!(base.topology.windows[0].tabs.len(), 3);
    }

    // ---- DiffChain tests ----

    #[test]
    fn chain_restore_latest_no_diffs() {
        let base = make_base(&[1, 2]);
        let chain = DiffChain::new(base.clone());
        let restored = chain.restore_latest();
        assert_eq!(restored.pane_states.len(), 2);
        assert_eq!(restored.captured_at, base.captured_at);
    }

    #[test]
    fn chain_restore_latest_with_diffs() {
        let base = make_base(&[1, 2]);
        let mut chain = DiffChain::new(base);

        // Add pane 3
        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneCreated {
                pane_id: 3,
                snapshot: make_pane_state(3, 24, 80),
            }],
        });

        // Close pane 1
        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 3000,
            diffs: vec![SnapshotDiff::PaneClosed { pane_id: 1 }],
        });

        let restored = chain.restore_latest();
        assert_eq!(restored.pane_states.len(), 2);
        assert!(restored.pane_states.contains_key(&2));
        assert!(restored.pane_states.contains_key(&3));
        assert!(!restored.pane_states.contains_key(&1));
        assert_eq!(restored.captured_at, 3000);
    }

    #[test]
    fn chain_restore_at_specific_seq() {
        let base = make_base(&[1, 2]);
        let mut chain = DiffChain::new(base);

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneCreated {
                pane_id: 3,
                snapshot: make_pane_state(3, 24, 80),
            }],
        });

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 3000,
            diffs: vec![SnapshotDiff::PaneClosed { pane_id: 1 }],
        });

        // At seq 0 (base)
        let at_base = chain.restore_at(0).unwrap();
        assert_eq!(at_base.pane_states.len(), 2);

        // At seq 1 (after adding pane 3)
        let at_1 = chain.restore_at(1).unwrap();
        assert_eq!(at_1.pane_states.len(), 3);

        // At seq 2 (after closing pane 1)
        let at_2 = chain.restore_at(2).unwrap();
        assert_eq!(at_2.pane_states.len(), 2);

        // Invalid seq
        assert!(chain.restore_at(99).is_none());
    }

    #[test]
    fn chain_compact_merges_diffs() {
        let base = make_base(&[1, 2]);
        let mut chain = DiffChain::new(base);

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneCreated {
                pane_id: 3,
                snapshot: make_pane_state(3, 24, 80),
            }],
        });

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 3000,
            diffs: vec![SnapshotDiff::PaneClosed { pane_id: 1 }],
        });

        assert_eq!(chain.chain_len(), 2);

        let merged = chain.compact();
        assert_eq!(merged, 2);
        assert_eq!(chain.chain_len(), 0);

        // Compacted base should have panes 2 and 3
        assert_eq!(chain.base.pane_states.len(), 2);
        assert!(chain.base.pane_states.contains_key(&2));
        assert!(chain.base.pane_states.contains_key(&3));
    }

    #[test]
    fn chain_compact_empty_is_noop() {
        let base = make_base(&[1]);
        let mut chain = DiffChain::new(base);
        let merged = chain.compact();
        assert_eq!(merged, 0);
    }

    #[test]
    fn chain_sequence_numbers_monotonic() {
        let base = make_base(&[1]);
        let mut chain = DiffChain::new(base);

        for i in 0..5 {
            chain.push_diff(DiffSnapshot {
                seq: 0,
                captured_at: 1000 + i * 1000,
                diffs: vec![],
            });
        }

        let seqs: Vec<u64> = chain.diffs.iter().map(|d| d.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn chain_seq_survives_compaction() {
        let base = make_base(&[1]);
        let mut chain = DiffChain::new(base);

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 2000,
            diffs: vec![],
        });
        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 3000,
            diffs: vec![],
        });

        chain.compact();

        chain.push_diff(DiffSnapshot {
            seq: 0,
            captured_at: 4000,
            diffs: vec![],
        });

        // After compaction at seq=2, new diff should get seq=3
        assert_eq!(chain.diffs[0].seq, 3);
    }

    // ---- SnapshotDiff tests ----

    #[test]
    fn diff_pane_id_returns_correct_value() {
        let closed = SnapshotDiff::PaneClosed { pane_id: 42 };
        assert_eq!(closed.pane_id(), Some(42));

        let layout = SnapshotDiff::LayoutChanged {
            new_topology: make_topology(&[1]),
        };
        assert_eq!(layout.pane_id(), None);
    }

    #[test]
    fn diff_snapshot_serialization_roundtrip() {
        let diff = DiffSnapshot {
            seq: 5,
            captured_at: 5000,
            diffs: vec![
                SnapshotDiff::PaneCreated {
                    pane_id: 10,
                    snapshot: make_pane_state(10, 24, 80),
                },
                SnapshotDiff::PaneClosed { pane_id: 1 },
            ],
        };

        let json = serde_json::to_string(&diff).unwrap();
        let restored: DiffSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(diff, restored);
    }

    // ---- DiffSnapshotEngine tests ----

    #[test]
    fn engine_not_initialized_returns_none() {
        let mut engine = DiffSnapshotEngine::new(10);
        assert!(!engine.is_initialized());
        assert!(engine.restore_latest().is_none());
        assert!(engine.capture_diff(&HashMap::new(), None, 1000).is_none());
    }

    #[test]
    fn engine_capture_diff_only_dirty_panes() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2, 3, 4, 5]);
        engine.initialize(base);

        // Mark panes 2 and 4 as dirty
        engine.tracker_mut().mark_metadata(2);
        engine.tracker_mut().mark_output(4);

        let mut current = HashMap::new();
        for id in [1, 2, 3, 4, 5] {
            current.insert(id, make_pane_state(id, 24, 80));
        }
        // Modify pane 2 metadata
        current.get_mut(&2).unwrap().cwd = Some("/changed".to_string());

        let diff = engine.capture_diff(&current, None, 2000);
        assert!(diff.is_some());
        let diff = diff.unwrap();

        // Should only have diffs for panes 2 and 4
        assert_eq!(diff.diffs.len(), 2);
        let pane_ids: HashSet<u64> = diff.diffs.iter().filter_map(|d| d.pane_id()).collect();
        assert!(pane_ids.contains(&2));
        assert!(pane_ids.contains(&4));
        assert!(!pane_ids.contains(&1));
        assert!(!pane_ids.contains(&3));
    }

    #[test]
    fn engine_capture_diff_clean_returns_none() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2]);
        engine.initialize(base);

        // No dirty panes
        let diff = engine.capture_diff(&HashMap::new(), None, 2000);
        assert!(diff.is_none());
    }

    #[test]
    fn engine_capture_diff_created_pane() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2]);
        engine.initialize(base);

        engine.tracker_mut().mark_created(3);

        let mut current = HashMap::new();
        current.insert(3, make_pane_state(3, 30, 120));

        let diff = engine.capture_diff(&current, None, 2000).unwrap();
        assert!(
            diff.diffs
                .iter()
                .any(|d| matches!(d, SnapshotDiff::PaneCreated { pane_id: 3, .. }))
        );

        // Restore should have pane 3
        let restored = engine.restore_latest().unwrap();
        assert!(restored.pane_states.contains_key(&3));
    }

    #[test]
    fn engine_capture_diff_closed_pane() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2, 3]);
        engine.initialize(base);

        engine.tracker_mut().mark_closed(2);

        let diff = engine.capture_diff(&HashMap::new(), None, 2000).unwrap();
        assert!(
            diff.diffs
                .iter()
                .any(|d| matches!(d, SnapshotDiff::PaneClosed { pane_id: 2 }))
        );

        let restored = engine.restore_latest().unwrap();
        assert!(!restored.pane_states.contains_key(&2));
        assert_eq!(restored.pane_states.len(), 2);
    }

    #[test]
    fn engine_auto_compaction() {
        let mut engine = DiffSnapshotEngine::new(3); // compact after 3 diffs
        let base = make_base(&[1]);
        engine.initialize(base);

        let mut current = HashMap::new();
        current.insert(1, make_pane_state(1, 24, 80));

        for i in 0..4 {
            engine.tracker_mut().mark_metadata(1);
            current.get_mut(&1).unwrap().cwd = Some(format!("/path/{i}"));
            engine.capture_diff(&current, None, 2000 + i * 1000);
        }

        // After 4 captures with max_chain_len=3, should have auto-compacted
        // Chain len should be 0 (compacted) or 1 (one after compaction)
        assert!(engine.chain_len() <= 1);
    }

    #[test]
    fn engine_clears_tracker_after_capture() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2]);
        engine.initialize(base);

        engine.tracker_mut().mark_output(1);
        assert!(!engine.tracker().is_clean());

        let mut current = HashMap::new();
        current.insert(1, make_pane_state(1, 24, 80));
        engine.capture_diff(&current, None, 2000);

        assert!(engine.tracker().is_clean());
    }

    #[test]
    fn engine_layout_change_captured() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2]);
        engine.initialize(base);

        engine.tracker_mut().mark_layout_dirty();

        let new_topo = make_topology(&[1, 2, 3]);
        let diff = engine
            .capture_diff(&HashMap::new(), Some(&new_topo), 2000)
            .unwrap();

        assert!(
            diff.diffs
                .iter()
                .any(|d| matches!(d, SnapshotDiff::LayoutChanged { .. }))
        );

        let restored = engine.restore_latest().unwrap();
        assert_eq!(restored.topology.windows[0].tabs.len(), 3);
    }

    #[test]
    fn engine_manual_compact() {
        let mut engine = DiffSnapshotEngine::new(0); // no auto-compact
        let base = make_base(&[1]);
        engine.initialize(base);

        let mut current = HashMap::new();
        current.insert(1, make_pane_state(1, 24, 80));

        for i in 0..5 {
            engine.tracker_mut().mark_metadata(1);
            current.get_mut(&1).unwrap().cwd = Some(format!("/path/{i}"));
            engine.capture_diff(&current, None, 2000 + i * 1000);
        }

        assert_eq!(engine.chain_len(), 5);

        let merged = engine.compact().unwrap();
        assert_eq!(merged, 5);
        assert_eq!(engine.chain_len(), 0);
    }

    #[test]
    fn engine_pane_created_after_base_then_closed() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1]);
        engine.initialize(base);

        // Create pane 5
        engine.tracker_mut().mark_created(5);
        let mut current = HashMap::new();
        current.insert(5, make_pane_state(5, 24, 80));
        engine.capture_diff(&current, None, 2000);

        let restored = engine.restore_latest().unwrap();
        assert!(restored.pane_states.contains_key(&5));
        assert_eq!(restored.pane_states.len(), 2);

        // Close pane 5 before next snapshot
        engine.tracker_mut().mark_closed(5);
        engine.capture_diff(&HashMap::new(), None, 3000);

        let restored = engine.restore_latest().unwrap();
        assert!(!restored.pane_states.contains_key(&5));
        assert_eq!(restored.pane_states.len(), 1);
    }

    #[test]
    fn engine_restore_after_compaction_matches_before() {
        let mut engine = DiffSnapshotEngine::new(10);
        let base = make_base(&[1, 2, 3]);
        engine.initialize(base);

        let mut current: HashMap<u64, PaneStateSnapshot> = HashMap::new();
        for id in [1, 2, 3] {
            current.insert(id, make_pane_state(id, 24, 80));
        }

        // Make several changes
        engine.tracker_mut().mark_metadata(1);
        current.get_mut(&1).unwrap().cwd = Some("/new/path/1".to_string());
        engine.capture_diff(&current, None, 2000);

        engine.tracker_mut().mark_created(4);
        current.insert(4, make_pane_state(4, 30, 120));
        engine.capture_diff(&current, None, 3000);

        engine.tracker_mut().mark_closed(2);
        engine.capture_diff(&current, None, 4000);

        // Snapshot state before compaction
        let before = engine.restore_latest().unwrap();

        // Compact
        engine.compact();

        // State after compaction should be identical
        let after = engine.restore_latest().unwrap();
        assert_eq!(before.pane_states.len(), after.pane_states.len());
        for (id, state) in &before.pane_states {
            assert_eq!(state, after.pane_states.get(id).unwrap());
        }
    }

    // ---- proptest ----

    #[cfg(test)]
    mod prop {
        use super::*;
        use proptest::prelude::*;

        #[derive(Debug, Clone)]
        enum PaneAction {
            Create(u64),
            Close(u64),
            ModifyMetadata(u64),
            ModifyScrollback(u64),
        }

        fn arb_action(max_pane_id: u64) -> impl Strategy<Value = PaneAction> {
            let id_range = 1..=max_pane_id;
            prop_oneof![
                id_range.clone().prop_map(PaneAction::Create),
                id_range.clone().prop_map(PaneAction::Close),
                id_range.clone().prop_map(PaneAction::ModifyMetadata),
                id_range.prop_map(PaneAction::ModifyScrollback),
            ]
        }

        fn arb_action_sequence(
            len: usize,
            max_pane_id: u64,
        ) -> impl Strategy<Value = Vec<PaneAction>> {
            proptest::collection::vec(arb_action(max_pane_id), 1..=len)
        }

        proptest! {
            /// After any sequence of actions + snapshot + restore, the restored
            /// state matches the live "current" state.
            #[test]
            fn restore_matches_live_state(
                actions in arb_action_sequence(20, 10)
            ) {
                let initial_ids: Vec<u64> = vec![1, 2, 3];
                let mut engine = DiffSnapshotEngine::new(0);
                engine.initialize(make_base(&initial_ids));

                let mut live_panes: HashMap<u64, PaneStateSnapshot> = initial_ids
                    .iter()
                    .map(|&id| (id, make_pane_state(id, 24, 80)))
                    .collect();

                let mut time = 2000u64;

                for action in &actions {
                    match action {
                        PaneAction::Create(id) => {
                            if !live_panes.contains_key(id) {
                                let ps = make_pane_state(*id, 24, 80);
                                live_panes.insert(*id, ps);
                                engine.tracker_mut().mark_created(*id);
                            }
                        }
                        PaneAction::Close(id) => {
                            if live_panes.contains_key(id) {
                                live_panes.remove(id);
                                engine.tracker_mut().mark_closed(*id);
                            }
                        }
                        PaneAction::ModifyMetadata(id) => {
                            if let Some(ps) = live_panes.get_mut(id) {
                                ps.cwd = Some(format!("/modified/{time}"));
                                engine.tracker_mut().mark_metadata(*id);
                            }
                        }
                        PaneAction::ModifyScrollback(id) => {
                            if let Some(ps) = live_panes.get_mut(id) {
                                ps.scrollback_ref = Some(ScrollbackRef {
                                    output_segments_seq: time as i64,
                                    total_lines_captured: time,
                                    last_capture_at: time,
                                });
                                engine.tracker_mut().mark_output(*id);
                            }
                        }
                    }

                    time += 1000;
                    engine.capture_diff(&live_panes, None, time);
                }

                let restored = engine.restore_latest().unwrap();
                // Same set of pane IDs
                let live_ids: HashSet<u64> = live_panes.keys().copied().collect();
                let restored_ids: HashSet<u64> = restored.pane_states.keys().copied().collect();
                prop_assert_eq!(live_ids, restored_ids);

                // Each pane state matches
                for (id, live_state) in &live_panes {
                    let restored_state = restored.pane_states.get(id).unwrap();
                    prop_assert_eq!(live_state.cwd.as_deref(), restored_state.cwd.as_deref());
                    prop_assert_eq!(&live_state.scrollback_ref, &restored_state.scrollback_ref);
                }
            }

            /// Compaction preserves final state.
            #[test]
            fn compaction_preserves_state(
                actions in arb_action_sequence(15, 8)
            ) {
                let initial_ids: Vec<u64> = vec![1, 2, 3];
                let mut engine = DiffSnapshotEngine::new(0);
                engine.initialize(make_base(&initial_ids));

                let mut live_panes: HashMap<u64, PaneStateSnapshot> = initial_ids
                    .iter()
                    .map(|&id| (id, make_pane_state(id, 24, 80)))
                    .collect();

                let mut time = 2000u64;

                for action in &actions {
                    match action {
                        PaneAction::Create(id) => {
                            if !live_panes.contains_key(id) {
                                live_panes.insert(*id, make_pane_state(*id, 24, 80));
                                engine.tracker_mut().mark_created(*id);
                            }
                        }
                        PaneAction::Close(id) => {
                            if live_panes.contains_key(id) {
                                live_panes.remove(id);
                                engine.tracker_mut().mark_closed(*id);
                            }
                        }
                        PaneAction::ModifyMetadata(id) => {
                            if let Some(ps) = live_panes.get_mut(id) {
                                ps.cwd = Some(format!("/m/{time}"));
                                engine.tracker_mut().mark_metadata(*id);
                            }
                        }
                        PaneAction::ModifyScrollback(id) => {
                            if let Some(ps) = live_panes.get_mut(id) {
                                ps.scrollback_ref = Some(ScrollbackRef {
                                    output_segments_seq: time as i64,
                                    total_lines_captured: time,
                                    last_capture_at: time,
                                });
                                engine.tracker_mut().mark_output(*id);
                            }
                        }
                    }
                    time += 1000;
                    engine.capture_diff(&live_panes, None, time);
                }

                let before = engine.restore_latest().unwrap();
                engine.compact();
                let after = engine.restore_latest().unwrap();

                let before_ids: HashSet<u64> = before.pane_states.keys().copied().collect();
                let after_ids: HashSet<u64> = after.pane_states.keys().copied().collect();
                prop_assert_eq!(before_ids, after_ids);

                for (id, before_state) in &before.pane_states {
                    let after_state = after.pane_states.get(id).unwrap();
                    prop_assert_eq!(before_state, after_state);
                }
            }

            /// Dirty tracker always reports accurate dirty count.
            #[test]
            fn dirty_count_matches_dirty_set(
                actions in arb_action_sequence(30, 20)
            ) {
                let mut tracker = DirtyTracker::new();

                for action in &actions {
                    match action {
                        PaneAction::Create(id) => tracker.mark_created(*id),
                        PaneAction::Close(id) => tracker.mark_closed(*id),
                        PaneAction::ModifyMetadata(id) => tracker.mark_metadata(*id),
                        PaneAction::ModifyScrollback(id) => tracker.mark_output(*id),
                    }
                }

                prop_assert_eq!(tracker.dirty_count(), tracker.dirty_pane_ids().len());
                prop_assert_eq!(tracker.is_clean(), tracker.dirty_count() == 0);
            }
        }
    }
}
