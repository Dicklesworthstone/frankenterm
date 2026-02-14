//! Resize/reflow invariant definitions and runtime enforcement.
//!
//! Formalizes enforceable invariants spanning scheduler state, logical/physical
//! line mappings, cursor consistency, and presentation validity. Provides
//! structured diagnostics for violations and telemetry for continuous monitoring.
//!
//! These invariants encode the correctness contract for the resize subsystem:
//!
//! - **Scheduler**: At most one active transaction per pane, monotonic intent
//!   sequences, bounded queue depth, stale work never commits.
//! - **Screen**: Physical dimensions match content array bounds, cursor within
//!   viewport, line count consistent with rows + scrollback.
//! - **Cursor**: Logical↔physical cursor mapping round-trips, stable row index
//!   consistent before and after resize.
//! - **Presentation**: Committed resize has matching dimensions across PTY,
//!   terminal, and screen.
//!
//! Bead: wa-1u90p.6.1

use crate::resize_scheduler::{
    ResizeExecutionPhase, ResizeLifecycleDetail, ResizeLifecycleStage, ResizeSchedulerSnapshot,
    ResizeTransactionLifecycleEvent,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

// ---------------------------------------------------------------------------
// Violation types
// ---------------------------------------------------------------------------

/// Severity of a resize invariant violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeViolationSeverity {
    /// Suspicious but technically functional (e.g. near-threshold lock hold).
    Warning,
    /// Invariant violated — resize result may be incorrect.
    Error,
    /// Critical — data integrity or crash risk.
    Critical,
}

/// Category of resize invariant violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeViolationKind {
    // -- Scheduler invariants --
    /// More than one active transaction for a single pane.
    ConcurrentPaneTransaction,
    /// Intent sequence not strictly monotonic for a pane.
    IntentSequenceRegression,
    /// Queue depth exceeds bound after coalescing (should be <= 1).
    QueueDepthOverflow,
    /// Stale transaction committed (active_seq < latest_seq at commit).
    StaleCommit,
    /// Transaction phase transition violated (e.g. Idle -> Reflowing).
    IllegalPhaseTransition,
    /// Aggregate pending count in snapshot does not match pane rows.
    SnapshotPendingCountMismatch,
    /// Aggregate active count in snapshot does not match pane rows.
    SnapshotActiveCountMismatch,
    /// Duplicate pane row found in scheduler snapshot.
    DuplicatePaneSnapshotRow,
    /// Lifecycle event stream regressed in ordering metadata.
    LifecycleEventSequenceRegression,
    /// Lifecycle detail payload is inconsistent with lifecycle stage.
    LifecycleDetailStageMismatch,

    // -- Screen invariants --
    /// `physical_rows` does not match actual content bounds.
    RowCountMismatch,
    /// `physical_cols` changed mid-resize without content reflow.
    ColsContentDesync,
    /// Line array length below minimum viewport requirement.
    InsufficientLines,
    /// Line array length exceeds capacity (rows + scrollback).
    ExcessiveLines,

    // -- Cursor invariants --
    /// Cursor Y position outside physical line array bounds.
    CursorOutOfBounds,
    /// Logical↔physical cursor mapping does not round-trip.
    CursorMappingRoundtripFail,
    /// Stable row index round-trip mismatch after resize.
    StableRowRoundtripFail,
    /// Cursor seqno does not match current terminal seqno.
    CursorSeqnoMismatch,

    // -- Presentation invariants --
    /// PTY dimensions do not match terminal dimensions after commit.
    PtyTerminalDimensionMismatch,
    /// Render dimensions read differ from committed terminal dimensions.
    RenderDimensionStale,
    /// Frame presented with dimensions from a cancelled transaction.
    StaleFramePresented,
}

/// A single resize invariant violation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeViolation {
    pub severity: ResizeViolationSeverity,
    pub kind: ResizeViolationKind,
    pub pane_id: Option<u64>,
    pub intent_seq: Option<u64>,
    pub message: String,
}

