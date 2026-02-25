//! Structured mission event taxonomy (ft-1i2ge.6.2).
//!
//! Defines and emits structured events across plan/dispatch/reconcile/safety
//! paths in the mission subsystem. Each event carries:
//!
//! - A stable `event_kind` discriminant for filtering and routing.
//! - A `reason_code` explaining *why* the event happened.
//! - A `correlation_id` linking events within the same evaluation cycle.
//! - Phase-specific payloads for downstream telemetry and audit.
//!
//! Events are collected into a bounded `MissionEventLog` that the caller
//! can drain after each cycle for persistence, forwarding, or display.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Event kind taxonomy ─────────────────────────────────────────────────────

/// Top-level mission event kind.
///
/// Each variant maps to a specific phase of the mission pipeline.
/// The `snake_case` serde representation is the canonical wire format
/// (e.g. `"readiness_resolved"`, `"assignment_emitted"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionEventKind {
    // ── Plan phase ──────────────────────────────────────────────────────
    /// Readiness resolution completed for the bead set.
    ReadinessResolved,
    /// Feature extraction completed for candidate beads.
    FeaturesExtracted,
    /// Candidate scoring completed.
    ScoringCompleted,
    /// Assignment solver produced an assignment set.
    AssignmentsSolved,

    // ── Safety phase ────────────────────────────────────────────────────
    /// Safety envelope enforcement applied to assignment set.
    SafetyEnvelopeApplied,
    /// A candidate was rejected by a safety gate.
    SafetyGateRejection,
    /// A retry storm was detected and throttled.
    RetryStormThrottled,

    // ── Dispatch phase ──────────────────────────────────────────────────
    /// An assignment was emitted for dispatch.
    AssignmentEmitted,
    /// An assignment was rejected (conflict, policy, or solver).
    AssignmentRejected,

    // ── Reconcile phase ─────────────────────────────────────────────────
    /// A conflict was detected between assignments/reservations.
    ConflictDetected,
    /// A conflict was auto-resolved by the deconfliction strategy.
    ConflictAutoResolved,
    /// A conflict requires manual operator resolution.
    ConflictPendingManual,
    /// Unblock transitions detected (beads moved from blocked to ready).
    UnblockTransitionDetected,
    /// Planner churn detected (assignment agent changed between cycles).
    PlannerChurnDetected,

    // ── Lifecycle ───────────────────────────────────────────────────────
    /// A mission evaluation cycle started.
    CycleStarted,
    /// A mission evaluation cycle completed.
    CycleCompleted,
    /// A trigger was enqueued for evaluation.
    TriggerEnqueued,
    /// Metrics sample was recorded.
    MetricsSampleRecorded,
}

impl MissionEventKind {
    /// Return the pipeline phase this event belongs to.
    #[must_use]
    pub fn phase(&self) -> MissionPhase {
        match self {
            Self::ReadinessResolved
            | Self::FeaturesExtracted
            | Self::ScoringCompleted
            | Self::AssignmentsSolved => MissionPhase::Plan,

            Self::SafetyEnvelopeApplied
            | Self::SafetyGateRejection
            | Self::RetryStormThrottled => MissionPhase::Safety,

            Self::AssignmentEmitted | Self::AssignmentRejected => MissionPhase::Dispatch,

            Self::ConflictDetected
            | Self::ConflictAutoResolved
            | Self::ConflictPendingManual
            | Self::UnblockTransitionDetected
            | Self::PlannerChurnDetected => MissionPhase::Reconcile,

            Self::CycleStarted
            | Self::CycleCompleted
            | Self::TriggerEnqueued
            | Self::MetricsSampleRecorded => MissionPhase::Lifecycle,
        }
    }
}

/// Pipeline phase grouping for events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionPhase {
    Plan,
    Safety,
    Dispatch,
    Reconcile,
    Lifecycle,
}

// ── Reason codes ────────────────────────────────────────────────────────────

/// Stable reason codes explaining why an event occurred.
///
/// Format: `mission.<phase>.<detail>` for grep-friendliness.
/// These are intentionally string constants (not an enum) so new codes
/// can be added without breaking downstream match arms.
pub mod reason_codes {
    // Plan phase
    pub const READINESS_NOMINAL: &str = "mission.plan.readiness_nominal";
    pub const READINESS_EMPTY: &str = "mission.plan.readiness_empty";
    pub const EXTRACTION_NOMINAL: &str = "mission.plan.extraction_nominal";
    pub const EXTRACTION_NO_CANDIDATES: &str = "mission.plan.extraction_no_candidates";
    pub const SCORING_NOMINAL: &str = "mission.plan.scoring_nominal";
    pub const SCORING_ALL_BELOW_THRESHOLD: &str = "mission.plan.scoring_all_below_threshold";
    pub const SOLVER_NOMINAL: &str = "mission.plan.solver_nominal";
    pub const SOLVER_NO_ASSIGNMENTS: &str = "mission.plan.solver_no_assignments";

    // Safety phase
    pub const ENVELOPE_PASS: &str = "mission.safety.envelope_pass";
    pub const ENVELOPE_TRIMMED: &str = "mission.safety.envelope_trimmed";
    pub const GATE_MAX_ASSIGNMENTS: &str = "mission.safety.gate_max_assignments";
    pub const GATE_MAX_RISKY: &str = "mission.safety.gate_max_risky";
    pub const GATE_RETRY_STORM: &str = "mission.safety.gate_retry_storm";

    // Dispatch phase
    pub const ASSIGNMENT_EMITTED: &str = "mission.dispatch.assignment_emitted";
    pub const REJECTION_CONFLICT: &str = "mission.dispatch.rejection_conflict";
    pub const REJECTION_SAFETY: &str = "mission.dispatch.rejection_safety";
    pub const REJECTION_SOLVER: &str = "mission.dispatch.rejection_solver";

    // Reconcile phase
    pub const CONFLICT_RESERVATION_OVERLAP: &str = "mission.reconcile.reservation_overlap";
    pub const CONFLICT_CONCURRENT_CLAIM: &str = "mission.reconcile.concurrent_claim";
    pub const CONFLICT_ACTIVE_COLLISION: &str = "mission.reconcile.active_collision";
    pub const CONFLICT_AUTO_RESOLVED: &str = "mission.reconcile.auto_resolved";
    pub const CONFLICT_PENDING_MANUAL: &str = "mission.reconcile.pending_manual";
    pub const UNBLOCK_DETECTED: &str = "mission.reconcile.unblock_detected";
    pub const CHURN_DETECTED: &str = "mission.reconcile.churn_detected";

