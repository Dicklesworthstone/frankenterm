//! Fixed-total-memory, allocation-free interaction flight recorder.
//!
//! The recorder is deliberately lower in the dependency graph than the mux,
//! client, GUI, server, and `frankenterm-core`. Producers register a shard on
//! an off-path boundary and then publish fixed-size numeric events through a
//! bounded [`ArrayQueue`]. The initialized hot path performs no allocation,
//! blocking, locking, formatting, serialization, logging, clock lookup, I/O,
//! marker emission, dynamic dispatch, or arbitrary callback.
//!
//! Close and semantic conversion are explicitly off-path. A read-mostly
//! recorder-wide close flag and shard-local in-flight words establish the
//! close cut without forcing every producer to write the same cache line.
//! Once the close flag is visible, no new operation can enter, and freezing
//! waits for every shard-local admitted count to reach zero before draining.

use std::marker::PhantomData;
use std::mem::{align_of, size_of};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_queue::ArrayQueue;
use crossbeam_utils::CachePadded;
use frankenterm_core_audit_types::interaction_flight_recorder_v1::{
    CONVERSION_WORKSPACE_EVENTS, MAX_RAW_EVENT_BYTES, RecorderAccountingAuthority,
    RecorderCapacityV1, RecorderContractError, RecorderEpochId, RecorderEventAccountingV1,
    RecorderMode, RecorderSamplerConfigV1, RecorderTraceAccountingV1,
    SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION, SampledTraceContextV1,
};
use frankenterm_core_audit_types::interaction_trace_v2::{
    INTERACTION_TRACE_V2_SCHEMA_VERSION, InteractionTraceClockDomain, InteractionTraceCorrelation,
    InteractionTraceCounterField, InteractionTraceCounterUnavailability, InteractionTraceCounters,
    InteractionTraceEventV2, InteractionTraceGenerations, InteractionTraceId,
    InteractionTraceObservationBoundary, InteractionTracePath, InteractionTracePhysicalDetector,
    InteractionTraceProducer, InteractionTraceRunId, InteractionTraceSamplingLoss,
    InteractionTraceStage, InteractionTraceStageOutcome, InteractionTraceTimestamp,
    InteractionTraceTopology,
};
use thiserror::Error;

const AUTHORITY_EXACT: u8 = 0;
const AUTHORITY_EXHAUSTED: u8 = 1;
const COUNTER_EXHAUSTED: u64 = u64::MAX;
const LAST_EXACT_COUNTER_VALUE: u64 = u64::MAX - 1;
const DEFAULT_SERIALIZATION_WORKSPACE_BYTES: usize = 64 * 1024;
// Reserve fixed bookkeeping slack for each heap-backed recorder component.
// This is intentionally charged once per shard below, so a multi-shard
// configuration overestimates rather than hides recorder-global metadata.
const FIXED_BOOKKEEPING_WORDS_PER_SHARD: usize = 16;

/// Immutable configuration for one recorder epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecorderConfig {
    epoch_id: RecorderEpochId,
    local_run_id: InteractionTraceRunId,
    mode: RecorderMode,
    sampler: RecorderSamplerConfigV1,
    capacity: RecorderCapacityV1,
}

impl RecorderConfig {
    /// Build and validate a recorder configuration before any recorder-owned
    /// queue or workspace allocation occurs.
    pub fn new(
        epoch_id: RecorderEpochId,
        local_run_id: InteractionTraceRunId,
        mode: RecorderMode,
        sampler: RecorderSamplerConfigV1,
        shard_count: u16,
        total_slots: u32,
        configured_byte_ceiling: u64,
    ) -> Result<Self, RecorderError> {
        if !epoch_id.is_valid() {
            return Err(RecorderError::InvalidEpochId);
        }
        if !local_run_id.is_valid() {
            return Err(RecorderError::InvalidRunId);
        }
        sampler.validate_for_mode(mode)?;

        let raw_event_bytes = u16::try_from(size_of::<RawInteractionEvent>()).map_err(|_| {
            RecorderError::RawEventTooLarge {
                actual: size_of::<RawInteractionEvent>(),
                maximum: usize::from(MAX_RAW_EVENT_BYTES),
            }
        })?;
        if raw_event_bytes > MAX_RAW_EVENT_BYTES {
            return Err(RecorderError::RawEventTooLarge {
                actual: usize::from(raw_event_bytes),
                maximum: usize::from(MAX_RAW_EVENT_BYTES),
            });
        }

        // Crossbeam's private queue slot contains a sequence word beside the
        // payload. Adding one payload alignment unit is a conservative bound
        // for private padding without freezing Crossbeam's Rust layout.
        let queue_slot_overhead_bytes = u16::try_from(
            size_of::<usize>()
                .checked_add(align_of::<RawInteractionEvent>())
                .ok_or(RecorderError::CapacityArithmeticOverflow)?,
        )
        .map_err(|_| RecorderError::CapacityArithmeticOverflow)?;
        let queue_header_bytes_per_shard =
            u32::try_from(size_of::<CachePadded<ArrayQueue<RawInteractionEvent>>>())
                .map_err(|_| RecorderError::CapacityArithmeticOverflow)?;
        let padded_counter_bytes_per_shard = u32::try_from(size_of::<CachePadded<ShardCounters>>())
            .map_err(|_| RecorderError::CapacityArithmeticOverflow)?;
        let shard_metadata_bytes_per_shard = u32::try_from(
            size_of::<AtomicBool>()
                .checked_add(size_of::<usize>())
                .and_then(|bytes| bytes.checked_add(size_of::<FlightRecorder>()))
                .and_then(|bytes| bytes.checked_add(size_of::<RecorderWorkspace>()))
                .and_then(|bytes| {
                    FIXED_BOOKKEEPING_WORDS_PER_SHARD
                        .checked_mul(size_of::<usize>())
                        .and_then(|bookkeeping| bytes.checked_add(bookkeeping))
                })
                .ok_or(RecorderError::CapacityArithmeticOverflow)?,
        )
        .map_err(|_| RecorderError::CapacityArithmeticOverflow)?;
        let conversion_event_bytes = u32::from(raw_event_bytes);
        let serialization_workspace_bytes = u64::try_from(DEFAULT_SERIALIZATION_WORKSPACE_BYTES)
            .map_err(|_| RecorderError::CapacityArithmeticOverflow)?;

        let capacity = RecorderCapacityV1 {
            shard_count,
            total_slots,
            raw_event_bytes,
            queue_slot_overhead_bytes,
            queue_header_bytes_per_shard,
            padded_counter_bytes_per_shard,
            shard_metadata_bytes_per_shard,
            frozen_export_slot_bytes: raw_event_bytes,
            conversion_event_bytes,
            serialization_workspace_bytes,
            configured_byte_ceiling,
        };
        capacity.validate()?;

        Ok(Self {
            epoch_id,
            local_run_id,
            mode,
            sampler,
            capacity,
        })
    }

    #[must_use]
    pub const fn epoch_id(self) -> RecorderEpochId {
        self.epoch_id
    }

    #[must_use]
    pub const fn local_run_id(self) -> InteractionTraceRunId {
        self.local_run_id
    }

    #[must_use]
    pub const fn mode(self) -> RecorderMode {
        self.mode
    }

    #[must_use]
    pub const fn sampler(self) -> RecorderSamplerConfigV1 {
        self.sampler
    }

    #[must_use]
    pub const fn capacity(self) -> RecorderCapacityV1 {
        self.capacity
    }

    pub fn validate(self) -> Result<(), RecorderError> {
        if !self.epoch_id.is_valid() {
            return Err(RecorderError::InvalidEpochId);
        }
        if !self.local_run_id.is_valid() {
            return Err(RecorderError::InvalidRunId);
        }
        self.sampler.validate_for_mode(self.mode)?;
        let raw_size = size_of::<RawInteractionEvent>();
        if raw_size > usize::from(MAX_RAW_EVENT_BYTES)
            || usize::from(self.capacity.raw_event_bytes) < raw_size
            || usize::from(self.capacity.frozen_export_slot_bytes) < raw_size
            || usize::try_from(self.capacity.conversion_event_bytes)
                .map_or(true, |bytes| bytes < raw_size)
        {
            return Err(RecorderError::RawEventTooLarge {
                actual: raw_size,
                maximum: usize::from(MAX_RAW_EVENT_BYTES),
            });
        }
        self.capacity.validate()?;
        Ok(())
    }
}

/// Caller-supplied monotonic start/completion stamps.
///
/// The recorder never queries a clock. Constructing this value does not make
/// unlike clock domains comparable; validation fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockStamp {
    pub started_at: InteractionTraceTimestamp,
    pub completed_at: InteractionTraceTimestamp,
}

impl ClockStamp {
    pub fn validate(self, producer: InteractionTraceProducer) -> Result<(), RecorderError> {
        for clock in [self.started_at.clock_domain, self.completed_at.clock_domain] {
            if clock.clock_id == 0
                || clock.host_id != producer.host_id
                || clock.process_generation != producer.process_generation
            {
                return Err(RecorderError::InvalidClock);
            }
        }
        self.started_at
            .duration_until(self.completed_at)
            .map_err(|_| RecorderError::InvalidClock)?;
        Ok(())
    }
}

/// Fixed-shape, content-free semantic fields supplied by a producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventFields {
    event_ordinal: u64,
    span_id: u64,
    parent_span_id: Option<u64>,
    stage: InteractionTraceStage,
    stage_outcome: InteractionTraceStageOutcome,
    producer: InteractionTraceProducer,
    topology: InteractionTraceTopology,
    clock: ClockStamp,
    correlation: InteractionTraceCorrelation,
    counters: InteractionTraceCounters,
    counter_unavailability: InteractionTraceCounterUnavailability,
    generations: InteractionTraceGenerations,
    observation_boundary: InteractionTraceObservationBoundary,
    physical_detector: Option<InteractionTracePhysicalDetector>,
}

impl EventFields {
    /// Construct and intrinsically validate one content-free event. The hot
    /// path only checks the admitted token and caller-supplied clock stamp.
    pub fn new(
        event_ordinal: u64,
        span_id: u64,
        parent_span_id: Option<u64>,
        stage: InteractionTraceStage,
        stage_outcome: InteractionTraceStageOutcome,
        producer: InteractionTraceProducer,
        topology: InteractionTraceTopology,
        clock: ClockStamp,
        correlation: InteractionTraceCorrelation,
        counters: InteractionTraceCounters,
        counter_unavailability: InteractionTraceCounterUnavailability,
        generations: InteractionTraceGenerations,
        observation_boundary: InteractionTraceObservationBoundary,
        physical_detector: Option<InteractionTracePhysicalDetector>,
    ) -> Result<Self, RecorderError> {
        let fields = Self {
            event_ordinal,
            span_id,
            parent_span_id,
            stage,
            stage_outcome,
            producer,
            topology,
            clock,
            correlation,
            counters,
            counter_unavailability,
            generations,
            observation_boundary,
            physical_detector,
        };
        fields.validate_intrinsic()?;
        Ok(fields)
    }

