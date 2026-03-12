//! Core↔vendored async contract and compatibility tests (ft-e34d9.10.5.4).
//!
//! Defines and enforces async contract boundaries between core and vendored
//! layers — ownership, cancellation, channeling, error mapping, backpressure,
//! timeout, and task lifecycle.
//!
//! # Architecture
//!
//! ```text
//! AsyncBoundaryContract
//!   ├── ContractCategory (Ownership/Cancellation/Channeling/ErrorMapping/
//!   │                     Backpressure/Timeout/TaskLifecycle)
//!   ├── BoundaryDirection (CoreToVendored/VendoredToCore/Bidirectional)
//!   └── invariant + violation_impact + verifiable flag
//!
//! ContractCompliance
//!   ├── contract: AsyncBoundaryContract
//!   ├── evidence: Vec<ContractEvidence>
//!   ├── compliant: bool   (all evidence passed)
//!   └── coverage: f64     (passed / total)
//!
//! CompatibilityMapping
//!   ├── compat_api: String         (from SURFACE_CONTRACT_V1)
//!   └── satisfies_contracts: Vec<String>
//!
//! ContractAuditReport
//!   ├── contracts: Vec<ContractCompliance>
//!   ├── surface_status: SurfaceContractStatus
//!   ├── overall_compliant: bool
//!   ├── compliance_rate: f64
//!   └── uncovered_contracts: Vec<String>
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::dependency_eradication::SurfaceContractStatus;

// =============================================================================
// Direction / Category
// =============================================================================

/// Direction of async operation flow across the crate boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundaryDirection {
    /// Core calls into the vendored mux layer.
    CoreToVendored,
    /// Vendored mux calls back into core.
    VendoredToCore,
    /// Flow occurs in both directions.
    Bidirectional,
}

/// Async contract category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ContractCategory {
    /// Who owns the async task / future.
    Ownership,
    /// Cancellation propagation rules.
    Cancellation,
    /// Channel type and protocol contracts.
    Channeling,
    /// Error type mapping across the boundary.
    ErrorMapping,
    /// Backpressure signal propagation.
    Backpressure,
    /// Timeout inheritance rules.
    Timeout,
    /// Spawn / join / detach contract.
    TaskLifecycle,
}

// =============================================================================
// AsyncBoundaryContract
// =============================================================================

/// A single async boundary contract between core and the vendored layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsyncBoundaryContract {
    /// Unique contract identifier (e.g. `"ABC-OWN-001"`).
    pub contract_id: String,
    /// Category of this contract.
    pub category: ContractCategory,
    /// Direction of the operation flow this contract governs.
    pub direction: BoundaryDirection,
    /// Human-readable description of what this contract covers.
    pub description: String,
    /// The invariant rule that must hold.
    pub invariant: String,
    /// What happens if this contract is violated.
    pub violation_impact: String,
    /// Whether this contract can be verified at compile time or in tests.
    pub verifiable: bool,
}

