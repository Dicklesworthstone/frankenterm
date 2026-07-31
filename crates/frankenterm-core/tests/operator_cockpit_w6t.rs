//! W6.T — Standalone integration proof for the Attention Console + `ft intervene`
//! operator cockpit (bead ft-7h5da.7.6, zero-RCH slice).
//!
//! This harness exercises ONLY the shipped, public W6 surfaces, using the
//! library's public API (no source edits, no daemon, no wall-clock-dependent
//! ranking). It is deliberately net-new relative to the existing inline
//! `attention_router` unit tests (which use private test helpers) and
//! `proptest_intervention_console.rs` (which covers console primitives but not
//! the emergency-stop fail-closed gate or the `expire_stale_approvals` method).
//!
//! Coverage map (W6.T acceptance):
//!
//! W6.1 — `AttentionItem` read-only aggregation across many source kinds:
//! facts from policy-denials, approvals, connector breaker, stuck panes, and
//! ownership conflicts all fold into ranked items, the input is never mutated,
//! and the cockpit surface is read-only.
//!
//! W6.2 — Deterministic ranking (`ft robot next` / `ft ln`): severity-dominant
//! order, golden-stable across rebuilds and JSON encodings, mandatory non-empty
//! `reasons[]` + `suggested_command`, advisory-only, and token-budget elision
//! with a continuation cursor.
//!
//! W6.3 — `ft intervene` console: every verb writes an audit row, the
//! emergency-stop policy gate fails closed (resume is blocked and audited),
//! approvals are one-shot tokens, and stale approvals expire via TTL.
//!
//! Explicitly NOT covered here (BLOCKED / unimplemented at this SHA — tracked as
//! the lane-gated remainder on ft-7h5da.7.6): W6.4 `ft deck` ftui composition,
//! W6.5 `wa://swarm/scent` TTL resource, W6.6 notification mutes + dedup.

use frankenterm_core::attention_router::{
    AttentionRouterSeverity, AttentionRouterSourceAdapterInput, AttentionRouterSourceFact,
    AttentionRouterSourceFactKind, AttentionRouterSourceHealth, AttentionRouterSourceKind,
    AttentionRouterSourceObservation, AttentionRouterSurface, build_attention_router_next_view,
    build_attention_router_snapshot, build_attention_router_surface_payload,
};
use frankenterm_core::intervention_console::{
    ApprovalStatus, EmergencyScope, InterventionAction, InterventionConsole, PaneControlState,
    PendingApproval, RiskLevel,
};

// Fixed inputs keep the whole attention-router pipeline pure & deterministic:
// nothing in the scoring path reads the wall clock.
const GENERATED_AT_MS: u64 = 1_770_000_100_000;
const FRESHNESS_MS: u64 = 120_000;

/// Build a healthy, live read-only observation carrying exactly one fact.
/// Uniform freshness across sources makes severity the dominant ranking signal.
fn observation(
    source_id: &str,
    kind: AttentionRouterSourceKind,
    fact: AttentionRouterSourceFact,
) -> AttentionRouterSourceObservation {
    AttentionRouterSourceObservation::new(
        source_id,
        kind,
        AttentionRouterSourceHealth::Available,
        "fixture.read_only",
        format!("{source_id} source observation"),
    )
    .live(GENERATED_AT_MS, FRESHNESS_MS)
    .with_fact(fact)
}

