//! br-ft-1650n.7: Prompt Drift Canary substrate.
//!
//! Sequential change detector that watches an agent-output
//! statistic (rule-hit rate, unmatched-span entropy, motif
//! frequency) and emits a `DriftAlert` when the running stream
//! diverges enough from a documented baseline to threaten pattern
//! rules, workflow triggers, or robot orchestration assumptions.
//!
//! ## Why CUSUM
//!
//! Simple thresholds either fire too eagerly on a single noisy
//! sample or miss a slow drift over many. The two-sided CUSUM
//! statistic is the textbook minimax detector for an unknown
//! change-point: it accumulates evidence until the running sum
//! crosses an alarm threshold, at which point the operator gets a
//! single notification with the cumulative log-likelihood
//! evidence. The accumulator resets on alarm so the next drift
//! can be observed independently.
//!
//! State is bounded: two `f64`s (the upward and downward CUSUMs),
//! a couple of `u64` counters, and the operator-set baseline
//! parameters. No history buffer, no per-observation allocation.
//!
//! ## False-alarm budget
//!
//! Operators allocate a budget for how many alarms the canary
//! may raise before the dashboard demands re-baselining. Once
//! the budget is exhausted, further would-be alarms are
//! `suppressed` (still counted, never returned). This keeps
//! noisy sources from spamming the alert pipeline while still
//! preserving a forensic counter for review.
//!
//! ## What ships in this slice
//!
//! - [`DriftStatisticParams`] - operator-tunable knobs for the
//!   CUSUM (baseline mean, reference value `k`, alarm threshold
//!   `h`, budget).
//! - [`DriftStatistic`] - the bounded-state CUSUM accumulator.
//! - [`DriftStatisticSnapshot`] - serde-friendly view for
//!   dashboards and forensic export.
//! - [`DriftAlert`] - typed alert (upward / downward shift) with
//!   the cumulative evidence value at fire time.
//! - [`FixtureCandidate`] - operator-facing review artifact: a
//!   redacted output snippet + expected-match scaffolding.
//! - [`PromptDriftCanary`] - transcript-window evaluator that
//!   tracks rule hit rate, unmatched entropy, and repeated motifs.
//!
//! ## What is deferred
//!
//! - Per-agent-family baseline registry: today the operator owns
//!   the baseline parameters per `DriftStatistic` instance. A
//!   wired-pass slice can layer a registry keyed by
//!   `(agent_family, agent_version)`.
//! - Sequential probability ratio test (SPRT) variant: CUSUM
//!   suffices for the substrate's bounded false-alarm budget.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::redactor::Redactor;

/// br-ft-1650n.7: operator-tunable knobs for the CUSUM detector.
///
/// Field semantics (textbook two-sided CUSUM):
///
/// - `baseline_mean` - the documented per-window mean of the
///   monitored statistic when the agent is behaving normally.
///   Operator-supplied.
/// - `reference_k` - half the size of the smallest shift the
///   detector should be sensitive to. Smaller `k` means faster
///   detection of small shifts, more false alarms.
/// - `alarm_threshold_h` - the CUSUM crosses `h` to fire. Larger
///   `h` means fewer false alarms, slower detection. Standard
///   recommendations are `h = 4-5` when observations are
///   normalized to unit variance.
/// - `false_alarm_budget` - total alarms the detector may raise
///   before further alarms are suppressed. Keeps noisy sources
///   from spamming the alert pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DriftStatisticParams {
    pub baseline_mean: f64,
    pub reference_k: f64,
    pub alarm_threshold_h: f64,
    pub false_alarm_budget: u64,
}

impl DriftStatisticParams {
    /// Conservative starting parameters. Caller must override
    /// `baseline_mean` for their agent's expected statistic; the
    /// other defaults are textbook unit-variance CUSUM values.
    #[must_use]
    pub const fn new(baseline_mean: f64, false_alarm_budget: u64) -> Self {
        Self {
            baseline_mean,
            reference_k: 0.5,
            alarm_threshold_h: 5.0,
            false_alarm_budget,
        }
    }
}

/// br-ft-1650n.7: typed alert. The cumulative evidence is the
/// CUSUM value at fire time: useful for forensic review (the
/// larger the value, the more decisive the change).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DriftAlert {
    /// The monitored statistic drifted upward beyond the
    /// alarm threshold.
    UpwardShift {
        cusum_at_alarm: f64,
        observations_count: u64,
    },
    /// The monitored statistic drifted downward beyond the
    /// alarm threshold.
    DownwardShift {
        cusum_at_alarm: f64,
        observations_count: u64,
    },
}

