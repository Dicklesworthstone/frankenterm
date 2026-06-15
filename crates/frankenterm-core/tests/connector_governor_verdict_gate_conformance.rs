//! Conformance: connector **governor verdict-gate** fail-closed contract
//! (ft-7h5da.5 / W4 family).
//!
//! `GovernorVerdict` (`connector_governor.rs`) is the connector admission
//! verdict gate consulted on the outbound dispatch path:
//!   - `Allow`    — proceed immediately
//!   - `Throttle` — proceed after a recommended delay
//!   - `Reject`   — blocked (quota/budget exhausted)
//!
//! `GovernorDecision::is_allowed()` is `Allow | Throttle`; `is_rejected()` is
//! `Reject`. These tests pin that decision contract (behaviorally, via the
//! public API) so a refactor cannot silently widen "allowed", make a rejected
//! action look allowed, or introduce a verdict that is classified as neither.
//!
//! They also pin the fail-closed relationship to the production CONSUMER: the
//! outbound bridge proceeds only on `Allow` (a strict subset of `is_allowed`),
//! so it can never dispatch a verdict the governor would reject.

use std::path::PathBuf;

use frankenterm_core::connector_governor::{GovernorDecision, GovernorReason, GovernorVerdict};

/// Read a `frankenterm-core` source file relative to the crate manifest.
fn core_src(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// A `Reject` verdict must block: rejected, not allowed, no dispatch delay.
#[test]
fn governor_reject_is_blocked_and_not_allowed() {
    let d = GovernorDecision::reject(
        "slack",
        "notify",
        GovernorReason::ConnectorQuotaExhausted,
        1000,
    );
    assert!(d.is_rejected(), "Reject verdict must report is_rejected()");
    assert!(
        !d.is_allowed(),
        "Reject verdict must NOT report is_allowed() (fail closed)"
    );
    assert_eq!(d.verdict, GovernorVerdict::Reject);
    assert_eq!(d.delay_ms, 0, "a rejected action carries no dispatch delay");
}

/// An `Allow` verdict proceeds immediately with no delay.
#[test]
fn governor_allow_proceeds_without_delay() {
    let d = GovernorDecision::allow("slack", "notify", 1000);
    assert!(d.is_allowed());
    assert!(!d.is_rejected());
    assert_eq!(d.verdict, GovernorVerdict::Allow);
    assert_eq!(d.delay_ms, 0);
}

/// A `Throttle` verdict is allowed-after-delay (per the gate contract) and is
/// NOT a rejection; it carries the recommended delay.
#[test]
fn governor_throttle_is_allowed_with_delay_but_not_rejected() {
    let d = GovernorDecision::throttle(
        "slack",
        "notify",
        GovernorReason::ConnectorRateLimit,
        250,
        1000,
    );
    assert!(
        d.is_allowed(),
        "Throttle is allowed-after-delay per the verdict-gate contract"
    );
    assert!(!d.is_rejected());
    assert_eq!(d.verdict, GovernorVerdict::Throttle);
    assert_eq!(d.delay_ms, 250, "throttle carries the recommended delay");
}

/// Fail-closed classification: every verdict is EITHER allowed (possibly
/// delayed) XOR rejected — never both, never neither. A future verdict variant
/// that is classified as neither would break this and must be handled
/// explicitly rather than silently falling through to "allowed".
#[test]
fn governor_is_allowed_and_is_rejected_are_mutually_exclusive() {
    let decisions = [
        GovernorDecision::allow("c", "k", 1),
        GovernorDecision::throttle("c", "k", GovernorReason::Backpressure, 10, 1),
        GovernorDecision::reject("c", "k", GovernorReason::BudgetExceeded, 1),
    ];
    for d in decisions {
        assert_ne!(
            d.is_allowed(),
            d.is_rejected(),
            "verdict {:?} must be exactly one of allowed / rejected (fail-closed classification)",
            d.verdict
        );
    }
}

/// Fail-closed CONSUMER subset: the production outbound bridge proceeds only on
/// `GovernorVerdict::Allow` (`!matches!(verdict, Allow)` blocks), a strict
/// subset of `is_allowed()` (`Allow | Throttle`). So the consumer is at least
/// as strict as the gate and can never dispatch a verdict the governor would
/// reject.
#[test]
fn outbound_consumer_never_proceeds_on_a_rejected_verdict() {
    // Source side: the consumer's proceed-set is exactly `Allow`.
    let src = core_src("connector_outbound_bridge.rs");
    assert!(
        src.contains("!matches!(&governor_decision.verdict, GovernorVerdict::Allow)"),
        "outbound consumer must block any verdict that is not Allow (ft-7h5da.5.10 fail-closed)"
    );
    // Contract side: a rejected verdict is never `is_allowed()`, so even the
    // looser gate predicate excludes it — the stricter consumer certainly does.
    let rejected =
        GovernorDecision::reject("slack", "notify", GovernorReason::GlobalQuotaExhausted, 1);
    assert!(rejected.is_rejected() && !rejected.is_allowed());
}