    // Lifecycle
    pub const CYCLE_STARTED: &str = "mission.lifecycle.cycle_started";
    pub const CYCLE_COMPLETED: &str = "mission.lifecycle.cycle_completed";
    pub const TRIGGER_ENQUEUED: &str = "mission.lifecycle.trigger_enqueued";
    pub const METRICS_RECORDED: &str = "mission.lifecycle.metrics_recorded";
}

// ── Event payload ───────────────────────────────────────────────────────────

/// A structured mission event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionEvent {
    /// Monotonically increasing sequence within the event log.
    pub sequence: u64,
    /// Evaluation cycle this event belongs to.
    pub cycle_id: u64,
    /// Millisecond timestamp of the event.
    pub timestamp_ms: i64,
    /// The event kind discriminant.
    pub kind: MissionEventKind,
    /// Stable reason code explaining why this event occurred.
    pub reason_code: String,
    /// Correlation ID linking events within the same evaluation cycle.
    pub correlation_id: String,
    /// Pipeline phase (derived from kind, included for filtering convenience).
    pub phase: MissionPhase,
    /// Event-specific structured payload.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub details: HashMap<String, serde_json::Value>,
    /// Workspace label for segmentation.
    pub workspace: String,
    /// Track label for segmentation.
    pub track: String,
}

impl MissionEvent {
    /// Build a new event (use `MissionEventBuilder` for ergonomic construction).
    #[must_use]
    fn new(
        sequence: u64,
        cycle_id: u64,
        timestamp_ms: i64,
        kind: MissionEventKind,
        reason_code: &str,
        correlation_id: &str,
        workspace: &str,
        track: &str,
    ) -> Self {
        let phase = kind.phase();
        Self {
            sequence,
            cycle_id,
            timestamp_ms,
            kind,
            reason_code: reason_code.to_string(),
            correlation_id: correlation_id.to_string(),
            phase,
            details: HashMap::new(),
            workspace: workspace.to_string(),
            track: track.to_string(),
        }
    }
}

// ── Builder ─────────────────────────────────────────────────────────────────

/// Ergonomic builder for `MissionEvent`.
pub struct MissionEventBuilder {
    cycle_id: u64,
    timestamp_ms: i64,
    kind: MissionEventKind,
    reason_code: String,
    correlation_id: String,
    workspace: String,
    track: String,
    details: HashMap<String, serde_json::Value>,
}

impl MissionEventBuilder {
    /// Start building an event.
    #[must_use]
    pub fn new(kind: MissionEventKind, reason_code: &str) -> Self {
        Self {
            cycle_id: 0,
            timestamp_ms: 0,
            kind,
            reason_code: reason_code.to_string(),
            correlation_id: String::new(),
            workspace: String::new(),
            track: String::new(),
            details: HashMap::new(),
        }
    }

    /// Set the cycle context.
    #[must_use]
    pub fn cycle(mut self, cycle_id: u64, timestamp_ms: i64) -> Self {
        self.cycle_id = cycle_id;
        self.timestamp_ms = timestamp_ms;
        self
    }

    /// Set the correlation ID.
    #[must_use]
    pub fn correlation(mut self, id: &str) -> Self {
        self.correlation_id = id.to_string();
        self
    }

    /// Set workspace and track labels.
    #[must_use]
    pub fn labels(mut self, workspace: &str, track: &str) -> Self {
        self.workspace = workspace.to_string();
        self.track = track.to_string();
        self
    }

    /// Add a string detail.
    #[must_use]
    pub fn detail_str(mut self, key: &str, value: &str) -> Self {
        self.details
            .insert(key.to_string(), serde_json::Value::String(value.to_string()));
        self
    }

    /// Add a numeric detail (u64).
    #[must_use]
    pub fn detail_u64(mut self, key: &str, value: u64) -> Self {
        self.details
            .insert(key.to_string(), serde_json::Value::Number(value.into()));
        self
    }

    /// Add a float detail (f64).
    #[must_use]
    pub fn detail_f64(mut self, key: &str, value: f64) -> Self {
        self.details.insert(
            key.to_string(),
            serde_json::Number::from_f64(value)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
        );
        self
    }

    /// Add a boolean detail.
    #[must_use]
    pub fn detail_bool(mut self, key: &str, value: bool) -> Self {
        self.details
            .insert(key.to_string(), serde_json::Value::Bool(value));
        self
    }

    /// Add a string list detail.
    #[must_use]
    pub fn detail_strings(mut self, key: &str, values: &[String]) -> Self {
        let arr: Vec<serde_json::Value> = values
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect();
        self.details
            .insert(key.to_string(), serde_json::Value::Array(arr));
        self
    }

    /// Finalize the event (sequence assigned by the log).
    fn build(self, sequence: u64) -> MissionEvent {
        let mut event = MissionEvent::new(
            sequence,
            self.cycle_id,
            self.timestamp_ms,
            self.kind,
            &self.reason_code,
            &self.correlation_id,
            &self.workspace,
            &self.track,
        );
        event.details = self.details;
        event
    }
}

// ── Event log ───────────────────────────────────────────────────────────────

/// Configuration for the mission event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionEventLogConfig {
    /// Maximum events retained in the log (FIFO eviction).
    pub max_events: usize,
    /// Whether event emission is enabled.
    pub enabled: bool,
}

impl Default for MissionEventLogConfig {
    fn default() -> Self {
        Self {
            max_events: 1024,
            enabled: true,
        }
    }
}

/// Bounded, append-only event log for mission events.
///
/// Events are assigned monotonically increasing sequence numbers.
/// When the log exceeds `max_events`, the oldest events are evicted (FIFO).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionEventLog {
    config: MissionEventLogConfig,
    events: Vec<MissionEvent>,
    next_sequence: u64,
    /// Total events ever appended (including evicted).
    total_appended: u64,
    /// Total events evicted due to capacity limits.
    total_evicted: u64,
}

impl MissionEventLog {
    /// Create a new event log with the given configuration.
    #[must_use]
    pub fn new(config: MissionEventLogConfig) -> Self {
        Self {
            config,
            events: Vec::new(),
            next_sequence: 1,
            total_appended: 0,
            total_evicted: 0,
        }
    }

    /// Emit an event into the log.
    ///
    /// Returns the assigned sequence number, or `None` if emission is disabled.
    pub fn emit(&mut self, builder: MissionEventBuilder) -> Option<u64> {
        if !self.config.enabled {
            return None;
        }
        let seq = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let event = builder.build(seq);
        let max = self.config.max_events.max(1);
        if self.events.len() >= max {
            self.events.remove(0);
            self.total_evicted = self.total_evicted.saturating_add(1);
        }
        self.events.push(event);
        self.total_appended = self.total_appended.saturating_add(1);
        Some(seq)
    }

