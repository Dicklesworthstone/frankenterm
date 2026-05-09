#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::module_name_repetitions)]

//! Token/IO rate-distortion controller for swarm capture and robot output.
//!
//! The controller chooses a bounded capture/output profile from explicit
//! resource budgets while keeping correctness-sensitive information above
//! configured floors. It is intentionally a pure substrate: CLI, MCP, search,
//! capture, and replay call sites can ask for a profile without changing their
//! wire contracts.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::byte_compression::CompressionLevel;

const TOKEN_BYTES: usize = 4;

/// How much pane content should be retained before compression and formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureDetail {
    /// Preserve only critical lines plus a short tail.
    Minimal,
    /// Preserve critical lines plus a mission-sized tail window.
    Tail,
    /// Preserve tail, critical context, and enough body content for search.
    Balanced,
    /// Preserve the full pane transcript.
    Full,
}

impl CaptureDetail {
    fn retention_floor(self) -> f64 {
        match self {
            Self::Minimal => 0.08,
            Self::Tail => 0.18,
            Self::Balanced => 0.58,
            Self::Full => 1.0,
        }
    }

    fn replay_fidelity_floor(self) -> f64 {
        match self {
            Self::Minimal => 0.62,
            Self::Tail => 0.80,
            Self::Balanced => 0.94,
            Self::Full => 1.0,
        }
    }
}

/// Semantic line-template compression aggressiveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticCompressionMode {
    /// Do not template repeated lines.
    Disabled,
    /// Template obvious repetitions while preserving most literal context.
    Conservative,
    /// Prefer template instances for repeated output when budgets are tight.
    Aggressive,
}

impl SemanticCompressionMode {
    fn retained_fraction(self, repeated_line_ratio: f64) -> f64 {
        let repeated = repeated_line_ratio.clamp(0.0, 1.0);
        match self {
            Self::Disabled => 1.0,
            Self::Conservative => (1.0 - repeated * 0.35).max(0.55),
            Self::Aggressive => (1.0 - repeated * 0.62).max(0.24),
        }
    }

    fn cpu_multiplier(self) -> f64 {
        match self {
            Self::Disabled => 1.0,
            Self::Conservative => 1.16,
            Self::Aggressive => 1.34,
        }
    }
}

/// Robot wire format preference selected by the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RobotFormatPreference {
    /// JSON is larger but cheapest to produce and universally familiar.
    Json,
    /// TOON is preferred when responses are consumed by other agents.
    Toon,
}

impl RobotFormatPreference {
    fn token_multiplier(self) -> f64 {
        match self {
            Self::Json => 1.0,
            Self::Toon => 0.62,
        }
    }

    fn cpu_multiplier(self) -> f64 {
        match self {
            Self::Json => 1.0,
            Self::Toon => 1.06,
        }
    }
}

/// Compression/capture profile selected for one robot or capture request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateDistortionProfile {
    /// Stable profile identifier for logs and golden evidence.
    pub id: String,
    /// Capture detail retained before token formatting.
    pub capture_detail: CaptureDetail,
    /// Semantic line-template compression policy.
    pub semantic_compression: SemanticCompressionMode,
    /// Byte compression level for stored or transferred blobs.
    pub byte_compression_level: CompressionLevel,
    /// Preferred robot output format.
    pub robot_format: RobotFormatPreference,
    /// Pane tail lines retained per pane.
    pub tail_lines: usize,
    /// Search result excerpts retained per response.
    pub search_excerpt_count: usize,
    /// Maximum chars per search excerpt.
    pub search_excerpt_chars: usize,
    /// Replay context lines retained around decision events.
    pub replay_context_lines: usize,
    /// Context lines retained around critical pattern hits.
    pub critical_context_lines: usize,
    /// Whether replay/provenance hashes should be retained even when text is compressed.
    pub include_provenance_hashes: bool,
    /// Whether critical detection lines are protected from tail truncation.
    pub preserve_critical_detections: bool,
}

impl RateDistortionProfile {
    /// Fixed conservative fallback used when controller inputs are unavailable.
    #[must_use]
    pub fn conservative_fallback() -> Self {
        Self {
            id: "conservative_fallback".to_string(),
            capture_detail: CaptureDetail::Tail,
            semantic_compression: SemanticCompressionMode::Conservative,
            byte_compression_level: CompressionLevel::Fast,
            robot_format: RobotFormatPreference::Toon,
            tail_lines: 120,
            search_excerpt_count: 6,
            search_excerpt_chars: 280,
            replay_context_lines: 6,
            critical_context_lines: 2,
            include_provenance_hashes: true,
            preserve_critical_detections: true,
        }
    }

    /// Full-fidelity baseline profile for comparison reports.
    #[must_use]
    pub fn full_fidelity() -> Self {
        Self {
            id: "full_fidelity".to_string(),
            capture_detail: CaptureDetail::Full,
            semantic_compression: SemanticCompressionMode::Disabled,
            byte_compression_level: CompressionLevel::Default,
            robot_format: RobotFormatPreference::Json,
            tail_lines: usize::MAX,
            search_excerpt_count: usize::MAX,
            search_excerpt_chars: usize::MAX,
            replay_context_lines: usize::MAX / 2,
            critical_context_lines: usize::MAX / 2,
            include_provenance_hashes: true,
            preserve_critical_detections: true,
        }
    }
}

