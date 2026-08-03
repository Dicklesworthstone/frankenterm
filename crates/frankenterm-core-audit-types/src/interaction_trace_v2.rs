//! Portable, content-free interaction trace v2 contract.
//!
//! This module freezes identity, clock, causality, privacy, and metric
//! semantics for production keypress and resize/zoom evidence.  It is a DTO
//! and validation layer only: it does not claim that any producer is wired,
//! that a display callback measures photons, or that clocks on different
//! hosts can be subtracted.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::renderer_scenario_catalog::{RendererKeypressTraceStage, RendererResizeTraceStage};

/// Exact wire schema accepted by this implementation.
pub const INTERACTION_TRACE_V2_SCHEMA_VERSION: &str = "ft.interaction-trace.v2";
/// A trace is deliberately small enough for bounded validation and replay.
pub const MAX_INTERACTION_TRACE_EVENTS: usize = 256;
/// One retained run may contain at most this many independently identified traces.
pub const MAX_INTERACTION_TRACES_PER_RUN: usize = 65_536;

/// Process-epoch identity.  The pair is an opaque 128-bit nonce, never a host
/// name, command, pane title, or input-content hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTraceRunId {
    pub epoch_nonce_hi: u64,
    pub epoch_nonce_lo: u64,
}

impl InteractionTraceRunId {
    #[must_use]
    pub const fn new(epoch_nonce_hi: u64, epoch_nonce_lo: u64) -> Option<Self> {
        if epoch_nonce_hi == 0 && epoch_nonce_lo == 0 {
            None
        } else {
            Some(Self {
                epoch_nonce_hi,
                epoch_nonce_lo,
            })
        }
    }

    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.epoch_nonce_hi != 0 || self.epoch_nonce_lo != 0
    }
}

/// One operator action identity: process epoch plus a strictly increasing
/// sequence.  Zero and `u64::MAX` are reserved so exhaustion is explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTraceId {
    pub run_id: InteractionTraceRunId,
    pub sequence: u64,
}

impl InteractionTraceId {
    #[must_use]
    pub const fn new(run_id: InteractionTraceRunId, sequence: u64) -> Option<Self> {
        if !run_id.is_valid() || sequence == 0 || sequence == u64::MAX {
            None
        } else {
            Some(Self { run_id, sequence })
        }
    }

    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.run_id.is_valid() && self.sequence != 0 && self.sequence != u64::MAX
    }
}

/// Fail-stop allocator for per-run trace IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionTraceIdAllocator {
    run_id: InteractionTraceRunId,
    next_sequence: u64,
    exhausted: bool,
}

impl InteractionTraceIdAllocator {
    /// Start a fresh process epoch at sequence one.
    pub fn new(run_id: InteractionTraceRunId) -> Result<Self, TraceContractError> {
        Self::resume(run_id, 1)
    }

    /// Resume from an externally retained next sequence.  This is intended for
    /// crash-safe producer state, not for recycling a prior ID.
    pub fn resume(
        run_id: InteractionTraceRunId,
        next_sequence: u64,
    ) -> Result<Self, TraceContractError> {
        if !run_id.is_valid() {
            return Err(TraceContractError::InvalidRunId);
        }
        if next_sequence == 0 || next_sequence == u64::MAX {
            return Err(TraceContractError::ReservedTraceSequence {
                sequence: next_sequence,
            });
        }
        Ok(Self {
            run_id,
            next_sequence,
            exhausted: false,
        })
    }

    /// Allocate exactly once.  After the last usable ID (`u64::MAX - 1`), the
    /// allocator remains exhausted and cannot wrap or silently recycle IDs.
    pub fn allocate(&mut self) -> Result<InteractionTraceId, TraceContractError> {
        if self.exhausted || self.next_sequence == 0 || self.next_sequence == u64::MAX {
            self.exhausted = true;
            return Err(TraceContractError::TraceSequenceExhausted);
        }

        let id = InteractionTraceId {
            run_id: self.run_id,
            sequence: self.next_sequence,
        };
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(TraceContractError::TraceSequenceExhausted)?;
        Ok(id)
    }

    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        self.exhausted || self.next_sequence == 0 || self.next_sequence == u64::MAX
    }
}

/// Which closed stage inventory a trace follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionTracePath {
    Keypress,
    ResizeZoom,
}

/// One stage from the already-frozen K0-K13 or R0-R25 inventories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(
    tag = "path",
    content = "stage",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum InteractionTraceStage {
    Keypress(RendererKeypressTraceStage),
    ResizeZoom(RendererResizeTraceStage),
}

impl InteractionTraceStage {
    #[must_use]
    pub const fn path(self) -> InteractionTracePath {
        match self {
            Self::Keypress(_) => InteractionTracePath::Keypress,
            Self::ResizeZoom(_) => InteractionTracePath::ResizeZoom,
        }
    }

    #[must_use]
    pub fn expected(path: InteractionTracePath) -> Vec<Self> {
        match path {
            InteractionTracePath::Keypress => RendererKeypressTraceStage::ALL
                .into_iter()
                .map(Self::Keypress)
                .collect(),
            InteractionTracePath::ResizeZoom => RendererResizeTraceStage::ALL
                .into_iter()
                .map(Self::ResizeZoom)
                .collect(),
        }
    }

    #[must_use]
    pub const fn requires_connection_generation(self) -> bool {
        matches!(
            self,
            Self::Keypress(
                RendererKeypressTraceStage::ClientRpcEnqueue
                    | RendererKeypressTraceStage::ClientEncodeSocketFlush
                    | RendererKeypressTraceStage::ServerReadableDecode
                    | RendererKeypressTraceStage::ServerDispatchMuxWait
                    | RendererKeypressTraceStage::TerminalLockPtyWriteFlush
                    | RendererKeypressTraceStage::PtyEchoParserApply
                    | RendererKeypressTraceStage::ServerDeltaCompute
                    | RendererKeypressTraceStage::ClientReceiveDecodeApply
                    | RendererKeypressTraceStage::LocalMuxGuiInvalidation
            ) | Self::ResizeZoom(RendererResizeTraceStage::MuxResizeDispatch)
        )
    }

    #[must_use]
    pub const fn is_display_completion(self) -> bool {
        matches!(
            self,
            Self::Keypress(RendererKeypressTraceStage::DisplayCompletion)
                | Self::ResizeZoom(RendererResizeTraceStage::DisplayCompletion)
        )
    }
}

/// Content-free producer identity.  `host_id` and generations are opaque
/// registry IDs; they are not wall-clock timestamps and are not subtraction
/// authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTraceProducer {
    pub host_id: u64,
    pub process_id: u32,
    pub process_generation: u64,
    pub thread_id: u64,
    pub connection_generation: Option<u64>,
}

/// Stable mux topology association.  These are numeric IDs only; pane titles,
/// current directories, input text, and pane output do not belong in a trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTraceTopology {
    pub window_id: u64,
    pub tab_id: u64,
    pub pane_id: u64,
}

