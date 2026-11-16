//! Contract guard for wa-1u90p.8.2 staged rollout plan.
//!
//! Ensures the rollout plan remains explicit about:
//! - phase staging
//! - cohorting
//! - rollback criteria
//! - dependency/reference links to compatibility and SLO contracts

use std::fs;
use std::path::{Path, PathBuf};

const ROLLOUT_DOC: &str = "docs/resize-rollout-plan-wa-1u90p.8.2.md";

const REQUIRED_PHASE_LABELS: &[&str] = &[
    "Phase 0: Safe Baseline",
    "Phase 1: Internal Canary",
    "Phase 2: Controlled Beta",
    "Phase 3: Broad Rollout",
];

const REQUIRED_COHORT_LABELS: &[&str] = &["`C0`", "`C1`", "`C2`", "`C3`"];

const REQUIRED_ROLLBACK_CRITERIA_SNIPPETS: &[&str] = &[
    "Any critical compatibility invariant failure",
    "Critical artifact count > 0",
    "M1` p99 exceeds target by >20%",
    "crash or hang",
    "emergency safe-mode activation",
];

const REQUIRED_REFERENCES: &[&str] = &[
    "wa-1u90p.8.1",
    "wa-1u90p.8.4",
    "wa-1u90p.8.6",
    "wa-1u90p.8.7",
];

const REQUIRED_PATH_REFERENCES: &[&str] = &[
    "docs/resize-no-regression-compatibility-contract-wa-1u90p.8.1.md",
    "docs/resize-performance-slos.md",
    "docs/resize-baseline-scenarios.md",
    "docs/resize-artifact-fault-model-wa-1u90p.4.1.md",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root should be discoverable from CARGO_MANIFEST_DIR")
}

#[test]
fn rollout_plan_contains_required_phase_cohort_and_rollback_contract() {
    let root = workspace_root();
    let doc_path = root.join(ROLLOUT_DOC);
    let doc = fs::read_to_string(&doc_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", doc_path.display()));

    assert!(
        doc.contains("# Staged Resize/Reflow Rollout Plan (`wa-1u90p.8.2`)"),
        "rollout plan title missing from {}",
        doc_path.display()
    );
    assert!(
        doc.contains("## Explicit Rollback Criteria"),
        "rollback criteria section missing from {}",
        doc_path.display()
    );
    assert!(
        doc.contains("## Decision Checkpoints"),
        "decision checkpoints section missing from {}",
        doc_path.display()
    );

    for phase in REQUIRED_PHASE_LABELS {
        assert!(
            doc.contains(phase),
            "required rollout phase label missing: {phase}"
        );
    }

    for cohort in REQUIRED_COHORT_LABELS {
        assert!(
            doc.contains(cohort),
            "required rollout cohort label missing: {cohort}"
        );
    }

    for criterion in REQUIRED_ROLLBACK_CRITERIA_SNIPPETS {
        assert!(
            doc.contains(criterion),
            "required rollback criterion snippet missing: {criterion}"
        );
    }

    for reference in REQUIRED_REFERENCES {
        assert!(
            doc.contains(reference),
            "required bead reference missing from rollout plan: {reference}"
        );
    }
}

#[test]
fn rollout_plan_referenced_paths_exist_and_are_listed() {
    let root = workspace_root();
    let doc_path = root.join(ROLLOUT_DOC);
    let doc = fs::read_to_string(&doc_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", doc_path.display()));

    for relative_path in REQUIRED_PATH_REFERENCES {
        let absolute = root.join(relative_path);
        assert!(
            absolute.exists(),
            "rollout plan references missing path: {}",
            absolute.display()
        );
        assert!(
            doc.contains(relative_path),
            "required path reference missing from rollout plan: {}",
            relative_path
        );
    }
}
