//! Bridge from BSU orchestrator telemetry to watchdog
//! telemetry (br-ft-2m6qe sub-task 6 substrate slice).
//!
//! The orchestrator substrate
//! ([`SyncOutputOrchestratorTelemetry`]) and the watchdog
//! substrate ([`SyncOutputTelemetry`]) each track their
//! own counters. The integration drives both: every PTY
//! chunk admitted into the buffer needs to bump
//! `mid_bsu_byte_count` on the watchdog telemetry; every
//! buffer drain needs to forward the BSU outcome to the
//! watchdog's depth-tracker. Without a typed bridge the
//! integration's call sites duplicate that translation,
//! and a missed bump silently masks the bead's
//! "mid_bsu_byte_count integration" requirement.
//!
//! [`SyncOutputOrchestratorTelemetry`]: crate::sync_output_buffer_orchestrator::SyncOutputOrchestratorTelemetry
//! [`SyncOutputTelemetry`]: crate::sync_output_watchdog::SyncOutputTelemetry
//!
//! ## What this module ships
//!
//! - [`forward_admission`] — given a
//!   [`BufferAdmissionDecision`] + the incoming chunk's
//!   byte count, bumps the watchdog's
//!   `mid_bsu_byte_count` by the right amount (skipped
//!   when the orchestrator refused).
//! - [`forward_drain`] — given a
//!   [`BufferDrainOutcome`] + the [`BsuDepthOutcome`] from
//!   the watchdog's depth-counter close path, calls
//!   `record_depth_outcome` with the resolved current-max.
//! - [`forward_mode_query`] — single-call helper for the
//!   `record_mode_query` bump.
//! - [`TelemetryDriftAuditor`] — cross-counter invariant
//!   checker the integration's audit lane runs after every
//!   pane lifecycle. Asserts (a) the watchdog's
//!   `mid_bsu_byte_count` equals the orchestrator's
//!   `bytes_accepted`, (b) the watchdog's `bsu_count`
//!   equals the orchestrator's BSU-open observations
//!   (computed as the sum of the relevant override counter,
//!   or zero when none) — drift signals an integration bug.
//!
//! [`BufferAdmissionDecision`]: crate::sync_output_buffer_orchestrator::BufferAdmissionDecision
//! [`BufferDrainOutcome`]: crate::sync_output_buffer_orchestrator::BufferDrainOutcome
//! [`BsuDepthOutcome`]: crate::sync_output_watchdog::BsuDepthOutcome

use crate::sync_output_buffer_orchestrator::{
    BufferAdmissionDecision, BufferDrainOutcome, SyncOutputOrchestratorTelemetry,
};
use crate::sync_output_watchdog::{BsuDepthOutcome, SyncOutputTelemetry};

// ============================================================================
// Forwarding helpers
// ============================================================================

/// Forward one PTY-chunk admission into the watchdog
/// telemetry's `mid_bsu_byte_count` counter.
///
/// Skipped when the orchestrator's decision was
/// [`BufferAdmissionDecision::Refused`] — refused bytes
/// never entered the buffer and shouldn't appear in the
/// mid-BSU byte total. For
/// [`BufferAdmissionDecision::Truncated`], the integration
/// admits `incoming_bytes` (the substrate dropped older
/// bytes from the buffer to make room); the
/// `mid_bsu_byte_count` correctly counts the new bytes.
///
/// `incoming_bytes` is the size of the incoming chunk —
/// the same value the integration passed to
/// [`evaluate_buffer_admission`] /
/// [`SyncOutputOrchestratorTelemetry::record_admission`].
///
/// [`evaluate_buffer_admission`]: crate::sync_output_buffer_orchestrator::evaluate_buffer_admission
pub fn forward_admission(
    decision: BufferAdmissionDecision,
    incoming_bytes: u64,
    watchdog_telemetry: &mut SyncOutputTelemetry,
) {
    if matches!(
        decision,
        BufferAdmissionDecision::Accepted | BufferAdmissionDecision::Truncated { .. }
    ) {
        watchdog_telemetry.record_mid_bsu_bytes(incoming_bytes);
    }
}

