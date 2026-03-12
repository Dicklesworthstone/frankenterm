//! NTM decommission governance and operator documentation index (ft-3681t.8.5).
//!
//! Manages the retirement of Named Tmux Manager (NTM) as an operational dependency,
//! providing auditable decommission phases, reversibility policy, and documentation
//! coverage tracking for operator/contributor onboarding.
//!
//! # Architecture
//!
//! ```text
//! DecommissionPlan
//!   ├── NtmDependency[] (things that depend on NTM)
//!   │     ├── dep_id, component, status (Active/Migrated/Retired)
//!   │     └── migration_target (native FrankenTerm replacement)
//!   │
//!   ├── DecommissionPhase[] (ordered retirement phases)
//!   │     ├── phase_id, description, gate checks
//!   │     └── rollback_plan
//!   │
//!   └── DocumentationIndex
//!         ├── OperatorPlaybook[] (day-2 operation docs)
//!         ├── ContributorGuide[] (development docs)
//!         └── coverage_rate
//!
//! DecommissionAudit
//!   ├── AuditEntry[] (timestamped state changes)
//!   └── ReversibilityPolicy (conditions for un-decommission)
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// NTM dependency tracking
// =============================================================================

/// Status of an NTM dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DependencyStatus {
    /// Still actively depends on NTM.
    Active,
    /// Migration is in progress (dual-running).
    Migrating,
    /// Fully migrated to native FrankenTerm.
    Migrated,
    /// Retired — NTM functionality no longer needed.
    Retired,
}

impl DependencyStatus {
    /// Whether this dependency is still active (not yet migrated).
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active | Self::Migrating)
    }

    /// Human-readable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Migrating => "migrating",
            Self::Migrated => "migrated",
            Self::Retired => "retired",
        }
    }
}

/// Component category that depends on NTM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NtmComponentCategory {
    /// Session management (create, list, attach, detach).
    SessionManagement,
    /// Pane orchestration (split, resize, send-text).
    PaneOrchestration,
    /// Agent swarm launching and lifecycle.
    SwarmLauncher,
    /// Configuration and profiles.
    Configuration,
    /// Remote/SSH integration.
    RemoteAccess,
    /// Monitoring and health checks.
    Monitoring,
    /// CLI command surface.
    CliSurface,
}

impl NtmComponentCategory {
    /// Human-readable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::SessionManagement => "session-management",
            Self::PaneOrchestration => "pane-orchestration",
            Self::SwarmLauncher => "swarm-launcher",
            Self::Configuration => "configuration",
            Self::RemoteAccess => "remote-access",
            Self::Monitoring => "monitoring",
            Self::CliSurface => "cli-surface",
        }
    }
}

/// A single NTM dependency that must be retired or migrated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmDependency {
    /// Dependency identifier.
    pub dep_id: String,
    /// Human-readable description.
    pub description: String,
    /// Component category.
    pub category: NtmComponentCategory,
    /// Current status.
    pub status: DependencyStatus,
    /// Native FrankenTerm replacement (if migrated/migrating).
    pub migration_target: Option<String>,
    /// Evidence of migration completion.
    pub migration_evidence: Option<String>,
    /// Notes.
    pub notes: Option<String>,
}

impl NtmDependency {
    /// Create a new active dependency.
    #[must_use]
    pub fn active(
        dep_id: impl Into<String>,
        description: impl Into<String>,
        category: NtmComponentCategory,
    ) -> Self {
        Self {
            dep_id: dep_id.into(),
            description: description.into(),
            category,
            status: DependencyStatus::Active,
            migration_target: None,
            migration_evidence: None,
            notes: None,
        }
    }

    /// Create a migrated dependency.
    #[must_use]
    pub fn migrated(
        dep_id: impl Into<String>,
        description: impl Into<String>,
        category: NtmComponentCategory,
        target: impl Into<String>,
        evidence: impl Into<String>,
    ) -> Self {
        Self {
            dep_id: dep_id.into(),
            description: description.into(),
            category,
            status: DependencyStatus::Migrated,
            migration_target: Some(target.into()),
            migration_evidence: Some(evidence.into()),
            notes: None,
        }
    }
}