/// Standard set of async boundary contracts for the Core↔Vendored boundary.
///
/// Returns 12 contracts covering all seven [`ContractCategory`] variants.
#[must_use]
pub fn standard_contracts() -> Vec<AsyncBoundaryContract> {
    vec![
        AsyncBoundaryContract {
            contract_id: "ABC-OWN-001".into(),
            category: ContractCategory::Ownership,
            direction: BoundaryDirection::Bidirectional,
            description: "Task ownership stays with spawner".into(),
            invariant: "The crate that spawns a task retains its JoinHandle and is responsible for driving it to completion or cancelling it.".into(),
            violation_impact: "Leaked tasks, silent data loss, or unbounded resource consumption.".into(),
            verifiable: true,
        },
        AsyncBoundaryContract {
            contract_id: "ABC-OWN-002".into(),
            category: ContractCategory::Ownership,
            direction: BoundaryDirection::Bidirectional,
            description: "Futures must not outlive their spawning scope".into(),
            invariant: "No future produced by core or vendored may hold references that outlive the spawning lexical scope without explicit Arc/owned cloning.".into(),
            violation_impact: "Use-after-free analogues, dangling references, or runtime panics under async cancellation.".into(),
            verifiable: true,
        },
        AsyncBoundaryContract {
            contract_id: "ABC-CAN-001".into(),
            category: ContractCategory::Cancellation,
            direction: BoundaryDirection::Bidirectional,
            description: "Cancellation must propagate within 50ms".into(),
            invariant: "When a cancellation signal is issued at the boundary, all in-flight futures on the receiving side must observe the cancellation within 50 milliseconds.".into(),
            violation_impact: "Stalled shutdown sequences, resource leaks, and delayed process termination.".into(),
            verifiable: true,
        },
        AsyncBoundaryContract {
            contract_id: "ABC-CAN-002".into(),
            category: ContractCategory::Cancellation,
            direction: BoundaryDirection::Bidirectional,
            description: "Drop of handle implies cancellation".into(),
            invariant: "Dropping a task handle or JoinHandle must trigger cancellation of the underlying future; no background task may continue running after its handle is dropped.".into(),
            violation_impact: "Zombie background tasks, resource exhaustion, and non-deterministic test failures.".into(),
            verifiable: true,
        },
        AsyncBoundaryContract {
            contract_id: "ABC-CHN-001".into(),
            category: ContractCategory::Channeling,
            direction: BoundaryDirection::Bidirectional,
            description: "Channels use runtime_compat wrappers, not raw tokio channels".into(),
            invariant: "All channels crossing the Core↔Vendored boundary must be created via runtime_compat channel wrappers (mpsc_send, mpsc_recv_option, etc.), never via raw tokio::sync::mpsc.".into(),
            violation_impact: "Runtime mismatch panics and data races when running under asupersync.".into(),
            verifiable: true,
        },
        AsyncBoundaryContract {
            contract_id: "ABC-CHN-002".into(),
            category: ContractCategory::Channeling,
            direction: BoundaryDirection::Bidirectional,
            description: "Channel close is non-lossy: all buffered items delivered".into(),
            invariant: "When a channel sender is closed, all items already buffered in the channel must be delivered to the receiver before the channel reports closure.".into(),
            violation_impact: "Silent message loss leading to incomplete processing, state corruption.".into(),
            verifiable: true,
        },
        AsyncBoundaryContract {
            contract_id: "ABC-ERR-001".into(),
            category: ContractCategory::ErrorMapping,
            direction: BoundaryDirection::VendoredToCore,
            description: "Vendored errors map to frankenterm_core::Error variants".into(),
            invariant: "All error types returned from vendored layer functions that cross into core must be converted to `frankenterm_core::Error` via an explicit From/Into impl or adapter function.".into(),
            violation_impact: "Type-system leakage of vendored internals into core public API; breaking changes on vendored version bumps.".into(),
            verifiable: true,
        },
        AsyncBoundaryContract {
            contract_id: "ABC-ERR-002".into(),
            category: ContractCategory::ErrorMapping,
            direction: BoundaryDirection::Bidirectional,
            description: "Error context preserved across boundary".into(),
            invariant: "Error values crossing the boundary must preserve their causal chain (source error, span context, or equivalent) — wrapping must not discard context.".into(),
            violation_impact: "Silent loss of diagnostic information making post-incident analysis impossible.".into(),
            verifiable: false,
        },
        AsyncBoundaryContract {
            contract_id: "ABC-BP-001".into(),
            category: ContractCategory::Backpressure,
            direction: BoundaryDirection::VendoredToCore,
            description: "Backpressure signals propagate from vendored to core".into(),
            invariant: "When the vendored mux layer signals backpressure (e.g., full channel, blocked writer), the core layer must observe this signal and pause submission within one event-loop tick.".into(),
            violation_impact: "Unbounded memory growth, OOM conditions, and cascading latency spikes.".into(),
            verifiable: true,
        },
        AsyncBoundaryContract {
            contract_id: "ABC-TO-001".into(),
            category: ContractCategory::Timeout,
            direction: BoundaryDirection::CoreToVendored,
            description: "Timeout from core overrides vendored internal timeouts".into(),
            invariant: "When core specifies a deadline or timeout for a cross-boundary call, the vendored layer must honour that deadline and must not extend it with its own internal retry or wait logic.".into(),
            violation_impact: "Operations outliving their deadline, breaking caller SLA guarantees and causing cascading failures.".into(),
            verifiable: true,
        },
        AsyncBoundaryContract {
            contract_id: "ABC-TL-001".into(),
            category: ContractCategory::TaskLifecycle,
            direction: BoundaryDirection::Bidirectional,
            description: "Spawned tasks must be tracked for graceful shutdown".into(),
            invariant: "Every task spawned across or at the Core↔Vendored boundary must be registered with a task tracker that participates in the graceful-shutdown sequence.".into(),
            violation_impact: "Tasks surviving process shutdown, causing data corruption or incomplete flushing.".into(),
            verifiable: true,
        },
        AsyncBoundaryContract {
            contract_id: "ABC-TL-002".into(),
            category: ContractCategory::TaskLifecycle,
            direction: BoundaryDirection::Bidirectional,
            description: "Detached tasks are forbidden in production paths".into(),
            invariant: "No production code path may call `spawn_detached` or equivalent fire-and-forget spawning across the boundary; all tasks must have an owned handle with a defined cancellation path.".into(),
            violation_impact: "Untracked tasks leading to resource leaks, unpredictable shutdown ordering, and hidden concurrency bugs.".into(),
            verifiable: true,
        },
    ]
}

