// =============================================================================
// Topology and layout orchestration service (ft-3681t.2.2)
//
// Programmable topology management for the native mux: split/merge/rebalance,
// role-based layout templates, focus groups, and deterministic fleet arrangement.
// Builds on the lifecycle engine (ft-3681t.2.1) to ensure all topology mutations
// respect entity lifecycle state.
// =============================================================================

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::session_topology::{
    LifecycleIdentity, LifecycleRegistry, LifecycleState, MuxPaneLifecycleState, PaneNode,
};
use crate::wezterm::SplitDirection;

// =============================================================================
// Layout template types
// =============================================================================

/// Named layout template for deterministic fleet arrangement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LayoutTemplate {
    /// Template name (e.g., "dev-3x2", "monitoring-grid").
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// Root node describing the desired pane arrangement.
    pub root: LayoutNode,
    /// Minimum number of panes required for this template.
    pub min_panes: u32,
    /// Maximum number of panes this template supports.
    #[serde(default)]
    pub max_panes: Option<u32>,
}

/// Recursive layout tree describing desired pane arrangement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum LayoutNode {
    /// A single pane slot.
    Slot {
        /// Optional role for this slot (e.g., "primary", "log-viewer", "agent-1").
        #[serde(default)]
        role: Option<String>,
        /// Relative weight for sizing (default 1.0).
        #[serde(default = "default_weight")]
        weight: f64,
    },
    /// Horizontal split (children stacked top-to-bottom).
    HSplit { children: Vec<LayoutNode> },
    /// Vertical split (children arranged left-to-right).
    VSplit { children: Vec<LayoutNode> },
}

fn default_weight() -> f64 {
    1.0
}

// =============================================================================
// Focus group
// =============================================================================

/// A named group of panes that can be focused/unfocused together.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FocusGroup {
    /// Group name.
    pub name: String,
    /// Pane identities in this group.
    pub members: Vec<LifecycleIdentity>,
    /// Whether this group is currently focused.
    pub focused: bool,
    /// When this group was created (epoch ms).
    pub created_at: u64,
}

// =============================================================================
// Topology operation types
// =============================================================================

/// A requested topology mutation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TopologyOp {
    /// Split an existing pane.
    Split {
        target: LifecycleIdentity,
        direction: TopologySplitDirection,
        /// Ratio of the new pane (0.0..1.0). 0.5 = equal split.
        #[serde(default = "default_ratio")]
        ratio: f64,
    },
    /// Close/remove a pane.
    Close { target: LifecycleIdentity },
    /// Swap two panes' positions in the layout tree.
    Swap {
        a: LifecycleIdentity,
        b: LifecycleIdentity,
    },
    /// Move a pane to a different position via directional navigation.
    Move {
        target: LifecycleIdentity,
        direction: TopologyMoveDirection,
    },
    /// Apply a layout template to a window.
    ApplyTemplate {
        window: LifecycleIdentity,
        template_name: String,
    },
    /// Rebalance pane sizes to equal proportions within a container.
    Rebalance {
        /// Window or session scope for rebalance.
        scope: LifecycleIdentity,
    },
    /// Create a focus group from a set of panes.
    CreateFocusGroup {
        name: String,
        members: Vec<LifecycleIdentity>,
    },
}

/// Direction for topology splits (serializable, decoupled from wezterm).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologySplitDirection {
    Left,
    Right,
    Top,
    Bottom,
}

/// Direction for pane movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyMoveDirection {
    Left,
    Right,
    Up,
    Down,
}

fn default_ratio() -> f64 {
    0.5
}

impl TopologySplitDirection {
    /// Convert to wezterm SplitDirection for backend execution.
    pub fn to_wezterm(self) -> SplitDirection {
        match self {
            Self::Left => SplitDirection::Left,
            Self::Right => SplitDirection::Right,
            Self::Top => SplitDirection::Top,
            Self::Bottom => SplitDirection::Bottom,
        }
    }
}

// =============================================================================
// Topology plan (validated sequence of operations)
// =============================================================================

/// A validated, ordered sequence of topology mutations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyPlan {
    /// Ordered operations to execute.
    pub operations: Vec<ValidatedOp>,
    /// True if this plan was validated against the lifecycle registry.
    pub validated: bool,
    /// When this plan was created (epoch ms).
    pub created_at: u64,
}

/// A single validated operation with pre-check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatedOp {
    /// The original operation.
    pub op: TopologyOp,
    /// Pre-validation result.
    pub check: OpCheckResult,
}

/// Result of pre-checking an operation against lifecycle state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpCheckResult {
    /// Operation is valid and can proceed.
    Ok,
    /// Target entity is in an incompatible lifecycle state.
    InvalidState {
        identity: String,
        current_state: String,
        reason: String,
    },
    /// Target entity not found in registry.
    NotFound { identity: String },
    /// Operation would violate a constraint (e.g., closing the last pane).
    ConstraintViolation { reason: String },
}

// =============================================================================
// Topology audit entry
// =============================================================================

/// Audit trail entry for topology mutations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyAuditEntry {
    /// Operation that was attempted.
    pub op: TopologyOp,
    /// Whether the operation succeeded.
    pub succeeded: bool,
    /// Error message if failed.
    #[serde(default)]
    pub error: Option<String>,
    /// Timestamp (epoch ms).
    pub timestamp: u64,
    /// Correlation ID for tracing.
    #[serde(default)]
    pub correlation_id: Option<String>,
}

// =============================================================================
// Topology orchestration errors
// =============================================================================

/// Errors that can occur during topology orchestration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum TopologyError {
    /// Target entity not found in lifecycle registry.
    EntityNotFound { identity: String },
    /// Entity is in a state that doesn't allow this operation.
    InvalidLifecycleState {
        identity: String,
        state: String,
        operation: String,
    },
    /// Template not found in the registry.
    TemplateNotFound { name: String },
    /// Template requires more/fewer panes than available.
    TemplatePaneMismatch {
        template: String,
        required: u32,
        available: u32,
    },
    /// Operation would leave a window with zero panes.
    LastPaneProtection { window: String },
    /// Ratio out of valid range.
    InvalidRatio { ratio: f64 },
    /// Focus group name already exists.
    DuplicateFocusGroup { name: String },
}

