//! Scope tree for structured concurrency across FrankenTerm task domains.
//!
//! Models explicit scope hierarchy for every long-lived task domain:
//! - Root scope (application lifetime)
//! - Daemon scopes (discovery, capture, persistence, maintenance)
//! - Watcher scopes (native events, config reload, health monitors)
//! - Worker scopes (connection handlers, IPC processors)
//! - Ephemeral scopes (one-shot operations, queries)
//!
//! # Architecture
//!
//! ```text
//! root
//! ├── daemon:discovery       (pane discovery polling)
//! ├── daemon:capture         (content capture pipeline)
//! │   ├── worker:capture:0   (per-pane capture)
//! │   └── worker:capture:1
//! ├── daemon:relay           (MPSC→SPMC bridge)
//! ├── daemon:persistence     (storage + pattern detection)
//! ├── daemon:maintenance     (retention, GC, checkpointing)
//! ├── watcher:native_events  (native event subscription)
//! ├── watcher:snapshot       (session persistence engine)
//! ├── watcher:config_reload  (hot-reload listener)
//! └── ephemeral:query:*      (one-shot robot/CLI queries)
//! ```
//!
//! # Shutdown ordering
//!
//! Scopes shut down bottom-up: children drain before parents. Within a tier,
//! shutdown proceeds in reverse registration order (LIFO). This matches
//! asupersync's region quiescence model.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

// ── Scope Identity ──────────────────────────────────────────────────────────

/// Unique identifier for a scope node in the tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScopeId(pub String);

impl ScopeId {
    /// Build a scope ID from path components (e.g. `["daemon", "capture"]` → `"daemon:capture"`).
    #[must_use]
    pub fn from_path(components: &[&str]) -> Self {
        Self(components.join(":"))
    }

    /// The root scope ID.
    #[must_use]
    pub fn root() -> Self {
        Self("root".into())
    }

    /// True if this is the root scope.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0 == "root"
    }
}

impl fmt::Display for ScopeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ── Scope Tier ──────────────────────────────────────────────────────────────

/// Classification of a scope's lifetime tier.
///
/// Tiers determine default shutdown priority and resource budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScopeTier {
    /// Application root — lives for entire process lifetime.
    Root,
    /// Long-lived daemon tasks (discovery, capture, persistence, maintenance).
    Daemon,
    /// Event-driven watchers (native events, config reload, health monitors).
    Watcher,
    /// Short-lived worker tasks (connection handlers, per-pane capture).
    Worker,
    /// One-shot ephemeral operations (queries, robot commands).
    Ephemeral,
}

impl ScopeTier {
    /// Default shutdown priority (higher = shuts down first).
    /// Ephemeral tasks drain first, then workers, then watchers, then daemons, then root.
    #[must_use]
    pub fn shutdown_priority(self) -> u32 {
        match self {
            Self::Root => 0,
            Self::Daemon => 10,
            Self::Watcher => 20,
            Self::Worker => 30,
            Self::Ephemeral => 40,
        }
    }

    /// Whether this tier's scopes can spawn children.
    #[must_use]
    pub fn can_have_children(self) -> bool {
        matches!(self, Self::Root | Self::Daemon | Self::Watcher)
    }
}

impl fmt::Display for ScopeTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root => f.write_str("root"),
            Self::Daemon => f.write_str("daemon"),
            Self::Watcher => f.write_str("watcher"),
            Self::Worker => f.write_str("worker"),
            Self::Ephemeral => f.write_str("ephemeral"),
        }
    }
}

// ── Scope State ─────────────────────────────────────────────────────────────

/// Lifecycle state of a scope node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeState {
    /// Scope is created but not yet started.
    Created,
    /// Scope is actively running.
    Running,
    /// Shutdown requested; draining children.
    Draining,
    /// All children drained; running finalizers.
    Finalizing,
    /// Scope fully closed.
    Closed,
}

impl ScopeState {
    /// True if the scope is in a terminal state.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        self == Self::Closed
    }

    /// True if the scope is shutting down (draining or finalizing).
    #[must_use]
    pub fn is_shutting_down(self) -> bool {
        matches!(self, Self::Draining | Self::Finalizing)
    }

    /// True if the scope can accept new children.
    #[must_use]
    pub fn accepts_children(self) -> bool {
        matches!(self, Self::Created | Self::Running)
    }
}

impl fmt::Display for ScopeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => f.write_str("created"),
            Self::Running => f.write_str("running"),
            Self::Draining => f.write_str("draining"),
            Self::Finalizing => f.write_str("finalizing"),
            Self::Closed => f.write_str("closed"),
        }
    }
}

