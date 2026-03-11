//! Policy Decision-Proof and Governance Enforcement Suite
//!
//! Proves governance invariants hold under adversarial and edge-case scenarios:
//! 1. Authorization determinism — same input always yields same decision
//! 2. Precedence correctness — kill-switch > quarantine > revocation > namespace > rate-limit
//! 3. Approval lifecycle integrity — no skipped states, no double-grant
//! 4. Revocation immediate enforcement — authorize() denies instantly after revoke
//! 5. Forensic completeness — every deny leaves a trace
//! 6. Quarantine isolation — quarantined components never get allowed
//! 7. Audit chain tamper-evidence — chain invariant holds across operations

use frankenterm_core::policy::{
    ActionKind, ActorKind, ApprovalStatus, ForensicQuery, PolicyDecision,
    PolicyEngine, PolicyInput,
};
use frankenterm_core::policy_quarantine::{
    ComponentKind, KillSwitchLevel, QuarantineReason, QuarantineSeverity,
};

// ============================================================================
// Helpers
// ============================================================================

fn make_engine() -> PolicyEngine {
    PolicyEngine::permissive()
}

fn connector_input(domain: &str) -> PolicyInput {
    PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot).with_domain(domain)
}

fn pane_write_input(pane_id: u64) -> PolicyInput {
    PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(pane_id)
}

fn pane_read_input(pane_id: u64) -> PolicyInput {
    PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot).with_pane(pane_id)
}

fn is_denied(decision: &PolicyDecision) -> bool {
    matches!(decision, PolicyDecision::Deny { .. })
}

fn is_allowed(decision: &PolicyDecision) -> bool {
    matches!(decision, PolicyDecision::Allow { .. })
}

fn deny_rule(decision: &PolicyDecision) -> Option<&str> {
    match decision {
        PolicyDecision::Deny { rule_id, .. } => rule_id.as_deref(),
        _ => None,
    }
}

// ============================================================================
// 1. Authorization Determinism
// ============================================================================

#[test]
fn proof_authorize_deterministic_same_input() {
    let mut engine = make_engine();
    let input = connector_input("test-api");

    let d1 = engine.authorize(&input);
    let d2 = engine.authorize(&input);
    let d3 = engine.authorize(&input);

    // All three decisions must be the same variant
    assert_eq!(is_allowed(&d1), is_allowed(&d2));
    assert_eq!(is_allowed(&d2), is_allowed(&d3));
}

#[test]
fn proof_authorize_deterministic_across_action_kinds() {
    let mut engine = make_engine();

    let actions = [
        ActionKind::SendText,
        ActionKind::ReadOutput,
        ActionKind::ConnectorInvoke,
        ActionKind::ConnectorNotify,
    ];

    for action in &actions {
        let input = PolicyInput::new(*action, ActorKind::Robot).with_pane(1);
        let d1 = engine.authorize(&input);
        let d2 = engine.authorize(&input);
        assert_eq!(
            is_allowed(&d1),
            is_allowed(&d2),
            "Non-deterministic for {:?}",
            action
        );
    }
}

// ============================================================================
// 2. Precedence Correctness
// ============================================================================

#[test]
fn proof_kill_switch_overrides_everything() {
    let mut engine = make_engine();

    // Verify connector works first
    let input = connector_input("payment-api");
    assert!(is_allowed(&engine.authorize(&input)), "should be allowed before kill switch");

    // Trip emergency halt
    engine.trip_kill_switch(
        KillSwitchLevel::EmergencyHalt,
        "admin",
        "security incident",
        1000,
    );

    // Even read actions must be denied
    let read_input = pane_read_input(1);
    let d = engine.authorize(&read_input);
    assert!(is_denied(&d), "kill switch must deny reads");
    assert_eq!(deny_rule(&d), Some("policy.kill_switch"));

    // Connectors also denied
    let d = engine.authorize(&input);
    assert!(is_denied(&d), "kill switch must deny connectors");
    assert_eq!(deny_rule(&d), Some("policy.kill_switch"));
}

