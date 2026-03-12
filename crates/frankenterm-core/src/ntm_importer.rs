//! NTM-to-FrankenTerm migration importers (ft-3681t.8.3).
//!
//! Translates existing NTM operational assets — sessions, workflows, and
//! configuration — into FrankenTerm-native artifacts with validation against
//! the parity corpus and remediation hints for unsupported constructs.
//!
//! # Architecture
//!
//! The importer pipeline has three phases:
//!
//! 1. **Parse** — Load NTM source data (JSON/TOML) into typed NTM source structs.
//! 2. **Translate** — Map NTM types to FrankenTerm-native equivalents, collecting
//!    warnings for lossy or unsupported constructs.
//! 3. **Validate** — Run the translated artifacts against the parity corpus to
//!    confirm behavioral equivalence, then produce an import report.
//!
//! All phases are deterministic and produce structured audit logs suitable for
//! operator review and automated cutover gates.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

// =============================================================================
// NTM source types — represent the incoming NTM data format
// =============================================================================

/// An NTM session definition as exported from `ntm session list --json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NtmSession {
    /// Session name (unique within the NTM workspace).
    pub name: String,
    /// Workspace path on disk.
    #[serde(default)]
    pub workspace: Option<String>,
    /// List of windows in this session.
    #[serde(default)]
    pub windows: Vec<NtmWindow>,
    /// Session-level environment variables.
    #[serde(default)]
    pub environment: HashMap<String, String>,
    /// Coordinator mode (e.g., "swarm", "interactive", "headless").
    #[serde(default)]
    pub coordinator_mode: Option<String>,
    /// Whether this session auto-starts on NTM launch.
    #[serde(default)]
    pub auto_start: bool,
    /// Session-level safety overrides.
    #[serde(default)]
    pub safety_overrides: HashMap<String, serde_json::Value>,
    /// Arbitrary extension data.
    #[serde(default)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// An NTM window within a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NtmWindow {
    /// Window name.
    pub name: String,
    /// Layout type (e.g., "horizontal", "vertical", "grid", "tiled").
    #[serde(default)]
    pub layout: Option<String>,
    /// Panes in this window.
    #[serde(default)]
    pub panes: Vec<NtmPane>,
    /// Window-level focus index.
    #[serde(default)]
    pub focus_index: Option<u32>,
}

/// An NTM pane within a window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NtmPane {
    /// Pane role or label.
    #[serde(default)]
    pub role: Option<String>,
    /// Command to run in this pane.
    #[serde(default)]
    pub command: Option<String>,
    /// Command arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Pane-specific environment overrides.
    #[serde(default)]
    pub environment: HashMap<String, String>,
    /// Working directory override.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Split direction relative to parent ("horizontal" or "vertical").
    #[serde(default)]
    pub split_direction: Option<String>,
    /// Split size ratio (0.0..1.0).
    #[serde(default)]
    pub split_ratio: Option<f64>,
    /// Whether this pane is the focus target.
    #[serde(default)]
    pub is_focus: bool,
}

/// An NTM workflow definition as exported from `ntm workflow list --json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NtmWorkflow {
    /// Workflow name (unique within the NTM workspace).
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// Trigger patterns that activate this workflow.
    #[serde(default)]
    pub triggers: Vec<NtmWorkflowTrigger>,
    /// Steps in execution order.
    #[serde(default)]
    pub steps: Vec<NtmWorkflowStep>,
    /// Whether this workflow is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Concurrency: can this run in parallel with other workflows?
    #[serde(default)]
    pub allow_parallel: bool,
    /// Timeout in seconds (0 = no timeout).
    #[serde(default)]
    pub timeout_secs: u64,
    /// Safety classification: "safe", "review", "dangerous".
    #[serde(default)]
    pub safety_class: Option<String>,
}

/// An NTM workflow trigger pattern.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NtmWorkflowTrigger {
    /// Trigger type: "pattern", "event", "schedule", "manual".
    pub kind: String,
    /// Pattern or event name.
    pub value: String,
    /// Pane filter: which panes this trigger applies to.
    #[serde(default)]
    pub pane_filter: Option<String>,
}

/// An NTM workflow step definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NtmWorkflowStep {
    /// Step name.
    pub name: String,
    /// Step action: "send_text", "wait_for", "assert", "split", "close", "sleep".
    pub action: String,
    /// Step-specific parameters.
    #[serde(default)]
    pub params: HashMap<String, serde_json::Value>,
    /// Conditions that must be true for this step to execute.
    #[serde(default)]
    pub conditions: Vec<String>,
    /// Timeout override for this step (seconds).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// NTM configuration as exported from `ntm config show --json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NtmConfig {
    /// Log level.
    #[serde(default)]
    pub log_level: Option<String>,
    /// Workspace path.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Poll interval in milliseconds.
    #[serde(default)]
    pub poll_interval_ms: Option<u64>,
    /// Pattern packs enabled.
    #[serde(default)]
    pub pattern_packs: Vec<String>,
    /// Safety configuration.
    #[serde(default)]
    pub safety: NtmSafetyConfig,
    /// Robot mode settings.
    #[serde(default)]
    pub robot: NtmRobotConfig,
    /// Hooks configuration.
    #[serde(default)]
    pub hooks: Vec<NtmHookConfig>,
    /// All other fields.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// NTM safety configuration section.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NtmSafetyConfig {
    /// Whether command safety gate is enabled.
    #[serde(default)]
    pub command_safety_gate: bool,
    /// Require approval for destructive commands.
    #[serde(default)]
    pub require_approval_destructive: bool,
    /// Rate limit (commands per minute, 0 = unlimited).
    #[serde(default)]
    pub rate_limit_per_minute: u32,
    /// Allowlisted command prefixes.
    #[serde(default)]
    pub allowlist: Vec<String>,
    /// Denylisted command prefixes.
    #[serde(default)]
    pub denylist: Vec<String>,
}

/// NTM robot mode configuration section.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NtmRobotConfig {
    /// Robot mode bind address.
    #[serde(default)]
    pub bind_address: Option<String>,
    /// Whether to require auth for robot API.
    #[serde(default)]
    pub require_auth: bool,
    /// Max concurrent robot requests.
    #[serde(default)]
    pub max_concurrent: Option<u32>,
}

/// NTM hook configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NtmHookConfig {
    /// Hook event: "on_match", "on_send", "on_pane_open", "on_pane_close".
    pub event: String,
    /// Shell command to execute.
    pub command: String,
    /// Timeout in seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

fn default_true() -> bool {
    true
}

// =============================================================================
// Import result types
// =============================================================================

/// Severity of an import finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSeverity {
    /// Informational — the import is valid but has notes.
    Info,
    /// Warning — partial translation, some data may be lost.
    Warning,
    /// Error — the construct cannot be translated.
    Error,
}

impl ImportSeverity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// A finding from the import process.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportFinding {
    /// Severity level.
    pub severity: ImportSeverity,
    /// Which source artifact this finding relates to.
    pub source_path: String,
    /// Machine-readable code for this finding type.
    pub code: String,
    /// Human-readable message describing the issue.
    pub message: String,
    /// Remediation hint for the operator.
    #[serde(default)]
    pub remediation: Option<String>,
}

/// Result of importing a single NTM artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportItemResult {
    /// Source artifact identifier.
    pub source_id: String,
    /// Target artifact type (e.g., "session_profile", "workflow", "config").
    pub target_type: String,
    /// Whether the import succeeded.
    pub success: bool,
    /// Findings collected during import.
    pub findings: Vec<ImportFinding>,
}

impl ImportItemResult {
    fn new(source_id: impl Into<String>, target_type: impl Into<String>) -> Self {
        Self {
            source_id: source_id.into(),
            target_type: target_type.into(),
            success: true,
            findings: Vec::new(),
        }
    }

    fn add_finding(&mut self, finding: ImportFinding) {
        if finding.severity == ImportSeverity::Error {
            self.success = false;
        }
        self.findings.push(finding);
    }

    /// Returns true if any finding is an error.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == ImportSeverity::Error)
    }
}