/// A realistic multi-source cockpit scenario: five distinct attention-worthy
/// facts spanning five source kinds and four classifications/severities.
///
/// | source kind                | fact                    | classification | severity |
/// |----------------------------|-------------------------|----------------|----------|
/// | Git (ownership conflict)   | Manual + reason         | DoNotTouch     | Critical |
/// | PolicyDeniedAudit          | PolicyDeniedAudit       | BlockedDomain  | High     |
/// | ApprovalStore              | ApprovalPending         | WaitingComm    | High     |
/// | ConnectorBreaker           | ConnectorBreakerOpen    | BlockedInfra   | High     |
/// | StuckPaneClassification    | StuckPaneClassification | StaleClaim     | Medium   |
fn cockpit_input() -> AttentionRouterSourceAdapterInput {
    AttentionRouterSourceAdapterInput::new(GENERATED_AT_MS, "/repo")
        .with_observation(observation(
            "git.overlap",
            AttentionRouterSourceKind::Git,
            AttentionRouterSourceFact::new(
                AttentionRouterSourceFactKind::Manual,
                "two agents claim ft-own-1",
            )
            .with_bead_id("ft-own-1")
            .with_reason_code("ownership.conflict"),
        ))
        .with_observation(observation(
            "policy.audit",
            AttentionRouterSourceKind::PolicyDeniedAudit,
            AttentionRouterSourceFact::new(
                AttentionRouterSourceFactKind::PolicyDeniedAudit,
                "send blocked by policy gate",
            )
            .with_bead_id("ft-pol-2"),
        ))
        .with_observation(observation(
            "approval.store",
            AttentionRouterSourceKind::ApprovalStore,
            AttentionRouterSourceFact::new(
                AttentionRouterSourceFactKind::ApprovalPending,
                "approval pending for pane 4",
            )
            .with_bead_id("ft-apr-3"),
        ))
        .with_observation(observation(
            "connector.breaker",
            AttentionRouterSourceKind::ConnectorBreaker,
            AttentionRouterSourceFact::new(
                AttentionRouterSourceFactKind::ConnectorBreakerOpen,
                "github connector breaker is open",
            ),
        ))
        .with_observation(observation(
            "stuck.pane",
            AttentionRouterSourceKind::StuckPaneClassification,
            AttentionRouterSourceFact::new(
                AttentionRouterSourceFactKind::StuckPaneClassification,
                "pane 9 idle for 40m",
            )
            .with_agent_name("agent-x"),
        ))
}

const EXPECTED_DOMAINS: [&str; 5] = [
    "git",
    "policy_denied_audit",
    "approval_store",
    "connector_breaker",
    "stuck_pane_classification",
];

// =============================================================================
// W6.1 — read-only aggregation across every source
// =============================================================================

#[test]
fn w6_1_attention_items_aggregate_from_every_source_kind() {
    let input = cockpit_input();
    let snapshot = build_attention_router_snapshot(&input);

    // One ranked item per attention-worthy fact, each with provenance evidence.
    assert_eq!(
        snapshot.items.len(),
        5,
        "every distinct source fact must aggregate into exactly one attention item"
    );

    let mut domains: Vec<String> = snapshot
        .items
        .iter()
        .map(|item| item.domain.clone())
        .collect();
    domains.sort();
    let mut expected: Vec<String> = EXPECTED_DOMAINS.iter().map(|d| (*d).to_string()).collect();
    expected.sort();
    assert_eq!(
        domains, expected,
        "items must be sourced from all five cockpit source kinds"
    );

    // Every item is corroborated by at least one source observation, and the
    // recorded evidence faithfully points back at its originating source.
    for item in &snapshot.items {
        assert!(
            !item.evidence.is_empty(),
            "item {} must carry read-only source evidence",
            item.item_id
        );
        assert!(
            !item.recommended_action.summary.trim().is_empty(),
            "item {} must carry a safe recommended-action summary",
            item.item_id
        );
        assert!(
            snapshot
                .sources
                .iter()
                .any(|src| src.source_kind == item.evidence[0].source_kind),
            "item {} evidence must reference a real source in the snapshot",
            item.item_id
        );
    }
}

#[test]
fn w6_1_scoring_is_read_only_and_executes_no_side_effects() {
    let input = cockpit_input();
    let input_before = input.clone();

    let snapshot = build_attention_router_snapshot(&input);

    // The aggregation path borrows the input immutably and must never mutate it.
    assert_eq!(
        input, input_before,
        "aggregation must not mutate the adapter input (read-only contract)"
    );
    assert!(
        !snapshot.side_effects_executed,
        "the attention router must never execute side effects while scoring"
    );
}

