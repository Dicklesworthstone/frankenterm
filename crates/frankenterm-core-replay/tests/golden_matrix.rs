use frankenterm_core_replay_types::replay_decision_graph::{
    DecisionEvent, DecisionGraph, DecisionType,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

const GOLDEN_SCHEMA: &str = "ft.replay.golden_matrix.v1";
const CANONICAL_SCHEMA: &str = "ft.replay.canonical_output.v1";
const TARGETS: &[&str] = &["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"];

#[derive(Debug, PartialEq, Eq, Serialize)]
struct GoldenMatrix {
    schema: &'static str,
    matrix_targets: Vec<&'static str>,
    canonical_output_sha256: String,
    canonical_output: CanonicalReplayOutput,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct CanonicalReplayOutput {
    schema: &'static str,
    graph: GraphSummary,
    events: Vec<EventSummary>,
    roots: Vec<u64>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct GraphSummary {
    node_count: usize,
    edge_count: usize,
    dag: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct EventSummary {
    node_id: u64,
    decision_type: DecisionType,
    pane_id: u64,
    rule_id: String,
    definition_hash: String,
    input_hash: String,
    output_hash: String,
    timestamp_ms: u64,
    causal_chain: Vec<u64>,
    effects: Vec<u64>,
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/replay/cross_arch_decision_matrix.json")
}

fn representative_events(target_triple: &str) -> Vec<DecisionEvent> {
    let mut events = vec![
        decision(
            DecisionType::PatternMatch,
            7,
            "codex.usage.reached",
            "rule=usage-limit;anchor=Usage limit reached;version=3",
            "pane=7\nUsage limit reached. Try again at 2026-04-29T08:00:00Z.",
            serde_json::json!({
                "captures": ["2026-04-29T08:00:00Z"],
                "matched": true,
                "rule": "codex.usage.reached",
            }),
            None,
            0.98,
            10_000,
        ),
        decision(
            DecisionType::WorkflowStep,
            7,
            "workflow.cooldown.annotate",
            "step=annotate;requires=codex.usage.reached;version=1",
            "cooldown decision for pane=7",
            serde_json::json!({
                "action": "annotate",
                "pane": 7,
                "state": {"next": "wait", "previous": "active"},
            }),
            Some(0),
            1.0,
            10_050,
        ),
        decision(
            DecisionType::PolicyDecision,
            7,
            "policy.wait.allowed",
            "policy=wait;decision=allow;version=2",
            "workflow.cooldown.annotate requested wait",
            serde_json::json!({
                "allow": true,
                "reason": "passive wait is side-effect-free",
            }),
            Some(1),
            1.0,
            10_060,
        ),
        decision(
            DecisionType::AlertFired,
            12,
            "fleet.rate_limit.aggregate",
            "rule=fleet-rate-limit-aggregate;threshold=2;version=4",
            "pane=12\nTwo agents entered cooldown in the same minute.",
            serde_json::json!({
                "fleet_state": {"cooldown": 2, "running": 4},
                "severity": "info",
            }),
            None,
            0.91,
            10_070,
        ),
        decision(
            DecisionType::OverrideApplied,
            7,
            "operator.override.resume",
            "override=resume;requires=policy.allow;version=1",
            "operator approved resume for pane=7",
            serde_json::json!({
                "approved_by": "operator",
                "pane": 7,
                "resume": true,
            }),
            Some(2),
            1.0,
            10_100,
        ),
    ];

    for (index, event) in events.iter_mut().enumerate() {
        event.replay_run_id = format!("{target_triple}:run-{index}");
        event.wall_clock_ms = if target_triple == "aarch64-apple-darwin" {
            1_800_000 + index as u64
        } else {
            2_400_000 + index as u64
        };
    }

    events
}

fn decision(
    decision_type: DecisionType,
    pane_id: u64,
    rule_id: &str,
    definition_text: &str,
    input_text: &str,
    output: serde_json::Value,
    triggered_by: Option<u64>,
    confidence: f64,
    timestamp_ms: u64,
) -> DecisionEvent {
    let mut event = DecisionEvent::new(
        decision_type,
        pane_id,
        rule_id,
        definition_text,
        input_text,
        output,
        None,
        Some(confidence),
        timestamp_ms,
    );
    event.triggered_by = triggered_by;
    event
}

fn canonical_output_for(target_triple: &str) -> CanonicalReplayOutput {
    let events = representative_events(target_triple);
    let graph = DecisionGraph::from_decisions(&events);
    let events = graph
        .nodes_canonical()
        .into_iter()
        .map(|node| EventSummary {
            node_id: node.node_id,
            decision_type: node.decision_type,
            pane_id: node.pane_id,
            rule_id: node.rule_id.clone(),
            definition_hash: node.definition_hash.clone(),
            input_hash: node.input_hash.clone(),
            output_hash: node.output_hash.clone(),
            timestamp_ms: node.timestamp_ms,
            causal_chain: graph
                .causal_chain(node.node_id)
                .into_iter()
                .map(|ancestor| ancestor.node_id)
                .collect(),
            effects: graph
                .effects(node.node_id)
                .into_iter()
                .map(|descendant| descendant.node_id)
                .collect(),
        })
        .collect();

    CanonicalReplayOutput {
        schema: CANONICAL_SCHEMA,
        graph: GraphSummary {
            node_count: graph.node_count(),
            edge_count: graph.edge_count(),
            dag: graph.is_dag(),
        },
        events,
        roots: graph.roots().into_iter().map(|node| node.node_id).collect(),
    }
}

fn canonical_bytes(output: &CanonicalReplayOutput) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(output).expect("canonical output serializes");
    bytes.push(b'\n');
    bytes
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn build_matrix() -> GoldenMatrix {
    let first = canonical_output_for(TARGETS[0]);
    let first_bytes = canonical_bytes(&first);

    for target in &TARGETS[1..] {
        let candidate = canonical_output_for(target);
        assert_eq!(
            canonical_bytes(&candidate),
            first_bytes,
            "canonical replay output drifted for target {target}"
        );
    }

    GoldenMatrix {
        schema: GOLDEN_SCHEMA,
        matrix_targets: TARGETS.to_vec(),
        canonical_output_sha256: sha256_hex(&first_bytes),
        canonical_output: first,
    }
}

fn read_golden() -> String {
    fs::read_to_string(golden_path()).expect("golden matrix fixture is checked in")
}

#[test]
fn golden_matrix_replay_output_is_byte_identical_across_arches() {
    if let Ok(ci_target) = std::env::var("FT_REPLAY_GOLDEN_MATRIX_TARGET") {
        assert!(
            TARGETS.contains(&ci_target.as_str()),
            "unexpected replay golden CI target {ci_target}"
        );
    }

    let actual = serde_json::to_string_pretty(&build_matrix()).expect("matrix serializes") + "\n";
    let path = golden_path();
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        fs::write(&path, &actual).expect("golden fixture can be updated");
    }

    assert_eq!(read_golden(), actual, "golden fixture drifted: {path:?}");
}
