//! Kill-switch tier × action-kind conformance matrix (ft-xxfwy.14, closing
//! ft-l59nq).
//!
//! Every `PolicyEngine` tier (`disarmed`, `soft_stop`, `hard_stop`,
//! `emergency_halt`) is crossed with every `ActionKind`, with and without a
//! pane, and the resulting decision is recorded: whether the kill-switch rule
//! (`policy.kill_switch`) denied it, or whether the action reached the rest
//! of the policy pipeline. The matrix is committed as
//! `docs/attestations/proofs/killswitch-tier-enforcement.json` and this test
//! fails when the live engine and the committed artifact disagree, so the
//! artifact is a proof of the shipped behaviour, not a hand-written table.
//!
//! Regenerate deliberately with
//! `FT_KILLSWITCH_MATRIX_BLESS=1 cargo test -p frankenterm-core --test killswitch_tier_matrix`
//! and review the diff.
//!
//! Tier contract under test (docs/robot-contracts/kill-switch.md):
//! - `disarmed` gates nothing;
//! - `soft_stop` denies new workflow launches (`WorkflowRun`,
//!   `ConnectorTriggerWorkflow`), pane or no pane, and nothing else;
//! - `hard_stop` denies every action that is not read-only, pane or no pane,
//!   and keeps `ReadOutput` / `SearchOutput` / `Activate` open
//!   (`ActionKind::is_read_only`);
//! - `emergency_halt` denies everything, reads included.
//!
//! `every_action_kind_is_in_the_matrix` pins the two classification sets to
//! the engine's own predicates so a reclassified variant fails here instead
//! of silently changing what a tier means.

use std::path::PathBuf;

use frankenterm_core::policy::{ActionKind, ActorKind, PolicyEngine, PolicyInput};
use frankenterm_core::policy_quarantine::KillSwitchLevel;
use serde_json::{Value, json};

const ARTIFACT_REL_PATH: &str = "docs/attestations/proofs/killswitch-tier-enforcement.json";
const KILL_SWITCH_RULE: &str = "policy.kill_switch";

const TIERS: [KillSwitchLevel; 4] = [
    KillSwitchLevel::Disarmed,
    KillSwitchLevel::SoftStop,
    KillSwitchLevel::HardStop,
    KillSwitchLevel::EmergencyHalt,
];

/// Every `ActionKind` variant. Adding a variant without listing it here
/// fails `every_action_kind_is_in_the_matrix` below.
const ACTIONS: [ActionKind; 24] = [
    ActionKind::SendText,
    ActionKind::SendCtrlC,
    ActionKind::SendCtrlD,
    ActionKind::SendCtrlZ,
    ActionKind::SendControl,
    ActionKind::Spawn,
    ActionKind::Split,
    ActionKind::Activate,
    ActionKind::Close,
    ActionKind::BrowserAuth,
    ActionKind::WorkflowRun,
    ActionKind::ReservePane,
    ActionKind::ReleasePane,
    ActionKind::ReadOutput,
    ActionKind::SearchOutput,
    ActionKind::WriteFile,
    ActionKind::DeleteFile,
    ActionKind::ExecCommand,
    ActionKind::ConnectorNotify,
    ActionKind::ConnectorTicket,
    ActionKind::ConnectorTriggerWorkflow,
    ActionKind::ConnectorAuditLog,
    ActionKind::ConnectorInvoke,
    ActionKind::ConnectorCredentialAction,
];

/// `ActionKind::is_read_only` as documented in the contract: observation plus
/// focusing a pane. HardStop keeps exactly these open.
const READS: [ActionKind; 3] = [
    ActionKind::ReadOutput,
    ActionKind::SearchOutput,
    ActionKind::Activate,
];
const WORKFLOW_LAUNCHES: [ActionKind; 2] = [
    ActionKind::WorkflowRun,
    ActionKind::ConnectorTriggerWorkflow,
];

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .ancestors()
        .find(|p| p.join("docs/attestations/manifest.json").is_file())
        .map(PathBuf::from)
        .expect("workspace root with docs/attestations/manifest.json")
}

