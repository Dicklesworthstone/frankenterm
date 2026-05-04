//! Deterministic large-swarm replay corpus generation and summaries.
//!
//! This module is intentionally in-memory and schema-first: it builds
//! `RecorderEvent` traces for the required large-swarm scale points, converts
//! them through the existing replay query shape, and summarizes the replay via
//! `ReplaySession`. The generated corpus is compact, versioned, and stable so
//! it can be used as a regression anchor without depending on live panes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::memory_budget::{MemoryBudgetConfig, MemoryBudgetManager};
use crate::policy::ActorKind;
use crate::recorder_audit::{AccessTier, ActorIdentity};
use crate::recorder_query::{QueryEventKind, QueryResultEvent};
use crate::recorder_replay::{ReplayConfig, ReplayError, ReplaySession};
use crate::recorder_retention::SensitivityTier;
use crate::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
    RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
    RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
};
use crate::runtime_telemetry::{
    SwarmCapacityStage, SwarmCapacityTelemetry, SwarmCapacityTelemetrySnapshot,
};
use crate::storage_telemetry::StorageTelemetry;

/// Version string for large-swarm replay corpus artifacts.
pub const LARGE_SWARM_REPLAY_CORPUS_VERSION: &str = "ft.large_swarm_replay.v1";

/// Scenario parameters for deterministic large-swarm trace generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeSwarmScenario {
    /// Stable scenario identifier.
    pub scenario_id: String,
    /// Number of panes represented by this scenario.
    pub pane_count: u64,
    /// Output bursts emitted per pane.
    pub output_bursts_per_pane: u64,
    /// Compaction waves emitted across every pane.
    pub compaction_waves: u64,
    /// Robot-mode search actions distributed across panes.
    pub search_queries: u64,
    /// Workflow mission-loop actions distributed across panes.
    pub mission_actions: u64,
    /// Fixed base timestamp used for deterministic event timestamps.
    pub base_timestamp_ms: u64,
    /// Stable session identifier embedded in recorder events.
    pub session_id: String,
}

impl LargeSwarmScenario {
    /// Build one of the required scale-point scenarios.
    #[must_use]
    pub fn scale_point(pane_count: u64) -> Option<Self> {
        let (output_bursts_per_pane, compaction_waves, search_queries, mission_actions) =
            match pane_count {
                10 => (3, 2, 3, 2),
                50 => (4, 2, 8, 5),
                200 => (4, 3, 20, 12),
                1_000 => (4, 3, 50, 30),
                _ => return None,
            };

        Some(Self {
            scenario_id: format!("large-swarm-{pane_count}p"),
            pane_count,
            output_bursts_per_pane,
            compaction_waves,
            search_queries,
            mission_actions,
            base_timestamp_ms: 1_800_000_000_000,
            session_id: format!("large-swarm-session-{pane_count}p"),
        })
    }

    /// Return all required large-swarm scale-point scenarios.
    #[must_use]
    pub fn required_scale_points() -> Vec<Self> {
        [10, 50, 200, 1_000]
            .into_iter()
            .filter_map(Self::scale_point)
            .collect()
    }

    fn validate(&self) -> Result<(), LargeSwarmReplayError> {
        if self.pane_count == 0 {
            return Err(LargeSwarmReplayError::InvalidScenario(
                "pane_count must be greater than zero".into(),
            ));
        }
        if self.output_bursts_per_pane == 0 {
            return Err(LargeSwarmReplayError::InvalidScenario(
                "output_bursts_per_pane must be greater than zero".into(),
            ));
        }
        if self.scenario_id.is_empty() {
            return Err(LargeSwarmReplayError::InvalidScenario(
                "scenario_id must not be empty".into(),
            ));
        }
        if self.session_id.is_empty() {
            return Err(LargeSwarmReplayError::InvalidScenario(
                "session_id must not be empty".into(),
            ));
        }
        Ok(())
    }
}

/// A compact, versioned large-swarm replay artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeSwarmReplayCorpus {
    /// Artifact schema version.
    pub version: String,
    /// Scenario used to generate the trace.
    pub scenario: LargeSwarmScenario,
    /// Deterministic recorder events in canonical replay order.
    pub events: Vec<RecorderEvent>,
}

