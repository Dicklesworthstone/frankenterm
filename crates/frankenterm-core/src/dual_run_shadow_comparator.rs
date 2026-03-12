//! Dual-run shadow comparator for NTM↔FrankenTerm migration (ft-3681t.8.2).
//!
//! Provides side-by-side comparison of command outputs, semantic outcomes,
//! and latency between NTM and FrankenTerm runs of the same scenarios.
//! Includes a structured drift triage workflow for classifying, tracking,
//! and resolving divergences before irreversible cutover.
//!
//! # Architecture
//!
//! ```text
//! NtmParityCorpus ─── ScenarioRunner (NTM) ──┐
//!                                              ├──► DualRunComparator ──► ComparisonReport
//! NtmParityCorpus ─── ScenarioRunner (FT) ───┘         │
//!                                                       ▼
//!                                              DriftTriageWorkflow
//!                                              (classify → resolve → gate)
//! ```
//!
//! # Key types
//!
//! - [`DualRunResult`]: Side-by-side captured outputs from both systems.
//! - [`ComparisonVerdict`]: Semantic + performance comparison verdict.
//! - [`DriftClassification`]: Categorized divergence for triage.
//! - [`DriftTriageWorkflow`]: Structured triage with resolution tracking.
//! - [`CutoverGateEvaluation`]: Go/no-go gate decision from shadow results.

use serde::{Deserialize, Serialize};

// ── Dual Run Capture ────────────────────────────────────────────────────────

/// Side-by-side captured output from a single scenario run on both systems.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DualRunResult {
    /// Scenario identifier.
    pub scenario_id: String,
    /// Scenario domain (e.g., "watch", "search", "panes").
    pub domain: String,
    /// Scenario priority.
    pub priority: DualRunPriority,
    /// NTM side output.
    pub ntm: RunCapture,
    /// FrankenTerm side output.
    pub ft: RunCapture,
    /// Timestamp when this comparison was made.
    pub compared_at_ms: u64,
    /// Correlation ID for tracing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

/// Priority level for dual-run scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DualRunPriority {
    /// Must match exactly — blocks cutover.
    Blocking,
    /// Should match — tracked in divergence budget.
    High,
    /// Nice to match — informational only.
    Medium,
    /// Informational — not gated.
    Low,
}

/// Captured output from a single system run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCapture {
    /// System identifier ("ntm" or "ft").
    pub system: String,
    /// Command executed.
    pub command: String,
    /// Exit code (None if execution failed before exit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Stdout.
    pub stdout: String,
    /// Stderr.
    pub stderr: String,
    /// Execution duration in milliseconds.
    pub duration_ms: u64,
    /// Whether the execution completed without infrastructure errors.
    pub completed: bool,
    /// Infrastructure error (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Comparison Verdict ──────────────────────────────────────────────────────

/// Result of comparing dual-run outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonVerdict {
    /// Scenario identifier.
    pub scenario_id: String,
    /// Overall match status.
    pub match_status: MatchStatus,
    /// Semantic comparison details.
    pub semantic: SemanticComparison,
    /// Performance comparison details.
    pub performance: PerformanceComparison,
    /// Per-field divergences found.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub divergences: Vec<FieldDivergence>,
    /// Human-readable verdict summary.
    pub summary: String,
}

/// Overall match status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchStatus {
    /// Outputs match semantically and within performance thresholds.
    Match,
    /// Outputs differ but the difference is an intentional/expected delta.
    IntentionalDelta,
    /// Outputs diverge — needs triage.
    Divergence,
    /// One or both runs failed to execute.
    ExecutionFailure,
    /// Comparison could not be performed (invalid inputs).
    Inconclusive,
}

/// Semantic comparison results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticComparison {
    /// Exit codes match.
    pub exit_code_match: bool,
    /// Stdout outputs match (normalized).
    pub stdout_match: bool,
    /// Stderr outputs match (normalized).
    pub stderr_match: bool,
    /// JSON structure match (if both outputs are valid JSON).
    pub json_structure_match: Option<bool>,
    /// Fraction of assertion-level matches (0.0 to 1.0).
    pub assertion_match_rate: f64,
}

/// Performance comparison results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceComparison {
    /// NTM execution time (ms).
    pub ntm_duration_ms: u64,
    /// FT execution time (ms).
    pub ft_duration_ms: u64,
    /// Absolute difference (ms).
    pub delta_ms: i64,
    /// Relative speedup (positive = FT faster, negative = FT slower).
    pub speedup_ratio: f64,
    /// Whether performance is within acceptable threshold.
    pub within_threshold: bool,
    /// Performance threshold used (ms).
    pub threshold_ms: u64,
}

/// A specific field-level divergence between NTM and FT outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDivergence {
    /// Field path (e.g., "stdout", "exit_code", "json.panes[0].title").
    pub field: String,
    /// NTM value.
    pub ntm_value: String,
    /// FT value.
    pub ft_value: String,
    /// Whether this divergence is blocking.
    pub is_blocking: bool,
    /// Reason code for the divergence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
}