// ── Scope Node ──────────────────────────────────────────────────────────────

/// A node in the scope tree representing a task domain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeNode {
    /// Unique identifier.
    pub id: ScopeId,
    /// Lifetime tier classification.
    pub tier: ScopeTier,
    /// Current lifecycle state.
    pub state: ScopeState,
    /// Parent scope ID (None for root).
    pub parent_id: Option<ScopeId>,
    /// Ordered list of child scope IDs (insertion order preserved for LIFO shutdown).
    pub children: Vec<ScopeId>,
    /// Human-readable description.
    pub description: String,
    /// Timestamp (epoch ms) when the scope was created.
    pub created_at_ms: i64,
    /// Timestamp (epoch ms) when the scope entered Running state.
    pub started_at_ms: Option<i64>,
    /// Timestamp (epoch ms) when shutdown was requested.
    pub shutdown_requested_at_ms: Option<i64>,
    /// Timestamp (epoch ms) when the scope fully closed.
    pub closed_at_ms: Option<i64>,
    /// Custom metadata tags.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tags: HashMap<String, String>,
}

impl ScopeNode {
    /// Construct a new scope node.
    #[must_use]
    pub fn new(
        id: ScopeId,
        tier: ScopeTier,
        parent_id: Option<ScopeId>,
        description: impl Into<String>,
        created_at_ms: i64,
    ) -> Self {
        Self {
            id,
            tier,
            state: ScopeState::Created,
            parent_id,
            children: Vec::new(),
            description: description.into(),
            created_at_ms,
            started_at_ms: None,
            shutdown_requested_at_ms: None,
            closed_at_ms: None,
            tags: HashMap::new(),
        }
    }

    /// True if this node has live (non-closed) children.
    #[must_use]
    pub fn has_live_children(&self, tree: &ScopeTree) -> bool {
        self.children.iter().any(|cid| {
            tree.get(cid)
                .is_some_and(|child| !child.state.is_terminal())
        })
    }

    /// Deterministic canonical string for hashing and comparison.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "scope_id={}|tier={}|state={}|parent={}|children={}|created={}",
            self.id,
            self.tier,
            self.state,
            self.parent_id
                .as_ref()
                .map_or_else(|| "none".to_string(), |p| p.0.clone()),
            self.children.len(),
            self.created_at_ms,
        )
    }
}

// ── Scope Tree Errors ───────────────────────────────────────────────────────

/// Errors from scope tree operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeTreeError {
    /// Scope ID already exists in the tree.
    DuplicateScope { scope_id: ScopeId },
    /// Parent scope not found.
    ParentNotFound { parent_id: ScopeId },
    /// Parent scope is not accepting children (shutting down or closed).
    ParentNotAccepting {
        parent_id: ScopeId,
        state: ScopeState,
    },
    /// Scope not found.
    ScopeNotFound { scope_id: ScopeId },
    /// Invalid state transition.
    InvalidTransition {
        scope_id: ScopeId,
        from: ScopeState,
        to: ScopeState,
    },
    /// Cannot close: scope has live children.
    HasLiveChildren {
        scope_id: ScopeId,
        live_count: usize,
    },
    /// Tier does not support children.
    TierCannotHaveChildren { scope_id: ScopeId, tier: ScopeTier },
}

impl fmt::Display for ScopeTreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateScope { scope_id } => {
                write!(f, "scope already exists: {scope_id}")
            }
            Self::ParentNotFound { parent_id } => {
                write!(f, "parent scope not found: {parent_id}")
            }
            Self::ParentNotAccepting { parent_id, state } => {
                write!(
                    f,
                    "parent {parent_id} not accepting children (state: {state})"
                )
            }
            Self::ScopeNotFound { scope_id } => {
                write!(f, "scope not found: {scope_id}")
            }
            Self::InvalidTransition { scope_id, from, to } => {
                write!(f, "invalid transition for {scope_id}: {from} → {to}")
            }
            Self::HasLiveChildren {
                scope_id,
                live_count,
            } => {
                write!(f, "scope {scope_id} has {live_count} live children")
            }
            Self::TierCannotHaveChildren { scope_id, tier } => {
                write!(f, "tier {tier} cannot have children (scope: {scope_id})")
            }
        }
    }
}

// ── Scope Tree ──────────────────────────────────────────────────────────────

