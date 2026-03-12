//! Staged cutover playbook with rollback gates (ft-3681t.8.4).
//!
//! Orchestrates the multi-stage migration cutover from NTM to FrankenTerm-native
//! control surfaces with objective gate evaluation, rollback triggers, and
//! evidence collection at each stage transition.
//!
//! # Architecture
//!
//! ```text
//! CutoverPlaybook
//!   ├── CutoverStage (Preflight → Shadow → Canary → Progressive → Default)
//!   ├── StageGate[] (objective criteria per stage transition)
//!   ├── StageTransition[] (recorded transitions with evidence)
//!   ├── RollbackPolicy (trigger conditions + procedure)
//!   ├── ApprovalRecord[] (operator sign-offs)
//!   └── PlaybookSnapshot (serializable state)
//!
//! Composition:
//!   canary_rehearsal  → CohortDefinition, PromotionCriteria, RollbackTrigger
//!   cutover_evidence  → EvidencePackage, GoNoGoVerdict, SoakOutcome
//!   soak_confidence   → ConfidenceGate, ConfidenceVerdict
//! ```
//!
//! # Usage
//!
//! ```rust
//! use frankenterm_core::cutover_playbook::*;
//!
//! let mut playbook = CutoverPlaybook::new("ntm-to-ft", 1);
//!
//! // Register gates for preflight
//! playbook.register_gate(StageGate::new("G-01", GateCategory::Parity, "blocking parity 100%"));
//!
//! // Evaluate and advance
//! playbook.pass_gate("G-01", "all 47 blocking scenarios pass");
//! let result = playbook.try_advance(1000, "PinkForge");
//! assert!(result.advanced);
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Cutover stages
// =============================================================================

/// The five stages of the cutover playbook (maps to doc stages 0–4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CutoverStage {
    /// Stage 0: Verify prerequisites and freeze scope.
    Preflight,
    /// Stage 1: Run NTM and ft in parallel, ft as observer only.
    Shadow,
    /// Stage 2: Route a small low-risk cohort to ft-native execution.
    Canary,
    /// Stage 3: Increase traffic share in controlled increments.
    Progressive,
    /// Stage 4: Make ft-native path default for migration scope.
    Default,
}

impl CutoverStage {
    /// All stages in order.
    pub const ALL: &'static [CutoverStage] = &[
        Self::Preflight,
        Self::Shadow,
        Self::Canary,
        Self::Progressive,
        Self::Default,
    ];

    /// Numeric stage index (0–4).
    #[must_use]
    pub fn index(&self) -> u32 {
        match self {
            Self::Preflight => 0,
            Self::Shadow => 1,
            Self::Canary => 2,
            Self::Progressive => 3,
            Self::Default => 4,
        }
    }

    /// Human label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Preflight => "Preflight Readiness",
            Self::Shadow => "Shadow-Only Verification",
            Self::Canary => "Canary Cutover",
            Self::Progressive => "Progressive Expansion",
            Self::Default => "Default Cutover",
        }
    }

    /// Next stage, if any.
    #[must_use]
    pub fn next(&self) -> Option<CutoverStage> {
        match self {
            Self::Preflight => Some(Self::Shadow),
            Self::Shadow => Some(Self::Canary),
            Self::Canary => Some(Self::Progressive),
            Self::Progressive => Some(Self::Default),
            Self::Default => None,
        }
    }

    /// Previous stage, if any.
    #[must_use]
    pub fn previous(&self) -> Option<CutoverStage> {
        match self {
            Self::Preflight => None,
            Self::Shadow => Some(Self::Preflight),
            Self::Canary => Some(Self::Shadow),
            Self::Progressive => Some(Self::Canary),
            Self::Default => Some(Self::Progressive),
        }
    }
}

// =============================================================================
// Gate definitions
// =============================================================================

/// Category of a stage gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GateCategory {
    /// Functional parity (blocking scenario pass rate).
    Parity,
    /// Envelope/contract stability (schema, ordering, idempotency).
    Contract,
    /// Divergence within budget across shadow windows.
    Divergence,
    /// Policy safety (no ungated mutations).
    PolicySafety,
    /// Rollback readiness (drill pass within window).
    RollbackReadiness,
    /// Performance (latency, throughput within thresholds).
    Performance,
    /// Operator/approval sign-off.
    Approval,
    /// Soak confidence gate.
    SoakConfidence,
}

impl GateCategory {
    /// Human label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Parity => "parity",
            Self::Contract => "contract stability",
            Self::Divergence => "divergence budget",
            Self::PolicySafety => "policy safety",
            Self::RollbackReadiness => "rollback readiness",
            Self::Performance => "performance",
            Self::Approval => "approval",
            Self::SoakConfidence => "soak confidence",
        }
    }
}

/// An objective gate criterion that must pass before stage transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageGate {
    /// Gate identifier (e.g., "G-01").
    pub gate_id: String,
    /// Category of check.
    pub category: GateCategory,
    /// Human-readable description of the threshold.
    pub description: String,
    /// Which stage transition this gate guards (advance FROM this stage).
    pub stage: CutoverStage,
    /// Whether this gate is blocking (must pass) or advisory.
    pub blocking: bool,
    /// Current pass/fail state.
    pub passed: bool,
    /// Evidence note recorded when gate was evaluated.
    pub evidence: String,
    /// When the gate was last evaluated (epoch ms).
    pub evaluated_at_ms: u64,
}

impl StageGate {
    /// Create a new blocking gate for the current playbook stage.
    #[must_use]
    pub fn new(
        gate_id: impl Into<String>,
        category: GateCategory,
        description: impl Into<String>,
    ) -> Self {
        Self {
            gate_id: gate_id.into(),
            category,
            description: description.into(),
            stage: CutoverStage::Preflight,
            blocking: true,
            passed: false,
            evidence: String::new(),
            evaluated_at_ms: 0,
        }
    }

    /// Set the stage this gate guards.
    #[must_use]
    pub fn for_stage(mut self, stage: CutoverStage) -> Self {
        self.stage = stage;
        self
    }

    /// Mark as advisory (non-blocking).
    #[must_use]
    pub fn advisory(mut self) -> Self {
        self.blocking = false;
        self
    }
}

// =============================================================================
// Rollback
// =============================================================================

