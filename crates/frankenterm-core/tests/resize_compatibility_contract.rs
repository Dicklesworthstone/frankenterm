//! Contract guard for wa-1u90p.8.1 compatibility policy.
//!
//! Keeps the no-regression resize compatibility contract machine-checkable:
//! - required invariant IDs remain present
//! - rollout/go-no-go bead references remain present
//! - mapped coverage paths exist in the repository

use std::fs;
use std::path::{Path, PathBuf};

const CONTRACT_DOC: &str = "docs/resize-no-regression-compatibility-contract-wa-1u90p.8.1.md";

const REQUIRED_INVARIANTS: &[&str] = &[
    "RC-CURSOR-001",
    "RC-WRAP-001",
    "RC-SCROLLBACK-001",
    "RC-ALTSCREEN-001",
    "RC-INTERACTION-001",
    "RC-LIFECYCLE-001",
];

const REQUIRED_BEAD_REFERENCES: &[&str] = &["wa-1u90p.8.2", "wa-1u90p.8.6"];

const REQUIRED_COVERAGE_PATHS: &[&str] = &[
    "docs/resize-performance-slos.md",
    "docs/resize-baseline-scenarios.md",
    "docs/resize-artifact-fault-model-wa-1u90p.4.1.md",
    "crates/frankenterm-core/tests/resize_invariant_contract.rs",
    "crates/frankenterm-core/tests/resize_pipeline_integration.rs",
    "crates/frankenterm-core/tests/resize_scheduler_state_machine_tests.rs",
    "crates/frankenterm-core/tests/proptest_resize_invariants.rs",
    "crates/frankenterm-core/tests/proptest_resize_scheduler.rs",
    "crates/frankenterm-core/tests/proptest_viewport_reflow_planner.rs",
    "crates/frankenterm-core/tests/proptest_restore_scrollback.rs",
    "crates/frankenterm-core/tests/simulation_resize_suite.rs",
    "crates/frankenterm-core/src/screen_state.rs",
    "crates/frankenterm-core/src/ingest.rs",
    "crates/frankenterm-core/src/workflows/mod.rs",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root should be discoverable from CARGO_MANIFEST_DIR")
}

#[test]
fn compatibility_contract_contains_required_invariants_and_escalation_sections() {
    let root = workspace_root();
    let doc_path = root.join(CONTRACT_DOC);
    let doc = fs::read_to_string(&doc_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", doc_path.display()));

    assert!(
        doc.contains("# Resize/Reflow No-Regression Compatibility Contract"),
        "contract title missing from {}",
        doc_path.display()
    );
    assert!(
        doc.contains("## Compatibility Invariants"),
        "compatibility invariants section missing from {}",
        doc_path.display()
    );
    assert!(
        doc.contains("## Escalation Process"),
        "escalation process section missing from {}",
        doc_path.display()
    );

    for invariant in REQUIRED_INVARIANTS {
        assert!(
            doc.contains(invariant),
            "required invariant {invariant} missing from {}",
            doc_path.display()
        );
    }

    for bead in REQUIRED_BEAD_REFERENCES {
        assert!(
            doc.contains(bead),
            "required downstream bead reference {bead} missing from {}",
            doc_path.display()
        );
    }
}

#[test]
fn compatibility_contract_coverage_map_points_to_real_paths() {
    let root = workspace_root();
    let doc_path = root.join(CONTRACT_DOC);
    let doc = fs::read_to_string(&doc_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", doc_path.display()));

    for relative_path in REQUIRED_COVERAGE_PATHS {
        let absolute = root.join(relative_path);
        assert!(
            absolute.exists(),
            "coverage path listed by contract does not exist: {}",
            absolute.display()
        );
        assert!(
            doc.contains(relative_path),
            "coverage path missing from contract mapping: {}",
            relative_path
        );
    }
}