/// Resource and correctness floors for selecting a profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateDistortionBudget {
    /// Maximum estimated output tokens per response.
    pub max_tokens_per_response: usize,
    /// Maximum estimated CPU work per pane in microseconds.
    pub max_cpu_micros_per_pane: u64,
    /// Maximum retained memory bytes for the response.
    pub max_memory_bytes: u64,
    /// Maximum estimated response latency.
    pub max_latency_ms: u64,
    /// Minimum pattern correctness recall, with critical hits weighted highest.
    pub min_pattern_recall: f64,
    /// Minimum search correctness recall.
    pub min_search_recall: f64,
    /// Minimum replay reconstruction/provenance fidelity.
    pub min_replay_fidelity: f64,
}

impl Default for RateDistortionBudget {
    fn default() -> Self {
        Self {
            max_tokens_per_response: 32_000,
            max_cpu_micros_per_pane: 1_500,
            max_memory_bytes: 32 * 1024 * 1024,
            max_latency_ms: 250,
            min_pattern_recall: 0.99,
            min_search_recall: 0.90,
            min_replay_fidelity: 0.92,
        }
    }
}

/// Per-pane observations used to choose and evaluate a profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneRateDistortionObservation {
    /// Pane identifier.
    pub pane_id: u64,
    /// Raw bytes available before reduction.
    pub raw_bytes: usize,
    /// Raw line count available before reduction.
    pub line_count: usize,
    /// Number of correctness-critical pattern hits in this pane.
    pub critical_detection_count: usize,
    /// Fraction of lines that are repeated/template-friendly.
    pub repeated_line_ratio: f64,
}

impl PaneRateDistortionObservation {
    /// Create a bounded observation with sanitized ratios.
    #[must_use]
    pub fn new(
        pane_id: u64,
        raw_bytes: usize,
        line_count: usize,
        critical_detection_count: usize,
        repeated_line_ratio: f64,
    ) -> Self {
        Self {
            pane_id,
            raw_bytes,
            line_count,
            critical_detection_count,
            repeated_line_ratio: repeated_line_ratio.clamp(0.0, 1.0),
        }
    }
}

/// Mission and incident pressure that affect distortion weights.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateDistortionPressure {
    /// Current incident risk, 0.0 to 1.0.
    pub incident_risk: f64,
    /// Search/query pressure, 0.0 to 1.0.
    pub search_pressure: f64,
    /// Replay/debugging pressure, 0.0 to 1.0.
    pub replay_pressure: f64,
    /// Context-window pressure, 0.0 to 1.0.
    pub context_pressure: f64,
}

impl RateDistortionPressure {
    fn sanitized(self) -> Self {
        Self {
            incident_risk: self.incident_risk.clamp(0.0, 1.0),
            search_pressure: self.search_pressure.clamp(0.0, 1.0),
            replay_pressure: self.replay_pressure.clamp(0.0, 1.0),
            context_pressure: self.context_pressure.clamp(0.0, 1.0),
        }
    }
}

/// Controller input for one profile selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateDistortionInput {
    /// Candidate pane observations.
    pub panes: Vec<PaneRateDistortionObservation>,
    /// Hard budgets and correctness floors.
    pub budget: RateDistortionBudget,
    /// Current mission pressure.
    pub pressure: RateDistortionPressure,
    /// If true, use full fidelity regardless of budget scoring.
    pub operator_requested_full_fidelity: bool,
}

impl Default for RateDistortionInput {
    fn default() -> Self {
        Self {
            panes: Vec::new(),
            budget: RateDistortionBudget::default(),
            pressure: RateDistortionPressure::default(),
            operator_requested_full_fidelity: false,
        }
    }
}

/// Estimated correctness and resource metrics for a profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DistortionMetrics {
    /// Estimated output tokens.
    pub estimated_tokens: usize,
    /// Estimated retained memory bytes.
    pub estimated_memory_bytes: u64,
    /// Estimated total CPU microseconds.
    pub estimated_cpu_micros: u64,
    /// Estimated response latency milliseconds.
    pub estimated_latency_ms: u64,
    /// Fraction of critical pattern correctness retained.
    pub pattern_recall: f64,
    /// Fraction of search-relevant context retained.
    pub search_recall: f64,
    /// Fraction of replay/provenance fidelity retained.
    pub replay_fidelity: f64,
    /// Human-readable context retained.
    pub context_recall: f64,
    /// Raw input bytes considered.
    pub raw_bytes: usize,
    /// Raw line count considered.
    pub raw_lines: usize,
    /// Critical detections considered.
    pub critical_detections: usize,
}