/// A recorded rollback event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackEvent {
    /// What triggered the rollback.
    pub trigger_description: String,
    /// Stage at which rollback was initiated.
    pub from_stage: CutoverStage,
    /// Stage rolled back to.
    pub to_stage: CutoverStage,
    /// When the rollback started (epoch ms).
    pub initiated_at_ms: u64,
    /// When recovery was confirmed (epoch ms, 0 if pending).
    pub recovered_at_ms: u64,
    /// Who initiated the rollback.
    pub initiated_by: String,
    /// Free-form notes.
    pub notes: String,
}

impl RollbackEvent {
    /// Whether recovery is confirmed.
    #[must_use]
    pub fn is_recovered(&self) -> bool {
        self.recovered_at_ms > 0
    }

    /// Recovery duration in ms, or None if not yet recovered.
    #[must_use]
    pub fn recovery_duration_ms(&self) -> Option<u64> {
        if self.recovered_at_ms > 0 {
            Some(self.recovered_at_ms.saturating_sub(self.initiated_at_ms))
        } else {
            None
        }
    }
}

/// Rollback trigger thresholds used during active stages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookRollbackTrigger {
    /// Trigger identifier.
    pub trigger_id: String,
    /// Human description.
    pub description: String,
    /// Severity: critical triggers cause immediate rollback.
    pub critical: bool,
    /// Numeric threshold (interpretation depends on trigger type).
    pub threshold: f64,
    /// Category of the trigger.
    pub category: GateCategory,
}

impl PlaybookRollbackTrigger {
    /// Create a new critical trigger.
    #[must_use]
    pub fn critical(
        trigger_id: impl Into<String>,
        description: impl Into<String>,
        category: GateCategory,
        threshold: f64,
    ) -> Self {
        Self {
            trigger_id: trigger_id.into(),
            description: description.into(),
            critical: true,
            threshold,
            category,
        }
    }

    /// Create a non-critical (warning) trigger.
    #[must_use]
    pub fn warning(
        trigger_id: impl Into<String>,
        description: impl Into<String>,
        category: GateCategory,
        threshold: f64,
    ) -> Self {
        Self {
            trigger_id: trigger_id.into(),
            description: description.into(),
            critical: false,
            threshold,
            category,
        }
    }
}

/// A triggered rollback check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerEvaluation {
    /// Which trigger was evaluated.
    pub trigger_id: String,
    /// Whether the trigger fired.
    pub fired: bool,
    /// Observed value.
    pub observed: f64,
    /// Threshold that was checked.
    pub threshold: f64,
}

// =============================================================================
// Approvals
// =============================================================================

/// Approval role required for stage transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApproverRole {
    /// Engineering/migration lead.
    MigrationLead,
    /// Operations approver.
    Operations,
    /// Policy approver.
    PolicyOwner,
}

impl ApproverRole {
    /// Human label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::MigrationLead => "migration lead",
            Self::Operations => "operations",
            Self::PolicyOwner => "policy owner",
        }
    }
}

/// A recorded approval for a stage transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    /// Who approved.
    pub approver: String,
    /// Their role.
    pub role: ApproverRole,
    /// Stage being approved.
    pub stage: CutoverStage,
    /// When approved (epoch ms).
    pub approved_at_ms: u64,
    /// Optional notes.
    pub notes: String,
}

// =============================================================================
// Cohort tracking (progressive stage)
// =============================================================================

/// Traffic share increment during the Progressive stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficIncrement {
    /// Increment identifier (e.g., "increment-1").
    pub increment_id: String,
    /// Cohort being promoted.
    pub cohort_id: String,
    /// Target fraction after this increment (cumulative).
    pub target_fraction: f64,
    /// Whether the increment's gates passed.
    pub gates_passed: bool,
    /// When the increment was applied (epoch ms).
    pub applied_at_ms: u64,
    /// Evidence notes.
    pub evidence: String,
}

// =============================================================================
// Stage transition
// =============================================================================

/// Result of attempting a stage advance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvanceResult {
    /// Whether the advance succeeded.
    pub advanced: bool,
    /// The stage we were at.
    pub from: CutoverStage,
    /// The stage we moved to (same as from if not advanced).
    pub to: CutoverStage,
    /// Gates that blocked the advance (empty if advanced).
    pub blocking_gates: Vec<String>,
    /// Missing approvals (empty if all present).
    pub missing_approvals: Vec<ApproverRole>,
}

/// Record of a completed stage transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageTransition {
    /// From stage.
    pub from: CutoverStage,
    /// To stage.
    pub to: CutoverStage,
    /// When the transition occurred (epoch ms).
    pub transitioned_at_ms: u64,
    /// Who triggered the transition.
    pub triggered_by: String,
    /// Gate evaluation snapshot at transition time.
    pub gate_results: Vec<GateSnapshot>,
    /// Approval records for this transition.
    pub approvals: Vec<ApprovalRecord>,
}

/// Snapshot of a gate at transition time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateSnapshot {
    /// Gate identifier.
    pub gate_id: String,
    /// Pass/fail.
    pub passed: bool,
    /// Evidence note.
    pub evidence: String,
}

// =============================================================================
// Playbook telemetry
// =============================================================================

/// Telemetry counters for the playbook execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlaybookTelemetry {
    /// Total gate evaluations performed.
    pub gate_evaluations: u64,
    /// Total gates passed.
    pub gates_passed: u64,
    /// Total gates failed.
    pub gates_failed: u64,
    /// Total advance attempts.
    pub advance_attempts: u64,
    /// Successful advances.
    pub advances_succeeded: u64,
    /// Rollback events.
    pub rollback_count: u64,
    /// Approvals recorded.
    pub approvals_recorded: u64,
    /// Traffic increments applied.
    pub increments_applied: u64,
}

// =============================================================================
// Playbook (main orchestrator)
// =============================================================================

/// The staged cutover playbook — orchestrates migration from NTM to ft-native.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CutoverPlaybook {
    /// Migration identifier.
    pub migration_id: String,
    /// Playbook schema version.
    pub schema_version: u32,
    /// Current stage.
    pub current_stage: CutoverStage,
    /// Whether the playbook has been halted (manual or by critical trigger).
    pub halted: bool,
    /// Halt reason, if halted.
    pub halt_reason: String,
    /// All registered gates.
    pub gates: Vec<StageGate>,
    /// Rollback triggers.
    pub rollback_triggers: Vec<PlaybookRollbackTrigger>,
    /// Recorded stage transitions.
    pub transitions: Vec<StageTransition>,
    /// Rollback events.
    pub rollbacks: Vec<RollbackEvent>,
    /// Approval records.
    pub approvals: Vec<ApprovalRecord>,
    /// Traffic increments (Progressive stage).
    pub traffic_increments: Vec<TrafficIncrement>,
    /// Telemetry counters.
    pub telemetry: PlaybookTelemetry,
}