impl LargeSwarmReplayCorpus {
    /// Sort events into canonical replay order.
    pub fn canonicalize(&mut self) {
        self.events.sort_by(|left, right| {
            (
                left.occurred_at_ms,
                left.pane_id,
                left.sequence,
                left.event_id.as_str(),
            )
                .cmp(&(
                    right.occurred_at_ms,
                    right.pane_id,
                    right.sequence,
                    right.event_id.as_str(),
                ))
        });
    }

    /// Convert recorder-schema events into replay query events.
    #[must_use]
    pub fn to_query_events(&self) -> Vec<QueryResultEvent> {
        self.events.iter().map(query_event_from_recorder).collect()
    }
}

/// Deterministic replay summary for a large-swarm corpus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeSwarmReplaySummary {
    /// Artifact schema version used by the source corpus.
    pub version: String,
    /// Scenario identifier.
    pub scenario_id: String,
    /// Number of panes represented by the replay.
    pub pane_count: u64,
    /// Total recorder events replayed.
    pub event_count: u64,
    /// Original event span in milliseconds.
    pub duration_ms: u64,
    /// Number of replay frames emitted by `ReplaySession`.
    pub replay_frames: u64,
    /// Replay duration after instant replay timing is applied.
    pub replay_duration_ms: u64,
    /// Maximum number of events assigned to any pane.
    pub max_events_per_pane: u64,
    /// Total bytes of terminal egress output in the trace.
    pub output_bytes: u64,
    /// Compaction-wave marker count.
    pub compaction_waves: u64,
    /// Robot-mode search action count.
    pub search_queries: u64,
    /// Workflow mission-loop action count.
    pub mission_actions: u64,
    /// Event counts by replay event kind.
    pub by_kind: BTreeMap<String, u64>,
    /// Distilled replay output from the live telemetry collectors.
    pub collectors: LargeSwarmTelemetryCollectorSummary,
    /// Stable digest over the canonical summary fields.
    pub summary_digest: String,
}

impl LargeSwarmReplaySummary {
    fn digest_input(&self) -> String {
        let mut input = format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            self.version,
            self.scenario_id,
            self.pane_count,
            self.event_count,
            self.duration_ms,
            self.replay_frames,
            self.replay_duration_ms,
            self.max_events_per_pane,
            self.output_bytes,
            self.compaction_waves,
            self.search_queries,
            self.mission_actions,
            self.collectors.latency_arrivals,
            self.collectors.latency_completions,
            self.collectors.admission_arrivals,
            self.collectors.admission_completions,
            self.collectors.memory_panes_registered,
            self.collectors.memory_samples,
            self.collectors.storage_events_appended,
            self.collectors.storage_batches,
            self.collectors.storage_flushes
        );
        for (kind, count) in &self.by_kind {
            input.push('|');
            input.push_str(kind);
            input.push('=');
            input.push_str(&count.to_string());
        }
        input
    }
}

/// Compact deterministic proof that the corpus replayed through live collectors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeSwarmTelemetryCollectorSummary {
    /// Arrival observations recorded in the capacity/latency collector.
    pub latency_arrivals: u64,
    /// Completion observations recorded in the capacity/latency collector.
    pub latency_completions: u64,
    /// Admission observations recorded through the Robot/MCP capacity stage.
    pub admission_arrivals: u64,
    /// Admission completions recorded through the Robot/MCP capacity stage.
    pub admission_completions: u64,
    /// Panes registered in the memory budget telemetry manager.
    pub memory_panes_registered: u64,
    /// Memory sample passes recorded by the memory budget manager.
    pub memory_samples: u64,
    /// Events appended into the storage telemetry collector.
    pub storage_events_appended: u64,
    /// Append batches recorded by the storage telemetry collector.
    pub storage_batches: u64,
    /// Flush operations recorded by the storage telemetry collector.
    pub storage_flushes: u64,
}

/// Regression thresholds for a large-swarm replay summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeSwarmRegressionThresholds {
    /// Maximum acceptable total event count.
    pub max_event_count: u64,
    /// Maximum acceptable source replay duration in milliseconds.
    pub max_duration_ms: u64,
    /// Maximum acceptable egress output bytes.
    pub max_output_bytes: u64,
    /// Maximum acceptable events assigned to any one pane.
    pub max_events_per_pane: u64,
}

