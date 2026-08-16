//! Portable, content-free interaction trace v2 contract.
//!
//! This module freezes identity, clock, causality, privacy, and metric
//! semantics for production keypress and resize/zoom evidence.  It is a DTO
//! and validation layer only: it does not claim that any producer is wired,
//! that a display callback measures photons, or that clocks on different
//! hosts can be subtracted.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::renderer_scenario_catalog::{RendererKeypressTraceStage, RendererResizeTraceStage};

/// Exact wire schema accepted by this implementation.
pub const INTERACTION_TRACE_V2_SCHEMA_VERSION: &str = "ft.interaction-trace.v2";
/// A trace is deliberately small enough for bounded validation and replay.
pub const MAX_INTERACTION_TRACE_EVENTS: usize = 256;
/// One retained run may contain at most this many independently identified traces.
pub const MAX_INTERACTION_TRACES_PER_RUN: usize = 65_536;
/// Maximum JSON document accepted by the single-trace decoder.
///
/// This bound is checked before Serde can allocate the event vector. It is
/// deliberately larger than the canonical encoding of the maximum event
/// inventory so future numeric fields do not silently invalidate old readers.
pub const MAX_INTERACTION_TRACE_JSON_BYTES: usize = 2 * 1024 * 1024;
/// Maximum JSON document accepted by the aggregate-run decoder.
///
/// Large recorder exports should normally use the streaming JSONL surface.
/// This finite ceiling prevents the convenience run envelope from becoming an
/// unbounded allocation path.
pub const MAX_INTERACTION_TRACE_RUN_JSON_BYTES: usize = 64 * 1024 * 1024;

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
///
/// The allocator deliberately implements neither `Clone` nor `Copy`: duplicating
/// its cursor would permit two owners to issue the same supposedly unique trace
/// ID.
///
/// ```compile_fail
/// use frankenterm_core_audit_types::interaction_trace_v2::InteractionTraceIdAllocator;
///
/// fn require_clone<T: Clone>() {}
/// require_clone::<InteractionTraceIdAllocator>();
/// ```
#[derive(Debug, PartialEq, Eq)]
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

/// What happened at one frozen stage slot.
///
/// Resize/zoom paths contain conditional work: an intent can be a proven
/// no-op, superseded, or can avoid spawning a worker.  Recording one of those
/// outcomes explicitly preserves the closed R0-R25 inventory without
/// pretending that work ran.  The current qualification contract remains
/// conservative: only performed stages qualify until the scenario catalog
/// freezes a stage-specific optionality map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionTraceStageOutcome {
    Performed,
    NoOp,
    NotApplicable,
    Superseded,
    Cancelled,
    Failed,
}

impl InteractionTraceStageOutcome {
    #[must_use]
    pub const fn is_qualifying(self) -> bool {
        matches!(self, Self::Performed)
    }
}

impl InteractionTraceStage {
    #[must_use]
    pub const fn path(self) -> InteractionTracePath {
        match self {
            Self::Keypress(_) => InteractionTracePath::Keypress,
            Self::ResizeZoom(_) => InteractionTracePath::ResizeZoom,
        }
    }