/// The scope tree: a hierarchical registry of task domain lifetimes.
///
/// Thread-safety: This struct is not `Sync` — use `Arc<Mutex<ScopeTree>>`
/// for shared access across tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeTree {
    /// All nodes indexed by ID.
    nodes: HashMap<ScopeId, ScopeNode>,
    /// The root scope ID.
    root_id: ScopeId,
}

impl ScopeTree {
    /// Create a new scope tree with a root node.
    #[must_use]
    pub fn new(created_at_ms: i64) -> Self {
        let root_id = ScopeId::root();
        let root = ScopeNode::new(
            root_id.clone(),
            ScopeTier::Root,
            None,
            "application root",
            created_at_ms,
        );
        let mut nodes = HashMap::new();
        nodes.insert(root_id.clone(), root);
        Self { nodes, root_id }
    }

    /// Get a reference to the root node.
    #[must_use]
    pub fn root(&self) -> &ScopeNode {
        self.nodes.get(&self.root_id).expect("root always exists")
    }

    /// Get a reference to a scope by ID.
    #[must_use]
    pub fn get(&self, id: &ScopeId) -> Option<&ScopeNode> {
        self.nodes.get(id)
    }

    /// Get a mutable reference to a scope by ID.
    pub fn get_mut(&mut self, id: &ScopeId) -> Option<&mut ScopeNode> {
        self.nodes.get_mut(id)
    }

    /// Total number of nodes in the tree.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True if the tree contains only the root node.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    /// Register a new child scope under the given parent.
    pub fn register(
        &mut self,
        id: ScopeId,
        tier: ScopeTier,
        parent_id: &ScopeId,
        description: impl Into<String>,
        created_at_ms: i64,
    ) -> Result<(), ScopeTreeError> {
        // Prevent duplicates
        if self.nodes.contains_key(&id) {
            return Err(ScopeTreeError::DuplicateScope { scope_id: id });
        }

        // Validate parent
        let parent = self
            .nodes
            .get(parent_id)
            .ok_or_else(|| ScopeTreeError::ParentNotFound {
                parent_id: parent_id.clone(),
            })?;

        if !parent.state.accepts_children() {
            return Err(ScopeTreeError::ParentNotAccepting {
                parent_id: parent_id.clone(),
                state: parent.state,
            });
        }

        if !parent.tier.can_have_children() {
            return Err(ScopeTreeError::TierCannotHaveChildren {
                scope_id: parent_id.clone(),
                tier: parent.tier,
            });
        }

        // Create and insert node
        let node = ScopeNode::new(
            id.clone(),
            tier,
            Some(parent_id.clone()),
            description,
            created_at_ms,
        );
        self.nodes.insert(id.clone(), node);

        // Register as child of parent
        let parent_mut = self
            .nodes
            .get_mut(parent_id)
            .expect("parent validated above");
        parent_mut.children.push(id);

        Ok(())
    }

    /// Transition a scope to Running state.
    pub fn start(&mut self, id: &ScopeId, timestamp_ms: i64) -> Result<(), ScopeTreeError> {
        let node = self
            .nodes
            .get_mut(id)
            .ok_or_else(|| ScopeTreeError::ScopeNotFound {
                scope_id: id.clone(),
            })?;

        if node.state != ScopeState::Created {
            return Err(ScopeTreeError::InvalidTransition {
                scope_id: id.clone(),
                from: node.state,
                to: ScopeState::Running,
            });
        }

        node.state = ScopeState::Running;
        node.started_at_ms = Some(timestamp_ms);
        Ok(())
    }

    /// Request shutdown of a scope (transitions to Draining).
    pub fn request_shutdown(
        &mut self,
        id: &ScopeId,
        timestamp_ms: i64,
    ) -> Result<(), ScopeTreeError> {
        let node = self
            .nodes
            .get_mut(id)
            .ok_or_else(|| ScopeTreeError::ScopeNotFound {
                scope_id: id.clone(),
            })?;

        if !matches!(node.state, ScopeState::Created | ScopeState::Running) {
            return Err(ScopeTreeError::InvalidTransition {
                scope_id: id.clone(),
                from: node.state,
                to: ScopeState::Draining,
            });
        }

        node.state = ScopeState::Draining;
        node.shutdown_requested_at_ms = Some(timestamp_ms);
        Ok(())
    }