impl LargeSwarmRegressionThresholds {
    /// Build conservative thresholds for the deterministic built-in scenarios.
    #[must_use]
    pub fn for_scenario(scenario: &LargeSwarmScenario) -> Self {
        let expected_event_count = scenario
            .pane_count
            .saturating_mul(scenario.output_bursts_per_pane + scenario.compaction_waves)
            .saturating_add(scenario.search_queries)
            .saturating_add(scenario.mission_actions);
        let expected_events_per_pane = scenario
            .output_bursts_per_pane
            .saturating_add(scenario.compaction_waves)
            .saturating_add(2);

        Self {
            max_event_count: expected_event_count,
            max_duration_ms: expected_event_count.saturating_mul(10),
            max_output_bytes: scenario
                .pane_count
                .saturating_mul(scenario.output_bursts_per_pane)
                .saturating_mul(96),
            max_events_per_pane: expected_events_per_pane,
        }
    }
}

/// One explicit threshold violation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeSwarmRegressionDiff {
    /// Summary field that violated the threshold.
    pub field: String,
    /// Actual observed value.
    pub actual: u64,
    /// Configured threshold value.
    pub threshold: u64,
    /// Human-readable failure message.
    pub message: String,
}

/// Threshold verdict for a deterministic large-swarm replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeSwarmRegressionVerdict {
    /// Whether all thresholds passed.
    pub passed: bool,
    /// Detailed threshold failures.
    pub diffs: Vec<LargeSwarmRegressionDiff>,
}

/// Errors emitted by large-swarm corpus generation and replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LargeSwarmReplayError {
    /// Scenario parameters are invalid.
    InvalidScenario(String),
    /// Existing replay engine rejected the generated trace.
    Replay(ReplayError),
}

impl std::fmt::Display for LargeSwarmReplayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidScenario(message) => {
                write!(formatter, "invalid large-swarm scenario: {message}")
            }
            Self::Replay(error) => write!(formatter, "large-swarm replay failed: {error}"),
        }
    }
}

impl std::error::Error for LargeSwarmReplayError {}

impl From<ReplayError> for LargeSwarmReplayError {
    fn from(value: ReplayError) -> Self {
        Self::Replay(value)
    }
}

/// Generate a deterministic large-swarm replay corpus.
///
/// Events cover pane output bursts, compaction waves, search activity, and
/// mission-loop actions. They are always returned in canonical replay order.
pub fn generate_large_swarm_corpus(
    scenario: &LargeSwarmScenario,
) -> Result<LargeSwarmReplayCorpus, LargeSwarmReplayError> {
    scenario.validate()?;

    let mut builder = CorpusBuilder::new(scenario.clone());
    builder.push_output_bursts();
    builder.push_compaction_waves();
    builder.push_search_queries();
    builder.push_mission_actions();

    let mut corpus = LargeSwarmReplayCorpus {
        version: LARGE_SWARM_REPLAY_CORPUS_VERSION.into(),
        scenario: scenario.clone(),
        events: builder.events,
    };
    corpus.canonicalize();
    Ok(corpus)
}

/// Generate and summarize all required large-swarm scale points.
pub fn summarize_required_scale_points()
-> Result<Vec<LargeSwarmReplaySummary>, LargeSwarmReplayError> {
    LargeSwarmScenario::required_scale_points()
        .iter()
        .map(|scenario| {
            let corpus = generate_large_swarm_corpus(scenario)?;
            summarize_large_swarm_replay(&corpus)
        })
        .collect()
}