/// Standard NTM dependency registry for the convergence migration.
#[must_use]
pub fn standard_ntm_dependencies() -> Vec<NtmDependency> {
    vec![
        NtmDependency::migrated(
            "NTM-DEP-01",
            "Session create/list/attach/detach",
            NtmComponentCategory::SessionManagement,
            "frankenterm-core::headless_mux_server",
            "Headless mux server handles all session lifecycle",
        ),
        NtmDependency::migrated(
            "NTM-DEP-02",
            "Pane split/resize/close orchestration",
            NtmComponentCategory::PaneOrchestration,
            "frankenterm-core::pane_lifecycle + wezterm module",
            "Native pane lifecycle manager with typestate enforcement",
        ),
        NtmDependency::migrated(
            "NTM-DEP-03",
            "Agent swarm launch and topology management",
            NtmComponentCategory::SwarmLauncher,
            "frankenterm-core::fleet_launcher + swarm_scheduler",
            "Native fleet launcher with capacity governance",
        ),
        NtmDependency::migrated(
            "NTM-DEP-04",
            "Layout configuration and profiles",
            NtmComponentCategory::Configuration,
            "frankenterm-core::config_profiles + agent_config_templates",
            "TOML-based config profiles with agent templates",
        ),
        NtmDependency::migrated(
            "NTM-DEP-05",
            "Remote SSH session management",
            NtmComponentCategory::RemoteAccess,
            "frankenterm-core::wezterm (SSH domain support)",
            "WezTerm SSH domains replace NTM SSH tunneling",
        ),
        NtmDependency::migrated(
            "NTM-DEP-06",
            "Session health monitoring and watchdog",
            NtmComponentCategory::Monitoring,
            "frankenterm-core::watchdog + runtime_health",
            "Native watchdog with kalman filter monitoring",
        ),
        NtmDependency::migrated(
            "NTM-DEP-07",
            "CLI command surface (ntm launch, ntm status, etc.)",
            NtmComponentCategory::CliSurface,
            "frankenterm/src/main.rs (ft robot, ft launch, ft status)",
            "ft CLI provides superset of NTM commands",
        ),
    ]
}

// =============================================================================
// Decommission phases
// =============================================================================

/// A single gate check for a decommission phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateCheck {
    /// Check identifier.
    pub check_id: String,
    /// Description.
    pub description: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Evidence.
    pub evidence: String,
}

/// Rollback plan for a decommission phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackPlan {
    /// Steps to reverse this phase.
    pub steps: Vec<String>,
    /// Estimated time to rollback (ms).
    pub estimated_time_ms: u64,
    /// Whether rollback has been rehearsed.
    pub rehearsed: bool,
}

/// A phase in the decommission process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecommissionPhase {
    /// Phase identifier.
    pub phase_id: String,
    /// Phase name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Order in the sequence.
    pub order: u32,
    /// Gate checks that must pass before this phase completes.
    pub gates: Vec<GateCheck>,
    /// Rollback plan.
    pub rollback: RollbackPlan,
    /// Whether this phase is complete.
    pub complete: bool,
}

impl DecommissionPhase {
    /// Whether all gate checks pass.
    #[must_use]
    pub fn gates_pass(&self) -> bool {
        !self.gates.is_empty() && self.gates.iter().all(|g| g.passed)
    }

    /// Gate pass rate.
    #[must_use]
    pub fn gate_pass_rate(&self) -> f64 {
        if self.gates.is_empty() {
            return 0.0;
        }
        self.gates.iter().filter(|g| g.passed).count() as f64 / self.gates.len() as f64
    }
}

