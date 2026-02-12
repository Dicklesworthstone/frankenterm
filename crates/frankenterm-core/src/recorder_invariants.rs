//! Recorder invariant checking for ordering, completeness, and replay determinism.
//!
//! Formalizes and enforces the flight recorder's correctness contract:
//! - **Ordering**: Events within a (pane, stream) domain are monotonically sequenced.
//! - **Completeness**: No silent gaps — all discontinuities have explicit gap markers.
//! - **Correlation**: Causal links reference valid existing events.
//! - **Replay determinism**: Same log replays to identical merge-sorted output.
//!
//! Violations are reported as structured diagnostics with actionable context.
//!
//! Bead: wa-oegrb.7.3

use std::collections::{HashMap, HashSet};

use crate::event_id::{ClockAnomalyTracker, RecorderMergeKey, StreamKind};
use crate::recording::{RecorderEvent, RecorderEventPayload};

// ---------------------------------------------------------------------------
// Violation types
// ---------------------------------------------------------------------------

/// Severity of an invariant violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViolationSeverity {
    /// Data is technically valid but suspicious (e.g. large gap in sequence).
    Warning,
    /// Invariant is violated — data may be incomplete or misordered.
    Error,
    /// Critical violation — replay determinism is compromised.
    Critical,
}

/// Category of invariant violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViolationKind {
    /// Sequence number not monotonically increasing within a domain.
    SequenceRegression,
    /// Sequence number skipped without an explicit gap marker.
    SequenceGap,
    /// Duplicate sequence number within a domain.
    DuplicateSequence,
    /// Duplicate event_id across the entire event set.
    DuplicateEventId,
    /// Clock timestamp regression within a domain.
    ClockRegression,
    /// Clock timestamp suspiciously far in the future.
    ClockFutureSkew,
    /// Causal parent_event_id references a non-existent event.
    DanglingParentRef,
    /// Causal trigger_event_id references a non-existent event.
    DanglingTriggerRef,
    /// Causal root_event_id references a non-existent event.
    DanglingRootRef,
    /// Merge key ordering violated between consecutive events.
    MergeOrderViolation,
    /// Event has an empty event_id.
    EmptyEventId,
    /// Schema version mismatch within a single log.
    SchemaVersionMismatch,
}

/// A single invariant violation with diagnostic context.
#[derive(Debug, Clone)]
pub struct Violation {
    /// What kind of invariant was violated.
    pub kind: ViolationKind,
    /// Severity of the violation.
    pub severity: ViolationSeverity,
    /// The event_id of the offending event (if available).
    pub event_id: String,
    /// The pane_id of the offending event.
    pub pane_id: u64,
    /// Human-readable diagnostic message.
    pub message: String,
    /// Index of the event in the input sequence (0-based).
    pub event_index: usize,
}

// ---------------------------------------------------------------------------
// Invariant check report
// ---------------------------------------------------------------------------

/// Summary report from invariant checking.
#[derive(Debug, Clone)]
pub struct InvariantReport {
    /// All violations found.
    pub violations: Vec<Violation>,
    /// Total events checked.
    pub events_checked: usize,
    /// Number of distinct panes observed.
    pub panes_observed: usize,
    /// Number of distinct (pane, stream) domains observed.
    pub domains_observed: usize,
    /// Whether the event set passes all invariants.
    pub passed: bool,
}

impl InvariantReport {
    /// Count violations by kind.
    #[must_use]
    pub fn count_by_kind(&self, kind: ViolationKind) -> usize {
        self.violations.iter().filter(|v| v.kind == kind).count()
    }

    /// Count violations by severity.
    #[must_use]
    pub fn count_by_severity(&self, severity: ViolationSeverity) -> usize {
        self.violations
            .iter()
            .filter(|v| v.severity == severity)
            .count()
    }

    /// Returns true if there are any critical violations.
    #[must_use]
    pub fn has_critical(&self) -> bool {
        self.violations
            .iter()
            .any(|v| v.severity == ViolationSeverity::Critical)
    }

    /// Returns true if there are any error-level violations.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.violations
            .iter()
            .any(|v| v.severity == ViolationSeverity::Error)
    }
}

// ---------------------------------------------------------------------------
// Invariant checker
// ---------------------------------------------------------------------------

/// Configuration for invariant checking.
#[derive(Debug, Clone)]
pub struct InvariantCheckerConfig {
    /// Maximum allowed gap between consecutive sequence numbers before warning.
    pub max_sequence_gap: u64,
    /// Whether to check causal reference validity.
    pub check_causality: bool,
    /// Whether to check merge key ordering (requires events to be pre-sorted).
    pub check_merge_order: bool,
    /// Clock future-skew threshold in ms (0 to disable).
    pub clock_future_skew_threshold_ms: u64,
    /// Expected schema version (empty to skip check).
    pub expected_schema_version: String,
}