impl CutoverPlaybook {
    /// Create a new playbook starting at Preflight.
    #[must_use]
    pub fn new(migration_id: impl Into<String>, schema_version: u32) -> Self {
        Self {
            migration_id: migration_id.into(),
            schema_version,
            current_stage: CutoverStage::Preflight,
            halted: false,
            halt_reason: String::new(),
            gates: Vec::new(),
            rollback_triggers: Vec::new(),
            transitions: Vec::new(),
            rollbacks: Vec::new(),
            approvals: Vec::new(),
            traffic_increments: Vec::new(),
            telemetry: PlaybookTelemetry::default(),
        }
    }

    /// Register a gate for the playbook.
    pub fn register_gate(&mut self, gate: StageGate) {
        self.gates.push(gate);
    }

    /// Register a rollback trigger.
    pub fn register_trigger(&mut self, trigger: PlaybookRollbackTrigger) {
        self.rollback_triggers.push(trigger);
    }

    /// Record an approval.
    pub fn record_approval(&mut self, approval: ApprovalRecord) {
        self.telemetry.approvals_recorded += 1;
        self.approvals.push(approval);
    }

    /// Mark a gate as passed with evidence.
    pub fn pass_gate(&mut self, gate_id: &str, evidence: impl Into<String>) {
        self.telemetry.gate_evaluations += 1;
        for gate in &mut self.gates {
            if gate.gate_id == gate_id {
                gate.passed = true;
                gate.evidence = evidence.into();
                self.telemetry.gates_passed += 1;
                return;
            }
        }
    }

    /// Mark a gate as failed with evidence.
    pub fn fail_gate(&mut self, gate_id: &str, evidence: impl Into<String>) {
        self.telemetry.gate_evaluations += 1;
        for gate in &mut self.gates {
            if gate.gate_id == gate_id {
                gate.passed = false;
                gate.evidence = evidence.into();
                self.telemetry.gates_failed += 1;
                return;
            }
        }
    }

    /// Get all gates for the current stage.
    #[must_use]
    pub fn current_gates(&self) -> Vec<&StageGate> {
        self.gates
            .iter()
            .filter(|g| g.stage == self.current_stage)
            .collect()
    }

    /// Get blocking gates for the current stage that have NOT passed.
    #[must_use]
    pub fn blocking_failures(&self) -> Vec<&StageGate> {
        self.gates
            .iter()
            .filter(|g| g.stage == self.current_stage && g.blocking && !g.passed)
            .collect()
    }

    /// Check if all blocking gates for the current stage pass.
    #[must_use]
    pub fn all_blocking_gates_pass(&self) -> bool {
        self.blocking_failures().is_empty()
    }

    /// Required approval roles for the current stage transition.
    #[must_use]
    pub fn required_approvals(&self) -> Vec<ApproverRole> {
        match self.current_stage {
            CutoverStage::Preflight => vec![ApproverRole::MigrationLead],
            CutoverStage::Shadow => {
                vec![ApproverRole::MigrationLead, ApproverRole::Operations]
            }
            CutoverStage::Canary => vec![
                ApproverRole::MigrationLead,
                ApproverRole::Operations,
                ApproverRole::PolicyOwner,
            ],
            CutoverStage::Progressive => {
                vec![ApproverRole::MigrationLead, ApproverRole::Operations]
            }
            CutoverStage::Default => vec![
                ApproverRole::MigrationLead,
                ApproverRole::Operations,
                ApproverRole::PolicyOwner,
            ],
        }
    }

    /// Check which required approvals are missing for the current stage.
    #[must_use]
    pub fn missing_approvals(&self) -> Vec<ApproverRole> {
        let required = self.required_approvals();
        let have: Vec<ApproverRole> = self
            .approvals
            .iter()
            .filter(|a| a.stage == self.current_stage)
            .map(|a| a.role)
            .collect();
        required.into_iter().filter(|r| !have.contains(r)).collect()
    }

    /// Attempt to advance to the next stage.
    ///
    /// Advances only if:
    /// 1. Playbook is not halted.
    /// 2. All blocking gates for the current stage pass.
    /// 3. All required approvals are present.
    /// 4. There is a next stage to advance to.
    pub fn try_advance(&mut self, now_ms: u64, triggered_by: &str) -> AdvanceResult {
        self.telemetry.advance_attempts += 1;

        let from = self.current_stage;

        // Can't advance if halted
        if self.halted {
            return AdvanceResult {
                advanced: false,
                from,
                to: from,
                blocking_gates: vec!["PLAYBOOK_HALTED".into()],
                missing_approvals: Vec::new(),
            };
        }

        // Can't advance past Default
        let next = match from.next() {
            Some(s) => s,
            None => {
                return AdvanceResult {
                    advanced: false,
                    from,
                    to: from,
                    blocking_gates: vec!["ALREADY_AT_FINAL_STAGE".into()],
                    missing_approvals: Vec::new(),
                };
            }
        };

        // Check blocking gates
        let blocking: Vec<String> = self
            .blocking_failures()
            .iter()
            .map(|g| g.gate_id.clone())
            .collect();

        // Check approvals
        let missing = self.missing_approvals();

        if !blocking.is_empty() || !missing.is_empty() {
            return AdvanceResult {
                advanced: false,
                from,
                to: from,
                blocking_gates: blocking,
                missing_approvals: missing,
            };
        }

        // Record transition
        let gate_results: Vec<GateSnapshot> = self
            .gates
            .iter()
            .filter(|g| g.stage == from)
            .map(|g| GateSnapshot {
                gate_id: g.gate_id.clone(),
                passed: g.passed,
                evidence: g.evidence.clone(),
            })
            .collect();

        let transition_approvals: Vec<ApprovalRecord> = self
            .approvals
            .iter()
            .filter(|a| a.stage == from)
            .cloned()
            .collect();

        self.transitions.push(StageTransition {
            from,
            to: next,
            transitioned_at_ms: now_ms,
            triggered_by: triggered_by.into(),
            gate_results,
            approvals: transition_approvals,
        });

        self.current_stage = next;
        self.telemetry.advances_succeeded += 1;

        AdvanceResult {
            advanced: true,
            from,
            to: next,
            blocking_gates: Vec::new(),
            missing_approvals: Vec::new(),
        }
    }