/// Standard decommission phases.
#[must_use]
pub fn standard_decommission_phases() -> Vec<DecommissionPhase> {
    vec![
        DecommissionPhase {
            phase_id: "DC-01-parity".into(),
            name: "Feature parity verification".into(),
            description: "Verify all NTM features have native FrankenTerm equivalents".into(),
            order: 0,
            gates: vec![
                GateCheck {
                    check_id: "DC-01-G1".into(),
                    description: "NTM parity corpus covers all NTM commands".into(),
                    passed: true,
                    evidence: "ft-3681t.8.1 closed — parity corpus complete".into(),
                },
                GateCheck {
                    check_id: "DC-01-G2".into(),
                    description: "Dual-run shadow comparison shows no regressions".into(),
                    passed: true,
                    evidence: "ft-3681t.8.2 closed — shadow comparator verified".into(),
                },
            ],
            rollback: RollbackPlan {
                steps: vec!["Re-enable NTM in PATH".into(), "Restore NTM config".into()],
                estimated_time_ms: 30_000,
                rehearsed: true,
            },
            complete: true,
        },
        DecommissionPhase {
            phase_id: "DC-02-import".into(),
            name: "Session/workflow import".into(),
            description: "Import existing NTM sessions, workflows, and configuration".into(),
            order: 1,
            gates: vec![GateCheck {
                check_id: "DC-02-G1".into(),
                description: "NTM importers handle all session/workflow formats".into(),
                passed: true,
                evidence: "ft-3681t.8.3 closed — importers verified".into(),
            }],
            rollback: RollbackPlan {
                steps: vec!["Exported sessions are retained; re-install NTM".into()],
                estimated_time_ms: 60_000,
                rehearsed: true,
            },
            complete: true,
        },
        DecommissionPhase {
            phase_id: "DC-03-canary".into(),
            name: "Canary operation without NTM".into(),
            description: "Operate canary cohort with NTM removed from PATH".into(),
            order: 2,
            gates: vec![
                GateCheck {
                    check_id: "DC-03-G1".into(),
                    description: "Canary cohort operates for soak period without NTM".into(),
                    passed: true,
                    evidence: "Canary rehearsal (ft-e34d9.10.8.3) completed successfully".into(),
                },
                GateCheck {
                    check_id: "DC-03-G2".into(),
                    description: "No operator escalations during canary period".into(),
                    passed: true,
                    evidence: "Zero escalations in 1-hour soak".into(),
                },
            ],
            rollback: RollbackPlan {
                steps: vec!["Restore NTM to PATH for canary agents".into()],
                estimated_time_ms: 10_000,
                rehearsed: true,
            },
            complete: true,
        },
        DecommissionPhase {
            phase_id: "DC-04-full-removal".into(),
            name: "Full NTM removal".into(),
            description: "Remove NTM from all fleet agents and operator workstations".into(),
            order: 3,
            gates: vec![
                GateCheck {
                    check_id: "DC-04-G1".into(),
                    description: "All NTM dependencies migrated or retired".into(),
                    passed: true,
                    evidence: "standard_ntm_dependencies() all Migrated".into(),
                },
                GateCheck {
                    check_id: "DC-04-G2".into(),
                    description: "Operator documentation is complete".into(),
                    passed: true,
                    evidence: "Documentation index coverage >= 100%".into(),
                },
            ],
            rollback: RollbackPlan {
                steps: vec![
                    "Re-install NTM via package manager".into(),
                    "Restore NTM configuration from backup".into(),
                    "Update PATH to include NTM".into(),
                ],
                estimated_time_ms: 120_000,
                rehearsed: false,
            },
            complete: false,
        },
    ]
}

// =============================================================================
// Documentation index
// =============================================================================

/// Type of operator documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DocCategory {
    /// Day-2 operational playbook.
    OperatorPlaybook,
    /// Contributor development guide.
    ContributorGuide,
    /// Architecture decision record.
    Adr,
    /// Troubleshooting/runbook.
    Runbook,
    /// API reference.
    ApiReference,
}

impl DocCategory {
    /// Human-readable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::OperatorPlaybook => "operator-playbook",
            Self::ContributorGuide => "contributor-guide",
            Self::Adr => "adr",
            Self::Runbook => "runbook",
            Self::ApiReference => "api-reference",
        }
    }
}

/// A single documentation entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocEntry {
    /// Document identifier.
    pub doc_id: String,
    /// Title.
    pub title: String,
    /// Category.
    pub category: DocCategory,
    /// File path (relative to repo root).
    pub path: String,
    /// Whether the document exists and is up to date.
    pub complete: bool,
    /// Topics covered.
    pub topics: Vec<String>,
}

/// Documentation coverage summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentationIndex {
    /// All documentation entries.
    pub entries: Vec<DocEntry>,
    /// Required topics that must be documented.
    pub required_topics: Vec<String>,
}

impl DocumentationIndex {
    /// Create an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            required_topics: Vec::new(),
        }
    }

    /// Add a documentation entry.
    pub fn add(&mut self, entry: DocEntry) {
        self.entries.push(entry);
    }

    /// Add a required topic.
    pub fn require_topic(&mut self, topic: impl Into<String>) {
        self.required_topics.push(topic.into());
    }

    /// Coverage rate (complete entries / total entries).
    #[must_use]
    pub fn coverage_rate(&self) -> f64 {
        if self.entries.is_empty() {
            return 0.0;
        }
        self.entries.iter().filter(|e| e.complete).count() as f64 / self.entries.len() as f64
    }

    /// Topics that are documented.
    #[must_use]
    pub fn documented_topics(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|e| e.complete)
            .flat_map(|e| e.topics.iter().map(|t| t.as_str()))
            .collect()
    }

    /// Required topics that are missing documentation.
    #[must_use]
    pub fn missing_topics(&self) -> Vec<&str> {
        let documented: Vec<&str> = self.documented_topics();
        self.required_topics
            .iter()
            .filter(|t| !documented.contains(&t.as_str()))
            .map(|t| t.as_str())
            .collect()
    }

    /// Whether all required topics are documented.
    #[must_use]
    pub fn all_topics_covered(&self) -> bool {
        self.missing_topics().is_empty()
    }
}

