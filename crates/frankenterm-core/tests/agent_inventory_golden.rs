//! Golden freeze for the core agent-inventory robot payload contract (ft-bs0ec).
//!
//! Pins the canonical JSON serialization of [`AgentInventoryData`], including:
//! - mixed installed-agent rows with both present and omitted optional fields
//! - running-agent inventory keyed by pane id strings
//! - aggregate summary counts
//! - the `filesystem_detection_available` feature-availability bit
//!
//! Regenerate the golden with:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test agent_inventory_golden
//! ```

use frankenterm_core::robot_types::{
    ActiveAgentConvergenceState, ActiveAgentHealthData, ActiveAgentHealthRecord,
    AgentHealthBeadRef, AgentHealthCommitRef, AgentHealthConfidence, AgentHealthEvidenceKind,
    AgentHealthEvidenceLink, AgentHealthProofLane, AgentHealthProofStatus,
    AgentHealthRecommendedAction, AgentHealthRecommendedActionKind, AgentHealthRiskCode,
    AgentHealthRiskFlag, AgentHealthRiskSeverity, AgentInventoryData, AgentInventorySummary,
    InstalledAgentInfo, RunningAgentInfo,
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn mock_agent_inventory() -> AgentInventoryData {
    let installed = vec![
        InstalledAgentInfo {
            slug: "claude".to_string(),
            display_name: Some("Claude Code".to_string()),
            detected: true,
            evidence: vec![
                "found ~/.claude".to_string(),
                "binary: /usr/local/bin/claude".to_string(),
            ],
            root_paths: vec!["/Users/demo/.claude".to_string()],
            config_path: Some("/Users/demo/.claude/settings.json".to_string()),
            binary_path: Some("/usr/local/bin/claude".to_string()),
            version: Some("1.2.3".to_string()),
        },
        InstalledAgentInfo {
            slug: "codex".to_string(),
            display_name: Some("Codex".to_string()),
            detected: true,
            evidence: Vec::new(),
            root_paths: vec!["/Users/demo/.codex".to_string()],
            config_path: None,
            binary_path: Some("/usr/local/bin/codex".to_string()),
            version: Some("0.9.0".to_string()),
        },
        InstalledAgentInfo {
            slug: "aider".to_string(),
            display_name: None,
            detected: true,
            evidence: vec!["detected via ~/.config/aider".to_string()],
            root_paths: Vec::new(),
            config_path: Some("/Users/demo/.config/aider/aider.conf.yml".to_string()),
            binary_path: None,
            version: None,
        },
        InstalledAgentInfo {
            slug: "gemini".to_string(),
            display_name: Some("Gemini CLI".to_string()),
            detected: false,
            evidence: vec!["not installed".to_string()],
            root_paths: Vec::new(),
            config_path: None,
            binary_path: None,
            version: None,
        },
    ];

    let mut running = BTreeMap::new();
    running.insert(
        7,
        RunningAgentInfo {
            slug: "claude".to_string(),
            display_name: None,
            state: "waiting_approval".to_string(),
            session_id: None,
            source: "pane_title".to_string(),
            pane_id: 7,
        },
    );
    running.insert(
        42,
        RunningAgentInfo {
            slug: "codex".to_string(),
            display_name: Some("Codex".to_string()),
            state: "working".to_string(),
            session_id: Some("sess-codex-42".to_string()),
            source: "pattern_engine".to_string(),
            pane_id: 42,
        },
    );

    AgentInventoryData {
        installed,
        running,
        summary: AgentInventorySummary {
            installed_count: 3,
            running_count: 2,
            configured_count: 2,
            installed_but_idle_count: 1,
        },
        filesystem_detection_available: true,
    }
}

fn mock_active_agent_health() -> ActiveAgentHealthData {
    ActiveAgentHealthData::new(
        1_800_000_000_000,
        vec![
            active_agent_record(ActiveAgentFixture {
                agent_id: "codex-idle",
                agent_name: "CodexIdle",
                provider: "codex",
                pane_id: Some(11),
                state: ActiveAgentConvergenceState::Idle,
                bead: Some(bead("ft-idle", "Idle fixture bead", "in_progress")),
                proof_lane: None,
                recommended_action: action(
                    AgentHealthRecommendedActionKind::ContinueWork,
                    "recent pane output is quiet but the bead claim is fresh",
                ),
                evidence: vec![evidence(
                    AgentHealthEvidenceKind::PaneState,
                    "pane 11 classified idle",
                    "pane:11",
                )],
                risk_flags: vec![],
            }),
            active_agent_record(ActiveAgentFixture {
                agent_id: "claude-active",
                agent_name: "ClaudeActive",
                provider: "claude",
                pane_id: Some(12),
                state: ActiveAgentConvergenceState::Active,
                bead: Some(bead("ft-active", "Active fixture bead", "in_progress")),
                proof_lane: None,
                recommended_action: action(
                    AgentHealthRecommendedActionKind::ContinueWork,
                    "agent is producing output and proof is not due yet",
                ),
                evidence: vec![evidence(
                    AgentHealthEvidenceKind::RobotEvent,
                    "recent output event",
                    "event:active",
                )],
                risk_flags: vec![],
            }),
            active_agent_record(ActiveAgentFixture {
                agent_id: "codex-stuck",
                agent_name: "CodexStuck",
                provider: "codex",
                pane_id: Some(13),
                state: ActiveAgentConvergenceState::Stuck,
                bead: Some(bead("ft-stuck", "Stuck fixture bead", "in_progress")),
                proof_lane: None,
                recommended_action: action(
                    AgentHealthRecommendedActionKind::InspectPane,
                    "stale status age plus repeated error event needs inspection",
                ),
                evidence: vec![
                    evidence(
                        AgentHealthEvidenceKind::PlaceholderIdleText,
                        "placeholder idle banner",
                        "pane:13#placeholder",
                    ),
                    evidence(
                        AgentHealthEvidenceKind::RobotEvent,
                        "same error repeated for 15 minutes",
                        "event:stuck",
                    ),
                ],
                risk_flags: vec![risk(
                    AgentHealthRiskCode::StalePaneState,
                    AgentHealthRiskSeverity::High,
                    "pane state is stale beyond the operator threshold",
                    Some(1),
                )],
            }),
            active_agent_record(ActiveAgentFixture {
                agent_id: "gemini-rate",
                agent_name: "GeminiRate",
                provider: "gemini",
                pane_id: Some(14),
                state: ActiveAgentConvergenceState::RateLimited,
                bead: Some(bead("ft-rate", "Rate limit fixture bead", "in_progress")),
                proof_lane: None,
                recommended_action: action(
                    AgentHealthRecommendedActionKind::Wait,
                    "rate-limit evidence is current and should not be reassigned yet",
                ),
                evidence: vec![evidence(
                    AgentHealthEvidenceKind::RobotEvent,
                    "provider rate limit detected",
                    "event:rate-limit",
                )],
                risk_flags: vec![risk(
                    AgentHealthRiskCode::RateLimited,
                    AgentHealthRiskSeverity::Medium,
                    "provider quota window is active",
                    Some(0),
                )],
            }),
            active_agent_record(ActiveAgentFixture {
                agent_id: "codex-converged",
                agent_name: "CodexConverged",
                provider: "codex",
                pane_id: Some(15),
                state: ActiveAgentConvergenceState::Converged,
                bead: Some(bead("ft-done", "Converged fixture bead", "closed")),
                proof_lane: Some(AgentHealthProofLane {
                    command: "cargo test -p frankenterm-core active_agent_health --lib".to_string(),
                    status: AgentHealthProofStatus::Passed,
                    backend: Some("rch".to_string()),
                    last_run_at_ms: Some(1_800_000_000_123),
                    artifact_uri: Some("proof:active-agent-health".to_string()),
                }),
                recommended_action: action(
                    AgentHealthRecommendedActionKind::MarkConverged,
                    "bead is closed and the proof lane passed",
                ),
                evidence: vec![
                    evidence(
                        AgentHealthEvidenceKind::GitCommit,
                        "latest commit pushed",
                        "git:abc1234",
                    ),
                    evidence(
                        AgentHealthEvidenceKind::ProofLane,
                        "proof lane passed",
                        "proof:active-agent-health",
                    ),
                ],
                risk_flags: vec![],
            })
            .with_latest_commit(AgentHealthCommitRef {
                sha: "abc1234".to_string(),
                summary: "test: active agent health fixture".to_string(),
                authored_at_ms: Some(1_800_000_000_050),
                pushed: Some(true),
            }),
            active_agent_record(ActiveAgentFixture {
                agent_id: "unknown-orphan",
                agent_name: "UnknownOrphan",
                provider: "unknown",
                pane_id: None,
                state: ActiveAgentConvergenceState::Unknown,
                bead: None,
                proof_lane: None,
                recommended_action: action(
                    AgentHealthRecommendedActionKind::EscalateHuman,
                    "agent has no pane, no bead assignment, and no proof trail",
                ),
                evidence: vec![evidence(
                    AgentHealthEvidenceKind::ManualNote,
                    "operator imported orphan agent row",
                    "note:unknown",
                )],
                risk_flags: vec![risk(
                    AgentHealthRiskCode::UnknownAgent,
                    AgentHealthRiskSeverity::Critical,
                    "agent cannot be correlated to a live pane or bead",
                    Some(0),
                )],
            }),
        ],
    )
}

trait WithLatestCommit {
    fn with_latest_commit(self, latest_commit: AgentHealthCommitRef) -> Self;
}

impl WithLatestCommit for ActiveAgentHealthRecord {
    fn with_latest_commit(mut self, latest_commit: AgentHealthCommitRef) -> Self {
        self.latest_commit = Some(latest_commit);
        self
    }
}

struct ActiveAgentFixture {
    agent_id: &'static str,
    agent_name: &'static str,
    provider: &'static str,
    pane_id: Option<u64>,
    state: ActiveAgentConvergenceState,
    bead: Option<AgentHealthBeadRef>,
    proof_lane: Option<AgentHealthProofLane>,
    recommended_action: AgentHealthRecommendedAction,
    evidence: Vec<AgentHealthEvidenceLink>,
    risk_flags: Vec<AgentHealthRiskFlag>,
}

fn active_agent_record(fixture: ActiveAgentFixture) -> ActiveAgentHealthRecord {
    ActiveAgentHealthRecord {
        agent_id: fixture.agent_id.to_string(),
        agent_name: Some(fixture.agent_name.to_string()),
        provider: fixture.provider.to_string(),
        pane_id: fixture.pane_id,
        cwd: Some("/Users/demo/projects/frankenterm".to_string()),
        state: fixture.state,
        status_age_ms: Some(30_000),
        bead: fixture.bead,
        latest_commit: None,
        proof_lane: fixture.proof_lane,
        risk_flags: fixture.risk_flags,
        recommended_action: fixture.recommended_action,
        evidence: fixture.evidence,
        confidence: AgentHealthConfidence::High,
    }
}

fn bead(id: &str, title: &str, status: &str) -> AgentHealthBeadRef {
    AgentHealthBeadRef {
        id: id.to_string(),
        title: title.to_string(),
        status: status.to_string(),
        assignee: Some("codex".to_string()),
        status_age_ms: Some(30_000),
    }
}

fn action(kind: AgentHealthRecommendedActionKind, reason: &str) -> AgentHealthRecommendedAction {
    AgentHealthRecommendedAction {
        kind,
        confidence: AgentHealthConfidence::High,
        reason: reason.to_string(),
        command: None,
        evidence_indices: vec![0],
    }
}

fn evidence(kind: AgentHealthEvidenceKind, label: &str, uri: &str) -> AgentHealthEvidenceLink {
    AgentHealthEvidenceLink {
        kind,
        label: label.to_string(),
        uri: Some(uri.to_string()),
        observed_at_ms: Some(1_800_000_000_000),
        note: None,
    }
}

fn risk(
    code: AgentHealthRiskCode,
    severity: AgentHealthRiskSeverity,
    message: &str,
    evidence_index: Option<usize>,
) -> AgentHealthRiskFlag {
    AgentHealthRiskFlag {
        code,
        severity,
        message: message.to_string(),
        evidence_index,
    }
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = Map::new();
            for (k, v) in sorted {
                out.insert(k.clone(), canonicalize(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

fn pretty_canonical(value: &Value) -> String {
    serde_json::to_string_pretty(&canonicalize(value)).expect("serialize inventory")
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("agent_inventory_contract.json")
}

fn active_agent_health_golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("active_agent_health_contract.json")
}

fn read_or_update_golden(path: &PathBuf, actual: &str) -> String {
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixtures dir");
        }
        std::fs::write(path, format!("{actual}\n")).expect("write golden");
        return actual.to_string();
    }

    std::fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "missing agent inventory golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test agent_inventory_golden",
            path.display()
        )
    })
}

