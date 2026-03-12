//! Swarm command center dashboard and command palette (ft-3681t.9.2).
//!
//! Provides a keyboard-first, policy-aware command center for daily fleet
//! operations. The command palette offers fuzzy-matched, state-consistent,
//! safely interruptible actions with explicit latency budgets.
//!
//! # Architecture
//!
//! ```text
//! SwarmCommandCenter
//!   ├── CommandPalette       — fuzzy-matched action registry
//!   │     ├── PaletteAction  — individual command with preconditions
//!   │     └── ActionResult   — outcome + structured log
//!   ├── LiveView             — real-time fleet state panel
//!   │     ├── PaneStatusRow  — per-pane status summary
//!   │     └── UpdateThrottle — backpressure-aware refresh
//!   ├── KeyBinding           — keyboard-first navigation
//!   └── CommandCenterTelemetry
//! ```
//!
//! # Usage
//!
//! ```rust
//! use frankenterm_core::swarm_command_center::*;
//!
//! let mut center = SwarmCommandCenter::new(LatencyBudget::default());
//!
//! // Register actions
//! center.palette.register(PaletteAction::new(
//!     "fleet-pause",
//!     "Pause all agent panes",
//!     ActionCategory::FleetControl,
//! ).requires_role(OperatorLevel::SeniorOperator));
//!
//! // Search palette
//! let matches = center.palette.search("pause");
//! assert!(!matches.is_empty());
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Operator level (access control)
// =============================================================================

/// Operator access level for command gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum OperatorLevel {
    /// Read-only observer.
    Observer,
    /// Standard operator — can issue non-destructive commands.
    Operator,
    /// Senior operator — can issue destructive and fleet-wide commands.
    SeniorOperator,
    /// Admin — full access including policy overrides.
    Admin,
}

impl OperatorLevel {
    /// Numeric level (0–3).
    #[must_use]
    pub fn level(&self) -> u32 {
        match self {
            Self::Observer => 0,
            Self::Operator => 1,
            Self::SeniorOperator => 2,
            Self::Admin => 3,
        }
    }

    /// Whether this level has at least the given level.
    #[must_use]
    pub fn has_at_least(&self, required: OperatorLevel) -> bool {
        self.level() >= required.level()
    }

    /// Human label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Observer => "observer",
            Self::Operator => "operator",
            Self::SeniorOperator => "senior operator",
            Self::Admin => "admin",
        }
    }
}

// =============================================================================
// Action categories and status
// =============================================================================

/// Category of a palette action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActionCategory {
    /// Fleet-wide control (pause, resume, scale).
    FleetControl,
    /// Individual pane management (focus, close, restart).
    PaneManagement,
    /// Agent lifecycle (spawn, terminate, reassign).
    AgentLifecycle,
    /// Policy and safety (quarantine, approve, override).
    PolicySafety,
    /// Monitoring and diagnostics (health, logs, traces).
    Diagnostics,
    /// Session management (save, restore, export).
    SessionManagement,
    /// Search and navigation (find pane, search logs).
    Navigation,
    /// Configuration (settings, profiles, tuning).
    Configuration,
}

impl ActionCategory {
    /// All categories.
    pub const ALL: &'static [ActionCategory] = &[
        Self::FleetControl,
        Self::PaneManagement,
        Self::AgentLifecycle,
        Self::PolicySafety,
        Self::Diagnostics,
        Self::SessionManagement,
        Self::Navigation,
        Self::Configuration,
    ];

    /// Human label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::FleetControl => "Fleet Control",
            Self::PaneManagement => "Pane Management",
            Self::AgentLifecycle => "Agent Lifecycle",
            Self::PolicySafety => "Policy & Safety",
            Self::Diagnostics => "Diagnostics",
            Self::SessionManagement => "Session Management",
            Self::Navigation => "Navigation",
            Self::Configuration => "Configuration",
        }
    }

    /// Keyboard shortcut prefix for category quick-access.
    #[must_use]
    pub fn shortcut_prefix(&self) -> &'static str {
        match self {
            Self::FleetControl => "f",
            Self::PaneManagement => "p",
            Self::AgentLifecycle => "a",
            Self::PolicySafety => "s",
            Self::Diagnostics => "d",
            Self::SessionManagement => "e",
            Self::Navigation => "n",
            Self::Configuration => "c",
        }
    }
}

/// Whether a palette action is safe to execute in the current context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionAvailability {
    /// Action is available and safe to execute.
    Available,
    /// Action is available but requires confirmation.
    RequiresConfirmation,
    /// Action is blocked by policy.
    PolicyBlocked,
    /// Action requires a higher operator level.
    InsufficientPrivilege,
    /// Action is not applicable in the current state.
    NotApplicable,
}

impl ActionAvailability {
    /// Whether the action can be executed (possibly after confirmation).
    #[must_use]
    pub fn is_executable(&self) -> bool {
        matches!(self, Self::Available | Self::RequiresConfirmation)
    }
}

// =============================================================================
// Palette actions
// =============================================================================

/// A command that can be invoked from the palette.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaletteAction {
    /// Unique action identifier.
    pub action_id: String,
    /// Human-readable label (shown in palette).
    pub label: String,
    /// Category for grouping and filtering.
    pub category: ActionCategory,
    /// Minimum operator level required.
    pub min_level: OperatorLevel,
    /// Whether this is a destructive action (requires confirmation).
    pub destructive: bool,
    /// Whether this action can be safely interrupted mid-execution.
    pub interruptible: bool,
    /// Keyboard shortcut (e.g., "Ctrl+Shift+P").
    pub shortcut: String,
    /// Tags for fuzzy search.
    pub tags: Vec<String>,
    /// Brief description.
    pub description: String,
}

impl PaletteAction {
    /// Create a new action.
    #[must_use]
    pub fn new(
        action_id: impl Into<String>,
        label: impl Into<String>,
        category: ActionCategory,
    ) -> Self {
        Self {
            action_id: action_id.into(),
            label: label.into(),
            category,
            min_level: OperatorLevel::Operator,
            destructive: false,
            interruptible: true,
            shortcut: String::new(),
            tags: Vec::new(),
            description: String::new(),
        }
    }