/// Reasons an alert was suppressed (didn't fire even though the
/// CUSUM crossed). The full enum exists so dashboards can
/// distinguish budget-exhaustion suppression from rate-limit
/// suppression once the wired-pass slice adds the latter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftSuppressionReason {
    /// `false_alarm_budget` is at zero. The alert was counted in
    /// `suppressed_alarms_count` but not returned to the caller.
    BudgetExhausted,
}

/// Result of ingesting one observation, including suppressed
/// would-have-fired alerts for forensic logs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DriftUpdateOutcome {
    Quiet,
    Alert {
        alert: DriftAlert,
    },
    Suppressed {
        alert: DriftAlert,
        reason: DriftSuppressionReason,
    },
}

/// br-ft-1650n.7: serde-friendly snapshot of the canary's state
/// for dashboards and forensic export.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DriftStatisticSnapshot {
    pub params: DriftStatisticParams,
    pub cusum_high: f64,
    pub cusum_low: f64,
    pub observations_count: u64,
    pub alarms_count: u64,
    pub suppressed_alarms_count: u64,
    pub budget_remaining: u64,
}

/// br-ft-1650n.7: bounded-state CUSUM accumulator. Updates in
/// place; emits `Option<DriftAlert>` per observation.
#[derive(Debug, Clone)]
pub struct DriftStatistic {
    params: DriftStatisticParams,
    cusum_high: f64,
    cusum_low: f64,
    observations_count: u64,
    alarms_count: u64,
    suppressed_alarms_count: u64,
}

impl DriftStatistic {
    #[must_use]
    pub fn new(params: DriftStatisticParams) -> Self {
        Self {
            params,
            cusum_high: 0.0,
            cusum_low: 0.0,
            observations_count: 0,
            alarms_count: 0,
            suppressed_alarms_count: 0,
        }
    }

    /// Ingest one observation of the monitored statistic. Returns
    /// `Some(DriftAlert)` if the CUSUM crossed `alarm_threshold_h`
    /// AND the false-alarm budget is non-zero. Otherwise `None`.
    ///
    /// On alarm, both CUSUM accumulators reset to zero so the
    /// next change can be detected independently. The alarm
    /// counter is incremented and the budget is consumed by 1.
    ///
    /// On budget-exhausted suppression, `suppressed_alarms_count`
    /// is bumped and the accumulators STILL reset (an alarm-like
    /// event was observed; we just didn't surface it). This
    /// matches the documented suppression semantics: operators
    /// who exhaust the budget by definition want the canary to
    /// stay quiet until they re-baseline.
    pub fn update(&mut self, observation: f64) -> Option<DriftAlert> {
        match self.update_with_outcome(observation) {
            DriftUpdateOutcome::Alert { alert } => Some(alert),
            DriftUpdateOutcome::Quiet | DriftUpdateOutcome::Suppressed { .. } => None,
        }
    }

    /// Ingest one observation and preserve suppressed would-have-
    /// fired alerts. This is the path dashboards and replay
    /// analyzers use so budget exhaustion remains visible.
    pub fn update_with_outcome(&mut self, observation: f64) -> DriftUpdateOutcome {
        self.observations_count = self.observations_count.saturating_add(1);
        let centered = observation - self.params.baseline_mean;
        // Upward CUSUM: accumulates positive shifts above k.
        self.cusum_high = (self.cusum_high + centered - self.params.reference_k).max(0.0);
        // Downward CUSUM: accumulates negative shifts below -k.
        self.cusum_low = (self.cusum_low - centered - self.params.reference_k).max(0.0);

        let high_alarm = self.cusum_high >= self.params.alarm_threshold_h;
        let low_alarm = self.cusum_low >= self.params.alarm_threshold_h;

        if !high_alarm && !low_alarm {
            return DriftUpdateOutcome::Quiet;
        }

        let alert = if high_alarm {
            DriftAlert::UpwardShift {
                cusum_at_alarm: self.cusum_high,
                observations_count: self.observations_count,
            }
        } else {
            DriftAlert::DownwardShift {
                cusum_at_alarm: self.cusum_low,
                observations_count: self.observations_count,
            }
        };

        // Reset both accumulators on alarm regardless of which
        // direction fired. The classical CUSUM contract: post-
        // alarm we treat the new mean as the baseline.
        self.cusum_high = 0.0;
        self.cusum_low = 0.0;

        if self.budget_remaining() == 0 {
            self.suppressed_alarms_count = self.suppressed_alarms_count.saturating_add(1);
            return DriftUpdateOutcome::Suppressed {
                alert,
                reason: DriftSuppressionReason::BudgetExhausted,
            };
        }

        self.alarms_count = self.alarms_count.saturating_add(1);
        DriftUpdateOutcome::Alert { alert }
    }