impl DistortionMetrics {
    fn correctness_floor_violation(&self, budget: &RateDistortionBudget) -> f64 {
        let pattern_violation = (budget.min_pattern_recall - self.pattern_recall).max(0.0);
        let search_violation = (budget.min_search_recall - self.search_recall).max(0.0);
        let replay_violation = (budget.min_replay_fidelity - self.replay_fidelity).max(0.0);

        pattern_violation.mul_add(7.0, search_violation.mul_add(3.0, replay_violation * 4.0))
    }
}

/// Profile rejected during scoring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RejectedRateDistortionProfile {
    /// Candidate profile id.
    pub profile_id: String,
    /// Human-readable rejection reasons.
    pub reasons: Vec<String>,
    /// Estimated metrics that caused rejection.
    pub metrics: DistortionMetrics,
}

/// Selected profile plus audit evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateDistortionPlan {
    /// Selected profile.
    pub profile: RateDistortionProfile,
    /// Metrics for the selected profile.
    pub metrics: DistortionMetrics,
    /// Full-fidelity baseline metrics for before/after reporting.
    pub baseline_metrics: DistortionMetrics,
    /// Lower is better; includes distortion plus resource pressure.
    pub objective_score: f64,
    /// True when conservative fallback was used instead of learned/scored state.
    pub used_fallback: bool,
    /// Why this profile was selected.
    pub reasons: Vec<String>,
    /// Rejected candidate evidence.
    pub rejected_profiles: Vec<RejectedRateDistortionProfile>,
}

/// Before/after comparison for proof artifacts and telemetry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateDistortionComparisonReport {
    /// Baseline full-fidelity metrics.
    pub baseline: DistortionMetrics,
    /// Selected profile metrics.
    pub selected: DistortionMetrics,
    /// Estimated token savings versus full fidelity.
    pub token_savings: usize,
    /// Estimated memory savings versus full fidelity.
    pub memory_savings_bytes: u64,
    /// Estimated CPU savings versus full fidelity.
    pub cpu_savings_micros: u64,
    /// Whether correctness floors remain satisfied.
    pub correctness_preserved: bool,
}

impl RateDistortionComparisonReport {
    /// Build a comparison report from a selected plan.
    #[must_use]
    pub fn from_plan(plan: &RateDistortionPlan, budget: &RateDistortionBudget) -> Self {
        Self {
            baseline: plan.baseline_metrics.clone(),
            selected: plan.metrics.clone(),
            token_savings: plan
                .baseline_metrics
                .estimated_tokens
                .saturating_sub(plan.metrics.estimated_tokens),
            memory_savings_bytes: plan
                .baseline_metrics
                .estimated_memory_bytes
                .saturating_sub(plan.metrics.estimated_memory_bytes),
            cpu_savings_micros: plan
                .baseline_metrics
                .estimated_cpu_micros
                .saturating_sub(plan.metrics.estimated_cpu_micros),
            correctness_preserved: plan.metrics.pattern_recall >= budget.min_pattern_recall
                && plan.metrics.search_recall >= budget.min_search_recall
                && plan.metrics.replay_fidelity >= budget.min_replay_fidelity,
        }
    }
}

/// Controller configuration and candidate profile set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateDistortionControllerConfig {
    /// Candidate profiles. Empty means use built-ins.
    pub candidate_profiles: Vec<RateDistortionProfile>,
    /// Weight for pattern correctness loss.
    pub pattern_weight: f64,
    /// Weight for search correctness loss.
    pub search_weight: f64,
    /// Weight for replay correctness loss.
    pub replay_weight: f64,
    /// Weight for human context loss.
    pub context_weight: f64,
    /// Weight for token budget utilization.
    pub token_weight: f64,
    /// Weight for CPU budget utilization.
    pub cpu_weight: f64,
    /// Weight for memory budget utilization.
    pub memory_weight: f64,
    /// Weight for latency budget utilization.
    pub latency_weight: f64,
}

impl Default for RateDistortionControllerConfig {
    fn default() -> Self {
        Self {
            candidate_profiles: Vec::new(),
            pattern_weight: 8.0,
            search_weight: 3.0,
            replay_weight: 4.0,
            context_weight: 1.0,
            token_weight: 2.2,
            cpu_weight: 0.6,
            memory_weight: 1.0,
            latency_weight: 0.8,
        }
    }
}

/// Pure rate-distortion controller.
#[derive(Debug, Clone)]
pub struct RateDistortionController {
    config: RateDistortionControllerConfig,
}

impl RateDistortionController {
    /// Create a controller with explicit config.
    #[must_use]
    pub fn new(config: RateDistortionControllerConfig) -> Self {
        Self { config }
    }

