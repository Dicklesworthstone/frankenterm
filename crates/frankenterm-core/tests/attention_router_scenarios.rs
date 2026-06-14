//! No-mock conformance + safety-invariant proof harness for the attention-router
//! golden scenario inventory (ft-x3nsb.6).
//!
//! This harness loads the REAL retained scenario inventory
//! (`fixtures/attention-router/scenarios.v1.json`) — no hand-wavy mocks — and
//! proves it stays stable and testable:
//!
//! 1. It parses into typed structs (schema is well-formed).
//! 2. It covers every scenario the bead requires, with unique ids.
//! 3. Every scenario is well-formed against the declared
//!    `classification_vocabulary` and carries the required source/expected/
//!    forbidden fields (each source has a real `command_or_api`).
//! 4. The `must_fail_when` safety doctrine is actually ENCODED in the scenarios
//!    — e.g. the RCH-degraded scenario forbids service mutation, the
//!    dirty-overlap scenario forbids touching another agent's work, the
//!    stale-claim scenario forbids force-release without a status check.
//!
//! The deeper engine-execution golden harness (per-scenario JSON/TOON output +
//! decision log produced by running the scoring engine, per
//! `future_harness_requirements`) is tracked as the remaining ft-x3nsb.6 work.

use serde::Deserialize;

const INVENTORY_JSON: &str =
    include_str!("../../../fixtures/attention-router/scenarios.v1.json");

