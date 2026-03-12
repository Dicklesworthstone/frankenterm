//! Operator runbooks, tutorials, and decision-support overlays (ft-3681t.9.6).
//!
//! Provides embedded runbook definitions, tutorial flows, and role-aware
//! decision-support for operators executing swarm/connector operations
//! during both routine operations and incident response.
//!
//! # Architecture
//!
//! ```text
//! RunbookRegistry
//!   ├── Runbook[] (embedded operation guides)
//!   │     ├── RunbookStep[] (ordered steps with preconditions)
//!   │     │     ├── step_type (Action/Verify/Decision/Escalate)
//!   │     │     └── decision_support: Option<DecisionOverlay>
//!   │     └── applicability (WorkflowClass, OperatorRole)
//!   │
//!   ├── TutorialFlow[] (onboarding sequences)
//!   │     ├── prerequisite_knowledge
//!   │     └── TutorialStep[] with validation
//!   │
//!   └── DecisionOverlay
//!         ├── options with risk/benefit analysis
//!         ├── telemetry_refs (live data pointers)
//!         └── recommended_action
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Runbook types
// =============================================================================

/// Operator role for access control and content filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OperatorRole {
    /// New operator in training.
    Trainee,
    /// Standard operator.
    Operator,
    /// Senior operator with escalation authority.
    SeniorOperator,
    /// Admin with full access.
    Admin,
}

impl OperatorRole {
    /// Minimum role level (for ordering).
    #[must_use]
    pub fn level(&self) -> u8 {
        match self {
            Self::Trainee => 0,
            Self::Operator => 1,
            Self::SeniorOperator => 2,
            Self::Admin => 3,
        }
    }

    /// Whether this role has at least the given level.
    #[must_use]
    pub fn has_at_least(&self, required: &OperatorRole) -> bool {
        self.level() >= required.level()
    }
}

/// Type of step within a runbook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StepType {
    /// Execute an action (command, button click).
    Action,
    /// Verify a condition before proceeding.
    Verify,
    /// Make a decision based on current state.
    Decision,
    /// Escalate to a higher authority.
    Escalate,
    /// Observe and record state.
    Observe,
    /// Wait for a condition to be met.
    Wait,
}

/// A decision option presented in a decision overlay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionOption {
    /// Option identifier.
    pub option_id: String,
    /// Label.
    pub label: String,
    /// Description of what this option does.
    pub description: String,
    /// Risk level (0–100).
    pub risk_score: u8,
    /// Expected benefit description.
    pub benefit: String,
    /// Whether this is the recommended option.
    pub recommended: bool,
    /// Telemetry fields to check before choosing this option.
    pub telemetry_refs: Vec<String>,
}

/// Decision-support overlay attached to a runbook step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionOverlay {
    /// Context description.
    pub context: String,
    /// Available options.
    pub options: Vec<DecisionOption>,
    /// Policy references.
    pub policy_refs: Vec<String>,
    /// Live telemetry fields to display.
    pub telemetry_fields: Vec<String>,
}

impl DecisionOverlay {
    /// Get the recommended option, if any.
    #[must_use]
    pub fn recommended_option(&self) -> Option<&DecisionOption> {
        self.options.iter().find(|o| o.recommended)
    }

    /// Options sorted by risk (lowest first).
    #[must_use]
    pub fn options_by_risk(&self) -> Vec<&DecisionOption> {
        let mut sorted: Vec<&DecisionOption> = self.options.iter().collect();
        sorted.sort_by_key(|o| o.risk_score);
        sorted
    }
}

/// A single step in a runbook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunbookStep {
    /// Step identifier.
    pub step_id: String,
    /// Human-readable instruction.
    pub instruction: String,
    /// Step type.
    pub step_type: StepType,
    /// Precondition that must be true before executing this step.
    pub precondition: Option<String>,
    /// Expected outcome after this step.
    pub expected_outcome: Option<String>,
    /// Decision overlay for Decision-type steps.
    pub decision_support: Option<DecisionOverlay>,
    /// Minimum role required to execute this step.
    pub min_role: OperatorRole,
    /// Warning/caution text.
    pub caution: Option<String>,
    /// Estimated time to complete (seconds).
    pub estimated_seconds: u32,
}