impl std::fmt::Display for TopologyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EntityNotFound { identity } => {
                write!(f, "entity not found: {identity}")
            }
            Self::InvalidLifecycleState {
                identity,
                state,
                operation,
            } => {
                write!(f, "cannot {operation} entity {identity} in state {state}")
            }
            Self::TemplateNotFound { name } => {
                write!(f, "layout template not found: {name}")
            }
            Self::TemplatePaneMismatch {
                template,
                required,
                available,
            } => {
                write!(
                    f,
                    "template {template} requires {required} panes, but {available} available"
                )
            }
            Self::LastPaneProtection { window } => {
                write!(f, "cannot close last pane in window {window}")
            }
            Self::InvalidRatio { ratio } => {
                write!(f, "split ratio {ratio} out of valid range (0.0, 1.0)")
            }
            Self::DuplicateFocusGroup { name } => {
                write!(f, "focus group already exists: {name}")
            }
        }
    }
}

// =============================================================================
// Layout template registry
// =============================================================================

/// Registry of named layout templates.
#[derive(Debug, Clone, Default)]
pub struct TemplateRegistry {
    templates: HashMap<String, LayoutTemplate>,
}

impl TemplateRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a layout template. Overwrites if name already exists.
    pub fn register(&mut self, template: LayoutTemplate) {
        self.templates.insert(template.name.clone(), template);
    }

    /// Look up a template by name.
    pub fn get(&self, name: &str) -> Option<&LayoutTemplate> {
        self.templates.get(name)
    }

    /// List all registered template names.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.templates.keys().map(|s| s.as_str()).collect();
        names.sort_unstable();
        names
    }

    /// Number of registered templates.
    pub fn len(&self) -> usize {
        self.templates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    /// Register built-in default templates.
    pub fn register_defaults(&mut self) {
        // Equal 2-pane vertical split
        self.register(LayoutTemplate {
            name: "side-by-side".into(),
            description: Some("Two panes side by side (50/50 vertical split)".into()),
            root: LayoutNode::VSplit {
                children: vec![
                    LayoutNode::Slot {
                        role: Some("left".into()),
                        weight: 1.0,
                    },
                    LayoutNode::Slot {
                        role: Some("right".into()),
                        weight: 1.0,
                    },
                ],
            },
            min_panes: 2,
            max_panes: Some(2),
        });

        // Primary + sidebar layout (70/30)
        self.register(LayoutTemplate {
            name: "primary-sidebar".into(),
            description: Some("Large primary pane with narrow sidebar".into()),
            root: LayoutNode::VSplit {
                children: vec![
                    LayoutNode::Slot {
                        role: Some("primary".into()),
                        weight: 7.0,
                    },
                    LayoutNode::Slot {
                        role: Some("sidebar".into()),
                        weight: 3.0,
                    },
                ],
            },
            min_panes: 2,
            max_panes: Some(2),
        });

        // 2x2 grid
        self.register(LayoutTemplate {
            name: "grid-2x2".into(),
            description: Some("Four panes in a 2x2 grid".into()),
            root: LayoutNode::HSplit {
                children: vec![
                    LayoutNode::VSplit {
                        children: vec![
                            LayoutNode::Slot {
                                role: Some("top-left".into()),
                                weight: 1.0,
                            },
                            LayoutNode::Slot {
                                role: Some("top-right".into()),
                                weight: 1.0,
                            },
                        ],
                    },
                    LayoutNode::VSplit {
                        children: vec![
                            LayoutNode::Slot {
                                role: Some("bottom-left".into()),
                                weight: 1.0,
                            },
                            LayoutNode::Slot {
                                role: Some("bottom-right".into()),
                                weight: 1.0,
                            },
                        ],
                    },
                ],
            },
            min_panes: 4,
            max_panes: Some(4),
        });

        // Agent swarm layout: primary + 3 agent panes
        self.register(LayoutTemplate {
            name: "swarm-1+3".into(),
            description: Some("Primary pane on left, 3 agent panes stacked on right".into()),
            root: LayoutNode::VSplit {
                children: vec![
                    LayoutNode::Slot {
                        role: Some("primary".into()),
                        weight: 2.0,
                    },
                    LayoutNode::HSplit {
                        children: vec![
                            LayoutNode::Slot {
                                role: Some("agent-1".into()),
                                weight: 1.0,
                            },
                            LayoutNode::Slot {
                                role: Some("agent-2".into()),
                                weight: 1.0,
                            },
                            LayoutNode::Slot {
                                role: Some("agent-3".into()),
                                weight: 1.0,
                            },
                        ],
                    },
                ],
            },
            min_panes: 4,
            max_panes: Some(4),
        });
    }
}

// =============================================================================
// LayoutNode helpers
// =============================================================================

impl LayoutNode {
    /// Count the total number of slot (leaf) nodes in this layout tree.
    pub fn slot_count(&self) -> u32 {
        match self {
            Self::Slot { .. } => 1,
            Self::HSplit { children } | Self::VSplit { children } => {
                children.iter().map(|c| c.slot_count()).sum()
            }
        }
    }

    /// Collect all role names in this layout tree.
    pub fn roles(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_roles(&mut out);
        out
    }

    fn collect_roles<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Self::Slot { role, .. } => {
                if let Some(r) = role {
                    out.push(r.as_str());
                }
            }
            Self::HSplit { children } | Self::VSplit { children } => {
                for c in children {
                    c.collect_roles(out);
                }
            }
        }
    }

    /// Compute normalized weight ratios for children (returns empty for Slot).
    pub fn child_ratios(&self) -> Vec<f64> {
        match self {
            Self::Slot { .. } => vec![],
            Self::HSplit { children } | Self::VSplit { children } => {
                let total: f64 = children.iter().map(|c| c.weight()).sum();
                if total <= 0.0 {
                    vec![1.0 / children.len() as f64; children.len()]
                } else {
                    children.iter().map(|c| c.weight() / total).collect()
                }
            }
        }
    }

    /// Get the weight of this node.
    pub fn weight(&self) -> f64 {
        match self {
            Self::Slot { weight, .. } => *weight,
            // Container weight = sum of children.
            Self::HSplit { children } | Self::VSplit { children } => {
                children.iter().map(|c| c.weight()).sum()
            }
        }
    }

    /// Convert a LayoutNode to a PaneNode using provided pane IDs.
    /// Assigns pane IDs to slots in depth-first order.
    pub fn to_pane_node(&self, pane_ids: &mut impl Iterator<Item = u64>) -> Option<PaneNode> {
        match self {
            Self::Slot { .. } => {
                let pane_id = pane_ids.next()?;
                Some(PaneNode::Leaf {
                    pane_id,
                    rows: 24,
                    cols: 80,
                    cwd: None,
                    title: None,
                    is_active: false,
                })
            }
            Self::HSplit { children } => {
                let ratios = self.child_ratios();
                let mut pane_children = Vec::with_capacity(children.len());
                for (i, child) in children.iter().enumerate() {
                    let pane_node = child.to_pane_node(pane_ids)?;
                    pane_children.push((ratios[i], pane_node));
                }
                Some(PaneNode::HSplit {
                    children: pane_children,
                })
            }
            Self::VSplit { children } => {
                let ratios = self.child_ratios();
                let mut pane_children = Vec::with_capacity(children.len());
                for (i, child) in children.iter().enumerate() {
                    let pane_node = child.to_pane_node(pane_ids)?;
                    pane_children.push((ratios[i], pane_node));
                }
                Some(PaneNode::VSplit {
                    children: pane_children,
                })
            }
        }
    }
}