    /// Transition a scope to Finalizing (all children must be closed first).
    pub fn finalize(&mut self, id: &ScopeId) -> Result<(), ScopeTreeError> {
        // Check live children first (need immutable borrow)
        let live_count = {
            let node = self
                .nodes
                .get(id)
                .ok_or_else(|| ScopeTreeError::ScopeNotFound {
                    scope_id: id.clone(),
                })?;
            if node.state != ScopeState::Draining {
                return Err(ScopeTreeError::InvalidTransition {
                    scope_id: id.clone(),
                    from: node.state,
                    to: ScopeState::Finalizing,
                });
            }
            node.children
                .iter()
                .filter(|cid| self.nodes.get(*cid).is_some_and(|c| !c.state.is_terminal()))
                .count()
        };

        if live_count > 0 {
            return Err(ScopeTreeError::HasLiveChildren {
                scope_id: id.clone(),
                live_count,
            });
        }

        let node = self.nodes.get_mut(id).expect("validated above");
        node.state = ScopeState::Finalizing;
        Ok(())
    }

    /// Close a scope (must be in Finalizing state).
    pub fn close(&mut self, id: &ScopeId, timestamp_ms: i64) -> Result<(), ScopeTreeError> {
        let node = self
            .nodes
            .get_mut(id)
            .ok_or_else(|| ScopeTreeError::ScopeNotFound {
                scope_id: id.clone(),
            })?;

        if node.state != ScopeState::Finalizing {
            return Err(ScopeTreeError::InvalidTransition {
                scope_id: id.clone(),
                from: node.state,
                to: ScopeState::Closed,
            });
        }

        node.state = ScopeState::Closed;
        node.closed_at_ms = Some(timestamp_ms);
        Ok(())
    }

    /// Compute the shutdown order: nodes sorted by depth (deepest first),
    /// then by shutdown priority (highest first), then by LIFO registration order.
    #[must_use]
    pub fn shutdown_order(&self) -> Vec<ScopeId> {
        let mut entries: Vec<(ScopeId, usize, u32, usize)> = Vec::new();

        for (id, node) in &self.nodes {
            if node.state.is_terminal() {
                continue;
            }
            let depth = self.depth(id);
            let priority = node.tier.shutdown_priority();
            // LIFO: later-registered children should shut down first.
            // We use the index in parent's children list (inverted).
            let lifo_order = node
                .parent_id
                .as_ref()
                .and_then(|pid| self.nodes.get(pid))
                .and_then(|parent| parent.children.iter().position(|c| c == id))
                .map_or(0, |pos| usize::MAX - pos);

            entries.push((id.clone(), depth, priority, lifo_order));
        }

        // Sort: deepest first, then highest priority, then LIFO
        entries.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(b.3.cmp(&a.3)));

        entries.into_iter().map(|(id, _, _, _)| id).collect()
    }

    /// Compute the depth of a scope (root = 0).
    #[must_use]
    pub fn depth(&self, id: &ScopeId) -> usize {
        let mut d = 0;
        let mut current = id.clone();
        while let Some(node) = self.nodes.get(&current) {
            if let Some(ref pid) = node.parent_id {
                d += 1;
                current = pid.clone();
            } else {
                break;
            }
        }
        d
    }

    /// Count scopes by state.
    #[must_use]
    pub fn count_by_state(&self, state: ScopeState) -> usize {
        self.nodes.values().filter(|n| n.state == state).count()
    }

    /// Count scopes by tier.
    #[must_use]
    pub fn count_by_tier(&self, tier: ScopeTier) -> usize {
        self.nodes.values().filter(|n| n.tier == tier).count()
    }

    /// All scope IDs for a given tier.
    #[must_use]
    pub fn scopes_for_tier(&self, tier: ScopeTier) -> Vec<ScopeId> {
        self.nodes
            .values()
            .filter(|n| n.tier == tier)
            .map(|n| n.id.clone())
            .collect()
    }

    /// Direct children of a scope.
    #[must_use]
    pub fn children_of(&self, id: &ScopeId) -> Vec<&ScopeNode> {
        self.nodes
            .get(id)
            .map(|node| {
                node.children
                    .iter()
                    .filter_map(|cid| self.nodes.get(cid))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// All descendant scope IDs (breadth-first).
    #[must_use]
    pub fn descendants(&self, id: &ScopeId) -> Vec<ScopeId> {
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();

        if let Some(node) = self.nodes.get(id) {
            for cid in &node.children {
                queue.push_back(cid.clone());
            }
        }

        while let Some(current) = queue.pop_front() {
            result.push(current.clone());
            if let Some(node) = self.nodes.get(&current) {
                for cid in &node.children {
                    queue.push_back(cid.clone());
                }
            }
        }

        result
    }

    /// Produce a snapshot of the tree for diagnostics/telemetry.
    #[must_use]
    pub fn snapshot(&self) -> ScopeTreeSnapshot {
        ScopeTreeSnapshot {
            total_scopes: self.nodes.len(),
            running: self.count_by_state(ScopeState::Running),
            draining: self.count_by_state(ScopeState::Draining),
            closed: self.count_by_state(ScopeState::Closed),
            daemons: self.count_by_tier(ScopeTier::Daemon),
            watchers: self.count_by_tier(ScopeTier::Watcher),
            workers: self.count_by_tier(ScopeTier::Worker),
            ephemeral: self.count_by_tier(ScopeTier::Ephemeral),
            max_depth: self
                .nodes
                .keys()
                .map(|id| self.depth(id))
                .max()
                .unwrap_or(0),
        }
    }

    /// Serde-friendly representation of the full tree.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let snap = self.snapshot();
        format!(
            "scope_tree|total={}|running={}|draining={}|closed={}|daemons={}|watchers={}|workers={}|ephemeral={}|max_depth={}",
            snap.total_scopes,
            snap.running,
            snap.draining,
            snap.closed,
            snap.daemons,
            snap.watchers,
            snap.workers,
            snap.ephemeral,
            snap.max_depth,
        )
    }
}

/// Diagnostic snapshot of scope tree state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeTreeSnapshot {
    pub total_scopes: usize,
    pub running: usize,
    pub draining: usize,
    pub closed: usize,
    pub daemons: usize,
    pub watchers: usize,
    pub workers: usize,
    pub ephemeral: usize,
    pub max_depth: usize,
}