    /// Zero-based position in the frozen K0-K13 or R0-R25 inventory.
    ///
    /// This is the allocation-free stage identity frozen for flight-recorder
    /// implementations.  Its value follows the corresponding `ALL` array;
    /// changing either order is a schema change.
    #[must_use]
    pub const fn ordinal(self) -> u8 {
        match self {
            Self::Keypress(stage) => match stage {
                RendererKeypressTraceStage::KeyAppkitReceipt => 0,
                RendererKeypressTraceStage::GuiKeyMappingComplete => 1,
                RendererKeypressTraceStage::ClientRpcEnqueue => 2,
                RendererKeypressTraceStage::ClientEncodeSocketFlush => 3,
                RendererKeypressTraceStage::ServerReadableDecode => 4,
                RendererKeypressTraceStage::ServerDispatchMuxWait => 5,
                RendererKeypressTraceStage::TerminalLockPtyWriteFlush => 6,
                RendererKeypressTraceStage::PtyEchoParserApply => 7,
                RendererKeypressTraceStage::ServerDeltaCompute => 8,
                RendererKeypressTraceStage::ClientReceiveDecodeApply => 9,
                RendererKeypressTraceStage::LocalMuxGuiInvalidation => 10,
                RendererKeypressTraceStage::PaintShapeAtlas => 11,
                RendererKeypressTraceStage::GpuSubmitDrawableRequest => 12,
                RendererKeypressTraceStage::DisplayCompletion => 13,
            },
            Self::ResizeZoom(stage) => match stage {
                RendererResizeTraceStage::NativeEventReceipt => 0,
                RendererResizeTraceStage::GuiReturn => 1,
                RendererResizeTraceStage::IntentEnqueue => 2,
                RendererResizeTraceStage::MuxResizeDispatch => 3,
                RendererResizeTraceStage::PaneResizeApply => 4,
                RendererResizeTraceStage::IntentSupersession => 5,
                RendererResizeTraceStage::WorkerCreate => 6,
                RendererResizeTraceStage::WorkerStart => 7,
                RendererResizeTraceStage::TerminalLockWait => 8,
                RendererResizeTraceStage::TerminalLockHold => 9,
                RendererResizeTraceStage::ViewportReflow => 10,
                RendererResizeTraceStage::NearReflow => 11,
                RendererResizeTraceStage::ColdReflow => 12,
                RendererResizeTraceStage::FirstCoherentViewport => 13,
                RendererResizeTraceStage::WorkerJoin => 14,
                RendererResizeTraceStage::GuiInvalidation => 15,
                RendererResizeTraceStage::Paint => 16,
                RendererResizeTraceStage::TextShaping => 17,
                RendererResizeTraceStage::GlyphRaster => 18,
                RendererResizeTraceStage::GlyphAtlas => 19,
                RendererResizeTraceStage::LineQuadReuseRebuild => 20,
                RendererResizeTraceStage::GpuBind => 21,
                RendererResizeTraceStage::GpuUpload => 22,
                RendererResizeTraceStage::GpuSubmit => 23,
                RendererResizeTraceStage::DrawablePresentRequest => 24,
                RendererResizeTraceStage::DisplayCompletion => 25,
            },
        }
    }

    /// Number of stage slots in the frozen inventory for `path`.
    #[must_use]
    pub const fn stage_count(path: InteractionTracePath) -> u8 {
        match path {
            InteractionTracePath::Keypress => RendererKeypressTraceStage::ALL.len() as u8,
            InteractionTracePath::ResizeZoom => RendererResizeTraceStage::ALL.len() as u8,
        }
    }