impl Default for DocumentationIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Standard documentation index for the NTM→FrankenTerm migration.
#[must_use]
pub fn standard_documentation_index() -> DocumentationIndex {
    let mut index = DocumentationIndex::new();

    // Required topics.
    for topic in &[
        "fleet-launch",
        "pane-management",
        "agent-swarm-orchestration",
        "session-persistence",
        "remote-ssh-access",
        "monitoring-health-checks",
        "policy-governance",
        "connector-development",
        "robot-api",
        "troubleshooting",
        "migration-from-ntm",
    ] {
        index.require_topic(*topic);
    }

    // Existing documentation.
    index.add(DocEntry {
        doc_id: "DOC-01".into(),
        title: "NTM→FrankenTerm Convergence Architecture".into(),
        category: DocCategory::Adr,
        path: "docs/ft-3681t-convergence-architecture.md".into(),
        complete: true,
        topics: vec!["migration-from-ntm".into(), "architecture".into()],
    });
    index.add(DocEntry {
        doc_id: "DOC-02".into(),
        title: "Fleet Launch and Swarm Orchestration".into(),
        category: DocCategory::OperatorPlaybook,
        path: "docs/operator/fleet-launch.md".into(),
        complete: true,
        topics: vec!["fleet-launch".into(), "agent-swarm-orchestration".into()],
    });
    index.add(DocEntry {
        doc_id: "DOC-03".into(),
        title: "Pane Management and Lifecycle".into(),
        category: DocCategory::OperatorPlaybook,
        path: "docs/operator/pane-management.md".into(),
        complete: true,
        topics: vec!["pane-management".into()],
    });
    index.add(DocEntry {
        doc_id: "DOC-04".into(),
        title: "Session Persistence and Restore".into(),
        category: DocCategory::OperatorPlaybook,
        path: "docs/operator/session-persistence.md".into(),
        complete: true,
        topics: vec!["session-persistence".into()],
    });
    index.add(DocEntry {
        doc_id: "DOC-05".into(),
        title: "Remote SSH Access".into(),
        category: DocCategory::OperatorPlaybook,
        path: "docs/operator/remote-ssh.md".into(),
        complete: true,
        topics: vec!["remote-ssh-access".into()],
    });
    index.add(DocEntry {
        doc_id: "DOC-06".into(),
        title: "Monitoring, Health Checks, and Watchdog".into(),
        category: DocCategory::OperatorPlaybook,
        path: "docs/operator/monitoring.md".into(),
        complete: true,
        topics: vec!["monitoring-health-checks".into()],
    });
    index.add(DocEntry {
        doc_id: "DOC-07".into(),
        title: "Policy Governance and Safety".into(),
        category: DocCategory::OperatorPlaybook,
        path: "docs/operator/policy-governance.md".into(),
        complete: true,
        topics: vec!["policy-governance".into()],
    });
    index.add(DocEntry {
        doc_id: "DOC-08".into(),
        title: "Connector Development Guide".into(),
        category: DocCategory::ContributorGuide,
        path: "docs/contributor/connector-development.md".into(),
        complete: true,
        topics: vec!["connector-development".into()],
    });
    index.add(DocEntry {
        doc_id: "DOC-09".into(),
        title: "Robot API Reference".into(),
        category: DocCategory::ApiReference,
        path: "docs/api/robot-api.md".into(),
        complete: true,
        topics: vec!["robot-api".into()],
    });
    index.add(DocEntry {
        doc_id: "DOC-10".into(),
        title: "Troubleshooting and Common Issues".into(),
        category: DocCategory::Runbook,
        path: "docs/runbooks/troubleshooting.md".into(),
        complete: true,
        topics: vec!["troubleshooting".into()],
    });

    index
}

// =============================================================================
// Decommission plan and audit
// =============================================================================

/// Reversibility policy for the decommission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReversibilityPolicy {
    /// Whether reversal is allowed.
    pub reversal_allowed: bool,
    /// Conditions under which reversal is permitted.
    pub conditions: Vec<String>,
    /// Who can authorize reversal.
    pub authorized_by: Vec<String>,
    /// Maximum time after decommission that reversal is supported (ms).
    pub reversal_window_ms: u64,
}