    /// Number of events currently in the log.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Get all events currently in the log.
    #[must_use]
    pub fn events(&self) -> &[MissionEvent] {
        &self.events
    }

    /// Get the most recent event.
    #[must_use]
    pub fn latest(&self) -> Option<&MissionEvent> {
        self.events.last()
    }

    /// Total events ever appended (including evicted).
    #[must_use]
    pub fn total_appended(&self) -> u64 {
        self.total_appended
    }

    /// Total events evicted.
    #[must_use]
    pub fn total_evicted(&self) -> u64 {
        self.total_evicted
    }

    /// Drain events matching a filter predicate.
    pub fn drain_matching<F>(&mut self, predicate: F) -> Vec<MissionEvent>
    where
        F: Fn(&MissionEvent) -> bool,
    {
        let mut drained = Vec::new();
        self.events.retain(|event| {
            if predicate(event) {
                drained.push(event.clone());
                false
            } else {
                true
            }
        });
        drained
    }

    /// Get events filtered by phase.
    #[must_use]
    pub fn events_by_phase(&self, phase: MissionPhase) -> Vec<&MissionEvent> {
        self.events.iter().filter(|e| e.phase == phase).collect()
    }

    /// Get events filtered by cycle.
    #[must_use]
    pub fn events_by_cycle(&self, cycle_id: u64) -> Vec<&MissionEvent> {
        self.events
            .iter()
            .filter(|e| e.cycle_id == cycle_id)
            .collect()
    }

    /// Get events filtered by kind.
    #[must_use]
    pub fn events_by_kind(&self, kind: &MissionEventKind) -> Vec<&MissionEvent> {
        self.events.iter().filter(|e| &e.kind == kind).collect()
    }

    /// Count events by kind across all retained events.
    #[must_use]
    pub fn count_by_kind(&self) -> HashMap<MissionEventKind, usize> {
        let mut counts = HashMap::new();
        for event in &self.events {
            *counts.entry(event.kind.clone()).or_insert(0) += 1;
        }
        counts
    }

    /// Count events by phase.
    #[must_use]
    pub fn count_by_phase(&self) -> HashMap<MissionPhase, usize> {
        let mut counts = HashMap::new();
        for event in &self.events {
            *counts.entry(event.phase).or_insert(0) += 1;
        }
        counts
    }

    /// Summary snapshot for telemetry export.
    #[must_use]
    pub fn summary(&self) -> MissionEventLogSummary {
        MissionEventLogSummary {
            retained_count: self.events.len(),
            total_appended: self.total_appended,
            total_evicted: self.total_evicted,
            next_sequence: self.next_sequence,
            by_phase: self.count_by_phase(),
            by_kind: self.count_by_kind(),
        }
    }

    /// Clear all events (for testing or reset).
    pub fn clear(&mut self) {
        self.events.clear();
    }
}

/// Summary snapshot of the event log for export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionEventLogSummary {
    pub retained_count: usize,
    pub total_appended: u64,
    pub total_evicted: u64,
    pub next_sequence: u64,
    pub by_phase: HashMap<MissionPhase, usize>,
    pub by_kind: HashMap<MissionEventKind, usize>,
}

// ── Cycle event emitter (convenience) ───────────────────────────────────────

/// Convenience struct for emitting events during a single evaluation cycle.
///
/// Binds the cycle context (id, timestamp, correlation, labels) so individual
/// emit calls only need kind, reason, and optional details.
pub struct CycleEventEmitter<'a> {
    log: &'a mut MissionEventLog,
    cycle_id: u64,
    timestamp_ms: i64,
    correlation_id: String,
    workspace: String,
    track: String,
}

impl<'a> CycleEventEmitter<'a> {
    /// Create a new emitter bound to a cycle.
    pub fn new(
        log: &'a mut MissionEventLog,
        cycle_id: u64,
        timestamp_ms: i64,
        correlation_id: &str,
        workspace: &str,
        track: &str,
    ) -> Self {
        Self {
            log,
            cycle_id,
            timestamp_ms,
            correlation_id: correlation_id.to_string(),
            workspace: workspace.to_string(),
            track: track.to_string(),
        }
    }

    /// Emit a simple event with no extra details.
    pub fn emit(&mut self, kind: MissionEventKind, reason_code: &str) -> Option<u64> {
        let builder = MissionEventBuilder::new(kind, reason_code)
            .cycle(self.cycle_id, self.timestamp_ms)
            .correlation(&self.correlation_id)
            .labels(&self.workspace, &self.track);
        self.log.emit(builder)
    }

    /// Emit an event with a pre-configured builder (for adding details).
    pub fn emit_builder(&mut self, builder: MissionEventBuilder) -> Option<u64> {
        let builder = builder
            .cycle(self.cycle_id, self.timestamp_ms)
            .correlation(&self.correlation_id)
            .labels(&self.workspace, &self.track);
        self.log.emit(builder)
    }

    /// Emit cycle-started event.
    pub fn emit_cycle_started(&mut self, trigger_kind: &str) -> Option<u64> {
        let builder = MissionEventBuilder::new(
            MissionEventKind::CycleStarted,
            reason_codes::CYCLE_STARTED,
        )
        .detail_str("trigger_kind", trigger_kind);
        self.emit_builder(builder)
    }

    /// Emit readiness-resolved event.
    pub fn emit_readiness_resolved(
        &mut self,
        total_candidates: usize,
        ready_count: usize,
    ) -> Option<u64> {
        let reason = if ready_count > 0 {
            reason_codes::READINESS_NOMINAL
        } else {
            reason_codes::READINESS_EMPTY
        };
        let builder = MissionEventBuilder::new(MissionEventKind::ReadinessResolved, reason)
            .detail_u64("total_candidates", total_candidates as u64)
            .detail_u64("ready_count", ready_count as u64);
        self.emit_builder(builder)
    }

    /// Emit features-extracted event.
    pub fn emit_features_extracted(
        &mut self,
        feature_count: usize,
        top_impact_bead: Option<&str>,
    ) -> Option<u64> {
        let reason = if feature_count > 0 {
            reason_codes::EXTRACTION_NOMINAL
        } else {
            reason_codes::EXTRACTION_NO_CANDIDATES
        };
        let mut builder = MissionEventBuilder::new(MissionEventKind::FeaturesExtracted, reason)
            .detail_u64("feature_count", feature_count as u64);
        if let Some(bead) = top_impact_bead {
            builder = builder.detail_str("top_impact_bead", bead);
        }
        self.emit_builder(builder)
    }