#[test]
fn proof_quarantine_overrides_revocation() {
    let mut engine = make_engine();

    // Quarantine pane-1 (Isolated = blocks all)
    engine
        .quarantine_component(
            "pane-1",
            ComponentKind::Pane,
            QuarantineSeverity::Isolated,
            QuarantineReason::PolicyViolation {
                rule_id: "test".into(),
                detail: "test".into(),
            },
            "admin",
            1000,
            0,
        )
        .expect("quarantine should succeed");

    // Also revoke a connector
    engine.revoke_resource("connector", "slack", "test", "admin", 1000);

    // Pane action should be denied by quarantine, not revocation
    let input = pane_write_input(1);
    let d = engine.authorize(&input);
    assert!(is_denied(&d), "quarantine must deny");
    assert_eq!(
        deny_rule(&d),
        Some("policy.quarantine"),
        "quarantine rule takes precedence"
    );
}

#[test]
fn proof_revocation_overrides_namespace() {
    use frankenterm_core::namespace_isolation::{
        CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
    };

    let ns_config = NamespaceIsolationConfig {
        enabled: true,
        cross_tenant_policy: CrossTenantPolicy::strict(),
        ..Default::default()
    };
    let safety = frankenterm_core::config::SafetyConfig {
        namespace_isolation: ns_config,
        ..Default::default()
    };
    let mut engine = PolicyEngine::from_safety_config(&safety);

    // Bind connector to ns-b
    let ns_b = TenantNamespace::new("org-b").unwrap();
    engine.namespace_registry_mut().bind(
        NamespacedResourceKind::Connector,
        "payment-api",
        ns_b,
    );

    // Revoke the connector
    engine.revoke_resource("connector", "payment-api", "compromised", "sec-team", 1000);

    // Access from org-a (cross-tenant) — should be denied by revocation, not namespace
    let ns_a = TenantNamespace::new("org-a").unwrap();
    let input = PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot)
        .with_domain("payment-api")
        .with_namespace(ns_a);
    let d = engine.authorize(&input);
    assert!(is_denied(&d), "revocation must deny");
    assert_eq!(
        deny_rule(&d),
        Some("policy.revocation"),
        "revocation check comes before namespace"
    );
}

#[test]
fn proof_kill_switch_trumps_quarantine() {
    let mut engine = make_engine();

    // Quarantine pane-5
    engine
        .quarantine_component(
            "pane-5",
            ComponentKind::Pane,
            QuarantineSeverity::Isolated,
            QuarantineReason::OperatorDirected {
                operator: "ops".into(),
                note: "test".into(),
            },
            "ops",
            1000,
            0,
        )
        .expect("quarantine ok");

    // Trip kill switch
    engine.trip_kill_switch(
        KillSwitchLevel::EmergencyHalt,
        "admin",
        "total shutdown",
        2000,
    );

    // Any action should be denied by kill_switch (checked first)
    let input = pane_write_input(5);
    let d = engine.authorize(&input);
    assert!(is_denied(&d));
    assert_eq!(
        deny_rule(&d),
        Some("policy.kill_switch"),
        "kill switch checked before quarantine"
    );
}

// ============================================================================
// 3. Approval Lifecycle Integrity
// ============================================================================

#[test]
fn proof_approval_state_machine_no_skip() {
    let mut engine = make_engine();

    // Submit → Pending
    let id = engine.submit_approval("deploy", "bot", "prod", "need deploy", "rule.deploy", 1000, 0);
    let tracker = engine.approval_tracker();
    let entry = tracker.get(&id).expect("should find approval");
    assert_eq!(entry.status, ApprovalStatus::Pending);

    // Cannot revoke a Pending approval (revoke only applies to Approved)
    let revoked = engine.revoke_approval(&id, "admin", 2000);
    assert!(!revoked, "cannot revoke a pending approval");

    // Grant → Approved
    let granted = engine.grant_approval(&id, "manager", 3000);
    assert!(granted, "should grant pending");
    let entry = engine.approval_tracker().get(&id).unwrap();
    assert_eq!(entry.status, ApprovalStatus::Approved);

    // Cannot grant again (idempotent or no-op)
    let double_grant = engine.grant_approval(&id, "manager2", 4000);
    assert!(!double_grant, "cannot grant an already-approved approval");

    // Revoke approved → Revoked
    let revoked = engine.revoke_approval(&id, "sec-team", 5000);
    assert!(revoked, "should revoke approved");
    let entry = engine.approval_tracker().get(&id).unwrap();
    assert_eq!(entry.status, ApprovalStatus::Revoked);

    // Cannot re-approve after revocation
    let re_grant = engine.grant_approval(&id, "manager", 6000);
    assert!(!re_grant, "cannot re-grant after revocation");
}