fn assert_matches_golden(actual: &str, golden: &PathBuf) {
    let expected = read_or_update_golden(golden, actual);
    let expected_trimmed = expected.trim_end_matches('\n');
    let actual_trimmed = actual.trim_end_matches('\n');

    if expected_trimmed != actual_trimmed {
        let actual_path = golden.with_extension("actual.json");
        let _ = std::fs::write(&actual_path, format!("{actual}\n"));
        panic!(
            "agent inventory golden drift detected. Review the diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test agent_inventory_golden",
            golden.display(),
            actual_path.display()
        );
    }
}

#[test]
fn agent_inventory_contract_matches_golden() {
    let payload = mock_agent_inventory();
    let json = serde_json::to_value(&payload).expect("serialize AgentInventoryData");
    let actual = pretty_canonical(&json);
    assert_matches_golden(&actual, &golden_path());
}

#[test]
fn active_agent_health_contract_matches_golden() {
    let payload = mock_active_agent_health();
    let json = serde_json::to_value(&payload).expect("serialize ActiveAgentHealthData");
    let actual = pretty_canonical(&json);
    assert_matches_golden(&actual, &active_agent_health_golden_path());
}

#[test]
fn active_agent_health_summary_pins_all_convergence_states() {
    let payload = mock_active_agent_health();
    assert_eq!(payload.summary.total_agents, 6);
    assert_eq!(payload.summary.idle_agents, 1);
    assert_eq!(payload.summary.active_agents, 1);
    assert_eq!(payload.summary.stuck_agents, 1);
    assert_eq!(payload.summary.rate_limited_agents, 1);
    assert_eq!(payload.summary.converged_agents, 1);
    assert_eq!(payload.summary.unknown_agents, 1);
    assert_eq!(payload.summary.high_risk_agents, 2);
    assert_eq!(payload.summary.needs_human_action, 1);
    assert_eq!(payload.summary.missing_bead_assignment, 1);
    assert_eq!(payload.summary.missing_proof_lane, 5);
}