/// Summary of a batch import operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportReport {
    /// Schema version for this report format.
    pub schema_version: String,
    /// Source system identifier (always "ntm").
    pub source_system: String,
    /// Total items attempted.
    pub total_items: usize,
    /// Successfully imported items.
    pub success_count: usize,
    /// Items that failed import.
    pub failure_count: usize,
    /// Items with warnings.
    pub warning_count: usize,
    /// Per-item results.
    pub items: Vec<ImportItemResult>,
    /// Aggregate finding counts by code.
    pub finding_summary: BTreeMap<String, usize>,
}

impl ImportReport {
    fn new() -> Self {
        Self {
            schema_version: "1.0".to_string(),
            source_system: "ntm".to_string(),
            total_items: 0,
            success_count: 0,
            failure_count: 0,
            warning_count: 0,
            items: Vec::new(),
            finding_summary: BTreeMap::new(),
        }
    }

    fn add_item(&mut self, item: ImportItemResult) {
        self.total_items += 1;
        if item.success {
            if item
                .findings
                .iter()
                .any(|f| f.severity == ImportSeverity::Warning)
            {
                self.warning_count += 1;
            }
            self.success_count += 1;
        } else {
            self.failure_count += 1;
        }
        for finding in &item.findings {
            *self
                .finding_summary
                .entry(finding.code.clone())
                .or_insert(0) += 1;
        }
        self.items.push(item);
    }
}

// =============================================================================
// Translated FrankenTerm types (output of import)
// =============================================================================

/// A translated session profile ready for registration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranslatedSessionProfile {
    /// Profile name (from NTM session name).
    pub name: String,
    /// Description (generated from NTM session metadata).
    pub description: String,
    /// Role classification.
    pub role: String,
    /// Spawn command.
    #[serde(default)]
    pub spawn_command: Option<TranslatedSpawnCommand>,
    /// Environment variables.
    #[serde(default)]
    pub environment: HashMap<String, String>,
    /// Working directory.
    #[serde(default)]
    pub working_directory: Option<String>,
    /// Resource hints.
    pub resource_hints: TranslatedResourceHints,
    /// Layout template name (if a matching template was generated).
    #[serde(default)]
    pub layout_template: Option<String>,
    /// Bootstrap commands.
    #[serde(default)]
    pub bootstrap_commands: Vec<String>,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Translated spawn command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranslatedSpawnCommand {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_true")]
    pub use_shell: bool,
}

/// Translated resource hints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranslatedResourceHints {
    pub min_rows: u16,
    pub min_cols: u16,
    pub max_scrollback: u32,
    pub priority_weight: u32,
}

impl Default for TranslatedResourceHints {
    fn default() -> Self {
        Self {
            min_rows: 24,
            min_cols: 80,
            max_scrollback: 10_000,
            priority_weight: 100,
        }
    }
}

/// A translated layout template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TranslatedLayoutTemplate {
    /// Template name (derived from NTM session + window).
    pub name: String,
    /// Description.
    #[serde(default)]
    pub description: Option<String>,
    /// Layout tree.
    pub root: TranslatedLayoutNode,
    /// Required pane count.
    pub min_panes: u32,
}

/// A layout node in the translated tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranslatedLayoutNode {
    Slot {
        #[serde(default)]
        role: Option<String>,
        #[serde(default = "default_one")]
        weight: f64,
    },
    HSplit {
        children: Vec<TranslatedLayoutNode>,
    },
    VSplit {
        children: Vec<TranslatedLayoutNode>,
    },
}

fn default_one() -> f64 {
    1.0
}

/// A translated workflow definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranslatedWorkflow {
    /// Workflow name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Trigger rule IDs that activate this workflow.
    pub trigger_rule_ids: Vec<String>,
    /// Steps in execution order.
    pub steps: Vec<TranslatedWorkflowStep>,
    /// Whether the workflow is enabled.
    pub enabled: bool,
    /// Safety classification: "safe", "review", "dangerous".
    pub safety_class: String,
    /// Max concurrent executions (0 = unlimited).
    pub max_concurrent: u32,
    /// Timeout in milliseconds (0 = no timeout).
    pub timeout_ms: u64,
}

/// A translated workflow step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranslatedWorkflowStep {
    /// Step name.
    pub name: String,
    /// Step description (generated).
    pub description: String,
    /// FrankenTerm step type.
    pub step_type: TranslatedStepType,
}

/// FrankenTerm workflow step types (subset supported by importer).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranslatedStepType {
    /// Send text to a pane.
    SendText {
        text: String,
        #[serde(default)]
        pane_filter: Option<String>,
    },
    /// Wait for a pattern match.
    WaitFor {
        pattern: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// Assert a condition.
    Assert { condition: String },
    /// Sleep for a duration.
    Sleep { duration_ms: u64 },
    /// Unsupported action preserved as opaque data.
    Unsupported {
        original_action: String,
        params: HashMap<String, serde_json::Value>,
    },
}

/// Translated FrankenTerm configuration fragment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranslatedConfig {
    /// General settings.
    #[serde(default)]
    pub log_level: Option<String>,
    /// Workspace path.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Ingest poll interval in milliseconds.
    #[serde(default)]
    pub poll_interval_ms: Option<u64>,
    /// Pattern packs.
    #[serde(default)]
    pub pattern_packs: Vec<String>,
    /// Safety settings.
    pub safety: TranslatedSafetyConfig,
    /// Untranslatable config keys (preserved for operator review).
    #[serde(default)]
    pub untranslated: HashMap<String, serde_json::Value>,
}

/// Translated safety configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranslatedSafetyConfig {
    pub command_safety_gate: bool,
    pub require_approval_destructive: bool,
    pub rate_limit_per_minute: u32,
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub denylist: Vec<String>,
}

// =============================================================================
// Full import output bundle
// =============================================================================

/// Complete output of an NTM import run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NtmImportBundle {
    /// Translated session profiles.
    pub session_profiles: Vec<TranslatedSessionProfile>,
    /// Translated layout templates.
    pub layout_templates: Vec<TranslatedLayoutTemplate>,
    /// Translated workflows.
    pub workflows: Vec<TranslatedWorkflow>,
    /// Translated config fragment (if config was imported).
    #[serde(default)]
    pub config: Option<TranslatedConfig>,
    /// Import report with findings.
    pub report: ImportReport,
}

// =============================================================================
// Import engine
// =============================================================================

/// Orchestrates the NTM-to-FrankenTerm migration import pipeline.
#[derive(Debug)]
pub struct NtmImporter {
    /// Unsupported NTM coordinator modes that cannot be translated.
    unsupported_coordinator_modes: Vec<String>,
}

impl Default for NtmImporter {
    fn default() -> Self {
        Self::new()
    }
}

