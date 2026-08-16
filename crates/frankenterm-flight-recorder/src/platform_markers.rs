//! Independent, content-free platform-marker emission authority.
//!
//! Marker emission is deliberately not part of [`crate::FlightRecorder::record`].
//! [`FlightRecorder::record_and_prepare_platform_marker`] first records an
//! event and returns a fixed numeric payload only when that exact marker-mode
//! recorder retained it. The caller then invokes [`PlatformMarkerEmitter::emit`]
//! outside recorder admission. Platform tooling is an external loss domain:
//! adapters report their own acceptance and delivery authority, and ambiguity
//! cannot strengthen the internal recorder evidence.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(all(feature = "platform-markers", target_os = "linux"))]
use std::sync::{Arc, Mutex, TryLockError};

#[cfg(all(feature = "platform-markers", target_os = "linux"))]
use crossbeam_utils::CachePadded;
use frankenterm_core_audit_types::interaction_flight_recorder_v1::{
    PlatformMarkerAccountingV1, PlatformMarkerAuthorityV1, RecorderEpochId, RecorderMode,
};
use frankenterm_core_audit_types::interaction_trace_v2::{
    InteractionTraceId, InteractionTracePath, InteractionTraceStage,
};
use thiserror::Error;

use crate::{EventFields, FlightRecorder, ProducerHandle, RecordOutcome, TraceToken};

/// Static Linux `user_events` name for a keypress stage.
pub const KEYPRESS_STAGE_MARKER_NAME: &str = "frankenterm_keypress_stage";
/// Static Linux `user_events` name for a resize or zoom stage.
pub const RESIZE_ZOOM_STAGE_MARKER_NAME: &str = "frankenterm_resize_zoom_stage";

// `signpost` routes this through kdebug's 14-bit code field, so path and stage
// must remain below 0x4000. Trace and span identity travel in the four words.
const KEYPRESS_STAGE_NAMESPACE: u32 = 1 << 8;
const RESIZE_ZOOM_STAGE_NAMESPACE: u32 = 2 << 8;
const MARKER_ADMISSION_SEALED: u64 = 1 << 63;
const MARKER_IN_FLIGHT_MASK: u64 = MARKER_ADMISSION_SEALED - 1;

/// Fixed-shape numeric identity passed to a target-specific marker adapter.
///
/// The stage namespace and ordinal form the marker site code. The remaining
/// four words fit macOS `kdebug_trace`'s numeric argument surface; Linux
/// `user_events` sites carry the same representation without serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformMarkerPayload {
    local_epoch_id: RecorderEpochId,
    trace_id: InteractionTraceId,
    span_id: u64,
    stage: InteractionTraceStage,
    stage_code: u32,
}

impl PlatformMarkerPayload {
    fn from_recorded_event(
        local_epoch_id: RecorderEpochId,
        token: TraceToken,
        fields: &EventFields,
    ) -> Result<Self, MarkerPayloadError> {
        if token.local_epoch_id() != local_epoch_id {
            return Err(MarkerPayloadError::EpochMismatch);
        }
        let trace_id = token.trace_id();
        if !trace_id.is_valid() {
            return Err(MarkerPayloadError::InvalidTraceId);
        }
        if fields.span_id == 0 {
            return Err(MarkerPayloadError::InvalidSpanId);
        }
        if !fields.matches_token(token) {
            return Err(MarkerPayloadError::PathMismatch);
        }
        let stage = fields.stage;
        let stage_code = marker_stage_code(stage);
        Ok(Self {
            local_epoch_id,
            trace_id,
            span_id: fields.span_id,
            stage,
            stage_code,
        })
    }

    #[must_use]
    pub const fn local_epoch_id(self) -> RecorderEpochId {
        self.local_epoch_id
    }

    #[must_use]
    pub const fn trace_id(self) -> InteractionTraceId {
        self.trace_id
    }

    #[must_use]
    pub const fn span_id(self) -> u64 {
        self.span_id
    }

    #[must_use]
    pub const fn stage(self) -> InteractionTraceStage {
        self.stage
    }

    #[must_use]
    pub const fn stage_code(self) -> u32 {
        self.stage_code
    }

    /// Four content-free words shared by all target-specific adapters.
    #[must_use]
    pub const fn identity_words(self) -> [u64; 4] {
        [
            self.trace_id.run_id.epoch_nonce_hi,
            self.trace_id.run_id.epoch_nonce_lo,
            self.trace_id.sequence,
            self.span_id,
        ]
    }

    /// Return one of two compile-time Linux event names; no caller string
    /// reaches a platform API.
    #[must_use]
    pub const fn static_name(self) -> &'static str {
        match self.stage.path() {
            InteractionTracePath::Keypress => KEYPRESS_STAGE_MARKER_NAME,
            InteractionTracePath::ResizeZoom => RESIZE_ZOOM_STAGE_MARKER_NAME,
        }
    }
}

impl FlightRecorder {
    /// Record one event, then prepare a platform payload only when that exact
    /// recorder retained it in the explicit marker-enabled epoch mode.
    ///
    /// The platform call remains outside recorder admission: callers pass the
    /// returned payload to [`PlatformMarkerEmitter::emit`] only after this
    /// method returns. Ordinary modes and all non-recorded outcomes return no
    /// payload and therefore cannot accidentally perform marker work.
    pub fn record_and_prepare_platform_marker(
        &self,
        producer: &ProducerHandle,
        token: TraceToken,
        fields: &EventFields,
    ) -> Result<(RecordOutcome, Option<PlatformMarkerPayload>), MarkerPayloadError> {
        let outcome = self.record(producer, token, fields);
        if self.config().mode() != RecorderMode::CertificationWithMarkers
            || !matches!(outcome, RecordOutcome::Recorded { .. })
        {
            return Ok((outcome, None));
        }
        let payload =
            PlatformMarkerPayload::from_recorded_event(self.config().epoch_id(), token, fields)?;
        Ok((outcome, Some(payload)))
    }
}

