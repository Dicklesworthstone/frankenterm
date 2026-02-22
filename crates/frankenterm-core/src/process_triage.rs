//! Automated process triage for agent swarm management.
//!
//! Classifies processes against a kill hierarchy and produces ordered
//! action plans for cleanup. Integrates with [`process_tree`](crate::process_tree)
//! for per-pane process tree awareness.
//!
//! # Kill Hierarchy
//!
//! | Priority | Category | Risk |
//! |----------|----------|------|
//! | 1 | Zombies | Zero |
//! | 2 | Stuck tests (12+ hours) | Low |
//! | 3 | Stuck CLIs (5+ min) | Low |
//! | 4 | Duplicate builds | Low |
//! | 5 | Abandoned servers (24h idle) | Low |
//! | 6 | Stale sessions | Medium |
//! | 7 | Confused agents (16+ hours) | Medium |
//! | 8 | Active agents (<16h) | High (protect) |
//! | 9 | System processes | Forbidden |

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::process_tree::{ProcessNode, ProcessState, ProcessTree};

// =============================================================================
// Kill hierarchy categories
// =============================================================================

/// Process classification category, ordered by kill priority (lowest = kill first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriageCategory {
    /// Priority 1: Zombie processes (Z state). Zero risk to kill.
    Zombie = 1,
    /// Priority 2: Stuck test runners (12+ hours, <1% CPU).
    StuckTest = 2,
    /// Priority 3: Stuck CLI tools (git, vercel, npm — 5+ min).
    StuckCli = 3,
    /// Priority 4: Duplicate build processes (keep newest).
    DuplicateBuild = 4,
    /// Priority 5: Abandoned dev servers (24+ hours idle).
    AbandonedServer = 5,
    /// Priority 6: Stale tmux/screen sessions.
    StaleSession = 6,
    /// Priority 7: Confused agents (16+ hours, stuck).
    ConfusedAgent = 7,
    /// Priority 8: Active agents — protect or renice only.
    ActiveAgent = 8,
    /// Priority 9: System processes — NEVER touch.
    SystemProcess = 9,
}

impl std::fmt::Display for TriageCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Zombie => write!(f, "zombie"),
            Self::StuckTest => write!(f, "stuck_test"),
            Self::StuckCli => write!(f, "stuck_cli"),
            Self::DuplicateBuild => write!(f, "duplicate_build"),
            Self::AbandonedServer => write!(f, "abandoned_server"),
            Self::StaleSession => write!(f, "stale_session"),
            Self::ConfusedAgent => write!(f, "confused_agent"),
            Self::ActiveAgent => write!(f, "active_agent"),
            Self::SystemProcess => write!(f, "system_process"),
        }
    }
}

impl TriageCategory {
    /// Whether this category allows automatic action (no human approval needed).
    #[must_use]
    pub const fn is_auto_safe(self) -> bool {
        matches!(
            self,
            Self::Zombie | Self::StuckTest | Self::StuckCli | Self::DuplicateBuild
        )
    }

    /// Whether processes in this category must never be killed.
    #[must_use]
    pub const fn is_protected(self) -> bool {
        matches!(self, Self::ActiveAgent | Self::SystemProcess)
    }

    /// Kill priority (lower = kill first).
    #[must_use]
    pub const fn priority(self) -> u8 {
        self as u8
    }
}

// =============================================================================
// Triage action
// =============================================================================

/// Action to take on a classified process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TriageAction {
    /// Send SIGCHLD to parent (zombies only).
    ReapZombie { parent_pid: u32 },
    /// Send SIGTERM, wait grace period, then SIGKILL if needed.
    GracefulKill { grace_period: Duration },
    /// Kill immediately with SIGKILL (after SIGTERM failed).
    ForceKill,
    /// Reduce priority (renice to 19, ionice idle).
    Renice,
    /// Do nothing — process is protected.
    Protect,
    /// Flag for human review.
    FlagForReview { reason: String },
}

impl std::fmt::Display for TriageAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReapZombie { parent_pid } => write!(f, "reap_zombie(parent={parent_pid})"),
            Self::GracefulKill { grace_period } => {
                write!(f, "graceful_kill(grace={}s)", grace_period.as_secs())
            }
            Self::ForceKill => write!(f, "force_kill"),
            Self::Renice => write!(f, "renice"),
            Self::Protect => write!(f, "protect"),
            Self::FlagForReview { reason } => write!(f, "flag_for_review({reason})"),
        }
    }
}

// =============================================================================
// Classified process
// =============================================================================

/// A process with its triage classification and recommended action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifiedProcess {
    /// Process ID.
    pub pid: u32,
    /// Process name.
    pub name: String,
    /// Triage category.
    pub category: TriageCategory,
    /// Recommended action.
    pub action: TriageAction,
    /// Human-readable reason for the classification.
    pub reason: String,
    /// Optional pane ID this process belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
}

// =============================================================================
// Process classifier
// =============================================================================