/// Replay a corpus through `ReplaySession` and return a deterministic summary.
pub fn summarize_large_swarm_replay(
    corpus: &LargeSwarmReplayCorpus,
) -> Result<LargeSwarmReplaySummary, LargeSwarmReplayError> {
    let query_events = corpus.to_query_events();
    let mut replay = ReplaySession::new(
        query_events,
        ReplayConfig::instant(),
        ActorIdentity::new(ActorKind::Robot, "large-swarm-replay-corpus"),
        AccessTier::A2FullQuery,
        format!("{}-summary", corpus.scenario.scenario_id),
    )?;
    let frames = replay.collect_remaining();
    let stats = replay.stats().clone();

    let mut events_per_pane = BTreeMap::<u64, u64>::new();
    let mut by_kind = BTreeMap::<String, u64>::new();
    let mut output_bytes = 0_u64;
    let mut compaction_waves = 0_u64;
    let mut search_queries = 0_u64;
    let mut mission_actions = 0_u64;

    for event in &corpus.events {
        *events_per_pane.entry(event.pane_id).or_insert(0) += 1;
        *by_kind.entry(kind_name(event).into()).or_insert(0) += 1;

        match &event.payload {
            RecorderEventPayload::EgressOutput { text, .. } => {
                output_bytes = output_bytes.saturating_add(usize_to_u64(text.len()));
            }
            RecorderEventPayload::IngressText { text, .. } => {
                if text.starts_with("ft robot search ") {
                    search_queries = search_queries.saturating_add(1);
                }
            }
            RecorderEventPayload::ControlMarker { details, .. } => {
                if details.get("activity").and_then(serde_json::Value::as_str)
                    == Some("compaction_wave")
                {
                    compaction_waves = compaction_waves.saturating_add(1);
                }
                if details.get("activity").and_then(serde_json::Value::as_str)
                    == Some("mission_loop_action")
                {
                    mission_actions = mission_actions.saturating_add(1);
                }
            }
            RecorderEventPayload::LifecycleMarker { .. } => {}
        }
    }

    let max_events_per_pane = events_per_pane.values().copied().max().unwrap_or(0);
    let collectors = replay_against_telemetry_collectors(corpus, output_bytes);
    let mut summary = LargeSwarmReplaySummary {
        version: corpus.version.clone(),
        scenario_id: corpus.scenario.scenario_id.clone(),
        pane_count: corpus.scenario.pane_count,
        event_count: usize_to_u64(corpus.events.len()),
        duration_ms: stats.original_duration_ms,
        replay_frames: usize_to_u64(frames.len()),
        replay_duration_ms: stats.replay_duration_ms,
        max_events_per_pane,
        output_bytes,
        compaction_waves,
        search_queries,
        mission_actions,
        by_kind,
        collectors,
        summary_digest: String::new(),
    };
    summary.summary_digest = stable_digest(&summary.digest_input());
    Ok(summary)
}

/// Compare a summary against explicit regression thresholds.
#[must_use]
pub fn evaluate_large_swarm_thresholds(
    summary: &LargeSwarmReplaySummary,
    thresholds: &LargeSwarmRegressionThresholds,
) -> LargeSwarmRegressionVerdict {
    let mut diffs = Vec::new();
    push_threshold_diff(
        &mut diffs,
        "event_count",
        summary.event_count,
        thresholds.max_event_count,
    );
    push_threshold_diff(
        &mut diffs,
        "duration_ms",
        summary.duration_ms,
        thresholds.max_duration_ms,
    );
    push_threshold_diff(
        &mut diffs,
        "output_bytes",
        summary.output_bytes,
        thresholds.max_output_bytes,
    );
    push_threshold_diff(
        &mut diffs,
        "max_events_per_pane",
        summary.max_events_per_pane,
        thresholds.max_events_per_pane,
    );

    LargeSwarmRegressionVerdict {
        passed: diffs.is_empty(),
        diffs,
    }
}