/// A process-local monotonic clock domain.  Equal values assert one common
/// epoch/rate.  Wall time is retained separately and is never duration input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTraceClockDomain {
    pub host_id: u64,
    pub process_generation: u64,
    pub clock_id: u64,
}

/// One monotonic timestamp plus optional wall-time metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTraceTimestamp {
    pub clock_domain: InteractionTraceClockDomain,
    pub monotonic_ns: u64,
    pub wall_time_unix_ns: Option<u64>,
}

impl InteractionTraceTimestamp {
    /// Compute a duration only when both endpoints assert the exact same clock
    /// domain.  Cross-host and merely-similar clock labels fail closed.
    pub fn duration_until(self, later: Self) -> Result<u64, TraceContractError> {
        if self.clock_domain != later.clock_domain {
            return Err(TraceContractError::CrossClockArithmetic {
                from: self.clock_domain,
                to: later.clock_domain,
            });
        }
        later.monotonic_ns.checked_sub(self.monotonic_ns).ok_or(
            TraceContractError::ClockRegression {
                start_ns: self.monotonic_ns,
                end_ns: later.monotonic_ns,
            },
        )
    }
}

/// Causal authority for one stage receipt.  All tokens are opaque numeric
/// identities; they may not encode key bytes or pane contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "quality", rename_all = "snake_case", deny_unknown_fields)]
pub enum InteractionTraceCorrelation {
    ExactProtocol {
        protocol_token: u64,
        protocol_generation: u64,
    },
    ExactEchoFixture {
        fixture_token: u64,
        expected_terminal_generation: u64,
    },
    CausalCandidate {
        candidate_window_ns: u64,
    },
    Uncorrelated,
}

/// Closed quality labels for dashboards and evidence summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionTraceCorrelationQuality {
    ExactProtocol,
    ExactEchoFixture,
    CausalCandidate,
    Uncorrelated,
}

impl InteractionTraceCorrelation {
    #[must_use]
    pub const fn quality(self) -> InteractionTraceCorrelationQuality {
        match self {
            Self::ExactProtocol { .. } => InteractionTraceCorrelationQuality::ExactProtocol,
            Self::ExactEchoFixture { .. } => InteractionTraceCorrelationQuality::ExactEchoFixture,
            Self::CausalCandidate { .. } => InteractionTraceCorrelationQuality::CausalCandidate,
            Self::Uncorrelated => InteractionTraceCorrelationQuality::Uncorrelated,
        }
    }
}

/// Strongest admissible claim boundary.  Ordering is intentionally strongest
/// to weakest so a trace can conservatively take the maximum value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionTraceClaimBoundary {
    CausalSoftwarePath,
    ExactFixturePath,
    DiagnosticCandidate,
    AggregateOnly,
}

impl From<InteractionTraceCorrelationQuality> for InteractionTraceClaimBoundary {
    fn from(value: InteractionTraceCorrelationQuality) -> Self {
        match value {
            InteractionTraceCorrelationQuality::ExactProtocol => Self::CausalSoftwarePath,
            InteractionTraceCorrelationQuality::ExactEchoFixture => Self::ExactFixturePath,
            InteractionTraceCorrelationQuality::CausalCandidate => Self::DiagnosticCandidate,
            InteractionTraceCorrelationQuality::Uncorrelated => Self::AggregateOnly,
        }
    }
}

/// Observation boundary for a receipt.  `photon` requires the separate
/// detector authority below; GPU submit and drawable request never imply it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionTraceObservationBoundary {
    InternalState,
    SoftwarePresent,
    MetalDrawable,
    DisplayPresented,
    Photon,
}

/// Physical detector provenance.  IDs must resolve through a retained run
/// manifest; no free-form operator or input text is accepted here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTracePhysicalDetector {
    pub detector_id: u64,
    pub calibration_id: u64,
}

/// Queue/work/allocation counters carried by every structured event.  A zero
/// means observed zero, not omitted; producers unable to observe a metric must
/// classify the event as degraded in the downstream recorder rather than
/// inventing a value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTraceCounters {
    pub queue_depth: u64,
    pub oldest_queue_age_ns: u64,
    pub work_units: u64,
    pub bytes: u64,
    pub rows: u64,
    pub allocation_count: u64,
    pub allocated_bytes: u64,
    pub copy_count: u64,
    pub copied_bytes: u64,
    pub rpc_count: u64,
    pub delta_count: u64,
    pub dirty_rows: u64,
    pub full_viewport_clones: u64,
    pub cursor_row_duplicates: u64,
    pub paint_count: u64,
    pub frame_count: u64,
}

/// Causal state generations.  `None` is explicit unavailable data; later
/// stage validators require the generations that should exist by then.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTraceGenerations {
    pub terminal_generation: Option<u64>,
    pub snapshot_generation: Option<u64>,
    pub frame_generation: Option<u64>,
}

/// Recorder loss observed before this receipt.  Exact traces require both
/// counters to stay zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTraceSamplingLoss {
    pub dropped_events: u64,
    pub overwritten_events: u64,
}

impl InteractionTraceSamplingLoss {
    #[must_use]
    pub const fn is_lossless(self) -> bool {
        self.dropped_events == 0 && self.overwritten_events == 0
    }
}

/// One canonical JSONL-ready structured trace receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTraceEventV2 {
    pub schema_version: String,
    pub trace_id: InteractionTraceId,
    pub event_ordinal: u64,
    pub span_id: u64,
    pub parent_span_id: Option<u64>,
    pub stage: InteractionTraceStage,
    pub producer: InteractionTraceProducer,
    pub topology: InteractionTraceTopology,
    pub started_at: InteractionTraceTimestamp,
    pub completed_at: InteractionTraceTimestamp,
    pub correlation: InteractionTraceCorrelation,
    pub counters: InteractionTraceCounters,
    pub generations: InteractionTraceGenerations,
    pub sampling_loss: InteractionTraceSamplingLoss,
    pub observation_boundary: InteractionTraceObservationBoundary,
    pub physical_detector: Option<InteractionTracePhysicalDetector>,
}

impl InteractionTraceEventV2 {
    pub fn duration_ns(&self) -> Result<u64, TraceContractError> {
        self.started_at.duration_until(self.completed_at)
    }
}

/// One complete or diagnostic interaction trace.  Structural validation allows
/// a prefix so an interrupted run remains diagnosable; qualification requires
/// the entire closed stage inventory and zero sampling loss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTraceV2 {
    pub schema_version: String,
    pub trace_id: InteractionTraceId,
    pub path: InteractionTracePath,
    pub events: Vec<InteractionTraceEventV2>,
}