// ── Drift Classification ────────────────────────────────────────────────────

/// Classification of a divergence for triage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftClassification {
    /// Scenario that produced the divergence.
    pub scenario_id: String,
    /// Category of drift.
    pub category: DriftCategory,
    /// Severity assessment.
    pub severity: DriftSeverity,
    /// Human-readable description.
    pub description: String,
    /// Suggested resolution action.
    pub suggested_action: DriftAction,
    /// Whether this blocks cutover.
    pub blocks_cutover: bool,
    /// Related divergence fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_fields: Vec<String>,
}

/// Category of drift between NTM and FT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftCategory {
    /// Behavioral difference — different semantic outcomes.
    Behavioral,
    /// Output format difference — same data, different representation.
    Format,
    /// Performance difference — latency or throughput divergence.
    Performance,
    /// Feature gap — FT missing capability that NTM has.
    FeatureGap,
    /// Intentional improvement — FT deliberately differs from NTM.
    IntentionalImprovement,
    /// Infrastructure — difference caused by test environment.
    Infrastructure,
    /// Timing — race conditions or ordering differences.
    Timing,
}

/// Severity of a drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftSeverity {
    /// Informational — no action needed.
    Info,
    /// Low impact — track but don't block.
    Low,
    /// Medium impact — should resolve before cutover.
    Medium,
    /// High impact — must resolve before cutover.
    High,
    /// Critical — blocks cutover immediately.
    Critical,
}

/// Suggested action for resolving a drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftAction {
    /// No action needed — intentional or acceptable.
    Accept,
    /// Document the intentional delta.
    Document,
    /// Fix the FT implementation to match NTM.
    FixFt,
    /// The NTM behavior was wrong; FT is correct.
    AcknowledgeImprovement,
    /// Investigation needed to determine root cause.
    Investigate,
    /// Adjust the test/assertion to accommodate valid difference.
    AdjustTest,
    /// Defer to post-cutover cleanup.
    DeferPostCutover,
}

// ── Drift Triage Workflow ───────────────────────────────────────────────────

/// Resolution status for a triaged drift item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionStatus {
    /// Needs triage — not yet classified.
    Untriaged,
    /// Triaged and classified, action pending.
    Triaged,
    /// Action in progress.
    InProgress,
    /// Resolved.
    Resolved,
    /// Accepted as intentional delta — won't fix.
    Accepted,
    /// Deferred to post-cutover.
    Deferred,
}

/// A single drift item in the triage workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftTriageItem {
    /// Unique item ID.
    pub item_id: u64,
    /// Classification.
    pub classification: DriftClassification,
    /// Current resolution status.
    pub resolution_status: ResolutionStatus,
    /// Resolution notes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resolution_notes: String,
    /// Who owns this item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// When this was first observed.
    pub first_observed_ms: u64,
    /// When this was last observed (for recurring drifts).
    pub last_observed_ms: u64,
    /// Number of times observed.
    pub observation_count: u32,
}

/// Structured drift triage workflow manager.
///
/// Tracks all observed divergences, their classification, and resolution
/// status. Produces cutover gate evaluations from current triage state.
pub struct DriftTriageWorkflow {
    /// All triage items.
    items: Vec<DriftTriageItem>,
    /// Next item ID.
    next_item_id: u64,
    /// Performance threshold (ms) for flagging latency differences.
    perf_threshold_ms: u64,
    /// Maximum blocking divergences allowed for cutover.
    max_blocking_divergences: usize,
    /// Maximum high-priority divergences allowed for cutover.
    max_high_priority_divergences: usize,
    /// Telemetry.
    telemetry: TriageTelemetry,
}

/// Telemetry for the triage workflow.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriageTelemetry {
    /// Total comparisons performed.
    pub comparisons_performed: u64,
    /// Total divergences found.
    pub divergences_found: u64,
    /// Total items triaged.
    pub items_triaged: u64,
    /// Total items resolved.
    pub items_resolved: u64,
    /// Gate evaluations performed.
    pub gate_evaluations: u64,
}

/// Cutover gate evaluation result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CutoverGateEvaluation {
    /// Overall go/no-go decision.
    pub decision: CutoverDecision,
    /// Blocking divergences still open.
    pub open_blocking: usize,
    /// High-priority divergences still open.
    pub open_high_priority: usize,
    /// Total items triaged.
    pub total_triaged: usize,
    /// Total items resolved or accepted.
    pub total_resolved: usize,
    /// Total items deferred.
    pub total_deferred: usize,
    /// Gate check details.
    pub gate_checks: Vec<GateCheck>,
    /// Human-readable summary.
    pub summary: String,
}

/// Individual gate check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateCheck {
    /// Gate name.
    pub gate: String,
    /// Whether this gate passed.
    pub passed: bool,
    /// Reason for pass/fail.
    pub reason: String,
}

/// Cutover decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutoverDecision {
    /// All gates pass — safe to cut over.
    Go,
    /// Some gates fail — cutover blocked.
    NoGo,
    /// Manual review recommended.
    ReviewRequired,
}