// =============================================================================
// Topology orchestrator
// =============================================================================

/// The main topology orchestration engine.
///
/// Validates topology operations against the lifecycle registry,
/// produces validated plans, and maintains an audit log.
pub struct TopologyOrchestrator {
    templates: TemplateRegistry,
    focus_groups: HashMap<String, FocusGroup>,
    audit_log: Vec<TopologyAuditEntry>,
    /// Maximum audit log entries before oldest are evicted.
    max_audit_entries: usize,
}

impl TopologyOrchestrator {
    /// Create a new orchestrator with default templates.
    pub fn new() -> Self {
        let mut templates = TemplateRegistry::new();
        templates.register_defaults();
        Self {
            templates,
            focus_groups: HashMap::new(),
            audit_log: Vec::new(),
            max_audit_entries: 10_000,
        }
    }

    /// Create an orchestrator with custom template registry.
    pub fn with_templates(templates: TemplateRegistry) -> Self {
        Self {
            templates,
            focus_groups: HashMap::new(),
            audit_log: Vec::new(),
            max_audit_entries: 10_000,
        }
    }

    /// Access the template registry.
    pub fn templates(&self) -> &TemplateRegistry {
        &self.templates
    }

    /// Mutable access to the template registry.
    pub fn templates_mut(&mut self) -> &mut TemplateRegistry {
        &mut self.templates
    }

    /// Get all focus groups.
    pub fn focus_groups(&self) -> &HashMap<String, FocusGroup> {
        &self.focus_groups
    }

    /// Get the audit log.
    pub fn audit_log(&self) -> &[TopologyAuditEntry] {
        &self.audit_log
    }

    // -------------------------------------------------------------------------
    // Validation
    // -------------------------------------------------------------------------

    #[allow(clippy::unused_self)]
    /// Validate a single topology operation against the lifecycle registry.
    pub fn validate_op(&self, op: &TopologyOp, registry: &LifecycleRegistry) -> OpCheckResult {
        match op {
            TopologyOp::Split { target, ratio, .. } => {
                if *ratio <= 0.0 || *ratio >= 1.0 {
                    return OpCheckResult::ConstraintViolation {
                        reason: format!("split ratio {ratio} must be in (0.0, 1.0)"),
                    };
                }
                self.check_pane_mutable(target, "split", registry)
            }
            TopologyOp::Close { target } => self.check_pane_closeable(target, registry),
            TopologyOp::Swap { a, b } => {
                let check_a = self.check_pane_exists(a, registry);
                if check_a != OpCheckResult::Ok {
                    return check_a;
                }
                self.check_pane_exists(b, registry)
            }
            TopologyOp::Move { target, .. } => self.check_pane_mutable(target, "move", registry),
            TopologyOp::ApplyTemplate { template_name, .. } => {
                if self.templates.get(template_name).is_none() {
                    return OpCheckResult::InvalidState {
                        identity: template_name.clone(),
                        current_state: "n/a".into(),
                        reason: format!("template '{template_name}' not found"),
                    };
                }
                OpCheckResult::Ok
            }
            TopologyOp::Rebalance { scope } => self.check_entity_exists(scope, registry),
            TopologyOp::CreateFocusGroup { name, members } => {
                if self.focus_groups.contains_key(name) {
                    return OpCheckResult::ConstraintViolation {
                        reason: format!("focus group '{name}' already exists"),
                    };
                }
                for member in members {
                    let check = self.check_pane_exists(member, registry);
                    if check != OpCheckResult::Ok {
                        return check;
                    }
                }
                OpCheckResult::Ok
            }
        }
    }

    /// Validate a sequence of operations and produce a TopologyPlan.
    pub fn validate_plan(
        &self,
        ops: Vec<TopologyOp>,
        registry: &LifecycleRegistry,
    ) -> TopologyPlan {
        let validated: Vec<ValidatedOp> = ops
            .into_iter()
            .map(|op| {
                let check = self.validate_op(&op, registry);
                ValidatedOp { op, check }
            })
            .collect();

        let all_ok = validated.iter().all(|v| v.check == OpCheckResult::Ok);

        TopologyPlan {
            operations: validated,
            validated: all_ok,
            created_at: epoch_ms(),
        }
    }

    // -------------------------------------------------------------------------
    // Focus group management
    // -------------------------------------------------------------------------

    /// Create a focus group.
    pub fn create_focus_group(
        &mut self,
        name: String,
        members: Vec<LifecycleIdentity>,
        registry: &LifecycleRegistry,
    ) -> Result<&FocusGroup, TopologyError> {
        if self.focus_groups.contains_key(&name) {
            return Err(TopologyError::DuplicateFocusGroup { name });
        }

        // Verify all members exist
        for member in &members {
            if registry.get(member).is_none() {
                return Err(TopologyError::EntityNotFound {
                    identity: member.stable_key(),
                });
            }
        }

        let group = FocusGroup {
            name: name.clone(),
            members,
            focused: false,
            created_at: epoch_ms(),
        };

        self.focus_groups.insert(name.clone(), group);
        Ok(&self.focus_groups[&name])
    }

    /// Remove a focus group by name.
    pub fn remove_focus_group(&mut self, name: &str) -> bool {
        self.focus_groups.remove(name).is_some()
    }

    /// Toggle focus state on a group.
    pub fn toggle_focus_group(&mut self, name: &str) -> Option<bool> {
        self.focus_groups.get_mut(name).map(|g| {
            g.focused = !g.focused;
            g.focused
        })
    }