impl InteractionTraceV2 {
    pub fn validate_structure(&self) -> Result<(), TraceContractError> {
        validate_schema(&self.schema_version)?;
        validate_trace_id(self.trace_id)?;
        if self.events.is_empty() {
            return Err(TraceContractError::EmptyTrace);
        }
        if self.events.len() > MAX_INTERACTION_TRACE_EVENTS {
            return Err(TraceContractError::TooManyEvents {
                actual: self.events.len(),
                maximum: MAX_INTERACTION_TRACE_EVENTS,
            });
        }

        let expected = InteractionTraceStage::expected(self.path);
        let mut spans = BTreeSet::new();
        let mut seen_stages = BTreeSet::new();
        let mut last_start_by_clock = BTreeMap::new();

        for (index, event) in self.events.iter().enumerate() {
            validate_schema(&event.schema_version)?;
            if event.trace_id != self.trace_id {
                return Err(TraceContractError::EventTraceIdMismatch {
                    expected: self.trace_id,
                    actual: event.trace_id,
                });
            }
            let expected_ordinal = index as u64;
            if event.event_ordinal != expected_ordinal {
                return Err(TraceContractError::EventOrdinalNotContiguous {
                    expected: expected_ordinal,
                    actual: event.event_ordinal,
                });
            }
            if event.stage.path() != self.path {
                return Err(TraceContractError::TracePathMismatch {
                    expected: self.path,
                    actual: event.stage.path(),
                });
            }
            let Some(expected_stage) = expected.get(index).copied() else {
                return Err(TraceContractError::UnexpectedStage { stage: event.stage });
            };
            if event.stage != expected_stage {
                if seen_stages.contains(&event.stage) {
                    return Err(TraceContractError::DuplicateStage { stage: event.stage });
                }
                return Err(TraceContractError::StageOutOfOrder {
                    expected: expected_stage,
                    actual: event.stage,
                });
            }
            if !seen_stages.insert(event.stage) {
                return Err(TraceContractError::DuplicateStage { stage: event.stage });
            }
            validate_event(event, &spans)?;
            if let Some(previous_start_ns) = last_start_by_clock
                .insert(event.started_at.clock_domain, event.started_at.monotonic_ns)
                && event.started_at.monotonic_ns < previous_start_ns
            {
                return Err(TraceContractError::CrossEventClockRegression {
                    clock_domain: event.started_at.clock_domain,
                    previous_start_ns,
                    actual_start_ns: event.started_at.monotonic_ns,
                    event_ordinal: event.event_ordinal,
                });
            }
            spans.insert(event.span_id);
        }
        Ok(())
    }

    /// Qualify a trace for its declared (still bounded) claim class.
    pub fn validate_qualifying(&self) -> Result<InteractionTraceClaimBoundary, TraceContractError> {
        self.validate_structure()?;
        let expected = InteractionTraceStage::expected(self.path);
        if self.events.len() != expected.len() {
            return Err(TraceContractError::MissingStage {
                stage: expected[self.events.len()],
            });
        }
        for event in &self.events {
            if !event.sampling_loss.is_lossless() {
                return Err(TraceContractError::SamplingLoss {
                    event_ordinal: event.event_ordinal,
                    dropped_events: event.sampling_loss.dropped_events,
                    overwritten_events: event.sampling_loss.overwritten_events,
                });
            }
        }

        Ok(self
            .events
            .iter()
            .map(|event| InteractionTraceClaimBoundary::from(event.correlation.quality()))
            .max()
            .unwrap_or(InteractionTraceClaimBoundary::AggregateOnly))
    }

    /// Duration between two stage markers, admitted only when their start
    /// timestamps share one exact monotonic clock domain.
    pub fn duration_between_starts_ns(
        &self,
        from: InteractionTraceStage,
        to: InteractionTraceStage,
    ) -> Result<u64, TraceContractError> {
        let from_timestamp = self
            .events
            .iter()
            .find(|event| event.stage == from)
            .ok_or(TraceContractError::MissingStage { stage: from })?
            .started_at;
        let to_timestamp = self
            .events
            .iter()
            .find(|event| event.stage == to)
            .ok_or(TraceContractError::MissingStage { stage: to })?
            .started_at;
        from_timestamp.duration_until(to_timestamp)
    }
}

/// Run envelope used to reject duplicate or regressing trace IDs and process
/// epoch mixing before an evidence bundle is retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTraceRunV2 {
    pub schema_version: String,
    pub run_id: InteractionTraceRunId,
    pub traces: Vec<InteractionTraceV2>,
}

impl InteractionTraceRunV2 {
    pub fn validate_structure(&self) -> Result<(), TraceContractError> {
        validate_schema(&self.schema_version)?;
        if !self.run_id.is_valid() {
            return Err(TraceContractError::InvalidRunId);
        }
        if self.traces.is_empty() {
            return Err(TraceContractError::EmptyRun);
        }
        if self.traces.len() > MAX_INTERACTION_TRACES_PER_RUN {
            return Err(TraceContractError::TooManyTraces {
                actual: self.traces.len(),
                maximum: MAX_INTERACTION_TRACES_PER_RUN,
            });
        }

        let mut previous = None;
        for trace in &self.traces {
            if trace.trace_id.run_id != self.run_id {
                return Err(TraceContractError::TraceRunMismatch {
                    expected: self.run_id,
                    actual: trace.trace_id.run_id,
                });
            }
            if let Some(previous_sequence) = previous
                && trace.trace_id.sequence <= previous_sequence
            {
                return Err(TraceContractError::TraceSequenceNotIncreasing {
                    previous: previous_sequence,
                    actual: trace.trace_id.sequence,
                });
            }
            trace.validate_structure()?;
            previous = Some(trace.trace_id.sequence);
        }
        Ok(())
    }
}

fn validate_schema(schema_version: &str) -> Result<(), TraceContractError> {
    if schema_version == INTERACTION_TRACE_V2_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(TraceContractError::UnsupportedSchemaVersion)
    }
}

fn validate_trace_id(trace_id: InteractionTraceId) -> Result<(), TraceContractError> {
    if !trace_id.run_id.is_valid() {
        return Err(TraceContractError::InvalidRunId);
    }
    if trace_id.sequence == 0 || trace_id.sequence == u64::MAX {
        return Err(TraceContractError::ReservedTraceSequence {
            sequence: trace_id.sequence,
        });
    }
    Ok(())
}

fn validate_event(
    event: &InteractionTraceEventV2,
    prior_spans: &BTreeSet<u64>,
) -> Result<(), TraceContractError> {
    if event.span_id == 0 {
        return Err(TraceContractError::ReservedSpanId);
    }
    if prior_spans.contains(&event.span_id) {
        return Err(TraceContractError::DuplicateSpanId {
            span_id: event.span_id,
        });
    }
    if event.parent_span_id == Some(event.span_id) {
        return Err(TraceContractError::SelfParentSpan {
            span_id: event.span_id,
        });
    }
    if let Some(parent_span_id) = event.parent_span_id
        && !prior_spans.contains(&parent_span_id)
    {
        return Err(TraceContractError::UnknownParentSpan { parent_span_id });
    }

    validate_producer(event.producer, event.stage)?;
    validate_topology(event.topology)?;
    validate_clock(event.started_at.clock_domain, event.producer)?;
    validate_clock(event.completed_at.clock_domain, event.producer)?;
    event.duration_ns()?;
    validate_correlation(event.correlation)?;
    validate_generations(event)?;
    validate_observation_boundary(event)?;
    Ok(())
}