    /// Set minimum operator level.
    #[must_use]
    pub fn requires_role(mut self, level: OperatorLevel) -> Self {
        self.min_level = level;
        self
    }

    /// Mark as destructive (requires confirmation).
    #[must_use]
    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    /// Mark as non-interruptible.
    #[must_use]
    pub fn non_interruptible(mut self) -> Self {
        self.interruptible = false;
        self
    }

    /// Set keyboard shortcut.
    #[must_use]
    pub fn with_shortcut(mut self, shortcut: impl Into<String>) -> Self {
        self.shortcut = shortcut.into();
        self
    }

    /// Add tags for fuzzy search.
    #[must_use]
    pub fn with_tags(mut self, tags: &[&str]) -> Self {
        self.tags = tags.iter().map(|s| (*s).to_string()).collect();
        self
    }

    /// Set description.
    #[must_use]
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// Check availability for a given operator level and policy state.
    #[must_use]
    pub fn availability(
        &self,
        operator_level: OperatorLevel,
        policy_blocked: bool,
    ) -> ActionAvailability {
        if policy_blocked {
            return ActionAvailability::PolicyBlocked;
        }
        if !operator_level.has_at_least(self.min_level) {
            return ActionAvailability::InsufficientPrivilege;
        }
        if self.destructive {
            return ActionAvailability::RequiresConfirmation;
        }
        ActionAvailability::Available
    }

    /// Compute a fuzzy match score against a query (0 = no match, higher = better).
    #[must_use]
    pub fn match_score(&self, query: &str) -> u32 {
        if query.is_empty() {
            return 1; // everything matches empty query with base score
        }

        let query_lower = query.to_lowercase();
        let mut score: u32 = 0;

        // Exact ID match
        if self.action_id.to_lowercase() == query_lower {
            score += 100;
        }

        // Label contains query
        let label_lower = self.label.to_lowercase();
        if label_lower.contains(&query_lower) {
            score += 50;
        }

        // Label starts with query
        if label_lower.starts_with(&query_lower) {
            score += 25;
        }

        // Tag match
        for tag in &self.tags {
            if tag.to_lowercase().contains(&query_lower) {
                score += 20;
            }
        }

        // Category prefix match
        if self.category.shortcut_prefix().starts_with(&query_lower) {
            score += 10;
        }

        // Subsequence match on label
        if is_subsequence(&query_lower, &label_lower) {
            score += 15;
        }

        // Description contains query
        if self.description.to_lowercase().contains(&query_lower) {
            score += 5;
        }

        score
    }
}

/// Check if `needle` is a subsequence of `haystack`.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut needle_chars = needle.chars();
    let mut current = match needle_chars.next() {
        Some(c) => c,
        None => return true,
    };
    for h in haystack.chars() {
        if h == current {
            current = match needle_chars.next() {
                Some(c) => c,
                None => return true,
            };
        }
    }
    false
}

/// Result of executing a palette action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    /// Which action was executed.
    pub action_id: String,
    /// Whether execution succeeded.
    pub success: bool,
    /// Human-readable outcome message.
    pub message: String,
    /// Duration of execution in ms.
    pub duration_ms: u64,
    /// Whether the action was interrupted.
    pub interrupted: bool,
    /// Structured log entries from the action.
    pub log_entries: Vec<ActionLogEntry>,
}

/// A structured log entry from action execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionLogEntry {
    /// Timestamp (epoch ms).
    pub timestamp_ms: u64,
    /// Component that emitted the log.
    pub component: String,
    /// Correlation ID for tracing.
    pub correlation_id: String,
    /// Log message.
    pub message: String,
    /// Structured fields.
    pub fields: BTreeMap<String, String>,
}

// =============================================================================
// Command palette
// =============================================================================

/// The command palette — searchable, category-filtered action registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandPalette {
    /// Registered actions.
    actions: Vec<PaletteAction>,
    /// Whether the palette is currently open/visible.
    pub is_open: bool,
    /// Current search query.
    pub query: String,
    /// Currently highlighted result index.
    pub selected_index: usize,
    /// Telemetry.
    pub telemetry: PaletteTelemetry,
}

/// Palette usage telemetry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PaletteTelemetry {
    /// Times the palette was opened.
    pub open_count: u64,
    /// Actions executed from the palette.
    pub executions: u64,
    /// Searches performed.
    pub searches: u64,
    /// Times a search yielded zero results.
    pub empty_searches: u64,
}

impl CommandPalette {
    /// Create a new empty palette.
    #[must_use]
    pub fn new() -> Self {
        Self {
            actions: Vec::new(),
            is_open: false,
            query: String::new(),
            selected_index: 0,
            telemetry: PaletteTelemetry::default(),
        }
    }

    /// Register an action.
    pub fn register(&mut self, action: PaletteAction) {
        self.actions.push(action);
    }

    /// Number of registered actions.
    #[must_use]
    pub fn action_count(&self) -> usize {
        self.actions.len()
    }

    /// Open the palette.
    pub fn open(&mut self) {
        self.is_open = true;
        self.query.clear();
        self.selected_index = 0;
        self.telemetry.open_count += 1;
    }

    /// Close the palette.
    pub fn close(&mut self) {
        self.is_open = false;
        self.query.clear();
        self.selected_index = 0;
    }

    /// Update the search query.
    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
        self.selected_index = 0;
        self.telemetry.searches += 1;
    }

    /// Search actions by query, sorted by relevance score descending.
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<SearchResult> {
        let mut results: Vec<SearchResult> = self
            .actions
            .iter()
            .map(|action| {
                let score = action.match_score(query);
                SearchResult {
                    action_id: action.action_id.clone(),
                    label: action.label.clone(),
                    category: action.category,
                    score,
                    shortcut: action.shortcut.clone(),
                }
            })
            .filter(|r| r.score > 0)
            .collect();

        results.sort_by(|a, b| b.score.cmp(&a.score));
        results
    }

    /// Get filtered actions by category.
    #[must_use]
    pub fn by_category(&self, category: ActionCategory) -> Vec<&PaletteAction> {
        self.actions
            .iter()
            .filter(|a| a.category == category)
            .collect()
    }

    /// Get actions available to a given operator level.
    #[must_use]
    pub fn available_actions(&self, level: OperatorLevel) -> Vec<&PaletteAction> {
        self.actions
            .iter()
            .filter(|a| level.has_at_least(a.min_level))
            .collect()
    }

    /// Get action by ID.
    #[must_use]
    pub fn get_action(&self, action_id: &str) -> Option<&PaletteAction> {
        self.actions.iter().find(|a| a.action_id == action_id)
    }

    /// Move selection up.
    pub fn select_previous(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    /// Move selection down, bounded by result count.
    pub fn select_next(&mut self, result_count: usize) {
        if result_count > 0 && self.selected_index < result_count - 1 {
            self.selected_index += 1;
        }
    }
}