impl fmt::Display for ResizeViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{:?}] {:?} pane={} seq={}: {}",
            self.severity,
            self.kind,
            self.pane_id
                .map_or_else(|| "n/a".to_string(), |id| id.to_string()),
            self.intent_seq
                .map_or_else(|| "n/a".to_string(), |seq| seq.to_string()),
            self.message
        )
    }
}

// ---------------------------------------------------------------------------
// Invariant checker
// ---------------------------------------------------------------------------

/// Accumulated invariant check results for a resize operation.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeInvariantReport {
    pub violations: Vec<ResizeViolation>,
    pub checks_passed: u64,
    pub checks_failed: u64,
}

impl ResizeInvariantReport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn total_checks(&self) -> u64 {
        self.checks_passed + self.checks_failed
    }

    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }

    pub fn has_critical(&self) -> bool {
        self.violations
            .iter()
            .any(|v| v.severity == ResizeViolationSeverity::Critical)
    }

    pub fn has_errors(&self) -> bool {
        self.violations
            .iter()
            .any(|v| v.severity == ResizeViolationSeverity::Error)
    }

    fn record_pass(&mut self) {
        self.checks_passed += 1;
    }

    fn record_violation(&mut self, violation: ResizeViolation) {
        self.checks_failed += 1;
        tracing::warn!("resize_invariant_violation: {}", violation);
        self.violations.push(violation);
    }

    fn check(
        &mut self,
        condition: bool,
        severity: ResizeViolationSeverity,
        kind: ResizeViolationKind,
        pane_id: Option<u64>,
        intent_seq: Option<u64>,
        message: impl Into<String>,
    ) {
        if condition {
            self.record_pass();
        } else {
            self.record_violation(ResizeViolation {
                severity,
                kind,
                pane_id,
                intent_seq,
                message: message.into(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Scheduler invariant checks
// ---------------------------------------------------------------------------

/// Validates scheduler state invariants for a pane's resize transaction.
///
/// Invariants from ADR-0011:
/// 1. At most one active transaction per pane.
/// 2. `intent_seq` is strictly monotonic per pane.
/// 3. Commit is valid only for the latest known intent.
/// 4. Stale work is cancellable and never commits presentation.
/// 5. Queue depth for a pane is bounded (`<= 1` pending after coalescing).
pub fn check_scheduler_invariants(
    report: &mut ResizeInvariantReport,
    pane_id: u64,
    active_seq: Option<u64>,
    latest_seq: Option<u64>,
    queue_depth: usize,
    is_committing: bool,
) {
    // INV-S1: Queue depth bounded
    report.check(
        queue_depth <= 1,
        ResizeViolationSeverity::Error,
        ResizeViolationKind::QueueDepthOverflow,
        Some(pane_id),
        latest_seq,
        format!(
            "queue_depth={} exceeds bound of 1 after coalescing",
            queue_depth
        ),
    );

    // INV-S2: Monotonic intent sequence
    if let (Some(active), Some(latest)) = (active_seq, latest_seq) {
        report.check(
            latest >= active,
            ResizeViolationSeverity::Error,
            ResizeViolationKind::IntentSequenceRegression,
            Some(pane_id),
            Some(latest),
            format!(
                "latest_seq={} < active_seq={}: sequence regression",
                latest, active
            ),
        );
    }

    // INV-S3: Stale commit prevention
    if is_committing {
        if let (Some(active), Some(latest)) = (active_seq, latest_seq) {
            report.check(
                active == latest,
                ResizeViolationSeverity::Critical,
                ResizeViolationKind::StaleCommit,
                Some(pane_id),
                Some(active),
                format!(
                    "committing active_seq={} but latest_seq={}: stale commit",
                    active, latest
                ),
            );
        }
    }
}

/// Validates per-pane scheduler snapshot invariants that are independent of
/// specific lifecycle events.
///
/// Invariants:
/// - `pending_seq` (if any) must not exceed `latest_seq`.
/// - `active_seq` (if any) must not exceed `latest_seq`.
/// - `active_phase` implies `active_seq` exists.
/// - If both pending and active exist, pending must be newer than active.
pub fn check_scheduler_snapshot_row_invariants(
    report: &mut ResizeInvariantReport,
    pane_id: u64,
    latest_seq: Option<u64>,
    pending_seq: Option<u64>,
    active_seq: Option<u64>,
    active_phase_present: bool,
) {
    if let (Some(pending), Some(latest)) = (pending_seq, latest_seq) {
        report.check(
            pending <= latest,
            ResizeViolationSeverity::Error,
            ResizeViolationKind::IntentSequenceRegression,
            Some(pane_id),
            Some(pending),
            format!(
                "pending_seq={} > latest_seq={}: pending sequence ahead of latest",
                pending, latest
            ),
        );
    }

    if let (Some(active), Some(latest)) = (active_seq, latest_seq) {
        report.check(
            active <= latest,
            ResizeViolationSeverity::Error,
            ResizeViolationKind::IntentSequenceRegression,
            Some(pane_id),
            Some(active),
            format!(
                "active_seq={} > latest_seq={}: active sequence ahead of latest",
                active, latest
            ),
        );
    }

    report.check(
        !active_phase_present || active_seq.is_some(),
        ResizeViolationSeverity::Critical,
        ResizeViolationKind::IllegalPhaseTransition,
        Some(pane_id),
        latest_seq,
        "active phase present without active sequence".to_string(),
    );

    if let (Some(active), Some(pending)) = (active_seq, pending_seq) {
        report.check(
            pending > active,
            ResizeViolationSeverity::Error,
            ResizeViolationKind::ConcurrentPaneTransaction,
            Some(pane_id),
            Some(pending),
            format!(
                "pending_seq={} must be greater than active_seq={} when both exist",
                pending, active
            ),
        );
    }
}

/// Validates aggregate scheduler snapshot invariants.
///
/// This bridges per-row and aggregate invariants so status/debug surfaces can be
/// checked as a single contract object.
pub fn check_scheduler_snapshot_invariants(
    report: &mut ResizeInvariantReport,
    snapshot: &ResizeSchedulerSnapshot,
) {
    let pending_rows = snapshot
        .panes
        .iter()
        .filter(|row| row.pending_seq.is_some())
        .count();
    report.check(
        pending_rows == snapshot.pending_total,
        ResizeViolationSeverity::Error,
        ResizeViolationKind::SnapshotPendingCountMismatch,
        None,
        None,
        format!(
            "pending_total={} but pane rows report {} pending entries",
            snapshot.pending_total, pending_rows
        ),
    );

    let active_rows = snapshot
        .panes
        .iter()
        .filter(|row| row.active_seq.is_some())
        .count();
    report.check(
        active_rows == snapshot.active_total,
        ResizeViolationSeverity::Error,
        ResizeViolationKind::SnapshotActiveCountMismatch,
        None,
        None,
        format!(
            "active_total={} but pane rows report {} active entries",
            snapshot.active_total, active_rows
        ),
    );

    let mut seen_panes = HashSet::new();
    for row in &snapshot.panes {
        report.check(
            seen_panes.insert(row.pane_id),
            ResizeViolationSeverity::Error,
            ResizeViolationKind::DuplicatePaneSnapshotRow,
            Some(row.pane_id),
            row.latest_seq,
            format!("duplicate pane row for pane_id={}", row.pane_id),
        );

        check_scheduler_snapshot_row_invariants(
            report,
            row.pane_id,
            row.latest_seq,
            row.pending_seq,
            row.active_seq,
            row.active_phase.is_some(),
        );

        check_scheduler_invariants(
            report,
            row.pane_id,
            row.active_seq,
            row.latest_seq,
            usize::from(row.pending_seq.is_some()),
            false,
        );
    }
}

/// Validates lifecycle event stream invariants.
///
/// Checks:
/// - event ordering (`event_seq` strictly increasing, `frame_seq` nondecreasing)
/// - lifecycle detail payload consistency with stage
pub fn check_lifecycle_event_invariants(
    report: &mut ResizeInvariantReport,
    events: &[ResizeTransactionLifecycleEvent],
) {
    let mut prev_event_seq = None;
    let mut prev_frame_seq = None;

    for event in events {
        if let Some(prev) = prev_event_seq {
            report.check(
                event.event_seq > prev,
                ResizeViolationSeverity::Error,
                ResizeViolationKind::LifecycleEventSequenceRegression,
                Some(event.pane_id),
                Some(event.intent_seq),
                format!(
                    "event_seq regression/non-increase: prev={} current={}",
                    prev, event.event_seq
                ),
            );
        }
        prev_event_seq = Some(event.event_seq);

        if let Some(prev) = prev_frame_seq {
            report.check(
                event.frame_seq >= prev,
                ResizeViolationSeverity::Error,
                ResizeViolationKind::LifecycleEventSequenceRegression,
                Some(event.pane_id),
                Some(event.intent_seq),
                format!(
                    "frame_seq regression: prev={} current={}",
                    prev, event.frame_seq
                ),
            );
        }
        prev_frame_seq = Some(event.frame_seq);

        match &event.detail {
            ResizeLifecycleDetail::IntentSubmitted { .. } => report.check(
                event.stage == ResizeLifecycleStage::Queued,
                ResizeViolationSeverity::Error,
                ResizeViolationKind::LifecycleDetailStageMismatch,
                Some(event.pane_id),
                Some(event.intent_seq),
                "IntentSubmitted detail must use Queued stage".to_string(),
            ),
            ResizeLifecycleDetail::IntentRejectedNonMonotonic { .. }
            | ResizeLifecycleDetail::IntentRejectedOverload { .. }
            | ResizeLifecycleDetail::IntentSuppressedByGate { .. }
            | ResizeLifecycleDetail::ActiveCompletionRejected { .. }
            | ResizeLifecycleDetail::ActivePhaseTransitionRejected { .. } => report.check(
                event.stage == ResizeLifecycleStage::Failed,
                ResizeViolationSeverity::Error,
                ResizeViolationKind::LifecycleDetailStageMismatch,
                Some(event.pane_id),
                Some(event.intent_seq),
                format!("{:?} detail must use Failed stage", event.detail),
            ),
            ResizeLifecycleDetail::PendingDroppedOverload { .. }
            | ResizeLifecycleDetail::ActiveCancelledSuperseded { .. } => report.check(
                event.stage == ResizeLifecycleStage::Cancelled,
                ResizeViolationSeverity::Error,
                ResizeViolationKind::LifecycleDetailStageMismatch,
                Some(event.pane_id),
                Some(event.intent_seq),
                format!("{:?} detail must use Cancelled stage", event.detail),
            ),
            ResizeLifecycleDetail::IntentScheduled { .. } => report.check(
                event.stage == ResizeLifecycleStage::Scheduled,
                ResizeViolationSeverity::Error,
                ResizeViolationKind::LifecycleDetailStageMismatch,
                Some(event.pane_id),
                Some(event.intent_seq),
                "IntentScheduled detail must use Scheduled stage".to_string(),
            ),
            ResizeLifecycleDetail::ActiveCompleted => report.check(
                event.stage == ResizeLifecycleStage::Committed,
                ResizeViolationSeverity::Error,
                ResizeViolationKind::LifecycleDetailStageMismatch,
                Some(event.pane_id),
                Some(event.intent_seq),
                "ActiveCompleted detail must use Committed stage".to_string(),
            ),
            ResizeLifecycleDetail::ActivePhaseTransition { phase } => {
                let expected_stage = phase_to_stage(*phase);
                report.check(
                    event.stage == expected_stage,
                    ResizeViolationSeverity::Error,
                    ResizeViolationKind::LifecycleDetailStageMismatch,
                    Some(event.pane_id),
                    Some(event.intent_seq),
                    format!(
                        "ActivePhaseTransition({:?}) detail must use {:?} stage (got {:?})",
                        phase, expected_stage, event.stage
                    ),
                );
            }
        }
    }
}

const fn phase_to_stage(phase: ResizeExecutionPhase) -> ResizeLifecycleStage {
    match phase {
        ResizeExecutionPhase::Preparing => ResizeLifecycleStage::Preparing,
        ResizeExecutionPhase::Reflowing => ResizeLifecycleStage::Reflowing,
        ResizeExecutionPhase::Presenting => ResizeLifecycleStage::Presenting,
    }
}

// ---------------------------------------------------------------------------
// Screen invariant checks
// ---------------------------------------------------------------------------

/// Screen state snapshot for invariant checking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    pub physical_rows: usize,
    pub physical_cols: usize,
    pub lines_len: usize,
    pub scrollback_size: usize,
    pub cursor_x: usize,
    pub cursor_y: i64,
    pub cursor_phys_row: usize,
}

/// Validates screen buffer invariants after a resize operation.
///
/// Invariants:
/// - Line array length >= physical_rows (viewport must be filled).
/// - Line array length <= physical_rows + scrollback_size (capacity bound).
/// - Cursor within viewport bounds.
pub fn check_screen_invariants(
    report: &mut ResizeInvariantReport,
    pane_id: Option<u64>,
    snapshot: &ScreenSnapshot,
) {
    // INV-SCR1: Minimum line count
    report.check(
        snapshot.lines_len >= snapshot.physical_rows,
        ResizeViolationSeverity::Error,
        ResizeViolationKind::InsufficientLines,
        pane_id,
        None,
        format!(
            "lines_len={} < physical_rows={}: viewport not filled",
            snapshot.lines_len, snapshot.physical_rows
        ),
    );

    // INV-SCR2: Maximum line count
    let capacity = snapshot.physical_rows + snapshot.scrollback_size;
    report.check(
        snapshot.lines_len <= capacity,
        ResizeViolationSeverity::Warning,
        ResizeViolationKind::ExcessiveLines,
        pane_id,
        None,
        format!(
            "lines_len={} > capacity={} (rows={} + scrollback={})",
            snapshot.lines_len, capacity, snapshot.physical_rows, snapshot.scrollback_size
        ),
    );

    // INV-SCR3: Cursor within physical bounds
    report.check(
        snapshot.cursor_phys_row < snapshot.lines_len,
        ResizeViolationSeverity::Error,
        ResizeViolationKind::CursorOutOfBounds,
        pane_id,
        None,
        format!(
            "cursor_phys_row={} >= lines_len={}: cursor outside line array",
            snapshot.cursor_phys_row, snapshot.lines_len
        ),
    );

    // INV-SCR4: Cursor Y within viewport
    report.check(
        snapshot.cursor_y >= 0 && (snapshot.cursor_y as usize) < snapshot.physical_rows,
        ResizeViolationSeverity::Warning,
        ResizeViolationKind::CursorOutOfBounds,
        pane_id,
        None,
        format!(
            "cursor_y={} outside viewport [0, {})",
            snapshot.cursor_y, snapshot.physical_rows
        ),
    );

    // INV-SCR5: Cursor X within column bounds
    // Note: cursor_x == physical_cols is valid (cursor at end of line, wrap_next state)
    report.check(
        snapshot.cursor_x <= snapshot.physical_cols,
        ResizeViolationSeverity::Warning,
        ResizeViolationKind::CursorOutOfBounds,
        pane_id,
        None,
        format!(
            "cursor_x={} > physical_cols={}: cursor past right margin",
            snapshot.cursor_x, snapshot.physical_cols
        ),
    );
}

// ---------------------------------------------------------------------------
// Presentation invariant checks
// ---------------------------------------------------------------------------

/// Dimension triple for presentation consistency checking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DimensionTriple {
    pub pty_rows: usize,
    pub pty_cols: usize,
    pub terminal_rows: usize,
    pub terminal_cols: usize,
    pub screen_rows: usize,
    pub screen_cols: usize,
}

/// Validates that PTY, terminal, and screen dimensions are consistent
/// after a resize commit.
pub fn check_presentation_invariants(
    report: &mut ResizeInvariantReport,
    pane_id: Option<u64>,
    intent_seq: Option<u64>,
    dims: &DimensionTriple,
) {
    // INV-P1: PTY matches terminal
    report.check(
        dims.pty_rows == dims.terminal_rows && dims.pty_cols == dims.terminal_cols,
        ResizeViolationSeverity::Error,
        ResizeViolationKind::PtyTerminalDimensionMismatch,
        pane_id,
        intent_seq,
        format!(
            "pty={}x{} != terminal={}x{}: dimension mismatch after commit",
            dims.pty_cols, dims.pty_rows, dims.terminal_cols, dims.terminal_rows
        ),
    );

    // INV-P2: Terminal matches screen
    report.check(
        dims.terminal_rows == dims.screen_rows && dims.terminal_cols == dims.screen_cols,
        ResizeViolationSeverity::Error,
        ResizeViolationKind::RenderDimensionStale,
        pane_id,
        intent_seq,
        format!(
            "terminal={}x{} != screen={}x{}: screen not updated",
            dims.terminal_cols, dims.terminal_rows, dims.screen_cols, dims.screen_rows
        ),
    );
}

// ---------------------------------------------------------------------------
// Phase transition invariant
// ---------------------------------------------------------------------------

/// Valid resize transaction phases from ADR-0011.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizePhase {
    Idle,
    Queued,
    Preparing,
    Reflowing,
    Presenting,
    Committed,
    Cancelled,
    Failed,
}

impl ResizePhase {
    /// Returns the set of valid successor phases for this phase.
    pub fn valid_transitions(self) -> &'static [ResizePhase] {
        match self {
            Self::Idle => &[Self::Queued],
            Self::Queued => &[Self::Preparing, Self::Cancelled],
            Self::Preparing => &[Self::Reflowing, Self::Cancelled, Self::Failed],
            Self::Reflowing => &[Self::Presenting, Self::Cancelled, Self::Failed],
            Self::Presenting => &[Self::Committed, Self::Cancelled, Self::Failed],
            Self::Committed => &[Self::Idle],
            Self::Cancelled => &[Self::Queued, Self::Idle],
            Self::Failed => &[Self::Idle],
        }
    }
}

/// Validates a phase transition is legal per ADR-0011.
pub fn check_phase_transition(
    report: &mut ResizeInvariantReport,
    pane_id: Option<u64>,
    intent_seq: Option<u64>,
    from: ResizePhase,
    to: ResizePhase,
) {
    let valid = from.valid_transitions().contains(&to);
    report.check(
        valid,
        ResizeViolationSeverity::Critical,
        ResizeViolationKind::IllegalPhaseTransition,
        pane_id,
        intent_seq,
        format!(
            "illegal transition {:?} -> {:?}, valid targets: {:?}",
            from,
            to,
            from.valid_transitions()
        ),
    );
}

// ---------------------------------------------------------------------------
// Telemetry snapshot
// ---------------------------------------------------------------------------

/// Aggregate telemetry for resize invariant monitoring.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeInvariantTelemetry {
    pub total_checks: u64,
    pub total_passes: u64,
    pub total_failures: u64,
    pub critical_count: u64,
    pub error_count: u64,
    pub warning_count: u64,
}

