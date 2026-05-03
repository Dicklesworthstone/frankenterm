//! Deterministic synthetic swarm fixture factory.
//!
//! Bead: `ft-1650n.3`.
//!
//! The factory emits recorder events that look like real pane output from a
//! mixed agent fleet, but it does not fake the ft paths that consume those
//! events. The verification helper drives the real pattern engine and semantic
//! chunking policy, producing an auditable manifest for 10/50/100/200-pane
//! scenarios.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::patterns::PatternEngine;
use crate::recorder_storage::RecorderOffset;
use crate::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
    RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
    RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
};
use crate::search::{ChunkInputEvent, ChunkPolicyConfig, build_semantic_chunks};

/// Stable schema identifier for generated manifests.
pub const SYNTHETIC_SWARM_FIXTURE_SCHEMA_VERSION: &str = "ft.synthetic.swarm.fixture.v1";

/// Default deterministic seed for generated scenarios.
pub const DEFAULT_SYNTHETIC_SWARM_SEED: u64 = 0xF716_5000_0003;

const BASE_TIME_MS: u64 = 1_766_000_000_000;
const EVENTS_PER_PANE: usize = 5;

/// Supported synthetic swarm scale profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyntheticSwarmScale {
    Fleet10,
    Fleet50,
    Fleet100,
    Fleet200,
}

impl SyntheticSwarmScale {
    /// Number of panes generated for this scale.
    #[must_use]
    pub const fn pane_count(self) -> usize {
        match self {
            Self::Fleet10 => 10,
            Self::Fleet50 => 50,
            Self::Fleet100 => 100,
            Self::Fleet200 => 200,
        }
    }

    /// Stable label used in manifest IDs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fleet10 => "fleet_10",
            Self::Fleet50 => "fleet_50",
            Self::Fleet100 => "fleet_100",
            Self::Fleet200 => "fleet_200",
        }
    }
}

/// All supported scale profiles in verifier order.
pub const SYNTHETIC_SWARM_SCALES: [SyntheticSwarmScale; 4] = [
    SyntheticSwarmScale::Fleet10,
    SyntheticSwarmScale::Fleet50,
    SyntheticSwarmScale::Fleet100,
    SyntheticSwarmScale::Fleet200,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyntheticAgentScript {
    CodexUsageReached,
    ClaudeApprovalNeeded,
    GeminiRateLimit,
    WeztermPaneExited,
    ClaudeOverloaded,
}

impl SyntheticAgentScript {
    const ALL: [Self; 5] = [
        Self::CodexUsageReached,
        Self::ClaudeApprovalNeeded,
        Self::GeminiRateLimit,
        Self::WeztermPaneExited,
        Self::ClaudeOverloaded,
    ];

    fn for_pane_index(index: usize) -> Self {
        Self::ALL[index % Self::ALL.len()]
    }

    fn agent_label(self) -> &'static str {
        match self {
            Self::CodexUsageReached => "codex",
            Self::ClaudeApprovalNeeded | Self::ClaudeOverloaded => "claude_code",
            Self::GeminiRateLimit => "gemini",
            Self::WeztermPaneExited => "wezterm",
        }
    }

    fn expected_rule_id(self) -> &'static str {
        match self {
            Self::CodexUsageReached => "codex.usage.reached",
            Self::ClaudeApprovalNeeded => "claude_code.approval_needed",
            Self::GeminiRateLimit => "gemini.rate_limit.detected",
            Self::WeztermPaneExited => "wezterm.pane.exited",
            Self::ClaudeOverloaded => "claude_code.error.overloaded",
        }
    }

    fn signal_text(self) -> &'static str {
        match self {
            Self::CodexUsageReached => {
                "You've hit your usage limit. Please try again at 2026-01-20 12:34 UTC."
            }
            Self::ClaudeApprovalNeeded => "Do you want to allow this operation? Approve?",
            Self::GeminiRateLimit => {
                "RESOURCE_EXHAUSTED: quota exceeded for gemini-1.5-pro. Please back off for 45 seconds."
            }
            Self::WeztermPaneExited => "pane exited with status 1",
            Self::ClaudeOverloaded => "Error: API overloaded, retry in 30 seconds",
        }
    }

    fn recovery_text(self) -> &'static str {
        match self {
            Self::CodexUsageReached => "codex pane parked until reset; recovery workflow queued",
            Self::ClaudeApprovalNeeded => {
                "approval checkpoint recorded; awaiting operator decision"
            }
            Self::GeminiRateLimit => "gemini pane throttled; retry budget preserved",
            Self::WeztermPaneExited => "mux pane exit captured; replacement pane requested",
            Self::ClaudeOverloaded => "claude overload backoff accepted; scheduler yielded",
        }
    }
}