impl Default for CommandPalette {
    fn default() -> Self {
        Self::new()
    }
}

/// A search result from the palette.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Action ID.
    pub action_id: String,
    /// Action label.
    pub label: String,
    /// Category.
    pub category: ActionCategory,
    /// Relevance score.
    pub score: u32,
    /// Keyboard shortcut.
    pub shortcut: String,
}

// =============================================================================
// Live view (fleet status panel)
// =============================================================================

/// Health status for a pane in the live view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneHealth {
    /// Healthy and responsive.
    Healthy,
    /// Minor issues (high latency, warnings).
    Degraded,
    /// Errors or unresponsive.
    Unhealthy,
    /// Pane not running.
    Stopped,
}

/// Summary row for a single pane in the live view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneStatusRow {
    /// Pane identifier.
    pub pane_id: String,
    /// Human-readable title (agent name, command, etc.).
    pub title: String,
    /// Current health.
    pub health: PaneHealth,
    /// Agent identity if assigned.
    pub agent_id: Option<String>,
    /// Last activity timestamp (epoch ms).
    pub last_activity_ms: u64,
    /// CPU usage percentage (0–100).
    pub cpu_percent: f64,
    /// Memory usage in MB.
    pub memory_mb: f64,
    /// Event throughput (events/sec).
    pub event_rate: f64,
    /// Active alerts count.
    pub alert_count: u32,
}

/// Configuration for the live view update throttle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateThrottle {
    /// Minimum interval between updates (ms).
    pub min_interval_ms: u64,
    /// Last update timestamp (epoch ms).
    pub last_update_ms: u64,
    /// Updates skipped due to throttling.
    pub skipped_count: u64,
}

impl UpdateThrottle {
    /// Create a new throttle with given minimum interval.
    #[must_use]
    pub fn new(min_interval_ms: u64) -> Self {
        Self {
            min_interval_ms,
            last_update_ms: 0,
            skipped_count: 0,
        }
    }

    /// Check if an update should be performed at the given time.
    pub fn should_update(&mut self, now_ms: u64) -> bool {
        if now_ms.saturating_sub(self.last_update_ms) >= self.min_interval_ms {
            self.last_update_ms = now_ms;
            true
        } else {
            self.skipped_count += 1;
            false
        }
    }
}

/// The live view — real-time fleet status panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveView {
    /// Pane status rows, sorted by last_activity descending.
    pub panes: Vec<PaneStatusRow>,
    /// Update throttle.
    pub throttle: UpdateThrottle,
    /// Whether the view is in compact mode.
    pub compact: bool,
    /// Current sort order.
    pub sort_by: LiveViewSort,
    /// Filter by health status (None = show all).
    pub health_filter: Option<PaneHealth>,
}

/// Sort order for the live view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LiveViewSort {
    /// Most recently active first.
    Activity,
    /// By health (unhealthy first).
    Health,
    /// By agent ID.
    AgentId,
    /// By CPU usage (highest first).
    CpuUsage,
    /// By alert count (most first).
    AlertCount,
}

impl LiveView {
    /// Create a new live view with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            panes: Vec::new(),
            throttle: UpdateThrottle::new(250), // 4 Hz refresh
            compact: false,
            sort_by: LiveViewSort::Activity,
            health_filter: None,
        }
    }

    /// Update the pane status list.
    pub fn update_panes(&mut self, panes: Vec<PaneStatusRow>, now_ms: u64) {
        if !self.throttle.should_update(now_ms) {
            return;
        }
        self.panes = panes;
        self.sort();
    }

    /// Apply the current sort order.
    pub fn sort(&mut self) {
        match self.sort_by {
            LiveViewSort::Activity => {
                self.panes.sort_by(|a, b| b.last_activity_ms.cmp(&a.last_activity_ms));
            }
            LiveViewSort::Health => {
                self.panes.sort_by_key(|p| match p.health {
                    PaneHealth::Unhealthy => 0,
                    PaneHealth::Degraded => 1,
                    PaneHealth::Healthy => 2,
                    PaneHealth::Stopped => 3,
                });
            }
            LiveViewSort::AgentId => {
                self.panes.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
            }
            LiveViewSort::CpuUsage => {
                self.panes
                    .sort_by(|a, b| b.cpu_percent.partial_cmp(&a.cpu_percent).unwrap_or(std::cmp::Ordering::Equal));
            }
            LiveViewSort::AlertCount => {
                self.panes.sort_by(|a, b| b.alert_count.cmp(&a.alert_count));
            }
        }
    }

    /// Get filtered panes based on current health filter.
    #[must_use]
    pub fn filtered_panes(&self) -> Vec<&PaneStatusRow> {
        match self.health_filter {
            Some(filter) => self.panes.iter().filter(|p| p.health == filter).collect(),
            None => self.panes.iter().collect(),
        }
    }

    /// Count panes by health status.
    #[must_use]
    pub fn health_summary(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        counts.insert("healthy", 0);
        counts.insert("degraded", 0);
        counts.insert("unhealthy", 0);
        counts.insert("stopped", 0);
        for pane in &self.panes {
            let key = match pane.health {
                PaneHealth::Healthy => "healthy",
                PaneHealth::Degraded => "degraded",
                PaneHealth::Unhealthy => "unhealthy",
                PaneHealth::Stopped => "stopped",
            };
            *counts.get_mut(key).unwrap() += 1;
        }
        counts
    }

    /// Total event throughput across all panes.
    #[must_use]
    pub fn total_event_rate(&self) -> f64 {
        self.panes.iter().map(|p| p.event_rate).sum()
    }
}