fn validate_producer(
    producer: InteractionTraceProducer,
    stage: InteractionTraceStage,
) -> Result<(), TraceContractError> {
    if producer.host_id == 0 {
        return Err(TraceContractError::InvalidProducerIdentity { field: "host_id" });
    }
    if producer.process_id == 0 {
        return Err(TraceContractError::InvalidProducerIdentity {
            field: "process_id",
        });
    }
    if producer.process_generation == 0 {
        return Err(TraceContractError::InvalidProducerIdentity {
            field: "process_generation",
        });
    }
    if producer.thread_id == 0 {
        return Err(TraceContractError::InvalidProducerIdentity { field: "thread_id" });
    }
    if producer.connection_generation == Some(0) {
        return Err(TraceContractError::InvalidProducerIdentity {
            field: "connection_generation",
        });
    }
    if stage.requires_connection_generation() && producer.connection_generation.is_none() {
        return Err(TraceContractError::ConnectionGenerationMissing { stage });
    }
    Ok(())
}

fn validate_topology(topology: InteractionTraceTopology) -> Result<(), TraceContractError> {
    if topology.window_id == 0 {
        return Err(TraceContractError::InvalidTopologyIdentity { field: "window_id" });
    }
    if topology.tab_id == 0 {
        return Err(TraceContractError::InvalidTopologyIdentity { field: "tab_id" });
    }
    if topology.pane_id == 0 {
        return Err(TraceContractError::InvalidTopologyIdentity { field: "pane_id" });
    }
    Ok(())
}

fn validate_clock(
    clock: InteractionTraceClockDomain,
    producer: InteractionTraceProducer,
) -> Result<(), TraceContractError> {
    if clock.clock_id == 0 {
        return Err(TraceContractError::InvalidClockDomain { field: "clock_id" });
    }
    if clock.host_id != producer.host_id {
        return Err(TraceContractError::ClockProducerMismatch { field: "host_id" });
    }
    if clock.process_generation != producer.process_generation {
        return Err(TraceContractError::ClockProducerMismatch {
            field: "process_generation",
        });
    }
    Ok(())
}

fn validate_correlation(
    correlation: InteractionTraceCorrelation,
) -> Result<(), TraceContractError> {
    match correlation {
        InteractionTraceCorrelation::ExactProtocol {
            protocol_token,
            protocol_generation,
        } if protocol_token == 0 || protocol_generation == 0 => {
            Err(TraceContractError::InvalidCorrelationAuthority)
        }
        InteractionTraceCorrelation::ExactEchoFixture {
            fixture_token,
            expected_terminal_generation,
        } if fixture_token == 0 || expected_terminal_generation == 0 => {
            Err(TraceContractError::InvalidCorrelationAuthority)
        }
        InteractionTraceCorrelation::CausalCandidate {
            candidate_window_ns: 0,
        } => Err(TraceContractError::InvalidCorrelationAuthority),
        _ => Ok(()),
    }
}

fn validate_generations(event: &InteractionTraceEventV2) -> Result<(), TraceContractError> {
    for (field, value) in [
        ("terminal_generation", event.generations.terminal_generation),
        ("snapshot_generation", event.generations.snapshot_generation),
        ("frame_generation", event.generations.frame_generation),
    ] {
        if value == Some(0) {
            return Err(TraceContractError::InvalidGeneration { field });
        }
    }

    let required = match event.stage {
        InteractionTraceStage::Keypress(RendererKeypressTraceStage::PtyEchoParserApply) => {
            Some(("terminal_generation", event.generations.terminal_generation))
        }
        InteractionTraceStage::Keypress(RendererKeypressTraceStage::ServerDeltaCompute) => {
            Some(("snapshot_generation", event.generations.snapshot_generation))
        }
        InteractionTraceStage::Keypress(RendererKeypressTraceStage::DisplayCompletion)
        | InteractionTraceStage::ResizeZoom(RendererResizeTraceStage::DisplayCompletion) => {
            Some(("frame_generation", event.generations.frame_generation))
        }
        InteractionTraceStage::ResizeZoom(RendererResizeTraceStage::FirstCoherentViewport) => {
            Some(("snapshot_generation", event.generations.snapshot_generation))
        }
        _ => None,
    };
    if let Some((field, None)) = required {
        return Err(TraceContractError::GenerationMissing {
            field,
            stage: event.stage,
        });
    }
    Ok(())
}

fn validate_observation_boundary(
    event: &InteractionTraceEventV2,
) -> Result<(), TraceContractError> {
    match (event.observation_boundary, event.physical_detector) {
        (InteractionTraceObservationBoundary::Photon, None) => {
            return Err(TraceContractError::PhysicalDetectorMissing);
        }
        (InteractionTraceObservationBoundary::Photon, Some(detector))
            if detector.detector_id == 0 || detector.calibration_id == 0 =>
        {
            return Err(TraceContractError::InvalidPhysicalDetector);
        }
        (InteractionTraceObservationBoundary::Photon, Some(_)) => {}
        (_, Some(_)) => return Err(TraceContractError::PhysicalDetectorUnexpected),
        (_, None) => {}
    }

    if matches!(
        event.observation_boundary,
        InteractionTraceObservationBoundary::DisplayPresented
            | InteractionTraceObservationBoundary::Photon
    ) && !event.stage.is_display_completion()
    {
        return Err(TraceContractError::ObservationBoundaryTooStrong {
            stage: event.stage,
            boundary: event.observation_boundary,
        });
    }
    if event.stage.is_display_completion()
        && !matches!(
            event.observation_boundary,
            InteractionTraceObservationBoundary::DisplayPresented
                | InteractionTraceObservationBoundary::Photon
        )
    {
        return Err(TraceContractError::DisplayCompletionBoundaryMissing { stage: event.stage });
    }
    Ok(())
}

/// Closed metric vocabulary for the producer/claim-boundary schema lint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InteractionTraceMetric {
    MonotonicTimestamp,
    QueueDepth,
    OldestQueueAge,
    WorkUnits,
    Bytes,
    Rows,
    AllocationCount,
    AllocatedBytes,
    CopyCount,
    CopiedBytes,
    RpcCount,
    DeltaCount,
    DirtyRows,
    FullViewportClones,
    CursorRowDuplicates,
    PaintCount,
    FrameCount,
    TerminalGeneration,
    SnapshotGeneration,
    FrameGeneration,
    SamplingLoss,
    DisplayCompletion,
    PhotonDetection,
}

impl InteractionTraceMetric {
    pub const ALL: [Self; 23] = [
        Self::MonotonicTimestamp,
        Self::QueueDepth,
        Self::OldestQueueAge,
        Self::WorkUnits,
        Self::Bytes,
        Self::Rows,
        Self::AllocationCount,
        Self::AllocatedBytes,
        Self::CopyCount,
        Self::CopiedBytes,
        Self::RpcCount,
        Self::DeltaCount,
        Self::DirtyRows,
        Self::FullViewportClones,
        Self::CursorRowDuplicates,
        Self::PaintCount,
        Self::FrameCount,
        Self::TerminalGeneration,
        Self::SnapshotGeneration,
        Self::FrameGeneration,
        Self::SamplingLoss,
        Self::DisplayCompletion,
        Self::PhotonDetection,
    ];
}