#[test]
fn w6_1_cockpit_surface_is_read_only_composition_only() {
    // W6.4's deck is a composition layer with NO decision logic; the underlying
    // surface payload must therefore advertise itself as strictly read-only.
    let input = cockpit_input();
    let payload =
        build_attention_router_surface_payload(&input, AttentionRouterSurface::Next, "w6t", None);

    assert!(payload.dry_run, "cockpit surface must be dry-run");
    assert!(
        !payload.raw_pane_content_stored,
        "cockpit surface must not persist raw pane content"
    );
    assert!(
        !payload.raw_message_bodies_stored,
        "cockpit surface must not persist raw message bodies"
    );
    assert!(
        !payload.live_mutation_allowed,
        "cockpit surface must not permit live mutation"
    );
    assert!(
        !payload.side_effects_executed,
        "cockpit surface must not execute side effects"
    );

    assert!(
        !payload.mcp_resources.is_empty(),
        "cockpit must advertise its MCP resources"
    );
    for resource in &payload.mcp_resources {
        assert!(
            resource.read_only,
            "MCP resource {} must be read-only",
            resource.name
        );
        assert!(
            !resource.live_mutation_allowed,
            "MCP resource {} must forbid live mutation",
            resource.name
        );
    }

    // The surface composes over the very same scored pipeline.
    assert_eq!(payload.snapshot.items.len(), 5);
}

// =============================================================================
// W6.2 — deterministic, explainable ranking (`ft robot next` / `ft ln`)
// =============================================================================

#[test]
fn w6_2_next_view_is_severity_dominant_and_explainable() {
    let snapshot = build_attention_router_snapshot(&cockpit_input());
    let view = build_attention_router_next_view(&snapshot, None);

    assert!(
        view.advisory,
        "`next` is advisory, never an exclusive gateway"
    );
    assert_eq!(view.total_items, 5);
    assert_eq!(view.returned_items, 5);
    assert!(view.cursor.is_none(), "no budget => no continuation cursor");
    assert_eq!(view.entries.len(), 5);

    // Ranks are dense + 1-based; composite score is non-increasing; and severity
    // dominates the ordering (top-48-bits of the composite). The single Critical
    // (ownership) item ranks first; the single Medium (stuck pane) item ranks
    // last.
    let mut prev_composite = u64::MAX;
    let mut prev_severity_weight = u32::MAX;
    for (index, entry) in view.entries.iter().enumerate() {
        assert_eq!(
            entry.rank,
            u32::try_from(index + 1).expect("rank fits u32"),
            "ranks must be dense and 1-based"
        );
        assert!(
            entry.score.composite <= prev_composite,
            "composite score must be non-increasing down the ranked list"
        );
        assert!(
            entry.severity.weight() <= prev_severity_weight,
            "severity must dominate the composite ordering"
        );
        prev_composite = entry.score.composite;
        prev_severity_weight = entry.severity.weight();

        // W6.2 mandatory explainability fields.
        assert!(
            !entry.reasons.is_empty(),
            "entry {} must carry a non-empty reasons[]",
            entry.item_id
        );
        assert!(
            entry.reasons.iter().all(|reason| !reason.trim().is_empty()),
            "entry {} must not carry a blank reason",
            entry.item_id
        );
        assert!(
            !entry.suggested_command.trim().is_empty(),
            "entry {} must carry a self-teaching suggested_command",
            entry.item_id
        );
        assert!(
            entry.estimated_tokens > 0,
            "entry {} token estimate must be positive so budgets progress",
            entry.item_id
        );
    }

    assert_eq!(
        view.entries[0].severity,
        AttentionRouterSeverity::Critical,
        "the ownership-conflict item is the lone Critical and must rank first"
    );
    assert_eq!(
        view.entries.last().unwrap().severity,
        AttentionRouterSeverity::Medium,
        "the stuck-pane stale-claim item is the lone Medium and must rank last"
    );

    // At least one entry teaches a concrete, read-only follow-up command.
    assert!(
        view.entries
            .iter()
            .any(|entry| entry.suggested_command.starts_with("br show ")),
        "bead-bound items should suggest the read-only `br show <bead>` command"
    );
}