    fn validate_intrinsic(&self) -> Result<(), RecorderError> {
        if self.event_ordinal != u64::from(self.stage.ordinal()) {
            return Err(RecorderError::InvalidEvent("stage ordinal mismatch"));
        }
        if self.span_id == 0 || self.parent_span_id == Some(self.span_id) {
            return Err(RecorderError::InvalidEvent("invalid span identity"));
        }
        if self.producer.host_id == 0
            || self.producer.process_id == 0
            || self.producer.process_generation == 0
            || self.producer.thread_id == 0
            || self.producer.connection_generation == Some(0)
            || (self.stage.requires_connection_generation()
                && self.producer.connection_generation.is_none())
        {
            return Err(RecorderError::InvalidEvent("invalid producer identity"));
        }
        if self.topology.window_id == 0 || self.topology.tab_id == 0 || self.topology.pane_id == 0 {
            return Err(RecorderError::InvalidEvent("invalid topology identity"));
        }
        if matches!(self.stage, InteractionTraceStage::Keypress(_))
            && self.stage_outcome == InteractionTraceStageOutcome::NotApplicable
        {
            return Err(RecorderError::InvalidEvent(
                "not-applicable is invalid on keypress path",
            ));
        }
        if matches!(
            self.stage_outcome,
            InteractionTraceStageOutcome::NoOp
                | InteractionTraceStageOutcome::NotApplicable
                | InteractionTraceStageOutcome::Superseded
        ) && self.clock.started_at.monotonic_ns != self.clock.completed_at.monotonic_ns
        {
            return Err(RecorderError::InvalidEvent(
                "inactive stage has nonzero duration",
            ));
        }
        for field in InteractionTraceCounterField::ALL {
            if self.counter_unavailability.is_unavailable(field) && self.counters.value(field) != 0
            {
                return Err(RecorderError::InvalidEvent(
                    "unavailable counter has a nonzero value",
                ));
            }
        }
        validate_correlation(self.correlation)?;
        validate_generations(self.stage, self.generations)?;
        validate_observation(
            self.stage,
            self.observation_boundary,
            self.physical_detector,
        )?;
        Ok(())
    }

    fn matches_token(&self, token: TraceToken) -> bool {
        self.stage.path() == token.context.path
    }
}

/// Whole-trace token. The origin context survives a remote hop while
/// `local_epoch_id` binds publication to exactly one receiving recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceToken {
    local_epoch_id: RecorderEpochId,
    context: SampledTraceContextV1,
}

impl TraceToken {
    #[must_use]
    pub const fn local_epoch_id(self) -> RecorderEpochId {
        self.local_epoch_id
    }

    #[must_use]
    pub const fn sampled_context(self) -> SampledTraceContextV1 {
        self.context
    }

    #[must_use]
    pub const fn trace_id(self) -> InteractionTraceId {
        self.context.trace_id
    }

    #[must_use]
    pub const fn path(self) -> InteractionTracePath {
        self.context.path
    }
}

/// Result of one whole-trace admission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceAdmission {
    Off,
    Admitted {
        token: TraceToken,
        accounting_authority: RecorderAccountingAuthority,
    },
    SampledOut {
        accounting_authority: RecorderAccountingAuthority,
    },
    TraceIdExhausted {
        accounting_authority: RecorderAccountingAuthority,
    },
    InvalidRemoteContext,
    /// The call linearized after the close cut and is therefore outside the
    /// closed epoch's event-attempt accounting. Admitted pre-cut operations
    /// are allowed to finish, so this implementation's frozen `closing`
    /// counter remains zero by construction.
    Closing,
    WrongRecorder,
}

/// Result of one event-publication attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    Off,
    Recorded {
        accounting_authority: RecorderAccountingAuthority,
    },
    QueueFull {
        accounting_authority: RecorderAccountingAuthority,
    },
    Closing,
    WrongRecorder,
    EpochMismatch {
        accounting_authority: RecorderAccountingAuthority,
    },
    ClockInvalid {
        accounting_authority: RecorderAccountingAuthority,
    },
}

/// Linearizable close/freeze outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseOutcome {
    Ready,
    Draining { in_flight_operations: u64 },
    AlreadyFrozen,
    WorkspacePoisoned,
}

/// Bounded semantic export result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportOutcome {
    Completed { exported_events: usize },
    DestinationCapacityInsufficient { required: usize, available: usize },
    InvalidRawEvent { index: usize },
}

/// Exact or exhausted aggregate counter snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecorderAccountingSnapshot {
    pub trace: RecorderTraceAccountingV1,
    pub event: RecorderEventAccountingV1,
    pub authority: RecorderAccountingAuthority,
}

/// Deterministically ordered, fixed-bounded set of frozen raw events.
#[derive(Debug)]
pub struct FrozenBatch {
    epoch_id: RecorderEpochId,
    events: Vec<RawInteractionEvent>,
    accounting: RecorderAccountingSnapshot,
}

impl FrozenBatch {
    #[must_use]
    pub const fn epoch_id(&self) -> RecorderEpochId {
        self.epoch_id
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    #[must_use]
    pub const fn accounting(&self) -> RecorderAccountingSnapshot {
        self.accounting
    }

    /// Convert into the canonical DTO without transmutation or layout claims.
    /// Conversion is off-path and may allocate the schema string in each DTO.
    pub fn export_into(&self, destination: &mut Vec<InteractionTraceEventV2>) -> ExportOutcome {
        let available = destination.capacity().saturating_sub(destination.len());
        if available < self.events.len() {
            return ExportOutcome::DestinationCapacityInsufficient {
                required: self.events.len(),
                available,
            };
        }
        let original_len = destination.len();
        for (index, raw) in self.events.iter().copied().enumerate() {
            let Ok(event) = raw.decode() else {
                destination.truncate(original_len);
                return ExportOutcome::InvalidRawEvent { index };
            };
            destination.push(event);
        }
        ExportOutcome::Completed {
            exported_events: self.events.len(),
        }
    }
}

/// Explicit off-path producer registration bound to one exact shard.
///
/// The `Rc` marker intentionally makes this type neither `Send` nor `Sync`.
/// There is no hidden TLS registration or first-record allocation.
pub struct ProducerHandle {
    recorder: Arc<FlightRecorder>,
    epoch_id: RecorderEpochId,
    shard_index: usize,
    owns_claim: bool,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl std::fmt::Debug for ProducerHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProducerHandle")
            .field("epoch_id", &self.epoch_id)
            .field("shard_index", &self.shard_index)
            .finish_non_exhaustive()
    }
}

impl ProducerHandle {
    #[must_use]
    pub const fn shard_index(&self) -> usize {
        self.shard_index
    }

    #[must_use]
    pub const fn epoch_id(&self) -> RecorderEpochId {
        self.epoch_id
    }
}

impl Drop for ProducerHandle {
    fn drop(&mut self) {
        if self.owns_claim
            && let Some(shard) = self.recorder.shards.get(self.shard_index)
        {
            shard.producer_claimed.store(false, Ordering::Release);
        }
    }
}

#[derive(Debug)]
struct RecorderShard {
    queue: CachePadded<ArrayQueue<RawInteractionEvent>>,
    counters: CachePadded<ShardCounters>,
    producer_claimed: AtomicBool,
}

#[derive(Debug, Default)]
struct ShardCounters {
    in_flight: AtomicU64,
    sampled_in: AtomicU64,
    sampled_out: AtomicU64,
    trace_id_exhausted: AtomicU64,
    recorded: AtomicU64,
    queue_full: AtomicU64,
    closing: AtomicU64,
    clock_invalid: AtomicU64,
    epoch_mismatch: AtomicU64,
}

#[derive(Debug)]
struct RecorderWorkspace {
    frozen_events: Option<Vec<RawInteractionEvent>>,
    conversion_workspace: Vec<RawInteractionEvent>,
    serialization_workspace: Vec<u8>,
}

/// Cross-layer bounded flight recorder.
#[derive(Debug)]
pub struct FlightRecorder {
    config: RecorderConfig,
    shards: Vec<RecorderShard>,
    next_trace_sequence: AtomicU64,
    closing: AtomicBool,
    accounting_authority: AtomicU8,
    workspace: Mutex<RecorderWorkspace>,
}

impl FlightRecorder {
    /// Allocate every queue and workspace before the recorder becomes visible.
    pub fn new(config: RecorderConfig) -> Result<Arc<Self>, RecorderError> {
        config.validate()?;
        let distribution = config.capacity.checked_shard_distribution()?;

        let mut shards = Vec::new();
        let enabled = config.mode != RecorderMode::Off;
        if enabled {
            let requested_shards = usize::from(config.capacity.shard_count);
            shards
                .try_reserve_exact(requested_shards)
                .map_err(|_| RecorderError::AllocationFailed("shard metadata"))?;
            reject_allocator_over_reservation(
                "shard metadata",
                requested_shards,
                shards.capacity(),
            )?;
            for shard_index in 0..requested_shards {
                let extra = u32::from(u8::from(
                    shard_index < usize::from(distribution.remainder_shards),
                ));
                let slots = distribution
                    .base_slots_per_shard
                    .checked_add(extra)
                    .ok_or(RecorderError::CapacityArithmeticOverflow)?;
                let slots = usize::try_from(slots)
                    .map_err(|_| RecorderError::CapacityArithmeticOverflow)?;
                shards.push(RecorderShard {
                    queue: CachePadded::new(ArrayQueue::new(slots)),
                    counters: CachePadded::new(ShardCounters::default()),
                    producer_claimed: AtomicBool::new(false),
                });
            }
        }

        let mut frozen_events = Vec::new();
        let mut conversion_workspace = Vec::new();
        let mut serialization_workspace = Vec::new();
        if enabled {
            let requested_frozen_events = usize::try_from(config.capacity.total_slots)
                .map_err(|_| RecorderError::CapacityArithmeticOverflow)?;
            frozen_events
                .try_reserve_exact(requested_frozen_events)
                .map_err(|_| RecorderError::AllocationFailed("frozen export"))?;
            reject_allocator_over_reservation(
                "frozen export",
                requested_frozen_events,
                frozen_events.capacity(),
            )?;
            // Keep the allocation identical to the manifest's checked memory
            // equation. This workspace converts at most one bounded trace at
            // a time; it must not grow with the recorder's global slot count.
            let requested_conversion_events = usize::from(CONVERSION_WORKSPACE_EVENTS);
            conversion_workspace
                .try_reserve_exact(requested_conversion_events)
                .map_err(|_| RecorderError::AllocationFailed("conversion workspace"))?;
            reject_allocator_over_reservation(
                "conversion workspace",
                requested_conversion_events,
                conversion_workspace.capacity(),
            )?;
            let requested_serialization_bytes =
                usize::try_from(config.capacity.serialization_workspace_bytes)
                    .map_err(|_| RecorderError::CapacityArithmeticOverflow)?;
            serialization_workspace
                .try_reserve_exact(requested_serialization_bytes)
                .map_err(|_| RecorderError::AllocationFailed("serialization workspace"))?;
            reject_allocator_over_reservation(
                "serialization workspace",
                requested_serialization_bytes,
                serialization_workspace.capacity(),
            )?;
        }

        Ok(Arc::new(Self {
            config,
            shards,
            next_trace_sequence: AtomicU64::new(1),
            closing: AtomicBool::new(false),
            accounting_authority: AtomicU8::new(AUTHORITY_EXACT),
            workspace: Mutex::new(RecorderWorkspace {
                frozen_events: Some(frozen_events),
                conversion_workspace,
                serialization_workspace,
            }),
        }))
    }