// =============================================================================
// ContractEvidence
// =============================================================================

/// Type of evidence supporting (or refuting) contract compliance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceType {
    /// Evidence from a unit test.
    UnitTest,
    /// Evidence from an integration test.
    IntegrationTest,
    /// Evidence from static analysis tooling.
    StaticAnalysis,
    /// Evidence from a manual code review.
    CodeReview,
    /// Evidence from a runtime assertion or invariant check.
    RuntimeAssertion,
}

/// A single piece of evidence for or against a contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractEvidence {
    /// The contract this evidence applies to.
    pub contract_id: String,
    /// Name of the test or analysis that produced this evidence.
    pub test_name: String,
    /// Whether the evidence indicates the contract was satisfied.
    pub passed: bool,
    /// How this evidence was gathered.
    pub evidence_type: EvidenceType,
    /// Additional detail about the outcome.
    pub detail: String,
}

// =============================================================================
// ContractCompliance
// =============================================================================

/// Compliance result for a single [`AsyncBoundaryContract`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractCompliance {
    /// The contract being evaluated.
    pub contract: AsyncBoundaryContract,
    /// Evidence gathered for this contract.
    pub evidence: Vec<ContractEvidence>,
    /// Whether the contract is considered compliant (all evidence passed).
    pub compliant: bool,
    /// Fraction of evidence that passed (0.0 – 1.0).
    pub coverage: f64,
}

impl ContractCompliance {
    /// Build a [`ContractCompliance`] from a contract and its evidence.
    ///
    /// `compliant` is `true` iff every piece of evidence passed.
    /// `coverage` is `passed_count / total_count`, or `0.0` when there is no
    /// evidence.
    #[must_use]
    pub fn from_evidence(contract: AsyncBoundaryContract, evidence: Vec<ContractEvidence>) -> Self {
        let total = evidence.len();
        let passed = evidence.iter().filter(|e| e.passed).count();

        let compliant = !evidence.is_empty() && passed == total;
        let coverage = if total == 0 {
            0.0
        } else {
            passed as f64 / total as f64
        };

        Self {
            contract,
            evidence,
            compliant,
            coverage,
        }
    }
}

// =============================================================================
// CompatibilityMapping
// =============================================================================

/// Maps a single `runtime_compat` API to the async boundary contracts it satisfies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompatibilityMapping {
    /// API name exactly as it appears in `SURFACE_CONTRACT_V1`.
    pub compat_api: String,
    /// Contract IDs (from [`standard_contracts`]) that this API satisfies.
    pub satisfies_contracts: Vec<String>,
    /// Whether the API's `SurfaceDisposition` is aligned with its contract
    /// direction (e.g., a `Keep` API that satisfies a permanent contract is
    /// aligned; a `Retire` API that satisfies a permanent contract is not).
    pub disposition_aligned: bool,
}