#[derive(Debug, Deserialize)]
struct ScenarioInventory {
    schema_version: String,
    contract_id: String,
    classification_vocabulary: Vec<String>,
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
struct Scenario {
    scenario_id: String,
    #[serde(default)]
    title: String,
    source_fixture_requirements: Vec<SourceReq>,
    expected: Expected,
    #[serde(default)]
    forbidden_actions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SourceReq {
    #[serde(default)]
    source_id: String,
    command_or_api: String,
    #[serde(default)]
    required_reason_codes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Expected {
    classification: String,
    #[serde(default)]
    confidence: String,
    #[serde(default)]
    explanation_must_include: Vec<String>,
    recommended_safe_action: String,
}

fn load_inventory() -> ScenarioInventory {
    serde_json::from_str(INVENTORY_JSON)
        .expect("scenarios.v1.json must deserialize into the typed inventory schema")
}

fn scenario<'a>(inv: &'a ScenarioInventory, id: &str) -> &'a Scenario {
    inv.scenarios
        .iter()
        .find(|s| s.scenario_id == id)
        .unwrap_or_else(|| panic!("required scenario '{id}' missing from inventory"))
}

/// The six scenarios the bead's scope mandates.
const REQUIRED_SCENARIOS: &[&str] = &[
    "empty-ready-bv-blocked-recommendation",
    "rch-no-admissible-worker",
    "agent-mail-ack-required",
    "stale-in-progress-candidate",
    "dirty-overlap-active-owner",
    "docs-only-ready-while-proof-blocked",
];

#[test]
fn inventory_parses_and_covers_required_scenarios() {
    let inv = load_inventory();
    assert!(
        !inv.schema_version.is_empty() && !inv.contract_id.is_empty(),
        "inventory must declare schema_version + contract_id"
    );
    assert!(
        inv.scenarios.len() >= REQUIRED_SCENARIOS.len(),
        "expected at least {} scenarios, found {}",
        REQUIRED_SCENARIOS.len(),
        inv.scenarios.len()
    );

    // Unique scenario ids.
    let mut seen = std::collections::BTreeSet::new();
    for s in &inv.scenarios {
        assert!(
            seen.insert(s.scenario_id.as_str()),
            "duplicate scenario_id: {}",
            s.scenario_id
        );
    }

    // Every bead-required scenario is present.
    for id in REQUIRED_SCENARIOS {
        assert!(
            inv.scenarios.iter().any(|s| s.scenario_id == *id),
            "bead-required scenario missing: {id}"
        );
    }
}

#[test]
fn every_scenario_is_well_formed() {
    let inv = load_inventory();
    let vocab: std::collections::BTreeSet<&str> =
        inv.classification_vocabulary.iter().map(String::as_str).collect();
    assert!(!vocab.is_empty(), "classification_vocabulary must be declared");

    for s in &inv.scenarios {
        let id = &s.scenario_id;
        assert!(
            vocab.contains(s.expected.classification.as_str()),
            "scenario {id}: classification '{}' not in declared vocabulary",
            s.expected.classification
        );
        assert!(
            !s.expected.recommended_safe_action.is_empty(),
            "scenario {id}: recommended_safe_action must be non-empty"
        );
        assert!(
            !s.expected.confidence.is_empty(),
            "scenario {id}: confidence must be declared"
        );
        assert!(
            !s.expected.explanation_must_include.is_empty(),
            "scenario {id}: explanation_must_include must be non-empty"
        );
        assert!(
            !s.forbidden_actions.is_empty(),
            "scenario {id}: forbidden_actions must be non-empty (the safety surface)"
        );
        assert!(
            !s.source_fixture_requirements.is_empty(),
            "scenario {id}: must declare real source fixture requirements (no mocks)"
        );
        for src in &s.source_fixture_requirements {
            assert!(
                !src.command_or_api.is_empty(),
                "scenario {id} source {}: command_or_api must be a real command/API",
                src.source_id
            );
            assert!(
                !src.required_reason_codes.is_empty(),
                "scenario {id} source {}: must require at least one reason code",
                src.source_id
            );
        }
        // title is informative; if present it should be meaningful.
        if !s.title.is_empty() {
            assert!(s.title.len() >= 8, "scenario {id}: title too terse");
        }
    }
}

/// True when any forbidden action contains one of the given substrings.
fn forbids(s: &Scenario, needles: &[&str]) -> bool {
    s.forbidden_actions
        .iter()
        .any(|a| needles.iter().any(|n| a.contains(n)))
}

#[test]
fn safety_invariants_are_encoded() {
    let inv = load_inventory();

    // bv advisory is never claim authority; local cargo is never proof; RCH is
    // never restarted/cancelled when bv points at a blocked issue.
    let bv = scenario(&inv, "empty-ready-bv-blocked-recommendation");
    assert_eq!(bv.expected.classification, "blocked_infra");
    assert!(
        forbids(bv, &["claim"]),
        "empty-ready scenario must forbid claiming the bv pick"
    );
    assert!(
        forbids(bv, &["cargo"]) && forbids(bv, &["rch", "restart", "cancel"]),
        "empty-ready scenario must forbid local-cargo proof and RCH mutation"
    );

    // RCH degraded → never recommend service mutation / build cancellation.
    let rch = scenario(&inv, "rch-no-admissible-worker");
    assert!(
        matches!(rch.expected.classification.as_str(), "proof_starved" | "blocked_infra"),
        "rch-degraded must classify as proof_starved/blocked_infra, got {}",
        rch.expected.classification
    );
    assert!(
        forbids(rch, &["restart", "cancel", "repair", "mutate"]),
        "rch-degraded scenario must forbid RCH service mutation / build cancellation"
    );

    // Agent Mail ack → wait/acknowledge, never repair/restart the service.
    let mail = scenario(&inv, "agent-mail-ack-required");
    assert_eq!(mail.expected.classification, "waiting_comm");
    assert!(
        forbids(mail, &["repair", "restart"]),
        "agent-mail scenario must forbid repairing/restarting the mail service"
    );

    // Stale in-progress → status-check, never force-release without evidence.
    let stale = scenario(&inv, "stale-in-progress-candidate");
    assert_eq!(stale.expected.classification, "stale_claim");
    assert!(
        forbids(stale, &["force_release", "force-release", "reopen"]),
        "stale-claim scenario must forbid force-release/reopen before a status check"
    );

    // Dirty overlap owned by another agent → do not touch their work.
    let dirty = scenario(&inv, "dirty-overlap-active-owner");
    assert!(
        matches!(dirty.expected.classification.as_str(), "dirty_overlap" | "do_not_touch"),
        "dirty-overlap must classify as dirty_overlap/do_not_touch, got {}",
        dirty.expected.classification
    );
    assert!(
        forbids(dirty, &["stash", "revert", "stage", "checkout", "discard"]),
        "dirty-overlap scenario must forbid stashing/reverting/staging another agent's work"
    );

    // Docs-only ready slice is actionable even while proof lanes are blocked.
    let docs = scenario(&inv, "docs-only-ready-while-proof-blocked");
    assert_eq!(docs.expected.classification, "ready_now");
}