impl DriftTriageWorkflow {
    /// Create a new triage workflow with configurable thresholds.
    #[must_use]
    pub fn new(
        perf_threshold_ms: u64,
        max_blocking_divergences: usize,
        max_high_priority_divergences: usize,
    ) -> Self {
        Self {
            items: Vec::new(),
            next_item_id: 1,
            perf_threshold_ms,
            max_blocking_divergences,
            max_high_priority_divergences,
            telemetry: TriageTelemetry::default(),
        }
    }

    /// Create with sensible defaults.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(500, 0, 3)
    }

    /// Compare a dual-run result and produce a comparison verdict.
    pub fn compare(&mut self, dual: &DualRunResult) -> ComparisonVerdict {
        self.telemetry.comparisons_performed += 1;

        // Handle execution failures
        if !dual.ntm.completed || !dual.ft.completed {
            return ComparisonVerdict {
                scenario_id: dual.scenario_id.clone(),
                match_status: MatchStatus::ExecutionFailure,
                semantic: SemanticComparison {
                    exit_code_match: false,
                    stdout_match: false,
                    stderr_match: false,
                    json_structure_match: None,
                    assertion_match_rate: 0.0,
                },
                performance: PerformanceComparison {
                    ntm_duration_ms: dual.ntm.duration_ms,
                    ft_duration_ms: dual.ft.duration_ms,
                    delta_ms: 0,
                    speedup_ratio: 0.0,
                    within_threshold: false,
                    threshold_ms: self.perf_threshold_ms,
                },
                divergences: Vec::new(),
                summary: format!(
                    "Execution failure: NTM completed={}, FT completed={}",
                    dual.ntm.completed, dual.ft.completed,
                ),
            };
        }

        let mut divergences = Vec::new();

        // Compare exit codes
        let exit_code_match = dual.ntm.exit_code == dual.ft.exit_code;
        if !exit_code_match {
            divergences.push(FieldDivergence {
                field: "exit_code".to_string(),
                ntm_value: format!("{:?}", dual.ntm.exit_code),
                ft_value: format!("{:?}", dual.ft.exit_code),
                is_blocking: dual.priority == DualRunPriority::Blocking,
                reason_code: Some("EXIT_CODE_MISMATCH".to_string()),
            });
        }

        // Compare stdout (normalized: trim trailing whitespace)
        let ntm_stdout = dual.ntm.stdout.trim_end();
        let ft_stdout = dual.ft.stdout.trim_end();
        let stdout_match = ntm_stdout == ft_stdout;
        if !stdout_match {
            divergences.push(FieldDivergence {
                field: "stdout".to_string(),
                ntm_value: truncate(ntm_stdout, 200),
                ft_value: truncate(ft_stdout, 200),
                is_blocking: dual.priority == DualRunPriority::Blocking,
                reason_code: Some("STDOUT_MISMATCH".to_string()),
            });
        }

        // Compare stderr (normalized)
        let ntm_stderr = dual.ntm.stderr.trim_end();
        let ft_stderr = dual.ft.stderr.trim_end();
        let stderr_match = ntm_stderr == ft_stderr;
        if !stderr_match && !ntm_stderr.is_empty() && !ft_stderr.is_empty() {
            divergences.push(FieldDivergence {
                field: "stderr".to_string(),
                ntm_value: truncate(ntm_stderr, 200),
                ft_value: truncate(ft_stderr, 200),
                is_blocking: false,
                reason_code: Some("STDERR_MISMATCH".to_string()),
            });
        }

        // JSON structure comparison (if both outputs parse as JSON)
        let json_structure_match = if let (Ok(ntm_json), Ok(ft_json)) = (
            serde_json::from_str::<serde_json::Value>(ntm_stdout),
            serde_json::from_str::<serde_json::Value>(ft_stdout),
        ) {
            Some(json_keys_match(&ntm_json, &ft_json))
        } else {
            None
        };

        // Performance comparison
        let ntm_ms = dual.ntm.duration_ms;
        let ft_ms = dual.ft.duration_ms;
        let delta_ms = ft_ms as i64 - ntm_ms as i64;
        let speedup_ratio = if ntm_ms > 0 {
            ntm_ms as f64 / ft_ms.max(1) as f64
        } else {
            1.0
        };
        let within_threshold = delta_ms.unsigned_abs() <= self.perf_threshold_ms;

        if !within_threshold {
            divergences.push(FieldDivergence {
                field: "duration_ms".to_string(),
                ntm_value: format!("{ntm_ms}ms"),
                ft_value: format!("{ft_ms}ms"),
                is_blocking: false,
                reason_code: Some("PERF_THRESHOLD_EXCEEDED".to_string()),
            });
        }

        // Determine assertion match rate
        let total_checks = 3u32; // exit_code, stdout, stderr
        let passed = exit_code_match as u32 + stdout_match as u32 + stderr_match as u32;
        let assertion_match_rate = passed as f64 / total_checks as f64;

        // Determine overall match status
        let match_status = if divergences.is_empty() {
            MatchStatus::Match
        } else if divergences.iter().any(|d| d.is_blocking) {
            MatchStatus::Divergence
        } else {
            MatchStatus::IntentionalDelta
        };

        if !divergences.is_empty() {
            self.telemetry.divergences_found += divergences.len() as u64;
        }

        let summary = match match_status {
            MatchStatus::Match => format!("{}: outputs match", dual.scenario_id),
            MatchStatus::IntentionalDelta => format!(
                "{}: {} non-blocking divergences",
                dual.scenario_id,
                divergences.len()
            ),
            MatchStatus::Divergence => format!(
                "{}: {} divergences ({} blocking)",
                dual.scenario_id,
                divergences.len(),
                divergences.iter().filter(|d| d.is_blocking).count()
            ),
            MatchStatus::ExecutionFailure => {
                format!("{}: execution failure", dual.scenario_id)
            }
            MatchStatus::Inconclusive => format!("{}: inconclusive", dual.scenario_id),
        };

        ComparisonVerdict {
            scenario_id: dual.scenario_id.clone(),
            match_status,
            semantic: SemanticComparison {
                exit_code_match,
                stdout_match,
                stderr_match,
                json_structure_match,
                assertion_match_rate,
            },
            performance: PerformanceComparison {
                ntm_duration_ms: ntm_ms,
                ft_duration_ms: ft_ms,
                delta_ms,
                speedup_ratio,
                within_threshold,
                threshold_ms: self.perf_threshold_ms,
            },
            divergences,
            summary,
        }
    }

    /// Classify a divergence and add it to the triage queue.
    pub fn classify_and_add(
        &mut self,
        scenario_id: &str,
        verdict: &ComparisonVerdict,
        now_ms: u64,
    ) -> Vec<u64> {
        let mut ids = Vec::new();

        for divergence in &verdict.divergences {
            let (category, severity, action) =
                classify_divergence(divergence, &verdict.performance);

            let blocks_cutover = severity >= DriftSeverity::High && divergence.is_blocking;

            let classification = DriftClassification {
                scenario_id: scenario_id.to_string(),
                category,
                severity,
                description: format!(
                    "{}: NTM={}, FT={}",
                    divergence.field, divergence.ntm_value, divergence.ft_value
                ),
                suggested_action: action,
                blocks_cutover,
                related_fields: vec![divergence.field.clone()],
            };

            // Check if we've seen this scenario+field before
            let existing = self.items.iter_mut().find(|item| {
                item.classification.scenario_id == scenario_id
                    && item.classification.related_fields == classification.related_fields
            });

            if let Some(item) = existing {
                item.last_observed_ms = now_ms;
                item.observation_count += 1;
                ids.push(item.item_id);
            } else {
                let item_id = self.next_item_id;
                self.next_item_id += 1;

                self.items.push(DriftTriageItem {
                    item_id,
                    classification,
                    resolution_status: ResolutionStatus::Untriaged,
                    resolution_notes: String::new(),
                    owner: None,
                    first_observed_ms: now_ms,
                    last_observed_ms: now_ms,
                    observation_count: 1,
                });

                ids.push(item_id);
            }
        }

        ids
    }

    /// Triage an item — classify and assign owner.
    pub fn triage_item(&mut self, item_id: u64, owner: Option<String>, notes: &str) -> bool {
        if let Some(item) = self.items.iter_mut().find(|i| i.item_id == item_id) {
            item.resolution_status = ResolutionStatus::Triaged;
            item.owner = owner;
            if !notes.is_empty() {
                item.resolution_notes = notes.to_string();
            }
            self.telemetry.items_triaged += 1;
            return true;
        }
        false
    }

    /// Resolve a triage item.
    pub fn resolve_item(&mut self, item_id: u64, status: ResolutionStatus, notes: &str) -> bool {
        if let Some(item) = self.items.iter_mut().find(|i| i.item_id == item_id) {
            item.resolution_status = status;
            if !notes.is_empty() {
                item.resolution_notes = notes.to_string();
            }
            if matches!(
                status,
                ResolutionStatus::Resolved | ResolutionStatus::Accepted
            ) {
                self.telemetry.items_resolved += 1;
            }
            return true;
        }
        false
    }

    /// Get a triage item by ID.
    #[must_use]
    pub fn get_item(&self, item_id: u64) -> Option<&DriftTriageItem> {
        self.items.iter().find(|i| i.item_id == item_id)
    }

    /// Get all items with a given resolution status.
    #[must_use]
    pub fn items_by_status(&self, status: ResolutionStatus) -> Vec<&DriftTriageItem> {
        self.items
            .iter()
            .filter(|i| i.resolution_status == status)
            .collect()
    }

    /// Get all items for a given scenario.
    #[must_use]
    pub fn items_for_scenario(&self, scenario_id: &str) -> Vec<&DriftTriageItem> {
        self.items
            .iter()
            .filter(|i| i.classification.scenario_id == scenario_id)
            .collect()
    }

    /// Total number of triage items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there are no triage items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Evaluate cutover gates based on current triage state.
    pub fn evaluate_cutover_gate(&mut self) -> CutoverGateEvaluation {
        self.telemetry.gate_evaluations += 1;

        let open_blocking = self
            .items
            .iter()
            .filter(|i| {
                i.classification.blocks_cutover
                    && !matches!(
                        i.resolution_status,
                        ResolutionStatus::Resolved
                            | ResolutionStatus::Accepted
                            | ResolutionStatus::Deferred
                    )
            })
            .count();

        let open_high_priority = self
            .items
            .iter()
            .filter(|i| {
                i.classification.severity >= DriftSeverity::High
                    && !i.classification.blocks_cutover
                    && !matches!(
                        i.resolution_status,
                        ResolutionStatus::Resolved
                            | ResolutionStatus::Accepted
                            | ResolutionStatus::Deferred
                    )
            })
            .count();

        let total_triaged = self
            .items
            .iter()
            .filter(|i| i.resolution_status != ResolutionStatus::Untriaged)
            .count();

        let total_resolved = self
            .items
            .iter()
            .filter(|i| {
                matches!(
                    i.resolution_status,
                    ResolutionStatus::Resolved | ResolutionStatus::Accepted
                )
            })
            .count();

        let total_deferred = self
            .items
            .iter()
            .filter(|i| i.resolution_status == ResolutionStatus::Deferred)
            .count();

        let untriaged_count = self
            .items
            .iter()
            .filter(|i| i.resolution_status == ResolutionStatus::Untriaged)
            .count();

        // Gate checks
        let mut gate_checks = Vec::new();

        // G-01: No open blocking divergences
        let g01_pass = open_blocking <= self.max_blocking_divergences;
        gate_checks.push(GateCheck {
            gate: "G-01-blocking".to_string(),
            passed: g01_pass,
            reason: format!(
                "{open_blocking} open blocking divergences (max: {})",
                self.max_blocking_divergences
            ),
        });

        // G-02: High-priority divergences within budget
        let g02_pass = open_high_priority <= self.max_high_priority_divergences;
        gate_checks.push(GateCheck {
            gate: "G-02-high-priority".to_string(),
            passed: g02_pass,
            reason: format!(
                "{open_high_priority} open high-priority divergences (max: {})",
                self.max_high_priority_divergences
            ),
        });

        // G-03: All items triaged (no untriaged items)
        let g03_pass = untriaged_count == 0;
        gate_checks.push(GateCheck {
            gate: "G-03-all-triaged".to_string(),
            passed: g03_pass,
            reason: format!("{untriaged_count} items still untriaged"),
        });

        let all_pass = gate_checks.iter().all(|g| g.passed);
        let decision = if all_pass {
            CutoverDecision::Go
        } else if !g01_pass {
            CutoverDecision::NoGo
        } else {
            CutoverDecision::ReviewRequired
        };

        let summary = match decision {
            CutoverDecision::Go => {
                format!("GO: All gates pass. {total_resolved} resolved, {total_deferred} deferred.")
            }
            CutoverDecision::NoGo => format!("NO-GO: {open_blocking} blocking divergences remain."),
            CutoverDecision::ReviewRequired => format!(
                "REVIEW: No blocking divergences but {open_high_priority} high-priority \
                 and {untriaged_count} untriaged items remain."
            ),
        };

        CutoverGateEvaluation {
            decision,
            open_blocking,
            open_high_priority,
            total_triaged,
            total_resolved,
            total_deferred,
            gate_checks,
            summary,
        }
    }

    /// Get telemetry snapshot.
    #[must_use]
    pub fn telemetry(&self) -> &TriageTelemetry {
        &self.telemetry
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Truncate a string to max_len, appending "..." if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

/// Compare JSON key structure (ignoring values).
fn json_keys_match(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a, b) {
        (serde_json::Value::Object(ma), serde_json::Value::Object(mb)) => {
            let keys_a: std::collections::HashSet<&String> = ma.keys().collect();
            let keys_b: std::collections::HashSet<&String> = mb.keys().collect();
            keys_a == keys_b
        }
        (serde_json::Value::Array(va), serde_json::Value::Array(vb)) => va.len() == vb.len(),
        _ => std::mem::discriminant(a) == std::mem::discriminant(b),
    }
}