#[test]
fn active_agent_health_placeholder_idle_text_is_not_stuck_evidence() {
    let record = active_agent_record(ActiveAgentFixture {
        agent_id: "placeholder-only",
        agent_name: "PlaceholderOnly",
        provider: "codex",
        pane_id: Some(99),
        state: ActiveAgentConvergenceState::Stuck,
        bead: Some(bead(
            "ft-placeholder",
            "Placeholder-only fixture bead",
            "in_progress",
        )),
        proof_lane: None,
        recommended_action: action(
            AgentHealthRecommendedActionKind::InspectPane,
            "placeholder idle text alone is insufficient",
        ),
        evidence: vec![evidence(
            AgentHealthEvidenceKind::PlaceholderIdleText,
            "default idle banner",
            "pane:99#placeholder",
        )],
        risk_flags: vec![],
    });

    assert!(record.has_only_placeholder_idle_evidence());
    assert!(!record.has_actionable_stuck_evidence());
}

#[test]
fn agent_inventory_contract_is_deterministic() {
    let payload = mock_agent_inventory();
    let first = pretty_canonical(&serde_json::to_value(&payload).unwrap());
    let second = pretty_canonical(&serde_json::to_value(&payload).unwrap());
    let third = pretty_canonical(&serde_json::to_value(&payload).unwrap());
    assert_eq!(
        first, second,
        "golden must be deterministic across captures"
    );
    assert_eq!(
        second, third,
        "golden must stay deterministic across captures"
    );
}

#[test]
fn agent_inventory_running_map_uses_stringified_pane_keys() {
    let payload = mock_agent_inventory();
    let json = serde_json::to_value(&payload).expect("serialize AgentInventoryData");
    let running = json["running"]
        .as_object()
        .expect("running inventory should serialize as an object");
    assert!(running.contains_key("7"));
    assert!(running.contains_key("42"));
}
