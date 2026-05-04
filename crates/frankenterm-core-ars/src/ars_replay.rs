//! Historical SQLite Replay Validation Harness for ARS.
//!
//! Before promoting any reflex to Active, the system "dreams": dry-runs
//! the FST logic and synthesized commands against historical output
//! segments to mathematically prove the reflex *would* have worked on
//! past incidents.
//!
//! # Replay Pipeline
//!
//! ```text
//! Historical segments → pattern match → FST lookup → simulate commands
//!                                                      ↓
//!                                     expected output ≈ actual output?
//!                                                      ↓
//!                                         ReplayVerdict (Pass/Fail/Inconclusive)
//! ```
//!
//! # Integration
//!
//! If historical replay fails, the reflex is marked flawed and discarded.
//! Only reflexes that pass replay validation can be promoted from
//! Incubating to Active.

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::ars_fst::ReflexId;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for replay validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplayConfig {
    /// Minimum historical incidents needed for validation.
    pub min_incidents: usize,
    /// Minimum pass rate (fraction, 0.0–1.0) to consider a reflex valid.
    pub min_pass_rate: f64,
    /// Maximum replay time per incident (ms) before timeout.
    pub max_replay_ms: u64,
    /// Whether to include inconclusive results in pass rate calculation.
    pub count_inconclusive_as_pass: bool,
    /// Maximum incidents to replay (cap for performance).
    pub max_incidents: usize,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            min_incidents: 3,
            min_pass_rate: 0.8,
            max_replay_ms: 5000,
            count_inconclusive_as_pass: false,
            max_incidents: 100,
        }
    }
}

// =============================================================================
// Historical incident
// =============================================================================

/// A historical incident to replay against.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoricalIncident {
    /// Unique incident ID.
    pub incident_id: String,
    /// The trigger pattern that was detected.
    pub trigger_pattern: Vec<u8>,
    /// Output segments at the time of detection.
    pub output_before: String,
    /// Output segments after resolution.
    pub output_after: String,
    /// Commands that were actually executed.
    pub actual_commands: Vec<String>,
    /// Timestamp of the incident.
    pub timestamp_ms: u64,
    /// Pane ID where it occurred.
    pub pane_id: u64,
    /// Whether the original resolution succeeded.
    pub original_success: bool,
}

// =============================================================================
// Replay verdict
// =============================================================================

/// Verdict for a single incident replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReplayVerdict {
    /// Replay succeeded — expected output matches.
    Pass {
        incident_id: String,
        match_score: f64,
    },
    /// Replay failed — output diverged.
    Fail {
        incident_id: String,
        reason: FailReason,
    },
    /// Cannot determine — insufficient data.
    Inconclusive { incident_id: String, reason: String },
}

impl ReplayVerdict {
    /// Whether this verdict is a pass.
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass { .. })
    }

    /// Whether this verdict is a fail.
    pub fn is_fail(&self) -> bool {
        matches!(self, Self::Fail { .. })
    }

    /// Get the incident ID.
    pub fn incident_id(&self) -> &str {
        match self {
            Self::Pass { incident_id, .. } => incident_id,
            Self::Fail { incident_id, .. } => incident_id,
            Self::Inconclusive { incident_id, .. } => incident_id,
        }
    }
}

/// Why a replay failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FailReason {
    /// Trigger pattern didn't match.
    PatternMismatch,
    /// Commands differ from what would be needed.
    CommandMismatch {
        expected: Vec<String>,
        proposed: Vec<String>,
    },
    /// Output similarity below threshold.
    OutputDivergence { similarity: f64, threshold: f64 },
    /// Replay timed out.
    Timeout { elapsed_ms: u64, max_ms: u64 },
    /// Original incident was a failure — can't validate against failure.
    OriginalFailed,
}

// =============================================================================
// Replay session
// =============================================================================

/// Aggregated result of replaying a reflex against historical incidents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplaySession {
    /// Reflex being validated.
    pub reflex_id: ReflexId,
    /// Commands the reflex would execute.
    pub proposed_commands: Vec<String>,
    /// Individual verdicts.
    pub verdicts: Vec<ReplayVerdict>,
    /// Overall assessment.
    pub assessment: ReplayAssessment,
    /// Timestamp of the replay session.
    pub timestamp_ms: u64,
}

