//! Crash/restart persistence invariants and recovery replay gate (ft-e34d9.10.6.6).
//!
//! Validates crash-consistency and restart recovery so users don't lose
//! critical session/capture/search state under abrupt termination, restart
//! loops, or partial I/O failure.
//!
//! # Architecture
//!
//! ```text
//! PersistenceInvariant
//!   ├── CaptureFlush (no buffered data lost)
//!   ├── SessionCheckpoint (state recoverable)
//!   ├── SearchIndexSync (index matches storage)
//!   ├── EventQueueDrain (acknowledged events delivered)
//!   └── ControlPlaneAck (acknowledged ops committed)
//!
//! RecoveryScenario
//!   ├── CleanShutdown → RestartVerify
//!   ├── SigkillCrash → RecoveryVerify
//!   ├── PartialWrite → ConsistencyVerify
//!   └── RestartLoop → StabilityVerify
//!
//! RecoveryGate
//!   └── evaluate(invariants, scenarios) → GateVerdict
//! ```

use serde::{Deserialize, Serialize};

// =============================================================================
// Persistence invariants
// =============================================================================

/// Persistence invariant identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PersistenceInvariantId {
    /// Capture pipeline flushes all buffered data before checkpoint.
    CaptureFlush,
    /// Session state is checkpointed and recoverable.
    SessionCheckpoint,
    /// Search index is consistent with storage after recovery.
    SearchIndexSync,
    /// Acknowledged events in the queue are durably committed.
    EventQueueDrain,
    /// Control-plane acknowledgements are committed before response.
    ControlPlaneAck,
    /// WAL entries are fsync'd before commit acknowledgement.
    WalFsync,
    /// In-flight transactions are either fully committed or fully rolled back.
    TransactionAtomicity,
}

impl PersistenceInvariantId {
    /// Canonical string identifier.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CaptureFlush => "PERSIST-01-capture-flush",
            Self::SessionCheckpoint => "PERSIST-02-session-checkpoint",
            Self::SearchIndexSync => "PERSIST-03-search-index-sync",
            Self::EventQueueDrain => "PERSIST-04-event-queue-drain",
            Self::ControlPlaneAck => "PERSIST-05-control-plane-ack",
            Self::WalFsync => "PERSIST-06-wal-fsync",
            Self::TransactionAtomicity => "PERSIST-07-transaction-atomicity",
        }
    }

    /// All invariant IDs.
    #[must_use]
    pub fn all() -> &'static [PersistenceInvariantId] {
        &[
            Self::CaptureFlush,
            Self::SessionCheckpoint,
            Self::SearchIndexSync,
            Self::EventQueueDrain,
            Self::ControlPlaneAck,
            Self::WalFsync,
            Self::TransactionAtomicity,
        ]
    }
}

/// A persistence invariant definition with verification criteria.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceInvariant {
    /// Invariant identifier.
    pub id: PersistenceInvariantId,
    /// Human-readable description.
    pub description: String,
    /// What data/state this invariant protects.
    pub protected_resource: String,
    /// How to verify this invariant holds.
    pub verification_method: String,
    /// Whether failure of this invariant is data-loss (critical).
    pub data_loss_risk: bool,
}