    /// Create a controller with built-in candidates.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(RateDistortionControllerConfig::default())
    }

    /// Select a profile for the current input.
    #[must_use]
    pub fn select_profile(&self, input: &RateDistortionInput) -> RateDistortionPlan {
        let fallback = RateDistortionProfile::conservative_fallback();
        let baseline = estimate_metrics(&RateDistortionProfile::full_fidelity(), input);

        if input.panes.is_empty() {
            let metrics = estimate_metrics(&fallback, input);
            return RateDistortionPlan {
                profile: fallback,
                metrics,
                baseline_metrics: baseline,
                objective_score: f64::INFINITY,
                used_fallback: true,
                reasons: vec![
                    "controller_state_unavailable".to_string(),
                    "using fixed conservative capture profile".to_string(),
                ],
                rejected_profiles: Vec::new(),
            };
        }

        if input.operator_requested_full_fidelity {
            let profile = RateDistortionProfile::full_fidelity();
            let metrics = estimate_metrics(&profile, input);
            return RateDistortionPlan {
                profile,
                metrics,
                baseline_metrics: baseline,
                objective_score: 0.0,
                used_fallback: false,
                reasons: vec!["operator_requested_full_fidelity".to_string()],
                rejected_profiles: Vec::new(),
            };
        }

        let candidates = self.candidates();
        let mut rejected = Vec::new();
        let mut best_feasible: Option<(RateDistortionProfile, DistortionMetrics, f64)> = None;
        let mut best_any: Option<(RateDistortionProfile, DistortionMetrics, f64)> = None;

        for profile in candidates {
            let metrics = estimate_metrics(&profile, input);
            let score = self.objective_score(&metrics, input);
            let reasons = rejection_reasons(&metrics, &input.budget);
            if reasons.is_empty() {
                if best_feasible
                    .as_ref()
                    .is_none_or(|(_, _, best_score)| score < *best_score)
                {
                    best_feasible = Some((profile.clone(), metrics.clone(), score));
                }
            } else {
                rejected.push(RejectedRateDistortionProfile {
                    profile_id: profile.id.clone(),
                    reasons,
                    metrics: metrics.clone(),
                });
            }

            if best_any
                .as_ref()
                .is_none_or(|(_, _, best_score)| score < *best_score)
            {
                best_any = Some((profile, metrics, score));
            }
        }

        let (profile, metrics, score, used_fallback, mut reasons) =
            if let Some((profile, metrics, score)) = best_feasible {
                (
                    profile,
                    metrics,
                    score,
                    false,
                    vec!["selected lowest feasible rate-distortion objective".to_string()],
                )
            } else {
                let (profile, metrics, score) = best_any.unwrap_or_else(|| {
                    let metrics = estimate_metrics(&fallback, input);
                    (fallback.clone(), metrics, f64::INFINITY)
                });
                (
                    profile,
                    metrics,
                    score,
                    true,
                    vec![
                        "no candidate satisfied every hard budget".to_string(),
                        "selected least-violating conservative profile".to_string(),
                    ],
                )
            };

        if metrics.critical_detections > 0 && profile.preserve_critical_detections {
            reasons.push("critical pattern hits protected from truncation".to_string());
        }
        if profile.robot_format == RobotFormatPreference::Toon {
            reasons.push("toon selected to reduce agent-to-agent token IO".to_string());
        }

        RateDistortionPlan {
            profile,
            metrics,
            baseline_metrics: baseline,
            objective_score: score,
            used_fallback,
            reasons,
            rejected_profiles: rejected,
        }
    }

    fn candidates(&self) -> Vec<RateDistortionProfile> {
        if self.config.candidate_profiles.is_empty() {
            built_in_profiles()
        } else {
            self.config.candidate_profiles.clone()
        }
    }

    fn objective_score(&self, metrics: &DistortionMetrics, input: &RateDistortionInput) -> f64 {
        let budget = &input.budget;
        let pressure = input.pressure.sanitized();
        let token_ratio = ratio_usize(metrics.estimated_tokens, budget.max_tokens_per_response);
        let cpu_ratio = ratio_u64(
            metrics.estimated_cpu_micros,
            budget
                .max_cpu_micros_per_pane
                .saturating_mul(input.panes.len().max(1) as u64),
        );
        let memory_ratio = ratio_u64(metrics.estimated_memory_bytes, budget.max_memory_bytes);
        let latency_ratio = ratio_u64(metrics.estimated_latency_ms, budget.max_latency_ms);

        let pattern_loss = (1.0 - metrics.pattern_recall).max(0.0);
        let search_loss = (1.0 - metrics.search_recall).max(0.0);
        let replay_loss = (1.0 - metrics.replay_fidelity).max(0.0);
        let context_loss = (1.0 - metrics.context_recall).max(0.0);

        let pattern_weight = self.config.pattern_weight * (1.0 + pressure.incident_risk);
        let search_weight = self.config.search_weight * (1.0 + pressure.search_pressure);
        let replay_weight = self.config.replay_weight * (1.0 + pressure.replay_pressure);
        let correctness = pattern_loss.mul_add(
            pattern_weight,
            search_loss.mul_add(
                search_weight,
                replay_loss.mul_add(replay_weight, self.config.context_weight * context_loss),
            ),
        );

        let token_weight = self.config.token_weight * (1.0 + pressure.context_pressure);
        let resources = token_ratio.mul_add(
            token_weight,
            cpu_ratio.mul_add(
                self.config.cpu_weight,
                memory_ratio.mul_add(
                    self.config.memory_weight,
                    self.config.latency_weight * latency_ratio,
                ),
            ),
        );

        metrics
            .correctness_floor_violation(budget)
            .mul_add(100.0, correctness + resources)
    }
}