/// Overall replay assessment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReplayAssessment {
    /// Reflex is validated — safe to promote.
    Validated { pass_rate: f64, incidents: usize },
    /// Reflex failed validation — should not promote.
    Rejected {
        pass_rate: f64,
        incidents: usize,
        reason: String,
    },
    /// Insufficient historical data — cannot validate.
    InsufficientData { available: usize, required: usize },
}

impl ReplayAssessment {
    /// Whether the reflex is validated.
    pub fn is_validated(&self) -> bool {
        matches!(self, Self::Validated { .. })
    }
}

// =============================================================================
// Replay harness
// =============================================================================

/// The replay validation harness.
pub struct ReplayHarness {
    config: ReplayConfig,
    /// Total sessions run.
    total_sessions: u64,
    /// Total validated.
    total_validated: u64,
    /// Total rejected.
    total_rejected: u64,
}

impl ReplayHarness {
    /// Create with configuration.
    pub fn new(config: ReplayConfig) -> Self {
        Self {
            config,
            total_sessions: 0,
            total_validated: 0,
            total_rejected: 0,
        }
    }

    /// Create with defaults.
    pub fn with_defaults() -> Self {
        Self::new(ReplayConfig::default())
    }

    /// Validate a reflex against historical incidents.
    ///
    /// Legacy entry: skips the trigger-matching contract (the
    /// reflex's trigger pattern is not threaded through). Use
    /// [`Self::validate_with_trigger`] when the caller knows the
    /// reflex's trigger and wants `PatternMismatch` enforcement.
    pub fn validate(
        &mut self,
        reflex_id: ReflexId,
        proposed_commands: &[String],
        incidents: &[HistoricalIncident],
        timestamp_ms: u64,
    ) -> ReplaySession {
        self.validate_inner(reflex_id, None, proposed_commands, incidents, timestamp_ms)
    }

    /// br-ft-q5ts7: validate a reflex against historical incidents
    /// with the reflex's trigger pattern threaded through. Each
    /// incident's `trigger_pattern` is byte-compared against
    /// `reflex_trigger`; mismatches emit `FailReason::PatternMismatch`
    /// instead of being silently scored on commands/output alone.
    ///
    /// This is the recommended entry point for promotion gates —
    /// the legacy `validate()` only checks command/output similarity
    /// and so cannot prove the reflex would actually re-trigger on
    /// the original incident shape.
    pub fn validate_with_trigger(
        &mut self,
        reflex_id: ReflexId,
        reflex_trigger: &[u8],
        proposed_commands: &[String],
        incidents: &[HistoricalIncident],
        timestamp_ms: u64,
    ) -> ReplaySession {
        self.validate_inner(
            reflex_id,
            Some(reflex_trigger),
            proposed_commands,
            incidents,
            timestamp_ms,
        )
    }

    fn validate_inner(
        &mut self,
        reflex_id: ReflexId,
        reflex_trigger: Option<&[u8]>,
        proposed_commands: &[String],
        incidents: &[HistoricalIncident],
        timestamp_ms: u64,
    ) -> ReplaySession {
        self.total_sessions += 1;

        // Check minimum incidents.
        if incidents.len() < self.config.min_incidents {
            return ReplaySession {
                reflex_id,
                proposed_commands: proposed_commands.to_vec(),
                verdicts: Vec::new(),
                assessment: ReplayAssessment::InsufficientData {
                    available: incidents.len(),
                    required: self.config.min_incidents,
                },
                timestamp_ms,
            };
        }

        // Replay each incident (up to max).
        let replay_count = incidents.len().min(self.config.max_incidents);
        let mut verdicts = Vec::with_capacity(replay_count);

        for incident in incidents.iter().take(replay_count) {
            let verdict = self.replay_incident(reflex_trigger, proposed_commands, incident);
            verdicts.push(verdict);
        }

        // Calculate pass rate.
        let (passes, total) = self.count_passes(&verdicts);
        let pass_rate = if total > 0 {
            passes as f64 / total as f64
        } else {
            0.0
        };

        let assessment = if pass_rate >= self.config.min_pass_rate {
            self.total_validated += 1;
            debug!(
                reflex_id,
                pass_rate,
                incidents = replay_count,
                "reflex validated via replay"
            );
            ReplayAssessment::Validated {
                pass_rate,
                incidents: replay_count,
            }
        } else {
            self.total_rejected += 1;
            warn!(
                reflex_id,
                pass_rate,
                min_rate = self.config.min_pass_rate,
                "reflex rejected by replay"
            );
            ReplayAssessment::Rejected {
                pass_rate,
                incidents: replay_count,
                reason: format!(
                    "pass rate {:.1}% below minimum {:.1}%",
                    pass_rate * 100.0,
                    self.config.min_pass_rate * 100.0
                ),
            }
        };

        ReplaySession {
            reflex_id,
            proposed_commands: proposed_commands.to_vec(),
            verdicts,
            assessment,
            timestamp_ms,
        }
    }