/// Standard persistence invariants for the asupersync migration.
#[must_use]
pub fn standard_invariants() -> Vec<PersistenceInvariant> {
    vec![
        PersistenceInvariant {
            id: PersistenceInvariantId::CaptureFlush,
            description: "Capture pipeline flushes all buffered output before checkpoint".into(),
            protected_resource: "Pane output deltas in capture buffer".into(),
            verification_method: "Compare buffered count before crash with persisted count after recovery".into(),
            data_loss_risk: true,
        },
        PersistenceInvariant {
            id: PersistenceInvariantId::SessionCheckpoint,
            description: "Session state is recoverable from last checkpoint".into(),
            protected_resource: "Pane layout, positions, and metadata".into(),
            verification_method: "Restore session after crash and diff with pre-crash snapshot".into(),
            data_loss_risk: true,
        },
        PersistenceInvariant {
            id: PersistenceInvariantId::SearchIndexSync,
            description: "Search index is consistent with storage after recovery".into(),
            protected_resource: "FTS5 search index and backing SQLite rows".into(),
            verification_method: "Query known-ingested content and verify all expected results appear".into(),
            data_loss_risk: false,
        },
        PersistenceInvariant {
            id: PersistenceInvariantId::EventQueueDrain,
            description: "Acknowledged events are durably committed".into(),
            protected_resource: "Event bus queue entries with delivery acknowledgement".into(),
            verification_method: "Count acknowledged events before crash; verify all present after recovery".into(),
            data_loss_risk: true,
        },
        PersistenceInvariant {
            id: PersistenceInvariantId::ControlPlaneAck,
            description: "Control-plane operations are committed before acknowledgement".into(),
            protected_resource: "Pane operations (split, close, resize) state".into(),
            verification_method: "Issue operation, receive ack, crash, verify operation state persisted".into(),
            data_loss_risk: false,
        },
        PersistenceInvariant {
            id: PersistenceInvariantId::WalFsync,
            description: "WAL entries are fsync'd before commit acknowledgement".into(),
            protected_resource: "Write-ahead log entries".into(),
            verification_method: "Write entry, receive commit ack, power-kill, verify WAL entry present".into(),
            data_loss_risk: true,
        },
        PersistenceInvariant {
            id: PersistenceInvariantId::TransactionAtomicity,
            description: "In-flight transactions are either fully committed or fully rolled back".into(),
            protected_resource: "Multi-step storage operations".into(),
            verification_method: "Start multi-step op, crash mid-way, verify no partial state".into(),
            data_loss_risk: true,
        },
    ]
}

// =============================================================================
// Recovery scenarios
// =============================================================================

/// Type of crash/restart scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CrashScenarioType {
    /// Clean shutdown and restart (baseline).
    CleanShutdown,
    /// SIGKILL (immediate process death, no cleanup).
    Sigkill,
    /// Partial write (crash during storage write).
    PartialWrite,
    /// Restart loop (3+ rapid restarts).
    RestartLoop,
    /// I/O fault during checkpoint.
    IoFaultDuringCheckpoint,
    /// Disk-full during write.
    DiskFull,
    /// Corrupted checkpoint file.
    CorruptedCheckpoint,
}

impl CrashScenarioType {
    /// Human-readable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::CleanShutdown => "clean-shutdown",
            Self::Sigkill => "sigkill",
            Self::PartialWrite => "partial-write",
            Self::RestartLoop => "restart-loop",
            Self::IoFaultDuringCheckpoint => "io-fault-checkpoint",
            Self::DiskFull => "disk-full",
            Self::CorruptedCheckpoint => "corrupted-checkpoint",
        }
    }

    /// All scenario types.
    #[must_use]
    pub fn all() -> &'static [CrashScenarioType] {
        &[
            Self::CleanShutdown,
            Self::Sigkill,
            Self::PartialWrite,
            Self::RestartLoop,
            Self::IoFaultDuringCheckpoint,
            Self::DiskFull,
            Self::CorruptedCheckpoint,
        ]
    }
}

/// A recovery scenario definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryScenario {
    /// Scenario identifier.
    pub scenario_id: String,
    /// Crash type.
    pub crash_type: CrashScenarioType,
    /// Description of what this scenario tests.
    pub description: String,
    /// Which invariants this scenario validates.
    pub validates_invariants: Vec<PersistenceInvariantId>,
    /// Expected recovery outcome.
    pub expected_outcome: RecoveryOutcome,
}

/// Expected outcome after recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryOutcome {
    /// Full recovery — all data preserved, all services healthy.
    FullRecovery,
    /// Partial recovery — most data preserved, some replay needed.
    PartialRecovery,
    /// Degraded recovery — service healthy but some data re-indexing needed.
    DegradedRecovery,
    /// Graceful failure — service detects unrecoverable state, reports clearly.
    GracefulFailure,
}