fn engine_at(level: KillSwitchLevel) -> PolicyEngine {
    let mut engine = PolicyEngine::permissive();
    if level != KillSwitchLevel::Disarmed {
        assert!(
            engine.quarantine_registry_mut().trip_kill_switch(
                level,
                "matrix",
                "conformance",
                1_000
            ),
            "trip to {level} must succeed on a disarmed engine"
        );
    }
    engine
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cell {
    tier: KillSwitchLevel,
    action: ActionKind,
    pane: Option<u64>,
    kill_switch_denied: bool,
    decision: &'static str,
    rule_id: Option<String>,
}

fn evaluate(tier: KillSwitchLevel, action: ActionKind, pane: Option<u64>) -> Cell {
    let mut engine = engine_at(tier);
    let mut input = PolicyInput::new(action, ActorKind::Robot);
    if let Some(p) = pane {
        input = input.with_pane(p);
    }
    let decision = engine.authorize(&input);
    let rule_id = decision.rule_id().map(str::to_string);
    let kill_switch_denied = decision.is_denied() && rule_id.as_deref() == Some(KILL_SWITCH_RULE);
    let label = if decision.is_allowed() {
        "allow"
    } else if decision.requires_approval() {
        "approval"
    } else {
        "deny"
    };
    Cell {
        tier,
        action,
        pane,
        kill_switch_denied,
        decision: label,
        rule_id,
    }
}

fn matrix() -> Vec<Cell> {
    let mut rows = Vec::with_capacity(TIERS.len() * ACTIONS.len() * 2);
    for tier in TIERS {
        for action in ACTIONS {
            for pane in [None, Some(1)] {
                rows.push(evaluate(tier, action, pane));
            }
        }
    }
    rows
}

fn expected_kill_switch_denial(tier: KillSwitchLevel, action: ActionKind) -> bool {
    match tier {
        KillSwitchLevel::Disarmed => false,
        KillSwitchLevel::SoftStop => WORKFLOW_LAUNCHES.contains(&action),
        KillSwitchLevel::HardStop => !READS.contains(&action),
        KillSwitchLevel::EmergencyHalt => true,
    }
}

fn artifact(rows: &[Cell]) -> Value {
    let rows_json: Vec<Value> = rows
        .iter()
        .map(|c| {
            json!({
                "tier": c.tier.to_string(),
                "action": format!("{:?}", c.action),
                "pane": c.pane,
                "kill_switch_denied": c.kill_switch_denied,
                "decision": c.decision,
                "rule_id": c.rule_id,
            })
        })
        .collect();
    let verdict = json!({
        "disarmed_gates_nothing": rows.iter().filter(|c| c.tier == KillSwitchLevel::Disarmed).all(|c| !c.kill_switch_denied),
        "soft_stop_denies_only_workflow_launches": rows.iter().filter(|c| c.tier == KillSwitchLevel::SoftStop).all(|c| c.kill_switch_denied == WORKFLOW_LAUNCHES.contains(&c.action)),
        "hard_stop_denies_every_non_read": rows.iter().filter(|c| c.tier == KillSwitchLevel::HardStop).all(|c| c.kill_switch_denied == !READS.contains(&c.action)),
        "emergency_halt_denies_everything": rows.iter().filter(|c| c.tier == KillSwitchLevel::EmergencyHalt).all(|c| c.kill_switch_denied),
        "pane_presence_never_changes_the_gate": TIERS.iter().all(|t| ACTIONS.iter().all(|a| {
            let with = rows.iter().find(|c| c.tier == *t && c.action == *a && c.pane.is_some()).map(|c| c.kill_switch_denied);
            let without = rows.iter().find(|c| c.tier == *t && c.action == *a && c.pane.is_none()).map(|c| c.kill_switch_denied);
            with == without
        })),
    });
    json!({
        "schema": "ft.proof.killswitch-tier-enforcement.v1",
        "produced_by_bead": "ft-xxfwy.14",
        "closes": ["ft-l59nq"],
        "generator": "crates/frankenterm-core/tests/killswitch_tier_matrix.rs",
        "rule_id": KILL_SWITCH_RULE,
        "tiers": TIERS.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
        "action_kinds": ACTIONS.iter().map(|a| format!("{a:?}")).collect::<Vec<_>>(),
        "row_count": rows.len(),
        "verdict": verdict,
        "rows": rows_json,
    })
}

#[test]
fn every_tier_matches_the_documented_contract_with_and_without_a_pane() {
    for cell in matrix() {
        assert_eq!(
            cell.kill_switch_denied,
            expected_kill_switch_denial(cell.tier, cell.action),
            "tier={} action={:?} pane={:?} decision={} rule={:?}",
            cell.tier,
            cell.action,
            cell.pane,
            cell.decision,
            cell.rule_id
        );
    }
}

#[test]
fn every_action_kind_is_in_the_matrix() {
    // The Debug names double as the artifact's action labels; a new variant
    // that is not listed in ACTIONS shows up here as a count mismatch once
    // ActionKind grows (the compiler cannot enumerate it for us).
    let mut names: Vec<String> = ACTIONS.iter().map(|a| format!("{a:?}")).collect();
    names.sort();
    names.dedup();
    assert_eq!(
        names.len(),
        ACTIONS.len(),
        "duplicate ActionKind in ACTIONS"
    );
    for action in ACTIONS {
        assert_eq!(
            action.is_read_only(),
            READS.contains(&action),
            "READS must equal ActionKind::is_read_only for {action:?}"
        );
        assert_eq!(
            action.is_workflow_launch(),
            WORKFLOW_LAUNCHES.contains(&action),
            "WORKFLOW_LAUNCHES must equal ActionKind::is_workflow_launch for {action:?}"
        );
    }
    let source =
        std::fs::read_to_string(workspace_root().join("crates/frankenterm-core/src/policy.rs"))
            .expect("read policy.rs");
    let start = source
        .find("pub enum ActionKind {")
        .expect("ActionKind enum in policy.rs");
    let body_end = source[start..].find("\n}").expect("enum end") + start;
    let declared: Vec<&str> = source[start..body_end]
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            if t.starts_with("///") || t.starts_with('#') || t.starts_with("pub enum") {
                return None;
            }
            t.strip_suffix(',')
                .filter(|v| v.chars().all(|c| c.is_ascii_alphanumeric()))
        })
        .collect();
    let mut declared_sorted: Vec<String> = declared.iter().map(|s| (*s).to_string()).collect();
    declared_sorted.sort();
    assert_eq!(
        declared_sorted, names,
        "ACTIONS in this test must list every ActionKind variant"
    );
}

#[test]
fn committed_artifact_matches_the_live_engine() {
    let live = artifact(&matrix());
    let path = workspace_root().join(ARTIFACT_REL_PATH);
    let pretty = serde_json::to_string_pretty(&live).expect("serialize") + "\n";
    if std::env::var("FT_KILLSWITCH_MATRIX_BLESS").as_deref() == Ok("1") {
        std::fs::write(&path, pretty).expect("write artifact");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{} is missing ({e}); bless it with FT_KILLSWITCH_MATRIX_BLESS=1",
            path.display()
        )
    });
    let committed: Value = serde_json::from_str(&committed).expect("artifact is JSON");
    assert_eq!(
        committed,
        live,
        "{} drifted from the live engine; re-bless deliberately and review the diff",
        path.display()
    );
    assert!(
        live["verdict"]
            .as_object()
            .expect("verdict object")
            .values()
            .all(|v| v == &Value::Bool(true)),
        "every verdict must hold: {}",
        live["verdict"]
    );
}