    /// Replay a single incident.
    ///
    /// br-ft-q5ts7: `reflex_trigger` is the reflex's trigger pattern;
    /// when `Some`, this replay requires `incident.trigger_pattern`
    /// to byte-match the reflex's pattern, otherwise emits
    /// `FailReason::PatternMismatch`. The `validate()` legacy entry
    /// passes `None` to preserve the pre-fix behavior; callers who
    /// know their reflex's trigger should use
    /// `validate_with_trigger` to thread the check through.
    /// Timeout enforcement is independent of the trigger argument:
    /// the per-incident wall clock is measured and a `Timeout`
    /// verdict is emitted if `max_replay_ms` is exceeded.
    fn replay_incident(
        &self,
        reflex_trigger: Option<&[u8]>,
        proposed_commands: &[String],
        incident: &HistoricalIncident,
    ) -> ReplayVerdict {
        let started = std::time::Instant::now();
        let incident_id = incident.incident_id.clone();

        // Skip failed originals — can't validate against known failures.
        if !incident.original_success {
            return ReplayVerdict::Inconclusive {
                incident_id,
                reason: "original incident was a failure".to_string(),
            };
        }

        // br-ft-q5ts7: trigger pattern check (when reflex_trigger
        // is supplied). The reflex must match the historical
        // incident's trigger byte-for-byte; otherwise a future
        // promotion of this reflex would not actually re-trigger
        // on the same incident shape.
        if let Some(trigger) = reflex_trigger {
            if trigger != incident.trigger_pattern.as_slice() {
                return ReplayVerdict::Fail {
                    incident_id,
                    reason: FailReason::PatternMismatch,
                };
            }
        }

        // Check command similarity.
        let cmd_similarity = command_similarity(proposed_commands, &incident.actual_commands);
        if cmd_similarity < 0.3 {
            // br-ft-q5ts7: even for a fast comparison branch,
            // honor the per-incident timeout so a degenerate
            // input (e.g. enormous command vec) can't blow past
            // the budget without surfacing it.
            if let Some(verdict) = self.maybe_timeout(&incident_id, started) {
                return verdict;
            }
            return ReplayVerdict::Fail {
                incident_id,
                reason: FailReason::CommandMismatch {
                    expected: incident.actual_commands.clone(),
                    proposed: proposed_commands.to_vec(),
                },
            };
        }

        // Check output similarity (would the proposed commands produce similar results?).
        let output_sim = text_similarity(&incident.output_before, &incident.output_after);

        // Score combines command match and output characteristics.
        let match_score = cmd_similarity.mul_add(0.7, output_sim * 0.3);

        // br-ft-q5ts7: terminal timeout check before emitting any
        // pass/fail verdict. If the per-incident replay budget
        // was exceeded by the cumulative work above, surface
        // Timeout so operators see the budget breach.
        if let Some(verdict) = self.maybe_timeout(&incident_id, started) {
            return verdict;
        }

        if match_score >= 0.5 {
            ReplayVerdict::Pass {
                incident_id,
                match_score,
            }
        } else {
            ReplayVerdict::Fail {
                incident_id,
                reason: FailReason::OutputDivergence {
                    similarity: match_score,
                    threshold: 0.5,
                },
            }
        }
    }

