//! Golden + property tests for connector governor / gate **consultation** across
//! the W4 `.5.x` bridge family (ft-7h5da.5):
//!   - inbound   (.5.9)  gates via privacy/classification
//!   - outbound  (.5.10) gates via the connector governor (Allow/Throttle/Reject)
//!   - lifecycle (.5.11) gates via the emergency kill switch
//!
//! The connector *governor* — the gate with a `Throttle` tier — is
//! outbound-specific; inbound and lifecycle consult their own gates. These tests
//! pin two things:
//!   1. GOLDEN (source-level, inline frozen matrix): every `.5.x` bridge consults
//!      a gate on its dispatch path, and the outbound governor HONORS `Throttle`
//!      (proceeds, distinct telemetry) rather than dropping it (ft-7h5da.5.15).
//!   2. PROPERTY (proptest over the public `GovernorDecision` API): for any
//!      verdict, `is_allowed` XOR `is_rejected`; a `Throttle` is always honored
//!      (allowed, not rejected, consumer proceeds); and the modeled outbound
//!      consumer proceed-set equals `is_allowed` — so `Throttle` is never
//!      silently dropped while `Reject` always blocks.
//!
//! The golden matrix depends on the `.5.11` (policy.rs) and `.5.15`
//! (connector_outbound_bridge.rs) wiring, so it lands with the bridge family.
//! The property tests use only HEAD APIs and stand alone.

use std::path::PathBuf;

use proptest::prelude::*;

use frankenterm_core::connector_governor::{GovernorDecision, GovernorReason, GovernorVerdict};

// ---------------------------------------------------------------------------
// Golden: per-bridge gate consultation + outbound Throttle-honored
// ---------------------------------------------------------------------------