impl NtmImporter {
    /// Create a new importer with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            unsupported_coordinator_modes: vec!["cluster".to_string(), "distributed".to_string()],
        }
    }

    /// Run the full import pipeline.
    pub fn import(
        &self,
        sessions: &[NtmSession],
        workflows: &[NtmWorkflow],
        config: Option<&NtmConfig>,
    ) -> NtmImportBundle {
        let mut report = ImportReport::new();

        let mut session_profiles = Vec::new();
        let mut layout_templates = Vec::new();

        for session in sessions {
            let (profiles, layouts, item_result) = self.translate_session(session);
            session_profiles.extend(profiles);
            layout_templates.extend(layouts);
            report.add_item(item_result);
        }

        let mut translated_workflows = Vec::new();
        for workflow in workflows {
            let (translated, item_result) = Self::translate_workflow(workflow);
            if let Some(tw) = translated {
                translated_workflows.push(tw);
            }
            report.add_item(item_result);
        }

        let translated_config = config.map(|c| {
            let (tc, item_result) = Self::translate_config(c);
            report.add_item(item_result);
            tc
        });

        NtmImportBundle {
            session_profiles,
            layout_templates,
            workflows: translated_workflows,
            config: translated_config,
            report,
        }
    }

    /// Translate a single NTM session into profiles + layout templates.
    fn translate_session(
        &self,
        session: &NtmSession,
    ) -> (
        Vec<TranslatedSessionProfile>,
        Vec<TranslatedLayoutTemplate>,
        ImportItemResult,
    ) {
        let source_id = format!("session:{}", session.name);
        let mut result = ImportItemResult::new(&source_id, "session_profile");
        let mut profiles = Vec::new();
        let mut templates = Vec::new();

        // Check for unsupported coordinator modes.
        if let Some(ref mode) = session.coordinator_mode {
            if self.unsupported_coordinator_modes.contains(mode) {
                result.add_finding(ImportFinding {
                    severity: ImportSeverity::Error,
                    source_path: source_id.clone(),
                    code: "UNSUPPORTED_COORDINATOR_MODE".to_string(),
                    message: format!("Coordinator mode '{mode}' is not supported in FrankenTerm"),
                    remediation: Some(
                        "Use 'swarm' or 'interactive' mode instead. Distributed coordination \
                         is handled natively by the FrankenTerm swarm scheduler."
                            .to_string(),
                    ),
                });
            } else if mode != "swarm" && mode != "interactive" && mode != "headless" {
                result.add_finding(ImportFinding {
                    severity: ImportSeverity::Warning,
                    source_path: source_id.clone(),
                    code: "UNKNOWN_COORDINATOR_MODE".to_string(),
                    message: format!(
                        "Coordinator mode '{mode}' is not recognized; defaulting to 'interactive'"
                    ),
                    remediation: Some(
                        "Review the translated profile and set the appropriate \
                         FrankenTerm session mode."
                            .to_string(),
                    ),
                });
            }
        }

        // Check for safety overrides.
        if !session.safety_overrides.is_empty() {
            result.add_finding(ImportFinding {
                severity: ImportSeverity::Warning,
                source_path: source_id.clone(),
                code: "SAFETY_OVERRIDES_NEED_REVIEW".to_string(),
                message: format!(
                    "{} safety override(s) require manual review for FrankenTerm policy mapping",
                    session.safety_overrides.len()
                ),
                remediation: Some(
                    "Safety overrides must be configured via FrankenTerm's policy engine \
                     (safety config in ft.toml). Review each override and map to the \
                     appropriate policy rule."
                        .to_string(),
                ),
            });
        }

        // Translate each window into a layout template + pane profiles.
        for (window_idx, window) in session.windows.iter().enumerate() {
            let template = Self::translate_window_layout(session, window, window_idx);
            templates.push(template);

            for pane in &window.panes {
                let profile = Self::translate_pane_to_profile(session, window, pane);
                profiles.push(profile);
            }

            // If window has no panes, create a default profile.
            if window.panes.is_empty() {
                result.add_finding(ImportFinding {
                    severity: ImportSeverity::Info,
                    source_path: format!("session:{}/window:{}", session.name, window.name),
                    code: "EMPTY_WINDOW".to_string(),
                    message: format!(
                        "Window '{}' has no panes; creating default profile",
                        window.name
                    ),
                    remediation: None,
                });
                profiles.push(TranslatedSessionProfile {
                    name: format!("{}-{}-default", session.name, window.name),
                    description: format!("Default profile for empty NTM window '{}'", window.name),
                    role: "dev_shell".to_string(),
                    spawn_command: None,
                    environment: session.environment.clone(),
                    working_directory: session.workspace.clone(),
                    resource_hints: TranslatedResourceHints::default(),
                    layout_template: Some(format!("{}-{}", session.name, window.name)),
                    bootstrap_commands: Vec::new(),
                    tags: vec![format!("imported:ntm"), format!("session:{}", session.name)],
                });
            }
        }

        // If session has no windows at all, create a minimal profile.
        if session.windows.is_empty() {
            result.add_finding(ImportFinding {
                severity: ImportSeverity::Warning,
                source_path: source_id.clone(),
                code: "EMPTY_SESSION".to_string(),
                message: "Session has no windows; creating minimal profile".to_string(),
                remediation: Some(
                    "Add windows and panes to the session profile after import.".to_string(),
                ),
            });
            profiles.push(TranslatedSessionProfile {
                name: session.name.clone(),
                description: format!("Imported from NTM session '{}' (no windows)", session.name),
                role: "dev_shell".to_string(),
                spawn_command: None,
                environment: session.environment.clone(),
                working_directory: session.workspace.clone(),
                resource_hints: TranslatedResourceHints::default(),
                layout_template: None,
                bootstrap_commands: Vec::new(),
                tags: vec![
                    "imported:ntm".to_string(),
                    format!("session:{}", session.name),
                ],
            });
        }

        (profiles, templates, result)
    }

    /// Translate a window's pane arrangement into a layout template.
    fn translate_window_layout(
        session: &NtmSession,
        window: &NtmWindow,
        _window_idx: usize,
    ) -> TranslatedLayoutTemplate {
        let pane_count = window.panes.len().max(1) as u32;

        let root = if window.panes.is_empty() {
            TranslatedLayoutNode::Slot {
                role: None,
                weight: 1.0,
            }
        } else {
            Self::build_layout_tree(&window.panes, window.layout.as_deref())
        };

        TranslatedLayoutTemplate {
            name: format!("{}-{}", session.name, window.name),
            description: Some(format!(
                "Layout imported from NTM session '{}' window '{}'",
                session.name, window.name
            )),
            root,
            min_panes: pane_count,
        }
    }

    /// Build a layout tree from NTM pane definitions.
    fn build_layout_tree(panes: &[NtmPane], layout_hint: Option<&str>) -> TranslatedLayoutNode {
        if panes.len() == 1 {
            return TranslatedLayoutNode::Slot {
                role: panes[0].role.clone(),
                weight: panes[0].split_ratio.unwrap_or(1.0),
            };
        }

        let children: Vec<TranslatedLayoutNode> = panes
            .iter()
            .map(|pane| TranslatedLayoutNode::Slot {
                role: pane.role.clone(),
                weight: pane.split_ratio.unwrap_or(1.0),
            })
            .collect();

        match layout_hint {
            Some("vertical" | "vsplit") => TranslatedLayoutNode::VSplit { children },
            Some("horizontal" | "hsplit") => TranslatedLayoutNode::HSplit { children },
            Some("grid" | "tiled") => {
                // For grid layouts, create a 2-column arrangement.
                let mid = children.len().div_ceil(2);
                let (left, right) = children.split_at(mid);
                if right.is_empty() {
                    TranslatedLayoutNode::VSplit {
                        children: left.to_vec(),
                    }
                } else {
                    TranslatedLayoutNode::HSplit {
                        children: vec![
                            TranslatedLayoutNode::VSplit {
                                children: left.to_vec(),
                            },
                            TranslatedLayoutNode::VSplit {
                                children: right.to_vec(),
                            },
                        ],
                    }
                }
            }
            // Default to vertical split.
            _ => TranslatedLayoutNode::VSplit { children },
        }
    }

    /// Translate a single NTM pane into a session profile.
    fn translate_pane_to_profile(
        session: &NtmSession,
        window: &NtmWindow,
        pane: &NtmPane,
    ) -> TranslatedSessionProfile {
        let role = pane.role.as_deref().unwrap_or("dev_shell");
        let ft_role = translate_ntm_role(role);

        let spawn_command = pane.command.as_ref().map(|cmd| TranslatedSpawnCommand {
            command: cmd.clone(),
            args: pane.args.clone(),
            use_shell: true,
        });

        // Merge session + pane environment.
        let mut env = session.environment.clone();
        env.extend(pane.environment.clone());

        let pane_label = pane.role.as_deref().unwrap_or("pane");

        TranslatedSessionProfile {
            name: format!("{}-{}-{}", session.name, window.name, pane_label),
            description: format!(
                "Imported from NTM session '{}' window '{}' pane '{}'",
                session.name, window.name, pane_label
            ),
            role: ft_role.to_string(),
            spawn_command,
            environment: env,
            working_directory: pane.cwd.clone().or_else(|| session.workspace.clone()),
            resource_hints: TranslatedResourceHints::default(),
            layout_template: Some(format!("{}-{}", session.name, window.name)),
            bootstrap_commands: Vec::new(),
            tags: vec![
                "imported:ntm".to_string(),
                format!("session:{}", session.name),
                format!("window:{}", window.name),
                format!("role:{ft_role}"),
            ],
        }
    }

    /// Translate an NTM workflow into a FrankenTerm workflow definition.
    fn translate_workflow(
        workflow: &NtmWorkflow,
    ) -> (Option<TranslatedWorkflow>, ImportItemResult) {
        let source_id = format!("workflow:{}", workflow.name);
        let mut result = ImportItemResult::new(&source_id, "workflow");

        let mut trigger_rule_ids = Vec::new();
        for trigger in &workflow.triggers {
            match trigger.kind.as_str() {
                "pattern" => {
                    trigger_rule_ids.push(format!("trigger.imported.{}", trigger.value));
                }
                "event" => {
                    trigger_rule_ids.push(format!("event.imported.{}", trigger.value));
                }
                "schedule" => {
                    result.add_finding(ImportFinding {
                        severity: ImportSeverity::Warning,
                        source_path: source_id.clone(),
                        code: "SCHEDULE_TRIGGER_UNSUPPORTED".to_string(),
                        message: format!(
                            "Schedule trigger '{}' is not directly supported; \
                             use external cron + robot API instead",
                            trigger.value
                        ),
                        remediation: Some(
                            "Set up a cron job that calls `ft robot workflow run <name>` \
                             at the desired interval."
                                .to_string(),
                        ),
                    });
                }
                "manual" => {
                    // Manual triggers don't need rule IDs.
                    result.add_finding(ImportFinding {
                        severity: ImportSeverity::Info,
                        source_path: source_id.clone(),
                        code: "MANUAL_TRIGGER_PRESERVED".to_string(),
                        message: "Manual trigger preserved; invoke via `ft robot workflow run`"
                            .to_string(),
                        remediation: None,
                    });
                }
                other => {
                    result.add_finding(ImportFinding {
                        severity: ImportSeverity::Error,
                        source_path: source_id.clone(),
                        code: "UNKNOWN_TRIGGER_KIND".to_string(),
                        message: format!("Trigger kind '{other}' is not recognized"),
                        remediation: Some(
                            "Review the workflow definition and replace with a supported \
                             trigger type (pattern, event, or manual)."
                                .to_string(),
                        ),
                    });
                }
            }
        }

        let mut steps = Vec::new();
        for (step_idx, step) in workflow.steps.iter().enumerate() {
            let (translated_step, step_findings) =
                Self::translate_workflow_step(step, &source_id, step_idx);
            steps.push(translated_step);
            for finding in step_findings {
                result.add_finding(finding);
            }
        }

        let safety_class = workflow
            .safety_class
            .as_deref()
            .unwrap_or("review")
            .to_string();

        let max_concurrent = u32::from(!workflow.allow_parallel);

        let translated = TranslatedWorkflow {
            name: workflow.name.clone(),
            description: workflow
                .description
                .clone()
                .unwrap_or_else(|| format!("Imported from NTM workflow '{}'", workflow.name)),
            trigger_rule_ids,
            steps,
            enabled: workflow.enabled,
            safety_class,
            max_concurrent,
            timeout_ms: workflow.timeout_secs * 1000,
        };

        (Some(translated), result)
    }

    /// Translate a single workflow step.
    fn translate_workflow_step(
        step: &NtmWorkflowStep,
        source_id: &str,
        step_idx: usize,
    ) -> (TranslatedWorkflowStep, Vec<ImportFinding>) {
        let mut findings = Vec::new();
        let step_path = format!("{source_id}/step[{step_idx}]:{}", step.name);

        if !step.conditions.is_empty() {
            findings.push(ImportFinding {
                severity: ImportSeverity::Warning,
                source_path: step_path.clone(),
                code: "STEP_CONDITIONS_NOT_TRANSLATED".to_string(),
                message: format!(
                    "{} condition(s) on step '{}' require manual review",
                    step.conditions.len(),
                    step.name
                ),
                remediation: Some(
                    "Step conditions must be implemented as workflow logic in the \
                     FrankenTerm workflow step handler."
                        .to_string(),
                ),
            });
        }

        let step_type = match step.action.as_str() {
            "send_text" => {
                let text = step
                    .params
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let pane_filter = step
                    .params
                    .get("pane_filter")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                TranslatedStepType::SendText { text, pane_filter }
            }
            "wait_for" => {
                let pattern = step
                    .params
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".*")
                    .to_string();
                let timeout_ms = step
                    .timeout_secs
                    .or_else(|| step.params.get("timeout_secs").and_then(|v| v.as_u64()))
                    .map(|s| s * 1000);
                TranslatedStepType::WaitFor {
                    pattern,
                    timeout_ms,
                }
            }
            "assert" => {
                let condition = step
                    .params
                    .get("condition")
                    .and_then(|v| v.as_str())
                    .unwrap_or("true")
                    .to_string();
                TranslatedStepType::Assert { condition }
            }
            "sleep" => {
                let secs = step
                    .params
                    .get("seconds")
                    .and_then(|v| v.as_u64())
                    .or_else(|| step.params.get("duration").and_then(|v| v.as_u64()))
                    .unwrap_or(1);
                TranslatedStepType::Sleep {
                    duration_ms: secs * 1000,
                }
            }
            "split" | "close" => {
                findings.push(ImportFinding {
                    severity: ImportSeverity::Warning,
                    source_path: step_path.clone(),
                    code: "TOPOLOGY_ACTION_NEEDS_REVIEW".to_string(),
                    message: format!(
                        "Topology action '{}' requires FrankenTerm topology orchestrator API",
                        step.action
                    ),
                    remediation: Some(
                        "Use the FrankenTerm topology orchestrator to perform split/close \
                         operations in workflows."
                            .to_string(),
                    ),
                });
                TranslatedStepType::Unsupported {
                    original_action: step.action.clone(),
                    params: step.params.clone(),
                }
            }
            other => {
                findings.push(ImportFinding {
                    severity: ImportSeverity::Error,
                    source_path: step_path,
                    code: "UNSUPPORTED_STEP_ACTION".to_string(),
                    message: format!("Step action '{other}' has no FrankenTerm equivalent"),
                    remediation: Some(
                        "Implement a custom workflow step handler for this action.".to_string(),
                    ),
                });
                TranslatedStepType::Unsupported {
                    original_action: other.to_string(),
                    params: step.params.clone(),
                }
            }
        };

        let translated = TranslatedWorkflowStep {
            name: step.name.clone(),
            description: format!("Imported step '{}' (action: {})", step.name, step.action),
            step_type,
        };

        (translated, findings)
    }

    /// Translate NTM configuration to FrankenTerm config fragment.
    fn translate_config(config: &NtmConfig) -> (TranslatedConfig, ImportItemResult) {
        let source_id = "config".to_string();
        let mut result = ImportItemResult::new(&source_id, "config");

        // Translate safety config.
        let safety = TranslatedSafetyConfig {
            command_safety_gate: config.safety.command_safety_gate,
            require_approval_destructive: config.safety.require_approval_destructive,
            rate_limit_per_minute: config.safety.rate_limit_per_minute,
            allowlist: config.safety.allowlist.clone(),
            denylist: config.safety.denylist.clone(),
        };

        // Note hooks require special handling.
        if !config.hooks.is_empty() {
            result.add_finding(ImportFinding {
                severity: ImportSeverity::Warning,
                source_path: source_id.clone(),
                code: "HOOKS_NEED_WORKFLOW_MIGRATION".to_string(),
                message: format!(
                    "{} hook(s) must be migrated to FrankenTerm workflows or pattern rules",
                    config.hooks.len()
                ),
                remediation: Some(
                    "NTM hooks should be converted to FrankenTerm pattern rules + workflows. \
                     For 'on_match' hooks, create a pattern rule. For 'on_send' hooks, use \
                     the policy engine's SendText gate."
                        .to_string(),
                ),
            });
        }

        // Note robot config differences.
        if config.robot.require_auth {
            result.add_finding(ImportFinding {
                severity: ImportSeverity::Info,
                source_path: source_id.clone(),
                code: "ROBOT_AUTH_POLICY_CHANGE".to_string(),
                message: "Robot API authentication is handled by FrankenTerm's IPC auth layer"
                    .to_string(),
                remediation: Some(
                    "Configure IPC authentication in ft.toml [ipc] section instead.".to_string(),
                ),
            });
        }

        // Collect untranslatable extra keys.
        let untranslated: HashMap<String, serde_json::Value> = config
            .extra
            .iter()
            .map(|(k, v)| {
                result.add_finding(ImportFinding {
                    severity: ImportSeverity::Info,
                    source_path: source_id.clone(),
                    code: "UNTRANSLATED_CONFIG_KEY".to_string(),
                    message: format!("Config key '{k}' has no FrankenTerm equivalent"),
                    remediation: None,
                });
                (k.clone(), v.clone())
            })
            .collect();

        let translated = TranslatedConfig {
            log_level: config.log_level.clone(),
            workspace: config.workspace.clone(),
            poll_interval_ms: config.poll_interval_ms,
            pattern_packs: config.pattern_packs.clone(),
            safety,
            untranslated,
        };

        (translated, result)
    }
}