/// Forward one buffer-drain outcome into the watchdog
/// telemetry's depth-outcome counter.
///
/// `current_max` is the maximum BSU depth observed for the
/// pane so far — the watchdog's `record_depth_outcome` uses
/// it to maintain `max_bsu_depth_observed`. The integration
/// reads this from
/// [`crate::sync_output_watchdog::BsuDepthCounter::current_max`].
///
/// [`BufferDrainOutcome::NoOp`] is treated as a no-op at
/// this layer — the watchdog already knows the BSU window
/// closed via its own state machine; the bridge only
/// records depth outcomes for actual buffer drains.
pub fn forward_drain(
    drain_outcome: BufferDrainOutcome,
    bsu_depth_outcome: BsuDepthOutcome,
    current_max: u32,
    watchdog_telemetry: &mut SyncOutputTelemetry,
) {
    if matches!(drain_outcome, BufferDrainOutcome::Drained { .. }) {
        watchdog_telemetry.record_depth_outcome(bsu_depth_outcome, current_max);
    }
}

/// Forward one mode-query response into the watchdog
/// telemetry's `mode_query_count` counter. Single-call
/// helper so the integration's parser hook doesn't need
/// to know which counter the watchdog uses.
pub fn forward_mode_query(watchdog_telemetry: &mut SyncOutputTelemetry) {
    watchdog_telemetry.record_mode_query();
}

// ============================================================================
// Cross-counter audit
// ============================================================================

/// Drift findings the auditor surfaces.
///
/// Each variant names a specific invariant that should hold
/// between the two telemetry surfaces. The integration's
/// audit lane runs the auditor at pane teardown and on
/// `ft doctor` invocations; a non-empty `Vec<TelemetryDrift>`
/// signals an integration bug (missed forward call, or a
/// counter being reset on one side but not the other).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryDrift {
    /// `watchdog.mid_bsu_byte_count` doesn't equal
    /// `orchestrator.bytes_accepted`. Cause is usually a
    /// missed [`forward_admission`] call.
    MidBsuByteCountMismatch {
        watchdog_count: u64,
        orchestrator_count: u64,
    },
    /// Watchdog observed more BSU opens than the orchestrator
    /// admissions can account for: every BSU window should
    /// have at least one accepted byte (otherwise why open
    /// a BSU?). This is a soft invariant — operators can
    /// silence it by configuring
    /// [`AuditorConfig::expect_bytes_per_bsu`] to false.
    BsuWithoutAdmissions { bsu_count: u64 },
    /// Orchestrator recorded admissions but the watchdog's
    /// BSU counter is zero — the integration probably forgot
    /// to call `BsuDepthCounter::open_bsu` before admitting
    /// bytes.
    AdmissionsWithoutBsu {
        orchestrator_admissions: u64,
        watchdog_bsu_count: u64,
    },
}

/// Auditor configuration. Defaults pin the bead's strict
/// invariants; integration tests may relax some checks for
/// scenarios that legitimately violate them (e.g., a pane
/// that opens a BSU then pipes through pure ESU without
/// any bytes — rare but legal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditorConfig {
    /// When true (default), every BSU open must be matched
    /// by at least one accepted-byte admission. Disable for
    /// scenarios that legitimately open and close a BSU
    /// without any bytes (rare but legal per the protocol).
    pub expect_bytes_per_bsu: bool,
    /// When true (default), the orchestrator's
    /// `bytes_accepted` must equal the watchdog's
    /// `mid_bsu_byte_count` exactly. Cumulative — the
    /// watchdog never decrements the counter. Disable only
    /// for tests that intentionally introduce drift.
    pub expect_byte_count_parity: bool,
}

impl Default for AuditorConfig {
    fn default() -> Self {
        Self {
            expect_bytes_per_bsu: true,
            expect_byte_count_parity: true,
        }
    }
}

/// Cross-counter auditor.
///
/// Runs in O(1) — just compares counter values from the two
/// telemetry surfaces. The integration's audit lane calls
/// [`TelemetryDriftAuditor::audit`] at pane teardown and
/// emits the resulting `Vec<TelemetryDrift>` as a structured
/// log line.
#[derive(Debug, Clone, Copy, Default)]
pub struct TelemetryDriftAuditor {
    pub config: AuditorConfig,
}

impl TelemetryDriftAuditor {
    #[must_use]
    pub const fn new(config: AuditorConfig) -> Self {
        Self { config }
    }