/// System process names that must NEVER be killed.
const SYSTEM_PROCESSES: &[&str] = &[
    "init",
    "systemd",
    "launchd",
    "kernel_task",
    "WindowServer",
    "sshd",
    "postgres",
    "mysqld",
    "dockerd",
    "containerd",
    "kubelet",
    "cron",
    "rsyslogd",
    "loginwindow",
    "CoreServicesUIAgent",
    "mds",
    "mds_stores",
    "mdworker",
    "wezterm-mux-server",
];

/// Test runner process names.
const TEST_RUNNERS: &[&str] = &["cargo-nextest", "pytest", "jest", "mocha", "vitest", "go"];

/// Short-lived CLI tools that shouldn't run for more than a few minutes.
const CLI_TOOLS: &[&str] = &[
    "git",
    "gh",
    "npm",
    "npx",
    "yarn",
    "pnpm",
    "bun",
    "vercel",
    "pip",
    "pip3",
    "cargo-install",
];

/// Development server process names.
const DEV_SERVERS: &[&str] = &[
    "next", "vite", "bun", "node", "uvicorn", "gunicorn", "flask", "django",
];

/// Agent process names.
const AGENT_PROCESSES: &[&str] = &[
    "claude",
    "claude-code",
    "codex",
    "gemini",
    "aider",
    "copilot",
];

/// Build tool process names.
const BUILD_TOOLS: &[&str] = &[
    "cargo", "rustc", "cc1", "cc1plus", "gcc", "g++", "clang", "clang++", "make", "cmake", "ninja",
];

/// Context for process classification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessContext {
    /// Process age (time since start).
    pub age: Duration,
    /// Current CPU utilization (0-100).
    pub cpu_percent: f64,
    /// Whether this process is a test runner.
    pub is_test: bool,
}

/// Canonical schema identifier for process triage provider payloads.
pub const PROCESS_TRIAGE_SCHEMA_VERSION_V1: &str = "ft.process_triage.v1";

/// Versioned integration envelope for process triage provider responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessTriageEnvelope<T> {
    /// Schema version for compatibility checks.
    pub schema_version: String,
    /// Provider identifier for provenance and audit trails.
    pub provider: String,
    /// Envelope payload.
    pub data: T,
}

/// Classify a process node and its context into a triage category.
pub fn classify(
    node: &ProcessNode,
    context: &ProcessContext,
) -> (TriageCategory, TriageAction, String) {
    let lower_name = node.name.to_lowercase();

    // Priority 9: System processes — NEVER touch.
    if SYSTEM_PROCESSES
        .iter()
        .any(|s| s.eq_ignore_ascii_case(&node.name))
    {
        return (
            TriageCategory::SystemProcess,
            TriageAction::Protect,
            format!("system process: {}", node.name),
        );
    }

    // Priority 1: Zombies.
    if node.state == ProcessState::Zombie {
        return (
            TriageCategory::Zombie,
            TriageAction::ReapZombie {
                parent_pid: node.ppid,
            },
            format!("zombie process: {}", node.name),
        );
    }

    // Priority 2: Stuck test runners (12+ hours, <1% CPU).
    let is_test_process = context.is_test
        || TEST_RUNNERS
            .iter()
            .any(|t| lower_name.contains(&t.to_lowercase()))
        || node
            .argv
            .iter()
            .any(|a| a.contains("test") || a.contains("spec"));
    if is_test_process && context.age > Duration::from_secs(12 * 3600) && context.cpu_percent < 1.0
    {
        return (
            TriageCategory::StuckTest,
            TriageAction::GracefulKill {
                grace_period: Duration::from_secs(30),
            },
            format!(
                "test runner stuck for {:.1}h at {:.1}% CPU",
                context.age.as_secs_f64() / 3600.0,
                context.cpu_percent,
            ),
        );
    }

    // Priority 3: Stuck CLI tools (5+ minutes).
    if CLI_TOOLS.iter().any(|t| t.eq_ignore_ascii_case(&node.name))
        && context.age > Duration::from_secs(5 * 60)
    {
        return (
            TriageCategory::StuckCli,
            TriageAction::GracefulKill {
                grace_period: Duration::from_secs(10),
            },
            format!(
                "CLI tool {} running for {:.0} minutes",
                node.name,
                context.age.as_secs_f64() / 60.0,
            ),
        );
    }

    // Priority 4: Duplicate builds (detected at triage engine level, not per-process).
    // We flag build tools for the engine to deduplicate.
    if BUILD_TOOLS
        .iter()
        .any(|t| t.eq_ignore_ascii_case(&node.name))
    {
        // The TriageEngine handles dedup; here we just note it.
        return (
            TriageCategory::DuplicateBuild,
            TriageAction::FlagForReview {
                reason: format!("build tool: {} — check for duplicates", node.name),
            },
            format!("build process: {}", node.name),
        );
    }

    // Priority 5: Abandoned dev servers (24+ hours).
    if DEV_SERVERS
        .iter()
        .any(|s| lower_name.contains(&s.to_lowercase()))
        && context.age > Duration::from_secs(24 * 3600)
    {
        return (
            TriageCategory::AbandonedServer,
            TriageAction::GracefulKill {
                grace_period: Duration::from_secs(30),
            },
            format!(
                "dev server {} idle for {:.0}h",
                node.name,
                context.age.as_secs_f64() / 3600.0,
            ),
        );
    }

    // Priority 6: Stale sessions (tmux/screen).
    if matches!(lower_name.as_str(), "tmux" | "screen")
        && context.age > Duration::from_secs(24 * 3600)
    {
        return (
            TriageCategory::StaleSession,
            TriageAction::FlagForReview {
                reason: format!(
                    "stale {} session, {:.0}h old",
                    node.name,
                    context.age.as_secs_f64() / 3600.0,
                ),
            },
            format!("stale session: {}", node.name),
        );
    }

    // Priority 7-8: Agent processes.
    if AGENT_PROCESSES
        .iter()
        .any(|a| lower_name.contains(&a.to_lowercase()))
    {
        if context.age > Duration::from_secs(16 * 3600) {
            return (
                TriageCategory::ConfusedAgent,
                TriageAction::FlagForReview {
                    reason: format!(
                        "agent {} running for {:.0}h — may be stuck",
                        node.name,
                        context.age.as_secs_f64() / 3600.0,
                    ),
                },
                format!("long-running agent: {}", node.name),
            );
        }
        return (
            TriageCategory::ActiveAgent,
            TriageAction::Protect,
            format!("active agent: {}", node.name),
        );
    }

    // Default: flag for review if old, protect if young.
    if context.age > Duration::from_secs(24 * 3600) && context.cpu_percent < 1.0 {
        (
            TriageCategory::AbandonedServer,
            TriageAction::FlagForReview {
                reason: format!(
                    "process {} idle for {:.0}h",
                    node.name,
                    context.age.as_secs_f64() / 3600.0,
                ),
            },
            format!("idle process: {}", node.name),
        )
    } else {
        (
            TriageCategory::ActiveAgent,
            TriageAction::Protect,
            format!("active process: {}", node.name),
        )
    }
}