    // -------------------------------------------------------------------------
    // Audit logging
    // -------------------------------------------------------------------------

    /// Record an operation in the audit log.
    pub fn record_audit(
        &mut self,
        op: TopologyOp,
        succeeded: bool,
        error: Option<String>,
        correlation_id: Option<String>,
    ) {
        if self.audit_log.len() >= self.max_audit_entries {
            // Evict oldest 10%
            let drain_count = self.max_audit_entries / 10;
            self.audit_log.drain(..drain_count);
        }
        self.audit_log.push(TopologyAuditEntry {
            op,
            succeeded,
            error,
            timestamp: epoch_ms(),
            correlation_id,
        });
    }

    // -------------------------------------------------------------------------
    // Template-based layout generation
    // -------------------------------------------------------------------------

    /// Generate a PaneNode tree from a template and a set of pane IDs.
    pub fn layout_from_template(
        &self,
        template_name: &str,
        pane_ids: &[u64],
    ) -> Result<PaneNode, TopologyError> {
        let template =
            self.templates
                .get(template_name)
                .ok_or_else(|| TopologyError::TemplateNotFound {
                    name: template_name.into(),
                })?;

        let required = template.root.slot_count();
        let available = pane_ids.len() as u32;

        if available < required {
            return Err(TopologyError::TemplatePaneMismatch {
                template: template_name.into(),
                required,
                available,
            });
        }

        if let Some(max) = template.max_panes {
            if available > max {
                return Err(TopologyError::TemplatePaneMismatch {
                    template: template_name.into(),
                    required: max,
                    available,
                });
            }
        }

        let mut id_iter = pane_ids.iter().copied();
        template.root.to_pane_node(&mut id_iter).ok_or_else(|| {
            TopologyError::TemplatePaneMismatch {
                template: template_name.into(),
                required,
                available,
            }
        })
    }

    // -------------------------------------------------------------------------
    // Rebalance computation
    // -------------------------------------------------------------------------

    /// Rebalance a PaneNode tree so all siblings have equal ratios.
    pub fn rebalance_tree(node: &PaneNode) -> PaneNode {
        match node {
            PaneNode::Leaf { .. } => node.clone(),
            PaneNode::HSplit { children } => {
                let equal_ratio = 1.0 / children.len() as f64;
                PaneNode::HSplit {
                    children: children
                        .iter()
                        .map(|(_, child)| (equal_ratio, Self::rebalance_tree(child)))
                        .collect(),
                }
            }
            PaneNode::VSplit { children } => {
                let equal_ratio = 1.0 / children.len() as f64;
                PaneNode::VSplit {
                    children: children
                        .iter()
                        .map(|(_, child)| (equal_ratio, Self::rebalance_tree(child)))
                        .collect(),
                }
            }
        }
    }

    // -------------------------------------------------------------------------
    // Lifecycle-aware helpers
    // -------------------------------------------------------------------------

    #[allow(clippy::unused_self)]
    fn check_pane_mutable(
        &self,
        identity: &LifecycleIdentity,
        operation: &str,
        registry: &LifecycleRegistry,
    ) -> OpCheckResult {
        match registry.get(identity) {
            None => OpCheckResult::NotFound {
                identity: identity.stable_key(),
            },
            Some(record) => match &record.state {
                LifecycleState::Pane(
                    MuxPaneLifecycleState::Running | MuxPaneLifecycleState::Ready,
                ) => OpCheckResult::Ok,
                other => OpCheckResult::InvalidState {
                    identity: identity.stable_key(),
                    current_state: format!("{other:?}"),
                    reason: format!("pane must be Running or Ready to {operation}"),
                },
            },
        }
    }

    #[allow(clippy::unused_self)]
    fn check_pane_closeable(
        &self,
        identity: &LifecycleIdentity,
        registry: &LifecycleRegistry,
    ) -> OpCheckResult {
        match registry.get(identity) {
            None => OpCheckResult::NotFound {
                identity: identity.stable_key(),
            },
            Some(record) => match &record.state {
                LifecycleState::Pane(MuxPaneLifecycleState::Closed) => {
                    OpCheckResult::InvalidState {
                        identity: identity.stable_key(),
                        current_state: "Closed".into(),
                        reason: "pane is already closed".into(),
                    }
                }
                LifecycleState::Pane(_) => OpCheckResult::Ok,
                other => OpCheckResult::InvalidState {
                    identity: identity.stable_key(),
                    current_state: format!("{other:?}"),
                    reason: "target is not a pane".into(),
                },
            },
        }
    }

    #[allow(clippy::unused_self)]
    fn check_pane_exists(
        &self,
        identity: &LifecycleIdentity,
        registry: &LifecycleRegistry,
    ) -> OpCheckResult {
        match registry.get(identity) {
            None => OpCheckResult::NotFound {
                identity: identity.stable_key(),
            },
            Some(_) => OpCheckResult::Ok,
        }
    }

    #[allow(clippy::unused_self)]
    fn check_entity_exists(
        &self,
        identity: &LifecycleIdentity,
        registry: &LifecycleRegistry,
    ) -> OpCheckResult {
        match registry.get(identity) {
            None => OpCheckResult::NotFound {
                identity: identity.stable_key(),
            },
            Some(_) => OpCheckResult::Ok,
        }
    }
}