/// Stable numeric namespace for a trace-v2 stage.
#[must_use]
pub fn marker_stage_code(stage: InteractionTraceStage) -> u32 {
    let namespace = match stage.path() {
        InteractionTracePath::Keypress => KEYPRESS_STAGE_NAMESPACE,
        InteractionTracePath::ResizeZoom => RESIZE_ZOOM_STAGE_NAMESPACE,
    };
    namespace | u32::from(stage.ordinal())
}

/// Construction failure for a fixed numeric marker payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MarkerPayloadError {
    #[error("platform marker event belongs to a different recorder epoch")]
    EpochMismatch,
    #[error("platform marker trace id is invalid")]
    InvalidTraceId,
    #[error("platform marker span id is invalid")]
    InvalidSpanId,
    #[error("platform marker stage path differs from the sampled trace")]
    PathMismatch,
}

/// Whether an accepted platform call proves downstream marker delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerDeliveryAuthority {
    /// The adapter has an exact, reconciled delivery witness.
    Exact,
    /// The platform accepted the call but may lose it after returning.
    ExternalLossUnknown,
}

/// Typed reason that a platform marker site could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerUnavailableReason {
    UnsupportedPlatform,
    Disabled,
    PermissionDenied,
    RegistrationFailed,
}

/// Typed reason that a requested marker was dropped before platform acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerDropReason {
    QueueFull,
    EmissionRejected,
}

/// One target adapter's synchronous result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformMarkerAdapterOutcome {
    Emitted { delivery: MarkerDeliveryAuthority },
    Unavailable(MarkerUnavailableReason),
    Dropped(MarkerDropReason),
}

mod sealed {
    pub trait Sealed {}
}

/// Safe, fixed-payload target adapter seam.
///
/// Implementations must not retain the payload, wait on application locks or
/// queues, format dynamic strings, mutate the internal recorder, or claim
/// exact delivery without a reconciled witness. A target tracing system call
/// may still contribute diagnostic-mode latency. The emitter is generic over
/// this trait, so production calls do not require dynamic dispatch.
pub trait PlatformMarkerAdapter: sealed::Sealed + Send + Sync {
    fn emit(&self, payload: PlatformMarkerPayload) -> PlatformMarkerAdapterOutcome;
}

#[cfg(all(feature = "platform-markers", target_os = "linux"))]
const LINUX_MARKER_BUILDER_META_CAPACITY: u16 = 256;
#[cfg(all(feature = "platform-markers", target_os = "linux"))]
const LINUX_MARKER_BUILDER_DATA_CAPACITY: u16 = 64;
#[cfg(all(feature = "platform-markers", target_os = "linux"))]
const LINUX_MARKER_MAX_SHARDS: usize = 256;
#[cfg(all(feature = "platform-markers", target_os = "linux"))]
const LINUX_USER_EVENTS_KEYWORD: u64 = 1;
#[cfg(all(feature = "platform-markers", target_os = "linux"))]
const LINUX_ERRNO_EPERM: i32 = 1;
#[cfg(all(feature = "platform-markers", target_os = "linux"))]
const LINUX_ERRNO_ENOENT: i32 = 2;
#[cfg(all(feature = "platform-markers", target_os = "linux"))]
const LINUX_ERRNO_EBADF: i32 = 9;
#[cfg(all(feature = "platform-markers", target_os = "linux"))]
const LINUX_ERRNO_EACCES: i32 = 13;
#[cfg(all(feature = "platform-markers", target_os = "linux"))]
const LINUX_ERRNO_ENOSYS: i32 = 38;
#[cfg(all(feature = "platform-markers", target_os = "linux"))]
const LINUX_ERRNO_EOPNOTSUPP: i32 = 95;

/// Linux `user_events` adapter.
///
/// Provider registration and all allocation happen in [`Self::try_new`]. Each
/// emitter shard owns a builder with enough fixed capacity for the complete
/// static schema. Emission uses `try_lock`, so concurrent shard collisions are
/// reported as bounded drops instead of waiting on the keypress or render path.
#[cfg(all(feature = "platform-markers", target_os = "linux"))]
#[derive(Debug)]
pub struct LinuxUserEventsMarkerAdapter {
    _provider: eventheader_dynamic::Provider,
    event_set: Arc<eventheader_dynamic::EventSet>,
    // Keep independent shard admission words off the same cache line. Without
    // padding, a many-core host can pay coherence traffic even when emitters
    // hash to different builders and never contend on the same mutex.
    builders: Box<[CachePadded<Mutex<eventheader_dynamic::EventBuilder>>]>,
}

#[cfg(all(feature = "platform-markers", target_os = "linux"))]
impl sealed::Sealed for LinuxUserEventsMarkerAdapter {}

#[cfg(all(feature = "platform-markers", target_os = "linux"))]
impl LinuxUserEventsMarkerAdapter {
    /// Register the numeric event set and preallocate one builder per available
    /// CPU, capped to keep off-path setup bounded on large Threadripper hosts.
    pub fn try_new() -> Result<Self, MarkerUnavailableReason> {
        let shard_count = std::thread::available_parallelism().map_or(1, usize::from);
        Self::try_new_with_shards(shard_count.min(LINUX_MARKER_MAX_SHARDS))
    }

    /// Register with at least the requested nonzero builder count. The actual
    /// count is rounded to a power of two for an allocation-free mask on emit.
    pub fn try_new_with_shards(requested_shards: usize) -> Result<Self, MarkerUnavailableReason> {
        let shard_count = linux_marker_shard_count(requested_shards)?;

        let mut provider = eventheader_dynamic::Provider::new(
            "frankenterm",
            &eventheader_dynamic::Provider::new_options(),
        );
        let event_set = provider.register_set(
            eventheader_dynamic::Level::Verbose,
            LINUX_USER_EVENTS_KEYWORD,
        );
        if event_set.errno() != 0 {
            return Err(map_linux_registration_errno(event_set.errno()));
        }

        let mut builders = Vec::new();
        builders
            .try_reserve_exact(shard_count)
            .map_err(|_| MarkerUnavailableReason::RegistrationFailed)?;
        builders.extend((0..shard_count).map(|_| {
            let mut builder = eventheader_dynamic::EventBuilder::new_with_capacity(
                LINUX_MARKER_BUILDER_META_CAPACITY,
                LINUX_MARKER_BUILDER_DATA_CAPACITY,
            );
            prepare_linux_marker_event(&mut builder, RESIZE_ZOOM_STAGE_MARKER_NAME, 0, [0; 4]);
            CachePadded::new(Mutex::new(builder))
        }));

        Ok(Self {
            _provider: provider,
            event_set,
            builders: builders.into_boxed_slice(),
        })
    }
}