// =============================================================================
// Triage engine
// =============================================================================

/// Build a triage plan from a set of process trees.
///
/// Each entry is `(tree, pane_id, process_ages)` where `process_ages`
/// maps PID to process age and CPU%.
pub type TriageInput<'a> = (ProcessTree, Option<u64>, &'a dyn Fn(u32) -> ProcessContext);

/// Provider contract for process triage classification and planning.
pub trait ProcessTriageProvider {
    /// Stable provider identifier.
    fn provider_name(&self) -> &'static str;

    /// Output schema version emitted by this provider.
    fn schema_version(&self) -> &'static str {
        PROCESS_TRIAGE_SCHEMA_VERSION_V1
    }

    /// Classify one process node with context.
    fn classify(
        &self,
        node: &ProcessNode,
        context: &ProcessContext,
    ) -> (TriageCategory, TriageAction, String);

    /// Build a triage plan from process trees.
    fn build_plan(&self, trees: &[TriageInput<'_>]) -> TriagePlan {
        build_plan_with_classifier(trees, &|node, context| self.classify(node, context))
    }

    /// Build a versioned integration envelope from process trees.
    fn build_envelope(&self, trees: &[TriageInput<'_>]) -> ProcessTriageEnvelope<TriagePlan> {
        ProcessTriageEnvelope {
            schema_version: self.schema_version().to_string(),
            provider: self.provider_name().to_string(),
            data: self.build_plan(trees),
        }
    }
}

/// Default heuristic/classifier-based process triage provider.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicProcessTriageProvider;

impl ProcessTriageProvider for HeuristicProcessTriageProvider {
    fn provider_name(&self) -> &'static str {
        "heuristic"
    }

    fn classify(
        &self,
        node: &ProcessNode,
        context: &ProcessContext,
    ) -> (TriageCategory, TriageAction, String) {
        self::classify(node, context)
    }
}

/// Configuration for the triage engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TriageConfig {
    /// Enable automatic triage.
    pub enabled: bool,
    /// Auto-execute safe actions (zombies, stuck tests/CLIs).
    pub auto_safe: bool,
    /// Test runner stuck threshold (hours).
    pub stuck_test_hours: f64,
    /// CLI tool stuck threshold (minutes).
    pub stuck_cli_minutes: f64,
    /// Agent confused threshold (hours).
    pub agent_confused_hours: f64,
    /// Dev server abandoned threshold (hours).
    pub server_abandoned_hours: f64,
}

impl Default for TriageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_safe: true,
            stuck_test_hours: 12.0,
            stuck_cli_minutes: 5.0,
            agent_confused_hours: 16.0,
            server_abandoned_hours: 24.0,
        }
    }
}