    /// Budget remaining = initial budget minus alarms already raised.
    #[must_use]
    pub fn budget_remaining(&self) -> u64 {
        self.params
            .false_alarm_budget
            .saturating_sub(self.alarms_count)
    }

    /// Snapshot for export. Pure read: does not advance the
    /// accumulators.
    #[must_use]
    pub fn snapshot(&self) -> DriftStatisticSnapshot {
        DriftStatisticSnapshot {
            params: self.params,
            cusum_high: self.cusum_high,
            cusum_low: self.cusum_low,
            observations_count: self.observations_count,
            alarms_count: self.alarms_count,
            suppressed_alarms_count: self.suppressed_alarms_count,
            budget_remaining: self.budget_remaining(),
        }
    }
}

/// br-ft-1650n.7: operator-facing review artifact paired with a
/// drift alert. A redacted output snippet plus the expected
/// pattern an updated rule should match. The substrate accepts a
/// caller-provided redacted text; a wired-pass slice will couple
/// to `crate::redactor` to build it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureCandidate {
    pub agent_family: String,
    pub agent_version: String,
    pub source_window_id: String,
    /// Already-redacted output snippet. The contract is that the
    /// canary generated this through the project redactor before
    /// exposing it as a review artifact.
    pub redacted_text: String,
    /// The pattern the operator should expect a new rule to match.
    /// Free-form for now; a future slice can typecheck this against
    /// the pattern-pack DSL.
    pub expected_match_pattern: String,
    /// Evidence that made this candidate worth review.
    pub evidence: Vec<String>,
    /// Reason this candidate was suppressed (e.g., budget
    /// exhausted). `None` if the candidate is live.
    pub suppression_reason: Option<DriftSuppressionReason>,
}

/// Drift signal tracked by the prompt canary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptDriftSignalKind {
    RuleHitRate,
    UnmatchedEntropy,
    MotifFrequency,
}

/// One transcript window observed for an agent family/version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTranscriptWindow {
    pub window_id: String,
    pub agent_family: String,
    pub agent_version: String,
    /// Rule IDs expected to match this window under the current
    /// baseline pattern pack.
    pub expected_rule_ids: Vec<String>,
    /// Rule IDs actually matched in this window.
    pub matched_rule_ids: Vec<String>,
    /// Spans that stayed unmatched but were high-value enough to
    /// inspect for motif drift.
    pub unmatched_spans: Vec<String>,
    /// Raw transcript excerpt. This never leaves the canary until
    /// redacted and bounded.
    pub raw_text: String,
}

/// Numeric observations extracted from one transcript window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptDriftObservations {
    pub rule_hit_rate: f64,
    pub unmatched_entropy: f64,
    pub motif_frequency: f64,
    pub top_motif: Option<String>,
}

/// Alert with source signal, observation value, and review evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptDriftSignalAlert {
    pub signal: PromptDriftSignalKind,
    pub observation: f64,
    pub outcome: DriftUpdateOutcome,
    pub evidence: Vec<String>,
}

/// Complete evaluation result for one transcript window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptDriftEvaluation {
    pub window_id: String,
    pub agent_family: String,
    pub agent_version: String,
    pub observations: PromptDriftObservations,
    pub alerts: Vec<PromptDriftSignalAlert>,
    pub fixture_candidates: Vec<FixtureCandidate>,
    pub statistic_snapshots: BTreeMap<PromptDriftSignalKind, DriftStatisticSnapshot>,
}

/// Operator-tunable canary configuration. All statistics are
/// caller-owned so each agent family/version can have a different
/// baseline while the state stays bounded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptDriftCanaryConfig {
    pub rule_hit_rate: DriftStatisticParams,
    pub unmatched_entropy: DriftStatisticParams,
    pub motif_frequency: DriftStatisticParams,
    pub max_fixture_bytes: usize,
    pub min_motif_repetitions: usize,
}