    /// Emit scoring-completed event.
    pub fn emit_scoring_completed(
        &mut self,
        scored_count: usize,
        above_threshold: usize,
        top_scored_bead: Option<&str>,
    ) -> Option<u64> {
        let reason = if above_threshold > 0 {
            reason_codes::SCORING_NOMINAL
        } else {
            reason_codes::SCORING_ALL_BELOW_THRESHOLD
        };
        let mut builder = MissionEventBuilder::new(MissionEventKind::ScoringCompleted, reason)
            .detail_u64("scored_count", scored_count as u64)
            .detail_u64("above_threshold", above_threshold as u64);
        if let Some(bead) = top_scored_bead {
            builder = builder.detail_str("top_scored_bead", bead);
        }
        self.emit_builder(builder)
    }

    /// Emit assignments-solved event.
    pub fn emit_assignments_solved(
        &mut self,
        assigned_count: usize,
        rejected_count: usize,
    ) -> Option<u64> {
        let reason = if assigned_count > 0 {
            reason_codes::SOLVER_NOMINAL
        } else {
            reason_codes::SOLVER_NO_ASSIGNMENTS
        };
        let builder = MissionEventBuilder::new(MissionEventKind::AssignmentsSolved, reason)
            .detail_u64("assigned_count", assigned_count as u64)
            .detail_u64("rejected_count", rejected_count as u64);
        self.emit_builder(builder)
    }

    /// Emit safety-envelope-applied event.
    pub fn emit_safety_envelope_applied(
        &mut self,
        kept_count: usize,
        trimmed_count: usize,
    ) -> Option<u64> {
        let reason = if trimmed_count > 0 {
            reason_codes::ENVELOPE_TRIMMED
        } else {
            reason_codes::ENVELOPE_PASS
        };
        let builder = MissionEventBuilder::new(MissionEventKind::SafetyEnvelopeApplied, reason)
            .detail_u64("kept_count", kept_count as u64)
            .detail_u64("trimmed_count", trimmed_count as u64);
        self.emit_builder(builder)
    }

    /// Emit safety-gate-rejection for a specific gate and bead.
    pub fn emit_safety_gate_rejection(
        &mut self,
        gate_name: &str,
        bead_id: &str,
        score: f64,
    ) -> Option<u64> {
        let reason = match gate_name {
            g if g.contains("max_assignments") => reason_codes::GATE_MAX_ASSIGNMENTS,
            g if g.contains("max_risky") => reason_codes::GATE_MAX_RISKY,
            g if g.contains("retry_storm") => reason_codes::GATE_RETRY_STORM,
            _ => "mission.safety.gate_unknown",
        };
        let builder = MissionEventBuilder::new(MissionEventKind::SafetyGateRejection, reason)
            .detail_str("gate_name", gate_name)
            .detail_str("bead_id", bead_id)
            .detail_f64("score", score);
        self.emit_builder(builder)
    }

    /// Emit assignment-emitted for a dispatched assignment.
    pub fn emit_assignment_emitted(
        &mut self,
        bead_id: &str,
        agent_id: &str,
        score: f64,
        rank: usize,
    ) -> Option<u64> {
        let builder = MissionEventBuilder::new(
            MissionEventKind::AssignmentEmitted,
            reason_codes::ASSIGNMENT_EMITTED,
        )
        .detail_str("bead_id", bead_id)
        .detail_str("agent_id", agent_id)
        .detail_f64("score", score)
        .detail_u64("rank", rank as u64);
        self.emit_builder(builder)
    }

    /// Emit assignment-rejected for a rejected candidate.
    pub fn emit_assignment_rejected(
        &mut self,
        bead_id: &str,
        reason_code: &str,
        reasons: &[String],
    ) -> Option<u64> {
        let builder =
            MissionEventBuilder::new(MissionEventKind::AssignmentRejected, reason_code)
                .detail_str("bead_id", bead_id)
                .detail_strings("rejection_reasons", reasons);
        self.emit_builder(builder)
    }

    /// Emit conflict-detected event.
    pub fn emit_conflict_detected(
        &mut self,
        conflict_id: &str,
        conflict_type: &str,
        involved_agents: &[String],
        involved_beads: &[String],
    ) -> Option<u64> {
        let reason = match conflict_type {
            "file_reservation_overlap" => reason_codes::CONFLICT_RESERVATION_OVERLAP,
            "concurrent_bead_claim" => reason_codes::CONFLICT_CONCURRENT_CLAIM,
            "active_claim_collision" => reason_codes::CONFLICT_ACTIVE_COLLISION,
            _ => "mission.reconcile.conflict_unknown",
        };
        let builder = MissionEventBuilder::new(MissionEventKind::ConflictDetected, reason)
            .detail_str("conflict_id", conflict_id)
            .detail_str("conflict_type", conflict_type)
            .detail_strings("involved_agents", involved_agents)
            .detail_strings("involved_beads", involved_beads);
        self.emit_builder(builder)
    }