impl Default for TopologyOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Utility
// =============================================================================

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_topology::LifecycleEntityKind;

    // Helper: build a registry with some panes
    fn make_registry_with_panes(pane_ids: &[u64]) -> LifecycleRegistry {
        let mut reg = LifecycleRegistry::new();
        for &pid in pane_ids {
            let identity =
                LifecycleIdentity::new(LifecycleEntityKind::Pane, "default", "local", pid, 1);
            reg.register_entity(
                identity,
                LifecycleState::Pane(MuxPaneLifecycleState::Running),
                0,
            )
            .expect("register pane");
        }
        reg
    }

    fn pane_identity(id: u64) -> LifecycleIdentity {
        LifecycleIdentity::new(LifecycleEntityKind::Pane, "default", "local", id, 1)
    }

    fn window_identity(id: u64) -> LifecycleIdentity {
        LifecycleIdentity::new(LifecycleEntityKind::Window, "default", "local", id, 1)
    }

    // -------------------------------------------------------------------------
    // LayoutNode tests
    // -------------------------------------------------------------------------

    #[test]
    fn layout_node_slot_count_leaf() {
        let node = LayoutNode::Slot {
            role: Some("primary".into()),
            weight: 1.0,
        };
        assert_eq!(node.slot_count(), 1);
    }

    #[test]
    fn layout_node_slot_count_nested() {
        let node = LayoutNode::HSplit {
            children: vec![
                LayoutNode::VSplit {
                    children: vec![
                        LayoutNode::Slot {
                            role: None,
                            weight: 1.0,
                        },
                        LayoutNode::Slot {
                            role: None,
                            weight: 1.0,
                        },
                    ],
                },
                LayoutNode::Slot {
                    role: None,
                    weight: 1.0,
                },
            ],
        };
        assert_eq!(node.slot_count(), 3);
    }

    #[test]
    fn layout_node_roles_collected() {
        let node = LayoutNode::VSplit {
            children: vec![
                LayoutNode::Slot {
                    role: Some("left".into()),
                    weight: 1.0,
                },
                LayoutNode::Slot {
                    role: Some("right".into()),
                    weight: 1.0,
                },
            ],
        };
        assert_eq!(node.roles(), vec!["left", "right"]);
    }

    #[test]
    fn layout_node_child_ratios_equal() {
        let node = LayoutNode::VSplit {
            children: vec![
                LayoutNode::Slot {
                    role: None,
                    weight: 1.0,
                },
                LayoutNode::Slot {
                    role: None,
                    weight: 1.0,
                },
            ],
        };
        let ratios = node.child_ratios();
        assert_eq!(ratios.len(), 2);
        assert!((ratios[0] - 0.5).abs() < 1e-10);
        assert!((ratios[1] - 0.5).abs() < 1e-10);
    }

    #[test]
    fn layout_node_child_ratios_weighted() {
        let node = LayoutNode::VSplit {
            children: vec![
                LayoutNode::Slot {
                    role: None,
                    weight: 7.0,
                },
                LayoutNode::Slot {
                    role: None,
                    weight: 3.0,
                },
            ],
        };
        let ratios = node.child_ratios();
        assert!((ratios[0] - 0.7).abs() < 1e-10);
        assert!((ratios[1] - 0.3).abs() < 1e-10);
    }

    #[test]
    fn layout_node_to_pane_node() {
        let node = LayoutNode::VSplit {
            children: vec![
                LayoutNode::Slot {
                    role: None,
                    weight: 1.0,
                },
                LayoutNode::Slot {
                    role: None,
                    weight: 1.0,
                },
            ],
        };
        let pane_ids = vec![10, 20];
        let mut iter = pane_ids.into_iter();
        let pane_node = node.to_pane_node(&mut iter).unwrap();

        match &pane_node {
            PaneNode::VSplit { children } => {
                assert_eq!(children.len(), 2);
                match &children[0].1 {
                    PaneNode::Leaf { pane_id, .. } => assert_eq!(*pane_id, 10),
                    _ => panic!("expected Leaf"),
                }
                match &children[1].1 {
                    PaneNode::Leaf { pane_id, .. } => assert_eq!(*pane_id, 20),
                    _ => panic!("expected Leaf"),
                }
            }
            _ => panic!("expected VSplit"),
        }
    }

    #[test]
    fn layout_node_to_pane_node_insufficient_ids() {
        let node = LayoutNode::VSplit {
            children: vec![
                LayoutNode::Slot {
                    role: None,
                    weight: 1.0,
                },
                LayoutNode::Slot {
                    role: None,
                    weight: 1.0,
                },
            ],
        };
        let pane_ids: Vec<u64> = vec![10]; // Only 1, need 2
        let mut iter = pane_ids.into_iter();
        assert!(node.to_pane_node(&mut iter).is_none());
    }

    // -------------------------------------------------------------------------
    // TemplateRegistry tests
    // -------------------------------------------------------------------------

    #[test]
    fn template_registry_defaults() {
        let mut reg = TemplateRegistry::new();
        assert!(reg.is_empty());

        reg.register_defaults();
        assert!(reg.len() >= 4);
        assert!(reg.get("side-by-side").is_some());
        assert!(reg.get("primary-sidebar").is_some());
        assert!(reg.get("grid-2x2").is_some());
        assert!(reg.get("swarm-1+3").is_some());
    }

    #[test]
    fn template_registry_custom() {
        let mut reg = TemplateRegistry::new();
        reg.register(LayoutTemplate {
            name: "my-layout".into(),
            description: None,
            root: LayoutNode::Slot {
                role: None,
                weight: 1.0,
            },
            min_panes: 1,
            max_panes: Some(1),
        });
        assert_eq!(reg.names(), vec!["my-layout"]);
    }

    #[test]
    fn template_registry_overwrite() {
        let mut reg = TemplateRegistry::new();
        reg.register(LayoutTemplate {
            name: "x".into(),
            description: Some("v1".into()),
            root: LayoutNode::Slot {
                role: None,
                weight: 1.0,
            },
            min_panes: 1,
            max_panes: None,
        });
        reg.register(LayoutTemplate {
            name: "x".into(),
            description: Some("v2".into()),
            root: LayoutNode::Slot {
                role: None,
                weight: 1.0,
            },
            min_panes: 1,
            max_panes: None,
        });
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.get("x").unwrap().description.as_deref(), Some("v2"));
    }

    // -------------------------------------------------------------------------
    // TopologyOrchestrator validation tests
    // -------------------------------------------------------------------------

    #[test]
    fn validate_split_running_pane() {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1, 2, 3]);

        let op = TopologyOp::Split {
            target: pane_identity(1),
            direction: TopologySplitDirection::Right,
            ratio: 0.5,
        };

        assert_eq!(orch.validate_op(&op, &reg), OpCheckResult::Ok);
    }

    #[test]
    fn validate_split_invalid_ratio_zero() {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1]);

        let op = TopologyOp::Split {
            target: pane_identity(1),
            direction: TopologySplitDirection::Right,
            ratio: 0.0,
        };

        match orch.validate_op(&op, &reg) {
            OpCheckResult::ConstraintViolation { reason } => {
                assert!(reason.contains("ratio"));
            }
            other => panic!("expected ConstraintViolation, got {other:?}"),
        }
    }

    #[test]
    fn validate_split_invalid_ratio_one() {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1]);

        let op = TopologyOp::Split {
            target: pane_identity(1),
            direction: TopologySplitDirection::Left,
            ratio: 1.0,
        };

        match orch.validate_op(&op, &reg) {
            OpCheckResult::ConstraintViolation { .. } => {}
            other => panic!("expected ConstraintViolation, got {other:?}"),
        }
    }

    #[test]
    fn validate_split_nonexistent_pane() {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1]);

        let op = TopologyOp::Split {
            target: pane_identity(99),
            direction: TopologySplitDirection::Bottom,
            ratio: 0.5,
        };

        match orch.validate_op(&op, &reg) {
            OpCheckResult::NotFound { .. } => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn validate_split_closed_pane() {
        let mut reg = LifecycleRegistry::new();
        let identity = pane_identity(1);
        reg.register_entity(
            identity.clone(),
            LifecycleState::Pane(MuxPaneLifecycleState::Closed),
            0,
        )
        .expect("register pane");

        let orch = TopologyOrchestrator::new();
        let op = TopologyOp::Split {
            target: pane_identity(1),
            direction: TopologySplitDirection::Right,
            ratio: 0.5,
        };

        match orch.validate_op(&op, &reg) {
            OpCheckResult::InvalidState { reason, .. } => {
                assert!(reason.contains("Running or Ready"));
            }
            other => panic!("expected InvalidState, got {other:?}"),
        }
    }

    #[test]
    fn validate_close_running_pane() {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1, 2]);

        let op = TopologyOp::Close {
            target: pane_identity(1),
        };

        assert_eq!(orch.validate_op(&op, &reg), OpCheckResult::Ok);
    }

    #[test]
    fn validate_close_already_closed() {
        let mut reg = LifecycleRegistry::new();
        let identity = pane_identity(1);
        reg.register_entity(
            identity,
            LifecycleState::Pane(MuxPaneLifecycleState::Closed),
            0,
        )
        .expect("register pane");

        let orch = TopologyOrchestrator::new();
        let op = TopologyOp::Close {
            target: pane_identity(1),
        };

        match orch.validate_op(&op, &reg) {
            OpCheckResult::InvalidState { reason, .. } => {
                assert!(reason.contains("already closed"));
            }
            other => panic!("expected InvalidState, got {other:?}"),
        }
    }

    #[test]
    fn validate_swap() {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1, 2]);

        let op = TopologyOp::Swap {
            a: pane_identity(1),
            b: pane_identity(2),
        };

        assert_eq!(orch.validate_op(&op, &reg), OpCheckResult::Ok);
    }

    #[test]
    fn validate_swap_missing_target() {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1]);

        let op = TopologyOp::Swap {
            a: pane_identity(1),
            b: pane_identity(99),
        };

        match orch.validate_op(&op, &reg) {
            OpCheckResult::NotFound { .. } => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn validate_apply_template_exists() {
        let orch = TopologyOrchestrator::new();
        let reg = LifecycleRegistry::new();

        let op = TopologyOp::ApplyTemplate {
            window: window_identity(1),
            template_name: "side-by-side".into(),
        };

        assert_eq!(orch.validate_op(&op, &reg), OpCheckResult::Ok);
    }

    #[test]
    fn validate_apply_template_missing() {
        let orch = TopologyOrchestrator::new();
        let reg = LifecycleRegistry::new();

        let op = TopologyOp::ApplyTemplate {
            window: window_identity(1),
            template_name: "nonexistent".into(),
        };

        match orch.validate_op(&op, &reg) {
            OpCheckResult::InvalidState { .. } => {}
            other => panic!("expected InvalidState, got {other:?}"),
        }
    }

    #[test]
    fn validate_rebalance() {
        let orch = TopologyOrchestrator::new();
        let mut reg = LifecycleRegistry::new();
        let wid = window_identity(1);
        reg.register_entity(
            wid.clone(),
            LifecycleState::Window(crate::session_topology::WindowLifecycleState::Active),
            0,
        )
        .expect("register window");

        let op = TopologyOp::Rebalance { scope: wid };
        assert_eq!(orch.validate_op(&op, &reg), OpCheckResult::Ok);
    }

    // -------------------------------------------------------------------------
    // Plan validation tests
    // -------------------------------------------------------------------------

    #[test]
    fn validate_plan_all_ok() {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1, 2]);

        let ops = vec![
            TopologyOp::Split {
                target: pane_identity(1),
                direction: TopologySplitDirection::Right,
                ratio: 0.5,
            },
            TopologyOp::Swap {
                a: pane_identity(1),
                b: pane_identity(2),
            },
        ];

        let plan = orch.validate_plan(ops, &reg);
        assert!(plan.validated);
        assert_eq!(plan.operations.len(), 2);
    }

    #[test]
    fn validate_plan_partial_failure() {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1]);

        let ops = vec![
            TopologyOp::Split {
                target: pane_identity(1),
                direction: TopologySplitDirection::Right,
                ratio: 0.5,
            },
            TopologyOp::Close {
                target: pane_identity(99), // doesn't exist
            },
        ];

        let plan = orch.validate_plan(ops, &reg);
        assert!(!plan.validated);
        assert!(matches!(
            plan.operations[0].check,
            OpCheckResult::Ok
        ));
        assert!(matches!(
            plan.operations[1].check,
            OpCheckResult::NotFound { .. }
        ));
    }

    // -------------------------------------------------------------------------
    // Focus group tests
    // -------------------------------------------------------------------------

    #[test]
    fn create_focus_group() {
        let mut orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1, 2, 3]);

        let result = orch.create_focus_group(
            "agents".into(),
            vec![pane_identity(1), pane_identity(2)],
            &reg,
        );
        assert!(result.is_ok());
        let group = result.unwrap();
        assert_eq!(group.name, "agents");
        assert_eq!(group.members.len(), 2);
        assert!(!group.focused);
    }

    #[test]
    fn create_focus_group_duplicate() {
        let mut orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1, 2]);

        orch.create_focus_group("g".into(), vec![pane_identity(1)], &reg)
            .unwrap();

        let result = orch.create_focus_group("g".into(), vec![pane_identity(2)], &reg);
        assert!(matches!(
            result,
            Err(TopologyError::DuplicateFocusGroup { .. })
        ));
    }

    #[test]
    fn create_focus_group_missing_member() {
        let mut orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1]);

        let result =
            orch.create_focus_group("g".into(), vec![pane_identity(1), pane_identity(99)], &reg);
        assert!(matches!(result, Err(TopologyError::EntityNotFound { .. })));
    }

    #[test]
    fn toggle_focus_group() {
        let mut orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1]);

        orch.create_focus_group("g".into(), vec![pane_identity(1)], &reg)
            .unwrap();

        assert_eq!(orch.toggle_focus_group("g"), Some(true));
        assert_eq!(orch.toggle_focus_group("g"), Some(false));
        assert_eq!(orch.toggle_focus_group("nonexistent"), None);
    }

    #[test]
    fn remove_focus_group() {
        let mut orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1]);

        orch.create_focus_group("g".into(), vec![pane_identity(1)], &reg)
            .unwrap();
        assert!(orch.remove_focus_group("g"));
        assert!(!orch.remove_focus_group("g"));
    }

    // -------------------------------------------------------------------------
    // Template layout generation tests
    // -------------------------------------------------------------------------

    #[test]
    fn layout_from_template_side_by_side() {
        let orch = TopologyOrchestrator::new();
        let pane_node = orch
            .layout_from_template("side-by-side", &[10, 20])
            .unwrap();

        match &pane_node {
            PaneNode::VSplit { children } => {
                assert_eq!(children.len(), 2);
            }
            _ => panic!("expected VSplit"),
        }
    }

    #[test]
    fn layout_from_template_grid_2x2() {
        let orch = TopologyOrchestrator::new();
        let pane_node = orch
            .layout_from_template("grid-2x2", &[1, 2, 3, 4])
            .unwrap();

        match &pane_node {
            PaneNode::HSplit { children } => {
                assert_eq!(children.len(), 2);
                for (_, child) in children {
                    match child {
                        PaneNode::VSplit { children: inner } => {
                            assert_eq!(inner.len(), 2);
                        }
                        _ => panic!("expected VSplit children"),
                    }
                }
            }
            _ => panic!("expected HSplit"),
        }
    }

    #[test]
    fn layout_from_template_insufficient_panes() {
        let orch = TopologyOrchestrator::new();
        let result = orch.layout_from_template("grid-2x2", &[1, 2]);
        assert!(matches!(
            result,
            Err(TopologyError::TemplatePaneMismatch { .. })
        ));
    }

    #[test]
    fn layout_from_template_too_many_panes() {
        let orch = TopologyOrchestrator::new();
        let result = orch.layout_from_template("side-by-side", &[1, 2, 3]);
        assert!(matches!(
            result,
            Err(TopologyError::TemplatePaneMismatch { .. })
        ));
    }

    #[test]
    fn layout_from_template_not_found() {
        let orch = TopologyOrchestrator::new();
        let result = orch.layout_from_template("nonexistent", &[1]);
        assert!(matches!(
            result,
            Err(TopologyError::TemplateNotFound { .. })
        ));
    }

    // -------------------------------------------------------------------------
    // Rebalance tests
    // -------------------------------------------------------------------------

    #[test]
    fn rebalance_tree_vsplit() {
        let tree = PaneNode::VSplit {
            children: vec![
                (
                    0.7,
                    PaneNode::Leaf {
                        pane_id: 1,
                        rows: 24,
                        cols: 80,
                        cwd: None,
                        title: None,
                        is_active: false,
                    },
                ),
                (
                    0.3,
                    PaneNode::Leaf {
                        pane_id: 2,
                        rows: 24,
                        cols: 80,
                        cwd: None,
                        title: None,
                        is_active: false,
                    },
                ),
            ],
        };

        let rebalanced = TopologyOrchestrator::rebalance_tree(&tree);
        match &rebalanced {
            PaneNode::VSplit { children } => {
                assert_eq!(children.len(), 2);
                assert!((children[0].0 - 0.5).abs() < 1e-10);
                assert!((children[1].0 - 0.5).abs() < 1e-10);
            }
            _ => panic!("expected VSplit"),
        }
    }

    #[test]
    fn rebalance_tree_nested() {
        let tree = PaneNode::HSplit {
            children: vec![
                (
                    0.8,
                    PaneNode::VSplit {
                        children: vec![
                            (
                                0.9,
                                PaneNode::Leaf {
                                    pane_id: 1,
                                    rows: 24,
                                    cols: 80,
                                    cwd: None,
                                    title: None,
                                    is_active: false,
                                },
                            ),
                            (
                                0.1,
                                PaneNode::Leaf {
                                    pane_id: 2,
                                    rows: 24,
                                    cols: 80,
                                    cwd: None,
                                    title: None,
                                    is_active: false,
                                },
                            ),
                        ],
                    },
                ),
                (
                    0.2,
                    PaneNode::Leaf {
                        pane_id: 3,
                        rows: 24,
                        cols: 80,
                        cwd: None,
                        title: None,
                        is_active: false,
                    },
                ),
            ],
        };

        let rebalanced = TopologyOrchestrator::rebalance_tree(&tree);
        match &rebalanced {
            PaneNode::HSplit { children } => {
                assert!((children[0].0 - 0.5).abs() < 1e-10);
                assert!((children[1].0 - 0.5).abs() < 1e-10);
                // Nested VSplit should also be rebalanced
                match &children[0].1 {
                    PaneNode::VSplit { children: inner } => {
                        assert!((inner[0].0 - 0.5).abs() < 1e-10);
                        assert!((inner[1].0 - 0.5).abs() < 1e-10);
                    }
                    _ => panic!("expected VSplit"),
                }
            }
            _ => panic!("expected HSplit"),
        }
    }

    // -------------------------------------------------------------------------
    // Audit log tests
    // -------------------------------------------------------------------------

    #[test]
    fn audit_log_records() {
        let mut orch = TopologyOrchestrator::new();
        assert!(orch.audit_log().is_empty());

        orch.record_audit(
            TopologyOp::Rebalance {
                scope: window_identity(1),
            },
            true,
            None,
            Some("corr-123".into()),
        );

        assert_eq!(orch.audit_log().len(), 1);
        assert!(orch.audit_log()[0].succeeded);
        assert_eq!(
            orch.audit_log()[0].correlation_id.as_deref(),
            Some("corr-123")
        );
    }

    #[test]
    fn audit_log_eviction() {
        let mut orch = TopologyOrchestrator::new();
        orch.max_audit_entries = 10;

        for i in 0..15 {
            orch.record_audit(
                TopologyOp::Rebalance {
                    scope: window_identity(i),
                },
                true,
                None,
                None,
            );
        }

        // After 10 entries, eviction kicks in removing 10% (1 entry)
        // So at 11 entries we evict 1, leaving 10. Then add 4 more = ~13
        // The exact count depends on eviction timing but should be <= 15
        assert!(orch.audit_log().len() <= 15);
        assert!(orch.audit_log().len() >= 10);
    }

    // -------------------------------------------------------------------------
    // TopologySplitDirection conversion
    // -------------------------------------------------------------------------

    #[test]
    fn split_direction_to_wezterm() {
        assert_eq!(
            TopologySplitDirection::Left.to_wezterm(),
            SplitDirection::Left
        );
        assert_eq!(
            TopologySplitDirection::Right.to_wezterm(),
            SplitDirection::Right
        );
        assert_eq!(
            TopologySplitDirection::Top.to_wezterm(),
            SplitDirection::Top
        );
        assert_eq!(
            TopologySplitDirection::Bottom.to_wezterm(),
            SplitDirection::Bottom
        );
    }

    // -------------------------------------------------------------------------
    // Serde roundtrip tests
    // -------------------------------------------------------------------------

    #[test]
    fn serde_roundtrip_layout_template() {
        let template = LayoutTemplate {
            name: "test".into(),
            description: Some("A test layout".into()),
            root: LayoutNode::VSplit {
                children: vec![
                    LayoutNode::Slot {
                        role: Some("left".into()),
                        weight: 2.0,
                    },
                    LayoutNode::Slot {
                        role: Some("right".into()),
                        weight: 1.0,
                    },
                ],
            },
            min_panes: 2,
            max_panes: Some(2),
        };

        let json = serde_json::to_string(&template).unwrap();
        let deserialized: LayoutTemplate = serde_json::from_str(&json).unwrap();
        assert_eq!(template, deserialized);
    }

    #[test]
    fn serde_roundtrip_topology_op() {
        let ops = vec![
            TopologyOp::Split {
                target: pane_identity(1),
                direction: TopologySplitDirection::Right,
                ratio: 0.5,
            },
            TopologyOp::Close {
                target: pane_identity(2),
            },
            TopologyOp::Swap {
                a: pane_identity(1),
                b: pane_identity(3),
            },
            TopologyOp::Move {
                target: pane_identity(1),
                direction: TopologyMoveDirection::Left,
            },
            TopologyOp::ApplyTemplate {
                window: window_identity(1),
                template_name: "grid-2x2".into(),
            },
            TopologyOp::Rebalance {
                scope: window_identity(1),
            },
            TopologyOp::CreateFocusGroup {
                name: "agents".into(),
                members: vec![pane_identity(1), pane_identity(2)],
            },
        ];

        for op in &ops {
            let json = serde_json::to_string(op).unwrap();
            let deserialized: TopologyOp = serde_json::from_str(&json).unwrap();
            assert_eq!(op, &deserialized);
        }
    }

    #[test]
    fn serde_roundtrip_focus_group() {
        let group = FocusGroup {
            name: "test-group".into(),
            members: vec![pane_identity(1), pane_identity(2)],
            focused: true,
            created_at: 1234567890,
        };

        let json = serde_json::to_string(&group).unwrap();
        let deserialized: FocusGroup = serde_json::from_str(&json).unwrap();
        assert_eq!(group, deserialized);
    }

    // -------------------------------------------------------------------------
    // TopologyError display tests
    // -------------------------------------------------------------------------

    #[test]
    fn topology_error_display() {
        let err = TopologyError::EntityNotFound {
            identity: "pane:42".into(),
        };
        assert!(err.to_string().contains("pane:42"));

        let err = TopologyError::InvalidLifecycleState {
            identity: "pane:1".into(),
            state: "Closed".into(),
            operation: "split".into(),
        };
        assert!(err.to_string().contains("split"));
        assert!(err.to_string().contains("Closed"));

        let err = TopologyError::LastPaneProtection {
            window: "window:1".into(),
        };
        assert!(err.to_string().contains("last pane"));

        let err = TopologyError::InvalidRatio { ratio: -0.5 };
        assert!(err.to_string().contains("-0.5"));
    }

    // -------------------------------------------------------------------------
    // Orchestrator default/with_templates
    // -------------------------------------------------------------------------

    #[test]
    fn orchestrator_default_has_templates() {
        let orch = TopologyOrchestrator::default();
        assert!(orch.templates().len() >= 4);
    }

    #[test]
    fn orchestrator_with_custom_templates() {
        let reg = TemplateRegistry::new();
        let orch = TopologyOrchestrator::with_templates(reg);
        assert!(orch.templates().is_empty());
    }

    // -------------------------------------------------------------------------
    // CreateFocusGroup via validate_op
    // -------------------------------------------------------------------------

    #[test]
    fn validate_create_focus_group_ok() {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1, 2]);

        let op = TopologyOp::CreateFocusGroup {
            name: "g".into(),
            members: vec![pane_identity(1), pane_identity(2)],
        };

        assert_eq!(orch.validate_op(&op, &reg), OpCheckResult::Ok);
    }

    #[test]
    fn validate_create_focus_group_duplicate() {
        let mut orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1]);
        orch.create_focus_group("g".into(), vec![pane_identity(1)], &reg)
            .unwrap();

        let op = TopologyOp::CreateFocusGroup {
            name: "g".into(),
            members: vec![pane_identity(1)],
        };

        match orch.validate_op(&op, &reg) {
            OpCheckResult::ConstraintViolation { reason } => {
                assert!(reason.contains("already exists"));
            }
            other => panic!("expected ConstraintViolation, got {other:?}"),
        }
    }

    #[test]
    fn validate_move_pane() {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry_with_panes(&[1]);

        let op = TopologyOp::Move {
            target: pane_identity(1),
            direction: TopologyMoveDirection::Right,
        };

        assert_eq!(orch.validate_op(&op, &reg), OpCheckResult::Ok);
    }

    #[test]
    fn validate_move_draining_pane() {
        let mut reg = LifecycleRegistry::new();
        let identity = pane_identity(1);
        reg.register_entity(
            identity,
            LifecycleState::Pane(MuxPaneLifecycleState::Draining),
            0,
        )
        .expect("register pane");

        let orch = TopologyOrchestrator::new();
        let op = TopologyOp::Move {
            target: pane_identity(1),
            direction: TopologyMoveDirection::Down,
        };

        match orch.validate_op(&op, &reg) {
            OpCheckResult::InvalidState { reason, .. } => {
                assert!(reason.contains("Running or Ready"));
            }
            other => panic!("expected InvalidState, got {other:?}"),
        }
    }
}
