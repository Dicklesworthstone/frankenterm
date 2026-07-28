//! W2.T (ft-7h5da.3.8) verified-submit delivery-receipt tests — standalone
//! integration file, PUBLIC API only.
//!
//! Covers the delivery-receipt core of Verified-Submit Send (consensus #1) that
//! is reachable without a live agent pane:
//!   * every `SubmitReceiptState` is reachable + serializes to its stable wire
//!     string (JSON), and a fully-populated `SubmitReceipt` round-trips;
//!   * `SubmitGuaranteeLevel::is_met_by` — the silent-success + fail-open guards:
//!     a `stuck_in_composer` / `verification_unavailable` / crashed / failed /
//!     denied / approval state NEVER satisfies the `submitted`/`working`
//!     guarantee (so a stuck or unverifiable send is never reported as success);
//!   * `submit_receipt_state` fail-open mapping — an allowed send with no
//!     verification report is still `submitted` (status quo: verification never
//!     makes a send WORSE), while denied / requires-approval / error map to
//!     their typed states and a report state (incl. `verification_unavailable`)
//!     passes through honestly;
//!   * idempotency-key replay returns the original receipt, the key guard
//!     rejects path-traversal, and an unrecorded key resolves to `None`.
//!
//! The agent-driven e2e (tests/e2e/verified_submit.sh — stuck composer, codex
//! queue-behind no-Enter-spam, crash-to-shell, telemetry counters) needs live
//! panes + W2.4/W2.6/W2.7/W2.8 and is out of scope for this public-API unit file.

use frankenterm_core::policy::{ActionKind, InjectionResult, PolicyDecision};
use frankenterm_core::robot_types::{SubmitGuaranteeLevel, SubmitReceipt, SubmitReceiptState};
use frankenterm_core::submit_idempotency_store::{is_valid_submit_key, lookup, record};
use frankenterm_core::verified_submit::{
    VerifiedSubmitReport, submit_receipt_evidence_rule_ids, submit_receipt_state,
};

const ALL_STATES: &[SubmitReceiptState] = &[
    SubmitReceiptState::Submitted,
    SubmitReceiptState::QueuedBehindOperation,
    SubmitReceiptState::StuckInComposer,
    SubmitReceiptState::PaneCrashedToShell,
    SubmitReceiptState::VerificationUnavailable,
    SubmitReceiptState::PolicyDenied,
    SubmitReceiptState::RequiresApproval,
    SubmitReceiptState::SendFailed,
];

fn report_with_state(state: SubmitReceiptState, evidence: Vec<String>) -> VerifiedSubmitReport {
    VerifiedSubmitReport {
        state,
        agent_type: Some("claude_code".to_string()),
        profile_id: Some("builtin:claude_code".to_string()),
        profile_version: Some("1".to_string()),
        attempts: 1,
        evidence_rule_ids: evidence,
        polls: 2,
        cursor_before: Some("pane:before".to_string()),
        cursor_after: Some("pane:after".to_string()),
    }
}

fn allowed(rule: &str) -> InjectionResult {
    InjectionResult::Allowed {
        decision: PolicyDecision::allow_with_rule(rule.to_string()),
        summary: "send <redacted>".to_string(),
        pane_id: 7,
        action: ActionKind::SendText,
        audit_action_id: None,
    }
}

#[test]
fn every_submit_state_serializes_to_its_stable_wire_string() {
    // Each enum variant -> its as_str() snake_case wire form, and a SubmitReceipt
    // carrying it serializes `state` to exactly that string (JSON golden per state).
    let expected = [
        (SubmitReceiptState::Submitted, "submitted"),
        (
            SubmitReceiptState::QueuedBehindOperation,
            "queued_behind_operation",
        ),
        (SubmitReceiptState::StuckInComposer, "stuck_in_composer"),
        (
            SubmitReceiptState::PaneCrashedToShell,
            "pane_crashed_to_shell",
        ),
        (
            SubmitReceiptState::VerificationUnavailable,
            "verification_unavailable",
        ),
        (SubmitReceiptState::PolicyDenied, "policy_denied"),
        (SubmitReceiptState::RequiresApproval, "requires_approval"),
        (SubmitReceiptState::SendFailed, "send_failed"),
    ];
    assert_eq!(
        expected.len(),
        ALL_STATES.len(),
        "a state was added without a wire-string lock"
    );
    for (state, wire) in expected {
        assert_eq!(state.as_str(), wire, "as_str drifted for {state:?}");
        let receipt = receipt_with_state(state);
        let json = serde_json::to_value(&receipt).expect("serialize receipt");
        assert_eq!(
            json["state"],
            serde_json::json!(wire),
            "JSON state wire drift for {state:?}"
        );
    }
}