    /// Evaluate rollback triggers against observed metrics.
    ///
    /// Returns evaluations for all triggers; fires rollback if any critical
    /// trigger exceeds its threshold.
    pub fn evaluate_triggers(
        &mut self,
        observations: &BTreeMap<String, f64>,
        now_ms: u64,
        operator: &str,
    ) -> Vec<TriggerEvaluation> {
        let mut evals = Vec::new();
        let mut critical_fired = false;
        let mut fire_description = String::new();

        for trigger in &self.rollback_triggers {
            let observed = observations
                .get(&trigger.trigger_id)
                .copied()
                .unwrap_or(0.0);
            let fired = observed > trigger.threshold;
            if fired && trigger.critical {
                critical_fired = true;
                fire_description = trigger.description.clone();
            }
            evals.push(TriggerEvaluation {
                trigger_id: trigger.trigger_id.clone(),
                fired,
                observed,
                threshold: trigger.threshold,
            });
        }

        if critical_fired {
            self.initiate_rollback(&fire_description, now_ms, operator);
        }

        evals
    }

    /// Initiate a rollback to the previous stage.
    pub fn initiate_rollback(&mut self, reason: &str, now_ms: u64, initiated_by: &str) {
        let from = self.current_stage;
        let to = from.previous().unwrap_or(CutoverStage::Preflight);

        self.rollbacks.push(RollbackEvent {
            trigger_description: reason.into(),
            from_stage: from,
            to_stage: to,
            initiated_at_ms: now_ms,
            recovered_at_ms: 0,
            initiated_by: initiated_by.into(),
            notes: String::new(),
        });

        self.current_stage = to;
        self.halted = true;
        self.halt_reason = format!(
            "Rollback from {} to {}: {}",
            from.label(),
            to.label(),
            reason
        );
        self.telemetry.rollback_count += 1;
    }

    /// Confirm rollback recovery and resume the playbook.
    pub fn confirm_recovery(&mut self, now_ms: u64, notes: &str) {
        if let Some(last) = self.rollbacks.last_mut() {
            last.recovered_at_ms = now_ms;
            last.notes = notes.into();
        }
        self.halted = false;
        self.halt_reason.clear();
    }

    /// Record a traffic increment during the Progressive stage.
    pub fn record_increment(&mut self, increment: TrafficIncrement) {
        self.telemetry.increments_applied += 1;
        self.traffic_increments.push(increment);
    }

    /// Current cumulative traffic fraction (from progressive increments).
    #[must_use]
    pub fn current_traffic_fraction(&self) -> f64 {
        self.traffic_increments
            .last()
            .map(|i| i.target_fraction)
            .unwrap_or(0.0)
    }

    /// Whether the playbook has completed (reached Default stage with all gates passing).
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.current_stage == CutoverStage::Default && !self.halted
    }

    /// Create a serializable snapshot of the playbook state.
    #[must_use]
    pub fn snapshot(&self) -> PlaybookSnapshot {
        let gates_total = self.gates.iter().filter(|g| g.blocking).count();
        let gates_passed = self.gates.iter().filter(|g| g.blocking && g.passed).count();

        PlaybookSnapshot {
            migration_id: self.migration_id.clone(),
            current_stage: self.current_stage,
            halted: self.halted,
            halt_reason: self.halt_reason.clone(),
            gates_total,
            gates_passed,
            transitions_count: self.transitions.len(),
            rollbacks_count: self.rollbacks.len(),
            increments_count: self.traffic_increments.len(),
            current_traffic_fraction: self.current_traffic_fraction(),
            is_complete: self.is_complete(),
            telemetry: self.telemetry.clone(),
        }
    }

    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let snap = self.snapshot();
        let mut out = String::new();

        out.push_str(&format!("# Cutover Playbook: {}\n\n", snap.migration_id));
        out.push_str(&format!(
            "Stage: {} ({})\n",
            snap.current_stage.index(),
            snap.current_stage.label()
        ));

        if snap.halted {
            out.push_str(&format!("HALTED: {}\n", snap.halt_reason));
        }

        out.push_str(&format!(
            "Gates: {}/{} blocking passed\n",
            snap.gates_passed, snap.gates_total
        ));
        out.push_str(&format!("Transitions: {}\n", snap.transitions_count));
        out.push_str(&format!("Rollbacks: {}\n", snap.rollbacks_count));

        if snap.increments_count > 0 {
            out.push_str(&format!(
                "Traffic: {:.1}% ({} increments)\n",
                snap.current_traffic_fraction * 100.0,
                snap.increments_count
            ));
        }

        if snap.is_complete {
            out.push_str("\nCUTOVER COMPLETE\n");
        }

        out
    }
}

/// Serializable snapshot of playbook state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookSnapshot {
    /// Migration identifier.
    pub migration_id: String,
    /// Current stage.
    pub current_stage: CutoverStage,
    /// Whether halted.
    pub halted: bool,
    /// Halt reason.
    pub halt_reason: String,
    /// Total blocking gates.
    pub gates_total: usize,
    /// Passed blocking gates.
    pub gates_passed: usize,
    /// Number of completed stage transitions.
    pub transitions_count: usize,
    /// Number of rollback events.
    pub rollbacks_count: usize,
    /// Number of traffic increments.
    pub increments_count: usize,
    /// Current cumulative traffic fraction.
    pub current_traffic_fraction: f64,
    /// Whether the playbook is complete.
    pub is_complete: bool,
    /// Telemetry counters.
    pub telemetry: PlaybookTelemetry,
}

// =============================================================================
// Standard playbook factory
// =============================================================================