// ── Thread-safe Handle ──────────────────────────────────────────────────────

/// A thread-safe, atomically-flagged scope handle for shutdown coordination.
///
/// Each running scope holds a `ScopeHandle` that it checks in its polling loop.
/// The tree manager sets the shutdown flag when the scope should drain.
#[derive(Debug, Clone)]
pub struct ScopeHandle {
    /// The scope's identity.
    pub scope_id: ScopeId,
    /// Shutdown flag — set when the scope should begin draining.
    pub shutdown_flag: Arc<AtomicBool>,
    /// Monotonic generation counter — incremented on each lifecycle transition.
    pub generation: Arc<AtomicU64>,
}

impl ScopeHandle {
    /// Create a new handle for a scope.
    #[must_use]
    pub fn new(scope_id: ScopeId) -> Self {
        Self {
            scope_id,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Request shutdown by setting the flag.
    pub fn request_shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::Release);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Check if shutdown has been requested.
    #[must_use]
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown_flag.load(Ordering::Acquire)
    }

    /// Current generation counter.
    #[must_use]
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

// ── Well-known Scope IDs ────────────────────────────────────────────────────

/// Well-known scope IDs for the standard FrankenTerm task domains.
pub mod well_known {
    use super::ScopeId;

    pub fn root() -> ScopeId {
        ScopeId::root()
    }
    pub fn discovery() -> ScopeId {
        ScopeId::from_path(&["daemon", "discovery"])
    }
    pub fn capture() -> ScopeId {
        ScopeId::from_path(&["daemon", "capture"])
    }
    pub fn relay() -> ScopeId {
        ScopeId::from_path(&["daemon", "relay"])
    }
    pub fn persistence() -> ScopeId {
        ScopeId::from_path(&["daemon", "persistence"])
    }
    pub fn maintenance() -> ScopeId {
        ScopeId::from_path(&["daemon", "maintenance"])
    }
    pub fn native_events() -> ScopeId {
        ScopeId::from_path(&["watcher", "native_events"])
    }
    pub fn snapshot() -> ScopeId {
        ScopeId::from_path(&["watcher", "snapshot"])
    }
    pub fn config_reload() -> ScopeId {
        ScopeId::from_path(&["watcher", "config_reload"])
    }
    pub fn capture_worker(index: usize) -> ScopeId {
        ScopeId(format!("worker:capture:{index}"))
    }
    pub fn ipc_handler(conn_id: u64) -> ScopeId {
        ScopeId(format!("worker:ipc:{conn_id}"))
    }
    pub fn ephemeral_query(query_id: &str) -> ScopeId {
        ScopeId(format!("ephemeral:query:{query_id}"))
    }
}

/// Register the standard FrankenTerm daemon and watcher scopes.
pub fn register_standard_scopes(
    tree: &mut ScopeTree,
    created_at_ms: i64,
) -> Result<(), ScopeTreeError> {
    let root = ScopeId::root();

    // Daemons (under root)
    tree.register(
        well_known::discovery(),
        ScopeTier::Daemon,
        &root,
        "pane discovery polling daemon",
        created_at_ms,
    )?;
    tree.register(
        well_known::capture(),
        ScopeTier::Daemon,
        &root,
        "content capture pipeline daemon",
        created_at_ms,
    )?;
    tree.register(
        well_known::relay(),
        ScopeTier::Daemon,
        &root,
        "MPSC→SPMC relay bridge daemon",
        created_at_ms,
    )?;
    tree.register(
        well_known::persistence(),
        ScopeTier::Daemon,
        &root,
        "storage + pattern detection daemon",
        created_at_ms,
    )?;
    tree.register(
        well_known::maintenance(),
        ScopeTier::Daemon,
        &root,
        "retention, GC, and checkpointing daemon",
        created_at_ms,
    )?;

    // Watchers (under root)
    tree.register(
        well_known::native_events(),
        ScopeTier::Watcher,
        &root,
        "native event subscription watcher",
        created_at_ms,
    )?;
    tree.register(
        well_known::snapshot(),
        ScopeTier::Watcher,
        &root,
        "session persistence engine watcher",
        created_at_ms,
    )?;
    tree.register(
        well_known::config_reload(),
        ScopeTier::Watcher,
        &root,
        "hot-reload config listener watcher",
        created_at_ms,
    )?;

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tree_has_root() {
        let tree = ScopeTree::new(1000);
        assert_eq!(tree.len(), 1);
        assert!(tree.root().id.is_root());
        assert_eq!(tree.root().tier, ScopeTier::Root);
        assert_eq!(tree.root().state, ScopeState::Created);
    }

    #[test]
    fn register_child_scope() {
        let mut tree = ScopeTree::new(1000);
        tree.register(
            well_known::discovery(),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "discovery",
            1000,
        )
        .unwrap();
        assert_eq!(tree.len(), 2);
        let node = tree.get(&well_known::discovery()).unwrap();
        assert_eq!(node.tier, ScopeTier::Daemon);
        assert_eq!(node.parent_id, Some(ScopeId::root()));
    }

    #[test]
    fn register_duplicate_rejected() {
        let mut tree = ScopeTree::new(1000);
        tree.register(
            well_known::discovery(),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "first",
            1000,
        )
        .unwrap();
        let err = tree
            .register(
                well_known::discovery(),
                ScopeTier::Daemon,
                &ScopeId::root(),
                "second",
                2000,
            )
            .unwrap_err();
        assert!(matches!(err, ScopeTreeError::DuplicateScope { .. }));
    }

    #[test]
    fn register_under_missing_parent_rejected() {
        let mut tree = ScopeTree::new(1000);
        let err = tree
            .register(
                ScopeId("child".into()),
                ScopeTier::Worker,
                &ScopeId("nonexistent".into()),
                "orphan",
                1000,
            )
            .unwrap_err();
        assert!(matches!(err, ScopeTreeError::ParentNotFound { .. }));
    }

    #[test]
    fn lifecycle_transitions() {
        let mut tree = ScopeTree::new(1000);
        tree.register(
            well_known::discovery(),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "discovery",
            1000,
        )
        .unwrap();

        // Created → Running
        tree.start(&well_known::discovery(), 2000).unwrap();
        assert_eq!(
            tree.get(&well_known::discovery()).unwrap().state,
            ScopeState::Running
        );

        // Running → Draining
        tree.request_shutdown(&well_known::discovery(), 3000)
            .unwrap();
        assert_eq!(
            tree.get(&well_known::discovery()).unwrap().state,
            ScopeState::Draining
        );

        // Draining → Finalizing (no children)
        tree.finalize(&well_known::discovery()).unwrap();
        assert_eq!(
            tree.get(&well_known::discovery()).unwrap().state,
            ScopeState::Finalizing
        );

        // Finalizing → Closed
        tree.close(&well_known::discovery(), 4000).unwrap();
        assert_eq!(
            tree.get(&well_known::discovery()).unwrap().state,
            ScopeState::Closed
        );
    }

    #[test]
    fn cannot_finalize_with_live_children() {
        let mut tree = ScopeTree::new(1000);
        tree.register(
            well_known::capture(),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "capture",
            1000,
        )
        .unwrap();
        tree.start(&well_known::capture(), 1100).unwrap();

        // Add a worker child
        tree.register(
            well_known::capture_worker(0),
            ScopeTier::Worker,
            &well_known::capture(),
            "worker-0",
            1200,
        )
        .unwrap();
        tree.start(&well_known::capture_worker(0), 1300).unwrap();

        // Request shutdown on parent
        tree.request_shutdown(&well_known::capture(), 2000).unwrap();

        // Cannot finalize parent while child is running
        let err = tree.finalize(&well_known::capture()).unwrap_err();
        assert!(matches!(
            err,
            ScopeTreeError::HasLiveChildren { live_count: 1, .. }
        ));

        // Shut down child first
        tree.request_shutdown(&well_known::capture_worker(0), 2100)
            .unwrap();
        tree.finalize(&well_known::capture_worker(0)).unwrap();
        tree.close(&well_known::capture_worker(0), 2200).unwrap();

        // Now parent can finalize
        tree.finalize(&well_known::capture()).unwrap();
        tree.close(&well_known::capture(), 2300).unwrap();
    }

    #[test]
    fn register_under_closed_parent_rejected() {
        let mut tree = ScopeTree::new(1000);
        tree.register(
            well_known::capture(),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "capture",
            1000,
        )
        .unwrap();

        // Close the parent
        tree.start(&well_known::capture(), 1100).unwrap();
        tree.request_shutdown(&well_known::capture(), 1200).unwrap();
        tree.finalize(&well_known::capture()).unwrap();
        tree.close(&well_known::capture(), 1300).unwrap();

        // Try to register child under closed parent
        let err = tree
            .register(
                well_known::capture_worker(0),
                ScopeTier::Worker,
                &well_known::capture(),
                "worker",
                1400,
            )
            .unwrap_err();
        assert!(matches!(err, ScopeTreeError::ParentNotAccepting { .. }));
    }

    #[test]
    fn worker_cannot_have_children() {
        let mut tree = ScopeTree::new(1000);
        tree.register(
            well_known::capture(),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "capture",
            1000,
        )
        .unwrap();
        tree.register(
            well_known::capture_worker(0),
            ScopeTier::Worker,
            &well_known::capture(),
            "worker",
            1000,
        )
        .unwrap();

        // Worker cannot have children
        let err = tree
            .register(
                ScopeId("sub-worker".into()),
                ScopeTier::Ephemeral,
                &well_known::capture_worker(0),
                "sub",
                1000,
            )
            .unwrap_err();
        assert!(matches!(err, ScopeTreeError::TierCannotHaveChildren { .. }));
    }

    #[test]
    fn shutdown_order_deepest_first() {
        let mut tree = ScopeTree::new(1000);
        register_standard_scopes(&mut tree, 1000).unwrap();

        // Start everything
        tree.start(&ScopeId::root(), 1100).unwrap();
        for id in tree.root().children.clone() {
            tree.start(&id, 1200).unwrap();
        }

        // Add workers under capture
        tree.register(
            well_known::capture_worker(0),
            ScopeTier::Worker,
            &well_known::capture(),
            "w0",
            1300,
        )
        .unwrap();
        tree.start(&well_known::capture_worker(0), 1400).unwrap();

        let order = tree.shutdown_order();

        // Workers should be first (deepest, highest priority)
        let worker_idx = order
            .iter()
            .position(|id| *id == well_known::capture_worker(0))
            .unwrap();
        let capture_idx = order
            .iter()
            .position(|id| *id == well_known::capture())
            .unwrap();
        let root_idx = order.iter().position(|id| id.is_root()).unwrap();

        assert!(worker_idx < capture_idx, "workers before daemons");
        assert!(capture_idx < root_idx, "daemons before root");
    }

    #[test]
    fn depth_computation() {
        let mut tree = ScopeTree::new(1000);
        register_standard_scopes(&mut tree, 1000).unwrap();
        tree.register(
            well_known::capture_worker(0),
            ScopeTier::Worker,
            &well_known::capture(),
            "w0",
            1000,
        )
        .unwrap();

        assert_eq!(tree.depth(&ScopeId::root()), 0);
        assert_eq!(tree.depth(&well_known::capture()), 1);
        assert_eq!(tree.depth(&well_known::capture_worker(0)), 2);
    }

    #[test]
    fn descendants_breadth_first() {
        let mut tree = ScopeTree::new(1000);
        tree.register(
            well_known::capture(),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "capture",
            1000,
        )
        .unwrap();
        tree.register(
            well_known::capture_worker(0),
            ScopeTier::Worker,
            &well_known::capture(),
            "w0",
            1000,
        )
        .unwrap();
        tree.register(
            well_known::capture_worker(1),
            ScopeTier::Worker,
            &well_known::capture(),
            "w1",
            1000,
        )
        .unwrap();

        let desc = tree.descendants(&ScopeId::root());
        assert_eq!(desc.len(), 3); // capture + 2 workers
        assert_eq!(desc[0], well_known::capture());
    }

    #[test]
    fn snapshot_counts() {
        let mut tree = ScopeTree::new(1000);
        register_standard_scopes(&mut tree, 1000).unwrap();

        let snap = tree.snapshot();
        assert_eq!(snap.total_scopes, 9); // 1 root + 5 daemons + 3 watchers
        assert_eq!(snap.daemons, 5);
        assert_eq!(snap.watchers, 3);
        assert_eq!(snap.workers, 0);
        assert_eq!(snap.ephemeral, 0);
    }

    #[test]
    fn scope_handle_shutdown_flag() {
        let handle = ScopeHandle::new(well_known::discovery());
        assert!(!handle.is_shutdown_requested());
        assert_eq!(handle.current_generation(), 0);

        handle.request_shutdown();
        assert!(handle.is_shutdown_requested());
        assert_eq!(handle.current_generation(), 1);
    }

    #[test]
    fn canonical_string_deterministic() {
        let tree = ScopeTree::new(1000);
        let s1 = tree.canonical_string();
        let s2 = tree.canonical_string();
        assert_eq!(s1, s2);
    }

    #[test]
    fn scope_node_canonical_string_deterministic() {
        let node = ScopeNode::new(
            well_known::capture(),
            ScopeTier::Daemon,
            Some(ScopeId::root()),
            "test",
            1000,
        );
        let s1 = node.canonical_string();
        let s2 = node.canonical_string();
        assert_eq!(s1, s2);
    }

    #[test]
    fn serde_roundtrip_scope_tree() {
        let mut tree = ScopeTree::new(1000);
        register_standard_scopes(&mut tree, 1000).unwrap();
        tree.start(&ScopeId::root(), 1100).unwrap();

        let json = serde_json::to_string(&tree).unwrap();
        let restored: ScopeTree = serde_json::from_str(&json).unwrap();

        assert_eq!(tree.len(), restored.len());
        assert_eq!(tree.canonical_string(), restored.canonical_string());
    }

    #[test]
    fn serde_roundtrip_snapshot() {
        let mut tree = ScopeTree::new(1000);
        register_standard_scopes(&mut tree, 1000).unwrap();

        let snap = tree.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let restored: ScopeTreeSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, restored);
    }