/// Read a `frankenterm-core` source file relative to the crate manifest.
fn core_src(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn contains_all(src: &str, needles: &[&str]) -> bool {
    needles.iter().all(|needle| src.contains(needle))
}

/// Compute the governor/gate consultation contract from production source.
fn governor_consultation_contract() -> serde_json::Value {
    let inbound = core_src("connector_inbound_bridge.rs");
    let outbound = core_src("connector_outbound_bridge.rs");
    let policy = core_src("policy.rs");

    serde_json::json!([
        {
            "bead": "ft-7h5da.5.9",
            "bridge": "inbound",
            "gate": "privacy/classification",
            "gate_consulted": contains_all(
                &inbound,
                &["fn route_signal", "ClassificationFailed", "PrivacyRejected"],
            ),
            "throttle_tier": false,
        },
        {
            "bead": "ft-7h5da.5.10",
            "bridge": "outbound",
            "gate": "connector_governor",
            "gate_consulted": contains_all(
                &outbound,
                &["let governor_decision", "governor.evaluate(&action, now_ms)"],
            ),
            "throttle_tier": true,
            // ft-7h5da.5.15: Throttle proceeds (not dropped) + distinct telemetry.
            "throttle_honored": contains_all(
                &outbound,
                &[
                    "GovernorVerdict::Throttle",
                    "self.telemetry.actions_throttled",
                    "proceeding (delay advisory)",
                ],
            ),
        },
        {
            "bead": "ft-7h5da.5.11",
            "bridge": "lifecycle",
            "gate": "kill_switch",
            "gate_consulted": contains_all(
                &policy,
                &["fn run_connector_lifecycle_intent", "kill_switch().is_emergency()"],
            ),
            "throttle_tier": false,
        },
    ])
}

#[test]
fn governor_consultation_matches_golden() {
    let matrix = governor_consultation_contract();
    // GOLDEN: the frozen target contract. Every bridge consults its gate, and
    // the outbound governor honors Throttle (ft-7h5da.5.15). `serde_json::Value`
    // equality is structural/order-independent, so this is robust to field
    // ordering. Lands with the `.5.11` + `.5.15` bridge wiring.
    let expected = serde_json::json!([
        {
            "bead": "ft-7h5da.5.9",
            "bridge": "inbound",
            "gate": "privacy/classification",
            "gate_consulted": true,
            "throttle_tier": false,
        },
        {
            "bead": "ft-7h5da.5.10",
            "bridge": "outbound",
            "gate": "connector_governor",
            "gate_consulted": true,
            "throttle_tier": true,
            "throttle_honored": true,
        },
        {
            "bead": "ft-7h5da.5.11",
            "bridge": "lifecycle",
            "gate": "kill_switch",
            "gate_consulted": true,
            "throttle_tier": false,
        },
    ]);
    assert_eq!(
        matrix, expected,
        "connector governor/gate consultation contract drifted from golden"
    );
}

// ---------------------------------------------------------------------------
// Property: governor verdict honoring (Throttle is never dropped)
// ---------------------------------------------------------------------------

fn arb_verdict() -> impl Strategy<Value = GovernorVerdict> {
    prop_oneof![
        Just(GovernorVerdict::Allow),
        Just(GovernorVerdict::Throttle),
        Just(GovernorVerdict::Reject),
    ]
}

fn arb_reason() -> impl Strategy<Value = GovernorReason> {
    prop_oneof![
        Just(GovernorReason::Clear),
        Just(GovernorReason::ConnectorRateLimit),
        Just(GovernorReason::GlobalRateLimit),
        Just(GovernorReason::ConnectorQuotaExhausted),
        Just(GovernorReason::GlobalQuotaExhausted),
        Just(GovernorReason::BudgetExceeded),
        Just(GovernorReason::Backpressure),
        Just(GovernorReason::AdaptiveBackoff),
    ]
}

/// Build a `GovernorDecision` from arbitrary parts via the public constructors.
fn decision_for(
    verdict: GovernorVerdict,
    reason: GovernorReason,
    delay_ms: u64,
) -> GovernorDecision {
    match verdict {
        GovernorVerdict::Allow => GovernorDecision::allow("c", "k", 1),
        GovernorVerdict::Throttle => GovernorDecision::throttle("c", "k", reason, delay_ms, 1),
        GovernorVerdict::Reject => GovernorDecision::reject("c", "k", reason, 1),
    }
}

/// Models the outbound consumer's proceed predicate AFTER ft-7h5da.5.15: proceed
/// on anything that is not a hard `Reject` (i.e. `Allow` or `Throttle`).
fn outbound_consumer_proceeds(verdict: &GovernorVerdict) -> bool {
    !matches!(verdict, GovernorVerdict::Reject)
}

proptest! {
    /// Every governor decision is classified as exactly one of allowed/rejected.
    #[test]
    fn prop_verdict_partitions(v in arb_verdict(), r in arb_reason(), d in any::<u64>()) {
        let decision = decision_for(v, r, d);
        prop_assert_ne!(decision.is_allowed(), decision.is_rejected());
    }

    /// ft-7h5da.5.15: a `Throttle` verdict is HONORED for any reason/delay —
    /// allowed (after delay), not a rejection, and the production consumer
    /// proceeds on it (it is never dropped).
    #[test]
    fn prop_throttle_is_honored_not_dropped(r in arb_reason(), d in any::<u64>()) {
        let decision = GovernorDecision::throttle("c", "k", r, d, 1);
        prop_assert!(decision.is_allowed(), "throttle must be allowed-after-delay");
        prop_assert!(!decision.is_rejected(), "throttle must not be a rejection");
        prop_assert!(
            outbound_consumer_proceeds(&decision.verdict),
            "outbound consumer must proceed on throttle (not drop it)"
        );
    }

    /// The outbound consumer's proceed-set equals the governor's `is_allowed`
    /// set for every verdict: it proceeds iff the verdict is not a hard
    /// rejection. This pins that `Throttle` is never silently dropped while
    /// `Reject` is always blocked.
    #[test]
    fn prop_consumer_proceed_matches_is_allowed(v in arb_verdict(), r in arb_reason(), d in any::<u64>()) {
        let decision = decision_for(v, r, d);
        prop_assert_eq!(outbound_consumer_proceeds(&decision.verdict), decision.is_allowed());
    }
}