/// Standard recovery scenarios.
#[must_use]
pub fn standard_recovery_scenarios() -> Vec<RecoveryScenario> {
    vec![
        RecoveryScenario {
            scenario_id: "CRASH-001-clean".into(),
            crash_type: CrashScenarioType::CleanShutdown,
            description: "Graceful shutdown and restart — baseline".into(),
            validates_invariants: PersistenceInvariantId::all().to_vec(),
            expected_outcome: RecoveryOutcome::FullRecovery,
        },
        RecoveryScenario {
            scenario_id: "CRASH-002-sigkill".into(),
            crash_type: CrashScenarioType::Sigkill,
            description: "SIGKILL during active capture and search".into(),
            validates_invariants: vec![
                PersistenceInvariantId::CaptureFlush,
                PersistenceInvariantId::SessionCheckpoint,
                PersistenceInvariantId::WalFsync,
                PersistenceInvariantId::TransactionAtomicity,
            ],
            expected_outcome: RecoveryOutcome::PartialRecovery,
        },
        RecoveryScenario {
            scenario_id: "CRASH-003-partial-write".into(),
            crash_type: CrashScenarioType::PartialWrite,
            description: "Crash during multi-row storage write".into(),
            validates_invariants: vec![
                PersistenceInvariantId::TransactionAtomicity,
                PersistenceInvariantId::WalFsync,
                PersistenceInvariantId::SearchIndexSync,
            ],
            expected_outcome: RecoveryOutcome::FullRecovery,
        },
        RecoveryScenario {
            scenario_id: "CRASH-004-restart-loop".into(),
            crash_type: CrashScenarioType::RestartLoop,
            description: "3 rapid restarts — convergence check".into(),
            validates_invariants: vec![
                PersistenceInvariantId::SessionCheckpoint,
                PersistenceInvariantId::SearchIndexSync,
            ],
            expected_outcome: RecoveryOutcome::FullRecovery,
        },
        RecoveryScenario {
            scenario_id: "CRASH-005-io-fault".into(),
            crash_type: CrashScenarioType::IoFaultDuringCheckpoint,
            description: "I/O error during checkpoint write".into(),
            validates_invariants: vec![
                PersistenceInvariantId::SessionCheckpoint,
                PersistenceInvariantId::WalFsync,
            ],
            expected_outcome: RecoveryOutcome::PartialRecovery,
        },
        RecoveryScenario {
            scenario_id: "CRASH-006-disk-full".into(),
            crash_type: CrashScenarioType::DiskFull,
            description: "Disk full during capture ingestion".into(),
            validates_invariants: vec![
                PersistenceInvariantId::CaptureFlush,
                PersistenceInvariantId::EventQueueDrain,
            ],
            expected_outcome: RecoveryOutcome::DegradedRecovery,
        },
        RecoveryScenario {
            scenario_id: "CRASH-007-corrupted".into(),
            crash_type: CrashScenarioType::CorruptedCheckpoint,
            description: "Corrupted checkpoint file at startup".into(),
            validates_invariants: vec![
                PersistenceInvariantId::SessionCheckpoint,
            ],
            expected_outcome: RecoveryOutcome::GracefulFailure,
        },
    ]
}

// =============================================================================
// Recovery gate evaluation
// =============================================================================

/// Result of verifying one invariant in a scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantResult {
    /// Invariant ID.
    pub invariant_id: PersistenceInvariantId,
    /// Whether the invariant held.
    pub held: bool,
    /// Evidence for the result.
    pub evidence: String,
    /// Whether this invariant's failure means data loss.
    pub data_loss: bool,
}

/// Result of running one recovery scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    /// Scenario ID.
    pub scenario_id: String,
    /// Crash type.
    pub crash_type: CrashScenarioType,
    /// Whether recovery met expected outcome.
    pub met_expectation: bool,
    /// Actual recovery outcome.
    pub actual_outcome: RecoveryOutcome,
    /// Expected outcome.
    pub expected_outcome: RecoveryOutcome,
    /// Per-invariant results.
    pub invariant_results: Vec<InvariantResult>,
    /// Recovery time in milliseconds.
    pub recovery_time_ms: u64,
}