#[test]
fn w6_2_next_view_is_golden_stable_across_rebuilds_and_json() {
    let snapshot = build_attention_router_snapshot(&cockpit_input());

    // Determinism: scoring the same input twice yields byte-identical rankings.
    let snapshot_b = build_attention_router_snapshot(&cockpit_input());
    let view_a = build_attention_router_next_view(&snapshot, None);
    let view_b = build_attention_router_next_view(&snapshot_b, None);
    assert_eq!(
        view_a, view_b,
        "ranking must be deterministic (golden-fixture stable)"
    );

    // The JSON encoding (the robot/MCP wire surface) is itself stable, and the
    // view round-trips losslessly through serde.
    let json_a = serde_json::to_string(&view_a).expect("serialize next view");
    let json_b = serde_json::to_string(&view_b).expect("serialize next view");
    assert_eq!(json_a, json_b, "next-view JSON must be byte-stable");

    let decoded: frankenterm_core::attention_router::AttentionRouterNextView =
        serde_json::from_str(&json_a).expect("deserialize next view");
    assert_eq!(decoded, view_a, "next view must round-trip through JSON");
}

#[test]
fn w6_2_next_view_budget_elides_with_continuation_cursor() {
    let snapshot = build_attention_router_snapshot(&cockpit_input());
    let total = snapshot.items.len();
    let full = build_attention_router_next_view(&snapshot, None);
    let first_tokens = full.entries[0].estimated_tokens;

    // A budget sized to the top entry returns exactly it and yields a cursor.
    let trimmed = build_attention_router_next_view(&snapshot, Some(first_tokens));
    assert_eq!(trimmed.returned_items, 1);
    assert_eq!(trimmed.elided_items, total - 1);
    let cursor = trimmed
        .cursor
        .expect("budget elision must yield a continuation cursor");
    assert_eq!(cursor.after_rank, 1);
    assert_eq!(cursor.after_item_id, full.entries[0].item_id);
    assert_eq!(cursor.remaining_items, total - 1);

    // A zero budget still emits the top entry — `next` is never empty.
    let minimal = build_attention_router_next_view(&snapshot, Some(0));
    assert_eq!(
        minimal.returned_items, 1,
        "next is never empty when items exist"
    );
    assert!(minimal.cursor.is_some());

    // A generous budget returns everything with no cursor, identical to no budget.
    let everything = build_attention_router_next_view(&snapshot, Some(u32::MAX));
    assert_eq!(everything.returned_items, total);
    assert!(everything.cursor.is_none());
    assert_eq!(everything.entries, full.entries);
}

// =============================================================================
// W6.3 — `ft intervene` console: audit, fail-closed policy gate, one-shot, TTL
// =============================================================================

#[test]
fn w6_3_emergency_stop_blocks_resume_and_audits_the_denial() {
    let mut console = InterventionConsole::new();
    console.register_pane(1);

    assert!(
        console
            .execute("op", InterventionAction::PausePane { pane_id: 1 })
            .success
    );
    assert_eq!(console.pane_state(1), PaneControlState::Paused);

    // Trip the global kill switch.
    assert!(
        console
            .execute(
                "op",
                InterventionAction::EmergencyStop {
                    scope: EmergencyScope::Global,
                },
            )
            .success
    );
    assert!(console.is_emergency_stop_active());

    // The policy gate fails CLOSED: resume is denied while the stop is active.
    let resume = console.execute("op", InterventionAction::ResumePane { pane_id: 1 });
    assert!(
        !resume.success,
        "resume must be blocked while emergency stop is active"
    );
    assert!(
        resume.message.contains("emergency stop"),
        "denial message must explain the emergency-stop gate, got: {}",
        resume.message
    );
    assert_eq!(
        console.pane_state(1),
        PaneControlState::Paused,
        "pane must remain Paused after a blocked resume"
    );

    // The blocked resume is still written to the audit trail (fail-closed and
    // observable). Find the resume record and assert it failed.
    let blocked = console
        .audit_log()
        .iter()
        .rev()
        .find(|record| matches!(record.action, InterventionAction::ResumePane { pane_id: 1 }))
        .expect("blocked resume must be audited");
    assert!(!blocked.result.success);
    assert_eq!(blocked.operator, "op");

    // Releasing the stop re-opens the gate.
    assert!(
        console
            .execute("op", InterventionAction::ReleaseEmergencyStop)
            .success
    );
    let resume2 = console.execute("op", InterventionAction::ResumePane { pane_id: 1 });
    assert!(
        resume2.success,
        "resume must succeed once the stop is released"
    );
    assert_eq!(console.pane_state(1), PaneControlState::Active);
}