#[test]
fn proof_approval_reject_is_terminal() {
    let mut engine = make_engine();

    let id = engine.submit_approval("scale", "bot", "infra", "scale up", "rule.scale", 1000, 0);
    let rejected = engine.reject_approval(&id, "manager", 2000);
    assert!(rejected, "should reject pending");

    let entry = engine.approval_tracker().get(&id).unwrap();
    assert_eq!(entry.status, ApprovalStatus::Rejected);

    // Cannot grant after rejection
    let grant = engine.grant_approval(&id, "admin", 3000);
    assert!(!grant, "cannot grant after rejection");

    // Cannot reject again
    let re_reject = engine.reject_approval(&id, "admin", 4000);
    assert!(!re_reject, "cannot reject twice");
}

#[test]
fn proof_approval_expiry_prevents_grant() {
    let mut engine = make_engine();

    // Submit with expiry at 2000ms
    let id = engine.submit_approval("deploy", "bot", "prod", "timed", "rule.timed", 1000, 2000);

    // Expire stale approvals at time 3000 (past expiry)
    engine.approval_tracker_mut().expire_stale(3000);

    let entry = engine.approval_tracker().get(&id).unwrap();
    assert_eq!(entry.status, ApprovalStatus::Expired, "should be expired");

    // Cannot grant expired
    let grant = engine.grant_approval(&id, "admin", 4000);
    assert!(!grant, "cannot grant expired approval");
}

// ============================================================================
// 4. Revocation Immediate Enforcement
// ============================================================================

#[test]
fn proof_revocation_instantly_blocks_authorize() {
    let mut engine = make_engine();
    let input = connector_input("slack-webhook");

    // Allowed before revocation
    assert!(is_allowed(&engine.authorize(&input)), "allowed before revoke");

    // Revoke
    engine.revoke_resource("connector", "slack-webhook", "compromised", "sec", 1000);

    // Immediately denied
    let d = engine.authorize(&input);
    assert!(is_denied(&d), "must be denied immediately after revoke");
    assert_eq!(deny_rule(&d), Some("policy.revocation"));
}

#[test]
fn proof_reinstate_restores_access() {
    let mut engine = make_engine();
    let input = connector_input("github-api");

    // Revoke then reinstate
    let rev_id = engine.revoke_resource("connector", "github-api", "test", "admin", 1000);
    assert!(is_denied(&engine.authorize(&input)));

    let reinstated = engine.reinstate_resource(&rev_id, "admin", 2000);
    assert!(reinstated, "reinstate should succeed");

    // Access restored
    assert!(
        is_allowed(&engine.authorize(&input)),
        "access must be restored after reinstatement"
    );
}

#[test]
fn proof_revocation_scoped_to_resource() {
    let mut engine = make_engine();

    // Revoke only "slack"
    engine.revoke_resource("connector", "slack", "test", "admin", 1000);

    // "github" should still work
    let github_input = connector_input("github");
    assert!(
        is_allowed(&engine.authorize(&github_input)),
        "non-revoked connector must still be accessible"
    );

    // "slack" denied
    let slack_input = connector_input("slack");
    assert!(
        is_denied(&engine.authorize(&slack_input)),
        "revoked connector must be denied"
    );
}

// ============================================================================
// 5. Forensic Completeness — Every Deny Leaves a Trace
// ============================================================================

#[test]
fn proof_every_deny_recorded_in_decision_log() {
    let mut engine = make_engine();

    // Create multiple denial scenarios
    engine.revoke_resource("connector", "api-1", "test", "admin", 1000);
    engine.revoke_resource("connector", "api-2", "test", "admin", 1000);

    let d1 = engine.authorize(&connector_input("api-1"));
    let d2 = engine.authorize(&connector_input("api-2"));
    assert!(is_denied(&d1));
    assert!(is_denied(&d2));

    // Forensic report must capture both denials
    let query = ForensicQuery {
        denials_only: true,
        ..ForensicQuery::default()
    };
    let report = engine.generate_forensic_report(&query, 10000);

    assert!(
        report.decisions.len() >= 2,
        "forensic report must capture all denied decisions, got {}",
        report.decisions.len()
    );
}