/// Runbook applicability context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunbookApplicability {
    /// Workflow classes this runbook applies to.
    pub workflow_classes: Vec<String>,
    /// Minimum operator role.
    pub min_role: OperatorRole,
    /// Tags for filtering.
    pub tags: Vec<String>,
    /// Whether this runbook applies during incidents.
    pub incident_applicable: bool,
}

/// A complete runbook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Runbook {
    /// Runbook identifier.
    pub runbook_id: String,
    /// Title.
    pub title: String,
    /// Summary description.
    pub summary: String,
    /// Version.
    pub version: String,
    /// Applicability.
    pub applicability: RunbookApplicability,
    /// Ordered steps.
    pub steps: Vec<RunbookStep>,
    /// Related runbook IDs.
    pub related: Vec<String>,
}

impl Runbook {
    /// Total estimated time for all steps (seconds).
    #[must_use]
    pub fn estimated_total_seconds(&self) -> u32 {
        self.steps.iter().map(|s| s.estimated_seconds).sum()
    }

    /// Steps that require decision-making.
    #[must_use]
    pub fn decision_steps(&self) -> Vec<&RunbookStep> {
        self.steps
            .iter()
            .filter(|s| s.step_type == StepType::Decision)
            .collect()
    }

    /// Steps filtered by role access.
    #[must_use]
    pub fn steps_for_role(&self, role: &OperatorRole) -> Vec<&RunbookStep> {
        self.steps
            .iter()
            .filter(|s| role.has_at_least(&s.min_role))
            .collect()
    }

    /// Whether this runbook is applicable for a given role.
    #[must_use]
    pub fn is_applicable_for(&self, role: &OperatorRole) -> bool {
        role.has_at_least(&self.applicability.min_role)
    }
}

// =============================================================================
// Tutorial flows
// =============================================================================

/// A tutorial step with validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TutorialStep {
    /// Step identifier.
    pub step_id: String,
    /// Instruction text.
    pub instruction: String,
    /// Hint text (shown if the operator is stuck).
    pub hint: Option<String>,
    /// Validation description (what to check to confirm completion).
    pub validation: String,
    /// Estimated time (seconds).
    pub estimated_seconds: u32,
}

/// A complete tutorial flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TutorialFlow {
    /// Tutorial identifier.
    pub tutorial_id: String,
    /// Title.
    pub title: String,
    /// Summary.
    pub summary: String,
    /// Target role.
    pub target_role: OperatorRole,
    /// Prerequisite knowledge.
    pub prerequisites: Vec<String>,
    /// Ordered steps.
    pub steps: Vec<TutorialStep>,
    /// Tags.
    pub tags: Vec<String>,
}

impl TutorialFlow {
    /// Total estimated time (seconds).
    #[must_use]
    pub fn estimated_total_seconds(&self) -> u32 {
        self.steps.iter().map(|s| s.estimated_seconds).sum()
    }

    /// Number of steps.
    #[must_use]
    pub fn step_count(&self) -> usize {
        self.steps.len()
    }
}

// =============================================================================
// Runbook registry
// =============================================================================

/// Registry of all runbooks and tutorials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunbookRegistry {
    /// All runbooks.
    pub runbooks: Vec<Runbook>,
    /// All tutorials.
    pub tutorials: Vec<TutorialFlow>,
}