/// Reduction output for one pane transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptReduction {
    /// Reduced text to emit.
    pub text: String,
    /// Original line count.
    pub original_lines: usize,
    /// Retained line count.
    pub retained_lines: usize,
    /// Omitted line count.
    pub omitted_lines: usize,
    /// Critical marker lines in the original.
    pub critical_lines_total: usize,
    /// Critical marker lines retained in output.
    pub critical_lines_retained: usize,
    /// Estimated output tokens after reduction.
    pub estimated_tokens: usize,
}

impl TranscriptReduction {
    /// Critical marker recall in this reduction.
    #[must_use]
    pub fn critical_recall(&self) -> f64 {
        if self.critical_lines_total == 0 {
            1.0
        } else {
            self.critical_lines_retained as f64 / self.critical_lines_total as f64
        }
    }
}

/// Reduce a pane transcript with tail truncation while preserving critical hits.
#[must_use]
pub fn reduce_transcript_for_profile(
    text: &str,
    profile: &RateDistortionProfile,
    critical_markers: &[&str],
) -> TranscriptReduction {
    let lines: Vec<&str> = text.lines().collect();
    if profile.capture_detail == CaptureDetail::Full {
        return TranscriptReduction {
            text: text.to_string(),
            original_lines: lines.len(),
            retained_lines: lines.len(),
            omitted_lines: 0,
            critical_lines_total: count_critical_lines(&lines, critical_markers),
            critical_lines_retained: count_critical_lines(&lines, critical_markers),
            estimated_tokens: estimate_tokens(text.len()),
        };
    }

    let mut retained = BTreeSet::new();
    let tail_start = lines.len().saturating_sub(profile.tail_lines);
    for idx in tail_start..lines.len() {
        retained.insert(idx);
    }

    let critical_indices = critical_line_indices(&lines, critical_markers);
    if profile.preserve_critical_detections {
        for idx in &critical_indices {
            let start = idx.saturating_sub(profile.critical_context_lines);
            let end = (*idx)
                .saturating_add(profile.critical_context_lines)
                .min(lines.len().saturating_sub(1));
            for retained_idx in start..=end {
                retained.insert(retained_idx);
            }
        }
    }

    let reduced_lines: Vec<&str> = retained.iter().map(|idx| lines[*idx]).collect();
    let reduced_text = reduced_lines.join("\n");
    let retained_critical = reduced_lines
        .iter()
        .filter(|line| line_contains_any_marker(line, critical_markers))
        .count();

    TranscriptReduction {
        text: reduced_text.clone(),
        original_lines: lines.len(),
        retained_lines: reduced_lines.len(),
        omitted_lines: lines.len().saturating_sub(reduced_lines.len()),
        critical_lines_total: critical_indices.len(),
        critical_lines_retained: retained_critical,
        estimated_tokens: estimate_tokens(reduced_text.len()),
    }
}

fn built_in_profiles() -> Vec<RateDistortionProfile> {
    vec![
        RateDistortionProfile::full_fidelity(),
        RateDistortionProfile {
            id: "incident_balanced".to_string(),
            capture_detail: CaptureDetail::Balanced,
            semantic_compression: SemanticCompressionMode::Conservative,
            byte_compression_level: CompressionLevel::Default,
            robot_format: RobotFormatPreference::Toon,
            tail_lines: 320,
            search_excerpt_count: 12,
            search_excerpt_chars: 420,
            replay_context_lines: 16,
            critical_context_lines: 4,
            include_provenance_hashes: true,
            preserve_critical_detections: true,
        },
        RateDistortionProfile {
            id: "swarm_constrained".to_string(),
            capture_detail: CaptureDetail::Tail,
            semantic_compression: SemanticCompressionMode::Aggressive,
            byte_compression_level: CompressionLevel::Fast,
            robot_format: RobotFormatPreference::Toon,
            tail_lines: 100,
            search_excerpt_count: 5,
            search_excerpt_chars: 240,
            replay_context_lines: 8,
            critical_context_lines: 2,
            include_provenance_hashes: true,
            preserve_critical_detections: true,
        },
        RateDistortionProfile {
            id: "emergency_minimal".to_string(),
            capture_detail: CaptureDetail::Minimal,
            semantic_compression: SemanticCompressionMode::Aggressive,
            byte_compression_level: CompressionLevel::Fast,
            robot_format: RobotFormatPreference::Toon,
            tail_lines: 32,
            search_excerpt_count: 2,
            search_excerpt_chars: 160,
            replay_context_lines: 2,
            critical_context_lines: 1,
            include_provenance_hashes: true,
            preserve_critical_detections: true,
        },
        RateDistortionProfile::conservative_fallback(),
    ]
}