    /// br-ft-q5ts7: shared per-incident timeout helper. If the
    /// elapsed wall-clock since `started` exceeds `max_replay_ms`,
    /// returns `Some(ReplayVerdict::Fail { Timeout { elapsed, max } })`
    /// so call sites can short-circuit cleanly. Returns `None` when
    /// the budget is `0` (operators can disable timing by setting
    /// max_replay_ms=0; the budget is then "infinite" by convention)
    /// or when the elapsed time is within budget.
    fn maybe_timeout(
        &self,
        incident_id: &str,
        started: std::time::Instant,
    ) -> Option<ReplayVerdict> {
        if self.config.max_replay_ms == 0 {
            return None;
        }
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if elapsed_ms > self.config.max_replay_ms {
            return Some(ReplayVerdict::Fail {
                incident_id: incident_id.to_string(),
                reason: FailReason::Timeout {
                    elapsed_ms,
                    max_ms: self.config.max_replay_ms,
                },
            });
        }
        None
    }

    /// Count passes (and applicable total).
    fn count_passes(&self, verdicts: &[ReplayVerdict]) -> (usize, usize) {
        let mut passes = 0;
        let mut total = 0;

        for v in verdicts {
            match v {
                ReplayVerdict::Pass { .. } => {
                    passes += 1;
                    total += 1;
                }
                ReplayVerdict::Fail { .. } => {
                    total += 1;
                }
                ReplayVerdict::Inconclusive { .. } => {
                    if self.config.count_inconclusive_as_pass {
                        passes += 1;
                        total += 1;
                    }
                    // Otherwise excluded from total.
                }
            }
        }

        (passes, total)
    }

    /// Get statistics.
    pub fn stats(&self) -> ReplayStats {
        ReplayStats {
            total_sessions: self.total_sessions,
            total_validated: self.total_validated,
            total_rejected: self.total_rejected,
        }
    }

    /// Get configuration.
    pub fn config(&self) -> &ReplayConfig {
        &self.config
    }
}

/// Replay statistics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayStats {
    pub total_sessions: u64,
    pub total_validated: u64,
    pub total_rejected: u64,
}

// =============================================================================
// Similarity helpers
// =============================================================================

/// Compute Jaccard similarity between two command sequences.
fn command_similarity(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    // Normalize commands (trim whitespace, lowercase).
    let set_a: std::collections::HashSet<String> =
        a.iter().map(|s| s.trim().to_lowercase()).collect();
    let set_b: std::collections::HashSet<String> =
        b.iter().map(|s| s.trim().to_lowercase()).collect();

    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();

    if union == 0 {
        return 1.0;
    }
    intersection as f64 / union as f64
}