impl ScenarioResult {
    /// Whether any data loss was detected.
    #[must_use]
    pub fn has_data_loss(&self) -> bool {
        self.invariant_results
            .iter()
            .any(|r| !r.held && r.data_loss)
    }

    /// Count of invariants that held.
    #[must_use]
    pub fn invariants_held(&self) -> usize {
        self.invariant_results.iter().filter(|r| r.held).count()
    }
}

/// Overall gate verdict for crash persistence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PersistenceGateVerdict {
    /// All scenarios pass, all invariants hold.
    Pass,
    /// Minor invariant violations (no data loss risk).
    ConditionalPass,
    /// Data loss detected or critical scenario failed.
    Fail,
}

/// Complete gate evaluation report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceGateReport {
    /// Report identifier.
    pub report_id: String,
    /// When evaluation was performed.
    pub evaluated_at_ms: u64,
    /// Per-scenario results.
    pub results: Vec<ScenarioResult>,
    /// Overall verdict.
    pub verdict: PersistenceGateVerdict,
    /// Counts.
    pub total_scenarios: usize,
    pub passed_scenarios: usize,
    pub failed_scenarios: usize,
    /// Whether any data loss was detected.
    pub any_data_loss: bool,
    /// Max recovery time observed.
    pub max_recovery_time_ms: u64,
}