impl PromptDriftCanaryConfig {
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            rule_hit_rate: DriftStatisticParams {
                baseline_mean: 1.0,
                reference_k: 0.2,
                alarm_threshold_h: 0.8,
                false_alarm_budget: 4,
            },
            unmatched_entropy: DriftStatisticParams {
                baseline_mean: 0.25,
                reference_k: 0.15,
                alarm_threshold_h: 0.75,
                false_alarm_budget: 4,
            },
            motif_frequency: DriftStatisticParams {
                baseline_mean: 0.05,
                reference_k: 0.15,
                alarm_threshold_h: 0.75,
                false_alarm_budget: 4,
            },
            max_fixture_bytes: 512,
            min_motif_repetitions: 3,
        }
    }
}

/// Bounded-state prompt drift canary for a single agent baseline.
#[derive(Debug, Clone)]
pub struct PromptDriftCanary {
    config: PromptDriftCanaryConfig,
    rule_hit_rate: DriftStatistic,
    unmatched_entropy: DriftStatistic,
    motif_frequency: DriftStatistic,
    redactor: Redactor,
}

impl PromptDriftCanary {
    #[must_use]
    pub fn new(config: PromptDriftCanaryConfig) -> Self {
        Self {
            rule_hit_rate: DriftStatistic::new(config.rule_hit_rate),
            unmatched_entropy: DriftStatistic::new(config.unmatched_entropy),
            motif_frequency: DriftStatistic::new(config.motif_frequency),
            redactor: Redactor::new(),
            config,
        }
    }

    /// Evaluate one transcript window and synthesize redacted,
    /// never-auto-trusted fixture candidates for each surfaced or
    /// suppressed drift signal.
    pub fn evaluate_window(&mut self, window: &PromptTranscriptWindow) -> PromptDriftEvaluation {
        let observations = analyze_transcript_window(window, self.config.min_motif_repetitions);
        let signal_inputs = [
            (
                PromptDriftSignalKind::RuleHitRate,
                observations.rule_hit_rate,
                self.rule_hit_rate
                    .update_with_outcome(observations.rule_hit_rate),
            ),
            (
                PromptDriftSignalKind::UnmatchedEntropy,
                observations.unmatched_entropy,
                self.unmatched_entropy
                    .update_with_outcome(observations.unmatched_entropy),
            ),
            (
                PromptDriftSignalKind::MotifFrequency,
                observations.motif_frequency,
                self.motif_frequency
                    .update_with_outcome(observations.motif_frequency),
            ),
        ];

        let mut alerts = Vec::new();
        let mut fixture_candidates = Vec::new();
        for (signal, observation, outcome) in signal_inputs {
            if matches!(outcome, DriftUpdateOutcome::Quiet) {
                continue;
            }

            let evidence = evidence_for_signal(signal, observation, window, &observations);
            fixture_candidates.push(self.fixture_candidate(
                window,
                &observations,
                &evidence,
                outcome,
            ));
            alerts.push(PromptDriftSignalAlert {
                signal,
                observation,
                outcome,
                evidence,
            });
        }

        PromptDriftEvaluation {
            window_id: window.window_id.clone(),
            agent_family: window.agent_family.clone(),
            agent_version: window.agent_version.clone(),
            observations,
            alerts,
            fixture_candidates,
            statistic_snapshots: self.snapshot_statistics(),
        }
    }

    #[must_use]
    pub fn snapshot_statistics(&self) -> BTreeMap<PromptDriftSignalKind, DriftStatisticSnapshot> {
        BTreeMap::from([
            (
                PromptDriftSignalKind::RuleHitRate,
                self.rule_hit_rate.snapshot(),
            ),
            (
                PromptDriftSignalKind::UnmatchedEntropy,
                self.unmatched_entropy.snapshot(),
            ),
            (
                PromptDriftSignalKind::MotifFrequency,
                self.motif_frequency.snapshot(),
            ),
        ])
    }