fn estimate_metrics(
    profile: &RateDistortionProfile,
    input: &RateDistortionInput,
) -> DistortionMetrics {
    let pressure = input.pressure.sanitized();
    let raw_bytes: usize = input.panes.iter().map(|pane| pane.raw_bytes).sum();
    let raw_lines: usize = input.panes.iter().map(|pane| pane.line_count).sum();
    let critical_detections: usize = input
        .panes
        .iter()
        .map(|pane| pane.critical_detection_count)
        .sum();
    let pane_count = input.panes.len().max(1);
    let repeated_ratio = weighted_repeated_ratio(&input.panes);
    let baseline_tokens = estimate_tokens(raw_bytes);
    let retention = retained_fraction(profile, input);
    let semantic = profile
        .semantic_compression
        .retained_fraction(repeated_ratio);
    let token_multiplier = profile.robot_format.token_multiplier() * semantic;
    let estimated_tokens = ((baseline_tokens as f64 * retention * token_multiplier).ceil()
        as usize)
        .saturating_add(profile.search_excerpt_count.min(64).saturating_mul(8))
        .max(usize::from(raw_bytes > 0));

    let retained_bytes = ((raw_bytes as f64 * retention * semantic).ceil() as u64)
        .saturating_add((critical_detections as u64).saturating_mul(96));
    let compression_cpu = compression_cpu_multiplier(profile.byte_compression_level)
        * profile.semantic_compression.cpu_multiplier()
        * profile.robot_format.cpu_multiplier();
    let estimated_cpu_micros = ((retained_bytes as f64 / 1024.0)
        .mul_add(18.0 * compression_cpu, pane_count as f64 * 180.0))
    .ceil() as u64;
    let estimated_latency_ms = (estimated_cpu_micros / 1_000)
        .saturating_add((pane_count as u64).saturating_mul(2))
        .max(1);

    DistortionMetrics {
        estimated_tokens,
        estimated_memory_bytes: retained_bytes.saturating_add((pane_count as u64) * 512),
        estimated_cpu_micros,
        estimated_latency_ms,
        pattern_recall: pattern_recall(profile, critical_detections, retention),
        search_recall: search_recall(profile, retention, pressure.search_pressure),
        replay_fidelity: replay_fidelity(profile, pressure.replay_pressure),
        context_recall: context_recall(profile, retention),
        raw_bytes,
        raw_lines,
        critical_detections,
    }
}

fn retained_fraction(profile: &RateDistortionProfile, input: &RateDistortionInput) -> f64 {
    if profile.capture_detail == CaptureDetail::Full {
        return 1.0;
    }

    let raw_lines: usize = input.panes.iter().map(|pane| pane.line_count).sum();
    if raw_lines == 0 {
        return profile.capture_detail.retention_floor();
    }

    let tail_lines = profile.tail_lines.saturating_mul(input.panes.len());
    let critical_lines: usize = input
        .panes
        .iter()
        .map(|pane| {
            pane.critical_detection_count.saturating_mul(
                profile
                    .critical_context_lines
                    .saturating_mul(2)
                    .saturating_add(1),
            )
        })
        .sum();
    let retained_lines = tail_lines.saturating_add(critical_lines).min(raw_lines);
    let observed = retained_lines as f64 / raw_lines as f64;
    let pressure_bonus = input.pressure.sanitized().incident_risk * 0.08;
    observed
        .max(profile.capture_detail.retention_floor())
        .saturating_add_f64(pressure_bonus)
        .min(1.0)
}

fn pattern_recall(
    profile: &RateDistortionProfile,
    critical_detections: usize,
    retained_fraction: f64,
) -> f64 {
    if critical_detections > 0 && profile.preserve_critical_detections {
        1.0
    } else {
        retained_fraction
            .max(profile.capture_detail.retention_floor())
            .min(1.0)
    }
}

fn search_recall(
    profile: &RateDistortionProfile,
    retained_fraction: f64,
    search_pressure: f64,
) -> f64 {
    let excerpt_factor = match profile.search_excerpt_count {
        0 => 0.0,
        1..=2 => 0.55,
        3..=6 => 0.78,
        7..=12 => 0.92,
        _ => 1.0,
    };
    let char_factor = (profile.search_excerpt_chars.min(512) as f64 / 512.0).max(0.25);
    let body = retained_fraction.mul_add(0.45, excerpt_factor * char_factor * 0.55);
    search_pressure.clamp(0.0, 1.0).mul_add(0.04, body).min(1.0)
}

fn replay_fidelity(profile: &RateDistortionProfile, replay_pressure: f64) -> f64 {
    let base = profile.capture_detail.replay_fidelity_floor();
    let context_bonus = match profile.replay_context_lines {
        0 => 0.0,
        1..=2 => 0.02,
        3..=8 => 0.06,
        9..=16 => 0.09,
        _ => 0.12,
    };
    let hash_bonus = if profile.include_provenance_hashes {
        0.025
    } else {
        0.0
    };
    replay_pressure
        .clamp(0.0, 1.0)
        .mul_add(0.02, base + context_bonus + hash_bonus)
        .min(1.0)
}