impl Default for InvariantCheckerConfig {
    fn default() -> Self {
        Self {
            max_sequence_gap: 100,
            check_causality: true,
            check_merge_order: true,
            clock_future_skew_threshold_ms: 60_000, // 1 minute
            expected_schema_version: String::new(),
        }
    }
}

/// Checks a sequence of recorder events against the flight recorder's invariant contract.
pub struct InvariantChecker {
    config: InvariantCheckerConfig,
}

impl InvariantChecker {
    /// Create a checker with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: InvariantCheckerConfig::default(),
        }
    }

    /// Create a checker with custom configuration.
    #[must_use]
    pub fn with_config(config: InvariantCheckerConfig) -> Self {
        Self { config }
    }

    /// Check a slice of events and return a violation report.
    ///
    /// Events should be in the order they appear in the log (append order).
    /// If `check_merge_order` is enabled, events are also checked for
    /// RecorderMergeKey ordering (requires pre-sorted input).
    #[must_use]
    pub fn check(&self, events: &[RecorderEvent]) -> InvariantReport {
        let mut violations = Vec::new();

        // Per-domain tracking: (pane_id, StreamKind) → last sequence
        let mut domain_sequences: HashMap<(u64, StreamKind), u64> = HashMap::new();
        let mut domain_seen_sequences: HashMap<(u64, StreamKind), HashSet<u64>> = HashMap::new();

        // Global tracking
        let mut event_ids: HashSet<String> = HashSet::new();
        let mut panes: HashSet<u64> = HashSet::new();
        let mut domains: HashSet<(u64, StreamKind)> = HashSet::new();

        // Clock anomaly tracking
        let mut clock_tracker =
            ClockAnomalyTracker::new(self.config.clock_future_skew_threshold_ms);

        // Previous merge key for ordering check
        let mut prev_merge_key: Option<RecorderMergeKey> = None;

        for (idx, event) in events.iter().enumerate() {
            let stream_kind = StreamKind::from_payload(&event.payload);
            let domain = (event.pane_id, stream_kind);

            panes.insert(event.pane_id);
            domains.insert(domain);

            // -- Empty event_id check --
            if event.event_id.is_empty() {
                violations.push(Violation {
                    kind: ViolationKind::EmptyEventId,
                    severity: ViolationSeverity::Error,
                    event_id: String::new(),
                    pane_id: event.pane_id,
                    message: format!(
                        "Event at index {} has empty event_id (pane={}, seq={})",
                        idx, event.pane_id, event.sequence
                    ),
                    event_index: idx,
                });
            }

            // -- Schema version check --
            if !self.config.expected_schema_version.is_empty()
                && event.schema_version != self.config.expected_schema_version
            {
                violations.push(Violation {
                    kind: ViolationKind::SchemaVersionMismatch,
                    severity: ViolationSeverity::Error,
                    event_id: event.event_id.clone(),
                    pane_id: event.pane_id,
                    message: format!(
                        "Schema version mismatch: expected '{}', got '{}' at index {}",
                        self.config.expected_schema_version, event.schema_version, idx
                    ),
                    event_index: idx,
                });
            }

            // -- Duplicate event_id check --
            if !event.event_id.is_empty() && !event_ids.insert(event.event_id.clone()) {
                violations.push(Violation {
                    kind: ViolationKind::DuplicateEventId,
                    severity: ViolationSeverity::Critical,
                    event_id: event.event_id.clone(),
                    pane_id: event.pane_id,
                    message: format!(
                        "Duplicate event_id '{}' at index {} (pane={})",
                        event.event_id, idx, event.pane_id
                    ),
                    event_index: idx,
                });
            }

            // -- Per-domain sequence checks --
            let seen = domain_seen_sequences.entry(domain).or_default();

            if !seen.insert(event.sequence) {
                violations.push(Violation {
                    kind: ViolationKind::DuplicateSequence,
                    severity: ViolationSeverity::Error,
                    event_id: event.event_id.clone(),
                    pane_id: event.pane_id,
                    message: format!(
                        "Duplicate sequence {} in domain ({}, {:?}) at index {}",
                        event.sequence, event.pane_id, stream_kind, idx
                    ),
                    event_index: idx,
                });
            }

            if let Some(&last_seq) = domain_sequences.get(&domain) {
                if event.sequence < last_seq {
                    violations.push(Violation {
                        kind: ViolationKind::SequenceRegression,
                        severity: ViolationSeverity::Critical,
                        event_id: event.event_id.clone(),
                        pane_id: event.pane_id,
                        message: format!(
                            "Sequence regression in ({}, {:?}): {} < {} at index {}",
                            event.pane_id, stream_kind, event.sequence, last_seq, idx
                        ),
                        event_index: idx,
                    });
                } else if event.sequence > last_seq + 1 {
                    let gap = event.sequence - last_seq - 1;
                    // Check if there's an explicit gap marker
                    let has_gap_marker = matches!(
                        &event.payload,
                        RecorderEventPayload::EgressOutput { is_gap: true, .. }
                    );

                    if !has_gap_marker && gap > 0 {
                        let severity = if gap > self.config.max_sequence_gap {
                            ViolationSeverity::Error
                        } else {
                            ViolationSeverity::Warning
                        };
                        violations.push(Violation {
                            kind: ViolationKind::SequenceGap,
                            severity,
                            event_id: event.event_id.clone(),
                            pane_id: event.pane_id,
                            message: format!(
                                "Sequence gap of {} in ({}, {:?}): {} → {} at index {}",
                                gap, event.pane_id, stream_kind, last_seq, event.sequence, idx
                            ),
                            event_index: idx,
                        });
                    }
                }
            }
            domain_sequences.insert(domain, event.sequence);

            // -- Clock anomaly check --
            let clock_result =
                clock_tracker.observe(event.pane_id, stream_kind, event.occurred_at_ms);
            if clock_result.is_anomaly {
                let reason = clock_result.reason.unwrap_or_default();
                let kind = if reason.contains("regression") {
                    ViolationKind::ClockRegression
                } else {
                    ViolationKind::ClockFutureSkew
                };
                violations.push(Violation {
                    kind,
                    severity: ViolationSeverity::Warning,
                    event_id: event.event_id.clone(),
                    pane_id: event.pane_id,
                    message: format!("Clock anomaly at index {}: {}", idx, reason),
                    event_index: idx,
                });
            }

            // -- Merge key ordering check --
            if self.config.check_merge_order {
                let key = RecorderMergeKey::from_event(event);
                if let Some(ref prev) = prev_merge_key {
                    if key < *prev {
                        violations.push(Violation {
                            kind: ViolationKind::MergeOrderViolation,
                            severity: ViolationSeverity::Error,
                            event_id: event.event_id.clone(),
                            pane_id: event.pane_id,
                            message: format!(
                                "Merge order violated at index {}: current key < previous key",
                                idx
                            ),
                            event_index: idx,
                        });
                    }
                }
                prev_merge_key = Some(key);
            }

            // -- Causality reference checks --
            if self.config.check_causality {
                if let Some(ref parent_id) = event.causality.parent_event_id {
                    if !parent_id.is_empty() && !event_ids.contains(parent_id) {
                        violations.push(Violation {
                            kind: ViolationKind::DanglingParentRef,
                            severity: ViolationSeverity::Warning,
                            event_id: event.event_id.clone(),
                            pane_id: event.pane_id,
                            message: format!(
                                "Dangling parent_event_id '{}' at index {} (event '{}')",
                                parent_id, idx, event.event_id
                            ),
                            event_index: idx,
                        });
                    }
                }
                if let Some(ref trigger_id) = event.causality.trigger_event_id {
                    if !trigger_id.is_empty() && !event_ids.contains(trigger_id) {
                        violations.push(Violation {
                            kind: ViolationKind::DanglingTriggerRef,
                            severity: ViolationSeverity::Warning,
                            event_id: event.event_id.clone(),
                            pane_id: event.pane_id,
                            message: format!(
                                "Dangling trigger_event_id '{}' at index {} (event '{}')",
                                trigger_id, idx, event.event_id
                            ),
                            event_index: idx,
                        });
                    }
                }
                if let Some(ref root_id) = event.causality.root_event_id {
                    if !root_id.is_empty() && !event_ids.contains(root_id) {
                        violations.push(Violation {
                            kind: ViolationKind::DanglingRootRef,
                            severity: ViolationSeverity::Warning,
                            event_id: event.event_id.clone(),
                            pane_id: event.pane_id,
                            message: format!(
                                "Dangling root_event_id '{}' at index {} (event '{}')",
                                root_id, idx, event.event_id
                            ),
                            event_index: idx,
                        });
                    }
                }
            }
        }

        let passed = !violations
            .iter()
            .any(|v| matches!(v.severity, ViolationSeverity::Error | ViolationSeverity::Critical));

        InvariantReport {
            violations,
            events_checked: events.len(),
            panes_observed: panes.len(),
            domains_observed: domains.len(),
            passed,
        }
    }
}