    /// Resolve a zero-based frozen stage ordinal without allocating.
    #[must_use]
    pub const fn from_ordinal(path: InteractionTracePath, ordinal: u8) -> Option<Self> {
        if ordinal >= Self::stage_count(path) {
            return None;
        }

        match path {
            InteractionTracePath::Keypress => Some(Self::Keypress(
                RendererKeypressTraceStage::ALL[ordinal as usize],
            )),
            InteractionTracePath::ResizeZoom => Some(Self::ResizeZoom(
                RendererResizeTraceStage::ALL[ordinal as usize],
            )),
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
/// means observed zero unless the corresponding field in
/// [`InteractionTraceCounterUnavailability`] is `true`.  Producers unable to
/// observe a metric must declare it unavailable rather than inventing a value.
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

impl InteractionTraceCounters {
    #[must_use]
    pub const fn value(self, field: InteractionTraceCounterField) -> u64 {
        match field {
            InteractionTraceCounterField::QueueDepth => self.queue_depth,
            InteractionTraceCounterField::OldestQueueAgeNs => self.oldest_queue_age_ns,
            InteractionTraceCounterField::WorkUnits => self.work_units,
            InteractionTraceCounterField::Bytes => self.bytes,
            InteractionTraceCounterField::Rows => self.rows,
            InteractionTraceCounterField::AllocationCount => self.allocation_count,
            InteractionTraceCounterField::AllocatedBytes => self.allocated_bytes,
            InteractionTraceCounterField::CopyCount => self.copy_count,
            InteractionTraceCounterField::CopiedBytes => self.copied_bytes,
            InteractionTraceCounterField::RpcCount => self.rpc_count,
            InteractionTraceCounterField::DeltaCount => self.delta_count,
            InteractionTraceCounterField::DirtyRows => self.dirty_rows,
            InteractionTraceCounterField::FullViewportClones => self.full_viewport_clones,
            InteractionTraceCounterField::CursorRowDuplicates => self.cursor_row_duplicates,
            InteractionTraceCounterField::PaintCount => self.paint_count,
            InteractionTraceCounterField::FrameCount => self.frame_count,
        }
    }
}

/// Closed names for the fixed counter fields above.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionTraceCounterField {
    QueueDepth,
    OldestQueueAgeNs,
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
}

impl InteractionTraceCounterField {
    pub const ALL: [Self; 16] = [
        Self::QueueDepth,
        Self::OldestQueueAgeNs,
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
    ];
}

/// Explicit, fixed-size counter observability for one event.  `false` means
/// the numeric value was observed, including observed zero; `true` means the
/// corresponding numeric field is only a zero placeholder.  The fixed shape
/// prevents hostile input from allocating an unbounded unavailability list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTraceCounterUnavailability {
    pub queue_depth: bool,
    pub oldest_queue_age_ns: bool,
    pub work_units: bool,
    pub bytes: bool,
    pub rows: bool,
    pub allocation_count: bool,
    pub allocated_bytes: bool,
    pub copy_count: bool,
    pub copied_bytes: bool,
    pub rpc_count: bool,
    pub delta_count: bool,
    pub dirty_rows: bool,
    pub full_viewport_clones: bool,
    pub cursor_row_duplicates: bool,
    pub paint_count: bool,
    pub frame_count: bool,
}

impl InteractionTraceCounterUnavailability {
    /// Explicitly assert that every counter was observed.  There is
    /// intentionally no `Default` implementation: a producer must not gain a
    /// qualifying all-observed claim by mechanically defaulting the DTO.
    #[must_use]
    pub const fn all_available() -> Self {
        Self {
            queue_depth: false,
            oldest_queue_age_ns: false,
            work_units: false,
            bytes: false,
            rows: false,
            allocation_count: false,
            allocated_bytes: false,
            copy_count: false,
            copied_bytes: false,
            rpc_count: false,
            delta_count: false,
            dirty_rows: false,
            full_viewport_clones: false,
            cursor_row_duplicates: false,
            paint_count: false,
            frame_count: false,
        }
    }

    #[must_use]
    pub const fn is_all_available(self) -> bool {
        !self.queue_depth
            && !self.oldest_queue_age_ns
            && !self.work_units
            && !self.bytes
            && !self.rows
            && !self.allocation_count
            && !self.allocated_bytes
            && !self.copy_count
            && !self.copied_bytes
            && !self.rpc_count
            && !self.delta_count
            && !self.dirty_rows
            && !self.full_viewport_clones
            && !self.cursor_row_duplicates
            && !self.paint_count
            && !self.frame_count
    }