impl Default for LiveView {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Latency budget
// =============================================================================

/// Interaction latency budgets for the command center.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyBudget {
    /// Max latency for palette open (ms).
    pub palette_open_ms: u64,
    /// Max latency for search results (ms).
    pub search_results_ms: u64,
    /// Max latency for action execution feedback (ms).
    pub action_feedback_ms: u64,
    /// Max latency for live view refresh (ms).
    pub live_view_refresh_ms: u64,
    /// Max latency for keyboard navigation (ms).
    pub keyboard_nav_ms: u64,
}

impl Default for LatencyBudget {
    fn default() -> Self {
        Self {
            palette_open_ms: 50,
            search_results_ms: 16, // one frame at 60fps
            action_feedback_ms: 100,
            live_view_refresh_ms: 250,
            keyboard_nav_ms: 16,
        }
    }
}

impl LatencyBudget {
    /// Strict budget for high-performance setups.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            palette_open_ms: 16,
            search_results_ms: 8,
            action_feedback_ms: 50,
            live_view_refresh_ms: 100,
            keyboard_nav_ms: 8,
        }
    }

    /// Check if a measured latency is within budget.
    #[must_use]
    pub fn check(&self, category: &str, measured_ms: u64) -> LatencyCheck {
        let budget = match category {
            "palette_open" => self.palette_open_ms,
            "search_results" => self.search_results_ms,
            "action_feedback" => self.action_feedback_ms,
            "live_view_refresh" => self.live_view_refresh_ms,
            "keyboard_nav" => self.keyboard_nav_ms,
            _ => u64::MAX,
        };
        LatencyCheck {
            category: category.into(),
            budget_ms: budget,
            measured_ms,
            within_budget: measured_ms <= budget,
        }
    }
}

/// Result of a latency budget check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyCheck {
    /// Category checked.
    pub category: String,
    /// Budget limit (ms).
    pub budget_ms: u64,
    /// Measured value (ms).
    pub measured_ms: u64,
    /// Whether within budget.
    pub within_budget: bool,
}

// =============================================================================
// Key bindings
// =============================================================================

/// A keyboard binding for the command center.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyBinding {
    /// Key combination (e.g., "Ctrl+P", "Ctrl+Shift+F").
    pub keys: String,
    /// What the binding does.
    pub action: KeyAction,
    /// Human label.
    pub label: String,
}

/// Actions that can be bound to keyboard shortcuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyAction {
    /// Open/close the command palette.
    TogglePalette,
    /// Focus the next pane in the live view.
    NextPane,
    /// Focus the previous pane.
    PreviousPane,
    /// Toggle compact mode.
    ToggleCompact,
    /// Cycle sort order.
    CycleSort,
    /// Filter by health status.
    CycleHealthFilter,
    /// Open diagnostics for focused pane.
    OpenDiagnostics,
    /// Trigger emergency stop.
    EmergencyStop,
    /// Refresh the live view.
    RefreshView,
    /// Navigate to search.
    FocusSearch,
}

/// Standard keyboard bindings.
#[must_use]
pub fn standard_keybindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding {
            keys: "Ctrl+P".into(),
            action: KeyAction::TogglePalette,
            label: "Toggle command palette".into(),
        },
        KeyBinding {
            keys: "Tab".into(),
            action: KeyAction::NextPane,
            label: "Focus next pane".into(),
        },
        KeyBinding {
            keys: "Shift+Tab".into(),
            action: KeyAction::PreviousPane,
            label: "Focus previous pane".into(),
        },
        KeyBinding {
            keys: "Ctrl+\\".into(),
            action: KeyAction::ToggleCompact,
            label: "Toggle compact mode".into(),
        },
        KeyBinding {
            keys: "Ctrl+S".into(),
            action: KeyAction::CycleSort,
            label: "Cycle sort order".into(),
        },
        KeyBinding {
            keys: "Ctrl+H".into(),
            action: KeyAction::CycleHealthFilter,
            label: "Cycle health filter".into(),
        },
        KeyBinding {
            keys: "Ctrl+D".into(),
            action: KeyAction::OpenDiagnostics,
            label: "Open diagnostics".into(),
        },
        KeyBinding {
            keys: "Ctrl+Shift+X".into(),
            action: KeyAction::EmergencyStop,
            label: "Emergency stop".into(),
        },
        KeyBinding {
            keys: "F5".into(),
            action: KeyAction::RefreshView,
            label: "Refresh live view".into(),
        },
        KeyBinding {
            keys: "/".into(),
            action: KeyAction::FocusSearch,
            label: "Focus search".into(),
        },
    ]
}

// =============================================================================
// Command center (main orchestrator)
// =============================================================================

/// Telemetry for the command center.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandCenterTelemetry {
    /// Actions executed.
    pub actions_executed: u64,
    /// Actions that failed.
    pub actions_failed: u64,
    /// Actions blocked by policy.
    pub policy_blocks: u64,
    /// Actions blocked by privilege level.
    pub privilege_blocks: u64,
    /// Live view updates performed.
    pub view_updates: u64,
    /// Latency budget violations.
    pub budget_violations: u64,
}

/// The swarm command center — main UI orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmCommandCenter {
    /// The command palette.
    pub palette: CommandPalette,
    /// The live fleet view.
    pub live_view: LiveView,
    /// Keyboard bindings.
    pub keybindings: Vec<KeyBinding>,
    /// Latency budget.
    pub latency_budget: LatencyBudget,
    /// Current operator level.
    pub operator_level: OperatorLevel,
    /// Telemetry.
    pub telemetry: CommandCenterTelemetry,
    /// Action execution log.
    pub action_log: Vec<ActionResult>,
    /// Maximum log entries to retain.
    pub max_log_entries: usize,
}

impl SwarmCommandCenter {
    /// Create a new command center.
    #[must_use]
    pub fn new(latency_budget: LatencyBudget) -> Self {
        Self {
            palette: CommandPalette::new(),
            live_view: LiveView::new(),
            keybindings: standard_keybindings(),
            latency_budget,
            operator_level: OperatorLevel::Operator,
            telemetry: CommandCenterTelemetry::default(),
            action_log: Vec::new(),
            max_log_entries: 1000,
        }
    }