    /// Emit cycle-completed event.
    pub fn emit_cycle_completed(
        &mut self,
        evaluation_latency_ms: u64,
        assignment_count: usize,
        rejection_count: usize,
    ) -> Option<u64> {
        let builder = MissionEventBuilder::new(
            MissionEventKind::CycleCompleted,
            reason_codes::CYCLE_COMPLETED,
        )
        .detail_u64("evaluation_latency_ms", evaluation_latency_ms)
        .detail_u64("assignment_count", assignment_count as u64)
        .detail_u64("rejection_count", rejection_count as u64);
        self.emit_builder(builder)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_log() -> MissionEventLog {
        MissionEventLog::new(MissionEventLogConfig::default())
    }

    fn small_log(max: usize) -> MissionEventLog {
        MissionEventLog::new(MissionEventLogConfig {
            max_events: max,
            enabled: true,
        })
    }

    fn disabled_log() -> MissionEventLog {
        MissionEventLog::new(MissionEventLogConfig {
            max_events: 100,
            enabled: false,
        })
    }

    fn sample_builder(kind: MissionEventKind, reason: &str) -> MissionEventBuilder {
        MissionEventBuilder::new(kind, reason)
            .cycle(1, 1000)
            .correlation("corr-001")
            .labels("ws", "trk")
    }

    // ── Event kind taxonomy tests ───────────────────────────────────────

    #[test]
    fn event_kind_phase_mapping_plan() {
        assert_eq!(
            MissionEventKind::ReadinessResolved.phase(),
            MissionPhase::Plan
        );
        assert_eq!(
            MissionEventKind::FeaturesExtracted.phase(),
            MissionPhase::Plan
        );
        assert_eq!(
            MissionEventKind::ScoringCompleted.phase(),
            MissionPhase::Plan
        );
        assert_eq!(
            MissionEventKind::AssignmentsSolved.phase(),
            MissionPhase::Plan
        );
    }

    #[test]
    fn event_kind_phase_mapping_safety() {
        assert_eq!(
            MissionEventKind::SafetyEnvelopeApplied.phase(),
            MissionPhase::Safety
        );
        assert_eq!(
            MissionEventKind::SafetyGateRejection.phase(),
            MissionPhase::Safety
        );
        assert_eq!(
            MissionEventKind::RetryStormThrottled.phase(),
            MissionPhase::Safety
        );
    }

    #[test]
    fn event_kind_phase_mapping_dispatch() {
        assert_eq!(
            MissionEventKind::AssignmentEmitted.phase(),
            MissionPhase::Dispatch
        );
        assert_eq!(
            MissionEventKind::AssignmentRejected.phase(),
            MissionPhase::Dispatch
        );
    }

    #[test]
    fn event_kind_phase_mapping_reconcile() {
        assert_eq!(
            MissionEventKind::ConflictDetected.phase(),
            MissionPhase::Reconcile
        );
        assert_eq!(
            MissionEventKind::ConflictAutoResolved.phase(),
            MissionPhase::Reconcile
        );
        assert_eq!(
            MissionEventKind::ConflictPendingManual.phase(),
            MissionPhase::Reconcile
        );
        assert_eq!(
            MissionEventKind::UnblockTransitionDetected.phase(),
            MissionPhase::Reconcile
        );
        assert_eq!(
            MissionEventKind::PlannerChurnDetected.phase(),
            MissionPhase::Reconcile
        );
    }

    #[test]
    fn event_kind_phase_mapping_lifecycle() {
        assert_eq!(
            MissionEventKind::CycleStarted.phase(),
            MissionPhase::Lifecycle
        );
        assert_eq!(
            MissionEventKind::CycleCompleted.phase(),
            MissionPhase::Lifecycle
        );
        assert_eq!(
            MissionEventKind::TriggerEnqueued.phase(),
            MissionPhase::Lifecycle
        );
        assert_eq!(
            MissionEventKind::MetricsSampleRecorded.phase(),
            MissionPhase::Lifecycle
        );
    }

    // ── Builder tests ───────────────────────────────────────────────────

    #[test]
    fn builder_produces_correct_event() {
        let mut log = default_log();
        let builder = MissionEventBuilder::new(
            MissionEventKind::CycleStarted,
            reason_codes::CYCLE_STARTED,
        )
        .cycle(5, 5000)
        .correlation("corr-abc")
        .labels("my-ws", "my-trk")
        .detail_str("trigger_kind", "cadence_tick")
        .detail_u64("pending_triggers", 3)
        .detail_bool("forced", false);
        let seq = log.emit(builder).unwrap();
        assert_eq!(seq, 1);

        let event = log.latest().unwrap();
        assert_eq!(event.sequence, 1);
        assert_eq!(event.cycle_id, 5);
        assert_eq!(event.timestamp_ms, 5000);
        assert_eq!(event.kind, MissionEventKind::CycleStarted);
        assert_eq!(event.reason_code, reason_codes::CYCLE_STARTED);
        assert_eq!(event.correlation_id, "corr-abc");
        assert_eq!(event.phase, MissionPhase::Lifecycle);
        assert_eq!(event.workspace, "my-ws");
        assert_eq!(event.track, "my-trk");
        assert_eq!(
            event.details.get("trigger_kind"),
            Some(&serde_json::Value::String("cadence_tick".to_string()))
        );
        assert_eq!(
            event.details.get("pending_triggers"),
            Some(&serde_json::Value::Number(3.into()))
        );
        assert_eq!(
            event.details.get("forced"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn builder_detail_f64_non_finite_becomes_null() {
        let mut log = default_log();
        let builder = sample_builder(
            MissionEventKind::ScoringCompleted,
            reason_codes::SCORING_NOMINAL,
        )
        .detail_f64("nan_value", f64::NAN);
        log.emit(builder);
        let event = log.latest().unwrap();
        assert_eq!(
            event.details.get("nan_value"),
            Some(&serde_json::Value::Null)
        );
    }

    #[test]
    fn builder_detail_strings_empty() {
        let mut log = default_log();
        let builder = sample_builder(
            MissionEventKind::ConflictDetected,
            reason_codes::CONFLICT_RESERVATION_OVERLAP,
        )
        .detail_strings("agents", &[]);
        log.emit(builder);
        let event = log.latest().unwrap();
        assert_eq!(
            event.details.get("agents"),
            Some(&serde_json::Value::Array(vec![]))
        );
    }

    // ── Event log tests ─────────────────────────────────────────────────

    #[test]
    fn log_emit_increments_sequence() {
        let mut log = default_log();
        let s1 = log
            .emit(sample_builder(
                MissionEventKind::CycleStarted,
                reason_codes::CYCLE_STARTED,
            ))
            .unwrap();
        let s2 = log
            .emit(sample_builder(
                MissionEventKind::CycleCompleted,
                reason_codes::CYCLE_COMPLETED,
            ))
            .unwrap();
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(log.len(), 2);
        assert_eq!(log.total_appended(), 2);
        assert_eq!(log.total_evicted(), 0);
    }

    #[test]
    fn log_disabled_rejects_emit() {
        let mut log = disabled_log();
        let result = log.emit(sample_builder(
            MissionEventKind::CycleStarted,
            reason_codes::CYCLE_STARTED,
        ));
        assert!(result.is_none());
        assert!(log.is_empty());
        assert_eq!(log.total_appended(), 0);
    }

    #[test]
    fn log_fifo_eviction_at_capacity() {
        let mut log = small_log(3);
        for i in 0..5 {
            log.emit(
                sample_builder(MissionEventKind::CycleStarted, reason_codes::CYCLE_STARTED)
                    .cycle(i as u64, (i * 1000) as i64),
            );
        }
        assert_eq!(log.len(), 3);
        assert_eq!(log.total_appended(), 5);
        assert_eq!(log.total_evicted(), 2);
        // Oldest events (cycle 0 and 1) should be evicted.
        let cycles: Vec<u64> = log.events().iter().map(|e| e.cycle_id).collect();
        assert_eq!(cycles, vec![2, 3, 4]);
    }

    #[test]
    fn log_max_events_one_never_grows_past_one() {
        let mut log = small_log(1);
        for i in 0..10 {
            log.emit(
                sample_builder(MissionEventKind::CycleStarted, reason_codes::CYCLE_STARTED)
                    .cycle(i as u64, 0),
            );
        }
        assert_eq!(log.len(), 1);
        assert_eq!(log.total_evicted(), 9);
        assert_eq!(log.events()[0].cycle_id, 9);
    }

    // ── Filter tests ────────────────────────────────────────────────────

    #[test]
    fn filter_by_phase() {
        let mut log = default_log();
        log.emit(sample_builder(
            MissionEventKind::CycleStarted,
            reason_codes::CYCLE_STARTED,
        ));
        log.emit(sample_builder(
            MissionEventKind::ReadinessResolved,
            reason_codes::READINESS_NOMINAL,
        ));
        log.emit(sample_builder(
            MissionEventKind::SafetyEnvelopeApplied,
            reason_codes::ENVELOPE_PASS,
        ));
        log.emit(sample_builder(
            MissionEventKind::CycleCompleted,
            reason_codes::CYCLE_COMPLETED,
        ));

        let lifecycle = log.events_by_phase(MissionPhase::Lifecycle);
        assert_eq!(lifecycle.len(), 2);
        let plan = log.events_by_phase(MissionPhase::Plan);
        assert_eq!(plan.len(), 1);
        let safety = log.events_by_phase(MissionPhase::Safety);
        assert_eq!(safety.len(), 1);
    }

    #[test]
    fn filter_by_cycle() {
        let mut log = default_log();
        log.emit(
            sample_builder(MissionEventKind::CycleStarted, reason_codes::CYCLE_STARTED)
                .cycle(1, 1000),
        );
        log.emit(
            sample_builder(MissionEventKind::CycleStarted, reason_codes::CYCLE_STARTED)
                .cycle(2, 2000),
        );
        log.emit(
            sample_builder(
                MissionEventKind::CycleCompleted,
                reason_codes::CYCLE_COMPLETED,
            )
            .cycle(2, 2100),
        );

        assert_eq!(log.events_by_cycle(1).len(), 1);
        assert_eq!(log.events_by_cycle(2).len(), 2);
        assert_eq!(log.events_by_cycle(99).len(), 0);
    }

    #[test]
    fn filter_by_kind() {
        let mut log = default_log();
        for _ in 0..3 {
            log.emit(sample_builder(
                MissionEventKind::AssignmentEmitted,
                reason_codes::ASSIGNMENT_EMITTED,
            ));
        }
        log.emit(sample_builder(
            MissionEventKind::AssignmentRejected,
            reason_codes::REJECTION_CONFLICT,
        ));

        assert_eq!(
            log.events_by_kind(&MissionEventKind::AssignmentEmitted)
                .len(),
            3
        );
        assert_eq!(
            log.events_by_kind(&MissionEventKind::AssignmentRejected)
                .len(),
            1
        );
    }

    // ── Count aggregation tests ─────────────────────────────────────────

    #[test]
    fn count_by_kind_and_phase() {
        let mut log = default_log();
        log.emit(sample_builder(
            MissionEventKind::CycleStarted,
            reason_codes::CYCLE_STARTED,
        ));
        log.emit(sample_builder(
            MissionEventKind::CycleStarted,
            reason_codes::CYCLE_STARTED,
        ));
        log.emit(sample_builder(
            MissionEventKind::AssignmentEmitted,
            reason_codes::ASSIGNMENT_EMITTED,
        ));

        let by_kind = log.count_by_kind();
        assert_eq!(by_kind.get(&MissionEventKind::CycleStarted), Some(&2));
        assert_eq!(by_kind.get(&MissionEventKind::AssignmentEmitted), Some(&1));

        let by_phase = log.count_by_phase();
        assert_eq!(by_phase.get(&MissionPhase::Lifecycle), Some(&2));
        assert_eq!(by_phase.get(&MissionPhase::Dispatch), Some(&1));
    }

    // ── Drain tests ─────────────────────────────────────────────────────

    #[test]
    fn drain_matching_removes_and_returns() {
        let mut log = default_log();
        log.emit(
            sample_builder(MissionEventKind::CycleStarted, reason_codes::CYCLE_STARTED)
                .cycle(1, 1000),
        );
        log.emit(
            sample_builder(
                MissionEventKind::AssignmentEmitted,
                reason_codes::ASSIGNMENT_EMITTED,
            )
            .cycle(1, 1100),
        );
        log.emit(
            sample_builder(MissionEventKind::CycleStarted, reason_codes::CYCLE_STARTED)
                .cycle(2, 2000),
        );

        let drained = log.drain_matching(|e| e.cycle_id == 1);
        assert_eq!(drained.len(), 2);
        assert_eq!(log.len(), 1);
        assert_eq!(log.events()[0].cycle_id, 2);
    }

    // ── Summary tests ───────────────────────────────────────────────────

    #[test]
    fn summary_reflects_log_state() {
        let mut log = small_log(5);
        for i in 0..8 {
            let kind = if i % 2 == 0 {
                MissionEventKind::CycleStarted
            } else {
                MissionEventKind::AssignmentEmitted
            };
            let reason = if i % 2 == 0 {
                reason_codes::CYCLE_STARTED
            } else {
                reason_codes::ASSIGNMENT_EMITTED
            };
            log.emit(sample_builder(kind, reason).cycle(i as u64, (i * 100) as i64));
        }

        let summary = log.summary();
        assert_eq!(summary.retained_count, 5);
        assert_eq!(summary.total_appended, 8);
        assert_eq!(summary.total_evicted, 3);
        assert_eq!(summary.next_sequence, 9);
        assert!(summary.by_kind.contains_key(&MissionEventKind::CycleStarted));
        assert!(summary
            .by_kind
            .contains_key(&MissionEventKind::AssignmentEmitted));
    }

    // ── Serde roundtrip tests ───────────────────────────────────────────

    #[test]
    fn event_serde_roundtrip() {
        let mut log = default_log();
        let builder = MissionEventBuilder::new(
            MissionEventKind::ConflictDetected,
            reason_codes::CONFLICT_RESERVATION_OVERLAP,
        )
        .cycle(3, 3000)
        .correlation("corr-x")
        .labels("ws-a", "trk-b")
        .detail_str("conflict_id", "c-001")
        .detail_u64("severity", 2)
        .detail_f64("overlap_ratio", 0.75)
        .detail_bool("auto_resolvable", true)
        .detail_strings("agents", &["a1".to_string(), "a2".to_string()]);
        log.emit(builder);

        let event = log.latest().unwrap();
        let json = serde_json::to_string(event).unwrap();
        let deserialized: MissionEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.sequence, event.sequence);
        assert_eq!(deserialized.cycle_id, event.cycle_id);
        assert_eq!(deserialized.timestamp_ms, event.timestamp_ms);
        assert_eq!(deserialized.kind, event.kind);
        assert_eq!(deserialized.reason_code, event.reason_code);
        assert_eq!(deserialized.correlation_id, event.correlation_id);
        assert_eq!(deserialized.phase, event.phase);
        assert_eq!(deserialized.workspace, event.workspace);
        assert_eq!(deserialized.track, event.track);
        assert_eq!(deserialized.details.len(), event.details.len());
    }

    #[test]
    fn event_log_serde_roundtrip() {
        let mut log = small_log(10);
        for i in 0..5 {
            log.emit(
                sample_builder(MissionEventKind::CycleStarted, reason_codes::CYCLE_STARTED)
                    .cycle(i as u64, (i * 1000) as i64),
            );
        }

        let json = serde_json::to_string(&log).unwrap();
        let deserialized: MissionEventLog = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.len(), log.len());
        assert_eq!(deserialized.total_appended(), log.total_appended());
        assert_eq!(deserialized.total_evicted(), log.total_evicted());
        assert_eq!(deserialized.next_sequence, log.next_sequence);
    }

    #[test]
    fn event_log_summary_serde_roundtrip() {
        let mut log = default_log();
        log.emit(sample_builder(
            MissionEventKind::CycleStarted,
            reason_codes::CYCLE_STARTED,
        ));
        let summary = log.summary();
        let json = serde_json::to_string(&summary).unwrap();
        let deserialized: MissionEventLogSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.retained_count, summary.retained_count);
        assert_eq!(deserialized.total_appended, summary.total_appended);
    }

    // ── Reason code stability tests ─────────────────────────────────────

    #[test]
    fn reason_codes_follow_naming_convention() {
        let codes = [
            reason_codes::READINESS_NOMINAL,
            reason_codes::READINESS_EMPTY,
            reason_codes::EXTRACTION_NOMINAL,
            reason_codes::EXTRACTION_NO_CANDIDATES,
            reason_codes::SCORING_NOMINAL,
            reason_codes::SCORING_ALL_BELOW_THRESHOLD,
            reason_codes::SOLVER_NOMINAL,
            reason_codes::SOLVER_NO_ASSIGNMENTS,
            reason_codes::ENVELOPE_PASS,
            reason_codes::ENVELOPE_TRIMMED,
            reason_codes::GATE_MAX_ASSIGNMENTS,
            reason_codes::GATE_MAX_RISKY,
            reason_codes::GATE_RETRY_STORM,
            reason_codes::ASSIGNMENT_EMITTED,
            reason_codes::REJECTION_CONFLICT,
            reason_codes::REJECTION_SAFETY,
            reason_codes::REJECTION_SOLVER,
            reason_codes::CONFLICT_RESERVATION_OVERLAP,
            reason_codes::CONFLICT_CONCURRENT_CLAIM,
            reason_codes::CONFLICT_ACTIVE_COLLISION,
            reason_codes::CONFLICT_AUTO_RESOLVED,
            reason_codes::CONFLICT_PENDING_MANUAL,
            reason_codes::UNBLOCK_DETECTED,
            reason_codes::CHURN_DETECTED,
            reason_codes::CYCLE_STARTED,
            reason_codes::CYCLE_COMPLETED,
            reason_codes::TRIGGER_ENQUEUED,
            reason_codes::METRICS_RECORDED,
        ];
        for code in &codes {
            assert!(
                code.starts_with("mission."),
                "Reason code '{}' must start with 'mission.'",
                code
            );
            let parts: Vec<&str> = code.split('.').collect();
            assert!(
                parts.len() == 3,
                "Reason code '{}' must have exactly 3 dot-separated segments",
                code
            );
        }
    }

    // ── CycleEventEmitter tests ─────────────────────────────────────────

    #[test]
    fn cycle_emitter_binds_context() {
        let mut log = default_log();
        {
            let mut emitter =
                CycleEventEmitter::new(&mut log, 7, 7000, "corr-007", "workspace-a", "track-x");
            emitter.emit_cycle_started("cadence_tick");
            emitter.emit_readiness_resolved(10, 5);
            emitter.emit_features_extracted(5, Some("bead-alpha"));
            emitter.emit_scoring_completed(5, 3, Some("bead-alpha"));
            emitter.emit_assignments_solved(2, 1);
            emitter.emit_safety_envelope_applied(2, 0);
            emitter.emit_cycle_completed(42, 2, 1);
        }

        assert_eq!(log.len(), 7);
        for event in log.events() {
            assert_eq!(event.cycle_id, 7);
            assert_eq!(event.timestamp_ms, 7000);
            assert_eq!(event.correlation_id, "corr-007");
            assert_eq!(event.workspace, "workspace-a");
            assert_eq!(event.track, "track-x");
        }

        let kinds: Vec<&MissionEventKind> = log.events().iter().map(|e| &e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                &MissionEventKind::CycleStarted,
                &MissionEventKind::ReadinessResolved,
                &MissionEventKind::FeaturesExtracted,
                &MissionEventKind::ScoringCompleted,
                &MissionEventKind::AssignmentsSolved,
                &MissionEventKind::SafetyEnvelopeApplied,
                &MissionEventKind::CycleCompleted,
            ]
        );
    }