/// Standard compatibility mappings between `runtime_compat` APIs and async
/// boundary contracts.
///
/// Covers all 15 entries in `SURFACE_CONTRACT_V1`.
#[must_use]
pub fn standard_compatibility_mappings() -> Vec<CompatibilityMapping> {
    vec![
        CompatibilityMapping {
            compat_api: "RuntimeBuilder".into(),
            satisfies_contracts: vec!["ABC-TL-001".into()],
            disposition_aligned: true, // Keep — permanent task-lifecycle seam
        },
        CompatibilityMapping {
            compat_api: "Runtime".into(),
            satisfies_contracts: vec!["ABC-TL-001".into(), "ABC-OWN-001".into()],
            disposition_aligned: true,
        },
        CompatibilityMapping {
            compat_api: "CompatRuntime::block_on".into(),
            satisfies_contracts: vec!["ABC-OWN-001".into(), "ABC-CAN-001".into()],
            disposition_aligned: true,
        },
        CompatibilityMapping {
            compat_api: "CompatRuntime::spawn_detached".into(),
            // Violates ABC-TL-002 (detached tasks forbidden); Replace disposition.
            satisfies_contracts: vec![],
            disposition_aligned: false,
        },
        CompatibilityMapping {
            compat_api: "sleep".into(),
            satisfies_contracts: vec!["ABC-TO-001".into()],
            disposition_aligned: true,
        },
        CompatibilityMapping {
            compat_api: "timeout".into(),
            satisfies_contracts: vec!["ABC-TO-001".into(), "ABC-CAN-001".into()],
            disposition_aligned: true,
        },
        CompatibilityMapping {
            compat_api: "spawn_blocking".into(),
            satisfies_contracts: vec!["ABC-TL-001".into(), "ABC-OWN-001".into()],
            disposition_aligned: true,
        },
        CompatibilityMapping {
            compat_api: "task::spawn_blocking".into(),
            satisfies_contracts: vec!["ABC-TL-001".into()],
            disposition_aligned: false, // Replace — JoinHandle semantics misaligned
        },
        CompatibilityMapping {
            compat_api: "mpsc_recv_option".into(),
            satisfies_contracts: vec!["ABC-CHN-001".into()],
            disposition_aligned: false, // Replace — hides cancellation semantics
        },
        CompatibilityMapping {
            compat_api: "mpsc_send".into(),
            satisfies_contracts: vec!["ABC-CHN-001".into(), "ABC-BP-001".into()],
            disposition_aligned: false, // Replace — abstracts over reserve/commit
        },
        CompatibilityMapping {
            compat_api: "watch_has_changed".into(),
            satisfies_contracts: vec!["ABC-CHN-001".into()],
            disposition_aligned: false, // Replace
        },
        CompatibilityMapping {
            compat_api: "watch_borrow_and_update_clone".into(),
            satisfies_contracts: vec!["ABC-CHN-001".into(), "ABC-CHN-002".into()],
            disposition_aligned: false, // Replace
        },
        CompatibilityMapping {
            compat_api: "watch_changed".into(),
            satisfies_contracts: vec!["ABC-CHN-001".into(), "ABC-CAN-001".into()],
            disposition_aligned: false, // Replace — hides cancellation/wake semantics
        },
        CompatibilityMapping {
            compat_api: "process::Command".into(),
            satisfies_contracts: vec![],
            disposition_aligned: false, // Retire — tokio-only shim
        },
        CompatibilityMapping {
            compat_api: "signal".into(),
            satisfies_contracts: vec![],
            disposition_aligned: false, // Retire — tokio-only shim
        },
    ]
}

// =============================================================================
// ContractAuditReport
// =============================================================================