impl RunbookRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runbooks: Vec::new(),
            tutorials: Vec::new(),
        }
    }

    /// Add a runbook.
    pub fn add_runbook(&mut self, runbook: Runbook) {
        self.runbooks.push(runbook);
    }

    /// Add a tutorial.
    pub fn add_tutorial(&mut self, tutorial: TutorialFlow) {
        self.tutorials.push(tutorial);
    }

    /// Find runbooks applicable for a given role.
    #[must_use]
    pub fn runbooks_for_role(&self, role: &OperatorRole) -> Vec<&Runbook> {
        self.runbooks
            .iter()
            .filter(|r| r.is_applicable_for(role))
            .collect()
    }

    /// Find runbooks applicable for incidents.
    #[must_use]
    pub fn incident_runbooks(&self) -> Vec<&Runbook> {
        self.runbooks
            .iter()
            .filter(|r| r.applicability.incident_applicable)
            .collect()
    }

    /// Find runbooks by tag.
    #[must_use]
    pub fn runbooks_by_tag(&self, tag: &str) -> Vec<&Runbook> {
        self.runbooks
            .iter()
            .filter(|r| r.applicability.tags.iter().any(|t| t == tag))
            .collect()
    }

    /// Find tutorials for a given role.
    #[must_use]
    pub fn tutorials_for_role(&self, role: &OperatorRole) -> Vec<&TutorialFlow> {
        self.tutorials
            .iter()
            .filter(|t| role.has_at_least(&t.target_role))
            .collect()
    }

    /// Total runbook count.
    #[must_use]
    pub fn runbook_count(&self) -> usize {
        self.runbooks.len()
    }

    /// Total tutorial count.
    #[must_use]
    pub fn tutorial_count(&self) -> usize {
        self.tutorials.len()
    }

    /// Coverage: unique workflow tags covered by runbooks.
    #[must_use]
    pub fn workflow_coverage(&self) -> Vec<String> {
        let mut tags: Vec<String> = self
            .runbooks
            .iter()
            .flat_map(|r| r.applicability.workflow_classes.iter().cloned())
            .collect();
        tags.sort();
        tags.dedup();
        tags
    }

    /// Registry telemetry snapshot.
    #[must_use]
    pub fn snapshot(&self) -> RegistrySnapshot {
        let mut steps_by_type: HashMap<String, usize> = HashMap::new();
        for rb in &self.runbooks {
            for step in &rb.steps {
                let key = format!("{:?}", step.step_type);
                *steps_by_type.entry(key).or_insert(0) += 1;
            }
        }

        RegistrySnapshot {
            runbook_count: self.runbooks.len(),
            tutorial_count: self.tutorials.len(),
            total_runbook_steps: self.runbooks.iter().map(|r| r.steps.len()).sum(),
            total_tutorial_steps: self.tutorials.iter().map(|t| t.steps.len()).sum(),
            incident_runbook_count: self.incident_runbooks().len(),
            workflow_classes_covered: self.workflow_coverage().len(),
            decision_points: self.runbooks.iter().map(|r| r.decision_steps().len()).sum(),
            steps_by_type,
        }
    }
}

impl Default for RunbookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Telemetry snapshot for the runbook registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrySnapshot {
    /// Total runbooks.
    pub runbook_count: usize,
    /// Total tutorials.
    pub tutorial_count: usize,
    /// Total runbook steps.
    pub total_runbook_steps: usize,
    /// Total tutorial steps.
    pub total_tutorial_steps: usize,
    /// Incident-applicable runbooks.
    pub incident_runbook_count: usize,
    /// Unique workflow classes covered.
    pub workflow_classes_covered: usize,
    /// Total decision points across all runbooks.
    pub decision_points: usize,
    /// Steps by type.
    pub steps_by_type: HashMap<String, usize>,
}

// =============================================================================
// Standard runbooks catalog
// =============================================================================