impl ReversibilityPolicy {
    /// Standard policy: reversible within 30 days by operator.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            reversal_allowed: true,
            conditions: vec![
                "Critical regression in native FrankenTerm discovered".into(),
                "Operator escalation for feature gap".into(),
                "Security incident requiring NTM fallback".into(),
            ],
            authorized_by: vec!["operator".into(), "admin".into()],
            reversal_window_ms: 30 * 24 * 3_600_000, // 30 days.
        }
    }

    /// Whether reversal is still within the allowed window.
    #[must_use]
    pub fn within_window(&self, decommission_at_ms: u64, now_ms: u64) -> bool {
        self.reversal_allowed && (now_ms - decommission_at_ms) <= self.reversal_window_ms
    }
}

/// A decommission audit entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Timestamp (epoch ms).
    pub timestamp_ms: u64,
    /// Action taken.
    pub action: String,
    /// Who performed it.
    pub actor: String,
    /// Phase that changed.
    pub phase_id: Option<String>,
    /// Previous state.
    pub previous_state: Option<String>,
    /// New state.
    pub new_state: Option<String>,
    /// Notes.
    pub notes: Option<String>,
}

/// Complete decommission plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecommissionPlan {
    /// Plan identifier.
    pub plan_id: String,
    /// NTM dependencies.
    pub dependencies: Vec<NtmDependency>,
    /// Decommission phases.
    pub phases: Vec<DecommissionPhase>,
    /// Documentation index.
    pub documentation: DocumentationIndex,
    /// Reversibility policy.
    pub reversibility: ReversibilityPolicy,
    /// Audit trail.
    pub audit: Vec<AuditEntry>,
    /// When the plan was created (epoch ms).
    pub created_at_ms: u64,
}

impl DecommissionPlan {
    /// Create the standard decommission plan.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            plan_id: "NTM-DECOM-001".into(),
            dependencies: standard_ntm_dependencies(),
            phases: standard_decommission_phases(),
            documentation: standard_documentation_index(),
            reversibility: ReversibilityPolicy::standard(),
            audit: Vec::new(),
            created_at_ms: 0,
        }
    }

    /// Record an audit entry.
    pub fn record_audit(&mut self, entry: AuditEntry) {
        self.audit.push(entry);
    }

    /// Count of active (not yet migrated) dependencies.
    #[must_use]
    pub fn active_dependency_count(&self) -> usize {
        self.dependencies
            .iter()
            .filter(|d| d.status.is_active())
            .count()
    }

    /// Count of migrated dependencies.
    #[must_use]
    pub fn migrated_dependency_count(&self) -> usize {
        self.dependencies
            .iter()
            .filter(|d| {
                matches!(
                    d.status,
                    DependencyStatus::Migrated | DependencyStatus::Retired
                )
            })
            .count()
    }

    /// Dependency migration rate.
    #[must_use]
    pub fn migration_rate(&self) -> f64 {
        if self.dependencies.is_empty() {
            return 0.0;
        }
        self.migrated_dependency_count() as f64 / self.dependencies.len() as f64
    }

    /// Current phase (first incomplete phase).
    #[must_use]
    pub fn current_phase(&self) -> Option<&DecommissionPhase> {
        self.phases.iter().find(|p| !p.complete)
    }

    /// Count of completed phases.
    #[must_use]
    pub fn completed_phases(&self) -> usize {
        self.phases.iter().filter(|p| p.complete).count()
    }

    /// Whether the decommission is ready for final execution.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.active_dependency_count() == 0
            && self.documentation.all_topics_covered()
            && self
                .phases
                .iter()
                .take(self.phases.len().saturating_sub(1))
                .all(|p| p.complete)
    }

    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push("=== NTM Decommission Plan ===".to_string());
        lines.push(format!("Plan: {}", self.plan_id));
        lines.push(String::new());

        lines.push("--- Dependencies ---".to_string());
        lines.push(format!(
            "  Migrated: {}/{} ({:.0}%)",
            self.migrated_dependency_count(),
            self.dependencies.len(),
            self.migration_rate() * 100.0
        ));
        for dep in &self.dependencies {
            lines.push(format!(
                "  [{}] {} — {}",
                dep.status.label(),
                dep.dep_id,
                dep.description
            ));
        }

        lines.push(String::new());
        lines.push("--- Phases ---".to_string());
        for phase in &self.phases {
            let icon = if phase.complete { "DONE" } else { "TODO" };
            lines.push(format!(
                "  [{}] {} — {} (gates: {:.0}%)",
                icon,
                phase.phase_id,
                phase.name,
                phase.gate_pass_rate() * 100.0
            ));
        }

        lines.push(String::new());
        lines.push("--- Documentation ---".to_string());
        lines.push(format!(
            "  Coverage: {:.0}%",
            self.documentation.coverage_rate() * 100.0
        ));
        let missing = self.documentation.missing_topics();
        if !missing.is_empty() {
            lines.push(format!("  Missing topics: {}", missing.join(", ")));
        }

        lines.push(String::new());
        lines.push("--- Reversibility ---".to_string());
        lines.push(format!(
            "  Reversal allowed: {}",
            self.reversibility.reversal_allowed
        ));
        lines.push(format!(
            "  Window: {} days",
            self.reversibility.reversal_window_ms / (24 * 3_600_000)
        ));

        lines.push(String::new());
        lines.push(format!(
            "Ready for execution: {}",
            if self.is_ready() { "YES" } else { "NO" }
        ));

        lines.join("\n")
    }

    /// Dependencies grouped by category.
    #[must_use]
    pub fn deps_by_category(&self) -> HashMap<String, Vec<&NtmDependency>> {
        let mut map: HashMap<String, Vec<&NtmDependency>> = HashMap::new();
        for dep in &self.dependencies {
            map.entry(dep.category.label().to_string())
                .or_default()
                .push(dep);
        }
        map
    }
}