    #[must_use]
    pub const fn config(&self) -> RecorderConfig {
        self.config
    }

    #[must_use]
    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    /// Explicitly claim one producer shard. This operation is off the record
    /// hot path. Off mode returns a no-op handle without touching recorder
    /// atomics or queue state.
    pub fn register_producer(
        self: &Arc<Self>,
        shard_index: usize,
    ) -> Result<ProducerHandle, RecorderError> {
        if self.config.mode == RecorderMode::Off {
            return Ok(ProducerHandle {
                recorder: Arc::clone(self),
                epoch_id: self.config.epoch_id,
                shard_index: 0,
                owns_claim: false,
                _not_send_or_sync: PhantomData,
            });
        }
        if self.is_closing() {
            return Err(RecorderError::Closing);
        }
        let shard = self
            .shards
            .get(shard_index)
            .ok_or(RecorderError::ShardOutOfRange {
                requested: shard_index,
                shard_count: self.shards.len(),
            })?;
        shard
            .producer_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| RecorderError::ShardAlreadyClaimed { shard_index })?;
        if self.is_closing() {
            shard.producer_claimed.store(false, Ordering::Release);
            return Err(RecorderError::Closing);
        }
        Ok(ProducerHandle {
            recorder: Arc::clone(self),
            epoch_id: self.config.epoch_id,
            shard_index,
            owns_claim: true,
            _not_send_or_sync: PhantomData,
        })
    }

    /// Allocate and sample one local trace exactly once.
    pub fn admit_local_trace(
        &self,
        producer: &ProducerHandle,
        path: InteractionTracePath,
    ) -> TraceAdmission {
        if self.config.mode == RecorderMode::Off {
            return TraceAdmission::Off;
        }
        let Some(shard) = self.shard_for_handle(producer) else {
            return TraceAdmission::WrongRecorder;
        };
        let Some(_admission) = self.try_enter(shard) else {
            return TraceAdmission::Closing;
        };
        let Some(trace_id) = self.allocate_trace_id() else {
            let authority = self.increment(&shard.counters.trace_id_exhausted);
            return TraceAdmission::TraceIdExhausted {
                accounting_authority: authority,
            };
        };
        let sampled = self.config.sampler.samples(trace_id).unwrap_or(false);
        if !sampled {
            let authority = self.increment(&shard.counters.sampled_out);
            return TraceAdmission::SampledOut {
                accounting_authority: authority,
            };
        }
        let authority = self.increment(&shard.counters.sampled_in);
        TraceAdmission::Admitted {
            token: TraceToken {
                local_epoch_id: self.config.epoch_id,
                context: SampledTraceContextV1 {
                    schema_version: SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION,
                    trace_id,
                    path,
                    origin_recorder_epoch_id: self.config.epoch_id,
                    sampler_algorithm: self.config.sampler.algorithm,
                },
            },
            accounting_authority: authority,
        }
    }

    /// Admit an already-sampled remote trace without resampling or replacing
    /// its origin run/epoch identity.
    pub fn admit_remote_trace(
        &self,
        producer: &ProducerHandle,
        context: SampledTraceContextV1,
    ) -> TraceAdmission {
        if self.config.mode == RecorderMode::Off {
            return TraceAdmission::Off;
        }
        let Some(shard) = self.shard_for_handle(producer) else {
            return TraceAdmission::WrongRecorder;
        };
        if context.validate().is_err() {
            return TraceAdmission::InvalidRemoteContext;
        }
        let Some(_admission) = self.try_enter(shard) else {
            return TraceAdmission::Closing;
        };
        let authority = self.increment(&shard.counters.sampled_in);
        TraceAdmission::Admitted {
            token: TraceToken {
                local_epoch_id: self.config.epoch_id,
                context,
            },
            accounting_authority: authority,
        }
    }

    /// Publish one fixed-size numeric event through the producer's queue.
    pub fn record(
        &self,
        producer: &ProducerHandle,
        token: TraceToken,
        fields: &EventFields,
    ) -> RecordOutcome {
        if self.config.mode == RecorderMode::Off {
            return RecordOutcome::Off;
        }
        let Some(shard) = self.shard_for_handle(producer) else {
            return RecordOutcome::WrongRecorder;
        };
        let Some(_admission) = self.try_enter(shard) else {
            return RecordOutcome::Closing;
        };
        if token.local_epoch_id != self.config.epoch_id || token.context.validate().is_err() {
            return RecordOutcome::EpochMismatch {
                accounting_authority: self.increment(&shard.counters.epoch_mismatch),
            };
        }
        if !fields.matches_token(token) {
            return RecordOutcome::EpochMismatch {
                accounting_authority: self.increment(&shard.counters.epoch_mismatch),
            };
        }
        if fields.clock.validate(fields.producer).is_err() {
            return RecordOutcome::ClockInvalid {
                accounting_authority: self.increment(&shard.counters.clock_invalid),
            };
        }
        let prior_dropped = exact_counter_value(&shard.counters.queue_full);
        let raw = RawInteractionEvent::encode(token, fields, prior_dropped);
        if shard.queue.push(raw).is_err() {
            return RecordOutcome::QueueFull {
                accounting_authority: self.increment(&shard.counters.queue_full),
            };
        }
        RecordOutcome::Recorded {
            accounting_authority: self.increment(&shard.counters.recorded),
        }
    }

    /// Set the linearizable close bit. Repeated calls are harmless.
    #[must_use]
    pub fn begin_close(&self) -> CloseOutcome {
        self.closing.store(true, Ordering::SeqCst);
        let in_flight_operations = self.in_flight_operations();
        if in_flight_operations == 0 {
            CloseOutcome::Ready
        } else {
            CloseOutcome::Draining {
                in_flight_operations,
            }
        }
    }

    /// Freeze only after the close cut has excluded new admissions and every
    /// admitted hot operation has left. Queue drain, sort, and mutex use are
    /// deliberately off-path.
    pub fn try_freeze(&self) -> Result<FrozenBatch, CloseOutcome> {
        let _ = self.begin_close();
        let in_flight_operations = self.in_flight_operations();
        if in_flight_operations != 0 {
            return Err(CloseOutcome::Draining {
                in_flight_operations,
            });
        }
        let accounting = self.accounting_snapshot();
        let Ok(mut workspace) = self.workspace.lock() else {
            return Err(CloseOutcome::WorkspacePoisoned);
        };
        let Some(mut events) = workspace.frozen_events.take() else {
            return Err(CloseOutcome::AlreadyFrozen);
        };
        for shard in &self.shards {
            while let Some(event) = shard.queue.pop() {
                events.push(event);
            }
        }
        events.sort_unstable();
        Ok(FrozenBatch {
            epoch_id: self.config.epoch_id,
            events,
            accounting,
        })
    }

    #[must_use]
    pub fn accounting_snapshot(&self) -> RecorderAccountingSnapshot {
        let mut trace = RecorderTraceAccountingV1::default();
        let mut event = RecorderEventAccountingV1::default();
        let mut exact = self.accounting_authority.load(Ordering::Acquire) == AUTHORITY_EXACT;
        for shard in &self.shards {
            exact &= checked_add_counter(&mut trace.sampled_in, &shard.counters.sampled_in);
            exact &= checked_add_counter(&mut trace.sampled_out, &shard.counters.sampled_out);
            exact &= checked_add_counter(
                &mut trace.trace_id_exhausted,
                &shard.counters.trace_id_exhausted,
            );
            exact &= checked_add_counter(&mut event.recorded, &shard.counters.recorded);
            exact &= checked_add_counter(&mut event.queue_full, &shard.counters.queue_full);
            exact &= checked_add_counter(&mut event.closing, &shard.counters.closing);
            exact &= checked_add_counter(&mut event.clock_invalid, &shard.counters.clock_invalid);
            exact &= checked_add_counter(&mut event.epoch_mismatch, &shard.counters.epoch_mismatch);
        }
        if !exact {
            self.accounting_authority
                .store(AUTHORITY_EXHAUSTED, Ordering::Release);
        }
        RecorderAccountingSnapshot {
            trace,
            event,
            authority: if exact {
                RecorderAccountingAuthority::Exact
            } else {
                RecorderAccountingAuthority::Exhausted
            },
        }
    }

    #[must_use]
    pub fn queued_events(&self) -> usize {
        self.shards.iter().map(|shard| shard.queue.len()).sum()
    }

    #[must_use]
    pub fn global_queue_capacity(&self) -> usize {
        self.shards.iter().map(|shard| shard.queue.capacity()).sum()
    }

    #[must_use]
    pub fn workspace_capacities(&self) -> (usize, usize, usize) {
        let Ok(workspace) = self.workspace.lock() else {
            return (0, 0, 0);
        };
        (
            workspace.frozen_events.as_ref().map_or(0, Vec::capacity),
            workspace.conversion_workspace.capacity(),
            workspace.serialization_workspace.capacity(),
        )
    }

    fn shard_for_handle(&self, producer: &ProducerHandle) -> Option<&RecorderShard> {
        if !std::ptr::eq(self, Arc::as_ptr(&producer.recorder))
            || producer.epoch_id != self.config.epoch_id
            || !producer.owns_claim
        {
            return None;
        }
        self.shards.get(producer.shard_index)
    }

    fn try_enter<'a>(&self, shard: &'a RecorderShard) -> Option<AdmissionGuard<'a>> {
        if self.closing.load(Ordering::Relaxed) {
            return None;
        }
        let mut observed = shard.counters.in_flight.load(Ordering::Relaxed);
        loop {
            if observed == u64::MAX {
                return None;
            }
            match shard.counters.in_flight.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => observed = actual,
            }
        }
        if self.closing.load(Ordering::SeqCst) {
            shard.counters.in_flight.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(AdmissionGuard {
            in_flight: &shard.counters.in_flight,
        })
    }

    fn in_flight_operations(&self) -> u64 {
        self.shards.iter().fold(0_u64, |total, shard| {
            total.saturating_add(shard.counters.in_flight.load(Ordering::SeqCst))
        })
    }

    fn allocate_trace_id(&self) -> Option<InteractionTraceId> {
        let mut observed = self.next_trace_sequence.load(Ordering::Acquire);
        loop {
            if observed == 0 || observed == u64::MAX {
                return None;
            }
            let next = if observed == u64::MAX - 1 {
                u64::MAX
            } else {
                observed + 1
            };
            match self.next_trace_sequence.compare_exchange_weak(
                observed,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return InteractionTraceId::new(self.config.local_run_id, observed);
                }
                Err(actual) => observed = actual,
            }
        }
    }

    fn increment(&self, counter: &AtomicU64) -> RecorderAccountingAuthority {
        let mut observed = counter.load(Ordering::Relaxed);
        loop {
            if observed >= LAST_EXACT_COUNTER_VALUE {
                if observed != COUNTER_EXHAUSTED {
                    let _ = counter.compare_exchange_weak(
                        observed,
                        COUNTER_EXHAUSTED,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    );
                }
                self.accounting_authority
                    .store(AUTHORITY_EXHAUSTED, Ordering::Release);
                return RecorderAccountingAuthority::Exhausted;
            }
            match counter.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return self.current_authority(),
                Err(actual) => observed = actual,
            }
        }
    }

    fn current_authority(&self) -> RecorderAccountingAuthority {
        if self.accounting_authority.load(Ordering::Acquire) == AUTHORITY_EXACT {
            RecorderAccountingAuthority::Exact
        } else {
            RecorderAccountingAuthority::Exhausted
        }
    }
}

