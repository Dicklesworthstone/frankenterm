//! Contract tests for the ft-b94bx.4 high-scale simulation corpus.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

const CONTRACT_ID: &str = "ft.swarm_capacity_simulation_corpus.v1";
const SCHEMA_VERSION: u16 = 1;
const REQUIRED_SCALES: [u32; 4] = [50, 100, 200, 500];
const REQUIRED_FEATURES: [&str; 6] = [
    "idle_tails",
    "build_bursts",
    "rate_limits",
    "blocker_cascades",
    "context_rotations",
    "render_resize_storms",
];

#[derive(Debug, Deserialize)]
struct SimulationScenario {
    schema_version: u16,
    contract_id: String,
    scenario_id: String,
    stable_seed: u64,
    pane_count: u32,
    content_hash: String,
    workload_mix: Vec<WorkloadMixRow>,
    features: Vec<String>,
    expected_bottleneck: String,
    evidence_state_assumptions: EvidenceStateAssumptions,
    expected_summary: ExpectedSummary,
    decision_trace: Vec<DecisionStep>,
    raw_pane_content_stored: bool,
    side_effects_executed: bool,
}

#[derive(Debug, Deserialize)]
struct WorkloadMixRow {
    workload_class: String,
    pane_count: u32,
    requested_units_per_pane: u32,
}

#[derive(Debug, Deserialize)]
struct EvidenceStateAssumptions {
    context_horizon: String,
    blocker_radar: String,
    herd_wave: String,
    resource_pressure: String,
}

#[derive(Debug, Deserialize)]
struct ExpectedSummary {
    admitted_panes: u32,
    deferred_panes: u32,
    throttled_panes: u32,
    shed_panes: u32,
    admitted_units: u64,
    deferred_units: u64,
    throttled_units: u64,
    shed_units: u64,
    total_requested_units: u64,
}

#[derive(Debug, Deserialize)]
struct DecisionStep {
    step_index: u32,
    step_id: String,
    capacity_units: u64,
    admission_action: String,
    admitted_units: u64,
    deferred_units: u64,
    throttled_units: u64,
    shed_units: u64,
    evidence_state: String,
    reason_code: String,
}

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("swarm_capacity_simulation_corpus")
        .join("high_scale.v1.jsonl")
}

fn doc_path() -> PathBuf {
    workspace_root()
        .join("docs")
        .join("swarm-capacity-simulation-corpus.md")
}

fn e2e_path() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("e2e")
        .join("test_swarm_capacity_simulation_corpus.sh")
}

fn load_scenarios() -> Vec<SimulationScenario> {
    let path = fixture_path();
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str::<SimulationScenario>(line).unwrap_or_else(|err| {
                panic!(
                    "failed to parse JSONL line {} in {}: {err}",
                    index + 1,
                    path.display()
                )
            })
        })
        .collect()
}