    /// Set the operator level.
    pub fn set_operator_level(&mut self, level: OperatorLevel) {
        self.operator_level = level;
    }

    /// Check action availability with current operator level.
    #[must_use]
    pub fn check_action(&self, action_id: &str, policy_blocked: bool) -> ActionAvailability {
        match self.palette.get_action(action_id) {
            Some(action) => action.availability(self.operator_level, policy_blocked),
            None => ActionAvailability::NotApplicable,
        }
    }

    /// Record an action result.
    pub fn record_action(&mut self, result: ActionResult) {
        if result.success {
            self.telemetry.actions_executed += 1;
        } else {
            self.telemetry.actions_failed += 1;
        }

        self.action_log.push(result);

        // Trim log if over limit
        if self.action_log.len() > self.max_log_entries {
            let drain_count = self.action_log.len() - self.max_log_entries;
            self.action_log.drain(..drain_count);
        }
    }

    /// Record a latency measurement and check against budget.
    pub fn record_latency(&mut self, category: &str, measured_ms: u64) -> LatencyCheck {
        let check = self.latency_budget.check(category, measured_ms);
        if !check.within_budget {
            self.telemetry.budget_violations += 1;
        }
        check
    }

    /// Get a snapshot of the command center state.
    #[must_use]
    pub fn snapshot(&self) -> CommandCenterSnapshot {
        let health_summary = self.live_view.health_summary();
        CommandCenterSnapshot {
            operator_level: self.operator_level,
            palette_open: self.palette.is_open,
            palette_query: self.palette.query.clone(),
            registered_actions: self.palette.action_count(),
            available_actions: self.palette.available_actions(self.operator_level).len(),
            total_panes: self.live_view.panes.len(),
            healthy_panes: *health_summary.get("healthy").unwrap_or(&0),
            degraded_panes: *health_summary.get("degraded").unwrap_or(&0),
            unhealthy_panes: *health_summary.get("unhealthy").unwrap_or(&0),
            total_event_rate: self.live_view.total_event_rate(),
            telemetry: self.telemetry.clone(),
        }
    }
}

/// Serializable snapshot of the command center.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandCenterSnapshot {
    /// Current operator level.
    pub operator_level: OperatorLevel,
    /// Whether palette is open.
    pub palette_open: bool,
    /// Current palette query.
    pub palette_query: String,
    /// Total registered actions.
    pub registered_actions: usize,
    /// Actions available to current operator.
    pub available_actions: usize,
    /// Total panes in live view.
    pub total_panes: usize,
    /// Healthy panes count.
    pub healthy_panes: usize,
    /// Degraded panes count.
    pub degraded_panes: usize,
    /// Unhealthy panes count.
    pub unhealthy_panes: usize,
    /// Aggregate event throughput.
    pub total_event_rate: f64,
    /// Telemetry counters.
    pub telemetry: CommandCenterTelemetry,
}

// =============================================================================
// Standard actions factory
// =============================================================================