/// Bounded generation and verifier budget recorded in every manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntheticSwarmBudgets {
    pub max_events_per_pane: usize,
    pub expected_event_count: usize,
    pub max_pattern_scans: usize,
    pub max_semantic_chunk_events: usize,
    pub max_text_bytes_per_pane: usize,
}

/// Expected lower-bound count for one pattern rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedPatternHit {
    pub rule_id: String,
    pub min_count: usize,
}

/// Deterministic scenario manifest suitable for golden tests and future e2e
/// scripts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntheticSwarmManifest {
    pub schema_version: String,
    pub scenario_id: String,
    pub scale: SyntheticSwarmScale,
    pub seed: u64,
    pub pane_count: usize,
    pub budgets: SyntheticSwarmBudgets,
    pub expected_pattern_hits: Vec<ExpectedPatternHit>,
    pub event_checksum_sha256: String,
    pub verifier_commands: Vec<String>,
    pub proof_surfaces: Vec<String>,
}

/// Per-pane script summary with the recorder event IDs it generated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntheticPaneScriptManifest {
    pub pane_id: u64,
    pub agent: String,
    pub script: SyntheticAgentScript,
    pub event_ids: Vec<String>,
}

/// Fully materialized synthetic scenario.
#[derive(Debug, Clone)]
pub struct SyntheticSwarmScenario {
    pub manifest: SyntheticSwarmManifest,
    pub pane_scripts: Vec<SyntheticPaneScriptManifest>,
    pub events: Vec<RecorderEvent>,
}

/// Verification report for a generated scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntheticSwarmVerificationReport {
    pub scenario_id: String,
    pub pane_count: usize,
    pub event_count: usize,
    pub pattern_scan_count: usize,
    pub semantic_chunk_count: usize,
    pub checksum_matches_manifest: bool,
    pub pattern_scan_count_within_budget: bool,
    pub semantic_chunk_count_within_budget: bool,
    pub actual_pattern_hits: BTreeMap<String, usize>,
    pub missing_pattern_hits: Vec<ExpectedPatternHit>,
    pub diagnostics: Vec<String>,
}

impl SyntheticSwarmVerificationReport {
    /// True when event count, checksum, expected pattern hits, and search chunks
    /// all satisfy the manifest contract.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checksum_matches_manifest
            && self.missing_pattern_hits.is_empty()
            && self.event_count == self.pane_count * EVENTS_PER_PANE
            && self.pattern_scan_count_within_budget
            && self.semantic_chunk_count_within_budget
            && self.semantic_chunk_count > 0
    }
}

/// Generate a deterministic scenario for the requested scale.
#[must_use]
pub fn synthetic_swarm_scenario(scale: SyntheticSwarmScale) -> SyntheticSwarmScenario {
    synthetic_swarm_scenario_with_seed(scale, DEFAULT_SYNTHETIC_SWARM_SEED)
}