#[cfg(all(feature = "platform-markers", target_os = "linux"))]
const fn map_linux_registration_errno(errno: i32) -> MarkerUnavailableReason {
    match errno {
        LINUX_ERRNO_EPERM | LINUX_ERRNO_EACCES => MarkerUnavailableReason::PermissionDenied,
        LINUX_ERRNO_ENOENT | LINUX_ERRNO_ENOSYS | LINUX_ERRNO_EOPNOTSUPP => {
            MarkerUnavailableReason::UnsupportedPlatform
        }
        _ => MarkerUnavailableReason::RegistrationFailed,
    }
}

#[cfg(all(feature = "platform-markers", target_os = "linux"))]
fn linux_marker_shard_count(requested_shards: usize) -> Result<usize, MarkerUnavailableReason> {
    if requested_shards == 0 {
        return Err(MarkerUnavailableReason::RegistrationFailed);
    }
    requested_shards
        .checked_next_power_of_two()
        .filter(|count| *count <= LINUX_MARKER_MAX_SHARDS)
        .ok_or(MarkerUnavailableReason::RegistrationFailed)
}

#[cfg(all(feature = "platform-markers", target_os = "linux"))]
fn prepare_linux_marker_event(
    builder: &mut eventheader_dynamic::EventBuilder,
    static_name: &'static str,
    stage_code: u32,
    [trace_run_hi, trace_run_lo, trace_sequence, span_id]: [u64; 4],
) {
    builder
        .reset(static_name, 0)
        .add_value(
            "stage_code",
            stage_code,
            eventheader_dynamic::FieldFormat::Default,
            0,
        )
        .add_value(
            "trace_run_hi",
            trace_run_hi,
            eventheader_dynamic::FieldFormat::Default,
            0,
        )
        .add_value(
            "trace_run_lo",
            trace_run_lo,
            eventheader_dynamic::FieldFormat::Default,
            0,
        )
        .add_value(
            "trace_sequence",
            trace_sequence,
            eventheader_dynamic::FieldFormat::Default,
            0,
        )
        .add_value(
            "span_id",
            span_id,
            eventheader_dynamic::FieldFormat::Default,
            0,
        );
}

#[cfg(all(feature = "platform-markers", target_os = "linux"))]
impl PlatformMarkerAdapter for LinuxUserEventsMarkerAdapter {
    fn emit(&self, payload: PlatformMarkerPayload) -> PlatformMarkerAdapterOutcome {
        if !self.event_set.enabled() {
            return PlatformMarkerAdapterOutcome::Unavailable(MarkerUnavailableReason::Disabled);
        }

        let mixed_identity = payload.trace_id().run_id.epoch_nonce_hi
            ^ payload.trace_id().run_id.epoch_nonce_lo.rotate_left(17)
            ^ payload.trace_id().sequence.rotate_left(31)
            ^ payload.span_id().rotate_left(47)
            ^ u64::from(payload.stage_code()).rotate_left(7);
        let folded_identity =
            u32::try_from((mixed_identity ^ (mixed_identity >> 32)) & u64::from(u32::MAX))
                .unwrap_or(0);
        let builder_index =
            usize::try_from(folded_identity).unwrap_or(0) & (self.builders.len() - 1);
        let mut builder = match self.builders[builder_index].try_lock() {
            Ok(builder) => builder,
            Err(TryLockError::WouldBlock) => {
                return PlatformMarkerAdapterOutcome::Dropped(MarkerDropReason::QueueFull);
            }
            Err(TryLockError::Poisoned(_)) => {
                return PlatformMarkerAdapterOutcome::Dropped(MarkerDropReason::EmissionRejected);
            }
        };

        prepare_linux_marker_event(
            &mut builder,
            payload.static_name(),
            payload.stage_code(),
            payload.identity_words(),
        );
        let errno = builder.write(&self.event_set, None, None);
        match errno {
            0 => PlatformMarkerAdapterOutcome::Emitted {
                delivery: MarkerDeliveryAuthority::ExternalLossUnknown,
            },
            LINUX_ERRNO_EBADF => {
                PlatformMarkerAdapterOutcome::Unavailable(MarkerUnavailableReason::Disabled)
            }
            _ => PlatformMarkerAdapterOutcome::Dropped(MarkerDropReason::EmissionRejected),
        }
    }
}

/// macOS numeric kdebug adapter. The safe dependency accepts a 14-bit site
/// code and four machine words, which preserves the complete trace-run,
/// sequence, and span identity without formatting. Its void API cannot prove
/// downstream buffer retention, so every accepted call remains inexact.
#[cfg(all(feature = "platform-markers", target_os = "macos"))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MacOsKdebugMarkerAdapter;

#[cfg(all(feature = "platform-markers", target_os = "macos"))]
impl sealed::Sealed for MacOsKdebugMarkerAdapter {}

#[cfg(all(feature = "platform-markers", target_os = "macos"))]
impl PlatformMarkerAdapter for MacOsKdebugMarkerAdapter {
    fn emit(&self, payload: PlatformMarkerPayload) -> PlatformMarkerAdapterOutcome {
        let [trace_run_hi, trace_run_lo, trace_sequence, span_id] = payload.identity_words();
        let (Ok(trace_run_hi), Ok(trace_run_lo), Ok(trace_sequence), Ok(span_id)) = (
            usize::try_from(trace_run_hi),
            usize::try_from(trace_run_lo),
            usize::try_from(trace_sequence),
            usize::try_from(span_id),
        ) else {
            return PlatformMarkerAdapterOutcome::Dropped(MarkerDropReason::EmissionRejected);
        };
        let args = [trace_run_hi, trace_run_lo, trace_sequence, span_id];
        signpost::trace(payload.stage_code(), &args);
        PlatformMarkerAdapterOutcome::Emitted {
            delivery: MarkerDeliveryAuthority::ExternalLossUnknown,
        }
    }
}