    #[test]
    fn cycle_emitter_readiness_empty_uses_correct_reason() {
        let mut log = default_log();
        {
            let mut emitter =
                CycleEventEmitter::new(&mut log, 1, 1000, "corr-1", "ws", "trk");
            emitter.emit_readiness_resolved(10, 0);
        }
        assert_eq!(
            log.latest().unwrap().reason_code,
            reason_codes::READINESS_EMPTY
        );
    }

    #[test]
    fn cycle_emitter_scoring_below_threshold_uses_correct_reason() {
        let mut log = default_log();
        {
            let mut emitter =
                CycleEventEmitter::new(&mut log, 1, 1000, "corr-1", "ws", "trk");
            emitter.emit_scoring_completed(5, 0, None);
        }
        assert_eq!(
            log.latest().unwrap().reason_code,
            reason_codes::SCORING_ALL_BELOW_THRESHOLD
        );
    }

    #[test]
    fn cycle_emitter_safety_gate_rejection_maps_gate_name() {
        let mut log = default_log();
        {
            let mut emitter =
                CycleEventEmitter::new(&mut log, 1, 1000, "corr-1", "ws", "trk");
            emitter.emit_safety_gate_rejection(
                "mission.envelope.max_assignments_per_cycle",
                "bead-x",
                0.8,
            );
            emitter.emit_safety_gate_rejection(
                "mission.envelope.max_risky_assignments_per_cycle",
                "bead-y",
                0.6,
            );
            emitter.emit_safety_gate_rejection(
                "mission.envelope.retry_storm",
                "bead-z",
                0.4,
            );
        }
        let events = log.events();
        assert_eq!(events[0].reason_code, reason_codes::GATE_MAX_ASSIGNMENTS);
        assert_eq!(events[1].reason_code, reason_codes::GATE_MAX_RISKY);
        assert_eq!(events[2].reason_code, reason_codes::GATE_RETRY_STORM);
    }