/// Create a standard cutover playbook with the 6 gates from the playbook doc.
#[must_use]
pub fn standard_playbook(migration_id: impl Into<String>) -> CutoverPlaybook {
    let mut pb = CutoverPlaybook::new(migration_id, 1);

    // Stage 0 (Preflight) gates
    pb.register_gate(
        StageGate::new(
            "G-01",
            GateCategory::Parity,
            "Blocking parity scenarios 100% pass",
        )
        .for_stage(CutoverStage::Preflight),
    );
    pb.register_gate(
        StageGate::new(
            "G-02",
            GateCategory::Parity,
            "High-priority parity >= 90% pass, <= 1 intentional delta",
        )
        .for_stage(CutoverStage::Preflight),
    );
    pb.register_gate(
        StageGate::new(
            "G-03",
            GateCategory::Contract,
            "Envelope contract stability: 0 blocking violations",
        )
        .for_stage(CutoverStage::Preflight),
    );

    // Stage 1 (Shadow) gates
    pb.register_gate(
        StageGate::new(
            "G-04",
            GateCategory::Divergence,
            "Divergence within budget for two consecutive windows",
        )
        .for_stage(CutoverStage::Shadow),
    );
    pb.register_gate(
        StageGate::new(
            "G-05",
            GateCategory::PolicySafety,
            "0 ungated mutation events in policy/audit exports",
        )
        .for_stage(CutoverStage::Shadow),
    );

    // Stage 2 (Canary) gates
    pb.register_gate(
        StageGate::new(
            "G-06",
            GateCategory::RollbackReadiness,
            "Rollback drill pass in last 24h",
        )
        .for_stage(CutoverStage::Canary),
    );
    pb.register_gate(
        StageGate::new(
            "G-SLO",
            GateCategory::Performance,
            "SLO and safety thresholds hold for full canary window",
        )
        .for_stage(CutoverStage::Canary),
    );

    // Stage 3 (Progressive) gates
    pb.register_gate(
        StageGate::new(
            "G-DRIFT",
            GateCategory::Divergence,
            "Drift/divergence bounded after each increment",
        )
        .for_stage(CutoverStage::Progressive),
    );
    pb.register_gate(
        StageGate::new(
            "G-INCIDENT",
            GateCategory::PolicySafety,
            "No backlog of unresolved high-severity incidents",
        )
        .for_stage(CutoverStage::Progressive),
    );

    // Stage 4 (Default) gates
    pb.register_gate(
        StageGate::new(
            "G-FINAL-REVIEW",
            GateCategory::Approval,
            "Final pre-switch gate review signed by eng + ops",
        )
        .for_stage(CutoverStage::Default),
    );
    pb.register_gate(
        StageGate::new(
            "G-REHEARSAL-24H",
            GateCategory::RollbackReadiness,
            "Rollback procedure rehearsed in last 24h",
        )
        .for_stage(CutoverStage::Default),
    );

    // Standard rollback triggers
    pb.register_trigger(PlaybookRollbackTrigger::critical(
        "RT-PARITY",
        "Blocking parity gate failure after shadow",
        GateCategory::Parity,
        0.0, // any failure triggers
    ));
    pb.register_trigger(PlaybookRollbackTrigger::critical(
        "RT-CONTRACT",
        "Envelope contract break causing automation incompatibility",
        GateCategory::Contract,
        0.0,
    ));
    pb.register_trigger(PlaybookRollbackTrigger::critical(
        "RT-POLICY",
        "Policy enforcement bypass or audit-chain break",
        GateCategory::PolicySafety,
        0.0,
    ));
    pb.register_trigger(PlaybookRollbackTrigger::warning(
        "RT-DIVERGENCE",
        "Sustained divergence above budget for one evaluation window",
        GateCategory::Divergence,
        0.05, // 5% divergence threshold
    ));
    pb.register_trigger(PlaybookRollbackTrigger::critical(
        "RT-INCIDENT",
        "Operator-declared safety incident with critical severity",
        GateCategory::PolicySafety,
        0.0,
    ));

    pb
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- CutoverStage ----

    #[test]
    fn stage_ordering() {
        assert!(CutoverStage::Preflight < CutoverStage::Shadow);
        assert!(CutoverStage::Shadow < CutoverStage::Canary);
        assert!(CutoverStage::Canary < CutoverStage::Progressive);
        assert!(CutoverStage::Progressive < CutoverStage::Default);
    }

    #[test]
    fn stage_navigation() {
        assert_eq!(CutoverStage::Preflight.next(), Some(CutoverStage::Shadow));
        assert_eq!(CutoverStage::Default.next(), None);
        assert_eq!(
            CutoverStage::Shadow.previous(),
            Some(CutoverStage::Preflight)
        );
        assert_eq!(CutoverStage::Preflight.previous(), None);
    }

    #[test]
    fn stage_index_contiguous() {
        for (i, stage) in CutoverStage::ALL.iter().enumerate() {
            assert_eq!(stage.index() as usize, i);
        }
    }

    #[test]
    fn stage_labels_non_empty() {
        for stage in CutoverStage::ALL {
            assert!(!stage.label().is_empty());
        }
    }

    // ---- StageGate ----

    #[test]
    fn gate_defaults_to_blocking() {
        let gate = StageGate::new("G-01", GateCategory::Parity, "test");
        assert!(gate.blocking);
        assert!(!gate.passed);
        assert_eq!(gate.stage, CutoverStage::Preflight);
    }

    #[test]
    fn gate_builder_methods() {
        let gate = StageGate::new("G-X", GateCategory::Performance, "perf check")
            .for_stage(CutoverStage::Canary)
            .advisory();
        assert_eq!(gate.stage, CutoverStage::Canary);
        assert!(!gate.blocking);
    }

    // ---- RollbackEvent ----

    #[test]
    fn rollback_recovery_tracking() {
        let mut evt = RollbackEvent {
            trigger_description: "test".into(),
            from_stage: CutoverStage::Canary,
            to_stage: CutoverStage::Shadow,
            initiated_at_ms: 1000,
            recovered_at_ms: 0,
            initiated_by: "op".into(),
            notes: String::new(),
        };

        assert!(!evt.is_recovered());
        assert_eq!(evt.recovery_duration_ms(), None);

        evt.recovered_at_ms = 1500;
        assert!(evt.is_recovered());
        assert_eq!(evt.recovery_duration_ms(), Some(500));
    }

    // ---- CutoverPlaybook basic operations ----

    #[test]
    fn new_playbook_starts_at_preflight() {
        let pb = CutoverPlaybook::new("test", 1);
        assert_eq!(pb.current_stage, CutoverStage::Preflight);
        assert!(!pb.halted);
        assert!(!pb.is_complete());
    }

    #[test]
    fn pass_and_fail_gate() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.register_gate(StageGate::new("G-01", GateCategory::Parity, "parity"));

        pb.pass_gate("G-01", "all pass");
        assert!(pb.gates[0].passed);
        assert_eq!(pb.gates[0].evidence, "all pass");
        assert_eq!(pb.telemetry.gates_passed, 1);

        pb.fail_gate("G-01", "regression found");
        assert!(!pb.gates[0].passed);
        assert_eq!(pb.telemetry.gates_failed, 1);
    }

    #[test]
    fn current_gates_filters_by_stage() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.register_gate(
            StageGate::new("G-01", GateCategory::Parity, "preflight gate")
                .for_stage(CutoverStage::Preflight),
        );
        pb.register_gate(
            StageGate::new("G-04", GateCategory::Divergence, "shadow gate")
                .for_stage(CutoverStage::Shadow),
        );

        let gates = pb.current_gates();
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0].gate_id, "G-01");
    }

    // ---- Advance logic ----

    #[test]
    fn cannot_advance_with_failing_gate() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.register_gate(
            StageGate::new("G-01", GateCategory::Parity, "must pass")
                .for_stage(CutoverStage::Preflight),
        );
        // Need approval too
        pb.record_approval(ApprovalRecord {
            approver: "lead".into(),
            role: ApproverRole::MigrationLead,
            stage: CutoverStage::Preflight,
            approved_at_ms: 100,
            notes: String::new(),
        });

        let result = pb.try_advance(1000, "test");
        assert!(!result.advanced);
        assert!(result.blocking_gates.contains(&"G-01".to_string()));
    }

    #[test]
    fn cannot_advance_without_approvals() {
        let mut pb = CutoverPlaybook::new("test", 1);
        // No gates, but approval required for Preflight -> Shadow
        let result = pb.try_advance(1000, "test");
        assert!(!result.advanced);
        assert!(
            result
                .missing_approvals
                .contains(&ApproverRole::MigrationLead)
        );
    }

    #[test]
    fn advance_succeeds_when_all_clear() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.register_gate(
            StageGate::new("G-01", GateCategory::Parity, "parity")
                .for_stage(CutoverStage::Preflight),
        );
        pb.pass_gate("G-01", "all pass");
        pb.record_approval(ApprovalRecord {
            approver: "lead".into(),
            role: ApproverRole::MigrationLead,
            stage: CutoverStage::Preflight,
            approved_at_ms: 100,
            notes: String::new(),
        });

        let result = pb.try_advance(1000, "test");
        assert!(result.advanced);
        assert_eq!(result.from, CutoverStage::Preflight);
        assert_eq!(result.to, CutoverStage::Shadow);
        assert_eq!(pb.current_stage, CutoverStage::Shadow);
        assert_eq!(pb.transitions.len(), 1);
    }

    #[test]
    fn cannot_advance_past_default() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.current_stage = CutoverStage::Default;

        let result = pb.try_advance(1000, "test");
        assert!(!result.advanced);
        assert!(
            result
                .blocking_gates
                .contains(&"ALREADY_AT_FINAL_STAGE".to_string())
        );
    }

    #[test]
    fn cannot_advance_when_halted() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.halted = true;
        pb.halt_reason = "rollback".into();

        let result = pb.try_advance(1000, "test");
        assert!(!result.advanced);
        assert!(
            result
                .blocking_gates
                .contains(&"PLAYBOOK_HALTED".to_string())
        );
    }

    // ---- Rollback ----

    #[test]
    fn rollback_moves_to_previous_stage() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.current_stage = CutoverStage::Canary;

        pb.initiate_rollback("error rate spike", 2000, "oncall");
        assert_eq!(pb.current_stage, CutoverStage::Shadow);
        assert!(pb.halted);
        assert_eq!(pb.rollbacks.len(), 1);
        assert_eq!(pb.telemetry.rollback_count, 1);
    }

    #[test]
    fn rollback_from_preflight_stays_at_preflight() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.initiate_rollback("issue", 1000, "op");
        assert_eq!(pb.current_stage, CutoverStage::Preflight);
    }

    #[test]
    fn confirm_recovery_unhalts_playbook() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.current_stage = CutoverStage::Canary;
        pb.initiate_rollback("error", 1000, "op");
        assert!(pb.halted);

        pb.confirm_recovery(2000, "root cause fixed");
        assert!(!pb.halted);
        assert!(pb.halt_reason.is_empty());
        assert_eq!(pb.rollbacks[0].recovered_at_ms, 2000);
    }

    // ---- Trigger evaluation ----

    #[test]
    fn trigger_evaluation_fires_on_threshold() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.current_stage = CutoverStage::Canary;
        pb.register_trigger(PlaybookRollbackTrigger::critical(
            "RT-ERR",
            "error rate spike",
            GateCategory::Performance,
            0.05,
        ));
        pb.register_trigger(PlaybookRollbackTrigger::warning(
            "RT-LAT",
            "latency warning",
            GateCategory::Performance,
            100.0,
        ));

        let mut obs = BTreeMap::new();
        obs.insert("RT-ERR".to_string(), 0.10); // exceeds 0.05 threshold
        obs.insert("RT-LAT".to_string(), 50.0); // under 100.0 threshold

        let evals = pb.evaluate_triggers(&obs, 3000, "monitor");

        // RT-ERR should fire (critical), RT-LAT should not
        let err_eval = evals.iter().find(|e| e.trigger_id == "RT-ERR").unwrap();
        assert!(err_eval.fired);

        let lat_eval = evals.iter().find(|e| e.trigger_id == "RT-LAT").unwrap();
        assert!(!lat_eval.fired);

        // Critical trigger should have caused rollback
        assert!(pb.halted);
        assert_eq!(pb.current_stage, CutoverStage::Shadow);
    }

    #[test]
    fn non_critical_trigger_does_not_rollback() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.current_stage = CutoverStage::Canary;
        pb.register_trigger(PlaybookRollbackTrigger::warning(
            "RT-LAT",
            "latency",
            GateCategory::Performance,
            100.0,
        ));

        let mut obs = BTreeMap::new();
        obs.insert("RT-LAT".to_string(), 200.0); // exceeds, but non-critical

        let evals = pb.evaluate_triggers(&obs, 3000, "monitor");
        assert!(evals[0].fired);
        assert!(!pb.halted); // warning doesn't halt
    }

    // ---- Traffic increments ----

    #[test]
    fn traffic_increment_tracking() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.current_stage = CutoverStage::Progressive;

        pb.record_increment(TrafficIncrement {
            increment_id: "inc-1".into(),
            cohort_id: "canary".into(),
            target_fraction: 0.05,
            gates_passed: true,
            applied_at_ms: 1000,
            evidence: "all green".into(),
        });

        assert_eq!(pb.current_traffic_fraction(), 0.05);

        pb.record_increment(TrafficIncrement {
            increment_id: "inc-2".into(),
            cohort_id: "early-adopters".into(),
            target_fraction: 0.25,
            gates_passed: true,
            applied_at_ms: 2000,
            evidence: "all green".into(),
        });

        assert_eq!(pb.current_traffic_fraction(), 0.25);
        assert_eq!(pb.telemetry.increments_applied, 2);
    }

    // ---- Standard playbook factory ----

    #[test]
    fn standard_playbook_has_all_gates() {
        let pb = standard_playbook("ntm-to-ft");
        assert_eq!(pb.migration_id, "ntm-to-ft");

        // Preflight gates: G-01, G-02, G-03
        let preflight: Vec<_> = pb
            .gates
            .iter()
            .filter(|g| g.stage == CutoverStage::Preflight)
            .collect();
        assert_eq!(preflight.len(), 3);

        // Shadow gates: G-04, G-05
        let shadow: Vec<_> = pb
            .gates
            .iter()
            .filter(|g| g.stage == CutoverStage::Shadow)
            .collect();
        assert_eq!(shadow.len(), 2);

        // Canary gates: G-06, G-SLO
        let canary: Vec<_> = pb
            .gates
            .iter()
            .filter(|g| g.stage == CutoverStage::Canary)
            .collect();
        assert_eq!(canary.len(), 2);

        // Progressive gates: G-DRIFT, G-INCIDENT
        let prog: Vec<_> = pb
            .gates
            .iter()
            .filter(|g| g.stage == CutoverStage::Progressive)
            .collect();
        assert_eq!(prog.len(), 2);

        // Default gates: G-FINAL-REVIEW, G-REHEARSAL-24H
        let def: Vec<_> = pb
            .gates
            .iter()
            .filter(|g| g.stage == CutoverStage::Default)
            .collect();
        assert_eq!(def.len(), 2);

        // Rollback triggers
        assert_eq!(pb.rollback_triggers.len(), 5);
    }

    #[test]
    fn standard_playbook_all_gates_blocking() {
        let pb = standard_playbook("test");
        assert!(pb.gates.iter().all(|g| g.blocking));
    }

    // ---- Snapshot and summary ----

    #[test]
    fn snapshot_reflects_state() {
        let mut pb = standard_playbook("test");
        pb.pass_gate("G-01", "ok");

        let snap = pb.snapshot();
        assert_eq!(snap.current_stage, CutoverStage::Preflight);
        assert!(!snap.halted);
        assert_eq!(snap.gates_passed, 1);
        assert_eq!(snap.gates_total, 11);
        assert!(!snap.is_complete);
    }

    #[test]
    fn render_summary_includes_key_info() {
        let pb = standard_playbook("ntm-migration");
        let summary = pb.render_summary();
        assert!(summary.contains("ntm-migration"));
        assert!(summary.contains("Preflight"));
        assert!(summary.contains("Gates:"));
    }

    // ---- Serde round-trip ----

    #[test]
    fn playbook_serde_roundtrip() {
        let pb = standard_playbook("test");
        let json = serde_json::to_string(&pb).unwrap();
        let pb2: CutoverPlaybook = serde_json::from_str(&json).unwrap();
        assert_eq!(pb2.migration_id, "test");
        assert_eq!(pb2.gates.len(), pb.gates.len());
        assert_eq!(pb2.rollback_triggers.len(), pb.rollback_triggers.len());
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let pb = standard_playbook("test");
        let snap = pb.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let snap2: PlaybookSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap2.migration_id, snap.migration_id);
        assert_eq!(snap2.gates_total, snap.gates_total);
    }

    // ---- E2E lifecycle ----

    #[test]
    fn e2e_full_cutover_lifecycle() {
        let mut pb = standard_playbook("ntm-to-ft");

        // === Stage 0: Preflight ===
        assert_eq!(pb.current_stage, CutoverStage::Preflight);

        // Pass all preflight gates
        pb.pass_gate("G-01", "47/47 blocking scenarios pass");
        pb.pass_gate("G-02", "93% high-priority pass, 0 intentional deltas");
        pb.pass_gate("G-03", "0 envelope violations");
        pb.record_approval(ApprovalRecord {
            approver: "migration-lead".into(),
            role: ApproverRole::MigrationLead,
            stage: CutoverStage::Preflight,
            approved_at_ms: 100,
            notes: "preflight clear".into(),
        });

        let result = pb.try_advance(200, "lead");
        assert!(result.advanced);
        assert_eq!(pb.current_stage, CutoverStage::Shadow);

        // === Stage 1: Shadow ===
        pb.pass_gate("G-04", "divergence 0.2% for 2 consecutive windows");
        pb.pass_gate("G-05", "0 ungated mutations");
        pb.record_approval(ApprovalRecord {
            approver: "lead".into(),
            role: ApproverRole::MigrationLead,
            stage: CutoverStage::Shadow,
            approved_at_ms: 300,
            notes: String::new(),
        });
        pb.record_approval(ApprovalRecord {
            approver: "oncall".into(),
            role: ApproverRole::Operations,
            stage: CutoverStage::Shadow,
            approved_at_ms: 310,
            notes: String::new(),
        });

        let result = pb.try_advance(400, "lead");
        assert!(result.advanced);
        assert_eq!(pb.current_stage, CutoverStage::Canary);

        // === Stage 2: Canary ===
        pb.pass_gate("G-06", "rollback drill passed 2h ago");
        pb.pass_gate("G-SLO", "SLO held for full canary window");
        pb.record_approval(ApprovalRecord {
            approver: "lead".into(),
            role: ApproverRole::MigrationLead,
            stage: CutoverStage::Canary,
            approved_at_ms: 500,
            notes: String::new(),
        });
        pb.record_approval(ApprovalRecord {
            approver: "ops".into(),
            role: ApproverRole::Operations,
            stage: CutoverStage::Canary,
            approved_at_ms: 510,
            notes: String::new(),
        });
        pb.record_approval(ApprovalRecord {
            approver: "policy".into(),
            role: ApproverRole::PolicyOwner,
            stage: CutoverStage::Canary,
            approved_at_ms: 520,
            notes: String::new(),
        });

        let result = pb.try_advance(600, "lead");
        assert!(result.advanced);
        assert_eq!(pb.current_stage, CutoverStage::Progressive);

        // === Stage 3: Progressive ===
        pb.record_increment(TrafficIncrement {
            increment_id: "inc-1".into(),
            cohort_id: "canary".into(),
            target_fraction: 0.05,
            gates_passed: true,
            applied_at_ms: 700,
            evidence: "canary cohort green".into(),
        });
        pb.record_increment(TrafficIncrement {
            increment_id: "inc-2".into(),
            cohort_id: "early-adopters".into(),
            target_fraction: 0.25,
            gates_passed: true,
            applied_at_ms: 800,
            evidence: "early adopters green".into(),
        });
        pb.record_increment(TrafficIncrement {
            increment_id: "inc-3".into(),
            cohort_id: "general-availability".into(),
            target_fraction: 1.0,
            gates_passed: true,
            applied_at_ms: 900,
            evidence: "full fleet green".into(),
        });

        pb.pass_gate("G-DRIFT", "drift bounded at each increment");
        pb.pass_gate("G-INCIDENT", "0 unresolved incidents");
        pb.record_approval(ApprovalRecord {
            approver: "lead".into(),
            role: ApproverRole::MigrationLead,
            stage: CutoverStage::Progressive,
            approved_at_ms: 950,
            notes: String::new(),
        });
        pb.record_approval(ApprovalRecord {
            approver: "ops".into(),
            role: ApproverRole::Operations,
            stage: CutoverStage::Progressive,
            approved_at_ms: 960,
            notes: String::new(),
        });

        let result = pb.try_advance(1000, "lead");
        assert!(result.advanced);
        assert_eq!(pb.current_stage, CutoverStage::Default);

        // Verify completion state
        assert!(pb.is_complete());
        assert_eq!(pb.transitions.len(), 4);
        assert_eq!(pb.traffic_increments.len(), 3);
        assert_eq!(pb.current_traffic_fraction(), 1.0);

        let snap = pb.snapshot();
        assert!(snap.is_complete);
        assert_eq!(snap.transitions_count, 4);
        assert_eq!(snap.gates_passed, 9); // all non-Default gates passed

        let summary = pb.render_summary();
        assert!(summary.contains("CUTOVER COMPLETE"));
    }

    #[test]
    fn e2e_rollback_and_recovery() {
        let mut pb = standard_playbook("test");

        // Advance to Canary
        pb.pass_gate("G-01", "ok");
        pb.pass_gate("G-02", "ok");
        pb.pass_gate("G-03", "ok");
        pb.record_approval(ApprovalRecord {
            approver: "lead".into(),
            role: ApproverRole::MigrationLead,
            stage: CutoverStage::Preflight,
            approved_at_ms: 100,
            notes: String::new(),
        });
        pb.try_advance(200, "lead");

        pb.pass_gate("G-04", "ok");
        pb.pass_gate("G-05", "ok");
        pb.record_approval(ApprovalRecord {
            approver: "lead".into(),
            role: ApproverRole::MigrationLead,
            stage: CutoverStage::Shadow,
            approved_at_ms: 300,
            notes: String::new(),
        });
        pb.record_approval(ApprovalRecord {
            approver: "ops".into(),
            role: ApproverRole::Operations,
            stage: CutoverStage::Shadow,
            approved_at_ms: 310,
            notes: String::new(),
        });
        pb.try_advance(400, "lead");
        assert_eq!(pb.current_stage, CutoverStage::Canary);

        // Simulate critical trigger firing during canary
        let mut obs = BTreeMap::new();
        obs.insert("RT-PARITY".to_string(), 1.0); // parity failure
        let evals = pb.evaluate_triggers(&obs, 500, "monitor");

        assert!(evals.iter().any(|e| e.trigger_id == "RT-PARITY" && e.fired));
        assert!(pb.halted);
        assert_eq!(pb.current_stage, CutoverStage::Shadow);
        assert_eq!(pb.rollbacks.len(), 1);

        // Cannot advance while halted
        let result = pb.try_advance(600, "lead");
        assert!(!result.advanced);

        // Confirm recovery
        pb.confirm_recovery(700, "parity regression fixed and re-validated");
        assert!(!pb.halted);

        // Can re-advance after recovery
        pb.pass_gate("G-04", "re-verified after rollback");
        pb.pass_gate("G-05", "re-verified");
        pb.record_approval(ApprovalRecord {
            approver: "lead".into(),
            role: ApproverRole::MigrationLead,
            stage: CutoverStage::Shadow,
            approved_at_ms: 800,
            notes: "post-rollback re-approval".into(),
        });
        pb.record_approval(ApprovalRecord {
            approver: "ops".into(),
            role: ApproverRole::Operations,
            stage: CutoverStage::Shadow,
            approved_at_ms: 810,
            notes: String::new(),
        });

        let result = pb.try_advance(900, "lead");
        assert!(result.advanced);
        assert_eq!(pb.current_stage, CutoverStage::Canary);
        assert_eq!(pb.transitions.len(), 3); // preflight->shadow, shadow->canary, shadow->canary again
    }

    // ---- Advisory gates ----

    #[test]
    fn advisory_gate_does_not_block_advance() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.register_gate(
            StageGate::new("G-ADV", GateCategory::Performance, "advisory perf")
                .for_stage(CutoverStage::Preflight)
                .advisory(),
        );
        pb.record_approval(ApprovalRecord {
            approver: "lead".into(),
            role: ApproverRole::MigrationLead,
            stage: CutoverStage::Preflight,
            approved_at_ms: 100,
            notes: String::new(),
        });

        // Advisory gate not passed, but should not block
        let result = pb.try_advance(200, "test");
        assert!(result.advanced);
    }

    // ---- Approval requirements ----

    #[test]
    fn default_stage_requires_three_approvals() {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.current_stage = CutoverStage::Default;

        let missing = pb.missing_approvals();
        assert_eq!(missing.len(), 3);
        assert!(missing.contains(&ApproverRole::MigrationLead));
        assert!(missing.contains(&ApproverRole::Operations));
        assert!(missing.contains(&ApproverRole::PolicyOwner));
    }
}