/// Portable adapter for targets or builds without platform marker support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedPlatformMarkerAdapter {
    reason: MarkerUnavailableReason,
}

impl sealed::Sealed for UnsupportedPlatformMarkerAdapter {}

impl UnsupportedPlatformMarkerAdapter {
    #[must_use]
    pub const fn new(reason: MarkerUnavailableReason) -> Self {
        Self { reason }
    }
}

impl Default for UnsupportedPlatformMarkerAdapter {
    fn default() -> Self {
        Self::new(MarkerUnavailableReason::UnsupportedPlatform)
    }
}

impl PlatformMarkerAdapter for UnsupportedPlatformMarkerAdapter {
    fn emit(&self, _payload: PlatformMarkerPayload) -> PlatformMarkerAdapterOutcome {
        PlatformMarkerAdapterOutcome::Unavailable(self.reason)
    }
}

/// Result returned to the caller for one marker request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformMarkerOutcome {
    /// The recorder epoch is not the explicit marker-enabled mode.
    NotRequested,
    /// Marker finalization has sealed this emitter against future calls.
    Closed,
    /// The payload belongs to a different immutable recorder epoch.
    WrongEpoch,
    Emitted {
        delivery: MarkerDeliveryAuthority,
    },
    Unavailable(MarkerUnavailableReason),
    Dropped(MarkerDropReason),
    /// The adapter outcome occurred, but a bounded aggregate counter could no
    /// longer represent it exactly. `None` means the attempt counter was
    /// already exhausted and the adapter was not called. `loss_unknown`
    /// becomes sticky in either case.
    AccountingExhausted {
        adapter_outcome: Option<PlatformMarkerAdapterOutcome>,
    },
}

/// Immutable snapshot of the independent platform-marker loss domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformMarkerSnapshot {
    pub authority: PlatformMarkerAuthorityV1,
    pub accounting: PlatformMarkerAccountingV1,
    pub accounting_exhausted: bool,
}

/// Linearizable finalization result for the independent marker loss domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformMarkerFinishOutcome {
    Ready(PlatformMarkerSnapshot),
    Draining { in_flight_operations: u64 },
}

/// Separate marker controller for the explicit marker-enabled certification
/// mode. It owns no recorder reference and has no locks or queues.
#[derive(Debug)]
pub struct PlatformMarkerEmitter<A> {
    epoch_id: RecorderEpochId,
    mode: RecorderMode,
    adapter: A,
    admission: AtomicU64,
    attempted: AtomicU64,
    emitted: AtomicU64,
    unavailable: AtomicU64,
    dropped: AtomicU64,
    loss_unknown: AtomicBool,
    accounting_exhausted: AtomicBool,
}

impl<A> PlatformMarkerEmitter<A>
where
    A: PlatformMarkerAdapter,
{
    /// Bind marker accounting to one exact recorder epoch.
    #[must_use]
    pub fn for_recorder(recorder: &FlightRecorder, adapter: A) -> Self {
        let config = recorder.config();
        Self {
            epoch_id: config.epoch_id(),
            mode: config.mode(),
            adapter,
            admission: AtomicU64::new(0),
            attempted: AtomicU64::new(0),
            emitted: AtomicU64::new(0),
            unavailable: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            loss_unknown: AtomicBool::new(false),
            accounting_exhausted: AtomicBool::new(false),
        }
    }

    /// Attempt one marker outside the recorder admission boundary.
    pub fn emit(&self, payload: PlatformMarkerPayload) -> PlatformMarkerOutcome {
        if self.mode != RecorderMode::CertificationWithMarkers {
            return PlatformMarkerOutcome::NotRequested;
        }
        if payload.local_epoch_id() != self.epoch_id {
            return PlatformMarkerOutcome::WrongEpoch;
        }
        let _admission = match MarkerAdmissionGuard::enter(&self.admission) {
            Ok(admission) => admission,
            Err(MarkerAdmissionFailure::Closed) => return PlatformMarkerOutcome::Closed,
            Err(MarkerAdmissionFailure::Exhausted) => {
                self.mark_accounting_exhausted();
                return PlatformMarkerOutcome::AccountingExhausted {
                    adapter_outcome: None,
                };
            }
        };
        if !try_increment(&self.attempted) {
            self.mark_accounting_exhausted();
            return PlatformMarkerOutcome::AccountingExhausted {
                adapter_outcome: None,
            };
        }

        let adapter_outcome = self.adapter.emit(payload);
        let (counter, external_loss_unknown) = match adapter_outcome {
            PlatformMarkerAdapterOutcome::Emitted { delivery } => (
                &self.emitted,
                delivery == MarkerDeliveryAuthority::ExternalLossUnknown,
            ),
            PlatformMarkerAdapterOutcome::Unavailable(_) => (&self.unavailable, false),
            PlatformMarkerAdapterOutcome::Dropped(_) => (&self.dropped, false),
        };
        if external_loss_unknown {
            self.loss_unknown.store(true, Ordering::Release);
        }
        if !try_increment(counter) {
            self.mark_accounting_exhausted();
            return PlatformMarkerOutcome::AccountingExhausted {
                adapter_outcome: Some(adapter_outcome),
            };
        }

        match adapter_outcome {
            PlatformMarkerAdapterOutcome::Emitted { delivery } => {
                PlatformMarkerOutcome::Emitted { delivery }
            }
            PlatformMarkerAdapterOutcome::Unavailable(reason) => {
                PlatformMarkerOutcome::Unavailable(reason)
            }
            PlatformMarkerAdapterOutcome::Dropped(reason) => PlatformMarkerOutcome::Dropped(reason),
        }
    }

    /// Read a fail-closed diagnostic view against an internal recorded-event
    /// count. Exact authority is impossible until [`Self::finish`] has sealed
    /// admission and every admitted adapter call has returned.
    #[must_use]
    pub fn snapshot(&self, recorded_events: u64) -> PlatformMarkerSnapshot {
        if self.mode != RecorderMode::CertificationWithMarkers {
            return PlatformMarkerSnapshot {
                authority: PlatformMarkerAuthorityV1::NotRequested,
                accounting: PlatformMarkerAccountingV1::default(),
                accounting_exhausted: false,
            };
        }

        let admission = self.admission.load(Ordering::Acquire);
        let finalized =
            admission & MARKER_ADMISSION_SEALED != 0 && admission & MARKER_IN_FLIGHT_MASK == 0;
        let accounting_exhausted = self.accounting_exhausted.load(Ordering::Acquire);
        // Read classified outcomes before attempts. Successful outcome-counter
        // increments publish the preceding attempt, so the trailing acquire
        // cannot produce a diagnostic snapshot with more classified outcomes
        // than attempts even while other emissions remain in flight.
        let emitted = self.emitted.load(Ordering::Acquire);
        let unavailable = self.unavailable.load(Ordering::Acquire);
        let dropped = self.dropped.load(Ordering::Acquire);
        let accounting = PlatformMarkerAccountingV1 {
            attempted: self.attempted.load(Ordering::Acquire),
            emitted,
            unavailable,
            dropped,
            loss_unknown: self.loss_unknown.load(Ordering::Acquire) || accounting_exhausted,
        };
        let exact = finalized
            && !accounting_exhausted
            && !accounting.loss_unknown
            && accounting.attempted == recorded_events
            && accounting.emitted == accounting.attempted
            && accounting.unavailable == 0
            && accounting.dropped == 0;
        PlatformMarkerSnapshot {
            authority: if exact {
                PlatformMarkerAuthorityV1::ExactEveryRecordedEvent
            } else {
                PlatformMarkerAuthorityV1::Inexact
            },
            accounting,
            accounting_exhausted,
        }
    }

    /// Seal admission and return an exact-or-inexact terminal snapshot once
    /// every callback admitted before the seal has returned. Repeated calls
    /// while draining or after completion are harmless and nonblocking.
    #[must_use]
    pub fn finish(&self, recorded_events: u64) -> PlatformMarkerFinishOutcome {
        if self.mode != RecorderMode::CertificationWithMarkers {
            return PlatformMarkerFinishOutcome::Ready(self.snapshot(recorded_events));
        }
        let observed = self
            .admission
            .fetch_or(MARKER_ADMISSION_SEALED, Ordering::AcqRel);
        let in_flight_operations = observed & MARKER_IN_FLIGHT_MASK;
        if in_flight_operations != 0 {
            return PlatformMarkerFinishOutcome::Draining {
                in_flight_operations,
            };
        }
        PlatformMarkerFinishOutcome::Ready(self.snapshot(recorded_events))
    }

    fn mark_accounting_exhausted(&self) {
        self.accounting_exhausted.store(true, Ordering::Release);
        self.loss_unknown.store(true, Ordering::Release);
    }
}

