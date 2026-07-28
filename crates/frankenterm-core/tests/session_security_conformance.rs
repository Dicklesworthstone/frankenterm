//! Consolidated session security-conformance suite — core crate (ft-bkn2g).
//!
//! Asserts the session's security fixes still hold so the whole "advertised-
//! but-unenforced" class cannot silently regress. ZERO-RCH.
//!
//! Coverage in this file (frankenterm-core):
//!   * ft-l59nq — kill-switch SoftStop/HardStop/EmergencyHalt tiers enforced
//!     across action classes (behavioral, public API).
//!   * ft-pfton — persisted/captured content is redacted by the real Redactor
//!     chokepoint, and the CaptureRedactionPolicy scaffolding keeps secure
//!     defaults (behavioral, public API).
//!   * ft-cdxrr — the operator data-classifier is consulted on the outbound
//!     connector dispatch path (wiring guard; the behavioral dispatch test
//!     lives inline in connector_outbound_bridge.rs).
//!   * ft-ps9fu — the MCP wa://swarm/scent read routes through the read-policy
//!     + Redactor boundary (wiring guard; needs a live wezterm handle to drive
//!     behaviorally).
//!
//! Scripting-crate fixes (ft-0dki4 WASM sandbox, ft-a58cq extension
//! path-traversal) are covered by the sibling suite
//! frankenterm/scripting/tests/session_security_conformance.rs.
//! ft-l59nq also has the dedicated killswitch_tier_enforcement_conformance.rs;
//! this file adds the cross-cutting representative gate.

use std::path::Path;

use frankenterm_core::policy::{ActionKind, ActorKind, PolicyEngine, PolicyInput};
use frankenterm_core::policy_quarantine::KillSwitchLevel;
use frankenterm_core::redactor::{REDACTED_MARKER, Redactor};
use frankenterm_core::replay_capture::{CaptureRedactionMode, CaptureRedactionPolicy};

// ---------------------------------------------------------------------------
// Source-scan helpers for the wiring guards (a deleted production gate must
// fail even if a same-named callsite survives inside a test module).
// ---------------------------------------------------------------------------