/// Producer class named by the schema lint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionTraceMetricProducer {
    DeclaredStageProducer,
    ClientTransport,
    ServerTransport,
    TerminalOrPty,
    DeltaEngine,
    Renderer,
    DisplayCallback,
    PhysicalDetector,
}

/// Maximum conclusion one metric can support by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionTraceMetricClaim {
    HostLocalOnly,
    SingleClockRoundTripOnly,
    CausalSoftwareOnly,
    DisplayPresentedOnly,
    PhysicalPhotonOnly,
    DiagnosticOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractionTraceMetricBinding {
    pub metric: InteractionTraceMetric,
    pub producer: InteractionTraceMetricProducer,
    pub claim_boundary: InteractionTraceMetricClaim,
}

/// Exhaustive metric-to-producer and claim-boundary map.  The linter below
/// rejects missing/duplicate rows and any physical-photon authority assigned
/// to a non-detector producer.
pub const INTERACTION_TRACE_V2_METRIC_MAP: &[InteractionTraceMetricBinding] = &[
    metric_binding(
        InteractionTraceMetric::MonotonicTimestamp,
        InteractionTraceMetricProducer::DeclaredStageProducer,
        InteractionTraceMetricClaim::HostLocalOnly,
    ),
    metric_binding(
        InteractionTraceMetric::QueueDepth,
        InteractionTraceMetricProducer::DeclaredStageProducer,
        InteractionTraceMetricClaim::DiagnosticOnly,
    ),
    metric_binding(
        InteractionTraceMetric::OldestQueueAge,
        InteractionTraceMetricProducer::DeclaredStageProducer,
        InteractionTraceMetricClaim::HostLocalOnly,
    ),
    metric_binding(
        InteractionTraceMetric::WorkUnits,
        InteractionTraceMetricProducer::DeclaredStageProducer,
        InteractionTraceMetricClaim::DiagnosticOnly,
    ),
    metric_binding(
        InteractionTraceMetric::Bytes,
        InteractionTraceMetricProducer::DeclaredStageProducer,
        InteractionTraceMetricClaim::DiagnosticOnly,
    ),
    metric_binding(
        InteractionTraceMetric::Rows,
        InteractionTraceMetricProducer::DeltaEngine,
        InteractionTraceMetricClaim::DiagnosticOnly,
    ),
    metric_binding(
        InteractionTraceMetric::AllocationCount,
        InteractionTraceMetricProducer::DeclaredStageProducer,
        InteractionTraceMetricClaim::DiagnosticOnly,
    ),
    metric_binding(
        InteractionTraceMetric::AllocatedBytes,
        InteractionTraceMetricProducer::DeclaredStageProducer,
        InteractionTraceMetricClaim::DiagnosticOnly,
    ),
    metric_binding(
        InteractionTraceMetric::CopyCount,
        InteractionTraceMetricProducer::DeclaredStageProducer,
        InteractionTraceMetricClaim::DiagnosticOnly,
    ),
    metric_binding(
        InteractionTraceMetric::CopiedBytes,
        InteractionTraceMetricProducer::DeclaredStageProducer,
        InteractionTraceMetricClaim::DiagnosticOnly,
    ),
    metric_binding(
        InteractionTraceMetric::RpcCount,
        InteractionTraceMetricProducer::ClientTransport,
        InteractionTraceMetricClaim::SingleClockRoundTripOnly,
    ),
    metric_binding(
        InteractionTraceMetric::DeltaCount,
        InteractionTraceMetricProducer::ServerTransport,
        InteractionTraceMetricClaim::CausalSoftwareOnly,
    ),
    metric_binding(
        InteractionTraceMetric::DirtyRows,
        InteractionTraceMetricProducer::DeltaEngine,
        InteractionTraceMetricClaim::DiagnosticOnly,
    ),
    metric_binding(
        InteractionTraceMetric::FullViewportClones,
        InteractionTraceMetricProducer::DeltaEngine,
        InteractionTraceMetricClaim::DiagnosticOnly,
    ),
    metric_binding(
        InteractionTraceMetric::CursorRowDuplicates,
        InteractionTraceMetricProducer::DeltaEngine,
        InteractionTraceMetricClaim::DiagnosticOnly,
    ),
    metric_binding(
        InteractionTraceMetric::PaintCount,
        InteractionTraceMetricProducer::Renderer,
        InteractionTraceMetricClaim::CausalSoftwareOnly,
    ),
    metric_binding(
        InteractionTraceMetric::FrameCount,
        InteractionTraceMetricProducer::Renderer,
        InteractionTraceMetricClaim::CausalSoftwareOnly,
    ),
    metric_binding(
        InteractionTraceMetric::TerminalGeneration,
        InteractionTraceMetricProducer::TerminalOrPty,
        InteractionTraceMetricClaim::CausalSoftwareOnly,
    ),
    metric_binding(
        InteractionTraceMetric::SnapshotGeneration,
        InteractionTraceMetricProducer::DeltaEngine,
        InteractionTraceMetricClaim::CausalSoftwareOnly,
    ),
    metric_binding(
        InteractionTraceMetric::FrameGeneration,
        InteractionTraceMetricProducer::Renderer,
        InteractionTraceMetricClaim::CausalSoftwareOnly,
    ),
    metric_binding(
        InteractionTraceMetric::SamplingLoss,
        InteractionTraceMetricProducer::DeclaredStageProducer,
        InteractionTraceMetricClaim::DiagnosticOnly,
    ),
    metric_binding(
        InteractionTraceMetric::DisplayCompletion,
        InteractionTraceMetricProducer::DisplayCallback,
        InteractionTraceMetricClaim::DisplayPresentedOnly,
    ),
    metric_binding(
        InteractionTraceMetric::PhotonDetection,
        InteractionTraceMetricProducer::PhysicalDetector,
        InteractionTraceMetricClaim::PhysicalPhotonOnly,
    ),
];

const fn metric_binding(
    metric: InteractionTraceMetric,
    producer: InteractionTraceMetricProducer,
    claim_boundary: InteractionTraceMetricClaim,
) -> InteractionTraceMetricBinding {
    InteractionTraceMetricBinding {
        metric,
        producer,
        claim_boundary,
    }
}

pub fn lint_interaction_trace_v2_metric_map() -> Result<(), TraceContractError> {
    let mut seen = BTreeSet::new();
    for binding in INTERACTION_TRACE_V2_METRIC_MAP {
        if !seen.insert(binding.metric) {
            return Err(TraceContractError::DuplicateMetricBinding {
                metric: binding.metric,
            });
        }
        if binding.claim_boundary == InteractionTraceMetricClaim::PhysicalPhotonOnly
            && (binding.metric != InteractionTraceMetric::PhotonDetection
                || binding.producer != InteractionTraceMetricProducer::PhysicalDetector)
        {
            return Err(TraceContractError::InvalidPhysicalMetricAuthority {
                metric: binding.metric,
            });
        }
    }
    for metric in InteractionTraceMetric::ALL {
        if !seen.contains(&metric) {
            return Err(TraceContractError::MissingMetricBinding { metric });
        }
    }
    Ok(())
}