    #[test]
    fn register_standard_scopes_all_present() {
        let mut tree = ScopeTree::new(1000);
        register_standard_scopes(&mut tree, 1000).unwrap();

        // All well-known IDs exist
        assert!(tree.get(&well_known::discovery()).is_some());
        assert!(tree.get(&well_known::capture()).is_some());
        assert!(tree.get(&well_known::relay()).is_some());
        assert!(tree.get(&well_known::persistence()).is_some());
        assert!(tree.get(&well_known::maintenance()).is_some());
        assert!(tree.get(&well_known::native_events()).is_some());
        assert!(tree.get(&well_known::snapshot()).is_some());
        assert!(tree.get(&well_known::config_reload()).is_some());
    }

    #[test]
    fn invalid_transition_rejects() {
        let mut tree = ScopeTree::new(1000);
        tree.register(
            well_known::discovery(),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "discovery",
            1000,
        )
        .unwrap();

        // Cannot start → draining directly
        let err = tree.request_shutdown(&well_known::discovery(), 2000);
        // Actually this should work — Created → Draining is allowed
        assert!(err.is_ok());

        // Cannot finalize from Created
        let mut tree2 = ScopeTree::new(1000);
        tree2
            .register(
                well_known::relay(),
                ScopeTier::Daemon,
                &ScopeId::root(),
                "relay",
                1000,
            )
            .unwrap();
        let err = tree2.finalize(&well_known::relay());
        assert!(matches!(err, Err(ScopeTreeError::InvalidTransition { .. })));
    }

    #[test]
    fn ephemeral_scope_lifecycle() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let query_id = well_known::ephemeral_query("q1");
        tree.register(
            query_id.clone(),
            ScopeTier::Ephemeral,
            &ScopeId::root(),
            "query-1",
            2000,
        )
        .unwrap();
        tree.start(&query_id, 2001).unwrap();

        assert_eq!(tree.count_by_tier(ScopeTier::Ephemeral), 1);

        tree.request_shutdown(&query_id, 2500).unwrap();
        tree.finalize(&query_id).unwrap();
        tree.close(&query_id, 3000).unwrap();

        assert_eq!(tree.count_by_state(ScopeState::Closed), 1);
    }
}