/// Build the standard runbook registry with all built-in runbooks and tutorials.
#[must_use]
pub fn standard_registry() -> RunbookRegistry {
    let mut registry = RunbookRegistry::new();

    // --- Runbook: Fleet Launch ---
    registry.add_runbook(Runbook {
        runbook_id: "RB-001-fleet-launch".into(),
        title: "Fleet Launch Procedure".into(),
        summary: "Step-by-step guide for launching an agent swarm fleet".into(),
        version: "1.0".into(),
        applicability: RunbookApplicability {
            workflow_classes: vec!["launch".into(), "swarm".into()],
            min_role: OperatorRole::Operator,
            tags: vec!["launch".into(), "swarm".into(), "routine".into()],
            incident_applicable: false,
        },
        steps: vec![
            RunbookStep {
                step_id: "launch-01".into(),
                instruction: "Verify system resources (CPU, memory, disk) are within budget".into(),
                step_type: StepType::Verify,
                precondition: None,
                expected_outcome: Some("All resource metrics green".into()),
                decision_support: None,
                min_role: OperatorRole::Operator,
                caution: None,
                estimated_seconds: 30,
            },
            RunbookStep {
                step_id: "launch-02".into(),
                instruction: "Select fleet configuration profile".into(),
                step_type: StepType::Decision,
                precondition: Some("Resources verified".into()),
                expected_outcome: Some("Profile selected".into()),
                decision_support: Some(DecisionOverlay {
                    context: "Choose fleet size based on available resources and task complexity"
                        .into(),
                    options: vec![
                        DecisionOption {
                            option_id: "small".into(),
                            label: "Small Fleet (4-8 agents)".into(),
                            description: "Low resource usage, suitable for focused tasks".into(),
                            risk_score: 10,
                            benefit: "Minimal resource impact".into(),
                            recommended: false,
                            telemetry_refs: vec![
                                "cpu_utilization".into(),
                                "memory_utilization".into(),
                            ],
                        },
                        DecisionOption {
                            option_id: "medium".into(),
                            label: "Medium Fleet (16-32 agents)".into(),
                            description: "Balanced resource usage, suitable for parallel work"
                                .into(),
                            risk_score: 30,
                            benefit: "Good parallelism with manageable overhead".into(),
                            recommended: true,
                            telemetry_refs: vec![
                                "cpu_utilization".into(),
                                "memory_utilization".into(),
                            ],
                        },
                        DecisionOption {
                            option_id: "large".into(),
                            label: "Large Fleet (64+ agents)".into(),
                            description: "High resource usage, maximum parallelism".into(),
                            risk_score: 60,
                            benefit: "Maximum throughput for large tasks".into(),
                            recommended: false,
                            telemetry_refs: vec![
                                "cpu_utilization".into(),
                                "memory_utilization".into(),
                                "io_pressure".into(),
                            ],
                        },
                    ],
                    policy_refs: vec!["capacity_governor".into()],
                    telemetry_fields: vec![
                        "cpu_utilization".into(),
                        "memory_utilization".into(),
                        "active_panes".into(),
                    ],
                }),
                min_role: OperatorRole::Operator,
                caution: Some("Large fleets may trigger capacity governor throttling".into()),
                estimated_seconds: 60,
            },
            RunbookStep {
                step_id: "launch-03".into(),
                instruction: "Execute ft launch with selected profile".into(),
                step_type: StepType::Action,
                precondition: Some("Profile selected".into()),
                expected_outcome: Some("Fleet launched, all panes visible".into()),
                decision_support: None,
                min_role: OperatorRole::Operator,
                caution: None,
                estimated_seconds: 120,
            },
            RunbookStep {
                step_id: "launch-04".into(),
                instruction: "Verify all panes are active and healthy in dashboard".into(),
                step_type: StepType::Verify,
                precondition: Some("Fleet launched".into()),
                expected_outcome: Some("All panes show Active state".into()),
                decision_support: None,
                min_role: OperatorRole::Operator,
                caution: None,
                estimated_seconds: 30,
            },
        ],
        related: vec!["RB-002-emergency-response".into()],
    });

    // --- Runbook: Emergency Response ---
    registry.add_runbook(Runbook {
        runbook_id: "RB-002-emergency-response".into(),
        title: "Emergency Response Procedure".into(),
        summary: "Incident response runbook for critical fleet issues".into(),
        version: "1.0".into(),
        applicability: RunbookApplicability {
            workflow_classes: vec!["incident".into(), "intervention".into()],
            min_role: OperatorRole::Operator,
            tags: vec!["incident".into(), "emergency".into(), "critical".into()],
            incident_applicable: true,
        },
        steps: vec![
            RunbookStep {
                step_id: "emer-01".into(),
                instruction: "Assess scope: check fleet dashboard for affected panes".into(),
                step_type: StepType::Observe,
                precondition: None,
                expected_outcome: Some("Affected panes identified".into()),
                decision_support: None,
                min_role: OperatorRole::Operator,
                caution: None,
                estimated_seconds: 30,
            },
            RunbookStep {
                step_id: "emer-02".into(),
                instruction: "Decide: emergency stop or targeted intervention".into(),
                step_type: StepType::Decision,
                precondition: Some("Scope assessed".into()),
                expected_outcome: Some("Response strategy chosen".into()),
                decision_support: Some(DecisionOverlay {
                    context: "Choose response based on blast radius and severity".into(),
                    options: vec![
                        DecisionOption {
                            option_id: "targeted".into(),
                            label: "Targeted Intervention".into(),
                            description: "Pause/quarantine affected panes only".into(),
                            risk_score: 20,
                            benefit: "Minimal disruption to unaffected agents".into(),
                            recommended: true,
                            telemetry_refs: vec!["affected_pane_count".into()],
                        },
                        DecisionOption {
                            option_id: "global-stop".into(),
                            label: "Global Emergency Stop".into(),
                            description: "Stop all panes immediately".into(),
                            risk_score: 80,
                            benefit: "Immediate containment of unknown blast radius".into(),
                            recommended: false,
                            telemetry_refs: vec!["fleet_health".into(), "error_rate".into()],
                        },
                    ],
                    policy_refs: vec!["intervention_console".into(), "kill_switch".into()],
                    telemetry_fields: vec!["fleet_health".into(), "error_rate".into(), "affected_pane_count".into()],
                }),
                min_role: OperatorRole::Operator,
                caution: Some("Global stop affects all running agents — use only when blast radius is unknown".into()),
                estimated_seconds: 30,
            },
            RunbookStep {
                step_id: "emer-03".into(),
                instruction: "Execute chosen response".into(),
                step_type: StepType::Action,
                precondition: Some("Response strategy chosen".into()),
                expected_outcome: Some("Affected operations stopped/contained".into()),
                decision_support: None,
                min_role: OperatorRole::Operator,
                caution: None,
                estimated_seconds: 60,
            },
            RunbookStep {
                step_id: "emer-04".into(),
                instruction: "Verify containment: confirm affected panes are paused/quarantined".into(),
                step_type: StepType::Verify,
                precondition: Some("Response executed".into()),
                expected_outcome: Some("All affected panes show Paused/Quarantined state".into()),
                decision_support: None,
                min_role: OperatorRole::Operator,
                caution: None,
                estimated_seconds: 30,
            },
            RunbookStep {
                step_id: "emer-05".into(),
                instruction: "Escalate to senior operator if root cause unknown".into(),
                step_type: StepType::Escalate,
                precondition: Some("Containment verified".into()),
                expected_outcome: Some("Senior operator notified".into()),
                decision_support: None,
                min_role: OperatorRole::Operator,
                caution: None,
                estimated_seconds: 60,
            },
        ],
        related: vec!["RB-001-fleet-launch".into()],
    });

    // --- Runbook: Context Budget Management ---
    registry.add_runbook(Runbook {
        runbook_id: "RB-003-context-budget".into(),
        title: "Context Budget Management".into(),
        summary: "Managing agent context windows and compaction".into(),
        version: "1.0".into(),
        applicability: RunbookApplicability {
            workflow_classes: vec!["context".into(), "maintenance".into()],
            min_role: OperatorRole::Operator,
            tags: vec!["context".into(), "compaction".into(), "routine".into()],
            incident_applicable: false,
        },
        steps: vec![
            RunbookStep {
                step_id: "ctx-01".into(),
                instruction:
                    "Check context budget dashboard for panes in Yellow/Red/Black pressure".into(),
                step_type: StepType::Observe,
                precondition: None,
                expected_outcome: Some("Pressure tier distribution known".into()),
                decision_support: None,
                min_role: OperatorRole::Operator,
                caution: None,
                estimated_seconds: 20,
            },
            RunbookStep {
                step_id: "ctx-02".into(),
                instruction: "For Red/Black panes, follow recovery guidance".into(),
                step_type: StepType::Action,
                precondition: Some("Pressure tiers assessed".into()),
                expected_outcome: Some("Recovery actions applied".into()),
                decision_support: None,
                min_role: OperatorRole::Operator,
                caution: Some("Black pressure panes may need session rotation".into()),
                estimated_seconds: 120,
            },
        ],
        related: vec![],
    });

    // --- Tutorial: Getting Started ---
    registry.add_tutorial(TutorialFlow {
        tutorial_id: "TUT-001-getting-started".into(),
        title: "Getting Started with FrankenTerm".into(),
        summary: "First-time setup and basic fleet operations".into(),
        target_role: OperatorRole::Trainee,
        prerequisites: vec![],
        steps: vec![
            TutorialStep {
                step_id: "gs-01".into(),
                instruction: "Open a terminal and run `ft status` to verify installation".into(),
                hint: Some("If ft is not found, check PATH includes ~/.cargo/bin".into()),
                validation: "Output shows FrankenTerm version and system info".into(),
                estimated_seconds: 30,
            },
            TutorialStep {
                step_id: "gs-02".into(),
                instruction: "Launch a small fleet with `ft launch --profile small`".into(),
                hint: Some("Use --dry-run first to preview what will be created".into()),
                validation: "Fleet launches with 4 panes visible".into(),
                estimated_seconds: 60,
            },
            TutorialStep {
                step_id: "gs-03".into(),
                instruction: "Check fleet health with `ft status --fleet`".into(),
                hint: None,
                validation: "All panes show Active status".into(),
                estimated_seconds: 20,
            },
        ],
        tags: vec!["onboarding".into(), "basics".into()],
    });

    // --- Tutorial: Incident Response ---
    registry.add_tutorial(TutorialFlow {
        tutorial_id: "TUT-002-incident-response".into(),
        title: "Incident Response Training".into(),
        summary: "Practice detecting and responding to fleet incidents".into(),
        target_role: OperatorRole::Operator,
        prerequisites: vec!["TUT-001-getting-started".into()],
        steps: vec![
            TutorialStep {
                step_id: "ir-01".into(),
                instruction: "Open the fleet dashboard and identify the simulated error".into(),
                hint: Some("Look for panes with Red/Black health indicators".into()),
                validation: "Identified the failing pane and error type".into(),
                estimated_seconds: 30,
            },
            TutorialStep {
                step_id: "ir-02".into(),
                instruction: "Use the intervention console to pause the failing pane".into(),
                hint: Some("Run `ft robot pause-pane --pane-id <id>`".into()),
                validation: "Pane shows Paused state".into(),
                estimated_seconds: 60,
            },
            TutorialStep {
                step_id: "ir-03".into(),
                instruction: "Review the explainability trace for the incident".into(),
                hint: None,
                validation: "Root cause identified in trace output".into(),
                estimated_seconds: 60,
            },
        ],
        tags: vec!["incident".into(), "training".into()],
    });

    registry
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Operator role tests ---

    #[test]
    fn role_ordering() {
        assert!(OperatorRole::Trainee.level() < OperatorRole::Operator.level());
        assert!(OperatorRole::Operator.level() < OperatorRole::SeniorOperator.level());
        assert!(OperatorRole::SeniorOperator.level() < OperatorRole::Admin.level());
    }

    #[test]
    fn role_has_at_least() {
        assert!(OperatorRole::Admin.has_at_least(&OperatorRole::Trainee));
        assert!(OperatorRole::Operator.has_at_least(&OperatorRole::Operator));
        assert!(!OperatorRole::Trainee.has_at_least(&OperatorRole::Operator));
    }

    // --- Decision overlay tests ---

    #[test]
    fn decision_overlay_recommended() {
        let overlay = DecisionOverlay {
            context: "test".into(),
            options: vec![
                DecisionOption {
                    option_id: "a".into(),
                    label: "A".into(),
                    description: "option a".into(),
                    risk_score: 50,
                    benefit: "high".into(),
                    recommended: false,
                    telemetry_refs: vec![],
                },
                DecisionOption {
                    option_id: "b".into(),
                    label: "B".into(),
                    description: "option b".into(),
                    risk_score: 20,
                    benefit: "medium".into(),
                    recommended: true,
                    telemetry_refs: vec![],
                },
            ],
            policy_refs: vec![],
            telemetry_fields: vec![],
        };

        let rec = overlay
            .recommended_option()
            .expect("should have recommendation");
        assert_eq!(rec.option_id, "b");
    }

    #[test]
    fn decision_overlay_risk_sorted() {
        let overlay = DecisionOverlay {
            context: "test".into(),
            options: vec![
                DecisionOption {
                    option_id: "high".into(),
                    label: "High".into(),
                    description: "high risk".into(),
                    risk_score: 80,
                    benefit: "big".into(),
                    recommended: false,
                    telemetry_refs: vec![],
                },
                DecisionOption {
                    option_id: "low".into(),
                    label: "Low".into(),
                    description: "low risk".into(),
                    risk_score: 10,
                    benefit: "small".into(),
                    recommended: true,
                    telemetry_refs: vec![],
                },
            ],
            policy_refs: vec![],
            telemetry_fields: vec![],
        };

        let sorted = overlay.options_by_risk();
        assert_eq!(sorted[0].option_id, "low");
        assert_eq!(sorted[1].option_id, "high");
    }

    // --- Runbook tests ---

    #[test]
    fn standard_registry_has_runbooks_and_tutorials() {
        let registry = standard_registry();
        assert!(registry.runbook_count() >= 3);
        assert!(registry.tutorial_count() >= 2);
    }

    #[test]
    fn runbook_estimated_time() {
        let registry = standard_registry();
        let launch = registry
            .runbooks
            .iter()
            .find(|r| r.runbook_id == "RB-001-fleet-launch")
            .unwrap();
        assert!(launch.estimated_total_seconds() > 0);
    }

    #[test]
    fn runbook_decision_steps() {
        let registry = standard_registry();
        let launch = registry
            .runbooks
            .iter()
            .find(|r| r.runbook_id == "RB-001-fleet-launch")
            .unwrap();
        let decisions = launch.decision_steps();
        assert!(!decisions.is_empty());
        assert!(decisions[0].decision_support.is_some());
    }

    #[test]
    fn runbook_role_filtering() {
        let registry = standard_registry();
        let launch = registry
            .runbooks
            .iter()
            .find(|r| r.runbook_id == "RB-001-fleet-launch")
            .unwrap();

        // Operator can access all steps.
        let op_steps = launch.steps_for_role(&OperatorRole::Operator);
        assert_eq!(op_steps.len(), launch.steps.len());

        // Trainee cannot access operator-level runbook.
        assert!(!launch.is_applicable_for(&OperatorRole::Trainee));
    }

    #[test]
    fn registry_incident_runbooks() {
        let registry = standard_registry();
        let incident = registry.incident_runbooks();
        assert!(!incident.is_empty());
        assert!(
            incident
                .iter()
                .any(|r| r.runbook_id == "RB-002-emergency-response")
        );
    }

    #[test]
    fn registry_runbooks_by_tag() {
        let registry = standard_registry();
        let launch = registry.runbooks_by_tag("launch");
        assert!(!launch.is_empty());

        let emergency = registry.runbooks_by_tag("emergency");
        assert!(!emergency.is_empty());
    }

    #[test]
    fn registry_for_role() {
        let registry = standard_registry();

        // Operator sees all runbooks.
        let op_runbooks = registry.runbooks_for_role(&OperatorRole::Operator);
        assert_eq!(op_runbooks.len(), registry.runbook_count());

        // Admin sees all runbooks.
        let admin_runbooks = registry.runbooks_for_role(&OperatorRole::Admin);
        assert_eq!(admin_runbooks.len(), registry.runbook_count());
    }

    #[test]
    fn registry_tutorials_for_role() {
        let registry = standard_registry();
        let trainee_tuts = registry.tutorials_for_role(&OperatorRole::Trainee);
        assert!(!trainee_tuts.is_empty());

        let op_tuts = registry.tutorials_for_role(&OperatorRole::Operator);
        assert!(op_tuts.len() >= trainee_tuts.len());
    }

    #[test]
    fn registry_workflow_coverage() {
        let registry = standard_registry();
        let coverage = registry.workflow_coverage();
        assert!(coverage.contains(&"launch".to_string()));
        assert!(coverage.contains(&"incident".to_string()));
    }

    // --- Tutorial tests ---

    #[test]
    fn tutorial_step_count() {
        let registry = standard_registry();
        let gs = registry
            .tutorials
            .iter()
            .find(|t| t.tutorial_id == "TUT-001-getting-started")
            .unwrap();
        assert_eq!(gs.step_count(), 3);
        assert!(gs.estimated_total_seconds() > 0);
    }

    // --- Snapshot tests ---

    #[test]
    fn registry_snapshot() {
        let registry = standard_registry();
        let snap = registry.snapshot();
        assert_eq!(snap.runbook_count, registry.runbook_count());
        assert_eq!(snap.tutorial_count, registry.tutorial_count());
        assert!(snap.total_runbook_steps > 0);
        assert!(snap.total_tutorial_steps > 0);
        assert!(snap.incident_runbook_count > 0);
        assert!(snap.decision_points > 0);
        assert!(snap.workflow_classes_covered > 0);
    }

    // --- Serde roundtrip tests ---

    #[test]
    fn registry_serde_roundtrip() {
        let registry = standard_registry();
        let json = serde_json::to_string(&registry).expect("serialize");
        let restored: RunbookRegistry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.runbook_count(), registry.runbook_count());
        assert_eq!(restored.tutorial_count(), registry.tutorial_count());
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let registry = standard_registry();
        let snap = registry.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let restored: RegistrySnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.runbook_count, snap.runbook_count);
        assert_eq!(restored.decision_points, snap.decision_points);
    }

    // --- E2E lifecycle test ---

    #[test]
    fn e2e_runbook_lifecycle() {
        // 1. Load registry.
        let registry = standard_registry();

        // 2. Find applicable runbooks for an operator during an incident.
        let incident_rbs = registry.incident_runbooks();
        assert!(!incident_rbs.is_empty());

        // 3. Select the emergency response runbook.
        let emergency = incident_rbs
            .iter()
            .find(|r| r.runbook_id == "RB-002-emergency-response")
            .expect("should have emergency runbook");

        // 4. Verify decision support is available.
        let decisions = emergency.decision_steps();
        assert!(!decisions.is_empty());

        let overlay = decisions[0]
            .decision_support
            .as_ref()
            .expect("should have overlay");
        let recommended = overlay
            .recommended_option()
            .expect("should have recommendation");
        assert_eq!(recommended.option_id, "targeted");

        // 5. Verify risk-sorted options.
        let by_risk = overlay.options_by_risk();
        assert!(by_risk[0].risk_score <= by_risk[1].risk_score);

        // 6. Verify all steps are accessible to operator.
        let op_steps = emergency.steps_for_role(&OperatorRole::Operator);
        assert_eq!(op_steps.len(), emergency.steps.len());

        // 7. Verify related runbooks link.
        assert!(!emergency.related.is_empty());

        // 8. Check telemetry snapshot.
        let snap = registry.snapshot();
        assert!(snap.decision_points > 0);
        assert!(snap.incident_runbook_count > 0);
    }

    #[test]
    fn e2e_tutorial_onboarding() {
        let registry = standard_registry();

        // Trainee starts with getting started tutorial.
        let trainee_tuts = registry.tutorials_for_role(&OperatorRole::Trainee);
        assert!(!trainee_tuts.is_empty());

        let gs = trainee_tuts
            .iter()
            .find(|t| t.tutorial_id == "TUT-001-getting-started")
            .expect("should have getting started");
        assert!(gs.prerequisites.is_empty());

        // After completing getting started, can proceed to incident response.
        let op_tuts = registry.tutorials_for_role(&OperatorRole::Operator);
        let ir = op_tuts
            .iter()
            .find(|t| t.tutorial_id == "TUT-002-incident-response")
            .expect("should have incident response");
        assert!(
            ir.prerequisites
                .contains(&"TUT-001-getting-started".to_string())
        );
    }
}