#[test]
fn proof_kill_switch_denial_in_audit_chain() {
    let mut engine = make_engine();

    engine.trip_kill_switch(
        KillSwitchLevel::EmergencyHalt,
        "admin",
        "test incident",
        1000,
    );
    engine.authorize(&pane_read_input(1));

    let report = engine.generate_forensic_report(&ForensicQuery::default(), 10000);

    // Kill switch trip must appear in audit trail
    let has_kill_switch_audit = report
        .audit_trail
        .iter()
        .any(|e| e.surface.contains("kill_switch"));
    assert!(
        has_kill_switch_audit,
        "kill switch trip must be in audit trail"
    );

    // Kill switch must be flagged active
    assert!(report.kill_switch_active, "kill_switch_active must be true");
}

#[test]
fn proof_quarantine_denial_recorded() {
    let mut engine = make_engine();

    engine
        .quarantine_component(
            "pane-42",
            ComponentKind::Pane,
            QuarantineSeverity::Isolated,
            QuarantineReason::AnomalousBehavior {
                metric: "cpu".into(),
                observed: "100%".into(),
            },
            "monitor",
            1000,
            0,
        )
        .expect("quarantine ok");

    let d = engine.authorize(&pane_write_input(42));
    assert!(is_denied(&d));

    let report = engine.generate_forensic_report(&ForensicQuery::default(), 10000);

    // Must have audit trail for quarantine action
    let has_quarantine_audit = report
        .audit_trail
        .iter()
        .any(|e| e.description.contains("quarantined pane-42"));
    assert!(
        has_quarantine_audit,
        "quarantine action must appear in audit trail"
    );

    // Active quarantines list must contain the component
    assert!(
        report.quarantine_active.contains(&"pane-42".to_string()),
        "quarantine_active must list pane-42"
    );
}

#[test]
fn proof_revocation_in_forensic_report() {
    let mut engine = make_engine();

    engine.revoke_resource("connector", "rogue-api", "breach", "sec-team", 1000);
    engine.authorize(&connector_input("rogue-api"));

    let report = engine.generate_forensic_report(&ForensicQuery::default(), 10000);

    assert_eq!(report.revocations.len(), 1);
    assert_eq!(report.revocations[0].resource_id, "rogue-api");
    assert!(report.revocations[0].active, "revocation must be active");

    // Compliance denials should be tracked
    assert!(
        report.compliance_summary.total_denials > 0,
        "compliance must record the denial"
    );
}

// ============================================================================
// 6. Quarantine Isolation Guarantees
// ============================================================================

#[test]
fn proof_isolated_quarantine_blocks_all_action_kinds() {
    let mut engine = make_engine();

    engine
        .quarantine_component(
            "pane-99",
            ComponentKind::Pane,
            QuarantineSeverity::Isolated,
            QuarantineReason::CredentialCompromise {
                credential_id: "cred-x".into(),
            },
            "sec",
            1000,
            0,
        )
        .expect("quarantine ok");

    // Test all action kinds against the quarantined pane
    let actions = [
        ActionKind::SendText,
        ActionKind::ReadOutput,
        ActionKind::Split,
        ActionKind::Close,
        ActionKind::Spawn,
    ];

    for action in &actions {
        let input = PolicyInput::new(*action, ActorKind::Robot).with_pane(99);
        let d = engine.authorize(&input);
        assert!(
            is_denied(&d),
            "Isolated quarantine must deny {:?}, but got {:?}",
            action,
            d
        );
    }
}

#[test]
fn proof_restricted_quarantine_allows_reads_denies_writes() {
    let mut engine = make_engine();

    engine
        .quarantine_component(
            "pane-50",
            ComponentKind::Pane,
            QuarantineSeverity::Restricted,
            QuarantineReason::AnomalousBehavior {
                metric: "rate".into(),
                observed: "high".into(),
            },
            "monitor",
            1000,
            0,
        )
        .expect("quarantine ok");

    // Reads should be allowed (Restricted only blocks writes)
    let read = pane_read_input(50);
    assert!(
        is_allowed(&engine.authorize(&read)),
        "Restricted quarantine must allow reads"
    );

    // Writes should be denied
    let write = pane_write_input(50);
    let d = engine.authorize(&write);
    assert!(
        is_denied(&d),
        "Restricted quarantine must deny writes"
    );
    assert_eq!(deny_rule(&d), Some("policy.quarantine"));
}