    #[must_use]
    pub const fn is_unavailable(self, field: InteractionTraceCounterField) -> bool {
        match field {
            InteractionTraceCounterField::QueueDepth => self.queue_depth,
            InteractionTraceCounterField::OldestQueueAgeNs => self.oldest_queue_age_ns,
            InteractionTraceCounterField::WorkUnits => self.work_units,
            InteractionTraceCounterField::Bytes => self.bytes,
            InteractionTraceCounterField::Rows => self.rows,
            InteractionTraceCounterField::AllocationCount => self.allocation_count,
            InteractionTraceCounterField::AllocatedBytes => self.allocated_bytes,
            InteractionTraceCounterField::CopyCount => self.copy_count,
            InteractionTraceCounterField::CopiedBytes => self.copied_bytes,
            InteractionTraceCounterField::RpcCount => self.rpc_count,
            InteractionTraceCounterField::DeltaCount => self.delta_count,
            InteractionTraceCounterField::DirtyRows => self.dirty_rows,
            InteractionTraceCounterField::FullViewportClones => self.full_viewport_clones,
            InteractionTraceCounterField::CursorRowDuplicates => self.cursor_row_duplicates,
            InteractionTraceCounterField::PaintCount => self.paint_count,
            InteractionTraceCounterField::FrameCount => self.frame_count,
        }
    }
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
    pub stage_outcome: InteractionTraceStageOutcome,
    pub producer: InteractionTraceProducer,
    pub topology: InteractionTraceTopology,
    pub started_at: InteractionTraceTimestamp,
    pub completed_at: InteractionTraceTimestamp,
    pub correlation: InteractionTraceCorrelation,
    pub counters: InteractionTraceCounters,
    pub counter_unavailability: InteractionTraceCounterUnavailability,
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
    /// Decode one bounded, closed-shape trace document.
    ///
    /// The byte ceiling is enforced before Serde allocation. Semantic
    /// qualification remains explicit through [`Self::validate_structure`] or
    /// [`Self::validate_qualifying`].
    pub fn decode_json_bounded(raw: &[u8]) -> Result<Self, InteractionTraceDecodeError> {
        decode_json_bounded(raw, MAX_INTERACTION_TRACE_JSON_BYTES)
    }

    pub fn validate_structure(&self) -> Result<(), TraceContractError> {
        validate_interaction_trace_structure(
            &self.schema_version,
            self.trace_id,
            self.path,
            &self.events,
        )
    }