impl PersistenceGateReport {
    /// Evaluate a set of scenario results.
    #[must_use]
    pub fn evaluate(results: Vec<ScenarioResult>) -> Self {
        let total_scenarios = results.len();
        let passed_scenarios = results.iter().filter(|r| r.met_expectation).count();
        let failed_scenarios = total_scenarios - passed_scenarios;
        let any_data_loss = results.iter().any(|r| r.has_data_loss());
        let max_recovery_time_ms = results.iter().map(|r| r.recovery_time_ms).max().unwrap_or(0);

        let verdict = if any_data_loss {
            PersistenceGateVerdict::Fail
        } else if failed_scenarios == 0 {
            PersistenceGateVerdict::Pass
        } else {
            // Check if failures are only in non-data-loss invariants.
            let critical_failures = results
                .iter()
                .filter(|r| !r.met_expectation)
                .any(|r| r.invariant_results.iter().any(|i| !i.held && i.data_loss));

            if critical_failures {
                PersistenceGateVerdict::Fail
            } else {
                PersistenceGateVerdict::ConditionalPass
            }
        };

        Self {
            report_id: "crash-persistence-gate".into(),
            evaluated_at_ms: 0,
            results,
            verdict,
            total_scenarios,
            passed_scenarios,
            failed_scenarios,
            any_data_loss,
            max_recovery_time_ms,
        }
    }

    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("=== Crash Persistence Gate: {} ===", self.report_id));
        lines.push(format!("Verdict: {:?}", self.verdict));
        lines.push(format!(
            "Scenarios: {}/{} passed",
            self.passed_scenarios, self.total_scenarios
        ));
        lines.push(format!("Data loss detected: {}", self.any_data_loss));
        lines.push(format!("Max recovery time: {}ms", self.max_recovery_time_ms));

        lines.push(String::new());
        for result in &self.results {
            let status = if result.met_expectation { "PASS" } else { "FAIL" };
            let data_loss = if result.has_data_loss() { " [DATA LOSS]" } else { "" };
            lines.push(format!(
                "  [{}] {} ({}) — {}/{} invariants held, recovery {}ms{}",
                status,
                result.scenario_id,
                result.crash_type.label(),
                result.invariants_held(),
                result.invariant_results.len(),
                result.recovery_time_ms,
                data_loss,
            ));
        }

        lines.join("\n")
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn passing_scenario_result(scenario: &RecoveryScenario) -> ScenarioResult {
        let invariant_results: Vec<InvariantResult> = scenario
            .validates_invariants
            .iter()
            .map(|inv_id| {
                let inv = standard_invariants().into_iter().find(|i| i.id == *inv_id);
                InvariantResult {
                    invariant_id: *inv_id,
                    held: true,
                    evidence: "Invariant verified".into(),
                    data_loss: inv.map(|i| i.data_loss_risk).unwrap_or(false),
                }
            })
            .collect();

        ScenarioResult {
            scenario_id: scenario.scenario_id.clone(),
            crash_type: scenario.crash_type,
            met_expectation: true,
            actual_outcome: scenario.expected_outcome,
            expected_outcome: scenario.expected_outcome,
            invariant_results,
            recovery_time_ms: 500,
        }
    }

    #[test]
    fn standard_invariants_cover_all_ids() {
        let invariants = standard_invariants();
        let all_ids = PersistenceInvariantId::all();
        for id in all_ids {
            assert!(
                invariants.iter().any(|i| i.id == *id),
                "missing invariant {:?}",
                id
            );
        }
    }

    #[test]
    fn standard_scenarios_cover_all_crash_types() {
        let scenarios = standard_recovery_scenarios();
        let all_types = CrashScenarioType::all();
        for ct in all_types {
            assert!(
                scenarios.iter().any(|s| s.crash_type == *ct),
                "missing crash type {:?}",
                ct
            );
        }
    }

    #[test]
    fn standard_scenarios_have_invariants() {
        let scenarios = standard_recovery_scenarios();
        for s in &scenarios {
            assert!(
                !s.validates_invariants.is_empty(),
                "{} has no invariants",
                s.scenario_id
            );
        }
    }

    #[test]
    fn gate_report_all_pass() {
        let scenarios = standard_recovery_scenarios();
        let results: Vec<ScenarioResult> =
            scenarios.iter().map(|s| passing_scenario_result(s)).collect();
        let report = PersistenceGateReport::evaluate(results);
        assert_eq!(report.verdict, PersistenceGateVerdict::Pass);
        assert!(!report.any_data_loss);
    }

    #[test]
    fn gate_report_fail_on_data_loss() {
        let scenarios = standard_recovery_scenarios();
        let mut results: Vec<ScenarioResult> =
            scenarios.iter().map(|s| passing_scenario_result(s)).collect();

        // Simulate data loss in sigkill scenario.
        if let Some(r) = results.iter_mut().find(|r| r.crash_type == CrashScenarioType::Sigkill) {
            r.met_expectation = false;
            if let Some(inv) = r.invariant_results.iter_mut().find(|i| i.data_loss) {
                inv.held = false;
            }
        }

        let report = PersistenceGateReport::evaluate(results);
        assert_eq!(report.verdict, PersistenceGateVerdict::Fail);
        assert!(report.any_data_loss);
    }

    #[test]
    fn gate_report_conditional_on_non_critical_failure() {
        let scenarios = standard_recovery_scenarios();
        let mut results: Vec<ScenarioResult> =
            scenarios.iter().map(|s| passing_scenario_result(s)).collect();

        // Fail a scenario but only on non-data-loss invariant.
        if let Some(r) = results.iter_mut().find(|r| r.crash_type == CrashScenarioType::RestartLoop) {
            r.met_expectation = false;
            // SearchIndexSync is not data_loss_risk.
            for inv in &mut r.invariant_results {
                if !inv.data_loss {
                    inv.held = false;
                }
            }
        }

        let report = PersistenceGateReport::evaluate(results);
        assert_eq!(report.verdict, PersistenceGateVerdict::ConditionalPass);
    }

    #[test]
    fn scenario_result_data_loss_detection() {
        let result = ScenarioResult {
            scenario_id: "test".into(),
            crash_type: CrashScenarioType::Sigkill,
            met_expectation: false,
            actual_outcome: RecoveryOutcome::PartialRecovery,
            expected_outcome: RecoveryOutcome::FullRecovery,
            invariant_results: vec![
                InvariantResult {
                    invariant_id: PersistenceInvariantId::CaptureFlush,
                    held: false,
                    evidence: "Data lost".into(),
                    data_loss: true,
                },
            ],
            recovery_time_ms: 1000,
        };
        assert!(result.has_data_loss());
    }

    #[test]
    fn scenario_result_no_data_loss_when_held() {
        let result = ScenarioResult {
            scenario_id: "test".into(),
            crash_type: CrashScenarioType::CleanShutdown,
            met_expectation: true,
            actual_outcome: RecoveryOutcome::FullRecovery,
            expected_outcome: RecoveryOutcome::FullRecovery,
            invariant_results: vec![
                InvariantResult {
                    invariant_id: PersistenceInvariantId::CaptureFlush,
                    held: true,
                    evidence: "OK".into(),
                    data_loss: true,
                },
            ],
            recovery_time_ms: 100,
        };
        assert!(!result.has_data_loss());
    }

    #[test]
    fn max_recovery_time_tracked() {
        let scenarios = standard_recovery_scenarios();
        let mut results: Vec<ScenarioResult> =
            scenarios.iter().map(|s| passing_scenario_result(s)).collect();
        results[0].recovery_time_ms = 5000;
        let report = PersistenceGateReport::evaluate(results);
        assert_eq!(report.max_recovery_time_ms, 5000);
    }

    #[test]
    fn invariant_ids_unique() {
        let all = PersistenceInvariantId::all();
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a.as_str(), b.as_str());
                }
            }
        }
    }

    #[test]
    fn crash_scenario_type_labels_unique() {
        let all = CrashScenarioType::all();
        let labels: Vec<&str> = all.iter().map(|c| c.label()).collect();
        for (i, a) in labels.iter().enumerate() {
            for (j, b) in labels.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn render_summary_shows_pass() {
        let scenarios = standard_recovery_scenarios();
        let results: Vec<ScenarioResult> =
            scenarios.iter().map(|s| passing_scenario_result(s)).collect();
        let report = PersistenceGateReport::evaluate(results);
        let summary = report.render_summary();
        assert!(summary.contains("Pass"));
        assert!(summary.contains("PASS"));
    }

    #[test]
    fn render_summary_shows_data_loss() {
        let scenarios = standard_recovery_scenarios();
        let mut results: Vec<ScenarioResult> =
            scenarios.iter().map(|s| passing_scenario_result(s)).collect();
        if let Some(r) = results.iter_mut().find(|r| r.crash_type == CrashScenarioType::Sigkill) {
            r.met_expectation = false;
            if let Some(inv) = r.invariant_results.iter_mut().find(|i| i.data_loss) {
                inv.held = false;
            }
        }
        let report = PersistenceGateReport::evaluate(results);
        let summary = report.render_summary();
        assert!(summary.contains("DATA LOSS"));
    }

    #[test]
    fn serde_roundtrip_gate_report() {
        let scenarios = standard_recovery_scenarios();
        let results: Vec<ScenarioResult> =
            scenarios.iter().map(|s| passing_scenario_result(s)).collect();
        let report = PersistenceGateReport::evaluate(results);
        let json = serde_json::to_string(&report).expect("serialize");
        let restored: PersistenceGateReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.verdict, report.verdict);
        assert_eq!(restored.results.len(), report.results.len());
    }

    #[test]
    fn serde_roundtrip_scenarios() {
        let scenarios = standard_recovery_scenarios();
        let json = serde_json::to_string(&scenarios).expect("serialize");
        let restored: Vec<RecoveryScenario> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.len(), scenarios.len());
    }

    #[test]
    fn data_loss_invariants_exist() {
        let invariants = standard_invariants();
        let data_loss_count = invariants.iter().filter(|i| i.data_loss_risk).count();
        assert!(data_loss_count >= 3, "expected >= 3 data-loss invariants");
    }

    #[test]
    fn clean_shutdown_validates_all_invariants() {
        let scenarios = standard_recovery_scenarios();
        let clean = scenarios.iter().find(|s| s.crash_type == CrashScenarioType::CleanShutdown).unwrap();
        assert_eq!(clean.validates_invariants.len(), PersistenceInvariantId::all().len());
    }
}