fn context_recall(profile: &RateDistortionProfile, retained_fraction: f64) -> f64 {
    match profile.capture_detail {
        CaptureDetail::Full => 1.0,
        CaptureDetail::Balanced => retained_fraction.max(0.72),
        CaptureDetail::Tail => retained_fraction.max(0.45),
        CaptureDetail::Minimal => retained_fraction.max(0.22),
    }
}

fn weighted_repeated_ratio(panes: &[PaneRateDistortionObservation]) -> f64 {
    let total_bytes: usize = panes.iter().map(|pane| pane.raw_bytes).sum();
    if total_bytes == 0 {
        return 0.0;
    }
    panes
        .iter()
        .map(|pane| pane.repeated_line_ratio.clamp(0.0, 1.0) * pane.raw_bytes as f64)
        .sum::<f64>()
        / total_bytes as f64
}

fn compression_cpu_multiplier(level: CompressionLevel) -> f64 {
    match level {
        CompressionLevel::Fast => 1.02,
        CompressionLevel::Default => 1.12,
        CompressionLevel::High => 1.55,
        CompressionLevel::Maximum => 2.40,
    }
}

fn rejection_reasons(metrics: &DistortionMetrics, budget: &RateDistortionBudget) -> Vec<String> {
    let mut reasons = Vec::new();
    if metrics.estimated_tokens > budget.max_tokens_per_response {
        reasons.push(format!(
            "estimated_tokens {} exceeds budget {}",
            metrics.estimated_tokens, budget.max_tokens_per_response
        ));
    }
    if metrics.estimated_memory_bytes > budget.max_memory_bytes {
        reasons.push(format!(
            "estimated_memory_bytes {} exceeds budget {}",
            metrics.estimated_memory_bytes, budget.max_memory_bytes
        ));
    }
    if metrics.estimated_latency_ms > budget.max_latency_ms {
        reasons.push(format!(
            "estimated_latency_ms {} exceeds budget {}",
            metrics.estimated_latency_ms, budget.max_latency_ms
        ));
    }
    if metrics.pattern_recall < budget.min_pattern_recall {
        reasons.push(format!(
            "pattern_recall {:.3} below floor {:.3}",
            metrics.pattern_recall, budget.min_pattern_recall
        ));
    }
    if metrics.search_recall < budget.min_search_recall {
        reasons.push(format!(
            "search_recall {:.3} below floor {:.3}",
            metrics.search_recall, budget.min_search_recall
        ));
    }
    if metrics.replay_fidelity < budget.min_replay_fidelity {
        reasons.push(format!(
            "replay_fidelity {:.3} below floor {:.3}",
            metrics.replay_fidelity, budget.min_replay_fidelity
        ));
    }
    reasons
}

fn critical_line_indices(lines: &[&str], critical_markers: &[&str]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| line_contains_any_marker(line, critical_markers).then_some(idx))
        .collect()
}

fn count_critical_lines(lines: &[&str], critical_markers: &[&str]) -> usize {
    lines
        .iter()
        .filter(|line| line_contains_any_marker(line, critical_markers))
        .count()
}

fn line_contains_any_marker(line: &str, critical_markers: &[&str]) -> bool {
    critical_markers
        .iter()
        .any(|marker| !marker.is_empty() && line.contains(marker))
}

fn estimate_tokens(bytes: usize) -> usize {
    bytes.div_ceil(TOKEN_BYTES).max(usize::from(bytes > 0))
}

fn ratio_usize(value: usize, budget: usize) -> f64 {
    if budget == 0 {
        return f64::INFINITY;
    }
    value as f64 / budget as f64
}

fn ratio_u64(value: u64, budget: u64) -> f64 {
    if budget == 0 {
        return f64::INFINITY;
    }
    value as f64 / budget as f64
}

trait SaturatingFloatAdd {
    fn saturating_add_f64(self, rhs: f64) -> f64;
}