    /// Qualify a trace for its declared (still bounded) claim class.
    pub fn validate_qualifying(&self) -> Result<InteractionTraceClaimBoundary, TraceContractError> {
        self.validate_structure()?;
        let expected_count = usize::from(InteractionTraceStage::stage_count(self.path));
        if self.events.len() != expected_count {
            let missing_stage = u8::try_from(self.events.len())
                .ok()
                .and_then(|ordinal| InteractionTraceStage::from_ordinal(self.path, ordinal))
                .ok_or(TraceContractError::TooManyEvents {
                    actual: self.events.len(),
                    maximum: expected_count,
                })?;
            return Err(TraceContractError::MissingStage {
                stage: missing_stage,
            });
        }
        for event in &self.events {
            if !event.stage_outcome.is_qualifying() {
                return Err(TraceContractError::NonQualifyingStageOutcome {
                    stage: event.stage,
                    outcome: event.stage_outcome,
                });
            }
            if !event.counter_unavailability.is_all_available() {
                return Err(TraceContractError::CountersUnavailable {
                    event_ordinal: event.event_ordinal,
                });
            }
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

/// Validate a borrowed trace without transferring or cloning its bounded
/// event storage. Operational recorders use this surface so recoverable unwind
/// cannot strand or drop their pre-reserved conversion workspace.
pub fn validate_interaction_trace_structure(
    schema_version: &str,
    trace_id: InteractionTraceId,
    path: InteractionTracePath,
    events: &[InteractionTraceEventV2],
) -> Result<(), TraceContractError> {
    validate_schema(schema_version)?;
    validate_trace_id(trace_id)?;
    if events.is_empty() {
        return Err(TraceContractError::EmptyTrace);
    }
    if events.len() > MAX_INTERACTION_TRACE_EVENTS {
        return Err(TraceContractError::TooManyEvents {
            actual: events.len(),
            maximum: MAX_INTERACTION_TRACE_EVENTS,
        });
    }

    let mut trace_topology = None;

    for (index, event) in events.iter().enumerate() {
        validate_schema(&event.schema_version)?;
        if event.trace_id != trace_id {
            return Err(TraceContractError::EventTraceIdMismatch {
                expected: trace_id,
                actual: event.trace_id,
            });
        }
        let expected_ordinal =
            u64::try_from(index).map_err(|_| TraceContractError::TooManyEvents {
                actual: events.len(),
                maximum: MAX_INTERACTION_TRACE_EVENTS,
            })?;
        if event.event_ordinal != expected_ordinal {
            return Err(TraceContractError::EventOrdinalNotContiguous {
                expected: expected_ordinal,
                actual: event.event_ordinal,
            });
        }
        if event.stage.path() != path {
            return Err(TraceContractError::TracePathMismatch {
                expected: path,
                actual: event.stage.path(),
            });
        }
        if let Some(expected) = trace_topology {
            if event.topology != expected {
                return Err(TraceContractError::TraceTopologyChanged {
                    expected,
                    actual: event.topology,
                    event_ordinal: event.event_ordinal,
                });
            }
        } else {
            trace_topology = Some(event.topology);
        }
        let Some(expected_stage) = u8::try_from(index)
            .ok()
            .and_then(|ordinal| InteractionTraceStage::from_ordinal(path, ordinal))
        else {
            return Err(TraceContractError::UnexpectedStage { stage: event.stage });
        };
        if event.stage != expected_stage {
            if events[..index]
                .iter()
                .any(|prior| prior.stage == event.stage)
            {
                return Err(TraceContractError::DuplicateStage { stage: event.stage });
            }
            return Err(TraceContractError::StageOutOfOrder {
                expected: expected_stage,
                actual: event.stage,
            });
        }
        validate_event(event, |span_id| {
            events[..index].iter().any(|prior| prior.span_id == span_id)
        })?;
        if let Some(previous) = events[..index]
            .iter()
            .rev()
            .find(|prior| prior.started_at.clock_domain == event.started_at.clock_domain)
            && event.started_at.monotonic_ns < previous.started_at.monotonic_ns
        {
            return Err(TraceContractError::CrossEventClockRegression {
                clock_domain: event.started_at.clock_domain,
                previous_start_ns: previous.started_at.monotonic_ns,
                actual_start_ns: event.started_at.monotonic_ns,
                event_ordinal: event.event_ordinal,
            });
        }
    }
    Ok(())
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
    /// Decode one bounded, closed-shape aggregate run document.
    ///
    /// High-volume exports should prefer streaming JSONL; this convenience
    /// envelope is intentionally finite before allocation begins.
    pub fn decode_json_bounded(raw: &[u8]) -> Result<Self, InteractionTraceDecodeError> {
        decode_json_bounded(raw, MAX_INTERACTION_TRACE_RUN_JSON_BYTES)
    }

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

/// Stable bounded-decoder failure for interaction trace envelopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionTraceDecodeError {
    /// Raw bytes exceeded the ceiling before deserialization began.
    PayloadTooLarge {
        actual_bytes: usize,
        max_bytes: usize,
    },
    /// Serde rejected malformed JSON, a duplicate/unknown field, or a wrong type.
    InvalidJson,
    /// A valid first JSON value was followed by non-whitespace data.
    TrailingData,
}

impl fmt::Display for InteractionTraceDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge {
                actual_bytes,
                max_bytes,
            } => write!(
                formatter,
                "interaction trace document is {actual_bytes} bytes (maximum {max_bytes})"
            ),
            Self::InvalidJson => formatter.write_str("invalid interaction trace JSON"),
            Self::TrailingData => formatter.write_str("interaction trace JSON has trailing data"),
        }
    }
}

impl std::error::Error for InteractionTraceDecodeError {}

fn decode_json_bounded<T>(raw: &[u8], max_bytes: usize) -> Result<T, InteractionTraceDecodeError>
where
    T: for<'de> Deserialize<'de>,
{
    if raw.len() > max_bytes {
        return Err(InteractionTraceDecodeError::PayloadTooLarge {
            actual_bytes: raw.len(),
            max_bytes,
        });
    }

    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value =
        T::deserialize(&mut decoder).map_err(|_| InteractionTraceDecodeError::InvalidJson)?;
    decoder
        .end()
        .map_err(|_| InteractionTraceDecodeError::TrailingData)?;
    Ok(value)
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

fn validate_event<F>(
    event: &InteractionTraceEventV2,
    prior_span_exists: F,
) -> Result<(), TraceContractError>
where
    F: Fn(u64) -> bool,
{
    if event.span_id == 0 {
        return Err(TraceContractError::ReservedSpanId);
    }
    if prior_span_exists(event.span_id) {
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
        && !prior_span_exists(parent_span_id)
    {
        return Err(TraceContractError::UnknownParentSpan { parent_span_id });
    }

    validate_producer(event.producer, event.stage)?;
    validate_topology(event.topology)?;
    validate_clock(event.started_at.clock_domain, event.producer)?;
    validate_clock(event.completed_at.clock_domain, event.producer)?;
    event.duration_ns()?;
    validate_correlation(event.correlation)?;
    validate_stage_outcome(event)?;
    validate_counter_unavailability(event)?;
    validate_generations(event)?;
    validate_observation_boundary(event)?;
    Ok(())
}

fn validate_stage_outcome(event: &InteractionTraceEventV2) -> Result<(), TraceContractError> {
    if matches!(event.stage, InteractionTraceStage::Keypress(_))
        && event.stage_outcome == InteractionTraceStageOutcome::NotApplicable
    {
        return Err(TraceContractError::StageOutcomeInvalidForPath {
            stage: event.stage,
            outcome: event.stage_outcome,
        });
    }

    if matches!(
        event.stage_outcome,
        InteractionTraceStageOutcome::NoOp
            | InteractionTraceStageOutcome::NotApplicable
            | InteractionTraceStageOutcome::Superseded
    ) && event.started_at.monotonic_ns != event.completed_at.monotonic_ns
    {
        return Err(TraceContractError::InactiveStageHasDuration {
            stage: event.stage,
            outcome: event.stage_outcome,
        });
    }
    Ok(())
}

fn validate_counter_unavailability(
    event: &InteractionTraceEventV2,
) -> Result<(), TraceContractError> {
    for field in InteractionTraceCounterField::ALL {
        if event.counter_unavailability.is_unavailable(field) && event.counters.value(field) != 0 {
            return Err(TraceContractError::UnavailableCounterHasValue { field });
        }
    }
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
    TraceTopologyChanged {
        expected: InteractionTraceTopology,
        actual: InteractionTraceTopology,
        event_ordinal: u64,
    },
    StageOutcomeInvalidForPath {
        stage: InteractionTraceStage,
        outcome: InteractionTraceStageOutcome,
    },
    InactiveStageHasDuration {
        stage: InteractionTraceStage,
        outcome: InteractionTraceStageOutcome,
    },
    NonQualifyingStageOutcome {
        stage: InteractionTraceStage,
        outcome: InteractionTraceStageOutcome,
    },
    UnavailableCounterHasValue {
        field: InteractionTraceCounterField,
    },
    CountersUnavailable {
        event_ordinal: u64,
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
            stage_outcome: InteractionTraceStageOutcome::Performed,
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
            counter_unavailability: InteractionTraceCounterUnavailability::all_available(),
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
    fn frozen_stage_ordinals_round_trip_without_allocation() {
        assert_eq!(
            InteractionTraceStage::stage_count(InteractionTracePath::Keypress),
            14
        );
        assert_eq!(
            InteractionTraceStage::stage_count(InteractionTracePath::ResizeZoom),
            26
        );

        for (ordinal, stage) in RendererKeypressTraceStage::ALL.into_iter().enumerate() {
            let stage = InteractionTraceStage::Keypress(stage);
            assert_eq!(stage.ordinal(), ordinal as u8);
            assert_eq!(
                InteractionTraceStage::from_ordinal(InteractionTracePath::Keypress, ordinal as u8),
                Some(stage)
            );
        }
        for (ordinal, stage) in RendererResizeTraceStage::ALL.into_iter().enumerate() {
            let stage = InteractionTraceStage::ResizeZoom(stage);
            assert_eq!(stage.ordinal(), ordinal as u8);
            assert_eq!(
                InteractionTraceStage::from_ordinal(
                    InteractionTracePath::ResizeZoom,
                    ordinal as u8
                ),
                Some(stage)
            );
        }

        assert_eq!(
            InteractionTraceStage::from_ordinal(InteractionTracePath::Keypress, 14),
            None
        );
        assert_eq!(
            InteractionTraceStage::from_ordinal(InteractionTracePath::ResizeZoom, 26),
            None
        );
        assert_eq!(
            InteractionTraceStage::from_ordinal(InteractionTracePath::Keypress, u8::MAX),
            None
        );
        assert_eq!(
            InteractionTraceStage::from_ordinal(InteractionTracePath::ResizeZoom, u8::MAX),
            None
        );
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
    fn bounded_decoders_reject_oversize_trailing_and_unknown_input_before_authority() {
        let trace = keypress_trace(1);
        let encoded = serde_json::to_vec(&trace).expect("trace encodes");
        assert_eq!(
            InteractionTraceV2::decode_json_bounded(&encoded).expect("bounded trace decodes"),
            trace
        );

        let mut trailing = encoded.clone();
        trailing.extend_from_slice(b" null");
        assert!(matches!(
            InteractionTraceV2::decode_json_bounded(&trailing),
            Err(InteractionTraceDecodeError::TrailingData)
        ));

        let mut unknown = serde_json::to_value(&trace).expect("trace converts to JSON value");
        unknown
            .as_object_mut()
            .expect("trace JSON is an object")
            .insert("pane_text".to_owned(), serde_json::json!("secret"));
        assert!(matches!(
            InteractionTraceV2::decode_json_bounded(
                &serde_json::to_vec(&unknown).expect("unknown-field fixture encodes")
            ),
            Err(InteractionTraceDecodeError::InvalidJson)
        ));

        let oversized = vec![b' '; MAX_INTERACTION_TRACE_JSON_BYTES + 1];
        assert_eq!(
            InteractionTraceV2::decode_json_bounded(&oversized),
            Err(InteractionTraceDecodeError::PayloadTooLarge {
                actual_bytes: MAX_INTERACTION_TRACE_JSON_BYTES + 1,
                max_bytes: MAX_INTERACTION_TRACE_JSON_BYTES,
            })
        );

        let run = InteractionTraceRunV2 {
            schema_version: INTERACTION_TRACE_V2_SCHEMA_VERSION.to_owned(),
            run_id: run_id(),
            traces: vec![trace],
        };
        let encoded_run = serde_json::to_vec(&run).expect("run encodes");
        assert_eq!(
            InteractionTraceRunV2::decode_json_bounded(&encoded_run).expect("bounded run decodes"),
            run
        );
    }

    #[test]
    fn conditional_resize_stage_outcomes_are_explicit_and_fail_closed() {
        let mut trace = resize_trace(2);
        let worker_create = &mut trace.events[6];
        worker_create.stage_outcome = InteractionTraceStageOutcome::NotApplicable;
        worker_create.completed_at = worker_create.started_at;
        assert!(matches!(
            trace.validate_qualifying(),
            Err(TraceContractError::NonQualifyingStageOutcome {
                stage: InteractionTraceStage::ResizeZoom(RendererResizeTraceStage::WorkerCreate),
                outcome: InteractionTraceStageOutcome::NotApplicable,
            })
        ));

        trace.events[5].stage_outcome = InteractionTraceStageOutcome::Superseded;
        trace.events[5].completed_at = trace.events[5].started_at;
        assert!(matches!(
            trace.validate_qualifying(),
            Err(TraceContractError::NonQualifyingStageOutcome {
                stage: InteractionTraceStage::ResizeZoom(
                    RendererResizeTraceStage::IntentSupersession
                ),
                outcome: InteractionTraceStageOutcome::Superseded,
            })
        ));

        let mut keypress = keypress_trace(3);
        keypress.events[0].stage_outcome = InteractionTraceStageOutcome::NotApplicable;
        keypress.events[0].completed_at = keypress.events[0].started_at;
        assert!(matches!(
            keypress.validate_structure(),
            Err(TraceContractError::StageOutcomeInvalidForPath { .. })
        ));
    }

    #[test]
    fn counter_unavailability_is_explicit_and_non_qualifying() {
        let mut trace = keypress_trace(1);
        trace.events[3].counter_unavailability.queue_depth = true;
        assert!(trace.validate_structure().is_ok());
        assert_eq!(
            trace.validate_qualifying(),
            Err(TraceContractError::CountersUnavailable { event_ordinal: 3 })
        );

        trace.events[3].counters.queue_depth = 1;
        assert_eq!(
            trace.validate_structure(),
            Err(TraceContractError::UnavailableCounterHasValue {
                field: InteractionTraceCounterField::QueueDepth,
            })
        );
    }

    #[test]
    fn outcome_and_counter_unavailability_are_mandatory_closed_metadata() {
        let encoded = serde_json::to_value(keypress_trace(1)).expect("trace serializes");
        for field in ["stage_outcome", "counter_unavailability"] {
            let mut missing = encoded.clone();
            missing["events"][0]
                .as_object_mut()
                .expect("event is an object")
                .remove(field);
            assert!(
                serde_json::from_value::<InteractionTraceV2>(missing).is_err(),
                "missing {field} unexpectedly defaulted"
            );
        }

        let mut unknown = encoded.clone();
        unknown["events"][0]["counter_unavailability"]["unknown_counter"] =
            serde_json::json!(false);
        assert!(
            serde_json::from_value::<InteractionTraceV2>(unknown).is_err(),
            "unknown counter unavailability unexpectedly decoded"
        );

        let mut inverted_name = encoded;
        let first_event = inverted_name["events"][0]
            .as_object_mut()
            .expect("event is an object");
        let flags = first_event
            .remove("counter_unavailability")
            .expect("fixture carries counter unavailability");
        first_event.insert("counter_availability".to_owned(), flags);
        assert!(
            serde_json::from_value::<InteractionTraceV2>(inverted_name).is_err(),
            "inverted counter-availability field name unexpectedly decoded"
        );
    }

    #[test]
    fn trace_topology_must_remain_stable() {
        let mut trace = keypress_trace(1);
        let expected = trace.events[0].topology;
        trace.events[4].topology.pane_id = 99;
        assert_eq!(
            trace.validate_structure(),
            Err(TraceContractError::TraceTopologyChanged {
                expected,
                actual: trace.events[4].topology,
                event_ordinal: 4,
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

        let old: serde_json::Value =
            serde_json::from_str(OLD_FIXTURE).expect("old fixture parses as JSON");
        assert!(!validator.is_valid(&old), "old schema unexpectedly passed");

        let privacy_overlay: serde_json::Value = serde_json::from_str(PRIVACY_BAD_FIXTURE)
            .expect("privacy-negative overlay parses as JSON");
        let mut privacy_bad: serde_json::Value =
            serde_json::from_str(GOOD_FIXTURE).expect("good fixture parses as JSON");
        for field in ["raw_key", "pane_text"] {
            privacy_bad
                .as_object_mut()
                .expect("trace is an object")
                .insert(field.to_owned(), privacy_overlay[field].clone());
        }
        assert!(
            !validator.is_valid(&privacy_bad),
            "otherwise-valid trace with raw-content fields unexpectedly passed"
        );
        let privacy_bad_object = privacy_bad.as_object_mut().expect("trace is an object");
        privacy_bad_object.remove("raw_key");
        privacy_bad_object.remove("pane_text");
        assert!(
            validator.is_valid(&privacy_bad),
            "removing only the planted raw-content fields did not restore validity"
        );
    }

    #[test]
    fn old_fixture_is_retained_but_rejected_by_version() {
        let trace: InteractionTraceV2 =
            serde_json::from_str(OLD_FIXTURE).expect("old top-level envelope remains parseable");
        assert_eq!(
            trace.validate_structure(),
            Err(TraceContractError::UnsupportedSchemaVersion)
        );
    }

    #[test]
    fn unknown_raw_content_fields_are_rejected_and_never_serialized() {
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
        let spans = (1..=12).collect::<BTreeSet<_>>();
        assert!(matches!(
            validate_event(&event, |span_id| spans.contains(&span_id)),
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