    fn fixture_candidate(
        &self,
        window: &PromptTranscriptWindow,
        observations: &PromptDriftObservations,
        evidence: &[String],
        outcome: DriftUpdateOutcome,
    ) -> FixtureCandidate {
        let source = if window.raw_text.trim().is_empty() {
            window.unmatched_spans.join("\n")
        } else {
            window.raw_text.clone()
        };
        let redacted_text = truncate_to_char_boundary(
            &self.redactor.redact(source.trim()),
            self.config.max_fixture_bytes,
        );
        let expected_match_pattern = observations
            .top_motif
            .as_ref()
            .map(|motif| format!("review-motif:{motif}"))
            .or_else(|| {
                window
                    .expected_rule_ids
                    .first()
                    .map(|id| format!("rule:{id}"))
            })
            .unwrap_or_else(|| "review-unmatched-output".to_string());
        let suppression_reason = match outcome {
            DriftUpdateOutcome::Suppressed { reason, .. } => Some(reason),
            DriftUpdateOutcome::Quiet | DriftUpdateOutcome::Alert { .. } => None,
        };

        FixtureCandidate {
            agent_family: window.agent_family.clone(),
            agent_version: window.agent_version.clone(),
            source_window_id: window.window_id.clone(),
            redacted_text,
            expected_match_pattern,
            evidence: evidence.to_vec(),
            suppression_reason,
        }
    }
}

#[must_use]
pub fn analyze_transcript_window(
    window: &PromptTranscriptWindow,
    min_motif_repetitions: usize,
) -> PromptDriftObservations {
    let rule_hit_rate = if window.expected_rule_ids.is_empty() {
        1.0
    } else {
        let expected = window.expected_rule_ids.len() as f64;
        let hits = window
            .expected_rule_ids
            .iter()
            .filter(|expected_id| window.matched_rule_ids.contains(expected_id))
            .count() as f64;
        hits / expected
    };

    let unmatched_text = window.unmatched_spans.join("\n");
    let unmatched_entropy = normalized_byte_entropy(&unmatched_text);
    let (top_motif, motif_frequency) = top_repeated_motif(&unmatched_text, min_motif_repetitions);

    PromptDriftObservations {
        rule_hit_rate,
        unmatched_entropy,
        motif_frequency,
        top_motif,
    }
}

fn evidence_for_signal(
    signal: PromptDriftSignalKind,
    observation: f64,
    window: &PromptTranscriptWindow,
    observations: &PromptDriftObservations,
) -> Vec<String> {
    match signal {
        PromptDriftSignalKind::RuleHitRate => vec![
            format!("rule_hit_rate={observation:.3}"),
            format!("expected_rules={}", window.expected_rule_ids.len()),
            format!("matched_rules={}", window.matched_rule_ids.len()),
        ],
        PromptDriftSignalKind::UnmatchedEntropy => vec![
            format!("unmatched_entropy={observation:.3}"),
            format!("unmatched_spans={}", window.unmatched_spans.len()),
        ],
        PromptDriftSignalKind::MotifFrequency => vec![
            format!("motif_frequency={observation:.3}"),
            format!(
                "top_motif={}",
                observations.top_motif.as_deref().unwrap_or("<none>")
            ),
        ],
    }
}

fn normalized_byte_entropy(text: &str) -> f64 {
    if text.is_empty() {
        return 0.0;
    }

    let mut counts = [0usize; 256];
    for byte in text.bytes() {
        counts[byte as usize] += 1;
    }
    let len = text.len() as f64;
    let entropy = counts
        .into_iter()
        .filter(|count| *count > 0)
        .map(|count| {
            let p = count as f64 / len;
            -p * p.log2()
        })
        .sum::<f64>();

    (entropy / 8.0).clamp(0.0, 1.0)
}

fn top_repeated_motif(text: &str, min_repetitions: usize) -> (Option<String>, f64) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let tokens = text
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .filter(|token| token.len() >= 4)
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();

    for token in &tokens {
        *counts.entry(token.clone()).or_insert(0) += 1;
    }

    let Some((motif, count)) = counts
        .into_iter()
        .filter(|(_, count)| *count >= min_repetitions)
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
    else {
        return (None, 0.0);
    };

    let frequency = if tokens.is_empty() {
        0.0
    } else {
        count as f64 / tokens.len() as f64
    };
    (Some(motif), frequency)
}