impl ResizeInvariantTelemetry {
    pub fn absorb(&mut self, report: &ResizeInvariantReport) {
        self.total_checks += report.total_checks();
        self.total_passes += report.checks_passed;
        self.total_failures += report.checks_failed;
        for v in &report.violations {
            match v.severity {
                ResizeViolationSeverity::Critical => self.critical_count += 1,
                ResizeViolationSeverity::Error => self.error_count += 1,
                ResizeViolationSeverity::Warning => self.warning_count += 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Scheduler invariant tests --

    #[test]
    fn scheduler_clean_single_intent() {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_invariants(&mut report, 1, Some(1), Some(1), 0, false);
        assert!(report.is_clean());
        assert!(report.checks_passed > 0);
    }

    #[test]
    fn scheduler_detects_queue_overflow() {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_invariants(&mut report, 1, Some(1), Some(2), 3, false);
        assert!(report.has_errors());
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.kind == ResizeViolationKind::QueueDepthOverflow)
        );
    }

    #[test]
    fn scheduler_detects_stale_commit() {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_invariants(&mut report, 1, Some(5), Some(7), 0, true);
        assert!(report.has_critical());
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.kind == ResizeViolationKind::StaleCommit)
        );
    }

    #[test]
    fn scheduler_allows_valid_commit() {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_invariants(&mut report, 1, Some(5), Some(5), 0, true);
        assert!(report.is_clean());
    }

    #[test]
    fn scheduler_detects_sequence_regression() {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_invariants(&mut report, 1, Some(10), Some(5), 0, false);
        assert!(report.has_errors());
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.kind == ResizeViolationKind::IntentSequenceRegression)
        );
    }

    #[test]
    fn scheduler_snapshot_row_detects_pending_ahead_of_latest() {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_snapshot_row_invariants(&mut report, 7, Some(5), Some(6), Some(4), true);
        assert!(report.has_errors());
        assert!(
            report
                .violations
                .iter()
                .any(|v| { matches!(v.kind, ResizeViolationKind::IntentSequenceRegression) })
        );
    }

    #[test]
    fn scheduler_snapshot_row_detects_phase_without_active_seq() {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_snapshot_row_invariants(&mut report, 9, Some(3), None, None, true);
        assert!(report.has_critical());
        assert!(
            report
                .violations
                .iter()
                .any(|v| matches!(v.kind, ResizeViolationKind::IllegalPhaseTransition))
        );
    }

    // -- Screen invariant tests --

    #[test]
    fn screen_clean_after_resize() {
        let mut report = ResizeInvariantReport::new();
        let snapshot = ScreenSnapshot {
            physical_rows: 24,
            physical_cols: 80,
            lines_len: 1024,
            scrollback_size: 1000,
            cursor_x: 5,
            cursor_y: 10,
            cursor_phys_row: 1010,
        };
        check_screen_invariants(&mut report, Some(1), &snapshot);
        assert!(report.is_clean());
    }

    #[test]
    fn screen_detects_insufficient_lines() {
        let mut report = ResizeInvariantReport::new();
        let snapshot = ScreenSnapshot {
            physical_rows: 24,
            physical_cols: 80,
            lines_len: 10,
            scrollback_size: 1000,
            cursor_x: 0,
            cursor_y: 0,
            cursor_phys_row: 5,
        };
        check_screen_invariants(&mut report, Some(1), &snapshot);
        assert!(report.has_errors());
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.kind == ResizeViolationKind::InsufficientLines)
        );
    }

    #[test]
    fn screen_detects_cursor_out_of_bounds() {
        let mut report = ResizeInvariantReport::new();
        let snapshot = ScreenSnapshot {
            physical_rows: 24,
            physical_cols: 80,
            lines_len: 1024,
            scrollback_size: 1000,
            cursor_x: 5,
            cursor_y: 10,
            cursor_phys_row: 2000, // past end of lines array
        };
        check_screen_invariants(&mut report, Some(1), &snapshot);
        assert!(report.has_errors());
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.kind == ResizeViolationKind::CursorOutOfBounds)
        );
    }

    #[test]
    fn screen_warns_cursor_past_viewport() {
        let mut report = ResizeInvariantReport::new();
        let snapshot = ScreenSnapshot {
            physical_rows: 24,
            physical_cols: 80,
            lines_len: 1024,
            scrollback_size: 1000,
            cursor_x: 5,
            cursor_y: 30, // past viewport
            cursor_phys_row: 1010,
        };
        check_screen_invariants(&mut report, Some(1), &snapshot);
        assert!(!report.is_clean());
    }

    // -- Presentation invariant tests --

    #[test]
    fn presentation_clean_when_all_match() {
        let mut report = ResizeInvariantReport::new();
        let dims = DimensionTriple {
            pty_rows: 24,
            pty_cols: 80,
            terminal_rows: 24,
            terminal_cols: 80,
            screen_rows: 24,
            screen_cols: 80,
        };
        check_presentation_invariants(&mut report, Some(1), Some(1), &dims);
        assert!(report.is_clean());
    }

    #[test]
    fn presentation_detects_pty_terminal_mismatch() {
        let mut report = ResizeInvariantReport::new();
        let dims = DimensionTriple {
            pty_rows: 30,
            pty_cols: 120,
            terminal_rows: 24,
            terminal_cols: 80,
            screen_rows: 24,
            screen_cols: 80,
        };
        check_presentation_invariants(&mut report, Some(1), Some(1), &dims);
        assert!(report.has_errors());
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.kind == ResizeViolationKind::PtyTerminalDimensionMismatch)
        );
    }

    // -- Phase transition tests --

    #[test]
    fn phase_transition_happy_path() {
        let mut report = ResizeInvariantReport::new();
        let transitions = [
            (ResizePhase::Idle, ResizePhase::Queued),
            (ResizePhase::Queued, ResizePhase::Preparing),
            (ResizePhase::Preparing, ResizePhase::Reflowing),
            (ResizePhase::Reflowing, ResizePhase::Presenting),
            (ResizePhase::Presenting, ResizePhase::Committed),
            (ResizePhase::Committed, ResizePhase::Idle),
        ];
        for (from, to) in transitions {
            check_phase_transition(&mut report, Some(1), Some(1), from, to);
        }
        assert!(report.is_clean());
        assert_eq!(report.checks_passed, 6);
    }

    #[test]
    fn phase_transition_cancellation_path() {
        let mut report = ResizeInvariantReport::new();
        check_phase_transition(
            &mut report,
            Some(1),
            Some(1),
            ResizePhase::Preparing,
            ResizePhase::Cancelled,
        );
        check_phase_transition(
            &mut report,
            Some(1),
            Some(2),
            ResizePhase::Cancelled,
            ResizePhase::Queued,
        );
        assert!(report.is_clean());
    }

    #[test]
    fn phase_transition_detects_illegal() {
        let mut report = ResizeInvariantReport::new();
        check_phase_transition(
            &mut report,
            Some(1),
            Some(1),
            ResizePhase::Idle,
            ResizePhase::Reflowing,
        );
        assert!(report.has_critical());
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.kind == ResizeViolationKind::IllegalPhaseTransition)
        );
    }

    #[test]
    fn phase_transition_detects_skip() {
        let mut report = ResizeInvariantReport::new();
        check_phase_transition(
            &mut report,
            Some(1),
            Some(1),
            ResizePhase::Queued,
            ResizePhase::Presenting,
        );
        assert!(report.has_critical());
    }

    // -- Telemetry tests --

    #[test]
    fn telemetry_absorbs_report() {
        let mut telemetry = ResizeInvariantTelemetry::default();
        let mut report = ResizeInvariantReport::new();

        check_scheduler_invariants(&mut report, 1, Some(1), Some(1), 0, false);
        check_scheduler_invariants(&mut report, 2, Some(5), Some(7), 3, true);

        telemetry.absorb(&report);
        assert!(telemetry.total_checks > 0);
        assert!(telemetry.total_failures > 0);
        assert!(telemetry.critical_count > 0); // stale commit
        assert!(telemetry.error_count > 0); // queue overflow
    }

    // -- Report Display tests --

    #[test]
    fn violation_display_format() {
        let v = ResizeViolation {
            severity: ResizeViolationSeverity::Error,
            kind: ResizeViolationKind::QueueDepthOverflow,
            pane_id: Some(42),
            intent_seq: Some(7),
            message: "queue_depth=3".to_string(),
        };
        let s = format!("{}", v);
        assert!(s.contains("Error"));
        assert!(s.contains("QueueDepthOverflow"));
        assert!(s.contains("42"));
        assert!(s.contains("7"));
        assert!(s.contains("queue_depth=3"));
    }
}