fn receipt_with_state(state: SubmitReceiptState) -> SubmitReceipt {
    SubmitReceipt {
        state,
        guarantee_level: SubmitGuaranteeLevel::Submitted,
        guarantee_met: matches!(
            state,
            SubmitReceiptState::Submitted | SubmitReceiptState::QueuedBehindOperation
        ),
        agent_type: Some("claude_code".to_string()),
        profile_id: Some("builtin:claude_code".to_string()),
        profile_version: Some("1".to_string()),
        attempts: 1,
        evidence_rule_ids: vec!["claude_code:submit:composer_cleared".to_string()],
        elapsed_ms: 42,
        polls: 3,
        cursor_before: Some("pane:before".to_string()),
        cursor_after: Some("pane:after".to_string()),
        idempotency_key: "idem:7:00112233445566ff".to_string(),
    }
}

#[test]
fn submit_receipt_round_trips_through_json() {
    let receipt = receipt_with_state(SubmitReceiptState::Submitted);
    let json = serde_json::to_string(&receipt).expect("serialize");
    let back: SubmitReceipt = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, receipt);
}

#[test]
fn submit_guarantee_level_silent_success_and_fail_open_guards() {
    use SubmitGuaranteeLevel::{Composer, Submitted, Working, Write};
    use SubmitReceiptState as St;
    let none: &[String] = &[];

    // SECURITY: a stuck composer is NOT a `submitted`/`working` success — the
    // headline silent-success regression guard.
    assert!(
        !Submitted.is_met_by(St::StuckInComposer, none),
        "stuck must not meet submitted"
    );
    assert!(
        !Working.is_met_by(St::StuckInComposer, none),
        "stuck must not meet working"
    );
    // ...but `write` only proves the call happened, which a stuck send still did.
    assert!(Write.is_met_by(St::StuckInComposer, none));
    assert!(Composer.is_met_by(St::StuckInComposer, none));

    // SECURITY: fail-open / failure states never satisfy ANY guarantee level —
    // verification can never falsely upgrade an unverifiable/failed send.
    for state in [
        St::VerificationUnavailable,
        St::PaneCrashedToShell,
        St::SendFailed,
        St::PolicyDenied,
        St::RequiresApproval,
    ] {
        for level in [Write, Composer, Submitted, Working] {
            assert!(
                !level.is_met_by(state, none),
                "{level:?} must not be met by {state:?}"
            );
        }
    }

    // `submitted` is met by submitted + queued-behind-operation (queued is a
    // legitimate success: the text is in the agent's queue), but not stuck.
    assert!(Submitted.is_met_by(St::Submitted, none));
    assert!(Submitted.is_met_by(St::QueuedBehindOperation, none));

    // `working` requires the `submitted` state AND a working-evidence rule id.
    assert!(
        !Working.is_met_by(St::Submitted, none),
        "working needs evidence"
    );
    let working_evidence = vec!["claude_code:working_state:busy".to_string()];
    assert!(Working.is_met_by(St::Submitted, &working_evidence));
    // ...and even with the evidence, a queued (not-yet-working) state is not `working`.
    assert!(!Working.is_met_by(St::QueuedBehindOperation, &working_evidence));

    // Profile requirement: only `write` is profile-free.
    assert!(!Write.requires_submit_profile());
    for level in [Composer, Submitted, Working] {
        assert!(
            level.requires_submit_profile(),
            "{level:?} must require a submit profile"
        );
    }
}

