use frankenterm_core_replay::replay_decision_diff::{
    DecisionDiff, DiffConfig, DivergenceType, EquivalenceLevel, MissionCausalityCategory, RootCause,
};
use frankenterm_core_replay_types::recorder_metadata::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEventSource,
    RecorderIngressKind, RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
};
use frankenterm_core_replay_types::replay_decision_graph::{
    CausalEdge, DecisionEvent, DecisionGraph, DecisionNode, DecisionType,
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

const GOLDEN_SCHEMA: &str = "ft.replay.golden_matrix.v1";
const CANONICAL_SCHEMA: &str = "ft.replay.canonical_output.v1";
const RECORDER_METADATA_GOLDEN_SCHEMA: &str = "ft.recorder.metadata.roundtrip.v1";
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

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RecorderMetadataGolden {
    schema: String,
    recorder_event_schema_version: String,
    event_sources: Vec<RecorderEventSource>,
    text_encodings: Vec<RecorderTextEncoding>,
    redaction_levels: Vec<RecorderRedactionLevel>,
    ingress_kinds: Vec<RecorderIngressKind>,
    segment_kinds: Vec<RecorderSegmentKind>,
    control_marker_types: Vec<RecorderControlMarkerType>,
}

#[derive(Debug, Clone)]
struct DecisionEventSpec {
    decision_type: DecisionType,
    pane_id: u64,
    rule_suffix: u16,
    definition_suffix: u16,
    input_suffix: u16,
    output_suffix: u16,
    timestamp_ms: u64,
    trigger_previous: bool,
    override_two_back: bool,
    confidence_pct: u8,
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/replay/cross_arch_decision_matrix.json")
}

fn decision_graph_roundtrip_golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/replay/decision_graph_roundtrip.v1.json")
}

fn recorder_metadata_roundtrip_golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/replay/recorder_metadata_roundtrip.v1.json")
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

fn arb_decision_type() -> impl Strategy<Value = DecisionType> {
    prop_oneof![
        Just(DecisionType::PatternMatch),
        Just(DecisionType::WorkflowStep),
        Just(DecisionType::PolicyDecision),
        Just(DecisionType::AlertFired),
        Just(DecisionType::OverrideApplied),
        Just(DecisionType::BarrierDecision),
        Just(DecisionType::NoOp),
        Just(DecisionType::PolicyEvaluation),
    ]
}

fn arb_decision_event_specs() -> impl Strategy<Value = Vec<DecisionEventSpec>> {
    prop::collection::vec(
        (
            arb_decision_type(),
            0_u64..8,
            0_u16..256,
            0_u16..256,
            0_u16..256,
            0_u16..256,
            0_u64..10_000,
            any::<bool>(),
            any::<bool>(),
            0_u8..=100,
        ),
        0..32,
    )
    .prop_map(|rows| {
        rows.into_iter()
            .map(
                |(
                    decision_type,
                    pane_id,
                    rule_suffix,
                    definition_suffix,
                    input_suffix,
                    output_suffix,
                    timestamp_ms,
                    trigger_previous,
                    override_two_back,
                    confidence_pct,
                )| DecisionEventSpec {
                    decision_type,
                    pane_id,
                    rule_suffix,
                    definition_suffix,
                    input_suffix,
                    output_suffix,
                    timestamp_ms,
                    trigger_previous,
                    override_two_back,
                    confidence_pct,
                },
            )
            .collect()
    })
}

fn events_from_specs(specs: &[DecisionEventSpec]) -> Vec<DecisionEvent> {
    specs
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            let mut event = DecisionEvent::new(
                spec.decision_type,
                spec.pane_id,
                format!("det.rule.{}", spec.rule_suffix),
                &format!(
                    "definition-version={};kind={:?}",
                    spec.definition_suffix, spec.decision_type
                ),
                &format!(
                    "pane={};input={};position={index}",
                    spec.pane_id, spec.input_suffix
                ),
                serde_json::json!({
                    "output": spec.output_suffix,
                    "pane": spec.pane_id,
                    "kind": format!("{:?}", spec.decision_type),
                }),
                Some(format!("parent-{index}")),
                Some(f64::from(spec.confidence_pct) / 100.0),
                spec.timestamp_ms,
            );
            event.triggered_by = if index > 0 && spec.trigger_previous {
                Some((index - 1) as u64)
            } else {
                None
            };
            event.overrides = if index > 1 && spec.override_two_back {
                Some((index - 2) as u64)
            } else {
                None
            };
            event.wall_clock_ms = spec.timestamp_ms.saturating_add(123_456);
            event.replay_run_id = format!("determinism-run-{}", index % 3);
            event
        })
        .collect()
}