/// Generate a deterministic scenario for the requested scale and seed.
#[must_use]
pub fn synthetic_swarm_scenario_with_seed(
    scale: SyntheticSwarmScale,
    seed: u64,
) -> SyntheticSwarmScenario {
    let pane_count = scale.pane_count();
    let scenario_id = format!("synthetic-swarm-{}-{seed:016x}", scale.as_str());
    let mut events = Vec::with_capacity(pane_count * EVENTS_PER_PANE);
    let mut pane_scripts = Vec::with_capacity(pane_count);
    let mut expected_counts: BTreeMap<String, usize> = BTreeMap::new();

    for pane_index in 0..pane_count {
        let script = SyntheticAgentScript::for_pane_index(pane_index);
        *expected_counts
            .entry(script.expected_rule_id().to_string())
            .or_default() += 1;

        let pane_id = 1_000 + pane_index as u64;
        let pane_events = pane_events(&scenario_id, pane_id, pane_index, script);
        let event_ids = pane_events
            .iter()
            .map(|event| event.event_id.clone())
            .collect();
        pane_scripts.push(SyntheticPaneScriptManifest {
            pane_id,
            agent: script.agent_label().to_string(),
            script,
            event_ids,
        });
        events.extend(pane_events);
    }

    let event_checksum_sha256 = checksum_events(&events);
    let expected_event_count = events.len();
    let manifest = SyntheticSwarmManifest {
        schema_version: SYNTHETIC_SWARM_FIXTURE_SCHEMA_VERSION.to_string(),
        scenario_id,
        scale,
        seed,
        pane_count,
        budgets: SyntheticSwarmBudgets {
            max_events_per_pane: EVENTS_PER_PANE,
            expected_event_count,
            max_pattern_scans: pane_count * 3,
            max_semantic_chunk_events: expected_event_count,
            max_text_bytes_per_pane: 512,
        },
        expected_pattern_hits: expected_counts
            .into_iter()
            .map(|(rule_id, min_count)| ExpectedPatternHit { rule_id, min_count })
            .collect(),
        event_checksum_sha256,
        verifier_commands: vec![
            "cargo test -p frankenterm-core --lib synthetic_swarm -- --nocapture".to_string(),
            "cargo test -p frankenterm-core --lib synthetic_swarm_all_scale_manifests_verify_through_real_pattern_and_chunking_paths".to_string(),
        ],
        proof_surfaces: vec![
            "recording::RecorderEvent schema".to_string(),
            "patterns::PatternEngine builtin packs".to_string(),
            "search::build_semantic_chunks".to_string(),
            "policy/workflow control markers".to_string(),
        ],
    };

    SyntheticSwarmScenario {
        manifest,
        pane_scripts,
        events,
    }
}

/// Verify a scenario against the real pattern engine and semantic chunking
/// policy.
#[must_use]
pub fn verify_synthetic_swarm_scenario(
    scenario: &SyntheticSwarmScenario,
    engine: &PatternEngine,
) -> SyntheticSwarmVerificationReport {
    let mut actual_pattern_hits: BTreeMap<String, usize> = BTreeMap::new();
    let mut pattern_scans = 0usize;

    for event in &scenario.events {
        let RecorderEventPayload::EgressOutput { text, .. } = &event.payload else {
            continue;
        };
        pattern_scans += 1;
        for detection in engine.detect(text) {
            *actual_pattern_hits.entry(detection.rule_id).or_default() += 1;
        }
    }

    let missing_pattern_hits = scenario
        .manifest
        .expected_pattern_hits
        .iter()
        .filter(|expected| {
            actual_pattern_hits
                .get(&expected.rule_id)
                .copied()
                .unwrap_or_default()
                < expected.min_count
        })
        .cloned()
        .collect::<Vec<_>>();

    let chunk_inputs = scenario
        .events
        .iter()
        .enumerate()
        .map(|(index, event)| ChunkInputEvent {
            event: event.clone(),
            offset: RecorderOffset {
                segment_id: 0,
                byte_offset: index as u64 * 256,
                ordinal: index as u64,
            },
        })
        .collect::<Vec<_>>();
    let chunks = build_semantic_chunks(&chunk_inputs, &ChunkPolicyConfig::default());
    let checksum = checksum_events(&scenario.events);
    let checksum_matches_manifest = checksum == scenario.manifest.event_checksum_sha256;
    let pattern_scan_count_within_budget =
        pattern_scans <= scenario.manifest.budgets.max_pattern_scans;
    let semantic_chunk_count_within_budget =
        chunks.len() <= scenario.manifest.budgets.max_semantic_chunk_events;
    let diagnostics = diagnostics_for(
        scenario,
        pattern_scans,
        chunks.len(),
        checksum_matches_manifest,
        &actual_pattern_hits,
    );

    SyntheticSwarmVerificationReport {
        scenario_id: scenario.manifest.scenario_id.clone(),
        pane_count: scenario.manifest.pane_count,
        event_count: scenario.events.len(),
        pattern_scan_count: pattern_scans,
        semantic_chunk_count: chunks.len(),
        checksum_matches_manifest,
        pattern_scan_count_within_budget,
        semantic_chunk_count_within_budget,
        actual_pattern_hits,
        missing_pattern_hits,
        diagnostics,
    }
}