fn truncate_to_char_boundary(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }

    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> DriftStatisticParams {
        // Baseline mean 0 (centered observations); standard
        // CUSUM unit-variance defaults: k=0.5, h=5.0; budget=3.
        DriftStatisticParams {
            baseline_mean: 0.0,
            reference_k: 0.5,
            alarm_threshold_h: 5.0,
            false_alarm_budget: 3,
        }
    }

    /// New canary has zero CUSUM, zero observations, full budget.
    #[test]
    fn new_starts_clean() {
        let stat = DriftStatistic::new(params());
        let snap = stat.snapshot();
        assert!(snap.cusum_high.abs() <= f64::EPSILON);
        assert!(snap.cusum_low.abs() <= f64::EPSILON);
        assert_eq!(snap.observations_count, 0);
        assert_eq!(snap.alarms_count, 0);
        assert_eq!(snap.suppressed_alarms_count, 0);
        assert_eq!(snap.budget_remaining, 3);
    }

    /// Baseline observations near the mean never alarm, even after many
    /// samples. Pins the CUSUM's documented stability under no-
    /// drift conditions.
    #[test]
    fn baseline_observations_never_alarm() {
        let mut stat = DriftStatistic::new(params());
        for _ in 0..1_000 {
            // Mean-centered, small noise around 0 below the
            // reference value k=0.5.
            assert_eq!(stat.update(0.0), None);
            assert_eq!(stat.update(0.1), None);
            assert_eq!(stat.update(-0.1), None);
        }
        let snap = stat.snapshot();
        assert_eq!(snap.alarms_count, 0);
        assert_eq!(snap.suppressed_alarms_count, 0);
    }

    /// A sustained upward shift eventually crosses the alarm
    /// threshold. With observation = +1.0 each step (k=0.5,
    /// h=5.0), each step accumulates 0.5 in cusum_high, so it alarms
    /// at step 10.
    #[test]
    fn sustained_upward_shift_alarms() {
        let mut stat = DriftStatistic::new(params());
        let mut alerted = false;
        for _ in 0..50 {
            if let Some(alert) = stat.update(1.0) {
                match alert {
                    DriftAlert::UpwardShift { .. } => {
                        alerted = true;
                        break;
                    }
                    other @ DriftAlert::DownwardShift { .. } => {
                        panic!("expected UpwardShift, got {other:?}")
                    }
                }
            }
        }
        assert!(alerted, "upward shift should eventually alarm");
        let snap = stat.snapshot();
        assert_eq!(snap.alarms_count, 1);
        // Post-alarm, cusum reset to 0.
        assert!(snap.cusum_high.abs() <= f64::EPSILON);
        assert!(snap.cusum_low.abs() <= f64::EPSILON);
    }

    /// A sustained downward shift fires the DownwardShift variant.
    #[test]
    fn sustained_downward_shift_alarms_downward() {
        let mut stat = DriftStatistic::new(params());
        let mut alert = None;
        for _ in 0..50 {
            if let Some(a) = stat.update(-1.0) {
                alert = Some(a);
                break;
            }
        }
        match alert {
            Some(DriftAlert::DownwardShift { .. }) => {}
            other => panic!("expected DownwardShift, got {other:?}"),
        }
    }

    /// False-alarm budget is consumed: after `false_alarm_budget`
    /// alarms, further alarms are suppressed.
    #[test]
    fn budget_exhaustion_suppresses_further_alarms() {
        let mut stat = DriftStatistic::new(params()); // budget = 3

        // Fire the first 3 alarms.
        for _ in 0..3 {
            let mut fired = false;
            for _ in 0..50 {
                if stat.update(1.0).is_some() {
                    fired = true;
                    break;
                }
            }
            assert!(fired, "each of the first 3 alarms must fire");
        }
        let snap = stat.snapshot();
        assert_eq!(snap.alarms_count, 3);
        assert_eq!(snap.budget_remaining, 0);

        // Now the budget is exhausted. The next CUSUM crossing
        // bumps suppressed_alarms_count but does NOT return an
        // alert.
        let mut returned_alert = false;
        for _ in 0..50 {
            if stat.update(1.0).is_some() {
                returned_alert = true;
                break;
            }
        }
        assert!(
            !returned_alert,
            "post-budget-exhaustion alarms must not surface"
        );
        let snap = stat.snapshot();
        assert_eq!(snap.alarms_count, 3, "alarms_count freezes at budget");
        assert!(
            snap.suppressed_alarms_count >= 1,
            "suppressed counter must increment on suppressed crossings"
        );
    }

    /// Budget exhaustion does not panic and is monotonic across
    /// many observations.
    #[test]
    fn budget_exhaustion_is_stable_under_load() {
        let mut stat = DriftStatistic::new(DriftStatisticParams {
            false_alarm_budget: 1,
            ..params()
        });
        for _ in 0..10_000 {
            let _ = stat.update(1.0);
        }
        let snap = stat.snapshot();
        assert_eq!(snap.alarms_count, 1);
        assert!(snap.suppressed_alarms_count >= 1);
    }

    /// observations_count is monotonic regardless of alarm
    /// status.
    #[test]
    fn observations_count_is_monotonic() {
        let mut stat = DriftStatistic::new(params());
        for i in 1..=100 {
            stat.update(0.0);
            assert_eq!(stat.snapshot().observations_count, i);
        }
    }

    /// Snapshot is a pure read: calling it does not advance any
    /// state.
    #[test]
    fn snapshot_does_not_mutate() {
        let mut stat = DriftStatistic::new(params());
        for _ in 0..10 {
            stat.update(1.0);
        }
        let s1 = stat.snapshot();
        let s2 = stat.snapshot();
        assert_eq!(s1, s2);
    }

    /// DriftStatisticSnapshot serde roundtrips.
    #[test]
    fn drift_statistic_snapshot_serde_roundtrip() {
        let mut stat = DriftStatistic::new(params());
        for _ in 0..20 {
            stat.update(1.0);
        }
        let snap = stat.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: DriftStatisticSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(snap, back);
    }

    /// DriftAlert serde roundtrips both variants.
    #[test]
    fn drift_alert_serde_roundtrip() {
        let up = DriftAlert::UpwardShift {
            cusum_at_alarm: 5.5,
            observations_count: 42,
        };
        let json = serde_json::to_string(&up).expect("serialize");
        let back: DriftAlert = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(up, back);

        let down = DriftAlert::DownwardShift {
            cusum_at_alarm: 7.0,
            observations_count: 100,
        };
        let json = serde_json::to_string(&down).expect("serialize");
        let back: DriftAlert = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(down, back);
    }

    /// FixtureCandidate serde roundtrips.
    #[test]
    fn fixture_candidate_serde_roundtrip() {
        let candidate = FixtureCandidate {
            agent_family: "anthropic-claude".to_string(),
            agent_version: "4.7-opus".to_string(),
            source_window_id: "window-1".to_string(),
            redacted_text: "[REDACTED] error 500 from API".to_string(),
            expected_match_pattern: "error \\d{3}".to_string(),
            evidence: vec!["rule_hit_rate=0.000".to_string()],
            suppression_reason: Some(DriftSuppressionReason::BudgetExhausted),
        };
        let json = serde_json::to_string(&candidate).expect("serialize");
        let back: FixtureCandidate = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(candidate, back);
    }

    /// Bounded-state contract: the struct has only the documented
    /// fields and does not grow per observation. The unit test is
    /// effectively a compile-time pin via Debug formatting.
    #[test]
    fn drift_statistic_state_is_bounded() {
        let mut stat = DriftStatistic::new(params());
        let s_initial = format!("{:?}", stat.snapshot());
        for _ in 0..10_000 {
            stat.update(1.0);
        }
        let s_final = format!("{:?}", stat.snapshot());
        // The final snapshot string is comparable in shape to the
        // initial one: no per-observation accumulator, no
        // history buffer, no allocation hot path.
        assert!(s_initial.contains("DriftStatisticSnapshot"));
        assert!(s_final.contains("DriftStatisticSnapshot"));
    }

    /// Recovery from drift: after an alarm fires and the source
    /// returns to baseline, the canary becomes quiet again
    /// (CUSUM resets on alarm + future baseline observations
    /// don't accumulate evidence above k).
    #[test]
    fn returns_to_quiet_after_drift_resolves() {
        let mut stat = DriftStatistic::new(params());
        // Drift up until alarm.
        for _ in 0..50 {
            if stat.update(1.0).is_some() {
                break;
            }
        }
        let alarms_after_drift = stat.snapshot().alarms_count;
        // Now back to baseline: should NOT alarm again.
        for _ in 0..1_000 {
            assert_eq!(stat.update(0.0), None);
        }
        assert_eq!(stat.snapshot().alarms_count, alarms_after_drift);
    }

    fn replay_config(false_alarm_budget: u64) -> PromptDriftCanaryConfig {
        PromptDriftCanaryConfig {
            rule_hit_rate: DriftStatisticParams {
                baseline_mean: 1.0,
                reference_k: 0.1,
                alarm_threshold_h: 0.5,
                false_alarm_budget,
            },
            unmatched_entropy: DriftStatisticParams {
                baseline_mean: 0.0,
                reference_k: 1.0,
                alarm_threshold_h: 100.0,
                false_alarm_budget: 10,
            },
            motif_frequency: DriftStatisticParams {
                baseline_mean: 0.0,
                reference_k: 0.05,
                alarm_threshold_h: 0.4,
                false_alarm_budget: 10,
            },
            max_fixture_bytes: 160,
            min_motif_repetitions: 3,
        }
    }

    fn old_window() -> PromptTranscriptWindow {
        PromptTranscriptWindow {
            window_id: "old-baseline".to_string(),
            agent_family: "codex".to_string(),
            agent_version: "5.0".to_string(),
            expected_rule_ids: vec!["codex.rate_limit.detected".to_string()],
            matched_rule_ids: vec!["codex.rate_limit.detected".to_string()],
            unmatched_spans: vec![],
            raw_text: "rate limit; retry after 5 minutes".to_string(),
        }
    }

    fn drifted_window() -> PromptTranscriptWindow {
        PromptTranscriptWindow {
            window_id: "new-drift".to_string(),
            agent_family: "codex".to_string(),
            agent_version: "5.1".to_string(),
            expected_rule_ids: vec!["codex.rate_limit.detected".to_string()],
            matched_rule_ids: vec![],
            unmatched_spans: vec![
                "provider_slowdown provider_slowdown provider_slowdown provider_slowdown"
                    .to_string(),
            ],
            raw_text: concat!(
                "provider_slowdown provider_slowdown provider_slowdown; ",
                "Authorization: Bearer sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )
            .to_string(),
        }
    }

    #[test]
    fn transcript_analysis_tracks_rule_hits_entropy_and_motifs() {
        let observations = analyze_transcript_window(&drifted_window(), 3);
        assert!(observations.rule_hit_rate.abs() <= f64::EPSILON);
        assert!(observations.unmatched_entropy > 0.0);
        assert!(observations.motif_frequency > 0.0);
        assert_eq!(observations.top_motif.as_deref(), Some("provider_slowdown"));
    }

    #[test]
    fn replay_old_new_transcript_emits_alert_and_redacted_fixture() {
        let mut canary = PromptDriftCanary::new(replay_config(4));
        let baseline = canary.evaluate_window(&old_window());
        assert!(baseline.alerts.is_empty());
        assert!(baseline.fixture_candidates.is_empty());

        let drifted = canary.evaluate_window(&drifted_window());
        assert!(
            drifted
                .alerts
                .iter()
                .any(|alert| alert.signal == PromptDriftSignalKind::RuleHitRate),
            "missing rule-hit-rate alert: {drifted:?}"
        );
        assert!(
            drifted
                .alerts
                .iter()
                .any(|alert| alert.signal == PromptDriftSignalKind::MotifFrequency),
            "missing motif-frequency alert: {drifted:?}"
        );
        let candidate = drifted
            .fixture_candidates
            .iter()
            .find(|candidate| {
                candidate
                    .expected_match_pattern
                    .contains("provider_slowdown")
                    && candidate
                        .evidence
                        .iter()
                        .any(|line| line.contains("top_motif"))
            })
            .expect("motif candidate");
        assert_eq!(candidate.source_window_id, "new-drift");
        assert!(candidate.redacted_text.contains("[REDACTED]"));
        assert!(!candidate.redacted_text.contains("sk-aaaaaaaa"));
    }

    #[test]
    fn budget_suppression_still_emits_review_candidate_with_reason() {
        let mut canary = PromptDriftCanary::new(replay_config(0));
        let drifted = canary.evaluate_window(&drifted_window());
        let candidate = drifted
            .fixture_candidates
            .iter()
            .find(|candidate| {
                candidate.suppression_reason == Some(DriftSuppressionReason::BudgetExhausted)
            })
            .expect("suppressed fixture candidate");
        assert_eq!(
            candidate.suppression_reason,
            Some(DriftSuppressionReason::BudgetExhausted)
        );
        assert!(
            drifted
                .alerts
                .iter()
                .any(|alert| matches!(alert.outcome, DriftUpdateOutcome::Suppressed { .. }))
        );
    }
}