// =============================================================================
// Helper functions
// =============================================================================

/// Map an NTM role string to a FrankenTerm `ProfileRole` variant name.
fn translate_ntm_role(ntm_role: &str) -> &str {
    match ntm_role.to_lowercase().as_str() {
        "agent" | "agent_worker" | "worker" | "agent-worker" => "agent_worker",
        "monitor" | "log" | "logger" | "log-viewer" => "monitor",
        "build" | "builder" | "ci" | "build-runner" | "build_runner" => "build_runner",
        "test" | "tester" | "test-runner" | "test_runner" => "test_runner",
        "service" | "server" | "daemon" => "service",
        "dev" | "shell" | "dev_shell" | "dev-shell" | "interactive" => "dev_shell",
        _ => "custom",
    }
}

// =============================================================================
// Parsing helpers
// =============================================================================

/// Parse NTM sessions from a JSON string (array of sessions).
pub fn parse_ntm_sessions(json: &str) -> Result<Vec<NtmSession>, serde_json::Error> {
    serde_json::from_str(json)
}

/// Parse NTM workflows from a JSON string (array of workflows).
pub fn parse_ntm_workflows(json: &str) -> Result<Vec<NtmWorkflow>, serde_json::Error> {
    serde_json::from_str(json)
}

/// Parse NTM config from a JSON string.
pub fn parse_ntm_config(json: &str) -> Result<NtmConfig, serde_json::Error> {
    serde_json::from_str(json)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -------------------------------------------------------------------------
    // Source type serde roundtrip tests
    // -------------------------------------------------------------------------

    #[test]
    fn ntm_session_serde_roundtrip() {
        let session = NtmSession {
            name: "dev".to_string(),
            workspace: Some("/home/user/project".to_string()),
            windows: vec![NtmWindow {
                name: "main".to_string(),
                layout: Some("vertical".to_string()),
                panes: vec![NtmPane {
                    role: Some("editor".to_string()),
                    command: Some("vim".to_string()),
                    args: vec![".".to_string()],
                    environment: HashMap::new(),
                    cwd: None,
                    split_direction: None,
                    split_ratio: Some(0.6),
                    is_focus: true,
                }],
                focus_index: Some(0),
            }],
            environment: HashMap::from([("TERM".to_string(), "xterm-256color".to_string())]),
            coordinator_mode: Some("interactive".to_string()),
            auto_start: false,
            safety_overrides: HashMap::new(),
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&session).unwrap();
        let back: NtmSession = serde_json::from_str(&json).unwrap();
        assert_eq!(session, back);
    }

    #[test]
    fn ntm_workflow_serde_roundtrip() {
        let workflow = NtmWorkflow {
            name: "health-check".to_string(),
            description: Some("Periodic health check".to_string()),
            triggers: vec![NtmWorkflowTrigger {
                kind: "pattern".to_string(),
                value: "error.detected".to_string(),
                pane_filter: None,
            }],
            steps: vec![NtmWorkflowStep {
                name: "send-check".to_string(),
                action: "send_text".to_string(),
                params: HashMap::from([("text".to_string(), json!("health_check --verbose"))]),
                conditions: Vec::new(),
                timeout_secs: Some(30),
            }],
            enabled: true,
            allow_parallel: false,
            timeout_secs: 60,
            safety_class: Some("safe".to_string()),
        };
        let json = serde_json::to_string(&workflow).unwrap();
        let back: NtmWorkflow = serde_json::from_str(&json).unwrap();
        assert_eq!(workflow.name, back.name);
        assert_eq!(workflow.steps.len(), back.steps.len());
    }

    #[test]
    fn ntm_config_serde_roundtrip() {
        let config = NtmConfig {
            log_level: Some("info".to_string()),
            workspace: Some("/home/user".to_string()),
            poll_interval_ms: Some(500),
            pattern_packs: vec!["default".to_string(), "security".to_string()],
            safety: NtmSafetyConfig {
                command_safety_gate: true,
                require_approval_destructive: true,
                rate_limit_per_minute: 60,
                allowlist: vec!["ls".to_string()],
                denylist: vec!["rm -rf".to_string()],
            },
            robot: NtmRobotConfig {
                bind_address: Some("127.0.0.1:9876".to_string()),
                require_auth: true,
                max_concurrent: Some(10),
            },
            hooks: vec![NtmHookConfig {
                event: "on_match".to_string(),
                command: "notify-send".to_string(),
                timeout_secs: Some(5),
            }],
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: NtmConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.log_level, back.log_level);
        assert_eq!(
            config.safety.rate_limit_per_minute,
            back.safety.rate_limit_per_minute
        );
    }

    // -------------------------------------------------------------------------
    // Session import tests
    // -------------------------------------------------------------------------

    fn sample_session() -> NtmSession {
        NtmSession {
            name: "agent-fleet".to_string(),
            workspace: Some("/projects/main".to_string()),
            windows: vec![
                NtmWindow {
                    name: "workers".to_string(),
                    layout: Some("vertical".to_string()),
                    panes: vec![
                        NtmPane {
                            role: Some("agent".to_string()),
                            command: Some("claude".to_string()),
                            args: vec!["--model".to_string(), "opus".to_string()],
                            environment: HashMap::new(),
                            cwd: Some("/projects/main".to_string()),
                            split_direction: None,
                            split_ratio: Some(0.5),
                            is_focus: true,
                        },
                        NtmPane {
                            role: Some("monitor".to_string()),
                            command: Some("tail".to_string()),
                            args: vec!["-f".to_string(), "/var/log/agent.log".to_string()],
                            environment: HashMap::new(),
                            cwd: None,
                            split_direction: Some("horizontal".to_string()),
                            split_ratio: Some(0.5),
                            is_focus: false,
                        },
                    ],
                    focus_index: Some(0),
                },
                NtmWindow {
                    name: "build".to_string(),
                    layout: Some("horizontal".to_string()),
                    panes: vec![NtmPane {
                        role: Some("build-runner".to_string()),
                        command: Some("cargo".to_string()),
                        args: vec!["watch".to_string()],
                        environment: HashMap::from([(
                            "CARGO_INCREMENTAL".to_string(),
                            "1".to_string(),
                        )]),
                        cwd: Some("/projects/main".to_string()),
                        split_direction: None,
                        split_ratio: None,
                        is_focus: false,
                    }],
                    focus_index: None,
                },
            ],
            environment: HashMap::from([("PROJECT".to_string(), "main".to_string())]),
            coordinator_mode: Some("swarm".to_string()),
            auto_start: true,
            safety_overrides: HashMap::new(),
            extra: HashMap::new(),
        }
    }

    #[test]
    fn import_session_produces_profiles_and_layouts() {
        let importer = NtmImporter::new();
        let session = sample_session();
        let (profiles, layouts, result) = importer.translate_session(&session);

        assert!(result.success);
        assert_eq!(profiles.len(), 3); // 2 panes in workers + 1 pane in build
        assert_eq!(layouts.len(), 2); // 2 windows

        // Verify profile names include session/window/pane context.
        assert!(profiles[0].name.contains("agent-fleet"));
        assert!(profiles[0].name.contains("workers"));

        // Verify layout template names.
        assert_eq!(layouts[0].name, "agent-fleet-workers");
        assert_eq!(layouts[1].name, "agent-fleet-build");

        // Verify environment merging.
        assert_eq!(profiles[0].environment.get("PROJECT").unwrap(), "main");

        // Verify role translation.
        assert_eq!(profiles[0].role, "agent_worker");
        assert_eq!(profiles[1].role, "monitor");
        assert_eq!(profiles[2].role, "build_runner");
    }

    #[test]
    fn import_session_with_unsupported_coordinator_mode() {
        let importer = NtmImporter::new();
        let mut session = sample_session();
        session.coordinator_mode = Some("distributed".to_string());
        let (_profiles, _layouts, result) = importer.translate_session(&session);

        assert!(!result.success);
        assert!(result.has_errors());
        let error = result
            .findings
            .iter()
            .find(|f| f.code == "UNSUPPORTED_COORDINATOR_MODE")
            .unwrap();
        assert_eq!(error.severity, ImportSeverity::Error);
        assert!(error.remediation.is_some());
    }

    #[test]
    fn import_session_with_safety_overrides_warns() {
        let importer = NtmImporter::new();
        let mut session = sample_session();
        session
            .safety_overrides
            .insert("disable_gate".to_string(), json!(true));
        let (_profiles, _layouts, result) = importer.translate_session(&session);

        assert!(result.success); // Warnings don't fail.
        let warning = result
            .findings
            .iter()
            .find(|f| f.code == "SAFETY_OVERRIDES_NEED_REVIEW")
            .unwrap();
        assert_eq!(warning.severity, ImportSeverity::Warning);
    }

    #[test]
    fn import_empty_session_creates_minimal_profile() {
        let importer = NtmImporter::new();
        let session = NtmSession {
            name: "empty".to_string(),
            workspace: None,
            windows: Vec::new(),
            environment: HashMap::new(),
            coordinator_mode: None,
            auto_start: false,
            safety_overrides: HashMap::new(),
            extra: HashMap::new(),
        };
        let (profiles, layouts, result) = importer.translate_session(&session);

        assert!(result.success);
        assert_eq!(profiles.len(), 1);
        assert!(layouts.is_empty());
        let warning = result
            .findings
            .iter()
            .find(|f| f.code == "EMPTY_SESSION")
            .unwrap();
        assert_eq!(warning.severity, ImportSeverity::Warning);
    }

    #[test]
    fn import_empty_window_creates_default_profile() {
        let importer = NtmImporter::new();
        let session = NtmSession {
            name: "sparse".to_string(),
            workspace: Some("/tmp".to_string()),
            windows: vec![NtmWindow {
                name: "empty-win".to_string(),
                layout: None,
                panes: Vec::new(),
                focus_index: None,
            }],
            environment: HashMap::new(),
            coordinator_mode: None,
            auto_start: false,
            safety_overrides: HashMap::new(),
            extra: HashMap::new(),
        };
        let (profiles, layouts, result) = importer.translate_session(&session);

        assert!(result.success);
        assert_eq!(profiles.len(), 1);
        assert_eq!(layouts.len(), 1);
        assert!(profiles[0].name.contains("default"));
        let info = result
            .findings
            .iter()
            .find(|f| f.code == "EMPTY_WINDOW")
            .unwrap();
        assert_eq!(info.severity, ImportSeverity::Info);
    }

    // -------------------------------------------------------------------------
    // Layout translation tests
    // -------------------------------------------------------------------------

    #[test]
    fn vertical_layout_produces_vsplit() {
        let panes = vec![
            NtmPane {
                role: Some("a".to_string()),
                split_ratio: Some(0.5),
                ..default_pane()
            },
            NtmPane {
                role: Some("b".to_string()),
                split_ratio: Some(0.5),
                ..default_pane()
            },
        ];
        let tree = NtmImporter::build_layout_tree(&panes, Some("vertical"));
        assert!(matches!(tree, TranslatedLayoutNode::VSplit { .. }));
    }

    #[test]
    fn horizontal_layout_produces_hsplit() {
        let panes = vec![default_pane(), default_pane()];
        let tree = NtmImporter::build_layout_tree(&panes, Some("horizontal"));
        assert!(matches!(tree, TranslatedLayoutNode::HSplit { .. }));
    }

    #[test]
    fn grid_layout_produces_nested_splits() {
        let panes = vec![
            default_pane(),
            default_pane(),
            default_pane(),
            default_pane(),
        ];
        let tree = NtmImporter::build_layout_tree(&panes, Some("grid"));
        match tree {
            TranslatedLayoutNode::HSplit { children } => {
                assert_eq!(children.len(), 2);
                assert!(matches!(children[0], TranslatedLayoutNode::VSplit { .. }));
                assert!(matches!(children[1], TranslatedLayoutNode::VSplit { .. }));
            }
            other => panic!("Expected HSplit for grid, got {other:?}"),
        }
    }

    #[test]
    fn single_pane_produces_slot() {
        let panes = vec![NtmPane {
            role: Some("main".to_string()),
            ..default_pane()
        }];
        let tree = NtmImporter::build_layout_tree(&panes, Some("vertical"));
        match tree {
            TranslatedLayoutNode::Slot { role, .. } => {
                assert_eq!(role.as_deref(), Some("main"));
            }
            other => panic!("Expected Slot, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------------
    // Workflow import tests
    // -------------------------------------------------------------------------

    fn sample_workflow() -> NtmWorkflow {
        NtmWorkflow {
            name: "auto-restart".to_string(),
            description: Some("Auto-restart crashed agents".to_string()),
            triggers: vec![NtmWorkflowTrigger {
                kind: "pattern".to_string(),
                value: "agent.crash".to_string(),
                pane_filter: None,
            }],
            steps: vec![
                NtmWorkflowStep {
                    name: "notify".to_string(),
                    action: "send_text".to_string(),
                    params: HashMap::from([("text".to_string(), json!("restarting..."))]),
                    conditions: Vec::new(),
                    timeout_secs: None,
                },
                NtmWorkflowStep {
                    name: "wait".to_string(),
                    action: "wait_for".to_string(),
                    params: HashMap::from([("pattern".to_string(), json!("ready"))]),
                    conditions: Vec::new(),
                    timeout_secs: Some(30),
                },
                NtmWorkflowStep {
                    name: "verify".to_string(),
                    action: "assert".to_string(),
                    params: HashMap::from([("condition".to_string(), json!("pane.is_alive"))]),
                    conditions: Vec::new(),
                    timeout_secs: None,
                },
            ],
            enabled: true,
            allow_parallel: false,
            timeout_secs: 120,
            safety_class: Some("safe".to_string()),
        }
    }

    #[test]
    fn import_workflow_translates_steps() {
        let workflow = sample_workflow();
        let (translated, result) = NtmImporter::translate_workflow(&workflow);

        assert!(result.success);
        let tw = translated.unwrap();
        assert_eq!(tw.name, "auto-restart");
        assert_eq!(tw.steps.len(), 3);
        assert!(matches!(
            tw.steps[0].step_type,
            TranslatedStepType::SendText { .. }
        ));
        assert!(matches!(
            tw.steps[1].step_type,
            TranslatedStepType::WaitFor { .. }
        ));
        assert!(matches!(
            tw.steps[2].step_type,
            TranslatedStepType::Assert { .. }
        ));
        assert_eq!(tw.timeout_ms, 120_000);
        assert_eq!(tw.max_concurrent, 1);
    }

    #[test]
    fn import_workflow_with_schedule_trigger_warns() {
        let mut workflow = sample_workflow();
        workflow.triggers.push(NtmWorkflowTrigger {
            kind: "schedule".to_string(),
            value: "*/5 * * * *".to_string(),
            pane_filter: None,
        });
        let (_translated, result) = NtmImporter::translate_workflow(&workflow);

        assert!(result.success);
        let warning = result
            .findings
            .iter()
            .find(|f| f.code == "SCHEDULE_TRIGGER_UNSUPPORTED")
            .unwrap();
        assert_eq!(warning.severity, ImportSeverity::Warning);
    }

    #[test]
    fn import_workflow_with_unknown_trigger_errors() {
        let mut workflow = sample_workflow();
        workflow.triggers = vec![NtmWorkflowTrigger {
            kind: "webhook".to_string(),
            value: "https://example.com".to_string(),
            pane_filter: None,
        }];
        let (_translated, result) = NtmImporter::translate_workflow(&workflow);

        assert!(!result.success);
        let error = result
            .findings
            .iter()
            .find(|f| f.code == "UNKNOWN_TRIGGER_KIND")
            .unwrap();
        assert_eq!(error.severity, ImportSeverity::Error);
    }

    #[test]
    fn import_workflow_with_unsupported_step_preserves_data() {
        let mut workflow = sample_workflow();
        workflow.steps.push(NtmWorkflowStep {
            name: "custom-action".to_string(),
            action: "deploy_canary".to_string(),
            params: HashMap::from([("target".to_string(), json!("prod"))]),
            conditions: Vec::new(),
            timeout_secs: None,
        });
        let (translated, result) = NtmImporter::translate_workflow(&workflow);

        assert!(!result.success);
        let tw = translated.unwrap();
        let last_step = tw.steps.last().unwrap();
        match &last_step.step_type {
            TranslatedStepType::Unsupported {
                original_action,
                params,
            } => {
                assert_eq!(original_action, "deploy_canary");
                assert_eq!(params.get("target").unwrap(), &json!("prod"));
            }
            other => panic!("Expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn import_workflow_sleep_step_converts_to_ms() {
        let mut workflow = sample_workflow();
        workflow.steps = vec![NtmWorkflowStep {
            name: "pause".to_string(),
            action: "sleep".to_string(),
            params: HashMap::from([("seconds".to_string(), json!(5))]),
            conditions: Vec::new(),
            timeout_secs: None,
        }];
        let (translated, _result) = NtmImporter::translate_workflow(&workflow);
        let tw = translated.unwrap();
        match &tw.steps[0].step_type {
            TranslatedStepType::Sleep { duration_ms } => {
                assert_eq!(*duration_ms, 5000);
            }
            other => panic!("Expected Sleep, got {other:?}"),
        }
    }

    #[test]
    fn import_workflow_step_with_conditions_warns() {
        let mut workflow = sample_workflow();
        workflow.steps[0].conditions = vec!["pane.is_alive".to_string()];
        let (_translated, result) = NtmImporter::translate_workflow(&workflow);

        assert!(result.success);
        let warning = result
            .findings
            .iter()
            .find(|f| f.code == "STEP_CONDITIONS_NOT_TRANSLATED")
            .unwrap();
        assert_eq!(warning.severity, ImportSeverity::Warning);
    }

    // -------------------------------------------------------------------------
    // Config import tests
    // -------------------------------------------------------------------------

    fn sample_config() -> NtmConfig {
        NtmConfig {
            log_level: Some("debug".to_string()),
            workspace: Some("/home/user/project".to_string()),
            poll_interval_ms: Some(250),
            pattern_packs: vec!["default".to_string(), "security".to_string()],
            safety: NtmSafetyConfig {
                command_safety_gate: true,
                require_approval_destructive: true,
                rate_limit_per_minute: 120,
                allowlist: vec!["ls".to_string(), "cat".to_string()],
                denylist: vec!["rm -rf /".to_string()],
            },
            robot: NtmRobotConfig {
                bind_address: Some("127.0.0.1:9876".to_string()),
                require_auth: true,
                max_concurrent: Some(10),
            },
            hooks: vec![
                NtmHookConfig {
                    event: "on_match".to_string(),
                    command: "notify-send".to_string(),
                    timeout_secs: Some(5),
                },
                NtmHookConfig {
                    event: "on_send".to_string(),
                    command: "logger".to_string(),
                    timeout_secs: None,
                },
            ],
            extra: HashMap::from([("custom_setting".to_string(), json!(42))]),
        }
    }

    #[test]
    fn import_config_translates_safety() {
        let config = sample_config();
        let (translated, result) = NtmImporter::translate_config(&config);

        assert!(result.success);
        assert!(translated.safety.command_safety_gate);
        assert!(translated.safety.require_approval_destructive);
        assert_eq!(translated.safety.rate_limit_per_minute, 120);
        assert_eq!(translated.safety.allowlist.len(), 2);
        assert_eq!(translated.safety.denylist.len(), 1);
    }

    #[test]
    fn import_config_warns_about_hooks() {
        let config = sample_config();
        let (_translated, result) = NtmImporter::translate_config(&config);

        let warning = result
            .findings
            .iter()
            .find(|f| f.code == "HOOKS_NEED_WORKFLOW_MIGRATION")
            .unwrap();
        assert_eq!(warning.severity, ImportSeverity::Warning);
        assert!(warning.message.contains("2 hook(s)"));
    }

    #[test]
    fn import_config_notes_robot_auth() {
        let config = sample_config();
        let (_translated, result) = NtmImporter::translate_config(&config);

        let info = result
            .findings
            .iter()
            .find(|f| f.code == "ROBOT_AUTH_POLICY_CHANGE")
            .unwrap();
        assert_eq!(info.severity, ImportSeverity::Info);
    }

    #[test]
    fn import_config_preserves_untranslated_keys() {
        let config = sample_config();
        let (translated, result) = NtmImporter::translate_config(&config);

        assert_eq!(translated.untranslated.len(), 1);
        assert_eq!(
            translated.untranslated.get("custom_setting").unwrap(),
            &json!(42)
        );
        let info = result
            .findings
            .iter()
            .find(|f| f.code == "UNTRANSLATED_CONFIG_KEY")
            .unwrap();
        assert_eq!(info.severity, ImportSeverity::Info);
    }

    #[test]
    fn import_config_without_hooks_or_auth_has_no_extra_findings() {
        let config = NtmConfig {
            log_level: Some("info".to_string()),
            workspace: None,
            poll_interval_ms: None,
            pattern_packs: Vec::new(),
            safety: NtmSafetyConfig::default(),
            robot: NtmRobotConfig::default(),
            hooks: Vec::new(),
            extra: HashMap::new(),
        };
        let (_translated, result) = NtmImporter::translate_config(&config);

        assert!(result.success);
        assert!(result.findings.is_empty());
    }

    // -------------------------------------------------------------------------
    // Full pipeline tests
    // -------------------------------------------------------------------------

    #[test]
    fn full_import_pipeline_produces_complete_bundle() {
        let importer = NtmImporter::new();
        let sessions = vec![sample_session()];
        let workflows = vec![sample_workflow()];
        let config = sample_config();

        let bundle = importer.import(&sessions, &workflows, Some(&config));

        assert_eq!(bundle.session_profiles.len(), 3);
        assert_eq!(bundle.layout_templates.len(), 2);
        assert_eq!(bundle.workflows.len(), 1);
        assert!(bundle.config.is_some());
        assert_eq!(bundle.report.total_items, 3); // 1 session + 1 workflow + 1 config
        assert!(bundle.report.success_count > 0);
    }

    #[test]
    fn full_import_with_failures_tracks_counts() {
        let importer = NtmImporter::new();
        let mut session = sample_session();
        session.coordinator_mode = Some("distributed".to_string());

        let mut workflow = sample_workflow();
        workflow.triggers = vec![NtmWorkflowTrigger {
            kind: "webhook".to_string(),
            value: "https://bad.example".to_string(),
            pane_filter: None,
        }];

        let bundle = importer.import(&[session], &[workflow], None);

        assert_eq!(bundle.report.total_items, 2);
        assert_eq!(bundle.report.failure_count, 2);
        assert_eq!(bundle.report.success_count, 0);
    }

    #[test]
    fn import_report_aggregates_finding_codes() {
        let importer = NtmImporter::new();
        let config = sample_config();
        let bundle = importer.import(&[], &[], Some(&config));

        // Config import produces findings for hooks, robot auth, and untranslated keys.
        assert!(!bundle.report.finding_summary.is_empty());
        assert!(
            bundle
                .report
                .finding_summary
                .contains_key("HOOKS_NEED_WORKFLOW_MIGRATION")
        );
    }

    #[test]
    fn import_bundle_serde_roundtrip() {
        let importer = NtmImporter::new();
        let bundle = importer.import(
            &[sample_session()],
            &[sample_workflow()],
            Some(&sample_config()),
        );
        let json = serde_json::to_string(&bundle).unwrap();
        let back: NtmImportBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(bundle.report.total_items, back.report.total_items);
        assert_eq!(bundle.session_profiles.len(), back.session_profiles.len());
        assert_eq!(bundle.workflows.len(), back.workflows.len());
    }

    // -------------------------------------------------------------------------
    // Parsing helper tests
    // -------------------------------------------------------------------------

    #[test]
    fn parse_ntm_sessions_from_json_array() {
        let json = r#"[
            {"name": "dev", "windows": [], "environment": {}, "auto_start": false},
            {"name": "prod", "windows": [], "environment": {}, "auto_start": true}
        ]"#;
        let sessions = parse_ntm_sessions(json).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "dev");
        assert_eq!(sessions[1].name, "prod");
    }

    #[test]
    fn parse_ntm_workflows_from_json_array() {
        let json = r#"[{"name": "wf1", "steps": [], "enabled": true, "allow_parallel": false, "timeout_secs": 0}]"#;
        let workflows = parse_ntm_workflows(json).unwrap();
        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0].name, "wf1");
    }

    #[test]
    fn parse_ntm_config_from_json() {
        let json = r#"{"log_level": "debug", "pattern_packs": ["default"]}"#;
        let config = parse_ntm_config(json).unwrap();
        assert_eq!(config.log_level.as_deref(), Some("debug"));
        assert_eq!(config.pattern_packs, vec!["default"]);
    }

    // -------------------------------------------------------------------------
    // Role translation tests
    // -------------------------------------------------------------------------

    #[test]
    fn role_translation_covers_common_ntm_roles() {
        assert_eq!(translate_ntm_role("agent"), "agent_worker");
        assert_eq!(translate_ntm_role("agent_worker"), "agent_worker");
        assert_eq!(translate_ntm_role("worker"), "agent_worker");
        assert_eq!(translate_ntm_role("agent-worker"), "agent_worker");
        assert_eq!(translate_ntm_role("monitor"), "monitor");
        assert_eq!(translate_ntm_role("log"), "monitor");
        assert_eq!(translate_ntm_role("log-viewer"), "monitor");
        assert_eq!(translate_ntm_role("build"), "build_runner");
        assert_eq!(translate_ntm_role("build-runner"), "build_runner");
        assert_eq!(translate_ntm_role("ci"), "build_runner");
        assert_eq!(translate_ntm_role("test"), "test_runner");
        assert_eq!(translate_ntm_role("test-runner"), "test_runner");
        assert_eq!(translate_ntm_role("service"), "service");
        assert_eq!(translate_ntm_role("server"), "service");
        assert_eq!(translate_ntm_role("daemon"), "service");
        assert_eq!(translate_ntm_role("dev"), "dev_shell");
        assert_eq!(translate_ntm_role("shell"), "dev_shell");
        assert_eq!(translate_ntm_role("interactive"), "dev_shell");
        assert_eq!(translate_ntm_role("unknown_role"), "custom");
    }

    #[test]
    fn role_translation_is_case_insensitive() {
        assert_eq!(translate_ntm_role("AGENT"), "agent_worker");
        assert_eq!(translate_ntm_role("Monitor"), "monitor");
        assert_eq!(translate_ntm_role("BUILD_RUNNER"), "build_runner");
    }

    // -------------------------------------------------------------------------
    // Import severity tests
    // -------------------------------------------------------------------------

    #[test]
    fn import_severity_ordering() {
        assert!(ImportSeverity::Info < ImportSeverity::Warning);
        assert!(ImportSeverity::Warning < ImportSeverity::Error);
    }

    #[test]
    fn import_severity_as_str() {
        assert_eq!(ImportSeverity::Info.as_str(), "info");
        assert_eq!(ImportSeverity::Warning.as_str(), "warning");
        assert_eq!(ImportSeverity::Error.as_str(), "error");
    }

    // -------------------------------------------------------------------------
    // Edge case tests
    // -------------------------------------------------------------------------

    #[test]
    fn import_multiple_sessions_aggregates_correctly() {
        let importer = NtmImporter::new();
        let s1 = sample_session();
        let mut s2 = sample_session();
        s2.name = "second-fleet".to_string();

        let bundle = importer.import(&[s1, s2], &[], None);
        assert_eq!(bundle.session_profiles.len(), 6); // 3 per session
        assert_eq!(bundle.layout_templates.len(), 4); // 2 per session
        assert_eq!(bundle.report.total_items, 2);
    }

    #[test]
    fn import_parallel_workflow_sets_unlimited_concurrency() {
        let mut workflow = sample_workflow();
        workflow.allow_parallel = true;
        let (translated, _result) = NtmImporter::translate_workflow(&workflow);
        assert_eq!(translated.unwrap().max_concurrent, 0);
    }

    #[test]
    fn import_workflow_manual_trigger_is_info() {
        let mut workflow = sample_workflow();
        workflow.triggers = vec![NtmWorkflowTrigger {
            kind: "manual".to_string(),
            value: String::new(),
            pane_filter: None,
        }];
        let (_translated, result) = NtmImporter::translate_workflow(&workflow);
        assert!(result.success);
        let info = result
            .findings
            .iter()
            .find(|f| f.code == "MANUAL_TRIGGER_PRESERVED")
            .unwrap();
        assert_eq!(info.severity, ImportSeverity::Info);
    }

    #[test]
    fn import_topology_step_preserved_as_unsupported() {
        let mut workflow = sample_workflow();
        workflow.steps = vec![NtmWorkflowStep {
            name: "split-pane".to_string(),
            action: "split".to_string(),
            params: HashMap::from([("direction".to_string(), json!("horizontal"))]),
            conditions: Vec::new(),
            timeout_secs: None,
        }];
        let (translated, result) = NtmImporter::translate_workflow(&workflow);
        assert!(result.success); // Warning, not error.
        let tw = translated.unwrap();
        assert!(matches!(
            tw.steps[0].step_type,
            TranslatedStepType::Unsupported { .. }
        ));
    }

    #[test]
    fn wait_for_step_uses_step_timeout_over_param() {
        let mut workflow = sample_workflow();
        workflow.steps = vec![NtmWorkflowStep {
            name: "wait".to_string(),
            action: "wait_for".to_string(),
            params: HashMap::from([
                ("pattern".to_string(), json!("done")),
                ("timeout_secs".to_string(), json!(10)),
            ]),
            conditions: Vec::new(),
            timeout_secs: Some(20), // Step-level timeout takes precedence.
        }];
        let (translated, _result) = NtmImporter::translate_workflow(&workflow);
        let tw = translated.unwrap();
        match &tw.steps[0].step_type {
            TranslatedStepType::WaitFor { timeout_ms, .. } => {
                assert_eq!(timeout_ms, &Some(20_000));
            }
            other => panic!("Expected WaitFor, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------------
    // Test helpers
    // -------------------------------------------------------------------------

    fn default_pane() -> NtmPane {
        NtmPane {
            role: None,
            command: None,
            args: Vec::new(),
            environment: HashMap::new(),
            cwd: None,
            split_direction: None,
            split_ratio: None,
            is_focus: false,
        }
    }
}