#[test]
fn proof_quarantine_release_restores_access() {
    let mut engine = make_engine();

    engine
        .quarantine_component(
            "pane-60",
            ComponentKind::Pane,
            QuarantineSeverity::Isolated,
            QuarantineReason::OperatorDirected {
                operator: "admin".into(),
                note: "investigation".into(),
            },
            "admin",
            1000,
            0,
        )
        .expect("quarantine ok");

    // Use ReadOutput (non-mutating) to isolate the quarantine effect
    let input = pane_read_input(60);
    let d = engine.authorize(&input);
    assert!(is_denied(&d), "Isolated quarantine denies reads too");
    assert_eq!(deny_rule(&d), Some("policy.quarantine"), "denied by quarantine");

    // Release
    engine
        .release_component("pane-60", "admin", false, 2000)
        .expect("release ok");

    // After release, ReadOutput should no longer be blocked by quarantine
    let d = engine.authorize(&input);
    assert_ne!(
        deny_rule(&d),
        Some("policy.quarantine"),
        "quarantine must no longer block after release"
    );
    // If permissive, should be allowed
    assert!(
        is_allowed(&d),
        "reads must be allowed after quarantine release"
    );
}

// ============================================================================
// 7. Audit Chain Tamper-Evidence
// ============================================================================

#[test]
fn proof_audit_chain_monotonic_sequence() {
    let mut engine = make_engine();

    // Perform several governance actions
    engine.revoke_resource("connector", "c1", "r1", "admin", 1000);
    engine.revoke_resource("connector", "c2", "r2", "admin", 2000);
    engine.trip_kill_switch(KillSwitchLevel::SoftStop, "admin", "caution", 3000);

    // Access raw audit chain entries for sequence/hash verification
    let entries = engine.audit_chain().entries_in_range(0, u64::MAX);

    // Audit trail entries must have monotonically increasing sequences
    let mut prev_seq = 0u64;
    for entry in &entries {
        assert!(
            entry.sequence > prev_seq || prev_seq == 0,
            "audit chain sequence must be monotonically increasing: {} should be > {}",
            entry.sequence,
            prev_seq
        );
        prev_seq = entry.sequence;
    }
}

#[test]
fn proof_audit_chain_hash_continuity() {
    let mut engine = make_engine();

    engine.revoke_resource("connector", "a", "test", "admin", 1000);
    engine
        .quarantine_component(
            "pane-1",
            ComponentKind::Pane,
            QuarantineSeverity::Advisory,
            QuarantineReason::OperatorDirected {
                operator: "admin".into(),
                note: "check".into(),
            },
            "admin",
            2000,
            0,
        )
        .expect("quarantine ok");
    engine.trip_kill_switch(KillSwitchLevel::HardStop, "admin", "hard stop", 3000);

    // Access raw audit chain entries for hash verification
    let entries = engine.audit_chain().entries_in_range(0, u64::MAX);

    // Each entry's previous_hash should match the prior entry's chain_hash
    for window in entries.windows(2) {
        assert_eq!(
            window[1].previous_hash, window[0].chain_hash,
            "audit chain hash link broken between seq {} and {}",
            window[0].sequence, window[1].sequence
        );
    }
}

#[test]
fn proof_audit_chain_entries_not_empty() {
    let mut engine = make_engine();

    // Each governance operation should produce an audit entry
    engine.revoke_resource("connector", "x", "test", "admin", 1000);
    engine.authorize(&connector_input("x")); // This denial doesn't add to audit chain directly

    let report = engine.generate_forensic_report(&ForensicQuery::default(), 10000);

    assert!(
        !report.audit_trail.is_empty(),
        "governance operations must produce audit trail entries"
    );

    // Verify actor and description are non-empty
    for entry in &report.audit_trail {
        assert!(!entry.actor.is_empty(), "audit entry actor must not be empty");
        assert!(
            !entry.description.is_empty(),
            "audit entry description must not be empty"
        );
    }
}