fn pane_events(
    scenario_id: &str,
    pane_id: u64,
    pane_index: usize,
    script: SyntheticAgentScript,
) -> Vec<RecorderEvent> {
    let base_sequence = pane_index as u64 * EVENTS_PER_PANE as u64;
    let root_id = event_id(scenario_id, pane_id, base_sequence, "ingress");
    let heartbeat_id = event_id(scenario_id, pane_id, base_sequence + 1, "heartbeat");
    let signal_id = event_id(scenario_id, pane_id, base_sequence + 2, "signal");
    let decision_id = event_id(scenario_id, pane_id, base_sequence + 3, "policy");
    let recovery_id = event_id(scenario_id, pane_id, base_sequence + 4, "recovery");

    vec![
        recorder_event(
            &root_id,
            pane_id,
            base_sequence,
            RecorderEventSource::RobotMode,
            RecorderEventCausality::default(),
            RecorderEventPayload::IngressText {
                text: format!(
                    "ft synthetic scenario {scenario_id} start pane {pane_id} agent {}",
                    script.agent_label()
                ),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        ),
        recorder_event(
            &heartbeat_id,
            pane_id,
            base_sequence + 1,
            RecorderEventSource::WeztermMux,
            child_causality(&root_id, &root_id),
            RecorderEventPayload::EgressOutput {
                text: format!(
                    "{} heartbeat pane {pane_id}; synthetic workload active",
                    script.agent_label()
                ),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            },
        ),
        recorder_event(
            &signal_id,
            pane_id,
            base_sequence + 2,
            RecorderEventSource::WeztermMux,
            child_causality(&heartbeat_id, &root_id),
            RecorderEventPayload::EgressOutput {
                text: script.signal_text().to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            },
        ),
        recorder_event(
            &decision_id,
            pane_id,
            base_sequence + 3,
            RecorderEventSource::WorkflowEngine,
            RecorderEventCausality {
                parent_event_id: Some(signal_id.clone()),
                trigger_event_id: Some(signal_id.clone()),
                root_event_id: Some(root_id.clone()),
            },
            RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::PolicyDecision,
                details: serde_json::json!({
                    "scenario": scenario_id,
                    "script": script,
                    "decision": "observe_only",
                    "reason": "synthetic fixture proof path",
                }),
            },
        ),
        recorder_event(
            &recovery_id,
            pane_id,
            base_sequence + 4,
            RecorderEventSource::RecoveryFlow,
            child_causality(&decision_id, &root_id),
            RecorderEventPayload::EgressOutput {
                text: script.recovery_text().to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            },
        ),
    ]
}

fn recorder_event(
    event_id: &str,
    pane_id: u64,
    sequence: u64,
    source: RecorderEventSource,
    causality: RecorderEventCausality,
    payload: RecorderEventPayload,
) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some("synthetic-swarm-session".to_string()),
        workflow_id: Some("synthetic-swarm-fixture".to_string()),
        correlation_id: Some(format!("synthetic-swarm-pane-{pane_id}")),
        source,
        occurred_at_ms: BASE_TIME_MS + sequence * 100,
        recorded_at_ms: BASE_TIME_MS + sequence * 100 + 1,
        sequence,
        causality,
        payload,
    }
}

fn event_id(scenario_id: &str, pane_id: u64, sequence: u64, phase: &str) -> String {
    format!("{scenario_id}:pane-{pane_id}:seq-{sequence}:{phase}")
}

fn child_causality(parent_event_id: &str, root_event_id: &str) -> RecorderEventCausality {
    RecorderEventCausality {
        parent_event_id: Some(parent_event_id.to_string()),
        trigger_event_id: None,
        root_event_id: Some(root_event_id.to_string()),
    }
}