fn replay_against_telemetry_collectors(
    corpus: &LargeSwarmReplayCorpus,
    output_bytes: u64,
) -> LargeSwarmTelemetryCollectorSummary {
    let mut latency = SwarmCapacityTelemetry::with_defaults();
    let mut admission = SwarmCapacityTelemetry::with_defaults();
    let memory = MemoryBudgetManager::new(MemoryBudgetConfig {
        use_cgroups: false,
        ..MemoryBudgetConfig::default()
    });
    let storage = StorageTelemetry::with_defaults();

    for pane_id in 1..=corpus.scenario.pane_count {
        memory.register_pane(pane_id, None);
    }
    let _memory_budget_summary = memory.sample_all();

    for event in &corpus.events {
        let queue_depth = event.pane_id % 64;
        let stage = capacity_stage_for_event(event);
        latency.record_arrival(stage, queue_depth);
        latency.record_wait_time_ms(stage, 1.0);
        latency.record_completion(stage, 2.0, queue_depth);

        if is_admission_event(event) {
            admission.record_arrival(SwarmCapacityStage::RobotMcp, queue_depth);
            admission.record_wait_time_ms(SwarmCapacityStage::RobotMcp, 1.0);
            admission.record_completion(SwarmCapacityStage::RobotMcp, 2.0, queue_depth);
        }
    }

    storage.record_append(100.0, corpus.events.len(), output_bytes, false);
    storage.record_flush(200.0);

    let latency_snapshot = latency.snapshot();
    let admission_snapshot = admission.snapshot();
    let memory_snapshot = memory.telemetry().snapshot();
    let storage_snapshot = storage.snapshot();

    let (latency_arrivals, latency_completions) = capacity_snapshot_totals(&latency_snapshot);
    let (admission_arrivals, admission_completions) = capacity_snapshot_totals(&admission_snapshot);

    LargeSwarmTelemetryCollectorSummary {
        latency_arrivals,
        latency_completions,
        admission_arrivals,
        admission_completions,
        memory_panes_registered: memory_snapshot.panes_registered,
        memory_samples: memory_snapshot.samples,
        storage_events_appended: storage_snapshot.total_events_appended,
        storage_batches: storage_snapshot.total_batches,
        storage_flushes: storage_snapshot.total_flushes,
    }
}

fn capacity_snapshot_totals(snapshot: &SwarmCapacityTelemetrySnapshot) -> (u64, u64) {
    snapshot.stages.iter().fold((0, 0), |acc, stage| {
        (
            acc.0.saturating_add(stage.arrivals),
            acc.1.saturating_add(stage.completions),
        )
    })
}

fn capacity_stage_for_event(event: &RecorderEvent) -> SwarmCapacityStage {
    match &event.payload {
        RecorderEventPayload::IngressText { .. } => SwarmCapacityStage::RobotMcp,
        RecorderEventPayload::EgressOutput { .. } => SwarmCapacityStage::IngestCapture,
        RecorderEventPayload::ControlMarker { .. } => SwarmCapacityStage::WorkflowRunner,
        RecorderEventPayload::LifecycleMarker { .. } => SwarmCapacityStage::EventBusFanout,
    }
}

fn is_admission_event(event: &RecorderEvent) -> bool {
    match &event.payload {
        RecorderEventPayload::IngressText { text, .. } => text.starts_with("ft robot search "),
        RecorderEventPayload::ControlMarker { details, .. } => {
            details.get("activity").and_then(serde_json::Value::as_str)
                == Some("mission_loop_action")
        }
        RecorderEventPayload::EgressOutput { .. }
        | RecorderEventPayload::LifecycleMarker { .. } => false,
    }
}

struct CorpusBuilder {
    scenario: LargeSwarmScenario,
    events: Vec<RecorderEvent>,
    sequence: u64,
}

impl CorpusBuilder {
    fn new(scenario: LargeSwarmScenario) -> Self {
        Self {
            scenario,
            events: Vec::new(),
            sequence: 0,
        }
    }

    fn push_output_bursts(&mut self) {
        for pane_index in 0..self.scenario.pane_count {
            let pane_id = pane_index + 1;
            for burst_index in 0..self.scenario.output_bursts_per_pane {
                let text = format!(
                    "pane={pane_id} burst={burst_index} large-swarm-output token-window stable-line\n"
                );
                self.push_event(
                    pane_id,
                    RecorderEventSource::WeztermMux,
                    RecorderEventPayload::EgressOutput {
                        text,
                        encoding: RecorderTextEncoding::Utf8,
                        redaction: RecorderRedactionLevel::None,
                        segment_kind: RecorderSegmentKind::Delta,
                        is_gap: false,
                    },
                );
            }
        }
    }

    fn push_compaction_waves(&mut self) {
        for wave_index in 0..self.scenario.compaction_waves {
            for pane_index in 0..self.scenario.pane_count {
                let pane_id = pane_index + 1;
                self.push_event(
                    pane_id,
                    RecorderEventSource::WorkflowEngine,
                    RecorderEventPayload::ControlMarker {
                        control_marker_type: RecorderControlMarkerType::PromptBoundary,
                        details: serde_json::json!({
                            "activity": "compaction_wave",
                            "wave": wave_index,
                            "pane_id": pane_id,
                        }),
                    },
                );
            }
        }
    }