fn graph_node_signature(graph: &DecisionGraph) -> Vec<DecisionNode> {
    graph
        .nodes_canonical()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>()
}

fn graph_edge_signature(graph: &DecisionGraph) -> Vec<CausalEdge> {
    graph.edges().to_vec()
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

fn build_recorder_metadata_golden() -> RecorderMetadataGolden {
    RecorderMetadataGolden {
        schema: RECORDER_METADATA_GOLDEN_SCHEMA.to_string(),
        recorder_event_schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_sources: vec![
            RecorderEventSource::WeztermMux,
            RecorderEventSource::RobotMode,
            RecorderEventSource::Mcp,
            RecorderEventSource::WorkflowEngine,
            RecorderEventSource::Beads,
            RecorderEventSource::Rch,
            RecorderEventSource::AgentMail,
            RecorderEventSource::Git,
            RecorderEventSource::OperatorAction,
            RecorderEventSource::RecoveryFlow,
        ],
        text_encodings: vec![RecorderTextEncoding::Utf8],
        redaction_levels: vec![
            RecorderRedactionLevel::None,
            RecorderRedactionLevel::Partial,
            RecorderRedactionLevel::Full,
        ],
        ingress_kinds: vec![
            RecorderIngressKind::SendText,
            RecorderIngressKind::Paste,
            RecorderIngressKind::WorkflowAction,
        ],
        segment_kinds: vec![
            RecorderSegmentKind::Delta,
            RecorderSegmentKind::Gap,
            RecorderSegmentKind::Snapshot,
        ],
        control_marker_types: vec![
            RecorderControlMarkerType::PromptBoundary,
            RecorderControlMarkerType::Resize,
            RecorderControlMarkerType::PolicyDecision,
            RecorderControlMarkerType::ApprovalCheckpoint,
        ],
    }
}

fn read_golden() -> String {
    fs::read_to_string(golden_path()).expect("golden matrix fixture is checked in")
}

fn read_decision_graph_roundtrip_golden() -> String {
    fs::read_to_string(decision_graph_roundtrip_golden_path())
        .expect("decision graph round-trip fixture is checked in")
}

fn read_recorder_metadata_roundtrip_golden() -> String {
    fs::read_to_string(recorder_metadata_roundtrip_golden_path())
        .expect("recorder metadata round-trip fixture is checked in")
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

#[test]
fn decision_graph_json_roundtrip_matches_golden_artifact() {
    let graph = DecisionGraph::from_decisions(&representative_events(TARGETS[0]));
    let actual = graph.to_json() + "\n";
    let path = decision_graph_roundtrip_golden_path();
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        fs::write(&path, &actual).expect("decision graph golden fixture can be updated");
    }

    assert_eq!(
        read_decision_graph_roundtrip_golden(),
        actual,
        "decision graph golden fixture drifted: {path:?}"
    );

    let restored = DecisionGraph::from_json(&actual).expect("golden graph JSON round-trips");
    assert!(graph.l1_equivalent(&restored));
    assert_eq!(restored.node_count(), 5);
    assert_eq!(restored.edge_count(), 6);
    assert!(restored.is_dag());
    assert_eq!(
        restored
            .roots()
            .into_iter()
            .map(|node| node.node_id)
            .collect::<Vec<_>>(),
        vec![0, 3]
    );
    assert_eq!(
        restored
            .causal_chain(4)
            .into_iter()
            .map(|node| node.node_id)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(
        restored
            .effects(0)
            .into_iter()
            .map(|node| node.node_id)
            .collect::<Vec<_>>(),
        vec![1, 2, 4]
    );
    assert_eq!(restored.to_json() + "\n", actual);
}

#[test]
fn decision_graph_roundtrip_preserves_edge_topology() {
    let restored =
        DecisionGraph::from_json(&read_decision_graph_roundtrip_golden()).expect("golden parses");
    let edges = restored
        .edges()
        .iter()
        .map(|edge| format!("{:?}:{}->{}", edge.edge_type, edge.from_node, edge.to_node))
        .collect::<Vec<_>>();

    assert_eq!(
        edges,
        vec![
            "TriggeredBy:0->1",
            "PrecededBy:0->1",
            "TriggeredBy:1->2",
            "PrecededBy:1->2",
            "TriggeredBy:2->4",
            "PrecededBy:2->4",
        ]
    );
    assert_eq!(
        restored
            .nodes_canonical()
            .into_iter()
            .map(|node| node.rule_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "codex.usage.reached",
            "workflow.cooldown.annotate",
            "policy.wait.allowed",
            "fleet.rate_limit.aggregate",
            "operator.override.resume",
        ]
    );
    assert_eq!(
        restored.nodes_by_type(DecisionType::PolicyDecision).len(),
        1
    );
}

#[test]
fn decision_graph_l1_roundtrip_ignores_transient_replay_metadata() {
    let golden =
        DecisionGraph::from_json(&read_decision_graph_roundtrip_golden()).expect("golden parses");
    let linux = DecisionGraph::from_decisions(&representative_events(TARGETS[1]));

    assert!(golden.l1_equivalent(&linux));
    assert_ne!(
        golden.to_json(),
        linux.to_json(),
        "target-specific wall_clock_ms/replay_run_id must remain visible in full graph JSON"
    );
    assert_eq!(
        canonical_bytes(&canonical_output_for(TARGETS[0])),
        canonical_bytes(&canonical_output_for(TARGETS[1])),
        "canonical replay output should scrub target-specific transient metadata"
    );
}

#[test]
fn forensics_diff_conformance_classifies_policy_causal_chain() {
    let baseline = DecisionGraph::from_decisions(&[
        decision(
            DecisionType::PatternMatch,
            42,
            "usage.limit.detected",
            "pattern=usage-limit;version=1",
            "pane=42 usage limit marker",
            serde_json::json!({"matched": true}),
            None,
            0.99,
            1_000,
        ),
        decision(
            DecisionType::PolicyDecision,
            42,
            "policy.approval_required",
            "policy=approval;version=7",
            "usage limit marker produced approval policy input",
            serde_json::json!({"decision": "allow"}),
            Some(0),
            1.0,
            1_010,
        ),
        decision(
            DecisionType::WorkflowStep,
            42,
            "workflow.continue_after_policy",
            "step=continue;requires=policy.approval_required",
            "policy allow emitted continue workflow input",
            serde_json::json!({"next": "continue"}),
            Some(1),
            1.0,
            1_020,
        ),
    ]);
    let candidate = DecisionGraph::from_decisions(&[
        decision(
            DecisionType::PatternMatch,
            42,
            "usage.limit.detected",
            "pattern=usage-limit;version=1",
            "pane=42 usage limit marker",
            serde_json::json!({"matched": true}),
            None,
            0.99,
            1_000,
        ),
        decision(
            DecisionType::PolicyDecision,
            42,
            "policy.approval_required",
            "policy=approval;version=7",
            "usage limit marker produced approval policy input",
            serde_json::json!({"decision": "require_approval"}),
            Some(0),
            1.0,
            1_010,
        ),
        decision(
            DecisionType::WorkflowStep,
            42,
            "workflow.pause_for_approval",
            "step=pause;requires=policy.approval_required",
            "policy approval emitted pause workflow input",
            serde_json::json!({"next": "wait_for_operator"}),
            Some(1),
            1.0,
            1_020,
        ),
    ]);

    let diff = DecisionDiff::diff(&baseline, &candidate, &DiffConfig::default());
    assert_eq!(diff.summary.total_baseline, 3);
    assert_eq!(diff.summary.total_candidate, 3);
    assert_eq!(diff.summary.unchanged, 1);
    assert_eq!(diff.summary.modified, 1);
    assert_eq!(diff.summary.removed, 1);
    assert_eq!(diff.summary.added, 1);
    assert_eq!(diff.summary.shifted, 0);
    assert!(!diff.is_equivalent(EquivalenceLevel::L0));
    assert!(!diff.is_equivalent(EquivalenceLevel::L1));
    assert!(!diff.is_equivalent(EquivalenceLevel::L2));

    let first = &diff.divergences[0];
    // position is a dense canonical rank since 01f2a5325 (normalize_divergence_positions):
    // the policy Modified divergence (ts 1010) sorts first, ranking 0 — not its original
    // baseline sequence index (1).
    assert_eq!(first.position, 0);
    assert_eq!(first.divergence_type, DivergenceType::Modified);
    assert_eq!(first.root_cause, RootCause::Unknown);
    assert_eq!(
        first
            .baseline_node
            .as_ref()
            .expect("modified diff has a baseline node")
            .rule_id,
        "policy.approval_required"
    );
    assert_eq!(
        first
            .candidate_node
            .as_ref()
            .expect("modified diff has a candidate node")
            .rule_id,
        "policy.approval_required"
    );

    let mission = diff.to_mission_causality_diff(&baseline, &candidate);
    assert!(!mission.summary.identical);
    assert_eq!(mission.summary.total_divergences, 3);
    assert_eq!(
        mission.summary.first_category,
        MissionCausalityCategory::PolicyDecision
    );

    let first_mission = mission
        .first_divergence
        .as_ref()
        .expect("mission diff reports the first divergence");
    assert_eq!(first_mission.position, 0);
    assert_eq!(
        first_mission.category,
        MissionCausalityCategory::PolicyDecision
    );
    assert_eq!(first_mission.divergence_type, DivergenceType::Modified);
    assert_eq!(
        first_mission.evidence_refs,
        vec![
            "divergence_position:0",
            "divergence_type:Modified",
            "root_cause:unknown",
            "baseline_node:1",
            "candidate_node:1",
        ]
    );

    let first_chain = &mission.reason_chains[0];
    assert_eq!(
        first_chain
            .upstream_chain
            .iter()
            .map(|node| node.rule_id.as_str())
            .collect::<Vec<_>>(),
        vec!["usage.limit.detected"]
    );
    assert_eq!(
        first_chain
            .downstream_effects
            .iter()
            .map(|node| node.rule_id.as_str())
            .collect::<Vec<_>>(),
        vec!["workflow.pause_for_approval"]
    );

    let mission_json = mission.to_json();
    assert!(!mission_json.contains("approval policy input"));
    assert!(!mission_json.contains("wait_for_operator"));
}

#[test]
fn forensics_diff_conformance_preserves_timing_equivalence_levels() {
    let baseline = DecisionGraph::from_decisions(&[
        decision(
            DecisionType::PatternMatch,
            7,
            "prompt.boundary",
            "pattern=prompt-boundary;version=2",
            "pane=7 prompt boundary",
            serde_json::json!({"matched": true}),
            None,
            1.0,
            2_000,
        ),
        decision(
            DecisionType::WorkflowStep,
            7,
            "workflow.dispatch_next",
            "step=dispatch-next;version=4",
            "prompt boundary dispatched next step",
            serde_json::json!({"next": "dispatch"}),
            Some(0),
            1.0,
            2_040,
        ),
    ]);
    let candidate = DecisionGraph::from_decisions(&[
        decision(
            DecisionType::PatternMatch,
            7,
            "prompt.boundary",
            "pattern=prompt-boundary;version=2",
            "pane=7 prompt boundary",
            serde_json::json!({"matched": true}),
            None,
            1.0,
            2_000,
        ),
        decision(
            DecisionType::WorkflowStep,
            7,
            "workflow.dispatch_next",
            "step=dispatch-next;version=4",
            "prompt boundary dispatched next step",
            serde_json::json!({"next": "dispatch"}),
            Some(0),
            1.0,
            2_095,
        ),
    ]);

    let diff = DecisionDiff::diff(&baseline, &candidate, &DiffConfig::default());
    assert_eq!(diff.summary.unchanged, 1);
    assert_eq!(diff.summary.shifted, 1);
    assert_eq!(diff.summary.total_divergences(), 1);
    assert!(diff.is_equivalent(EquivalenceLevel::L0));
    assert!(diff.is_equivalent(EquivalenceLevel::L1));
    assert!(!diff.is_equivalent(EquivalenceLevel::L2));
    assert_eq!(diff.divergences[0].divergence_type, DivergenceType::Shifted);
    assert_eq!(
        diff.divergences[0].root_cause,
        RootCause::TimingShift {
            baseline_ms: 2_040,
            candidate_ms: 2_095,
            delta_ms: 55,
        }
    );

    let mission = diff.to_mission_causality_diff(&baseline, &candidate);
    assert_eq!(
        mission.summary.first_category,
        MissionCausalityCategory::Timing
    );
    assert_eq!(
        mission
            .first_divergence
            .as_ref()
            .expect("timing diff has a first divergence")
            .evidence_refs,
        vec![
            "divergence_position:0",
            "divergence_type:Shifted",
            "root_cause:timing_shift",
            "baseline_node:1",
            "candidate_node:1",
        ]
    );
}

#[test]
fn recorder_metadata_enum_catalog_roundtrips_through_golden_artifact() {
    let expected = build_recorder_metadata_golden();
    let actual = serde_json::to_string_pretty(&expected).expect("metadata serializes") + "\n";
    let path = recorder_metadata_roundtrip_golden_path();
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        fs::write(&path, &actual).expect("recorder metadata golden fixture can be updated");
    }

    assert_eq!(
        read_recorder_metadata_roundtrip_golden(),
        actual,
        "recorder metadata golden fixture drifted: {path:?}"
    );

    let restored_actual: RecorderMetadataGolden =
        serde_json::from_str(&actual).expect("generated metadata JSON round-trips");
    assert_eq!(restored_actual, expected);

    let restored_golden: RecorderMetadataGolden =
        serde_json::from_str(&read_recorder_metadata_roundtrip_golden())
            .expect("checked-in metadata golden JSON round-trips");
    assert_eq!(restored_golden, build_recorder_metadata_golden());
}

proptest! {
    #[test]
    fn replay_decision_graph_determinism_property_same_input_yields_identical_graph(
        specs in arb_decision_event_specs(),
    ) {
        let events = events_from_specs(&specs);
        let first = DecisionGraph::from_decisions(&events);
        let second = DecisionGraph::from_decisions(&events);

        prop_assert_eq!(first.to_json(), second.to_json());
        prop_assert_eq!(graph_node_signature(&first), graph_node_signature(&second));
        prop_assert_eq!(graph_edge_signature(&first), graph_edge_signature(&second));
        prop_assert_eq!(first.roots().len(), second.roots().len());
        prop_assert!(first.l1_equivalent(&second));
        prop_assert!(first.is_dag());
        prop_assert!(second.is_dag());
    }

    #[test]
    fn replay_decision_graph_determinism_property_same_bytes_yields_identical_graph(
        specs in arb_decision_event_specs(),
    ) {
        let events = events_from_specs(&specs);
        let bytes = serde_json::to_vec(&events).expect("generated decisions serialize");
        let decoded_first: Vec<DecisionEvent> =
            serde_json::from_slice(&bytes).expect("generated decision bytes decode");
        let decoded_second: Vec<DecisionEvent> =
            serde_json::from_slice(&bytes).expect("same generated decision bytes decode again");

        let first = DecisionGraph::from_decisions(&decoded_first);
        let second = DecisionGraph::from_decisions(&decoded_second);

        let first_json = first.to_json();
        let second_json = second.to_json();
        prop_assert_eq!(first_json.as_bytes(), second_json.as_bytes());
        prop_assert_eq!(graph_node_signature(&first), graph_node_signature(&second));
        prop_assert_eq!(graph_edge_signature(&first), graph_edge_signature(&second));
        prop_assert!(first.l1_equivalent(&second));
        prop_assert!(first.is_dag());
        prop_assert!(second.is_dag());
    }
}