// ============================================================================
// 8. Composite Governance Scenarios
// ============================================================================

#[test]
fn proof_full_incident_response_lifecycle() {
    let mut engine = make_engine();

    // Phase 1: Normal operation
    let input = connector_input("payment-gateway");
    assert!(is_allowed(&engine.authorize(&input)));

    // Phase 2: Anomaly detected — submit approval for investigation
    let approval_id = engine.submit_approval(
        "investigate",
        "anomaly-detector",
        "payment-gateway",
        "unusual traffic pattern",
        "rule.anomaly",
        1000,
        0,
    );

    // Phase 3: Approval granted
    engine.grant_approval(&approval_id, "sec-lead", 2000);

    // Phase 4: Compromise confirmed — revoke connector
    engine.revoke_resource(
        "connector",
        "payment-gateway",
        "credential compromise confirmed",
        "sec-lead",
        3000,
    );

    // Phase 5: Verify immediate enforcement
    let d = engine.authorize(&input);
    assert!(is_denied(&d), "revoked connector must be denied");

    // Phase 6: Quarantine related pane
    engine
        .quarantine_component(
            "pane-10",
            ComponentKind::Pane,
            QuarantineSeverity::Isolated,
            QuarantineReason::CascadeFromParent {
                parent_component_id: "payment-gateway".into(),
            },
            "sec-lead",
            4000,
            0,
        )
        .expect("quarantine ok");

    // Phase 7: Forensic report captures everything
    let report = engine.generate_forensic_report(&ForensicQuery::default(), 10000);

    assert!(!report.decisions.is_empty(), "decisions captured");
    assert!(!report.audit_trail.is_empty(), "audit trail captured");
    assert_eq!(report.revocations.len(), 1, "one revocation");
    assert_eq!(report.approvals.len(), 1, "one approval");
    assert!(
        report.quarantine_active.contains(&"pane-10".to_string()),
        "quarantine active"
    );
    assert!(
        report.compliance_summary.total_denials > 0,
        "denials tracked"
    );

    // Phase 8: Resolution — reinstate after patch
    let rev_id = &report.revocations[0].revocation_id;
    engine.reinstate_resource(rev_id, "sec-lead", 5000);
    engine
        .release_component("pane-10", "sec-lead", true, 5000) // probation
        .expect("release ok");

    // Phase 9: Verify restored access
    assert!(
        is_allowed(&engine.authorize(&input)),
        "connector access restored"
    );
}

#[test]
fn proof_cascading_governance_actions_all_audited() {
    let mut engine = make_engine();

    // Multiple governance actions in sequence
    let a1 = engine.submit_approval("action1", "bot1", "r1", "reason1", "rule1", 100, 0);
    engine.grant_approval(&a1, "mgr", 200);
    engine.revoke_resource("connector", "c1", "breach", "sec", 300);
    engine
        .quarantine_component(
            "pane-1",
            ComponentKind::Pane,
            QuarantineSeverity::Restricted,
            QuarantineReason::PolicyViolation {
                rule_id: "r1".into(),
                detail: "violation".into(),
            },
            "sec",
            400,
            0,
        )
        .expect("quarantine ok");
    engine.trip_kill_switch(KillSwitchLevel::SoftStop, "admin", "caution", 500);

    let report = engine.generate_forensic_report(&ForensicQuery::default(), 10000);

    // Verify all actions are audited
    assert!(!report.approvals.is_empty(), "approval tracked");
    assert!(!report.revocations.is_empty(), "revocation tracked");
    assert!(!report.quarantine_active.is_empty(), "quarantine tracked");
    assert!(report.audit_trail.len() >= 4, "at least 4 audit trail entries (revoke, quarantine, kill-switch, quarantine compliance)");
}