/// Fail-closed contract validation errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceContractError {
    UnsupportedSchemaVersion,
    InvalidRunId,
    ReservedTraceSequence {
        sequence: u64,
    },
    TraceSequenceExhausted,
    EmptyTrace,
    EmptyRun,
    TooManyEvents {
        actual: usize,
        maximum: usize,
    },
    TooManyTraces {
        actual: usize,
        maximum: usize,
    },
    EventTraceIdMismatch {
        expected: InteractionTraceId,
        actual: InteractionTraceId,
    },
    TraceRunMismatch {
        expected: InteractionTraceRunId,
        actual: InteractionTraceRunId,
    },
    TraceSequenceNotIncreasing {
        previous: u64,
        actual: u64,
    },
    EventOrdinalNotContiguous {
        expected: u64,
        actual: u64,
    },
    TracePathMismatch {
        expected: InteractionTracePath,
        actual: InteractionTracePath,
    },
    UnexpectedStage {
        stage: InteractionTraceStage,
    },
    DuplicateStage {
        stage: InteractionTraceStage,
    },
    StageOutOfOrder {
        expected: InteractionTraceStage,
        actual: InteractionTraceStage,
    },
    MissingStage {
        stage: InteractionTraceStage,
    },
    ReservedSpanId,
    DuplicateSpanId {
        span_id: u64,
    },
    SelfParentSpan {
        span_id: u64,
    },
    UnknownParentSpan {
        parent_span_id: u64,
    },
    InvalidProducerIdentity {
        field: &'static str,
    },
    ConnectionGenerationMissing {
        stage: InteractionTraceStage,
    },
    InvalidTopologyIdentity {
        field: &'static str,
    },
    InvalidClockDomain {
        field: &'static str,
    },
    ClockProducerMismatch {
        field: &'static str,
    },
    CrossClockArithmetic {
        from: InteractionTraceClockDomain,
        to: InteractionTraceClockDomain,
    },
    ClockRegression {
        start_ns: u64,
        end_ns: u64,
    },
    CrossEventClockRegression {
        clock_domain: InteractionTraceClockDomain,
        previous_start_ns: u64,
        actual_start_ns: u64,
        event_ordinal: u64,
    },
    InvalidCorrelationAuthority,
    InvalidGeneration {
        field: &'static str,
    },
    GenerationMissing {
        field: &'static str,
        stage: InteractionTraceStage,
    },
    SamplingLoss {
        event_ordinal: u64,
        dropped_events: u64,
        overwritten_events: u64,
    },
    PhysicalDetectorMissing,
    PhysicalDetectorUnexpected,
    InvalidPhysicalDetector,
    ObservationBoundaryTooStrong {
        stage: InteractionTraceStage,
        boundary: InteractionTraceObservationBoundary,
    },
    DisplayCompletionBoundaryMissing {
        stage: InteractionTraceStage,
    },
    DuplicateMetricBinding {
        metric: InteractionTraceMetric,
    },
    MissingMetricBinding {
        metric: InteractionTraceMetric,
    },
    InvalidPhysicalMetricAuthority {
        metric: InteractionTraceMetric,
    },
}

impl fmt::Display for TraceContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "interaction trace v2 contract violation: {self:?}"
        )
    }
}