/// Full contract audit report for the Core↔Vendored async boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractAuditReport {
    /// Unique identifier for this audit run.
    pub audit_id: String,
    /// When the report was generated (milliseconds since epoch).
    pub generated_at_ms: u64,
    /// Per-contract compliance results.
    pub contracts: Vec<ContractCompliance>,
    /// Summary of the `runtime_compat` surface contract status.
    pub surface_status: SurfaceContractStatus,
    /// Whether all contracts are compliant.
    pub overall_compliant: bool,
    /// Fraction of contracts that are compliant (0.0 – 1.0).
    pub compliance_rate: f64,
    /// Contract IDs for which no evidence has been provided.
    pub uncovered_contracts: Vec<String>,
}

impl ContractAuditReport {
    /// Create a new, empty audit report.
    #[must_use]
    pub fn new(audit_id: &str, generated_at_ms: u64) -> Self {
        Self {
            audit_id: audit_id.to_owned(),
            generated_at_ms,
            contracts: Vec::new(),
            surface_status: SurfaceContractStatus {
                keep_count: 0,
                replace_count: 0,
                retire_count: 0,
                replaced_count: 0,
                retired_count: 0,
            },
            overall_compliant: false,
            compliance_rate: 0.0,
            uncovered_contracts: Vec::new(),
        }
    }

    /// Add a contract compliance result.
    pub fn add_compliance(&mut self, compliance: ContractCompliance) {
        self.contracts.push(compliance);
    }

    /// Set the surface contract status from `dependency_eradication`.
    pub fn set_surface_status(&mut self, status: SurfaceContractStatus) {
        self.surface_status = status;
    }

    /// Finalize the report by computing `overall_compliant`, `compliance_rate`,
    /// and `uncovered_contracts`.
    pub fn finalize(&mut self) {
        let total = self.contracts.len();
        let compliant_count = self.contracts.iter().filter(|c| c.compliant).count();

        self.overall_compliant = total > 0 && compliant_count == total;
        self.compliance_rate = if total == 0 {
            0.0
        } else {
            compliant_count as f64 / total as f64
        };

        self.uncovered_contracts = self
            .contracts
            .iter()
            .filter(|c| c.evidence.is_empty())
            .map(|c| c.contract.contract_id.clone())
            .collect();
    }

    /// Group compliance results by [`ContractCategory`].
    ///
    /// Keys are the `Debug` representation of each category
    /// (e.g., `"Ownership"`, `"Cancellation"`).
    #[must_use]
    pub fn by_category(&self) -> BTreeMap<String, Vec<&ContractCompliance>> {
        let mut map: BTreeMap<String, Vec<&ContractCompliance>> = BTreeMap::new();
        for compliance in &self.contracts {
            let key = format!("{:?}", compliance.contract.category);
            map.entry(key).or_default().push(compliance);
        }
        map
    }

    /// All contracts that are not compliant.
    #[must_use]
    pub fn failing_contracts(&self) -> Vec<&ContractCompliance> {
        self.contracts.iter().filter(|c| !c.compliant).collect()
    }