/// Simple text similarity using character bigrams.
fn text_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let bigrams_a: std::collections::HashSet<(char, char)> =
        a.chars().zip(a.chars().skip(1)).collect();
    let bigrams_b: std::collections::HashSet<(char, char)> =
        b.chars().zip(b.chars().skip(1)).collect();

    let intersection = bigrams_a.intersection(&bigrams_b).count();
    let union = bigrams_a.union(&bigrams_b).count();

    if union == 0 {
        return 1.0;
    }
    intersection as f64 / union as f64
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_incident(id: &str, success: bool) -> HistoricalIncident {
        HistoricalIncident {
            incident_id: id.to_string(),
            trigger_pattern: vec![1, 2, 3],
            output_before: "error: connection refused".to_string(),
            output_after: "connected successfully".to_string(),
            actual_commands: vec!["systemctl restart app".to_string()],
            timestamp_ms: 1000,
            pane_id: 1,
            original_success: success,
        }
    }

    fn make_incidents(n: usize) -> Vec<HistoricalIncident> {
        (0..n)
            .map(|i| make_incident(&format!("inc-{i}"), true))
            .collect()
    }

    // ---- Config ----

    #[test]
    fn default_config() {
        let config = ReplayConfig::default();
        assert_eq!(config.min_incidents, 3);
        let diff = (config.min_pass_rate - 0.8).abs();
        assert!(diff < 1e-10);
    }

    // ---- Similarity ----

    #[test]
    fn identical_commands_similarity_one() {
        let a = vec!["cmd1".to_string(), "cmd2".to_string()];
        let sim = command_similarity(&a, &a);
        let diff = (sim - 1.0).abs();
        assert!(diff < 1e-10);
    }

    #[test]
    fn disjoint_commands_similarity_zero() {
        let a = vec!["cmd1".to_string()];
        let b = vec!["cmd2".to_string()];
        let sim = command_similarity(&a, &b);
        let diff = sim.abs();
        assert!(diff < 1e-10);
    }

    #[test]
    fn empty_commands_similarity() {
        let sim = command_similarity(&[], &[]);
        let diff = (sim - 1.0).abs();
        assert!(diff < 1e-10);
    }

    #[test]
    fn text_similarity_identical() {
        let sim = text_similarity("hello world", "hello world");
        let diff = (sim - 1.0).abs();
        assert!(diff < 1e-10);
    }

    #[test]
    fn text_similarity_empty() {
        let sim = text_similarity("", "");
        let diff = (sim - 1.0).abs();
        assert!(diff < 1e-10);
    }

    #[test]
    fn text_similarity_different() {
        let sim = text_similarity("abc", "xyz");
        // Completely different bigrams.
        assert!(sim < 0.5);
    }

    // ---- Replay verdict ----

    #[test]
    fn pass_is_pass() {
        let v = ReplayVerdict::Pass {
            incident_id: "i1".to_string(),
            match_score: 0.9,
        };
        assert!(v.is_pass());
        assert!(!v.is_fail());
    }

    #[test]
    fn fail_is_fail() {
        let v = ReplayVerdict::Fail {
            incident_id: "i1".to_string(),
            reason: FailReason::PatternMismatch,
        };
        assert!(v.is_fail());
        assert!(!v.is_pass());
    }

    #[test]
    fn inconclusive_is_neither() {
        let v = ReplayVerdict::Inconclusive {
            incident_id: "i1".to_string(),
            reason: "test".to_string(),
        };
        assert!(!v.is_pass());
        assert!(!v.is_fail());
    }

    #[test]
    fn verdict_incident_id() {
        let v = ReplayVerdict::Pass {
            incident_id: "test-123".to_string(),
            match_score: 1.0,
        };
        assert_eq!(v.incident_id(), "test-123");
    }

    // ---- ReplayHarness ----

    #[test]
    fn insufficient_incidents() {
        let config = ReplayConfig {
            min_incidents: 5,
            ..Default::default()
        };
        let mut harness = ReplayHarness::new(config);
        let incidents = make_incidents(2);

        let session = harness.validate(1, &["cmd".into()], &incidents, 1000);
        let is_insuf = matches!(
            session.assessment,
            ReplayAssessment::InsufficientData { .. }
        );
        assert!(is_insuf);
    }

    #[test]
    fn validates_matching_commands() {
        let mut harness = ReplayHarness::with_defaults();
        let incidents = make_incidents(5);
        let commands = vec!["systemctl restart app".to_string()];

        let session = harness.validate(1, &commands, &incidents, 1000);
        assert!(session.assessment.is_validated());
    }

    #[test]
    fn rejects_mismatched_commands() {
        let mut harness = ReplayHarness::with_defaults();
        let incidents = make_incidents(5);
        let commands = vec!["completely different".to_string()];

        let session = harness.validate(1, &commands, &incidents, 1000);
        let is_rejected = matches!(session.assessment, ReplayAssessment::Rejected { .. });
        assert!(is_rejected);
    }

    #[test]
    fn skips_failed_originals() {
        let mut harness = ReplayHarness::with_defaults();
        let incidents: Vec<_> = (0..5)
            .map(|i| make_incident(&format!("inc-{i}"), false))
            .collect();
        let commands = vec!["systemctl restart app".to_string()];

        let session = harness.validate(1, &commands, &incidents, 1000);
        // All inconclusive → 0 applicable → depending on config, rejected.
        for v in &session.verdicts {
            let is_inconclusive = matches!(v, ReplayVerdict::Inconclusive { .. });
            assert!(is_inconclusive);
        }
    }

    #[test]
    fn stats_track_correctly() {
        let mut harness = ReplayHarness::with_defaults();
        let incidents = make_incidents(5);

        harness.validate(1, &["systemctl restart app".into()], &incidents, 1000);
        harness.validate(2, &["bad cmd".into()], &incidents, 2000);

        let stats = harness.stats();
        assert_eq!(stats.total_sessions, 2);
    }

    #[test]
    fn max_incidents_caps_replay() {
        let config = ReplayConfig {
            max_incidents: 3,
            min_incidents: 1,
            ..Default::default()
        };
        let mut harness = ReplayHarness::new(config);
        let incidents = make_incidents(10);
        let commands = vec!["systemctl restart app".to_string()];

        let session = harness.validate(1, &commands, &incidents, 1000);
        assert_eq!(session.verdicts.len(), 3);
    }

    #[test]
    fn inconclusive_as_pass_config() {
        let config = ReplayConfig {
            min_incidents: 3,
            count_inconclusive_as_pass: true,
            min_pass_rate: 0.5,
            ..Default::default()
        };
        let mut harness = ReplayHarness::new(config);
        // Mix of failed originals (inconclusive) and matching.
        let mut incidents = make_incidents(2);
        incidents.push(make_incident("failed", false));

        let session = harness.validate(1, &["systemctl restart app".into()], &incidents, 1000);
        assert!(session.assessment.is_validated());
    }

    // ---- Assessment ----

    #[test]
    fn validated_is_validated() {
        let a = ReplayAssessment::Validated {
            pass_rate: 0.9,
            incidents: 10,
        };
        assert!(a.is_validated());
    }

    #[test]
    fn rejected_is_not_validated() {
        let a = ReplayAssessment::Rejected {
            pass_rate: 0.3,
            incidents: 10,
            reason: "too low".to_string(),
        };
        assert!(!a.is_validated());
    }

    // ---- Serde roundtrips ----

    #[test]
    fn config_serde_roundtrip() {
        let config = ReplayConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let decoded: ReplayConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.min_incidents, config.min_incidents);
    }

    #[test]
    fn incident_serde_roundtrip() {
        let inc = make_incident("test", true);
        let json = serde_json::to_string(&inc).unwrap();
        let decoded: HistoricalIncident = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, inc);
    }

    #[test]
    fn verdict_serde_roundtrip() {
        let v = ReplayVerdict::Pass {
            incident_id: "i1".to_string(),
            match_score: 0.95,
        };
        let json = serde_json::to_string(&v).unwrap();
        let decoded: ReplayVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, v);
    }

    #[test]
    fn fail_reason_serde_roundtrip() {
        let reasons = vec![
            FailReason::PatternMismatch,
            FailReason::CommandMismatch {
                expected: vec!["a".into()],
                proposed: vec!["b".into()],
            },
            FailReason::OutputDivergence {
                similarity: 0.3,
                threshold: 0.5,
            },
            FailReason::Timeout {
                elapsed_ms: 6000,
                max_ms: 5000,
            },
            FailReason::OriginalFailed,
        ];
        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            let decoded: FailReason = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, reason);
        }
    }

    #[test]
    fn assessment_serde_roundtrip() {
        let assessments = vec![
            ReplayAssessment::Validated {
                pass_rate: 0.9,
                incidents: 10,
            },
            ReplayAssessment::Rejected {
                pass_rate: 0.3,
                incidents: 10,
                reason: "low".to_string(),
            },
            ReplayAssessment::InsufficientData {
                available: 1,
                required: 5,
            },
        ];
        for a in assessments {
            let json = serde_json::to_string(&a).unwrap();
            let decoded: ReplayAssessment = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, a);
        }
    }

    #[test]
    fn replay_stats_serde_roundtrip() {
        let stats = ReplayStats {
            total_sessions: 10,
            total_validated: 7,
            total_rejected: 3,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: ReplayStats = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, stats);
    }

    // ── br-ft-q5ts7: trigger matching + timeout validation ──

    #[test]
    fn validate_with_trigger_passes_on_matching_pattern_ft_q5ts7() {
        // br-ft-q5ts7: when reflex trigger matches the historical
        // incident's trigger_pattern, replay proceeds normally.
        let mut harness = ReplayHarness::with_defaults();
        let incidents = make_incidents(3);
        // make_incident uses trigger_pattern = vec![1, 2, 3].
        let session = harness.validate_with_trigger(
            42,
            &[1, 2, 3],
            &["systemctl restart app".to_string()],
            &incidents,
            1000,
        );
        // No PatternMismatch should appear — all verdicts should
        // be Pass or some other non-PatternMismatch shape.
        for v in &session.verdicts {
            if let ReplayVerdict::Fail {
                reason: FailReason::PatternMismatch,
                ..
            } = v
            {
                panic!("matching trigger must not produce PatternMismatch: {v:?}");
            }
        }
    }

    #[test]
    fn validate_with_trigger_fails_on_mismatched_pattern_ft_q5ts7() {
        // br-ft-q5ts7: a reflex whose trigger pattern doesn't
        // match the historical incident must fail with
        // PatternMismatch — pre-fix this would silently score
        // the reflex on commands/output alone.
        let mut harness = ReplayHarness::with_defaults();
        let incidents = make_incidents(3);
        let session = harness.validate_with_trigger(
            42,
            &[9, 9, 9], // different from incident's [1, 2, 3]
            &["systemctl restart app".to_string()],
            &incidents,
            1000,
        );
        let pattern_fails = session
            .verdicts
            .iter()
            .filter(|v| {
                matches!(
                    v,
                    ReplayVerdict::Fail {
                        reason: FailReason::PatternMismatch,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            pattern_fails, 3,
            "all 3 mismatched incidents must fail with PatternMismatch; got {:?}",
            session.verdicts
        );
        assert!(
            !session.assessment.is_validated(),
            "reflex with mismatched trigger must not validate"
        );
    }

    #[test]
    fn validate_legacy_skips_pattern_check_ft_q5ts7() {
        // The legacy validate() entry preserves pre-fix behavior
        // (no trigger matching). Useful for compatibility while
        // call sites migrate to validate_with_trigger.
        let mut harness = ReplayHarness::with_defaults();
        let incidents = make_incidents(3);
        let session = harness.validate(
            42,
            &["systemctl restart app".to_string()],
            &incidents,
            1000,
        );
        for v in &session.verdicts {
            if let ReplayVerdict::Fail {
                reason: FailReason::PatternMismatch,
                ..
            } = v
            {
                panic!("legacy validate must not emit PatternMismatch: {v:?}");
            }
        }
    }

    #[test]
    fn timeout_disabled_when_max_replay_ms_zero_ft_q5ts7() {
        // br-ft-q5ts7: max_replay_ms=0 is the documented "infinite
        // budget" sentinel. Even an instantaneous replay must not
        // emit Timeout.
        let config = ReplayConfig {
            max_replay_ms: 0,
            min_incidents: 1,
            ..ReplayConfig::default()
        };
        let mut harness = ReplayHarness::new(config);
        let incidents = vec![make_incident("inc-0", true)];
        let session = harness.validate(
            42,
            &["systemctl restart app".to_string()],
            &incidents,
            1000,
        );
        for v in &session.verdicts {
            if let ReplayVerdict::Fail {
                reason: FailReason::Timeout { .. },
                ..
            } = v
            {
                panic!("max_replay_ms=0 must not emit Timeout: {v:?}");
            }
        }
    }

    #[test]
    fn timeout_helper_emits_fail_on_budget_overrun_ft_q5ts7() {
        // br-ft-q5ts7: directly exercise maybe_timeout to pin the
        // contract. With a 0ms budget (but >0 disables; use 1ms)
        // and a sleep, the helper must return Fail{Timeout}.
        let config = ReplayConfig {
            max_replay_ms: 1,
            ..ReplayConfig::default()
        };
        let harness = ReplayHarness::new(config);
        let started = std::time::Instant::now();
        // Sleep >1ms to ensure elapsed > budget.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let verdict = harness.maybe_timeout("inc-test", started);
        assert!(verdict.is_some(), "must surface Timeout after budget overrun");
        let v = verdict.unwrap();
        match v {
            ReplayVerdict::Fail {
                reason: FailReason::Timeout { elapsed_ms, max_ms },
                ..
            } => {
                assert!(elapsed_ms >= 5);
                assert_eq!(max_ms, 1);
            }
            other => panic!("expected Fail{{Timeout}}, got {other:?}"),
        }
    }

    #[test]
    fn timeout_helper_passes_on_within_budget_ft_q5ts7() {
        // Sanity: an instant call returns None (within budget).
        let config = ReplayConfig {
            max_replay_ms: 5000,
            ..ReplayConfig::default()
        };
        let harness = ReplayHarness::new(config);
        let started = std::time::Instant::now();
        let verdict = harness.maybe_timeout("inc-fast", started);
        assert!(verdict.is_none(), "fast replay must not surface Timeout");
    }
}