impl std::error::Error for TraceContractError {}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonschema::{Draft, Validator};

    const GOOD_FIXTURE: &str =
        include_str!("../../../fixtures/perf/interaction-trace-v2/good-keypress-v2.json");
    const OLD_FIXTURE: &str =
        include_str!("../../../fixtures/perf/interaction-trace-v2/old-keypress-v1.json");
    const PRIVACY_BAD_FIXTURE: &str =
        include_str!("../../../fixtures/perf/interaction-trace-v2/bad-raw-content-v2.json");
    const JSON_SCHEMA: &str = include_str!("../../../docs/perf/interaction-trace-v2.schema.json");

    fn run_id() -> InteractionTraceRunId {
        InteractionTraceRunId::new(0xfeed, 0xbeef).expect("test run ID is non-zero")
    }

    fn trace_id(sequence: u64) -> InteractionTraceId {
        InteractionTraceId::new(run_id(), sequence).expect("test trace ID is admissible")
    }

    fn keypress_event(
        sequence: u64,
        ordinal: usize,
        stage: RendererKeypressTraceStage,
    ) -> InteractionTraceEventV2 {
        let host_id = if matches!(
            stage,
            RendererKeypressTraceStage::ServerReadableDecode
                | RendererKeypressTraceStage::ServerDispatchMuxWait
                | RendererKeypressTraceStage::TerminalLockPtyWriteFlush
                | RendererKeypressTraceStage::PtyEchoParserApply
                | RendererKeypressTraceStage::ServerDeltaCompute
        ) {
            2
        } else {
            1
        };
        let process_generation = host_id * 10;
        let base = ordinal as u64 * 100;
        let parent_span_id = (ordinal > 0).then_some(ordinal as u64);
        let stage = InteractionTraceStage::Keypress(stage);
        InteractionTraceEventV2 {
            schema_version: INTERACTION_TRACE_V2_SCHEMA_VERSION.to_owned(),
            trace_id: trace_id(sequence),
            event_ordinal: ordinal as u64,
            span_id: ordinal as u64 + 1,
            parent_span_id,
            stage,
            producer: InteractionTraceProducer {
                host_id,
                process_id: host_id as u32,
                process_generation,
                thread_id: ordinal as u64 + 1,
                connection_generation: stage.requires_connection_generation().then_some(1),
            },
            topology: InteractionTraceTopology {
                window_id: 1,
                tab_id: 2,
                pane_id: 3,
            },
            started_at: InteractionTraceTimestamp {
                clock_domain: InteractionTraceClockDomain {
                    host_id,
                    process_generation,
                    clock_id: 1,
                },
                monotonic_ns: base,
                wall_time_unix_ns: Some(1_000_000 + base),
            },
            completed_at: InteractionTraceTimestamp {
                clock_domain: InteractionTraceClockDomain {
                    host_id,
                    process_generation,
                    clock_id: 1,
                },
                monotonic_ns: base + 50,
                wall_time_unix_ns: Some(1_000_050 + base),
            },
            correlation: InteractionTraceCorrelation::ExactProtocol {
                protocol_token: sequence,
                protocol_generation: 1,
            },
            counters: InteractionTraceCounters::default(),
            generations: InteractionTraceGenerations {
                terminal_generation: Some(1),
                snapshot_generation: Some(1),
                frame_generation: Some(1),
            },
            sampling_loss: InteractionTraceSamplingLoss::default(),
            observation_boundary: if stage.is_display_completion() {
                InteractionTraceObservationBoundary::DisplayPresented
            } else {
                InteractionTraceObservationBoundary::InternalState
            },
            physical_detector: None,
        }
    }

    fn keypress_trace(sequence: u64) -> InteractionTraceV2 {
        InteractionTraceV2 {
            schema_version: INTERACTION_TRACE_V2_SCHEMA_VERSION.to_owned(),
            trace_id: trace_id(sequence),
            path: InteractionTracePath::Keypress,
            events: RendererKeypressTraceStage::ALL
                .into_iter()
                .enumerate()
                .map(|(ordinal, stage)| keypress_event(sequence, ordinal, stage))
                .collect(),
        }
    }

    fn resize_trace(sequence: u64) -> InteractionTraceV2 {
        let trace_id = trace_id(sequence);
        let events = RendererResizeTraceStage::ALL
            .into_iter()
            .enumerate()
            .map(|(ordinal, resize_stage)| {
                let mut event = keypress_event(
                    sequence,
                    ordinal.min(RendererKeypressTraceStage::ALL.len() - 1),
                    RendererKeypressTraceStage::KeyAppkitReceipt,
                );
                let stage = InteractionTraceStage::ResizeZoom(resize_stage);
                event.event_ordinal = ordinal as u64;
                event.span_id = ordinal as u64 + 1;
                event.parent_span_id = (ordinal > 0).then_some(ordinal as u64);
                event.stage = stage;
                event.producer.thread_id = ordinal as u64 + 1;
                event.producer.connection_generation =
                    stage.requires_connection_generation().then_some(1);
                event.started_at.monotonic_ns = ordinal as u64 * 100;
                event.completed_at.monotonic_ns = ordinal as u64 * 100 + 50;
                event.observation_boundary = if stage.is_display_completion() {
                    InteractionTraceObservationBoundary::DisplayPresented
                } else {
                    InteractionTraceObservationBoundary::InternalState
                };
                event
            })
            .collect();
        InteractionTraceV2 {
            schema_version: INTERACTION_TRACE_V2_SCHEMA_VERSION.to_owned(),
            trace_id,
            path: InteractionTracePath::ResizeZoom,
            events,
        }
    }

    #[test]
    fn sequence_exhaustion_is_sticky_and_never_wraps() {
        let mut allocator = InteractionTraceIdAllocator::resume(run_id(), u64::MAX - 1)
            .expect("last usable sequence is admissible");
        assert_eq!(
            allocator.allocate().expect("last ID allocates").sequence,
            u64::MAX - 1
        );
        assert_eq!(
            allocator.allocate(),
            Err(TraceContractError::TraceSequenceExhausted)
        );
        assert!(allocator.is_exhausted());
        assert_eq!(
            allocator.allocate(),
            Err(TraceContractError::TraceSequenceExhausted)
        );
    }

    #[test]
    fn run_envelope_rejects_duplicate_or_regressing_trace_ids() {
        let trace = keypress_trace(7);
        let duplicate = InteractionTraceRunV2 {
            schema_version: INTERACTION_TRACE_V2_SCHEMA_VERSION.to_owned(),
            run_id: run_id(),
            traces: vec![trace.clone(), trace],
        };
        assert_eq!(
            duplicate.validate_structure(),
            Err(TraceContractError::TraceSequenceNotIncreasing {
                previous: 7,
                actual: 7,
            })
        );

        let regressing = InteractionTraceRunV2 {
            schema_version: INTERACTION_TRACE_V2_SCHEMA_VERSION.to_owned(),
            run_id: run_id(),
            traces: vec![keypress_trace(8), keypress_trace(7)],
        };
        assert_eq!(
            regressing.validate_structure(),
            Err(TraceContractError::TraceSequenceNotIncreasing {
                previous: 8,
                actual: 7,
            })
        );
    }

    #[test]
    fn process_restart_requires_a_new_run_envelope() {
        let first_run = InteractionTraceRunV2 {
            schema_version: INTERACTION_TRACE_V2_SCHEMA_VERSION.to_owned(),
            run_id: run_id(),
            traces: vec![keypress_trace(1)],
        };
        assert!(first_run.validate_structure().is_ok());

        let second_run_id =
            InteractionTraceRunId::new(0xfeed, 0xcafe).expect("second run ID is non-zero");
        let mut second_trace = keypress_trace(1);
        second_trace.trace_id.run_id = second_run_id;
        for event in &mut second_trace.events {
            event.trace_id.run_id = second_run_id;
        }
        let second_run = InteractionTraceRunV2 {
            schema_version: INTERACTION_TRACE_V2_SCHEMA_VERSION.to_owned(),
            run_id: second_run_id,
            traces: vec![second_trace.clone()],
        };
        assert!(second_run.validate_structure().is_ok());

        let mixed = InteractionTraceRunV2 {
            schema_version: INTERACTION_TRACE_V2_SCHEMA_VERSION.to_owned(),
            run_id: run_id(),
            traces: vec![second_trace],
        };
        assert!(matches!(
            mixed.validate_structure(),
            Err(TraceContractError::TraceRunMismatch { .. })
        ));
    }

    #[test]
    fn missing_stage_is_diagnostic_but_not_qualifying() {
        let mut trace = keypress_trace(1);
        trace.events.pop();
        assert!(trace.validate_structure().is_ok());
        assert_eq!(
            trace.validate_qualifying(),
            Err(TraceContractError::MissingStage {
                stage: InteractionTraceStage::Keypress(
                    RendererKeypressTraceStage::DisplayCompletion
                ),
            })
        );
    }

    #[test]
    fn full_resize_inventory_is_qualifying_and_ordered() {
        let trace = resize_trace(2);
        assert_eq!(trace.events.len(), RendererResizeTraceStage::ALL.len());
        assert_eq!(
            trace.validate_qualifying(),
            Ok(InteractionTraceClaimBoundary::CausalSoftwarePath)
        );

        let encoded = serde_json::to_string(&trace).expect("resize trace encodes");
        let decoded: InteractionTraceV2 =
            serde_json::from_str(&encoded).expect("resize trace decodes");
        assert_eq!(decoded, trace);

        let mut missing_completion = decoded;
        missing_completion.events.pop();
        assert_eq!(
            missing_completion.validate_qualifying(),
            Err(TraceContractError::MissingStage {
                stage: InteractionTraceStage::ResizeZoom(
                    RendererResizeTraceStage::DisplayCompletion
                ),
            })
        );
    }

    #[test]
    fn cross_clock_and_clock_regression_arithmetic_fail_closed() {
        let from = InteractionTraceTimestamp {
            clock_domain: InteractionTraceClockDomain {
                host_id: 1,
                process_generation: 1,
                clock_id: 1,
            },
            monotonic_ns: 100,
            wall_time_unix_ns: Some(10_000),
        };
        let other_host = InteractionTraceTimestamp {
            clock_domain: InteractionTraceClockDomain {
                host_id: 2,
                process_generation: 1,
                clock_id: 1,
            },
            monotonic_ns: 200,
            wall_time_unix_ns: Some(10_100),
        };
        assert!(matches!(
            from.duration_until(other_host),
            Err(TraceContractError::CrossClockArithmetic { .. })
        ));

        let earlier = InteractionTraceTimestamp {
            monotonic_ns: 99,
            ..from
        };
        assert_eq!(
            from.duration_until(earlier),
            Err(TraceContractError::ClockRegression {
                start_ns: 100,
                end_ns: 99,
            })
        );
    }

    #[test]
    fn same_clock_cross_event_start_regression_fails_closed() {
        let mut trace = keypress_trace(1);
        trace.events[2].started_at.monotonic_ns = 50;
        trace.events[2].completed_at.monotonic_ns = 60;
        assert_eq!(
            trace.validate_structure(),
            Err(TraceContractError::CrossEventClockRegression {
                clock_domain: trace.events[2].started_at.clock_domain,
                previous_start_ns: 100,
                actual_start_ns: 50,
                event_ordinal: 2,
            })
        );
    }

    #[test]
    fn schema_round_trip_and_current_fixture_are_exact() {
        let trace: InteractionTraceV2 =
            serde_json::from_str(GOOD_FIXTURE).expect("current fixture decodes");
        assert_eq!(
            trace.validate_qualifying(),
            Ok(InteractionTraceClaimBoundary::CausalSoftwarePath)
        );
        let encoded = serde_json::to_string(&trace).expect("trace encodes");
        let decoded: InteractionTraceV2 =
            serde_json::from_str(&encoded).expect("encoded trace decodes");
        assert_eq!(decoded, trace);

        let schema: serde_json::Value =
            serde_json::from_str(JSON_SCHEMA).expect("JSON schema parses");
        assert_eq!(
            schema["properties"]["schema_version"]["const"].as_str(),
            Some(INTERACTION_TRACE_V2_SCHEMA_VERSION)
        );
    }

    #[test]
    fn json_schema_accepts_typed_traces_and_rejects_negative_fixtures() {
        let schema: serde_json::Value =
            serde_json::from_str(JSON_SCHEMA).expect("JSON schema parses");
        let validator = Validator::options()
            .with_draft(Draft::Draft202012)
            .build(&schema)
            .expect("interaction trace v2 schema compiles");

        for (label, value) in [
            (
                "committed keypress fixture",
                serde_json::from_str(GOOD_FIXTURE).expect("keypress fixture parses"),
            ),
            (
                "typed keypress roundtrip",
                serde_json::to_value(keypress_trace(1)).expect("keypress trace serializes"),
            ),
            (
                "typed resize roundtrip",
                serde_json::to_value(resize_trace(2)).expect("resize trace serializes"),
            ),
        ] {
            let errors = validator
                .iter_errors(&value)
                .map(|error| format!("{error} at {}", error.instance_path()))
                .collect::<Vec<_>>();
            assert!(
                errors.is_empty(),
                "{label} failed schema validation: {errors:?}"
            );
        }

        for (label, fixture) in [
            ("old schema", OLD_FIXTURE),
            ("raw-content fields", PRIVACY_BAD_FIXTURE),
        ] {
            let value: serde_json::Value =
                serde_json::from_str(fixture).expect("negative fixture parses as JSON");
            assert!(
                !validator.is_valid(&value),
                "{label} fixture unexpectedly passed"
            );
        }
    }

    #[test]
    fn old_fixture_is_retained_but_rejected_by_version() {
        let trace: InteractionTraceV2 =
            serde_json::from_str(OLD_FIXTURE).expect("old shape remains parseable");
        assert_eq!(
            trace.validate_structure(),
            Err(TraceContractError::UnsupportedSchemaVersion)
        );
    }

    #[test]
    fn unknown_raw_content_fields_are_rejected_and_never_serialized() {
        assert!(serde_json::from_str::<InteractionTraceV2>(PRIVACY_BAD_FIXTURE).is_err());
        let encoded = serde_json::to_string(&keypress_trace(1)).expect("trace encodes");
        for forbidden in [
            "raw_key",
            "key_text",
            "pane_content",
            "pane_text",
            "title",
            "cwd",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "serialized trace leaked {forbidden}"
            );
        }

        let mut nested: serde_json::Value =
            serde_json::from_str(&encoded).expect("serialized trace is JSON");
        nested["events"][0]["raw_key"] = serde_json::json!("forbidden");
        assert!(serde_json::from_value::<InteractionTraceV2>(nested.clone()).is_err());
        nested["events"][0]
            .as_object_mut()
            .expect("event is an object")
            .remove("raw_key");
        nested["events"][0]["stage"]["pane_text"] = serde_json::json!("forbidden");
        assert!(serde_json::from_value::<InteractionTraceV2>(nested).is_err());
    }

    #[test]
    fn all_correlation_classes_map_to_closed_claim_boundaries() {
        let cases = [
            (
                InteractionTraceCorrelation::ExactProtocol {
                    protocol_token: 1,
                    protocol_generation: 1,
                },
                InteractionTraceCorrelationQuality::ExactProtocol,
                InteractionTraceClaimBoundary::CausalSoftwarePath,
            ),
            (
                InteractionTraceCorrelation::ExactEchoFixture {
                    fixture_token: 1,
                    expected_terminal_generation: 1,
                },
                InteractionTraceCorrelationQuality::ExactEchoFixture,
                InteractionTraceClaimBoundary::ExactFixturePath,
            ),
            (
                InteractionTraceCorrelation::CausalCandidate {
                    candidate_window_ns: 1,
                },
                InteractionTraceCorrelationQuality::CausalCandidate,
                InteractionTraceClaimBoundary::DiagnosticCandidate,
            ),
            (
                InteractionTraceCorrelation::Uncorrelated,
                InteractionTraceCorrelationQuality::Uncorrelated,
                InteractionTraceClaimBoundary::AggregateOnly,
            ),
        ];
        for (correlation, quality, boundary) in cases {
            assert_eq!(correlation.quality(), quality);
            assert_eq!(InteractionTraceClaimBoundary::from(quality), boundary);
        }
    }

    #[test]
    fn metric_map_is_exhaustive_and_photon_authority_is_detector_only() {
        assert_eq!(
            INTERACTION_TRACE_V2_METRIC_MAP.len(),
            InteractionTraceMetric::ALL.len()
        );
        assert!(lint_interaction_trace_v2_metric_map().is_ok());
    }

    #[test]
    fn gpu_submit_cannot_impersonate_display_or_photon_completion() {
        let mut event = keypress_event(1, 12, RendererKeypressTraceStage::GpuSubmitDrawableRequest);
        event.observation_boundary = InteractionTraceObservationBoundary::Photon;
        event.physical_detector = Some(InteractionTracePhysicalDetector {
            detector_id: 1,
            calibration_id: 1,
        });
        let spans = (1..=12).collect();
        assert!(matches!(
            validate_event(&event, &spans),
            Err(TraceContractError::ObservationBoundaryTooStrong { .. })
        ));
    }

    #[test]
    fn sampling_loss_downgrades_an_otherwise_complete_trace() {
        let mut trace = keypress_trace(1);
        trace.events[4].sampling_loss.dropped_events = 1;
        assert_eq!(
            trace.validate_qualifying(),
            Err(TraceContractError::SamplingLoss {
                event_ordinal: 4,
                dropped_events: 1,
                overwritten_events: 0,
            })
        );
    }
}