fn checksum_events(events: &[RecorderEvent]) -> String {
    let mut hasher = Sha256::new();
    for event in events {
        let encoded = serde_json::to_vec(event).expect("synthetic recorder event serializes");
        hasher.update(encoded);
        hasher.update(b"\n");
    }
    format!("sha256:{}", hex_digest(hasher.finalize()))
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

fn diagnostics_for(
    scenario: &SyntheticSwarmScenario,
    pattern_scans: usize,
    chunk_count: usize,
    checksum_matches_manifest: bool,
    actual_pattern_hits: &BTreeMap<String, usize>,
) -> Vec<String> {
    vec![
        format!("scenario_id={}", scenario.manifest.scenario_id),
        format!("seed={:016x}", scenario.manifest.seed),
        format!("pane_count={}", scenario.manifest.pane_count),
        format!(
            "budget=max_events_per_pane:{} expected_event_count:{} max_pattern_scans:{}",
            scenario.manifest.budgets.max_events_per_pane,
            scenario.manifest.budgets.expected_event_count,
            scenario.manifest.budgets.max_pattern_scans
        ),
        format!("actual_event_count={}", scenario.events.len()),
        format!("actual_pattern_scans={pattern_scans}"),
        format!("semantic_chunk_count={chunk_count}"),
        format!("checksum_matches_manifest={checksum_matches_manifest}"),
        format!(
            "actual_rule_hits={}",
            actual_pattern_hits
                .iter()
                .map(|(rule_id, count)| format!("{rule_id}={count}"))
                .collect::<Vec<_>>()
                .join(",")
        ),
        format!(
            "expected_rules={}",
            scenario
                .manifest
                .expected_pattern_hits
                .iter()
                .map(|hit| format!("{}>={}", hit.rule_id, hit.min_count))
                .collect::<Vec<_>>()
                .join(",")
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder_storage::{
        AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, DurabilityLevel,
        RecorderStorage,
    };
    use crate::runtime_async::CompatRuntime;
    use tempfile::tempdir;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build synthetic swarm test runtime");
        runtime.block_on(future);
    }

    fn append_log_config(dir: &std::path::Path) -> AppendLogStorageConfig {
        AppendLogStorageConfig {
            data_path: dir.join("events.log"),
            state_path: dir.join("state.json"),
            queue_capacity: 4,
            max_batch_events: 2_000,
            max_batch_bytes: 4 * 1024 * 1024,
            max_idempotency_entries: 16,
        }
    }

    #[test]
    fn synthetic_swarm_all_scale_manifests_verify_through_real_pattern_and_chunking_paths() {
        let engine = PatternEngine::new();

        for scale in SYNTHETIC_SWARM_SCALES {
            let scenario = synthetic_swarm_scenario(scale);
            let report = verify_synthetic_swarm_scenario(&scenario, &engine);

            assert!(
                report.passed(),
                "synthetic swarm report failed for {:?}: {report:#?}",
                scale
            );
            assert_eq!(scenario.manifest.pane_count, scale.pane_count());
            assert_eq!(
                scenario.manifest.budgets.expected_event_count,
                scale.pane_count() * EVENTS_PER_PANE
            );
            assert_eq!(scenario.pane_scripts.len(), scale.pane_count());
            assert_eq!(scenario.events.len(), scale.pane_count() * EVENTS_PER_PANE);
            assert_eq!(report.pattern_scan_count, scale.pane_count() * 3);
            assert!(report.pattern_scan_count_within_budget);
            assert!(report.semantic_chunk_count_within_budget);
            assert!(report.diagnostics.iter().any(|line| line.contains("seed=")));
            assert!(
                report
                    .diagnostics
                    .iter()
                    .any(|line| line.contains("pane_count="))
            );
            assert!(
                report
                    .diagnostics
                    .iter()
                    .any(|line| line.contains("expected_rules="))
            );
            assert!(
                report
                    .diagnostics
                    .iter()
                    .any(|line| line.contains("actual_rule_hits="))
            );
        }
    }

    #[test]
    fn synthetic_swarm_events_append_to_real_recorder_storage() {
        run_async_test(async {
            let scenario = synthetic_swarm_scenario(SyntheticSwarmScale::Fleet10);
            let dir = tempdir().expect("tempdir");
            let storage =
                AppendLogRecorderStorage::open(append_log_config(dir.path())).expect("open");

            let response = storage
                .append_batch(AppendRequest {
                    batch_id: "synthetic-swarm-fleet10".to_string(),
                    events: scenario.events.clone(),
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: BASE_TIME_MS,
                })
                .await
                .expect("append synthetic swarm events");

            assert_eq!(response.accepted_count, scenario.events.len());
            assert_eq!(response.first_offset.ordinal, 0);
            assert_eq!(
                response.last_offset.ordinal,
                scenario.events.len() as u64 - 1
            );

            let health = storage.health().await;
            assert!(
                !health.degraded,
                "synthetic append should leave storage healthy"
            );
            assert_eq!(
                health.latest_offset.as_ref().map(|offset| offset.ordinal),
                Some(scenario.events.len() as u64 - 1)
            );
        });
    }

    #[test]
    fn synthetic_swarm_manifest_generation_is_deterministic() {
        let first = synthetic_swarm_scenario(SyntheticSwarmScale::Fleet50);
        let second = synthetic_swarm_scenario(SyntheticSwarmScale::Fleet50);

        assert_eq!(first.manifest, second.manifest);
        assert_eq!(first.pane_scripts, second.pane_scripts);
        assert_eq!(first.events, second.events);
        assert_eq!(
            first.manifest.event_checksum_sha256,
            checksum_events(&first.events)
        );
    }

    #[test]
    fn synthetic_swarm_manifest_covers_all_expected_rule_families() {
        let scenario = synthetic_swarm_scenario(SyntheticSwarmScale::Fleet10);
        let expected = scenario
            .manifest
            .expected_pattern_hits
            .iter()
            .map(|hit| (hit.rule_id.as_str(), hit.min_count))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(expected.get("codex.usage.reached"), Some(&2));
        assert_eq!(expected.get("claude_code.approval_needed"), Some(&2));
        assert_eq!(expected.get("gemini.rate_limit.detected"), Some(&2));
        assert_eq!(expected.get("wezterm.pane.exited"), Some(&2));
        assert_eq!(expected.get("claude_code.error.overloaded"), Some(&2));
    }

    #[test]
    fn synthetic_swarm_negative_tamper_fails_loudly() {
        let engine = PatternEngine::new();
        let mut scenario = synthetic_swarm_scenario(SyntheticSwarmScale::Fleet10);

        for event in &mut scenario.events {
            if matches!(
                &event.payload,
                RecorderEventPayload::EgressOutput { text, .. }
                    if text.contains("RESOURCE_EXHAUSTED")
            ) {
                event.payload = RecorderEventPayload::EgressOutput {
                    text: "gemini pane emitted ordinary progress output".to_string(),
                    encoding: RecorderTextEncoding::Utf8,
                    redaction: RecorderRedactionLevel::None,
                    segment_kind: RecorderSegmentKind::Delta,
                    is_gap: false,
                };
                break;
            }
        }

        let report = verify_synthetic_swarm_scenario(&scenario, &engine);
        assert!(
            !report.passed(),
            "tampered synthetic scenario must fail verification"
        );
        assert!(
            !report.checksum_matches_manifest,
            "tampering must break the manifest checksum"
        );
        assert!(
            report
                .missing_pattern_hits
                .iter()
                .any(|hit| hit.rule_id == "gemini.rate_limit.detected"),
            "tampering must report the missing expected rule hit: {report:#?}"
        );
    }

    #[test]
    fn synthetic_swarm_pane_event_chains_are_causally_ordered() {
        let scenario = synthetic_swarm_scenario(SyntheticSwarmScale::Fleet10);

        for pane in &scenario.pane_scripts {
            assert_eq!(pane.event_ids.len(), EVENTS_PER_PANE);
            let root = &pane.event_ids[0];
            for event_id in &pane.event_ids[1..] {
                let event = scenario
                    .events
                    .iter()
                    .find(|event| &event.event_id == event_id)
                    .expect("pane event id exists");
                assert_eq!(
                    event.causality.root_event_id.as_deref(),
                    Some(root.as_str())
                );
            }
        }
    }
}