impl SaturatingFloatAdd for f64 {
    fn saturating_add_f64(self, rhs: f64) -> f64 {
        if self.is_finite() && rhs.is_finite() {
            self + rhs
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fifty_pane_input() -> RateDistortionInput {
        let panes = (0..50)
            .map(|pane_id| {
                PaneRateDistortionObservation::new(
                    pane_id,
                    180_000 + pane_id as usize * 311,
                    2_400,
                    usize::from(pane_id % 5 == 0),
                    0.72,
                )
            })
            .collect();
        RateDistortionInput {
            panes,
            budget: RateDistortionBudget {
                max_tokens_per_response: 800_000,
                max_cpu_micros_per_pane: 4_500,
                max_memory_bytes: 8 * 1024 * 1024,
                max_latency_ms: 1_200,
                min_pattern_recall: 1.0,
                min_search_recall: 0.70,
                min_replay_fidelity: 0.88,
            },
            pressure: RateDistortionPressure {
                incident_risk: 0.7,
                search_pressure: 0.5,
                replay_pressure: 0.4,
                context_pressure: 0.9,
            },
            operator_requested_full_fidelity: false,
        }
    }

    #[test]
    fn controller_selects_budgeted_toon_profile_for_fifty_pane_swarm() {
        let input = fifty_pane_input();
        let controller = RateDistortionController::with_defaults();
        let plan = controller.select_profile(&input);

        assert_ne!(plan.profile.capture_detail, CaptureDetail::Full);
        assert_eq!(plan.profile.robot_format, RobotFormatPreference::Toon);
        assert!(
            plan.metrics.estimated_tokens <= input.budget.max_tokens_per_response,
            "tokens={} budget={}",
            plan.metrics.estimated_tokens,
            input.budget.max_tokens_per_response
        );
        assert!((plan.metrics.pattern_recall - 1.0).abs() <= f64::EPSILON);
        assert!(
            plan.metrics.search_recall >= input.budget.min_search_recall,
            "search recall fell below floor: {:?}",
            plan.metrics
        );
        assert!(
            plan.metrics.replay_fidelity >= input.budget.min_replay_fidelity,
            "replay fidelity fell below floor: {:?}",
            plan.metrics
        );
    }

    #[test]
    fn transcript_reduction_keeps_critical_detection_outside_tail() {
        let mut lines = vec!["boot", "compile", "RATE_LIMIT: retry after 60s"];
        lines.extend(std::iter::repeat_n("noise progress 10/100", 40));
        lines.push("final prompt >");
        let text = lines.join("\n");
        let profile = RateDistortionProfile {
            tail_lines: 4,
            critical_context_lines: 1,
            ..RateDistortionProfile::conservative_fallback()
        };

        let reduced = reduce_transcript_for_profile(&text, &profile, &["RATE_LIMIT"]);

        assert!(reduced.text.contains("RATE_LIMIT: retry after 60s"));
        assert!(reduced.text.contains("final prompt >"));
        assert!(reduced.omitted_lines > 0);
        assert!((reduced.critical_recall() - 1.0).abs() <= f64::EPSILON);
    }

    #[test]
    fn fallback_profile_is_used_when_observations_are_missing() {
        let controller = RateDistortionController::with_defaults();
        let plan = controller.select_profile(&RateDistortionInput::default());

        assert!(plan.used_fallback);
        assert_eq!(plan.profile.id, "conservative_fallback");
        assert!(
            plan.reasons
                .iter()
                .any(|reason| reason == "controller_state_unavailable")
        );
    }

    #[test]
    fn comparison_report_covers_tokens_cpu_memory_and_correctness() {
        let input = fifty_pane_input();
        let controller = RateDistortionController::with_defaults();
        let plan = controller.select_profile(&input);
        let report = RateDistortionComparisonReport::from_plan(&plan, &input.budget);

        assert!(report.token_savings > 0, "report={report:?}");
        assert!(report.memory_savings_bytes > 0, "report={report:?}");
        assert!(report.cpu_savings_micros > 0, "report={report:?}");
        assert!(report.correctness_preserved, "report={report:?}");
        assert_eq!(report.selected.critical_detections, 10);
        assert!((report.selected.pattern_recall - 1.0).abs() <= f64::EPSILON);
    }

    #[test]
    fn replay_floor_rejects_profiles_that_drop_reconstruction_context() {
        let mut input = fifty_pane_input();
        input.budget.min_replay_fidelity = 0.985;
        input.budget.max_tokens_per_response = usize::MAX / 4;
        input.budget.max_memory_bytes = u64::MAX / 4;
        input.budget.max_latency_ms = u64::MAX / 4;

        let controller = RateDistortionController::with_defaults();
        let plan = controller.select_profile(&input);

        assert_eq!(plan.profile.capture_detail, CaptureDetail::Full);
        assert!(plan.rejected_profiles.iter().any(|rejected| {
            rejected
                .reasons
                .iter()
                .any(|reason| reason.contains("replay_fidelity"))
        }));
    }

    #[test]
    fn transcript_reduction_full_fidelity_is_lossless() {
        let text = "a\nb\nERROR critical\nc";
        let profile = RateDistortionProfile::full_fidelity();
        let reduced = reduce_transcript_for_profile(text, &profile, &["ERROR"]);

        assert_eq!(reduced.text, text);
        assert_eq!(reduced.original_lines, 4);
        assert_eq!(reduced.omitted_lines, 0);
        assert!((reduced.critical_recall() - 1.0).abs() <= f64::EPSILON);
    }

    #[test]
    fn line_observation_sanitizes_repeated_ratio() {
        let pane = PaneRateDistortionObservation::new(7, 12, 1, 0, 9.0);
        assert!((pane.repeated_line_ratio - 1.0).abs() <= f64::EPSILON);

        let pane = PaneRateDistortionObservation::new(8, 12, 1, 0, -1.0);
        assert!(pane.repeated_line_ratio.abs() <= f64::EPSILON);
    }
}