    #[test]
    fn cycle_emitter_assignment_emitted_carries_details() {
        let mut log = default_log();
        {
            let mut emitter =
                CycleEventEmitter::new(&mut log, 1, 1000, "corr-1", "ws", "trk");
            emitter.emit_assignment_emitted("bead-a", "agent-1", 0.95, 1);
        }
        let event = log.latest().unwrap();
        assert_eq!(event.kind, MissionEventKind::AssignmentEmitted);
        assert_eq!(
            event.details.get("bead_id"),
            Some(&serde_json::Value::String("bead-a".to_string()))
        );
        assert_eq!(
            event.details.get("agent_id"),
            Some(&serde_json::Value::String("agent-1".to_string()))
        );
        assert_eq!(
            event.details.get("rank"),
            Some(&serde_json::Value::Number(1.into()))
        );
    }

    #[test]
    fn cycle_emitter_conflict_detected_maps_type() {
        let mut log = default_log();
        {
            let mut emitter =
                CycleEventEmitter::new(&mut log, 1, 1000, "corr-1", "ws", "trk");
            emitter.emit_conflict_detected(
                "c-001",
                "file_reservation_overlap",
                &["a1".to_string()],
                &["b1".to_string()],
            );
            emitter.emit_conflict_detected(
                "c-002",
                "concurrent_bead_claim",
                &["a2".to_string()],
                &["b2".to_string()],
            );
            emitter.emit_conflict_detected(
                "c-003",
                "active_claim_collision",
                &["a3".to_string()],
                &["b3".to_string()],
            );
        }
        let events = log.events();
        assert_eq!(
            events[0].reason_code,
            reason_codes::CONFLICT_RESERVATION_OVERLAP
        );
        assert_eq!(
            events[1].reason_code,
            reason_codes::CONFLICT_CONCURRENT_CLAIM
        );
        assert_eq!(
            events[2].reason_code,
            reason_codes::CONFLICT_ACTIVE_COLLISION
        );
    }