struct AdmissionGuard<'a> {
    in_flight: &'a AtomicU64,
}

impl Drop for AdmissionGuard<'_> {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Private fixed-size numeric queue payload. Its semantic size is bounded;
/// Rust layout, byte layout, and transmutation are deliberately not API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RawInteractionEvent {
    trace_run_hi: u64,
    trace_run_lo: u64,
    trace_sequence: u64,
    event_ordinal: u64,
    span_id: u64,
    parent_span_id: u64,
    producer_host_id: u64,
    producer_process_generation: u64,
    producer_thread_id: u64,
    producer_connection_generation: u64,
    window_id: u64,
    tab_id: u64,
    pane_id: u64,
    started_clock_host_id: u64,
    started_clock_process_generation: u64,
    started_clock_id: u64,
    started_monotonic_ns: u64,
    started_wall_time_unix_ns: u64,
    completed_clock_host_id: u64,
    completed_clock_process_generation: u64,
    completed_clock_id: u64,
    completed_monotonic_ns: u64,
    completed_wall_time_unix_ns: u64,
    correlation_first: u64,
    correlation_second: u64,
    counters: [u64; 16],
    terminal_generation: u64,
    snapshot_generation: u64,
    frame_generation: u64,
    dropped_events: u64,
    detector_id: u64,
    calibration_id: u64,
    producer_process_id: u32,
    unavailable_mask: u16,
    flags: u16,
    path: u8,
    stage_ordinal: u8,
    stage_outcome: u8,
    correlation_kind: u8,
    observation_boundary: u8,
}

impl RawInteractionEvent {
    fn encode(token: TraceToken, fields: &EventFields, dropped_events: u64) -> Self {
        let (correlation_kind, correlation_first, correlation_second) =
            encode_correlation(fields.correlation);
        Self {
            trace_run_hi: token.context.trace_id.run_id.epoch_nonce_hi,
            trace_run_lo: token.context.trace_id.run_id.epoch_nonce_lo,
            trace_sequence: token.context.trace_id.sequence,
            event_ordinal: fields.event_ordinal,
            span_id: fields.span_id,
            parent_span_id: fields.parent_span_id.unwrap_or(0),
            producer_host_id: fields.producer.host_id,
            producer_process_generation: fields.producer.process_generation,
            producer_thread_id: fields.producer.thread_id,
            producer_connection_generation: fields.producer.connection_generation.unwrap_or(0),
            window_id: fields.topology.window_id,
            tab_id: fields.topology.tab_id,
            pane_id: fields.topology.pane_id,
            started_clock_host_id: fields.clock.started_at.clock_domain.host_id,
            started_clock_process_generation: fields
                .clock
                .started_at
                .clock_domain
                .process_generation,
            started_clock_id: fields.clock.started_at.clock_domain.clock_id,
            started_monotonic_ns: fields.clock.started_at.monotonic_ns,
            started_wall_time_unix_ns: fields.clock.started_at.wall_time_unix_ns.unwrap_or(0),
            completed_clock_host_id: fields.clock.completed_at.clock_domain.host_id,
            completed_clock_process_generation: fields
                .clock
                .completed_at
                .clock_domain
                .process_generation,
            completed_clock_id: fields.clock.completed_at.clock_domain.clock_id,
            completed_monotonic_ns: fields.clock.completed_at.monotonic_ns,
            completed_wall_time_unix_ns: fields.clock.completed_at.wall_time_unix_ns.unwrap_or(0),
            correlation_first,
            correlation_second,
            counters: encode_counters(fields.counters),
            terminal_generation: fields.generations.terminal_generation.unwrap_or(0),
            snapshot_generation: fields.generations.snapshot_generation.unwrap_or(0),
            frame_generation: fields.generations.frame_generation.unwrap_or(0),
            dropped_events,
            detector_id: fields
                .physical_detector
                .map_or(0, |detector| detector.detector_id),
            calibration_id: fields
                .physical_detector
                .map_or(0, |detector| detector.calibration_id),
            producer_process_id: fields.producer.process_id,
            unavailable_mask: encode_unavailability(fields.counter_unavailability),
            flags: encode_flags(fields),
            path: encode_path(token.context.path),
            stage_ordinal: fields.stage.ordinal(),
            stage_outcome: encode_stage_outcome(fields.stage_outcome),
            correlation_kind,
            observation_boundary: encode_observation(fields.observation_boundary),
        }
    }

    fn decode(self) -> Result<InteractionTraceEventV2, RecorderError> {
        let path = decode_path(self.path)?;
        let stage = InteractionTraceStage::from_ordinal(path, self.stage_ordinal)
            .ok_or(RecorderError::InvalidRawEvent)?;
        let run_id = InteractionTraceRunId::new(self.trace_run_hi, self.trace_run_lo)
            .ok_or(RecorderError::InvalidRawEvent)?;
        let trace_id = InteractionTraceId::new(run_id, self.trace_sequence)
            .ok_or(RecorderError::InvalidRawEvent)?;
        let producer = InteractionTraceProducer {
            host_id: self.producer_host_id,
            process_id: self.producer_process_id,
            process_generation: self.producer_process_generation,
            thread_id: self.producer_thread_id,
            connection_generation: flag(self.flags, 1)
                .then_some(self.producer_connection_generation),
        };
        let event = InteractionTraceEventV2 {
            schema_version: INTERACTION_TRACE_V2_SCHEMA_VERSION.to_owned(),
            trace_id,
            event_ordinal: self.event_ordinal,
            span_id: self.span_id,
            parent_span_id: flag(self.flags, 0).then_some(self.parent_span_id),
            stage,
            stage_outcome: decode_stage_outcome(self.stage_outcome)?,
            producer,
            topology: InteractionTraceTopology {
                window_id: self.window_id,
                tab_id: self.tab_id,
                pane_id: self.pane_id,
            },
            started_at: InteractionTraceTimestamp {
                clock_domain: InteractionTraceClockDomain {
                    host_id: self.started_clock_host_id,
                    process_generation: self.started_clock_process_generation,
                    clock_id: self.started_clock_id,
                },
                monotonic_ns: self.started_monotonic_ns,
                wall_time_unix_ns: flag(self.flags, 2).then_some(self.started_wall_time_unix_ns),
            },
            completed_at: InteractionTraceTimestamp {
                clock_domain: InteractionTraceClockDomain {
                    host_id: self.completed_clock_host_id,
                    process_generation: self.completed_clock_process_generation,
                    clock_id: self.completed_clock_id,
                },
                monotonic_ns: self.completed_monotonic_ns,
                wall_time_unix_ns: flag(self.flags, 3).then_some(self.completed_wall_time_unix_ns),
            },
            correlation: decode_correlation(
                self.correlation_kind,
                self.correlation_first,
                self.correlation_second,
            )?,
            counters: decode_counters(self.counters),
            counter_unavailability: decode_unavailability(self.unavailable_mask),
            generations: InteractionTraceGenerations {
                terminal_generation: flag(self.flags, 4).then_some(self.terminal_generation),
                snapshot_generation: flag(self.flags, 5).then_some(self.snapshot_generation),
                frame_generation: flag(self.flags, 6).then_some(self.frame_generation),
            },
            sampling_loss: InteractionTraceSamplingLoss {
                dropped_events: self.dropped_events,
                overwritten_events: 0,
            },
            observation_boundary: decode_observation(self.observation_boundary)?,
            physical_detector: flag(self.flags, 7).then_some(InteractionTracePhysicalDetector {
                detector_id: self.detector_id,
                calibration_id: self.calibration_id,
            }),
        };
        Ok(event)
    }
}