impl Default for InvariantChecker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Replay determinism verifier
// ---------------------------------------------------------------------------

/// Verifies that two sequences of events produce identical merge-sorted output.
///
/// This is the core replay determinism check: given the same log, sorting by
/// RecorderMergeKey must always produce the same ordering.
#[must_use]
pub fn verify_replay_determinism(
    events_a: &[RecorderEvent],
    events_b: &[RecorderEvent],
) -> ReplayDeterminismResult {
    if events_a.len() != events_b.len() {
        return ReplayDeterminismResult {
            deterministic: false,
            divergence_index: Some(events_a.len().min(events_b.len())),
            message: format!(
                "Event count mismatch: {} vs {}",
                events_a.len(),
                events_b.len()
            ),
        };
    }

    let mut keys_a: Vec<RecorderMergeKey> =
        events_a.iter().map(RecorderMergeKey::from_event).collect();
    let mut keys_b: Vec<RecorderMergeKey> =
        events_b.iter().map(RecorderMergeKey::from_event).collect();

    keys_a.sort();
    keys_b.sort();

    for (i, (ka, kb)) in keys_a.iter().zip(keys_b.iter()).enumerate() {
        if ka != kb {
            return ReplayDeterminismResult {
                deterministic: false,
                divergence_index: Some(i),
                message: format!(
                    "Merge key divergence at position {}: {:?} vs {:?}",
                    i, ka, kb
                ),
            };
        }
    }

    ReplayDeterminismResult {
        deterministic: true,
        divergence_index: None,
        message: String::new(),
    }
}