    /// Run all enabled checks against the two telemetry
    /// surfaces. Returns the drift findings (empty when
    /// invariants hold).
    #[must_use]
    pub fn audit(
        &self,
        orchestrator: &SyncOutputOrchestratorTelemetry,
        watchdog: &SyncOutputTelemetry,
    ) -> Vec<TelemetryDrift> {
        let mut findings = Vec::new();

        if self.config.expect_byte_count_parity
            && watchdog.mid_bsu_byte_count() != orchestrator.bytes_accepted
        {
            findings.push(TelemetryDrift::MidBsuByteCountMismatch {
                watchdog_count: watchdog.mid_bsu_byte_count(),
                orchestrator_count: orchestrator.bytes_accepted,
            });
        }

        if self.config.expect_bytes_per_bsu {
            // BSU without admissions: bsu_count > 0 but
            // bytes_accepted == 0 → integration opened a
            // BSU and then closed it without any chunks.
            if watchdog.bsu_count() > 0 && orchestrator.bytes_accepted == 0 {
                findings.push(TelemetryDrift::BsuWithoutAdmissions {
                    bsu_count: watchdog.bsu_count(),
                });
            }
            // Admissions without BSU: bytes_accepted > 0
            // but bsu_count == 0 → integration admitted
            // bytes without first opening a BSU window.
            if orchestrator.bytes_accepted > 0 && watchdog.bsu_count() == 0 {
                findings.push(TelemetryDrift::AdmissionsWithoutBsu {
                    orchestrator_admissions: orchestrator.bytes_accepted,
                    watchdog_bsu_count: watchdog.bsu_count(),
                });
            }
        }

        findings
    }