// =============================================================================
// Decommission snapshot for telemetry
// =============================================================================

/// Telemetry snapshot for the decommission subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecommissionSnapshot {
    /// Total dependencies.
    pub total_dependencies: usize,
    /// Migrated dependencies.
    pub migrated_dependencies: usize,
    /// Active dependencies.
    pub active_dependencies: usize,
    /// Completed phases.
    pub completed_phases: usize,
    /// Total phases.
    pub total_phases: usize,
    /// Documentation coverage rate.
    pub doc_coverage_rate: f64,
    /// Missing documentation topics.
    pub missing_topics: Vec<String>,
    /// Whether ready for execution.
    pub ready: bool,
    /// Audit entry count.
    pub audit_entries: usize,
}

impl DecommissionPlan {
    /// Create a telemetry snapshot.
    #[must_use]
    pub fn snapshot(&self) -> DecommissionSnapshot {
        DecommissionSnapshot {
            total_dependencies: self.dependencies.len(),
            migrated_dependencies: self.migrated_dependency_count(),
            active_dependencies: self.active_dependency_count(),
            completed_phases: self.completed_phases(),
            total_phases: self.phases.len(),
            doc_coverage_rate: self.documentation.coverage_rate(),
            missing_topics: self
                .documentation
                .missing_topics()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ready: self.is_ready(),
            audit_entries: self.audit.len(),
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Dependency tests ---

    #[test]
    fn standard_dependencies_all_migrated() {
        let deps = standard_ntm_dependencies();
        assert!(deps.len() >= 7);
        for dep in &deps {
            assert_eq!(dep.status, DependencyStatus::Migrated);
            assert!(dep.migration_target.is_some());
            assert!(dep.migration_evidence.is_some());
        }
    }

    #[test]
    fn dependency_status_is_active() {
        assert!(DependencyStatus::Active.is_active());
        assert!(DependencyStatus::Migrating.is_active());
        assert!(!DependencyStatus::Migrated.is_active());
        assert!(!DependencyStatus::Retired.is_active());
    }

    #[test]
    fn dependency_constructors() {
        let active = NtmDependency::active("D1", "test", NtmComponentCategory::Monitoring);
        assert_eq!(active.status, DependencyStatus::Active);
        assert!(active.migration_target.is_none());

        let migrated = NtmDependency::migrated(
            "D2",
            "test",
            NtmComponentCategory::CliSurface,
            "target",
            "evidence",
        );
        assert_eq!(migrated.status, DependencyStatus::Migrated);
        assert_eq!(migrated.migration_target.as_deref(), Some("target"));
    }

    // --- Phase tests ---

    #[test]
    fn standard_phases_ordered() {
        let phases = standard_decommission_phases();
        for w in phases.windows(2) {
            assert!(w[0].order < w[1].order);
        }
    }

    #[test]
    fn phase_gate_pass_rate() {
        let phase = &standard_decommission_phases()[0];
        assert_eq!(phase.gate_pass_rate(), 1.0);
        assert!(phase.gates_pass());
    }

    #[test]
    fn phase_with_failing_gate() {
        let mut phase = standard_decommission_phases()[0].clone();
        phase.gates.push(GateCheck {
            check_id: "FAIL".into(),
            description: "fails".into(),
            passed: false,
            evidence: "not ready".into(),
        });
        assert!(!phase.gates_pass());
        assert!(phase.gate_pass_rate() < 1.0);
    }

    // --- Documentation index tests ---

    #[test]
    fn standard_docs_cover_all_topics() {
        let index = standard_documentation_index();
        assert!(
            index.all_topics_covered(),
            "missing: {:?}",
            index.missing_topics()
        );
        assert_eq!(index.coverage_rate(), 1.0);
    }

    #[test]
    fn empty_docs_index() {
        let index = DocumentationIndex::new();
        assert_eq!(index.coverage_rate(), 0.0);
        assert!(index.all_topics_covered()); // No required topics.
    }

    #[test]
    fn missing_topic_detection() {
        let mut index = DocumentationIndex::new();
        index.require_topic("fleet-launch");
        index.require_topic("monitoring");
        index.add(DocEntry {
            doc_id: "D1".into(),
            title: "Fleet".into(),
            category: DocCategory::OperatorPlaybook,
            path: "docs/fleet.md".into(),
            complete: true,
            topics: vec!["fleet-launch".into()],
        });
        assert!(!index.all_topics_covered());
        assert_eq!(index.missing_topics(), vec!["monitoring"]);
    }

    #[test]
    fn incomplete_doc_not_counted() {
        let mut index = DocumentationIndex::new();
        index.require_topic("topic-a");
        index.add(DocEntry {
            doc_id: "D1".into(),
            title: "Draft".into(),
            category: DocCategory::Runbook,
            path: "docs/draft.md".into(),
            complete: false,
            topics: vec!["topic-a".into()],
        });
        assert!(!index.all_topics_covered());
        assert_eq!(index.coverage_rate(), 0.0);
    }

    // --- Reversibility policy tests ---

    #[test]
    fn standard_policy_allows_reversal() {
        let policy = ReversibilityPolicy::standard();
        assert!(policy.reversal_allowed);
        assert!(!policy.conditions.is_empty());
    }

    #[test]
    fn within_window_check() {
        let policy = ReversibilityPolicy::standard();
        let decom_at = 1_000_000;
        let within = decom_at + 1_000_000; // 1 second later.
        let outside = decom_at + policy.reversal_window_ms + 1;
        assert!(policy.within_window(decom_at, within));
        assert!(!policy.within_window(decom_at, outside));
    }

    // --- Decommission plan tests ---

    #[test]
    fn standard_plan_creation() {
        let plan = DecommissionPlan::standard();
        assert_eq!(plan.plan_id, "NTM-DECOM-001");
        assert!(plan.dependencies.len() >= 7);
        assert!(plan.phases.len() >= 4);
    }

    #[test]
    fn standard_plan_all_deps_migrated() {
        let plan = DecommissionPlan::standard();
        assert_eq!(plan.active_dependency_count(), 0);
        assert_eq!(plan.migration_rate(), 1.0);
    }

    #[test]
    fn plan_with_active_dep_not_ready() {
        let mut plan = DecommissionPlan::standard();
        plan.dependencies.push(NtmDependency::active(
            "EXTRA",
            "extra dependency",
            NtmComponentCategory::Monitoring,
        ));
        assert_eq!(plan.active_dependency_count(), 1);
        assert!(!plan.is_ready());
    }

    #[test]
    fn plan_current_phase() {
        let plan = DecommissionPlan::standard();
        let current = plan.current_phase().expect("should have incomplete phase");
        assert_eq!(current.phase_id, "DC-04-full-removal");
    }

    #[test]
    fn plan_completed_phases_count() {
        let plan = DecommissionPlan::standard();
        assert_eq!(plan.completed_phases(), 3); // First 3 are complete.
    }

    #[test]
    fn plan_deps_by_category() {
        let plan = DecommissionPlan::standard();
        let by_cat = plan.deps_by_category();
        assert!(by_cat.contains_key("session-management"));
        assert!(by_cat.contains_key("cli-surface"));
    }

    #[test]
    fn plan_audit_trail() {
        let mut plan = DecommissionPlan::standard();
        plan.record_audit(AuditEntry {
            timestamp_ms: 1000,
            action: "phase-complete".into(),
            actor: "operator".into(),
            phase_id: Some("DC-01-parity".into()),
            previous_state: Some("incomplete".into()),
            new_state: Some("complete".into()),
            notes: None,
        });
        assert_eq!(plan.audit.len(), 1);
    }

    // --- Snapshot tests ---

    #[test]
    fn snapshot_reflects_plan_state() {
        let plan = DecommissionPlan::standard();
        let snap = plan.snapshot();
        assert_eq!(snap.total_dependencies, plan.dependencies.len());
        assert_eq!(snap.migrated_dependencies, plan.migrated_dependency_count());
        assert_eq!(snap.active_dependencies, 0);
        assert_eq!(snap.completed_phases, 3);
        assert_eq!(snap.total_phases, plan.phases.len());
        assert_eq!(snap.doc_coverage_rate, 1.0);
        assert!(snap.missing_topics.is_empty());
    }

    // --- Render tests ---

    #[test]
    fn render_summary_contains_key_info() {
        let plan = DecommissionPlan::standard();
        let summary = plan.render_summary();
        assert!(summary.contains("Decommission Plan"));
        assert!(summary.contains("NTM-DECOM-001"));
        assert!(summary.contains("Migrated: 7/7"));
        assert!(summary.contains("Coverage: 100%"));
        assert!(summary.contains("Reversal allowed: true"));
    }

    // --- Serde roundtrip tests ---

    #[test]
    fn plan_serde_roundtrip() {
        let plan = DecommissionPlan::standard();
        let json = serde_json::to_string(&plan).expect("serialize");
        let restored: DecommissionPlan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.plan_id, plan.plan_id);
        assert_eq!(restored.dependencies.len(), plan.dependencies.len());
        assert_eq!(restored.phases.len(), plan.phases.len());
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let plan = DecommissionPlan::standard();
        let snap = plan.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let restored: DecommissionSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.total_dependencies, snap.total_dependencies);
        assert_eq!(restored.ready, snap.ready);
    }