/// Register the standard set of fleet operations actions.
pub fn register_standard_actions(palette: &mut CommandPalette) {
    // Fleet Control
    palette.register(
        PaletteAction::new("fleet-pause", "Pause all agent panes", ActionCategory::FleetControl)
            .requires_role(OperatorLevel::SeniorOperator)
            .destructive()
            .with_shortcut("Ctrl+Shift+P")
            .with_tags(&["pause", "stop", "halt", "fleet"])
            .with_description("Pause event processing on all agent panes"),
    );
    palette.register(
        PaletteAction::new("fleet-resume", "Resume all agent panes", ActionCategory::FleetControl)
            .requires_role(OperatorLevel::SeniorOperator)
            .with_shortcut("Ctrl+Shift+R")
            .with_tags(&["resume", "start", "unpause", "fleet"])
            .with_description("Resume event processing on all agent panes"),
    );
    palette.register(
        PaletteAction::new("fleet-scale", "Scale fleet size", ActionCategory::FleetControl)
            .requires_role(OperatorLevel::Admin)
            .destructive()
            .with_tags(&["scale", "resize", "add", "remove", "fleet"])
            .with_description("Adjust the number of agent panes in the fleet"),
    );

    // Pane Management
    palette.register(
        PaletteAction::new("pane-focus", "Focus pane", ActionCategory::PaneManagement)
            .requires_role(OperatorLevel::Observer)
            .with_tags(&["focus", "select", "pane", "switch"])
            .with_description("Focus a specific pane by ID or name"),
    );
    palette.register(
        PaletteAction::new("pane-close", "Close pane", ActionCategory::PaneManagement)
            .requires_role(OperatorLevel::Operator)
            .destructive()
            .with_tags(&["close", "kill", "terminate", "pane"])
            .with_description("Close a specific pane"),
    );
    palette.register(
        PaletteAction::new("pane-restart", "Restart pane", ActionCategory::PaneManagement)
            .requires_role(OperatorLevel::Operator)
            .with_tags(&["restart", "reboot", "pane", "refresh"])
            .with_description("Restart a pane process"),
    );

    // Agent Lifecycle
    palette.register(
        PaletteAction::new("agent-spawn", "Spawn new agent", ActionCategory::AgentLifecycle)
            .requires_role(OperatorLevel::Operator)
            .with_tags(&["spawn", "new", "create", "agent", "launch"])
            .with_description("Launch a new agent in a fresh pane"),
    );
    palette.register(
        PaletteAction::new("agent-terminate", "Terminate agent", ActionCategory::AgentLifecycle)
            .requires_role(OperatorLevel::SeniorOperator)
            .destructive()
            .with_tags(&["terminate", "kill", "stop", "agent"])
            .with_description("Gracefully terminate an agent"),
    );

    // Policy & Safety
    palette.register(
        PaletteAction::new(
            "policy-quarantine",
            "Quarantine component",
            ActionCategory::PolicySafety,
        )
        .requires_role(OperatorLevel::SeniorOperator)
        .destructive()
        .with_tags(&["quarantine", "isolate", "block", "safety"])
        .with_description("Quarantine a component from executing actions"),
    );
    palette.register(
        PaletteAction::new("policy-approve", "Approve pending action", ActionCategory::PolicySafety)
            .requires_role(OperatorLevel::Operator)
            .with_tags(&["approve", "allow", "permit", "action"])
            .with_description("Approve a pending action requiring operator confirmation"),
    );

    // Diagnostics
    palette.register(
        PaletteAction::new("diag-health", "System health check", ActionCategory::Diagnostics)
            .requires_role(OperatorLevel::Observer)
            .with_shortcut("Ctrl+H")
            .with_tags(&["health", "status", "doctor", "check"])
            .with_description("Run ft doctor health check"),
    );
    palette.register(
        PaletteAction::new("diag-logs", "View structured logs", ActionCategory::Diagnostics)
            .requires_role(OperatorLevel::Observer)
            .with_tags(&["logs", "trace", "debug", "structured"])
            .with_description("View recent structured log entries"),
    );

    // Session Management
    palette.register(
        PaletteAction::new(
            "session-save",
            "Save session state",
            ActionCategory::SessionManagement,
        )
        .requires_role(OperatorLevel::Operator)
        .with_tags(&["save", "snapshot", "persist", "session"])
        .with_description("Save current session state for later restore"),
    );
    palette.register(
        PaletteAction::new(
            "session-restore",
            "Restore saved session",
            ActionCategory::SessionManagement,
        )
        .requires_role(OperatorLevel::Operator)
        .with_tags(&["restore", "load", "resume", "session"])
        .with_description("Restore a previously saved session"),
    );

    // Navigation
    palette.register(
        PaletteAction::new("nav-search", "Search panes", ActionCategory::Navigation)
            .requires_role(OperatorLevel::Observer)
            .with_shortcut("/")
            .with_tags(&["search", "find", "filter", "pane"])
            .with_description("Search and filter panes by name, agent, or content"),
    );
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- OperatorLevel ----

    #[test]
    fn operator_level_ordering() {
        assert!(OperatorLevel::Observer < OperatorLevel::Operator);
        assert!(OperatorLevel::Operator < OperatorLevel::SeniorOperator);
        assert!(OperatorLevel::SeniorOperator < OperatorLevel::Admin);
    }

    #[test]
    fn operator_has_at_least() {
        assert!(OperatorLevel::Admin.has_at_least(OperatorLevel::Observer));
        assert!(OperatorLevel::Operator.has_at_least(OperatorLevel::Operator));
        assert!(!OperatorLevel::Observer.has_at_least(OperatorLevel::Operator));
    }

    // ---- ActionCategory ----

    #[test]
    fn all_categories_have_labels() {
        for cat in ActionCategory::ALL {
            assert!(!cat.label().is_empty());
            assert!(!cat.shortcut_prefix().is_empty());
        }
    }

    // ---- ActionAvailability ----

    #[test]
    fn availability_executable() {
        assert!(ActionAvailability::Available.is_executable());
        assert!(ActionAvailability::RequiresConfirmation.is_executable());
        assert!(!ActionAvailability::PolicyBlocked.is_executable());
        assert!(!ActionAvailability::InsufficientPrivilege.is_executable());
        assert!(!ActionAvailability::NotApplicable.is_executable());
    }

    // ---- PaletteAction ----

    #[test]
    fn action_builder() {
        let action = PaletteAction::new("test", "Test Action", ActionCategory::Diagnostics)
            .requires_role(OperatorLevel::Admin)
            .destructive()
            .non_interruptible()
            .with_shortcut("Ctrl+T")
            .with_tags(&["test", "debug"])
            .with_description("A test action");

        assert_eq!(action.action_id, "test");
        assert_eq!(action.min_level, OperatorLevel::Admin);
        assert!(action.destructive);
        assert!(!action.interruptible);
        assert_eq!(action.shortcut, "Ctrl+T");
        assert_eq!(action.tags.len(), 2);
    }

    #[test]
    fn action_availability_checks() {
        let action = PaletteAction::new("test", "Test", ActionCategory::FleetControl)
            .requires_role(OperatorLevel::SeniorOperator)
            .destructive();

        // Policy blocked overrides everything
        assert_eq!(
            action.availability(OperatorLevel::Admin, true),
            ActionAvailability::PolicyBlocked
        );

        // Insufficient privilege
        assert_eq!(
            action.availability(OperatorLevel::Operator, false),
            ActionAvailability::InsufficientPrivilege
        );

        // Destructive requires confirmation
        assert_eq!(
            action.availability(OperatorLevel::SeniorOperator, false),
            ActionAvailability::RequiresConfirmation
        );

        // Non-destructive with sufficient privilege
        let safe_action = PaletteAction::new("safe", "Safe", ActionCategory::Diagnostics);
        assert_eq!(
            safe_action.availability(OperatorLevel::Operator, false),
            ActionAvailability::Available
        );
    }

    // ---- Fuzzy matching ----

    #[test]
    fn match_score_exact_id() {
        let action = PaletteAction::new("fleet-pause", "Pause Fleet", ActionCategory::FleetControl);
        let score = action.match_score("fleet-pause");
        assert!(score >= 100); // exact match bonus
    }

    #[test]
    fn match_score_label_contains() {
        let action = PaletteAction::new("fp", "Pause All Fleet Panes", ActionCategory::FleetControl);
        let score = action.match_score("pause");
        assert!(score >= 50);
    }

    #[test]
    fn match_score_tag_match() {
        let action = PaletteAction::new("fp", "Fleet Pause", ActionCategory::FleetControl)
            .with_tags(&["halt", "stop"]);
        let score = action.match_score("halt");
        assert!(score >= 20);
    }

    #[test]
    fn match_score_empty_query_matches_all() {
        let action = PaletteAction::new("test", "Test", ActionCategory::Diagnostics);
        assert_eq!(action.match_score(""), 1);
    }

    #[test]
    fn match_score_no_match() {
        let action = PaletteAction::new("test", "Test", ActionCategory::Diagnostics);
        assert_eq!(action.match_score("zzzznotfound"), 0);
    }

    #[test]
    fn subsequence_matching() {
        assert!(is_subsequence("fp", "fleet pause"));
        assert!(is_subsequence("flt", "fleet"));
        assert!(!is_subsequence("xyz", "fleet"));
        assert!(is_subsequence("", "anything"));
    }

    // ---- CommandPalette ----

    #[test]
    fn palette_open_close() {
        let mut palette = CommandPalette::new();
        assert!(!palette.is_open);

        palette.open();
        assert!(palette.is_open);
        assert_eq!(palette.telemetry.open_count, 1);

        palette.close();
        assert!(!palette.is_open);
    }

    #[test]
    fn palette_search_returns_sorted() {
        let mut palette = CommandPalette::new();
        palette.register(
            PaletteAction::new("fleet-pause", "Pause Fleet", ActionCategory::FleetControl)
                .with_tags(&["pause"]),
        );
        palette.register(
            PaletteAction::new("pane-focus", "Focus Pane", ActionCategory::PaneManagement),
        );

        let results = palette.search("pause");
        assert!(!results.is_empty());
        assert_eq!(results[0].action_id, "fleet-pause"); // best match first
    }

    #[test]
    fn palette_by_category() {
        let mut palette = CommandPalette::new();
        palette.register(PaletteAction::new("a", "A", ActionCategory::FleetControl));
        palette.register(PaletteAction::new("b", "B", ActionCategory::Diagnostics));
        palette.register(PaletteAction::new("c", "C", ActionCategory::FleetControl));

        let fleet = palette.by_category(ActionCategory::FleetControl);
        assert_eq!(fleet.len(), 2);
    }

    #[test]
    fn palette_available_actions_filters_by_level() {
        let mut palette = CommandPalette::new();
        palette.register(
            PaletteAction::new("obs", "Observe", ActionCategory::Diagnostics)
                .requires_role(OperatorLevel::Observer),
        );
        palette.register(
            PaletteAction::new("admin", "Admin Op", ActionCategory::PolicySafety)
                .requires_role(OperatorLevel::Admin),
        );

        let for_observer = palette.available_actions(OperatorLevel::Observer);
        assert_eq!(for_observer.len(), 1);
        assert_eq!(for_observer[0].action_id, "obs");

        let for_admin = palette.available_actions(OperatorLevel::Admin);
        assert_eq!(for_admin.len(), 2);
    }

    #[test]
    fn palette_navigation() {
        let mut palette = CommandPalette::new();
        palette.select_next(5);
        assert_eq!(palette.selected_index, 1);
        palette.select_next(5);
        assert_eq!(palette.selected_index, 2);
        palette.select_previous();
        assert_eq!(palette.selected_index, 1);
        palette.select_previous();
        assert_eq!(palette.selected_index, 0);
        palette.select_previous(); // cannot go below 0
        assert_eq!(palette.selected_index, 0);
    }

    // ---- LiveView ----

    #[test]
    fn live_view_health_summary() {
        let mut view = LiveView::new();
        view.panes = vec![
            make_pane("p1", PaneHealth::Healthy),
            make_pane("p2", PaneHealth::Healthy),
            make_pane("p3", PaneHealth::Degraded),
            make_pane("p4", PaneHealth::Unhealthy),
        ];

        let summary = view.health_summary();
        assert_eq!(summary["healthy"], 2);
        assert_eq!(summary["degraded"], 1);
        assert_eq!(summary["unhealthy"], 1);
        assert_eq!(summary["stopped"], 0);
    }

    #[test]
    fn live_view_filter_by_health() {
        let mut view = LiveView::new();
        view.panes = vec![
            make_pane("p1", PaneHealth::Healthy),
            make_pane("p2", PaneHealth::Unhealthy),
        ];
        view.health_filter = Some(PaneHealth::Unhealthy);

        let filtered = view.filtered_panes();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].pane_id, "p2");
    }

    #[test]
    fn live_view_sort_by_health() {
        let mut view = LiveView::new();
        view.sort_by = LiveViewSort::Health;
        view.panes = vec![
            make_pane("healthy", PaneHealth::Healthy),
            make_pane("unhealthy", PaneHealth::Unhealthy),
            make_pane("degraded", PaneHealth::Degraded),
        ];
        view.sort();

        assert_eq!(view.panes[0].pane_id, "unhealthy");
        assert_eq!(view.panes[1].pane_id, "degraded");
        assert_eq!(view.panes[2].pane_id, "healthy");
    }

    #[test]
    fn live_view_total_event_rate() {
        let mut view = LiveView::new();
        view.panes = vec![
            PaneStatusRow {
                event_rate: 10.5,
                ..make_pane("p1", PaneHealth::Healthy)
            },
            PaneStatusRow {
                event_rate: 20.3,
                ..make_pane("p2", PaneHealth::Healthy)
            },
        ];

        let total = view.total_event_rate();
        assert!((total - 30.8).abs() < 0.01);
    }

    // ---- UpdateThrottle ----

    #[test]
    fn throttle_allows_first_update() {
        let mut throttle = UpdateThrottle::new(100);
        // First call at time >= min_interval succeeds (last_update_ms starts at 0).
        assert!(throttle.should_update(100));
    }

    #[test]
    fn throttle_blocks_too_frequent_updates() {
        let mut throttle = UpdateThrottle::new(100);
        assert!(throttle.should_update(1000)); // first call at realistic timestamp
        assert!(!throttle.should_update(1050)); // too soon (50ms < 100ms)
        assert_eq!(throttle.skipped_count, 1);
        assert!(throttle.should_update(1100)); // exactly at interval
    }

    // ---- LatencyBudget ----

    #[test]
    fn latency_budget_checks() {
        let budget = LatencyBudget::default();
        let check = budget.check("palette_open", 30);
        assert!(check.within_budget);

        let check = budget.check("palette_open", 100);
        assert!(!check.within_budget);
    }

    // ---- Standard actions ----

    #[test]
    fn standard_actions_register() {
        let mut palette = CommandPalette::new();
        register_standard_actions(&mut palette);
        assert!(palette.action_count() >= 15);
    }

    #[test]
    fn standard_actions_searchable() {
        let mut palette = CommandPalette::new();
        register_standard_actions(&mut palette);

        let results = palette.search("pause");
        assert!(!results.is_empty());

        let results = palette.search("health");
        assert!(!results.is_empty());

        let results = palette.search("agent");
        assert!(!results.is_empty());
    }

    // ---- Standard keybindings ----

    #[test]
    fn standard_keybindings_non_empty() {
        let bindings = standard_keybindings();
        assert!(bindings.len() >= 10);
        assert!(bindings.iter().any(|b| b.action == KeyAction::TogglePalette));
        assert!(bindings.iter().any(|b| b.action == KeyAction::EmergencyStop));
    }

    // ---- SwarmCommandCenter ----

    #[test]
    fn command_center_check_action() {
        let mut center = SwarmCommandCenter::new(LatencyBudget::default());
        center.palette.register(
            PaletteAction::new("test", "Test", ActionCategory::Diagnostics)
                .requires_role(OperatorLevel::Admin),
        );

        // Operator can't access admin action
        assert_eq!(
            center.check_action("test", false),
            ActionAvailability::InsufficientPrivilege
        );

        center.set_operator_level(OperatorLevel::Admin);
        assert_eq!(
            center.check_action("test", false),
            ActionAvailability::Available
        );
    }

    #[test]
    fn command_center_action_log_trimming() {
        let mut center = SwarmCommandCenter::new(LatencyBudget::default());
        center.max_log_entries = 3;

        for i in 0..5 {
            center.record_action(ActionResult {
                action_id: format!("action-{i}"),
                success: true,
                message: String::new(),
                duration_ms: 10,
                interrupted: false,
                log_entries: Vec::new(),
            });
        }

        assert_eq!(center.action_log.len(), 3);
        assert_eq!(center.action_log[0].action_id, "action-2"); // oldest trimmed
    }

    #[test]
    fn command_center_latency_tracking() {
        let mut center = SwarmCommandCenter::new(LatencyBudget::default());

        let check = center.record_latency("palette_open", 30);
        assert!(check.within_budget);
        assert_eq!(center.telemetry.budget_violations, 0);

        let check = center.record_latency("palette_open", 200);
        assert!(!check.within_budget);
        assert_eq!(center.telemetry.budget_violations, 1);
    }

    #[test]
    fn command_center_snapshot() {
        let mut center = SwarmCommandCenter::new(LatencyBudget::default());
        register_standard_actions(&mut center.palette);

        center.live_view.panes = vec![
            make_pane("p1", PaneHealth::Healthy),
            make_pane("p2", PaneHealth::Degraded),
        ];

        let snap = center.snapshot();
        assert!(snap.registered_actions >= 15);
        assert_eq!(snap.total_panes, 2);
        assert_eq!(snap.healthy_panes, 1);
        assert_eq!(snap.degraded_panes, 1);
    }

    // ---- Serde ----

    #[test]
    fn command_center_serde_roundtrip() {
        let mut center = SwarmCommandCenter::new(LatencyBudget::default());
        register_standard_actions(&mut center.palette);

        let json = serde_json::to_string(&center).unwrap();
        let center2: SwarmCommandCenter = serde_json::from_str(&json).unwrap();
        assert_eq!(center2.palette.action_count(), center.palette.action_count());
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let center = SwarmCommandCenter::new(LatencyBudget::default());
        let snap = center.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let snap2: CommandCenterSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap2.operator_level, snap.operator_level);
    }

    // ---- E2E lifecycle ----

    #[test]
    fn e2e_command_center_workflow() {
        let mut center = SwarmCommandCenter::new(LatencyBudget::default());
        register_standard_actions(&mut center.palette);
        center.set_operator_level(OperatorLevel::SeniorOperator);

        // Open palette and search
        center.palette.open();
        assert!(center.palette.is_open);

        let results = center.palette.search("pause");
        assert!(!results.is_empty());
        assert_eq!(results[0].action_id, "fleet-pause");

        // Check availability
        let avail = center.check_action("fleet-pause", false);
        assert_eq!(avail, ActionAvailability::RequiresConfirmation);

        // Execute (simulate)
        center.record_action(ActionResult {
            action_id: "fleet-pause".into(),
            success: true,
            message: "All 48 panes paused".into(),
            duration_ms: 85,
            interrupted: false,
            log_entries: vec![ActionLogEntry {
                timestamp_ms: 1000,
                component: "fleet_controller".into(),
                correlation_id: "corr-001".into(),
                message: "Pause signal sent to all panes".into(),
                fields: BTreeMap::new(),
            }],
        });

        // Check latency
        let check = center.record_latency("action_feedback", 85);
        assert!(check.within_budget);

        // Update live view
        center.live_view.update_panes(
            vec![
                make_pane("p1", PaneHealth::Stopped),
                make_pane("p2", PaneHealth::Stopped),
            ],
            2000,
        );

        // Verify state
        let snap = center.snapshot();
        assert_eq!(snap.total_panes, 2);
        assert_eq!(snap.telemetry.actions_executed, 1);

        // Close palette
        center.palette.close();
        assert!(!center.palette.is_open);
    }

    #[test]
    fn e2e_policy_blocked_workflow() {
        let mut center = SwarmCommandCenter::new(LatencyBudget::default());
        register_standard_actions(&mut center.palette);
        center.set_operator_level(OperatorLevel::Operator);

        // Observer tries to quarantine (requires SeniorOperator)
        let avail = center.check_action("policy-quarantine", false);
        assert_eq!(avail, ActionAvailability::InsufficientPrivilege);
        center.telemetry.privilege_blocks += 1;

        // Even with sufficient level, policy can block
        center.set_operator_level(OperatorLevel::Admin);
        let avail = center.check_action("policy-quarantine", true);
        assert_eq!(avail, ActionAvailability::PolicyBlocked);
        center.telemetry.policy_blocks += 1;

        // Without policy block, destructive action needs confirmation
        let avail = center.check_action("policy-quarantine", false);
        assert_eq!(avail, ActionAvailability::RequiresConfirmation);
    }

    // ---- Helpers ----

    fn make_pane(id: &str, health: PaneHealth) -> PaneStatusRow {
        PaneStatusRow {
            pane_id: id.into(),
            title: format!("Pane {id}"),
            health,
            agent_id: None,
            last_activity_ms: 0,
            cpu_percent: 0.0,
            memory_mb: 0.0,
            event_rate: 0.0,
            alert_count: 0,
        }
    }
}