    /// True iff `audit` returns no findings.
    #[must_use]
    pub fn invariants_hold(
        &self,
        orchestrator: &SyncOutputOrchestratorTelemetry,
        watchdog: &SyncOutputTelemetry,
    ) -> bool {
        self.audit(orchestrator, watchdog).is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_output_buffer_orchestrator::DrainCause;

    fn fresh_telemetry() -> (SyncOutputOrchestratorTelemetry, SyncOutputTelemetry) {
        (
            SyncOutputOrchestratorTelemetry::default(),
            SyncOutputTelemetry::default(),
        )
    }

    // ----------------------------------------------------------------
    // forward_admission
    // ----------------------------------------------------------------

    #[test]
    fn forward_admission_accepted_bumps_mid_bsu_bytes() {
        let mut wd = SyncOutputTelemetry::default();
        forward_admission(BufferAdmissionDecision::Accepted, 1024, &mut wd);
        assert_eq!(wd.mid_bsu_byte_count(), 1024);
    }

    #[test]
    fn forward_admission_truncated_bumps_mid_bsu_bytes() {
        // Truncated still admits the incoming chunk; the
        // substrate dropped older bytes to make room. The
        // counter should reflect the new bytes that entered.
        let mut wd = SyncOutputTelemetry::default();
        forward_admission(
            BufferAdmissionDecision::Truncated { dropped_bytes: 512 },
            2048,
            &mut wd,
        );
        assert_eq!(wd.mid_bsu_byte_count(), 2048);
    }

    #[test]
    fn forward_admission_refused_does_not_bump_counter() {
        let mut wd = SyncOutputTelemetry::default();
        forward_admission(BufferAdmissionDecision::Refused, 4096, &mut wd);
        assert_eq!(wd.mid_bsu_byte_count(), 0);
    }

    #[test]
    fn forward_admission_accumulates_across_multiple_calls() {
        let mut wd = SyncOutputTelemetry::default();
        for chunk in [256u64, 512, 1024, 2048] {
            forward_admission(BufferAdmissionDecision::Accepted, chunk, &mut wd);
        }
        assert_eq!(wd.mid_bsu_byte_count(), 256 + 512 + 1024 + 2048);
    }

    // ----------------------------------------------------------------
    // forward_drain
    // ----------------------------------------------------------------

    #[test]
    fn forward_drain_drained_records_depth_outcome() {
        let mut wd = SyncOutputTelemetry::default();
        forward_drain(
            BufferDrainOutcome::Drained {
                bytes: 2048,
                cause: DrainCause::Esu,
            },
            BsuDepthOutcome::Flushed,
            5, // current_max
            &mut wd,
        );
        // Flushed is recorded as both an ESU (esu_count++)
        // and an esu_flush_count++.
        assert_eq!(wd.esu_count(), 1);
    }

    #[test]
    fn forward_drain_noop_does_not_record() {
        let mut wd = SyncOutputTelemetry::default();
        forward_drain(
            BufferDrainOutcome::NoOp,
            BsuDepthOutcome::Flushed,
            0,
            &mut wd,
        );
        // No depth outcome recorded — the buffer was empty,
        // so the integration's drain was a no-op.
        assert_eq!(wd.esu_count(), 0);
        assert_eq!(wd.bsu_count(), 0);
    }

    #[test]
    fn forward_drain_propagates_max_depth() {
        let mut wd = SyncOutputTelemetry::default();
        forward_drain(
            BufferDrainOutcome::Drained {
                bytes: 100,
                cause: DrainCause::Watchdog,
            },
            BsuDepthOutcome::Closed { new_depth: 1 },
            7, // current_max — should propagate
            &mut wd,
        );
        assert_eq!(wd.max_bsu_depth_observed(), 7);
    }

    // ----------------------------------------------------------------
    // forward_mode_query
    // ----------------------------------------------------------------

    #[test]
    fn forward_mode_query_bumps_mode_query_count() {
        let mut wd = SyncOutputTelemetry::default();
        forward_mode_query(&mut wd);
        forward_mode_query(&mut wd);
        forward_mode_query(&mut wd);
        assert_eq!(wd.mode_query_count(), 3);
    }

    // ----------------------------------------------------------------
    // Auditor: invariants hold
    // ----------------------------------------------------------------

    #[test]
    fn auditor_clean_state_returns_no_findings() {
        let (orch, wd) = fresh_telemetry();
        let auditor = TelemetryDriftAuditor::new(AuditorConfig::default());
        assert!(auditor.audit(&orch, &wd).is_empty());
        assert!(auditor.invariants_hold(&orch, &wd));
    }

    #[test]
    fn auditor_byte_parity_holds_after_consistent_admission() {
        let (mut orch, mut wd) = fresh_telemetry();
        // Open BSU on watchdog side.
        wd.record_depth_outcome(BsuDepthOutcome::Opened { new_depth: 1 }, 1);
        // Admit a chunk on both sides via the bridge.
        let chunk = 1024u64;
        orch.record_admission(BufferAdmissionDecision::Accepted, chunk);
        forward_admission(BufferAdmissionDecision::Accepted, chunk, &mut wd);

        let auditor = TelemetryDriftAuditor::new(AuditorConfig::default());
        assert!(auditor.invariants_hold(&orch, &wd));
    }

    // ----------------------------------------------------------------
    // Auditor: drift findings
    // ----------------------------------------------------------------

    #[test]
    fn auditor_detects_byte_count_mismatch_when_forward_skipped() {
        let (mut orch, mut wd) = fresh_telemetry();
        wd.record_depth_outcome(BsuDepthOutcome::Opened { new_depth: 1 }, 1);
        // Integration recorded admission on orchestrator
        // but FORGOT to call forward_admission.
        orch.record_admission(BufferAdmissionDecision::Accepted, 999);
        // wd.mid_bsu_byte_count remains 0 → drift.

        let auditor = TelemetryDriftAuditor::new(AuditorConfig::default());
        let findings = auditor.audit(&orch, &wd);
        assert_eq!(findings.len(), 1);
        match findings[0] {
            TelemetryDrift::MidBsuByteCountMismatch {
                watchdog_count,
                orchestrator_count,
            } => {
                assert_eq!(watchdog_count, 0);
                assert_eq!(orchestrator_count, 999);
            }
            ref other => panic!("expected MidBsuByteCountMismatch, got {other:?}"),
        }
    }

    #[test]
    fn auditor_detects_admissions_without_bsu() {
        let (mut orch, mut wd) = fresh_telemetry();
        // Admission recorded on both sides — but no BSU
        // open on watchdog. Integration bug: admitting
        // bytes without first opening a BSU window.
        let chunk = 512u64;
        orch.record_admission(BufferAdmissionDecision::Accepted, chunk);
        forward_admission(BufferAdmissionDecision::Accepted, chunk, &mut wd);

        let auditor = TelemetryDriftAuditor::new(AuditorConfig::default());
        let findings = auditor.audit(&orch, &wd);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0],
            TelemetryDrift::AdmissionsWithoutBsu {
                orchestrator_admissions: 512,
                watchdog_bsu_count: 0,
            }
        ));
    }

    #[test]
    fn auditor_detects_bsu_without_admissions() {
        let (orch, mut wd) = fresh_telemetry();
        // BSU opened on watchdog but no bytes admitted.
        wd.record_depth_outcome(BsuDepthOutcome::Opened { new_depth: 1 }, 1);

        let auditor = TelemetryDriftAuditor::new(AuditorConfig::default());
        let findings = auditor.audit(&orch, &wd);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0],
            TelemetryDrift::BsuWithoutAdmissions { bsu_count: 1 }
        ));
    }

    #[test]
    fn auditor_can_disable_bsu_without_admissions_check() {
        let (orch, mut wd) = fresh_telemetry();
        wd.record_depth_outcome(BsuDepthOutcome::Opened { new_depth: 1 }, 1);

        let cfg = AuditorConfig {
            expect_bytes_per_bsu: false,
            ..AuditorConfig::default()
        };
        let auditor = TelemetryDriftAuditor::new(cfg);
        assert!(
            auditor.invariants_hold(&orch, &wd),
            "disabling expect_bytes_per_bsu should silence the BsuWithoutAdmissions finding",
        );
    }

    #[test]
    fn auditor_can_disable_byte_count_parity_check() {
        let (mut orch, mut wd) = fresh_telemetry();
        wd.record_depth_outcome(BsuDepthOutcome::Opened { new_depth: 1 }, 1);
        // Drift on byte count — orchestrator records 100, watchdog 0.
        orch.record_admission(BufferAdmissionDecision::Accepted, 100);

        let cfg = AuditorConfig {
            expect_byte_count_parity: false,
            ..AuditorConfig::default()
        };
        let auditor = TelemetryDriftAuditor::new(cfg);
        assert!(auditor.invariants_hold(&orch, &wd));
    }

    // ----------------------------------------------------------------
    // End-to-end: clean BSU lifecycle
    // ----------------------------------------------------------------

    #[test]
    fn scenario_clean_bsu_lifecycle_passes_audit() {
        let (mut orch, mut wd) = fresh_telemetry();

        // BSU opens.
        wd.record_depth_outcome(BsuDepthOutcome::Opened { new_depth: 1 }, 1);

        // 4 chunks admitted.
        for chunk in [128u64, 256, 512, 1024] {
            let dec = BufferAdmissionDecision::Accepted;
            orch.record_admission(dec, chunk);
            forward_admission(dec, chunk, &mut wd);
        }

        // ESU drains (Flushed because depth went to 0).
        forward_drain(
            BufferDrainOutcome::Drained {
                bytes: 128 + 256 + 512 + 1024,
                cause: DrainCause::Esu,
            },
            BsuDepthOutcome::Flushed,
            1,
            &mut wd,
        );

        // Audit passes.
        let auditor = TelemetryDriftAuditor::new(AuditorConfig::default());
        assert!(auditor.invariants_hold(&orch, &wd));
        // Concrete counter values for posterity.
        assert_eq!(wd.bsu_count(), 1);
        assert_eq!(wd.esu_count(), 1);
        assert_eq!(wd.mid_bsu_byte_count(), 128 + 256 + 512 + 1024);
        assert_eq!(orch.bytes_accepted, 128 + 256 + 512 + 1024);
    }

    #[test]
    fn scenario_refused_chunks_do_not_drift_counters() {
        // 1 accepted + 2 refused → both counters in sync at
        // 1024 bytes; the refused chunks don't bump either.
        let (mut orch, mut wd) = fresh_telemetry();
        wd.record_depth_outcome(BsuDepthOutcome::Opened { new_depth: 1 }, 1);

        let accepted = BufferAdmissionDecision::Accepted;
        orch.record_admission(accepted, 1024);
        forward_admission(accepted, 1024, &mut wd);

        let refused = BufferAdmissionDecision::Refused;
        orch.record_admission(refused, 4096);
        forward_admission(refused, 4096, &mut wd);
        orch.record_admission(refused, 8192);
        forward_admission(refused, 8192, &mut wd);

        assert_eq!(wd.mid_bsu_byte_count(), 1024);
        assert_eq!(orch.bytes_accepted, 1024);
        assert_eq!(orch.bytes_refused, 4096 + 8192);

        let auditor = TelemetryDriftAuditor::new(AuditorConfig::default());
        assert!(auditor.invariants_hold(&orch, &wd));
    }
}