    fn push_search_queries(&mut self) {
        for query_index in 0..self.scenario.search_queries {
            let pane_id = distributed_pane_id(query_index, self.scenario.pane_count);
            self.push_event(
                pane_id,
                RecorderEventSource::RobotMode,
                RecorderEventPayload::IngressText {
                    text: format!("ft robot search large-swarm query={query_index} pane={pane_id}"),
                    encoding: RecorderTextEncoding::Utf8,
                    redaction: RecorderRedactionLevel::None,
                    ingress_kind: RecorderIngressKind::SendText,
                },
            );
        }
    }

    fn push_mission_actions(&mut self) {
        for action_index in 0..self.scenario.mission_actions {
            let pane_id =
                distributed_pane_id(action_index.saturating_mul(7), self.scenario.pane_count);
            self.push_event(
                pane_id,
                RecorderEventSource::WorkflowEngine,
                RecorderEventPayload::ControlMarker {
                    control_marker_type: RecorderControlMarkerType::PolicyDecision,
                    details: serde_json::json!({
                        "activity": "mission_loop_action",
                        "action_index": action_index,
                        "pane_id": pane_id,
                        "decision": "allow",
                    }),
                },
            );
        }
    }

    fn push_event(
        &mut self,
        pane_id: u64,
        source: RecorderEventSource,
        payload: RecorderEventPayload,
    ) {
        let sequence = self.sequence;
        let occurred_at_ms = self
            .scenario
            .base_timestamp_ms
            .saturating_add(sequence / 3 * 10);
        let kind = event_payload_name(&payload);
        self.events.push(RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.into(),
            event_id: format!("{}-{kind}-{sequence:08}", self.scenario.scenario_id),
            pane_id,
            session_id: Some(self.scenario.session_id.clone()),
            workflow_id: None,
            correlation_id: Some(format!("{}-corr", self.scenario.scenario_id)),
            source,
            occurred_at_ms,
            recorded_at_ms: occurred_at_ms.saturating_add(1),
            sequence,
            causality: RecorderEventCausality::default(),
            payload,
        });
        self.sequence = self.sequence.saturating_add(1);
    }
}

fn distributed_pane_id(index: u64, pane_count: u64) -> u64 {
    index % pane_count + 1
}

fn query_event_from_recorder(event: &RecorderEvent) -> QueryResultEvent {
    let (event_kind, text) = match &event.payload {
        RecorderEventPayload::IngressText { text, .. } => {
            (QueryEventKind::IngressText, Some(text.clone()))
        }
        RecorderEventPayload::EgressOutput { text, .. } => {
            (QueryEventKind::EgressOutput, Some(text.clone()))
        }
        RecorderEventPayload::ControlMarker { .. } => (QueryEventKind::ControlMarker, None),
        RecorderEventPayload::LifecycleMarker { .. } => (QueryEventKind::LifecycleMarker, None),
    };

    QueryResultEvent {
        event_id: event.event_id.clone(),
        pane_id: event.pane_id,
        source: event.source,
        occurred_at_ms: event.occurred_at_ms,
        sequence: event.sequence,
        session_id: event.session_id.clone(),
        text,
        redacted: false,
        sensitivity: SensitivityTier::T1Standard,
        event_kind,
    }
}

fn kind_name(event: &RecorderEvent) -> &'static str {
    event_payload_name(&event.payload)
}

fn event_payload_name(payload: &RecorderEventPayload) -> &'static str {
    match payload {
        RecorderEventPayload::IngressText { .. } => "ingress_text",
        RecorderEventPayload::EgressOutput { .. } => "egress_output",
        RecorderEventPayload::ControlMarker { .. } => "control_marker",
        RecorderEventPayload::LifecycleMarker { .. } => "lifecycle_marker",
    }
}

fn push_threshold_diff(
    diffs: &mut Vec<LargeSwarmRegressionDiff>,
    field: &str,
    actual: u64,
    threshold: u64,
) {
    if actual <= threshold {
        return;
    }
    diffs.push(LargeSwarmRegressionDiff {
        field: field.into(),
        actual,
        threshold,
        message: format!("{field} {actual} exceeded threshold {threshold}"),
    });
}

fn stable_digest(input: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