#[test]
fn proof_concurrent_revocation_and_quarantine_both_enforced() {
    let mut engine = make_engine();

    // Both revoke a connector and quarantine its associated pane
    engine.revoke_resource("connector", "slack-bot", "breach", "sec", 1000);
    engine
        .quarantine_component(
            "pane-20",
            ComponentKind::Pane,
            QuarantineSeverity::Isolated,
            QuarantineReason::CascadeFromParent {
                parent_component_id: "slack-bot".into(),
            },
            "sec",
            1000,
            0,
        )
        .expect("quarantine ok");

    // Both must be denied independently
    let connector_d = engine.authorize(&connector_input("slack-bot"));
    assert!(is_denied(&connector_d), "revoked connector denied");
    assert_eq!(deny_rule(&connector_d), Some("policy.revocation"));

    let pane_d = engine.authorize(&pane_write_input(20));
    assert!(is_denied(&pane_d), "quarantined pane denied");
    assert_eq!(deny_rule(&pane_d), Some("policy.quarantine"));
}

// ============================================================================
// 9. Namespace Isolation Proof
// ============================================================================

#[test]
fn proof_namespace_strict_isolation() {
    use frankenterm_core::namespace_isolation::{
        CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
    };

    let ns_config = NamespaceIsolationConfig {
        enabled: true,
        cross_tenant_policy: CrossTenantPolicy::strict(),
        ..Default::default()
    };
    let safety = frankenterm_core::config::SafetyConfig {
        namespace_isolation: ns_config,
        ..Default::default()
    };
    let mut engine = PolicyEngine::from_safety_config(&safety);

    let ns_prod = TenantNamespace::new("prod").unwrap();
    let ns_dev = TenantNamespace::new("dev").unwrap();

    engine.namespace_registry_mut().bind(
        NamespacedResourceKind::Connector,
        "database-prod",
        ns_prod.clone(),
    );

    // Same namespace → allowed
    let same_ns = PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot)
        .with_domain("database-prod")
        .with_namespace(ns_prod);
    assert!(
        is_allowed(&engine.authorize(&same_ns)),
        "same-namespace access must be allowed"
    );

    // Cross namespace → denied
    let cross_ns = PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot)
        .with_domain("database-prod")
        .with_namespace(ns_dev);
    let d = engine.authorize(&cross_ns);
    assert!(
        is_denied(&d),
        "cross-namespace access must be denied under strict policy"
    );
    assert_eq!(deny_rule(&d), Some("policy.namespace_isolation"));
}

// ============================================================================
// 10. Decision Context Evidence
// ============================================================================

#[test]
fn proof_deny_decisions_carry_context() {
    let mut engine = make_engine();

    engine.revoke_resource("connector", "api", "test", "admin", 1000);
    let d = engine.authorize(&connector_input("api"));

    match &d {
        PolicyDecision::Deny { context, .. } => {
            assert!(context.is_some(), "deny must carry context");
            let ctx = context.as_ref().unwrap();
            assert!(
                !ctx.rules_evaluated.is_empty(),
                "deny context must have evaluated rules"
            );
            let rule = &ctx.rules_evaluated[0];
            assert_eq!(
                rule.rule_id, "policy.revocation",
                "matched rule must identify the cause"
            );
            assert!(rule.matched, "rule must be marked as matched");
        }
        _ => panic!("expected Deny, got {:?}", d),
    }
}

#[test]
fn proof_compliance_counters_consistent_with_decisions() {
    let mut engine = make_engine();

    // Make some allow and deny decisions
    let allowed_input = connector_input("good-api");
    engine.authorize(&allowed_input);
    engine.authorize(&allowed_input);

    engine.revoke_resource("connector", "bad-api", "test", "admin", 1000);
    let denied_input = connector_input("bad-api");
    engine.authorize(&denied_input);
    engine.authorize(&denied_input);

    let report = engine.generate_forensic_report(&ForensicQuery::default(), 10000);

    // Total evaluations should be >= 4 (2 allowed + 2 denied)
    assert!(
        report.compliance_summary.total_evaluations >= 4,
        "total evaluations should be >= 4, got {}",
        report.compliance_summary.total_evaluations
    );

    // Denials should be >= 2
    assert!(
        report.compliance_summary.total_denials >= 2,
        "total denials should be >= 2, got {}",
        report.compliance_summary.total_denials
    );

    // Denial rate should be > 0 and < 100
    assert!(
        report.compliance_summary.denial_rate_percent > 0.0,
        "denial rate must be > 0"
    );
    assert!(
        report.compliance_summary.denial_rate_percent < 100.0,
        "denial rate must be < 100"
    );
}