    // --- E2E lifecycle test ---

    #[test]
    fn e2e_decommission_lifecycle() {
        // 1. Create plan.
        let mut plan = DecommissionPlan::standard();
        assert_eq!(plan.active_dependency_count(), 0);
        assert_eq!(plan.migration_rate(), 1.0);

        // 2. Verify documentation coverage.
        assert!(plan.documentation.all_topics_covered());

        // 3. Verify phases 1-3 are complete.
        assert_eq!(plan.completed_phases(), 3);

        // 4. Current phase should be DC-04 (full removal).
        let current = plan.current_phase().unwrap();
        assert_eq!(current.phase_id, "DC-04-full-removal");
        assert!(!current.complete);

        // 5. Plan is ready (all deps migrated, docs complete, first 3 phases done).
        assert!(plan.is_ready());

        // 6. Record audit entry for execution.
        plan.record_audit(AuditEntry {
            timestamp_ms: 1000,
            action: "decommission-initiated".into(),
            actor: "PinkForge".into(),
            phase_id: Some("DC-04-full-removal".into()),
            previous_state: None,
            new_state: Some("executing".into()),
            notes: Some("All gates passed, proceeding with full removal".into()),
        });
        assert_eq!(plan.audit.len(), 1);

        // 7. Verify reversibility window.
        assert!(plan.reversibility.within_window(1000, 5000));

        // 8. Snapshot captures state.
        let snap = plan.snapshot();
        assert!(snap.ready);
        assert_eq!(snap.audit_entries, 1);
    }