#[derive(Debug, Error)]
pub enum RecorderError {
    #[error("invalid recorder epoch id")]
    InvalidEpochId,
    #[error("invalid local trace run id")]
    InvalidRunId,
    #[error("raw event size {actual} exceeds semantic maximum {maximum}")]
    RawEventTooLarge { actual: usize, maximum: usize },
    #[error("recorder capacity arithmetic overflow")]
    CapacityArithmeticOverflow,
    #[error("recorder allocation failed for {0}")]
    AllocationFailed(&'static str),
    #[error(
        "allocator over-reserved {component}: requested {requested} elements but received capacity {actual}"
    )]
    AllocatorOverReserved {
        component: &'static str,
        requested: usize,
        actual: usize,
    },
    #[error("producer shard {requested} is outside shard count {shard_count}")]
    ShardOutOfRange {
        requested: usize,
        shard_count: usize,
    },
    #[error("producer shard {shard_index} is already claimed")]
    ShardAlreadyClaimed { shard_index: usize },
    #[error("recorder is closing")]
    Closing,
    #[error("invalid caller-supplied monotonic clock stamp")]
    InvalidClock,
    #[error("invalid content-free event: {0}")]
    InvalidEvent(&'static str),
    #[error("invalid frozen raw event")]
    InvalidRawEvent,
    #[error(transparent)]
    Contract(#[from] RecorderContractError),
}

fn validate_correlation(correlation: InteractionTraceCorrelation) -> Result<(), RecorderError> {
    match correlation {
        InteractionTraceCorrelation::ExactProtocol {
            protocol_token,
            protocol_generation,
        } if protocol_token == 0 || protocol_generation == 0 => Err(RecorderError::InvalidEvent(
            "invalid exact protocol authority",
        )),
        InteractionTraceCorrelation::ExactEchoFixture {
            fixture_token,
            expected_terminal_generation,
        } if fixture_token == 0 || expected_terminal_generation == 0 => Err(
            RecorderError::InvalidEvent("invalid echo fixture authority"),
        ),
        InteractionTraceCorrelation::CausalCandidate {
            candidate_window_ns: 0,
        } => Err(RecorderError::InvalidEvent(
            "invalid causal candidate window",
        )),
        _ => Ok(()),
    }
}

fn reject_allocator_over_reservation(
    component: &'static str,
    requested: usize,
    actual: usize,
) -> Result<(), RecorderError> {
    if actual > requested {
        Err(RecorderError::AllocatorOverReserved {
            component,
            requested,
            actual,
        })
    } else {
        Ok(())
    }
}

fn validate_generations(
    stage: InteractionTraceStage,
    generations: InteractionTraceGenerations,
) -> Result<(), RecorderError> {
    if generations.terminal_generation == Some(0)
        || generations.snapshot_generation == Some(0)
        || generations.frame_generation == Some(0)
    {
        return Err(RecorderError::InvalidEvent("zero state generation"));
    }
    if (matches!(stage.ordinal(), 7) && stage.path() == InteractionTracePath::Keypress)
        && generations.terminal_generation.is_none()
    {
        return Err(RecorderError::InvalidEvent("terminal generation missing"));
    }
    if ((stage.ordinal() == 8 && stage.path() == InteractionTracePath::Keypress)
        || (stage.ordinal() == 13 && stage.path() == InteractionTracePath::ResizeZoom))
        && generations.snapshot_generation.is_none()
    {
        return Err(RecorderError::InvalidEvent("snapshot generation missing"));
    }
    if stage.is_display_completion() && generations.frame_generation.is_none() {
        return Err(RecorderError::InvalidEvent("frame generation missing"));
    }
    Ok(())
}

fn validate_observation(
    stage: InteractionTraceStage,
    boundary: InteractionTraceObservationBoundary,
    detector: Option<InteractionTracePhysicalDetector>,
) -> Result<(), RecorderError> {
    match (boundary, detector) {
        (InteractionTraceObservationBoundary::Photon, Some(detector))
            if detector.detector_id != 0 && detector.calibration_id != 0 => {}
        (InteractionTraceObservationBoundary::Photon, _) => {
            return Err(RecorderError::InvalidEvent("invalid physical detector"));
        }
        (_, Some(_)) => {
            return Err(RecorderError::InvalidEvent("unexpected physical detector"));
        }
        (_, None) => {}
    }
    let presented = matches!(
        boundary,
        InteractionTraceObservationBoundary::DisplayPresented
            | InteractionTraceObservationBoundary::Photon
    );
    if presented != stage.is_display_completion() {
        return Err(RecorderError::InvalidEvent(
            "display completion and observation boundary disagree",
        ));
    }
    Ok(())
}

fn exact_counter_value(counter: &AtomicU64) -> u64 {
    counter
        .load(Ordering::Relaxed)
        .min(LAST_EXACT_COUNTER_VALUE)
}

fn checked_add_counter(total: &mut u64, counter: &AtomicU64) -> bool {
    let value = counter.load(Ordering::Acquire);
    if value == COUNTER_EXHAUSTED {
        return false;
    }
    let Some(next) = total.checked_add(value) else {
        return false;
    };
    *total = next;
    true
}

fn encode_path(path: InteractionTracePath) -> u8 {
    match path {
        InteractionTracePath::Keypress => 0,
        InteractionTracePath::ResizeZoom => 1,
    }
}

fn decode_path(path: u8) -> Result<InteractionTracePath, RecorderError> {
    match path {
        0 => Ok(InteractionTracePath::Keypress),
        1 => Ok(InteractionTracePath::ResizeZoom),
        _ => Err(RecorderError::InvalidRawEvent),
    }
}

fn encode_stage_outcome(outcome: InteractionTraceStageOutcome) -> u8 {
    match outcome {
        InteractionTraceStageOutcome::Performed => 0,
        InteractionTraceStageOutcome::NoOp => 1,
        InteractionTraceStageOutcome::NotApplicable => 2,
        InteractionTraceStageOutcome::Superseded => 3,
        InteractionTraceStageOutcome::Cancelled => 4,
        InteractionTraceStageOutcome::Failed => 5,
    }
}

fn decode_stage_outcome(value: u8) -> Result<InteractionTraceStageOutcome, RecorderError> {
    match value {
        0 => Ok(InteractionTraceStageOutcome::Performed),
        1 => Ok(InteractionTraceStageOutcome::NoOp),
        2 => Ok(InteractionTraceStageOutcome::NotApplicable),
        3 => Ok(InteractionTraceStageOutcome::Superseded),
        4 => Ok(InteractionTraceStageOutcome::Cancelled),
        5 => Ok(InteractionTraceStageOutcome::Failed),
        _ => Err(RecorderError::InvalidRawEvent),
    }
}

fn encode_observation(boundary: InteractionTraceObservationBoundary) -> u8 {
    match boundary {
        InteractionTraceObservationBoundary::InternalState => 0,
        InteractionTraceObservationBoundary::SoftwarePresent => 1,
        InteractionTraceObservationBoundary::MetalDrawable => 2,
        InteractionTraceObservationBoundary::DisplayPresented => 3,
        InteractionTraceObservationBoundary::Photon => 4,
    }
}

fn decode_observation(value: u8) -> Result<InteractionTraceObservationBoundary, RecorderError> {
    match value {
        0 => Ok(InteractionTraceObservationBoundary::InternalState),
        1 => Ok(InteractionTraceObservationBoundary::SoftwarePresent),
        2 => Ok(InteractionTraceObservationBoundary::MetalDrawable),
        3 => Ok(InteractionTraceObservationBoundary::DisplayPresented),
        4 => Ok(InteractionTraceObservationBoundary::Photon),
        _ => Err(RecorderError::InvalidRawEvent),
    }
}

fn encode_correlation(correlation: InteractionTraceCorrelation) -> (u8, u64, u64) {
    match correlation {
        InteractionTraceCorrelation::ExactProtocol {
            protocol_token,
            protocol_generation,
        } => (0, protocol_token, protocol_generation),
        InteractionTraceCorrelation::ExactEchoFixture {
            fixture_token,
            expected_terminal_generation,
        } => (1, fixture_token, expected_terminal_generation),
        InteractionTraceCorrelation::CausalCandidate {
            candidate_window_ns,
        } => (2, candidate_window_ns, 0),
        InteractionTraceCorrelation::Uncorrelated => (3, 0, 0),
    }
}

fn decode_correlation(
    kind: u8,
    first: u64,
    second: u64,
) -> Result<InteractionTraceCorrelation, RecorderError> {
    match kind {
        0 => Ok(InteractionTraceCorrelation::ExactProtocol {
            protocol_token: first,
            protocol_generation: second,
        }),
        1 => Ok(InteractionTraceCorrelation::ExactEchoFixture {
            fixture_token: first,
            expected_terminal_generation: second,
        }),
        2 => Ok(InteractionTraceCorrelation::CausalCandidate {
            candidate_window_ns: first,
        }),
        3 => Ok(InteractionTraceCorrelation::Uncorrelated),
        _ => Err(RecorderError::InvalidRawEvent),
    }
}

fn encode_counters(counters: InteractionTraceCounters) -> [u64; 16] {
    let mut values = [0; 16];
    for (index, field) in InteractionTraceCounterField::ALL.into_iter().enumerate() {
        values[index] = counters.value(field);
    }
    values
}

fn decode_counters(values: [u64; 16]) -> InteractionTraceCounters {
    InteractionTraceCounters {
        queue_depth: values[0],
        oldest_queue_age_ns: values[1],
        work_units: values[2],
        bytes: values[3],
        rows: values[4],
        allocation_count: values[5],
        allocated_bytes: values[6],
        copy_count: values[7],
        copied_bytes: values[8],
        rpc_count: values[9],
        delta_count: values[10],
        dirty_rows: values[11],
        full_viewport_clones: values[12],
        cursor_row_duplicates: values[13],
        paint_count: values[14],
        frame_count: values[15],
    }
}

fn encode_unavailability(unavailability: InteractionTraceCounterUnavailability) -> u16 {
    let mut mask = 0;
    for (index, field) in InteractionTraceCounterField::ALL.into_iter().enumerate() {
        if unavailability.is_unavailable(field) {
            mask |= 1 << index;
        }
    }
    mask
}

fn decode_unavailability(mask: u16) -> InteractionTraceCounterUnavailability {
    InteractionTraceCounterUnavailability {
        queue_depth: mask & (1 << 0) != 0,
        oldest_queue_age_ns: mask & (1 << 1) != 0,
        work_units: mask & (1 << 2) != 0,
        bytes: mask & (1 << 3) != 0,
        rows: mask & (1 << 4) != 0,
        allocation_count: mask & (1 << 5) != 0,
        allocated_bytes: mask & (1 << 6) != 0,
        copy_count: mask & (1 << 7) != 0,
        copied_bytes: mask & (1 << 8) != 0,
        rpc_count: mask & (1 << 9) != 0,
        delta_count: mask & (1 << 10) != 0,
        dirty_rows: mask & (1 << 11) != 0,
        full_viewport_clones: mask & (1 << 12) != 0,
        cursor_row_duplicates: mask & (1 << 13) != 0,
        paint_count: mask & (1 << 14) != 0,
        frame_count: mask & (1 << 15) != 0,
    }
}

fn encode_flags(fields: &EventFields) -> u16 {
    u16::from(fields.parent_span_id.is_some())
        | (u16::from(fields.producer.connection_generation.is_some()) << 1)
        | (u16::from(fields.clock.started_at.wall_time_unix_ns.is_some()) << 2)
        | (u16::from(fields.clock.completed_at.wall_time_unix_ns.is_some()) << 3)
        | (u16::from(fields.generations.terminal_generation.is_some()) << 4)
        | (u16::from(fields.generations.snapshot_generation.is_some()) << 5)
        | (u16::from(fields.generations.frame_generation.is_some()) << 6)
        | (u16::from(fields.physical_detector.is_some()) << 7)
}

fn flag(flags: u16, bit: u8) -> bool {
    flags & (1 << bit) != 0
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use frankenterm_core_audit_types::interaction_trace_v2::InteractionTraceV2;
    use proptest::prelude::*;
    use static_assertions::{assert_impl_all, assert_not_impl_any};

    use super::*;

    const TEST_BYTE_CEILING: u64 = 32 * 1024 * 1024;

    fn epoch(nonce: u64) -> RecorderEpochId {
        RecorderEpochId::new(nonce, nonce.rotate_left(17)).expect("test epoch must be nonzero")
    }

    fn run(nonce: u64) -> InteractionTraceRunId {
        InteractionTraceRunId::new(nonce, nonce.rotate_left(29)).expect("test run must be nonzero")
    }

    fn config(shard_count: u16, total_slots: u32) -> RecorderConfig {
        RecorderConfig::new(
            epoch(1),
            run(2),
            RecorderMode::Certification,
            RecorderSamplerConfigV1::certification(),
            shard_count,
            total_slots,
            TEST_BYTE_CEILING,
        )
        .expect("test recorder config must be valid")
    }

    fn stage(path: InteractionTracePath, ordinal: u8) -> InteractionTraceStage {
        InteractionTraceStage::from_ordinal(path, ordinal)
            .expect("test stage ordinal must be in range")
    }

    fn fields_for(thread_id: u64, stage: InteractionTraceStage) -> EventFields {
        let clock_domain = InteractionTraceClockDomain {
            host_id: 11,
            process_generation: 22,
            clock_id: 33,
        };
        let display_completion = stage.is_display_completion();
        EventFields::new(
            u64::from(stage.ordinal()),
            u64::from(stage.ordinal()) + 1,
            (stage.ordinal() > 0).then_some(u64::from(stage.ordinal())),
            stage,
            InteractionTraceStageOutcome::Performed,
            InteractionTraceProducer {
                host_id: 11,
                process_id: 44,
                process_generation: 22,
                thread_id,
                connection_generation: Some(55),
            },
            InteractionTraceTopology {
                window_id: 66,
                tab_id: 77,
                pane_id: 88,
            },
            ClockStamp {
                started_at: InteractionTraceTimestamp {
                    clock_domain,
                    monotonic_ns: 100 + u64::from(stage.ordinal()),
                    wall_time_unix_ns: None,
                },
                completed_at: InteractionTraceTimestamp {
                    clock_domain,
                    monotonic_ns: 101 + u64::from(stage.ordinal()),
                    wall_time_unix_ns: None,
                },
            },
            InteractionTraceCorrelation::Uncorrelated,
            InteractionTraceCounters::default(),
            InteractionTraceCounterUnavailability::all_available(),
            InteractionTraceGenerations {
                terminal_generation: Some(1),
                snapshot_generation: Some(2),
                frame_generation: display_completion.then_some(3),
            },
            if display_completion {
                InteractionTraceObservationBoundary::DisplayPresented
            } else {
                InteractionTraceObservationBoundary::InternalState
            },
            None,
        )
        .expect("test event must satisfy intrinsic invariants")
    }

    fn admitted_local(
        recorder: &FlightRecorder,
        producer: &ProducerHandle,
        path: InteractionTracePath,
    ) -> TraceToken {
        match recorder.admit_local_trace(producer, path) {
            TraceAdmission::Admitted { token, .. } => token,
            other => panic!("expected admitted trace, got {other:?}"),
        }
    }

    #[test]
    fn raw_event_is_private_copy_numeric_and_within_frozen_size_bound() {
        assert_impl_all!(RawInteractionEvent: Copy, Send, Sync);
        assert!(size_of::<RawInteractionEvent>() <= usize::from(MAX_RAW_EVENT_BYTES));
        assert!(!std::any::type_name::<RawInteractionEvent>().contains("String"));
        assert!(!std::any::type_name::<RawInteractionEvent>().contains("Vec"));
    }

    #[test]
    fn producer_registration_is_explicit_and_thread_bound() {
        assert_not_impl_any!(ProducerHandle: Send, Sync, Clone);
        let recorder = FlightRecorder::new(config(1, 2)).expect("recorder allocation must work");
        let producer = recorder
            .register_producer(0)
            .expect("first producer claims the shard");
        assert_eq!(producer.shard_index(), 0);
        assert!(matches!(
            recorder.register_producer(0),
            Err(RecorderError::ShardAlreadyClaimed { shard_index: 0 })
        ));
        drop(producer);
        assert!(recorder.register_producer(0).is_ok());
    }

    #[test]
    fn configuration_rejects_invalid_memory_before_recorder_allocation() {
        let error = RecorderConfig::new(
            epoch(1),
            run(2),
            RecorderMode::Certification,
            RecorderSamplerConfigV1::certification(),
            2,
            1,
            TEST_BYTE_CEILING,
        )
        .expect_err("more shards than slots must fail");
        assert!(matches!(error, RecorderError::Contract(_)));

        let error = RecorderConfig::new(
            epoch(1),
            run(2),
            RecorderMode::Certification,
            RecorderSamplerConfigV1::certification(),
            1,
            1,
            1,
        )
        .expect_err("byte ceiling below fixed workspaces must fail");
        assert!(matches!(error, RecorderError::Contract(_)));

        assert!(matches!(
            reject_allocator_over_reservation("planted allocator", 8, 9),
            Err(RecorderError::AllocatorOverReserved {
                component: "planted allocator",
                requested: 8,
                actual: 9,
            })
        ));
    }

    #[test]
    fn global_capacity_is_fixed_and_not_multiplied_per_shard() {
        let one = FlightRecorder::new(config(1, 64)).expect("one-shard recorder must allocate");
        let eight = FlightRecorder::new(config(8, 64)).expect("eight-shard recorder must allocate");
        assert_eq!(one.global_queue_capacity(), 64);
        assert_eq!(eight.global_queue_capacity(), 64);
        assert_eq!(one.workspace_capacities(), eight.workspace_capacities());
        assert_eq!(
            one.workspace_capacities().1,
            usize::from(CONVERSION_WORKSPACE_EVENTS)
        );
        assert_eq!(one.config().capacity().total_slots, 64);
        assert_eq!(eight.config().capacity().total_slots, 64);
        assert!(
            one.config()
                .capacity()
                .checked_reserved_bytes()
                .expect("checked")
                <= TEST_BYTE_CEILING
        );
        assert!(
            eight
                .config()
                .capacity()
                .checked_reserved_bytes()
                .expect("checked")
                <= TEST_BYTE_CEILING
        );
        assert!(
            usize::try_from(one.config().capacity().shard_metadata_bytes_per_shard)
                .expect("metadata reservation fits usize")
                >= size_of::<FlightRecorder>() + size_of::<RecorderWorkspace>()
        );
    }

    #[test]
    fn splitmix_golden_vectors_drive_one_whole_trace_decision() {
        let sampler = RecorderSamplerConfigV1 {
            algorithm: frankenterm_core_audit_types::interaction_flight_recorder_v1::RecorderSamplerAlgorithm::SplitMix64V1,
            numerator: 1,
            denominator: 2,
            seed_hi: 0,
            seed_lo: 0,
        };
        let local_run_id = InteractionTraceRunId::new(1, 2).expect("golden run id is valid");
        let first_trace = InteractionTraceId::new(local_run_id, 1).expect("golden trace is valid");
        let second_trace = InteractionTraceId::new(local_run_id, 2).expect("golden trace is valid");
        assert_eq!(sampler.hash(first_trace), Ok(0xa10a_fed3_c9e0_bd73));
        assert_eq!(sampler.hash(second_trace), Ok(0x9e75_9c08_0cb9_c871));
        assert_eq!(sampler.samples(first_trace), Ok(false));
        assert_eq!(sampler.samples(second_trace), Ok(false));

        let recorder = FlightRecorder::new(
            RecorderConfig::new(
                epoch(9),
                local_run_id,
                RecorderMode::Low,
                sampler,
                1,
                2,
                TEST_BYTE_CEILING,
            )
            .expect("golden recorder config is valid"),
        )
        .expect("golden recorder allocates");
        let producer = recorder.register_producer(0).expect("producer registers");
        for _ in 0..2 {
            assert!(matches!(
                recorder.admit_local_trace(&producer, InteractionTracePath::Keypress),
                TraceAdmission::SampledOut { .. }
            ));
        }
        let accounting = recorder.accounting_snapshot();
        assert_eq!(accounting.trace.sampled_in, 0);
        assert_eq!(accounting.trace.sampled_out, 2);
        assert_eq!(accounting.trace.checked_enabled_trace_attempts(), Ok(2));
        assert_eq!(recorder.next_trace_sequence.load(Ordering::Acquire), 3);
    }

    #[test]
    fn off_mode_admission_and_record_have_zero_recorder_side_effects() {
        let off = RecorderConfig::new(
            epoch(3),
            run(4),
            RecorderMode::Off,
            RecorderSamplerConfigV1::off(),
            1,
            1,
            TEST_BYTE_CEILING,
        )
        .expect("canonical off config must be valid");
        let recorder = FlightRecorder::new(off).expect("off recorder must construct");
        let producer = recorder
            .register_producer(999)
            .expect("off registration is a no-op handle");
        let before_closing = recorder.closing.load(Ordering::Relaxed);
        let before_sequence = recorder.next_trace_sequence.load(Ordering::Relaxed);
        let before_accounting = recorder.accounting_snapshot();
        assert_eq!(
            recorder.admit_local_trace(&producer, InteractionTracePath::Keypress),
            TraceAdmission::Off
        );
        let token = TraceToken {
            local_epoch_id: off.epoch_id(),
            context: SampledTraceContextV1 {
                schema_version: SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION,
                trace_id: InteractionTraceId::new(off.local_run_id(), 1).expect("test id is valid"),
                path: InteractionTracePath::Keypress,
                origin_recorder_epoch_id: off.epoch_id(),
                sampler_algorithm: off.sampler().algorithm,
            },
        };
        assert_eq!(
            recorder.record(
                &producer,
                token,
                &fields_for(1, stage(InteractionTracePath::Keypress, 0))
            ),
            RecordOutcome::Off
        );
        assert_eq!(recorder.closing.load(Ordering::Relaxed), before_closing);
        assert_eq!(
            recorder.next_trace_sequence.load(Ordering::Relaxed),
            before_sequence
        );
        assert_eq!(recorder.accounting_snapshot(), before_accounting);
        assert_eq!(recorder.queued_events(), 0);
        assert_eq!(recorder.workspace_capacities(), (0, 0, 0));
    }

    #[test]
    fn capacities_one_and_two_never_overwrite() {
        for capacity in [1_u32, 2] {
            let recorder = FlightRecorder::new(config(1, capacity)).expect("recorder allocates");
            let producer = recorder.register_producer(0).expect("producer registers");
            let token = admitted_local(&recorder, &producer, InteractionTracePath::Keypress);
            let event = fields_for(1, stage(InteractionTracePath::Keypress, 0));
            for _ in 0..capacity {
                assert!(matches!(
                    recorder.record(&producer, token, &event),
                    RecordOutcome::Recorded { .. }
                ));
            }
            assert!(matches!(
                recorder.record(&producer, token, &event),
                RecordOutcome::QueueFull { .. }
            ));
            assert_eq!(
                recorder.queued_events(),
                usize::try_from(capacity).expect("small capacity fits usize")
            );
            let frozen = recorder.try_freeze().expect("quiescent close freezes");
            assert_eq!(
                frozen.len(),
                usize::try_from(capacity).expect("small capacity fits usize")
            );
            assert_eq!(frozen.accounting().event.recorded, u64::from(capacity));
            assert_eq!(frozen.accounting().event.queue_full, 1);
        }
    }

    #[test]
    fn raw_event_roundtrips_semantically_without_layout_transmutation() {
        let recorder = FlightRecorder::new(config(1, 1)).expect("recorder allocates");
        let producer = recorder.register_producer(0).expect("producer registers");
        let token = admitted_local(&recorder, &producer, InteractionTracePath::Keypress);
        let fields = fields_for(9, stage(InteractionTracePath::Keypress, 0));
        assert!(matches!(
            recorder.record(&producer, token, &fields),
            RecordOutcome::Recorded { .. }
        ));
        let frozen = recorder.try_freeze().expect("freeze succeeds");
        let mut exported = Vec::with_capacity(1);
        assert_eq!(
            frozen.export_into(&mut exported),
            ExportOutcome::Completed { exported_events: 1 }
        );
        assert_eq!(exported[0].trace_id, token.trace_id());
        assert_eq!(exported[0].stage, fields.stage);
        assert_eq!(exported[0].producer, fields.producer);
        assert_eq!(exported[0].topology, fields.topology);
        assert_eq!(exported[0].sampling_loss.overwritten_events, 0);
        let trace = InteractionTraceV2 {
            schema_version: INTERACTION_TRACE_V2_SCHEMA_VERSION.to_owned(),
            trace_id: token.trace_id(),
            path: token.path(),
            events: exported,
        };
        assert_eq!(trace.validate_structure(), Ok(()));
    }

    #[test]
    fn export_capacity_failure_is_zero_mutation() {
        let recorder = FlightRecorder::new(config(1, 1)).expect("recorder allocates");
        let producer = recorder.register_producer(0).expect("producer registers");
        let token = admitted_local(&recorder, &producer, InteractionTracePath::Keypress);
        assert!(matches!(
            recorder.record(
                &producer,
                token,
                &fields_for(1, stage(InteractionTracePath::Keypress, 0))
            ),
            RecordOutcome::Recorded { .. }
        ));
        let frozen = recorder.try_freeze().expect("freeze succeeds");
        let mut destination = Vec::new();
        assert_eq!(
            frozen.export_into(&mut destination),
            ExportOutcome::DestinationCapacityInsufficient {
                required: 1,
                available: 0,
            }
        );
        assert!(destination.is_empty());
    }

    #[test]
    fn remote_trace_preserves_origin_run_and_origin_epoch() {
        let recorder = FlightRecorder::new(config(1, 2)).expect("recorder allocates");
        let producer = recorder.register_producer(0).expect("producer registers");
        let remote_epoch = epoch(99);
        let remote_id = InteractionTraceId::new(run(100), 7).expect("remote id is valid");
        let context = SampledTraceContextV1 {
            schema_version: SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION,
            trace_id: remote_id,
            path: InteractionTracePath::Keypress,
            origin_recorder_epoch_id: remote_epoch,
            sampler_algorithm: recorder.config().sampler().algorithm,
        };
        let token = match recorder.admit_remote_trace(&producer, context) {
            TraceAdmission::Admitted { token, .. } => token,
            other => panic!("expected admitted remote trace, got {other:?}"),
        };
        assert_eq!(token.trace_id(), remote_id);
        assert_eq!(
            token.sampled_context().origin_recorder_epoch_id,
            remote_epoch
        );
        assert_eq!(token.local_epoch_id(), recorder.config().epoch_id());
        assert_ne!(token.trace_id().run_id, recorder.config().local_run_id());
    }

    #[test]
    fn token_and_clock_rejections_use_frozen_accounting_domains() {
        let first = FlightRecorder::new(config(1, 4)).expect("first recorder allocates");
        let second_config = RecorderConfig::new(
            epoch(8),
            run(9),
            RecorderMode::Certification,
            RecorderSamplerConfigV1::certification(),
            1,
            4,
            TEST_BYTE_CEILING,
        )
        .expect("second config is valid");
        let second = FlightRecorder::new(second_config).expect("second recorder allocates");
        let first_producer = first
            .register_producer(0)
            .expect("first producer registers");
        let second_producer = second
            .register_producer(0)
            .expect("second producer registers");
        let first_token = admitted_local(&first, &first_producer, InteractionTracePath::Keypress);
        let event = fields_for(1, stage(InteractionTracePath::Keypress, 0));
        assert!(matches!(
            second.record(&second_producer, first_token, &event),
            RecordOutcome::EpochMismatch { .. }
        ));

        let second_token =
            admitted_local(&second, &second_producer, InteractionTracePath::Keypress);
        let mut invalid_clock = event;
        invalid_clock.clock.completed_at.clock_domain.clock_id = 999;
        assert!(matches!(
            second.record(&second_producer, second_token, &invalid_clock),
            RecordOutcome::ClockInvalid { .. }
        ));
        let snapshot = second.accounting_snapshot();
        assert_eq!(snapshot.event.epoch_mismatch, 1);
        assert_eq!(snapshot.event.clock_invalid, 1);
        assert_eq!(snapshot.event.checked_sampled_event_attempts(), Ok(2));
    }

    #[test]
    fn every_enabled_accounting_outcome_obeys_the_frozen_equations() {
        let recorder = FlightRecorder::new(config(1, 1)).expect("recorder allocates");
        let producer = recorder.register_producer(0).expect("producer registers");
        let token = admitted_local(&recorder, &producer, InteractionTracePath::Keypress);
        recorder
            .next_trace_sequence
            .store(u64::MAX, Ordering::Release);
        assert!(matches!(
            recorder.admit_local_trace(&producer, InteractionTracePath::Keypress),
            TraceAdmission::TraceIdExhausted { .. }
        ));

        let event = fields_for(1, stage(InteractionTracePath::Keypress, 0));
        assert!(matches!(
            recorder.record(&producer, token, &event),
            RecordOutcome::Recorded { .. }
        ));
        assert!(matches!(
            recorder.record(&producer, token, &event),
            RecordOutcome::QueueFull { .. }
        ));

        let mut invalid_clock = event;
        invalid_clock.clock.completed_at.clock_domain.host_id += 1;
        assert!(matches!(
            recorder.record(&producer, token, &invalid_clock),
            RecordOutcome::ClockInvalid { .. }
        ));
        let wrong_path = fields_for(1, stage(InteractionTracePath::ResizeZoom, 0));
        assert!(matches!(
            recorder.record(&producer, token, &wrong_path),
            RecordOutcome::EpochMismatch { .. }
        ));

        let before_close = recorder.accounting_snapshot();
        assert_eq!(before_close.trace.sampled_in, 1);
        assert_eq!(before_close.trace.sampled_out, 0);
        assert_eq!(before_close.trace.trace_id_exhausted, 1);
        assert_eq!(before_close.trace.checked_enabled_trace_attempts(), Ok(2));
        assert_eq!(before_close.event.recorded, 1);
        assert_eq!(before_close.event.queue_full, 1);
        assert_eq!(before_close.event.closing, 0);
        assert_eq!(before_close.event.clock_invalid, 1);
        assert_eq!(before_close.event.epoch_mismatch, 1);
        assert_eq!(before_close.event.checked_sampled_event_attempts(), Ok(4));
        assert_eq!(before_close.authority, RecorderAccountingAuthority::Exact);

        assert_eq!(recorder.begin_close(), CloseOutcome::Ready);
        assert_eq!(
            recorder.record(&producer, token, &event),
            RecordOutcome::Closing
        );
        assert_eq!(recorder.accounting_snapshot(), before_close);
    }

    #[test]
    fn trace_id_and_counter_exhaustion_are_nonwrapping_and_sticky() {
        let recorder = FlightRecorder::new(config(1, 4)).expect("recorder allocates");
        let producer = recorder.register_producer(0).expect("producer registers");
        recorder
            .next_trace_sequence
            .store(u64::MAX - 1, Ordering::Release);
        let final_token = admitted_local(&recorder, &producer, InteractionTracePath::Keypress);
        assert_eq!(final_token.trace_id().sequence, u64::MAX - 1);
        assert!(matches!(
            recorder.admit_local_trace(&producer, InteractionTracePath::Keypress),
            TraceAdmission::TraceIdExhausted { .. }
        ));
        assert_eq!(
            recorder.next_trace_sequence.load(Ordering::Acquire),
            u64::MAX
        );

        recorder.shards[0]
            .counters
            .recorded
            .store(u64::MAX - 2, Ordering::Release);
        let event = fields_for(1, stage(InteractionTracePath::Keypress, 0));
        assert_eq!(
            recorder.record(&producer, final_token, &event),
            RecordOutcome::Recorded {
                accounting_authority: RecorderAccountingAuthority::Exact,
            }
        );
        assert_eq!(
            recorder.record(&producer, final_token, &event),
            RecordOutcome::Recorded {
                accounting_authority: RecorderAccountingAuthority::Exhausted,
            }
        );
        assert_eq!(
            recorder.record(&producer, final_token, &event),
            RecordOutcome::Recorded {
                accounting_authority: RecorderAccountingAuthority::Exhausted,
            }
        );
        assert_eq!(
            recorder.shards[0].counters.recorded.load(Ordering::Acquire),
            COUNTER_EXHAUSTED
        );
        assert_eq!(
            recorder.accounting_snapshot().authority,
            RecorderAccountingAuthority::Exhausted
        );
    }

    #[test]
    fn aggregate_counter_overflow_is_sticky_authority_loss() {
        let recorder = FlightRecorder::new(config(2, 2)).expect("recorder allocates");
        let first = recorder
            .register_producer(0)
            .expect("first producer registers");
        let _second = recorder
            .register_producer(1)
            .expect("second producer registers");
        let per_shard = u64::MAX / 2 + 1;
        recorder.shards[0]
            .counters
            .sampled_in
            .store(per_shard, Ordering::Release);
        recorder.shards[1]
            .counters
            .sampled_in
            .store(per_shard, Ordering::Release);
        assert_eq!(
            recorder.accounting_snapshot().authority,
            RecorderAccountingAuthority::Exhausted
        );
        assert!(matches!(
            recorder.admit_local_trace(&first, InteractionTracePath::Keypress),
            TraceAdmission::Admitted {
                accounting_authority: RecorderAccountingAuthority::Exhausted,
                ..
            }
        ));
        assert_eq!(
            recorder.current_authority(),
            RecorderAccountingAuthority::Exhausted
        );
    }

    #[test]
    fn close_cut_waits_for_admitted_operations_and_rejects_post_cut_work() {
        let recorder = FlightRecorder::new(config(1, 2)).expect("recorder allocates");
        let producer = recorder.register_producer(0).expect("producer registers");
        let token = admitted_local(&recorder, &producer, InteractionTracePath::Keypress);
        let guard = recorder
            .try_enter(&recorder.shards[0])
            .expect("test operation enters before close");
        assert_eq!(
            recorder.begin_close(),
            CloseOutcome::Draining {
                in_flight_operations: 1,
            }
        );
        assert_eq!(
            recorder.record(
                &producer,
                token,
                &fields_for(1, stage(InteractionTracePath::Keypress, 0))
            ),
            RecordOutcome::Closing
        );
        assert!(matches!(
            recorder.try_freeze(),
            Err(CloseOutcome::Draining {
                in_flight_operations: 1,
            })
        ));
        drop(guard);
        let frozen = recorder.try_freeze().expect("freeze succeeds after drain");
        assert!(frozen.is_empty());
        assert_eq!(frozen.accounting().event.closing, 0);
        assert!(matches!(
            recorder.try_freeze(),
            Err(CloseOutcome::AlreadyFrozen)
        ));
    }

    #[test]
    fn concurrent_close_cut_waits_for_pre_cut_work_and_excludes_post_cut_attempts() {
        let recorder = FlightRecorder::new(config(1, 2)).expect("recorder allocates");
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker_recorder = Arc::clone(&recorder);
        let worker_entered = Arc::clone(&entered);
        let worker_release = Arc::clone(&release);
        let worker = thread::spawn(move || {
            let producer = worker_recorder
                .register_producer(0)
                .expect("worker claims its shard");
            let token = admitted_local(&worker_recorder, &producer, InteractionTracePath::Keypress);
            let guard = worker_recorder
                .try_enter(&worker_recorder.shards[0])
                .expect("worker operation enters before close");
            worker_entered.wait();
            worker_release.wait();
            drop(guard);
            worker_recorder.record(
                &producer,
                token,
                &fields_for(1, stage(InteractionTracePath::Keypress, 0)),
            )
        });

        entered.wait();
        assert_eq!(
            recorder.begin_close(),
            CloseOutcome::Draining {
                in_flight_operations: 1,
            }
        );
        release.wait();
        assert_eq!(
            worker.join().expect("worker exits without panic"),
            RecordOutcome::Closing
        );
        let frozen = recorder.try_freeze().expect("quiescent recorder freezes");
        assert!(frozen.is_empty());
        assert_eq!(frozen.accounting().trace.sampled_in, 1);
        assert_eq!(frozen.accounting().event.closing, 0);
        assert_eq!(
            frozen.accounting().event.checked_sampled_event_attempts(),
            Ok(0)
        );
    }

    #[test]
    fn concurrent_producers_fill_bounded_shards_without_overwrite() {
        const SHARDS: usize = 4;
        const ATTEMPTS_PER_SHARD: usize = 20;
        let recorder = FlightRecorder::new(config(
            u16::try_from(SHARDS).expect("test shard count fits u16"),
            64,
        ))
        .expect("concurrent recorder allocates");
        let barrier = Arc::new(Barrier::new(SHARDS));
        let mut threads = Vec::new();
        for shard_index in 0..SHARDS {
            let recorder = Arc::clone(&recorder);
            let barrier = Arc::clone(&barrier);
            threads.push(thread::spawn(move || {
                let producer = recorder
                    .register_producer(shard_index)
                    .expect("thread claims its exact shard");
                let token = admitted_local(&recorder, &producer, InteractionTracePath::Keypress);
                let event = fields_for(
                    u64::try_from(shard_index).expect("test shard index fits u64") + 1,
                    stage(InteractionTracePath::Keypress, 0),
                );
                barrier.wait();
                let mut recorded_count = 0;
                let mut full_count = 0;
                for _ in 0..ATTEMPTS_PER_SHARD {
                    match recorder.record(&producer, token, &event) {
                        RecordOutcome::Recorded { .. } => recorded_count += 1,
                        RecordOutcome::QueueFull { .. } => full_count += 1,
                        other => panic!("unexpected concurrent outcome: {other:?}"),
                    }
                }
                (recorded_count, full_count)
            }));
        }
        let mut total_recorded = 0;
        let mut total_full = 0;
        for handle in threads {
            let (thread_recorded, thread_full) =
                handle.join().expect("producer thread must not panic");
            total_recorded += thread_recorded;
            total_full += thread_full;
        }
        assert_eq!(total_recorded, 64);
        assert_eq!(total_full, SHARDS * ATTEMPTS_PER_SHARD - 64);
        assert_eq!(recorder.queued_events(), 64);
        let frozen = recorder.try_freeze().expect("concurrent batch freezes");
        assert_eq!(frozen.len(), 64);
        assert_eq!(frozen.accounting().event.recorded, 64);
        assert_eq!(
            frozen.accounting().event.queue_full,
            u64::try_from(total_full).expect("test attempt count fits u64")
        );
        assert_eq!(
            frozen.accounting().authority,
            RecorderAccountingAuthority::Exact
        );
    }

    #[test]
    fn record_api_has_no_callback_parameter_and_privacy_shape_is_content_free() {
        let record_api: fn(
            &FlightRecorder,
            &ProducerHandle,
            TraceToken,
            &EventFields,
        ) -> RecordOutcome = FlightRecorder::record;
        std::hint::black_box(record_api);

        let recorder = FlightRecorder::new(config(1, 1)).expect("recorder allocates");
        let producer = recorder.register_producer(0).expect("producer registers");
        let token = admitted_local(&recorder, &producer, InteractionTracePath::Keypress);
        recorder.record(
            &producer,
            token,
            &fields_for(1, stage(InteractionTracePath::Keypress, 0)),
        );
        let frozen = recorder.try_freeze().expect("freeze succeeds");
        let mut events = Vec::with_capacity(1);
        assert!(matches!(
            frozen.export_into(&mut events),
            ExportOutcome::Completed { .. }
        ));
        let json = serde_json::to_string(&events[0]).expect("DTO serializes");
        for forbidden_key in [
            "\"key\":",
            "\"pane_text\":",
            "\"title\":",
            "\"command\":",
            "\"cwd\":",
            "\"hostname\":",
            "\"reason\":",
        ] {
            assert!(!json.contains(forbidden_key), "found {forbidden_key}");
        }
        assert!(!json.contains("sk_live_planted_privacy_negative"));
    }

    proptest! {
        #[test]
        fn whole_trace_sampling_is_deterministic_and_never_event_local(
            run_hi in 1_u64..u64::MAX,
            run_lo in any::<u64>(),
            sequence in 1_u64..u64::MAX,
            seed_hi in any::<u64>(),
            seed_lo in any::<u64>(),
            denominator in 1_u64..=10_000,
        ) {
            let run_id = InteractionTraceRunId::new(run_hi, run_lo)
                .expect("run_hi keeps the generated run nonzero");
            let trace_id = InteractionTraceId::new(run_id, sequence)
                .expect("generated sequence excludes reserved endpoints");
            let sampler = RecorderSamplerConfigV1 {
                algorithm: frankenterm_core_audit_types::interaction_flight_recorder_v1::RecorderSamplerAlgorithm::SplitMix64V1,
                numerator: denominator / 2,
                denominator,
                seed_hi,
                seed_lo,
            };
            let first = sampler.samples(trace_id).expect("generated sampler is valid");
            for _event_ordinal in 0..InteractionTraceStage::stage_count(InteractionTracePath::Keypress) {
                prop_assert_eq!(sampler.samples(trace_id), Ok(first));
            }
        }
    }

    #[test]
    fn invalid_raw_decode_error_is_typed() {
        let recorder = FlightRecorder::new(config(1, 1)).expect("recorder allocates");
        let producer = recorder.register_producer(0).expect("producer registers");
        let token = admitted_local(&recorder, &producer, InteractionTracePath::Keypress);
        let fields = fields_for(1, stage(InteractionTracePath::Keypress, 0));
        let mut raw = RawInteractionEvent::encode(token, &fields, 0);
        raw.path = u8::MAX;
        assert!(matches!(raw.decode(), Err(RecorderError::InvalidRawEvent)));
    }
}