    /// Human-readable summary of the audit report.
    #[must_use]
    pub fn summary(&self) -> String {
        let total = self.contracts.len();
        let compliant_count = self.contracts.iter().filter(|c| c.compliant).count();
        let failing_count = total - compliant_count;
        let uncovered = self.uncovered_contracts.len();

        let mut lines = Vec::new();
        lines.push(format!(
            "ContractAuditReport [{}] generated_at={}ms",
            self.audit_id, self.generated_at_ms
        ));
        lines.push(format!(
            "  Contracts: {}/{} compliant ({:.0}%), {} uncovered",
            compliant_count,
            total,
            self.compliance_rate * 100.0,
            uncovered
        ));
        lines.push(format!(
            "  Overall: {}",
            if self.overall_compliant {
                "COMPLIANT"
            } else {
                "NON-COMPLIANT"
            }
        ));

        if failing_count > 0 {
            lines.push(format!("  Failing contracts ({}):", failing_count));
            for c in self.failing_contracts() {
                lines.push(format!(
                    "    - {} ({:?}): {} evidence item(s)",
                    c.contract.contract_id,
                    c.contract.category,
                    c.evidence.len()
                ));
            }
        }

        lines.push(format!(
            "  Surface status: keep={} replace={}/{} retire={}/{}",
            self.surface_status.keep_count,
            self.surface_status.replaced_count,
            self.surface_status.replace_count,
            self.surface_status.retired_count,
            self.surface_status.retire_count,
        ));

        lines.join("\n")
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    fn make_evidence(contract_id: &str, test_name: &str, passed: bool) -> ContractEvidence {
        ContractEvidence {
            contract_id: contract_id.to_owned(),
            test_name: test_name.to_owned(),
            passed,
            evidence_type: EvidenceType::UnitTest,
            detail: if passed {
                "assertion passed".to_owned()
            } else {
                "assertion failed".to_owned()
            },
        }
    }

    fn find_contract(id: &str) -> AsyncBoundaryContract {
        standard_contracts()
            .into_iter()
            .find(|c| c.contract_id == id)
            .unwrap_or_else(|| panic!("contract {id} not found"))
    }

    // -------------------------------------------------------------------------
    // standard_contracts
    // -------------------------------------------------------------------------

    #[test]
    fn standard_contracts_count() {
        let contracts = standard_contracts();
        assert!(
            contracts.len() >= 12,
            "expected at least 12 contracts, got {}",
            contracts.len()
        );
    }

    #[test]
    fn all_contracts_have_unique_ids() {
        let contracts = standard_contracts();
        let mut seen = std::collections::HashSet::new();
        for c in &contracts {
            assert!(
                seen.insert(c.contract_id.clone()),
                "duplicate contract_id: {}",
                c.contract_id
            );
        }
    }

    #[test]
    fn all_categories_represented() {
        let contracts = standard_contracts();
        let categories: std::collections::HashSet<String> = contracts
            .iter()
            .map(|c| format!("{:?}", c.category))
            .collect();

        for expected in &[
            "Ownership",
            "Cancellation",
            "Channeling",
            "ErrorMapping",
            "Backpressure",
            "Timeout",
            "TaskLifecycle",
        ] {
            assert!(
                categories.contains(*expected),
                "category {expected} not represented in standard_contracts"
            );
        }
    }

    #[test]
    fn ownership_contracts_bidirectional() {
        let contracts = standard_contracts();
        let ownership: Vec<_> = contracts
            .iter()
            .filter(|c| c.category == ContractCategory::Ownership)
            .collect();
        assert!(!ownership.is_empty(), "no Ownership contracts found");
        for c in &ownership {
            assert_eq!(
                c.direction,
                BoundaryDirection::Bidirectional,
                "expected Ownership contract {} to be Bidirectional",
                c.contract_id
            );
        }
    }

    #[test]
    fn cancellation_contract_has_timeout() {
        // ABC-CAN-001 must mention a numeric timeout in its invariant.
        let c = find_contract("ABC-CAN-001");
        assert!(
            c.invariant.contains("50"),
            "ABC-CAN-001 invariant should mention 50ms timeout, got: {}",
            c.invariant
        );
    }

    #[test]
    fn channel_contract_non_lossy() {
        let c = find_contract("ABC-CHN-002");
        let text = format!("{} {}", c.description, c.invariant).to_lowercase();
        assert!(
            text.contains("non-lossy") || text.contains("buffered") || text.contains("delivered"),
            "ABC-CHN-002 should describe non-lossy delivery, got: {text}"
        );
    }

    #[test]
    fn error_mapping_vendored_to_core() {
        let c = find_contract("ABC-ERR-001");
        assert_eq!(
            c.direction,
            BoundaryDirection::VendoredToCore,
            "ABC-ERR-001 should be VendoredToCore"
        );
    }

    // -------------------------------------------------------------------------
    // ContractEvidence
    // -------------------------------------------------------------------------

    #[test]
    fn contract_evidence_pass() {
        let e = make_evidence("ABC-OWN-001", "test_task_ownership", true);
        assert!(e.passed);
        assert_eq!(e.contract_id, "ABC-OWN-001");
        assert_eq!(e.evidence_type, EvidenceType::UnitTest);
    }

    #[test]
    fn contract_evidence_fail() {
        let e = make_evidence("ABC-CAN-001", "test_cancellation_slow", false);
        assert!(!e.passed);
        assert!(!e.detail.is_empty());
    }

    // -------------------------------------------------------------------------
    // ContractCompliance
    // -------------------------------------------------------------------------

    #[test]
    fn contract_compliance_full_coverage() {
        let contract = find_contract("ABC-OWN-001");
        let evidence = vec![
            make_evidence("ABC-OWN-001", "test_a", true),
            make_evidence("ABC-OWN-001", "test_b", true),
        ];
        let c = ContractCompliance::from_evidence(contract, evidence);
        assert!(c.compliant);
        assert!((c.coverage - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn contract_compliance_partial_coverage() {
        let contract = find_contract("ABC-OWN-001");
        let evidence = vec![
            make_evidence("ABC-OWN-001", "test_a", true),
            make_evidence("ABC-OWN-001", "test_b", false),
        ];
        let c = ContractCompliance::from_evidence(contract, evidence);
        assert!(!c.compliant);
        assert!((c.coverage - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn contract_compliance_zero_coverage() {
        let contract = find_contract("ABC-OWN-001");
        let c = ContractCompliance::from_evidence(contract, vec![]);
        assert!(!c.compliant);
        assert!((c.coverage - 0.0).abs() < f64::EPSILON);
    }

    // -------------------------------------------------------------------------
    // CompatibilityMapping
    // -------------------------------------------------------------------------

    #[test]
    fn compatibility_mapping_covers_apis() {
        let mappings = standard_compatibility_mappings();
        // All 15 entries from SURFACE_CONTRACT_V1 should be present.
        assert_eq!(
            mappings.len(),
            15,
            "expected 15 mappings (one per SURFACE_CONTRACT_V1 entry), got {}",
            mappings.len()
        );

        // No duplicate api names.
        let mut seen = std::collections::HashSet::new();
        for m in &mappings {
            assert!(
                seen.insert(m.compat_api.clone()),
                "duplicate compat_api: {}",
                m.compat_api
            );
        }
    }

    // -------------------------------------------------------------------------
    // ContractAuditReport
    // -------------------------------------------------------------------------

    fn make_all_compliant_report() -> ContractAuditReport {
        let mut report = ContractAuditReport::new("audit-test-001", 1_700_000_000_000);
        for contract in standard_contracts() {
            let id = contract.contract_id.clone();
            let evidence = vec![make_evidence(&id, "auto_test", true)];
            report.add_compliance(ContractCompliance::from_evidence(contract, evidence));
        }
        report.set_surface_status(SurfaceContractStatus {
            keep_count: 5,
            replace_count: 7,
            retire_count: 3,
            replaced_count: 7,
            retired_count: 3,
        });
        report.finalize();
        report
    }

    #[test]
    fn audit_report_all_compliant() {
        let report = make_all_compliant_report();
        assert!(report.overall_compliant);
        assert!((report.compliance_rate - 1.0).abs() < f64::EPSILON);
        assert!(report.uncovered_contracts.is_empty());
        assert!(report.failing_contracts().is_empty());
    }

    #[test]
    fn audit_report_with_failures() {
        let mut report = ContractAuditReport::new("audit-fail-001", 0);
        let contracts = standard_contracts();
        // Add one passing and one failing compliance.
        for (i, contract) in contracts.into_iter().enumerate() {
            let id = contract.contract_id.clone();
            let passed = i % 2 == 0;
            let evidence = vec![make_evidence(&id, "test", passed)];
            report.add_compliance(ContractCompliance::from_evidence(contract, evidence));
        }
        report.finalize();

        assert!(!report.overall_compliant);
        assert!(!report.failing_contracts().is_empty());
        assert!(report.compliance_rate > 0.0 && report.compliance_rate < 1.0);
    }

    #[test]
    fn audit_report_uncovered_contracts() {
        let mut report = ContractAuditReport::new("audit-uncovered-001", 0);
        let contracts = standard_contracts();
        let total = contracts.len();
        // Add all contracts but provide evidence for only the first half.
        for (i, contract) in contracts.into_iter().enumerate() {
            let evidence = if i < total / 2 {
                let id = contract.contract_id.clone();
                vec![make_evidence(&id, "test", true)]
            } else {
                vec![]
            };
            report.add_compliance(ContractCompliance::from_evidence(contract, evidence));
        }
        report.finalize();

        assert!(!report.uncovered_contracts.is_empty());
        // Should not be overall compliant because uncovered contracts have
        // zero evidence (compliant=false).
        assert!(!report.overall_compliant);
    }

    #[test]
    fn audit_report_by_category() {
        let report = make_all_compliant_report();
        let by_cat = report.by_category();

        // All 7 categories should appear.
        for expected in &[
            "Ownership",
            "Cancellation",
            "Channeling",
            "ErrorMapping",
            "Backpressure",
            "Timeout",
            "TaskLifecycle",
        ] {
            assert!(
                by_cat.contains_key(*expected),
                "by_category missing key: {expected}"
            );
        }

        // Total entries across all categories equals total contracts.
        let total_entries: usize = by_cat.values().map(|v| v.len()).sum();
        assert_eq!(total_entries, report.contracts.len());
    }

    #[test]
    fn audit_report_summary_format() {
        let report = make_all_compliant_report();
        let summary = report.summary();

        assert!(
            summary.contains("audit-test-001"),
            "summary should contain audit_id"
        );
        assert!(
            summary.contains("COMPLIANT"),
            "summary should contain compliance status"
        );
        assert!(
            summary.contains("keep="),
            "summary should include surface status"
        );
    }

    #[test]
    fn finalize_computes_compliance_rate() {
        let mut report = ContractAuditReport::new("rate-test", 0);
        let contracts = standard_contracts();
        let total = contracts.len();

        // Make exactly half compliant.
        let half = total / 2;
        for (i, contract) in contracts.into_iter().enumerate() {
            let id = contract.contract_id.clone();
            let passed = i < half;
            let evidence = vec![make_evidence(&id, "t", passed)];
            report.add_compliance(ContractCompliance::from_evidence(contract, evidence));
        }
        report.finalize();

        let expected_rate = half as f64 / total as f64;
        assert!(
            (report.compliance_rate - expected_rate).abs() < 0.01,
            "expected compliance_rate ~{expected_rate:.3}, got {:.3}",
            report.compliance_rate
        );
    }

    #[test]
    fn surface_status_integration() {
        let mut report = ContractAuditReport::new("surface-test", 42_000);
        let status = SurfaceContractStatus {
            keep_count: 3,
            replace_count: 7,
            retire_count: 2,
            replaced_count: 5,
            retired_count: 1,
        };
        report.set_surface_status(status);
        report.finalize();

        assert_eq!(report.surface_status.keep_count, 3);
        assert_eq!(report.surface_status.replace_count, 7);
        assert_eq!(report.surface_status.retire_count, 2);
        assert_eq!(report.surface_status.replaced_count, 5);
        assert_eq!(report.surface_status.retired_count, 1);
        // 1 replace + 1 retire still pending.
        assert!(!report.surface_status.all_transitional_resolved());
        assert_eq!(report.surface_status.remaining_transitional(), 3);
        assert_eq!(report.surface_status.total_count(), 12);
    }

    // -------------------------------------------------------------------------
    // Serde round-trip
    // -------------------------------------------------------------------------

    #[test]
    fn serde_roundtrip_boundary_direction() {
        for dir in &[
            BoundaryDirection::CoreToVendored,
            BoundaryDirection::VendoredToCore,
            BoundaryDirection::Bidirectional,
        ] {
            let json = serde_json::to_string(dir).expect("serialize");
            let back: BoundaryDirection = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*dir, back);
        }
    }

    #[test]
    fn serde_roundtrip_contract_compliance() {
        let report = make_all_compliant_report();
        let json = serde_json::to_string(&report).expect("serialize");
        let back: ContractAuditReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.audit_id, report.audit_id);
        assert_eq!(back.contracts.len(), report.contracts.len());
        assert_eq!(back.overall_compliant, report.overall_compliant);
    }
}