fn read_core_src(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn brace_delta(line: &str) -> i32 {
    let opens = line.bytes().filter(|&b| b == b'{').count() as i32;
    let closes = line.bytes().filter(|&b| b == b'}').count() as i32;
    opens - closes
}

/// Strip `#[cfg(test)]`-attributed items so wiring assertions count only
/// production code (line-based brace tracking; adequate for these sources).
fn production_only(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut armed = false;
    let mut in_test = false;
    let mut depth: i32 = 0;
    for line in src.lines() {
        if !in_test && !armed && line.trim_start().starts_with("#[cfg(test)]") {
            armed = true;
            continue;
        }
        if armed {
            if line.contains('{') {
                depth += brace_delta(line);
                if depth > 0 {
                    in_test = true;
                } else {
                    depth = 0;
                }
                armed = false;
                continue;
            }
            if line.trim_end().ends_with(';') {
                armed = false;
            }
            continue;
        }
        if in_test {
            depth += brace_delta(line);
            if depth <= 0 {
                in_test = false;
                depth = 0;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// ft-l59nq — kill-switch tiers enforced across action classes
// ---------------------------------------------------------------------------

fn killswitch_denies(level: KillSwitchLevel, action: ActionKind) -> bool {
    let mut engine = PolicyEngine::permissive();
    if level != KillSwitchLevel::Disarmed {
        engine
            .quarantine_registry_mut()
            .trip_kill_switch(level, "conformance", "ft-bkn2g", 1_000);
    }
    let decision = engine.authorize(&PolicyInput::new(action, ActorKind::Robot));
    decision.is_denied() && decision.rule_id() == Some("policy.kill_switch")
}

#[test]
fn ft_l59nq_killswitch_tiers_enforced_across_action_classes() {
    use ActionKind::{DeleteFile, ExecCommand, ReadOutput, WorkflowRun, WriteFile};
    use KillSwitchLevel::{Disarmed, EmergencyHalt, HardStop, SoftStop};

    // Disarmed: nothing blocked by the kill switch.
    assert!(!killswitch_denies(Disarmed, WorkflowRun));

    // SoftStop: pauses NEW workflow launches (the exact filed gap — a pane-less
    // WorkflowRun used to sail through), but routine reads still flow.
    assert!(
        killswitch_denies(SoftStop, WorkflowRun),
        "SoftStop must deny WorkflowRun"
    );
    assert!(
        !killswitch_denies(SoftStop, ReadOutput),
        "SoftStop must still allow reads"
    );

    // HardStop: blocks all new non-read actions — including the classes the old
    // is_mutating() gate excluded (WorkflowRun/exec/file) — reads stay open.
    for action in [WorkflowRun, ExecCommand, WriteFile, DeleteFile] {
        assert!(
            killswitch_denies(HardStop, action),
            "HardStop must deny {action:?}"
        );
    }
    assert!(
        !killswitch_denies(HardStop, ReadOutput),
        "HardStop must still allow reads (drain visibility)"
    );

    // EmergencyHalt: blocks even reads.
    assert!(
        killswitch_denies(EmergencyHalt, ReadOutput),
        "EmergencyHalt must block even reads"
    );
}

// ---------------------------------------------------------------------------
// ft-pfton — real persisted/captured-content redaction + secure scaffolding
// ---------------------------------------------------------------------------

#[test]
fn ft_pfton_persisted_content_redaction_masks_secrets() {
    // The production capture/persist path redacts via crate::policy::Redactor.
    let redactor = Redactor::new();

    // AWS access key (AKIA[0-9A-Z]{16}).
    let aws = redactor.redact("aws_key=AKIAIOSFODNN7EXAMPLE trailing");
    assert!(
        !aws.contains("AKIAIOSFODNN7EXAMPLE"),
        "AWS access key must be masked, got: {aws}"
    );
    assert!(aws.contains(REDACTED_MARKER));

    // GitHub token (gh[pousr]_[a-zA-Z0-9]{36,}).
    let token_body = "a".repeat(36);
    let gh = redactor.redact(&format!("Authorization: ghp_{token_body}"));
    assert!(
        !gh.contains(&token_body),
        "GitHub token must be masked, got: {gh}"
    );
    assert!(gh.contains(REDACTED_MARKER));

    // Non-secret content is untouched (redaction is targeted, not destructive).
    assert_eq!(redactor.redact("ordinary log line"), "ordinary log line");
}

#[test]
fn ft_pfton_capture_policy_keeps_secure_target_defaults() {
    // The CaptureRedactionPolicy scaffolding is not-yet-enforced (documented),
    // but its defaults must stay secure-by-target so wiring it later cannot
    // accidentally ship redaction-off or invert retention.
    let policy = CaptureRedactionPolicy::default();
    assert!(policy.enabled, "default must target redaction ON");
    assert_eq!(policy.mode, CaptureRedactionMode::Mask);
    assert!(
        policy.t3_retention_days <= policy.t1_retention_days,
        "most-sensitive (T3) retention must not exceed routine (T1)"
    );
}

// ---------------------------------------------------------------------------
// ft-cdxrr — operator data-classifier consulted on outbound dispatch
// ---------------------------------------------------------------------------

#[test]
fn ft_cdxrr_classifier_consulted_on_outbound_dispatch() {
    let src = production_only(&read_core_src("connector_outbound_bridge.rs"));
    for needle in [
        "classify_event(",
        "ingestion_decision(",
        "actions_blocked_classification",
    ] {
        assert!(
            src.contains(needle),
            "ft-cdxrr regression: outbound dispatch classification gate missing `{needle}` \
             (the configured data_classifier would stop processing events)"
        );
    }
}

// ---------------------------------------------------------------------------
// ft-ps9fu — MCP swarm-scent read routed through policy + redactor
// ---------------------------------------------------------------------------

#[test]
fn ft_ps9fu_swarm_scent_routed_through_policy_and_redactor() {
    let src = production_only(&read_core_src("mcp_resources.rs"));
    for needle in ["authorize_swarm_scent_panes(", "redact_swarm_scent_report("] {
        assert!(
            src.contains(needle),
            "ft-ps9fu regression: wa://swarm/scent read must route through `{needle}` \
             (else pane cwd/metadata is emitted raw, bypassing wa.state redaction + read policy)"
        );
    }
}