fn hash_material(scenario: &SimulationScenario) -> String {
    let mix = scenario
        .workload_mix
        .iter()
        .map(|row| {
            format!(
                "{}:{}:{}",
                row.workload_class, row.pane_count, row.requested_units_per_pane
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let trace = scenario
        .decision_trace
        .iter()
        .map(|step| {
            format!(
                "{}:{}:{}:{}:{}:{}:{}:{}:{}",
                step.step_index,
                step.step_id,
                step.capacity_units,
                step.admission_action,
                step.admitted_units,
                step.deferred_units,
                step.throttled_units,
                step.shed_units,
                step.reason_code
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    format!(
        "v{}|{}|{}|seed:{}|panes:{}|mix:{}|features:{}|bottleneck:{}|evidence:context_horizon={},blocker_radar={},herd_wave={},resource_pressure={}|summary:{}/{}/{}/{}/{}|trace:{}",
        scenario.schema_version,
        scenario.contract_id,
        scenario.scenario_id,
        scenario.stable_seed,
        scenario.pane_count,
        mix,
        scenario.features.join(","),
        scenario.expected_bottleneck,
        scenario.evidence_state_assumptions.context_horizon,
        scenario.evidence_state_assumptions.blocker_radar,
        scenario.evidence_state_assumptions.herd_wave,
        scenario.evidence_state_assumptions.resource_pressure,
        scenario.expected_summary.admitted_units,
        scenario.expected_summary.deferred_units,
        scenario.expected_summary.throttled_units,
        scenario.expected_summary.shed_units,
        scenario.expected_summary.total_requested_units,
        trace
    )
}

fn content_hash(scenario: &SimulationScenario) -> String {
    let digest = Sha256::digest(hash_material(scenario).as_bytes());
    format!("sha256:{digest:x}")
}

#[test]
fn corpus_parses_and_covers_required_scales_features_and_artifacts() {
    let scenarios = load_scenarios();
    assert_eq!(scenarios.len(), REQUIRED_SCALES.len());

    let scales = scenarios
        .iter()
        .map(|scenario| scenario.pane_count)
        .collect::<Vec<_>>();
    assert_eq!(scales, REQUIRED_SCALES);

    let all_features = scenarios
        .iter()
        .flat_map(|scenario| scenario.features.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    for required in REQUIRED_FEATURES {
        assert!(
            all_features.contains(required),
            "corpus omits required feature {required}"
        );
    }

    for path in [doc_path(), e2e_path()] {
        assert!(
            path.exists(),
            "required artifact is missing: {}",
            path.display()
        );
    }
}

#[test]
fn summaries_match_workload_mix_and_are_side_effect_free() {
    for scenario in load_scenarios() {
        assert_eq!(scenario.schema_version, SCHEMA_VERSION);
        assert_eq!(scenario.contract_id, CONTRACT_ID);
        assert!(
            !scenario.raw_pane_content_stored,
            "{} must not retain raw pane content",
            scenario.scenario_id
        );
        assert!(
            !scenario.side_effects_executed,
            "{} must be dry-run only",
            scenario.scenario_id
        );

        let pane_total = scenario
            .workload_mix
            .iter()
            .map(|row| row.pane_count)
            .sum::<u32>();
        assert_eq!(pane_total, scenario.pane_count, "{}", scenario.scenario_id);

        let unit_total = scenario
            .workload_mix
            .iter()
            .map(|row| u64::from(row.pane_count) * u64::from(row.requested_units_per_pane))
            .sum::<u64>();
        assert_eq!(
            unit_total, scenario.expected_summary.total_requested_units,
            "{}",
            scenario.scenario_id
        );

        let pane_actions = scenario.expected_summary.admitted_panes
            + scenario.expected_summary.deferred_panes
            + scenario.expected_summary.throttled_panes
            + scenario.expected_summary.shed_panes;
        assert_eq!(
            pane_actions, scenario.pane_count,
            "{}",
            scenario.scenario_id
        );

        let unit_actions = scenario.expected_summary.admitted_units
            + scenario.expected_summary.deferred_units
            + scenario.expected_summary.throttled_units
            + scenario.expected_summary.shed_units;
        assert_eq!(
            unit_actions, scenario.expected_summary.total_requested_units,
            "{}",
            scenario.scenario_id
        );
    }
}

#[test]
fn content_hashes_and_seeds_are_stable_and_unique() {
    let scenarios = load_scenarios();
    let mut seeds = BTreeSet::new();
    let mut hashes = BTreeSet::new();

    for scenario in scenarios {
        assert!(
            seeds.insert(scenario.stable_seed),
            "duplicate stable seed {}",
            scenario.stable_seed
        );
        assert!(
            hashes.insert(scenario.content_hash.clone()),
            "duplicate content hash {}",
            scenario.content_hash
        );
        assert_eq!(
            scenario.content_hash,
            content_hash(&scenario),
            "{} content hash drifted",
            scenario.scenario_id
        );
    }
}

#[test]
fn jsonl_logging_contract_has_ordered_capacity_and_admission_steps() {
    let allowed_actions = [
        "admit",
        "admit_with_stagger",
        "defer",
        "throttle_capture_polling",
        "shed",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    let allowed_evidence = ["measured", "inferred", "simulated", "stale", "unavailable"]
        .into_iter()
        .collect::<BTreeSet<_>>();

    for scenario in load_scenarios() {
        assert!(
            !scenario.decision_trace.is_empty(),
            "{} must emit per-step decisions",
            scenario.scenario_id
        );
        for (expected_index, step) in scenario.decision_trace.iter().enumerate() {
            assert_eq!(
                step.step_index, expected_index as u32,
                "{} decision steps must be stable and contiguous",
                scenario.scenario_id
            );
            assert!(
                step.capacity_units > 0,
                "{}:{} capacity units must be non-zero",
                scenario.scenario_id,
                step.step_id
            );
            assert!(
                allowed_actions.contains(step.admission_action.as_str()),
                "{}:{} unknown admission action {}",
                scenario.scenario_id,
                step.step_id,
                step.admission_action
            );
            assert!(
                allowed_evidence.contains(step.evidence_state.as_str()),
                "{}:{} unknown evidence state {}",
                scenario.scenario_id,
                step.step_id,
                step.evidence_state
            );
            assert!(
                step.reason_code.starts_with("capacity.simulation."),
                "{}:{} reason code must stay in the simulation namespace",
                scenario.scenario_id,
                step.step_id
            );
        }
    }
}

#[test]
fn doc_mentions_every_scenario_and_privacy_sentinel_is_absent() {
    let scenarios = load_scenarios();
    let mut scanned = fs::read_to_string(doc_path()).expect("simulation corpus doc is readable");
    scanned.push_str(
        &fs::read_to_string(fixture_path()).expect("simulation corpus fixture is readable"),
    );
    scanned.push_str(&fs::read_to_string(e2e_path()).expect("simulation corpus e2e is readable"));

    for scenario in scenarios {
        assert!(
            scanned.contains(&scenario.scenario_id),
            "doc/e2e surface omits {}",
            scenario.scenario_id
        );
    }

    for sentinel in [
        concat!("Bearer ", "ft-b94bx-private-token"),
        concat!("Cookie: ", "ft_session=private"),
        concat!("PROMPT", "_BODY:"),
        concat!("raw pane ", "excerpt with secret"),
        concat!("sk-", "proj-"),
    ] {
        assert!(
            !scanned.contains(sentinel),
            "simulation corpus leaked raw-content sentinel {sentinel}"
        );
    }
}