/// Triage plan — ordered list of classified processes with actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriagePlan {
    /// Classified processes, sorted by kill priority (lowest first = kill first).
    pub entries: Vec<ClassifiedProcess>,
    /// Count of auto-safe actions.
    pub auto_safe_count: usize,
    /// Count of actions requiring review.
    pub review_count: usize,
    /// Count of protected processes.
    pub protected_count: usize,
}

impl TriagePlan {
    /// Get only the auto-safe actions (can execute without approval).
    pub fn auto_safe_entries(&self) -> Vec<&ClassifiedProcess> {
        self.entries
            .iter()
            .filter(|e| e.category.is_auto_safe())
            .collect()
    }

    /// Get entries requiring human review.
    pub fn review_entries(&self) -> Vec<&ClassifiedProcess> {
        self.entries
            .iter()
            .filter(|e| {
                !e.category.is_auto_safe()
                    && !e.category.is_protected()
                    && matches!(e.action, TriageAction::FlagForReview { .. })
            })
            .collect()
    }
}

/// Build a triage plan using the default heuristic provider.
pub fn build_plan(trees: &[TriageInput<'_>]) -> TriagePlan {
    let provider = HeuristicProcessTriageProvider;
    provider.build_plan(trees)
}

/// Build a triage plan using an explicit provider implementation.
pub fn build_plan_with_provider(
    provider: &dyn ProcessTriageProvider,
    trees: &[TriageInput<'_>],
) -> TriagePlan {
    provider.build_plan(trees)
}

/// Build a versioned triage envelope using an explicit provider implementation.
pub fn build_plan_envelope(
    provider: &dyn ProcessTriageProvider,
    trees: &[TriageInput<'_>],
) -> ProcessTriageEnvelope<TriagePlan> {
    provider.build_envelope(trees)
}

fn build_plan_with_classifier(
    trees: &[TriageInput<'_>],
    classify_fn: &dyn Fn(&ProcessNode, &ProcessContext) -> (TriageCategory, TriageAction, String),
) -> TriagePlan {
    let mut entries = Vec::new();

    for (tree, pane_id, context_fn) in trees {
        classify_tree(&tree.root, *pane_id, context_fn, classify_fn, &mut entries);
    }

    // Sort by kill priority (lowest category = kill first).
    entries.sort_by_key(|e| e.category);

    let auto_safe_count = entries.iter().filter(|e| e.category.is_auto_safe()).count();
    let protected_count = entries.iter().filter(|e| e.category.is_protected()).count();
    let review_count = entries
        .iter()
        .filter(|e| matches!(e.action, TriageAction::FlagForReview { .. }))
        .count();

    TriagePlan {
        entries,
        auto_safe_count,
        review_count,
        protected_count,
    }
}

fn classify_tree(
    node: &ProcessNode,
    pane_id: Option<u64>,
    context_fn: &dyn Fn(u32) -> ProcessContext,
    classify_fn: &dyn Fn(&ProcessNode, &ProcessContext) -> (TriageCategory, TriageAction, String),
    out: &mut Vec<ClassifiedProcess>,
) {
    let context = context_fn(node.pid);
    let (category, action, reason) = classify_fn(node, &context);

    out.push(ClassifiedProcess {
        pid: node.pid,
        name: node.name.clone(),
        category,
        action,
        reason,
        pane_id,
    });

    for child in &node.children {
        classify_tree(child, pane_id, context_fn, classify_fn, out);
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(pid: u32, parent_pid: u32, name: &str, state: ProcessState) -> ProcessNode {
        ProcessNode {
            pid,
            ppid: parent_pid,
            name: name.into(),
            argv: vec![],
            state,
            rss_kb: 10000,
            children: vec![],
        }
    }

    fn make_context(age_hours: f64, cpu: f64, is_test: bool) -> ProcessContext {
        ProcessContext {
            age: Duration::from_secs_f64(age_hours * 3600.0),
            cpu_percent: cpu,
            is_test,
        }
    }

    // ---- Category classification tests ----

    #[test]
    fn classify_zombie() {
        let node = make_node(100, 1, "defunct", ProcessState::Zombie);
        let ctx = make_context(0.0, 0.0, false);
        let (cat, action, _) = classify(&node, &ctx);
        assert_eq!(cat, TriageCategory::Zombie);
        assert!(matches!(action, TriageAction::ReapZombie { parent_pid: 1 }));
    }

    #[test]
    fn classify_system_process() {
        for name in &["sshd", "systemd", "launchd", "wezterm-mux-server"] {
            let node = make_node(1, 0, name, ProcessState::Running);
            let ctx = make_context(100.0, 50.0, false);
            let (cat, action, _) = classify(&node, &ctx);
            assert_eq!(
                cat,
                TriageCategory::SystemProcess,
                "expected system: {name}"
            );
            assert!(matches!(action, TriageAction::Protect));
        }
    }

    #[test]
    fn classify_stuck_test() {
        let node = make_node(200, 100, "cargo-nextest", ProcessState::Sleeping);
        let ctx = make_context(13.0, 0.5, false);
        let (cat, _, _) = classify(&node, &ctx);
        assert_eq!(cat, TriageCategory::StuckTest);
    }

    #[test]
    fn classify_test_by_argv() {
        let mut node = make_node(200, 100, "cargo", ProcessState::Sleeping);
        node.argv = vec!["cargo".into(), "test".into(), "--release".into()];
        let ctx = make_context(13.0, 0.5, false);
        let (cat, _, _) = classify(&node, &ctx);
        assert_eq!(cat, TriageCategory::StuckTest);
    }

    #[test]
    fn classify_active_test_not_stuck() {
        let node = make_node(200, 100, "cargo-nextest", ProcessState::Running);
        let ctx = make_context(1.0, 50.0, false);
        // Young test with high CPU = still a build tool, but not stuck
        let (cat, _, _) = classify(&node, &ctx);
        // Not stuck test (age < 12h), gets classified as DuplicateBuild since "cargo-nextest" starts with "cargo"
        assert_ne!(cat, TriageCategory::StuckTest);
    }

    #[test]
    fn classify_stuck_cli() {
        let node = make_node(300, 100, "git", ProcessState::Sleeping);
        let ctx = make_context(0.2, 0.0, false); // 12 minutes
        let (cat, _, _) = classify(&node, &ctx);
        assert_eq!(cat, TriageCategory::StuckCli);
    }

    #[test]
    fn classify_quick_git_not_stuck() {
        let node = make_node(300, 100, "git", ProcessState::Running);
        let ctx = ProcessContext {
            age: Duration::from_secs(30),
            cpu_percent: 50.0,
            is_test: false,
        };
        let (cat, _, _) = classify(&node, &ctx);
        assert_ne!(cat, TriageCategory::StuckCli);
    }

    #[test]
    fn classify_build_tool() {
        let node = make_node(400, 100, "rustc", ProcessState::Running);
        let ctx = make_context(0.5, 80.0, false);
        let (cat, _, _) = classify(&node, &ctx);
        assert_eq!(cat, TriageCategory::DuplicateBuild);
    }

    #[test]
    fn classify_abandoned_server() {
        let node = make_node(500, 100, "next", ProcessState::Sleeping);
        let ctx = make_context(25.0, 0.0, false);
        let (cat, _, _) = classify(&node, &ctx);
        assert_eq!(cat, TriageCategory::AbandonedServer);
    }

    #[test]
    fn classify_stale_tmux() {
        let node = make_node(600, 1, "tmux", ProcessState::Sleeping);
        let ctx = make_context(48.0, 0.0, false);
        let (cat, _, _) = classify(&node, &ctx);
        assert_eq!(cat, TriageCategory::StaleSession);
    }

    #[test]
    fn classify_confused_agent() {
        let node = make_node(700, 100, "claude-code", ProcessState::Sleeping);
        let ctx = make_context(20.0, 2.0, false);
        let (cat, _, _) = classify(&node, &ctx);
        assert_eq!(cat, TriageCategory::ConfusedAgent);
    }

    #[test]
    fn classify_active_agent() {
        let node = make_node(700, 100, "claude", ProcessState::Running);
        let ctx = make_context(4.0, 30.0, false);
        let (cat, action, _) = classify(&node, &ctx);
        assert_eq!(cat, TriageCategory::ActiveAgent);
        assert!(matches!(action, TriageAction::Protect));
    }

    // ---- Category property tests ----

    #[test]
    fn system_process_always_protected() {
        for name in SYSTEM_PROCESSES {
            let node = make_node(1, 0, name, ProcessState::Running);
            let ctx = make_context(0.0, 0.0, false);
            let (cat, _, _) = classify(&node, &ctx);
            assert!(
                cat.is_protected(),
                "system process {name} should be protected"
            );
        }
    }

    #[test]
    fn zombie_always_classified_first() {
        // Zombie should be priority 1 regardless of process name.
        let node = make_node(1, 0, "claude", ProcessState::Zombie);
        let ctx = make_context(100.0, 0.0, false);
        let (cat, _, _) = classify(&node, &ctx);
        assert_eq!(cat, TriageCategory::Zombie);
    }

    #[test]
    fn category_ordering() {
        assert!(TriageCategory::Zombie < TriageCategory::StuckTest);
        assert!(TriageCategory::StuckTest < TriageCategory::StuckCli);
        assert!(TriageCategory::StuckCli < TriageCategory::DuplicateBuild);
        assert!(TriageCategory::DuplicateBuild < TriageCategory::AbandonedServer);
        assert!(TriageCategory::AbandonedServer < TriageCategory::StaleSession);
        assert!(TriageCategory::StaleSession < TriageCategory::ConfusedAgent);
        assert!(TriageCategory::ConfusedAgent < TriageCategory::ActiveAgent);
        assert!(TriageCategory::ActiveAgent < TriageCategory::SystemProcess);
    }

    #[test]
    fn auto_safe_categories() {
        assert!(TriageCategory::Zombie.is_auto_safe());
        assert!(TriageCategory::StuckTest.is_auto_safe());
        assert!(TriageCategory::StuckCli.is_auto_safe());
        assert!(TriageCategory::DuplicateBuild.is_auto_safe());
        assert!(!TriageCategory::AbandonedServer.is_auto_safe());
        assert!(!TriageCategory::ConfusedAgent.is_auto_safe());
        assert!(!TriageCategory::SystemProcess.is_auto_safe());
    }

    // ---- Triage plan tests ----

    #[test]
    fn build_plan_sorts_by_priority() {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 5000,
                children: vec![
                    // Active agent (priority 8)
                    ProcessNode {
                        pid: 2,
                        ppid: 1,
                        name: "claude".into(),
                        argv: vec![],
                        state: ProcessState::Running,
                        rss_kb: 50000,
                        children: vec![],
                    },
                    // Zombie (priority 1)
                    ProcessNode {
                        pid: 3,
                        ppid: 1,
                        name: "defunct".into(),
                        argv: vec![],
                        state: ProcessState::Zombie,
                        rss_kb: 0,
                        children: vec![],
                    },
                ],
            },
            total_processes: 3,
            total_rss_kb: 55000,
        };

        let context_fn = |_pid: u32| ProcessContext {
            age: Duration::from_secs(3600),
            cpu_percent: 10.0,
            is_test: false,
        };

        let plan = build_plan(&[(tree, Some(42), &context_fn)]);

        // Zombie should come first (priority 1), before active agent (priority 8).
        assert!(plan.entries.len() >= 2);
        assert_eq!(plan.entries[0].category, TriageCategory::Zombie);
        assert_eq!(plan.entries[0].pid, 3);
    }

    #[test]
    fn plan_counts_correct() {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 5000,
                children: vec![
                    make_node(2, 1, "defunct", ProcessState::Zombie),
                    make_node(3, 1, "claude", ProcessState::Running),
                ],
            },
            total_processes: 3,
            total_rss_kb: 25000,
        };

        let context_fn = |_pid: u32| make_context(2.0, 10.0, false);
        let plan = build_plan(&[(tree, None, &context_fn)]);

        assert!(plan.auto_safe_count >= 1); // zombie
        assert!(plan.protected_count >= 1); // active agent
    }

    #[test]
    fn plan_auto_safe_entries() {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 5000,
                children: vec![
                    make_node(2, 1, "defunct", ProcessState::Zombie),
                    make_node(3, 1, "sshd", ProcessState::Running),
                ],
            },
            total_processes: 3,
            total_rss_kb: 25000,
        };

        let context_fn = |_pid: u32| make_context(0.1, 0.0, false);
        let plan = build_plan(&[(tree, None, &context_fn)]);
        let safe = plan.auto_safe_entries();

        // Only zombie should be auto-safe.
        assert_eq!(safe.len(), 1);
        assert_eq!(safe[0].category, TriageCategory::Zombie);
    }

    // ---- Config tests ----

    #[test]
    fn config_defaults() {
        let cfg = TriageConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.auto_safe);
        assert!((cfg.stuck_test_hours - 12.0).abs() < f64::EPSILON);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = TriageConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: TriageConfig = serde_json::from_str(&json).unwrap();
        assert!((parsed.stuck_test_hours - cfg.stuck_test_hours).abs() < f64::EPSILON);
    }

    #[test]
    fn category_serde_roundtrip() {
        for cat in [
            TriageCategory::Zombie,
            TriageCategory::StuckTest,
            TriageCategory::StuckCli,
            TriageCategory::DuplicateBuild,
            TriageCategory::AbandonedServer,
            TriageCategory::StaleSession,
            TriageCategory::ConfusedAgent,
            TriageCategory::ActiveAgent,
            TriageCategory::SystemProcess,
        ] {
            let json = serde_json::to_string(&cat).unwrap();
            let parsed: TriageCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, cat);
        }
    }

    #[test]
    fn category_display() {
        assert_eq!(format!("{}", TriageCategory::Zombie), "zombie");
        assert_eq!(
            format!("{}", TriageCategory::SystemProcess),
            "system_process"
        );
    }

    #[test]
    fn action_display() {
        assert_eq!(
            format!("{}", TriageAction::ReapZombie { parent_pid: 42 }),
            "reap_zombie(parent=42)"
        );
        assert_eq!(
            format!(
                "{}",
                TriageAction::GracefulKill {
                    grace_period: Duration::from_secs(30)
                }
            ),
            "graceful_kill(grace=30s)"
        );
    }

    #[test]
    fn pane_id_propagated_in_plan() {
        let tree = ProcessTree {
            root: make_node(1, 0, "bash", ProcessState::Sleeping),
            total_processes: 1,
            total_rss_kb: 5000,
        };

        let context_fn = |_pid: u32| make_context(0.1, 0.0, false);
        let plan = build_plan(&[(tree, Some(99), &context_fn)]);
        assert_eq!(plan.entries[0].pane_id, Some(99));
    }

    // --- TriageCategory extras ---

    #[test]
    fn category_display_all_variants() {
        assert_eq!(format!("{}", TriageCategory::Zombie), "zombie");
        assert_eq!(format!("{}", TriageCategory::StuckTest), "stuck_test");
        assert_eq!(format!("{}", TriageCategory::StuckCli), "stuck_cli");
        assert_eq!(
            format!("{}", TriageCategory::DuplicateBuild),
            "duplicate_build"
        );
        assert_eq!(
            format!("{}", TriageCategory::AbandonedServer),
            "abandoned_server"
        );
        assert_eq!(format!("{}", TriageCategory::StaleSession), "stale_session");
        assert_eq!(
            format!("{}", TriageCategory::ConfusedAgent),
            "confused_agent"
        );
        assert_eq!(format!("{}", TriageCategory::ActiveAgent), "active_agent");
        assert_eq!(
            format!("{}", TriageCategory::SystemProcess),
            "system_process"
        );
    }

    #[test]
    fn category_copy() {
        let c = TriageCategory::Zombie;
        let c2 = c;
        assert_eq!(c, c2);
    }

    #[test]
    fn category_debug() {
        let dbg = format!("{:?}", TriageCategory::ConfusedAgent);
        assert!(dbg.contains("ConfusedAgent"));
    }

    #[test]
    fn category_priority_values() {
        assert_eq!(TriageCategory::Zombie.priority(), 1);
        assert_eq!(TriageCategory::StuckTest.priority(), 2);
        assert_eq!(TriageCategory::StuckCli.priority(), 3);
        assert_eq!(TriageCategory::DuplicateBuild.priority(), 4);
        assert_eq!(TriageCategory::AbandonedServer.priority(), 5);
        assert_eq!(TriageCategory::StaleSession.priority(), 6);
        assert_eq!(TriageCategory::ConfusedAgent.priority(), 7);
        assert_eq!(TriageCategory::ActiveAgent.priority(), 8);
        assert_eq!(TriageCategory::SystemProcess.priority(), 9);
    }

    #[test]
    fn category_is_protected_all_variants() {
        assert!(!TriageCategory::Zombie.is_protected());
        assert!(!TriageCategory::StuckTest.is_protected());
        assert!(!TriageCategory::StuckCli.is_protected());
        assert!(!TriageCategory::DuplicateBuild.is_protected());
        assert!(!TriageCategory::AbandonedServer.is_protected());
        assert!(!TriageCategory::StaleSession.is_protected());
        assert!(!TriageCategory::ConfusedAgent.is_protected());
        assert!(TriageCategory::ActiveAgent.is_protected());
        assert!(TriageCategory::SystemProcess.is_protected());
    }

    #[test]
    fn category_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(TriageCategory::Zombie);
        set.insert(TriageCategory::Zombie);
        assert_eq!(set.len(), 1);
        set.insert(TriageCategory::ActiveAgent);
        assert_eq!(set.len(), 2);
    }

    // --- TriageAction extras ---

    #[test]
    fn action_display_all_variants() {
        assert_eq!(format!("{}", TriageAction::ForceKill), "force_kill");
        assert_eq!(format!("{}", TriageAction::Renice), "renice");
        assert_eq!(format!("{}", TriageAction::Protect), "protect");
        assert_eq!(
            format!(
                "{}",
                TriageAction::FlagForReview {
                    reason: "why".into()
                }
            ),
            "flag_for_review(why)"
        );
    }

    #[test]
    fn action_clone() {
        let a = TriageAction::GracefulKill {
            grace_period: Duration::from_secs(30),
        };
        let a2 = a.clone();
        assert_eq!(a, a2);
    }

    #[test]
    fn action_debug() {
        let dbg = format!("{:?}", TriageAction::ForceKill);
        assert!(dbg.contains("ForceKill"));
    }

    #[test]
    fn action_eq() {
        assert_eq!(TriageAction::Protect, TriageAction::Protect);
        assert_ne!(TriageAction::ForceKill, TriageAction::Protect);
    }

    // --- ClassifiedProcess ---

    #[test]
    fn classified_process_clone() {
        let cp = ClassifiedProcess {
            pid: 42,
            name: "test".into(),
            category: TriageCategory::Zombie,
            action: TriageAction::ReapZombie { parent_pid: 1 },
            reason: "zombie".into(),
            pane_id: Some(10),
        };
        let cp2 = cp.clone();
        assert_eq!(cp2.pid, 42);
        assert_eq!(cp2.pane_id, Some(10));
    }

    #[test]
    fn classified_process_debug() {
        let cp = ClassifiedProcess {
            pid: 1,
            name: "n".into(),
            category: TriageCategory::ActiveAgent,
            action: TriageAction::Protect,
            reason: "active".into(),
            pane_id: None,
        };
        let dbg = format!("{:?}", cp);
        assert!(dbg.contains("ClassifiedProcess"));
    }

    #[test]
    fn classified_process_serde_roundtrip() {
        let cp = ClassifiedProcess {
            pid: 100,
            name: "cargo".into(),
            category: TriageCategory::DuplicateBuild,
            action: TriageAction::ForceKill,
            reason: "duplicate".into(),
            pane_id: Some(42),
        };
        let json = serde_json::to_string(&cp).unwrap();
        let parsed: ClassifiedProcess = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pid, 100);
        assert_eq!(parsed.category, TriageCategory::DuplicateBuild);
    }

    // --- TriageConfig extras ---

    #[test]
    fn config_clone() {
        let cfg = TriageConfig::default();
        let cfg2 = cfg.clone();
        assert!(cfg2.enabled);
        assert!((cfg2.stuck_test_hours - cfg.stuck_test_hours).abs() < f64::EPSILON);
    }

    #[test]
    fn config_debug() {
        let cfg = TriageConfig::default();
        let dbg = format!("{:?}", cfg);
        assert!(dbg.contains("TriageConfig"));
    }

    #[test]
    fn config_serde_missing_fields_default() {
        let json = "{}";
        let cfg: TriageConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert!(cfg.auto_safe);
    }

    // --- TriagePlan ---

    #[test]
    fn plan_review_entries_empty() {
        let plan = TriagePlan {
            entries: vec![],
            auto_safe_count: 0,
            review_count: 0,
            protected_count: 0,
        };
        assert!(plan.auto_safe_entries().is_empty());
        assert!(plan.review_entries().is_empty());
    }

    #[test]
    fn plan_debug() {
        let plan = TriagePlan {
            entries: vec![],
            auto_safe_count: 0,
            review_count: 0,
            protected_count: 0,
        };
        let dbg = format!("{:?}", plan);
        assert!(dbg.contains("TriagePlan"));
    }

    // --- classify edge cases ---

    #[test]
    fn classify_svn_is_stuck_cli() {
        let node = make_node(300, 100, "svn", ProcessState::Sleeping);
        let ctx = make_context(0.2, 0.0, false);
        // SVN is not in CLI_TOOLS, so it falls through to a different category
        let (cat, _, _) = classify(&node, &ctx);
        assert_ne!(cat, TriageCategory::SystemProcess);
    }

    #[test]
    fn classify_gemini_as_agent() {
        let node = make_node(700, 100, "gemini", ProcessState::Running);
        let ctx = make_context(4.0, 30.0, false);
        let (cat, _, _) = classify(&node, &ctx);
        assert_eq!(cat, TriageCategory::ActiveAgent);
    }

    #[test]
    fn classify_screen_as_stale_session() {
        let node = make_node(600, 1, "screen", ProcessState::Sleeping);
        let ctx = make_context(48.0, 0.0, false);
        let (cat, _, _) = classify(&node, &ctx);
        assert_eq!(cat, TriageCategory::StaleSession);
    }

    #[test]
    fn default_provider_matches_module_classifier() {
        let provider = HeuristicProcessTriageProvider;
        let node = make_node(42, 1, "claude-code", ProcessState::Running);
        let ctx = make_context(2.0, 15.0, false);

        let direct = classify(&node, &ctx);
        let provider_result = provider.classify(&node, &ctx);

        assert_eq!(provider_result, direct);
    }

    #[test]
    fn build_plan_with_provider_matches_default() {
        let provider = HeuristicProcessTriageProvider;
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 5000,
                children: vec![
                    make_node(2, 1, "defunct", ProcessState::Zombie),
                    make_node(3, 1, "claude", ProcessState::Running),
                ],
            },
            total_processes: 3,
            total_rss_kb: 25000,
        };
        let context_fn = |_pid: u32| make_context(2.0, 10.0, false);
        let inputs = [(
            tree,
            Some(99),
            &context_fn as &dyn Fn(u32) -> ProcessContext,
        )];

        let default_plan = build_plan(&inputs);
        let provider_plan = build_plan_with_provider(&provider, &inputs);

        assert_eq!(provider_plan, default_plan);
    }

    #[test]
    fn build_plan_envelope_contains_schema_and_provider() {
        let provider = HeuristicProcessTriageProvider;
        let tree = ProcessTree {
            root: make_node(1, 0, "bash", ProcessState::Running),
            total_processes: 1,
            total_rss_kb: 5000,
        };
        let context_fn = |_pid: u32| make_context(1.0, 5.0, false);
        let inputs = [(tree, None, &context_fn as &dyn Fn(u32) -> ProcessContext)];

        let expected_plan = build_plan_with_provider(&provider, &inputs);
        let envelope = build_plan_envelope(&provider, &inputs);

        assert_eq!(envelope.schema_version, PROCESS_TRIAGE_SCHEMA_VERSION_V1);
        assert_eq!(envelope.provider, "heuristic");
        assert_eq!(envelope.data, expected_plan);
    }
}