    // --- Label uniqueness tests ---

    #[test]
    fn dependency_status_labels_unique() {
        let statuses = [
            DependencyStatus::Active,
            DependencyStatus::Migrating,
            DependencyStatus::Migrated,
            DependencyStatus::Retired,
        ];
        let labels: Vec<&str> = statuses.iter().map(|s| s.label()).collect();
        let mut deduped = labels.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(labels.len(), deduped.len());
    }

    #[test]
    fn component_category_labels_unique() {
        let cats = [
            NtmComponentCategory::SessionManagement,
            NtmComponentCategory::PaneOrchestration,
            NtmComponentCategory::SwarmLauncher,
            NtmComponentCategory::Configuration,
            NtmComponentCategory::RemoteAccess,
            NtmComponentCategory::Monitoring,
            NtmComponentCategory::CliSurface,
        ];
        let labels: Vec<&str> = cats.iter().map(|c| c.label()).collect();
        let mut deduped = labels.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(labels.len(), deduped.len());
    }

    #[test]
    fn doc_category_labels_unique() {
        let cats = [
            DocCategory::OperatorPlaybook,
            DocCategory::ContributorGuide,
            DocCategory::Adr,
            DocCategory::Runbook,
            DocCategory::ApiReference,
        ];
        let labels: Vec<&str> = cats.iter().map(|c| c.label()).collect();
        let mut deduped = labels.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(labels.len(), deduped.len());
    }
}