struct MarkerAdmissionGuard<'a> {
    admission: &'a AtomicU64,
}

enum MarkerAdmissionFailure {
    Closed,
    Exhausted,
}

impl<'a> MarkerAdmissionGuard<'a> {
    fn enter(admission: &'a AtomicU64) -> Result<Self, MarkerAdmissionFailure> {
        let mut observed = admission.load(Ordering::Acquire);
        loop {
            if observed & MARKER_ADMISSION_SEALED != 0 {
                return Err(MarkerAdmissionFailure::Closed);
            }
            if observed & MARKER_IN_FLIGHT_MASK == MARKER_IN_FLIGHT_MASK {
                return Err(MarkerAdmissionFailure::Exhausted);
            }
            match admission.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Self { admission }),
                Err(actual) => observed = actual,
            }
        }
    }
}

impl Drop for MarkerAdmissionGuard<'_> {
    fn drop(&mut self) {
        let previous = self.admission.fetch_sub(1, Ordering::Release);
        debug_assert_ne!(previous & MARKER_IN_FLIGHT_MASK, 0);
    }
}

fn try_increment(counter: &AtomicU64) -> bool {
    let mut observed = counter.load(Ordering::Relaxed);
    loop {
        let Some(next) = observed.checked_add(1) else {
            return false;
        };
        match counter.compare_exchange_weak(observed, next, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(actual) => observed = actual,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use frankenterm_core_audit_types::interaction_flight_recorder_v1::{
        RecorderEpochId, RecorderSamplerConfigV1,
    };
    use frankenterm_core_audit_types::interaction_trace_v2::{
        InteractionTraceClockDomain, InteractionTraceCorrelation,
        InteractionTraceCounterUnavailability, InteractionTraceCounters,
        InteractionTraceGenerations, InteractionTraceObservationBoundary, InteractionTraceProducer,
        InteractionTraceRunId, InteractionTraceStageOutcome, InteractionTraceTimestamp,
        InteractionTraceTopology,
    };
    use static_assertions::assert_impl_all;

    use super::*;
    use crate::{ClockStamp, FlightRecorder, RecordOutcome, RecorderConfig, TraceAdmission};

    const TEST_BYTE_CEILING: u64 = 32 * 1024 * 1024;

    #[derive(Debug)]
    struct FixedAdapter {
        outcome: PlatformMarkerAdapterOutcome,
        calls: Arc<AtomicU64>,
        recorder: Option<Arc<FlightRecorder>>,
    }

    #[derive(Debug)]
    struct BarrierAdapter {
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    }

    impl super::sealed::Sealed for FixedAdapter {}
    impl super::sealed::Sealed for BarrierAdapter {}

    impl PlatformMarkerAdapter for BarrierAdapter {
        fn emit(&self, _payload: PlatformMarkerPayload) -> PlatformMarkerAdapterOutcome {
            self.entered.wait();
            self.release.wait();
            PlatformMarkerAdapterOutcome::Emitted {
                delivery: MarkerDeliveryAuthority::Exact,
            }
        }
    }

    impl PlatformMarkerAdapter for FixedAdapter {
        fn emit(&self, _payload: PlatformMarkerPayload) -> PlatformMarkerAdapterOutcome {
            if let Some(recorder) = &self.recorder {
                assert_eq!(
                    recorder.lifecycle_state(),
                    frankenterm_core_audit_types::interaction_flight_recorder_v1::RecorderLifecycleState::Active
                );
                assert_eq!(recorder.queued_events(), 1);
            }
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.outcome
        }
    }

    fn test_recorder(mode: RecorderMode) -> Arc<FlightRecorder> {
        let sampler = if mode == RecorderMode::Off {
            RecorderSamplerConfigV1::off()
        } else {
            RecorderSamplerConfigV1::certification()
        };
        let config = RecorderConfig::new(
            RecorderEpochId::new(1, 2).expect("test epoch is valid"),
            InteractionTraceRunId::new(3, 4).expect("test run is valid"),
            mode,
            sampler,
            1,
            4,
            TEST_BYTE_CEILING,
        )
        .expect("test recorder config is valid");
        FlightRecorder::new(config).expect("test recorder allocates")
    }

    fn event_parts(recorder: &Arc<FlightRecorder>) -> (ProducerHandle, TraceToken, EventFields) {
        let producer = recorder
            .register_producer(0)
            .expect("test producer registers");
        let token = match recorder.admit_local_trace(&producer, InteractionTracePath::Keypress) {
            TraceAdmission::Admitted { token, .. } => token,
            other => panic!("test trace admission failed: {other:?}"),
        };
        let stage = InteractionTraceStage::from_ordinal(InteractionTracePath::Keypress, 0)
            .expect("first keypress stage exists");
        let producer_identity = InteractionTraceProducer {
            host_id: 11,
            process_id: 12,
            process_generation: 13,
            thread_id: 14,
            connection_generation: Some(15),
        };
        let clock_domain = InteractionTraceClockDomain {
            host_id: producer_identity.host_id,
            process_generation: producer_identity.process_generation,
            clock_id: 16,
        };
        let fields = EventFields::new(
            u64::from(stage.ordinal()),
            17,
            None,
            stage,
            InteractionTraceStageOutcome::Performed,
            producer_identity,
            InteractionTraceTopology {
                window_id: 18,
                tab_id: 19,
                pane_id: 20,
            },
            ClockStamp {
                started_at: InteractionTraceTimestamp {
                    clock_domain,
                    monotonic_ns: 21,
                    wall_time_unix_ns: None,
                },
                completed_at: InteractionTraceTimestamp {
                    clock_domain,
                    monotonic_ns: 22,
                    wall_time_unix_ns: None,
                },
            },
            InteractionTraceCorrelation::ExactProtocol {
                protocol_token: 23,
                protocol_generation: 24,
            },
            InteractionTraceCounters::default(),
            InteractionTraceCounterUnavailability::all_available(),
            InteractionTraceGenerations {
                terminal_generation: None,
                snapshot_generation: None,
                frame_generation: None,
            },
            InteractionTraceObservationBoundary::InternalState,
            None,
        )
        .expect("test fields are valid");
        (producer, token, fields)
    }

    fn recorded_payload(recorder: &Arc<FlightRecorder>) -> PlatformMarkerPayload {
        let (producer, token, fields) = event_parts(recorder);
        let (outcome, payload) = recorder
            .record_and_prepare_platform_marker(&producer, token, &fields)
            .expect("test marker payload is valid");
        assert!(matches!(outcome, RecordOutcome::Recorded { .. }));
        payload.expect("recorded marker-mode event prepares a marker")
    }

    fn emitter(
        recorder: &FlightRecorder,
        outcome: PlatformMarkerAdapterOutcome,
    ) -> (PlatformMarkerEmitter<FixedAdapter>, Arc<AtomicU64>) {
        let calls = Arc::new(AtomicU64::new(0));
        (
            PlatformMarkerEmitter::for_recorder(
                recorder,
                FixedAdapter {
                    outcome,
                    calls: Arc::clone(&calls),
                    recorder: None,
                },
            ),
            calls,
        )
    }

    fn finish_ready<A>(
        emitter: &PlatformMarkerEmitter<A>,
        recorded_events: u64,
    ) -> PlatformMarkerSnapshot
    where
        A: PlatformMarkerAdapter,
    {
        match emitter.finish(recorded_events) {
            PlatformMarkerFinishOutcome::Ready(snapshot) => snapshot,
            draining @ PlatformMarkerFinishOutcome::Draining { .. } => {
                panic!("test marker emitter did not finish: {draining:?}")
            }
        }
    }

    #[test]
    fn payload_is_copyable_numeric_identity_with_static_names() {
        assert_impl_all!(PlatformMarkerPayload: Copy, Send, Sync);
        let recorder = test_recorder(RecorderMode::CertificationWithMarkers);
        let payload = recorded_payload(&recorder);
        assert_eq!(payload.static_name(), KEYPRESS_STAGE_MARKER_NAME);
        assert_eq!(payload.span_id(), 17);
        assert_eq!(payload.identity_words()[3], 17);
        assert_eq!(payload.stage_code(), KEYPRESS_STAGE_NAMESPACE);
        assert!(!format!("{payload:?}").contains("sk_live_planted_privacy_negative"));

        let resize = InteractionTraceStage::from_ordinal(InteractionTracePath::ResizeZoom, 0)
            .expect("first resize stage exists");
        assert_eq!(marker_stage_code(resize), RESIZE_ZOOM_STAGE_NAMESPACE);
        assert_ne!(marker_stage_code(resize), payload.stage_code());

        for path in [
            InteractionTracePath::Keypress,
            InteractionTracePath::ResizeZoom,
        ] {
            for ordinal in 0..InteractionTraceStage::stage_count(path) {
                let stage = InteractionTraceStage::from_ordinal(path, ordinal)
                    .expect("declared stage ordinal resolves");
                assert!(marker_stage_code(stage) < 0x4000);
                assert_eq!(marker_stage_code(stage) & 0xff, u32::from(ordinal));
                assert!(matches!(
                    PlatformMarkerPayload {
                        local_epoch_id: payload.local_epoch_id(),
                        trace_id: payload.trace_id(),
                        span_id: payload.span_id(),
                        stage,
                        stage_code: marker_stage_code(stage),
                    }
                    .static_name(),
                    KEYPRESS_STAGE_MARKER_NAME | RESIZE_ZOOM_STAGE_MARKER_NAME
                ));
            }
        }
    }

    #[test]
    fn ordinary_modes_perform_no_marker_work() {
        let marker_recorder = test_recorder(RecorderMode::CertificationWithMarkers);
        let payload = recorded_payload(&marker_recorder);
        for mode in [RecorderMode::Low, RecorderMode::Certification] {
            let ordinary_recorder = test_recorder(mode);
            let (ordinary_producer, ordinary_token, ordinary_fields) =
                event_parts(&ordinary_recorder);
            let (outcome, prepared) = ordinary_recorder
                .record_and_prepare_platform_marker(
                    &ordinary_producer,
                    ordinary_token,
                    &ordinary_fields,
                )
                .expect("ordinary-mode record path is valid");
            assert!(matches!(outcome, RecordOutcome::Recorded { .. }));
            assert_eq!(prepared, None);
            let (emitter, calls) = emitter(
                &ordinary_recorder,
                PlatformMarkerAdapterOutcome::Emitted {
                    delivery: MarkerDeliveryAuthority::Exact,
                },
            );
            assert_eq!(emitter.emit(payload), PlatformMarkerOutcome::NotRequested);
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert_eq!(
                emitter.snapshot(1),
                PlatformMarkerSnapshot {
                    authority: PlatformMarkerAuthorityV1::NotRequested,
                    accounting: PlatformMarkerAccountingV1::default(),
                    accounting_exhausted: false,
                }
            );
        }

        let off_recorder = test_recorder(RecorderMode::Off);
        let (foreign_producer, foreign_token, foreign_fields) = event_parts(&marker_recorder);
        let (outcome, prepared) = off_recorder
            .record_and_prepare_platform_marker(&foreign_producer, foreign_token, &foreign_fields)
            .expect("off-mode record path is valid");
        assert_eq!(outcome, RecordOutcome::Off);
        assert_eq!(prepared, None);
        let (off_emitter, off_calls) = emitter(
            &off_recorder,
            PlatformMarkerAdapterOutcome::Emitted {
                delivery: MarkerDeliveryAuthority::Exact,
            },
        );
        assert_eq!(
            off_emitter.emit(payload),
            PlatformMarkerOutcome::NotRequested
        );
        assert_eq!(off_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn nonrecorded_outcomes_prepare_no_marker_payload() {
        let recorder = test_recorder(RecorderMode::CertificationWithMarkers);
        let (producer, token, fields) = event_parts(&recorder);
        assert_eq!(recorder.begin_close(), crate::CloseOutcome::Ready);
        let (outcome, payload) = recorder
            .record_and_prepare_platform_marker(&producer, token, &fields)
            .expect("non-recorded outcome does not need marker preparation");
        assert_eq!(outcome, RecordOutcome::OutsideEpoch);
        assert_eq!(payload, None);
    }

    #[test]
    fn emitter_rejects_cross_epoch_payload_without_accounting_it() {
        let recorder = test_recorder(RecorderMode::CertificationWithMarkers);
        let payload = recorded_payload(&recorder);
        let foreign_payload = PlatformMarkerPayload {
            local_epoch_id: RecorderEpochId::new(99, 100).expect("foreign epoch is valid"),
            ..payload
        };
        let (emitter, calls) = emitter(
            &recorder,
            PlatformMarkerAdapterOutcome::Emitted {
                delivery: MarkerDeliveryAuthority::Exact,
            },
        );
        assert_eq!(
            emitter.emit(foreign_payload),
            PlatformMarkerOutcome::WrongEpoch
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let snapshot = finish_ready(&emitter, 1);
        assert_eq!(snapshot.accounting.attempted, 0);
        assert_eq!(snapshot.authority, PlatformMarkerAuthorityV1::Inexact);
    }

    #[test]
    fn exact_authority_requires_every_recorded_event_and_exact_delivery() {
        let recorder = test_recorder(RecorderMode::CertificationWithMarkers);
        let payload = recorded_payload(&recorder);
        let (emitter, calls) = emitter(
            &recorder,
            PlatformMarkerAdapterOutcome::Emitted {
                delivery: MarkerDeliveryAuthority::Exact,
            },
        );
        assert_eq!(
            emitter.emit(payload),
            PlatformMarkerOutcome::Emitted {
                delivery: MarkerDeliveryAuthority::Exact
            }
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            emitter.snapshot(1).authority,
            PlatformMarkerAuthorityV1::Inexact
        );
        let exact = finish_ready(&emitter, 1);
        assert_eq!(
            exact.authority,
            PlatformMarkerAuthorityV1::ExactEveryRecordedEvent
        );
        assert_eq!(exact.accounting.attempted, 1);
        assert_eq!(exact.accounting.emitted, 1);
        assert!(!exact.accounting.loss_unknown);
        assert_eq!(
            emitter.snapshot(2).authority,
            PlatformMarkerAuthorityV1::Inexact
        );
    }

    #[test]
    fn external_loss_unavailable_and_drop_stay_separate() {
        let recorder = test_recorder(RecorderMode::CertificationWithMarkers);
        let payload = recorded_payload(&recorder);
        let (lossy, _) = emitter(
            &recorder,
            PlatformMarkerAdapterOutcome::Emitted {
                delivery: MarkerDeliveryAuthority::ExternalLossUnknown,
            },
        );
        assert!(matches!(
            lossy.emit(payload),
            PlatformMarkerOutcome::Emitted {
                delivery: MarkerDeliveryAuthority::ExternalLossUnknown
            }
        ));
        let lossy_snapshot = finish_ready(&lossy, 1);
        assert_eq!(lossy_snapshot.authority, PlatformMarkerAuthorityV1::Inexact);
        assert!(lossy_snapshot.accounting.loss_unknown);

        let (unavailable, _) = emitter(
            &recorder,
            PlatformMarkerAdapterOutcome::Unavailable(MarkerUnavailableReason::PermissionDenied),
        );
        assert_eq!(
            unavailable.emit(payload),
            PlatformMarkerOutcome::Unavailable(MarkerUnavailableReason::PermissionDenied)
        );
        let unavailable_snapshot = finish_ready(&unavailable, 1);
        assert_eq!(unavailable_snapshot.accounting.unavailable, 1);
        assert_eq!(unavailable_snapshot.accounting.emitted, 0);

        let (dropped, _) = emitter(
            &recorder,
            PlatformMarkerAdapterOutcome::Dropped(MarkerDropReason::QueueFull),
        );
        assert_eq!(
            dropped.emit(payload),
            PlatformMarkerOutcome::Dropped(MarkerDropReason::QueueFull)
        );
        let dropped_snapshot = finish_ready(&dropped, 1);
        assert_eq!(dropped_snapshot.accounting.dropped, 1);
        assert_eq!(dropped_snapshot.accounting.unavailable, 0);

        let unsupported = PlatformMarkerEmitter::for_recorder(
            &recorder,
            UnsupportedPlatformMarkerAdapter::default(),
        );
        assert_eq!(
            unsupported.emit(payload),
            PlatformMarkerOutcome::Unavailable(MarkerUnavailableReason::UnsupportedPlatform)
        );
        let unsupported_snapshot = finish_ready(&unsupported, 1);
        assert_eq!(unsupported_snapshot.accounting.attempted, 1);
        assert_eq!(unsupported_snapshot.accounting.unavailable, 1);
        assert_eq!(
            unsupported_snapshot.authority,
            PlatformMarkerAuthorityV1::Inexact
        );
    }

    #[test]
    fn accounting_exhaustion_is_sticky_and_fail_closed() {
        let recorder = test_recorder(RecorderMode::CertificationWithMarkers);
        let payload = recorded_payload(&recorder);
        let (emitter, calls) = emitter(
            &recorder,
            PlatformMarkerAdapterOutcome::Emitted {
                delivery: MarkerDeliveryAuthority::Exact,
            },
        );
        emitter.attempted.store(u64::MAX, Ordering::Relaxed);
        assert!(matches!(
            emitter.emit(payload),
            PlatformMarkerOutcome::AccountingExhausted { .. }
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let snapshot = finish_ready(&emitter, u64::MAX);
        assert!(snapshot.accounting_exhausted);
        assert!(snapshot.accounting.loss_unknown);
        assert_eq!(snapshot.authority, PlatformMarkerAuthorityV1::Inexact);
    }

    #[test]
    fn finish_seals_future_calls_and_waits_without_blocking() {
        let recorder = test_recorder(RecorderMode::CertificationWithMarkers);
        let payload = recorded_payload(&recorder);
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let emitter = Arc::new(PlatformMarkerEmitter::for_recorder(
            &recorder,
            BarrierAdapter {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            },
        ));
        let worker_emitter = Arc::clone(&emitter);
        let worker = thread::spawn(move || worker_emitter.emit(payload));
        entered.wait();

        let live = emitter.snapshot(1);
        assert_eq!(live.authority, PlatformMarkerAuthorityV1::Inexact);
        assert_eq!(live.accounting.attempted, 1);
        assert_eq!(live.accounting.emitted, 0);
        assert_eq!(
            live.accounting.checked_classified(),
            Ok(0),
            "an in-flight diagnostic snapshot remains structurally coherent"
        );
        assert_eq!(
            emitter.finish(1),
            PlatformMarkerFinishOutcome::Draining {
                in_flight_operations: 1
            }
        );
        assert_eq!(emitter.emit(payload), PlatformMarkerOutcome::Closed);
        release.wait();
        assert!(matches!(
            worker.join().expect("marker worker must not panic"),
            PlatformMarkerOutcome::Emitted { .. }
        ));
        assert_eq!(
            finish_ready(&emitter, 1).authority,
            PlatformMarkerAuthorityV1::ExactEveryRecordedEvent
        );
    }

    #[test]
    fn adapter_reentrancy_observes_no_recorder_lock_boundary() {
        let recorder = test_recorder(RecorderMode::CertificationWithMarkers);
        let payload = recorded_payload(&recorder);
        let calls = Arc::new(AtomicU64::new(0));
        let emitter = PlatformMarkerEmitter::for_recorder(
            &recorder,
            FixedAdapter {
                outcome: PlatformMarkerAdapterOutcome::Emitted {
                    delivery: MarkerDeliveryAuthority::ExternalLossUnknown,
                },
                calls: Arc::clone(&calls),
                recorder: Some(Arc::clone(&recorder)),
            },
        );
        assert!(matches!(
            emitter.emit(payload),
            PlatformMarkerOutcome::Emitted { .. }
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[cfg(all(feature = "platform-markers", target_os = "linux"))]
    #[test]
    fn linux_registration_preserves_typed_failure_authority() {
        assert_eq!(
            map_linux_registration_errno(LINUX_ERRNO_EPERM),
            MarkerUnavailableReason::PermissionDenied
        );
        assert_eq!(
            map_linux_registration_errno(LINUX_ERRNO_EACCES),
            MarkerUnavailableReason::PermissionDenied
        );
        assert_eq!(
            map_linux_registration_errno(LINUX_ERRNO_ENOENT),
            MarkerUnavailableReason::UnsupportedPlatform
        );
        assert_eq!(
            map_linux_registration_errno(22),
            MarkerUnavailableReason::RegistrationFailed
        );
        assert_eq!(
            linux_marker_shard_count(0).unwrap_err(),
            MarkerUnavailableReason::RegistrationFailed
        );
        assert_eq!(linux_marker_shard_count(1), Ok(1));
        assert_eq!(linux_marker_shard_count(3), Ok(4));
        assert_eq!(linux_marker_shard_count(128), Ok(128));
        assert_eq!(linux_marker_shard_count(129), Ok(256));
        assert_eq!(
            linux_marker_shard_count(257).unwrap_err(),
            MarkerUnavailableReason::RegistrationFailed
        );
    }
}