/// Result of a replay determinism check.
#[derive(Debug, Clone)]
pub struct ReplayDeterminismResult {
    /// True if both sequences produce identical merge-sorted output.
    pub deterministic: bool,
    /// Index where the first divergence was found (if any).
    pub divergence_index: Option<usize>,
    /// Diagnostic message.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{
        RecorderControlMarkerType, RecorderEventCausality, RecorderEventSource,
        RecorderIngressKind, RecorderLifecyclePhase, RecorderRedactionLevel,
        RecorderSegmentKind, RecorderTextEncoding, RECORDER_EVENT_SCHEMA_VERSION_V1,
    };

    fn make_event(
        id: &str,
        pane_id: u64,
        seq: u64,
        ts: u64,
        text: &str,
    ) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: id.to_string(),
            pane_id,
            session_id: Some("s1".into()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::RobotMode,
            occurred_at_ms: ts,
            recorded_at_ms: ts + 1,
            sequence: seq,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::IngressText {
                text: text.into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        }
    }

    fn make_egress_gap(
        id: &str,
        pane_id: u64,
        seq: u64,
        ts: u64,
    ) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: id.to_string(),
            pane_id,
            session_id: Some("s1".into()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: ts,
            recorded_at_ms: ts + 1,
            sequence: seq,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::EgressOutput {
                text: String::new(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Gap,
                is_gap: true,
            },
        }
    }

    // -- Basic invariant checks --

    #[test]
    fn empty_events_pass() {
        let checker = InvariantChecker::new();
        let report = checker.check(&[]);
        assert!(report.passed);
        assert_eq!(report.events_checked, 0);
        assert!(report.violations.is_empty());
    }

    #[test]
    fn single_event_passes() {
        let checker = InvariantChecker::new();
        let events = vec![make_event("e1", 1, 0, 1000, "hello")];
        let report = checker.check(&events);
        assert!(report.passed);
        assert_eq!(report.events_checked, 1);
        assert_eq!(report.panes_observed, 1);
    }

    #[test]
    fn monotonic_sequence_passes() {
        let checker = InvariantChecker::new();
        let events = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_event("e2", 1, 1, 1001, "b"),
            make_event("e3", 1, 2, 1002, "c"),
        ];
        let report = checker.check(&events);
        assert!(report.passed);
        assert_eq!(report.violations.len(), 0);
    }

    // -- Sequence violations --

    #[test]
    fn sequence_regression_detected() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_event("e1", 1, 5, 1000, "a"),
            make_event("e2", 1, 3, 1001, "b"), // regression
        ];
        let report = checker.check(&events);
        assert!(!report.passed);
        assert_eq!(report.count_by_kind(ViolationKind::SequenceRegression), 1);
    }

    #[test]
    fn duplicate_sequence_detected() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_event("e2", 1, 0, 1001, "b"), // duplicate seq
        ];
        let report = checker.check(&events);
        assert!(!report.passed);
        assert_eq!(report.count_by_kind(ViolationKind::DuplicateSequence), 1);
    }

    #[test]
    fn sequence_gap_warning() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            max_sequence_gap: 100,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_event("e2", 1, 5, 1001, "b"), // gap of 4
        ];
        let report = checker.check(&events);
        assert!(report.passed); // warnings don't fail
        assert_eq!(report.count_by_kind(ViolationKind::SequenceGap), 1);
        assert_eq!(
            report.count_by_severity(ViolationSeverity::Warning),
            1
        );
    }

    #[test]
    fn large_sequence_gap_is_error() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            max_sequence_gap: 10,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_event("e2", 1, 50, 1001, "b"), // gap of 49 > max_sequence_gap
        ];
        let report = checker.check(&events);
        assert!(!report.passed);
        assert_eq!(report.count_by_kind(ViolationKind::SequenceGap), 1);
        assert_eq!(report.count_by_severity(ViolationSeverity::Error), 1);
    }

    #[test]
    fn gap_marker_suppresses_sequence_gap() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_egress_gap("e2", 1, 5, 1001), // gap marker → ok
        ];
        let report = checker.check(&events);
        // The egress gap is a different stream kind than ingress, so domains are independent
        // No gap violation because they're in different streams
        assert_eq!(report.count_by_kind(ViolationKind::SequenceGap), 0);
    }

    // -- Identity violations --

    #[test]
    fn duplicate_event_id_is_critical() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_event("same-id", 1, 0, 1000, "a"),
            make_event("same-id", 1, 1, 1001, "b"), // duplicate id!
        ];
        let report = checker.check(&events);
        assert!(!report.passed);
        assert!(report.has_critical());
        assert_eq!(report.count_by_kind(ViolationKind::DuplicateEventId), 1);
    }

    #[test]
    fn empty_event_id_is_error() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![make_event("", 1, 0, 1000, "a")];
        let report = checker.check(&events);
        assert!(!report.passed);
        assert_eq!(report.count_by_kind(ViolationKind::EmptyEventId), 1);
    }

    // -- Schema version check --

    #[test]
    fn schema_version_mismatch_detected() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            expected_schema_version: "ft.recorder.event.v1".to_string(),
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let mut event = make_event("e1", 1, 0, 1000, "a");
        event.schema_version = "ft.recorder.event.v2".to_string();
        let report = checker.check(&[event]);
        assert!(!report.passed);
        assert_eq!(
            report.count_by_kind(ViolationKind::SchemaVersionMismatch),
            1
        );
    }

    #[test]
    fn schema_version_skip_when_empty() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            expected_schema_version: String::new(),
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![make_event("e1", 1, 0, 1000, "a")];
        let report = checker.check(&events);
        assert_eq!(
            report.count_by_kind(ViolationKind::SchemaVersionMismatch),
            0
        );
    }

    // -- Clock anomaly checks --

    #[test]
    fn clock_regression_warning() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_event("e1", 1, 0, 2000, "a"),
            make_event("e2", 1, 1, 1000, "b"), // clock went backwards
        ];
        let report = checker.check(&events);
        assert!(report.passed); // clock anomalies are warnings
        assert_eq!(report.count_by_kind(ViolationKind::ClockRegression), 1);
    }

    #[test]
    fn clock_future_skew_warning() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            clock_future_skew_threshold_ms: 5_000,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_event("e2", 1, 1, 100_000, "b"), // 99 sec jump
        ];
        let report = checker.check(&events);
        assert!(report.passed);
        assert_eq!(report.count_by_kind(ViolationKind::ClockFutureSkew), 1);
    }

    // -- Multi-pane independence --

    #[test]
    fn independent_pane_sequences() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_event("e2", 2, 0, 1001, "b"), // same seq, different pane: ok
            make_event("e3", 1, 1, 1002, "c"),
            make_event("e4", 2, 1, 1003, "d"),
        ];
        let report = checker.check(&events);
        assert!(report.passed);
        assert_eq!(report.panes_observed, 2);
    }

    // -- Causality checks --

    #[test]
    fn dangling_parent_ref() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let mut event = make_event("e1", 1, 0, 1000, "a");
        event.causality.parent_event_id = Some("nonexistent".into());
        let report = checker.check(&[event]);
        assert!(report.passed); // dangling refs are warnings
        assert_eq!(report.count_by_kind(ViolationKind::DanglingParentRef), 1);
    }

    #[test]
    fn dangling_trigger_ref() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let mut event = make_event("e1", 1, 0, 1000, "a");
        event.causality.trigger_event_id = Some("nonexistent".into());
        let report = checker.check(&[event]);
        assert_eq!(report.count_by_kind(ViolationKind::DanglingTriggerRef), 1);
    }

    #[test]
    fn dangling_root_ref() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let mut event = make_event("e1", 1, 0, 1000, "a");
        event.causality.root_event_id = Some("nonexistent".into());
        let report = checker.check(&[event]);
        assert_eq!(report.count_by_kind(ViolationKind::DanglingRootRef), 1);
    }

    #[test]
    fn valid_causal_chain() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let mut e2 = make_event("e2", 1, 1, 1001, "b");
        e2.causality.parent_event_id = Some("e1".into());
        let events = vec![make_event("e1", 1, 0, 1000, "a"), e2];
        let report = checker.check(&events);
        assert_eq!(report.count_by_kind(ViolationKind::DanglingParentRef), 0);
    }

    #[test]
    fn causality_disabled() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            check_causality: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let mut event = make_event("e1", 1, 0, 1000, "a");
        event.causality.parent_event_id = Some("nonexistent".into());
        let report = checker.check(&[event]);
        assert_eq!(report.count_by_kind(ViolationKind::DanglingParentRef), 0);
    }

    // -- Merge order checks --

    #[test]
    fn sorted_events_pass_merge_order() {
        let checker = InvariantChecker::new();
        let events = vec![
            make_event("a", 1, 0, 1000, "first"),
            make_event("b", 1, 1, 1001, "second"),
            make_event("c", 2, 0, 1002, "third"),
        ];
        let report = checker.check(&events);
        assert_eq!(
            report.count_by_kind(ViolationKind::MergeOrderViolation),
            0
        );
    }

    #[test]
    fn unsorted_events_fail_merge_order() {
        let checker = InvariantChecker::new();
        let events = vec![
            make_event("b", 1, 0, 2000, "second"), // recorded_at=2001
            make_event("a", 1, 1, 1000, "first"),  // recorded_at=1001 < 2001
        ];
        let report = checker.check(&events);
        assert_eq!(
            report.count_by_kind(ViolationKind::MergeOrderViolation),
            1
        );
    }

    #[test]
    fn merge_order_disabled() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_event("b", 1, 0, 2000, "second"),
            make_event("a", 1, 1, 1000, "first"),
        ];
        let report = checker.check(&events);
        assert_eq!(
            report.count_by_kind(ViolationKind::MergeOrderViolation),
            0
        );
    }

    // -- Report helpers --

    #[test]
    fn report_count_by_kind() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_event("", 1, 0, 1000, "a"),  // empty id
            make_event("", 2, 0, 1001, "b"),  // empty id
            make_event("e3", 1, 0, 1002, "c"), // dup seq
        ];
        let report = checker.check(&events);
        assert_eq!(report.count_by_kind(ViolationKind::EmptyEventId), 2);
    }

    #[test]
    fn report_has_critical() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_event("dup", 1, 0, 1000, "a"),
            make_event("dup", 1, 1, 1001, "b"),
        ];
        let report = checker.check(&events);
        assert!(report.has_critical());
    }

    #[test]
    fn report_domain_count() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let mut e_egress = make_event("e2", 1, 0, 1001, "b");
        e_egress.payload = RecorderEventPayload::EgressOutput {
            text: "output".into(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
        };
        let events = vec![
            make_event("e1", 1, 0, 1000, "a"), // pane 1, ingress
            e_egress,                           // pane 1, egress
        ];
        let report = checker.check(&events);
        assert_eq!(report.panes_observed, 1);
        assert_eq!(report.domains_observed, 2); // ingress + egress
    }

    // -- Replay determinism tests --

    #[test]
    fn replay_determinism_identical_events() {
        let events = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_event("e2", 1, 1, 1001, "b"),
            make_event("e3", 2, 0, 1002, "c"),
        ];
        let result = verify_replay_determinism(&events, &events);
        assert!(result.deterministic);
        assert!(result.divergence_index.is_none());
    }

    #[test]
    fn replay_determinism_same_content_different_order() {
        let events_a = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_event("e2", 2, 0, 1001, "b"),
        ];
        let events_b = vec![
            make_event("e2", 2, 0, 1001, "b"),
            make_event("e1", 1, 0, 1000, "a"),
        ];
        // Same events in different order should still sort identically
        let result = verify_replay_determinism(&events_a, &events_b);
        assert!(result.deterministic);
    }

    #[test]
    fn replay_determinism_length_mismatch() {
        let events_a = vec![make_event("e1", 1, 0, 1000, "a")];
        let events_b = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_event("e2", 1, 1, 1001, "b"),
        ];
        let result = verify_replay_determinism(&events_a, &events_b);
        assert!(!result.deterministic);
        assert_eq!(result.divergence_index, Some(1));
    }

    #[test]
    fn replay_determinism_content_divergence() {
        let events_a = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_event("e2", 1, 1, 1001, "b"),
        ];
        let events_b = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_event("e3", 1, 1, 1001, "c"), // different event_id
        ];
        let result = verify_replay_determinism(&events_a, &events_b);
        assert!(!result.deterministic);
    }

    // -- Negative test: intentionally malformed data --

    #[test]
    fn multiple_violations_in_single_check() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            expected_schema_version: "ft.recorder.event.v1".to_string(),
            clock_future_skew_threshold_ms: 1_000,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);

        let mut bad_schema = make_event("e1", 1, 0, 1000, "a");
        bad_schema.schema_version = "wrong".into();

        let mut bad_ref = make_event("e2", 1, 1, 1001, "b");
        bad_ref.causality.parent_event_id = Some("ghost".into());

        let dup_seq = make_event("e3", 1, 1, 2000, "c"); // dup seq with e2
        let dup_id = make_event("e2", 1, 2, 3000, "d"); // dup id with e2

        let events = vec![bad_schema, bad_ref, dup_seq, dup_id];
        let report = checker.check(&events);
        assert!(!report.passed);

        // Should detect: schema mismatch(1), dangling parent(1), dup seq(1), dup id(1)
        assert!(report.count_by_kind(ViolationKind::SchemaVersionMismatch) >= 1);
        assert_eq!(report.count_by_kind(ViolationKind::DanglingParentRef), 1);
        assert_eq!(report.count_by_kind(ViolationKind::DuplicateSequence), 1);
        assert_eq!(report.count_by_kind(ViolationKind::DuplicateEventId), 1);
    }

    // -- Lifecycle and control marker variants --

    fn make_lifecycle(
        id: &str,
        pane_id: u64,
        seq: u64,
        ts: u64,
        phase: RecorderLifecyclePhase,
    ) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: id.to_string(),
            pane_id,
            session_id: Some("s1".into()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: ts,
            recorded_at_ms: ts + 1,
            sequence: seq,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::LifecycleMarker {
                lifecycle_phase: phase,
                reason: None,
                details: serde_json::Value::Null,
            },
        }
    }

    fn make_control(
        id: &str,
        pane_id: u64,
        seq: u64,
        ts: u64,
        marker: RecorderControlMarkerType,
    ) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: id.to_string(),
            pane_id,
            session_id: Some("s1".into()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: ts,
            recorded_at_ms: ts + 1,
            sequence: seq,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::ControlMarker {
                control_marker_type: marker,
                details: serde_json::Value::Null,
            },
        }
    }

    #[test]
    fn lifecycle_and_control_events_independent_domains() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        // Same pane, same sequence number, but different stream kinds → no conflict
        let events = vec![
            make_lifecycle("lc1", 1, 0, 1000, RecorderLifecyclePhase::CaptureStarted),
            make_control("ct1", 1, 0, 1001, RecorderControlMarkerType::PromptBoundary),
            make_event("ig1", 1, 0, 1002, "input"),
            make_egress_gap("eg1", 1, 0, 1003),
        ];
        let report = checker.check(&events);
        assert!(report.passed);
        assert_eq!(report.panes_observed, 1);
        assert_eq!(report.domains_observed, 4); // lifecycle, control, ingress, egress
        assert_eq!(report.count_by_kind(ViolationKind::DuplicateSequence), 0);
    }

    #[test]
    fn lifecycle_sequence_regression_in_same_stream() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_lifecycle("lc1", 1, 5, 1000, RecorderLifecyclePhase::CaptureStarted),
            make_lifecycle("lc2", 1, 3, 1001, RecorderLifecyclePhase::CaptureStopped),
        ];
        let report = checker.check(&events);
        assert!(!report.passed);
        assert_eq!(report.count_by_kind(ViolationKind::SequenceRegression), 1);
    }

    // -- Gap marker in correct stream domain --

    fn make_egress_delta(
        id: &str,
        pane_id: u64,
        seq: u64,
        ts: u64,
        text: &str,
    ) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: id.to_string(),
            pane_id,
            session_id: Some("s1".into()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: ts,
            recorded_at_ms: ts + 1,
            sequence: seq,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::EgressOutput {
                text: text.into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            },
        }
    }

    #[test]
    fn gap_marker_suppresses_in_same_egress_stream() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        // egress delta seq=0, then gap marker at seq=5 → gap suppressed
        let events = vec![
            make_egress_delta("ed1", 1, 0, 1000, "output"),
            make_egress_gap("eg1", 1, 5, 1001),
        ];
        let report = checker.check(&events);
        assert!(report.passed);
        assert_eq!(report.count_by_kind(ViolationKind::SequenceGap), 0);
    }

    #[test]
    fn gap_in_egress_without_marker_is_warning() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            max_sequence_gap: 100,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        // egress delta seq=0, then delta seq=5 (no gap marker) → warning
        let events = vec![
            make_egress_delta("ed1", 1, 0, 1000, "first"),
            make_egress_delta("ed2", 1, 5, 1001, "second"),
        ];
        let report = checker.check(&events);
        assert!(report.passed); // small gap → warning, not error
        assert_eq!(report.count_by_kind(ViolationKind::SequenceGap), 1);
    }

    // -- Forward-referencing causality --

    #[test]
    fn forward_reference_parent_detected_as_dangling() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        // e1 references e2 as parent, but e2 comes after e1 in the log
        let mut e1 = make_event("e1", 1, 0, 1000, "a");
        e1.causality.parent_event_id = Some("e2".into());
        let events = vec![e1, make_event("e2", 1, 1, 1001, "b")];
        let report = checker.check(&events);
        // Forward references are detected as dangling (not yet seen)
        assert_eq!(report.count_by_kind(ViolationKind::DanglingParentRef), 1);
    }

    // -- Empty causality strings --

    #[test]
    fn empty_string_causality_refs_ignored() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let mut event = make_event("e1", 1, 0, 1000, "a");
        event.causality.parent_event_id = Some(String::new());
        event.causality.trigger_event_id = Some(String::new());
        event.causality.root_event_id = Some(String::new());
        let report = checker.check(&[event]);
        // Empty strings should be ignored, not flagged as dangling
        assert_eq!(report.count_by_kind(ViolationKind::DanglingParentRef), 0);
        assert_eq!(report.count_by_kind(ViolationKind::DanglingTriggerRef), 0);
        assert_eq!(report.count_by_kind(ViolationKind::DanglingRootRef), 0);
    }

    // -- Violation event_index accuracy --

    #[test]
    fn violation_event_index_matches_input_position() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_event("e2", 1, 1, 1001, "b"),
            make_event("e3", 1, 5, 1002, "c"), // gap at index 2
        ];
        let report = checker.check(&events);
        let gap_violations: Vec<&Violation> = report
            .violations
            .iter()
            .filter(|v| v.kind == ViolationKind::SequenceGap)
            .collect();
        assert_eq!(gap_violations.len(), 1);
        assert_eq!(gap_violations[0].event_index, 2);
        assert_eq!(gap_violations[0].pane_id, 1);
    }

    // -- Large interleaved multi-pane --

    #[test]
    fn large_interleaved_multi_pane_passes() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let mut events = Vec::new();
        // 10 panes, 100 events each, interleaved
        for seq in 0u64..100 {
            for pane in 0u64..10 {
                let id = format!("e-p{}-s{}", pane, seq);
                let ts = 1000 + seq * 10 + pane;
                events.push(make_event(&id, pane, seq, ts, "data"));
            }
        }
        let report = checker.check(&events);
        assert!(report.passed);
        assert_eq!(report.events_checked, 1000);
        assert_eq!(report.panes_observed, 10);
        assert!(report.violations.is_empty());
    }

    // -- Default config smoke test --

    #[test]
    fn default_config_end_to_end() {
        let checker = InvariantChecker::default();
        let events = vec![
            make_event("e1", 1, 0, 1000, "a"),
            make_event("e2", 1, 1, 1001, "b"),
            make_event("e3", 2, 0, 1002, "c"),
        ];
        let report = checker.check(&events);
        assert!(report.passed);
        assert!(!report.has_critical());
        assert!(!report.has_errors());
    }

    // -- All trigger + root causality valid --

    #[test]
    fn full_causal_chain_with_trigger_and_root() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let e1 = make_event("root", 1, 0, 1000, "root event");
        let mut e2 = make_event("trigger", 1, 1, 1001, "trigger event");
        e2.causality.root_event_id = Some("root".into());
        let mut e3 = make_event("child", 1, 2, 1002, "child event");
        e3.causality.parent_event_id = Some("trigger".into());
        e3.causality.trigger_event_id = Some("trigger".into());
        e3.causality.root_event_id = Some("root".into());
        let report = checker.check(&[e1, e2, e3]);
        assert!(report.passed);
        assert!(report.violations.is_empty());
    }

    // -- Consecutive sequence (no gap) --

    #[test]
    fn consecutive_sequences_no_gap() {
        let config = InvariantCheckerConfig {
            check_merge_order: false,
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let events: Vec<RecorderEvent> = (0u64..50)
            .map(|i| make_event(&format!("e{}", i), 1, i, 1000 + i, "data"))
            .collect();
        let report = checker.check(&events);
        assert!(report.passed);
        assert!(report.violations.is_empty());
    }

    // -- Replay determinism with empty sequences --

    #[test]
    fn replay_determinism_empty_sequences() {
        let result = verify_replay_determinism(&[], &[]);
        assert!(result.deterministic);
    }
}