#[test]
fn submit_receipt_state_is_fail_open_for_allowed_sends() {
    // Status-quo: an allowed send with NO verification report is `submitted`
    // (verification never makes a send worse than today's behavior).
    assert_eq!(
        submit_receipt_state(&allowed("send.allow"), None),
        SubmitReceiptState::Submitted
    );

    // A verification report state passes through honestly — including
    // `verification_unavailable` (the explicit fail-open signal) and the
    // stuck/crashed states (surfaced as-is, never upgraded to a false success).
    for state in [
        SubmitReceiptState::Submitted,
        SubmitReceiptState::QueuedBehindOperation,
        SubmitReceiptState::StuckInComposer,
        SubmitReceiptState::PaneCrashedToShell,
        SubmitReceiptState::VerificationUnavailable,
    ] {
        let report = report_with_state(state, vec![]);
        assert_eq!(
            submit_receipt_state(&allowed("send.allow"), Some(&report)),
            state,
            "allowed + report should pass the report state through unchanged",
        );
    }

    // Non-allowed injections map to their typed states regardless of any report.
    let deny = InjectionResult::Denied {
        decision: PolicyDecision::deny_with_rule("blocked", "policy.block"),
        summary: "send <redacted>".to_string(),
        pane_id: 7,
        action: ActionKind::SendText,
        audit_action_id: None,
    };
    assert_eq!(
        submit_receipt_state(&deny, None),
        SubmitReceiptState::PolicyDenied
    );

    let approve = InjectionResult::RequiresApproval {
        decision: PolicyDecision::require_approval("needs sign-off"),
        summary: "send <redacted>".to_string(),
        pane_id: 7,
        action: ActionKind::SendText,
        audit_action_id: None,
    };
    assert_eq!(
        submit_receipt_state(&approve, None),
        SubmitReceiptState::RequiresApproval
    );

    let err = InjectionResult::Error {
        decision: PolicyDecision::allow_with_rule("send.allow"),
        error: "pane write failed".to_string(),
        pane_id: 7,
        action: ActionKind::SendText,
        audit_action_id: None,
    };
    assert_eq!(
        submit_receipt_state(&err, None),
        SubmitReceiptState::SendFailed
    );
}

#[test]
fn submit_receipt_evidence_rule_ids_merge_sorted_and_deduped() {
    let report = report_with_state(
        SubmitReceiptState::Submitted,
        vec!["zzz:later".to_string(), "send.allow".to_string()],
    );
    let merged = submit_receipt_evidence_rule_ids(&allowed("send.allow"), Some(&report));
    // The decision rule id and the report rule ids are merged, sorted, deduped
    // (send.allow appears in both but only once).
    assert_eq!(
        merged,
        vec!["send.allow".to_string(), "zzz:later".to_string()]
    );
}

#[test]
fn idempotency_replay_returns_the_original_receipt() {
    let ft_dir = std::env::temp_dir().join(format!("ft-7h5da38-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ft_dir);
    std::fs::create_dir_all(&ft_dir).expect("temp ft dir");

    let key = "idem:7:00112233445566ff";
    assert!(is_valid_submit_key(key));

    // Unrecorded key -> None.
    assert_eq!(lookup(&ft_dir, key).expect("lookup"), None);

    // Record a submitted receipt, then replay -> the original is returned verbatim.
    let original = report_with_state(
        SubmitReceiptState::Submitted,
        vec!["send.allow".to_string()],
    );
    record(&ft_dir, key, &original).expect("record");
    assert_eq!(
        lookup(&ft_dir, key).expect("lookup").as_ref(),
        Some(&original)
    );

    // A queued-behind-operation receipt under a distinct key is independent.
    let queued_key = "idem:9:aabbccddeeff0011";
    let queued = report_with_state(SubmitReceiptState::QueuedBehindOperation, vec![]);
    record(&ft_dir, queued_key, &queued).expect("record queued");
    assert_eq!(
        lookup(&ft_dir, queued_key).expect("lookup queued").as_ref(),
        Some(&queued)
    );
    assert_eq!(
        lookup(&ft_dir, key).expect("lookup").as_ref(),
        Some(&original)
    );

    let _ = std::fs::remove_dir_all(&ft_dir);
}

#[test]
fn submit_idempotency_key_guard_rejects_traversal_and_malformed() {
    assert!(is_valid_submit_key("idem:0:0123456789abcdef"));
    assert!(is_valid_submit_key(
        "idem:18446744073709551615:deadbeefdeadbeef"
    ));
    // Path traversal / separators / absolute.
    assert!(!is_valid_submit_key("idem:7:../etc/passwd"));
    assert!(!is_valid_submit_key("../escape"));
    assert!(!is_valid_submit_key("idem:7:a/b/c"));
    // Wrong shape: missing prefix, short/long/upper digest, leading-zero alias.
    assert!(!is_valid_submit_key("7:0123456789abcdef"));
    assert!(!is_valid_submit_key("idem:7:short"));
    assert!(!is_valid_submit_key("idem:7:0123456789ABCDEF"));
    assert!(!is_valid_submit_key("idem:07:0123456789abcdef"));
    assert!(!is_valid_submit_key(""));
}