#[test]
fn w6_3_pane_scoped_emergency_gate_is_precise() {
    let mut console = InterventionConsole::new();
    console.register_pane(7);
    console.register_pane(8);
    console.execute("op", InterventionAction::PausePane { pane_id: 7 });
    console.execute("op", InterventionAction::PausePane { pane_id: 8 });

    console.execute(
        "op",
        InterventionAction::EmergencyStop {
            scope: EmergencyScope::Pane(7),
        },
    );

    // Pane 7 is in-scope: resume blocked. Pane 8 is out-of-scope: resume allowed.
    assert!(
        !console
            .execute("op", InterventionAction::ResumePane { pane_id: 7 })
            .success,
        "in-scope pane resume must be blocked"
    );
    assert!(
        console
            .execute("op", InterventionAction::ResumePane { pane_id: 8 })
            .success,
        "out-of-scope pane resume must be allowed"
    );
    assert_eq!(console.pane_state(7), PaneControlState::Paused);
    assert_eq!(console.pane_state(8), PaneControlState::Active);
}

#[test]
fn w6_3_approval_is_a_one_shot_token() {
    let mut console = InterventionConsole::new();
    let request_id = console.submit_approval(4, "deploy release artifact", RiskLevel::High, 0);
    assert_eq!(console.pending_approvals().len(), 1);

    // First approval consumes the token.
    let first = console.execute("op", InterventionAction::ApproveRequest { request_id });
    assert!(first.success);
    assert!(
        console.pending_approvals().is_empty(),
        "an approved request must leave the pending queue (consumed)"
    );
    assert_eq!(console.snapshot().total_approvals_processed, 1);

    // The token is one-shot: re-approving fails and does not re-increment.
    let replay = console.execute("op", InterventionAction::ApproveRequest { request_id });
    assert!(
        !replay.success,
        "an already-consumed token cannot be re-approved"
    );
    assert!(replay.message.contains("not Pending"));
    assert_eq!(
        console.snapshot().total_approvals_processed,
        1,
        "replaying an approval must not double-count"
    );

    // Rejecting an already-consumed token also fails (no resurrection).
    let late_reject = console.execute(
        "op",
        InterventionAction::RejectRequest {
            request_id,
            reason: "too late".into(),
        },
    );
    assert!(!late_reject.success);

    // Submission + the three execute() calls each wrote an audit row.
    assert_eq!(
        console.audit_log().len(),
        4,
        "submit + approve + re-approve + reject must each be audited"
    );
}

#[test]
fn w6_3_pending_approval_ttl_boundary_is_exact() {
    // Pure, time-free boundary check of the TTL predicate.
    let approval = PendingApproval {
        request_id: 1,
        pane_id: 0,
        description: "deploy".into(),
        risk_level: RiskLevel::High,
        created_at_ms: 1_000,
        ttl_ms: 500,
        status: ApprovalStatus::Pending,
    };
    assert!(
        !approval.is_expired(1_000),
        "not expired at creation instant"
    );
    assert!(
        !approval.is_expired(1_499),
        "not expired one ms before deadline"
    );
    assert!(
        approval.is_expired(1_500),
        "expired exactly at created + ttl"
    );
    assert!(approval.is_expired(5_000), "expired well past the deadline");

    let no_ttl = PendingApproval {
        ttl_ms: 0,
        ..approval
    };
    assert!(
        !no_ttl.is_expired(u64::MAX),
        "ttl_ms == 0 means the approval never expires"
    );
}

#[test]
fn w6_3_stale_approvals_expire_and_cannot_be_approved() {
    let mut console = InterventionConsole::new();
    // 1ms TTL — guaranteed stale after the sleep below.
    let request_id = console.submit_approval(2, "deploy", RiskLevel::High, 1);

    std::thread::sleep(std::time::Duration::from_millis(25));

    let expired = console.expire_stale_approvals();
    assert_eq!(
        expired, 1,
        "the lone stale approval must be swept exactly once"
    );
    assert!(
        console.pending_approvals().is_empty(),
        "expired approvals must drop out of the pending queue"
    );

    // An expired token can no longer be approved.
    let late = console.execute("op", InterventionAction::ApproveRequest { request_id });
    assert!(
        !late.success,
        "an expired approval token cannot be approved"
    );
}