    // ── Clear test ──────────────────────────────────────────────────────

    #[test]
    fn clear_removes_all_events_preserves_counters() {
        let mut log = default_log();
        log.emit(sample_builder(
            MissionEventKind::CycleStarted,
            reason_codes::CYCLE_STARTED,
        ));
        log.emit(sample_builder(
            MissionEventKind::CycleCompleted,
            reason_codes::CYCLE_COMPLETED,
        ));
        assert_eq!(log.len(), 2);

        log.clear();
        assert!(log.is_empty());
        assert_eq!(log.total_appended(), 2); // preserved
        assert_eq!(log.next_sequence, 3); // preserved
    }

    // ── Edge: sequence monotonicity after eviction ──────────────────────

    #[test]
    fn sequence_monotonically_increases_across_eviction() {
        let mut log = small_log(2);
        let s1 = log
            .emit(sample_builder(
                MissionEventKind::CycleStarted,
                reason_codes::CYCLE_STARTED,
            ))
            .unwrap();
        let s2 = log
            .emit(sample_builder(
                MissionEventKind::CycleStarted,
                reason_codes::CYCLE_STARTED,
            ))
            .unwrap();
        let s3 = log
            .emit(sample_builder(
                MissionEventKind::CycleStarted,
                reason_codes::CYCLE_STARTED,
            ))
            .unwrap();
        assert!(s1 < s2);
        assert!(s2 < s3);
        // First event evicted, but remaining have monotonic sequences.
        let seqs: Vec<u64> = log.events().iter().map(|e| e.sequence).collect();
        assert_eq!(seqs, vec![2, 3]);
    }

    // ── Full pipeline emission test ─────────────────────────────────────

    #[test]
    fn full_pipeline_emits_ordered_events() {
        let mut log = default_log();
        {
            let mut emitter =
                CycleEventEmitter::new(&mut log, 1, 1000, "corr-pipeline", "ws", "trk");
            // Lifecycle start
            emitter.emit_cycle_started("bead_status_change");
            // Plan phase
            emitter.emit_readiness_resolved(20, 8);
            emitter.emit_features_extracted(8, Some("top-bead"));
            emitter.emit_scoring_completed(8, 5, Some("top-bead"));
            emitter.emit_assignments_solved(3, 2);
            // Safety phase
            emitter.emit_safety_envelope_applied(3, 0);
            // Dispatch phase
            emitter.emit_assignment_emitted("bead-1", "agent-a", 0.9, 1);
            emitter.emit_assignment_emitted("bead-2", "agent-b", 0.8, 2);
            emitter.emit_assignment_emitted("bead-3", "agent-a", 0.7, 3);
            emitter.emit_assignment_rejected("bead-4", reason_codes::REJECTION_CONFLICT, &[
                "ConflictWithAssigned".to_string(),
            ]);
            emitter.emit_assignment_rejected("bead-5", reason_codes::REJECTION_SOLVER, &[
                "BelowThreshold".to_string(),
            ]);
            // Reconcile phase
            emitter.emit_conflict_detected(
                "c-001",
                "file_reservation_overlap",
                &["agent-a".to_string(), "agent-c".to_string()],
                &["bead-1".to_string()],
            );
            // Lifecycle end
            emitter.emit_cycle_completed(15, 3, 2);
        }

        assert_eq!(log.len(), 13);

        // Verify phase distribution.
        let phase_counts = log.count_by_phase();
        assert_eq!(phase_counts.get(&MissionPhase::Lifecycle), Some(&2));
        assert_eq!(phase_counts.get(&MissionPhase::Plan), Some(&4));
        assert_eq!(phase_counts.get(&MissionPhase::Safety), Some(&1));
        assert_eq!(phase_counts.get(&MissionPhase::Dispatch), Some(&5));
        assert_eq!(phase_counts.get(&MissionPhase::Reconcile), Some(&1));

        // Verify sequences are monotonically increasing.
        let seqs: Vec<u64> = log.events().iter().map(|e| e.sequence).collect();
        for w in seqs.windows(2) {
            assert!(w[0] < w[1], "Sequences must be monotonic: {} >= {}", w[0], w[1]);
        }
    }
}