/// Classify a divergence based on its characteristics.
fn classify_divergence(
    div: &FieldDivergence,
    perf: &PerformanceComparison,
) -> (DriftCategory, DriftSeverity, DriftAction) {
    match div.reason_code.as_deref() {
        Some("PERF_THRESHOLD_EXCEEDED") => {
            let severity = if perf.delta_ms.unsigned_abs() > 2000 {
                DriftSeverity::High
            } else if perf.delta_ms.unsigned_abs() > 1000 {
                DriftSeverity::Medium
            } else {
                DriftSeverity::Low
            };
            (
                DriftCategory::Performance,
                severity,
                DriftAction::Investigate,
            )
        }
        Some("EXIT_CODE_MISMATCH") => {
            if div.is_blocking {
                (
                    DriftCategory::Behavioral,
                    DriftSeverity::Critical,
                    DriftAction::FixFt,
                )
            } else {
                (
                    DriftCategory::Behavioral,
                    DriftSeverity::High,
                    DriftAction::Investigate,
                )
            }
        }
        Some("STDOUT_MISMATCH") => {
            if div.is_blocking {
                (
                    DriftCategory::Behavioral,
                    DriftSeverity::High,
                    DriftAction::FixFt,
                )
            } else {
                (
                    DriftCategory::Format,
                    DriftSeverity::Medium,
                    DriftAction::Investigate,
                )
            }
        }
        Some("STDERR_MISMATCH") => (
            DriftCategory::Format,
            DriftSeverity::Low,
            DriftAction::Accept,
        ),
        _ => (
            DriftCategory::Behavioral,
            DriftSeverity::Medium,
            DriftAction::Investigate,
        ),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_capture(system: &str, stdout: &str, exit_code: i32, duration_ms: u64) -> RunCapture {
        RunCapture {
            system: system.to_string(),
            command: "ft watch".to_string(),
            exit_code: Some(exit_code),
            stdout: stdout.to_string(),
            stderr: String::new(),
            duration_ms,
            completed: true,
            error: None,
        }
    }

    fn make_dual(
        scenario_id: &str,
        ntm: RunCapture,
        ft: RunCapture,
        priority: DualRunPriority,
    ) -> DualRunResult {
        DualRunResult {
            scenario_id: scenario_id.to_string(),
            domain: "test".to_string(),
            priority,
            ntm,
            ft,
            compared_at_ms: 1000,
            correlation_id: None,
        }
    }

    // -- Comparison tests --

    #[test]
    fn compare_matching_outputs() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s1",
            make_capture("ntm", "output", 0, 100),
            make_capture("ft", "output", 0, 120),
            DualRunPriority::Blocking,
        );
        let verdict = wf.compare(&dual);
        assert_eq!(verdict.match_status, MatchStatus::Match);
        assert!(verdict.semantic.exit_code_match);
        assert!(verdict.semantic.stdout_match);
        assert_eq!(verdict.divergences.len(), 0);
    }

    #[test]
    fn compare_exit_code_mismatch() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s2",
            make_capture("ntm", "output", 0, 100),
            make_capture("ft", "output", 1, 100),
            DualRunPriority::Blocking,
        );
        let verdict = wf.compare(&dual);
        assert_eq!(verdict.match_status, MatchStatus::Divergence);
        assert!(!verdict.semantic.exit_code_match);
        assert!(verdict.divergences.iter().any(|d| d.field == "exit_code"));
    }

    #[test]
    fn compare_stdout_mismatch_non_blocking() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s3",
            make_capture("ntm", "hello", 0, 100),
            make_capture("ft", "world", 0, 100),
            DualRunPriority::Low,
        );
        let verdict = wf.compare(&dual);
        // Non-blocking divergence → IntentionalDelta
        assert_eq!(verdict.match_status, MatchStatus::IntentionalDelta);
    }

    #[test]
    fn compare_stdout_mismatch_blocking() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s4",
            make_capture("ntm", "hello", 0, 100),
            make_capture("ft", "world", 0, 100),
            DualRunPriority::Blocking,
        );
        let verdict = wf.compare(&dual);
        assert_eq!(verdict.match_status, MatchStatus::Divergence);
    }

    #[test]
    fn compare_execution_failure() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let mut ft_capture = make_capture("ft", "", 1, 0);
        ft_capture.completed = false;
        ft_capture.error = Some("timeout".to_string());

        let dual = make_dual(
            "s5",
            make_capture("ntm", "ok", 0, 100),
            ft_capture,
            DualRunPriority::Blocking,
        );
        let verdict = wf.compare(&dual);
        assert_eq!(verdict.match_status, MatchStatus::ExecutionFailure);
    }

    #[test]
    fn compare_performance_within_threshold() {
        let mut wf = DriftTriageWorkflow::new(200, 0, 3);
        let dual = make_dual(
            "s6",
            make_capture("ntm", "ok", 0, 100),
            make_capture("ft", "ok", 0, 250),
            DualRunPriority::High,
        );
        let verdict = wf.compare(&dual);
        assert!(verdict.performance.within_threshold);
    }

    #[test]
    fn compare_performance_exceeds_threshold() {
        let mut wf = DriftTriageWorkflow::new(100, 0, 3);
        let dual = make_dual(
            "s7",
            make_capture("ntm", "ok", 0, 100),
            make_capture("ft", "ok", 0, 500),
            DualRunPriority::High,
        );
        let verdict = wf.compare(&dual);
        assert!(!verdict.performance.within_threshold);
        assert!(verdict.divergences.iter().any(|d| d.field == "duration_ms"));
    }

    #[test]
    fn compare_json_structure_match() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s8",
            make_capture("ntm", r#"{"panes": [1], "count": 1}"#, 0, 100),
            make_capture("ft", r#"{"panes": [2], "count": 5}"#, 0, 100),
            DualRunPriority::High,
        );
        let verdict = wf.compare(&dual);
        assert_eq!(verdict.semantic.json_structure_match, Some(true));
    }

    #[test]
    fn compare_json_structure_mismatch() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s9",
            make_capture("ntm", r#"{"panes": [1]}"#, 0, 100),
            make_capture("ft", r#"{"panes": [1], "extra": true}"#, 0, 100),
            DualRunPriority::High,
        );
        let verdict = wf.compare(&dual);
        assert_eq!(verdict.semantic.json_structure_match, Some(false));
    }

    #[test]
    fn compare_whitespace_normalization() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s10",
            make_capture("ntm", "output\n\n", 0, 100),
            make_capture("ft", "output\n", 0, 100),
            DualRunPriority::High,
        );
        let verdict = wf.compare(&dual);
        assert!(verdict.semantic.stdout_match);
    }

    // -- Triage workflow tests --

    #[test]
    fn classify_and_add_divergences() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s1",
            make_capture("ntm", "hello", 0, 100),
            make_capture("ft", "world", 1, 100),
            DualRunPriority::Blocking,
        );
        let verdict = wf.compare(&dual);
        let ids = wf.classify_and_add("s1", &verdict, 1000);
        assert!(!ids.is_empty());
        assert_eq!(wf.len(), ids.len());
    }

    #[test]
    fn triage_item_workflow() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s1",
            make_capture("ntm", "a", 0, 100),
            make_capture("ft", "b", 0, 100),
            DualRunPriority::High,
        );
        let verdict = wf.compare(&dual);
        let ids = wf.classify_and_add("s1", &verdict, 1000);
        let id = ids[0];

        // Initially untriaged
        assert_eq!(
            wf.get_item(id).unwrap().resolution_status,
            ResolutionStatus::Untriaged
        );

        // Triage it
        assert!(wf.triage_item(id, Some("PinkForge".into()), "Investigating stdout diff"));
        assert_eq!(
            wf.get_item(id).unwrap().resolution_status,
            ResolutionStatus::Triaged
        );
        assert_eq!(wf.get_item(id).unwrap().owner.as_deref(), Some("PinkForge"));

        // Resolve it
        assert!(wf.resolve_item(id, ResolutionStatus::Resolved, "Fixed in ft"));
        assert_eq!(
            wf.get_item(id).unwrap().resolution_status,
            ResolutionStatus::Resolved
        );
    }

    #[test]
    fn triage_nonexistent_item() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        assert!(!wf.triage_item(999, None, ""));
    }

    #[test]
    fn resolve_nonexistent_item() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        assert!(!wf.resolve_item(999, ResolutionStatus::Resolved, ""));
    }

    #[test]
    fn items_by_status_filter() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s1",
            make_capture("ntm", "a", 0, 100),
            make_capture("ft", "b", 1, 100),
            DualRunPriority::Blocking,
        );
        let verdict = wf.compare(&dual);
        let ids = wf.classify_and_add("s1", &verdict, 1000);

        assert!(!wf.items_by_status(ResolutionStatus::Untriaged).is_empty());
        assert!(wf.items_by_status(ResolutionStatus::Resolved).is_empty());

        wf.resolve_item(ids[0], ResolutionStatus::Resolved, "fixed");
        assert!(!wf.items_by_status(ResolutionStatus::Resolved).is_empty());
    }

    #[test]
    fn items_for_scenario_filter() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let d1 = make_dual(
            "s1",
            make_capture("ntm", "a", 0, 100),
            make_capture("ft", "b", 0, 100),
            DualRunPriority::High,
        );
        let d2 = make_dual(
            "s2",
            make_capture("ntm", "x", 0, 100),
            make_capture("ft", "y", 0, 100),
            DualRunPriority::High,
        );
        let v1 = wf.compare(&d1);
        let v2 = wf.compare(&d2);
        wf.classify_and_add("s1", &v1, 1000);
        wf.classify_and_add("s2", &v2, 1001);

        assert_eq!(wf.items_for_scenario("s1").len(), 1);
        assert_eq!(wf.items_for_scenario("s2").len(), 1);
    }

    #[test]
    fn repeated_observation_increments_count() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s1",
            make_capture("ntm", "a", 0, 100),
            make_capture("ft", "b", 0, 100),
            DualRunPriority::High,
        );
        let verdict = wf.compare(&dual);
        let ids1 = wf.classify_and_add("s1", &verdict, 1000);
        let ids2 = wf.classify_and_add("s1", &verdict, 2000);

        // Same item ID should be returned
        assert_eq!(ids1[0], ids2[0]);
        // Observation count should be 2
        assert_eq!(wf.get_item(ids1[0]).unwrap().observation_count, 2);
        assert_eq!(wf.get_item(ids1[0]).unwrap().last_observed_ms, 2000);
    }

    // -- Cutover gate tests --

    #[test]
    fn gate_go_when_all_resolved() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s1",
            make_capture("ntm", "a", 0, 100),
            make_capture("ft", "b", 0, 100),
            DualRunPriority::High,
        );
        let verdict = wf.compare(&dual);
        let ids = wf.classify_and_add("s1", &verdict, 1000);

        for id in ids {
            wf.triage_item(id, None, "");
            wf.resolve_item(id, ResolutionStatus::Resolved, "fixed");
        }

        let gate = wf.evaluate_cutover_gate();
        assert_eq!(gate.decision, CutoverDecision::Go);
        assert!(gate.gate_checks.iter().all(|g| g.passed));
    }

    #[test]
    fn gate_nogo_with_blocking_divergence() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s1",
            make_capture("ntm", "a", 0, 100),
            make_capture("ft", "b", 1, 100),
            DualRunPriority::Blocking,
        );
        let verdict = wf.compare(&dual);
        wf.classify_and_add("s1", &verdict, 1000);

        let gate = wf.evaluate_cutover_gate();
        assert_eq!(gate.decision, CutoverDecision::NoGo);
        assert!(gate.open_blocking > 0);
    }

    #[test]
    fn gate_review_with_untriaged_items() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s1",
            make_capture("ntm", "a", 0, 100),
            make_capture("ft", "b", 0, 100),
            DualRunPriority::Medium,
        );
        let verdict = wf.compare(&dual);
        wf.classify_and_add("s1", &verdict, 1000);

        let gate = wf.evaluate_cutover_gate();
        assert_eq!(gate.decision, CutoverDecision::ReviewRequired);
    }

    #[test]
    fn gate_go_empty_workflow() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let gate = wf.evaluate_cutover_gate();
        assert_eq!(gate.decision, CutoverDecision::Go);
    }

    #[test]
    fn gate_deferred_items_dont_block() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s1",
            make_capture("ntm", "a", 0, 100),
            make_capture("ft", "b", 1, 100),
            DualRunPriority::Blocking,
        );
        let verdict = wf.compare(&dual);
        let ids = wf.classify_and_add("s1", &verdict, 1000);

        for id in ids {
            wf.triage_item(id, None, "");
            wf.resolve_item(id, ResolutionStatus::Deferred, "post-cutover");
        }

        let gate = wf.evaluate_cutover_gate();
        assert_eq!(gate.decision, CutoverDecision::Go);
    }

    // -- Telemetry tests --

    #[test]
    fn telemetry_tracks_comparisons() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s1",
            make_capture("ntm", "ok", 0, 100),
            make_capture("ft", "ok", 0, 100),
            DualRunPriority::High,
        );
        wf.compare(&dual);
        wf.compare(&dual);
        assert_eq!(wf.telemetry().comparisons_performed, 2);
    }

    #[test]
    fn telemetry_tracks_gate_evaluations() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        wf.evaluate_cutover_gate();
        assert_eq!(wf.telemetry().gate_evaluations, 1);
    }

    // -- Serde roundtrip tests --

    #[test]
    fn dual_run_result_serde_roundtrip() {
        let dual = make_dual(
            "s1",
            make_capture("ntm", "ok", 0, 100),
            make_capture("ft", "ok", 0, 120),
            DualRunPriority::Blocking,
        );
        let json = serde_json::to_string(&dual).unwrap();
        let dual2: DualRunResult = serde_json::from_str(&json).unwrap();
        assert_eq!(dual2.scenario_id, "s1");
        assert_eq!(dual2.priority, DualRunPriority::Blocking);
    }

    #[test]
    fn comparison_verdict_serde_roundtrip() {
        let mut wf = DriftTriageWorkflow::with_defaults();
        let dual = make_dual(
            "s1",
            make_capture("ntm", "a", 0, 100),
            make_capture("ft", "b", 0, 100),
            DualRunPriority::High,
        );
        let verdict = wf.compare(&dual);
        let json = serde_json::to_string(&verdict).unwrap();
        let verdict2: ComparisonVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(verdict2.scenario_id, "s1");
    }

    #[test]
    fn cutover_gate_serde_roundtrip() {
        let gate = CutoverGateEvaluation {
            decision: CutoverDecision::Go,
            open_blocking: 0,
            open_high_priority: 0,
            total_triaged: 5,
            total_resolved: 5,
            total_deferred: 0,
            gate_checks: vec![GateCheck {
                gate: "G-01".to_string(),
                passed: true,
                reason: "0 blocking".to_string(),
            }],
            summary: "GO".to_string(),
        };
        let json = serde_json::to_string(&gate).unwrap();
        let gate2: CutoverGateEvaluation = serde_json::from_str(&json).unwrap();
        assert_eq!(gate2.decision, CutoverDecision::Go);
    }

    #[test]
    fn drift_severity_ordering() {
        assert!(DriftSeverity::Info < DriftSeverity::Low);
        assert!(DriftSeverity::Low < DriftSeverity::Medium);
        assert!(DriftSeverity::Medium < DriftSeverity::High);
        assert!(DriftSeverity::High < DriftSeverity::Critical);
    }

    #[test]
    fn dual_run_priority_ordering() {
        assert!(DualRunPriority::Blocking < DualRunPriority::High);
        assert!(DualRunPriority::High < DualRunPriority::Medium);
        assert!(DualRunPriority::Medium < DualRunPriority::Low);
    }
}
